//! Minimal GPT (GUID Partition Table) builder.
//!
//! A volume backup is stored inside a VHDX as a disk with a GPT and a single
//! Basic Data partition wrapping the raw volume image — this is what makes the
//! image *mountable*: Windows (and Linux) recognize the partition and expose
//! the NTFS volume inside.
//!
//! The builder produces `(offset, bytes)` write operations that platform
//! engines apply to the attached virtual disk with sector-aligned raw writes.
//! The same code will later recreate partition tables during bare-metal
//! restore.
//!
//! Layout produced (for 512-byte sectors):
//! ```text
//! LBA 0        protective MBR (type 0xEE covering the whole disk)
//! LBA 1        primary GPT header
//! LBA 2..34    128 partition entries (16 KiB)
//! LBA 2048     partition data (DEFAULT_PARTITION_OFFSET = 1 MiB)
//! ...
//! last-33..-1  backup entries + backup GPT header
//! ```

use uuid::Uuid;

use own_backup_common::{OwnBackupError, Result};

/// GPT type GUID for a Basic Data partition (Windows).
pub const BASIC_DATA_GUID: &str = "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7";

/// Conventional start of the first partition: 1 MiB.
pub const DEFAULT_PARTITION_OFFSET: u64 = 1024 * 1024;

const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";
const GPT_REVISION: u32 = 0x0001_0000;
const GPT_HEADER_SIZE: u32 = 92;
const NUM_ENTRIES: u32 = 128;
const ENTRY_SIZE: u32 = 128;

/// Parameters describing the single-partition GPT disk to build.
pub struct GptLayout {
    /// Total virtual disk size in bytes (must be a multiple of `sector_size`).
    pub disk_size: u64,
    /// Logical sector size (512 or 4096).
    pub sector_size: u32,
    /// Byte offset where the partition starts (use [`DEFAULT_PARTITION_OFFSET`]).
    pub partition_offset: u64,
    /// Partition length in bytes (the backed-up volume size).
    pub partition_size: u64,
    /// Disk GUID.
    pub disk_guid: Uuid,
    /// Partition unique GUID.
    pub partition_guid: Uuid,
    /// Partition name (UTF-16, max 36 chars).
    pub partition_name: String,
}

/// A single raw write to apply to the disk.
pub struct WriteOp {
    pub offset: u64,
    pub bytes: Vec<u8>,
}

fn crc32(data: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(data);
    hasher.finalize()
}

fn put_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

fn put_u64(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

/// Serialize a GUID in GPT on-disk (mixed-endian) format.
fn put_guid(buf: &mut [u8], off: usize, guid: &Uuid) {
    buf[off..off + 16].copy_from_slice(&guid.to_bytes_le());
}

fn build_entries(layout: &GptLayout) -> Result<Vec<u8>> {
    let sector = layout.sector_size as u64;
    let first_lba = layout.partition_offset / sector;
    let last_lba = (layout.partition_offset + layout.partition_size).div_ceil(sector) - 1;

    let mut entries = vec![0u8; (NUM_ENTRIES * ENTRY_SIZE) as usize];
    let e = &mut entries[..ENTRY_SIZE as usize];

    let type_guid = Uuid::parse_str(BASIC_DATA_GUID)
        .map_err(|err| OwnBackupError::ImageFormat(format!("bad type GUID: {err}")))?;
    put_guid(e, 0, &type_guid);
    put_guid(e, 16, &layout.partition_guid);
    put_u64(e, 32, first_lba);
    put_u64(e, 40, last_lba);
    put_u64(e, 48, 0); // attributes

    // Partition name: UTF-16LE, max 36 code units.
    for (i, unit) in layout.partition_name.encode_utf16().take(36).enumerate() {
        e[56 + i * 2..56 + i * 2 + 2].copy_from_slice(&unit.to_le_bytes());
    }

    Ok(entries)
}

#[allow(clippy::too_many_arguments)]
fn build_header(
    layout: &GptLayout,
    current_lba: u64,
    backup_lba: u64,
    first_usable_lba: u64,
    last_usable_lba: u64,
    entries_lba: u64,
    entries_crc: u32,
) -> Vec<u8> {
    let mut h = vec![0u8; layout.sector_size as usize];
    h[0..8].copy_from_slice(GPT_SIGNATURE);
    put_u32(&mut h, 8, GPT_REVISION);
    put_u32(&mut h, 12, GPT_HEADER_SIZE);
    // CRC32 at offset 16 computed below with the field zeroed.
    put_u64(&mut h, 24, current_lba);
    put_u64(&mut h, 32, backup_lba);
    put_u64(&mut h, 40, first_usable_lba);
    put_u64(&mut h, 48, last_usable_lba);
    put_guid(&mut h, 56, &layout.disk_guid);
    put_u64(&mut h, 72, entries_lba);
    put_u32(&mut h, 80, NUM_ENTRIES);
    put_u32(&mut h, 84, ENTRY_SIZE);
    put_u32(&mut h, 88, entries_crc);

    let header_crc = crc32(&h[..GPT_HEADER_SIZE as usize]);
    put_u32(&mut h, 16, header_crc);
    h
}

fn build_protective_mbr(layout: &GptLayout) -> Vec<u8> {
    let sector = layout.sector_size as u64;
    let total_lba = layout.disk_size / sector;

    let mut mbr = vec![0u8; layout.sector_size as usize];
    let entry = &mut mbr[0x1BE..0x1BE + 16];
    entry[0] = 0x00; // not bootable
    entry[1..4].copy_from_slice(&[0x00, 0x02, 0x00]); // CHS of LBA 1
    entry[4] = 0xEE; // GPT protective
    entry[5..8].copy_from_slice(&[0xFF, 0xFF, 0xFF]); // CHS end (capped)
    entry[8..12].copy_from_slice(&1u32.to_le_bytes()); // first LBA
    let size = u32::try_from(total_lba - 1).unwrap_or(u32::MAX);
    entry[12..16].copy_from_slice(&size.to_le_bytes());
    mbr[510] = 0x55;
    mbr[511] = 0xAA;
    mbr
}

/// Build the raw write operations for a single-partition GPT disk.
pub fn gpt_write_ops(layout: &GptLayout) -> Result<Vec<WriteOp>> {
    let sector = layout.sector_size as u64;
    if layout.sector_size != 512 && layout.sector_size != 4096 {
        return Err(OwnBackupError::ImageFormat(format!(
            "unsupported sector size {}",
            layout.sector_size
        )));
    }
    if layout.disk_size % sector != 0 {
        return Err(OwnBackupError::ImageFormat(
            "disk size is not a multiple of the sector size".into(),
        ));
    }

    let total_lba = layout.disk_size / sector;
    let entries_bytes = (NUM_ENTRIES * ENTRY_SIZE) as u64; // 16 KiB
    let entries_sectors = entries_bytes.div_ceil(sector);

    let primary_entries_lba = 2;
    let first_usable_lba = primary_entries_lba + entries_sectors;
    let backup_header_lba = total_lba - 1;
    let backup_entries_lba = backup_header_lba - entries_sectors;
    let last_usable_lba = backup_entries_lba - 1;

    // Validate the partition fits the usable range.
    let part_first_lba = layout.partition_offset / sector;
    let part_last_lba =
        (layout.partition_offset + layout.partition_size).div_ceil(sector) - 1;
    if layout.partition_offset % sector != 0 {
        return Err(OwnBackupError::ImageFormat(
            "partition offset is not sector-aligned".into(),
        ));
    }
    if part_first_lba < first_usable_lba || part_last_lba > last_usable_lba {
        return Err(OwnBackupError::ImageFormat(format!(
            "partition [{part_first_lba}..{part_last_lba}] does not fit usable range \
             [{first_usable_lba}..{last_usable_lba}] of a {total_lba}-sector disk"
        )));
    }

    let entries = build_entries(layout)?;
    let entries_crc = crc32(&entries);

    let primary_header = build_header(
        layout,
        1,
        backup_header_lba,
        first_usable_lba,
        last_usable_lba,
        primary_entries_lba,
        entries_crc,
    );
    let backup_header = build_header(
        layout,
        backup_header_lba,
        1,
        first_usable_lba,
        last_usable_lba,
        backup_entries_lba,
        entries_crc,
    );

    Ok(vec![
        WriteOp {
            offset: 0,
            bytes: build_protective_mbr(layout),
        },
        WriteOp {
            offset: sector,
            bytes: primary_header,
        },
        WriteOp {
            offset: primary_entries_lba * sector,
            bytes: entries.clone(),
        },
        WriteOp {
            offset: backup_entries_lba * sector,
            bytes: entries,
        },
        WriteOp {
            offset: backup_header_lba * sector,
            bytes: backup_header,
        },
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn apply_ops(disk: &mut [u8], ops: &[WriteOp]) {
        for op in ops {
            let start = op.offset as usize;
            disk[start..start + op.bytes.len()].copy_from_slice(&op.bytes);
        }
    }

    fn make_layout(disk_size: u64, partition_size: u64) -> GptLayout {
        GptLayout {
            disk_size,
            sector_size: 512,
            partition_offset: DEFAULT_PARTITION_OFFSET,
            partition_size,
            disk_guid: Uuid::from_u128(0x1111_2222_3333_4444_5555_6666_7777_8888),
            partition_guid: Uuid::from_u128(0xAAAA_BBBB_CCCC_DDDD_EEEE_0000_1111_2222),
            partition_name: "OwnBackup".into(),
        }
    }

    /// Cross-validate our GPT against the independent `gpt` crate parser.
    #[test]
    fn produced_gpt_parses_with_gpt_crate() {
        let disk_size = 8 * 1024 * 1024u64; // 8 MiB virtual disk
        let part_size = 4 * 1024 * 1024u64; // 4 MiB partition
        let layout = make_layout(disk_size, part_size);

        let mut disk = vec![0u8; disk_size as usize];
        apply_ops(&mut disk, &gpt_write_ops(&layout).unwrap());

        let cursor = Cursor::new(disk);
        let gpt_disk = gpt::GptConfig::new()
            .writable(false)
            .open_from_device(Box::new(cursor))
            .expect("gpt crate must accept our table");

        let partitions = gpt_disk.partitions();
        assert_eq!(partitions.len(), 1);
        let p = partitions.values().next().unwrap();
        assert_eq!(p.first_lba, DEFAULT_PARTITION_OFFSET / 512);
        assert_eq!(
            (p.last_lba - p.first_lba + 1) * 512,
            part_size
        );
        assert_eq!(p.name, "OwnBackup");
        assert_eq!(
            p.part_type_guid.guid.to_string().to_uppercase(),
            BASIC_DATA_GUID
        );
    }

    #[test]
    fn rejects_partition_larger_than_disk() {
        let layout = make_layout(4 * 1024 * 1024, 8 * 1024 * 1024);
        assert!(gpt_write_ops(&layout).is_err());
    }

    #[test]
    fn rejects_unaligned_partition_offset() {
        let mut layout = make_layout(8 * 1024 * 1024, 2 * 1024 * 1024);
        layout.partition_offset = 1000; // not sector aligned
        assert!(gpt_write_ops(&layout).is_err());
    }

    #[test]
    fn protective_mbr_has_signature_and_ee_entry() {
        let layout = make_layout(8 * 1024 * 1024, 2 * 1024 * 1024);
        let ops = gpt_write_ops(&layout).unwrap();
        let mbr = &ops[0].bytes;
        assert_eq!(mbr[510], 0x55);
        assert_eq!(mbr[511], 0xAA);
        assert_eq!(mbr[0x1BE + 4], 0xEE);
    }
}
