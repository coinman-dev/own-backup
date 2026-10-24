//! Linux volume backup — the `/dev` counterpart of [`crate::engine_win`].
//!
//! A volume is copied by the shared engine ([`crate::engine_core::volume`]) on
//! the Linux platform (`engine_platform_linux`): the pipeline, the containers,
//! the private publication and the sidecar are the ones Windows runs. What is
//! Linux's own is how the source is held — through the snapshot its caller
//! prepared (`own_backup_snapshot::linux`) — and the check, made right before
//! the image is published, that the snapshot held to the end of the copy.
//!
//! Restore, mounting through the in-process NBD server and the unlock probe
//! are here too; verification is the shared one.

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tracing::{info, warn};
use zeroize::Zeroizing;

use own_backup_common::progress::{BackupProgress, RestoreProgress};
use own_backup_common::types::BackupStatus;
use own_backup_common::{OwnBackupError, Result};
use own_backup_snapshot::Snapshot;

use crate::checksum::compute_block_checksum;
use crate::engine_platform_linux::LinuxPlatform;
use crate::linux_device::LinuxRawDevice;
use crate::sidecar::{
    read_embedded_sidecar, read_embedded_sidecar_header, read_sidecar, sidecar_path_for,
};

pub use crate::engine_core::disk_image::{DiskBackupRequest, DiskPartitionBackup};
pub use crate::engine_core::disk_target::DiskBackupSummary;
pub use crate::engine_core::target::DEFAULT_BLOCK_SIZE;
pub use crate::engine_core::volume::{VolumeBackupRequest, VolumeBackupSummary};

/// Back up the volume `req.source` through `snapshot`: the point in time its
/// caller prepared for it (`LinuxSnapshotProvider::create_snapshot` of
/// `req.source`). `req.source` names the device itself — the image records
/// who it is from that — and the snapshot is what is read.
///
/// The image is written under a private name and published only when the
/// whole copy is done, the caller's progress callback (asked last, with
/// `backup_id == "publish"`) still wants it, and the snapshot is still whole.
/// An overflowed snapshot, or a partition thawed or remounted read-write
/// meanwhile, means what was read is not one point in time: the private image
/// is dropped and the snapshot's error returned. The snapshot stays the
/// caller's: it releases it after this returns, whatever the outcome, and
/// before the image is read back.
pub fn backup_volume(
    req: &VolumeBackupRequest,
    snapshot: &Snapshot,
    progress: impl FnMut(&BackupProgress) -> Result<()>,
) -> Result<VolumeBackupSummary> {
    backup_volume_checked(
        req,
        snapshot,
        own_backup_snapshot::linux::check_intact_blocking,
        progress,
    )
}

/// [`backup_volume`] with the intactness check passed in.
fn backup_volume_checked(
    req: &VolumeBackupRequest,
    snapshot: &Snapshot,
    mut check_intact: impl FnMut(&Snapshot) -> Result<()>,
    mut progress: impl FnMut(&BackupProgress) -> Result<()>,
) -> Result<VolumeBackupSummary> {
    let platform = LinuxPlatform::new([snapshot.clone()]);
    let mut asked_at_publication = false;
    let copied = crate::engine_core::volume::backup_volume(&platform, req, |p| {
        progress(p)?;
        if p.backup_id == "publish" {
            asked_at_publication = true;
            check_intact(snapshot)?;
            info!(source = %snapshot.source_device, "the snapshot held to the end of the copy");
        }
        Ok(())
    });
    if asked_at_publication {
        return copied;
    }
    explain_broken_snapshot(copied, std::slice::from_ref(snapshot), &mut check_intact)
}

/// A copy that failed before its publication while a snapshot it read broke
/// under it is reported as the snapshot's failure (one that reached the
/// publication was asked there already). device-mapper invalidates an overflowed snapshot,
/// and every read of it then fails with EIO: the run said only
/// `read(/dev/dm-2) at 74284269568: Input/output error` (owner, 24.10), where
/// the snapshot's own error says what happened and what to do about it.
fn explain_broken_snapshot<T>(
    copied: Result<T>,
    snapshots: &[Snapshot],
    check_intact: &mut impl FnMut(&Snapshot) -> Result<()>,
) -> Result<T> {
    match copied {
        Err(e) if !matches!(e, OwnBackupError::Cancelled) => {
            for snapshot in snapshots {
                if let Err(broken) = check_intact(snapshot) {
                    warn!(
                        source = %snapshot.source_device,
                        error = %e,
                        "the copy failed with its snapshot broken: reported as the snapshot's failure"
                    );
                    return Err(broken);
                }
            }
            Err(e)
        }
        other => other,
    }
}

/// Back up a whole disk (`req.disk_device`, a `/dev/disk/by-id/…` path — R1)
/// into one image with its GPT and restore manifest, each partition read
/// through its snapshot in `snapshots` (one per `req.partitions[].device`,
/// taken by the caller up front, so the partitions are copied as close to
/// one point in time as Linux allows). A partition without one is refused.
///
/// As for a volume, the image is published only when every snapshot is
/// still whole at the end of the copy; the snapshots stay the caller's to
/// release, after this returns and before the image is read back.
pub fn backup_disk(
    req: &DiskBackupRequest,
    snapshots: &[Snapshot],
    progress: impl FnMut(&BackupProgress) -> Result<()>,
) -> Result<DiskBackupSummary> {
    backup_disk_checked(
        req,
        snapshots,
        own_backup_snapshot::linux::check_intact_blocking,
        progress,
    )
}

/// [`backup_disk`] with the intactness check passed in.
fn backup_disk_checked(
    req: &DiskBackupRequest,
    snapshots: &[Snapshot],
    mut check_intact: impl FnMut(&Snapshot) -> Result<()>,
    mut progress: impl FnMut(&BackupProgress) -> Result<()>,
) -> Result<DiskBackupSummary> {
    if req.disk_device.is_none() {
        return Err(OwnBackupError::Config(
            "a whole disk on Linux is named by its device (/dev/disk/by-id/…), not by a number"
                .into(),
        ));
    }
    let platform = LinuxPlatform::new(snapshots.iter().cloned());
    let mut asked_at_publication = false;
    let copied = crate::engine_core::disk::backup_disk(&platform, req, |p| {
        progress(p)?;
        if p.backup_id == "publish" {
            asked_at_publication = true;
            for snapshot in snapshots {
                check_intact(snapshot)?;
            }
            info!(
                partitions = snapshots.len(),
                "every partition's snapshot held to the end of the copy"
            );
        }
        Ok(())
    });
    if asked_at_publication {
        return copied;
    }
    explain_broken_snapshot(copied, snapshots, &mut check_intact)
}

/// The reader of a finished image, picked by the sidecar's `container` field.
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

    // An OPENED LUKS container (`/dev/mapper/…`) reads as plain data, but its
    // password still matters: a device-mapper snapshot of it needs the
    // container key, which after `cryptsetup open` lives only in the kernel.
    // Asked of the header on the device underneath, or every password would
    // "open" it.
    if let Some(container) = opened_luks_container(device) {
        let mut under = LinuxRawDevice::open_read(&container)?;
        match crate::luks::open_luks(&mut under, passphrase.as_bytes()) {
            Ok(_) => {}
            Err(e) if crate::luks::is_wrong_passphrase(&e) => {
                info!(error = %e, "LUKS passphrase rejected");
                return Ok(DecryptedProbe::default());
            }
            Err(e) => return Err(e),
        }
        return probe_decrypted(&mut LinuxRawDevice::open_read(device)?);
    }

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

/// The volume key of the LUKS container on `device`, recovered with
/// `passphrase` — for a device-mapper snapshot of the container's opened
/// mapping when the kernel keeps the mapping's key to itself (the snapshot
/// crate's `ContainerKey`). A device that is not LUKS is an error, so is a
/// wrong passphrase.
pub fn luks_volume_key(device: &Path, passphrase: &str) -> Result<Zeroizing<Vec<u8>>> {
    let path = device
        .to_str()
        .ok_or_else(|| OwnBackupError::Device(format!("{} is not UTF-8", device.display())))?;
    let mut dev = LinuxRawDevice::open_read(path)?;
    let mut header = vec![0u8; 4096];
    dev.read_at(0, &mut header)?;
    if !crate::luks::is_luks(&header) {
        return Err(OwnBackupError::Device(format!(
            "{path} has no LUKS header (a detached header is not supported)"
        )));
    }
    Ok(crate::luks::open_luks(&mut dev, passphrase.as_bytes())?.master_key)
}

/// A snapshot crate `ContainerKey` from the password the run has for its
/// source, if any: without one the snapshot says it needs it.
pub fn container_key_from(
    passphrase: Option<Zeroizing<String>>,
) -> impl Fn(&Path) -> Result<Option<Zeroizing<Vec<u8>>>> + Send + Sync + 'static {
    move |device: &Path| match &passphrase {
        Some(p) => luks_volume_key(device, p).map(Some),
        None => Ok(None),
    }
}

/// Whether `device` itself starts with a LUKS or BitLocker header: the engine
/// then decrypts it while copying and needs the passphrase before it starts.
/// An unreadable device answers `false`; the copy reports the real error.
pub fn has_encryption_header(device: &str) -> bool {
    let Ok(dev) = LinuxRawDevice::open_read(device) else {
        return false;
    };
    let mut header = vec![0u8; 4096];
    if dev.read_at(0, &mut header).is_err() {
        return false;
    }
    crate::luks::is_luks(&header) || crate::bitlocker::is_bitlocker(&header)
}

/// Whether `device` is an opened LUKS mapping (`/dev/mapper/…` over one LUKS
/// container). Its data reads as plain, but a device-mapper snapshot of it
/// needs the container's password (the key lives only in the kernel).
pub fn is_opened_luks_mapping(device: &str) -> bool {
    opened_luks_container(device).is_some()
}

/// The LUKS container under an opened mapping: `/dev/<slave>` when `device`
/// is a dm-crypt device whose device-mapper UUID says LUKS (`CRYPT-LUKS1-…`,
/// `CRYPT-LUKS2-…`) and it sits on exactly one device.
fn opened_luks_container(device: &str) -> Option<String> {
    use own_backup_common::linux_block as lb;
    let name = lb::kernel_name(std::path::Path::new(device))?;
    let sys = std::path::Path::new(lb::SYS_CLASS_BLOCK);
    let uuid = std::fs::read_to_string(sys.join(&name).join("dm/uuid")).ok()?;
    if !uuid.trim().starts_with("CRYPT-LUKS") {
        return None;
    }
    let slaves: Vec<String> = std::fs::read_dir(sys.join(&name).join("slaves"))
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    match slaves.as_slice() {
        [one] => Some(format!("/dev/{one}")),
        _ => None,
    }
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
    // A copy of a whole disk restored as a volume wrote only some of its
    // blocks and said "Restore complete" — a disk with no partition table
    // (stand, 2026-10-23). It has its own path, which lays the table out.
    if sidecar.disk_manifest.is_some() {
        return Err(OwnBackupError::RestoreIncompatible(
            own_backup_common::tr!(
                "engine_linux.whole_disk_as_volume",
                image = req.image_path.display()
            )
            .into_owned(),
        ));
    }
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
    /// Every file the mount reads: the image and the parents behind it.
    reads: Vec<PathBuf>,
}

/// The files a read of `image` goes through: the image itself, then each
/// parent its container names (qcow2 backing file, VHDX parent locator), each
/// resolved against the folder of the image that names it. Canonical paths,
/// leaf first; a parent that cannot be read or found ends the walk there.
fn chain_files(image: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut current = Some(image.to_path_buf());
    while let Some(path) = current.take() {
        let here = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if out.contains(&here) || out.len() >= 64 {
            break;
        }
        out.push(here);
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        current = match ext.as_str() {
            "qcow2" => std::fs::File::open(&path)
                .ok()
                .and_then(|f| crate::qcow2::Qcow2Reader::open(f).ok())
                .and_then(|r| r.backing_file().map(PathBuf::from))
                .map(|b| match path.parent() {
                    Some(folder) if b.is_relative() => folder.join(b),
                    _ => b,
                }),
            "vhdx" | "avhdx" => crate::vhdx_bat::vhdx_parent_path(&path).ok().flatten(),
            _ => None,
        };
    }
    out
}

/// The mounted image, served by this process, that reads any of `images` —
/// itself or as one of its parents. Consolidation must not rewrite a file a
/// mount is reading through (`consolidate::ensure_idle`).
pub(crate) fn mounted_reading(images: &[&Path]) -> Option<PathBuf> {
    let mounts = active_mounts().lock().unwrap();
    first_reading(
        mounts.iter().map(|(image, m)| (image, &m.reads[..])),
        images,
    )
}

/// Of `mounts` (mounted image, the files it reads), the first that reads any
/// of `images`.
fn first_reading<'a>(
    mut mounts: impl Iterator<Item = (&'a PathBuf, &'a [PathBuf])>,
    images: &[&Path],
) -> Option<PathBuf> {
    let wanted: Vec<PathBuf> = images
        .iter()
        .map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()))
        .collect();
    mounts
        .find(|(_, reads)| reads.iter().any(|r| wanted.contains(r)))
        .map(|(image, _)| image.clone())
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
    mount_image_with_options(image_path, read_write, "")
}

/// [`mount_image`] with extra mount options (`nouuid` for an XFS copy beside
/// the original, from `identity_check_linux::mount_options_for`).
pub fn mount_image_with_options(
    image_path: &Path,
    read_write: bool,
    extra: &str,
) -> Result<String> {
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
            "{},loop,offset={}{}{}",
            mode_str,
            info.volume_data_offset,
            if extra.is_empty() { "" } else { "," },
            extra
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
            reads: chain_files(image_path),
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

// Verification is the engine-independent one (`engine_core::verify`): the
// same reads as on Windows, the same re-read before a block is called damaged,
// and the same "latest copy only" scope.
pub use crate::engine_core::verify::{
    verify_image, verify_image_scoped, verify_image_with_keys, VerifyScope, VerifySummary,
};

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use own_backup_common::types::{BackupMode, BackupType, FileSystem, ImageFormat};

    use super::*;
    use crate::gpt_writer::DEFAULT_PARTITION_OFFSET;

    const MIB: u64 = 1024 * 1024;

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

    /// The point in time of `source` as the snapshot crate describes one:
    /// `mode` as it decided, the data read at `read`.
    fn snapshot_of(source: &str, read: &Path, mode: &str) -> Snapshot {
        Snapshot {
            provider_name: "linux".into(),
            source_device: source.into(),
            snapshot_device_path: read.to_str().unwrap().into(),
            created_at: chrono::Utc::now(),
            metadata: HashMap::from([("mode".to_string(), mode.to_string())]),
        }
    }

    /// A read that failed under a broken snapshot is the snapshot's failure;
    /// a cancel, a success and a failure under a whole snapshot are left as
    /// they are (24.10: `read(/dev/dm-2) … Input/output error` over an
    /// overflowed one).
    #[test]
    fn a_failure_under_a_broken_snapshot_is_reported_as_the_snapshot() {
        let snap = snapshot_of("/dev/root", Path::new("/dev/dm-2"), "dm");
        let eio = || -> Result<()> {
            Err(OwnBackupError::Io(std::io::Error::from_raw_os_error(
                libc::EIO,
            )))
        };
        let mut broken = |_: &Snapshot| -> Result<()> {
            Err(OwnBackupError::NoSnapshotProvider("overflowed".into()))
        };
        let got = explain_broken_snapshot(eio(), std::slice::from_ref(&snap), &mut broken);
        assert!(
            matches!(&got, Err(OwnBackupError::NoSnapshotProvider(m)) if m == "overflowed"),
            "{got:?}"
        );
        let cancelled = explain_broken_snapshot::<()>(
            Err(OwnBackupError::Cancelled),
            std::slice::from_ref(&snap),
            &mut broken,
        );
        assert!(matches!(cancelled, Err(OwnBackupError::Cancelled)));
        let mut asked = 0;
        let mut whole = |_: &Snapshot| -> Result<()> {
            asked += 1;
            Ok(())
        };
        assert!(explain_broken_snapshot(Ok(()), std::slice::from_ref(&snap), &mut whole).is_ok());
        let got = explain_broken_snapshot(eio(), std::slice::from_ref(&snap), &mut whole);
        assert!(
            matches!(got, Err(OwnBackupError::Io(_))),
            "a whole snapshot: the read's own error"
        );
        assert_eq!(asked, 1, "a success asks nothing");
    }

    /// An image file as a source, read as it is; `vouched`: its caller says
    /// nothing writes it meanwhile (decision R5), so its metadata may be read.
    fn file_snapshot(path: &Path, vouched: bool) -> Snapshot {
        let mut s = snapshot_of(path.to_str().unwrap(), path, "file");
        if vouched {
            own_backup_snapshot::linux::assert_file_frozen(&mut s);
        }
        s
    }

    fn request(source: &Path, image: &Path, format: ImageFormat) -> VolumeBackupRequest {
        VolumeBackupRequest {
            source: source.to_str().unwrap().into(),
            image_path: image.to_path_buf(),
            full_image_path: None,
            block_size: DEFAULT_BLOCK_SIZE,
            all_sectors: false,
            use_vss: true,
            kind: BackupType::Full,
            parent_image: None,
            exclusion_patterns: Vec::new(),
            exclusion_patterns_linux: Vec::new(),
            job_id: None,
            passphrase: None,
            image_format: format,
            source_disk: None,
            encryption: None,
            zstd_level: crate::qcow2::DEFAULT_ZSTD_LEVEL,
        }
    }

    /// A sparse `size`-byte file at `path`.
    fn blank(path: &Path, size: u64) {
        std::fs::File::create(path).unwrap().set_len(size).unwrap();
    }

    /// Run `tool args… path`; `false` when the tool is not installed, so a
    /// test can say it was skipped instead of failing for want of mkfs.
    fn mkfs(tool: &str, args: &[&str], path: &Path) -> bool {
        match std::process::Command::new(tool)
            .args(args)
            .arg(path)
            .output()
        {
            Ok(out) => {
                assert!(
                    out.status.success(),
                    "{tool} failed: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
                true
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("skipped: {tool} is not installed");
                false
            }
            Err(e) => panic!("{tool}: {e}"),
        }
    }

    /// A 64 MiB ext4 holding a few known files, made without mounting
    /// (`mkfs.ext4 -d`). `None` without mkfs.ext4.
    fn ext4_volume(dir: &Path, label: &str) -> Option<PathBuf> {
        let content = dir.join("content");
        std::fs::create_dir_all(content.join("nested")).unwrap();
        let payload: Vec<u8> = (0..700_000u32).flat_map(|i| i.to_le_bytes()).collect();
        std::fs::write(content.join("payload.bin"), &payload).unwrap();
        std::fs::write(content.join("nested/hello.txt"), b"Own Backup on Linux\n").unwrap();
        let src = dir.join(format!("{label}.ext4"));
        blank(&src, 64 * MIB);
        let d = content.to_str().unwrap().to_string();
        mkfs("mkfs.ext4", &["-q", "-F", "-L", label, "-d", &d], &src).then_some(src)
    }

    /// Restore `image` onto a blank file the size of the volume it holds.
    fn restore_to_blank(
        image: &Path,
        size: u64,
        keys: Option<crate::seal::context::ArchiveKeys>,
    ) -> (PathBuf, LinuxRestoreSummary) {
        let target = image.with_extension("restored");
        blank(&target, size);
        let summary = restore_volume(
            &LinuxRestoreRequest {
                image_path: image.to_path_buf(),
                target: target.to_str().unwrap().into(),
                verify_checksums: true,
                encryption: keys,
            },
            |_| Ok(()),
        )
        .unwrap();
        (target, summary)
    }

    /// Every name in `dir` but the sources the test made there.
    fn images_in(dir: &Path, sources: &[&Path]) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| !sources.contains(&p.as_path()))
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
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
        let req = VolumeBackupRequest {
            job_id: Some("job-encrypted".into()),
            image_format: ImageFormat::Qcow2,
            encryption: Some(test_keys()),
            ..request(&src, &img, ImageFormat::Qcow2)
        };
        let summary = backup_volume(&req, &file_snapshot(&src, false), no_progress).unwrap();
        assert!(summary.bytes_copied >= content.len() as u64);
        assert_eq!(summary.image_path, img);

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

        // A plaintext reader refuses it, and so does verify without a key. By
        // kind, not by wording: the message is in the user's language.
        assert!(crate::qcow2::Qcow2RandomReader::open(&img).is_err());
        let error = verify_image(&img, |_, _| Ok(())).unwrap_err();
        assert!(
            matches!(error, OwnBackupError::Encryption(_)),
            "unexpected: {error}"
        );

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
        let (target, restored) = restore_to_blank(&img, content.len() as u64, Some(keys));
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
        let img = dir.path().join("full.qcow2");
        let req = VolumeBackupRequest {
            encryption: Some(test_keys()),
            ..request(&src, &img, ImageFormat::Qcow2)
        };
        let Err(error) = backup_volume(&req, &file_snapshot(&src, false), no_progress) else {
            panic!("an encrypted image named .qcow2 must be refused");
        };
        let error = error.to_string();
        assert!(error.contains(".obk"), "unexpected: {error}");
        assert!(images_in(dir.path(), &[&src]).is_empty());
    }

    /// An ext4 read through a snapshot is copied by its used map into a VHDX
    /// with Windows' geometry, verifies, and records the device it is a copy
    /// OF — not the snapshot it was read from.
    #[test]
    fn ext4_through_a_snapshot_is_copied_by_its_used_map() {
        let dir = tempfile::tempdir().unwrap();
        let Some(src) = ext4_volume(dir.path(), "smart") else {
            return;
        };
        let origin = "/dev/obk-test/origin";
        let img = dir.path().join("out.vhdx");
        let req = VolumeBackupRequest {
            source: origin.into(),
            job_id: Some("job-ext4".into()),
            ..request(&src, &img, ImageFormat::Vhdx)
        };
        let mut phases = Vec::new();
        let sum = backup_volume(&req, &snapshot_of(origin, &src, "lvm"), |p| {
            if p.backup_id.len() < 16 {
                phases.push(p.backup_id.clone());
            }
            Ok(())
        })
        .unwrap();
        assert!(sum.smart_mode && sum.vss_used);
        assert_eq!(sum.backup_kind, BackupType::Full);
        assert!(sum.used_blocks > 0 && sum.used_blocks < sum.total_blocks);
        assert_eq!(sum.total_blocks, 64);
        assert_eq!(
            sum.file_scan_skipped.as_deref(),
            Some(&*crate::engine_core::target::no_patterns())
        );
        assert_eq!(phases.last().map(String::as_str), Some("publish"));

        let v = verify_image(&img, |_, _| Ok(())).unwrap();
        assert!(v.valid(), "verify mismatches: {:?}", v.mismatched_blocks);

        let sidecar = read_embedded_sidecar(&img, "vhdx").unwrap();
        assert_eq!(sidecar.manifest.source_os, "linux");
        assert_eq!(sidecar.manifest.source_disk.device_id, origin);
        assert_eq!(sidecar.manifest.backup_mode, BackupMode::UsedSectors);
        assert_eq!(sidecar.info.container, "vhdx");
        assert_eq!(sidecar.info.filesystem.as_deref(), Some("ext4"));
        assert_eq!(sidecar.info.job_id.as_deref(), Some("job-ext4"));

        // 1 MiB payload blocks, as CreateVirtualDisk makes them on Windows.
        let alloc = crate::vhdx_bat::allocated_payload_blocks(&img).unwrap();
        assert_eq!(alloc.block_size, MIB);

        // The image's partition (at the 1 MiB GPT head) holds a recognisable
        // ext4 — the superblock and metadata were copied to the right place.
        let mut dev = crate::vhdx_format::VhdxReader::open(&img).unwrap();
        let mut head = vec![0u8; own_backup_inventory::fs_probe::PROBE_LEN];
        dev.read_at(DEFAULT_PARTITION_OFFSET, &mut head).unwrap();
        let probed =
            own_backup_inventory::fs_probe::identify(&head).expect("image holds a filesystem");
        assert_eq!(probed.fs, FileSystem::Ext4);
    }

    /// Back up a real ext4, restore it onto a blank target: the target is the
    /// source byte for byte — every block the filesystem uses was copied, and
    /// the rest was never written on either side.
    #[test]
    fn ext4_round_trips_through_vhdx_and_qcow2() {
        for format in [ImageFormat::Vhdx, ImageFormat::Qcow2] {
            let dir = tempfile::tempdir().unwrap();
            let Some(src) = ext4_volume(dir.path(), "roundtrip") else {
                return;
            };
            let img = dir.path().join(format!("out.{}", format.extension()));
            let sum = backup_volume(
                &request(&src, &img, format),
                &file_snapshot(&src, true),
                no_progress,
            )
            .unwrap();
            assert!(sum.smart_mode, "{format}");
            assert!(!sum.vss_used, "a file is read as it is");
            assert!(sum.used_blocks < sum.total_blocks);

            let sidecar = read_embedded_sidecar(&img, format.extension()).unwrap();
            assert_eq!(sidecar.info.container, format.extension());
            if format == ImageFormat::Qcow2 {
                assert_eq!(
                    sidecar.manifest.compression_algo,
                    own_backup_common::types::CompressionAlgo::Zstd
                );
                // ext4 metadata compresses well.
                let image_len = std::fs::metadata(&img).unwrap().len();
                assert!(
                    image_len < sum.bytes_copied,
                    "{image_len} >= {}",
                    sum.bytes_copied
                );
            }
            let v = verify_image(&img, |_, _| Ok(())).unwrap();
            assert!(v.valid(), "{format}: {:?}", v.mismatched_blocks);

            let (target, restored) = restore_to_blank(&img, sum.source_bytes, None);
            assert_eq!(restored.blocks_restored, sum.used_blocks);
            assert!(
                std::fs::read(&target).unwrap() == std::fs::read(&src).unwrap(),
                "{format}: the restored volume differs from the source"
            );
            let probed =
                own_backup_inventory::fs_probe::probe_partition_path(target.to_str().unwrap())
                    .unwrap();
            assert_eq!(probed.fs, FileSystem::Ext4);
            assert_eq!(probed.label.as_deref(), Some("roundtrip"));
        }
    }

    /// FAT32, made by mkfs.vfat: copied by its own map, restored byte for
    /// byte.
    #[test]
    fn fat32_round_trips_through_qcow2() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.fat32");
        blank(&src, 64 * MIB);
        if !mkfs(
            "mkfs.vfat",
            &["-F", "32", "-S", "512", "-s", "1", "-n", "OBKFAT"],
            &src,
        ) {
            return;
        }
        let img = dir.path().join("out.qcow2");
        let sum = backup_volume(
            &request(&src, &img, ImageFormat::Qcow2),
            &file_snapshot(&src, true),
            no_progress,
        )
        .unwrap();
        assert!(sum.smart_mode);
        assert!(sum.used_blocks < sum.total_blocks);
        let sidecar = read_embedded_sidecar(&img, "qcow2").unwrap();
        assert_eq!(sidecar.info.filesystem.as_deref(), Some("FAT32"));
        assert!(verify_image(&img, |_, _| Ok(())).unwrap().valid());

        let (target, restored) = restore_to_blank(&img, sum.source_bytes, None);
        assert_eq!(restored.blocks_restored, sum.used_blocks);
        assert!(std::fs::read(&target).unwrap() == std::fs::read(&src).unwrap());
        let probed =
            own_backup_inventory::fs_probe::probe_partition_path(target.to_str().unwrap()).unwrap();
        assert_eq!(probed.fs, FileSystem::Fat32);
    }

    /// Nobody vouched that the file holds still: its metadata is not parsed,
    /// every block is copied, and the result says why (decision R5).
    #[test]
    fn a_source_that_may_change_is_copied_whole() {
        let dir = tempfile::tempdir().unwrap();
        let Some(src) = ext4_volume(dir.path(), "unfrozen") else {
            return;
        };
        let img = dir.path().join("out.qcow2");
        let sum = backup_volume(
            &request(&src, &img, ImageFormat::Qcow2),
            &file_snapshot(&src, false),
            no_progress,
        )
        .unwrap();
        assert!(!sum.smart_mode);
        assert_eq!(sum.used_blocks, sum.total_blocks);
        assert_eq!(
            sum.file_scan_skipped.as_deref(),
            Some(&*own_backup_common::tr!("engine.skip_not_frozen"))
        );
        let sidecar = read_embedded_sidecar(&img, "qcow2").unwrap();
        assert_eq!(sidecar.manifest.backup_mode, BackupMode::SectorBySector);
        assert_eq!(sidecar.info.filesystem, None, "nothing parsed it");
        assert!(verify_image(&img, |_, _| Ok(())).unwrap().valid());
        let (target, _) = restore_to_blank(&img, sum.source_bytes, None);
        assert!(std::fs::read(&target).unwrap() == std::fs::read(&src).unwrap());
    }

    /// A mount reads its image and every parent behind it; consolidation
    /// stays off all of them, and off nothing else.
    #[test]
    fn a_mounted_child_holds_its_whole_chain() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.raw");
        std::fs::write(&src, vec![7u8; 4 * MIB as usize]).unwrap();
        let snap = file_snapshot(&src, true);
        let full = dir.path().join("full.qcow2");
        backup_volume(
            &request(&src, &full, ImageFormat::Qcow2),
            &snap,
            no_progress,
        )
        .unwrap();
        let mut parent = full.clone();
        let mut chain = vec![full.clone()];
        for n in 1..=2 {
            let inc = dir.path().join(format!("inc{n}.qcow2"));
            backup_volume(
                &VolumeBackupRequest {
                    kind: BackupType::Incremental,
                    parent_image: Some(parent.clone()),
                    ..request(&src, &inc, ImageFormat::Qcow2)
                },
                &snap,
                no_progress,
            )
            .unwrap();
            chain.push(inc.clone());
            parent = inc;
        }
        let canon = |p: &Path| std::fs::canonicalize(p).unwrap();
        let reads = chain_files(&chain[2]);
        assert_eq!(
            reads,
            vec![canon(&chain[2]), canon(&chain[1]), canon(&chain[0])]
        );
        assert_eq!(chain_files(&chain[0]), vec![canon(&chain[0])]);

        let mounted = chain[1].clone();
        let mid_reads = chain_files(&mounted);
        let mounts = || std::iter::once((&mounted, &mid_reads[..]));
        assert_eq!(first_reading(mounts(), &[&chain[0]]), Some(mounted.clone()));
        assert_eq!(first_reading(mounts(), &[&chain[1]]), Some(mounted.clone()));
        // A later child is not read by the mount of its parent.
        assert_eq!(first_reading(mounts(), &[&chain[2]]), None);
        assert_eq!(first_reading(std::iter::empty(), &[&chain[0]]), None);
        // And nothing is mounted by this test process.
        assert!(crate::consolidate::ensure_idle(&[&chain[0], &chain[2]]).is_ok());
    }

    /// A qcow2 chain on Linux (plan L2): the increment holds only the blocks
    /// that changed, and the chain restores to the source as it is now, byte
    /// for byte. A VHDX job and a block smaller than a qcow2 cluster get a
    /// full copy instead of a child.
    #[test]
    fn a_qcow2_increment_holds_the_changes_and_restores_the_chain() {
        use std::os::unix::fs::FileExt;
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.raw");
        let data: Vec<u8> = (0..(8 * MIB) as u32).map(|i| (i % 249) as u8).collect();
        std::fs::write(&src, &data).unwrap();
        let snap = file_snapshot(&src, true);
        let full = dir.path().join("full.qcow2");
        let first = backup_volume(
            &request(&src, &full, ImageFormat::Qcow2),
            &snap,
            no_progress,
        )
        .unwrap();
        assert_eq!(first.backup_kind, BackupType::Full);

        // Two blocks change: one in the middle, one at the very end.
        let file = std::fs::OpenOptions::new().write(true).open(&src).unwrap();
        file.write_all_at(&[0xEE; 4096], 3 * MIB + 100).unwrap();
        file.write_all_at(&[0x11; 512], 8 * MIB - 512).unwrap();
        drop(file);

        let inc = dir.path().join("inc.qcow2");
        let second = backup_volume(
            &VolumeBackupRequest {
                kind: BackupType::Incremental,
                parent_image: Some(full.clone()),
                ..request(&src, &inc, ImageFormat::Qcow2)
            },
            &snap,
            no_progress,
        )
        .unwrap();
        assert_eq!(second.backup_kind, BackupType::Incremental);
        assert_eq!(second.blocks_copied, 2);
        assert_eq!(second.blocks_unchanged, second.used_blocks - 2);
        let sidecar = read_embedded_sidecar(&inc, "qcow2").unwrap();
        assert_eq!(
            sidecar.info.chain_role,
            crate::sidecar::ChainRole::Incremental
        );
        assert_eq!(
            sidecar.info.parent_image_file_name.as_deref(),
            Some("full.qcow2")
        );
        assert!(verify_image(&inc, |_, _| Ok(())).unwrap().valid());
        let (target, _) = restore_to_blank(&inc, second.source_bytes, None);
        assert!(std::fs::read(&target).unwrap() == std::fs::read(&src).unwrap());
        // The parent still restores to the source as it was.
        let (target, _) = restore_to_blank(&full, first.source_bytes, None);
        assert!(std::fs::read(&target).unwrap() == data);

        // VHDX: a differencing child written in userspace (CHECKS 12).
        let vfull = dir.path().join("full.vhdx");
        backup_volume(
            &request(&src, &vfull, ImageFormat::Vhdx),
            &snap,
            no_progress,
        )
        .unwrap();
        let vinc = dir.path().join("inc.avhdx");
        let v = backup_volume(
            &VolumeBackupRequest {
                kind: BackupType::Incremental,
                parent_image: Some(vfull.clone()),
                ..request(&src, &vinc, ImageFormat::Vhdx)
            },
            &snap,
            no_progress,
        )
        .unwrap();
        assert_eq!(v.backup_kind, BackupType::Incremental);
        assert_eq!(
            read_embedded_sidecar(&v.image_path, "vhdx")
                .unwrap()
                .info
                .parent_image_file_name
                .as_deref(),
            Some("full.vhdx")
        );
        assert!(verify_image(&v.image_path, |_, _| Ok(())).unwrap().valid());
        let (target, _) = restore_to_blank(&v.image_path, v.source_bytes, None);
        assert!(std::fs::read(&target).unwrap() == std::fs::read(&src).unwrap());

        // 4 KiB blocks: a qcow2 child would zero-fill the rest of each
        // 64 KiB cluster it touches — a full copy instead.
        let small = |name: &str| VolumeBackupRequest {
            block_size: 4096,
            ..request(&src, &dir.path().join(name), ImageFormat::Qcow2)
        };
        backup_volume(&small("s-full.qcow2"), &snap, no_progress).unwrap();
        let s = backup_volume(
            &VolumeBackupRequest {
                kind: BackupType::Incremental,
                parent_image: Some(dir.path().join("s-full.qcow2")),
                ..small("s-inc.qcow2")
            },
            &snap,
            no_progress,
        )
        .unwrap();
        assert_eq!(s.backup_kind, BackupType::Full);
    }

    /// A filesystem image made by `tool` from a folder (no mount, no root):
    /// `data/cache/junk.bin` and `data/keep/report.tmp` for the rules to take,
    /// `data/keep/report.txt` to keep.
    fn linux_fs_with_junk(dir: &Path, tool: &str, args: &[&str], size: u64) -> Option<PathBuf> {
        let content = dir.join("content");
        std::fs::create_dir_all(content.join("data/cache")).unwrap();
        std::fs::create_dir_all(content.join("data/keep")).unwrap();
        let junk: Vec<u8> = (0..3 * MIB as u32).map(|i| (i % 211) as u8 | 1).collect();
        std::fs::write(content.join("data/cache/junk.bin"), &junk).unwrap();
        std::fs::write(content.join("data/keep/report.tmp"), vec![0x5A; 300_000]).unwrap();
        std::fs::write(content.join("data/keep/report.txt"), b"kept on purpose\n").unwrap();
        let src = dir.join(format!("src.{tool}"));
        blank(&src, size);
        let mut all: Vec<&str> = args.to_vec();
        let d = content.to_str().unwrap().to_string();
        all.push(&d);
        mkfs(tool, &all, &src).then_some(src)
    }

    fn linux_rules() -> Vec<String> {
        vec!["data/cache/**".into(), "*.tmp".into()]
    }

    /// Does any 64 KiB-aligned window of `path` hold `needle` 4 KiB long? The
    /// excluded files' bytes must not be anywhere in the restored volume.
    fn holds_run(path: &Path, byte: u8) -> bool {
        let data = std::fs::read(path).unwrap();
        data.chunks(4096)
            .any(|c| c.len() == 4096 && c.iter().all(|&b| b == byte))
    }

    /// ext4 under the Linux rules (plan L3), from a source that holds still:
    /// the files leave the catalog and their blocks leave the copy (Stage 2),
    /// the restored filesystem is clean, the kept file reads back.
    #[test]
    fn ext4_exclusions_remove_the_files_from_a_frozen_source() {
        let dir = tempfile::tempdir().unwrap();
        let Some(src) = linux_fs_with_junk(
            dir.path(),
            "mkfs.ext4",
            &["-q", "-F", "-b", "4096", "-d"],
            64 * MIB,
        ) else {
            return;
        };
        let img = dir.path().join("x.qcow2");
        let req = VolumeBackupRequest {
            exclusion_patterns_linux: linux_rules(),
            // Windows rules are not for ext.
            exclusion_patterns: vec!["*.txt".into()],
            ..request(&src, &img, ImageFormat::Qcow2)
        };
        let sum = backup_volume(&req, &file_snapshot(&src, true), no_progress).unwrap();
        assert_eq!(sum.excluded_files, 2, "{:?}", sum.file_scan_skipped);
        assert_eq!(sum.excluded_bytes, 3 * MIB + 300_000);
        assert!(sum.file_scan_skipped.is_none() && sum.excluded_entries_kept.is_none());
        assert!(sum.categories.is_some());
        let (target, _) = restore_to_blank(&img, sum.source_bytes, None);
        let fsck = std::process::Command::new("e2fsck")
            .args(["-fn"])
            .arg(&target)
            .output();
        if let Ok(out) = fsck {
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stdout)
            );
        }
        let ls = |path: &str| {
            std::process::Command::new("debugfs")
                .args(["-R", &format!("ls -l {path}")])
                .arg(&target)
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        };
        if let Ok(keep) = ls("/data/keep") {
            assert!(
                keep.contains("report.txt") && !keep.contains("report.tmp"),
                "{keep}"
            );
            assert!(!ls("/data/cache").unwrap().contains("junk.bin"));
        }
        assert!(
            !holds_run(&target, 0x5A),
            "the excluded file's bytes are gone"
        );

        // Not held still: nothing parsed, nothing excluded, and it says so.
        let img2 = dir.path().join("y.qcow2");
        let sum = backup_volume(
            &VolumeBackupRequest {
                image_path: img2.clone(),
                ..req.clone()
            },
            &file_snapshot(&src, false),
            no_progress,
        )
        .unwrap();
        assert_eq!(sum.excluded_files, 0);
        assert_eq!(
            sum.file_scan_skipped.as_deref(),
            Some(&*own_backup_common::tr!("engine.skip_not_frozen"))
        );
    }

    /// btrfs under the Linux rules: removed from the trees where Stage 2 may
    /// (a file it spares keeps its name over zeroed data, and the result says
    /// so), and the restored filesystem passes `btrfs check`.
    #[test]
    fn btrfs_exclusions_remove_the_files_from_a_frozen_source() {
        let dir = tempfile::tempdir().unwrap();
        let Some(src) =
            linux_fs_with_junk(dir.path(), "mkfs.btrfs", &["-q", "-f", "-r"], 128 * MIB)
        else {
            return;
        };
        let img = dir.path().join("b.qcow2");
        let req = VolumeBackupRequest {
            exclusion_patterns_linux: linux_rules(),
            ..request(&src, &img, ImageFormat::Qcow2)
        };
        let sum = backup_volume(&req, &file_snapshot(&src, true), no_progress).unwrap();
        assert_eq!(sum.excluded_files, 2, "{:?}", sum.file_scan_skipped);
        let (target, _) = restore_to_blank(&img, sum.source_bytes, None);
        let btrfs = |args: &[&str]| {
            std::process::Command::new("btrfs")
                .args(args)
                .arg(&target)
                .output()
        };
        if let Ok(out) = btrfs(&["check", "--readonly"]) {
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        assert!(!holds_run(&target, 0x5A), "no excluded byte is left");
        // The report matches the trees: a name the scanner had to spare
        // (Stage 1 — its data zeroed) is said to be kept, and only then.
        if let Ok(out) = btrfs(&["inspect-internal", "dump-tree", "-t", "fs"]) {
            let tree = String::from_utf8_lossy(&out.stdout);
            assert!(tree.contains("name: report.txt"));
            let kept = tree.contains("name: junk.bin") || tree.contains("name: report.tmp");
            assert_eq!(sum.excluded_entries_kept.is_some(), kept, "{tree}");
        }
    }

    /// FAT32 (an ESP, a stick) under the job's Windows-family rules, as on
    /// Windows: the entries are deleted and the clusters freed, and the
    /// restored filesystem passes its checker.
    #[test]
    fn fat32_exclusions_delete_the_entries() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("esp.fat32");
        blank(&src, 64 * MIB);
        if !mkfs("mkfs.vfat", &["-F", "32", "-n", "OBKESP"], &src) {
            return;
        }
        let put = |name: &str, data: &[u8]| {
            let file = dir.path().join(name.replace('/', "_"));
            std::fs::write(&file, data).unwrap();
            std::process::Command::new("mcopy")
                .arg("-i")
                .arg(&src)
                .arg(&file)
                .arg(format!("::{name}"))
                .output()
        };
        let mmd = std::process::Command::new("mmd")
            .arg("-i")
            .arg(&src)
            .arg("::/EFI")
            .output();
        if mmd.is_err() {
            return; // no mtools
        }
        put("EFI/BOOTX64.EFI", &vec![0x42; 200_000]).unwrap();
        put("EFI/CRASH.TMP", &vec![0x5A; 300_000]).unwrap();
        let img = dir.path().join("esp.qcow2");
        let req = VolumeBackupRequest {
            exclusion_patterns: vec!["*.tmp".into()],
            exclusion_patterns_linux: vec!["*.efi".into()],
            ..request(&src, &img, ImageFormat::Qcow2)
        };
        let sum = backup_volume(&req, &file_snapshot(&src, true), no_progress).unwrap();
        assert_eq!(sum.excluded_files, 1, "{:?}", sum.file_scan_skipped);
        assert!(sum.excluded_entries_kept.is_none());
        let (target, _) = restore_to_blank(&img, sum.source_bytes, None);
        if let Ok(out) = std::process::Command::new("fsck.vfat")
            .arg("-n")
            .arg(&target)
            .output()
        {
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stdout)
            );
        }
        let dir_list = std::process::Command::new("mdir")
            .arg("-i")
            .arg(&target)
            .arg("::/EFI")
            .output()
            .unwrap();
        let listing = String::from_utf8_lossy(&dir_list.stdout).to_uppercase();
        assert!(
            listing.contains("BOOTX64") && !listing.contains("CRASH"),
            "{listing}"
        );
        assert!(!holds_run(&target, 0x5A));
    }

    /// xfs captured frozen (an LVM or dm snapshot): its log reads dirty, so
    /// the excluded files are zeroed but stay listed — and the result says
    /// so instead of claiming them removed.
    #[test]
    fn xfs_from_a_frozen_capture_zeroes_and_reports_the_names_kept() {
        let zst = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/xfs_reflink.img.zst");
        if !zst.exists() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("xfs.img");
        let mut raw = zstd::decode_all(std::fs::File::open(&zst).unwrap()).unwrap();
        // Break the unmount record, as freezing leaves the log.
        let sect = u16::from_be_bytes(raw[0x66..0x68].try_into().unwrap()) as usize;
        let bs = u32::from_be_bytes(raw[0x04..0x08].try_into().unwrap()) as u64;
        let logstart = u64::from_be_bytes(raw[0x30..0x38].try_into().unwrap());
        let agblocks = u32::from_be_bytes(raw[0x54..0x58].try_into().unwrap()) as u64;
        let agblklog = raw[0x7C] as u32;
        let log_off =
            ((logstart >> agblklog) * agblocks + (logstart & ((1u64 << agblklog) - 1))) * bs;
        let logblocks = u32::from_be_bytes(raw[0x60..0x64].try_into().unwrap()) as u64;
        let mut best: Option<(u64, usize)> = None;
        let mut o = log_off as usize;
        while o + sect <= (log_off + logblocks * bs) as usize {
            if u32::from_be_bytes(raw[o..o + 4].try_into().unwrap()) == 0xFEED_BABE {
                let lsn = u64::from_be_bytes(raw[o + 0x10..o + 0x18].try_into().unwrap());
                if best.map(|(b, _)| lsn > b).unwrap_or(true) {
                    best = Some((lsn, o));
                }
            }
            o += sect;
        }
        raw[best.unwrap().1 + sect + 12] ^= 0xFF;
        std::fs::write(&src, &raw).unwrap();

        let source = src.to_str().unwrap();
        let req = VolumeBackupRequest {
            exclusion_patterns_linux: vec!["cache".into()],
            ..request(&src, &dir.path().join("q.qcow2"), ImageFormat::Qcow2)
        };
        // An LVM snapshot of it: taken frozen.
        let sum = backup_volume(&req, &snapshot_of(source, &src, "lvm"), no_progress).unwrap();
        assert!(sum.excluded_files >= 1, "{:?}", sum.file_scan_skipped);
        assert!(sum.excluded_bytes > 0);
        let said = sum.excluded_entries_kept.clone().unwrap_or_default();
        let kept: u64 = said
            .split(' ')
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("no count in {said:?}"));
        assert!((1..=sum.excluded_files).contains(&kept), "{said}");
        assert_eq!(
            said,
            own_backup_common::tr!(
                "engine.entries_kept_named",
                kept = kept,
                total = sum.excluded_files,
                fs = "xfs"
            )
        );
        // Only vouched to hold still, not captured frozen: the dirty log may
        // be a crash's, and the scan refuses — said, not hidden.
        let sum = backup_volume(
            &VolumeBackupRequest {
                image_path: dir.path().join("r.qcow2"),
                ..req.clone()
            },
            &file_snapshot(&src, true),
            no_progress,
        )
        .unwrap();
        assert_eq!(sum.excluded_files, 0);
        assert!(sum.file_scan_skipped.is_some());
    }

    /// A snapshot that did not hold to the end of the copy: the check comes
    /// after the whole copy, right before publication, and what was written
    /// is gone — for the plain containers and for the encrypted one.
    #[test]
    fn a_snapshot_that_did_not_hold_publishes_nothing() {
        for (name, encryption) in [("out.qcow2", None), ("out.obk", Some(test_keys()))] {
            let dir = tempfile::tempdir().unwrap();
            let src = dir.path().join("src.bin");
            std::fs::write(&src, vec![7u8; 3 << 20]).unwrap();
            let img = dir.path().join(name);
            let req = VolumeBackupRequest {
                encryption,
                ..request(&src, &img, ImageFormat::Qcow2)
            };
            let mut seen = Vec::new();
            let mut checked = 0;
            let result = backup_volume_checked(
                &req,
                &file_snapshot(&src, false),
                |_| {
                    checked += 1;
                    Err(OwnBackupError::NoSnapshotProvider(
                        "the snapshot overflowed".into(),
                    ))
                },
                |p| {
                    seen.push(p.backup_id.clone());
                    Ok(())
                },
            );
            let Err(OwnBackupError::NoSnapshotProvider(why)) = result else {
                panic!("{name}: the snapshot's error must come back");
            };
            assert_eq!(why, "the snapshot overflowed");
            assert_eq!(checked, 1, "asked once, at publication");
            assert_eq!(seen.last().map(String::as_str), Some("publish"));
            assert!(seen.iter().any(|id| id == "finalize"), "after the copy");
            assert_eq!(
                images_in(dir.path(), &[&src]),
                Vec::<String>::new(),
                "{name}"
            );
        }
    }

    /// A disk image file with a GPT and two partitions — FAT32 at 1 MiB and
    /// ext4 at 66 MiB, each also its own file (what its snapshot reads).
    /// `None` without mkfs.vfat/mkfs.ext4.
    struct LinuxTestDisk {
        disk: PathBuf,
        parts: Vec<(u32, PathBuf, u64)>,
    }

    impl LinuxTestDisk {
        fn new(dir: &Path) -> Option<Self> {
            let fat = dir.join("p1.fat32");
            blank(&fat, 64 * MIB);
            if !mkfs("mkfs.vfat", &["-F", "32", "-n", "OBKESP"], &fat) {
                return None;
            }
            let content = dir.join("ext-content");
            std::fs::create_dir_all(content.join("data")).unwrap();
            std::fs::write(content.join("data/keep.txt"), b"kept\n").unwrap();
            std::fs::write(content.join("data/junk.tmp"), vec![0x5A; 400_000]).unwrap();
            let ext = dir.join("p2.ext4");
            blank(&ext, 32 * MIB);
            let d = content.to_str().unwrap().to_string();
            if !mkfs("mkfs.ext4", &["-q", "-F", "-b", "4096", "-d", &d], &ext) {
                return None;
            }
            let parts = vec![(1u32, fat, MIB), (2u32, ext, 66 * MIB)];
            let disk = dir.join("disk.img");
            let size = 100 * MIB;
            let layout = own_backup_common::gpt::DiskLayout {
                disk_guid: "0F1E2D3C-4B5A-6978-8796-A5B4C3D2E1F0".into(),
                sector_size: 512,
                size_bytes: size,
                partitions: parts
                    .iter()
                    .map(
                        |(index, path, offset)| own_backup_common::gpt::GptPartitionEntry {
                            index: *index,
                            type_guid: "0FC63DAF-8483-4772-8E79-3D69D8477DE4".into(),
                            part_guid: format!("11111111-0000-0000-0000-00000000000{index}"),
                            first_lba: offset / 512,
                            last_lba: (offset + std::fs::metadata(path).unwrap().len()) / 512 - 1,
                            attributes: 0,
                            name: format!("part{index}"),
                        },
                    )
                    .collect(),
            };
            let this = Self { disk, parts };
            this.write_disk(&layout);
            Some(this)
        }

        /// The disk file as the partitions are now.
        fn write_disk(&self, layout: &own_backup_common::gpt::DiskLayout) {
            use std::os::unix::fs::FileExt;
            let file = std::fs::File::create(&self.disk).unwrap();
            file.set_len(layout.size_bytes).unwrap();
            for op in crate::gpt_writer::disk_gpt_write_ops(layout, layout.size_bytes).unwrap() {
                file.write_all_at(&op.bytes, op.offset).unwrap();
            }
            for (_, path, offset) in &self.parts {
                file.write_all_at(&std::fs::read(path).unwrap(), *offset)
                    .unwrap();
            }
        }

        fn snapshots(&self) -> Vec<Snapshot> {
            self.parts
                .iter()
                .map(|(_, p, _)| file_snapshot(p, true))
                .collect()
        }

        fn request(&self, image: &Path) -> DiskBackupRequest {
            DiskBackupRequest {
                disk_number: 0,
                disk_device: Some(self.disk.to_str().unwrap().into()),
                disk_model: "test disk".into(),
                partitions: self
                    .parts
                    .iter()
                    .map(|(index, path, _)| DiskPartitionBackup {
                        device: path.to_str().unwrap().into(),
                        part_index: *index,
                        passphrase: None,
                    })
                    .collect(),
                image_path: image.to_path_buf(),
                full_image_path: None,
                block_size: DEFAULT_BLOCK_SIZE,
                image_format: ImageFormat::Qcow2,
                job_id: Some("linux-disk".into()),
                encryption: None,
                exclusion_patterns_windows: Vec::new(),
                exclusion_patterns_linux: Vec::new(),
                kind: BackupType::Full,
                parent_image: None,
                source_disk: None,
                bootable: false,
                zstd_level: crate::qcow2::DEFAULT_ZSTD_LEVEL,
            }
        }
    }

    /// The bytes of `image` at `offset`, read through its chain.
    fn image_at(image: &Path, offset: u64, len: usize) -> Vec<u8> {
        let sidecar = read_embedded_sidecar(image, "qcow2").unwrap();
        let mut r = LinuxImageReader::open(image, &sidecar.info.container).unwrap();
        let mut out = vec![0u8; len];
        r.read_at(offset, &mut out).unwrap();
        out
    }

    /// A whole disk on Linux (plan E6): one image, each partition at its
    /// offset, the disk named by its device, the Linux rules applied to ext4,
    /// an increment holding only the change.
    #[test]
    fn a_whole_disk_is_copied_through_its_partitions_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let Some(disk) = LinuxTestDisk::new(dir.path()) else {
            return;
        };
        let out = dir.path().join("out");
        let full = out.join("disk_full.qcow2");
        let req = with_rules(disk.request(&full));
        let sum = backup_disk(&req, &disk.snapshots(), no_progress).unwrap();
        assert_eq!(sum.partitions, 2);
        let sidecar = read_embedded_sidecar(&full, "qcow2").unwrap();
        assert_eq!(sum.excluded_files, 1, "{:?}", sum.file_scan_skipped);
        assert!(
            sum.file_scan_skipped.is_none(),
            "{:?}",
            sum.file_scan_skipped
        );
        assert_eq!(sidecar.manifest.source_os, "linux");
        assert_eq!(
            sidecar.manifest.source_disk.device_id,
            disk.disk.to_str().unwrap()
        );
        let manifest = sidecar.disk_manifest.unwrap();
        let fs: Vec<_> = manifest
            .images
            .iter()
            .map(|i| i.filesystem.as_str())
            .collect();
        assert_eq!(fs, ["fat32", "ext4"]);
        // FAT32 is in the image as it is on its partition.
        let (_, fat, fat_at) = &disk.parts[0];
        let want = std::fs::read(fat).unwrap();
        assert!(
            image_at(&full, *fat_at, want.len()) == want,
            "the FAT32 partition"
        );
        // ext4: no byte of the excluded file is left.
        let (_, ext, ext_at) = &disk.parts[1];
        let ext_len = std::fs::metadata(ext).unwrap().len() as usize;
        let got = image_at(&full, *ext_at, ext_len);
        assert!(!got.chunks(4096).any(|c| c.iter().all(|&b| b == 0x5A)));
        assert!(verify_image(&full, |_, _| Ok(())).unwrap().valid());

        // An increment: one used block of the FAT32 partition changes (its
        // label, in the boot sector; free space is never copied at all).
        {
            use std::os::unix::fs::FileExt;
            let f = std::fs::OpenOptions::new().write(true).open(fat).unwrap();
            f.write_all_at(b"CHANGED    ", 0x47).unwrap();
        }
        let inc = out.join("disk_inc.qcow2");
        let child = backup_disk(
            &DiskBackupRequest {
                kind: BackupType::Incremental,
                parent_image: Some(full.clone()),
                ..with_rules(disk.request(&inc))
            },
            &disk.snapshots(),
            no_progress,
        )
        .unwrap();
        assert_eq!(child.backup_kind, BackupType::Incremental);
        assert_eq!(child.blocks_copied, 1);
        let want = std::fs::read(fat).unwrap();
        assert!(image_at(&inc, *fat_at, want.len()) == want, "the child");
        assert!(verify_image(&inc, |_, _| Ok(())).unwrap().valid());

        // VHDX: a differencing child of the whole disk, verified.
        let vfull = out.join("disk_full.vhdx");
        let vreq = DiskBackupRequest {
            image_format: ImageFormat::Vhdx,
            ..disk.request(&vfull)
        };
        backup_disk(&vreq, &disk.snapshots(), no_progress).unwrap();
        let v = backup_disk(
            &DiskBackupRequest {
                kind: BackupType::Incremental,
                parent_image: Some(vfull.clone()),
                image_format: ImageFormat::Vhdx,
                ..disk.request(&out.join("disk_inc.avhdx"))
            },
            &disk.snapshots(),
            no_progress,
        )
        .unwrap();
        assert_eq!(v.backup_kind, BackupType::Incremental);
        assert!(verify_image(&out.join("disk_inc.avhdx"), |_, _| Ok(()))
            .unwrap()
            .valid());
    }

    /// `req` with a Windows rule (not for ext4: keep.txt stays) and a Linux
    /// one (junk.tmp goes).
    fn with_rules(req: DiskBackupRequest) -> DiskBackupRequest {
        DiskBackupRequest {
            exclusion_patterns_windows: vec!["*.txt".into()],
            exclusion_patterns_linux: vec!["*.tmp".into()],
            ..req
        }
    }

    /// What a Linux disk copy refuses, and what it publishes nothing for.
    #[test]
    fn a_whole_disk_needs_its_device_its_snapshots_and_their_hold() {
        let dir = tempfile::tempdir().unwrap();
        let Some(disk) = LinuxTestDisk::new(dir.path()) else {
            return;
        };
        let out = dir.path().join("out");
        std::fs::create_dir(&out).unwrap();
        let image = out.join("disk.qcow2");
        // Named by a number only: refused.
        let by_number = DiskBackupRequest {
            disk_device: None,
            ..disk.request(&image)
        };
        assert!(matches!(
            backup_disk(&by_number, &disk.snapshots(), no_progress),
            Err(OwnBackupError::Config(_))
        ));
        // A partition without its snapshot: refused.
        let one = &disk.snapshots()[..1];
        assert!(backup_disk(&disk.request(&image), one, no_progress).is_err());
        // A snapshot that did not hold: asked at publication, nothing left.
        let mut asked = 0;
        let r = backup_disk_checked(
            &disk.request(&image),
            &disk.snapshots(),
            |s| {
                asked += 1;
                if s.source_device.ends_with("p2.ext4") {
                    Err(OwnBackupError::NoSnapshotProvider("overflowed".into()))
                } else {
                    Ok(())
                }
            },
            no_progress,
        );
        assert!(matches!(r, Err(OwnBackupError::NoSnapshotProvider(_))));
        assert_eq!(asked, 2);
        assert_eq!(std::fs::read_dir(&out).unwrap().count(), 0);
    }

    /// A PV in use on Linux: the snapshot crate's record (`mode` `pv`) is
    /// taken apart by the platform — each logical volume read from its
    /// snapshot, ext4 through its own map and rules, the rest from the
    /// partition — the record is checked before publication, and a volume
    /// copy of the same partition is refused.
    #[test]
    fn a_physical_volume_is_read_through_its_volumes_snapshots() {
        use own_backup_common::lvm_pv::fixtures::{label, mda, vg_text};
        use own_backup_common::lvm_pv::{PvSegment, PvSnapshot, PvSnapshotVolume};
        let dir = tempfile::tempdir().unwrap();
        // The root volume's snapshot: a 32 MiB ext4 with a junk file.
        let content = dir.path().join("root-content");
        std::fs::create_dir_all(content.join("data")).unwrap();
        std::fs::write(content.join("data/keep.txt"), b"kept\n").unwrap();
        std::fs::write(content.join("data/junk.tmp"), vec![0x5A; 400_000]).unwrap();
        let root = dir.path().join("root.snap");
        blank(&root, 32 * MIB);
        let d = content.to_str().unwrap().to_string();
        if !mkfs("mkfs.ext4", &["-q", "-F", "-b", "4096", "-d", &d], &root) {
            return;
        }
        let root_bytes = std::fs::read(&root).unwrap();
        // The PV: 48 MiB; root on extents 0-7, extents 8-10 free.
        const EXT: u64 = 4 * MIB;
        let pv_len = 48 * MIB;
        let mut live = vec![0xEEu8; pv_len as usize];
        let head = label(pv_len, MIB, &[(4096, MIB - 4096)]);
        live[..head.len()].copy_from_slice(&head);
        let text = vg_text(11, &[("root", vec![("striped", vec![("pv0", 0)], 8)])]);
        live[4096..MIB as usize].copy_from_slice(&mda(MIB - 4096, 8192, &text));
        let part = dir.path().join("p2.pv");
        std::fs::write(&part, &live).unwrap();

        let disk_path = dir.path().join("disk.img");
        let part_at = 2 * MIB;
        let layout = own_backup_common::gpt::DiskLayout {
            disk_guid: "0F1E2D3C-4B5A-6978-8796-A5B4C3D2E1F0".into(),
            sector_size: 512,
            size_bytes: part_at + pv_len + MIB,
            partitions: vec![own_backup_common::gpt::GptPartitionEntry {
                index: 2,
                type_guid: "E6D6D379-F507-44C2-A23C-238F2A3DF928".into(),
                part_guid: "11111111-0000-0000-0000-000000000002".into(),
                first_lba: part_at / 512,
                last_lba: (part_at + pv_len) / 512 - 1,
                attributes: 0,
                name: "lvm".into(),
            }],
        };
        {
            use std::os::unix::fs::FileExt;
            let file = std::fs::File::create(&disk_path).unwrap();
            file.set_len(layout.size_bytes).unwrap();
            for op in crate::gpt_writer::disk_gpt_write_ops(&layout, layout.size_bytes).unwrap() {
                file.write_all_at(&op.bytes, op.offset).unwrap();
            }
            file.write_all_at(&live, part_at).unwrap();
        }
        let record = PvSnapshot {
            device: part.to_str().unwrap().into(),
            volumes: vec![PvSnapshotVolume {
                name: "vg0-root".into(),
                origin: root.to_str().unwrap().into(),
                snapshot: root.to_str().unwrap().into(),
                dm_id: "0".repeat(32),
                size: 8 * EXT,
                segments: vec![PvSegment {
                    pv_offset: MIB,
                    lv_offset: 0,
                    length: 8 * EXT,
                }],
            }],
            guard: vec![(0, MIB)],
            guard_digest: String::new(),
        };
        let mut snap = snapshot_of(part.to_str().unwrap(), &part, "pv");
        snap.metadata.insert(
            own_backup_snapshot::linux::PV_LAYOUT.into(),
            record.to_json(),
        );

        let out = dir.path().join("out");
        let image = out.join("disk.qcow2");
        let req = DiskBackupRequest {
            disk_number: 0,
            disk_device: Some(disk_path.to_str().unwrap().into()),
            disk_model: "test disk".into(),
            partitions: vec![DiskPartitionBackup {
                device: part.to_str().unwrap().into(),
                part_index: 2,
                passphrase: None,
            }],
            image_path: image.clone(),
            full_image_path: None,
            block_size: DEFAULT_BLOCK_SIZE,
            image_format: ImageFormat::Qcow2,
            job_id: Some("linux-pv".into()),
            encryption: None,
            exclusion_patterns_windows: Vec::new(),
            exclusion_patterns_linux: vec!["*.tmp".into()],
            kind: BackupType::Full,
            parent_image: None,
            source_disk: None,
            bootable: false,
            zstd_level: crate::qcow2::DEFAULT_ZSTD_LEVEL,
        };
        let mut checked = Vec::new();
        let sum = backup_disk_checked(
            &req,
            std::slice::from_ref(&snap),
            |s| {
                checked.push(own_backup_snapshot::linux::pv_layout(s).unwrap().unwrap());
                Ok(())
            },
            no_progress,
        )
        .unwrap();
        assert_eq!(
            checked,
            vec![record.clone()],
            "the record is checked at publication"
        );
        assert_eq!(sum.excluded_files, 1, "{:?}", sum.file_scan_skipped);
        assert!(
            sum.file_scan_skipped.is_none(),
            "{:?}",
            sum.file_scan_skipped
        );
        // The head from the partition, root's used blocks from its snapshot
        // and none of the junk, the free extents not at all.
        assert!(image_at(&image, part_at, MIB as usize) == live[..MIB as usize]);
        let got = image_at(&image, part_at + MIB, root_bytes.len());
        assert!(!got.chunks(4096).any(|c| c.iter().all(|&b| b == 0x5A)));
        assert!(!got.chunks(4096).any(|c| c.iter().all(|&b| b == 0xEE)));
        // The superblock is root's (its free counts patched by the removal).
        assert_eq!(&got[1024 + 56..1024 + 58], &[0x53, 0xEF], "ext4 magic");
        assert_eq!(
            &got[1024 + 104..1024 + 120],
            &root_bytes[1024 + 104..1024 + 120],
            "its UUID"
        );
        let free = image_at(&image, part_at + MIB + 8 * EXT, (3 * EXT) as usize);
        assert!(free.iter().all(|&b| b == 0), "free extents are not copied");
        let sidecar = read_embedded_sidecar(&image, "qcow2").unwrap();
        assert_eq!(
            sidecar.disk_manifest.unwrap().images[0].filesystem,
            "LVM2_member"
        );
        assert!(verify_image(&image, |_, _| Ok(())).unwrap().valid());

        // The record failing its check: nothing published.
        let failed = out.join("failed.qcow2");
        let r = backup_disk_checked(
            &DiskBackupRequest {
                image_path: failed.clone(),
                ..req.clone()
            },
            std::slice::from_ref(&snap),
            |_| Err(OwnBackupError::NoSnapshotProvider("changed".into())),
            no_progress,
        );
        assert!(matches!(r, Err(OwnBackupError::NoSnapshotProvider(_))));
        assert!(!failed.exists());

        // A volume copy of the partition: refused before a byte is read.
        let vol = backup_volume_checked(
            &request(&part, &out.join("vol.qcow2"), ImageFormat::Qcow2),
            &snap,
            |_| Ok(()),
            no_progress,
        );
        assert!(matches!(vol, Err(OwnBackupError::NoSnapshotProvider(_))));
        // And a `pv` snapshot without its record is no snapshot at all.
        let bare = snapshot_of(part.to_str().unwrap(), &part, "pv");
        assert!(backup_disk_checked(
            &DiskBackupRequest {
                image_path: out.join("bare.qcow2"),
                ..req
            },
            &[bare],
            |_| Ok(()),
            no_progress,
        )
        .is_err());
    }

    /// The real check, for a partition held still whose hold cannot be
    /// confirmed: an error, and no image.
    #[test]
    fn a_hold_that_cannot_be_confirmed_publishes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        std::fs::write(&src, vec![9u8; 2 << 20]).unwrap();
        let img = dir.path().join("out.vhdx");
        let held = snapshot_of(src.to_str().unwrap(), &src, "freeze");
        let result = backup_volume(&request(&src, &img, ImageFormat::Vhdx), &held, no_progress);
        assert!(result.is_err(), "a hold without its mount is not intact");
        assert_eq!(images_in(dir.path(), &[&src]), Vec::<String>::new());
    }

    /// A source with no snapshot of its own is refused, not read live.
    #[test]
    fn a_source_without_its_snapshot_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        std::fs::write(&src, vec![1u8; 1 << 20]).unwrap();
        let other = dir.path().join("other.bin");
        std::fs::write(&other, vec![2u8; 1 << 20]).unwrap();
        let img = dir.path().join("out.vhdx");
        let result = backup_volume(
            &request(&src, &img, ImageFormat::Vhdx),
            &file_snapshot(&other, true),
            no_progress,
        );
        assert!(matches!(result, Err(OwnBackupError::Config(_))));
        assert_eq!(images_in(dir.path(), &[&src, &other]), Vec::<String>::new());
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
        let src = ext4_volume(dir.path(), "mounttrip").expect("mkfs.ext4");
        let payload = std::fs::read(dir.path().join("content/payload.bin")).unwrap();

        // Back up into qcow2 and mount the backup through our NBD server.
        let img = dir.path().join("backup.qcow2");
        backup_volume(
            &request(&src, &img, ImageFormat::Qcow2),
            &file_snapshot(&src, true),
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
    use own_backup_common::disk::DiskInfo;
    use own_backup_common::image::ImageManifest;
    use own_backup_common::types::{
        BackupMode, BackupType, BootMode, CompressionAlgo, PartitionTable,
    };
    use uuid::Uuid;

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
                stable_device: String::new(),
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
