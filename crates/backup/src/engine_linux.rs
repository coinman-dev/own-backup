//! Linux volume backup engine — the `/dev` counterpart of [`crate::engine_win`].
//!
//! Produces a **raw disk image** (a 1 MiB GPT head + the volume + a GPT tail)
//! plus the `.obkm` sidecar, writing only used blocks. The image is a plain
//! sparse file, mountable with `losetup -P`. It reuses every cross-platform
//! building block: the used-block maps (FAT/ext/btrfs/xfs), the LUKS decrypt
//! layer, the GPT writer, the block copier and the sidecar.
//!
//! v1 scope: full backups. No snapshot (reads the live device — crash
//! consistent) and no incremental chains yet. Used-block maps cover FAT/exFAT,
//! ext, btrfs, xfs and NTFS (via [`crate::ntfs_map`]'s `$Bitmap` reader);
//! anything else is copied whole. Output is a dynamic VHDX for Windows-side
//! mounting.

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tracing::{info, warn};
use uuid::Uuid;
use zeroize::Zeroizing;

use own_backup_common::disk::DiskInfo;
use own_backup_common::image::ImageManifest;
use own_backup_common::progress::{BackupProgress, RestoreProgress};
use own_backup_common::types::{
    BackupMode, BackupStatus, BackupType, BootMode, CompressionAlgo, PartitionTable,
};
use own_backup_common::{OwnBackupError, Result};

use crate::block_reader::{BlockReader, RawVolumeBlockReader};
use crate::checksum::compute_block_checksum;
use crate::decrypt::DecryptingVolume;
use crate::fat_map::RawVolume;
use crate::gpt_writer::{gpt_write_ops, GptLayout, DEFAULT_PARTITION_OFFSET};
use crate::linux_device::LinuxRawDevice;
use crate::luks::LuksParams;
use crate::raw_copy::copy_used_blocks;
use crate::sidecar::{read_sidecar, sidecar_path_for, write_sidecar, SidecarInfo};
use crate::used_sectors::{AllUsedMap, UsedSectorMap};

/// Default backup block size: 1 MiB.
pub const DEFAULT_BLOCK_SIZE: u32 = 1024 * 1024;
const MIB: u64 = 1024 * 1024;
/// VHDX payload block size (allocation granularity of the dynamic image).
const VHDX_BLOCK_SIZE: u64 = 2 * MIB;

/// Parameters for a Linux volume backup.
pub struct LinuxBackupRequest {
    /// Source device (`/dev/sdb2`, a loop device, or an image file).
    pub source: String,
    /// Output image path (`.img`). Must not exist yet.
    pub image_path: PathBuf,
    /// Backup block size in bytes; use [`DEFAULT_BLOCK_SIZE`].
    pub block_size: u32,
    /// Copy every block even when a used-block map is available.
    pub all_sectors: bool,
    /// Passphrase for an encrypted (LUKS) source — its decrypted data is backed
    /// up (used blocks only). Ignored for non-encrypted sources.
    pub passphrase: Option<Zeroizing<String>>,
    /// GUI job id, stamped into the sidecar.
    pub job_id: Option<String>,
}

/// Outcome of a completed Linux backup.
pub struct LinuxBackupSummary {
    pub image_path: PathBuf,
    pub sidecar_path: PathBuf,
    pub source_bytes: u64,
    pub total_blocks: u64,
    pub used_blocks: u64,
    pub blocks_copied: u64,
    pub bytes_copied: u64,
    pub duration: Duration,
    /// True when a used-block map was applied (false = whole-device copy).
    pub smart_mode: bool,
    /// True when the source was decrypted (LUKS) before backup.
    pub encrypted: bool,
}

/// Back up a volume into a raw disk image + sidecar.
pub fn backup_volume(
    req: &LinuxBackupRequest,
    progress: impl FnMut(&BackupProgress) -> Result<()>,
) -> Result<LinuxBackupSummary> {
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
    let result = backup_inner(req, progress);
    if result.is_err() {
        let _ = std::fs::remove_file(&req.image_path);
    }
    result
}

fn backup_inner(
    req: &LinuxBackupRequest,
    mut progress: impl FnMut(&BackupProgress) -> Result<()>,
) -> Result<LinuxBackupSummary> {
    let started = Instant::now();
    let read_path = req.source.clone();

    // --- Optional LUKS unlock (back up the decrypted data area) ---
    let mut probe = LinuxRawDevice::open_read(&read_path)?;
    let luks: Option<LuksParams> = match &req.passphrase {
        Some(pass) => {
            let mut header = vec![0u8; 4096];
            probe.read_at(0, &mut header)?;
            if crate::luks::is_luks(&header) {
                let params = crate::luks::open_luks(&mut probe, pass.as_bytes())?;
                info!(kind = ?params.kind, "LUKS source unlocked; backing up decrypted data");
                Some(params)
            } else {
                None
            }
        }
        None => None,
    };
    let device_len = probe.length();
    let sector_size = probe.sector_size();
    drop(probe);

    let volume_len = match &luks {
        Some(p) => device_len.saturating_sub(p.data_offset),
        None => device_len,
    };
    if req.block_size == 0 || req.block_size % sector_size != 0 {
        return Err(OwnBackupError::Config(format!(
            "block size {} is not a multiple of the source sector size {sector_size}",
            req.block_size
        )));
    }
    let total_blocks = volume_len.div_ceil(req.block_size as u64);

    // A fresh read source (device, or a decrypting view over it).
    let open_source = |luks: &Option<LuksParams>| -> Result<Box<dyn RawVolume + Send>> {
        let dev = LinuxRawDevice::open_read(&read_path)?;
        Ok(match luks {
            Some(p) => Box::new(DecryptingVolume::new(Box::new(dev), p.clone())),
            None => Box::new(dev),
        })
    };

    // --- Used-block map: FAT/exFAT → ext → btrfs → xfs (NTFS copies whole) ---
    let (used, smart_mode): (Box<dyn UsedSectorMap>, bool) = if req.all_sectors {
        info!("Sector-by-sector mode requested; copying all blocks");
        (Box::new(AllUsedMap::new(total_blocks)), false)
    } else {
        let mut src = open_source(&luks)?;
        let map: Option<Box<dyn UsedSectorMap>> =
            if let Ok(m) = crate::fat_map::build_fat_used_map(src.as_mut(), req.block_size) {
                info!(fs = m.filesystem, "Using FAT used-cluster map");
                Some(Box::new(m))
            } else if let Ok(m) = crate::ext_map::build_ext_used_map(src.as_mut(), req.block_size) {
                info!(fs = "ext4", "Using ext used-block map");
                Some(Box::new(m))
            } else if let Ok(m) = crate::btrfs_map::build_btrfs_used_map(src.as_mut(), req.block_size)
            {
                info!(fs = "btrfs", "Using btrfs used-block map");
                Some(Box::new(m))
            } else if let Ok(m) = crate::xfs_map::build_xfs_used_map(src.as_mut(), req.block_size) {
                info!(fs = "xfs", "Using xfs used-block map");
                Some(Box::new(m))
            } else if let Ok(m) = crate::ntfs_map::build_ntfs_used_map(src.as_mut(), req.block_size) {
                info!(fs = "ntfs", "Using NTFS used-cluster map");
                Some(Box::new(m))
            } else {
                warn!("no used-block map available (unknown filesystem) — copying every block");
                None
            };
        match map {
            Some(m) => (m, true),
            None => (Box::new(AllUsedMap::new(total_blocks)), false),
        }
    };
    let used_blocks = used.used_block_count();

    // --- Create the dynamic VHDX: 1 MiB GPT head + volume + 1 MiB tail ---
    let disk_size = DEFAULT_PARTITION_OFFSET + volume_len.div_ceil(MIB) * MIB + MIB;
    let mut writer = crate::vhdx_format::VhdxWriter::create(
        &req.image_path,
        disk_size,
        VHDX_BLOCK_SIZE,
        sector_size,
    )?;
    let data_offset = DEFAULT_PARTITION_OFFSET;

    // --- GPT with a single Basic Data partition wrapping the volume ---
    let disk_guid = Uuid::new_v4();
    let layout = GptLayout {
        disk_size,
        sector_size,
        partition_offset: DEFAULT_PARTITION_OFFSET,
        partition_size: volume_len,
        disk_guid,
        partition_guid: Uuid::new_v4(),
        partition_name: "OwnBackup".into(),
    };
    for op in gpt_write_ops(&layout)? {
        writer.write_at(op.offset, &op.bytes)?;
    }

    // --- Copy used blocks into the partition ---
    let backup_id = Uuid::new_v4();
    let mut reader: Box<dyn BlockReader> =
        Box::new(RawVolumeBlockReader::new(open_source(&luks)?, req.block_size)?);
    let outcome = copy_used_blocks(
        &backup_id.to_string(),
        reader.as_mut(),
        used.as_ref(),
        None,
        |block_number, data| writer.write_at(data_offset + block_number * req.block_size as u64, data),
        &mut progress,
    )?;
    writer.finish()?;

    // --- Sidecar + manifest ---
    let manifest = ImageManifest {
        backup_id,
        created_at: chrono::Utc::now().to_rfc3339(),
        backup_type: BackupType::Full,
        parent_backup_id: None,
        source_os: "linux".into(),
        source_disk: DiskInfo {
            device_id: req.source.clone(),
            model: String::new(),
            serial: String::new(),
            size_bytes: volume_len,
            sector_size_logical: sector_size,
            sector_size_physical: std::cmp::max(sector_size, 4096),
            media_type: "Volume".into(),
            partition_table: PartitionTable::Gpt,
            disk_guid: disk_guid.to_string(),
            partitions: vec![],
        },
        boot_mode: BootMode::Uefi,
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
        volume_encryption: luks.as_ref().map(|p| {
            use own_backup_common::image::{VolumeCaptureMode, VolumeEncryptionState};
            VolumeEncryptionState {
                kind: p.kind.clone(),
                capture_mode: VolumeCaptureMode::Plaintext,
                luks_header_backup_b64: None,
                note: Some("Decrypted at backup; re-enable encryption after restore".into()),
            }
        }),
    };

    let image_file_name = req
        .image_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut sidecar_info = SidecarInfo::new_full(image_file_name, data_offset);
    sidecar_info.job_id = req.job_id.clone();
    let sidecar_path = sidecar_path_for(&req.image_path);
    write_sidecar(&sidecar_path, &sidecar_info, &manifest, &outcome.checksums)?;

    info!(
        image = %req.image_path.display(),
        blocks = outcome.blocks_copied,
        bytes = outcome.bytes_copied,
        secs = started.elapsed().as_secs(),
        "Linux volume backup complete"
    );

    Ok(LinuxBackupSummary {
        image_path: req.image_path.clone(),
        sidecar_path,
        source_bytes: volume_len,
        total_blocks,
        used_blocks,
        blocks_copied: outcome.blocks_copied,
        bytes_copied: outcome.bytes_copied,
        duration: started.elapsed(),
        smart_mode,
        encrypted: luks.is_some(),
    })
}

/// Parameters for a Linux volume restore. DESTRUCTIVE to the target.
pub struct LinuxRestoreRequest {
    /// Raw image (`.img`) with its `.obkm` sidecar next to it.
    pub image_path: PathBuf,
    /// Target device/file to overwrite (`/dev/sdb2`, a loop device, a file).
    pub target: String,
    /// Verify each block's checksum while restoring.
    pub verify_checksums: bool,
}

/// Outcome of a completed restore.
pub struct LinuxRestoreSummary {
    pub blocks_restored: u64,
    pub bytes_restored: u64,
    pub duration: Duration,
}

/// Restore a raw image's recorded (used) blocks onto a target. The volume is
/// written from target offset 0 (the image stored it at the 1 MiB GPT head).
/// Only used blocks are written, so the target should be blank or the same
/// volume — free regions keep their prior content, which the filesystem ignores.
pub fn restore_volume(
    req: &LinuxRestoreRequest,
    mut progress: impl FnMut(&RestoreProgress) -> Result<()>,
) -> Result<LinuxRestoreSummary> {
    let started = Instant::now();
    let sidecar_path = sidecar_path_for(&req.image_path);
    if !sidecar_path.exists() {
        return Err(OwnBackupError::ImageFormat(format!(
            "sidecar not found: {}",
            sidecar_path.display()
        )));
    }
    let sidecar = read_sidecar(&sidecar_path)?;
    let block_size = sidecar.manifest.block_size as u64;
    let volume_len = sidecar.manifest.source_disk.size_bytes;
    let data_offset = sidecar.info.volume_data_offset;

    let image = crate::vhdx_format::VhdxReader::open(&req.image_path)?;
    let mut target = LinuxRawDevice::open_write(&req.target)?;
    if target.length() < volume_len {
        return Err(OwnBackupError::RestoreIncompatible(format!(
            "target {} is {} bytes, smaller than the backed-up volume ({volume_len} bytes)",
            req.target,
            target.length()
        )));
    }

    let total = sidecar.checksums.len() as u64;
    let mut blocks_restored = 0u64;
    let mut bytes_restored = 0u64;
    for entry in &sidecar.checksums {
        let offset = entry.block_number * block_size;
        let len = std::cmp::min(block_size, volume_len - offset) as usize;
        let mut buf = vec![0u8; len];
        image.read_at(data_offset + offset, &mut buf)?;
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
        target.write_at(offset, &buf)?;
        blocks_restored += 1;
        bytes_restored += len as u64;

        let elapsed = started.elapsed().as_secs_f64();
        progress(&RestoreProgress {
            restore_id: String::new(),
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
    target.flush()?;

    info!(
        image = %req.image_path.display(),
        target = %req.target,
        blocks = blocks_restored,
        "Linux volume restore complete"
    );
    Ok(LinuxRestoreSummary {
        blocks_restored,
        bytes_restored,
        duration: started.elapsed(),
    })
}

/// Result of verifying an image against its sidecar checksums.
pub struct VerifySummary {
    pub blocks_checked: u64,
    pub mismatched_blocks: Vec<u64>,
}

impl VerifySummary {
    pub fn valid(&self) -> bool {
        self.mismatched_blocks.is_empty()
    }
}

/// Verify every recorded block of a raw image against its sidecar checksums.
pub fn verify_image(
    image_path: &Path,
    mut progress: impl FnMut(u64, u64) -> Result<()>,
) -> Result<VerifySummary> {
    let sidecar_path = sidecar_path_for(image_path);
    if !sidecar_path.exists() {
        return Err(OwnBackupError::ImageFormat(format!(
            "sidecar not found: {}",
            sidecar_path.display()
        )));
    }
    let sidecar = read_sidecar(&sidecar_path)?;
    let block_size = sidecar.manifest.block_size as u64;
    let volume_len = sidecar.manifest.source_disk.size_bytes;
    let data_offset = sidecar.info.volume_data_offset;

    let dev = crate::vhdx_format::VhdxReader::open(image_path)?;
    let total = sidecar.checksums.len() as u64;
    let mut checked = 0u64;
    let mut mismatched = Vec::new();

    for entry in &sidecar.checksums {
        let offset = entry.block_number * block_size;
        let len = std::cmp::min(block_size, volume_len - offset) as usize;
        let mut buf = vec![0u8; len];
        dev.read_at(data_offset + offset, &mut buf)?;
        if compute_block_checksum(&buf) != entry.checksum {
            mismatched.push(entry.block_number);
        }
        checked += 1;
        progress(checked, total)?;
    }

    Ok(VerifySummary { blocks_checked: checked, mismatched_blocks: mismatched })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_progress(_p: &BackupProgress) -> Result<()> {
        Ok(())
    }

    /// End-to-end on a real ext4 image: back up used blocks into a raw image and
    /// verify. `cargo test -p own-backup-backup -- --ignored` (needs mkfs.ext4).
    #[test]
    #[ignore]
    fn backup_and_verify_real_ext4() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.ext4");
        std::fs::File::create(&src).unwrap().set_len(64 * 1024 * 1024).unwrap();
        assert!(std::process::Command::new("mkfs.ext4")
            .args(["-q", "-F", src.to_str().unwrap()])
            .status()
            .unwrap()
            .success());

        let img = dir.path().join("out.img");
        let req = LinuxBackupRequest {
            source: src.to_str().unwrap().to_string(),
            image_path: img.clone(),
            block_size: DEFAULT_BLOCK_SIZE,
            all_sectors: false,
            passphrase: None,
            job_id: None,
        };
        let sum = backup_volume(&req, no_progress).unwrap();
        assert!(sum.smart_mode);
        assert!(sum.used_blocks > 0 && sum.used_blocks < sum.total_blocks);

        let v = verify_image(&img, |_, _| Ok(())).unwrap();
        assert!(v.valid(), "verify mismatches: {:?}", v.mismatched_blocks);

        // The image's partition (at the 1 MiB GPT head) holds a recognisable
        // ext4 — proof the superblock/metadata were copied to the right place.
        let dev = crate::vhdx_format::VhdxReader::open(&img).unwrap();
        let mut head = vec![0u8; own_backup_inventory::fs_probe::PROBE_LEN];
        dev.read_at(DEFAULT_PARTITION_OFFSET, &mut head).unwrap();
        let probed = own_backup_inventory::fs_probe::identify(&head).expect("image holds a filesystem");
        assert_eq!(probed.fs, own_backup_common::types::FileSystem::Ext4);
    }

    /// Full round trip: back up a real ext4, restore to a blank target, and
    /// confirm the target is a valid ext4. `--ignored` (needs mkfs.ext4).
    #[test]
    #[ignore]
    fn backup_then_restore_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.ext4");
        std::fs::File::create(&src).unwrap().set_len(64 * 1024 * 1024).unwrap();
        assert!(std::process::Command::new("mkfs.ext4")
            .args(["-q", "-F", "-L", "roundtrip", src.to_str().unwrap()])
            .status()
            .unwrap()
            .success());

        let img = dir.path().join("out.img");
        let sum = backup_volume(
            &LinuxBackupRequest {
                source: src.to_str().unwrap().to_string(),
                image_path: img.clone(),
                block_size: DEFAULT_BLOCK_SIZE,
                all_sectors: false,
                passphrase: None,
                job_id: None,
            },
            no_progress,
        )
        .unwrap();

        // Restore onto a blank target sized to the volume.
        let tgt = dir.path().join("restored.bin");
        std::fs::File::create(&tgt).unwrap().set_len(sum.source_bytes).unwrap();
        let r = restore_volume(
            &LinuxRestoreRequest {
                image_path: img,
                target: tgt.to_str().unwrap().to_string(),
                verify_checksums: true,
            },
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(r.blocks_restored, sum.used_blocks);

        // The restored target is a valid ext4 with the original label.
        let probed =
            own_backup_inventory::fs_probe::probe_partition_path(tgt.to_str().unwrap()).unwrap();
        assert_eq!(probed.fs, own_backup_common::types::FileSystem::Ext4);
        assert_eq!(probed.label.as_deref(), Some("roundtrip"));
    }
}
