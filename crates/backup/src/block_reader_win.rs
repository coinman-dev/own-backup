//! Windows block reader using raw device I/O.
//!
//! This module is only compiled on Windows (`#[cfg(target_os = "windows")]` in
//! `lib.rs`). It uses `CreateFileW` with `FILE_FLAG_NO_BUFFERING` to open
//! physical drives or VSS snapshot device paths for aligned, direct reads.
//!
//! ## Design notes
//!
//! - `CreateFileW` is used with `GENERIC_READ`, `FILE_SHARE_READ |
//!   FILE_SHARE_WRITE`, and `FILE_FLAG_NO_BUFFERING` to bypass the filesystem
//!   cache and read raw sectors.
//! - All reads must be aligned to the sector size (typically 512 or 4096).
//!   The `WinBlockReader` allocates an aligned buffer internally.
//! - The block size (from the backup manifest) must be a multiple of the
//!   sector size.
//! - `DeviceIoControl` with `IOCTL_DISK_GET_LENGTH_INFO` is used to determine
//!   the total size of the device for computing `total_blocks`.

use own_backup_common::error::Result;

use crate::block_reader::BlockReader;

/// Reads raw blocks from a Windows device handle.
///
/// Opened via `CreateFileW` with `FILE_FLAG_NO_BUFFERING` for direct,
/// sector-aligned reads.
pub struct WinBlockReader {
    // TODO: fields will include:
    // - handle: windows::Win32::Foundation::HANDLE
    // - block_size: u32
    // - total_blocks: u64
    // - sector_size: u32  (for alignment)
    _private: (),
}

impl WinBlockReader {
    /// Open a device path for raw block reads.
    ///
    /// `device_path` is typically a VSS snapshot path or a physical drive
    /// path like `\\.\PhysicalDrive0`.
    ///
    /// `block_size` must be a multiple of the device sector size.
    pub fn open(_device_path: &str, _block_size: u32) -> Result<Self> {
        todo!(
            "Open device with CreateFileW + FILE_FLAG_NO_BUFFERING, \
             query IOCTL_DISK_GET_LENGTH_INFO for total size, \
             compute total_blocks"
        )
    }
}

impl BlockReader for WinBlockReader {
    fn read_block(&mut self, _block_number: u64) -> Result<Vec<u8>> {
        todo!(
            "Seek to block_number * block_size, \
             ReadFile into sector-aligned buffer, \
             return the block data"
        )
    }

    fn block_size(&self) -> u32 {
        todo!("return self.block_size")
    }

    fn total_blocks(&self) -> u64 {
        todo!("return self.total_blocks")
    }
}
