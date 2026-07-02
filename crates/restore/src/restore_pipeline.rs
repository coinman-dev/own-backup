use std::io::{Read, Seek};

use own_backup_backup::encryption::EncryptionKey;
use own_backup_backup::ImageReader;
use own_backup_common::Result;
use tracing::info;

use crate::block_writer::BlockWriter;

pub struct RestorePipeline<'k, R: Read + Seek> {
    reader: ImageReader<R>,
    writer: Box<dyn BlockWriter>,
    encryption_key: Option<&'k EncryptionKey>,
}

impl<'k, R: Read + Seek> RestorePipeline<'k, R> {
    pub fn new(
        reader: ImageReader<R>,
        writer: Box<dyn BlockWriter>,
        encryption_key: Option<&'k EncryptionKey>,
    ) -> Self {
        Self {
            reader,
            writer,
            encryption_key,
        }
    }

    pub fn run<F>(&mut self, progress_callback: F) -> Result<()>
    where
        F: Fn(u64, u64),
    {
        let index = self.reader.block_index().to_vec();
        let total = index.len() as u64;

        info!("Restoring {} blocks", total);

        for (i, entry) in index.iter().enumerate() {
            let data = self
                .reader
                .read_block(entry, self.encryption_key)?;

            self.writer.write_block(entry.block_number, &data)?;

            progress_callback(i as u64 + 1, total);
        }

        self.writer.flush()?;
        info!("Restore complete: {} blocks written", total);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_writer::MockBlockWriter;
    use own_backup_backup::ImageWriter;
    use own_backup_common::disk::DiskInfo;
    use own_backup_common::image::*;
    use own_backup_common::types::*;
    use std::io::Cursor;

    fn make_test_manifest() -> ImageManifest {
        ImageManifest {
            backup_id: uuid::Uuid::new_v4(),
            created_at: chrono::Utc::now().to_rfc3339(),
            backup_type: BackupType::Full,
            parent_backup_id: None,
            source_os: "windows".into(),
            source_disk: DiskInfo {
                device_id: "test".into(),
                model: "test".into(),
                serial: "test".into(),
                size_bytes: 4096,
                sector_size_logical: 512,
                sector_size_physical: 4096,
                media_type: "SSD".into(),
                partition_table: PartitionTable::Gpt,
                disk_guid: "test".into(),
                partitions: vec![],
            },
            boot_mode: BootMode::Uefi,
            partition_table: PartitionTable::Gpt,
            compression_algo: CompressionAlgo::Zstd,
            compression_level: 3,
            encryption_algo: None,
            encryption_salt: None,
            block_size: 4096,
            total_blocks: 1,
            used_blocks: 1,
            backup_mode: BackupMode::SectorBySector,
            checksum_algorithm: "xxhash64".into(),
            preflight_report: None,
            volume_encryption: None,
        }
    }

    #[test]
    fn restore_roundtrip() {
        let block_data = vec![42u8; 4096];

        // Write a .obk file
        let mut buf = Cursor::new(Vec::new());
        let manifest = make_test_manifest();
        let mut writer = ImageWriter::new(&mut buf, manifest.clone(), None).unwrap();
        writer.write_block(0, &block_data).unwrap();
        writer.finish().unwrap();

        // Read it back and restore
        buf.set_position(0);
        let reader = ImageReader::open(buf).unwrap();
        let mock_writer = Box::new(MockBlockWriter::new(4096));

        let mut pipeline = RestorePipeline::new(reader, mock_writer, None);
        pipeline.run(|done, total| {
            assert!(done <= total);
        }).unwrap();
    }
}
