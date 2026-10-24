//! Differencing-VHDX chain maintenance in userspace: fold a child into its
//! parent, and point a child at another parent — what `MergeVirtualDisk` and
//! `SetVirtualDiskInformation` do on Windows ([`crate::vhdx`]), for systems
//! without the Windows virtual-disk API. Consolidation
//! ([`crate::consolidate`]) uses these off Windows; its Windows arms keep the
//! native API.
//!
//! # Fold
//!
//! Every block the child carries is read through the CHILD's chain
//! ([`VhdxGuestReader`]) and written into the base with
//! [`VhdxWriter::open_existing`]. Reading through the chain is what makes a
//! PARTIALLY_PRESENT block (BAT state 7, sector-specific) come out right: the
//! sectors the child owns come from the child, the rest from the base — the
//! whole block as the child's restore point reads it.
//!
//! The fold can be re-run after a crash at any point and lands in the same
//! state, which is what consolidation's marker relies on:
//! - a sector the child does not own reads from the base, and every write the
//!   fold makes there puts back the very bytes it read, so a half-done fold
//!   does not change what the next attempt reads;
//! - a block the base did not have is appended past the end of the file and
//!   only becomes visible when the BAT is written, after the payload has been
//!   flushed; an interrupted attempt leaves unreferenced space, nothing else.
//!
//! The base's DataWriteGuid is left alone, as every userspace writer here
//! leaves it (and as a measured `MergeVirtualDisk` left it, see
//! `consolidate.rs`); a completed fold is never inferred from it.
//!
//! # Repoint, crash-safe without the log (decision R3)
//!
//! The parent locator lives in the metadata region, and MS-VHDX expects
//! metadata changes to go through the log. Writing a log entry would also
//! mean replaying one — and our readers refuse a file whose log is not empty,
//! because only Windows replays logs today. So nothing is changed in place:
//! 1. the active header is rewritten into the other header slot with the next
//!    sequence number and a new FileWriteGuid (MS-VHDX §2.2.2: before the
//!    first change to a file opened for writing). The DataWriteGuid stays —
//!    the guest data does not change, and a grandchild's linkage names it;
//! 2. a complete new metadata region, identical except for the parent
//!    locator, is written at a fresh 1 MiB-aligned offset past the end of the
//!    file and flushed;
//! 3. the backup region table (192 KiB + 64 KiB) is pointed at it and flushed;
//! 4. the primary region table is pointed at it and flushed — the switch.
//!
//! Readers take the first region table whose checksum is valid. Before step 4
//! the primary still names the old region; a torn primary fails its checksum
//! and the backup — already new — is used. The two copies differ from their
//! old selves only in the first sector (header and entries; the rest of the
//! 64 KiB stays zero), so on a disk with atomic sector writes the primary is
//! never torn at all, which is what readers that look at the primary only
//! (QEMU) see. Either way a reader gets the old locator or the new one,
//! never a mixture, and a repoint run again after the switch only copies the
//! believed table over a torn one. The old metadata region becomes unused space (1 MiB per
//! repoint; in consolidation a child is repointed once before it is folded).

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use tracing::info;
use uuid::Uuid;

use own_backup_common::{OwnBackupError, Result};

use crate::vhdx_bat::{
    allocated_payload_blocks, crc32c, read_valid_region_table, vhdx_geometry,
    vhdx_parent_linkage_matches, vhdx_parent_path, VhdxGuestReader, REGION1_OFF, REGION2_OFF,
};
use crate::vhdx_format::{parent_locator_for, VhdxWriter};

const MB: u64 = 1024 * 1024;
/// Header slots (MS-VHDX §2.2.2); the live one has the greater sequence number.
const HEADER_OFFS: [u64; 2] = [64 * 1024, 128 * 1024];
const HEADER_LEN: usize = 4096;
/// Metadata items start 64 KiB into the region (MS-VHDX §2.6.1.2).
const META_ITEMS_OFF: usize = 64 * 1024;
// Same literals as vhdx_bat.rs / vhdx_format.rs.
const META_GUID: &str = "8b7ca206-4790-4b9a-b8fe-575f050f886e";
const MI_PARENT_LOCATOR: &str = "a8d35f2d-b30b-454d-abf7-d3d84834ab0c";

fn bad(msg: impl std::fmt::Display) -> OwnBackupError {
    OwnBackupError::Vhdx(format!("vhdx chain: {msg}"))
}

fn guid_le(s: &str) -> [u8; 16] {
    Uuid::parse_str(s)
        .expect("valid GUID literal")
        .to_bytes_le()
}

fn read_exact_at(file: &mut File, off: u64, buf: &mut [u8]) -> Result<()> {
    file.seek(SeekFrom::Start(off)).map_err(bad)?;
    file.read_exact(buf).map_err(bad)
}

fn write_all_at(file: &mut File, off: u64, buf: &[u8]) -> Result<()> {
    file.seek(SeekFrom::Start(off)).map_err(bad)?;
    file.write_all(buf).map_err(bad)
}

/// Do two paths name the same file?
fn same_file(a: &Path, b: &Path) -> Result<bool> {
    let a = a
        .canonicalize()
        .map_err(|e| bad(format!("{}: {e}", a.display())))?;
    let b = b
        .canonicalize()
        .map_err(|e| bad(format!("{}: {e}", b.display())))?;
    Ok(a == b)
}

#[cfg(test)]
mod stop {
    //! Test-only interruption points inside [`super::repoint`].
    use std::cell::Cell;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum After {
        Header,
        Metadata,
        BackupRegionTable,
    }
    thread_local! { pub(super) static AT: Cell<Option<After>> = const { Cell::new(None) }; }

    pub(super) fn hit(point: After) -> own_backup_common::Result<()> {
        if AT.with(|at| at.get()) == Some(point) {
            return Err(own_backup_common::OwnBackupError::Cancelled);
        }
        Ok(())
    }
}

/// What a fold moved.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FoldStats {
    /// Payload blocks of the child written into the base.
    pub blocks: u64,
    pub bytes: u64,
}

/// Fold the differencing child `victim` into its parent `base`: afterwards the
/// base reads exactly as the victim did. Re-runnable after an interruption at
/// any point (see the module notes).
///
/// Refuses a victim that is not a child of `base` — its locator must lead to
/// that very file and its linkage must still accept it (the check Windows
/// makes before it opens the chain) — or that describes another disk.
pub fn fold(base: &Path, victim: &Path) -> Result<FoldStats> {
    let parent = vhdx_parent_path(victim)?.ok_or_else(|| {
        bad(format!(
            "{} is not a differencing image — nothing to fold",
            victim.display()
        ))
    })?;
    if !parent.exists() || !same_file(&parent, base)? {
        return Err(bad(format!(
            "{} names {} as its parent, not {}",
            victim.display(),
            parent.display(),
            base.display()
        )));
    }
    if !vhdx_parent_linkage_matches(victim, base)? {
        return Err(bad(format!(
            "{} no longer accepts {} as its parent (DataWriteGuid differs from its \
             parent_linkage) — the base was changed after the increment was made",
            victim.display(),
            base.display()
        )));
    }
    // Both also refuse an image whose log still holds entries.
    let vg = vhdx_geometry(victim)?;
    let bg = vhdx_geometry(base)?;
    if vg.virtual_size != bg.virtual_size || vg.logical_sector_size != bg.logical_sector_size {
        return Err(bad(format!(
            "{} ({} bytes, {}-byte sectors) and {} ({} bytes, {}-byte sectors) are not the same disk",
            victim.display(),
            vg.virtual_size,
            vg.logical_sector_size,
            base.display(),
            bg.virtual_size,
            bg.logical_sector_size
        )));
    }

    let carried = allocated_payload_blocks(victim)?;
    let mut reader = VhdxGuestReader::open(victim)?;
    let mut writer = VhdxWriter::open_existing(base)?;
    let mut stats = FoldStats::default();
    let mut buf = Vec::new();
    for &block in &carried.blocks {
        let (start, end) = carried.block_range(block);
        buf.resize((end - start) as usize, 0);
        reader.read_at(start, &mut buf)?;
        writer.write_at(start, &buf)?;
        stats.blocks += 1;
        stats.bytes += end - start;
    }
    // The payload must be on disk before a BAT entry may point at it.
    writer.flush_payload()?;
    writer.finish()?;
    info!(
        base = %base.display(),
        child = %victim.display(),
        blocks = stats.blocks,
        bytes = stats.bytes,
        "VHDX child folded into its parent"
    );
    Ok(stats)
}

/// Point the differencing `child` at `new_parent` (which must already read as
/// the child's old parent did — consolidation guarantees it by folding first).
/// Crash-safe and re-runnable; a child that already points there is left
/// untouched.
pub fn repoint(child: &Path, new_parent: &Path) -> Result<()> {
    if !new_parent.exists() {
        return Err(bad(format!(
            "new parent {} does not exist — refusing to repoint {}",
            new_parent.display(),
            child.display()
        )));
    }
    if same_file(child, new_parent)? {
        return Err(bad(format!("{} cannot be its own parent", child.display())));
    }
    // Both refuse an image whose log still holds entries.
    let cg = vhdx_geometry(child)?;
    let pg = vhdx_geometry(new_parent)?;
    if !cg.has_parent {
        return Err(bad(format!(
            "{} is not a differencing image",
            child.display()
        )));
    }
    if cg.virtual_size != pg.virtual_size || cg.logical_sector_size != pg.logical_sector_size {
        return Err(bad(format!(
            "{} and {} are not the same disk",
            child.display(),
            new_parent.display()
        )));
    }

    // A resumed consolidation repeats this step; so does a crash after the
    // switch. Nothing to do when the locator already leads to the new parent
    // and accepts it.
    if let Some(current) = vhdx_parent_path(child)? {
        if current.exists()
            && same_file(&current, new_parent)?
            && vhdx_parent_linkage_matches(child, new_parent)?
        {
            return align_region_tables(child);
        }
    }

    let locator = parent_locator_for(child, new_parent)?;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(child)
        .map_err(|e| bad(format!("open {}: {e}", child.display())))?;

    // Everything is read and checked before the first byte is written.
    let mut region = read_valid_region_table(&mut file)?.ok_or_else(|| {
        bad(format!(
            "{}: neither region table copy has a valid checksum",
            child.display()
        ))
    })?;
    let entry_count = u32::from_le_bytes(region[8..12].try_into().unwrap()) as usize;
    let meta_entry = (0..entry_count.min(2047))
        .map(|slot| 16 + slot * 32)
        .find(|&e| region[e..e + 16] == guid_le(META_GUID))
        .ok_or_else(|| bad("region table has no metadata region"))?;
    let meta_off = u64::from_le_bytes(region[meta_entry + 16..meta_entry + 24].try_into().unwrap());
    let meta_len =
        u32::from_le_bytes(region[meta_entry + 24..meta_entry + 28].try_into().unwrap()) as u64;
    if meta_len < MB || !meta_len.is_multiple_of(MB) || meta_len > 16 * MB {
        return Err(bad(format!("unexpected metadata region length {meta_len}")));
    }
    let mut meta = vec![0u8; meta_len as usize];
    read_exact_at(&mut file, meta_off, &mut meta)?;
    if &meta[0..8] != b"metadata" {
        return Err(bad("no metadata signature"));
    }
    let new_meta = metadata_with_locator(&meta, &locator)?;

    // 1. Header: next sequence number, new FileWriteGuid, same DataWriteGuid.
    bump_header(&mut file)?;
    #[cfg(test)]
    stop::hit(stop::After::Header)?;

    // 2. The new metadata region, past everything the file holds.
    let file_len = file.metadata().map_err(bad)?.len();
    let new_off = file_len.div_ceil(MB) * MB;
    write_all_at(&mut file, new_off, &new_meta)?;
    file.sync_all().map_err(bad)?;
    #[cfg(test)]
    stop::hit(stop::After::Metadata)?;

    // 3 + 4. Region tables: the backup copy first, then the primary.
    region[meta_entry + 16..meta_entry + 24].copy_from_slice(&new_off.to_le_bytes());
    region[4..8].fill(0);
    let crc = crc32c(&region);
    region[4..8].copy_from_slice(&crc.to_le_bytes());
    write_all_at(&mut file, REGION2_OFF, &region)?;
    file.sync_data().map_err(bad)?;
    #[cfg(test)]
    stop::hit(stop::After::BackupRegionTable)?;
    write_all_at(&mut file, REGION1_OFF, &region)?;
    file.sync_data().map_err(bad)?;

    info!(
        child = %child.display(),
        parent = %new_parent.display(),
        metadata_offset = new_off,
        "Differencing VHDX repointed at a new parent"
    );
    Ok(())
}

/// Make both region table copies equal to the one the readers believe.
///
/// Only a crash during the final write of a repoint can leave them apart: the
/// primary torn (and failing its checksum) while the backup is already the
/// new table. Our readers cope, but a reader that looks at the primary only
/// (QEMU does) would refuse the file, so the next repoint puts it back. A file
/// with no valid copy at all is left alone.
fn align_region_tables(path: &Path) -> Result<()> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| bad(format!("open {}: {e}", path.display())))?;
    let Some(good) = read_valid_region_table(&mut file)? else {
        return Ok(());
    };
    for off in [REGION2_OFF, REGION1_OFF] {
        let mut copy = vec![0u8; good.len()];
        read_exact_at(&mut file, off, &mut copy)?;
        if copy != good {
            write_all_at(&mut file, off, &good)?;
            file.sync_data().map_err(bad)?;
        }
    }
    Ok(())
}

/// A copy of the metadata region `meta` whose parent locator item is
/// `locator`; every other item keeps its id, flags and bytes (their offsets
/// move, items are packed from 64 KiB on).
fn metadata_with_locator(meta: &[u8], locator: &[u8]) -> Result<Vec<u8>> {
    let count = u16::from_le_bytes(meta[10..12].try_into().unwrap()) as usize;
    if count > 2047 {
        return Err(bad(format!("metadata table claims {count} entries")));
    }
    let mut out = vec![0u8; meta.len()];
    out[0..32].copy_from_slice(&meta[0..32]);
    let mut cur = META_ITEMS_OFF;
    let mut replaced = false;
    for slot in 0..count {
        let e = 32 + slot * 32;
        let entry = meta
            .get(e..e + 32)
            .ok_or_else(|| bad("metadata table runs past its region"))?;
        out[e..e + 32].copy_from_slice(entry);
        let off = u32::from_le_bytes(entry[16..20].try_into().unwrap()) as usize;
        let len = u32::from_le_bytes(entry[20..24].try_into().unwrap()) as usize;
        let payload: &[u8] = if entry[0..16] == guid_le(MI_PARENT_LOCATOR) {
            if replaced {
                return Err(bad("metadata holds two parent locators"));
            }
            replaced = true;
            locator
        } else if len == 0 {
            &[]
        } else {
            meta.get(off..off + len)
                .ok_or_else(|| bad("metadata item runs past its region"))?
        };
        if payload.is_empty() {
            out[e + 16..e + 24].fill(0);
            continue;
        }
        if cur + payload.len() > out.len() {
            return Err(bad("metadata items do not fit the metadata region"));
        }
        out[cur..cur + payload.len()].copy_from_slice(payload);
        out[e + 16..e + 20].copy_from_slice(&(cur as u32).to_le_bytes());
        out[e + 20..e + 24].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        cur += payload.len();
    }
    if !replaced {
        return Err(bad("the image has no parent locator to replace"));
    }
    Ok(out)
}

fn header_is_valid(h: &[u8]) -> bool {
    if &h[0..4] != b"head" {
        return false;
    }
    let stored = u32::from_le_bytes(h[4..8].try_into().unwrap());
    let mut copy = h.to_vec();
    copy[4..8].fill(0);
    crc32c(&copy) == stored
}

/// Write the next header (MS-VHDX §2.2.2.1): a copy of the live one with the
/// sequence number + 1 and a new FileWriteGuid, into the other slot. The
/// DataWriteGuid is kept — the guest data is not changing, and a grandchild's
/// parent linkage names it.
fn bump_header(file: &mut File) -> Result<()> {
    let mut live: Option<(usize, u64, Vec<u8>)> = None;
    for (slot, &off) in HEADER_OFFS.iter().enumerate() {
        let mut h = vec![0u8; HEADER_LEN];
        read_exact_at(file, off, &mut h)?;
        if !header_is_valid(&h) {
            continue;
        }
        let seq = u64::from_le_bytes(h[8..16].try_into().unwrap());
        if live.as_ref().is_none_or(|(_, s, _)| seq > *s) {
            live = Some((slot, seq, h));
        }
    }
    let (slot, seq, mut h) = live.ok_or_else(|| bad("no valid VHDX header"))?;
    if h[48..64] != [0u8; 16] {
        return Err(bad("the log still holds entries to replay"));
    }
    h[8..16].copy_from_slice(&(seq + 1).to_le_bytes());
    h[16..32].copy_from_slice(&Uuid::new_v4().to_bytes_le());
    h[4..8].fill(0);
    let crc = crc32c(&h);
    h[4..8].copy_from_slice(&crc.to_le_bytes());
    write_all_at(file, HEADER_OFFS[1 - slot], &h)?;
    file.sync_data().map_err(bad)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vhdx_bat::vhdx_data_write_guid;
    use std::path::PathBuf;

    /// Where `VhdxWriter::create` puts the BAT of the files these tests make.
    const BAT_OFF: u64 = 3 * MB;
    const VSIZE: u64 = 16 * MB;

    fn pattern(seed: u8, len: u64) -> Vec<u8> {
        (0..len)
            .map(|i| (i % 251) as u8 ^ seed.wrapping_mul(37))
            .collect()
    }

    /// A base the way the Linux engine makes one (2 MiB blocks), with data
    /// in some blocks and none in others.
    fn base_image(dir: &Path) -> PathBuf {
        let path = dir.join("base.vhdx");
        let mut w = VhdxWriter::create(&path, VSIZE, 2 * MB, 512).unwrap();
        w.write_at(0, &pattern(1, 4 * MB)).unwrap();
        w.write_at(10 * MB, &pattern(2, MB)).unwrap();
        w.finish().unwrap();
        path
    }

    fn child(dir: &Path, name: &str, parent: &Path, writes: &[(u64, Vec<u8>)]) -> PathBuf {
        let path = dir.join(name);
        let mut w = VhdxWriter::create_differencing(&path, parent).unwrap();
        for (off, bytes) in writes {
            w.write_at(*off, bytes).unwrap();
        }
        w.finish().unwrap();
        path
    }

    fn read_all(path: &Path) -> Vec<u8> {
        let mut r = VhdxGuestReader::open(path).unwrap();
        let mut buf = vec![0u8; r.virtual_size() as usize];
        r.read_at(0, &mut buf).unwrap();
        buf
    }

    fn parent_of(path: &Path) -> PathBuf {
        vhdx_parent_path(path)
            .unwrap()
            .expect("a parent")
            .canonicalize()
            .unwrap()
    }

    /// base ← inc1 ← inc2: zeros over the parent's data, partial writes, a
    /// write over a block the base never had.
    fn chain(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
        let base = base_image(dir);
        let inc1 = child(
            dir,
            "inc1.avhdx",
            &base,
            &[
                (0, vec![0u8; MB as usize]),
                (3 * MB + 100, pattern(3, 4096)),
                (9 * MB, pattern(4, MB)),
                (VSIZE - 512, pattern(5, 512)),
            ],
        );
        let inc2 = child(
            dir,
            "inc2.avhdx",
            &inc1,
            &[(512, pattern(6, 1024)), (12 * MB + 7, pattern(7, 300))],
        );
        (base, inc1, inc2)
    }

    #[test]
    fn fold_then_repoint_keeps_every_surviving_restore_point() {
        let dir = tempfile::tempdir().unwrap();
        let (base, inc1, inc2) = chain(dir.path());
        let (v1, v2) = (read_all(&inc1), read_all(&inc2));
        let leaf_identity = vhdx_data_write_guid(&inc2).unwrap();

        let stats = fold(&base, &inc1).unwrap();
        assert_eq!(stats.blocks, 4, "inc1 carries blocks 0, 3, 9 and 15");
        assert_eq!(read_all(&base), v1, "the base now IS inc1's point in time");
        assert_eq!(read_all(&inc2), v2, "the leaf reads the same through inc1");

        repoint(&inc2, &base).unwrap();
        assert_eq!(parent_of(&inc2), base.canonicalize().unwrap());
        assert!(vhdx_parent_linkage_matches(&inc2, &base).unwrap());
        assert_eq!(
            vhdx_data_write_guid(&inc2).unwrap(),
            leaf_identity,
            "a repoint must not change the child's own identity — its children link to it"
        );
        std::fs::remove_file(&inc1).unwrap();
        assert_eq!(read_all(&inc2), v2, "the leaf reads the same over the base");

        // Done is done: a second repoint changes nothing at all.
        let before = std::fs::read(&inc2).unwrap();
        repoint(&inc2, &base).unwrap();
        assert_eq!(std::fs::read(&inc2).unwrap(), before);
    }

    #[test]
    fn a_fold_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let (base, inc1, _) = chain(dir.path());
        let v1 = read_all(&inc1);
        fold(&base, &inc1).unwrap();
        fold(&base, &inc1).unwrap();
        assert_eq!(read_all(&base), v1);
    }

    /// A crash part-way through: some blocks written into the base, with or
    /// without the BAT that makes them visible. The next run must finish it.
    #[test]
    fn an_interrupted_fold_finishes_on_the_next_run() {
        for bat_written in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let (base, inc1, inc2) = chain(dir.path());
            let (v1, v2) = (read_all(&inc1), read_all(&inc2));

            let carried = allocated_payload_blocks(&inc1).unwrap();
            let mut reader = VhdxGuestReader::open(&inc1).unwrap();
            let mut writer = VhdxWriter::open_existing(&base).unwrap();
            for &block in carried.blocks.iter().take(2) {
                let (start, end) = carried.block_range(block);
                let mut buf = vec![0u8; (end - start) as usize];
                reader.read_at(start, &mut buf).unwrap();
                writer.write_at(start, &buf).unwrap();
            }
            if bat_written {
                writer.finish().unwrap();
            } else {
                drop(writer);
            }
            drop(reader);

            fold(&base, &inc1).unwrap();
            assert_eq!(read_all(&base), v1, "bat_written={bat_written}");
            repoint(&inc2, &base).unwrap();
            assert_eq!(read_all(&inc2), v2, "bat_written={bat_written}");
        }
    }

    /// A PARTIALLY_PRESENT block (state 7, as Windows leaves after a mount)
    /// folds sector by sector: the child's sectors from the child, the rest
    /// from the base — never the child file's bytes for the base's sectors.
    #[test]
    fn a_partially_present_block_folds_sector_by_sector() {
        let dir = tempfile::tempdir().unwrap();
        let base = base_image(dir.path());
        let victim = child(dir.path(), "v.avhdx", &base, &[(5 * MB, pattern(8, MB))]);

        // Block 1 of the victim: only sectors 1, 34 and 35 are its own.
        let len = std::fs::metadata(&victim).unwrap().len();
        let data_off = len.div_ceil(MB) * MB;
        let bitmap_off = data_off + MB;
        let mut f = OpenOptions::new().write(true).open(&victim).unwrap();
        f.set_len(bitmap_off + MB).unwrap();
        // 0x99 is what the parent's sectors must NOT read as.
        let mut data = vec![0x99u8; MB as usize];
        for s in [1u64, 34, 35] {
            data[(s * 512) as usize..(s * 512 + 512) as usize].fill(0xCC);
        }
        write_all_at(&mut f, data_off, &data).unwrap();
        // Chunk 0's bitmap covers its 4096 blocks; block 1 starts at sector
        // 2048 of the chunk.
        let mut bitmap = vec![0u8; MB as usize];
        for s in [1u64, 34, 35] {
            let bit = 2048 + s;
            bitmap[(bit / 8) as usize] |= 1 << (bit % 8);
        }
        write_all_at(&mut f, bitmap_off, &bitmap).unwrap();
        for (index, entry) in [(1u64, data_off | 7), (4096, bitmap_off | 6)] {
            write_all_at(&mut f, BAT_OFF + index * 8, &entry.to_le_bytes()).unwrap();
        }
        drop(f);

        let view = read_all(&victim);
        let parent_view = read_all(&base);
        let block1 = &view[MB as usize..2 * MB as usize];
        let parent_block1 = &parent_view[MB as usize..2 * MB as usize];
        for s in 0..2048usize {
            let sector = &block1[s * 512..s * 512 + 512];
            if matches!(s, 1 | 34 | 35) {
                assert!(sector.iter().all(|&b| b == 0xCC), "sector {s}");
            } else {
                assert_eq!(sector, &parent_block1[s * 512..s * 512 + 512], "sector {s}");
            }
        }

        fold(&base, &victim).unwrap();
        assert_eq!(read_all(&base), view);
    }

    #[test]
    fn a_fold_refuses_a_victim_of_another_base() {
        let dir = tempfile::tempdir().unwrap();
        let base = base_image(dir.path());
        let other = dir.path().join("other.vhdx");
        std::fs::copy(&base, &other).unwrap();
        let victim = child(dir.path(), "v.avhdx", &base, &[(0, pattern(9, 4096))]);
        let before = std::fs::read(&other).unwrap();
        assert!(fold(&other, &victim).is_err());
        assert_eq!(std::fs::read(&other).unwrap(), before);
    }

    /// The linkage is the check Windows makes before it opens a chain: a base
    /// whose DataWriteGuid moved since the child was made is not its parent.
    #[test]
    fn a_fold_refuses_a_base_changed_since() {
        let dir = tempfile::tempdir().unwrap();
        let base = base_image(dir.path());
        let victim = child(dir.path(), "v.avhdx", &base, &[(0, pattern(9, 4096))]);
        // Give the base a new DataWriteGuid in both header slots.
        let mut f = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&base)
            .unwrap();
        let fresh = Uuid::new_v4().to_bytes_le();
        for off in HEADER_OFFS {
            let mut h = vec![0u8; HEADER_LEN];
            read_exact_at(&mut f, off, &mut h).unwrap();
            h[32..48].copy_from_slice(&fresh);
            h[4..8].fill(0);
            let crc = crc32c(&h);
            h[4..8].copy_from_slice(&crc.to_le_bytes());
            write_all_at(&mut f, off, &h).unwrap();
        }
        drop(f);
        assert!(!vhdx_parent_linkage_matches(&victim, &base).unwrap());
        assert!(fold(&base, &victim).is_err());
    }

    fn interrupt_repoint_at(at: stop::After, child: &Path, parent: &Path) {
        stop::AT.with(|cell| cell.set(Some(at)));
        let result = repoint(child, parent);
        stop::AT.with(|cell| cell.set(None));
        assert!(
            matches!(result, Err(OwnBackupError::Cancelled)),
            "{at:?}: {result:?}"
        );
    }

    /// Every point a crash can stop a repoint at leaves the OLD locator in
    /// force — the data reads the same either way — and the next run
    /// completes it.
    #[test]
    fn an_interrupted_repoint_reads_old_until_the_switch() {
        for at in [
            stop::After::Header,
            stop::After::Metadata,
            stop::After::BackupRegionTable,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (base, inc1, inc2) = chain(dir.path());
            let v2 = read_all(&inc2);
            fold(&base, &inc1).unwrap();

            interrupt_repoint_at(at, &inc2, &base);
            assert_eq!(parent_of(&inc2), inc1.canonicalize().unwrap(), "{at:?}");
            assert_eq!(read_all(&inc2), v2, "{at:?}");

            repoint(&inc2, &base).unwrap();
            assert_eq!(parent_of(&inc2), base.canonicalize().unwrap(), "{at:?}");
            std::fs::remove_file(&inc1).unwrap();
            assert_eq!(read_all(&inc2), v2, "{at:?}");
        }
    }

    /// A torn region table fails its checksum, and the readers take the other
    /// copy: the backup once it is new, the primary while it is still old.
    #[test]
    fn a_torn_region_table_falls_back_to_the_other_copy() {
        for (at, torn, expect_new) in [
            (stop::After::BackupRegionTable, REGION1_OFF, true),
            (stop::After::Metadata, REGION2_OFF, false),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (base, inc1, inc2) = chain(dir.path());
            let v2 = read_all(&inc2);
            fold(&base, &inc1).unwrap();
            interrupt_repoint_at(at, &inc2, &base);

            // Half-written: new bytes over the entries, checksum not matching.
            let mut f = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&inc2)
                .unwrap();
            write_all_at(&mut f, torn + 40, &[0x5Au8; 40]).unwrap();
            drop(f);

            let expected = if expect_new { &base } else { &inc1 };
            assert_eq!(parent_of(&inc2), expected.canonicalize().unwrap(), "{at:?}");
            assert_eq!(read_all(&inc2), v2, "{at:?}");
            // The writer reads the same copy.
            VhdxWriter::open_existing(&inc2).unwrap().finish().unwrap();
            assert_eq!(read_all(&inc2), v2, "{at:?}");

            repoint(&inc2, &base).unwrap();
            std::fs::remove_file(&inc1).unwrap();
            assert_eq!(read_all(&inc2), v2, "{at:?}");
            // And both copies agree again, the primary included.
            let mut f = File::open(&inc2).unwrap();
            let (mut r1, mut r2) = (vec![0u8; 65536], vec![0u8; 65536]);
            read_exact_at(&mut f, REGION1_OFF, &mut r1).unwrap();
            read_exact_at(&mut f, REGION2_OFF, &mut r2).unwrap();
            assert_eq!(r1, r2, "{at:?}");
        }
    }

    #[test]
    fn a_repoint_keeps_the_rest_of_the_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let (base, inc1, inc2) = chain(dir.path());
        let before = vhdx_geometry(&inc2).unwrap();
        fold(&base, &inc1).unwrap();
        repoint(&inc2, &base).unwrap();
        assert_eq!(vhdx_geometry(&inc2).unwrap(), before);

        // The child is still writable after its metadata moved to the end of
        // the file: new blocks land past it and read back.
        let mut expected = read_all(&inc2);
        let mut w = VhdxWriter::open_existing(&inc2).unwrap();
        w.write_at(14 * MB + 3, &pattern(10, 4096)).unwrap();
        w.finish().unwrap();
        expected[(14 * MB + 3) as usize..(14 * MB + 3 + 4096) as usize]
            .copy_from_slice(&pattern(10, 4096));
        assert_eq!(read_all(&inc2), expected);
        assert_eq!(parent_of(&inc2), base.canonicalize().unwrap());
    }

    #[test]
    fn a_repoint_refuses_another_disk() {
        let dir = tempfile::tempdir().unwrap();
        let (_, inc1, _) = chain(dir.path());
        let small = dir.path().join("small.vhdx");
        VhdxWriter::create(&small, VSIZE / 2, 2 * MB, 512)
            .unwrap()
            .finish()
            .unwrap();
        let before = std::fs::read(&inc1).unwrap();
        assert!(repoint(&inc1, &small).is_err());
        assert!(repoint(&inc1, &inc1).is_err());
        assert_eq!(std::fs::read(&inc1).unwrap(), before);
    }
}
