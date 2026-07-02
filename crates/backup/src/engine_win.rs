//! Windows volume backup/restore engine (plan v2, phase 1).
//!
//! Ties the building blocks together:
//! [`WinBlockReader`] → [`NtfsUsedSectorMap`] → [`copy_used_blocks`] →
//! attached VHDX ([`crate::vhdx`]) + GPT ([`crate::gpt_writer`]) + sidecar.
//!
//! v1 scope: **volume** backups (`C:`, `\\.\D:`, VSS snapshot paths). The
//! image is a GPT disk with a single Basic Data partition holding the raw
//! volume — directly mountable by Windows. Whole-disk backups and VSS
//! integration come in later phases.

#![cfg(target_os = "windows")]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tracing::{info, warn};
use uuid::Uuid;

use own_backup_common::disk::DiskInfo;
use own_backup_common::image::ImageManifest;
use own_backup_common::progress::{BackupProgress, RestoreProgress};
use own_backup_common::types::{
    BackupMode, BackupStatus, BackupType, BootMode, CompressionAlgo, PartitionTable,
};
use own_backup_common::{OwnBackupError, Result};
use own_backup_snapshot::VssSnapshotSession;

use crate::block_reader_win::WinBlockReader;
use crate::checksum::compute_block_checksum;
use crate::gpt_writer::{gpt_write_ops, GptLayout, DEFAULT_PARTITION_OFFSET};
use crate::raw_copy::copy_used_blocks;
use crate::sidecar::{
    read_sidecar, sidecar_path_for, write_sidecar, Sidecar, SidecarInfo,
};
use crate::used_sectors::{AllUsedMap, UsedSectorMap};
use crate::used_sectors_win::NtfsUsedSectorMap;
use crate::vhdx::{attach_vhdx, create_dynamic_vhdx, detach_vhdx, AttachMode, VhdxCreateOptions};
use crate::win_device::{normalize_device_path, WinRawDevice};

/// Default backup block size: 1 MiB (multiple of every real sector size).
pub const DEFAULT_BLOCK_SIZE: u32 = 1024 * 1024;

const MIB: u64 = 1024 * 1024;

/// Parameters for a volume backup.
pub struct VolumeBackupRequest {
    /// Source volume: `C:`, `\\.\C:` or a VSS snapshot device path.
    pub source: String,
    /// Output image path (`.vhdx`). Must not exist yet.
    pub image_path: PathBuf,
    /// Backup block size in bytes; use [`DEFAULT_BLOCK_SIZE`].
    pub block_size: u32,
    /// Copy every block even when an NTFS bitmap is available.
    pub all_sectors: bool,
    /// Take a VSS shadow copy of the volume and read from it (consistent
    /// backup of a live volume). Falls back to reading the live volume with a
    /// warning if VSS fails. Ignored for non-volume sources.
    pub use_vss: bool,
}

/// Outcome of a completed volume backup.
pub struct VolumeBackupSummary {
    pub image_path: PathBuf,
    pub sidecar_path: PathBuf,
    pub source_bytes: u64,
    pub total_blocks: u64,
    pub used_blocks: u64,
    pub blocks_copied: u64,
    pub bytes_copied: u64,
    pub duration: Duration,
    /// True when the NTFS used-sector bitmap was applied; false = every
    /// block was copied (explicit sector-by-sector or non-NTFS fallback).
    pub smart_mode: bool,
    /// True when the data was read from a VSS shadow copy.
    pub vss_used: bool,
}

/// Open a freshly-attached virtual disk for writing; the PhysicalDrive device
/// node can take a moment to appear after AttachVirtualDisk.
fn open_write_with_retry(path: &str) -> Result<WinRawDevice> {
    let mut last_err: Option<OwnBackupError> = None;
    for attempt in 0..20 {
        match WinRawDevice::open_write(path) {
            Ok(dev) => return Ok(dev),
            Err(e) => {
                last_err = Some(e);
                std::thread::sleep(Duration::from_millis(100 + 50 * attempt));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| OwnBackupError::Device(format!("cannot open {path}"))))
}

fn open_read_with_retry(path: &str) -> Result<WinRawDevice> {
    let mut last_err: Option<OwnBackupError> = None;
    for attempt in 0..20 {
        match WinRawDevice::open_read(path) {
            Ok(dev) => return Ok(dev),
            Err(e) => {
                last_err = Some(e);
                std::thread::sleep(Duration::from_millis(100 + 50 * attempt));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| OwnBackupError::Device(format!("cannot open {path}"))))
}

/// Back up a volume into a freshly created, directly mountable VHDX.
pub fn backup_volume(
    req: &VolumeBackupRequest,
    progress: impl FnMut(&BackupProgress) -> Result<()>,
) -> Result<VolumeBackupSummary> {
    if req.image_path.exists() {
        return Err(OwnBackupError::Config(format!(
            "output image already exists: {}",
            req.image_path.display()
        )));
    }
    if let Some(parent) = req.image_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let result = backup_volume_inner(req, progress);
    if result.is_err() {
        // Best-effort cleanup of a partial image.
        let _ = std::fs::remove_file(&req.image_path);
    }
    result
}

/// `\\.\X:` → `X:\` (the volume-root form VSS expects); None for anything
/// that is not a plain lettered volume (physical drives, GLOBALROOT paths).
fn volume_root_of(device_path: &str) -> Option<String> {
    let rest = device_path.strip_prefix(r"\\.\")?;
    let bytes = rest.as_bytes();
    if bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        Some(format!("{}\\", rest))
    } else {
        None
    }
}

fn backup_volume_inner(
    req: &VolumeBackupRequest,
    progress: impl FnMut(&BackupProgress) -> Result<()>,
) -> Result<VolumeBackupSummary> {
    let started = Instant::now();
    let source = normalize_device_path(&req.source);

    // --- VSS shadow copy (live lettered volumes only) ---
    let mut vss: Option<VssSnapshotSession> = None;
    let read_path = if req.use_vss {
        match volume_root_of(&source) {
            Some(root) => match VssSnapshotSession::create(&root) {
                Ok(session) => {
                    let path = session.device_path().to_string();
                    vss = Some(session);
                    path
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        "VSS snapshot failed — reading the live volume (crash-consistent copy)"
                    );
                    source.clone()
                }
            },
            None => {
                info!("Source is not a lettered volume; skipping VSS");
                source.clone()
            }
        }
    } else {
        source.clone()
    };
    let vss_used = vss.is_some();

    // --- Source geometry (read from the snapshot when we have one) ---
    let src_probe = WinRawDevice::open_read(&read_path)?;
    let volume_len = src_probe.length();
    let sector_size = src_probe.sector_size();
    drop(src_probe);

    if req.block_size == 0 || req.block_size % sector_size != 0 {
        return Err(OwnBackupError::Config(format!(
            "block size {} is not a multiple of the source sector size {sector_size}",
            req.block_size
        )));
    }
    let total_blocks = volume_len.div_ceil(req.block_size as u64);

    // --- Used-sector map (NTFS bitmap or full copy) ---
    let (used, smart_mode): (Box<dyn UsedSectorMap>, bool) = if req.all_sectors {
        info!("Sector-by-sector mode requested; copying all blocks");
        (Box::new(AllUsedMap::new(total_blocks)), false)
    } else {
        match NtfsUsedSectorMap::open(&read_path, req.block_size) {
            Ok(map) => (Box::new(map), true),
            Err(e) => {
                warn!(
                    error = %e,
                    "NTFS bitmap unavailable (non-NTFS volume?), falling back to sector-by-sector"
                );
                (Box::new(AllUsedMap::new(total_blocks)), false)
            }
        }
    };
    let used_blocks = used.used_block_count();

    // --- Create and attach the image ---
    // Disk = 1 MiB GPT head + volume + tail for the backup GPT, rounded to MiB.
    let disk_size = DEFAULT_PARTITION_OFFSET + volume_len.div_ceil(MIB) * MIB + MIB;
    create_dynamic_vhdx(
        &req.image_path,
        &VhdxCreateOptions {
            max_size: disk_size,
            logical_sector_size: sector_size,
            physical_sector_size: std::cmp::max(sector_size, 4096),
        },
    )?;

    let attached = attach_vhdx(&req.image_path, AttachMode::RawWrite)?;
    let mut writer = open_write_with_retry(attached.physical_path())?;

    // --- GPT with a single Basic Data partition wrapping the volume ---
    let layout = GptLayout {
        disk_size: writer.length(),
        sector_size,
        partition_offset: DEFAULT_PARTITION_OFFSET,
        partition_size: volume_len,
        disk_guid: Uuid::new_v4(),
        partition_guid: Uuid::new_v4(),
        partition_name: "OwnBackup".into(),
    };
    for op in gpt_write_ops(&layout)? {
        writer.write_at(op.offset, &op.bytes)?;
    }

    // --- Copy used blocks into the partition ---
    let backup_id = Uuid::new_v4();
    let data_offset = DEFAULT_PARTITION_OFFSET;
    let mut reader = WinBlockReader::open(&read_path, req.block_size)?;

    let outcome = copy_used_blocks(
        &backup_id.to_string(),
        &mut reader,
        used.as_ref(),
        |block_number, data| {
            writer.write_at(data_offset + block_number * req.block_size as u64, data)
        },
        progress,
    )?;

    writer.flush()?;
    drop(writer);
    drop(attached); // detaches the image

    // Notify VSS writers of success and release the shadow copy.
    if let Some(session) = vss.take() {
        if let Err(e) = session.complete() {
            warn!(error = %e, "VSS BackupComplete failed (backup data is already safe)");
        }
    }

    // --- Sidecar ---
    let image_file_name = req
        .image_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let manifest = ImageManifest {
        backup_id,
        created_at: chrono::Utc::now().to_rfc3339(),
        backup_type: BackupType::Full,
        parent_backup_id: None,
        source_os: "windows".into(),
        source_disk: DiskInfo {
            device_id: source.clone(),
            model: String::new(),
            serial: String::new(),
            size_bytes: volume_len,
            sector_size_logical: sector_size,
            sector_size_physical: std::cmp::max(sector_size, 4096),
            media_type: "Volume".into(),
            partition_table: PartitionTable::Gpt,
            disk_guid: layout.disk_guid.to_string(),
            partitions: vec![],
        },
        boot_mode: BootMode::Uefi, // populated properly with inventory in phase 4
        partition_table: PartitionTable::Gpt,
        compression_algo: CompressionAlgo::None,
        compression_level: 0,
        encryption_algo: None,
        encryption_salt: None,
        block_size: req.block_size,
        total_blocks,
        used_blocks,
        backup_mode: if smart_mode {
            BackupMode::UsedSectors
        } else {
            BackupMode::SectorBySector
        },
        checksum_algorithm: "xxhash64".into(),
        preflight_report: None,
        volume_encryption: None,
    };

    let sidecar_path = sidecar_path_for(&req.image_path);
    write_sidecar(
        &sidecar_path,
        &SidecarInfo::new_full(image_file_name, data_offset),
        &manifest,
        &outcome.checksums,
    )?;

    info!(
        image = %req.image_path.display(),
        blocks = outcome.blocks_copied,
        bytes = outcome.bytes_copied,
        secs = started.elapsed().as_secs(),
        "Volume backup complete"
    );

    Ok(VolumeBackupSummary {
        image_path: req.image_path.clone(),
        sidecar_path,
        source_bytes: volume_len,
        total_blocks,
        used_blocks,
        blocks_copied: outcome.blocks_copied,
        bytes_copied: outcome.bytes_copied,
        duration: started.elapsed(),
        smart_mode,
        vss_used,
    })
}

/// Parameters for a volume restore.
pub struct VolumeRestoreRequest {
    /// Backup image (`.vhdx`) with its `.obkm` sidecar next to it.
    pub image_path: PathBuf,
    /// Target volume/device to overwrite (`D:`, `\\.\D:`). DESTRUCTIVE.
    pub target: String,
    /// Verify each block's checksum while reading from the image.
    pub verify_checksums: bool,
}

/// Outcome of a completed restore.
pub struct VolumeRestoreSummary {
    pub blocks_restored: u64,
    pub bytes_restored: u64,
    pub duration: Duration,
}

/// Restore a volume image onto a target device. Overwrites the target!
pub fn restore_volume(
    req: &VolumeRestoreRequest,
    mut progress: impl FnMut(&RestoreProgress) -> Result<()>,
) -> Result<VolumeRestoreSummary> {
    let started = Instant::now();
    let sidecar = load_sidecar_of(&req.image_path)?;
    let block_size = sidecar.manifest.block_size as u64;
    let volume_len = sidecar.manifest.source_disk.size_bytes;
    let data_offset = sidecar.info.volume_data_offset;

    let target = normalize_device_path(&req.target);
    let mut target_dev = WinRawDevice::open_write(&target)?;
    if target_dev.length() < volume_len {
        return Err(OwnBackupError::RestoreIncompatible(format!(
            "target {} is {} bytes, smaller than the backed-up volume ({} bytes)",
            target,
            target_dev.length(),
            volume_len
        )));
    }
    if block_size % target_dev.sector_size() as u64 != 0 {
        return Err(OwnBackupError::RestoreIncompatible(format!(
            "image block size {block_size} does not match target sector size {}",
            target_dev.sector_size()
        )));
    }

    let attached = attach_vhdx(&req.image_path, AttachMode::RawRead)?;
    let mut image_dev = open_read_with_retry(attached.physical_path())?;

    let restore_id = Uuid::new_v4().to_string();
    let total = sidecar.checksums.len() as u64;
    let mut blocks_restored = 0u64;
    let mut bytes_restored = 0u64;

    for entry in &sidecar.checksums {
        let offset = entry.block_number * block_size;
        let len = std::cmp::min(block_size, volume_len - offset) as usize;

        let mut buf = vec![0u8; len];
        image_dev.read_at(data_offset + offset, &mut buf)?;

        if req.verify_checksums {
            let actual = compute_block_checksum(&buf);
            if actual != entry.checksum {
                return Err(OwnBackupError::ChecksumMismatch {
                    block_index: entry.block_number,
                    expected: entry.checksum,
                    actual,
                });
            }
        }

        target_dev.write_at(offset, &buf)?;
        blocks_restored += 1;
        bytes_restored += len as u64;

        let elapsed = started.elapsed().as_secs_f64();
        progress(&RestoreProgress {
            restore_id: restore_id.clone(),
            status: BackupStatus::Running,
            blocks_done: blocks_restored,
            blocks_total: total,
            bytes_written: bytes_restored,
            speed_bps: if elapsed > 0.0 {
                (bytes_restored as f64 / elapsed) as u64
            } else {
                0
            },
        })?;
    }

    target_dev.flush()?;
    drop(image_dev);
    drop(attached);

    info!(
        image = %req.image_path.display(),
        target = %target,
        blocks = blocks_restored,
        "Volume restore complete"
    );

    Ok(VolumeRestoreSummary {
        blocks_restored,
        bytes_restored,
        duration: started.elapsed(),
    })
}

/// Result of verifying an image against its sidecar checksums.
pub struct VerifySummary {
    pub blocks_checked: u64,
    pub mismatched_blocks: Vec<u64>,
    pub duration: Duration,
}

impl VerifySummary {
    pub fn valid(&self) -> bool {
        self.mismatched_blocks.is_empty()
    }
}

/// Verify every block of an image against the sidecar checksums.
pub fn verify_image(
    image_path: &Path,
    mut progress: impl FnMut(u64, u64) -> Result<()>,
) -> Result<VerifySummary> {
    let started = Instant::now();
    let sidecar = load_sidecar_of(image_path)?;
    let block_size = sidecar.manifest.block_size as u64;
    let volume_len = sidecar.manifest.source_disk.size_bytes;
    let data_offset = sidecar.info.volume_data_offset;

    let attached = attach_vhdx(image_path, AttachMode::RawRead)?;
    let mut image_dev = open_read_with_retry(attached.physical_path())?;

    let total = sidecar.checksums.len() as u64;
    let mut checked = 0u64;
    let mut mismatched = Vec::new();

    for entry in &sidecar.checksums {
        let offset = entry.block_number * block_size;
        let len = std::cmp::min(block_size, volume_len - offset) as usize;

        let mut buf = vec![0u8; len];
        image_dev.read_at(data_offset + offset, &mut buf)?;

        if compute_block_checksum(&buf) != entry.checksum {
            mismatched.push(entry.block_number);
        }
        checked += 1;
        progress(checked, total)?;
    }

    Ok(VerifySummary {
        blocks_checked: checked,
        mismatched_blocks: mismatched,
        duration: started.elapsed(),
    })
}

/// Mount an image read-only; volumes inside get drive letters from Windows.
/// The attachment survives process exit — use [`unmount_image`] to detach.
pub fn mount_image(image_path: &Path) -> Result<String> {
    let attached = attach_vhdx(image_path, AttachMode::ReadOnlyMounted)?;
    Ok(attached.physical_path().to_string())
}

/// Detach a previously mounted image.
pub fn unmount_image(image_path: &Path) -> Result<()> {
    detach_vhdx(image_path)
}

fn load_sidecar_of(image_path: &Path) -> Result<Sidecar> {
    let sidecar_path = sidecar_path_for(image_path);
    if !sidecar_path.exists() {
        return Err(OwnBackupError::ImageFormat(format!(
            "sidecar not found: {} (needed for restore/verify)",
            sidecar_path.display()
        )));
    }
    read_sidecar(&sidecar_path)
}
