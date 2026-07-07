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
use std::io::{Read, Seek, SeekFrom};
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
}
