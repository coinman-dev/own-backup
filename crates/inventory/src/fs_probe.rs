//! Best-effort filesystem detection for partitions Windows can't mount
//! (letterless EFI, Linux ext4/btrfs, LUKS, …).
//!
//! Neither GPT *type* GUIDs nor Windows volume objects are reliable here:
//! a Linux `/boot` (ext4) can be typed as an EFI System Partition, and Windows
//! then creates a volume for it but reports its filesystem as RAW/Unknown.
//! So we read the partition's first bytes straight off the physical drive and
//! match well-known on-disk magic numbers (same signatures blkid uses).
//!
//! `identify()` is pure and unit-tested on any OS; the raw reading half is
//! Windows-only. Reading a physical drive needs administrator rights; when
//! that fails we return `None` and the caller leaves the filesystem `Unknown`.

use own_backup_common::types::FileSystem;

/// Enough to reach the btrfs superblock (magic at 0x10040, label at 0x1012B).
pub const PROBE_LEN: usize = 128 * 1024;

/// A partition's filesystem plus its volume label, read from the superblock.
#[derive(Debug, Clone)]
pub struct ProbedFs {
    /// `Unknown` is possible with a meaningful `label` (e.g. LUKS containers).
    pub fs: FileSystem,
    /// Volume label when the on-disk structures carry one (FAT/ext/btrfs).
    pub label: Option<String>,
    /// Bytes in use, when the superblock reports free/total counts (ext*).
    /// `None` for filesystems whose superblock we do not parse for this — the
    /// caller then leaves `used_bytes` at 0 and the UI shows "—".
    pub used_bytes: Option<u64>,
    /// The partition is an encrypted container (LUKS) — `fs` is `Unknown` but it
    /// can be backed up after decryption with a password.
    pub encrypted: bool,
}

/// Read a fixed-width, space/NUL-padded label; `None` when empty or a
/// placeholder ("NO NAME").
fn read_label(buf: &[u8], off: usize, len: usize) -> Option<String> {
    let end = off.checked_add(len)?;
    if buf.len() < end {
        return None;
    }
    let raw = &buf[off..end];
    let cut = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    let s = String::from_utf8_lossy(&raw[..cut]).trim().to_string();
    if s.is_empty() || s.eq_ignore_ascii_case("NO NAME") {
        None
    } else {
        Some(s)
    }
}

/// Used bytes of an ext2/3/4 volume, from its superblock (at byte 1024):
/// `(s_blocks_count - s_free_blocks_count) * block_size`, where
/// `block_size = 1024 << s_log_block_size`. 64-bit block counts (INCOMPAT_64BIT)
/// contribute the `_hi` halves. `None` if any field is out of the probed buffer
/// or the numbers are inconsistent (free > total).
fn ext_used_bytes(buf: &[u8]) -> Option<u64> {
    const SB: usize = 1024;
    let u32at = |off: usize| -> Option<u64> {
        buf.get(SB + off..SB + off + 4)
            .map(|s| u32::from_le_bytes(s.try_into().unwrap()) as u64)
    };

    let log_block_size = u32at(0x18)?;
    if log_block_size > 6 {
        return None; // block_size beyond 64 KiB — not a sane ext volume
    }
    let block_size = 1024u64 << log_block_size;

    let is_64bit = u32at(0x60)? & 0x80 != 0; // s_feature_incompat INCOMPAT_64BIT
    let (blocks_hi, free_hi) = if is_64bit {
        (u32at(0x150)?, u32at(0x158)?)
    } else {
        (0, 0)
    };
    let total = u32at(0x04)? | (blocks_hi << 32); // s_blocks_count
    let free = u32at(0x0C)? | (free_hi << 32); // s_free_blocks_count

    total.checked_sub(free)?.checked_mul(block_size)
}

/// Match well-known filesystem signatures. `buf` starts at the partition's
/// very first byte.
pub fn identify(buf: &[u8]) -> Option<ProbedFs> {
    let at = |off: usize, tag: &[u8]| buf.len() >= off + tag.len() && &buf[off..off + tag.len()] == tag;
    let some = |fs, label| Some(ProbedFs { fs, label, used_bytes: None, encrypted: false });

    // OEM name / type strings in the boot sector.
    if at(3, b"NTFS    ") {
        return some(FileSystem::Ntfs, None); // NTFS label lives in the MFT
    }
    if at(3, b"EXFAT   ") {
        return some(FileSystem::ExFat, None);
    }
    if at(0x52, b"FAT32   ") {
        // FAT32 boot-sector label: 11 bytes at 0x47.
        return some(FileSystem::Fat32, read_label(buf, 0x47, 11));
    }
    // XFS: "XFSB" at offset 0.
    if at(0, b"XFSB") {
        return some(FileSystem::Xfs, None);
    }
    // LUKS1/2 container: "LUKS\xba\xbe" at offset 0. Not a filesystem we can
    // back up smartly — reported as Unknown, but with a telling label so the
    // UI shows WHY the partition is not selectable.
    if at(0, b"LUKS\xba\xbe") {
        return Some(ProbedFs {
            fs: FileSystem::Unknown,
            label: Some("LUKS (зашифровано)".into()),
            used_bytes: None,
            encrypted: true,
        });
    }
    // ext2/3/4: s_magic 0xEF53 (LE) at superblock offset 0x38 → partition byte
    // 1080. Volume label (s_volume_name): 16 bytes at superblock offset 0x78.
    // Used bytes come from the superblock's block counts. Reported as ext4 (the
    // common case; enough for the UI).
    if buf.len() >= 1082 && buf[1080] == 0x53 && buf[1081] == 0xEF {
        return Some(ProbedFs {
            fs: FileSystem::Ext4,
            label: read_label(buf, 1024 + 0x78, 16),
            used_bytes: ext_used_bytes(buf),
            encrypted: false,
        });
    }
    // btrfs: "_BHRfS_M" at 0x10040; label: 256 bytes at superblock offset 0x12B;
    // used bytes: s_bytes_used (u64) at superblock offset 0x78.
    if at(0x10040, b"_BHRfS_M") {
        let used = buf
            .get(0x10078..0x10080)
            .map(|s| u64::from_le_bytes(s.try_into().unwrap()));
        return Some(ProbedFs {
            fs: FileSystem::Btrfs,
            label: read_label(buf, 0x10000 + 0x12B, 256),
            used_bytes: used,
            encrypted: false,
        });
    }
    None
}

#[cfg(target_os = "windows")]
mod win {
    use super::{identify, ProbedFs, PROBE_LEN};
    use std::alloc::{alloc, dealloc, Layout};

    use tracing::debug;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, SetFilePointerEx, FILE_BEGIN, FILE_FLAG_NO_BUFFERING,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    const SECTOR: u64 = 512;

    /// Identify a partition's filesystem (and label) by reading its first bytes
    /// off physical drive `disk` at byte `offset`. Returns `None` when the
    /// partition can't be read (no admin rights / I/O error) or nothing matched.
    pub fn probe_partition_fs(disk: u32, offset: u64, size: u64) -> Option<ProbedFs> {
        if offset % SECTOR != 0 || size < SECTOR {
            return None;
        }
        let want = (PROBE_LEN as u64).min(size / SECTOR * SECTOR).max(SECTOR) as usize;
        let buf = read_raw(disk, offset, want)?;
        let probed = identify(&buf);
        if let Some(p) = &probed {
            debug!(disk, offset, fs = ?p.fs, label = ?p.label, "Probed partition filesystem");
        } else {
            debug!(disk, offset, "Partition filesystem not recognized");
        }
        probed
    }

    /// Open `\\.\PhysicalDrive{disk}` and read `len` sector-aligned bytes at
    /// `offset` into a fresh `Vec`. `offset` and `len` must be sector multiples.
    fn read_raw(disk: u32, offset: u64, len: usize) -> Option<Vec<u8>> {
        let path = format!(r"\\.\PhysicalDrive{disk}");
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();

        // SAFETY: `wide` is NUL-terminated and outlives the call.
        let handle: HANDLE = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                GENERIC_READ.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_NO_BUFFERING,
                None,
            )
        }
        .ok()?;

        let out = read_into(handle, offset, len);
        // SAFETY: handle came from a successful CreateFileW.
        unsafe {
            let _ = CloseHandle(handle);
        }
        out
    }

    fn read_into(handle: HANDLE, offset: u64, len: usize) -> Option<Vec<u8>> {
        // FILE_FLAG_NO_BUFFERING requires a sector-aligned buffer.
        let layout = Layout::from_size_align(len, SECTOR as usize).ok()?;
        // SAFETY: len is non-zero (>= SECTOR).
        let ptr = unsafe { alloc(layout) };
        if ptr.is_null() {
            return None;
        }
        let result = (|| {
            // SAFETY: handle is valid for the duration of this closure.
            unsafe { SetFilePointerEx(handle, offset as i64, None, FILE_BEGIN) }.ok()?;
            let mut read = 0u32;
            // SAFETY: ptr is valid for `len` bytes.
            let slice = unsafe { std::slice::from_raw_parts_mut(ptr, len) };
            unsafe { ReadFile(handle, Some(slice), Some(&mut read), None) }.ok()?;
            // SAFETY: the device returned `read` valid bytes at ptr.
            let v = unsafe { std::slice::from_raw_parts(ptr, read as usize) }.to_vec();
            Some(v)
        })();
        // SAFETY: allocated with this exact layout above.
        unsafe { dealloc(ptr, layout) };
        result
    }
}

#[cfg(target_os = "windows")]
pub use win::probe_partition_fs;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_ext4_with_label() {
        // ext superblock at byte 1024; s_magic 0xEF53 LE at +0x38; label at +0x78.
        let mut buf = vec![0u8; 4096];
        buf[1080] = 0x53;
        buf[1081] = 0xEF;
        buf[1024 + 0x78..1024 + 0x78 + 12].copy_from_slice(b"kubuntu_boot");
        // s_log_block_size = 2 → 4 KiB blocks; 262144 total, 200000 free.
        buf[1024 + 0x18..1024 + 0x18 + 4].copy_from_slice(&2u32.to_le_bytes());
        buf[1024 + 0x04..1024 + 0x04 + 4].copy_from_slice(&262_144u32.to_le_bytes());
        buf[1024 + 0x0C..1024 + 0x0C + 4].copy_from_slice(&200_000u32.to_le_bytes());
        let p = identify(&buf).expect("ext4 detected");
        assert_eq!(p.fs, FileSystem::Ext4);
        assert_eq!(p.label.as_deref(), Some("kubuntu_boot"));
        // (262144 - 200000) blocks * 4096 bytes.
        assert_eq!(p.used_bytes, Some((262_144 - 200_000) * 4096));
    }

    #[test]
    fn detects_fat32_with_boot_sector_label() {
        let mut buf = vec![0u8; 4096];
        buf[0x52..0x52 + 8].copy_from_slice(b"FAT32   ");
        buf[0x47..0x47 + 11].copy_from_slice(b"BOOT_EFI   ");
        let p = identify(&buf).expect("fat32 detected");
        assert_eq!(p.fs, FileSystem::Fat32);
        assert_eq!(p.label.as_deref(), Some("BOOT_EFI"));
    }

    #[test]
    fn detects_luks_as_unknown_with_label() {
        let mut buf = vec![0u8; 4096];
        buf[..6].copy_from_slice(b"LUKS\xba\xbe");
        let p = identify(&buf).expect("luks detected");
        assert_eq!(p.fs, FileSystem::Unknown);
        assert!(p.label.as_deref().unwrap_or("").contains("LUKS"));
    }

    #[test]
    fn detects_btrfs_and_placeholder_labels_are_dropped() {
        let mut buf = vec![0u8; 128 * 1024];
        buf[0x10040..0x10040 + 8].copy_from_slice(b"_BHRfS_M");
        buf[0x10000 + 0x12B..0x10000 + 0x12B + 7].copy_from_slice(b"kubuntu");
        let p = identify(&buf).expect("btrfs detected");
        assert_eq!(p.fs, FileSystem::Btrfs);
        assert_eq!(p.label.as_deref(), Some("kubuntu"));

        // "NO NAME" / empty labels are placeholders, not real labels.
        let mut fat = vec![0u8; 4096];
        fat[0x52..0x52 + 8].copy_from_slice(b"FAT32   ");
        fat[0x47..0x47 + 11].copy_from_slice(b"NO NAME    ");
        assert_eq!(identify(&fat).unwrap().label, None);
    }

    #[test]
    fn zeros_are_not_recognized() {
        assert!(identify(&[0u8; 4096]).is_none());
    }
}
