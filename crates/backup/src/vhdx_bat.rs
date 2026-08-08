//! Portable BAT scanner for VHDX files (dynamic **and** differencing).
//!
//! Answers one question: *which payload blocks does this VHDX file itself
//! carry?* For a differencing child that is exactly the set of virtual-disk
//! regions the guest (or the NTFS/ReFS driver, during the exclusion
//! mount-&-delete pass — see [`crate::true_removal_win`]) has written or
//! unmapped — i.e. everything that differs from the parent. The caller uses
//! it to re-checksum only the touched backup blocks after a
//! `MergeVirtualDisk`.
//!
//! Read-only, plain `Seek`/`Read` I/O so it compiles and unit-tests on every
//! platform (unlike [`crate::vhdx_format`], which is Linux-only).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use uuid::Uuid;

use own_backup_common::{OwnBackupError, Result};

/// Region table 1 lives at a spec-fixed offset.
const REGION1_OFF: u64 = 192 * 1024;

// Region / metadata-item GUIDs (same literals as vhdx_format.rs).
const BAT_GUID: &str = "2dc27766-f623-4200-9d64-115e9bfd4a08";
const META_GUID: &str = "8b7ca206-4790-4b9a-b8fe-575f050f886e";
const MI_FILE_PARAMS: &str = "caa16737-fa36-4d43-b3b6-33f0aa44e76b";
const MI_VDISK_SIZE: &str = "2fa54224-cd1b-4876-b211-5dbed83bf4b8";
const MI_LOG_SECTOR: &str = "8141bf1d-a96f-4709-ba47-f233a8faab5f";
const MI_PARENT_LOCATOR: &str = "a8d35f2d-b30b-454d-abf7-d3d84834ab0c";

/// BAT payload-block state `SB_BLOCK_NOT_PRESENT` — the only state that means
/// "this file carries nothing for the block" (differencing: inherit from the
/// parent). Every other state (UNDEFINED/ZERO/UNMAPPED/FULLY/PARTIALLY
/// present) means the block's content differs from the parent's.
const PAYLOAD_BLOCK_NOT_PRESENT: u64 = 0;

fn bad(msg: impl std::fmt::Display) -> OwnBackupError {
    OwnBackupError::Vhdx(format!("vhdx bat: {msg}"))
}

fn guid_le(s: &str) -> [u8; 16] {
    Uuid::parse_str(s).expect("valid GUID literal").to_bytes_le()
}

fn read_exact_at(file: &mut File, off: u64, buf: &mut [u8]) -> Result<()> {
    file.seek(SeekFrom::Start(off)).map_err(bad)?;
    file.read_exact(buf).map_err(bad)
}

/// The payload blocks a VHDX file carries itself (BAT state ≠ NOT_PRESENT).
pub struct VhdxAllocation {
    /// Payload block size in bytes (1 MiB for images we create).
    pub block_size: u64,
    /// Virtual disk size in bytes.
    pub virtual_size: u64,
    /// 0-based payload block indices present in THIS file, ascending.
    pub blocks: Vec<u64>,
}

impl VhdxAllocation {
    /// Virtual-disk byte range `[start, end)` of payload block `i`.
    pub fn block_range(&self, i: u64) -> (u64, u64) {
        let start = i * self.block_size;
        (start, (start + self.block_size).min(self.virtual_size))
    }
}

/// Scan the BAT of `path` and list the payload blocks the file itself carries.
pub fn allocated_payload_blocks(path: &Path) -> Result<VhdxAllocation> {
    let mut file = File::open(path).map_err(|e| bad(format!("open {}: {e}", path.display())))?;

    // File-type identifier.
    let mut id = [0u8; 8];
    read_exact_at(&mut file, 0, &mut id)?;
    if &id != b"vhdxfile" {
        return Err(bad("not a VHDX file (no signature)"));
    }

    // Region table: locate the BAT and metadata regions.
    let mut region = vec![0u8; 64 * 1024];
    read_exact_at(&mut file, REGION1_OFF, &mut region)?;
    if &region[0..4] != b"regi" {
        return Err(bad("no region table signature"));
    }
    let entry_count = u32::from_le_bytes(region[8..12].try_into().unwrap()) as usize;
    let (mut bat_off, mut bat_len, mut meta_off, mut meta_len) = (0u64, 0u64, 0u64, 0u64);
    for slot in 0..entry_count.min(2047) {
        let base = 16 + slot * 32;
        let guid = &region[base..base + 16];
        let off = u64::from_le_bytes(region[base + 16..base + 24].try_into().unwrap());
        let len = u32::from_le_bytes(region[base + 24..base + 28].try_into().unwrap()) as u64;
        if guid == guid_le(BAT_GUID) {
            (bat_off, bat_len) = (off, len);
        } else if guid == guid_le(META_GUID) {
            (meta_off, meta_len) = (off, len);
        }
    }
    if bat_off == 0 || meta_off == 0 {
        return Err(bad("region table missing BAT or metadata"));
    }

    // Metadata: block size, logical sector size, virtual disk size.
    let mut meta = vec![0u8; meta_len.min(1024 * 1024) as usize];
    read_exact_at(&mut file, meta_off, &mut meta)?;
    if &meta[0..8] != b"metadata" {
        return Err(bad("no metadata signature"));
    }
    let mcount = u16::from_le_bytes(meta[10..12].try_into().unwrap()) as usize;
    let (mut vsize, mut bsize, mut ssize) = (0u64, 0u64, 512u32);
    for slot in 0..mcount.min(2046) {
        let e = 32 + slot * 32;
        let guid = &meta[e..e + 16];
        let ioff = u32::from_le_bytes(meta[e + 16..e + 20].try_into().unwrap()) as usize;
        if ioff + 8 > meta.len() {
            continue;
        }
        if guid == guid_le(MI_VDISK_SIZE) {
            vsize = u64::from_le_bytes(meta[ioff..ioff + 8].try_into().unwrap());
        } else if guid == guid_le(MI_FILE_PARAMS) {
            bsize = u32::from_le_bytes(meta[ioff..ioff + 4].try_into().unwrap()) as u64;
        } else if guid == guid_le(MI_LOG_SECTOR) {
            ssize = u32::from_le_bytes(meta[ioff..ioff + 4].try_into().unwrap());
        }
    }
    if vsize == 0 || bsize == 0 || ssize == 0 {
        return Err(bad("metadata missing virtual/block/sector size"));
    }

    // BAT: payload entries interleaved with one sector-bitmap entry every
    // `chunk_ratio` payload blocks (MS-VHDX §2.4).
    let chunk_ratio = (1u64 << 23) * ssize as u64 / bsize;
    let data_blocks = vsize.div_ceil(bsize);
    let mut raw = vec![0u8; bat_len as usize];
    read_exact_at(&mut file, bat_off, &mut raw)?;

    let mut blocks = Vec::new();
    for i in 0..data_blocks {
        let bat_index = (i + i / chunk_ratio) as usize;
        let Some(entry) = raw.get(bat_index * 8..bat_index * 8 + 8) else {
            break;
        };
        let entry = u64::from_le_bytes(entry.try_into().unwrap());
        if entry & 7 != PAYLOAD_BLOCK_NOT_PRESENT {
            blocks.push(i);
        }
    }

    Ok(VhdxAllocation { block_size: bsize, virtual_size: vsize, blocks })
}

/// Where one guest offset's bytes live, up to the end of its payload block.
enum GuestRun {
    /// THIS file carries the bytes at `file_offset`, for `run` bytes.
    Data { file_offset: u64, run: u64 },
    /// This file carries nothing for `run` bytes (differencing child: inherit
    /// from the parent; dynamic image: reads as zeros). `state` is the raw BAT
    /// block state, kept for error messages on the write path.
    Absent { run: u64, state: u64 },
}

/// Translate one guest offset into a [`GuestRun`]: the bytes till the end of
/// the containing payload block. VHDX payload blocks are NOT adjacent in the
/// file, so any read/write spanning a block boundary must re-translate per
/// block — a single contiguous file access past the run touches whatever
/// happens to live there (BAT/bitmap/another block).
fn guest_to_file_offset_in_file(file: &mut File, guest_offset: u64) -> Result<GuestRun> {
    // 1. File-type identifier.
    let mut id = [0u8; 8];
    read_exact_at(file, 0, &mut id)?;
    if &id != b"vhdxfile" {
        return Err(bad("not a VHDX file (no signature)"));
    }

    // 2. Region table.
    let mut region = vec![0u8; 64 * 1024];
    read_exact_at(file, REGION1_OFF, &mut region)?;
    if &region[0..4] != b"regi" {
        return Err(bad("no region table signature"));
    }
    let entry_count = u32::from_le_bytes(region[8..12].try_into().unwrap()) as usize;
    let (mut bat_off, mut bat_len, mut meta_off, mut meta_len) = (0u64, 0u64, 0u64, 0u64);
    for slot in 0..entry_count.min(2047) {
        let base = 16 + slot * 32;
        let guid = &region[base..base + 16];
        let off = u64::from_le_bytes(region[base + 16..base + 24].try_into().unwrap());
        let len = u32::from_le_bytes(region[base + 24..base + 28].try_into().unwrap()) as u64;
        if guid == guid_le(BAT_GUID) {
            (bat_off, bat_len) = (off, len);
        } else if guid == guid_le(META_GUID) {
            (meta_off, meta_len) = (off, len);
        }
    }
    if bat_off == 0 || meta_off == 0 {
        return Err(bad("region table missing BAT or metadata"));
    }

    // 3. Metadata.
    let mut meta = vec![0u8; meta_len.min(1024 * 1024) as usize];
    read_exact_at(file, meta_off, &mut meta)?;
    if &meta[0..8] != b"metadata" {
        return Err(bad("no metadata signature"));
    }
    let mcount = u16::from_le_bytes(meta[10..12].try_into().unwrap()) as usize;
    let (mut vsize, mut bsize, mut ssize) = (0u64, 0u64, 512u32);
    for slot in 0..mcount.min(2046) {
        let e = 32 + slot * 32;
        let guid = &meta[e..e + 16];
        let ioff = u32::from_le_bytes(meta[e + 16..e + 20].try_into().unwrap()) as usize;
        if ioff + 8 > meta.len() {
            continue;
        }
        if guid == guid_le(MI_VDISK_SIZE) {
            vsize = u64::from_le_bytes(meta[ioff..ioff + 8].try_into().unwrap());
        } else if guid == guid_le(MI_FILE_PARAMS) {
            bsize = u32::from_le_bytes(meta[ioff..ioff + 4].try_into().unwrap()) as u64;
        } else if guid == guid_le(MI_LOG_SECTOR) {
            ssize = u32::from_le_bytes(meta[ioff..ioff + 4].try_into().unwrap());
        }
    }
    if vsize == 0 || bsize == 0 || ssize == 0 {
        return Err(bad("metadata missing virtual/block/sector size"));
    }

    // 4. BAT lookup.
    if guest_offset >= vsize {
        return Err(bad(format!("guest offset {guest_offset} out of bounds (virtual size {vsize})")));
    }

    let payload_block_index = guest_offset / bsize;
    let block_offset = guest_offset % bsize;

    let chunk_ratio = (1u64 << 23) * ssize as u64 / bsize;
    let bat_entry_index = (payload_block_index + payload_block_index / chunk_ratio) as usize;

    if (bat_entry_index * 8 + 8) as u64 > bat_len {
        return Err(bad("guest offset BAT entry out of BAT bounds"));
    }

    let mut entry_buf = [0u8; 8];
    read_exact_at(file, bat_off + bat_entry_index as u64 * 8, &mut entry_buf)?;
    let entry = u64::from_le_bytes(entry_buf);

    let state = entry & 7;
    let run = bsize - block_offset;
    if state != 6 && state != 7 {
        return Ok(GuestRun::Absent { run, state });
    }
    let file_offset = entry & 0xFFFFFFFFFFF00000;
    if file_offset == 0 {
        return Err(bad(format!("block state {state} with a zero file offset — corrupt BAT")));
    }
    Ok(GuestRun::Data { file_offset: file_offset + block_offset, run })
}

/// Read the metadata region of an open VHDX file (signature-checked).
fn read_metadata_region(file: &mut File) -> Result<Vec<u8>> {
    let mut id = [0u8; 8];
    read_exact_at(file, 0, &mut id)?;
    if &id != b"vhdxfile" {
        return Err(bad("not a VHDX file (no signature)"));
    }
    let mut region = vec![0u8; 64 * 1024];
    read_exact_at(file, REGION1_OFF, &mut region)?;
    if &region[0..4] != b"regi" {
        return Err(bad("no region table signature"));
    }
    let entry_count = u32::from_le_bytes(region[8..12].try_into().unwrap()) as usize;
    for slot in 0..entry_count.min(2047) {
        let base = 16 + slot * 32;
        if &region[base..base + 16] == guid_le(META_GUID) {
            let off = u64::from_le_bytes(region[base + 16..base + 24].try_into().unwrap());
            let len = u32::from_le_bytes(region[base + 24..base + 28].try_into().unwrap()) as u64;
            let mut meta = vec![0u8; len.min(1024 * 1024) as usize];
            read_exact_at(file, off, &mut meta)?;
            if &meta[0..8] != b"metadata" {
                return Err(bad("no metadata signature"));
            }
            return Ok(meta);
        }
    }
    Err(bad("region table missing metadata"))
}

/// Extract the parent path string from a metadata region's parent locator
/// (MS-VHDX §2.6.3), preferring `relative_path`. `None` for dynamic images.
fn parse_parent_locator(meta: &[u8]) -> Option<String> {
    let mcount = u16::from_le_bytes(meta.get(10..12)?.try_into().ok()?) as usize;
    let (mut ioff, mut ilen) = (0usize, 0usize);
    for slot in 0..mcount.min(2046) {
        let e = 32 + slot * 32;
        if meta.get(e..e + 16)? == guid_le(MI_PARENT_LOCATOR) {
            ioff = u32::from_le_bytes(meta[e + 16..e + 20].try_into().ok()?) as usize;
            ilen = u32::from_le_bytes(meta[e + 20..e + 24].try_into().ok()?) as usize;
            break;
        }
    }
    if ioff == 0 || ilen < 20 {
        return None;
    }
    let locator = meta.get(ioff..ioff + ilen)?;
    let count = u16::from_le_bytes(locator[18..20].try_into().ok()?) as usize;

    let read_utf16 = |off: usize, len: usize| -> Option<String> {
        let bytes = locator.get(off..off + len)?;
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes(c.try_into().unwrap()))
            .collect();
        Some(String::from_utf16_lossy(&units))
    };

    let mut best: Option<(u8, String)> = None; // (priority, value) — lower wins
    for i in 0..count.min(64) {
        let e = 20 + i * 12;
        let entry = locator.get(e..e + 12)?;
        let key_off = u32::from_le_bytes(entry[0..4].try_into().ok()?) as usize;
        let val_off = u32::from_le_bytes(entry[4..8].try_into().ok()?) as usize;
        let key_len = u16::from_le_bytes(entry[8..10].try_into().ok()?) as usize;
        let val_len = u16::from_le_bytes(entry[10..12].try_into().ok()?) as usize;
        let Some(key) = read_utf16(key_off, key_len) else { continue };
        let priority = match key.as_str() {
            "relative_path" => 0u8,
            "absolute_win32_path" => 1,
            "volume_path" => 2,
            _ => continue,
        };
        if best.as_ref().map_or(true, |(p, _)| priority < *p) {
            if let Some(value) = read_utf16(val_off, val_len) {
                best = Some((priority, value));
            }
        }
    }
    best.map(|(_, v)| v)
}

/// Resolve a parent-locator path against the child image's directory.
fn resolve_parent(child: &Path, raw: &str) -> std::path::PathBuf {
    let s = raw.trim_start_matches(r"\\?\");
    #[cfg(not(target_os = "windows"))]
    let s = s.replace('\\', "/");
    #[cfg(target_os = "windows")]
    let s = s.to_string();
    let p = Path::new(&s);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        let rel = s.trim_start_matches("./").trim_start_matches(r".\");
        child.parent().unwrap_or_else(|| Path::new(".")).join(rel)
    }
}

/// Parent image path of a differencing VHDX (from its parent locator),
/// resolved against the child's directory; `None` for dynamic images.
pub fn vhdx_parent_path(path: &Path) -> Result<Option<std::path::PathBuf>> {
    let mut file = File::open(path).map_err(|e| bad(format!("open {}: {e}", path.display())))?;
    let meta = read_metadata_region(&mut file)?;
    Ok(parse_parent_locator(&meta).map(|raw| resolve_parent(path, &raw)))
}

/// Longest accepted parent chain (a loop is corruption, real chains are short).
const MAX_CHAIN_DEPTH: usize = 64;

/// A VHDX opened for repeated guest-offset reads: geometry parsed and the BAT
/// slurped ONCE, so each read costs only the payload I/O.
///
/// [`read_vhdx_guest_at`] re-parsed the header, region table, metadata and one
/// BAT entry for EVERY payload block it crossed — four small reads at the
/// start of the file interleaved with each 1 MiB read far into it. On the HDD
/// this turned the re-checksum pass into a head shuttle: ~74 ms per block,
/// identical whether the payload reads were batched or not (measured
/// 2026-08-08: 268 s for 3.4 GiB — the same as reading the attached disk).
/// The whole BAT of an 84 GB image is ~700 KiB: one read, kept in memory.
pub struct VhdxGuestReader {
    file: File,
    virtual_size: u64,
    block_size: u64,
    chunk_ratio: u64,
    bat: Vec<u8>,
    /// Parent, opened lazily on the first absent block (differencing images).
    parent_path: Option<std::path::PathBuf>,
    parent: Option<Box<VhdxGuestReader>>,
    depth: usize,
}

impl VhdxGuestReader {
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_at_depth(path, 0)
    }

    fn open_at_depth(path: &Path, depth: usize) -> Result<Self> {
        if depth >= MAX_CHAIN_DEPTH {
            return Err(bad(format!(
                "parent chain deeper than {MAX_CHAIN_DEPTH} images — corrupted chain?"
            )));
        }
        let mut file =
            File::open(path).map_err(|e| bad(format!("open {}: {e}", path.display())))?;

        let mut id = [0u8; 8];
        read_exact_at(&mut file, 0, &mut id)?;
        if &id != b"vhdxfile" {
            return Err(bad("not a VHDX file (no signature)"));
        }
        let mut region = vec![0u8; 64 * 1024];
        read_exact_at(&mut file, REGION1_OFF, &mut region)?;
        if &region[0..4] != b"regi" {
            return Err(bad("no region table signature"));
        }
        let entry_count = u32::from_le_bytes(region[8..12].try_into().unwrap()) as usize;
        let (mut bat_off, mut bat_len) = (0u64, 0u64);
        for slot in 0..entry_count.min(2047) {
            let base = 16 + slot * 32;
            if &region[base..base + 16] == guid_le(BAT_GUID) {
                bat_off = u64::from_le_bytes(region[base + 16..base + 24].try_into().unwrap());
                bat_len =
                    u32::from_le_bytes(region[base + 24..base + 28].try_into().unwrap()) as u64;
            }
        }
        if bat_off == 0 {
            return Err(bad("region table missing BAT"));
        }

        let meta = read_metadata_region(&mut file)?;
        let mcount = u16::from_le_bytes(meta[10..12].try_into().unwrap()) as usize;
        let (mut vsize, mut bsize, mut ssize) = (0u64, 0u64, 512u32);
        for slot in 0..mcount.min(2046) {
            let e = 32 + slot * 32;
            let guid = &meta[e..e + 16];
            let ioff = u32::from_le_bytes(meta[e + 16..e + 20].try_into().unwrap()) as usize;
            if ioff + 8 > meta.len() {
                continue;
            }
            if guid == guid_le(MI_VDISK_SIZE) {
                vsize = u64::from_le_bytes(meta[ioff..ioff + 8].try_into().unwrap());
            } else if guid == guid_le(MI_FILE_PARAMS) {
                bsize = u32::from_le_bytes(meta[ioff..ioff + 4].try_into().unwrap()) as u64;
            } else if guid == guid_le(MI_LOG_SECTOR) {
                ssize = u32::from_le_bytes(meta[ioff..ioff + 4].try_into().unwrap());
            }
        }
        if vsize == 0 || bsize == 0 || ssize == 0 {
            return Err(bad("metadata missing virtual/block/sector size"));
        }

        // The whole BAT in one read: ~8 bytes per payload block, i.e. well under
        // a megabyte for the images we make. Capped so a corrupt length cannot
        // ask for gigabytes.
        const MAX_BAT: u64 = 32 * 1024 * 1024;
        let bat_len = bat_len.min(MAX_BAT);
        let mut bat = vec![0u8; bat_len as usize];
        read_exact_at(&mut file, bat_off, &mut bat)?;

        let parent_path = parse_parent_locator(&meta).map(|raw| resolve_parent(path, &raw));
        Ok(Self {
            file,
            virtual_size: vsize,
            block_size: bsize,
            chunk_ratio: (1u64 << 23) * ssize as u64 / bsize,
            bat,
            parent_path,
            parent: None,
            depth,
        })
    }

    /// Where `guest_offset` lives: `Ok(Some(file_offset))` in this file,
    /// `Ok(None)` absent here (parent or zeros). Also returns the run length
    /// to the end of the payload block.
    fn locate(&self, guest_offset: u64) -> Result<(Option<u64>, u64)> {
        let block = guest_offset / self.block_size;
        let within = guest_offset % self.block_size;
        let run = self.block_size - within;
        let bat_index = (block + block / self.chunk_ratio) as usize;
        let Some(entry) = self.bat.get(bat_index * 8..bat_index * 8 + 8) else {
            return Err(bad("guest offset BAT entry out of BAT bounds"));
        };
        let entry = u64::from_le_bytes(entry.try_into().unwrap());
        let state = entry & 7;
        if state != 6 && state != 7 {
            return Ok((None, run));
        }
        let file_offset = entry & 0xFFFF_FFFF_FFF0_0000;
        if file_offset == 0 {
            return Err(bad(format!("block state {state} with a zero file offset — corrupt BAT")));
        }
        Ok((Some(file_offset + within), run))
    }

    /// Read `buf` from `guest_offset`, following the parent chain for absent
    /// blocks; past a dynamic (parentless) image absent blocks read as zeros.
    pub fn read_at(&mut self, guest_offset: u64, buf: &mut [u8]) -> Result<()> {
        if guest_offset + buf.len() as u64 > self.virtual_size {
            return Err(bad(format!(
                "guest read [{guest_offset}, +{}) beyond virtual size {}",
                buf.len(),
                self.virtual_size
            )));
        }
        let mut off = guest_offset;
        let mut rest: &mut [u8] = buf;
        while !rest.is_empty() {
            let (place, run) = self.locate(off)?;
            let take = (run.min(rest.len() as u64)) as usize;
            match place {
                Some(file_offset) => {
                    read_exact_at(&mut self.file, file_offset, &mut rest[..take])?
                }
                None => match &self.parent_path {
                    Some(p) => {
                        if self.parent.is_none() {
                            self.parent =
                                Some(Box::new(Self::open_at_depth(p, self.depth + 1)?));
                        }
                        self.parent.as_mut().unwrap().read_at(off, &mut rest[..take])?;
                    }
                    None => rest[..take].fill(0),
                },
            }
            off += take as u64;
            rest = &mut rest[take..];
        }
        Ok(())
    }
}


/// Read directly from a guest offset of a VHDX file without attaching.
/// Handles ranges spanning payload-block boundaries (blocks are scattered in
/// the file; each is translated separately) and WALKS THE PARENT CHAIN: a
/// block a differencing child does not carry is read from its parent — e.g.
/// the GPT of an incremental image, which only the full backup stores. Past a
/// dynamic (parentless) image, absent blocks read as zeros.
pub fn read_vhdx_guest_at(path: &Path, guest_offset: u64, buf: &mut [u8]) -> Result<()> {
    read_guest_chain(path, guest_offset, buf, 0)
}

fn read_guest_chain(path: &Path, guest_offset: u64, buf: &mut [u8], depth: usize) -> Result<()> {
    if depth >= MAX_CHAIN_DEPTH {
        return Err(bad(format!(
            "parent chain deeper than {MAX_CHAIN_DEPTH} images — corrupted chain?"
        )));
    }
    let mut file = File::open(path).map_err(|e| bad(format!("open {}: {e}", path.display())))?;
    let mut parent: Option<Option<std::path::PathBuf>> = None; // resolved once, lazily
    let mut off = guest_offset;
    let mut rest: &mut [u8] = buf;
    while !rest.is_empty() {
        match guest_to_file_offset_in_file(&mut file, off)? {
            GuestRun::Data { file_offset, run } => {
                let take = (run.min(rest.len() as u64)) as usize;
                read_exact_at(&mut file, file_offset, &mut rest[..take])?;
                off += take as u64;
                rest = &mut rest[take..];
            }
            GuestRun::Absent { run, .. } => {
                let take = (run.min(rest.len() as u64)) as usize;
                let resolved = match &parent {
                    Some(p) => p.clone(),
                    None => {
                        let p = vhdx_parent_path(path)?;
                        parent = Some(p.clone());
                        p
                    }
                };
                match resolved {
                    Some(parent_path) => {
                        read_guest_chain(&parent_path, off, &mut rest[..take], depth + 1)?
                    }
                    None => rest[..take].fill(0),
                }
                off += take as u64;
                rest = &mut rest[take..];
            }
        }
    }
    Ok(())
}

/// Write directly to a guest offset of a VHDX file without attaching.
/// Handles ranges spanning payload-block boundaries; every touched block must
/// already be allocated IN THIS FILE (the engine pre-allocates the meta
/// partition with a placeholder write during the copy) — writes never touch
/// the parent chain.
pub fn write_vhdx_guest_at(path: &Path, guest_offset: u64, buf: &[u8]) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| bad(format!("open {}: {e}", path.display())))?;
    let mut off = guest_offset;
    let mut rest = buf;
    while !rest.is_empty() {
        match guest_to_file_offset_in_file(&mut file, off)? {
            GuestRun::Data { file_offset, run } => {
                let take = (run.min(rest.len() as u64)) as usize;
                file.seek(SeekFrom::Start(file_offset)).map_err(bad)?;
                file.write_all(&rest[..take]).map_err(bad)?;
                off += take as u64;
                rest = &rest[take..];
            }
            GuestRun::Absent { state, .. } => {
                return Err(bad(format!(
                    "block state {state} is not physically allocated in VHDX (cannot seek/write)"
                )));
            }
        }
    }
    file.sync_all().map_err(bad)
}

#[cfg(test)]
#[cfg(target_os = "linux")]
mod tests {
    use super::*;
    use crate::vhdx_format::VhdxWriter;

    /// Writes land in the right payload indices even past the sector-bitmap
    /// interleave boundary (chunk_ratio = 4096 for 1 MiB blocks / 512 b sectors).
    #[test]
    fn scanner_finds_written_blocks_across_interleave() {
        const MB: u64 = 1024 * 1024;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.vhdx");
        let vsize = 5000 * MB; // > 4096 payload blocks → one bitmap entry interleaved

        let mut w = VhdxWriter::create(&path, vsize, MB, 512).unwrap();
        for block in [0u64, 7, 4095, 4100, 4999] {
            w.write_at(block * MB + 123, &[0xAAu8; 512]).unwrap();
        }
        w.finish().unwrap();

        let alloc = allocated_payload_blocks(&path).unwrap();
        assert_eq!(alloc.block_size, MB);
        assert_eq!(alloc.virtual_size, vsize);
        assert_eq!(alloc.blocks, vec![0, 7, 4095, 4100, 4999]);
        assert_eq!(alloc.block_range(4999), (4999 * MB, 5000 * MB));
    }

    #[test]
    fn scanner_rejects_non_vhdx() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not.vhdx");
        std::fs::write(&path, b"garbage garbage garbage").unwrap();
        assert!(allocated_payload_blocks(&path).is_err());
    }

    /// Guest reads/writes spanning a payload-block boundary must translate
    /// each block separately. Blocks are allocated OUT OF ORDER so their file
    /// positions do not mirror guest order — a single contiguous file access
    /// (the old behaviour) reads/writes the wrong bytes past the boundary.
    #[test]
    fn guest_io_across_payload_block_boundary() {
        const MB: u64 = 1024 * 1024;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.vhdx");

        let mut w = VhdxWriter::create(&path, 8 * MB, MB, 512).unwrap();
        w.write_at(3 * MB, &vec![0x33u8; MB as usize]).unwrap(); // block 3 first
        w.write_at(2 * MB, &vec![0x22u8; MB as usize]).unwrap(); // block 2 after
        w.finish().unwrap();

        // 8 KiB straddling the guest block 2 → 3 boundary at an odd offset.
        let data: Vec<u8> = (0..8192u32).map(|i| (i % 251) as u8).collect();
        write_vhdx_guest_at(&path, 3 * MB - 4096, &data).unwrap();

        let mut back = vec![0u8; 8192];
        read_vhdx_guest_at(&path, 3 * MB - 4096, &mut back).unwrap();
        assert_eq!(back, data, "straddling write must read back intact");

        // Neighbouring bytes on both sides keep their original fill.
        let mut edge = vec![0u8; 16];
        read_vhdx_guest_at(&path, 3 * MB - 4096 - 16, &mut edge).unwrap();
        assert_eq!(edge, [0x22u8; 16]);
        read_vhdx_guest_at(&path, 3 * MB + 4096, &mut edge).unwrap();
        assert_eq!(edge, [0x33u8; 16]);

        // An unallocated block of a dynamic (parentless) image reads as zeros
        // (VHDX semantics), spanning allocated → absent across the boundary.
        let mut span = vec![0xFFu8; 32];
        read_vhdx_guest_at(&path, 4 * MB - 16, &mut span).unwrap();
        assert_eq!(&span[..16], &[0x33u8; 16]);
        assert_eq!(&span[16..], &[0u8; 16]);

        // Writes must still refuse unallocated blocks.
        assert!(write_vhdx_guest_at(&path, 5 * MB, &[1u8; 16]).is_err());
    }

    /// Parent-locator parsing on a hand-built metadata region (MS-VHDX §2.6.3):
    /// entry offsets are relative to the locator header, strings are UTF-16LE.
    #[test]
    fn parent_locator_parses_relative_path() {
        let mut meta = vec![0u8; 4096];
        meta[0..8].copy_from_slice(b"metadata");
        meta[10..12].copy_from_slice(&1u16.to_le_bytes()); // one table entry

        // Table entry 0 at offset 32: parent locator item at 1024, length 256.
        meta[32..48].copy_from_slice(&guid_le(MI_PARENT_LOCATOR));
        meta[48..52].copy_from_slice(&1024u32.to_le_bytes());
        meta[52..56].copy_from_slice(&256u32.to_le_bytes());

        // Locator header at 1024: type GUID (16) + reserved (2) + count (2).
        meta[1042..1044].copy_from_slice(&2u16.to_le_bytes());
        let key1: Vec<u8> = "relative_path".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let val1: Vec<u8> = r".\parent.vhdx".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let key2: Vec<u8> = "parent_linkage".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let val2: Vec<u8> = "{GUID}".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        // Strings packed after the two 12-byte entries (offsets locator-relative).
        let mut cursor = 20 + 2 * 12;
        let mut put = |bytes: &[u8], meta: &mut Vec<u8>| {
            let at = cursor;
            meta[1024 + at..1024 + at + bytes.len()].copy_from_slice(bytes);
            cursor += bytes.len();
            at
        };
        let k1 = put(&key1, &mut meta);
        let v1 = put(&val1, &mut meta);
        let k2 = put(&key2, &mut meta);
        let v2 = put(&val2, &mut meta);
        for (i, (ko, vo, kl, vl)) in [
            (k1, v1, key1.len(), val1.len()),
            (k2, v2, key2.len(), val2.len()),
        ]
        .into_iter()
        .enumerate()
        {
            let e = 1024 + 20 + i * 12;
            meta[e..e + 4].copy_from_slice(&(ko as u32).to_le_bytes());
            meta[e + 4..e + 8].copy_from_slice(&(vo as u32).to_le_bytes());
            meta[e + 8..e + 10].copy_from_slice(&(kl as u16).to_le_bytes());
            meta[e + 10..e + 12].copy_from_slice(&(vl as u16).to_le_bytes());
        }

        assert_eq!(parse_parent_locator(&meta).as_deref(), Some(r".\parent.vhdx"));
        let resolved = resolve_parent(Path::new("/backups/child.avhdx"), r".\parent.vhdx");
        assert_eq!(resolved, Path::new("/backups/parent.vhdx"));
    }
    /// The reused reader must return byte-for-byte what the one-shot reads do:
    /// allocated blocks, absent-as-zero blocks, and odd boundaries alike. The
    /// reader exists purely for speed (one BAT parse instead of one per block),
    /// and speed refactors are where silent read bugs come from.
    #[test]
    fn reused_reader_matches_one_shot_reads() {
        const MB: u64 = 1024 * 1024;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.vhdx");
        let mut w = VhdxWriter::create(&path, 8 * MB, MB, 512).unwrap();
        w.write_at(MB, &vec![0x11u8; MB as usize]).unwrap();
        w.write_at(3 * MB, &(0..MB).map(|i| (i % 253) as u8).collect::<Vec<_>>()).unwrap();
        w.finish().unwrap();

        let mut reader = VhdxGuestReader::open(&path).unwrap();
        // Spans: inside a block, across allocated→absent, absent-only, odd sizes.
        for (off, len) in [
            (0u64, 4096usize),
            (MB - 17, 64),
            (MB, MB as usize),
            (2 * MB - 5, 11),
            (3 * MB + 12345, 100_000),
            (4 * MB - 3, 7),
            (6 * MB, 4096),
        ] {
            let mut a = vec![0xAAu8; len];
            let mut b = vec![0x55u8; len];
            reader.read_at(off, &mut a).unwrap();
            read_vhdx_guest_at(&path, off, &mut b).unwrap();
            assert_eq!(a, b, "mismatch at offset {off} len {len}");
        }
        // Reads past the virtual size are refused, not zero-filled.
        assert!(reader.read_at(8 * MB - 4, &mut [0u8; 8]).is_err());
    }

}
