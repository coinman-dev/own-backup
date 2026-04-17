use std::io::{Seek, SeekFrom, Write};

use own_backup_common::error::Result;
use own_backup_common::image::{
    BlockIndexEntry, ImageFooter, ImageHeader, ImageManifest, FLAG_COMPRESSED, FLAG_ENCRYPTED,
    OBK_MAGIC, OBK_VERSION,
};

use crate::checksum::compute_block_checksum;
use crate::compression::compress_block;
use crate::encryption::{encrypt_block, EncryptionKey};

/// Writes a complete `.obk` image file.
///
/// Usage:
/// 1. Create with [`ImageWriter::new`] — writes a placeholder header.
/// 2. Call [`ImageWriter::write_block`] for each data block.
/// 3. Call [`ImageWriter::finish`] to write block index, manifest, footer,
///    and rewrite the header with actual offsets.
///
/// # Encryption
///
/// When `encryption_key` is `Some`, a fresh 32-byte salt is automatically
/// generated and stored in `manifest.encryption_salt`. The encryption key is
/// derived: `key = Argon2id(password, fresh_salt)`. Since the salt is unique per
/// backup, the resulting key is unique, and nonces can safely be pure block
/// counters (0, 1, 2, …) — no birthday-paradox risk.
pub struct ImageWriter<'k, W: Write + Seek> {
    writer: W,
    manifest: ImageManifest,
    encryption_key: Option<&'k EncryptionKey>,
    compression_level: i32,
    block_index: Vec<BlockIndexEntry>,
    current_offset: u64,
    flags: u32,
}

impl<'k, W: Write + Seek> ImageWriter<'k, W> {
    pub fn new(
        mut writer: W,
        mut manifest: ImageManifest,
        encryption_key: Option<&'k EncryptionKey>,
    ) -> Result<Self> {
        let mut flags: u32 = 0;

        if encryption_key.is_some() {
            flags |= FLAG_ENCRYPTED;
            // Runtime check (not just debug_assert): the caller must have:
            //   1. Generated a unique salt:  `let salt = generate_salt();`
            //   2. Derived the key from it:  `let key = derive_key(password, &salt);`
            //   3. Set salt in manifest:     `manifest.encryption_salt = Some(salt.to_vec());`
            // Without the salt, decryption would be impossible after a restart.
            if manifest.encryption_salt.is_none() {
                return Err(own_backup_common::error::OwnBackupError::Encryption(
                    "manifest.encryption_salt must be set before creating an encrypted ImageWriter. \
                     Call generate_salt(), derive_key(), then set manifest.encryption_salt.".into(),
                ));
            }
            // Sanity: salt should be at least 16 bytes
            if manifest.encryption_salt.as_ref().map_or(0, |s| s.len()) < 16 {
                return Err(own_backup_common::error::OwnBackupError::Encryption(
                    "manifest.encryption_salt is too short (minimum 16 bytes)".into(),
                ));
            }
        }

        if manifest.compression_algo != own_backup_common::types::CompressionAlgo::None {
            flags |= FLAG_COMPRESSED;
        }

        let header = ImageHeader::placeholder();
        writer.write_all(&header.to_bytes())?;
        let current_offset = ImageHeader::SIZE as u64;

        Ok(Self {
            compression_level: manifest.compression_level,
            writer,
            manifest,
            encryption_key,
            block_index: Vec::new(),
            current_offset,
            flags,
        })
    }

    /// Process and write one data block:
    /// checksum → compress → encrypt → write → record index entry.
    pub fn write_block(&mut self, block_number: u64, data: &[u8]) -> Result<()> {
        let original_size = data.len() as u32;
        let checksum = compute_block_checksum(data);

        let (processed, is_compressed) =
            if self.manifest.compression_algo != own_backup_common::types::CompressionAlgo::None {
                compress_block(data, self.compression_level)?
            } else {
                (data.to_vec(), false)
            };

        let final_data = if let Some(key) = self.encryption_key {
            encrypt_block(&processed, key, block_number)?
        } else {
            processed
        };

        let compressed_size = final_data.len() as u32;
        self.writer.write_all(&final_data)?;

        self.block_index.push(BlockIndexEntry {
            block_number,
            offset_in_file: self.current_offset,
            original_size,
            compressed_size,
            is_compressed,
            checksum,
        });

        self.current_offset += compressed_size as u64;
        Ok(())
    }

    /// Finish the image: write block index, manifest JSON, footer,
    /// then seek back to 0 and rewrite the header with actual offsets.
    pub fn finish(mut self) -> Result<()> {
        let block_index_offset = self.current_offset;
        for entry in &self.block_index {
            self.writer.write_all(&entry.to_bytes())?;
        }
        let block_index_size = (self.block_index.len() * BlockIndexEntry::BINARY_SIZE) as u64;

        let manifest_offset = block_index_offset + block_index_size;
        let manifest_json = serde_json::to_vec(&self.manifest)?;
        self.writer.write_all(&manifest_json)?;
        let manifest_size = manifest_json.len() as u64;

        let footer = ImageFooter {
            magic: *OBK_MAGIC,
            manifest_offset,
            block_index_offset,
            block_count: self.block_index.len() as u64,
        };
        self.writer.write_all(&footer.to_bytes())?;

        let header = ImageHeader {
            magic: *OBK_MAGIC,
            version: OBK_VERSION,
            flags: self.flags,
            manifest_offset,
            manifest_size,
            block_index_offset,
            block_index_size,
        };
        self.writer.seek(SeekFrom::Start(0))?;
        self.writer.write_all(&header.to_bytes())?;
        self.writer.flush()?;
        Ok(())
    }
}
