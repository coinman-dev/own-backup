//! Windows block reader using raw device I/O.
//!
//! Uses [`WinRawDevice`] (`CreateFileW` + `FILE_FLAG_NO_BUFFERING`) to read
//! fixed-size blocks from physical drives, volumes or VSS snapshot device
//! paths.

#![cfg(target_os = "windows")]

use own_backup_common::{OwnBackupError, Result};

use crate::block_reader::BlockReader;
use crate::win_device::WinRawDevice;

/// Reads raw blocks from a Windows device.
pub struct WinBlockReader {
    device: WinRawDevice,
    block_size: u32,
    total_blocks: u64,
}

impl WinBlockReader {
    /// Open a device path for raw block reads.
    ///
    /// `device_path` may be `\\.\PhysicalDrive0`, `\\.\C:`, a plain `C:` or a
    /// VSS snapshot device path. `block_size` must be a multiple of the device
    /// sector size.
    pub fn open(device_path: &str, block_size: u32) -> Result<Self> {
        let device = WinRawDevice::open_read(device_path)?;

        if block_size == 0 || block_size % device.sector_size() != 0 {
            return Err(OwnBackupError::Device(format!(
                "block size {block_size} is not a multiple of the device sector size {}",
                device.sector_size()
            )));
        }

        let total_blocks = device.length().div_ceil(block_size as u64);
        Ok(Self {
            device,
            block_size,
            total_blocks,
        })
    }

    /// Total device length in bytes.
    pub fn device_length(&self) -> u64 {
        self.device.length()
    }

    /// Logical sector size of the underlying device.
    pub fn sector_size(&self) -> u32 {
        self.device.sector_size()
    }
}

impl BlockReader for WinBlockReader {
    fn read_block(&mut self, block_number: u64) -> Result<Vec<u8>> {
        if block_number >= self.total_blocks {
            return Err(OwnBackupError::Device(format!(
                "block {block_number} is beyond device end ({} blocks)",
                self.total_blocks
            )));
        }
        let offset = block_number * self.block_size as u64;
        let len = std::cmp::min(self.block_size as u64, self.device.length() - offset) as usize;

        let mut buf = vec![0u8; len];
        self.device.read_at(offset, &mut buf)?;
        Ok(buf)
    }

    fn block_size(&self) -> u32 {
        self.block_size
    }

    fn total_blocks(&self) -> u64 {
        self.total_blocks
    }
}
