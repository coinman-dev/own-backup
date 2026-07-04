//! Sidecar manifest (`.obkm`) written next to each VHDX backup image.
//!
//! VHDX has no per-block checksums and no room for our metadata, so every
//! image gets a sidecar file carrying:
//! - the JSON manifest (source disk layout, job parameters, chain links) —
//!   reuses [`ImageManifest`] from the `.obk` era;
//! - xxHash64 checksums of every backed-up block, for `verify`.
//!
//! File layout (all integers little-endian):
//! ```text
//! magic "OBKMSC01" (8 bytes)
//! u32   JSON length
//! [..]  JSON: { "info": SidecarInfo, "manifest": ImageManifest }
//! u64   checksum entry count
//! [..]  entries: (u64 block_number, u64 xxhash64) × count
//! ```

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

use own_backup_common::image::ImageManifest;
use own_backup_common::{OwnBackupError, Result};

pub const SIDECAR_MAGIC: &[u8; 8] = b"OBKMSC01";
pub const SIDECAR_EXTENSION: &str = "obkm";

/// Role of an image inside a backup chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChainRole {
    /// Full backup — a self-contained dynamic VHDX.
    Full,
    /// Incremental — differencing VHDX whose parent is the previous version.
    Incremental,
    /// Differential — differencing VHDX whose parent is the full backup.
    Differential,
}

/// Container-level info that has no place in the (obk-era) [`ImageManifest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarInfo {
    /// Sidecar format version (bump on layout changes).
    pub format_version: u32,
    /// Container kind; currently always `"vhdx"`.
    pub container: String,
    /// File name (not path) of the image this sidecar describes.
    pub image_file_name: String,
    /// Role of the image in its chain.
    pub chain_role: ChainRole,
    /// File name of the parent image for incremental/differential images.
    pub parent_image_file_name: Option<String>,
    /// Byte offset inside the virtual disk where the backed-up volume data
    /// begins (the GPT partition start; 0 for whole-disk images).
    #[serde(default)]
    pub volume_data_offset: u64,
    /// Id of the GUI job that produced this image. Retention only ever touches
    /// sidecars carrying a matching job id; images without one (older builds,
    /// CLI runs) are never auto-deleted.
    #[serde(default)]
    pub job_id: Option<String>,
}

impl SidecarInfo {
    pub fn new_full(image_file_name: impl Into<String>, volume_data_offset: u64) -> Self {
        Self {
            format_version: 1,
            container: "vhdx".into(),
            image_file_name: image_file_name.into(),
            chain_role: ChainRole::Full,
            parent_image_file_name: None,
            volume_data_offset,
            job_id: None,
        }
    }

    pub fn new_child(
        image_file_name: impl Into<String>,
        role: ChainRole,
        parent: impl Into<String>,
        volume_data_offset: u64,
    ) -> Self {
        Self {
            format_version: 1,
            container: "vhdx".into(),
            image_file_name: image_file_name.into(),
            chain_role: role,
            parent_image_file_name: Some(parent.into()),
            volume_data_offset,
            job_id: None,
        }
    }
}

/// One block checksum entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockChecksum {
    pub block_number: u64,
    pub checksum: u64,
}

/// A parsed sidecar file.
#[derive(Debug)]
pub struct Sidecar {
    pub info: SidecarInfo,
    pub manifest: ImageManifest,
    pub checksums: Vec<BlockChecksum>,
}

#[derive(Serialize, Deserialize)]
struct JsonHeader {
    info: SidecarInfo,
    manifest: ImageManifest,
}

/// The conventional sidecar path for an image: `<image>.obkm`
/// (e.g. `job_full_x.vhdx` → `job_full_x.vhdx.obkm`).
pub fn sidecar_path_for(image_path: &Path) -> std::path::PathBuf {
    let mut name = image_path.as_os_str().to_os_string();
    name.push(".");
    name.push(SIDECAR_EXTENSION);
    std::path::PathBuf::from(name)
}

/// Write a sidecar to `path` (atomically via a `.tmp` rename).
pub fn write_sidecar(
    path: &Path,
    info: &SidecarInfo,
    manifest: &ImageManifest,
    checksums: &[BlockChecksum],
) -> Result<()> {
    let tmp_path = path.with_extension(format!("{SIDECAR_EXTENSION}.tmp"));
    {
        let file = File::create(&tmp_path)?;
        let mut w = BufWriter::new(file);
        write_sidecar_to(&mut w, info, manifest, checksums)?;
        w.flush()?;
    }
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Write a sidecar to any writer.
pub fn write_sidecar_to<W: Write>(
    w: &mut W,
    info: &SidecarInfo,
    manifest: &ImageManifest,
    checksums: &[BlockChecksum],
) -> Result<()> {
    let json = serde_json::to_vec(&JsonHeader {
        info: info.clone(),
        manifest: manifest.clone(),
    })?;
    if json.len() > u32::MAX as usize {
        return Err(OwnBackupError::ImageFormat("sidecar JSON too large".into()));
    }

    w.write_all(SIDECAR_MAGIC)?;
    w.write_all(&(json.len() as u32).to_le_bytes())?;
    w.write_all(&json)?;
    w.write_all(&(checksums.len() as u64).to_le_bytes())?;
    for entry in checksums {
        w.write_all(&entry.block_number.to_le_bytes())?;
        w.write_all(&entry.checksum.to_le_bytes())?;
    }
    Ok(())
}

/// Maximum accepted JSON header size (defensive cap, mirrors .obk reader).
const MAX_JSON_LEN: u32 = 100 * 1024 * 1024;

/// Read a sidecar file from `path`.
pub fn read_sidecar(path: &Path) -> Result<Sidecar> {
    let file = File::open(path)?;
    read_sidecar_from(&mut BufReader::new(file))
}

/// Read only the JSON header (info + manifest) of a sidecar, skipping the
/// checksum table. Cheap enough for scanning whole folders (retention, chain
/// discovery, image info).
pub fn read_sidecar_header(path: &Path) -> Result<(SidecarInfo, ImageManifest)> {
    let file = File::open(path)?;
    let mut r = BufReader::new(file);

    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    if &magic != SIDECAR_MAGIC {
        return Err(OwnBackupError::ImageFormat(
            "not a sidecar file (bad magic)".into(),
        ));
    }
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let json_len = u32::from_le_bytes(len_buf);
    if json_len == 0 || json_len > MAX_JSON_LEN {
        return Err(OwnBackupError::ImageFormat(format!(
            "sidecar JSON length {json_len} out of range"
        )));
    }
    let mut json = vec![0u8; json_len as usize];
    r.read_exact(&mut json)?;
    let header: JsonHeader = serde_json::from_slice(&json)?;
    Ok((header.info, header.manifest))
}

/// Read a sidecar from any reader.
pub fn read_sidecar_from<R: Read>(r: &mut R) -> Result<Sidecar> {
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    if &magic != SIDECAR_MAGIC {
        return Err(OwnBackupError::ImageFormat(
            "not a sidecar file (bad magic)".into(),
        ));
    }

    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let json_len = u32::from_le_bytes(len_buf);
    if json_len == 0 || json_len > MAX_JSON_LEN {
        return Err(OwnBackupError::ImageFormat(format!(
            "sidecar JSON length {json_len} out of range"
        )));
    }

    let mut json = vec![0u8; json_len as usize];
    r.read_exact(&mut json)?;
    let header: JsonHeader = serde_json::from_slice(&json)?;

    let mut count_buf = [0u8; 8];
    r.read_exact(&mut count_buf)?;
    let count = u64::from_le_bytes(count_buf);
    // Defensive cap: 512 GiB of 4 KiB blocks is 134M entries; anything beyond
    // a few hundred million entries means corruption.
    if count > 1u64 << 32 {
        return Err(OwnBackupError::ImageFormat(format!(
            "sidecar checksum count {count} out of range"
        )));
    }

    let mut checksums = Vec::with_capacity(count as usize);
    let mut entry = [0u8; 16];
    for _ in 0..count {
        r.read_exact(&mut entry)?;
        checksums.push(BlockChecksum {
            block_number: u64::from_le_bytes(entry[0..8].try_into().unwrap()),
            checksum: u64::from_le_bytes(entry[8..16].try_into().unwrap()),
        });
    }

    Ok(Sidecar {
        info: header.info,
        manifest: header.manifest,
        checksums,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use own_backup_common::disk::DiskInfo;
    use own_backup_common::types::*;
    use std::io::Cursor;
    use uuid::Uuid;

    fn make_manifest() -> ImageManifest {
        ImageManifest {
            backup_id: Uuid::nil(),
            created_at: "2026-07-01T00:00:00Z".into(),
            backup_type: BackupType::Full,
            parent_backup_id: None,
            source_os: "windows".into(),
            source_disk: DiskInfo {
                device_id: r"\\.\PhysicalDrive0".into(),
                model: "Test SSD".into(),
                serial: "S1".into(),
                size_bytes: 1024 * 1024,
                sector_size_logical: 512,
                sector_size_physical: 4096,
                media_type: "SSD".into(),
                partition_table: PartitionTable::Gpt,
                disk_guid: Uuid::nil().to_string(),
                partitions: vec![],
            },
            boot_mode: BootMode::Uefi,
            partition_table: PartitionTable::Gpt,
            compression_algo: CompressionAlgo::None,
            compression_level: 0,
            encryption_algo: None,
            encryption_salt: None,
            block_size: 65536,
            total_blocks: 16,
            used_blocks: 3,
            backup_mode: BackupMode::UsedSectors,
            checksum_algorithm: "xxhash64".into(),
            preflight_report: None,
            volume_encryption: None,
        }
    }

    #[test]
    fn sidecar_roundtrip() {
        let info = SidecarInfo::new_full("job_full.vhdx", 1024 * 1024);
        let manifest = make_manifest();
        let checksums = vec![
            BlockChecksum { block_number: 0, checksum: 0xDEAD },
            BlockChecksum { block_number: 5, checksum: 0xBEEF },
            BlockChecksum { block_number: 15, checksum: u64::MAX },
        ];

        let mut buf = Vec::new();
        write_sidecar_to(&mut buf, &info, &manifest, &checksums).unwrap();

        let sidecar = read_sidecar_from(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(sidecar.info.container, "vhdx");
        assert_eq!(sidecar.info.chain_role, ChainRole::Full);
        assert_eq!(sidecar.info.image_file_name, "job_full.vhdx");
        assert_eq!(sidecar.info.volume_data_offset, 1024 * 1024);
        assert_eq!(sidecar.manifest.block_size, 65536);
        assert_eq!(sidecar.checksums, checksums);
    }

    #[test]
    fn sidecar_roundtrip_incremental_via_file() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("job_inc_1.avhdx");
        let path = sidecar_path_for(&image);
        assert!(path.to_string_lossy().ends_with("job_inc_1.avhdx.obkm"));

        let info = SidecarInfo::new_child(
            "job_inc_1.avhdx",
            ChainRole::Incremental,
            "job_full.vhdx",
            1024 * 1024,
        );
        write_sidecar(&path, &info, &make_manifest(), &[]).unwrap();

        let sidecar = read_sidecar(&path).unwrap();
        assert_eq!(sidecar.info.chain_role, ChainRole::Incremental);
        assert_eq!(
            sidecar.info.parent_image_file_name.as_deref(),
            Some("job_full.vhdx")
        );
        assert!(sidecar.checksums.is_empty());
    }

    #[test]
    fn sidecar_rejects_bad_magic() {
        let mut buf = Vec::new();
        write_sidecar_to(&mut buf, &SidecarInfo::new_full("x.vhdx", 0), &make_manifest(), &[])
            .unwrap();
        buf[0] = b'X';
        assert!(read_sidecar_from(&mut Cursor::new(&buf)).is_err());
    }

    #[test]
    fn sidecar_rejects_truncated_checksums() {
        let mut buf = Vec::new();
        let checksums = vec![BlockChecksum { block_number: 1, checksum: 2 }];
        write_sidecar_to(&mut buf, &SidecarInfo::new_full("x.vhdx", 0), &make_manifest(), &checksums)
            .unwrap();
        buf.truncate(buf.len() - 4);
        assert!(read_sidecar_from(&mut Cursor::new(&buf)).is_err());
    }
}
