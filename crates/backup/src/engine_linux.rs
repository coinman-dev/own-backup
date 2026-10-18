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
    BackupMode, BackupStatus, BackupType, BootMode, CompressionAlgo, ImageFormat, PartitionTable,
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
use crate::sidecar::{
    read_embedded_sidecar, read_embedded_sidecar_header, read_sidecar, sidecar_path_for,
    SidecarInfo,
};
use crate::used_sectors::{AllUsedMap, UsedSectorMap};

/// An unlocked encryption layer under the source volume, if any.
enum Unlock {
    None,
    Luks(LuksParams),
    BitLocker(crate::bitlocker::BitLockerParams),
}

impl Unlock {
    fn is_encrypted(&self) -> bool {
        !matches!(self, Unlock::None)
    }
}

/// Default backup block size: 1 MiB.
pub const DEFAULT_BLOCK_SIZE: u32 = 1024 * 1024;
const MIB: u64 = 1024 * 1024;
/// VHDX payload block size (allocation granularity of the dynamic image).
const VHDX_BLOCK_SIZE: u64 = 2 * MIB;

/// Image writer behind the format switch: dynamic VHDX (pure-Rust writer) or
/// zstd-compressed qcow2 — both expose the byte-granular `write_at` the
/// GPT/block pipeline uses.
enum LinuxImageWriter {
    Vhdx(crate::vhdx_format::VhdxWriter),
    Qcow2(crate::qcow2::Qcow2StagedWriter<std::io::BufWriter<std::fs::File>>),
    /// The encrypted container. Its sidecar is not written into the guest at
    /// all — it is handed to `finish` and sealed inside the image.
    Obk(Box<crate::seal::obk::ObkStagedWriter<std::io::BufWriter<std::fs::File>>>),
}

impl LinuxImageWriter {
    fn create(
        format: ImageFormat,
        path: &Path,
        disk_size: u64,
        sector_size: u32,
        zstd_level: i32,
    ) -> Result<Self> {
        Ok(match format {
            ImageFormat::Vhdx => Self::Vhdx(crate::vhdx_format::VhdxWriter::create(
                path,
                disk_size,
                VHDX_BLOCK_SIZE,
                sector_size,
            )?),
            ImageFormat::Qcow2 => Self::Qcow2(crate::qcow2::Qcow2StagedWriter::create(
                path,
                crate::qcow2::Qcow2WriterOptions {
                    virtual_size: disk_size,
                    compression: crate::qcow2::Qcow2Compression::Zstd {
                        level: crate::qcow2::zstd_level_or_default(zstd_level),
                    },
                    ..Default::default()
                },
            )?),
        })
    }

    /// Encryption is not a third format the user picks — it is the qcow2 path
    /// with our incompatible extension, so a job that encrypts always writes
    /// `.obk` whatever `image_format` says (§7.1).
    fn create_encrypted(
        path: &Path,
        keys: &crate::seal::context::ArchiveKeys,
        chain: &crate::seal::context::ChainSession,
        plan: crate::seal::context::ImagePlan<'_>,
    ) -> Result<Self> {
        Ok(Self::Obk(Box::new(
            crate::seal::obk::ObkStagedWriter::create(
                path,
                &chain.key,
                keys.writer_options(chain, plan),
            )?,
        )))
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        match self {
            Self::Vhdx(w) => w.write_at(offset, data),
            Self::Qcow2(w) => w.write_at(offset, data),
            Self::Obk(w) => w.write_at(offset, data),
        }
    }

    /// `sealed` is the sidecar of an encrypted image. The other two containers
    /// have already written theirs into their guest meta partition.
    ///
    /// Each is on disk when this returns: the VHDX writer syncs itself, the
    /// other two hand back their file for it. A copy that a power cut can
    /// still take back is not finished.
    fn finish(self, sealed: &[u8]) -> Result<()> {
        match self {
            Self::Vhdx(w) => w.finish(),
            Self::Qcow2(w) => Ok(w.finish()?.get_ref().sync_all()?),
            Self::Obk(w) => Ok(w.finish(sealed)?.get_ref().sync_all()?),
        }
    }
}

/// Read side of the same switch, picked by the sidecar's `container` field.
enum LinuxImageReader {
    /// Chain-aware: a differencing VHDX made on Windows reads its absent and
    /// partially present (BAT state 7) blocks from the parent. The plain
    /// `vhdx_format::VhdxReader` reads them as zeros.
    Vhdx(crate::vhdx_bat::VhdxGuestReader),
    Qcow2(crate::qcow2::Qcow2RandomReader),
    Obk(crate::seal::obk::ObkRandomReader),
}

impl LinuxImageReader {
    fn open(path: &Path, container: &str) -> Result<Self> {
        Self::open_with(path, container, None)
    }

    /// `key` is the chain key of an encrypted image — without it an `.obk` is
    /// refused by name rather than half-read.
    fn open_with(
        path: &Path,
        container: &str,
        key: Option<&crate::seal::ChainKey>,
    ) -> Result<Self> {
        match (container, key) {
            ("vhdx", _) => Ok(Self::Vhdx(crate::vhdx_bat::VhdxGuestReader::open(path)?)),
            ("qcow2", _) => Ok(Self::Qcow2(crate::qcow2::Qcow2RandomReader::open(path)?)),
            ("obk", Some(key)) => Ok(Self::Obk(crate::seal::obk::ObkRandomReader::open(
                path, key,
            )?)),
            ("obk", None) => Err(OwnBackupError::Encryption(format!(
                "{}: the image is encrypted — a password or the recovery key is needed to open it",
                path.display()
            ))),
            (other, _) => Err(OwnBackupError::ImageFormat(format!(
                "unknown image container '{other}' in sidecar"
            ))),
        }
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        match self {
            Self::Vhdx(r) => r.read_at(offset, buf),
            Self::Qcow2(r) => r.read_at(offset, buf),
            Self::Obk(r) => r.read_at(offset, buf),
        }
    }

    fn virtual_size(&self) -> u64 {
        match self {
            Self::Vhdx(r) => r.virtual_size(),
            Self::Qcow2(r) => r.virtual_size(),
            Self::Obk(r) => r.virtual_size(),
        }
    }
}

impl crate::block_source::BlockSource for LinuxImageReader {
    fn size(&self) -> u64 {
        self.virtual_size()
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        LinuxImageReader::read_at(self, offset, buf)
    }
}

/// The sidecar of an image, wherever it keeps it: sealed inside an encrypted
/// container, in the guest meta partition of a plaintext one, or in the
/// adjacent `.obkm` of an older copy.
fn read_sidecar_for(
    image_path: &Path,
    keys: Option<&crate::seal::context::ArchiveKeys>,
) -> Result<(crate::sidecar::Sidecar, Option<crate::seal::ChainKey>)> {
    let ext = image_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase();
    if ext == crate::seal::obk::OBK_EXTENSION {
        let keys = keys.ok_or_else(|| {
            OwnBackupError::Encryption(format!(
                "{}: the image is encrypted — a password or the recovery key is needed to open it",
                image_path.display()
            ))
        })?;
        let chain = keys.open_chain_of(image_path)?;
        let sidecar = crate::sidecar::read_sealed_sidecar(image_path, &chain.key)?;
        return Ok((sidecar, Some(chain.key)));
    }
    let sidecar = match read_embedded_sidecar(image_path, &ext) {
        Ok(s) => s,
        Err(e) => {
            let sidecar_path = sidecar_path_for(image_path);
            if !sidecar_path.exists() {
                return Err(OwnBackupError::ImageFormat(format!(
                    "embedded sidecar not found ({e}) and adjacent sidecar not found: {}",
                    sidecar_path.display()
                )));
            }
            read_sidecar(&sidecar_path)?
        }
    };
    Ok((sidecar, None))
}

/// Does `passphrase` open the encrypted volume at `device`, and what is in it?
///
/// The Linux counterpart of `engine_win::unlock_and_probe`, for the service's
/// `ProvideVolumePassword`: the device is read directly (root has raw access),
/// LUKS or BitLocker is opened with our own code, and the filesystem inside is
/// described. A rejected passphrase is `ok: false`; anything that is not about
/// the passphrase — an unsupported cipher, an unreadable header — is an error,
/// so nobody is sent to re-type a password that was right.
pub fn unlock_and_probe(
    device: &str,
    passphrase: &str,
) -> Result<crate::unlock_probe::DecryptedProbe> {
    use crate::unlock_probe::{probe_decrypted, DecryptedProbe};

    let mut dev = LinuxRawDevice::open_read(device)?;
    let mut header = vec![0u8; 4096];
    dev.read_at(0, &mut header)?;
    if crate::luks::is_luks(&header) {
        let params = match crate::luks::open_luks(&mut dev, passphrase.as_bytes()) {
            Ok(p) => p,
            Err(e) if crate::luks::is_wrong_passphrase(&e) => {
                info!(error = %e, "LUKS passphrase rejected");
                return Ok(DecryptedProbe::default());
            }
            Err(e) => return Err(e),
        };
        let mut view = crate::decrypt::DecryptingVolume::new(
            Box::new(LinuxRawDevice::open_read(device)?),
            params,
        );
        return probe_decrypted(&mut view);
    }
    if crate::bitlocker::is_bitlocker(&header) {
        let sector = dev.sector_size();
        let params = match crate::bitlocker::open_bitlocker_with(&mut dev, passphrase, Some(sector))
        {
            Ok(p) => p,
            Err(e) if crate::bitlocker::is_wrong_passphrase(&e) => {
                info!(error = %e, "BitLocker passphrase rejected");
                return Ok(DecryptedProbe::default());
            }
            Err(e) => return Err(e),
        };
        let mut view = crate::bitlocker::BitLockerVolume::new(
            Box::new(LinuxRawDevice::open_read(device)?),
            params,
        );
        return probe_decrypted(&mut view);
    }
    // Not encrypted: there was nothing for the passphrase to open, and the
    // volume reads as it is.
    drop(dev);
    probe_decrypted(&mut LinuxRawDevice::open_read(device)?)
}

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
    /// Passphrase for an encrypted source (LUKS or BitLocker) — its decrypted
    /// data is backed up (used blocks only). Ignored for non-encrypted sources.
    pub passphrase: Option<Zeroizing<String>>,
    /// GUI job id, stamped into the sidecar.
    pub job_id: Option<String>,
    /// Image container format: VHDX (default) or zstd-compressed qcow2.
    pub image_format: ImageFormat,
    /// Present for an encrypted job: the keys opened before the run started
    /// (the service's own, or a password's). The run then asks nobody anything
    /// and writes `.obk` whatever `image_format` says.
    pub encryption: Option<crate::seal::context::ArchiveKeys>,
    /// zstd level for qcow2 and `.obk` (1–19; anything else is the default).
    pub zstd_level: i32,
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
    if req.encryption.is_some()
        && req
            .image_path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .as_deref()
            != Some(crate::seal::obk::OBK_EXTENSION)
    {
        // Listings and retention find encrypted images by this extension; a
        // differently named one would be invisible to both.
        return Err(OwnBackupError::Config(format!(
            "an encrypted image must be named .{}: {}",
            crate::seal::obk::OBK_EXTENSION,
            req.image_path.display()
        )));
    }
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
    // VHDX and qcow2 are written under a private name and take their public
    // one only when complete, as on Windows (`pending_image`): a run killed
    // half-way must not leave something the next run lists as a version. The
    // encrypted container has its own private-name machinery, which this
    // engine does not drive yet; it keeps the old rule of deleting on error.
    let pending = match req.encryption {
        None => Some(crate::pending_image::PendingImage::new(&req.image_path)?),
        Some(_) => None,
    };
    let write_to = pending
        .as_ref()
        .map(|p| p.path().to_path_buf())
        .unwrap_or_else(|| req.image_path.clone());
    let result = backup_inner(req, &write_to, progress);
    match (result, pending) {
        (Ok(summary), Some(pending)) => {
            pending.publish_as(&req.image_path)?;
            Ok(summary)
        }
        (Ok(summary), None) => Ok(summary),
        // Dropping the private name deletes what was written under it.
        (Err(e), Some(_)) => Err(e),
        (Err(e), None) => {
            let _ = std::fs::remove_file(&req.image_path);
            Err(e)
        }
    }
}

/// `write_to` is where the bytes go — the private name while the run lasts;
/// everything recorded inside the image names `req.image_path`.
fn backup_inner(
    req: &LinuxBackupRequest,
    write_to: &Path,
    mut progress: impl FnMut(&BackupProgress) -> Result<()>,
) -> Result<LinuxBackupSummary> {
    let started = Instant::now();
    let read_path = req.source.clone();

    // --- Optional unlock of an encrypted source (LUKS or BitLocker); the
    // decrypted data is what gets backed up (used blocks only) ---
    let mut probe = LinuxRawDevice::open_read(&read_path)?;
    let unlock: Unlock = match &req.passphrase {
        Some(pass) => {
            let mut header = vec![0u8; 4096];
            probe.read_at(0, &mut header)?;
            if crate::luks::is_luks(&header) {
                let params = crate::luks::open_luks(&mut probe, pass.as_bytes())?;
                info!(kind = ?params.kind, "LUKS source unlocked; backing up decrypted data");
                Unlock::Luks(params)
            } else if crate::bitlocker::is_bitlocker(&header) {
                // The device's sector size: without it a container with exFAT
                // or ReFS inside does not open.
                let sector = probe.sector_size();
                let params =
                    crate::bitlocker::open_bitlocker_with(&mut probe, pass.as_str(), Some(sector))?;
                info!(
                    algo = format!("{:#x}", params.algorithm),
                    "BitLocker source unlocked; backing up decrypted data"
                );
                Unlock::BitLocker(params)
            } else {
                Unlock::None
            }
        }
        None => Unlock::None,
    };
    let device_len = probe.length();
    let sector_size = probe.sector_size();
    drop(probe);

    let volume_len = match &unlock {
        Unlock::None => device_len,
        Unlock::Luks(p) => device_len.saturating_sub(p.data_offset),
        Unlock::BitLocker(p) => p.encrypted_size.min(device_len),
    };
    if req.block_size == 0 || req.block_size % sector_size != 0 {
        return Err(OwnBackupError::Config(format!(
            "block size {} is not a multiple of the source sector size {sector_size}",
            req.block_size
        )));
    }
    let total_blocks = volume_len.div_ceil(req.block_size as u64);

    // A fresh read source (device, or a decrypting view over it).
    let open_source = |unlock: &Unlock| -> Result<Box<dyn RawVolume + Send>> {
        let dev = LinuxRawDevice::open_read(&read_path)?;
        Ok(match unlock {
            Unlock::None => Box::new(dev),
            Unlock::Luks(p) => Box::new(DecryptingVolume::new(Box::new(dev), p.clone())),
            Unlock::BitLocker(p) => Box::new(crate::bitlocker::BitLockerVolume::new(
                Box::new(dev),
                p.clone(),
            )),
        })
    };

    // The filesystem found INSIDE the volume — the decrypted one for a LUKS or
    // BitLocker source, since that is what the image holds (R8).
    let mut detected_fs: Option<String> = None;
    // --- Used-block map: FAT/exFAT → ext → btrfs → xfs → NTFS (else whole) ---
    let (used, smart_mode): (Box<dyn UsedSectorMap>, bool) = if req.all_sectors {
        info!("Sector-by-sector mode requested; copying all blocks");
        (Box::new(AllUsedMap::new(total_blocks)), false)
    } else {
        let mut src = open_source(&unlock)?;
        let map: Option<Box<dyn UsedSectorMap>> = if let Ok(m) =
            crate::fat_map::build_fat_used_map(src.as_mut(), req.block_size)
        {
            info!(fs = m.filesystem, "Using FAT used-cluster map");
            detected_fs = Some(m.filesystem.to_string());
            Some(Box::new(m))
        } else if let Ok(m) = crate::ext_map::build_ext_used_map(src.as_mut(), req.block_size) {
            info!(fs = "ext4", "Using ext used-block map");
            detected_fs = Some("ext4".into());
            Some(Box::new(m))
        } else if let Ok(m) = crate::btrfs_map::build_btrfs_used_map(src.as_mut(), req.block_size) {
            info!(fs = "btrfs", "Using btrfs used-block map");
            detected_fs = Some("btrfs".into());
            Some(Box::new(m))
        } else if let Ok(m) = crate::xfs_map::build_xfs_used_map(src.as_mut(), req.block_size) {
            info!(fs = "xfs", "Using xfs used-block map");
            detected_fs = Some("xfs".into());
            Some(Box::new(m))
        } else if let Ok(m) = crate::ntfs_map::build_ntfs_used_map(src.as_mut(), req.block_size) {
            info!(fs = "ntfs", "Using NTFS used-cluster map");
            detected_fs = Some("NTFS".into());
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

    // --- Create the image: 1 MiB GPT head + volume + meta partition + 1 MiB tail ---
    // The container's header goes into the sidecar (D19) and is measured in
    // megabytes; reserve for it or the final metadata write has nowhere to go.
    let luks_reserve = crate::sidecar::luks_reserve_for(match &unlock {
        Unlock::Luks(p) => Some(p.data_offset),
        _ => None,
    });
    // An encrypted image keeps its sidecar sealed inside the container, so it
    // needs no guest meta partition and no room for one (§7.3).
    let encrypted = req.encryption.is_some();
    let meta_size = if encrypted {
        0
    } else {
        crate::sidecar::calculate_meta_partition_size(total_blocks, luks_reserve)
    };
    let disk_size = DEFAULT_PARTITION_OFFSET + volume_len.div_ceil(MIB) * MIB + meta_size + MIB;
    let backup_id = Uuid::new_v4();
    let created_at = chrono::Utc::now().to_rfc3339();
    let chain = match &req.encryption {
        Some(keys) => Some(keys.start_chain()?),
        None => None,
    };
    let mut writer = match (&req.encryption, &chain) {
        (Some(keys), Some(chain)) => LinuxImageWriter::create_encrypted(
            write_to,
            keys,
            chain,
            crate::seal::context::ImagePlan {
                backup_id,
                role: crate::sidecar::ChainRole::Full,
                parent: None,
                created_at: created_at.clone(),
                virtual_size: disk_size,
                source_id: None,
                consolidated_increments: 0,
                chain_started_at: None,
                compression: crate::seal::obk::ObkCompression::Zstd {
                    level: crate::qcow2::zstd_level_or_default(req.zstd_level),
                },
            },
        )?,
        _ => LinuxImageWriter::create(
            req.image_format,
            write_to,
            disk_size,
            sector_size,
            req.zstd_level,
        )?,
    };
    let data_offset = DEFAULT_PARTITION_OFFSET;

    // --- GPT with Basic Data partition + OwnBackupMeta partition ---
    let disk_guid = Uuid::new_v4();
    let meta_partition_guid = Uuid::new_v4();
    let layout = GptLayout {
        disk_size,
        sector_size,
        partition_offset: DEFAULT_PARTITION_OFFSET,
        partition_size: volume_len,
        disk_guid,
        partition_guid: Uuid::new_v4(),
        partition_name: "OwnBackup".into(),
        meta_partition_size: (!encrypted).then_some(meta_size),
        meta_partition_guid: (!encrypted).then_some(meta_partition_guid),
    };
    for op in gpt_write_ops(&layout)? {
        writer.write_at(op.offset, &op.bytes)?;
    }

    // --- Copy used blocks into the partition ---
    let mut reader: Box<dyn BlockReader> = Box::new(RawVolumeBlockReader::new(
        open_source(&unlock)?,
        req.block_size,
    )?);
    let outcome = copy_used_blocks(
        &backup_id.to_string(),
        reader.as_mut(),
        used.as_ref(),
        None,
        |block_number, data| {
            writer.write_at(data_offset + block_number * req.block_size as u64, data)
        },
        &mut progress,
    )?;

    // --- Sidecar + manifest ---
    let manifest = ImageManifest {
        backup_id,
        // The same stamp the open header carries, so a reader holding the key
        // can check the two halves against each other.
        created_at: created_at.clone(),
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
        compression_algo: match req.image_format {
            ImageFormat::Vhdx => CompressionAlgo::None,
            ImageFormat::Qcow2 => CompressionAlgo::Zstd,
        },
        compression_level: match req.image_format {
            ImageFormat::Vhdx => 0,
            ImageFormat::Qcow2 => crate::qcow2::zstd_level_or_default(req.zstd_level),
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
        volume_encryption: {
            use own_backup_common::image::{VolumeCaptureMode, VolumeEncryptionState};
            let state = |kind| VolumeEncryptionState {
                kind,
                capture_mode: VolumeCaptureMode::Plaintext,
                luks_header_backup_b64: None,
                note: Some("Decrypted at backup; re-enable encryption after restore".into()),
            };
            match &unlock {
                Unlock::None => None,
                Unlock::Luks(p) => Some(state(p.kind.clone())),
                Unlock::BitLocker(p) => Some(state(p.kind.clone())),
            }
        },
    };

    let image_file_name = req
        .image_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut sidecar_info = SidecarInfo::new_full(image_file_name, data_offset);
    sidecar_info.container = if encrypted {
        crate::seal::obk::OBK_EXTENSION.into()
    } else {
        req.image_format.extension().into()
    };
    sidecar_info.job_id = req.job_id.clone();
    sidecar_info.filesystem = detected_fs;
    let sidecar_path = sidecar_path_for(&req.image_path);

    // The sidecar: into the guest meta partition for a plaintext image, sealed
    // inside the container for an encrypted one. Never both, and never in the
    // clear next to an encrypted image.
    let mut blob_bytes = Vec::new();
    crate::sidecar::write_sidecar_v2_to(
        &mut blob_bytes,
        &sidecar_info,
        &manifest,
        &outcome.checksums,
    )?;
    if !encrypted {
        let sector = sector_size as u64;
        let last_lba = (DEFAULT_PARTITION_OFFSET + volume_len).div_ceil(sector) - 1;
        let meta_offset = (last_lba + 1) * sector;
        writer.write_at(meta_offset, &blob_bytes)?;
    }
    writer.finish(&blob_bytes)?;

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
        encrypted: unlock.is_encrypted(),
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
    /// Keys for an encrypted image: the service's own for an automatic check,
    /// or a password's/recovery key's for a person restoring by hand.
    pub encryption: Option<crate::seal::context::ArchiveKeys>,
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
    let (sidecar, chain_key) = read_sidecar_for(&req.image_path, req.encryption.as_ref())?;
    let block_size = sidecar.manifest.block_size as u64;
    let volume_len = sidecar.manifest.source_disk.size_bytes;
    let data_offset = sidecar.info.volume_data_offset;

    let mut image =
        LinuxImageReader::open_with(&req.image_path, &sidecar.info.container, chain_key.as_ref())?;
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

// ---------------------------------------------------------------------------
// Mount / unmount (plan Q4): image → in-process NBD device → `mount -o ro`
// ---------------------------------------------------------------------------

/// Linux mount-support status — the counterpart of the Windows AIM-driver
/// status. There is no driver to install: mounting a backup uses the in-tree
/// kernel NBD module (this process serves the blocks itself — no qemu), and
/// browsing an NTFS backup needs the kernel `ntfs3` driver or `ntfs-3g`.
#[derive(serde::Serialize)]
pub struct MountSupport {
    /// Always `"linux"` — lets the GUI pick the right card.
    pub platform: &'static str,
    /// `/dev/nbd0` is present (the NBD module is loaded / built in).
    pub nbd_ok: bool,
    /// An NTFS backend is available for mounting NTFS backups.
    pub ntfs_ok: bool,
    /// Which NTFS backend was found: `"ntfs3"` (kernel) or `"ntfs-3g"`.
    pub ntfs_backend: Option<String>,
    /// Running kernel release (diagnostic).
    pub kernel: String,
}

/// Probe the host for the mounting prerequisites (read-only; loads nothing).
pub fn mount_support_status() -> MountSupport {
    let nbd_ok = Path::new("/dev/nbd0").exists()
        || std::fs::read_to_string("/proc/modules")
            .map(|m| m.lines().any(|l| l.starts_with("nbd ")))
            .unwrap_or(false);

    // Kernel ntfs3 shows up in /proc/filesystems; ntfs-3g is a userspace helper.
    let kernel_ntfs = std::fs::read_to_string("/proc/filesystems")
        .map(|f| f.split_whitespace().any(|t| t == "ntfs3" || t == "ntfs"))
        .unwrap_or(false);
    let ntfs3g = [
        "/sbin/mount.ntfs-3g",
        "/usr/sbin/mount.ntfs-3g",
        "/bin/ntfs-3g",
        "/usr/bin/ntfs-3g",
    ]
    .iter()
    .any(|p| Path::new(p).exists());
    let ntfs_backend = if kernel_ntfs {
        Some("ntfs3".to_string())
    } else if ntfs3g {
        Some("ntfs-3g".to_string())
    } else {
        None
    };

    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    MountSupport {
        platform: "linux",
        nbd_ok,
        ntfs_ok: ntfs_backend.is_some(),
        ntfs_backend,
        kernel,
    }
}

/// Best-effort: load the kernel NBD module so `/dev/nbdX` appears. Needs root
/// (the service already runs as root). Returns an error if it still isn't there.
pub fn ensure_nbd_loaded() -> Result<()> {
    crate::nbd_linux::ensure_nbd_module()
}

struct ActiveMount {
    nbd: crate::nbd_linux::NbdAttachment,
    mount_dir: PathBuf,
}

/// Mounts served by THIS process — the NBD device dies with the serving
/// process, so mounting only makes sense inside the long-running service.
static ACTIVE_MOUNTS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<PathBuf, ActiveMount>>,
> = std::sync::OnceLock::new();

fn active_mounts() -> &'static std::sync::Mutex<std::collections::HashMap<PathBuf, ActiveMount>> {
    ACTIVE_MOUNTS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Mount a backup image (qcow2 incl. chains, or VHDX) and return
/// the mount point. Requires root (mount(2) + /dev/nbdX). The volume inside
/// is found via the sidecar's `volume_data_offset` — no partition probing.
pub fn mount_image(image_path: &Path, read_write: bool) -> Result<String> {
    {
        let mounts = active_mounts().lock().unwrap();
        if let Some(existing) = mounts.get(image_path) {
            return Ok(existing.mount_dir.display().to_string());
        }
    }

    let ext = image_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase();
    let (info, _manifest) = match read_embedded_sidecar_header(image_path, &ext) {
        Ok(h) => h,
        Err(_) => {
            let sidecar_path = sidecar_path_for(image_path);
            crate::sidecar::read_sidecar_header(&sidecar_path)?
        }
    };
    let reader = LinuxImageReader::open(image_path, &info.container)?;

    let nbd = crate::nbd_linux::NbdAttachment::attach(Box::new(reader))?;

    // SAFETY: geteuid is always safe.
    let base = if unsafe { libc::geteuid() } == 0 {
        PathBuf::from("/run/own-backup/mounts")
    } else {
        std::env::temp_dir().join("own-backup-mounts")
    };
    let stem = image_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "image".into());
    let mount_dir = base.join(stem);
    std::fs::create_dir_all(&mount_dir)?;

    // `-o loop,offset=…` puts a loop device over the NBD disk at the volume
    // start, so the filesystem mounts regardless of nbd partition support.
    let mode_str = if read_write { "rw" } else { "ro" };
    let out = std::process::Command::new("mount")
        .arg("-o")
        .arg(format!(
            "{},loop,offset={}",
            mode_str, info.volume_data_offset
        ))
        .arg(&nbd.device)
        .arg(&mount_dir)
        .output()
        .map_err(|e| OwnBackupError::Device(format!("failed to run mount: {e}")))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let _ = nbd.detach();
        let _ = std::fs::remove_dir(&mount_dir);
        return Err(OwnBackupError::Device(format!(
            "mount failed (root required; NTFS needs the ntfs3 driver or ntfs-3g): {err}"
        )));
    }

    info!(image = %image_path.display(), dir = %mount_dir.display(), "backup mounted ({})", mode_str);
    active_mounts().lock().unwrap().insert(
        image_path.to_path_buf(),
        ActiveMount {
            nbd,
            mount_dir: mount_dir.clone(),
        },
    );
    Ok(mount_dir.display().to_string())
}

/// Unmount a previously mounted image and release its NBD device.
pub fn unmount_image(image_path: &Path) -> Result<()> {
    let entry = active_mounts().lock().unwrap().remove(image_path);
    let Some(entry) = entry else {
        return Err(OwnBackupError::Config(format!(
            "{} is not mounted by this service",
            image_path.display()
        )));
    };

    let out = std::process::Command::new("umount")
        .arg(&entry.mount_dir)
        .output()
        .map_err(|e| OwnBackupError::Device(format!("failed to run umount: {e}")))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        // Put the entry back so a later retry can still find it.
        active_mounts()
            .lock()
            .unwrap()
            .insert(image_path.to_path_buf(), entry);
        return Err(OwnBackupError::Device(format!("umount failed: {err}")));
    }

    entry.nbd.detach()?;
    let _ = std::fs::remove_dir(&entry.mount_dir);
    info!(image = %image_path.display(), "backup unmounted");
    Ok(())
}

pub fn get_all_active_mounts(
    internal_mounts: &[own_backup_common::engine_dto::ActiveMountDto],
) -> Vec<own_backup_common::engine_dto::ActiveMountDto> {
    internal_mounts.to_vec()
}

/// Result of verifying an image against its sidecar checksums.
#[derive(Debug)]
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
    progress: impl FnMut(u64, u64) -> Result<()>,
) -> Result<VerifySummary> {
    verify_image_with_keys(image_path, None, progress)
}

/// The same check for an encrypted image. With the key this is the full check
/// §7.4 asks for — the record tags and the authenticated index decide whether
/// the bytes are ours before their checksums are even computed.
pub fn verify_image_with_keys(
    image_path: &Path,
    keys: Option<&crate::seal::context::ArchiveKeys>,
    mut progress: impl FnMut(u64, u64) -> Result<()>,
) -> Result<VerifySummary> {
    let (sidecar, chain_key) = read_sidecar_for(image_path, keys)?;
    let block_size = sidecar.manifest.block_size as u64;
    let volume_len = sidecar.manifest.source_disk.size_bytes;
    let data_offset = sidecar.info.volume_data_offset;

    let mut dev =
        LinuxImageReader::open_with(image_path, &sidecar.info.container, chain_key.as_ref())?;
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

    Ok(VerifySummary {
        blocks_checked: checked,
        mismatched_blocks: mismatched,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_progress(_p: &BackupProgress) -> Result<()> {
        Ok(())
    }

    /// The keys a test job works with. The minimum production KDF profile, not
    /// the unit-test one: this module is outside `seal`, and using the real
    /// profile also proves the ordinary library path works.
    fn test_keys() -> crate::seal::context::ArchiveKeys {
        use crate::seal::keypair::{JobKeyPair, KdfParams, SealedPrivateKey};
        use crate::seal::recovery::RecoveryKey;
        let pair = JobKeyPair::generate().unwrap();
        let recovery = RecoveryKey::generate().unwrap();
        let kdf = KdfParams {
            algorithm: "argon2id".into(),
            m_kib: 64 * 1024,
            t: 3,
            p: 1,
        };
        let envelope =
            SealedPrivateKey::seal("job-encrypted", &pair, "пароль архива", &recovery, &kdf)
                .unwrap();
        crate::seal::context::ArchiveKeys::from_private(envelope, pair.private).unwrap()
    }

    /// A whole encrypted backup, end to end: the image is not readable without
    /// the key, holds none of the source's plaintext, is listed as a chain
    /// without any key at all, and restores byte-for-byte with one.
    #[test]
    fn an_encrypted_backup_round_trips_and_is_unreadable_without_its_key() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        // Repeating marker text: the friendliest possible case for anyone
        // grepping the image afterwards.
        let content: Vec<u8> = b"secret-payload--".repeat(4 * 1024 * 1024 / 16);
        std::fs::write(&src, &content).unwrap();

        let img = dir.path().join("full.obk");
        let summary = backup_volume(
            &LinuxBackupRequest {
                source: src.to_str().unwrap().to_string(),
                image_path: img.clone(),
                block_size: DEFAULT_BLOCK_SIZE,
                all_sectors: false,
                passphrase: None,
                job_id: Some("job-encrypted".into()),
                image_format: ImageFormat::Qcow2,
                encryption: Some(test_keys()),
                zstd_level: crate::qcow2::DEFAULT_ZSTD_LEVEL,
            },
            no_progress,
        )
        .unwrap();
        assert!(summary.bytes_copied >= content.len() as u64);

        // Nothing of the source survives in the clear, and no plaintext
        // sidecar was written beside the image.
        let raw = std::fs::read(&img).unwrap();
        assert!(
            !raw.windows(16).any(|w| w == b"secret-payload--"),
            "plaintext data survived into the encrypted image"
        );
        assert!(
            !sidecar_path_for(&img).exists(),
            "no sidecar file beside an .obk"
        );

        // A plaintext reader refuses it, and so does verify without a key.
        assert!(crate::qcow2::Qcow2RandomReader::open(&img).is_err());
        let error = verify_image(&img, |_, _| Ok(())).unwrap_err().to_string();
        assert!(error.contains("encrypted"), "unexpected: {error}");

        // With the key the image verifies and restores byte-for-byte. The key
        // comes from the image's own portable envelope and the password — what
        // another machine or a boot medium has, and nothing else.
        let keys =
            crate::seal::context::ArchiveKeys::from_image_password(&img, "пароль архива").unwrap();
        let verified = verify_image_with_keys(&img, Some(&keys), |_, _| Ok(())).unwrap();
        assert!(
            verified.valid(),
            "mismatches: {:?}",
            verified.mismatched_blocks
        );

        let target = dir.path().join("restored.bin");
        std::fs::File::create(&target)
            .unwrap()
            .set_len(content.len() as u64)
            .unwrap();
        let restored = restore_volume(
            &LinuxRestoreRequest {
                image_path: img.clone(),
                target: target.to_str().unwrap().to_string(),
                verify_checksums: true,
                encryption: Some(
                    crate::seal::context::ArchiveKeys::from_image_password(&img, "пароль архива")
                        .unwrap(),
                ),
            },
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(restored.blocks_restored, summary.used_blocks);
        assert_eq!(std::fs::read(&target).unwrap(), content);

        // A listing sees the chain without any key.
        let chains = crate::chains::list_job_chains(dir.path(), "job-encrypted").unwrap();
        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0].images.len(), 1);
        assert_eq!(chains[0].images[0].container, "obk");

        // Another key of the same job name opens nothing, and neither does a
        // wrong password.
        let stranger = test_keys();
        assert!(verify_image_with_keys(&img, Some(&stranger), |_, _| Ok(())).is_err());
        assert!(crate::seal::context::ArchiveKeys::from_image_password(&img, "не тот").is_err());
    }

    /// An encrypted image must be named so that listings and retention can
    /// find it at all.
    #[test]
    fn an_encrypted_image_must_carry_the_obk_extension() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        std::fs::write(&src, vec![3u8; 1 << 20]).unwrap();
        let error = backup_volume(
            &LinuxBackupRequest {
                source: src.to_str().unwrap().to_string(),
                image_path: dir.path().join("full.qcow2"),
                block_size: DEFAULT_BLOCK_SIZE,
                all_sectors: false,
                passphrase: None,
                job_id: None,
                image_format: ImageFormat::Qcow2,
                encryption: Some(test_keys()),
                zstd_level: crate::qcow2::DEFAULT_ZSTD_LEVEL,
            },
            no_progress,
        );
        let Err(error) = error else {
            panic!("an encrypted image named .qcow2 must be refused");
        };
        let error = error.to_string();
        assert!(error.contains(".obk"), "unexpected: {error}");
    }

    /// End-to-end on a real ext4 image: back up used blocks into a raw image and
    /// verify. `cargo test -p own-backup-backup -- --ignored` (needs mkfs.ext4).
    #[test]
    #[ignore]
    fn backup_and_verify_real_ext4() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.ext4");
        std::fs::File::create(&src)
            .unwrap()
            .set_len(64 * 1024 * 1024)
            .unwrap();
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
            image_format: ImageFormat::Vhdx,
            encryption: None,
            zstd_level: crate::qcow2::DEFAULT_ZSTD_LEVEL,
        };
        let sum = backup_volume(&req, no_progress).unwrap();
        assert!(sum.smart_mode);
        assert!(sum.used_blocks > 0 && sum.used_blocks < sum.total_blocks);

        let v = verify_image(&img, |_, _| Ok(())).unwrap();
        assert!(v.valid(), "verify mismatches: {:?}", v.mismatched_blocks);

        // The image's partition (at the 1 MiB GPT head) holds a recognisable
        // ext4 — proof the superblock/metadata were copied to the right place.
        let mut dev = crate::vhdx_format::VhdxReader::open(&img).unwrap();
        let mut head = vec![0u8; own_backup_inventory::fs_probe::PROBE_LEN];
        dev.read_at(DEFAULT_PARTITION_OFFSET, &mut head).unwrap();
        let probed =
            own_backup_inventory::fs_probe::identify(&head).expect("image holds a filesystem");
        assert_eq!(probed.fs, own_backup_common::types::FileSystem::Ext4);
    }

    /// Full round trip: back up a real ext4, restore to a blank target, and
    /// confirm the target is a valid ext4. `--ignored` (needs mkfs.ext4).
    #[test]
    #[ignore]
    fn backup_then_restore_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.ext4");
        std::fs::File::create(&src)
            .unwrap()
            .set_len(64 * 1024 * 1024)
            .unwrap();
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
                image_format: ImageFormat::Vhdx,
                encryption: None,
                zstd_level: crate::qcow2::DEFAULT_ZSTD_LEVEL,
            },
            no_progress,
        )
        .unwrap();

        // Restore onto a blank target sized to the volume.
        let tgt = dir.path().join("restored.bin");
        std::fs::File::create(&tgt)
            .unwrap()
            .set_len(sum.source_bytes)
            .unwrap();
        let r = restore_volume(
            &LinuxRestoreRequest {
                image_path: img,
                target: tgt.to_str().unwrap().to_string(),
                verify_checksums: true,
                encryption: None,
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

    /// Same full round trip through the **qcow2** container: backup → verify →
    /// restore → valid ext4. Also checks the sidecar records the container and
    /// that compression actually shrinks the image. `--ignored` (needs mkfs.ext4).
    #[test]
    #[ignore]
    fn qcow2_backup_verify_restore_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.ext4");
        std::fs::File::create(&src)
            .unwrap()
            .set_len(64 * 1024 * 1024)
            .unwrap();
        assert!(std::process::Command::new("mkfs.ext4")
            .args(["-q", "-F", "-L", "qcowtrip", src.to_str().unwrap()])
            .status()
            .unwrap()
            .success());

        let img = dir.path().join("out.qcow2");
        let sum = backup_volume(
            &LinuxBackupRequest {
                source: src.to_str().unwrap().to_string(),
                image_path: img.clone(),
                block_size: DEFAULT_BLOCK_SIZE,
                all_sectors: false,
                passphrase: None,
                job_id: None,
                image_format: ImageFormat::Qcow2,
                encryption: None,
                zstd_level: crate::qcow2::DEFAULT_ZSTD_LEVEL,
            },
            no_progress,
        )
        .unwrap();
        assert!(sum.smart_mode);

        // Sidecar must say qcow2 + zstd, and the image must be smaller than
        // the bytes that went in (ext4 metadata compresses well).
        let sidecar = read_embedded_sidecar(&img, "qcow2").unwrap();
        assert_eq!(sidecar.info.container, "qcow2");
        assert_eq!(
            sidecar.manifest.compression_algo,
            own_backup_common::types::CompressionAlgo::Zstd
        );
        let image_len = std::fs::metadata(&img).unwrap().len();
        assert!(
            image_len < sum.bytes_copied,
            "qcow2 image ({image_len} B) not smaller than copied data ({} B)",
            sum.bytes_copied
        );

        let v = verify_image(&img, |_, _| Ok(())).unwrap();
        assert!(v.valid(), "verify mismatches: {:?}", v.mismatched_blocks);

        let tgt = dir.path().join("restored.bin");
        std::fs::File::create(&tgt)
            .unwrap()
            .set_len(sum.source_bytes)
            .unwrap();
        let r = restore_volume(
            &LinuxRestoreRequest {
                image_path: img,
                target: tgt.to_str().unwrap().to_string(),
                verify_checksums: true,
                encryption: None,
            },
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(r.blocks_restored, sum.used_blocks);

        let probed =
            own_backup_inventory::fs_probe::probe_partition_path(tgt.to_str().unwrap()).unwrap();
        assert_eq!(probed.fs, own_backup_common::types::FileSystem::Ext4);
        assert_eq!(probed.label.as_deref(), Some("qcowtrip"));
    }

    /// The whole Q4 stack live: ext4 with a real file → qcow2 backup →
    /// in-process NBD device → `mount -o ro,loop,offset` → the file reads
    /// back byte-identical → clean unmount. Needs **root** and `nbd.ko`:
    /// `sudo <test-binary> --ignored engine_linux::tests::qcow2_mount`.
    #[test]
    #[ignore]
    fn qcow2_mount_roundtrip_via_nbd() {
        assert_eq!(unsafe { libc::geteuid() }, 0, "this test needs root");
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.ext4");
        std::fs::File::create(&src)
            .unwrap()
            .set_len(64 * 1024 * 1024)
            .unwrap();
        assert!(std::process::Command::new("mkfs.ext4")
            .args(["-q", "-F", "-L", "mounttrip", src.to_str().unwrap()])
            .status()
            .unwrap()
            .success());

        // Put a recognisable file into the source volume.
        let payload: Vec<u8> = (0..1_000_000u32).flat_map(|i| i.to_le_bytes()).collect();
        let srcmnt = dir.path().join("srcmnt");
        std::fs::create_dir(&srcmnt).unwrap();
        assert!(std::process::Command::new("mount")
            .args([
                "-o",
                "loop",
                src.to_str().unwrap(),
                srcmnt.to_str().unwrap()
            ])
            .status()
            .unwrap()
            .success());
        std::fs::write(srcmnt.join("payload.bin"), &payload).unwrap();
        assert!(std::process::Command::new("umount")
            .arg(&srcmnt)
            .status()
            .unwrap()
            .success());

        // Back up into qcow2 and mount the backup through our NBD server.
        let img = dir.path().join("backup.qcow2");
        backup_volume(
            &LinuxBackupRequest {
                source: src.to_str().unwrap().to_string(),
                image_path: img.clone(),
                block_size: DEFAULT_BLOCK_SIZE,
                all_sectors: false,
                passphrase: None,
                job_id: None,
                image_format: ImageFormat::Qcow2,
                encryption: None,
                zstd_level: crate::qcow2::DEFAULT_ZSTD_LEVEL,
            },
            no_progress,
        )
        .unwrap();

        let mount_dir = mount_image(&img, false).unwrap();
        let restored = std::fs::read(Path::new(&mount_dir).join("payload.bin")).unwrap();
        assert_eq!(
            restored, payload,
            "file read from the mounted backup differs"
        );

        // Read-only is enforced end to end.
        assert!(std::fs::write(Path::new(&mount_dir).join("x"), b"nope").is_err());

        unmount_image(&img).unwrap();
        assert!(!Path::new(&mount_dir).join("payload.bin").exists());
    }
}

/// Regression guard for audit AUD-12: the post-copy check has to answer to
/// cancel the same way the copy does.
///
/// Reading a whole image back is minutes of work, and every callback here used
/// to return `Ok(())` unconditionally — so pressing "Отмена" during "Проверка…"
/// did nothing, the read ran to the end, and the run was then reported as a
/// verified success. The engine already propagates a callback error; nothing
/// was passing one. These tests pin both halves of that: a callback that says
/// stop ends the read there, and one that does not lets it finish.
#[cfg(test)]
mod verify_cancel_tests {
    use super::*;
    use crate::sidecar::{write_sidecar, BlockChecksum, SidecarInfo};

    const BLOCK: u64 = 64 * 1024;
    const BLOCKS: u64 = 8;

    /// A tiny qcow2 with `BLOCKS` clusters of known bytes, plus the adjacent
    /// sidecar naming a checksum for each — the smallest thing `verify_image`
    /// will read.
    fn an_image_with_eight_blocks(dir: &std::path::Path) -> PathBuf {
        let image = dir.join("copy.qcow2");
        let mut checksums = Vec::new();
        {
            let file = std::fs::File::create(&image).unwrap();
            let mut w = crate::qcow2::Qcow2Writer::new(
                std::io::BufWriter::new(file),
                crate::qcow2::Qcow2WriterOptions {
                    virtual_size: BLOCK * BLOCKS,
                    ..Default::default()
                },
            )
            .unwrap();
            for i in 0..BLOCKS {
                let data = vec![(i + 1) as u8; BLOCK as usize];
                w.write_cluster(i, &data).unwrap();
                checksums.push(BlockChecksum {
                    block_number: i,
                    checksum: compute_block_checksum(&data),
                });
            }
            w.finish().unwrap();
        }

        let mut info = SidecarInfo::new_full("copy.qcow2", 0);
        info.container = "qcow2".into();
        let manifest = ImageManifest {
            backup_id: Uuid::nil(),
            created_at: "2026-09-19T00:00:00Z".into(),
            backup_type: BackupType::Full,
            parent_backup_id: None,
            source_os: "linux".into(),
            source_disk: DiskInfo {
                device_id: "/dev/null".into(),
                model: "test".into(),
                serial: "test".into(),
                size_bytes: BLOCK * BLOCKS,
                sector_size_logical: 512,
                sector_size_physical: 512,
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
            block_size: BLOCK as u32,
            total_blocks: BLOCKS,
            used_blocks: BLOCKS,
            backup_mode: BackupMode::UsedSectors,
            checksum_algorithm: "xxhash64".into(),
            preflight_report: None,
            volume_encryption: None,
        };
        write_sidecar(
            &crate::sidecar::sidecar_path_for(&image),
            &info,
            &manifest,
            &checksums,
        )
        .unwrap();
        image
    }

    #[test]
    fn a_check_stops_where_the_user_cancelled_it() {
        let dir = tempfile::tempdir().unwrap();
        let image = an_image_with_eight_blocks(dir.path());

        let mut seen = 0u64;
        let outcome = verify_image(&image, |done, _total| {
            seen = done;
            // Third block: far enough in to prove the loop was running, early
            // enough to prove it did not run to the end.
            if done == 3 {
                return Err(OwnBackupError::Cancelled);
            }
            Ok(())
        });

        assert!(
            matches!(outcome, Err(OwnBackupError::Cancelled)),
            "the cancellation must come back out, not be swallowed: {outcome:?}"
        );
        assert_eq!(
            seen, 3,
            "reading must stop at the cancelled block, not finish the image"
        );
    }

    #[test]
    fn a_check_nobody_cancels_still_reads_the_whole_image() {
        let dir = tempfile::tempdir().unwrap();
        let image = an_image_with_eight_blocks(dir.path());

        let summary = verify_image(&image, |_, _| Ok(())).unwrap();
        assert_eq!(summary.blocks_checked, BLOCKS);
        assert!(summary.valid(), "an untouched image must verify clean");
    }
}
