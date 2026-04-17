use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use own_backup_backup::encryption::EncryptionKey;
use own_backup_backup::ImageReader;
use own_backup_common::Result;
use tracing::info;

#[derive(Debug, Clone, serde::Serialize)]
pub struct ValidationResult {
    pub valid: bool,
    pub message: String,
    pub blocks_checked: u64,
    pub blocks_total: u64,
}

/// Quick validation: check header magic and version, read footer.
pub fn quick_validate(path: &Path) -> Result<ValidationResult> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);

    match ImageReader::open(reader) {
        Ok(r) => {
            let manifest = r.manifest();
            Ok(ValidationResult {
                valid: true,
                message: format!(
                    "Valid .obk file: {} backup, {} blocks, created {}",
                    manifest.backup_type, manifest.total_blocks, manifest.created_at
                ),
                blocks_checked: 0,
                blocks_total: manifest.total_blocks,
            })
        }
        Err(e) => Ok(ValidationResult {
            valid: false,
            message: format!("Invalid: {}", e),
            blocks_checked: 0,
            blocks_total: 0,
        }),
    }
}

/// Full validation: read and verify checksum of every block.
pub fn full_validate(
    path: &Path,
    encryption_key: Option<&EncryptionKey>,
) -> Result<ValidationResult> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);

    let mut image_reader = match ImageReader::open(reader) {
        Ok(r) => r,
        Err(e) => {
            return Ok(ValidationResult {
                valid: false,
                message: format!("Cannot open: {}", e),
                blocks_checked: 0,
                blocks_total: 0,
            });
        }
    };

    let index = image_reader.block_index().to_vec();
    let total = index.len() as u64;

    info!("Full validation: checking {} blocks", total);

    for (i, entry) in index.iter().enumerate() {
        match image_reader.read_block(entry, encryption_key) {
            Ok(_) => {}
            Err(e) => {
                return Ok(ValidationResult {
                    valid: false,
                    message: format!("Block {} failed: {}", entry.block_number, e),
                    blocks_checked: i as u64,
                    blocks_total: total,
                });
            }
        }
    }

    Ok(ValidationResult {
        valid: true,
        message: format!("All {} blocks verified successfully", total),
        blocks_checked: total,
        blocks_total: total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use own_backup_backup::ImageWriter;
    use own_backup_common::disk::DiskInfo;
    use own_backup_common::image::ImageManifest;
    use own_backup_common::types::*;
    use std::io::{Cursor, Write};
    use tempfile::TempDir;
    use uuid::Uuid;

    fn make_test_manifest() -> ImageManifest {
        ImageManifest {
            backup_id: Uuid::nil(),
            created_at: "2026-01-01T00:00:00Z".into(),
            backup_type: BackupType::Full,
            parent_backup_id: None,
            source_os: "test".into(),
            source_disk: DiskInfo {
                device_id: "test-disk".into(),
                model: "TestModel".into(),
                serial: "SN0001".into(),
                size_bytes: 8192,
                sector_size_logical: 512,
                sector_size_physical: 4096,
                media_type: "SSD".into(),
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
            block_size: 4096,
            total_blocks: 2,
            used_blocks: 2,
            backup_mode: BackupMode::SectorBySector,
            checksum_algorithm: "xxhash64".into(),
            preflight_report: None,
            volume_encryption: None,
        }
    }

    fn create_test_obk(dir: &std::path::Path) -> std::path::PathBuf {
        let obk_path = dir.join("test.obk");
        let file = std::fs::File::create(&obk_path).unwrap();
        let mut buf_writer = std::io::BufWriter::new(file);

        let manifest = make_test_manifest();
        let mut writer = ImageWriter::new(&mut buf_writer, manifest, None).unwrap();
        writer.write_block(0, &vec![0xAA; 4096]).unwrap();
        writer.write_block(1, &vec![0xBB; 4096]).unwrap();
        writer.finish().unwrap();
        buf_writer.flush().unwrap();

        obk_path
    }

    #[test]
    fn quick_validate_valid_file() {
        let dir = TempDir::new().unwrap();
        let obk_path = create_test_obk(dir.path());

        let result = quick_validate(&obk_path).unwrap();
        assert!(result.valid);
        assert_eq!(result.blocks_total, 2);
        assert!(result.message.contains("Valid"));
    }

    #[test]
    fn quick_validate_invalid_file() {
        let dir = TempDir::new().unwrap();
        let bad_path = dir.path().join("bad.obk");
        std::fs::write(&bad_path, b"this is not an obk file").unwrap();

        let result = quick_validate(&bad_path).unwrap();
        assert!(!result.valid);
        assert!(result.message.contains("Invalid"));
    }

    #[test]
    fn full_validate_valid_file() {
        let dir = TempDir::new().unwrap();
        let obk_path = create_test_obk(dir.path());

        let result = full_validate(&obk_path, None).unwrap();
        assert!(result.valid);
        assert_eq!(result.blocks_checked, 2);
        assert_eq!(result.blocks_total, 2);
    }

    #[test]
    fn quick_validate_nonexistent_file() {
        let result = quick_validate(std::path::Path::new("/tmp/nonexistent.obk"));
        assert!(result.is_err());
    }
}
