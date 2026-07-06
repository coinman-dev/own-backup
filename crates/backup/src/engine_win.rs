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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tracing::{info, warn};
use uuid::Uuid;
use zeroize::Zeroizing;

use own_backup_common::disk::DiskInfo;
use own_backup_common::gpt::DiskBackupManifest;
use own_backup_common::image::ImageManifest;
use own_backup_common::progress::{BackupProgress, RestoreProgress};
use own_backup_common::types::{
    BackupMode, BackupStatus, BackupType, BootMode, CompressionAlgo, PartitionTable,
};
use own_backup_common::{OwnBackupError, Result};
use own_backup_snapshot::VssSnapshotSession;

use crate::block_reader::BlockReader;
use crate::block_reader_win::WinBlockReader;
use crate::checksum::compute_block_checksum;
use crate::exclusions::{ByteRanges, ZeroingBlockReader};
use crate::exclusions_win::collect_excluded_ranges;
use crate::gpt_writer::{disk_gpt_write_ops, gpt_write_ops, GptLayout, DEFAULT_PARTITION_OFFSET};
use crate::raw_copy::copy_used_blocks;
use crate::san_policy::SanPolicyGuard;
use crate::sidecar::{
    read_sidecar, sidecar_path_for, write_sidecar, ChainRole, Sidecar, SidecarInfo,
};
use crate::used_sectors::{AllUsedMap, UsedSectorMap};
use crate::used_sectors_win::NtfsUsedSectorMap;
use crate::vhdx::{
    attach_vhdx, create_differencing_vhdx, create_dynamic_vhdx, detach_vhdx, AttachMode,
    VhdxCreateOptions,
};
use crate::win_device::{normalize_device_path, WinRawDevice};

/// Default backup block size: 1 MiB (multiple of every real sector size).
pub const DEFAULT_BLOCK_SIZE: u32 = 1024 * 1024;

const MIB: u64 = 1024 * 1024;

/// Flush the image to disk every this many written bytes during the copy, so
/// the OS write-back cache never grows huge and the final flush/detach is
/// quick — the wait is spread across the honest progress bar instead of a long
/// stall at 100 %. Especially visible for differencing (incremental) images,
/// whose scattered writes otherwise all land at detach time.
const FLUSH_INTERVAL_BYTES: u64 = 256 * MIB;

/// Parameters for a volume backup.
pub struct VolumeBackupRequest {
    /// Source volume: `C:`, `\\.\C:` or a VSS snapshot device path.
    pub source: String,
    /// Output image path (`.vhdx`/`.avhdx`). Must not exist yet.
    pub image_path: PathBuf,
    /// Backup block size in bytes; use [`DEFAULT_BLOCK_SIZE`].
    pub block_size: u32,
    /// Copy every block even when an NTFS bitmap is available.
    pub all_sectors: bool,
    /// Take a VSS shadow copy of the volume and read from it (consistent
    /// backup of a live volume). Falls back to reading the live volume with a
    /// warning if VSS fails. Ignored for non-volume sources.
    pub use_vss: bool,
    /// Full / Incremental / Differential (phase 5). Non-full kinds need
    /// `parent_image`; if the chain turns out unusable (parent missing,
    /// volume resized, block size changed) the engine falls back to a full
    /// backup — the summary reports what was actually done.
    pub kind: BackupType,
    /// Parent image for incremental (previous version) or differential (the
    /// chain's full backup).
    pub parent_image: Option<PathBuf>,
    /// File-exclusion glob patterns (phase 3). Applied only in smart NTFS
    /// mode; excluded clusters are dropped from the used map and zeroed
    /// inside blocks that are still copied.
    pub exclusion_patterns: Vec<String>,
    /// GUI job id, stamped into the sidecar so retention can find this
    /// job's chains later.
    pub job_id: Option<String>,
    /// Passphrase for an encrypted (LUKS) source. When present and the source
    /// is a LUKS container, the volume is unlocked and its *decrypted* data is
    /// backed up (used blocks only); on restore, encryption must be re-applied
    /// manually. Ignored for non-encrypted sources.
    pub passphrase: Option<Zeroizing<String>>,
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
    /// What was actually produced — may differ from the requested kind when
    /// the engine fell back to a full backup.
    pub backup_kind: BackupType,
    /// Used blocks skipped because they match the parent version.
    pub blocks_unchanged: u64,
    /// Files whose extents were excluded (phase 3).
    pub excluded_files: u64,
    /// Bytes excluded from the image by exclusion patterns.
    pub excluded_bytes: u64,
    /// Per-category byte totals of the files in the archive; None when no
    /// exclusion walk ran (exclusions disabled / non-NTFS / sector mode).
    pub categories: Option<crate::exclusions::CategorySizes>,
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

    // Enable SeBackupPrivilege so the exclusion scan can read/list ACL-protected
    // paths (System Volume Information, protected app data). Best-effort.
    crate::privileges_win::enable_backup_privileges();

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

/// Check that `parent_image` can serve as the parent of a new child image for
/// the volume being backed up.
fn validate_parent(parent_image: &Path, block_size: u32, volume_len: u64) -> Result<Sidecar> {
    if !parent_image.exists() {
        return Err(OwnBackupError::Config(format!(
            "parent image not found: {}",
            parent_image.display()
        )));
    }
    let sidecar = load_sidecar_of(parent_image)?;
    if sidecar.manifest.block_size != block_size {
        return Err(OwnBackupError::Config(format!(
            "parent block size {} != requested {block_size}",
            sidecar.manifest.block_size
        )));
    }
    if sidecar.manifest.source_disk.size_bytes != volume_len {
        return Err(OwnBackupError::Config(format!(
            "volume size changed since the parent backup ({} -> {volume_len} bytes)",
            sidecar.manifest.source_disk.size_bytes
        )));
    }
    Ok(sidecar)
}

fn backup_volume_inner(
    req: &VolumeBackupRequest,
    mut progress: impl FnMut(&BackupProgress) -> Result<()>,
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

    // --- Optional LUKS unlock ---
    // When a passphrase is supplied and the source is a LUKS container, we back
    // up its DECRYPTED data area (used blocks only). The inner filesystem is
    // Linux (btrfs/ext/xfs), so the NTFS-specific steps below are skipped.
    let mut src_probe = WinRawDevice::open_read(&read_path)?;
    let luks: Option<crate::luks::LuksParams> = match &req.passphrase {
        Some(pass) => {
            let mut header = vec![0u8; 4096];
            src_probe.read_at(0, &mut header)?;
            if crate::luks::is_luks(&header) {
                let params = crate::luks::open_luks(&mut src_probe, pass.as_bytes())?;
                info!(kind = ?params.kind, "LUKS source unlocked; backing up decrypted data");
                Some(params)
            } else {
                None
            }
        }
        None => None,
    };
    let device_len = src_probe.length();
    let sector_size = src_probe.sector_size();
    drop(src_probe);

    // For an encrypted source the backed-up "volume" is the decrypted data area.
    let volume_len = match &luks {
        Some(p) => device_len.saturating_sub(p.data_offset),
        None => device_len,
    };

    // Opens a fresh read source for the used-map builders and the block reader:
    // a plain device, or a decrypting view over it for a LUKS source.
    let open_source = |luks: &Option<crate::luks::LuksParams>| -> Result<Box<dyn crate::fat_map::RawVolume + Send>> {
        let dev = WinRawDevice::open_read(&read_path)?;
        Ok(match luks {
            Some(p) => Box::new(crate::decrypt::DecryptingVolume::new(Box::new(dev), p.clone())),
            None => Box::new(dev),
        })
    };

    if req.block_size == 0 || req.block_size % sector_size != 0 {
        return Err(OwnBackupError::Config(format!(
            "block size {} is not a multiple of the source sector size {sector_size}",
            req.block_size
        )));
    }
    let total_blocks = volume_len.div_ceil(req.block_size as u64);

    // --- Parent chain (incremental / differential) ---
    // A broken chain must not fail the run: fall back to a full backup and
    // report the fallback in the summary.
    let mut kind = req.kind;
    let mut parent: Option<(PathBuf, Sidecar)> = None;
    if kind != BackupType::Full {
        match &req.parent_image {
            None => {
                warn!("no parent image supplied for a {kind} backup — creating a full backup");
                kind = BackupType::Full;
            }
            Some(parent_path) => {
                match validate_parent(parent_path, req.block_size, volume_len) {
                    Ok(sidecar) => parent = Some((parent_path.clone(), sidecar)),
                    Err(e) => {
                        warn!(
                            parent = %parent_path.display(),
                            error = %e,
                            "parent chain unusable — creating a full backup instead"
                        );
                        kind = BackupType::Full;
                    }
                }
            }
        }
    }

    // --- Used-sector map: NTFS → FAT32/exFAT → ext → btrfs → xfs → full copy ---
    let mut ntfs_map: Option<NtfsUsedSectorMap> = None;
    let mut fat_map: Option<crate::fat_map::FatUsedMap> = None;
    let mut ext_map: Option<crate::ext_map::ExtUsedMap> = None;
    let mut btrfs_map: Option<crate::btrfs_map::BtrfsUsedMap> = None;
    let mut xfs_map: Option<crate::xfs_map::XfsUsedMap> = None;
    // For an encrypted source the map is built over the decrypting view and
    // returned boxed directly (no exclusions apply to a Linux volume).
    let mut luks_used: Option<Box<dyn UsedSectorMap>> = None;
    let smart_mode = if req.all_sectors {
        info!("Sector-by-sector mode requested; copying all blocks");
        false
    } else if luks.is_some() {
        // Decrypted Linux volume: only the RawVolume-based parsers apply
        // (FAT/ext/btrfs); NTFS FSCTL and file exclusions do not.
        let mut src = open_source(&luks)?;
        let map: Option<Box<dyn UsedSectorMap>> =
            if let Ok(m) = crate::fat_map::build_fat_used_map(src.as_mut(), req.block_size) {
                info!(fs = m.filesystem, "Using FAT map over decrypted volume");
                Some(Box::new(m))
            } else if let Ok(m) = crate::ext_map::build_ext_used_map(src.as_mut(), req.block_size) {
                info!(fs = "ext4", "Using ext map over decrypted volume");
                Some(Box::new(m))
            } else if let Ok(m) = crate::btrfs_map::build_btrfs_used_map(src.as_mut(), req.block_size) {
                info!(fs = "btrfs", "Using btrfs map over decrypted volume");
                Some(Box::new(m))
            } else if let Ok(m) = crate::xfs_map::build_xfs_used_map(src.as_mut(), req.block_size) {
                info!(fs = "xfs", "Using xfs map over decrypted volume");
                Some(Box::new(m))
            } else {
                warn!("no used-block map for the decrypted volume — sector-by-sector");
                None
            };
        let smart = map.is_some();
        luks_used = map;
        smart
    } else {
        match NtfsUsedSectorMap::open(&read_path, req.block_size) {
            Ok(map) => {
                ntfs_map = Some(map);
                true
            }
            Err(ntfs_err) => {
                // Not NTFS — try parsing FAT32/exFAT, then ext2/3/4, structures
                // directly (each on its own device handle).
                let fat_attempt = WinRawDevice::open_read(&read_path).and_then(|mut dev| {
                    crate::fat_map::build_fat_used_map(&mut dev, req.block_size)
                });
                match fat_attempt {
                    Ok(map) => {
                        info!(fs = map.filesystem, "Using FAT used-cluster map");
                        fat_map = Some(map);
                        true
                    }
                    Err(fat_err) => {
                        let ext_attempt = WinRawDevice::open_read(&read_path).and_then(|mut dev| {
                            crate::ext_map::build_ext_used_map(&mut dev, req.block_size)
                        });
                        match ext_attempt {
                            Ok(map) => {
                                info!(fs = "ext4", "Using ext used-block map");
                                ext_map = Some(map);
                                true
                            }
                            Err(ext_err) => {
                                let btrfs_attempt =
                                    WinRawDevice::open_read(&read_path).and_then(|mut dev| {
                                        crate::btrfs_map::build_btrfs_used_map(&mut dev, req.block_size)
                                    });
                                match btrfs_attempt {
                                    Ok(map) => {
                                        info!(fs = "btrfs", "Using btrfs used-block map");
                                        btrfs_map = Some(map);
                                        true
                                    }
                                    Err(btrfs_err) => {
                                        let xfs_attempt =
                                            WinRawDevice::open_read(&read_path).and_then(|mut dev| {
                                                crate::xfs_map::build_xfs_used_map(&mut dev, req.block_size)
                                            });
                                        match xfs_attempt {
                                            Ok(map) => {
                                                info!(fs = "xfs", "Using xfs used-block map");
                                                xfs_map = Some(map);
                                                true
                                            }
                                            Err(xfs_err) => {
                                                warn!(
                                                    ntfs = %ntfs_err,
                                                    fat = %fat_err,
                                                    ext = %ext_err,
                                                    btrfs = %btrfs_err,
                                                    xfs = %xfs_err,
                                                    "no used-block map available, falling back to sector-by-sector"
                                                );
                                                false
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    };

    // --- File exclusions (phase 3) ---
    // NTFS/ReFS: FSCTL_GET_RETRIEVAL_POINTERS per matched file. FAT32: walk the
    // raw directory tree ourselves (no FSCTL / no ACLs) and rebuild the map with
    // the matched clusters excluded.
    let mut excluded_files = 0u64;
    let mut excluded_bytes = 0u64;
    let mut zero_ranges: Option<ByteRanges> = None;
    let mut categories: Option<crate::exclusions::CategorySizes> = None;
    // "prepare"-phase progress tick (blocks_total == 0 marks it for the UI).
    let prepare_tick = |entries: u64| {
        progress(&BackupProgress {
            backup_id: "prepare".into(),
            status: BackupStatus::Running,
            blocks_done: entries,
            blocks_total: 0,
            bytes_written: 0,
            speed_bps: 0,
        })
    };
    let fat_exclusions = fat_map
        .as_ref()
        .is_some_and(|f| f.filesystem == "FAT32" || f.filesystem == "exFAT")
        && !req.exclusion_patterns.is_empty();
    if let Some(map) = ntfs_map.as_mut() {
        if !req.exclusion_patterns.is_empty() {
            let scan =
                collect_excluded_ranges(&read_path, map.cluster_size(), &req.exclusion_patterns, prepare_tick)?;
            excluded_files = scan.files_excluded;
            excluded_bytes = scan.bytes_excluded;
            categories = Some(scan.categories);
            if !scan.ranges.is_empty() {
                map.apply_excluded_ranges(&scan.ranges);
                zero_ranges = Some(scan.ranges);
            }
        }
        map.release_bitmap();
    } else if fat_exclusions {
        let mut dev = WinRawDevice::open_read(&read_path)?;
        let scan =
            crate::fat_exclusions::scan_fat_exclusions(&mut dev, &req.exclusion_patterns, prepare_tick)?;
        excluded_files = scan.files_excluded;
        excluded_bytes = scan.bytes_excluded;
        categories = Some(scan.categories);
        if !scan.ranges.is_empty() {
            // Rebuild the map so excluded clusters drop out (cluster-level).
            let mut dev2 = WinRawDevice::open_read(&read_path)?;
            let rebuilt =
                crate::fat_map::build_fat_used_map_excl(&mut dev2, req.block_size, Some(&scan.ranges))?;
            fat_map = Some(rebuilt);
            zero_ranges = Some(scan.ranges);
        }
    } else if !req.exclusion_patterns.is_empty() {
        warn!("file exclusions are supported on NTFS/ReFS and FAT32 in smart mode — ignored for this run");
    }

    let used: Box<dyn UsedSectorMap> = if let Some(map) = luks_used {
        map
    } else {
        match (ntfs_map, fat_map, ext_map, btrfs_map, xfs_map) {
            (Some(map), ..) => Box::new(map),
            (None, Some(map), ..) => Box::new(map),
            (None, None, Some(map), ..) => Box::new(map),
            (None, None, None, Some(map), _) => Box::new(map),
            (None, None, None, None, Some(map)) => Box::new(map),
            (None, None, None, None, None) => Box::new(AllUsedMap::new(total_blocks)),
        }
    };
    let used_blocks = used.used_block_count();

    // --- Create and attach the image ---
    let data_offset = match &parent {
        None => {
            // Disk = 1 MiB GPT head + volume + tail for the backup GPT.
            let disk_size = DEFAULT_PARTITION_OFFSET + volume_len.div_ceil(MIB) * MIB + MIB;
            create_dynamic_vhdx(
                &req.image_path,
                &VhdxCreateOptions {
                    max_size: disk_size,
                    logical_sector_size: sector_size,
                    physical_sector_size: std::cmp::max(sector_size, 4096),
                },
            )?;
            DEFAULT_PARTITION_OFFSET
        }
        Some((parent_path, parent_sidecar)) => {
            // Child inherits geometry and the GPT from its chain.
            create_differencing_vhdx(&req.image_path, parent_path)?;
            parent_sidecar.info.volume_data_offset
        }
    };

    // A differencing (child) image inherits a full NTFS filesystem from its
    // parent, which Windows auto-mounts the instant the disk is attached. That
    // transient mount is what makes DetachVirtualDisk block on a fixed ~180 s
    // volume-teardown timeout when the backup ends (full images start blank and
    // never mount, so they detach in milliseconds). Force new disks to arrive
    // OFFLINE for the attach window so the volume never surfaces at all; the
    // guard restores the machine policy the moment the disk is attached and
    // confirmed offline (and on the error/unwind path).
    let san_guard = SanPolicyGuard::offline_all();
    let attached = attach_vhdx(&req.image_path, AttachMode::RawWrite)?;
    let mut writer = open_write_with_retry(attached.physical_path())?;
    // Belt-and-suspenders: take the disk offline explicitly (in case the SAN
    // policy could not be changed) so raw sector writes are not blocked by a
    // mounted filesystem (ERROR_ACCESS_DENIED). For children the disk carries
    // the chain's GPT GUID, so ALSO persist the offline attribute. (Full images
    // do the persist step after their GPT is written below.)
    writer.set_offline_writable(parent.is_some())?;
    drop(san_guard);
    if writer.length() < data_offset + volume_len {
        return Err(OwnBackupError::Vhdx(format!(
            "attached image is {} bytes — too small for the volume ({} bytes at offset {data_offset})",
            writer.length(),
            volume_len
        )));
    }

    let disk_guid = match &parent {
        None => {
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
            // Let partmgr re-read the fresh GPT (the disk stays offline, so
            // no volumes surface), then persist the offline attribute keyed
            // to the new disk GUID — future attaches of this image and of
            // every differencing child arrive offline, closing the
            // auto-mount race for the whole chain. Best-effort: a failure
            // only means the next attach may transiently mount.
            if let Err(e) = writer
                .update_properties()
                .and_then(|()| writer.set_offline_writable(true))
            {
                warn!(error = %e, "could not persist the offline attribute for the new image");
            }
            layout.disk_guid.to_string()
        }
        Some((_, parent_sidecar)) => parent_sidecar.manifest.source_disk.disk_guid.clone(),
    };

    // --- Copy used blocks into the partition ---
    // For child images only blocks that differ from the parent version are
    // written; everything else resolves through the VHDX chain.
    let parent_hashes: Option<HashMap<u64, u64>> = parent.as_ref().map(|(_, sidecar)| {
        sidecar
            .checksums
            .iter()
            .map(|c| (c.block_number, c.checksum))
            .collect()
    });

    let backup_id = Uuid::new_v4();
    let mut reader: Box<dyn BlockReader> = if luks.is_some() {
        // Read decrypted blocks through the LUKS view.
        Box::new(crate::block_reader::RawVolumeBlockReader::new(
            open_source(&luks)?,
            req.block_size,
        )?)
    } else {
        Box::new(WinBlockReader::open(&read_path, req.block_size)?)
    };
    if let Some(ranges) = zero_ranges {
        reader = Box::new(ZeroingBlockReader::new(reader, ranges));
    }

    // The NTFS backup boot sector lives at TotalSectors × BytesPerSector —
    // the first sector past the filesystem end (the partition is a few
    // sectors longer than the filesystem). That region cannot be read through
    // a volume handle (short read) and would be stored as zeros; NTFS then
    // "self-heals" the zeroed copy when Windows transiently auto-mounts a
    // differencing chain at attach time, writing into the child image and
    // failing verify on the volume's last block (confirmed by VHDX forensics:
    // exactly one healed sector, at TotalSectors, byte-equal to sector 0).
    // Store the correct copy ourselves: NTFS finds a valid backup boot sector
    // and writes nothing — and even if it writes, the bytes are identical.
    // (Skipped for a decrypted LUKS source — its inner FS is never NTFS.)
    if luks.is_none() && volume_len > sector_size as u64 {
        let mut sector0 = vec![0u8; sector_size as usize];
        let mut probe = WinRawDevice::open_read(&read_path)?;
        probe.read_at(0, &mut sector0)?;
        drop(probe);
        if &sector0[3..7] == b"NTFS" {
            let bps = u16::from_le_bytes(sector0[0x0B..0x0D].try_into().unwrap()) as u64;
            let total_sectors =
                u64::from_le_bytes(sector0[0x28..0x30].try_into().unwrap());
            let backup_boot_offset = total_sectors.saturating_mul(bps);
            // Sanity: same sector size as the device, and the location must
            // fall within the volume's last MiB — a corrupt BPB must never
            // graft a boot sector into the middle of the data.
            if bps == sector_size as u64
                && backup_boot_offset + bps <= volume_len
                && backup_boot_offset >= volume_len.saturating_sub(MIB)
            {
                info!(
                    backup_boot_offset,
                    "Patching NTFS backup boot sector into the image tail"
                );
                reader = Box::new(crate::block_reader::PatchReader::new(
                    reader,
                    backup_boot_offset,
                    sector0,
                ));
            } else {
                warn!(
                    total_sectors,
                    bps, "NTFS BPB values out of range — tail patch skipped"
                );
            }
        }
    }

    let mut bytes_since_flush = 0u64;
    let outcome = copy_used_blocks(
        &backup_id.to_string(),
        reader.as_mut(),
        used.as_ref(),
        parent_hashes.as_ref(),
        |block_number, data| {
            writer.write_at(data_offset + block_number * req.block_size as u64, data)?;
            // Drain the OS cache in bounded chunks so the end-of-backup flush
            // stays short (see FLUSH_INTERVAL_BYTES).
            bytes_since_flush += data.len() as u64;
            if bytes_since_flush >= FLUSH_INTERVAL_BYTES {
                writer.flush()?;
                bytes_since_flush = 0;
            }
            Ok(())
        },
        &mut progress,
    )?;

    // Flushing a dynamic/differencing VHDX and detaching it writes all the
    // still-cached image data back to the destination drive — many seconds to
    // minutes for a large image. Signal a "finalize" phase (blocks_total == 0,
    // backup_id "finalize") so the UI shows progress instead of a frozen 100%.
    // Cancellation here is ignored: the data is already copied.
    let _ = progress(&BackupProgress {
        backup_id: "finalize".into(),
        status: BackupStatus::Running,
        blocks_done: 0,
        blocks_total: 0,
        bytes_written: outcome.bytes_copied,
        speed_bps: 0,
    });
    // Timed step by step: the finalize stall lives in ONE of these, and the
    // log should say which (flush of remaining cache vs handle close vs
    // detach of the virtual disk vs VSS release).
    let t = Instant::now();
    writer.flush()?;
    let flush_ms = t.elapsed().as_millis();
    let t = Instant::now();
    drop(writer);
    let close_ms = t.elapsed().as_millis();
    let t = Instant::now();
    drop(attached); // detaches the image
    let detach_ms = t.elapsed().as_millis();

    // Notify VSS writers of success and release the shadow copy.
    let t = Instant::now();
    if let Some(session) = vss.take() {
        if let Err(e) = session.complete() {
            warn!(error = %e, "VSS BackupComplete failed (backup data is already safe)");
        }
    }
    info!(
        flush_ms,
        close_ms,
        detach_ms,
        vss_release_ms = t.elapsed().as_millis(),
        "Image finalize timings"
    );

    // --- Sidecar ---
    // Child sidecars carry the FULL state checksums (every used block, written
    // or inherited), so verify/restore/mount treat the leaf exactly like a
    // full image — the VHDX chain resolves inherited blocks.
    let image_file_name = req
        .image_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let manifest = ImageManifest {
        backup_id,
        created_at: chrono::Utc::now().to_rfc3339(),
        backup_type: kind,
        parent_backup_id: parent.as_ref().map(|(_, s)| s.manifest.backup_id),
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
            disk_guid,
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
        // Encrypted source captured in the clear (used blocks only): record the
        // kind and that encryption must be re-applied after restore. The LUKS
        // header is intentionally not stored — a plaintext capture restores to
        // an unencrypted volume, so the original header is not needed.
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

    let mut sidecar_info = match &parent {
        None => SidecarInfo::new_full(image_file_name, data_offset),
        Some((parent_path, _)) => {
            let role = match kind {
                BackupType::Differential => ChainRole::Differential,
                _ => ChainRole::Incremental,
            };
            let parent_file_name = parent_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            SidecarInfo::new_child(image_file_name, role, parent_file_name, data_offset)
        }
    };
    sidecar_info.job_id = req.job_id.clone();

    let sidecar_path = sidecar_path_for(&req.image_path);
    write_sidecar(&sidecar_path, &sidecar_info, &manifest, &outcome.checksums)?;

    info!(
        image = %req.image_path.display(),
        kind = %kind,
        blocks_written = outcome.blocks_copied,
        blocks_unchanged = outcome.blocks_unchanged,
        bytes = outcome.bytes_copied,
        excluded_files,
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
        backup_kind: kind,
        blocks_unchanged: outcome.blocks_unchanged,
        excluded_files,
        excluded_bytes,
        categories,
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

// ---------------------------------------------------------------------------
// Whole-disk (bootable) restore
// ---------------------------------------------------------------------------

/// Restore a whole disk from a `.obdm` manifest onto a target physical drive.
/// DESTRUCTIVE — the target is repartitioned and every partition overwritten.
pub struct DiskRestoreRequest {
    /// Path to the `.obdm` disk-backup manifest (its images live alongside).
    pub manifest_path: PathBuf,
    /// Target physical drive number (`\\.\PhysicalDrive{target_disk}`).
    pub target_disk: u32,
    pub verify_checksums: bool,
}

/// Progress for a whole-disk restore.
pub struct DiskRestoreProgress {
    /// "gpt" while writing the partition table, "partition" while copying data.
    pub phase: String,
    /// 0-based index of the partition being restored, and the total count.
    pub partition_index: usize,
    pub partition_count: usize,
    pub part_label: String,
    pub blocks_done: u64,
    pub blocks_total: u64,
    /// Bytes written across the whole disk so far.
    pub bytes_written: u64,
}

pub struct DiskRestoreSummary {
    pub partitions: usize,
    pub bytes_written: u64,
    pub duration: Duration,
}

/// Recreate a disk from its `.obdm`: write the captured GPT, then restore each
/// partition's image into its original offset. The target is taken offline for
/// the duration so Windows does not auto-mount (and lock) partitions mid-write,
/// then brought back online. The GUID-preserving GPT keeps a UEFI system
/// bootable without touching the BCD.
///
/// The caller is responsible for the destructive confirmation and the
/// [`crate`]-external system-disk guard (`own_backup_restore::guard`).
pub fn restore_disk(
    req: &DiskRestoreRequest,
    mut progress: impl FnMut(&DiskRestoreProgress) -> Result<()>,
) -> Result<DiskRestoreSummary> {
    let started = Instant::now();

    let raw = std::fs::read(&req.manifest_path).map_err(|e| {
        OwnBackupError::ImageFormat(format!(
            "cannot read disk manifest {}: {e}",
            req.manifest_path.display()
        ))
    })?;
    let manifest: DiskBackupManifest = serde_json::from_slice(&raw)
        .map_err(|e| OwnBackupError::ImageFormat(format!("bad .obdm manifest: {e}")))?;
    let folder = req
        .manifest_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_default();
    let layout = &manifest.layout;
    let sector = layout.sector_size as u64;

    let target_path = format!(r"\\.\PhysicalDrive{}", req.target_disk);

    // Probe the target size and build the GPT (validates every partition fits).
    let target_size = WinRawDevice::open_read(&target_path)?.length();
    let ops = disk_gpt_write_ops(layout, target_size)?;

    info!(
        target = %target_path,
        model = %manifest.disk_model,
        partitions = manifest.images.len(),
        "Whole-disk restore: writing GPT"
    );
    progress(&DiskRestoreProgress {
        phase: "gpt".into(),
        partition_index: 0,
        partition_count: manifest.images.len(),
        part_label: "Таблица разделов (GPT)".into(),
        blocks_done: 0,
        blocks_total: 0,
        bytes_written: 0,
    })?;

    // Take the target offline (session-only), write the protective MBR + GPT,
    // then refresh so the OS re-reads the (offline) table.
    {
        let mut td = WinRawDevice::open_write(&target_path)?;
        td.set_offline_writable(false)?;
        for op in &ops {
            td.write_at(op.offset, &op.bytes)?;
        }
        td.flush()?;
        let _ = td.update_properties();
    }

    // Restore each partition image into its window on the (offline) target.
    let count = manifest.images.len();
    let mut total_bytes = 0u64;
    for (i, img) in manifest.images.iter().enumerate() {
        let entry = layout
            .partitions
            .iter()
            .find(|p| p.index == img.part_index)
            .ok_or_else(|| {
                OwnBackupError::RestoreIncompatible(format!(
                    "manifest references partition {} which is not in the GPT layout",
                    img.part_index
                ))
            })?;
        let off = entry.first_lba * sector;
        let len = (entry.last_lba + 1 - entry.first_lba) * sector;
        let target = format!("part:{}:{off}:{len}", req.target_disk);
        let label = if entry.name.is_empty() {
            format!("Раздел {} ({})", img.part_index, img.filesystem)
        } else {
            format!("{} ({})", entry.name, img.filesystem)
        };
        info!(
            image = %img.image_file_name,
            part = img.part_index,
            offset = off,
            "Whole-disk restore: restoring partition"
        );

        let vreq = VolumeRestoreRequest {
            image_path: folder.join(&img.image_file_name),
            target,
            verify_checksums: req.verify_checksums,
        };
        let base = total_bytes;
        let summary = restore_volume(&vreq, |p| {
            progress(&DiskRestoreProgress {
                phase: "partition".into(),
                partition_index: i,
                partition_count: count,
                part_label: label.clone(),
                blocks_done: p.blocks_done,
                blocks_total: p.blocks_total,
                bytes_written: base + p.bytes_written,
            })
        })?;
        total_bytes += summary.bytes_restored;
    }

    // Bring the target back online and refresh so the restored volumes appear.
    if let Ok(td) = WinRawDevice::open_write(&target_path) {
        let _ = td.set_online_this_session();
        let _ = td.update_properties();
    }

    info!(
        target = %target_path,
        partitions = count,
        bytes = total_bytes,
        secs = started.elapsed().as_secs(),
        "Whole-disk restore complete"
    );
    Ok(DiskRestoreSummary {
        partitions: count,
        bytes_written: total_bytes,
        duration: started.elapsed(),
    })
}

/// Recreate the boot files (and, on UEFI, the firmware NVRAM entry) for a
/// restored Windows via `bcdboot`. Needed only for a **fresh disk / new
/// hardware** — a same-disk restore that preserved the partition GUIDs boots
/// as-is. `windows_volume` is the restored OS volume ("C:"); `system_volume`
/// is the EFI System Partition mount ("S:") for UEFI (omit to let bcdboot pick
/// the current one). Typically run from WinPE.
pub fn finalize_boot(
    windows_volume: &str,
    system_volume: Option<&str>,
    uefi: bool,
) -> Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let win_dir = format!(r"{}\Windows", windows_volume.trim_end_matches(['\\', '/']));
    let mut cmd = std::process::Command::new("bcdboot");
    cmd.arg(&win_dir);
    if let Some(esp) = system_volume {
        cmd.args(["/s", esp]);
    }
    cmd.args(["/f", if uefi { "UEFI" } else { "BIOS" }]);
    cmd.creation_flags(CREATE_NO_WINDOW);

    let out = cmd
        .output()
        .map_err(|e| OwnBackupError::Device(format!("failed to run bcdboot: {e}")))?;
    if !out.status.success() {
        return Err(OwnBackupError::RestoreIncompatible(format!(
            "bcdboot failed (exit {}): {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    info!(win_dir, uefi, "bcdboot: boot files recreated");
    Ok(())
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
        let actual = compute_block_checksum(&buf);

        if actual != entry.checksum {
            // Re-read once before declaring corruption: a freshly detached and
            // re-attached image can hand back a transient bad read that a
            // second read clears. Only a checksum that stays wrong is a real
            // image defect — and we log exactly which block for diagnosis.
            let mut buf2 = vec![0u8; len];
            image_dev.read_at(data_offset + offset, &mut buf2)?;
            let actual2 = compute_block_checksum(&buf2);
            if actual2 != entry.checksum {
                // Structure hints: an all-zero block or a non-zero tail where
                // zeros were recorded each point at a specific failure mode
                // (unwritten region vs external write into the image).
                let all_zero = buf2.iter().all(|&b| b == 0);
                let tail_zero = buf2[len.saturating_sub(4096)..].iter().all(|&b| b == 0);
                warn!(
                    block = entry.block_number,
                    image_offset = data_offset + offset,
                    len,
                    expected = format!("{:016x}", entry.checksum),
                    actual = format!("{:016x}", actual2),
                    all_zero,
                    tail_4096_zero = tail_zero,
                    "verify: block checksum mismatch (image defect)"
                );
                mismatched.push(entry.block_number);
            } else {
                info!(
                    block = entry.block_number,
                    "verify: transient read mismatch cleared on re-read"
                );
            }
        }
        checked += 1;
        progress(checked, total)?;
    }

    if mismatched.is_empty() {
        info!(image = %image_path.display(), blocks = total, "verify: image OK");
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
    // Backup images carry a persisted "offline" disk attribute (it keeps the
    // chain from auto-mounting during backups), so for browsing the disk must
    // be brought online explicitly — for this session only, the registry
    // attribute stays. The attach is read-only, so mounted volumes are
    // write-protected either way. Requires a read-write device handle (IOCTL
    // access rights), which vhdmp grants even on a read-only attach.
    match open_write_with_retry(attached.physical_path()) {
        Ok(dev) => {
            if let Err(e) = dev.set_online_this_session() {
                warn!(error = %e, "could not bring the mounted image online — it may show as offline in Disk Management");
            }
        }
        Err(e) => {
            warn!(error = %e, "could not open the mounted image to bring it online");
        }
    }
    Ok(attached.physical_path().to_string())
}

/// Detach a previously mounted image.
pub fn unmount_image(image_path: &Path) -> Result<()> {
    detach_vhdx(image_path)
}

/// Outcome of decrypting an encrypted source: whether the passphrase unlocked
/// it, and (on success) the inner filesystem's name / volume label / used bytes
/// so the UI can show them like any other volume.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct DecryptedProbe {
    pub ok: bool,
    pub filesystem: String,
    pub label: String,
    pub used_bytes: u64,
}

/// Unlock the LUKS container at `device_path` with `passphrase` and probe the
/// decrypted filesystem. `ok = false` means the passphrase was rejected. A
/// source that is not LUKS returns `ok = true` with empty info (nothing to
/// unlock). Errors only on I/O so the UI can tell "wrong password" from
/// "could not read the device".
pub fn unlock_and_probe(device_path: &str, passphrase: &str) -> Result<DecryptedProbe> {
    use crate::fat_map::RawVolume;

    let src = normalize_device_path(device_path);
    let mut dev = WinRawDevice::open_read(&src)?;
    let mut header = vec![0u8; 4096];
    dev.read_at(0, &mut header)?;
    if !crate::luks::is_luks(&header) {
        return Ok(DecryptedProbe { ok: true, ..Default::default() });
    }
    let params = match crate::luks::open_luks(&mut dev, passphrase.as_bytes()) {
        Ok(p) => p,
        Err(_) => return Ok(DecryptedProbe { ok: false, ..Default::default() }),
    };
    drop(dev);

    // Read the decrypted superblock region and identify the inner filesystem.
    let mut view =
        crate::decrypt::DecryptingVolume::new(Box::new(WinRawDevice::open_read(&src)?), params);
    let want = (own_backup_inventory::fs_probe::PROBE_LEN as u64).min(view.len()) as usize;
    let mut buf = vec![0u8; want];
    view.read_at(0, &mut buf)?;
    let (filesystem, label, used_bytes) = match own_backup_inventory::fs_probe::identify(&buf) {
        Some(p) => (
            format!("{:?}", p.fs).to_lowercase(),
            p.label.unwrap_or_default(),
            p.used_bytes.unwrap_or(0),
        ),
        None => (String::new(), String::new(), 0),
    };
    Ok(DecryptedProbe { ok: true, filesystem, label, used_bytes })
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
