//! Restoring one partition's data, sector by sector (R4a).
//!
//! The existing `restore_volume` writes a whole volume image onto a whole
//! device. This does the harder case the designer produces: take *one*
//! partition out of an image — which may be a whole-disk image holding several
//! — and write it into *one* partition on the target, wherever that partition
//! happens to sit.
//!
//! Addressing goes through `part:<disk>:<offset>:<len>`
//! ([`crate::win_device`]), a window over the physical drive. That works for a
//! partition with no drive letter, no mounted volume, and no filesystem Windows
//! recognises — none of which is exotic: an EFI System Partition, a Linux
//! partition, and a partition that does not exist yet are all in that category.
//!
//! The arithmetic — which blocks, which slices, where they land — is in
//! [`crate::restore_map`] and tested there.

#![cfg(target_os = "windows")]

use std::path::Path;
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use own_backup_common::cancel::CancelToken;
use own_backup_common::{OwnBackupError, Result};

use crate::checksum::compute_block_checksum;
use crate::engine_win::ImageReadSource;
use crate::restore_map::{check_fits, plan_block_copy, uncovered_ranges, SourceWindow};
use crate::sidecar::Sidecar;
use crate::volume_win::VolumeDismountGuard;
use crate::win_device::WinRawDevice;

/// Chunk used when zeroing the gaps between restored blocks.
const WIPE_CHUNK: usize = 4 * 1024 * 1024;

/// Where the data for one partition is to be written.
#[derive(Debug, Clone, Copy)]
pub struct PartitionTarget {
    pub disk_number: u32,
    pub offset: u64,
    pub size: u64,
}

impl PartitionTarget {
    fn device_path(&self) -> String {
        format!("part:{}:{}:{}", self.disk_number, self.offset, self.size)
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SectorRestoreOptions {
    /// Verify each block against the checksum stored in the sidecar.
    pub verify_checksums: bool,
    /// Zero the stretches no stored block covers (D16).
    pub wipe_free_space: bool,
    /// Assume the caller has already taken the disk offline, so there is
    /// nothing to dismount. Skips the volume sweep, which is not free.
    pub disk_already_offline: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct SectorRestoreProgress {
    pub blocks_done: u64,
    pub blocks_total: u64,
    pub bytes_written: u64,
}

#[derive(Debug, Clone)]
pub struct SectorRestoreSummary {
    pub blocks_restored: u64,
    pub bytes_restored: u64,
    pub bytes_wiped: u64,
    pub duration: Duration,
    /// The user cancelled partway. The partition is left half-written and is
    /// not usable until restored again — there is no rollback.
    pub cancelled: bool,
}

/// Which stretch of the image belongs to source partition `src_index`.
///
/// For a whole-disk image the answer comes from the embedded disk manifest's
/// GPT. For a plain volume image the image *is* the volume, and `src_index` is
/// not consulted — there is only one thing it could mean.
pub fn source_window_for(sidecar: &Sidecar, src_index: u32) -> Result<SourceWindow> {
    let image_len = sidecar.manifest.source_disk.size_bytes;
    let Some(dm) = sidecar.disk_manifest.as_ref() else {
        return Ok(SourceWindow::whole(image_len));
    };
    let p = dm.layout.partitions.iter().find(|p| p.index == src_index).ok_or_else(|| {
        OwnBackupError::RestoreIncompatible(format!(
            "в копии нет раздела с номером {src_index} (есть: {})",
            dm.layout
                .partitions
                .iter()
                .map(|p| p.index.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    })?;
    Ok(SourceWindow::partition(p.first_lba, p.last_lba, dm.layout.sector_size))
}

/// Copy one partition's stored blocks from `image_path` into `target`.
///
/// DESTRUCTIVE. The caller is responsible for the confirmation, for the
/// system-disk guard, and — when the plan rewrites the partition table — for
/// having taken the disk offline first.
pub fn restore_partition_sectors(
    image_path: &Path,
    sidecar: &Sidecar,
    src_index: u32,
    target: PartitionTarget,
    opts: SectorRestoreOptions,
    cancel: &CancelToken,
    mut progress: impl FnMut(&SectorRestoreProgress) -> Result<()>,
) -> Result<SectorRestoreSummary> {
    let started = Instant::now();
    let block_size = sidecar.manifest.block_size as u64;
    let image_len = sidecar.manifest.source_disk.size_bytes;
    let data_offset = sidecar.info.volume_data_offset;
    let window = source_window_for(sidecar, src_index)?;

    if target.size < window.len {
        return Err(OwnBackupError::RestoreIncompatible(format!(
            "раздел-приёмник {} Б меньше исходного тома {} Б — посекторно не поместится",
            target.size, window.len
        )));
    }

    // Nothing may hold the target's sectors while we write them. When the disk
    // is already offline this is a no-op; otherwise it takes down exactly the
    // volumes in the way and keeps their lock handles alive until it drops.
    let _guard = if opts.disk_already_offline {
        None
    } else {
        let g = VolumeDismountGuard::for_range(target.disk_number, target.offset, target.size);
        if !g.failed.is_empty() {
            warn!(volumes = ?g.failed, "some volumes could not be dismounted before the restore");
        }
        Some(g)
    };

    let device_path = target.device_path();
    let mut dev = WinRawDevice::open_write(&device_path)?;
    if block_size % dev.sector_size() as u64 != 0 {
        return Err(OwnBackupError::RestoreIncompatible(format!(
            "размер блока образа {block_size} не кратен сектору приёмника {}",
            dev.sector_size()
        )));
    }
    let mut image = ImageReadSource::open(image_path, &sidecar.info.container)?;

    let total = sidecar.checksums.len() as u64;
    let mut blocks_restored = 0u64;
    let mut bytes_restored = 0u64;
    let mut covered: Vec<(u64, u64)> = Vec::new();
    let mut cancelled = false;

    for entry in &sidecar.checksums {
        // Checked at every block: the data copy is the one operation long
        // enough for cancellation to be worth anything (D18).
        if cancel.is_cancelled() {
            cancelled = true;
            warn!(
                blocks_restored,
                "restore cancelled — the partition is half-written and unusable until restored again"
            );
            break;
        }

        let Some(copy) = plan_block_copy(entry.block_number, block_size, image_len, data_offset, window)
        else {
            continue; // this block belongs to some other partition
        };
        check_fits(&copy, target.size)?;

        // The whole block is read even when only a slice is wanted: the stored
        // checksum was computed over the whole block, and half a block cannot
        // be checked against it.
        let mut buf = vec![0u8; copy.block_len];
        image.read_at(copy.image_offset, &mut buf)?;

        if opts.verify_checksums {
            let actual = compute_block_checksum(&buf);
            if actual != entry.checksum {
                return Err(OwnBackupError::ChecksumMismatch {
                    block_index: entry.block_number,
                    expected: entry.checksum,
                    actual,
                });
            }
        }

        dev.write_at(copy.dst_offset, &buf[copy.slice_start..copy.slice_start + copy.slice_len])?;
        blocks_restored += 1;
        bytes_restored += copy.slice_len as u64;
        if opts.wipe_free_space {
            covered.push((copy.dst_offset, copy.slice_len as u64));
        }

        progress(&SectorRestoreProgress {
            blocks_done: blocks_restored,
            blocks_total: total,
            bytes_written: bytes_restored,
        })?;
    }

    let mut bytes_wiped = 0u64;
    if opts.wipe_free_space && !cancelled {
        bytes_wiped = wipe_gaps(&mut dev, &mut covered, window.len, cancel)?;
    }

    dev.flush()?;
    drop(image);

    info!(
        image = %image_path.display(),
        target = %device_path,
        src_index,
        blocks = blocks_restored,
        bytes = bytes_restored,
        bytes_wiped,
        cancelled,
        secs = started.elapsed().as_secs(),
        "partition restored"
    );

    Ok(SectorRestoreSummary {
        blocks_restored,
        bytes_restored,
        bytes_wiped,
        duration: started.elapsed(),
        cancelled,
    })
}

/// Zero everything inside the volume that no stored block covered (D16).
///
/// Only the volume's own length is wiped, not the whole target partition: past
/// the volume's end lies space the filesystem does not know about yet, and on a
/// larger target that is exactly the space `ExtendFilesystem` is about to claim.
/// Writing zeroes there would be pointless work, sometimes a lot of it.
fn wipe_gaps(
    dev: &mut WinRawDevice,
    covered: &mut Vec<(u64, u64)>,
    volume_len: u64,
    cancel: &CancelToken,
) -> Result<u64> {
    let gaps = uncovered_ranges(covered, volume_len);
    let zeros = vec![0u8; WIPE_CHUNK];
    let mut wiped = 0u64;
    for (start, len) in gaps {
        let mut written = 0u64;
        while written < len {
            if cancel.is_cancelled() {
                debug!(wiped, "free-space wipe cancelled");
                return Ok(wiped);
            }
            let chunk = ((len - written) as usize).min(WIPE_CHUNK);
            dev.write_at(start + written, &zeros[..chunk])?;
            written += chunk as u64;
            wiped += chunk as u64;
        }
    }
    debug!(bytes = wiped, "free space between restored blocks zeroed");
    Ok(wiped)
}

// ---------------------------------------------------------------------------
// Verification after a sector restore (R9, D10)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct VerifySummary {
    pub blocks_checked: u64,
    pub bytes_checked: u64,
    /// Block numbers that did not match. Kept rather than counted: "block
    /// 41983 differs" is something a user can act on, "3 blocks differ" is not.
    pub mismatches: Vec<u64>,
    /// Blocks whose stored checksum did not match the image itself — the copy
    /// is damaged, and the restore faithfully wrote damaged data.
    pub image_corrupt: Vec<u64>,
    pub cancelled: bool,
}

impl VerifySummary {
    pub fn ok(&self) -> bool {
        self.mismatches.is_empty() && self.image_corrupt.is_empty() && !self.cancelled
    }
}

/// Read back what [`restore_partition_sectors`] wrote and check it.
///
/// Two different failures are worth telling apart, so both are checked:
///
/// - the block read from the *image* does not match its stored checksum → the
///   backup itself is damaged, and the restore dutifully wrote damaged data;
/// - the image block is fine but the *target* does not match it → the write did
///   not land (a failing disk, a driver that lied about flushing).
///
/// Comparing the target against the image directly, rather than recomputing a
/// checksum over it, is what makes a partial block checkable: only a slice of
/// the first and last block belongs to the partition, and a checksum over a
/// slice cannot be compared with one computed over the whole block.
pub fn verify_partition_sectors(
    image_path: &Path,
    sidecar: &Sidecar,
    src_index: u32,
    target: PartitionTarget,
    cancel: &CancelToken,
    mut progress: impl FnMut(&SectorRestoreProgress) -> Result<()>,
) -> Result<VerifySummary> {
    let started = Instant::now();
    let block_size = sidecar.manifest.block_size as u64;
    let image_len = sidecar.manifest.source_disk.size_bytes;
    let data_offset = sidecar.info.volume_data_offset;
    let window = source_window_for(sidecar, src_index)?;

    let device_path = target.device_path();
    let mut dev = WinRawDevice::open_read(&device_path)?;
    let mut image = ImageReadSource::open(image_path, &sidecar.info.container)?;

    let mut out = VerifySummary::default();
    let total = sidecar.checksums.len() as u64;

    for entry in &sidecar.checksums {
        if cancel.is_cancelled() {
            out.cancelled = true;
            break;
        }
        let Some(copy) = plan_block_copy(entry.block_number, block_size, image_len, data_offset, window)
        else {
            continue;
        };

        let mut from_image = vec![0u8; copy.block_len];
        image.read_at(copy.image_offset, &mut from_image)?;
        if compute_block_checksum(&from_image) != entry.checksum {
            out.image_corrupt.push(entry.block_number);
            continue; // no point comparing the target against a bad reference
        }

        let mut from_disk = vec![0u8; copy.slice_len];
        dev.read_at(copy.dst_offset, &mut from_disk)?;
        if from_disk != from_image[copy.slice_start..copy.slice_start + copy.slice_len] {
            out.mismatches.push(entry.block_number);
        }

        out.blocks_checked += 1;
        out.bytes_checked += copy.slice_len as u64;
        progress(&SectorRestoreProgress {
            blocks_done: out.blocks_checked,
            blocks_total: total,
            bytes_written: out.bytes_checked,
        })?;
    }

    info!(
        target = %device_path,
        src_index,
        blocks = out.blocks_checked,
        mismatches = out.mismatches.len(),
        image_corrupt = out.image_corrupt.len(),
        cancelled = out.cancelled,
        secs = started.elapsed().as_secs(),
        "restore verified"
    );
    Ok(out)
}

// ---------------------------------------------------------------------------
// Mounting the image for a file-by-file restore (R4b)
// ---------------------------------------------------------------------------

/// A read-only mount of a backup image, detached on drop.
///
/// The detach matters more than it looks: a leaked attachment keeps the image
/// file locked, so the next restore attempt fails with a file-in-use error that
/// says nothing about the real cause.
pub struct MountedImage {
    path: std::path::PathBuf,
    /// `\\.\PhysicalDriveN` of the attached image.
    physical_path: String,
}

impl MountedImage {
    /// Attach `image_path` read-only.
    pub fn open(image_path: &Path) -> Result<Self> {
        let physical_path = crate::engine_win::mount_image(image_path, false)?;
        info!(image = %image_path.display(), device = %physical_path, "image mounted for file restore");
        Ok(Self { path: image_path.to_path_buf(), physical_path })
    }

    /// Physical disk number of the attached image.
    pub fn disk_number(&self) -> Result<u32> {
        self.physical_path
            .rsplit(|c: char| !c.is_ascii_digit())
            .find(|s| !s.is_empty())
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| {
                OwnBackupError::Device(format!(
                    "cannot read a disk number out of {}",
                    self.physical_path
                ))
            })
    }

    /// Volume root (`\\?\Volume{…}\`) of source partition `src_index` inside
    /// the mounted image.
    pub fn volume_root_of(&self, sidecar: &Sidecar, src_index: u32) -> Result<String> {
        let disk = self.disk_number()?;
        // A volume image places its single volume at `volume_data_offset`; a
        // whole-disk image keeps every partition at its original offset. Both
        // answers are the window's start.
        let offset = source_window_for(sidecar, src_index)?.start;
        crate::volume_win::volume_root_at_offset(disk, offset).ok_or_else(|| {
            OwnBackupError::Device(format!(
                "Windows не показала том раздела {src_index} в подключённой копии \
                 (смещение {offset}) — прочитать файлы неоткуда"
            ))
        })
    }
}

impl Drop for MountedImage {
    fn drop(&mut self) {
        if let Err(e) = crate::engine_win::unmount_image(&self.path) {
            warn!(
                image = %self.path.display(), error = %e,
                "could not detach the mounted image — the file stays locked until the service restarts"
            );
        } else {
            debug!(image = %self.path.display(), "mounted image detached");
        }
    }
}

// ---------------------------------------------------------------------------
// Putting the encrypted container back (D19)
// ---------------------------------------------------------------------------

/// The container to rebuild on a partition, and the passphrase that opens it.
pub struct ReencryptRequest<'a> {
    pub header: &'a own_backup_common::image::LuksHeaderBackup,
    /// Supplied by the user at restore time and never stored anywhere. The
    /// backup keeps the header — public data — not the key to it.
    pub passphrase: &'a str,
}

/// Restore a partition **with its encryption**: write the container's saved
/// header back, then re-encrypt the payload under the master key its own
/// keyslots yield (D19).
///
/// The result is byte-identical to the original container — same UUID, same
/// keyslots, same passphrase — so nothing on the restored system has to change.
/// That is the whole point: the alternative leaves a bare filesystem, and a
/// Linux install whose `crypttab` and initramfs wait forever for a device that
/// is not there.
///
/// The payload moves in two steps that must not be confused:
/// - the image holds the volume **decrypted**, at offsets relative to the
///   payload start;
/// - the target holds it **encrypted**, at those offsets plus `data_offset`,
///   with the XTS tweak derived from the payload-relative offset.
///
/// Mixing the two up produces a container that opens and decrypts to noise,
/// with no error raised anywhere — hence the offsets are named apart and the
/// tweak always comes from `payload_off`.
pub fn restore_partition_encrypted(
    image_path: &Path,
    sidecar: &Sidecar,
    src_index: u32,
    target: PartitionTarget,
    opts: SectorRestoreOptions,
    req: &ReencryptRequest<'_>,
    cancel: &CancelToken,
    mut progress: impl FnMut(&SectorRestoreProgress) -> Result<()>,
) -> Result<SectorRestoreSummary> {
    let started = Instant::now();
    let block_size = sidecar.manifest.block_size as u64;
    let image_len = sidecar.manifest.source_disk.size_bytes;
    let data_offset = sidecar.info.volume_data_offset;
    let window = source_window_for(sidecar, src_index)?;

    let header = crate::luks::decode_stored_header(req.header)?;

    // The payload no longer starts at the partition's first byte: the header
    // takes the front. So the partition must hold both.
    let payload_len = crate::restore_map::payload_len(
        window.len,
        req.header.data_offset,
        sidecar.disk_manifest.is_some(),
    )?;
    let needed = req.header.data_offset + payload_len;
    if target.size < needed {
        return Err(OwnBackupError::RestoreIncompatible(format!(
            "раздел-приёмник {} Б мал для контейнера: заголовок {} Б + данные {} Б",
            target.size, req.header.data_offset, payload_len
        )));
    }

    let _guard = if opts.disk_already_offline {
        None
    } else {
        Some(VolumeDismountGuard::for_range(target.disk_number, target.offset, target.size))
    };

    let device_path = target.device_path();
    let mut dev = WinRawDevice::open_write(&device_path)?;
    dev.write_at(0, &header)?;
    dev.flush()?;
    info!(target = %device_path, bytes = header.len(), "LUKS header written back");

    // Parsed off the target, not off the blob we just decoded: this proves the
    // header actually landed and is readable where the system will look for it.
    let params = {
        let mut probe = WinRawDevice::open_read(&device_path)?;
        crate::luks::open_luks(&mut probe, req.passphrase.as_bytes())?
    };
    if params.data_offset != req.header.data_offset {
        return Err(OwnBackupError::ImageFormat(format!(
            "восстановленный заголовок сообщает смещение данных {}, ожидалось {}",
            params.data_offset, req.header.data_offset
        )));
    }

    let mut image = ImageReadSource::open(image_path, &sidecar.info.container)?;
    let total = sidecar.checksums.len() as u64;
    let mut out = SectorRestoreSummary {
        blocks_restored: 0,
        bytes_restored: 0,
        bytes_wiped: 0,
        duration: Duration::ZERO,
        cancelled: false,
    };

    for entry in &sidecar.checksums {
        if cancel.is_cancelled() {
            out.cancelled = true;
            warn!(
                blocks = out.blocks_restored,
                "re-encrypting restore cancelled — the container is incomplete and will not open"
            );
            break;
        }
        let Some(copy) = plan_block_copy(entry.block_number, block_size, image_len, data_offset, window)
        else {
            continue;
        };
        // `dst_offset` is where the byte sits inside the *payload*; on disk it
        // lands that far past the header.
        let payload_off = copy.dst_offset;
        if payload_off + copy.slice_len as u64 > payload_len {
            continue; // beyond the container's data area
        }

        let mut buf = vec![0u8; copy.block_len];
        image.read_at(copy.image_offset, &mut buf)?;
        if opts.verify_checksums {
            let actual = compute_block_checksum(&buf);
            if actual != entry.checksum {
                return Err(OwnBackupError::ChecksumMismatch {
                    block_index: entry.block_number,
                    expected: entry.checksum,
                    actual,
                });
            }
        }

        let mut slice = buf[copy.slice_start..copy.slice_start + copy.slice_len].to_vec();
        params.encrypt(payload_off, &mut slice)?;
        dev.write_at(req.header.data_offset + payload_off, &slice)?;

        out.blocks_restored += 1;
        out.bytes_restored += copy.slice_len as u64;
        progress(&SectorRestoreProgress {
            blocks_done: out.blocks_restored,
            blocks_total: total,
            bytes_written: out.bytes_restored,
        })?;
    }

    dev.flush()?;
    drop(image);
    out.duration = started.elapsed();
    info!(
        target = %device_path,
        src_index,
        blocks = out.blocks_restored,
        bytes = out.bytes_restored,
        cancelled = out.cancelled,
        secs = out.duration.as_secs(),
        "partition restored with its encryption intact"
    );
    Ok(out)
}

