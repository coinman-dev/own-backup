use own_backup_common::error::Result;

/// Trait for reading raw blocks from a device or file.
///
/// Implementations read fixed-size blocks identified by block number.
/// The total number of blocks is known in advance.
pub trait BlockReader: Send {
    /// Read block `block_number` and return its data.
    ///
    /// The returned `Vec<u8>` has exactly `block_size()` bytes, except
    /// possibly the last block which may be shorter if the source size is not
    /// a perfect multiple of the block size.
    fn read_block(&mut self, block_number: u64) -> Result<Vec<u8>>;

    /// The fixed block size in bytes.
    fn block_size(&self) -> u32;

    /// Total number of blocks in the source.
    fn total_blocks(&self) -> u64;
}

/// A mock block reader that reads from an in-memory `Vec<u8>`, splitting it
/// into fixed-size blocks. Useful for testing.
pub struct MockBlockReader {
    data: Vec<u8>,
    block_size: u32,
    total_blocks: u64,
}

impl MockBlockReader {
    /// Create a `MockBlockReader` over the given data with the specified block
    /// size. The data is split into ceil(data.len() / block_size) blocks.
    pub fn new(data: Vec<u8>, block_size: u32) -> Self {
        let total_blocks = if data.is_empty() {
            0
        } else {
            (data.len() as u64 + block_size as u64 - 1) / block_size as u64
        };
        Self {
            data,
            block_size,
            total_blocks,
        }
    }
}

impl BlockReader for MockBlockReader {
    fn read_block(&mut self, block_number: u64) -> Result<Vec<u8>> {
        let start = block_number as usize * self.block_size as usize;
        if start >= self.data.len() {
            return Err(own_backup_common::OwnBackupError::Io(
                std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!("block {block_number} is beyond data length"),
                ),
            ));
        }
        let end = std::cmp::min(start + self.block_size as usize, self.data.len());
        Ok(self.data[start..end].to_vec())
    }

    fn block_size(&self) -> u32 {
        self.block_size
    }

    fn total_blocks(&self) -> u64 {
        self.total_blocks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_reader_exact_blocks() {
        let data = vec![0xAA; 1024];
        let mut reader = MockBlockReader::new(data, 256);
        assert_eq!(reader.total_blocks(), 4);
        assert_eq!(reader.block_size(), 256);

        for i in 0..4 {
            let block = reader.read_block(i).unwrap();
            assert_eq!(block.len(), 256);
            assert!(block.iter().all(|&b| b == 0xAA));
        }
    }

    #[test]
    fn mock_reader_partial_last_block() {
        let data = vec![0xBB; 500];
        let mut reader = MockBlockReader::new(data, 256);
        assert_eq!(reader.total_blocks(), 2);

        let b0 = reader.read_block(0).unwrap();
        assert_eq!(b0.len(), 256);

        let b1 = reader.read_block(1).unwrap();
        assert_eq!(b1.len(), 244); // 500 - 256
    }

    #[test]
    fn mock_reader_out_of_bounds() {
        let data = vec![0xCC; 100];
        let mut reader = MockBlockReader::new(data, 256);
        assert!(reader.read_block(1).is_err());
    }

    #[test]
    fn mock_reader_empty() {
        let reader = MockBlockReader::new(Vec::new(), 256);
        assert_eq!(reader.total_blocks(), 0);
    }
}
