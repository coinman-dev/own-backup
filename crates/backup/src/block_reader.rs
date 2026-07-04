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

impl BlockReader for Box<dyn BlockReader> {
    fn read_block(&mut self, block_number: u64) -> Result<Vec<u8>> {
        (**self).read_block(block_number)
    }
    fn block_size(&self) -> u32 {
        (**self).block_size()
    }
    fn total_blocks(&self) -> u64 {
        (**self).total_blocks()
    }
}

/// Wraps a [`BlockReader`] and overlays `patch_bytes` at a fixed byte offset
/// of the source, replacing whatever the inner reader returned there.
///
/// Used for the NTFS backup boot sector: the last sector of a volume cannot
/// be read through the volume handle (short read → zero fill), but its
/// correct content is by definition a copy of sector 0. Storing the real
/// copy instead of zeros keeps the image NTFS-consistent, so a transiently
/// auto-mounted backup chain gives NTFS nothing to "self-heal" — and even if
/// it does write, it writes byte-identical data and checksums still match.
pub struct PatchReader<R: BlockReader> {
    inner: R,
    patch_offset: u64,
    patch_bytes: Vec<u8>,
}

impl<R: BlockReader> PatchReader<R> {
    pub fn new(inner: R, patch_offset: u64, patch_bytes: Vec<u8>) -> Self {
        Self {
            inner,
            patch_offset,
            patch_bytes,
        }
    }
}

impl<R: BlockReader> BlockReader for PatchReader<R> {
    fn read_block(&mut self, block_number: u64) -> Result<Vec<u8>> {
        let mut data = self.inner.read_block(block_number)?;
        let block_start = block_number * self.inner.block_size() as u64;
        let block_end = block_start + data.len() as u64;
        let patch_end = self.patch_offset + self.patch_bytes.len() as u64;

        if self.patch_offset < block_end && patch_end > block_start {
            let from = self.patch_offset.max(block_start);
            let to = patch_end.min(block_end);
            let dst = (from - block_start) as usize..(to - block_start) as usize;
            let src = (from - self.patch_offset) as usize..(to - self.patch_offset) as usize;
            data[dst].copy_from_slice(&self.patch_bytes[src]);
        }
        Ok(data)
    }

    fn block_size(&self) -> u32 {
        self.inner.block_size()
    }

    fn total_blocks(&self) -> u64 {
        self.inner.total_blocks()
    }
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

    #[test]
    fn patch_reader_replaces_tail_sector() {
        // 4 blocks of 256; patch the last 64 bytes with a copy of the first 64
        // (the backup-boot-sector shape: last sector := sector 0).
        let data: Vec<u8> = (0..1024u32).map(|i| (i % 251) as u8).collect();
        let sector0: Vec<u8> = data[..64].to_vec();
        let mut reader = PatchReader::new(MockBlockReader::new(data.clone(), 256), 1024 - 64, sector0.clone());

        // Untouched blocks come through verbatim.
        assert_eq!(reader.read_block(0).unwrap(), &data[0..256]);
        assert_eq!(reader.read_block(2).unwrap(), &data[512..768]);
        // Last block: head intact, tail replaced.
        let last = reader.read_block(3).unwrap();
        assert_eq!(&last[..192], &data[768..960]);
        assert_eq!(&last[192..], &sector0[..]);
    }

    #[test]
    fn patch_reader_spanning_blocks() {
        // Patch straddles the block boundary at 256.
        let data = vec![0u8; 512];
        let mut reader = PatchReader::new(MockBlockReader::new(data, 256), 250, vec![0xAB; 12]);
        let b0 = reader.read_block(0).unwrap();
        let b1 = reader.read_block(1).unwrap();
        assert!(b0[..250].iter().all(|&b| b == 0));
        assert!(b0[250..].iter().all(|&b| b == 0xAB));
        assert!(b1[..6].iter().all(|&b| b == 0xAB));
        assert!(b1[6..].iter().all(|&b| b == 0));
    }

    #[test]
    fn patch_reader_short_last_block() {
        // 500 bytes → last block is 244 bytes; patch its final 16.
        let data = vec![7u8; 500];
        let mut reader = PatchReader::new(MockBlockReader::new(data, 256), 484, vec![9u8; 16]);
        let last = reader.read_block(1).unwrap();
        assert_eq!(last.len(), 244);
        assert!(last[..228].iter().all(|&b| b == 7));
        assert!(last[228..].iter().all(|&b| b == 9));
    }
}
