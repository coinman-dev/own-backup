//! Read and parse a physical disk's GPT into a [`DiskLayout`] for whole-disk
//! (bootable) backup. The parser is cross-platform and unit-tested; the raw
//! read half is Windows-only.

use uuid::Uuid;

use own_backup_common::gpt::{DiskLayout, GptPartitionEntry};
use own_backup_common::{OwnBackupError, Result};

const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";

fn u32_at(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}
fn u64_at(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
}
/// GPT stores GUIDs mixed-endian (first three fields little-endian).
fn guid_at(buf: &[u8], off: usize) -> Uuid {
    let mut b = [0u8; 16];
    b.copy_from_slice(&buf[off..off + 16]);
    Uuid::from_bytes_le(b)
}

/// Parse the primary GPT from a buffer that starts at LBA 0 (must contain at
/// least the header at LBA 1 and the whole partition-entry array). `disk_size`
/// and `sector_size` describe the physical disk. Errors when the disk is not
/// GPT (e.g. an MBR-only disk) or the table is malformed.
pub fn parse_disk_layout(buf: &[u8], sector_size: u32, disk_size: u64) -> Result<DiskLayout> {
    let sector = sector_size as usize;
    let hdr_off = sector; // LBA 1
    if buf.len() < hdr_off + 92 {
        return Err(OwnBackupError::ImageFormat(
            "GPT: buffer too small for header".into(),
        ));
    }
    let hdr = &buf[hdr_off..];
    if &hdr[0..8] != GPT_SIGNATURE {
        return Err(OwnBackupError::ImageFormat(
            "disk has no GPT (MBR-only disks are not supported for whole-disk backup)".into(),
        ));
    }

    let disk_guid = guid_at(hdr, 56).to_string().to_uppercase();
    let entries_lba = u64_at(hdr, 72);
    let num_entries = u32_at(hdr, 80) as usize;
    let entry_size = u32_at(hdr, 84) as usize;

    if entry_size < 128 || num_entries == 0 || num_entries > 512 {
        return Err(OwnBackupError::ImageFormat(format!(
            "GPT: implausible entry array ({num_entries} × {entry_size})"
        )));
    }
    let entries_off = entries_lba as usize * sector;
    let entries_end = entries_off + num_entries * entry_size;
    if buf.len() < entries_end {
        return Err(OwnBackupError::ImageFormat(
            "GPT: partition entries extend past the read buffer".into(),
        ));
    }

    let zero_guid = Uuid::nil();
    let mut partitions = Vec::new();
    for i in 0..num_entries {
        let e = &buf[entries_off + i * entry_size..entries_off + (i + 1) * entry_size];
        let type_guid = guid_at(e, 0);
        if type_guid == zero_guid {
            continue; // empty slot
        }
        // Name: UTF-16LE, up to 36 code units, NUL-terminated.
        let name_units: Vec<u16> = (0..36)
            .map(|k| u16::from_le_bytes([e[56 + k * 2], e[56 + k * 2 + 1]]))
            .take_while(|&u| u != 0)
            .collect();
        partitions.push(GptPartitionEntry {
            index: (i + 1) as u32,
            type_guid: type_guid.to_string().to_uppercase(),
            part_guid: guid_at(e, 16).to_string().to_uppercase(),
            first_lba: u64_at(e, 32),
            last_lba: u64_at(e, 40),
            attributes: u64_at(e, 48),
            name: String::from_utf16_lossy(&name_units),
        });
    }

    Ok(DiskLayout {
        disk_guid,
        sector_size,
        size_bytes: disk_size,
        partitions,
    })
}

/// Read `\\.\PhysicalDrive{disk_number}`'s GPT off the physical drive.
#[cfg(target_os = "windows")]
pub fn read_disk_layout(disk_number: u32) -> Result<DiskLayout> {
    use crate::win_device::WinRawDevice;

    let path = format!(r"\\.\PhysicalDrive{disk_number}");
    let mut dev = WinRawDevice::open_read(&path)?;
    let sector = dev.sector_size();
    let disk_size = dev.length();
    // First 34 sectors: protective MBR (0), primary header (1), and the 128-entry
    // array (LBA 2..33 for 512-byte sectors; fewer for 4K). Covers a standard GPT.
    let sectors = 34u64;
    let want = (sectors * sector as u64).min(disk_size) as usize;
    let mut buf = vec![0u8; want];
    dev.read_at(0, &mut buf)?;
    parse_disk_layout(&buf, sector, disk_size)
}

/// Find the first partition matching the OwnBackupMeta partition type GUID.
/// Returns its guest offset (in bytes) and size (in bytes), or None if not found.
pub fn find_meta_partition(layout: &DiskLayout) -> Option<(u64, u64)> {
    layout
        .partitions
        .iter()
        .find(|p| p.type_guid == crate::gpt_writer::OWN_BACKUP_META_GUID)
        .map(|p| {
            (
                p.offset_bytes(layout.sector_size),
                p.size_bytes(layout.sector_size),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpt_writer::{gpt_write_ops, GptLayout, DEFAULT_PARTITION_OFFSET};

    #[test]
    fn round_trips_a_single_partition_gpt() {
        let disk_size = 100 * 1024 * 1024; // 100 MiB
        let sector = 512u32;
        let part_guid = Uuid::new_v4();
        let disk_guid = Uuid::new_v4();
        let part_size = 64 * 1024 * 1024;

        let layout = GptLayout {
            disk_size,
            sector_size: sector,
            partition_offset: DEFAULT_PARTITION_OFFSET,
            partition_size: part_size,
            disk_guid,
            partition_guid: part_guid,
            partition_name: "TESTVOL".into(),
            meta_partition_size: None,
            meta_partition_guid: None,
        };
        let ops = gpt_write_ops(&layout).unwrap();

        // Assemble a whole-disk buffer and apply the writes.
        let mut disk = vec![0u8; disk_size as usize];
        for op in &ops {
            disk[op.offset as usize..op.offset as usize + op.bytes.len()]
                .copy_from_slice(&op.bytes);
        }

        let parsed = parse_disk_layout(&disk, sector, disk_size).expect("parse GPT");
        assert_eq!(parsed.disk_guid, disk_guid.to_string().to_uppercase());
        assert_eq!(parsed.sector_size, sector);
        assert_eq!(parsed.partitions.len(), 1);
        let p = &parsed.partitions[0];
        assert_eq!(p.part_guid, part_guid.to_string().to_uppercase());
        assert_eq!(p.first_lba, DEFAULT_PARTITION_OFFSET / sector as u64);
        assert_eq!(p.name, "TESTVOL");
        // Offset/size helpers reproduce the on-disk geometry.
        assert_eq!(p.offset_bytes(sector), DEFAULT_PARTITION_OFFSET);
    }

    #[test]
    fn rejects_non_gpt_disk() {
        let disk = vec![0u8; 64 * 1024]; // no "EFI PART" signature at LBA 1
        assert!(parse_disk_layout(&disk, 512, 64 * 1024).is_err());
    }
}
