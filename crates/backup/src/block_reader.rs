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

/// Wraps a [`BlockReader`] and overlays many byte-level patches at fixed volume
/// offsets, replacing whatever the inner reader returned there.
///
/// Patches are the mechanism by which excluded files are truly *removed* from a
/// filesystem image (rather than left as zero-filled ghosts): the directory
/// entry is marked deleted and the allocation-table entries are freed — all in
/// the copied *metadata* — while the file's own data clusters are dropped from
/// the used map. The FS driver then sees no directory entry and free clusters:
/// the file is gone and its space is reclaimed, exactly like a real delete.
///
/// Patches MUST NOT overlap each other (each targets a distinct metadata byte
/// range). They are sorted by offset on construction so each `read_block` can
/// jump straight to the few patches touching that block instead of scanning all
/// of them — important because freeing a large file's chain can produce many
/// small FAT-entry patches.
pub struct MultiPatchReader<R: BlockReader> {
    inner: R,
    /// (offset, bytes), sorted by offset, non-overlapping.
    patches: Vec<(u64, Vec<u8>)>,
}

impl<R: BlockReader> MultiPatchReader<R> {
    pub fn new(inner: R, mut patches: Vec<(u64, Vec<u8>)>) -> Self {
        patches.sort_by_key(|(off, _)| *off);
        debug_assert!(
            patches
                .windows(2)
                .all(|w| w[0].0 + w[0].1.len() as u64 <= w[1].0),
            "MultiPatchReader patches must not overlap"
        );
        Self { inner, patches }
    }

    pub fn patch_count(&self) -> usize {
        self.patches.len()
    }
}

impl<R: BlockReader> BlockReader for MultiPatchReader<R> {
    fn read_block(&mut self, block_number: u64) -> Result<Vec<u8>> {
        let mut data = self.inner.read_block(block_number)?;
        let block_start = block_number * self.inner.block_size() as u64;
        let block_end = block_start + data.len() as u64;

        // Patches are sorted and non-overlapping, so each patch's END offset is
        // non-decreasing: binary-search for the first patch whose end reaches
        // into this block, then apply forward while the next patch still starts
        // before the block ends.
        let mut i = self
            .patches
            .partition_point(|(off, b)| off + b.len() as u64 <= block_start);
        while i < self.patches.len() && self.patches[i].0 < block_end {
            let (poff, pbytes) = &self.patches[i];
            let patch_end = poff + pbytes.len() as u64;
            let from = (*poff).max(block_start);
            let to = patch_end.min(block_end);
            if to > from {
                let dst = (from - block_start) as usize..(to - block_start) as usize;
                let src = (from - *poff) as usize..(to - *poff) as usize;
                data[dst].copy_from_slice(&pbytes[src]);
            }
            i += 1;
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

/// A [`BlockReader`] over any [`RawVolume`].
///
/// Used to read a decrypted (LUKS) volume: the source is a
/// [`crate::decrypt::DecryptingVolume`] presenting the plaintext data area from
/// offset 0, rather than a device path. Works over any `RawVolume`, so it is
/// testable on non-Windows hosts.
pub struct RawVolumeBlockReader {
    vol: Box<dyn crate::fat_map::RawVolume + Send>,
    block_size: u32,
    length: u64,
    total_blocks: u64,
}

impl RawVolumeBlockReader {
    /// Read `block_size`-byte blocks over `vol`; the last block may be shorter.
    pub fn new(vol: Box<dyn crate::fat_map::RawVolume + Send>, block_size: u32) -> Result<Self> {
        if block_size == 0 {
            return Err(own_backup_common::OwnBackupError::Device(
                "block size must be non-zero".into(),
            ));
        }
        let length = vol.len();
        Ok(Self {
            vol,
            block_size,
            length,
            total_blocks: length.div_ceil(block_size as u64),
        })
    }
}

impl BlockReader for RawVolumeBlockReader {
    fn read_block(&mut self, block_number: u64) -> Result<Vec<u8>> {
        if block_number >= self.total_blocks {
            return Err(own_backup_common::OwnBackupError::Device(format!(
                "block {block_number} is beyond volume end ({} blocks)",
                self.total_blocks
            )));
        }
        let offset = block_number * self.block_size as u64;
        let len = std::cmp::min(self.block_size as u64, self.length - offset) as usize;
        let mut buf = vec![0u8; len];
        self.vol.read_at(offset, &mut buf)?;
        Ok(buf)
    }

    fn block_size(&self) -> u32 {
        self.block_size
    }

    fn total_blocks(&self) -> u64 {
        self.total_blocks
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
            return Err(own_backup_common::OwnBackupError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("block {block_number} is beyond data length"),
            )));
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

    struct MemVolume(Vec<u8>);
    impl crate::fat_map::RawVolume for MemVolume {
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
            let (s, e) = (offset as usize, offset as usize + buf.len());
            buf.copy_from_slice(&self.0[s..e]);
            Ok(())
        }
        fn len(&self) -> u64 {
            self.0.len() as u64
        }
    }

    #[test]
    fn raw_volume_reader_blocks_and_partial_tail() {
        let data: Vec<u8> = (0..500u32).map(|i| i as u8).collect();
        let mut r = RawVolumeBlockReader::new(Box::new(MemVolume(data.clone())), 256).unwrap();
        assert_eq!(r.total_blocks(), 2);
        assert_eq!(r.read_block(0).unwrap(), &data[0..256]);
        let tail = r.read_block(1).unwrap();
        assert_eq!(tail.len(), 244);
        assert_eq!(tail, &data[256..500]);
        assert!(r.read_block(2).is_err());
    }

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
        let mut reader = PatchReader::new(
            MockBlockReader::new(data.clone(), 256),
            1024 - 64,
            sector0.clone(),
        );

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

    #[test]
    fn multi_patch_reader_applies_disjoint_patches() {
        // 4 blocks of 256. Patch a byte in block 0, a run spanning blocks 1→2,
        // and a byte in block 3. Order of construction is intentionally shuffled
        // to prove the reader sorts them.
        let data = vec![0u8; 1024];
        let patches = vec![
            (800u64, vec![0x33]),      // block 3
            (10u64, vec![0x11, 0x11]), // block 0
            (250u64, vec![0x22; 12]),  // spans block 0/1 boundary at 256
        ];
        let mut r = MultiPatchReader::new(MockBlockReader::new(data, 256), patches);
        assert_eq!(r.patch_count(), 3);

        let b0 = r.read_block(0).unwrap();
        assert_eq!(&b0[10..12], &[0x11, 0x11]);
        assert!(b0[250..256].iter().all(|&b| b == 0x22)); // head of the spanning patch
        assert!(b0[12..250].iter().all(|&b| b == 0));

        let b1 = r.read_block(1).unwrap();
        assert!(b1[..6].iter().all(|&b| b == 0x22)); // tail of the spanning patch
        assert!(b1[6..].iter().all(|&b| b == 0));

        let b2 = r.read_block(2).unwrap();
        assert!(b2.iter().all(|&b| b == 0));

        let b3 = r.read_block(3).unwrap();
        assert_eq!(b3[800 - 768], 0x33);
    }

    #[test]
    fn multi_patch_reader_no_patches_is_passthrough() {
        let data: Vec<u8> = (0..512u32).map(|i| i as u8).collect();
        let mut r = MultiPatchReader::new(MockBlockReader::new(data.clone(), 256), Vec::new());
        assert_eq!(r.read_block(0).unwrap(), &data[0..256]);
        assert_eq!(r.read_block(1).unwrap(), &data[256..512]);
    }

    #[test]
    fn multi_patch_reader_many_small_patches_land_correctly() {
        // Simulates freeing a fragmented FAT chain: many 4-byte zero patches
        // scattered across the block. Every one must land at its exact offset.
        let data = vec![0xFFu8; 4096];
        let offsets: Vec<u64> = (0..64).map(|k| k * 63).collect(); // 0,63,126,... all in-bounds
        let patches: Vec<(u64, Vec<u8>)> = offsets.iter().map(|&o| (o, vec![0u8; 4])).collect();
        let mut r = MultiPatchReader::new(MockBlockReader::new(data, 512), patches);
        for blk in 0..8 {
            let b = r.read_block(blk).unwrap();
            let base = blk * 512;
            for i in 0..512usize {
                let abs = base as usize + i;
                let patched = offsets
                    .iter()
                    .any(|&o| (o as usize..o as usize + 4).contains(&abs));
                assert_eq!(b[i], if patched { 0 } else { 0xFF }, "byte {abs}");
            }
        }
    }
}
