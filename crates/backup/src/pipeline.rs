use std::io::{Seek, Write};

use tracing::{debug, info};

use own_backup_common::error::Result;
use own_backup_common::image::ImageManifest;
use own_backup_common::progress::BackupProgress;
use own_backup_common::types::BackupStatus;

use crate::block_reader::BlockReader;
use crate::encryption::EncryptionKey;
use crate::image_writer::ImageWriter;
use crate::used_sectors::UsedSectorMap;

/// Orchestrates the full backup pipeline: read blocks -> skip unused ->
/// compress -> encrypt -> write to image.
///
/// The [`ImageWriter`] handles compression and encryption internally, so the
/// pipeline is responsible only for iterating blocks, consulting the
/// [`UsedSectorMap`], and driving progress reporting.
pub struct BackupPipeline<'k, W: Write + Seek> {
    block_reader: Box<dyn BlockReader>,
    used_sectors: Box<dyn UsedSectorMap>,
    image_writer: Option<ImageWriter<'k, W>>,
    backup_id: String,
}

impl<'k, W: Write + Seek> BackupPipeline<'k, W> {
    /// Create a new pipeline.
    ///
    /// - `block_reader` provides raw block data from the source device.
    /// - `used_sectors` determines which blocks to include.
    /// - `writer` is the destination (typically a `BufWriter<File>`).
    /// - `manifest` is the backup metadata.
    /// - `encryption_key` enables encryption when `Some`.
    pub fn new(
        block_reader: Box<dyn BlockReader>,
        used_sectors: Box<dyn UsedSectorMap>,
        writer: W,
        manifest: ImageManifest,
        encryption_key: Option<&'k EncryptionKey>,
    ) -> Result<Self> {
        let backup_id = manifest.backup_id.to_string();
        let image_writer = ImageWriter::new(writer, manifest, encryption_key)?;
        Ok(Self {
            block_reader,
            used_sectors,
            image_writer: Some(image_writer),
            backup_id,
        })
    }

    /// Run the backup pipeline to completion.
    ///
    /// `progress_callback` is invoked after each block with current progress.
    /// Return `Err(OwnBackupError::Cancelled)` from the callback to abort the
    /// backup early.
    pub fn run(
        &mut self,
        mut progress_callback: impl FnMut(&BackupProgress) -> Result<()>,
    ) -> Result<()> {
        let total_blocks = self.block_reader.total_blocks();
        let used_blocks = self.used_sectors.used_block_count();

        info!(
            backup_id = %self.backup_id,
            total_blocks,
            used_blocks,
            "Starting backup pipeline"
        );

        let mut blocks_done: u64 = 0;
        let mut bytes_written: u64 = 0;

        let writer = self
            .image_writer
            .as_mut()
            .expect("run() must only be called once");

        for block_number in 0..total_blocks {
            if !self.used_sectors.is_block_used(block_number) {
                debug!(block_number, "Skipping unused block");
                continue;
            }

            let data = self.block_reader.read_block(block_number)?;
            let data_len = data.len() as u64;

            writer.write_block(block_number, &data)?;

            blocks_done += 1;
            bytes_written += data_len;

            let progress = BackupProgress {
                backup_id: self.backup_id.clone(),
                status: BackupStatus::Running,
                blocks_done,
                blocks_total: used_blocks,
                bytes_written,
                speed_bps: 0,
            };

            progress_callback(&progress)?;
        }

        info!(
            backup_id = %self.backup_id,
            blocks_done,
            bytes_written,
            "All blocks written, finishing image"
        );

        // Take the writer out so we can call finish() which consumes it.
        let writer = self
            .image_writer
            .take()
            .expect("run() must only be called once");
        writer.finish()?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_reader::MockBlockReader;
    use crate::image_reader::ImageReader;
    use crate::used_sectors::AllUsedMap;
    use own_backup_common::disk::DiskInfo;
    use own_backup_common::types::*;
    use std::io::{Cursor, SeekFrom};
    use uuid::Uuid;

    fn make_manifest(total_blocks: u64, block_size: u32) -> ImageManifest {
        ImageManifest {
            backup_id: Uuid::nil(),
            created_at: "2026-01-01T00:00:00Z".into(),
            backup_type: BackupType::Full,
            parent_backup_id: None,
            source_os: "test".into(),
            source_disk: DiskInfo {
                device_id: "mock".into(),
                model: "Mock".into(),
                serial: "0000".into(),
                size_bytes: total_blocks * block_size as u64,
                sector_size_logical: 512,
                sector_size_physical: 4096,
                media_type: "Mock".into(),
                partition_table: PartitionTable::Gpt,
                disk_guid: Uuid::nil().to_string(),
                partitions: vec![],
            },
            boot_mode: BootMode::Uefi,
            partition_table: PartitionTable::Gpt,
            compression_algo: CompressionAlgo::Zstd,
            compression_level: 3,
            encryption_algo: None,
            encryption_salt: None,
            block_size,
            total_blocks,
            used_blocks: total_blocks,
            backup_mode: BackupMode::SectorBySector,
            checksum_algorithm: "xxhash64".into(),
            preflight_report: None,
            volume_encryption: None,
        }
    }

    #[test]
    fn pipeline_roundtrip() {
        let block_size = 512u32;
        let data = vec![0xABu8; 2048]; // 4 blocks
        let total_blocks = 4u64;

        let reader = MockBlockReader::new(data.clone(), block_size);
        let used_map = AllUsedMap::new(total_blocks);
        let manifest = make_manifest(total_blocks, block_size);

        let mut output = Cursor::new(Vec::new());
        let mut pipeline = BackupPipeline::new(
            Box::new(reader),
            Box::new(used_map),
            &mut output,
            manifest,
            None,
        )
        .unwrap();

        let mut progress_reports = Vec::new();
        pipeline
            .run(|p| {
                progress_reports.push(p.blocks_done);
                Ok(())
            })
            .unwrap();

        assert_eq!(progress_reports, vec![1, 2, 3, 4]);

        // Verify the image is readable.
        output.seek(SeekFrom::Start(0)).unwrap();
        let mut img_reader = ImageReader::open(&mut output).unwrap();
        assert_eq!(img_reader.block_index().len(), 4);

        let idx = img_reader.block_index().to_vec();
        for (i, entry) in idx.iter().enumerate() {
            let block = img_reader.read_block(entry, None).unwrap();
            let start = i * block_size as usize;
            let end = start + block_size as usize;
            assert_eq!(block, data[start..end]);
        }
    }
}
