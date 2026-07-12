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

use own_backup_common::disk::DiskInfo;
use own_backup_common::image::ImageManifest;
use own_backup_common::{OwnBackupError, Result};

pub const SIDECAR_MAGIC: &[u8; 8] = b"OBKMSC01";
pub const SIDECAR_MAGIC_V2: &[u8; 8] = b"OBKMSC02";
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
    /// Full snapshot of the physical source disk this image came from — the
    /// whole disk with its partition list, captured from the live inventory at
    /// backup time. Present for BOTH per-volume images (the parent disk; the
    /// backed-up partition is identified by the job's sources) and whole-disk
    /// images. Lets a restore UI / Boot-CD render the archived disk and
    /// highlight the backed-up partitions WITHOUT any external job database
    /// (`jobs.json`). `None` for CLI runs / backups taken before snapshots.
    #[serde(default)]
    pub source_disk_layout: Option<DiskInfo>,
    /// File-type breakdown (bytes per Изображения/Видео/Аудио/Документы/Система/
    /// Другое) of what this image holds, from the backup's file walk. Embedded
    /// so the restore card's "по типам файлов" renders from the image alone
    /// (Boot-CD) without jobs.json. `None` when the walk didn't run.
    #[serde(default)]
    pub categories: Option<crate::exclusions::CategorySizes>,
    /// Count of files dropped by exclusion rules for this image, embedded so the
    /// restore card's "Исключено" line works from the image alone (no jobs.json).
    #[serde(default)]
    pub excluded_files: u64,
    /// Total bytes of the excluded files (see `excluded_files`).
    #[serde(default)]
    pub excluded_bytes: u64,
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
            source_disk_layout: None,
            categories: None,
            excluded_files: 0,
            excluded_bytes: 0,
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
            source_disk_layout: None,
            categories: None,
            excluded_files: 0,
            excluded_bytes: 0,
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
    /// Whole-disk restore manifest, present when this image is a single
    /// bootable disk image carrying several partitions (the embedded successor
    /// of the standalone `.obdm`). `None` for a plain per-volume image.
    pub disk_manifest: Option<own_backup_common::gpt::DiskBackupManifest>,
}

#[derive(Serialize, Deserialize)]
struct JsonHeader {
    info: SidecarInfo,
    manifest: ImageManifest,
    /// Embedded whole-disk manifest (see [`Sidecar::disk_manifest`]). Absent in
    /// per-volume images and every pre-2026-07-11 sidecar.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    disk_manifest: Option<own_backup_common::gpt::DiskBackupManifest>,
}

/// The conventional sidecar path for an image: `<image>.obkm`
/// (e.g. `job_full_x.vhdx` → `job_full_x.vhdx.obkm`).
pub fn sidecar_path_for(image_path: &Path) -> std::path::PathBuf {
    let mut name = image_path.as_os_str().to_os_string();
    name.push(".");
    name.push(SIDECAR_EXTENSION);
    std::path::PathBuf::from(name)
}

/// Calculate the size of the metadata partition based on the total number of blocks.
pub fn calculate_meta_partition_size(total_blocks: u64) -> u64 {
    let bytes = total_blocks * 16 + 65536;
    let mib = 1024 * 1024;
    bytes.div_ceil(mib) * mib
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

fn write_varint<W: Write>(w: &mut W, mut val: u64) -> std::io::Result<usize> {
    let mut bytes_written = 0;
    loop {
        let mut byte = (val & 0x7F) as u8;
        val >>= 7;
        if val != 0 {
            byte |= 0x80;
        }
        w.write_all(&[byte])?;
        bytes_written += 1;
        if val == 0 {
            break;
        }
    }
    Ok(bytes_written)
}

fn read_varint<R: Read>(r: &mut R) -> std::io::Result<u64> {
    let mut val = 0u64;
    let mut shift = 0;
    loop {
        let mut byte = [0u8; 1];
        r.read_exact(&mut byte)?;
        let b = byte[0];
        val |= ((b & 0x7F) as u64) << shift;
        if b & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift >= 64 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "varint overflow"));
        }
    }
    Ok(val)
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
        disk_manifest: None, // legacy v1 sidecar never carries a whole-disk manifest
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

/// Write a sidecar format v2 blob (with compression and integrity check) to any
/// writer. Per-volume images carry no whole-disk manifest.
pub fn write_sidecar_v2_to<W: Write>(
    w: &mut W,
    info: &SidecarInfo,
    manifest: &ImageManifest,
    checksums: &[BlockChecksum],
) -> Result<()> {
    write_sidecar_v2_full_to(w, info, manifest, checksums, None)
}

/// Like [`write_sidecar_v2_to`] but also embeds a whole-disk restore manifest —
/// used by a single bootable disk image so its `.obdm` lives inside the file.
pub fn write_sidecar_v2_full_to<W: Write>(
    w: &mut W,
    info: &SidecarInfo,
    manifest: &ImageManifest,
    checksums: &[BlockChecksum],
    disk_manifest: Option<&own_backup_common::gpt::DiskBackupManifest>,
) -> Result<()> {
    let mut info = info.clone();
    info.format_version = 2;

    let json = serde_json::to_vec(&JsonHeader {
        info,
        manifest: manifest.clone(),
        disk_manifest: disk_manifest.cloned(),
    })?;
    let json_zstd = zstd::bulk::compress(&json, 3)
        .map_err(|e| OwnBackupError::Compression(e.to_string()))?;
    if json_zstd.len() > u32::MAX as usize {
        return Err(OwnBackupError::ImageFormat("compressed sidecar JSON too large".into()));
    }

    let mut checksums = checksums.to_vec();
    checksums.sort_by_key(|c| c.block_number);

    let mut table_buf = Vec::new();
    let mut prev = 0u64;
    for entry in &checksums {
        let delta = entry.block_number - prev;
        write_varint(&mut table_buf, delta)?;
        table_buf.write_all(&entry.checksum.to_le_bytes())?;
        prev = entry.block_number;
    }

    let table_zstd = if !table_buf.is_empty() {
        zstd::bulk::compress(&table_buf, 3)
            .map_err(|e| OwnBackupError::Compression(e.to_string()))?
    } else {
        Vec::new()
    };
    if table_zstd.len() > u32::MAX as usize {
        return Err(OwnBackupError::ImageFormat("compressed sidecar checksum table too large".into()));
    }

    let mut blob = Vec::new();
    blob.write_all(SIDECAR_MAGIC_V2)?;
    blob.write_all(&(json_zstd.len() as u32).to_le_bytes())?;
    blob.write_all(&json_zstd)?;
    blob.write_all(&(checksums.len() as u64).to_le_bytes())?;
    blob.write_all(&(table_zstd.len() as u32).to_le_bytes())?;
    blob.write_all(&table_zstd)?;

    let hash = xxhash_rust::xxh64::xxh64(&blob, 0);
    blob.write_all(&hash.to_le_bytes())?;

    w.write_all(&blob)?;
    Ok(())
}

/// Maximum accepted JSON header size (defensive cap, mirrors .obk reader).
const MAX_JSON_LEN: u32 = 100 * 1024 * 1024;

/// Read a sidecar file from `path`.
pub fn read_sidecar(path: &Path) -> Result<Sidecar> {
    let file = File::open(path)?;
    read_sidecar_from(&mut BufReader::new(file))
}

/// Format-agnostic guest-byte reader for the embedded sidecar blob.
enum GuestBlob {
    Vhdx(std::path::PathBuf),
    Qcow2(Box<crate::qcow2::Qcow2RandomReader>),
}

impl GuestBlob {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        match self {
            GuestBlob::Vhdx(path) => crate::vhdx_bat::read_vhdx_guest_at(path, offset, buf),
            GuestBlob::Qcow2(reader) => reader.read_at(offset, buf),
        }
    }
}

/// Locate the OwnBackupMeta partition of an image: returns a guest reader plus
/// the partition's (offset, size) in guest bytes.
fn open_meta_partition(image_path: &Path, container_format: &str) -> Result<(GuestBlob, u64, u64)> {
    const SECTOR: usize = 512;
    let (mut guest, vsize) = match container_format {
        "vhdx" | "avhdx" => {
            let alloc = crate::vhdx_bat::allocated_payload_blocks(image_path)?;
            (GuestBlob::Vhdx(image_path.to_path_buf()), alloc.virtual_size)
        }
        "qcow2" => {
            let reader = crate::qcow2::Qcow2RandomReader::open(image_path)?;
            let vsize = reader.virtual_size();
            (GuestBlob::Qcow2(Box::new(reader)), vsize)
        }
        other => {
            return Err(OwnBackupError::ImageFormat(format!(
                "unsupported container format: {other}"
            )))
        }
    };
    let mut gpt_buf = vec![0u8; 34 * SECTOR];
    guest.read_at(0, &mut gpt_buf)?;
    let layout = crate::gpt_reader::parse_disk_layout(&gpt_buf, SECTOR as u32, vsize)?;
    let (meta_offset, meta_size) = crate::gpt_reader::find_meta_partition(&layout).ok_or_else(
        || OwnBackupError::ImageFormat("OwnBackupMeta partition not found in GPT".into()),
    )?;
    Ok((guest, meta_offset, meta_size))
}

/// Read the embedded sidecar blob (exact size, derived from its own length
/// fields — never a fixed cap: the checksum table of a large disk runs to tens
/// of MiB). `header_only` stops after the JSON header.
fn read_meta_blob(image_path: &Path, container_format: &str, header_only: bool) -> Result<Vec<u8>> {
    let (mut guest, meta_offset, meta_size) = open_meta_partition(image_path, container_format)?;

    // magic (8) + JSON length (4) — shared by v1 and v2 layouts.
    if meta_size < 12 {
        return Err(OwnBackupError::ImageFormat("meta partition too small for a sidecar".into()));
    }
    let mut head = [0u8; 12];
    guest.read_at(meta_offset, &mut head)?;
    let json_len = u32::from_le_bytes(head[8..12].try_into().unwrap()) as u64;
    if json_len == 0 || json_len > MAX_JSON_LEN as u64 || 12 + json_len > meta_size {
        return Err(OwnBackupError::ImageFormat(format!(
            "embedded sidecar JSON length {json_len} out of range"
        )));
    }

    let total = if header_only {
        12 + json_len
    } else if &head[..8] == SIDECAR_MAGIC_V2 {
        // ... + entry count (8) + compressed table length (4) + table + hash (8)
        let mut mid = [0u8; 12];
        guest.read_at(meta_offset + 12 + json_len, &mut mid)?;
        let table_len = u32::from_le_bytes(mid[8..12].try_into().unwrap()) as u64;
        12 + json_len + 8 + 4 + table_len + 8
    } else if &head[..8] == SIDECAR_MAGIC {
        // ... + entry count (8) + entries (16 each)
        let mut cnt = [0u8; 8];
        guest.read_at(meta_offset + 12 + json_len, &mut cnt)?;
        let count = u64::from_le_bytes(cnt);
        if count > 1u64 << 32 {
            return Err(OwnBackupError::ImageFormat(format!(
                "embedded sidecar checksum count {count} out of range"
            )));
        }
        12 + json_len + 8 + count * 16
    } else {
        return Err(OwnBackupError::ImageFormat("no sidecar magic in the meta partition".into()));
    };
    if total > meta_size {
        return Err(OwnBackupError::ImageFormat(format!(
            "embedded sidecar ({total} bytes) exceeds the meta partition ({meta_size} bytes)"
        )));
    }

    let mut blob = vec![0u8; total as usize];
    guest.read_at(meta_offset, &mut blob)?;
    Ok(blob)
}

/// Read the embedded sidecar v2 from the virtual disk image.
pub fn read_embedded_sidecar(image_path: &Path, container_format: &str) -> Result<Sidecar> {
    let blob = read_meta_blob(image_path, container_format, false)?;
    read_sidecar_from(&mut std::io::Cursor::new(blob))
}

/// Read only the SidecarInfo and ImageManifest from the embedded sidecar of the virtual disk image.
pub fn read_embedded_sidecar_header(
    image_path: &Path,
    container_format: &str,
) -> Result<(SidecarInfo, ImageManifest)> {
    let blob = read_meta_blob(image_path, container_format, true)?;
    read_sidecar_header_from(&mut std::io::Cursor::new(blob))
}

/// Read an image's full sidecar: embedded (inside the image) first, then the
/// adjacent `.obkm` file (backups from before the embedded-sidecar change).
pub fn read_image_sidecar(image_path: &Path) -> Result<Sidecar> {
    let ext = image_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase();
    match read_embedded_sidecar(image_path, &ext) {
        Ok(s) => Ok(s),
        Err(embedded_err) => {
            let sidecar_path = sidecar_path_for(image_path);
            if !sidecar_path.exists() {
                return Err(OwnBackupError::ImageFormat(format!(
                    "embedded sidecar not found ({embedded_err}) and adjacent sidecar not found: {}",
                    sidecar_path.display()
                )));
            }
            read_sidecar(&sidecar_path)
        }
    }
}

/// Header-only counterpart of [`read_image_sidecar`] (info + manifest, no
/// checksum table) — cheap enough for folder scans and dispatch decisions.
pub fn read_image_sidecar_header(image_path: &Path) -> Result<(SidecarInfo, ImageManifest)> {
    let ext = image_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase();
    match read_embedded_sidecar_header(image_path, &ext) {
        Ok(h) => Ok(h),
        Err(embedded_err) => {
            let sidecar_path = sidecar_path_for(image_path);
            if !sidecar_path.exists() {
                return Err(OwnBackupError::ImageFormat(format!(
                    "embedded sidecar not found ({embedded_err}) and adjacent sidecar not found: {}",
                    sidecar_path.display()
                )));
            }
            read_sidecar_header(&sidecar_path)
        }
    }
}

/// Can `image_path` serve as a chain parent / be restored? True when its
/// sidecar metadata is readable — embedded (inside the image) or the adjacent
/// `.obkm` file. Chain planners must accept either: images written since the
/// embedded-sidecar change carry no external file at all.
pub fn image_sidecar_readable(image_path: &Path) -> bool {
    let ext = image_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase();
    read_embedded_sidecar_header(image_path, &ext).is_ok()
        || sidecar_path_for(image_path).exists()
}

/// Read only the JSON header (info + manifest) of a sidecar, skipping the
/// checksum table. Cheap enough for scanning whole folders (retention, chain
/// discovery, image info).
pub fn read_sidecar_header(path: &Path) -> Result<(SidecarInfo, ImageManifest)> {
    let file = File::open(path)?;
    read_sidecar_header_from(&mut BufReader::new(file))
}

/// Read only the JSON header from any reader.
pub fn read_sidecar_header_from<R: Read>(r: &mut R) -> Result<(SidecarInfo, ImageManifest)> {
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    if &magic == SIDECAR_MAGIC {
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
    } else if &magic == SIDECAR_MAGIC_V2 {
        let mut len_buf = [0u8; 4];
        r.read_exact(&mut len_buf)?;
        let json_len = u32::from_le_bytes(len_buf);
        if json_len == 0 || json_len > MAX_JSON_LEN {
            return Err(OwnBackupError::ImageFormat(format!(
                "sidecar v2 JSON length {json_len} out of range"
            )));
        }
        let mut json_zstd = vec![0u8; json_len as usize];
        r.read_exact(&mut json_zstd)?;
        let json_decompressed = zstd::bulk::decompress(&json_zstd, MAX_JSON_LEN as usize)
            .map_err(|e| OwnBackupError::Compression(e.to_string()))?;
        let header: JsonHeader = serde_json::from_slice(&json_decompressed)?;
        Ok((header.info, header.manifest))
    } else {
        Err(OwnBackupError::ImageFormat(
            "not a sidecar file (bad magic)".into(),
        ))
    }
}

fn read_sidecar_v2_from_after_magic<R: Read>(r: &mut R) -> Result<Sidecar> {
    let mut header_bytes = Vec::new();
    header_bytes.write_all(SIDECAR_MAGIC_V2)?;

    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    header_bytes.write_all(&len_buf)?;
    let json_len = u32::from_le_bytes(len_buf);
    if json_len == 0 || json_len > MAX_JSON_LEN {
        return Err(OwnBackupError::ImageFormat(format!(
            "sidecar v2 JSON length {json_len} out of range"
        )));
    }

    let mut json_zstd = vec![0u8; json_len as usize];
    r.read_exact(&mut json_zstd)?;
    header_bytes.write_all(&json_zstd)?;

    let json_decompressed = zstd::bulk::decompress(&json_zstd, MAX_JSON_LEN as usize)
        .map_err(|e| OwnBackupError::Compression(e.to_string()))?;
    let header: JsonHeader = serde_json::from_slice(&json_decompressed)?;

    let mut count_buf = [0u8; 8];
    r.read_exact(&mut count_buf)?;
    header_bytes.write_all(&count_buf)?;
    let count = u64::from_le_bytes(count_buf);
    if count > 1u64 << 32 {
        return Err(OwnBackupError::ImageFormat(format!(
            "sidecar v2 checksum count {count} out of range"
        )));
    }

    let mut table_len_buf = [0u8; 4];
    r.read_exact(&mut table_len_buf)?;
    header_bytes.write_all(&table_len_buf)?;
    let table_zstd_len = u32::from_le_bytes(table_len_buf);
    if table_zstd_len > 500 * 1024 * 1024 {
        return Err(OwnBackupError::ImageFormat(format!(
            "sidecar v2 table compressed length {table_zstd_len} too large"
        )));
    }

    let mut table_zstd = vec![0u8; table_zstd_len as usize];
    r.read_exact(&mut table_zstd)?;
    header_bytes.write_all(&table_zstd)?;

    let mut expected_hash_buf = [0u8; 8];
    r.read_exact(&mut expected_hash_buf)?;
    let expected_hash = u64::from_le_bytes(expected_hash_buf);

    let actual_hash = xxhash_rust::xxh64::xxh64(&header_bytes, 0);
    if actual_hash != expected_hash {
        return Err(OwnBackupError::ImageFormat(format!(
            "sidecar v2 integrity check failed (expected hash {expected_hash:x}, got {actual_hash:x})"
        )));
    }

    let mut checksums = Vec::with_capacity(count as usize);
    if count > 0 {
        let max_decompressed_table = (count * 16 + 10 * 1024 * 1024) as usize;
        let table_decompressed = zstd::bulk::decompress(&table_zstd, max_decompressed_table)
            .map_err(|e| OwnBackupError::Compression(e.to_string()))?;
        
        let mut cursor = std::io::Cursor::new(table_decompressed);
        let mut prev = 0u64;
        for _ in 0..count {
            let delta = read_varint(&mut cursor)?;
            let block_number = prev + delta;
            let mut cksum_buf = [0u8; 8];
            cursor.read_exact(&mut cksum_buf)?;
            let checksum = u64::from_le_bytes(cksum_buf);
            checksums.push(BlockChecksum {
                block_number,
                checksum,
            });
            prev = block_number;
        }
    }

    Ok(Sidecar {
        info: header.info,
        manifest: header.manifest,
        checksums,
        disk_manifest: header.disk_manifest,
    })
}

/// Read a sidecar from any reader.
pub fn read_sidecar_from<R: Read>(r: &mut R) -> Result<Sidecar> {
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    if &magic == SIDECAR_MAGIC {
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
            disk_manifest: header.disk_manifest,
        })
    } else if &magic == SIDECAR_MAGIC_V2 {
        read_sidecar_v2_from_after_magic(r)
    } else {
        Err(OwnBackupError::ImageFormat(
            "not a sidecar file (bad magic)".into(),
        ))
    }
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

    #[test]
    fn sidecar_v2_roundtrip() {
        let info = SidecarInfo::new_full("job_full.vhdx", 1024 * 1024);
        let manifest = make_manifest();
        let checksums = vec![
            BlockChecksum { block_number: 0, checksum: 0xDEAD },
            BlockChecksum { block_number: 5, checksum: 0xBEEF },
            BlockChecksum { block_number: 15, checksum: u64::MAX },
        ];

        let mut buf = Vec::new();
        write_sidecar_v2_to(&mut buf, &info, &manifest, &checksums).unwrap();

        // 1. Read entire sidecar
        let sidecar = read_sidecar_from(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(sidecar.info.container, "vhdx");
        assert_eq!(sidecar.info.chain_role, ChainRole::Full);
        assert_eq!(sidecar.info.image_file_name, "job_full.vhdx");
        assert_eq!(sidecar.info.volume_data_offset, 1024 * 1024);
        assert_eq!(sidecar.info.format_version, 2);
        assert_eq!(sidecar.manifest.block_size, 65536);
        assert_eq!(sidecar.checksums, checksums);

        // 2. Read header only
        let (read_info, read_manifest) = read_sidecar_header_from(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(read_info.format_version, 2);
        assert_eq!(read_info.image_file_name, "job_full.vhdx");
        assert_eq!(read_manifest.block_size, 65536);

        // Plain per-volume sidecar carries no whole-disk manifest.
        assert!(sidecar.disk_manifest.is_none());
    }

    #[test]
    fn sidecar_v2_embeds_disk_manifest() {
        use own_backup_common::gpt::{
            DiskBackupManifest, DiskImageRef, DiskLayout, GptPartitionEntry,
        };
        let info = SidecarInfo::new_full("modulator_disk3.vhdx", 1024 * 1024);
        let manifest = make_manifest();
        let checksums = vec![BlockChecksum { block_number: 0, checksum: 7 }];
        let disk = DiskBackupManifest {
            format_version: DiskBackupManifest::FORMAT_VERSION,
            created_at: "2026-07-11T00:00:00Z".into(),
            job_id: Some("job-1".into()),
            disk_number: 3,
            disk_model: "Samsung SSD 970 PRO 512GB".into(),
            layout: DiskLayout {
                disk_guid: "11111111-2222-3333-4444-555555555555".into(),
                sector_size: 512,
                size_bytes: 512 * 1024 * 1024 * 1024,
                partitions: vec![GptPartitionEntry {
                    index: 1,
                    type_guid: own_backup_common::gpt::GPT_TYPE_EFI_SYSTEM.into(),
                    part_guid: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
                    first_lba: 2048,
                    last_lba: 4095,
                    attributes: 0,
                    name: "EFI".into(),
                }],
            },
            images: vec![DiskImageRef {
                part_index: 1,
                token: "VE7F57C39".into(),
                image_file_name: "modulator_disk3.vhdx".into(),
                filesystem: "fat32".into(),
            }],
            source_disk: None,
        };

        let mut buf = Vec::new();
        write_sidecar_v2_full_to(&mut buf, &info, &manifest, &checksums, Some(&disk)).unwrap();

        let sidecar = read_sidecar_from(&mut Cursor::new(&buf)).unwrap();
        let got = sidecar.disk_manifest.expect("embedded disk manifest");
        assert_eq!(got.disk_number, 3);
        assert_eq!(got.disk_model, "Samsung SSD 970 PRO 512GB");
        assert_eq!(got.layout.partitions.len(), 1);
        assert_eq!(got.layout.partitions[0].type_guid, own_backup_common::gpt::GPT_TYPE_EFI_SYSTEM);
        assert_eq!(got.images[0].token, "VE7F57C39");
        assert_eq!(sidecar.checksums, checksums);
    }

    #[test]
    fn sidecar_v2_roundtrip_empty_checksums() {
        let info = SidecarInfo::new_full("job_full.vhdx", 0);
        let manifest = make_manifest();

        let mut buf = Vec::new();
        write_sidecar_v2_to(&mut buf, &info, &manifest, &[]).unwrap();

        let sidecar = read_sidecar_from(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(sidecar.info.format_version, 2);
        assert!(sidecar.checksums.is_empty());
    }

    #[test]
    fn sidecar_v2_rejects_bad_magic() {
        let mut buf = Vec::new();
        write_sidecar_v2_to(&mut buf, &SidecarInfo::new_full("x.vhdx", 0), &make_manifest(), &[])
            .unwrap();
        buf[0] = b'X';
        assert!(read_sidecar_from(&mut Cursor::new(&buf)).is_err());
    }

    #[test]
    fn sidecar_v2_rejects_corrupted_hash() {
        let mut buf = Vec::new();
        write_sidecar_v2_to(&mut buf, &SidecarInfo::new_full("x.vhdx", 0), &make_manifest(), &[])
            .unwrap();
        let len = buf.len();
        // Corrupt the xxhash64 at the end of the blob
        buf[len - 1] ^= 0xFF;
        assert!(read_sidecar_from(&mut Cursor::new(&buf)).is_err());
    }

    #[test]
    fn test_read_embedded_sidecar_qcow2() {
        use crate::gpt_writer::{gpt_write_ops, GptLayout, DEFAULT_PARTITION_OFFSET};
        use crate::qcow2::{Qcow2Writer, Qcow2WriterOptions, Qcow2Compression};

        let dir = tempfile::tempdir().unwrap();
        let image_path = dir.path().join("test.qcow2");

        // 1. Setup layout and construct sidecar info
        let sector_size = 512;
        let volume_len = 8 * 1024 * 1024; // 8 MiB volume
        let block_size = 65536;
        let total_blocks = volume_len / block_size;
        let meta_size = calculate_meta_partition_size(total_blocks);
        let disk_size = DEFAULT_PARTITION_OFFSET + volume_len + meta_size + 1024 * 1024; // 1 MiB tail

        let meta_partition_guid = uuid::Uuid::new_v4();
        let layout = GptLayout {
            disk_size,
            sector_size,
            partition_offset: DEFAULT_PARTITION_OFFSET,
            partition_size: volume_len,
            disk_guid: uuid::Uuid::new_v4(),
            partition_guid: uuid::Uuid::new_v4(),
            partition_name: "OwnBackup".into(),
            meta_partition_size: Some(meta_size),
            meta_partition_guid: Some(meta_partition_guid),
        };

        // Initialize mock disk memory buffer
        let mut disk_buf = vec![0u8; disk_size as usize];

        // Write the GPT layout
        for op in gpt_write_ops(&layout).unwrap() {
            disk_buf[op.offset as usize..op.offset as usize + op.bytes.len()].copy_from_slice(&op.bytes);
        }

        // Setup the sidecar blob
        let mut info = SidecarInfo::new_full("test.qcow2", DEFAULT_PARTITION_OFFSET);
        info.container = "qcow2".into();
        info.job_id = Some("test_job".into());
        let manifest = make_manifest();
        let checksums = vec![BlockChecksum { block_number: 2, checksum: 12345 }];

        let mut sidecar_bytes = Vec::new();
        write_sidecar_v2_to(&mut sidecar_bytes, &info, &manifest, &checksums).unwrap();

        // Write the sidecar blob to the meta partition guest offset in our disk buffer
        let last_lba = (DEFAULT_PARTITION_OFFSET + volume_len) / sector_size as u64 - 1;
        let meta_offset = (last_lba + 1) * sector_size as u64;
        disk_buf[meta_offset as usize..meta_offset as usize + sidecar_bytes.len()].copy_from_slice(&sidecar_bytes);

        // 2. Build the qcow2 image file using Qcow2Writer
        let file = std::fs::File::create(&image_path).unwrap();
        let mut qcow_writer = Qcow2Writer::new(
            std::io::BufWriter::new(file),
            Qcow2WriterOptions {
                virtual_size: disk_size,
                compression: Qcow2Compression::None,
                ..Default::default()
            },
        ).unwrap();

        let cs = 64 * 1024;
        let mut cluster_index = 0;
        while cluster_index * cs < disk_size {
            let chunk_start = (cluster_index * cs) as usize;
            let chunk_end = std::cmp::min(disk_size as usize, chunk_start + cs as usize);
            let chunk = &disk_buf[chunk_start..chunk_end];
            if chunk.iter().any(|&b| b != 0) {
                let mut cluster_data = vec![0u8; cs as usize];
                cluster_data[..chunk.len()].copy_from_slice(chunk);
                qcow_writer.write_cluster(cluster_index, &cluster_data).unwrap();
            }
            cluster_index += 1;
        }
        qcow_writer.finish().unwrap();

        // 3. Read the embedded sidecar back and verify!
        let read_sidecar = read_embedded_sidecar(&image_path, "qcow2").unwrap();
        assert_eq!(read_sidecar.info.image_file_name, "test.qcow2");
        assert_eq!(read_sidecar.info.container, "qcow2");
        assert_eq!(read_sidecar.info.job_id.as_deref(), Some("test_job"));
        assert_eq!(read_sidecar.checksums, checksums);

        let (read_info, read_manifest) = read_embedded_sidecar_header(&image_path, "qcow2").unwrap();
        assert_eq!(read_info.image_file_name, "test.qcow2");
        assert_eq!(read_manifest.block_size, 65536);
    }
}
