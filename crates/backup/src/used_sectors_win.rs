//! Windows used-sector bitmap via FSCTL_GET_VOLUME_BITMAP.
//!
//! This module is only compiled on Windows. It queries the NTFS allocation
//! bitmap to determine which clusters are in use, allowing smart-sector
//! backups to skip unallocated regions.
//!
//! ## How it works
//!
//! 1. Open the volume with `CreateFileW` (e.g. `\\.\C:`).
//! 2. Issue `DeviceIoControl` with `FSCTL_GET_VOLUME_BITMAP`.
//!    The output is a `VOLUME_BITMAP_BUFFER` containing:
//!    - `StartingLcn` (LARGE_INTEGER)
//!    - `BitmapSize`  (LARGE_INTEGER)
//!    - `Buffer[1]`   (the bitmap, 1 bit per cluster)
//! 3. Map cluster numbers to block numbers using cluster_size / block_size.
//! 4. Cache the bitmap in memory for O(1) lookups during the backup.
//!
//! ## Limitations
//!
//! - Only works on NTFS volumes (FAT32/exFAT would need different approaches).
//! - Requires admin/backup-operator privileges.
//! - The bitmap can be very large for multi-TB volumes (1 bit per 4 KiB
//!   cluster = ~32 MB for a 1 TB volume).

use crate::used_sectors::UsedSectorMap;
use own_backup_common::error::Result;

/// NTFS used-sector map backed by `FSCTL_GET_VOLUME_BITMAP`.
pub struct NtfsUsedSectorMap {
    // TODO: fields will include:
    // - bitmap: Vec<u8>        (the raw bitmap from FSCTL_GET_VOLUME_BITMAP)
    // - cluster_size: u32      (bytes per cluster, typically 4096)
    // - block_size: u32        (backup block size, typically 1 MiB)
    // - clusters_per_block: u32
    // - total_blocks: u64
    // - used_count: u64        (cached count of used blocks)
    _private: (),
}

impl NtfsUsedSectorMap {
    /// Query the NTFS volume bitmap and build the used-sector map.
    ///
    /// `volume_path` is e.g. `\\.\C:`.
    /// `block_size` is the backup block size in bytes.
    pub fn open(_volume_path: &str, _block_size: u32) -> Result<Self> {
        todo!(
            "Open volume with CreateFileW, \
             DeviceIoControl(FSCTL_GET_VOLUME_BITMAP), \
             parse VOLUME_BITMAP_BUFFER, \
             compute clusters_per_block and used_count"
        )
    }
}

impl UsedSectorMap for NtfsUsedSectorMap {
    fn is_block_used(&self, _block_number: u64) -> bool {
        todo!(
            "Check if any cluster in the block's range has its bitmap bit set"
        )
    }

    fn used_block_count(&self) -> u64 {
        todo!("return self.used_count")
    }
}
