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

use own_backup_common::gpt::DiskLayout;
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
fn build_header_raw(
    sector_size: u32,
    disk_guid: &Uuid,
    current_lba: u64,
    backup_lba: u64,
    first_usable_lba: u64,
    last_usable_lba: u64,
    entries_lba: u64,
    entries_crc: u32,
) -> Vec<u8> {
    let mut h = vec![0u8; sector_size as usize];
    h[0..8].copy_from_slice(GPT_SIGNATURE);
    put_u32(&mut h, 8, GPT_REVISION);
    put_u32(&mut h, 12, GPT_HEADER_SIZE);
    // CRC32 at offset 16 computed below with the field zeroed.
    put_u64(&mut h, 24, current_lba);
    put_u64(&mut h, 32, backup_lba);
    put_u64(&mut h, 40, first_usable_lba);
    put_u64(&mut h, 48, last_usable_lba);
    put_guid(&mut h, 56, disk_guid);
    put_u64(&mut h, 72, entries_lba);
    put_u32(&mut h, 80, NUM_ENTRIES);
    put_u32(&mut h, 84, ENTRY_SIZE);
    put_u32(&mut h, 88, entries_crc);

    let header_crc = crc32(&h[..GPT_HEADER_SIZE as usize]);
    put_u32(&mut h, 16, header_crc);
    h
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
    build_header_raw(
        layout.sector_size,
        &layout.disk_guid,
        current_lba,
        backup_lba,
        first_usable_lba,
        last_usable_lba,
        entries_lba,
        entries_crc,
    )
}

fn build_protective_mbr_raw(disk_size: u64, sector_size: u32) -> Vec<u8> {
    let sector = sector_size as u64;
    let total_lba = disk_size / sector;

    let mut mbr = vec![0u8; sector_size as usize];
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

fn build_protective_mbr(layout: &GptLayout) -> Vec<u8> {
    build_protective_mbr_raw(layout.disk_size, layout.sector_size)
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

/// Build the raw write operations that recreate a whole disk's GPT from a
/// captured [`DiskLayout`] — protective MBR + primary/backup headers + a full
/// 128-entry array with every partition's original type/unique GUIDs, LBAs,
/// attributes and name. Preserving the unique GUIDs keeps a UEFI system
/// bootable (BCD references partitions by GUID). The layout may be restored to
/// the same disk or a larger one (offsets/sizes are kept; the backup GPT moves
/// to the target's end); a smaller target, a different sector size, or a
/// partition that would fall off the end is rejected.
pub fn disk_gpt_write_ops(layout: &DiskLayout, target_disk_size: u64) -> Result<Vec<WriteOp>> {
    let sector_size = layout.sector_size;
    if sector_size != 512 && sector_size != 4096 {
        return Err(OwnBackupError::ImageFormat(format!(
            "unsupported sector size {sector_size}"
        )));
    }
    let sector = sector_size as u64;
    if target_disk_size % sector != 0 {
        return Err(OwnBackupError::ImageFormat(
            "target disk size is not a multiple of the sector size".into(),
        ));
    }

    let total_lba = target_disk_size / sector;
    let entries_sectors = ((NUM_ENTRIES * ENTRY_SIZE) as u64).div_ceil(sector); // 16 KiB

    let primary_entries_lba = 2u64;
    let first_usable_lba = primary_entries_lba + entries_sectors;
    let backup_header_lba = total_lba
        .checked_sub(1)
        .ok_or_else(|| OwnBackupError::ImageFormat("target disk has no sectors".into()))?;
    let backup_entries_lba = backup_header_lba - entries_sectors;
    let last_usable_lba = backup_entries_lba - 1;

    // Fill the 128-entry array, each partition at its GPT slot (index − 1).
    let mut entries = vec![0u8; (NUM_ENTRIES * ENTRY_SIZE) as usize];
    for p in &layout.partitions {
        if p.index == 0 || p.index > NUM_ENTRIES {
            return Err(OwnBackupError::ImageFormat(format!(
                "partition index {} out of GPT range 1..={NUM_ENTRIES}",
                p.index
            )));
        }
        if p.first_lba < first_usable_lba || p.last_lba > last_usable_lba {
            return Err(OwnBackupError::RestoreIncompatible(format!(
                "partition {} [{}..{}] does not fit the target disk (usable {}..{})",
                p.index, p.first_lba, p.last_lba, first_usable_lba, last_usable_lba
            )));
        }
        let type_guid = Uuid::parse_str(&p.type_guid)
            .map_err(|e| OwnBackupError::ImageFormat(format!("bad type GUID: {e}")))?;
        let part_guid = Uuid::parse_str(&p.part_guid)
            .map_err(|e| OwnBackupError::ImageFormat(format!("bad partition GUID: {e}")))?;

        let slot = (p.index - 1) as usize;
        let e = &mut entries[slot * ENTRY_SIZE as usize..(slot + 1) * ENTRY_SIZE as usize];
        put_guid(e, 0, &type_guid);
        put_guid(e, 16, &part_guid);
        put_u64(e, 32, p.first_lba);
        put_u64(e, 40, p.last_lba);
        put_u64(e, 48, p.attributes);
        for (i, unit) in p.name.encode_utf16().take(36).enumerate() {
            e[56 + i * 2..56 + i * 2 + 2].copy_from_slice(&unit.to_le_bytes());
        }
    }

    let entries_crc = crc32(&entries);
    let disk_guid = Uuid::parse_str(&layout.disk_guid)
        .map_err(|e| OwnBackupError::ImageFormat(format!("bad disk GUID: {e}")))?;

    let primary_header = build_header_raw(
        sector_size, &disk_guid, 1, backup_header_lba,
        first_usable_lba, last_usable_lba, primary_entries_lba, entries_crc,
    );
    let backup_header = build_header_raw(
        sector_size, &disk_guid, backup_header_lba, 1,
        first_usable_lba, last_usable_lba, backup_entries_lba, entries_crc,
    );

    Ok(vec![
        WriteOp { offset: 0, bytes: build_protective_mbr_raw(target_disk_size, sector_size) },
        WriteOp { offset: sector, bytes: primary_header },
        WriteOp { offset: primary_entries_lba * sector, bytes: entries.clone() },
        WriteOp { offset: backup_entries_lba * sector, bytes: entries },
        WriteOp { offset: backup_header_lba * sector, bytes: backup_header },
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use own_backup_common::gpt::GptPartitionEntry;
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

    // ---- whole-disk GPT recreation (Phase B) ----

    fn disk_layout_two_parts(disk_size: u64) -> DiskLayout {
        DiskLayout {
            disk_guid: Uuid::from_u128(0xDDDD_1111_2222_3333_4444_5555_6666_7777)
                .to_string()
                .to_uppercase(),
            sector_size: 512,
            size_bytes: disk_size,
            partitions: vec![
                // EFI System Partition (100 MiB at 1 MiB), required-flag set.
                GptPartitionEntry {
                    index: 1,
                    type_guid: own_backup_common::gpt::GPT_TYPE_EFI_SYSTEM.into(),
                    part_guid: Uuid::from_u128(0xEF01).to_string().to_uppercase(),
                    first_lba: 2048,
                    last_lba: 2048 + 100 * 1024 * 1024 / 512 - 1,
                    attributes: 1, // required/system partition
                    name: "EFI system partition".into(),
                },
                // Basic data (Windows), immediately after.
                GptPartitionEntry {
                    index: 2,
                    type_guid: BASIC_DATA_GUID.into(),
                    part_guid: Uuid::from_u128(0xC0FE).to_string().to_uppercase(),
                    first_lba: 2048 + 100 * 1024 * 1024 / 512,
                    last_lba: 2048 + 300 * 1024 * 1024 / 512 - 1,
                    attributes: 0,
                    name: "Basic data partition".into(),
                },
            ],
        }
    }

    #[test]
    fn disk_gpt_round_trips_all_partitions() {
        let disk_size = 512 * 1024 * 1024u64; // 512 MiB
        let layout = disk_layout_two_parts(disk_size);

        let mut disk = vec![0u8; disk_size as usize];
        apply_ops(&mut disk, &disk_gpt_write_ops(&layout, disk_size).unwrap());

        // Our own reader recovers every field verbatim.
        let parsed = crate::gpt_reader::parse_disk_layout(&disk, 512, disk_size).unwrap();
        assert_eq!(parsed.disk_guid, layout.disk_guid);
        assert_eq!(parsed.partitions, layout.partitions);

        // …and the independent `gpt` crate accepts the table.
        let gpt_disk = gpt::GptConfig::new()
            .writable(false)
            .open_from_device(Box::new(Cursor::new(disk)))
            .expect("gpt crate must accept the recreated table");
        assert_eq!(gpt_disk.partitions().len(), 2);
    }

    #[test]
    fn disk_gpt_restores_to_a_larger_disk() {
        // Captured on a 512 MiB disk, restored to a 1 GiB disk: partitions keep
        // their offsets; the backup GPT relocates to the larger disk's end.
        let layout = disk_layout_two_parts(512 * 1024 * 1024);
        let bigger = 1024 * 1024 * 1024u64;
        let ops = disk_gpt_write_ops(&layout, bigger).unwrap();
        // Backup header sits at the last sector of the *target* disk.
        assert_eq!(ops.last().unwrap().offset, bigger - 512);
    }

    #[test]
    fn disk_gpt_rejects_partition_off_the_end() {
        let mut layout = disk_layout_two_parts(512 * 1024 * 1024);
        // Shrink the target so partition 2 no longer fits.
        assert!(disk_gpt_write_ops(&layout, 64 * 1024 * 1024).is_err());
        // A partition index beyond the GPT array is rejected too.
        layout.partitions[0].index = 999;
        assert!(disk_gpt_write_ops(&layout, 512 * 1024 * 1024).is_err());
    }
}
