use async_trait::async_trait;
use own_backup_common::disk::DiskInfo;
use own_backup_common::Result;

/// Trait for enumerating physical disks and their partition layouts.
///
/// On Windows this is backed by WMI queries; on Linux we provide a mock
/// implementation for testing the rest of the backup pipeline.
#[async_trait]
pub trait DiskInventory: Send + Sync {
    /// List every physical disk visible to the OS.
    async fn enumerate_disks(&self) -> Result<Vec<DiskInfo>>;

    /// Fetch detailed information for a single disk identified by `device_id`
    /// (e.g. `\\.\PhysicalDrive0` on Windows).
    async fn get_disk_details(&self, device_id: &str) -> Result<DiskInfo>;
}
