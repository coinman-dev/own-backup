//! Used-block map for XFS, parsed read-only from the volume.
//!
//! Windows has no native XFS driver, so — as [`crate::ext_map`]/[`crate::btrfs_map`]
//! do — we read the on-disk structures ourselves. Unlike ext/btrfs (which record
//! *allocated* space), XFS records *free* space in a per-allocation-group B+tree
//! (`bnobt`, free space by block). So we mark everything used and then unmark
//! the free extents; metadata, the log and allocated data are never in that
//! tree, so they stay copied.
//!
//! Safety: after walking each AG's free tree we require its total to equal the
//! AGF's own `agf_freeblks` count — if they disagree we've misparsed, so we
//! error and the engine falls back to a full sector-by-sector copy. Combined
//! with per-block magic checks and in-AG bounds, a free block can never be
//! reported as one we may skip. All XFS on-disk integers are big-endian.

use tracing::info;

use own_backup_common::{OwnBackupError, Result};

use crate::fat_map::RawVolume;
use crate::used_sectors::UsedSectorMap;

/// XFS superblock magic "XFSB".
const SB_MAGIC: u64 = 0x5846_5342;
/// AGF header magic "XAGF".
const AGF_MAGIC: u64 = 0x5841_4746;
/// Free-space-by-block B+tree block magic: "ABTB" (v4) / "AB3B" (v5-crc).
const ABTB_MAGIC_V4: u64 = 0x4142_5442;
const ABTB_MAGIC_V5: u64 = 0x4142_3342;

/// Short-form B+tree block header lengths (v4 vs v5-crc).
const BTREE_HDR_V4: u64 = 16;
const BTREE_HDR_V5: u64 = 56;
/// `xfs_alloc_rec` / `xfs_alloc_key` size, and `xfs_alloc_ptr` size.
const REC_SIZE: u64 = 8;
const PTR_SIZE: u64 = 4;

const MAX_BTREE_BLOCKS: u64 = 4_000_000;
const MAX_AGS: u64 = 4_000_000;

/// Per-backup-block used map for an XFS volume.
pub struct XfsUsedMap {
    block_used: Vec<bool>,
    used_count: u64,
    /// XFS filesystem block size in bytes.
    pub block_size: u32,
}

impl UsedSectorMap for XfsUsedMap {
    fn is_block_used(&self, block_number: u64) -> bool {
        self.block_used.get(block_number as usize).copied().unwrap_or(false)
    }
    fn used_block_count(&self) -> u64 {
        self.used_count
    }
}

impl XfsUsedMap {
    /// Drop every backup block that lies ENTIRELY within `ranges` (excluded,
    /// exclusively-owned file data — plan D2) so it is not copied. `backup_block_size`
    /// is the block size the map was built with.
    pub fn apply_excluded_ranges(
        &mut self,
        ranges: &crate::exclusions::ByteRanges,
        backup_block_size: u32,
    ) {
        let bs = backup_block_size as u64;
        for (i, used) in self.block_used.iter_mut().enumerate() {
            if *used {
                let start = i as u64 * bs;
                if ranges.covers(start, start + bs) {
                    *used = false;
                    self.used_count -= 1;
                }
            }
        }
    }
}

fn bad(msg: impl std::fmt::Display) -> OwnBackupError {
    OwnBackupError::Device(format!("xfs map: {msg}"))
}

// XFS is big-endian.
fn u16at(b: &[u8], o: usize) -> u64 {
    u16::from_be_bytes(b[o..o + 2].try_into().unwrap()) as u64
}
fn u32at(b: &[u8], o: usize) -> u64 {
    u32::from_be_bytes(b[o..o + 4].try_into().unwrap()) as u64
}
fn u64at(b: &[u8], o: usize) -> u64 {
    u64::from_be_bytes(b[o..o + 8].try_into().unwrap())
}

/// Clear (mark free) every backup block *fully* inside `[start, end)` bytes. A
/// backup block straddling a used/free boundary stays set, so no used data is
/// ever dropped.
fn unmark_fully_free(map: &mut [bool], block_size: u64, start: u64, end: u64) {
    if end <= start {
        return;
    }
    let first = start.div_ceil(block_size);
    let last_excl = end / block_size; // floor: block B fits iff (B+1)*bs <= end
    if last_excl == 0 {
        return;
    }
    let last = last_excl - 1;
    if first > last {
        return;
    }
    for b in map
        .iter_mut()
        .take((last + 1) as usize)
        .skip(first as usize)
    {
        *b = false;
    }
}

/// Walk one AG's `bnobt` free-space B+tree, pushing `(ag_relative_start, count)`
/// free extents (in filesystem blocks) into `out`. `agf_length` is the AG size
/// in blocks; every btree block and free extent must lie inside it.
#[allow(clippy::too_many_arguments)]
fn walk_bnobt(
    vol: &mut dyn RawVolume,
    block_size: u64,
    ag_start_block: u64,
    root_block: u64,
    agf_length: u64,
    out: &mut Vec<(u64, u64)>,
) -> Result<()> {
    let mut stack = vec![root_block];
    let mut visited = 0u64;
    while let Some(agrel) = stack.pop() {
        visited += 1;
        if visited > MAX_BTREE_BLOCKS {
            return Err(bad("bnobt too large or cyclic"));
        }
        if agrel >= agf_length {
            return Err(bad(format!("bnobt block {agrel} outside the AG")));
        }
        let byte = (ag_start_block + agrel) * block_size;
        if byte + block_size > vol.len() {
            return Err(bad("bnobt block runs past the device"));
        }
        let mut node = vec![0u8; block_size as usize];
        vol.read_at(byte, &mut node)?;

        let header = match u32at(&node, 0) {
            ABTB_MAGIC_V4 => BTREE_HDR_V4,
            ABTB_MAGIC_V5 => BTREE_HDR_V5,
            other => return Err(bad(format!("bad bnobt block magic {other:#x}"))),
        };
        let level = u16at(&node, 4);
        let numrecs = u16at(&node, 6);

        if level == 0 {
            // Leaf: `numrecs` free-space records of REC_SIZE bytes.
            let maxrecs = (block_size - header) / REC_SIZE;
            if numrecs > maxrecs {
                return Err(bad("bnobt leaf numrecs out of range"));
            }
            for i in 0..numrecs {
                let r = (header + i * REC_SIZE) as usize;
                let startblock = u32at(&node, r);
                let count = u32at(&node, r + 4);
                if count == 0 || startblock + count > agf_length {
                    return Err(bad("free extent outside the AG"));
                }
                out.push((ag_start_block + startblock, count));
            }
        } else {
            // Node: keys[maxrecs] then ptrs[maxrecs]; only `numrecs` are valid.
            let maxrecs = (block_size - header) / (REC_SIZE + PTR_SIZE);
            if numrecs == 0 || numrecs > maxrecs {
                return Err(bad("bnobt node numrecs out of range"));
            }
            let ptr_base = header + maxrecs * REC_SIZE;
            for i in 0..numrecs {
                let child = u32at(&node, (ptr_base + i * PTR_SIZE) as usize);
                if child == 0 || child >= agf_length {
                    return Err(bad("bnobt child pointer outside the AG"));
                }
                stack.push(child);
            }
        }
    }
    Ok(())
}

/// Detect XFS on `vol` and build the used map. Errors (→ caller falls back to
/// sector-by-sector) on anything unrecognized or inconsistent.
pub fn build_xfs_used_map(vol: &mut dyn RawVolume, block_size: u32) -> Result<XfsUsedMap> {
    // --- Superblock at offset 0 ---
    let mut sb = [0u8; 512];
    vol.read_at(0, &mut sb)?;
    if u32at(&sb, 0) != SB_MAGIC {
        return Err(bad("no XFSB superblock magic"));
    }
    let fs_block = u32at(&sb, 0x04);
    let dblocks = u64at(&sb, 0x08);
    let agblocks = u32at(&sb, 0x54);
    let agcount = u32at(&sb, 0x58);
    let sectsize = u16at(&sb, 0x66);

    if !fs_block.is_power_of_two() || !(512..=65536).contains(&fs_block) {
        return Err(bad(format!("implausible block size {fs_block}")));
    }
    if !sectsize.is_power_of_two() || !(512..=65536).contains(&sectsize) {
        return Err(bad(format!("implausible sector size {sectsize}")));
    }
    if agblocks == 0 || agcount == 0 || agcount > MAX_AGS {
        return Err(bad(format!("implausible geometry: agblocks={agblocks} agcount={agcount}")));
    }
    if dblocks == 0 || dblocks > agblocks * agcount {
        return Err(bad(format!("implausible dblocks {dblocks}")));
    }
    // An unreplayed log means these btrees are older than the committed state:
    // blocks allocated in those transactions can still read as free, and a map
    // built from them would leave live data out of the copy.
    let logstart = u64at(&sb, 0x30);
    let logblocks = u32at(&sb, 0x60);
    let agblklog = sb[0x7C] as u32;
    if logstart == 0 {
        return Err(bad("внешний журнал xfs — проверить его нечем, копируем посекторно"));
    }
    let log_off = ((logstart >> agblklog) * agblocks + (logstart & ((1u64 << agblklog) - 1))) * fs_block;
    if !crate::xfs_exclusions::xfs_log_is_clean_at(vol, log_off, logblocks * fs_block, sectsize)? {
        return Err(bad("журнал xfs не пуст (нечистое завершение) — копируем посекторно"));
    }

    let fs_bytes = dblocks
        .checked_mul(fs_block)
        .ok_or_else(|| bad("dblocks * block_size overflows"))?;
    if fs_bytes > vol.len() {
        return Err(bad(format!(
            "filesystem claims {fs_bytes} bytes but the device is {}",
            vol.len()
        )));
    }

    // --- Everything used; free extents (below) unmark whole free backup blocks ---
    let total_backup = vol.len().div_ceil(block_size as u64);
    let mut used = vec![true; total_backup as usize];

    let mut free_extents = Vec::new();
    for ag in 0..agcount {
        let ag_start_block = ag * agblocks;
        // Read sector 0 (superblock copy) + sector 1 (AGF) of the AG; the read
        // starts on a filesystem-block boundary, so it is device-aligned.
        let mut head = vec![0u8; (2 * sectsize) as usize];
        vol.read_at(ag_start_block * fs_block, &mut head)?;
        let agf = &head[sectsize as usize..];

        if u32at(agf, 0) != AGF_MAGIC {
            return Err(bad(format!("no AGF magic in AG {ag}")));
        }
        if u32at(agf, 0x08) != ag {
            return Err(bad(format!("AGF seqno mismatch in AG {ag}")));
        }
        let agf_length = u32at(agf, 0x0C);
        let bno_root = u32at(agf, 0x10); // agf_roots[BNO]
        let bno_levels = u32at(agf, 0x1C); // agf_levels[BNO]
        let agf_freeblks = u32at(agf, 0x34);
        if agf_length == 0 || agf_length > agblocks {
            return Err(bad(format!("AG {ag} bad length {agf_length}")));
        }
        if bno_root == 0 || bno_root >= agf_length || bno_levels == 0 || bno_levels > 20 {
            return Err(bad(format!("AG {ag} bad bnobt root/levels")));
        }

        free_extents.clear();
        walk_bnobt(vol, fs_block, ag_start_block, bno_root, agf_length, &mut free_extents)?;

        // The tree must account for exactly the AGF's free block count — our
        // strongest guard against a misparse dropping used data.
        let free_sum: u64 = free_extents.iter().map(|(_, c)| *c).sum();
        if free_sum != agf_freeblks {
            return Err(bad(format!(
                "AG {ag}: bnobt free {free_sum} != agf_freeblks {agf_freeblks}"
            )));
        }
        for &(start, count) in &free_extents {
            let start_b = start * fs_block;
            unmark_fully_free(&mut used, block_size as u64, start_b, start_b + count * fs_block);
        }
    }

    let used_count = used.iter().filter(|&&u| u).count() as u64;
    info!(
        fs = "xfs",
        fs_block_size = fs_block,
        agcount,
        dblocks,
        used_blocks = used_count,
        total_blocks = total_backup,
        "xfs used-block map ready"
    );
    Ok(XfsUsedMap {
        block_used: used,
        used_count,
        block_size: fs_block as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MemVolume(Vec<u8>);
    impl RawVolume for MemVolume {
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
            let (s, e) = (offset as usize, offset as usize + buf.len());
            if e > self.0.len() {
                return Err(bad("read past end"));
            }
            buf.copy_from_slice(&self.0[s..e]);
            Ok(())
        }
        fn len(&self) -> u64 {
            self.0.len() as u64
        }
    }

    fn put_u16(b: &mut [u8], o: usize, v: u16) {
        b[o..o + 2].copy_from_slice(&v.to_be_bytes());
    }
    fn put_u32(b: &mut [u8], o: usize, v: u32) {
        b[o..o + 4].copy_from_slice(&v.to_be_bytes());
    }
    fn put_u64(b: &mut [u8], o: usize, v: u64) {
        b[o..o + 8].copy_from_slice(&v.to_be_bytes());
    }

    /// Tiny single-AG XFS: 512-byte blocks/sectors, 64 blocks. AGF at block 1,
    /// bnobt root (a leaf) at block 4 with free extents [20,+10) and [40,+20).
    fn tiny_xfs() -> MemVolume {
        let bs = 512usize;
        let mut img = vec![0u8; 64 * bs];

        // Superblock (block 0 / sector 0).
        put_u32(&mut img, 0x00, 0x5846_5342); // XFSB
        put_u32(&mut img, 0x04, 512); // blocksize
        put_u64(&mut img, 0x08, 64); // dblocks
        put_u32(&mut img, 0x54, 64); // agblocks
        put_u32(&mut img, 0x58, 1); // agcount
        // An internal log at blocks 50..54, left all zeros — which is what a
        // freshly made, never-mounted log looks like, and what the "is the log
        // clean" check has to accept. logstart = 0 would mean an EXTERNAL log
        // device, i.e. one this code cannot inspect and must refuse.
        put_u64(&mut img, 0x30, 50); // logstart (fsblock: ag 0, block 50)
        put_u32(&mut img, 0x60, 4); // logblocks
        img[0x7C] = 6; // agblklog — log2(agblocks); without it fsblock→AG is wrong
        put_u16(&mut img, 0x66, 512); // sectsize
        put_u64(&mut img, 0x90, 30); // fdblocks

        // AGF at sector 1 (byte 512).
        let agf = bs;
        put_u32(&mut img, agf + 0x00, 0x5841_4746); // XAGF
        put_u32(&mut img, agf + 0x08, 0); // seqno
        put_u32(&mut img, agf + 0x0C, 64); // length
        put_u32(&mut img, agf + 0x10, 4); // bnobt root = AG block 4
        put_u32(&mut img, agf + 0x1C, 1); // bnobt levels
        put_u32(&mut img, agf + 0x34, 30); // freeblks

        // bnobt root leaf at block 4 (byte 2048): 2 free extents.
        let bt = 4 * bs;
        put_u32(&mut img, bt + 0x00, 0x4142_5442); // ABTB (v4)
        put_u16(&mut img, bt + 0x04, 0); // level 0 (leaf)
        put_u16(&mut img, bt + 0x06, 2); // numrecs
        put_u32(&mut img, bt + 16, 20); // rec0 startblock
        put_u32(&mut img, bt + 20, 10); // rec0 count
        put_u32(&mut img, bt + 24, 40); // rec1 startblock
        put_u32(&mut img, bt + 28, 20); // rec1 count
        MemVolume(img)
    }

    #[test]
    fn marks_used_and_unmarks_free_extents() {
        let mut vol = tiny_xfs();
        // Backup block size == fs block size → block N maps to fs block N.
        let map = build_xfs_used_map(&mut vol, 512).unwrap();
        assert_eq!(map.block_size, 512);

        assert!(map.is_block_used(0)); // superblock
        assert!(map.is_block_used(4)); // bnobt root
        assert!(map.is_block_used(19));
        for b in 20..30 {
            assert!(!map.is_block_used(b), "free block {b}");
        }
        assert!(map.is_block_used(30));
        for b in 40..60 {
            assert!(!map.is_block_used(b), "free block {b}");
        }
        assert!(map.is_block_used(60));
        assert!(map.is_block_used(63));
        assert_eq!(map.used_block_count(), 64 - 30);
    }

    #[test]
    fn rejects_free_count_mismatch() {
        // agf_freeblks disagreeing with the tree total must fail (→ RAW).
        let mut img = tiny_xfs().0;
        put_u32(&mut img, 512 + 0x34, 999);
        assert!(build_xfs_used_map(&mut MemVolume(img), 512).is_err());
    }

    #[test]
    fn rejects_non_xfs() {
        assert!(build_xfs_used_map(&mut MemVolume(vec![0u8; 64 * 512]), 512).is_err());
    }
}
