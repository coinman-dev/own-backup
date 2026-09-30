//! Reading the structures the Windows storage stack answers with.
//!
//! Deliberately **not** gated behind `cfg(windows)`: these are pure functions
//! over byte buffers and integers, and the only way to keep them honest is to
//! test them — which happens on Linux, where this project's tests run. The
//! module that sends the IOCTLs ([`crate::win_disks`]) is where the platform
//! begins.

use own_backup_common::tr;
use own_backup_common::types::FileSystem;

pub fn u32_at(b: &[u8], off: usize) -> u32 {
    b.get(off..off + 4)
        .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
        .unwrap_or(0)
}

pub fn u64_at(b: &[u8], off: usize) -> u64 {
    b.get(off..off + 8)
        .map(|s| u64::from_le_bytes(s.try_into().unwrap()))
        .unwrap_or(0)
}

/// A GUID as Windows stores it in memory — first three fields little-endian,
/// the last eight bytes in order — formatted the way the rest of this codebase
/// writes GUIDs: upper case, no braces. An all-zero GUID reads as absent.
pub fn guid_at(b: &[u8], off: usize) -> String {
    let Some(g) = b.get(off..off + 16) else {
        return String::new();
    };
    if g.iter().all(|&x| x == 0) {
        return String::new();
    }
    format!(
        "{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        u32_at(g, 0),
        u16::from_le_bytes([g[4], g[5]]),
        u16::from_le_bytes([g[6], g[7]]),
        g[8],
        g[9],
        g[10],
        g[11],
        g[12],
        g[13],
        g[14],
        g[15]
    )
}

/// NUL-terminated ANSI string living at `off` inside a descriptor buffer.
///
/// `STORAGE_DEVICE_DESCRIPTOR` stores its model and serial as offsets into the
/// same buffer; an offset of 0 means the device did not answer that question.
pub fn ansi_at(buf: &[u8], off: u32) -> String {
    let off = off as usize;
    if off == 0 || off >= buf.len() {
        return String::new();
    }
    let end = buf[off..]
        .iter()
        .position(|&c| c == 0)
        .map(|n| off + n)
        .unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[off..end]).trim().to_string()
}

/// `STORAGE_BUS_TYPE` as a name a person reads.
pub fn bus_name(bus: u32) -> &'static str {
    match bus {
        1 => "SCSI",
        2 => "ATAPI",
        3 => "ATA",
        4 => "1394",
        5 => "SSA",
        6 => "Fibre",
        7 => "USB",
        8 => "RAID",
        9 => "iSCSI",
        10 => "SAS",
        11 => "SATA",
        12 => "SD",
        13 => "MMC",
        14 => "Virtual",
        15 => "Virtual (file-backed)",
        16 => "Storage Spaces",
        17 => "NVMe",
        18 => "SCM",
        19 => "UFS",
        _ => "",
    }
}

/// "NVMe SSD", "USB HDD (съёмный)" — what the interface shows for a disk.
///
/// Windows used to hand us `Win32_DiskDrive.MediaType`, which says "Fixed hard
/// disk media" about an NVMe SSD. The bus plus the seek penalty (the storage
/// stack's own way of saying "this thing has a head that moves") is both
/// shorter and true.
pub fn media_type_of(bus: u32, seek_penalty: Option<bool>, removable: bool) -> String {
    let kind = match seek_penalty {
        Some(false) => "SSD",
        Some(true) => "HDD",
        None if bus == 17 => "SSD", // NVMe never spins
        None => "",
    };
    let removable_tag = if removable {
        tr!("win_parse.removable").into_owned()
    } else {
        String::new()
    };
    let mut out = String::new();
    for piece in [bus_name(bus), kind, removable_tag.as_str()] {
        if piece.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(piece);
    }
    if out.is_empty() {
        "Unknown".into()
    } else {
        out
    }
}

/// The filesystem name `GetVolumeInformation` reports.
pub fn parse_filesystem(fs: &str) -> FileSystem {
    match fs.trim().to_uppercase().as_str() {
        "NTFS" => FileSystem::Ntfs,
        "FAT32" => FileSystem::Fat32,
        "FAT" | "FAT16" => FileSystem::Fat32,
        "EXFAT" => FileSystem::ExFat,
        "EXT4" => FileSystem::Ext4,
        "XFS" => FileSystem::Xfs,
        "BTRFS" => FileSystem::Btrfs,
        "REFS" => FileSystem::ReFs,
        _ => FileSystem::Unknown,
    }
}

/// Known EFI System Partition GPT type GUID.
pub const GPT_TYPE_EFI_SYSTEM: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";

/// Windows answers «MBR» about a disk that has no table at all.
///
/// A never-initialised disk comes back from `IOCTL_DISK_GET_DRIVE_LAYOUT_EX` as
/// `PARTITION_STYLE_MBR` with no partitions and a zero signature, while
/// `Get-Disk` says `RAW` about the very same disk (measured 2026-09-20 on a
/// fresh VHDX). Believing that answer cost a whole-disk restore: the interface
/// picked the scheme from what the TARGET called itself, saw "mbr", and wrote
/// an MBR table with a BIOS loader for a copy of a UEFI disk with an ESP —
/// every byte in place and nothing able to boot it.
///
/// A disk someone really did initialise as MBR and left empty is a different
/// thing and keeps its answer: initialising writes a signature, and that is
/// what tells the two apart.
pub fn synthetic_mbr(style: u32, partitions: usize, mbr_signature: u32) -> bool {
    const STYLE_MBR: u32 = 0;
    style == STYLE_MBR && partitions == 0 && mbr_signature == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_guid_is_read_the_way_windows_stores_it() {
        // C12A7328-F81F-11D2-BA4B-00A0C93EC93B — the EFI System partition type,
        // as it sits in a GPT entry.
        let bytes = [
            0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E,
            0xC9, 0x3B,
        ];
        assert_eq!(guid_at(&bytes, 0), GPT_TYPE_EFI_SYSTEM);
    }

    #[test]
    fn an_all_zero_guid_reads_as_absent() {
        assert_eq!(
            guid_at(&[0u8; 16], 0),
            "",
            "an empty GPT slot, not a partition"
        );
    }

    #[test]
    fn reading_past_the_buffer_yields_nothing_rather_than_panicking() {
        // Short answers happen: a device that fills in fewer fields than the
        // descriptor has room for. None of it may be a crash in an inventory.
        assert_eq!(guid_at(&[0u8; 8], 0), "");
        assert_eq!(u32_at(&[1, 2], 0), 0);
        assert_eq!(u64_at(&[1, 2, 3, 4], 0), 0);
        assert_eq!(ansi_at(&[0u8; 4], 99), "");
    }

    #[test]
    fn a_descriptor_string_stops_at_its_terminator() {
        let mut buf = vec![0u8; 64];
        buf[36..41].copy_from_slice(b"WDC \0");
        assert_eq!(ansi_at(&buf, 36), "WDC");
        assert_eq!(
            ansi_at(&buf, 0),
            "",
            "offset 0 means the device did not answer"
        );
    }

    #[test]
    fn media_type_says_what_the_disk_actually_is() {
        assert_eq!(
            media_type_of(17, None, false),
            "NVMe SSD",
            "NVMe never spins"
        );
        assert_eq!(media_type_of(11, Some(true), false), "SATA HDD");
        assert_eq!(media_type_of(7, Some(false), true), "USB SSD (съёмный)");
        assert_eq!(media_type_of(0, None, false), "Unknown");
    }

    #[test]
    fn filesystem_names_come_back_as_the_enum() {
        assert_eq!(parse_filesystem("NTFS"), FileSystem::Ntfs);
        assert_eq!(parse_filesystem(" fat32 "), FileSystem::Fat32);
        assert_eq!(parse_filesystem("RAW"), FileSystem::Unknown);
    }
}

#[cfg(test)]
mod layout_answers {
    use super::synthetic_mbr;

    const MBR: u32 = 0;
    const GPT: u32 = 1;
    const RAW: u32 = 2;

    #[test]
    fn an_untouched_disk_is_not_an_mbr_disk() {
        // What a never-initialised VHDX answers: style MBR, nothing in it.
        assert!(synthetic_mbr(MBR, 0, 0));
    }

    #[test]
    fn an_empty_but_initialised_mbr_disk_keeps_its_answer() {
        // "Initialise as MBR" writes a signature; that is the whole difference.
        assert!(!synthetic_mbr(MBR, 0, 0x1A2B_3C4D));
    }

    #[test]
    fn a_partitioned_mbr_disk_is_never_synthetic() {
        assert!(!synthetic_mbr(MBR, 1, 0));
    }

    #[test]
    fn other_styles_are_left_alone() {
        assert!(!synthetic_mbr(GPT, 0, 0));
        assert!(!synthetic_mbr(RAW, 0, 0));
    }
}
