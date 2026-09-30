//! One entry point that turns a [`PartitionTableSpec`] into raw disk writes,
//! whichever scheme it uses (R3b).
//!
//! [`crate::gpt_writer`] and [`crate::mbr_writer`] each know one format. This
//! module picks between them and handles the thing neither can see on its own:
//! **what the previous table left behind**.
//!
//! Writing an MBR onto a disk that used to hold a GPT does not remove the GPT.
//! Its primary header sits at LBA 1 and a full mirror sits in the last 33
//! sectors, both outside the 512 bytes an MBR occupies. Windows decides the
//! scheme from LBA 0 alone, so it would read the new MBR and be satisfied — but
//! every other tool that scans for a GPT signature would find one and disagree
//! about what is on the disk. So the stale structures are zeroed as part of
//! writing the new table.

use own_backup_common::gpt::{DiskLayout, GptPartitionEntry};
use own_backup_common::restore_plan::{PartitionKind, PartitionScheme, PartitionTableSpec};
use own_backup_common::{OwnBackupError, Result};
use uuid::Uuid;

use crate::gpt_writer::{disk_gpt_write_ops, WriteOp};
use crate::mbr_writer::mbr_write_ops;

/// Sectors a GPT occupies at each end of the disk: header + 128×128 B entries.
const GPT_STRUCTURE_SECTORS: u64 = 33;

/// Build every raw write needed to put `spec` on a disk — the table itself plus
/// the erasure of whatever scheme was there before.
pub fn partition_table_write_ops(spec: &PartitionTableSpec) -> Result<Vec<WriteOp>> {
    match spec.scheme {
        PartitionScheme::Gpt => gpt_ops(spec),
        PartitionScheme::Mbr => {
            let mut ops = mbr_write_ops(spec)?;
            ops.extend(erase_stale_gpt(spec));
            Ok(ops)
        }
    }
}

/// Convert the scheme-neutral spec into the [`DiskLayout`] the GPT writer
/// speaks, filling in identities the designer left blank.
fn gpt_ops(spec: &PartitionTableSpec) -> Result<Vec<WriteOp>> {
    let mut partitions = Vec::with_capacity(spec.entries.len());
    for e in &spec.entries {
        let PartitionKind::Gpt {
            type_guid,
            part_guid,
            attributes,
            name,
        } = &e.kind
        else {
            return Err(OwnBackupError::ImageFormat(format!(
                "partition {} carries MBR attributes but the table is GPT",
                e.index
            )));
        };
        partitions.push(GptPartitionEntry {
            index: e.index,
            type_guid: type_guid.clone(),
            // A partition restored from a backup brings its original unique
            // GUID, because the BCD and UEFI reference partitions by it. A
            // hand-created one has none yet, and any fresh value will do.
            part_guid: or_new_guid(part_guid),
            first_lba: e.first_lba,
            last_lba: e.last_lba,
            attributes: *attributes,
            name: name.clone(),
        });
    }

    let layout = DiskLayout {
        // The disk GUID is deliberately *not* inherited from the source of a
        // whole-disk backup: two disks with the same GPT GUID make Windows take
        // one of them offline.
        disk_guid: or_new_guid(&spec.disk_id),
        sector_size: spec.sector_size,
        size_bytes: spec.disk_size_bytes,
        partitions,
    };
    disk_gpt_write_ops(&layout, spec.disk_size_bytes)
}

fn or_new_guid(existing: &str) -> String {
    if existing.trim().is_empty() {
        Uuid::new_v4().to_string().to_uppercase()
    } else {
        existing.to_string()
    }
}

/// Zero the sectors a GPT would have lived in, so a disk converted to MBR does
/// not read as both.
///
/// The head of the disk (LBA 1 up to 33) is always safe to zero: no partition
/// may start before LBA 1, and in practice the first one starts at 1 MiB. The
/// tail is only zeroed when no partition reaches into it — an MBR layout is
/// free to use the very last sector, and destroying live data to tidy up a
/// stale signature would be a poor trade.
fn erase_stale_gpt(spec: &PartitionTableSpec) -> Vec<WriteOp> {
    let sector = spec.sector_size.max(512) as u64;
    let total_lba = spec.disk_size_bytes / sector;
    let mut ops = Vec::new();

    let head_end = spec
        .entries
        .iter()
        .map(|e| e.first_lba)
        .min()
        .unwrap_or(u64::MAX)
        .min(GPT_STRUCTURE_SECTORS + 1);
    if head_end > 1 {
        ops.push(WriteOp {
            offset: sector,
            bytes: vec![0u8; ((head_end - 1) * sector) as usize],
        });
    }

    let tail_start = total_lba.saturating_sub(GPT_STRUCTURE_SECTORS);
    let tail_is_free = tail_start > 0
        && !spec
            .entries
            .iter()
            .any(|e| e.end_lba_exclusive() > tail_start);
    if tail_is_free {
        ops.push(WriteOp {
            offset: tail_start * sector,
            bytes: vec![0u8; ((total_lba - tail_start) * sector) as usize],
        });
    }

    ops
}

#[cfg(test)]
mod tests {
    use super::*;
    use own_backup_common::restore_plan::PartitionSpec;

    const MIB: u64 = 1024 * 1024;

    fn gpt_spec(disk_size: u64) -> PartitionTableSpec {
        PartitionTableSpec {
            scheme: PartitionScheme::Gpt,
            sector_size: 512,
            disk_size_bytes: disk_size,
            disk_id: String::new(),
            entries: vec![PartitionSpec {
                index: 1,
                first_lba: 2048,
                last_lba: 2048 + 100 * MIB / 512 - 1,
                kind: PartitionKind::Gpt {
                    type_guid: own_backup_common::gpt::GPT_TYPE_EFI_SYSTEM.into(),
                    part_guid: String::new(),
                    attributes: 0,
                    name: "ESP".into(),
                },
            }],
        }
    }

    fn mbr_spec(disk_size: u64, last_lba: u64) -> PartitionTableSpec {
        PartitionTableSpec {
            scheme: PartitionScheme::Mbr,
            sector_size: 512,
            disk_size_bytes: disk_size,
            disk_id: "1A2B3C4D".into(),
            entries: vec![PartitionSpec {
                index: 1,
                first_lba: 2048,
                last_lba,
                kind: PartitionKind::Mbr {
                    type_byte: 0x07,
                    bootable: true,
                    logical: false,
                },
            }],
        }
    }

    #[test]
    fn a_gpt_spec_produces_a_table_our_own_reader_understands() {
        let disk_size = 512 * MIB;
        let spec = gpt_spec(disk_size);
        let mut disk = vec![0u8; disk_size as usize];
        for op in partition_table_write_ops(&spec).unwrap() {
            disk[op.offset as usize..][..op.bytes.len()].copy_from_slice(&op.bytes);
        }
        let parsed = crate::gpt_reader::parse_disk_layout(&disk, 512, disk_size).unwrap();
        assert_eq!(parsed.partitions.len(), 1);
        assert_eq!(parsed.partitions[0].first_lba, 2048);
        assert_eq!(parsed.partitions[0].name, "ESP");
    }

    #[test]
    fn a_blank_partition_guid_is_filled_in() {
        let disk_size = 512 * MIB;
        let mut disk = vec![0u8; disk_size as usize];
        for op in partition_table_write_ops(&gpt_spec(disk_size)).unwrap() {
            disk[op.offset as usize..][..op.bytes.len()].copy_from_slice(&op.bytes);
        }
        let parsed = crate::gpt_reader::parse_disk_layout(&disk, 512, disk_size).unwrap();
        assert!(Uuid::parse_str(&parsed.partitions[0].part_guid).is_ok());
        assert_ne!(
            parsed.partitions[0].part_guid,
            Uuid::nil().to_string().to_uppercase()
        );
    }

    #[test]
    fn writing_an_mbr_erases_the_gpt_that_was_there() {
        // Start from a disk carrying a valid GPT, write an MBR over it, and the
        // GPT must no longer parse — otherwise the disk reads as both.
        let disk_size = 512 * MIB;
        let mut disk = vec![0u8; disk_size as usize];
        for op in partition_table_write_ops(&gpt_spec(disk_size)).unwrap() {
            disk[op.offset as usize..][..op.bytes.len()].copy_from_slice(&op.bytes);
        }
        assert!(crate::gpt_reader::parse_disk_layout(&disk, 512, disk_size).is_ok());

        for op in
            partition_table_write_ops(&mbr_spec(disk_size, 2048 + 100 * MIB / 512 - 1)).unwrap()
        {
            disk[op.offset as usize..][..op.bytes.len()].copy_from_slice(&op.bytes);
        }
        assert!(
            crate::gpt_reader::parse_disk_layout(&disk, 512, disk_size).is_err(),
            "the stale GPT survived the conversion to MBR"
        );
        // …and the MBR itself is intact.
        assert_eq!(disk[510], 0x55);
        assert_eq!(disk[511], 0xAA);
        assert_eq!(disk[0x1BE + 4], 0x07);
    }

    #[test]
    fn the_gpt_tail_is_left_alone_when_a_partition_occupies_it() {
        // An MBR layout may legitimately use the last sector. Zeroing it to tidy
        // away a stale signature would destroy live data.
        let disk_size = 512 * MIB;
        let last = disk_size / 512 - 1;
        let ops = partition_table_write_ops(&mbr_spec(disk_size, last)).unwrap();
        let tail_start = (disk_size / 512 - GPT_STRUCTURE_SECTORS) * 512;
        assert!(
            !ops.iter().any(|op| op.offset >= tail_start),
            "a write landed in the tail a partition is using"
        );
        // The head is still cleaned — nothing can live at LBA 1..33.
        assert!(ops.iter().any(|op| op.offset == 512));
    }

    #[test]
    fn the_head_erase_never_reaches_the_first_partition() {
        let disk_size = 512 * MIB;
        let mut spec = mbr_spec(disk_size, 100_000);
        spec.entries[0].first_lba = 8; // pathological, but must not be clobbered
        let (first, last) = (spec.entries[0].first_lba, spec.entries[0].last_lba);
        for op in partition_table_write_ops(&spec).unwrap() {
            let start = op.offset / 512;
            let end = start + op.bytes.len() as u64 / 512;
            assert!(
                end <= first || start > last,
                "a write at LBA {start}..{end} landed inside the partition {first}..{last}"
            );
        }
    }

    #[test]
    fn a_gpt_entry_kind_in_an_mbr_spec_is_refused_and_vice_versa() {
        let mut spec = gpt_spec(512 * MIB);
        spec.scheme = PartitionScheme::Mbr;
        assert!(partition_table_write_ops(&spec).is_err());

        let mut spec = mbr_spec(512 * MIB, 100_000);
        spec.scheme = PartitionScheme::Gpt;
        assert!(partition_table_write_ops(&spec).is_err());
    }
}
