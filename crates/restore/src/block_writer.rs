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

// Windows-specific block writer using CreateFile + WriteFile
#[cfg(target_os = "windows")]
pub struct WinBlockWriter {
    // handle: windows::Win32::Foundation::HANDLE,
    // block_size: u32,
}

#[cfg(target_os = "windows")]
impl WinBlockWriter {
    /// Open a device for raw block writing.
    /// Uses CreateFile with GENERIC_WRITE, FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH
    pub fn open(_device_path: &str, _block_size: u32) -> Result<Self> {
        // TODO: Implement with windows-rs
        // let handle = unsafe {
        //     CreateFileW(
        //         device_path,
        //         GENERIC_WRITE.0,
        //         FILE_SHARE_READ | FILE_SHARE_WRITE,
        //         None,
        //         OPEN_EXISTING,
        //         FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH,
        //         None,
        //     )?
        // };
        todo!("WinBlockWriter::open")
    }
}

#[cfg(target_os = "windows")]
impl BlockWriter for WinBlockWriter {
    fn write_block(&mut self, _block_number: u64, _data: &[u8]) -> Result<()> {
        todo!("WinBlockWriter::write_block")
    }

    fn block_size(&self) -> u32 {
        todo!("WinBlockWriter::block_size")
    }

    fn flush(&mut self) -> Result<()> {
        todo!("WinBlockWriter::flush")
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
