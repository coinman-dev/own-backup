use own_backup_common::image::ImageManifest;
use own_backup_common::Result;

/// Describe the partition layout from a backup manifest in human-readable form.
pub fn describe_partition_layout(manifest: &ImageManifest) -> String {
    let mut lines = Vec::new();

    lines.push(format!(
        "Disk: {} ({:.1} GB)",
        manifest.source_disk.device_id,
        manifest.source_disk.size_bytes as f64 / 1_073_741_824.0
    ));
    lines.push(format!(
        "Partition table: {:?}",
        manifest.partition_table
    ));
    lines.push(format!("Boot mode: {}", manifest.boot_mode));
    lines.push(String::new());

    for part in &manifest.source_disk.partitions {
        lines.push(format!(
            "  Partition {}: {:?} | {:.1} GB | offset {:#x}{}{}",
            part.index,
            part.filesystem,
            part.size_bytes as f64 / 1_073_741_824.0,
            part.offset_bytes,
            if part.is_system { " [EFI]" } else { "" },
            part.volume_letter
                .map(|c| format!(" [{}:]", c))
                .unwrap_or_default(),
        ));
    }

    lines.join("\n")
}

/// Convenience alias for `recreate_partition_table`.
pub fn recreate_gpt_table(target_device: &str, manifest: &ImageManifest) -> Result<()> {
    recreate_partition_table(target_device, manifest)
}

/// Recreate GPT/MBR partition table on target device.
/// This is the actual restore operation — destructive!
pub fn recreate_partition_table(
    _target_device: &str,
    _manifest: &ImageManifest,
) -> Result<()> {
    // TODO: On real hardware, use:
    // 1. For GPT: the `gpt` crate to write GPT headers and partition entries
    //    - Write protective MBR
    //    - Write primary GPT header at LBA 1
    //    - Write partition entries starting at LBA 2
    //    - Write backup GPT header at last LBA
    //    - Match partition offsets, sizes, type GUIDs, and partition GUIDs from manifest
    // 2. For MBR: write MBR boot code + partition table at offset 0
    //    - Match partition types, offsets, sizes from manifest
    //
    // For v1, this function is called from Recovery ISO environment
    // where the target disk is not mounted.
    //
    // Alternative: shell out to sgdisk/sfdisk:
    //   sgdisk --zap-all /dev/sdX
    //   sgdisk --new=1:2048:+512M --typecode=1:EF00 /dev/sdX
    //   etc.

    Err(own_backup_common::OwnBackupError::RestoreIncompatible(
        "Partition table recreation not yet implemented. Use Recovery ISO for full restore.".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use own_backup_common::disk::{DiskInfo, PartitionInfo};
    use own_backup_common::types::*;
    use uuid::Uuid;

    fn make_manifest_with_partitions() -> ImageManifest {
        ImageManifest {
            backup_id: Uuid::nil(),
            created_at: "2026-01-01T00:00:00Z".into(),
            backup_type: BackupType::Full,
            parent_backup_id: None,
            source_os: "windows".into(),
            source_disk: DiskInfo {
                device_id: "\\\\.\\PhysicalDrive0".into(),
                model: "Samsung 970 EVO".into(),
                serial: "S1234567".into(),
                size_bytes: 512_000_000_000, // ~477 GB
                sector_size_logical: 512,
                sector_size_physical: 4096,
                media_type: "SSD".into(),
                partition_table: PartitionTable::Gpt,
                disk_guid: Uuid::nil().to_string(),
                partitions: vec![
                    PartitionInfo {
                        index: 1,
                        offset_bytes: 1_048_576,
                        size_bytes: 536_870_912, // 512 MB
                        gpt_type_guid: "C12A7328-F81F-11D2-BA4B-00A0C93EC93B".into(),
                        gpt_partition_guid: Uuid::new_v4().to_string(),
                        filesystem: FileSystem::Fat32,
                        volume_letter: None,
                        volume_label: "EFI System Partition".into(),
                        fs_uuid: String::new(),
                        is_system: true,
                        is_boot: true,
                    },
                    PartitionInfo {
                        index: 2,
                        offset_bytes: 537_919_488,
                        size_bytes: 511_462_080_512,
                        gpt_type_guid: "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7".into(),
                        gpt_partition_guid: Uuid::new_v4().to_string(),
                        filesystem: FileSystem::Ntfs,
                        volume_letter: Some('C'),
                        volume_label: "Windows".into(),
                        fs_uuid: String::new(),
                        is_system: false,
                        is_boot: false,
                    },
                ],
            },
            boot_mode: BootMode::Uefi,
            partition_table: PartitionTable::Gpt,
            compression_algo: CompressionAlgo::Zstd,
            compression_level: 3,
            encryption_algo: None,
            encryption_salt: None,
            block_size: 1_048_576,
            total_blocks: 488,
            used_blocks: 200,
            backup_mode: BackupMode::UsedSectors,
            checksum_algorithm: "xxhash64".into(),
            preflight_report: None,
            volume_encryption: None,
        }
    }

    #[test]
    fn describe_layout_includes_disk_info() {
        let manifest = make_manifest_with_partitions();
        let description = describe_partition_layout(&manifest);

        assert!(description.contains("PhysicalDrive0"));
        assert!(description.contains("Gpt"));
        assert!(description.contains("UEFI"));
    }

    #[test]
    fn describe_layout_includes_partitions() {
        let manifest = make_manifest_with_partitions();
        let description = describe_partition_layout(&manifest);

        assert!(description.contains("Partition 1"));
        assert!(description.contains("Partition 2"));
        assert!(description.contains("[EFI]"));
        assert!(description.contains("[C:]"));
        assert!(description.contains("Fat32"));
        assert!(description.contains("Ntfs"));
    }

    #[test]
    fn describe_layout_no_partitions() {
        let mut manifest = make_manifest_with_partitions();
        manifest.source_disk.partitions.clear();
        let description = describe_partition_layout(&manifest);

        assert!(description.contains("PhysicalDrive0"));
        // Should not contain numbered partitions like "Partition 1"
        assert!(!description.contains("Partition 1"));
    }
}
