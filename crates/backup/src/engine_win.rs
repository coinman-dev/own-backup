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
    BackupMode, BackupStatus, BackupType, BootMode, CompressionAlgo, ImageFormat, PartitionTable,
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
    read_embedded_sidecar, read_sidecar, sidecar_path_for,
    write_sidecar, ChainRole, Sidecar, SidecarInfo,
};
use crate::used_sectors::{AllUsedMap, UsedSectorMap};
use crate::used_sectors_win::NtfsUsedSectorMap;
use crate::vhdx::{
    attach_vhdx, create_differencing_vhdx, create_dynamic_vhdx, detach_vhdx, merge_into_parent, AttachMode,
    VhdxCreateOptions,
};
use crate::block_source::BlockSource;
use crate::vhdx_bat::allocated_payload_blocks;
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
    /// Image container format (decision #13): VHDX — kernel-written, Windows-
    /// native mounting, NTFS/ReFS true removal, AVHDX chains; qcow2 — written
    /// in userspace with per-cluster zstd compression, no attach at all.
    /// qcow2 limits for now: full backups only, exclusions are zeroing-only.
    pub image_format: ImageFormat,
    /// Full source-disk layout snapshot (the parent physical disk of this
    /// volume, from the GUI's live inventory) to embed in the sidecar, so a
    /// restore can show the whole disk and flag the backed-up partition without
    /// jobs.json. `None` for CLI runs (the sidecar then omits it).
    pub source_disk: Option<own_backup_common::disk::DiskInfo>,
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

/// Write target behind the format switch. VHDX goes through the kernel
/// (attach → raw writes to the PhysicalDrive node); qcow2 is written entirely
/// in userspace with per-cluster zstd compression — no attach, no SAN-policy
/// juggling, no flush pacing (the OS cache never holds a whole image).
enum ImageTarget {
    Vhdx {
        attached: crate::vhdx::AttachedVhdx,
        writer: WinRawDevice,
        /// Bytes written since the last pacing flush (see FLUSH_INTERVAL_BYTES).
        bytes_since_flush: u64,
    },
    Qcow2 {
        writer: crate::qcow2::Qcow2StagedWriter<std::io::BufWriter<std::fs::File>>,
    },
}

impl ImageTarget {
    /// Virtual disk size of the image being written.
    fn length(&self) -> u64 {
        match self {
            ImageTarget::Vhdx { writer, .. } => writer.length(),
            ImageTarget::Qcow2 { writer } => writer.length(),
        }
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        match self {
            ImageTarget::Vhdx { writer, bytes_since_flush, .. } => {
                writer.write_at(offset, data)?;
                // Drain the OS cache in bounded chunks so the end-of-backup
                // flush stays short (see FLUSH_INTERVAL_BYTES).
                *bytes_since_flush += data.len() as u64;
                if *bytes_since_flush >= FLUSH_INTERVAL_BYTES {
                    writer.flush()?;
                    *bytes_since_flush = 0;
                }
                Ok(())
            }
            ImageTarget::Qcow2 { writer } => writer.write_at(offset, data),
        }
    }

    /// Finalize the image and log per-step timings (the finalize stall lives
    /// in ONE of these, and the log should say which).
    fn finalize(self) -> Result<()> {
        match self {
            ImageTarget::Vhdx { attached, mut writer, .. } => {
                let t = Instant::now();
                writer.flush()?;
                let flush_ms = t.elapsed().as_millis();
                let t = Instant::now();
                drop(writer);
                let close_ms = t.elapsed().as_millis();
                let t = Instant::now();
                drop(attached); // detaches the image
                let detach_ms = t.elapsed().as_millis();
                info!(flush_ms, close_ms, detach_ms, "VHDX finalize timings");
            }
            ImageTarget::Qcow2 { writer } => {
                let t = Instant::now();
                // Write L2/L1/refcounts + header, then make the file durable.
                let buf = writer.finish()?;
                let file = buf
                    .into_inner()
                    .map_err(|e| OwnBackupError::Io(e.into_error()))?;
                file.sync_all()?;
                info!(finish_ms = t.elapsed().as_millis(), "qcow2 finalize timings");
            }
        }
        Ok(())
    }
}

/// Read source behind the same format switch, picked by the sidecar's
/// `container` field: VHDX images are attached read-only and read as a raw
/// disk; qcow2 images are read directly in userspace (no attach).
pub(crate) enum ImageReadSource {
    Vhdx {
        /// Keeps the read-only attachment alive; detaches on drop.
        _attached: crate::vhdx::AttachedVhdx,
        dev: WinRawDevice,
    },
    Qcow2(crate::qcow2::Qcow2RandomReader),
}

impl ImageReadSource {
    pub(crate) fn open(image_path: &Path, container: &str) -> Result<Self> {
        match container {
            "qcow2" => Ok(Self::Qcow2(crate::qcow2::Qcow2RandomReader::open(image_path)?)),
            "vhdx" => {
                let attached = attach_vhdx(image_path, AttachMode::RawRead)?;
                let dev = open_read_with_retry(attached.physical_path())?;
                Ok(Self::Vhdx { _attached: attached, dev })
            }
            other => Err(OwnBackupError::ImageFormat(format!(
                "unknown image container '{other}' in sidecar"
            ))),
        }
    }

    pub(crate) fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        match self {
            Self::Vhdx { dev, .. } => dev.read_at(offset, buf),
            Self::Qcow2(r) => r.read_at(offset, buf),
        }
    }
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
    // The container's header is stored in the sidecar (D19), and it is measured
    // in megabytes — the meta partition has to be reserved with that in mind or
    // the metadata write at the very end has nowhere to go.
    let luks_reserve =
        crate::sidecar::luks_reserve_for(luks.as_ref().map(|p| p.data_offset));
    let meta_size = crate::sidecar::calculate_meta_partition_size(total_blocks, luks_reserve);

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
    // A chain must never cross container formats (decision 2026-07-08): the
    // format of an existing archive cannot be changed — that is a hard error,
    // not a fallback. The job runners refuse such runs before reaching the
    // engine; this guards direct callers.
    if let Some((parent_path, parent_sidecar)) = &parent {
        if parent_sidecar.info.container != req.image_format.extension() {
            return Err(OwnBackupError::Config(format!(
                "parent image {} is '{}' but the requested format is '{}' — \
                 changing the format of an existing backup chain is not allowed; \
                 create a new job (or remove the old backups)",
                parent_path.display(),
                parent_sidecar.info.container,
                req.image_format
            )));
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
    // The filesystem actually found inside the volume. For a LUKS/BitLocker
    // source this is what is INSIDE the container, because that is what the
    // image holds — the partition table outside still says "unknown", and a
    // restore screen reading only that would show a padlock over a plain
    // btrfs (D14, R8).
    let mut detected_fs: Option<String> = None;
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
                detected_fs = Some(m.filesystem.to_string());
                Some(Box::new(m))
            } else if let Ok(m) = crate::ext_map::build_ext_used_map(src.as_mut(), req.block_size) {
                info!(fs = "ext4", "Using ext map over decrypted volume");
                detected_fs = Some("ext4".into());
                Some(Box::new(m))
            } else if let Ok(m) = crate::btrfs_map::build_btrfs_used_map(src.as_mut(), req.block_size) {
                info!(fs = "btrfs", "Using btrfs map over decrypted volume");
                detected_fs = Some("btrfs".into());
                Some(Box::new(m))
            } else if let Ok(m) = crate::xfs_map::build_xfs_used_map(src.as_mut(), req.block_size) {
                info!(fs = "xfs", "Using xfs map over decrypted volume");
                detected_fs = Some("xfs".into());
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
                detected_fs = Some("NTFS".into());
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
                        detected_fs = Some(map.filesystem.to_string());
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
                                detected_fs = Some("ext4".into());
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
                                        detected_fs = Some("btrfs".into());
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
                                                detected_fs = Some("xfs".into());
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
    // Both families achieve TRUE removal (like Acronis) — a restored/mounted
    // volume shows no excluded file and reclaims its space — by different,
    // filesystem-appropriate means:
    //
    // * FAT32/exFAT — metadata patches: the raw directory tree is walked
    //   ourselves and the matched files' metadata is patched out of the image
    //   (directory entry deleted, allocation table / bitmap freed). The
    //   simple, fully-specified on-disk structures make this safe and testable.
    //
    // * NTFS/ReFS — two steps. First, CONTENT ZEROING (CLAUDE.md: "exclusions
    //   via FSCTL_GET_RETRIEVAL_POINTERS (clusters zeroed)"): the matched
    //   clusters are dropped from the used map and zeroed, so the file's DATA
    //   never leaves the machine. Then, after the copy, the mount-&-delete
    //   pass (`true_removal_win`, decision 2026-07-07) lets the FS DRIVER
    //   delete the catalog entries inside the image copy — we never parse or
    //   write NTFS/ReFS structures ourselves ("no NTFS parser", CLAUDE.md,
    //   still holds); the driver updates $MFT/$Bitmap/directory indexes (or
    //   the ReFS equivalents) correctly by construction. If that pass cannot
    //   run, the excluded files remain as zero-filled ghosts — a valid backup,
    //   just without the space reclaim.
    let mut excluded_files = 0u64;
    let mut excluded_bytes = 0u64;
    let mut zero_ranges: Option<ByteRanges> = None;
    // Byte-level metadata edits that make excluded FAT/exFAT files truly
    // disappear (delete the directory entry, free the allocation table). Empty
    // for NTFS/ReFS, which still use zeroing for now.
    let mut fat_patches: Vec<(u64, Vec<u8>)> = Vec::new();
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
    // Matched paths for the post-copy true-removal pass (NTFS/ReFS): the FS
    // driver deletes them inside the finished image, so they vanish from the
    // catalog and their space is reclaimed — see `true_removal_win`.
    let mut removal_paths: Vec<crate::exclusions_win::ExcludedPath> = Vec::new();
    if let Some(map) = ntfs_map.as_mut() {
        if !req.exclusion_patterns.is_empty() {
            let mut scan =
                collect_excluded_ranges(&read_path, map.cluster_size(), &req.exclusion_patterns, prepare_tick)?;
            excluded_files = scan.files_excluded;
            excluded_bytes = scan.bytes_excluded;
            categories = Some(scan.categories);
            removal_paths = std::mem::take(&mut scan.paths);
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
            // Metadata patches: delete the directory entries + free the FAT
            // chains so the files vanish instead of remaining zero-filled ghosts.
            fat_patches = scan.patches;
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

    // --- Create the image target (and attach it, for VHDX) ---
    // Disk = 1 MiB GPT head + volume + meta partition + tail for the backup GPT.
    let disk_size = DEFAULT_PARTITION_OFFSET + volume_len.div_ceil(MIB) * MIB + meta_size + MIB;
    let (mut target, data_offset) = match req.image_format {
        ImageFormat::Qcow2 => {
            // Userspace-written compressed image: no attach, no kernel writer.
            // A child references its parent by FILE NAME (qemu resolves
            // relative backing paths against the image's directory), so the
            // backup folder stays relocatable — same convention as the sidecar.
            let (backing_file, data_offset) = match &parent {
                None => (None, DEFAULT_PARTITION_OFFSET),
                Some((parent_path, parent_sidecar)) => {
                    // Same folder (the normal case) → bare file name; a parent
                    // elsewhere falls back to its absolute path.
                    let backing = if parent_path.parent() == req.image_path.parent() {
                        parent_path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .ok_or_else(|| {
                                OwnBackupError::Config(format!(
                                    "parent image path has no file name: {}",
                                    parent_path.display()
                                ))
                            })?
                    } else {
                        parent_path.to_string_lossy().into_owned()
                    };
                    (Some(backing), parent_sidecar.info.volume_data_offset)
                }
            };
            let writer = crate::qcow2::Qcow2StagedWriter::create(
                &req.image_path,
                crate::qcow2::Qcow2WriterOptions {
                    virtual_size: disk_size,
                    backing_file,
                    ..Default::default()
                },
            )?;
            (ImageTarget::Qcow2 { writer }, data_offset)
        }
        ImageFormat::Vhdx => {
            let data_offset = match &parent {
                None => {
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
            let writer = open_write_with_retry(attached.physical_path())?;
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
            (ImageTarget::Vhdx { attached, writer, bytes_since_flush: 0 }, data_offset)
        }
    };

    let disk_guid = match &parent {
        None => {
            // --- GPT with Basic Data partition + OwnBackupMeta partition ---
            let meta_partition_guid = Uuid::new_v4();
            let layout = GptLayout {
                disk_size: target.length(),
                sector_size,
                partition_offset: DEFAULT_PARTITION_OFFSET,
                partition_size: volume_len,
                disk_guid: Uuid::new_v4(),
                partition_guid: Uuid::new_v4(),
                partition_name: "OwnBackup".into(),
                meta_partition_size: Some(meta_size),
                meta_partition_guid: Some(meta_partition_guid),
            };
            for op in gpt_write_ops(&layout)? {
                target.write_at(op.offset, &op.bytes)?;
            }
            // VHDX only: let partmgr re-read the fresh GPT (the disk stays
            // offline, so no volumes surface), then persist the offline
            // attribute keyed to the new disk GUID — future attaches of this
            // image and of every differencing child arrive offline, closing
            // the auto-mount race for the whole chain. Best-effort: a failure
            // only means the next attach may transiently mount. (qcow2 images
            // are never attached, so there is nothing to persist.)
            if let ImageTarget::Vhdx { writer, .. } = &mut target {
                if let Err(e) = writer
                    .update_properties()
                    .and_then(|()| writer.set_offline_writable(true))
                {
                    warn!(error = %e, "could not persist the offline attribute for the new image");
                }
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
    // FAT/exFAT true-removal metadata patches (directory entries + allocation
    // table). Disjoint from the zeroed data clusters and from the NTFS boot-
    // sector patch below (which only fires for NTFS sources), so ordering is
    // immaterial — the patches win on their own byte ranges either way.
    if !fat_patches.is_empty() {
        info!(patches = fat_patches.len(), "Applying FAT/exFAT file-removal metadata patches");
        reader = Box::new(crate::block_reader::MultiPatchReader::new(
            reader,
            std::mem::take(&mut fat_patches),
        ));
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

    let outcome = copy_used_blocks(
        &backup_id.to_string(),
        reader.as_mut(),
        used.as_ref(),
        parent_hashes.as_ref(),
        |block_number, data| {
            target.write_at(data_offset + block_number * req.block_size as u64, data)
        },
        &mut progress,
    )?;

    // Pre-allocate the metadata partition blocks in the VHDX/qcow2 image
    // by writing the initial sidecar blob before finalizing (prevents zero-write optimization).
    let sector = sector_size as u64;
    let last_lba = (DEFAULT_PARTITION_OFFSET + volume_len).div_ceil(sector) - 1;
    let meta_offset = (last_lba + 1) * sector;

    // Construct a temporary initial sidecar to allocate the block
    let temp_manifest = ImageManifest {
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
            disk_guid: disk_guid.clone(),
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
        volume_encryption: None,
    };
    let temp_image_file_name = req.image_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut temp_info = match &parent {
        None => SidecarInfo::new_full(temp_image_file_name, data_offset),
        Some((parent_path, _)) => {
            let parent_name = parent_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let role = match kind {
                BackupType::Incremental => ChainRole::Incremental,
                _ => ChainRole::Differential,
            };
            SidecarInfo::new_child(temp_image_file_name, role, parent_name, data_offset)
        }
    };
    temp_info.container = req.image_format.extension().into();
    temp_info.job_id = req.job_id.clone();
    temp_info.source_disk_layout = req.source_disk.clone();
    let mut temp_blob = Vec::new();
    crate::sidecar::write_sidecar_v2_to(&mut temp_blob, &temp_info, &temp_manifest, &outcome.checksums)?;

    // Ensure the placeholder is at least meta_size to cover the entire partition space.
    let mut placeholder = vec![0u8; meta_size as usize];
    let placeholder_len = placeholder.len();
    let temp_blob_len = temp_blob.len();
    if temp_blob_len <= placeholder_len {
        placeholder[..temp_blob_len].copy_from_slice(&temp_blob);
    } else {
        placeholder.copy_from_slice(&temp_blob[..placeholder_len]);
    }
    // Set a non-zero byte at the end of the placeholder to ensure it spans the whole allocation
    if placeholder_len > 0 {
        placeholder[placeholder_len - 1] = 1;
    }
    target.write_at(meta_offset, &placeholder)?;

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
    // Timed step by step inside finalize(): the stall lives in ONE of the
    // steps (flush of remaining cache vs handle close vs detach for VHDX;
    // metadata write + sync for qcow2), and the log says which.
    target.finalize()?;

    // Notify VSS writers of success and release the shadow copy.
    let t = Instant::now();
    if let Some(session) = vss.take() {
        if let Err(e) = session.complete() {
            warn!(error = %e, "VSS BackupComplete failed (backup data is already safe)");
        }
    }
    info!(vss_release_ms = t.elapsed().as_millis(), "VSS released");

    // --- True removal of excluded files (NTFS/ReFS) ---
    // The FS driver deletes the recorded paths inside the image copy (the
    // source is never touched): catalog entries disappear and the space is
    // reclaimed on a restored volume. Failures before the merge fall back to
    // the zero-filled ghosts — still a valid backup; failures after the merge
    // fail the run (image/checksum mismatch must never be shipped). Checksums
    // of the driver-touched blocks are updated in place, so the sidecar below
    // describes the final image.
    let mut checksums = outcome.checksums;
    let mut used_blocks = used_blocks;
    if !removal_paths.is_empty() {
        let bytes_copied = outcome.bytes_copied;
        let mut tick = |entries: u64| {
            let _ = progress(&BackupProgress {
                backup_id: "finalize".into(),
                status: BackupStatus::Running,
                blocks_done: entries,
                blocks_total: 0,
                bytes_written: bytes_copied,
                speed_bps: 0,
            });
        };
        match req.image_format {
            ImageFormat::Vhdx => {
                // Differencing child + MergeVirtualDisk via the kernel VHDX driver.
                if crate::true_removal_win::remove_excluded_from_image(
                    &req.image_path,
                    &removal_paths,
                    data_offset,
                    volume_len,
                    req.block_size,
                    &mut checksums,
                    &mut tick,
                )?
                .is_some()
                {
                    used_blocks = checksums.len() as u64;
                }
            }
            ImageFormat::Qcow2 => {
                // RW-mount via AIM over a CoW overlay, delete, fold the touched
                // clusters back with Qcow2InPlaceMerge (Q-R3).
                if crate::qcow2_removal_win::remove_excluded_from_qcow2(
                    &req.image_path,
                    &removal_paths,
                    data_offset,
                    volume_len,
                    req.block_size,
                    Some(crate::qcow2::DEFAULT_ZSTD_LEVEL),
                    &mut checksums,
                    &mut tick,
                )?
                .is_some()
                {
                    used_blocks = checksums.len() as u64;
                }
            }
        }
    }

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
        compression_algo: match req.image_format {
            ImageFormat::Vhdx => CompressionAlgo::None,
            ImageFormat::Qcow2 => CompressionAlgo::Zstd,
        },
        compression_level: match req.image_format {
            ImageFormat::Vhdx => 0,
            ImageFormat::Qcow2 => crate::qcow2::DEFAULT_ZSTD_LEVEL,
        },
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
    sidecar_info.container = req.image_format.extension().into();
    // The job id is what ties images into this job's chains: list_job_chains
    // filters on it, so without it every next run plans a FULL backup.
    sidecar_info.job_id = req.job_id.clone();
    sidecar_info.source_disk_layout = req.source_disk.clone();
    sidecar_info.categories = categories;
    sidecar_info.excluded_files = excluded_files;
    sidecar_info.excluded_bytes = excluded_bytes;
    sidecar_info.filesystem = detected_fs;
    // The container's own header, kept so a restore can put the encryption
    // back exactly as it was (D19). Failing to read it is not fatal — the
    // copy is still a good copy — but it does mean this image can only be
    // restored decrypted, so it is logged loudly.
    let luks_headers: Vec<own_backup_common::image::LuksHeaderBackup> = match &luks {
        Some(p) => {
            match WinRawDevice::open_read(&read_path)
                .and_then(|mut d| crate::luks::read_header_area(&mut d, p))
            {
                Ok(bytes) => {
                    use base64::Engine;
                    info!(bytes = bytes.len(), "LUKS header captured — restore can re-encrypt");
                    vec![own_backup_common::image::LuksHeaderBackup {
                        part_index: 0,
                        data_offset: p.data_offset,
                        sector_size: p.sector_size,
                        kind: p.kind.clone(),
                        header_b64: base64::engine::general_purpose::STANDARD.encode(&bytes),
                    }]
                }
                Err(e) => {
                    warn!(error = %e, "could not read the LUKS header — this copy will only restore decrypted");
                    Vec::new()
                }
            }
        }
        None => Vec::new(),
    };
    sidecar_info.luks_headers = luks_headers;
    let sidecar_path = sidecar_path_for(&req.image_path);


    // M0: Write embedded sidecar v2 to the meta partition in the finished image file
    let sector = sector_size as u64;
    let last_lba = (DEFAULT_PARTITION_OFFSET + volume_len).div_ceil(sector) - 1;
    let meta_offset = (last_lba + 1) * sector;
    let mut blob_bytes = Vec::new();
    crate::sidecar::write_sidecar_v2_to(&mut blob_bytes, &sidecar_info, &manifest, &checksums)?;
    match req.image_format {
        ImageFormat::Vhdx => {
            crate::vhdx_bat::write_vhdx_guest_at(&req.image_path, meta_offset, &blob_bytes)?;
        }
        ImageFormat::Qcow2 => {
            crate::qcow2::merge::write_qcow2_guest_blob(&req.image_path, meta_offset, &blob_bytes)?;
        }
    }

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

// ===========================================================================
// Whole-disk backup: one image per disk (plan D1, .ai/whole-disk-single-image)
// ===========================================================================

/// One partition to include in a whole-disk backup.
pub struct DiskPartitionBackup {
    /// Source volume device (`\\.\Volume{…}`, a drive letter, or `part:…`).
    pub device: String,
    /// GPT slot (matches [`own_backup_common::gpt::GptPartitionEntry::index`]).
    pub part_index: u32,
    /// Passphrase for a LUKS partition (its decrypted data is captured).
    pub passphrase: Option<Zeroizing<String>>,
}

/// Parameters for a whole-disk (bootable) backup into a single image.
pub struct DiskBackupRequest {
    pub disk_number: u32,
    pub disk_model: String,
    pub partitions: Vec<DiskPartitionBackup>,
    pub image_path: PathBuf,
    pub block_size: u32,
    pub image_format: ImageFormat,
    pub job_id: Option<String>,
    /// File-exclusion glob patterns for Windows-family filesystems (NTFS/ReFS/
    /// FAT), applied per partition (plan D2): NTFS/ReFS drop + zero the matched
    /// clusters (data never leaves the machine; the catalog entry stays as a
    /// zero-filled ghost); FAT32/exFAT also patch the metadata so the files
    /// fully disappear.
    pub exclusion_patterns_windows: Vec<String>,
    /// File-exclusion masks for Linux filesystems (ext/btrfs/xfs). Selected per
    /// partition by filesystem; enforcement on ext/btrfs/xfs is a follow-up
    /// (stored/passed now, applied once the Linux-FS exclusion scanner lands).
    pub exclusion_patterns_linux: Vec<String>,
    /// Full / Incremental / Differential (plan D5). A non-full kind needs
    /// `parent_image` to be the previous whole-disk image of the SAME disk; if
    /// the chain turns out unusable (missing parent, geometry changed, format
    /// mismatch) the engine falls back to a full backup and reports it in the
    /// summary.
    pub kind: BackupType,
    /// Parent whole-disk image for an incremental (the previous version) or
    /// differential (the chain's full). A differencing child is written over it.
    pub parent_image: Option<PathBuf>,
    /// Full source-disk layout snapshot (from the GUI's live inventory) to embed
    /// in the disk manifest, so a bare-metal restore can identify the archived
    /// disk without jobs.json. `None` for CLI runs (the manifest then omits it).
    pub source_disk: Option<own_backup_common::disk::DiskInfo>,
}

/// Outcome of a completed whole-disk backup.
pub struct DiskBackupSummary {
    pub image_path: PathBuf,
    pub bytes_copied: u64,
    pub blocks_copied: u64,
    pub partitions: u32,
    pub duration: Duration,
    /// What was actually produced — may differ from the requested kind when a
    /// broken/incompatible parent forced a fallback to a full backup.
    pub backup_kind: BackupType,
    /// Used image blocks skipped because they matched the parent version.
    pub blocks_unchanged: u64,
    /// Files excluded across all partitions (plan D2).
    pub excluded_files: u64,
    /// Bytes their extents freed from the image.
    pub excluded_bytes: u64,
    /// Per-category byte breakdown of kept files across all partitions (for the
    /// dashboard); `None` when no exclusion walk ran on any partition.
    pub categories: Option<crate::exclusions::CategorySizes>,
}

/// One prepared partition: its VSS snapshot, decrypt params, used-map and the
/// GPT entry it fills. Snapshots for all partitions are taken up front so the
/// image is cross-volume consistent, then copied in one pass.
struct PreparedPartition {
    part_index: u32,
    device: String,
    read_path: String,
    luks: Option<crate::luks::LuksParams>,
    /// The container's own header, captured so a restore can put the
    /// encryption back (D19). `None` when the partition was not encrypted or
    /// the header could not be read.
    luks_header: Option<own_backup_common::image::LuksHeaderBackup>,
    used: Box<dyn UsedSectorMap>,
    volume_len: u64,
    part_offset: u64,
    filesystem: String,
    /// Excluded byte ranges (VOLUME-relative) and FAT/exFAT metadata patches for
    /// this partition (plan D2); empty when no exclusions apply.
    zero_ranges: ByteRanges,
    patches: Vec<(u64, Vec<u8>)>,
    /// NTFS/ReFS matched paths for the post-copy true-removal pass (plan D2).
    removal_paths: Vec<crate::exclusions_win::ExcludedPath>,
    #[allow(dead_code)]
    vss: Option<VssSnapshotSession>, // kept alive until the copy is done
}

/// Short display token for a source device (mirrors core's source_token):
/// a drive letter, `V`+8 hex of a volume GUID, or `P<disk><offsetHex>`.
fn disk_source_token(device: &str) -> String {
    let d = device.trim_start_matches(r"\\.\").trim_start_matches(r"\\?\");
    if let Some(rest) = d.strip_prefix("part:") {
        let mut it = rest.split(':');
        if let (Some(disk), Some(off)) = (it.next(), it.next()) {
            if let (Ok(dn), Ok(o)) = (disk.parse::<u32>(), off.parse::<u64>()) {
                return format!("P{dn}{:X}", o);
            }
        }
    }
    let bytes = d.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return bytes[0].to_ascii_uppercase().to_string();
    }
    if let Some(pos) = d.find("Volume{") {
        let hex: String = d[pos + 7..].chars().take(8).collect();
        return format!("V{}", hex.to_uppercase());
    }
    "X".into()
}

/// A prepared partition used-map plus its file-exclusion outcome (plan D2).
struct PreparedMap {
    /// Used-sector map (`None` → sector-by-sector). For NTFS/FAT with exclusions
    /// the excluded clusters are already dropped from this map.
    map: Option<Box<dyn UsedSectorMap>>,
    /// Detected filesystem name for the manifest.
    filesystem: String,
    /// Excluded byte ranges (VOLUME-relative, finalized): zeroed in the image
    /// copy so excluded file DATA never leaves the machine.
    zero_ranges: ByteRanges,
    /// FAT/exFAT metadata patches (VOLUME-relative offset → bytes): deleted
    /// directory entries + freed allocation-table chains, so excluded FAT files
    /// truly disappear instead of remaining zero-filled ghosts.
    patches: Vec<(u64, Vec<u8>)>,
    /// NTFS/ReFS matched paths for the post-copy true-removal pass (plan D2):
    /// the FS driver deletes them inside the finished image so the catalog
    /// entry disappears too (not just a zero-filled ghost). Empty for FAT
    /// (patched directly) and Linux filesystems.
    removal_paths: Vec<crate::exclusions_win::ExcludedPath>,
    pub excluded_files: u64,
    pub excluded_bytes: u64,
    /// Per-category byte breakdown of the KEPT files on this partition (powers
    /// the dashboard's data breakdown); `None` when no exclusion walk ran.
    categories: Option<crate::exclusions::CategorySizes>,
}

/// Build a partition's used-map and apply file exclusions (plan D2). The
/// filesystem is determined from the map that parses (over the DECRYPTED view
/// for a LUKS volume, so an unlocked Linux root reports "btrfs", not "unknown").
///
/// The exclusion mask set is chosen by filesystem family: `patterns_windows`
/// for NTFS/ReFS/FAT, `patterns_linux` for ext/btrfs/xfs. NTFS/ReFS drop + zero
/// the excluded clusters (the catalog entry survives as a zero-filled ghost —
/// whole-disk NTFS catalog true-removal is a later step); FAT32/exFAT also patch
/// the metadata so the files vanish. Enforcement on ext/btrfs/xfs is a follow-up
/// (no Linux-FS exclusion scanner yet), so Linux patterns are selected but not
/// yet acted on; LUKS-decrypted volumes are captured whole.
///
/// `tick` reports "prepare"-phase progress (and can cancel by returning `Err`).
fn prepare_partition_map(
    read_path: &str,
    block_size: u32,
    luks: &Option<crate::luks::LuksParams>,
    patterns_windows: &[String],
    patterns_linux: &[String],
    tick: impl FnMut(u64) -> Result<()>,
) -> PreparedMap {
    // NTFS/ReFS/FAT use the Windows masks; ext/btrfs/xfs use the Linux masks.
    let patterns = patterns_windows;
    let none = |fs: &str, map: Option<Box<dyn UsedSectorMap>>| PreparedMap {
        map,
        filesystem: fs.into(),
        zero_ranges: ByteRanges::new(),
        patches: Vec::new(),
        removal_paths: Vec::new(),
        excluded_files: 0,
        excluded_bytes: 0,
        categories: None,
    };
    let open = || -> Result<Box<dyn crate::fat_map::RawVolume + Send>> {
        let dev = WinRawDevice::open_read(read_path)?;
        Ok(match luks {
            Some(p) => Box::new(crate::decrypt::DecryptingVolume::new(Box::new(dev), p.clone())),
            None => Box::new(dev),
        })
    };
    // NTFS first (only on a plain, non-decrypted volume), then FAT/ext/btrfs/xfs.
    if luks.is_none() {
        if let Ok(mut m) = NtfsUsedSectorMap::open(read_path, block_size) {
            let mut out = none("ntfs", None);
            if !patterns.is_empty() {
                if let Ok(mut scan) = collect_excluded_ranges(read_path, m.cluster_size(), patterns, tick) {
                    out.excluded_files = scan.files_excluded;
                    out.excluded_bytes = scan.bytes_excluded;
                    out.categories = Some(scan.categories);
                    out.removal_paths = std::mem::take(&mut scan.paths);
                    if !scan.ranges.is_empty() {
                        m.apply_excluded_ranges(&scan.ranges);
                        out.zero_ranges = std::mem::take(&mut scan.ranges);
                    }
                } else {
                    warn!(part = %read_path, "whole-disk: NTFS exclusion scan failed — keeping all files");
                }
            }
            m.release_bitmap();
            out.map = Some(Box::new(m));
            return out;
        }
    }
    let Ok(mut src) = open() else { return none("unknown", None) };
    if let Ok(m) = crate::fat_map::build_fat_used_map(src.as_mut(), block_size) {
        let fs = m.filesystem.to_lowercase();
        let is_fat = m.filesystem == "FAT32" || m.filesystem == "exFAT";
        drop(src);
        if is_fat && !patterns.is_empty() {
            let mut out = none(&fs, None);
            if let Ok(mut dev) = WinRawDevice::open_read(read_path) {
                if let Ok(scan) = crate::fat_exclusions::scan_fat_exclusions(&mut dev, patterns, tick) {
                    out.excluded_files = scan.files_excluded;
                    out.excluded_bytes = scan.bytes_excluded;
                    out.categories = Some(scan.categories);
                    if !scan.ranges.is_empty() {
                        // Rebuild the map so excluded clusters drop out.
                        if let Ok(mut dev2) = WinRawDevice::open_read(read_path) {
                            if let Ok(rebuilt) = crate::fat_map::build_fat_used_map_excl(&mut dev2, block_size, Some(&scan.ranges)) {
                                out.map = Some(Box::new(rebuilt));
                            }
                        }
                        out.zero_ranges = scan.ranges;
                        out.patches = scan.patches;
                    }
                }
            }
            if out.map.is_none() {
                // Exclusion rebuild failed → fall back to the full FAT map.
                if let Ok(mut src2) = open() {
                    if let Ok(m2) = crate::fat_map::build_fat_used_map(src2.as_mut(), block_size) {
                        out.map = Some(Box::new(m2));
                    }
                }
            }
            return out;
        }
        return none(&fs, Some(Box::new(m)));
    }
    if let Ok(mut m) = crate::ext_map::build_ext_used_map(src.as_mut(), block_size) {
        // ext2/3/4: full file-level exclusions (Stage 1 drop+zero, Stage 2
        // metadata true-removal) — ext data blocks are exclusively owned, so
        // this is safe (unlike btrfs). Scanned over the DECRYPTED view for LUKS.
        drop(src);
        let mut out = none("ext4", None);
        if !patterns_linux.is_empty() {
            if let Ok(mut src2) = open() {
                match crate::ext_exclusions::scan_ext_exclusions(src2.as_mut(), patterns_linux, tick) {
                    Ok(scan) => {
                        out.excluded_files = scan.files_excluded;
                        out.excluded_bytes = scan.bytes_excluded;
                        out.categories = Some(scan.categories);
                        out.patches = scan.patches;
                        if !scan.ranges.is_empty() {
                            m.apply_excluded_ranges(&scan.ranges, block_size);
                            out.zero_ranges = scan.ranges;
                        }
                    }
                    Err(e) => warn!(part = %read_path, error = %e, "ext exclusions failed — keeping all files"),
                }
            }
        }
        out.map = Some(Box::new(m));
        return out;
    }
    if let Ok(mut m) = crate::btrfs_map::build_btrfs_used_map(src.as_mut(), block_size) {
        // btrfs: CoW-safe file exclusions. Only extents PROVEN exclusively
        // owned by matched files are zeroed + dropped; anything shared with a
        // snapshot or a reflink is kept. Files whose extents are all private
        // are removed from the FS tree outright (Stage 2).
        //
        // Stage 2 destroyed a copy once (2026-07-28): excluding dense subtrees
        // (/proc, /sys, /dev, /run, /var/cache) emptied whole FS-tree leaves,
        // which stayed in the tree with no items while their parents still
        // pointed at them. The restored system aborted its first mount with
        // `tree first key mismatch … has=(0,0,0)`; later, stale csum entries
        // over freed extents corrupted the tree at first write. Three guards
        // now stand in the way (FS-, extent- and csum-tree leaves: files whose
        // removal would empty or overflow a leaf are spared to Stage 1), freed
        // extents LOSE their csum entries (`btrfs_del_csums` contract), and if
        // a leaf still breaks, the whole Stage 2 is discarded. A failed scan
        // keeps every file — a bigger image is always better than a corrupt
        // one.
        drop(src);
        let mut out = none("btrfs", None);
        if !patterns_linux.is_empty() {
            if let Ok(mut src2) = open() {
                // `true` = Stage 2: excluded files are REALLY removed from the
                // catalog (product requirement 2026-07-28, CLAUDE.md). The
                // scanner spares anything it cannot remove safely (shared
                // extents, hardlinks, leaf-breaking edits) down to Stage 1,
                // and any Stage-2 failure keeps the whole volume on Stage 1 —
                // see `scan_btrfs_exclusions`.
                match crate::btrfs_exclusions::scan_btrfs_exclusions(src2.as_mut(), patterns_linux, true, tick) {
                    Ok(scan) => {
                        out.excluded_files = scan.files_excluded;
                        out.excluded_bytes = scan.bytes_excluded;
                        out.categories = Some(scan.categories);
                        out.patches = scan.patches;
                        if !scan.ranges.is_empty() {
                            m.apply_excluded_ranges(&scan.ranges, block_size);
                            out.zero_ranges = scan.ranges;
                        }
                    }
                    Err(e) => warn!(part = %read_path, error = %e, "btrfs exclusions failed — keeping all files"),
                }
            }
        }
        out.map = Some(Box::new(m));
        return out;
    }
    if let Ok(mut m) = crate::xfs_map::build_xfs_used_map(src.as_mut(), block_size) {
        // xfs: reflink-aware Stage-1 zeroing (data is not checksummed on xfs, so
        // no metadata edits) — only extents proven NOT shared (refcount==1) are
        // zeroed + dropped. Catalog true-removal (Stage 2) is a follow-up.
        drop(src);
        let mut out = none("xfs", None);
        if !patterns_linux.is_empty() {
            if let Ok(mut src2) = open() {
                match crate::xfs_exclusions::scan_xfs_exclusions(src2.as_mut(), patterns_linux, tick) {
                    Ok(scan) => {
                        out.excluded_files = scan.files_excluded;
                        out.excluded_bytes = scan.bytes_excluded;
                        out.categories = Some(scan.categories);
                        if !scan.ranges.is_empty() {
                            m.apply_excluded_ranges(&scan.ranges, block_size);
                            out.zero_ranges = scan.ranges;
                        }
                    }
                    Err(e) => warn!(part = %read_path, error = %e, "xfs exclusions failed — keeping all files"),
                }
            }
        }
        out.map = Some(Box::new(m));
        return out;
    }
    none("unknown", None)
}

/// One partition region inside the whole-disk image: its byte offset on the
/// disk, its decrypted length, and a reader for its (decrypted) volume data.
struct DiskRegion {
    part_offset: u64,
    volume_len: u64,
    reader: Box<dyn crate::fat_map::RawVolume + Send>,
    /// Excluded byte ranges (VOLUME-relative): zeroed after reading so excluded
    /// file data never enters the image (plan D2).
    zero_ranges: ByteRanges,
    /// FAT/exFAT metadata patches (VOLUME-relative offset → bytes): applied over
    /// the read data so excluded FAT files vanish from the directory/allocation.
    patches: Vec<(u64, Vec<u8>)>,
}

/// A [`BlockReader`] over the WHOLE disk image: block `N` (bytes `N*bs..`) is
/// assembled from the GPT write-ops and the partition regions at their real
/// offsets; bytes belonging to nothing read as zeros. This lets partitions sit
/// at any sector offset while the checksum grid stays the image's own blocks.
struct DiskCompositeReader {
    block_size: u32,
    image_size: u64,
    gpt_ops: Vec<(u64, Vec<u8>)>,
    regions: Vec<DiskRegion>,
}

impl BlockReader for DiskCompositeReader {
    fn read_block(&mut self, n: u64) -> Result<Vec<u8>> {
        let bs = self.block_size as u64;
        let start = n * bs;
        let len = bs.min(self.image_size - start) as usize;
        let end = start + len as u64;
        let mut buf = vec![0u8; len];
        // GPT bytes (protective MBR, primary/backup headers + entry arrays).
        for (off, bytes) in &self.gpt_ops {
            let g_end = off + bytes.len() as u64;
            let from = start.max(*off);
            let to = end.min(g_end);
            if from < to {
                buf[(from - start) as usize..(to - start) as usize]
                    .copy_from_slice(&bytes[(from - off) as usize..(to - off) as usize]);
            }
        }
        // Partition data (overwrites any overlap — partitions never overlap the
        // GPT head/tail in a valid layout, so order is immaterial).
        for r in &mut self.regions {
            let p_end = r.part_offset + r.volume_len;
            let from = start.max(r.part_offset);
            let to = end.min(p_end);
            if from < to {
                let vol_off = from - r.part_offset;
                let dst = &mut buf[(from - start) as usize..(to - start) as usize];
                r.reader.read_at(vol_off, dst)?;
                // Plan D2 — content zeroing: excluded file clusters read as zero
                // so their data never enters the image.
                r.zero_ranges.zero_overlaps(vol_off, dst);
                // FAT/exFAT metadata patches (deleted dir entries / freed
                // chains), applied over the same volume-relative window.
                let dst_end = vol_off + dst.len() as u64;
                for (poff, bytes) in &r.patches {
                    let p_start = *poff;
                    let p_end2 = poff + bytes.len() as u64;
                    let ov_start = vol_off.max(p_start);
                    let ov_end = dst_end.min(p_end2);
                    if ov_start < ov_end {
                        let d0 = (ov_start - vol_off) as usize;
                        let d1 = (ov_end - vol_off) as usize;
                        let b0 = (ov_start - p_start) as usize;
                        let b1 = b0 + (d1 - d0);
                        dst[d0..d1].copy_from_slice(&bytes[b0..b1]);
                    }
                }
            }
        }
        Ok(buf)
    }
    fn block_size(&self) -> u32 {
        self.block_size
    }
    fn total_blocks(&self) -> u64 {
        self.image_size.div_ceil(self.block_size as u64)
    }
}

/// Whole-disk used-map: a disk-image block is "used" if it overlaps a used
/// block of any partition (straddling blocks count once).
struct DiskUsedMap {
    used: Vec<bool>,
    count: u64,
}

impl DiskUsedMap {
    fn build(prepared: &[PreparedPartition], bs: u64, total_blocks: u64) -> Self {
        let mut used = vec![false; total_blocks as usize];
        for prep in prepared {
            let part_end = prep.part_offset + prep.volume_len;
            let vblocks = prep.volume_len.div_ceil(bs);
            for vb in 0..vblocks {
                if prep.used.is_block_used(vb) {
                    let d0 = prep.part_offset + vb * bs;
                    let d1 = part_end.min(d0 + bs); // this volume block's disk byte span
                    for db in d0 / bs..=(d1 - 1) / bs {
                        if let Some(slot) = used.get_mut(db as usize) {
                            *slot = true;
                        }
                    }
                }
            }
        }
        let count = used.iter().filter(|&&u| u).count() as u64;
        Self { used, count }
    }
}

impl UsedSectorMap for DiskUsedMap {
    fn is_block_used(&self, block_number: u64) -> bool {
        self.used.get(block_number as usize).copied().unwrap_or(false)
    }
    fn used_block_count(&self) -> u64 {
        self.count
    }
}

/// Back up a whole disk (several partitions of one physical drive) into ONE
/// bootable image with the real GPT + an embedded restore manifest. Used
/// sectors only (never a raw disk dump); each partition's data lands at its
/// real offset, so mounting the image shows every volume with a drive letter.
///
/// Scope: full + incremental/differential chains (plan D5); NTFS/FAT/ext/btrfs/
/// xfs + LUKS. File exclusions (plan D2) drop + zero excluded clusters per
/// partition, and patch FAT/exFAT metadata; NTFS/ReFS catalog true-removal
/// inside the whole-disk image (mount-&-delete across volumes) is not yet done —
/// excluded NTFS files remain as zero-filled ghosts (data still never leaves).
pub fn backup_disk(
    req: &DiskBackupRequest,
    progress: impl FnMut(&BackupProgress) -> Result<()>,
) -> Result<DiskBackupSummary> {
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
    crate::privileges_win::enable_backup_privileges();
    let result = backup_disk_inner(req, progress);
    if result.is_err() {
        let _ = std::fs::remove_file(&req.image_path);
    }
    result
}

fn backup_disk_inner(
    req: &DiskBackupRequest,
    mut progress: impl FnMut(&BackupProgress) -> Result<()>,
) -> Result<DiskBackupSummary> {
    let started = Instant::now();
    let bs = req.block_size as u64;

    // 1. Real GPT, filtered to the selected partitions (variant B keeps MSR).
    let mut layout = crate::gpt_reader::read_disk_layout(req.disk_number)?;
    let kept: std::collections::HashSet<u32> =
        req.partitions.iter().map(|p| p.part_index).collect();
    layout.retain_for_backup(&kept);
    let sector = layout.sector_size as u64;
    if bs == 0 || bs % sector != 0 {
        return Err(OwnBackupError::Config(format!(
            "block size {} is not a multiple of the sector size {sector}",
            req.block_size
        )));
    }

    // 2. Snapshot + map every partition up front (cross-volume consistency).
    info!(disk = req.disk_number, parts = req.partitions.len(), "whole-disk backup: preparing partitions");
    let mut prepared: Vec<PreparedPartition> = Vec::new();
    let mut excluded_files = 0u64;
    let mut excluded_bytes = 0u64;
    let mut categories: Option<crate::exclusions::CategorySizes> = None;
    for p in &req.partitions {
        let entry = layout
            .partitions
            .iter()
            .find(|e| e.index == p.part_index)
            .ok_or_else(|| {
                OwnBackupError::Config(format!("partition {} not found in the disk GPT", p.part_index))
            })?;
        // Partitions sit at their REAL disk offset (sector-aligned, not
        // necessarily block-aligned — a BOOT partition may start at LBA 34). The
        // disk-composite reader below copies whole-disk blocks, so any offset is
        // fine; the checksum block grid is the image itself.
        let part_offset = entry.first_lba * sector;

        let source = normalize_device_path(&p.device);
        // VSS for a live lettered volume; else read the source directly.
        let mut vss: Option<VssSnapshotSession> = None;
        let read_path = match volume_root_of(&source) {
            Some(root) => match VssSnapshotSession::create(&root) {
                Ok(session) => {
                    let path = session.device_path().to_string();
                    vss = Some(session);
                    path
                }
                Err(e) => {
                    warn!(error = %e, device = %source, "VSS failed — reading the live volume");
                    source.clone()
                }
            },
            None => source.clone(),
        };

        // LUKS unlock (decrypted data area is captured).
        let mut probe = WinRawDevice::open_read(&read_path)?;
        let luks = match &p.passphrase {
            Some(pass) => {
                let mut header = vec![0u8; 4096];
                probe.read_at(0, &mut header)?;
                if crate::luks::is_luks(&header) {
                    Some(crate::luks::open_luks(&mut probe, pass.as_bytes())?)
                } else {
                    None
                }
            }
            None => None,
        };
        let device_len = probe.length();
        drop(probe);
        // The container's own header, kept verbatim so a restore can rebuild
        // it byte-for-byte (D19). A failure here costs the ability to restore
        // this partition encrypted, not the backup itself, so it only warns.
        let luks_header = luks.as_ref().and_then(|l| {
            match WinRawDevice::open_read(&read_path)
                .and_then(|mut d| crate::luks::read_header_area(&mut d, l))
            {
                Ok(bytes) => {
                    use base64::Engine;
                    info!(part = p.part_index, bytes = bytes.len(), "LUKS header captured");
                    Some(own_backup_common::image::LuksHeaderBackup {
                        part_index: p.part_index,
                        data_offset: l.data_offset,
                        sector_size: l.sector_size,
                        kind: l.kind.clone(),
                        header_b64: base64::engine::general_purpose::STANDARD.encode(&bytes),
                    })
                }
                Err(e) => {
                    warn!(part = p.part_index, error = %e,
                          "could not read the LUKS header — this partition will only restore decrypted");
                    None
                }
            }
        });
        let volume_len = match &luks {
            Some(l) => device_len.saturating_sub(l.data_offset),
            None => device_len,
        };
        // The volume must fit the GPT slot it fills.
        let slot_len = (entry.last_lba + 1 - entry.first_lba) * sector;
        if volume_len > slot_len {
            return Err(OwnBackupError::Config(format!(
                "partition {} volume ({volume_len} B) exceeds its GPT slot ({slot_len} B)",
                p.part_index
            )));
        }

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
        let pm = prepare_partition_map(
            &read_path,
            req.block_size,
            &luks,
            &req.exclusion_patterns_windows,
            &req.exclusion_patterns_linux,
            prepare_tick,
        );
        let used = pm.map.unwrap_or_else(|| Box::new(AllUsedMap::new(volume_len.div_ceil(bs))));
        excluded_files += pm.excluded_files;
        excluded_bytes += pm.excluded_bytes;
        if let Some(c) = &pm.categories {
            categories.get_or_insert_with(Default::default).merge(c);
        }
        info!(
            part = p.part_index,
            fs = %pm.filesystem,
            used_blocks = used.used_block_count(),
            excluded_files = pm.excluded_files,
            offset = part_offset,
            "whole-disk backup: partition prepared"
        );
        prepared.push(PreparedPartition {
            part_index: p.part_index,
            device: source,
            read_path,
            luks,
            volume_len,
            part_offset,
            filesystem: pm.filesystem,
            luks_header,
            zero_ranges: pm.zero_ranges,
            patches: pm.patches,
            removal_paths: pm.removal_paths,
            used,
            vss,
        });
    }

    // 3a. Parent chain (plan D5). A non-full kind writes a differencing CHILD
    // over the previous whole-disk image; a broken/incompatible parent falls
    // back to a full backup (reported in the summary) rather than failing.
    let mut kind = req.kind;
    let mut parent: Option<(PathBuf, Sidecar)> = None;
    if kind != BackupType::Full {
        match &req.parent_image {
            None => {
                warn!("no parent image supplied for a {kind} whole-disk backup — creating a full backup");
                kind = BackupType::Full;
            }
            Some(pp) => match validate_disk_parent(pp, req.block_size, req.image_format) {
                Ok(sc) => parent = Some((pp.clone(), sc)),
                Err(e) => {
                    warn!(parent = %pp.display(), error = %e,
                        "whole-disk parent chain unusable — creating a full backup instead");
                    kind = BackupType::Full;
                }
            },
        }
    }

    // 3b. Size the image from the used-block total, plan the meta partition.
    // Build the whole-disk used-map FIRST (over the partitions region) so the
    // meta partition is sized from the actual disk-block count: a partition not
    // aligned to the block size straddles two disk-blocks per volume block, so
    // the count can exceed the sum of per-volume used counts.
    let part_end = prepared.iter().map(|p| p.part_offset + p.volume_len).max().unwrap_or(0);
    let part_end_blocks = part_end.div_ceil(bs);
    let disk_used = DiskUsedMap::build(&prepared, bs, part_end_blocks);
    // A child inherits the parent's virtual size and GPT (the differencing image
    // resolves them through the chain), so its geometry MUST match the parent
    // exactly. Size the meta partition from the geometry the parent committed to
    // (its full-state used count) — not this run's count, which for an
    // incremental differs. If the resulting plan no longer matches the parent's
    // size (the disk was repartitioned), fall back to a full backup.
    // Stored container headers (D19) are megabytes each, so the reserve has to
    // include them. A child takes the PARENT's reserve, for the same reason it
    // takes the parent's block count: its geometry must match exactly.
    let own_reserve = crate::sidecar::luks_reserve_for(
        prepared.iter().filter_map(|p| p.luks_header.as_ref().map(|h| h.data_offset)),
    );
    let full_used = |used: u64, reserve: u64| {
        crate::sidecar::calculate_meta_partition_size(used.max(1), reserve)
    };
    let mut meta_size = match &parent {
        Some((_, sc)) => full_used(
            sc.manifest.used_blocks,
            crate::sidecar::luks_headers_reserve(&sc.info.luks_headers),
        ),
        None => full_used(disk_used.count, own_reserve),
    };
    // A chain started before container headers were stored has no room for one.
    // The child cannot grow the image — its geometry is the parent's — so the
    // only honest move is to start a new full backup rather than run for an
    // hour and fail at the metadata write.
    if let Some((pp, sc)) = &parent {
        let parent_reserve = crate::sidecar::luks_headers_reserve(&sc.info.luks_headers);
        if own_reserve > parent_reserve {
            warn!(
                parent = %pp.display(),
                parent_reserve,
                own_reserve,
                "parent image has no room for the container header — creating a full backup instead"
            );
            kind = BackupType::Full;
            parent = None;
            meta_size = full_used(disk_used.count, own_reserve);
        }
    }
    let mut plan = own_backup_common::gpt::plan_whole_disk_image(&layout, meta_size);
    if let Some((pp, sc)) = &parent {
        if plan.image_size != sc.manifest.source_disk.size_bytes {
            warn!(
                parent = %pp.display(),
                planned = plan.image_size,
                parent_size = sc.manifest.source_disk.size_bytes,
                "whole-disk parent geometry changed — creating a full backup instead"
            );
            kind = BackupType::Full;
            parent = None;
            meta_size = full_used(disk_used.count, own_reserve);
            plan = own_backup_common::gpt::plan_whole_disk_image(&layout, meta_size);
        }
    }
    let meta_offset = plan.meta_first_lba * sector;
    let meta_guid = Uuid::new_v4();

    // 4. Create the image and (for a full) write its GPT. The image GPT gets
    //    FRESH random disk + partition GUIDs, NEVER the source disk's — a
    //    byte-clone of the live disk's GUIDs collides with it (Windows offlines
    //    one of the two, and if the destination lives on the source disk the
    //    whole disk, incl. the backup drive, vanishes: the 2026-07-11 E:
    //    incident). The REAL GUIDs are kept in the manifest below; restore
    //    writes them back (bootable).
    let mut safe_layout = layout.clone();
    safe_layout.disk_guid = Uuid::new_v4().to_string().to_uppercase();
    for p in &mut safe_layout.partitions {
        p.part_guid = Uuid::new_v4().to_string().to_uppercase();
    }
    // The GPT write-op grid (offsets + lengths) is deterministic from the plan
    // and partition count — the GUID VALUES only change the bytes. A child
    // reuses the PARENT's exact GPT bytes (read below) so any used block that
    // overlaps the GPT head (a partition starting at LBA 34) matches the parent
    // and is not needlessly rewritten; a full writes its own fresh GPT.
    let mut gpt_ops: Vec<(u64, Vec<u8>)> =
        crate::gpt_writer::whole_disk_gpt_ops(&safe_layout, &plan, meta_guid)?
            .into_iter()
            .map(|op| (op.offset, op.bytes))
            .collect();

    let (mut target, sector_size) = match &parent {
        None => create_disk_image_target(&req.image_path, plan.image_size, req.image_format)?,
        Some((pp, _)) => {
            // Child inherits the parent's GPT through the chain — overwrite the
            // op bytes with the parent's actual GPT so the composite reader
            // reproduces them byte-for-byte. Drop the parent read handle before
            // creating the differencing child (VHDX CreateVirtualDisk opens the
            // parent itself).
            {
                let mut pimg = ImageReadSource::open(pp, &req.image_format.extension())?;
                for (off, bytes) in &mut gpt_ops {
                    pimg.read_at(*off, bytes)?;
                }
            }
            create_disk_child_target(&req.image_path, pp, plan.image_size, req.image_format)?
        }
    };
    if parent.is_none() {
        // Full backup: lay down the fresh GPT — but ONLY into blocks the copy
        // pass will NOT write. A partition that begins inside the first block
        // (LBA 34, offset 17408) makes block 0 "used"; the composite copy then
        // writes that block with the GPT bytes already baked in, so writing the
        // GPT op there too would hit the same host cluster twice — fatal for the
        // write-once qcow2 writer ("cluster 0 written twice") and merely
        // redundant for VHDX. Unused GPT regions (an aligned primary GPT, the
        // backup GPT at the image tail) still need this direct write.
        for (off, bytes) in &gpt_ops {
            let mut p = 0usize;
            while p < bytes.len() {
                let abs = off + p as u64;
                let block = abs / bs;
                let take = (((block + 1) * bs - abs) as usize).min(bytes.len() - p);
                if !disk_used.is_block_used(block) {
                    target.write_at(abs, &bytes[p..p + take])?;
                }
                p += take;
            }
        }
        if let ImageTarget::Vhdx { writer, .. } = &mut target {
            // Persist the offline attribute keyed to the image's OWN (random)
            // disk GUID so future attaches arrive offline — safe now that the
            // GUID is not the source disk's. Children inherit it via the chain.
            if let Err(e) = writer.update_properties().and_then(|()| writer.set_offline_writable(true)) {
                warn!(error = %e, "could not persist the offline attribute for the disk image");
            }
        }
    }

    // 5. Copy the disk as whole-image blocks (block N = image offset N*bs). Each
    //    block is assembled from the GPT and the partitions at their real
    //    offsets, so partitions need no block alignment and the checksum grid is
    //    the image itself (block_number * bs = disk offset — verifiable).
    //    `disk_used` (built above) returns false past the partitions region, so
    //    the meta/tail blocks (written separately) are not copied.
    let mut regions: Vec<DiskRegion> = Vec::new();
    for prep in &prepared {
        let dev = WinRawDevice::open_read(&prep.read_path)?;
        let reader: Box<dyn crate::fat_map::RawVolume + Send> = match &prep.luks {
            Some(l) => Box::new(crate::decrypt::DecryptingVolume::new(Box::new(dev), l.clone())),
            None => Box::new(dev),
        };
        regions.push(DiskRegion {
            part_offset: prep.part_offset,
            volume_len: prep.volume_len,
            reader,
            zero_ranges: prep.zero_ranges.clone(),
            patches: prep.patches.clone(),
        });
    }
    let mut composite = DiskCompositeReader {
        block_size: req.block_size,
        image_size: plan.image_size,
        gpt_ops,
        regions,
    };
    // For a child only blocks that differ from the parent version are written;
    // the rest resolve through the differencing chain. `outcome.checksums` still
    // covers EVERY used block, so the child's sidecar describes the full image.
    let parent_hashes: Option<HashMap<u64, u64>> = parent.as_ref().map(|(_, sc)| {
        sc.checksums.iter().map(|c| (c.block_number, c.checksum)).collect()
    });
    let outcome = copy_used_blocks(
        &format!("disk{}", req.disk_number),
        &mut composite,
        &disk_used,
        parent_hashes.as_ref(),
        |block, data| target.write_at(block * bs, data),
        &mut progress,
    )?;
    let mut checksums = outcome.checksums;
    let bytes_copied = outcome.bytes_copied;
    let blocks_copied = outcome.blocks_copied;
    let blocks_unchanged = outcome.blocks_unchanged;

    // 6. Reserve the meta partition region while the image is open so the VHDX
    //    BAT physically allocates EVERY block of it (an all-zero write is
    //    optimised away → the block stays ZERO/unallocated and the later
    //    guest-write of the real blob fails with "block state 2"). A non-zero
    //    fill across the whole region forces allocation; the guest-write below
    //    then overwrites it with the actual sidecar. Then finalize + release.
    let placeholder = vec![0x01u8; meta_size as usize];
    target.write_at(meta_offset, &placeholder)?;
    let _ = progress(&BackupProgress {
        backup_id: "finalize".into(),
        status: BackupStatus::Running,
        blocks_done: 0,
        blocks_total: 0,
        bytes_written: bytes_copied,
        speed_bps: 0,
    });
    target.finalize()?;
    for prep in &mut prepared {
        if let Some(session) = prep.vss.take() {
            if let Err(e) = session.complete() {
                warn!(error = %e, "VSS release failed (data is already safe)");
            }
        }
    }

    // 6b. True removal of excluded NTFS/ReFS files inside the finished image
    //     (plan D2, both formats, full AND incremental). Mount the whole-disk
    //     image once — an incremental is a differencing child, so the temp child
    //     the removal pass creates over it resolves the WHOLE chain and every
    //     partition's files are visible — let the FS driver delete each
    //     partition's recorded paths on its own volume, then merge into THIS
    //     image's leaf + re-checksum the touched blocks. So excluded files vanish
    //     from the catalog instead of remaining zero-filled ghosts. Same graceful
    //     contract as the per-volume path: a pre-merge failure keeps the ghosts
    //     (valid backup), only a post-merge failure fails the run — cannot break
    //     a working backup. The content is already zeroed either way, so no
    //     excluded data leaks even when this pass is skipped.
    let removal: Vec<(u64, Vec<crate::exclusions_win::ExcludedPath>)> = prepared
        .iter_mut()
        .filter(|p| !p.removal_paths.is_empty())
        .map(|p| (p.part_offset, std::mem::take(&mut p.removal_paths)))
        .collect();
    if !removal.is_empty() {
        let mut tick = |entries: u64| {
            let _ = progress(&BackupProgress {
                backup_id: "finalize".into(),
                status: BackupStatus::Running,
                blocks_done: entries,
                blocks_total: 0,
                bytes_written: bytes_copied,
                speed_bps: 0,
            });
        };
        match req.image_format {
            ImageFormat::Vhdx => {
                crate::true_removal_win::remove_excluded_from_disk_image(
                    &req.image_path,
                    &removal,
                    plan.image_size,
                    req.block_size,
                    &mut checksums,
                    &mut tick,
                )?;
            }
            ImageFormat::Qcow2 => {
                crate::qcow2_removal_win::remove_excluded_from_disk_image_qcow2(
                    &req.image_path,
                    &removal,
                    plan.image_size,
                    req.block_size,
                    Some(crate::qcow2::DEFAULT_ZSTD_LEVEL),
                    &mut checksums,
                    &mut tick,
                )?;
            }
        }
    }

    // 7. Build the embedded manifest + sidecar and write it into the meta region.
    let image_file_name = req
        .image_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let images: Vec<own_backup_common::gpt::DiskImageRef> = prepared
        .iter()
        .map(|p| own_backup_common::gpt::DiskImageRef {
            part_index: p.part_index,
            token: disk_source_token(&p.device),
            image_file_name: image_file_name.clone(),
            filesystem: p.filesystem.clone(),
        })
        .collect();
    let disk_manifest = own_backup_common::gpt::DiskBackupManifest {
        format_version: own_backup_common::gpt::DiskBackupManifest::FORMAT_VERSION,
        created_at: chrono::Utc::now().to_rfc3339(),
        job_id: req.job_id.clone(),
        disk_number: req.disk_number,
        disk_model: req.disk_model.clone(),
        layout: layout.clone(),
        images,
        source_disk: req.source_disk.clone(),
    };

    let total_blocks = plan.image_size.div_ceil(bs);
    // `used_blocks` is the FULL image state (every checksummed block), not just
    // this run's writes — the next child sizes its meta partition from it, so it
    // must stay stable across the chain (checksums.len() == disk_used.count).
    let manifest = whole_disk_image_manifest(
        &layout,
        req,
        sector_size,
        plan.image_size,
        total_blocks,
        checksums.len() as u64,
        kind,
        parent.as_ref().map(|(_, sc)| sc.manifest.backup_id),
    );
    // A whole-disk image is disk-block absolute → volume_data_offset = 0.
    let mut info = match &parent {
        None => SidecarInfo::new_full(image_file_name, 0),
        Some((pp, _)) => {
            let role = match kind {
                BackupType::Differential => ChainRole::Differential,
                _ => ChainRole::Incremental,
            };
            let parent_name = pp
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            SidecarInfo::new_child(image_file_name, role, parent_name, 0)
        }
    };
    info.container = req.image_format.extension().into();
    info.job_id = req.job_id.clone();
    info.source_disk_layout = req.source_disk.clone();
    info.categories = categories;
    info.excluded_files = excluded_files;
    info.excluded_bytes = excluded_bytes;
    // One captured container header per encrypted partition (D19): with them a
    // restore rebuilds the container byte-for-byte, so the restored system's
    // crypttab and initramfs keep working untouched.
    info.luks_headers = prepared.iter().filter_map(|p| p.luks_header.clone()).collect();

    let mut blob = Vec::new();
    crate::sidecar::write_sidecar_v2_full_to(&mut blob, &info, &manifest, &checksums, Some(&disk_manifest))?;
    match req.image_format {
        ImageFormat::Vhdx => crate::vhdx_bat::write_vhdx_guest_at(&req.image_path, meta_offset, &blob)?,
        ImageFormat::Qcow2 => {
            crate::qcow2::merge::write_qcow2_guest_blob(&req.image_path, meta_offset, &blob)?
        }
    }

    info!(
        image = %req.image_path.display(),
        disk = req.disk_number,
        kind = %kind,
        partitions = prepared.len(),
        blocks_written = blocks_copied,
        blocks_unchanged,
        bytes = bytes_copied,
        secs = started.elapsed().as_secs(),
        "Whole-disk backup complete"
    );
    Ok(DiskBackupSummary {
        image_path: req.image_path.clone(),
        bytes_copied,
        blocks_copied,
        partitions: prepared.len() as u32,
        duration: started.elapsed(),
        backup_kind: kind,
        blocks_unchanged,
        excluded_files,
        excluded_bytes,
        categories,
    })
}

/// Validate that `parent_image` can serve as the parent of a new whole-disk
/// child: it exists, is itself a whole-disk image (carries an embedded disk
/// manifest), uses the same block size and container format. The image-size
/// (geometry) match is checked by the caller once the child's plan is known.
/// Returns the parent's full sidecar (its checksums seed the incremental skip).
fn validate_disk_parent(
    parent_image: &Path,
    block_size: u32,
    format: ImageFormat,
) -> Result<Sidecar> {
    if !parent_image.exists() {
        return Err(OwnBackupError::Config(format!(
            "parent whole-disk image not found: {}",
            parent_image.display()
        )));
    }
    let ext = parent_image
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase();
    let sidecar = read_embedded_sidecar(parent_image, &ext)?;
    if sidecar.disk_manifest.is_none() {
        return Err(OwnBackupError::Config(format!(
            "parent {} is not a whole-disk image (no embedded disk manifest)",
            parent_image.display()
        )));
    }
    if sidecar.manifest.block_size != block_size {
        return Err(OwnBackupError::Config(format!(
            "parent block size {} != requested {block_size}",
            sidecar.manifest.block_size
        )));
    }
    if sidecar.info.container != format.extension() {
        return Err(OwnBackupError::Config(format!(
            "parent image is '{}' but the requested format is '{}' — a backup \
             chain cannot change container format",
            sidecar.info.container,
            format.extension()
        )));
    }
    Ok(sidecar)
}

/// Create a differencing CHILD target over `parent_path` for an incremental /
/// differential whole-disk backup. The child inherits the parent's virtual size
/// and its GPT (resolved through the chain), so no GPT is written here.
/// `image_size` is the parent's virtual size (used only to size the qcow2 child).
fn create_disk_child_target(
    image_path: &Path,
    parent_path: &Path,
    image_size: u64,
    format: ImageFormat,
) -> Result<(ImageTarget, u32)> {
    let sector_size = 512u32;
    match format {
        ImageFormat::Qcow2 => {
            // Reference the parent by bare file name when it sits in the same
            // folder (qemu resolves relative backing paths against the image's
            // directory) — same convention as the per-volume path.
            let backing = if parent_path.parent() == image_path.parent() {
                parent_path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .ok_or_else(|| {
                        OwnBackupError::Config(format!(
                            "parent image path has no file name: {}",
                            parent_path.display()
                        ))
                    })?
            } else {
                parent_path.to_string_lossy().into_owned()
            };
            let writer = crate::qcow2::Qcow2StagedWriter::create(
                image_path,
                crate::qcow2::Qcow2WriterOptions {
                    virtual_size: image_size,
                    backing_file: Some(backing),
                    ..Default::default()
                },
            )?;
            Ok((ImageTarget::Qcow2 { writer }, sector_size))
        }
        ImageFormat::Vhdx => {
            create_differencing_vhdx(image_path, parent_path)?;
            // The child arrives offline (the chain's GPT GUID carries a persisted
            // offline attribute), so raw sector writes are not blocked by an
            // auto-mounted filesystem; persist it for the child too.
            let san_guard = SanPolicyGuard::offline_all();
            let attached = attach_vhdx(image_path, AttachMode::RawWrite)?;
            let writer = open_write_with_retry(attached.physical_path())?;
            writer.set_offline_writable(true)?;
            drop(san_guard);
            Ok((ImageTarget::Vhdx { attached, writer, bytes_since_flush: 0 }, sector_size))
        }
    }
}

/// Create an image target of `disk_size` bytes with no parent (full backup).
/// Returns the target and its logical sector size.
fn create_disk_image_target(
    image_path: &Path,
    disk_size: u64,
    format: ImageFormat,
) -> Result<(ImageTarget, u32)> {
    let sector_size = 512u32;
    match format {
        ImageFormat::Qcow2 => {
            let writer = crate::qcow2::Qcow2StagedWriter::create(
                image_path,
                crate::qcow2::Qcow2WriterOptions { virtual_size: disk_size, backing_file: None, ..Default::default() },
            )?;
            Ok((ImageTarget::Qcow2 { writer }, sector_size))
        }
        ImageFormat::Vhdx => {
            create_dynamic_vhdx(
                image_path,
                &VhdxCreateOptions {
                    max_size: disk_size,
                    logical_sector_size: sector_size,
                    physical_sector_size: std::cmp::max(sector_size, 4096),
                },
            )?;
            let san_guard = SanPolicyGuard::offline_all();
            let attached = attach_vhdx(image_path, AttachMode::RawWrite)?;
            let writer = open_write_with_retry(attached.physical_path())?;
            writer.set_offline_writable(false)?;
            drop(san_guard);
            if writer.length() < disk_size {
                return Err(OwnBackupError::Vhdx(format!(
                    "attached image is {} bytes — smaller than the planned {disk_size}",
                    writer.length()
                )));
            }
            Ok((ImageTarget::Vhdx { attached, writer, bytes_since_flush: 0 }, sector_size))
        }
    }
}

/// Build the embedded [`ImageManifest`] describing the whole-disk image.
#[allow(clippy::too_many_arguments)]
fn whole_disk_image_manifest(
    layout: &own_backup_common::gpt::DiskLayout,
    req: &DiskBackupRequest,
    sector_size: u32,
    image_size: u64,
    total_blocks: u64,
    used_blocks: u64,
    kind: BackupType,
    parent_backup_id: Option<Uuid>,
) -> ImageManifest {
    ImageManifest {
        backup_id: Uuid::new_v4(),
        created_at: chrono::Utc::now().to_rfc3339(),
        backup_type: kind,
        parent_backup_id,
        source_os: "windows".into(),
        source_disk: DiskInfo {
            device_id: format!(r"\\.\PhysicalDrive{}", req.disk_number),
            model: req.disk_model.clone(),
            serial: String::new(),
            size_bytes: image_size,
            sector_size_logical: sector_size,
            sector_size_physical: std::cmp::max(sector_size, 4096),
            media_type: "Disk".into(),
            partition_table: PartitionTable::Gpt,
            disk_guid: layout.disk_guid.clone(),
            partitions: vec![],
        },
        boot_mode: BootMode::Uefi,
        partition_table: PartitionTable::Gpt,
        compression_algo: match req.image_format {
            ImageFormat::Vhdx => CompressionAlgo::None,
            ImageFormat::Qcow2 => CompressionAlgo::Zstd,
        },
        compression_level: match req.image_format {
            ImageFormat::Vhdx => 0,
            ImageFormat::Qcow2 => crate::qcow2::DEFAULT_ZSTD_LEVEL,
        },
        encryption_algo: None,
        encryption_salt: None,
        block_size: req.block_size,
        total_blocks,
        used_blocks,
        backup_mode: BackupMode::UsedSectors,
        checksum_algorithm: "xxhash64".into(),
        preflight_report: None,
        volume_encryption: None,
    }
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

    let mut image_dev = ImageReadSource::open(&req.image_path, &sidecar.info.container)?;

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
    progress: impl FnMut(&DiskRestoreProgress) -> Result<()>,
) -> Result<DiskRestoreSummary> {
    // A single whole-disk image (plan D1) carries its GPT + data + manifest
    // inside one file; the legacy path takes a `.obdm` + per-volume images.
    let ext = req
        .manifest_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase();
    if matches!(ext.as_str(), "vhdx" | "avhdx" | "qcow2") {
        return restore_disk_from_image(req, progress);
    }
    restore_disk_legacy(req, progress)
}

/// Restore a whole disk from a single embedded-manifest image (plan D3): copy
/// the image's used blocks to the target at their real offsets, then overlay
/// the REAL GPT (real disk + partition GUIDs from the manifest — the image
/// itself carries random GUIDs to avoid colliding with the live source disk).
/// The real GUIDs keep a UEFI system bootable without touching the BCD.
fn restore_disk_from_image(
    req: &DiskRestoreRequest,
    mut progress: impl FnMut(&DiskRestoreProgress) -> Result<()>,
) -> Result<DiskRestoreSummary> {
    let started = Instant::now();
    let ext = req
        .manifest_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase();
    let sidecar = read_embedded_sidecar(&req.manifest_path, &ext)?;
    let dm = sidecar.disk_manifest.clone().ok_or_else(|| {
        OwnBackupError::ImageFormat(
            "image has no embedded whole-disk manifest — not a bootable disk image".into(),
        )
    })?;
    let layout = &dm.layout; // real GUIDs
    let bs = sidecar.manifest.block_size as u64;
    let image_size = sidecar.manifest.source_disk.size_bytes;

    let target_path = format!(r"\\.\PhysicalDrive{}", req.target_disk);
    let target_size = WinRawDevice::open_read(&target_path)?.length();
    // Validates every partition fits the target (and rejects a smaller disk).
    let ops = disk_gpt_write_ops(layout, target_size)?;

    info!(
        target = %target_path,
        model = %dm.disk_model,
        partitions = dm.images.len(),
        "Whole-disk restore (embedded manifest): copying data"
    );

    let mut image = ImageReadSource::open(&req.manifest_path, &sidecar.info.container)?;
    let mut td = WinRawDevice::open_write(&target_path)?;
    td.set_offline_writable(false)?;

    // Copy every stored image block to the same offset on the target. A block
    // straddling the GPT head carries the image's random GPT bytes — harmless,
    // the real GPT below overwrites the GPT sectors while the partition data
    // stays. (data_offset is 0 for a whole-disk image → block N is at N*bs.)
    let total = sidecar.checksums.len() as u64;
    let mut done = 0u64;
    let mut bytes = 0u64;
    for entry in &sidecar.checksums {
        let offset = entry.block_number * bs;
        let len = bs.min(image_size - offset) as usize;
        if offset + len as u64 > target_size {
            return Err(OwnBackupError::RestoreIncompatible(format!(
                "image block at {offset} exceeds the target disk size {target_size}"
            )));
        }
        let mut buf = vec![0u8; len];
        image.read_at(offset, &mut buf)?;
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
        td.write_at(offset, &buf)?;
        done += 1;
        bytes += len as u64;
        progress(&DiskRestoreProgress {
            phase: "partition".into(),
            partition_index: 0,
            partition_count: dm.images.len(),
            part_label: "Данные разделов".into(),
            blocks_done: done,
            blocks_total: total,
            bytes_written: bytes,
        })?;
    }

    // Overlay the REAL GPT last, so the restored disk carries the source's disk
    // and partition GUIDs (bootable) rather than the image's random ones.
    progress(&DiskRestoreProgress {
        phase: "gpt".into(),
        partition_index: dm.images.len(),
        partition_count: dm.images.len(),
        part_label: "Таблица разделов (GPT)".into(),
        blocks_done: total,
        blocks_total: total,
        bytes_written: bytes,
    })?;
    for op in &ops {
        td.write_at(op.offset, &op.bytes)?;
    }
    td.flush()?;
    let _ = td.update_properties();
    drop(td);
    drop(image);

    if let Ok(td) = WinRawDevice::open_write(&target_path) {
        let _ = td.set_online_this_session();
        let _ = td.update_properties();
    }
    info!(
        target = %target_path,
        partitions = dm.images.len(),
        bytes,
        secs = started.elapsed().as_secs(),
        "Whole-disk restore (embedded manifest) complete"
    );
    Ok(DiskRestoreSummary {
        partitions: dm.images.len(),
        bytes_written: bytes,
        duration: started.elapsed(),
    })
}

/// Legacy whole-disk restore from a `.obdm` manifest + per-volume images.
fn restore_disk_legacy(
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

    let mut image_dev = ImageReadSource::open(image_path, &sidecar.info.container)?;

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

fn child_path_for(image_path: &Path) -> PathBuf {
    let mut name = image_path.file_name().unwrap_or_default().to_os_string();
    name.push(".mount_rw.avhdx");
    image_path.with_file_name(name)
}

fn scratch_path_for(image_path: &Path) -> PathBuf {
    let mut name = image_path.file_name().unwrap_or_default().to_os_string();
    name.push(".mount_rw.scratch");
    image_path.with_file_name(name)
}

/// Mount an image; volumes inside get drive letters from Windows.
/// The attachment survives process exit — use [`unmount_image`] to detach.
pub fn mount_image(image_path: &Path, read_write: bool) -> Result<String> {
    // Dispatch by the container's OWN magic, never by sidecar metadata (new
    // backups carry no external .obkm, and a silent fall-through once sent a
    // qcow2 into vhdmp, which reported it as "file corrupted", error 1392).
    // qcow2 cannot go through vhdmp; it mounts via the Arsenal Image Mounter
    // driver + our devio server (see `mount_qcow2_via_aim`).
    let mut magic = [0u8; 8];
    {
        use std::io::Read;
        let mut f = std::fs::File::open(image_path)
            .map_err(|e| OwnBackupError::Device(format!("open {}: {e}", image_path.display())))?;
        f.read_exact(&mut magic)
            .map_err(|e| OwnBackupError::Device(format!("read {}: {e}", image_path.display())))?;
    }
    if magic[..4] == [0x51, 0x46, 0x49, 0xFB] {
        // "QFI\xfb"
        return mount_qcow2_via_aim(image_path, read_write);
    }
    if &magic != b"vhdxfile" {
        return Err(OwnBackupError::ImageFormat(format!(
            "{} is neither a VHDX nor a qcow2 image (unrecognized signature)",
            image_path.display()
        )));
    }

    let attached = if read_write {
        let child_path = child_path_for(image_path);
        let _ = std::fs::remove_file(&child_path);
        create_differencing_vhdx(&child_path, image_path)?;
        attach_vhdx(&child_path, AttachMode::ReadWriteMounted)?
    } else {
        attach_vhdx(image_path, AttachMode::ReadOnlyMounted)?
    };

    // Backup images carry a persisted "offline" disk attribute (it keeps the
    // chain from auto-mounting during backups), so for browsing the disk must
    // be brought online explicitly — for this session only, the registry
    // attribute stays. Requires a read-write device handle (IOCTL
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
    // qcow2 mounts are tracked in-process (AIM device + devio server).
    if aim_mounts().lock().unwrap().contains_key(image_path) {
        return unmount_qcow2_via_aim(image_path);
    }

    let child_path = child_path_for(image_path);
    if child_path.exists() {
        info!(image = %image_path.display(), "VHDX RW mount: unmounting child differencing VHDX");
        
        // 1. Detach child first
        detach_vhdx(&child_path)?;

        // 2. Read the touched blocks from BAT before we merge/delete it
        let alloc = match allocated_payload_blocks(&child_path) {
            Ok(a) if !a.blocks.is_empty() => Some(a),
            _ => None,
        };

        if let Some(alloc) = alloc {
            info!(image = %image_path.display(), count = alloc.blocks.len(), "VHDX RW mount: merging modified blocks back into base");
            
            // 3. Merge the child into the parent
            merge_into_parent(&child_path)?;
            let _ = std::fs::remove_file(&child_path);

            // 4. Update the sidecar with rehashed checksums (embedded or external)
            let ext = image_path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_lowercase();
            let mut read_from_embedded = false;
            let sidecar_attempt = match read_embedded_sidecar(image_path, &ext) {
                Ok(s) => {
                    read_from_embedded = true;
                    Ok(s)
                }
                Err(_) => {
                    let sidecar_path = sidecar_path_for(image_path);
                    read_sidecar(&sidecar_path)
                }
            };
            if let Ok(mut sidecar) = sidecar_attempt {
                let data_offset = sidecar.info.volume_data_offset;
                let volume_len = sidecar.manifest.source_disk.size_bytes;
                let block_size = sidecar.manifest.block_size;
                let _blocks_rehashed = crate::true_removal_win::rehash_touched_blocks(
                    image_path,
                    &alloc,
                    data_offset,
                    volume_len,
                    block_size,
                    &mut sidecar.checksums,
                )?;
                if read_from_embedded {
                    let sector = 512u64;
                    let last_lba = (DEFAULT_PARTITION_OFFSET + volume_len).div_ceil(sector) - 1;
                    let meta_offset = (last_lba + 1) * sector;
                    let mut blob_bytes = Vec::new();
                    crate::sidecar::write_sidecar_v2_to(&mut blob_bytes, &sidecar.info, &sidecar.manifest, &sidecar.checksums)?;
                    crate::vhdx_bat::write_vhdx_guest_at(image_path, meta_offset, &blob_bytes)?;
                    info!(image = %image_path.display(), "VHDX RW mount: embedded sidecar updated successfully");
                } else {
                    let sidecar_path = sidecar_path_for(image_path);
                    write_sidecar(&sidecar_path, &sidecar.info, &sidecar.manifest, &sidecar.checksums)?;
                    info!(image = %image_path.display(), "VHDX RW mount: .obkm sidecar updated successfully");
                }
            }
        } else {
            // No blocks touched, just delete child
            let _ = std::fs::remove_file(&child_path);
        }
        Ok(())
    } else {
        detach_vhdx(image_path)
    }
}

// ---------------------------------------------------------------------------
// qcow2 mounting via Arsenal Image Mounter (plan Q5)
// ---------------------------------------------------------------------------

struct AimMount {
    adapter: crate::aim_win::AimAdapter,
    device_number: u32,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    server: Option<std::thread::JoinHandle<()>>,
    overlay: Option<std::sync::Arc<std::sync::Mutex<crate::overlay::WritableOverlaySource>>>,
}

/// qcow2 mounts served by THIS process: the devio server and AIM device die
/// with the process, so this only makes sense inside the long-running service
/// (or a foreground CLI command).
static AIM_MOUNTS: std::sync::OnceLock<std::sync::Mutex<HashMap<PathBuf, AimMount>>> =
    std::sync::OnceLock::new();

fn aim_mounts() -> &'static std::sync::Mutex<HashMap<PathBuf, AimMount>> {
    // Shadow the module-wide alias so the code above can name it directly.
    AIM_MOUNTS.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// Present a qcow2 backup (incl. its backing chain) to Windows as a real
/// read-only disk: start a devio TCP server on an ephemeral loopback port,
/// then have the AIM driver attach a proxy disk pointing at it. Windows mounts
/// the NTFS volume inside and assigns a drive letter. Returns the physical
/// disk path (`\\.\PhysicalDriveN`).
fn mount_qcow2_via_aim(image_path: &Path, read_write: bool) -> Result<String> {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    if let Some(existing) = aim_mounts().lock().unwrap().get(image_path) {
        if let Some(n) = crate::aim_win::find_physical_drive(existing.device_number) {
            return Ok(format!(r"\\.\PhysicalDrive{n}"));
        }
    }

    let (virtual_size, cluster_size) = {
        let r = crate::qcow2::Qcow2RandomReader::open(image_path)?;
        (r.virtual_size(), r.cluster_size())
    };

    let overlay_opt = if read_write {
        let scratch = scratch_path_for(image_path);
        let _ = std::fs::remove_file(&scratch);
        let overlay_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&scratch)
            .map_err(|e| OwnBackupError::Device(format!("create scratch file: {e}")))?;
        let base: Box<dyn BlockSource> = Box::new(crate::qcow2::Qcow2RandomReader::open(image_path)?);
        let ov = std::sync::Arc::new(std::sync::Mutex::new(crate::overlay::WritableOverlaySource::new(base, cluster_size, overlay_file)));
        Some(ov)
    } else {
        None
    };

    let reader = if overlay_opt.is_none() {
        Some(crate::qcow2::Qcow2RandomReader::open(image_path)?)
    } else {
        None
    };

    // Bind an ephemeral loopback port for the devio server.
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|e| OwnBackupError::Device(format!("devio bind: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| OwnBackupError::Device(format!("devio local_addr: {e}")))?
        .port();

    let stop = Arc::new(AtomicBool::new(false));
    let stop_bg = stop.clone();
    let overlay_bg = overlay_opt.clone();
    let server = std::thread::Builder::new()
        .name("aim-devio".into())
        .spawn(move || {
            if let Some(ov) = overlay_bg {
                if let Err(e) = crate::devio::serve_listener_rw(listener, Box::new(ov), stop_bg) {
                    warn!(error = %e, "devio server for AIM RW mount ended with an error");
                }
            } else {
                if let Err(e) = crate::devio::serve_listener(listener, Box::new(reader.unwrap()), stop_bg) {
                    warn!(error = %e, "devio server for AIM RO mount ended with an error");
                }
            }
        })
        .map_err(|e| OwnBackupError::Device(format!("spawn devio server: {e}")))?;

    // Create the AIM proxy disk pointing at our server.
    let create = (|| -> Result<(crate::aim_win::AimAdapter, u32)> {
        let adapter = crate::aim_win::AimAdapter::open()?;
        let device_number = adapter.create_tcp_proxy(port, virtual_size, !read_write)?;
        Ok((adapter, device_number))
    })();
    let (adapter, device_number) = match create {
        Ok(v) => v,
        Err(e) => {
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = server.join();
            return Err(e);
        }
    };

    // Bring the disk online so Windows mounts the volume and assigns a letter
    let physical = crate::aim_win::find_physical_drive(device_number)
        .map(|n| format!(r"\\.\PhysicalDrive{n}"));
    if let Some(path) = &physical {
        match open_write_with_retry(path) {
            Ok(dev) => {
                if let Err(e) = dev.set_online_this_session().and_then(|()| dev.update_properties()) {
                    warn!(error = %e, "could not bring the AIM disk online — it may show offline in Disk Management");
                }
            }
            Err(e) => warn!(error = %e, "could not open the AIM disk to bring it online"),
        }
    }

    aim_mounts().lock().unwrap().insert(
        image_path.to_path_buf(),
        AimMount { adapter, device_number, stop, server: Some(server), overlay: overlay_opt },
    );
    info!(image = %image_path.display(), device_number = format!("{device_number:#08x}"), "qcow2 mounted via Arsenal Image Mounter");
    Ok(physical.unwrap_or_else(|| "(disk created — see Disk Management)".into()))
}

fn unmount_qcow2_via_aim(image_path: &Path) -> Result<()> {
    let entry = aim_mounts().lock().unwrap().remove(image_path);
    let Some(mut entry) = entry else {
        return Err(OwnBackupError::Config(format!(
            "{} is not mounted by this service",
            image_path.display()
        )));
    };
    // Remove the AIM device first (stops the driver's proxy I/O), then the
    // devio server, then join.
    let removed = entry.adapter.remove_device(entry.device_number);
    entry.stop.store(true, std::sync::atomic::Ordering::SeqCst);
    if let Some(t) = entry.server.take() {
        let _ = t.join();
    }
    removed?;

    // If it was mounted in RW mode, we have the overlay and need to merge + rehash + update sidecar!
    if let Some(overlay_arc) = entry.overlay {
        let touched = overlay_arc.lock().unwrap().touched_clusters();
        if !touched.is_empty() {
            info!(image = %image_path.display(), count = touched.len(), "QCOW2 RW mount: merging modified clusters back into base");
            
            let zstd_level = Some(crate::qcow2::DEFAULT_ZSTD_LEVEL);
            {
                let mut ov = overlay_arc.lock().unwrap();
                let mut merge = crate::qcow2::Qcow2InPlaceMerge::open(image_path, zstd_level)?;
                for &c in &touched {
                    let data = ov.read_cluster(c)?;
                    merge.write_cluster(c, &data)?;
                }
                merge.commit()?;
            }

            // Now rehash the touched regions and update the sidecar (embedded or external)!
            let ext = image_path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_lowercase();
            let mut read_from_embedded = false;
            let sidecar_attempt = match read_embedded_sidecar(image_path, &ext) {
                Ok(s) => {
                    read_from_embedded = true;
                    Ok(s)
                }
                Err(_) => {
                    let sidecar_path = sidecar_path_for(image_path);
                    read_sidecar(&sidecar_path)
                }
            };
            if let Ok(mut sidecar) = sidecar_attempt {
                let cluster_size = {
                    let ov = overlay_arc.lock().unwrap();
                    ov.cluster_size()
                };
                let data_offset = sidecar.info.volume_data_offset;
                let volume_len = sidecar.manifest.source_disk.size_bytes;
                let block_size = sidecar.manifest.block_size;
                let _blocks_rehashed = crate::qcow2_removal_win::rehash_touched(
                    image_path,
                    &touched,
                    cluster_size,
                    data_offset,
                    volume_len,
                    block_size,
                    &mut sidecar.checksums,
                )?;
                if read_from_embedded {
                    let sector = 512u64;
                    let last_lba = (DEFAULT_PARTITION_OFFSET + volume_len).div_ceil(sector) - 1;
                    let meta_offset = (last_lba + 1) * sector;
                    let mut blob_bytes = Vec::new();
                    crate::sidecar::write_sidecar_v2_to(&mut blob_bytes, &sidecar.info, &sidecar.manifest, &sidecar.checksums)?;
                    crate::qcow2::merge::write_qcow2_guest_blob(image_path, meta_offset, &blob_bytes)?;
                    info!(image = %image_path.display(), "QCOW2 RW mount: embedded sidecar updated successfully");
                } else {
                    let sidecar_path = sidecar_path_for(image_path);
                    write_sidecar(&sidecar_path, &sidecar.info, &sidecar.manifest, &sidecar.checksums)?;
                    info!(image = %image_path.display(), "QCOW2 RW mount: .obkm sidecar updated successfully");
                }
            }
        }
        // Delete scratch file
        let scratch = scratch_path_for(image_path);
        let _ = std::fs::remove_file(&scratch);
    }

    info!(image = %image_path.display(), "qcow2 unmounted (AIM device removed)");
    Ok(())
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

/// Read an image's sidecar: embedded first, adjacent `.obkm` as a fallback
/// for images written before the sidecar moved inside the container.
pub fn load_sidecar_of(image_path: &Path) -> Result<Sidecar> {
    let ext = image_path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_lowercase();
    match read_embedded_sidecar(image_path, &ext) {
        Ok(s) => Ok(s),
        Err(e) => {
            let sidecar_path = sidecar_path_for(image_path);
            if !sidecar_path.exists() {
                return Err(OwnBackupError::ImageFormat(format!(
                    "embedded sidecar not found ({e}) and adjacent sidecar not found: {}",
                    sidecar_path.display()
                )));
            }
            read_sidecar(&sidecar_path)
        }
    }
}

fn get_diskpart_virtual_disks() -> Vec<(u32, String)> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    let mut results = Vec::new();

    let mut cmd = match std::process::Command::new("cmd")
        .args(["/c", "chcp 65001 >nul && diskpart"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "failed to spawn diskpart command proxy");
            return results;
        }
    };

    if let Some(mut stdin) = cmd.stdin.take() {
        use std::io::Write;
        if let Err(e) = writeln!(stdin, "list vdisk") {
            tracing::error!(error = %e, "failed to write to diskpart stdin");
        }
    }

    let mut byte_buf = Vec::new();
    if let Some(mut stdout) = cmd.stdout.take() {
        use std::io::Read;
        if let Err(e) = stdout.read_to_end(&mut byte_buf) {
            tracing::error!(error = %e, "failed to read from diskpart stdout");
        }
    }
    let output = String::from_utf8_lossy(&byte_buf).to_string();

    let mut stderr_output = String::new();
    if let Some(mut stderr) = cmd.stderr.take() {
        use std::io::Read;
        let _ = stderr.read_to_string(&mut stderr_output);
    }

    let _ = cmd.wait();

    for line in output.lines() {
        let line_trimmed = line.trim();
        if line_trimmed.is_empty() {
            continue;
        }

        let line_lower = line_trimmed.to_lowercase();
        if !line_lower.contains(".vhdx") && !line_lower.contains(".vhd") && !line_lower.contains(".avhdx") {
            continue;
        }

        let disk_num = (|| {
            let numeric_tokens: Vec<u32> = line_trimmed
                .split_whitespace()
                .filter_map(|t| t.parse::<u32>().ok())
                .collect();
            if numeric_tokens.len() >= 2 {
                Some(numeric_tokens[1])
            } else {
                None
            }
        })();

        let disk_num = match disk_num {
            Some(n) => n,
            None => continue,
        };

        let mut path_start = None;
        let chars: Vec<char> = line_trimmed.chars().collect();
        for i in 0..chars.len() {
            if chars[i] == ':' && i > 0 && i + 1 < chars.len() && chars[i + 1] == '\\' {
                path_start = Some(i - 1);
                break;
            }
            if chars[i] == '\\' && i + 1 < chars.len() && chars[i + 1] == '\\' {
                path_start = Some(i);
                break;
            }
        }

        if let Some(start) = path_start {
            let path = chars[start..].iter().collect::<String>().trim().to_string();
            results.push((disk_num, path));
        }
    }

    results
}

pub fn get_all_active_mounts(
    internal_mounts: &[own_backup_common::engine_dto::ActiveMountDto],
) -> Vec<own_backup_common::engine_dto::ActiveMountDto> {
    let mut result = internal_mounts.to_vec();

    for (disk_num, image_path) in get_diskpart_virtual_disks() {
        let device_path = format!(r"\\.\PhysicalDrive{disk_num}");

        let clean_item_path = image_path.replace('/', "\\");
        let exists = result.iter().any(|m| {
            let clean_m_path = m.image_path.replace('/', "\\");
            clean_m_path.eq_ignore_ascii_case(&clean_item_path)
        });

        if !exists {
            result.push(own_backup_common::engine_dto::ActiveMountDto {
                image_path,
                physical_path: device_path,
                read_write: false,
                volume_letters: Vec::new(),
            });
        }
    }

    // Resolve the drive letters Windows assigned to each mounted disk so the UI
    // can show "J:" instead of "PhysicalDrive5" / "Диск 5".
    for m in &mut result {
        if let Some(disk_num) = disk_number_from_physical(&m.physical_path) {
            m.volume_letters = crate::volume_win::drive_letters_on_disk(disk_num);
        }
    }

    result
}

/// Parse `\\.\PhysicalDriveN` → `N`.
fn disk_number_from_physical(physical_path: &str) -> Option<u32> {
    physical_path
        .rsplit("PhysicalDrive")
        .next()
        .and_then(|n| n.trim().parse().ok())
}



