use own_backup_common::Result;

pub trait BlockWriter: Send {
    fn write_block(&mut self, block_number: u64, data: &[u8]) -> Result<()>;
    fn block_size(&self) -> u32;
    fn flush(&mut self) -> Result<()>;
}

/// Mock block writer that stores all written blocks in memory.
pub struct MockBlockWriter {
    pub written_blocks: Vec<(u64, Vec<u8>)>,
    block_size: u32,
}

impl MockBlockWriter {
    pub fn new(block_size: u32) -> Self {
        Self {
            written_blocks: Vec::new(),
            block_size,
        }
    }

    pub fn get_block(&self, block_number: u64) -> Option<&[u8]> {
        self.written_blocks
            .iter()
            .find(|(n, _)| *n == block_number)
            .map(|(_, data)| data.as_slice())
    }
}

impl BlockWriter for MockBlockWriter {
    fn write_block(&mut self, block_number: u64, data: &[u8]) -> Result<()> {
        self.written_blocks.push((block_number, data.to_vec()));
        Ok(())
    }

    fn block_size(&self) -> u32 {
        self.block_size
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Windows block writer over raw device I/O
/// (`CreateFileW` + `FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH`).
///
/// Targets a restore destination (`\\.\PhysicalDriveN`, `\\.\D:`) or an
/// attached VHDX being written during a backup.
#[cfg(target_os = "windows")]
pub struct WinBlockWriter {
    device: own_backup_backup::win_device::WinRawDevice,
    block_size: u32,
}

#[cfg(target_os = "windows")]
impl WinBlockWriter {
    /// Open a device for raw block writing.
    ///
    /// `block_size` must be a multiple of the device sector size.
    pub fn open(device_path: &str, block_size: u32) -> Result<Self> {
        let device = own_backup_backup::win_device::WinRawDevice::open_write(device_path)?;
        if block_size == 0 || block_size % device.sector_size() != 0 {
            return Err(own_backup_common::OwnBackupError::Device(format!(
                "block size {block_size} is not a multiple of the device sector size {}",
                device.sector_size()
            )));
        }
        Ok(Self { device, block_size })
    }

    /// Total device length in bytes.
    pub fn device_length(&self) -> u64 {
        self.device.length()
    }
}

#[cfg(target_os = "windows")]
impl BlockWriter for WinBlockWriter {
    fn write_block(&mut self, block_number: u64, data: &[u8]) -> Result<()> {
        let offset = block_number * self.block_size as u64;
        self.device.write_at(offset, data)
    }

    fn block_size(&self) -> u32 {
        self.block_size
    }

    fn flush(&mut self) -> Result<()> {
        self.device.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_write_and_read() {
        let mut writer = MockBlockWriter::new(4096);
        writer.write_block(0, &[1; 4096]).unwrap();
        writer.write_block(5, &[2; 4096]).unwrap();

        assert_eq!(writer.written_blocks.len(), 2);
        assert_eq!(writer.get_block(0).unwrap(), &[1; 4096]);
        assert_eq!(writer.get_block(5).unwrap(), &[2; 4096]);
        assert!(writer.get_block(1).is_none());
    }
}
