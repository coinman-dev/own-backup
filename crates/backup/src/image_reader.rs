use std::io::{Read, Seek, SeekFrom};

use own_backup_common::error::{OwnBackupError, Result};
use own_backup_common::image::{
    BlockIndexEntry, ImageFooter, ImageHeader, ImageManifest, OBK_MAGIC,
};

use crate::checksum::compute_block_checksum;
use crate::compression::decompress_block;
use crate::encryption::{decrypt_block, EncryptionKey};

/// Reads and decodes `.obk` image files.
pub struct ImageReader<R: Read + Seek> {
    reader: R,
    header: ImageHeader,
    manifest: ImageManifest,
    block_index: Vec<BlockIndexEntry>,
}

impl<R: Read + Seek> ImageReader<R> {
    /// Open an `.obk` image, reading and validating the header, footer,
    /// manifest, and block index.
    pub fn open(mut reader: R) -> Result<Self> {
        // Read and validate header
        let mut header_buf = [0u8; ImageHeader::SIZE];
        reader.read_exact(&mut header_buf)?;
        let header = ImageHeader::from_bytes(&header_buf);
        header.validate()?;

        // Read footer (last bytes)
        reader.seek(SeekFrom::End(-(ImageFooter::SIZE as i64)))?;
        let mut footer_buf = [0u8; ImageFooter::SIZE];
        reader.read_exact(&mut footer_buf)?;
        let footer = ImageFooter::from_bytes(&footer_buf);

        if &footer.magic != OBK_MAGIC {
            return Err(OwnBackupError::ImageFormat("Invalid magic in footer".into()));
        }

        // --- Allocation safety checks ---
        // These prevent OOM attacks from malformed .obk files.

        // Check block_count against MAX
        if footer.block_count > ImageHeader::MAX_BLOCK_COUNT {
            return Err(OwnBackupError::ImageFormat(format!(
                "Block count {} exceeds maximum {}",
                footer.block_count,
                ImageHeader::MAX_BLOCK_COUNT
            )));
        }

        // Cross-check: block index region must be large enough to hold block_count entries.
        // This detects mismatch between block_count (in footer) and block_index_size (in header).
        let expected_index_bytes =
            footer.block_count.saturating_mul(BlockIndexEntry::BINARY_SIZE as u64);
        if expected_index_bytes > header.block_index_size {
            return Err(OwnBackupError::ImageFormat(format!(
                "Block count {} implies {} bytes for index, but header declares only {}",
                footer.block_count, expected_index_bytes, header.block_index_size
            )));
        }

        // Cap block index allocation: never allocate more than 256 MiB just for the index.
        // 256 MiB / 33 bytes per entry ≈ 8 million blocks — sufficient for ~8 PB at 1 MiB/block.
        const MAX_INDEX_ALLOC_BYTES: u64 = 256 * 1024 * 1024;
        if expected_index_bytes > MAX_INDEX_ALLOC_BYTES {
            return Err(OwnBackupError::ImageFormat(format!(
                "Block index would require {} bytes — exceeds allocation cap",
                expected_index_bytes
            )));
        }

        // Read manifest JSON (size already validated in header.validate())
        reader.seek(SeekFrom::Start(header.manifest_offset))?;
        let mut manifest_buf = vec![0u8; header.manifest_size as usize];
        reader.read_exact(&mut manifest_buf)?;
        let manifest: ImageManifest = serde_json::from_slice(&manifest_buf)?;

        // Read block index (binary records)
        reader.seek(SeekFrom::Start(header.block_index_offset))?;
        let block_count = footer.block_count as usize;
        let mut block_index = Vec::with_capacity(block_count);
        for _ in 0..block_count {
            let mut entry_buf = [0u8; BlockIndexEntry::BINARY_SIZE];
            reader.read_exact(&mut entry_buf)?;
            block_index.push(BlockIndexEntry::from_bytes(&entry_buf));
        }

        Ok(Self { reader, header, manifest, block_index })
    }

    pub fn manifest(&self) -> &ImageManifest {
        &self.manifest
    }

    pub fn block_index(&self) -> &[BlockIndexEntry] {
        &self.block_index
    }

    pub fn header(&self) -> &ImageHeader {
        &self.header
    }

    /// Read the raw (possibly compressed and/or encrypted) bytes for a block.
    pub fn read_raw_block(&mut self, entry: &BlockIndexEntry) -> Result<Vec<u8>> {
        // Cap allocation: compressed block can't be larger than decompressed (16 MiB)
        // plus AES-GCM tag (16 bytes). A malicious block index entry could set
        // compressed_size to u32::MAX (4 GiB) — reject before allocating.
        const MAX_RAW_BLOCK_SIZE: u32 = 16 * 1024 * 1024 + 256; // 16 MiB + overhead
        if entry.compressed_size > MAX_RAW_BLOCK_SIZE {
            return Err(OwnBackupError::ImageFormat(format!(
                "Block {} declares compressed_size {} > maximum {}",
                entry.block_number, entry.compressed_size, MAX_RAW_BLOCK_SIZE
            )));
        }
        self.reader.seek(SeekFrom::Start(entry.offset_in_file))?;
        let mut buf = vec![0u8; entry.compressed_size as usize];
        self.reader.read_exact(&mut buf)?;
        Ok(buf)
    }

    /// Read a block, decrypting and decompressing as needed, then verify its checksum.
    ///
    /// The `key` was derived from the password + `manifest.encryption_salt` (unique per file).
    /// Since the key is file-unique, `block_number` alone is a sufficient nonce.
    pub fn read_block(
        &mut self,
        entry: &BlockIndexEntry,
        key: Option<&EncryptionKey>,
    ) -> Result<Vec<u8>> {
        let raw = self.read_raw_block(entry)?;

        // Decrypt if key is provided
        let after_decrypt = if let Some(k) = key {
            decrypt_block(&raw, k, entry.block_number)?
        } else {
            raw
        };

        // Decompress if needed
        let original = if entry.is_compressed {
            decompress_block(&after_decrypt)?
        } else {
            after_decrypt
        };

        // Verify checksum
        let actual = compute_block_checksum(&original);
        if actual != entry.checksum {
            return Err(OwnBackupError::ChecksumMismatch {
                block_index: entry.block_number,
                expected: entry.checksum,
                actual,
            });
        }

        Ok(original)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encryption::derive_key;
    use crate::image_writer::ImageWriter;
    use own_backup_common::disk::DiskInfo;
    use own_backup_common::types::*;
    use std::io::Cursor;
    use uuid::Uuid;

    /// Create a test manifest. For encryption, caller sets salt themselves.
    fn make_manifest(compression: CompressionAlgo, salt: Option<Vec<u8>>) -> ImageManifest {
        ImageManifest {
            backup_id: Uuid::nil(),
            created_at: "2026-01-01T00:00:00Z".into(),
            backup_type: BackupType::Full,
            parent_backup_id: None,
            source_os: "test".into(),
            source_disk: DiskInfo {
                device_id: "test-disk".into(),
                model: "Model".into(),
                serial: "SN0001".into(),
                size_bytes: 1_048_576,
                sector_size_logical: 512,
                sector_size_physical: 4096,
                media_type: "SSD".into(),
                partition_table: PartitionTable::Gpt,
                disk_guid: "00000000-0000-0000-0000-000000000000".into(),
                partitions: vec![],
            },
            boot_mode: BootMode::Uefi,
            partition_table: PartitionTable::Gpt,
            compression_algo: compression,
            compression_level: 3,
            encryption_algo: salt.as_ref().map(|_| "AES-256-GCM".into()),
            encryption_salt: salt,
            block_size: 4096,
            total_blocks: 2,
            used_blocks: 2,
            backup_mode: BackupMode::SectorBySector,
            checksum_algorithm: "xxhash64".into(),
            preflight_report: None,
            volume_encryption: None,
        }
    }

    #[test]
    fn roundtrip_compressed() {
        let manifest = make_manifest(CompressionAlgo::Zstd, None);
        let block_a = vec![0xAAu8; 4096];
        let block_b = vec![0xBBu8; 4096];

        let mut buf = Cursor::new(Vec::new());
        let mut writer = ImageWriter::new(&mut buf, manifest, None).unwrap();
        writer.write_block(0, &block_a).unwrap();
        writer.write_block(1, &block_b).unwrap();
        writer.finish().unwrap();

        buf.set_position(0);
        let mut reader = ImageReader::open(&mut buf).unwrap();
        let idx = reader.block_index().to_vec();

        assert_eq!(reader.read_block(&idx[0], None).unwrap(), block_a);
        assert_eq!(reader.read_block(&idx[1], None).unwrap(), block_b);
    }

    #[test]
    fn roundtrip_with_encryption() {
        // Correct workflow: caller generates salt, derives key, stores salt in manifest.
        let salt = [0xCC; 32];
        let key = derive_key("test-password", &salt).unwrap();
        let manifest = make_manifest(CompressionAlgo::Zstd, Some(salt.to_vec()));
        let block = vec![0xDDu8; 4096];

        let mut buf = Cursor::new(Vec::new());
        let mut writer = ImageWriter::new(&mut buf, manifest, Some(&key)).unwrap();
        writer.write_block(0, &block).unwrap();
        writer.finish().unwrap();

        // Read back: derive same key from salt in manifest
        buf.set_position(0);
        let mut reader = ImageReader::open(&mut buf).unwrap();
        let stored_salt = reader.manifest().encryption_salt.clone().unwrap();
        let read_key = derive_key("test-password", &stored_salt).unwrap();

        let idx = reader.block_index().to_vec();
        assert_eq!(reader.read_block(&idx[0], Some(&read_key)).unwrap(), block);
    }

    #[test]
    fn wrong_key_fails() {
        let salt = [0xCC; 32];
        let key = derive_key("correct", &salt).unwrap();
        let wrong_key = derive_key("wrong", &salt).unwrap();
        let manifest = make_manifest(CompressionAlgo::Zstd, Some(salt.to_vec()));
        let block = vec![0xFFu8; 4096];

        let mut buf = Cursor::new(Vec::new());
        let mut writer = ImageWriter::new(&mut buf, manifest, Some(&key)).unwrap();
        writer.write_block(0, &block).unwrap();
        writer.finish().unwrap();

        buf.set_position(0);
        let mut reader = ImageReader::open(&mut buf).unwrap();
        let idx = reader.block_index().to_vec();
        assert!(reader.read_block(&idx[0], Some(&wrong_key)).is_err());
    }
}
