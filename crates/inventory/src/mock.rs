use async_trait::async_trait;
use own_backup_common::disk::{DiskInfo, PartitionInfo};
use own_backup_common::types::{FileSystem, PartitionTable};
use own_backup_common::{OwnBackupError, Result};

use crate::traits::DiskInventory;

/// Mock disk inventory for testing on Linux.
///
/// Returns a hardcoded layout that mimics a typical 500 GB Windows system disk
/// with three partitions: EFI System Partition, Microsoft Reserved (MSR), and
/// the primary NTFS system volume.
pub struct MockInventory;

impl MockInventory {
    pub fn new() -> Self {
        MockInventory
    }

    /// Build the hardcoded mock disk.
    fn mock_disk() -> DiskInfo {
        let efi_partition = PartitionInfo {
            index: 1,
            offset_bytes: 1_048_576, // 1 MiB
            size_bytes: 104_857_600, // 100 MiB
            used_bytes: 0,           // letterless — UI shows "—"
            gpt_type_guid: "C12A7328-F81F-11D2-BA4B-00A0C93EC93B".into(),
            gpt_partition_guid: "A1B2C3D4-0001-4000-8000-000000000001".into(),
            filesystem: FileSystem::Fat32,
            volume_letter: None,
            volume_device: String::new(),
            volume_label: "EFI System Partition".into(),
            fs_uuid: "1234-ABCD".into(),
            is_system: false,
            has_os: false,
            is_boot: true,
            linux_last_mounted: String::new(),
            is_encrypted: false,
            unlock_methods: Vec::new(),
            recovery_key_only: false,
            unlockable: true,
        };

        let msr_partition = PartitionInfo {
            index: 2,
            offset_bytes: 105_906_176, // right after EFI
            size_bytes: 16_777_216,    // 16 MiB
            used_bytes: 0,
            gpt_type_guid: "E3C9E316-0B5C-4DB8-817D-F92DF00215AE".into(),
            gpt_partition_guid: "A1B2C3D4-0002-4000-8000-000000000002".into(),
            filesystem: FileSystem::Unknown,
            volume_letter: None,
            volume_device: String::new(),
            volume_label: "Microsoft Reserved".into(),
            fs_uuid: String::new(),
            is_system: false,
            has_os: false,
            is_boot: false,
            linux_last_mounted: String::new(),
            is_encrypted: false,
            unlock_methods: Vec::new(),
            recovery_key_only: false,
            unlockable: true,
        };

        let ntfs_partition = PartitionInfo {
            index: 3,
            offset_bytes: 122_683_392,   // after MSR
            size_bytes: 499_877_316_608, // ~465 GB
            used_bytes: 187_904_819_200, // ~175 GB used
            gpt_type_guid: "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7".into(),
            gpt_partition_guid: "A1B2C3D4-0003-4000-8000-000000000003".into(),
            filesystem: FileSystem::Ntfs,
            volume_letter: Some('C'),
            volume_device: "C:".into(),
            volume_label: "Windows".into(),
            fs_uuid: "AABB1122CCDD3344".into(),
            is_system: true,
            has_os: true,
            is_boot: false,
            linux_last_mounted: String::new(),
            is_encrypted: false,
            unlock_methods: Vec::new(),
            recovery_key_only: false,
            unlockable: true,
        };

        DiskInfo {
            device_id: r"\\.\PhysicalDrive0".into(),
            model: "Samsung SSD 970 EVO Plus 500GB".into(),
            serial: "S4EWNX0R123456".into(),
            size_bytes: 500_107_862_016, // ~500 GB
            sector_size_logical: 512,
            sector_size_physical: 4096,
            media_type: "SSD".into(),
            partition_table: PartitionTable::Gpt,
            disk_guid: "DEADBEEF-1234-5678-9ABC-DEF012345678".into(),
            partitions: vec![efi_partition, msr_partition, ntfs_partition],
        }
    }
}

impl Default for MockInventory {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl DiskInventory for MockInventory {
    async fn enumerate_disks(&self) -> Result<Vec<DiskInfo>> {
        Ok(vec![Self::mock_disk()])
    }

    async fn get_disk_details(&self, device_id: &str) -> Result<DiskInfo> {
        let disk = Self::mock_disk();
        if disk.device_id == device_id {
            Ok(disk)
        } else {
            Err(OwnBackupError::Inventory(format!(
                "Mock: no disk found with device_id '{device_id}'"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn enumerate_returns_one_disk() {
        let inv = MockInventory::new();
        let disks = inv.enumerate_disks().await.unwrap();

        assert_eq!(disks.len(), 1);
        let disk = &disks[0];
        assert_eq!(disk.device_id, r"\\.\PhysicalDrive0");
        assert_eq!(disk.partition_table, PartitionTable::Gpt);
        assert_eq!(disk.partitions.len(), 3);
    }

    #[tokio::test]
    async fn get_disk_details_found() {
        let inv = MockInventory::new();
        let disk = inv
            .get_disk_details(r"\\.\PhysicalDrive0")
            .await
            .unwrap();

        assert_eq!(disk.model, "Samsung SSD 970 EVO Plus 500GB");
        assert_eq!(disk.size_bytes, 500_107_862_016);
        assert_eq!(disk.sector_size_logical, 512);
        assert_eq!(disk.sector_size_physical, 4096);

        // Verify partition layout
        let efi = &disk.partitions[0];
        assert_eq!(efi.filesystem, FileSystem::Fat32);
        assert!(efi.is_boot);
        assert!(!efi.is_system);

        let msr = &disk.partitions[1];
        assert_eq!(msr.filesystem, FileSystem::Unknown);
        assert_eq!(msr.volume_label, "Microsoft Reserved");

        let ntfs = &disk.partitions[2];
        assert_eq!(ntfs.filesystem, FileSystem::Ntfs);
        assert_eq!(ntfs.volume_letter, Some('C'));
        assert!(ntfs.is_system);
    }

    #[tokio::test]
    async fn get_disk_details_not_found() {
        let inv = MockInventory::new();
        let result = inv.get_disk_details(r"\\.\PhysicalDrive99").await;

        assert!(result.is_err());
    }
}
