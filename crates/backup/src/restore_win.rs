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
//! [`crate::restore_map`] and tested there; the loops around it are shared
//! with Linux ([`crate::sector_restore`]). What is Windows' own is the device:
//! a `part:` window whose writer takes down volumes that mount themselves over
//! the target and retries the refused write.

#![cfg(target_os = "windows")]

use std::path::Path;
use std::time::Duration;

use tracing::{debug, info, warn};

use own_backup_common::cancel::CancelToken;
use own_backup_common::{tr, OwnBackupError, Result};

use crate::sector_restore::{
    container_fitting, verify_partition_blocks, window_fitting, write_container_blocks,
    write_partition_blocks, SectorTarget,
};
use crate::sidecar::Sidecar;
use crate::volume_win::VolumeDismountGuard;
use crate::win_device::WinRawDevice;

// The loops themselves are shared with Linux (`sector_restore`); the names stay
// here because the plan executor is full of callers of them.
pub use crate::sector_restore::{
    source_window_for, ReencryptRequest, SectorRestoreOptions, SectorRestoreProgress,
    SectorRestoreSummary, VerifySummary,
};

/// How long to let Windows finish rebuilding its volumes after the partition
/// table changed, before writing into one of the new partitions.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(15);

/// How many times to take the volumes down again and retry a refused write.
///
/// Once is nearly always enough — the second attempt is made with the volume
/// that appeared mid-write locked — and three attempts is the point at which
/// something else is wrong and saying so beats retrying forever.
const WRITE_ATTEMPTS: u32 = 3;

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

/// The target partition, its open device, and the volumes held out of the way.
///
/// These three belong together because recovering from a refused write means
/// redoing all of them: Windows can mount a volume on top of the sectors being
/// written — a filesystem is recognisable long before it is fully written — and
/// from that moment the physical drive refuses them. The cure is to take the
/// new volume down and reopen; the cause is invisible in the error, which says
/// only "отказано в доступе".
struct TargetWriter {
    target: PartitionTarget,
    dev: WinRawDevice,
    /// `None` when the caller took the whole disk offline, which already
    /// guarantees no volume can appear.
    guard: Option<VolumeDismountGuard>,
}

impl TargetWriter {
    /// Open `target` for writing, with everything mounted on it taken down.
    fn open(target: PartitionTarget, guarded: bool) -> Result<Self> {
        let guard = guarded.then(|| {
            // Settle first: right after the table was rewritten, Windows is
            // still creating and destroying volumes over these sectors, and a
            // guard built during that sees a set that no longer exists a moment
            // later.
            crate::volume_win::wait_for_volumes_to_settle(
                target.disk_number,
                target.offset,
                target.size,
                SETTLE_TIMEOUT,
            );
            VolumeDismountGuard::for_restore_range(target.disk_number, target.offset, target.size)
        });
        if let Some(g) = &guard {
            if !g.failed.is_empty() {
                warn!(volumes = ?g.failed, "some volumes could not be dismounted before the restore");
            }
        }
        let dev = WinRawDevice::open_write(&target.device_path())?;
        Ok(Self { target, dev, guard })
    }

    /// Write, and when Windows takes the sectors away, take them back.
    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        for attempt in 1..=WRITE_ATTEMPTS {
            let failure = match self.dev.try_write_at(offset, data) {
                Ok(()) => return Ok(()),
                Err(f) => f,
            };
            if !failure.volume_in_the_way() || self.guard.is_none() {
                return Err(failure.into_error(&self.target.device_path(), offset, data.len()));
            }
            if attempt == WRITE_ATTEMPTS {
                // Say what is actually holding the sectors. "Отказано в
                // доступе" on a partition the user just created reads as a
                // permissions problem, and no amount of running as
                // Administrator fixes it — the block comes from Windows
                // protecting a volume, and the volume is usually one it left
                // behind over a partition that no longer exists.
                let blocking = crate::volume_win::volumes_overlapping(
                    self.target.disk_number,
                    self.target.offset,
                    self.target.size,
                );
                let volumes = if blocking.is_empty() {
                    tr!("restore_win.volumes_unknown").into_owned()
                } else {
                    blocking.join(", ")
                };
                return Err(OwnBackupError::Device(
                    tr!(
                        "restore_win.sectors_held",
                        offset = self.target.offset,
                        size = self.target.size,
                        failure = failure,
                        volumes = volumes
                    )
                    .into_owned(),
                ));
            }
            warn!(
                attempt,
                offset,
                bytes = data.len(),
                error = %failure,
                "the write was refused — a volume mounted itself over the target; \
                 taking it down and retrying"
            );
            // Drop the old guard before building the new one: its locks are on
            // volumes that may no longer exist, and a lock held on a stale
            // handle keeps nothing away.
            self.guard = None;
            std::thread::sleep(Duration::from_millis(500));
            self.guard = Some(VolumeDismountGuard::for_restore_range(
                self.target.disk_number,
                self.target.offset,
                self.target.size,
            ));
            // Reopening drops the old handle: after ERROR_NO_SUCH_DEVICE it
            // refers to a device stack that has since been rebuilt.
            self.dev = WinRawDevice::open_write(&self.target.device_path())?;
        }
        unreachable!("the loop returns on the last attempt")
    }
}

/// A whole disk the caller took offline and holds open for writing: the
/// whole-disk restore writes through it, a container's slot included
/// (`luks_rebuild::write_disk_data`).
impl SectorTarget for WinRawDevice {
    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        WinRawDevice::write_at(self, offset, data)
    }

    fn flush(&mut self) -> Result<()> {
        WinRawDevice::flush(self)
    }

    fn sector_size(&self) -> u32 {
        WinRawDevice::sector_size(self)
    }
}

impl SectorTarget for TargetWriter {
    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        TargetWriter::write_at(self, offset, data)
    }

    fn flush(&mut self) -> Result<()> {
        self.dev.flush()
    }

    fn sector_size(&self) -> u32 {
        self.dev.sector_size()
    }
}

/// Copy one partition's stored blocks from `image_path` into `target`.
///
/// DESTRUCTIVE. The caller is responsible for the confirmation, for the
/// system-disk guard, and — when the plan rewrites the partition table — for
/// having taken the disk offline first.
// Eight: the image and its key, what to read out of it, where to put it,
// and how to report. Bundling them into a struct would only move the
// same list somewhere the caller has to fill in by hand.
#[allow(clippy::too_many_arguments)]
pub fn restore_partition_sectors(
    image_path: &Path,
    keys: Option<&crate::seal::context::ArchiveKeys>,
    sidecar: &Sidecar,
    src_index: u32,
    target: PartitionTarget,
    opts: SectorRestoreOptions,
    cancel: &CancelToken,
    mut progress: impl FnMut(&SectorRestoreProgress) -> Result<()>,
) -> Result<SectorRestoreSummary> {
    // Refused before the device is opened: opening takes volumes down.
    window_fitting(sidecar, src_index, target.size)?;

    // Nothing may hold the target's sectors while we write them. When the disk
    // is already offline that is guaranteed; otherwise the writer waits for
    // Windows to settle, takes down exactly the volumes in the way, and keeps
    // their lock handles alive until it drops.
    let device_path = target.device_path();
    let mut dev = TargetWriter::open(target, !opts.disk_already_offline)?;
    write_partition_blocks(
        image_path,
        keys,
        sidecar,
        src_index,
        target.size,
        &mut dev,
        &device_path,
        opts,
        cancel,
        &mut progress,
    )
}

/// Verify a restored partition against the copy
/// ([`crate::sector_restore::verify_partition_blocks`]).
// Eight: the image and its key, what to read out of it, where to put it,
// and how to report. Bundling them into a struct would only move the
// same list somewhere the caller has to fill in by hand.
#[allow(clippy::too_many_arguments)]
pub fn verify_partition_sectors(
    image_path: &Path,
    keys: Option<&crate::seal::context::ArchiveKeys>,
    sidecar: &Sidecar,
    src_index: u32,
    target: PartitionTarget,
    crypt: Option<&crate::luks::LuksParams>,
    cancel: &CancelToken,
    mut progress: impl FnMut(&SectorRestoreProgress) -> Result<()>,
) -> Result<VerifySummary> {
    let device_path = target.device_path();
    let mut dev = WinRawDevice::open_read(&device_path)?;
    verify_partition_blocks(
        image_path,
        keys,
        sidecar,
        src_index,
        &mut dev,
        &device_path,
        crypt,
        cancel,
        &mut progress,
    )
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
    pub fn open(
        image_path: &Path,
        keys: Option<&crate::seal::context::ArchiveKeys>,
    ) -> Result<Self> {
        let physical_path = crate::engine_win::mount_image_with_keys(image_path, false, keys)?;
        info!(image = %image_path.display(), device = %physical_path, "image mounted for file restore");
        Ok(Self {
            path: image_path.to_path_buf(),
            physical_path,
        })
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
            OwnBackupError::Device(
                tr!(
                    "restore_win.volume_not_shown",
                    index = src_index,
                    offset = offset
                )
                .into_owned(),
            )
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

/// Restore a partition **with its encryption**
/// ([`crate::sector_restore::write_container_blocks`]).
#[allow(clippy::too_many_arguments)]
pub fn restore_partition_encrypted(
    image_path: &Path,
    keys: Option<&crate::seal::context::ArchiveKeys>,
    sidecar: &Sidecar,
    src_index: u32,
    target: PartitionTarget,
    opts: SectorRestoreOptions,
    req: &ReencryptRequest<'_>,
    cancel: &CancelToken,
    mut progress: impl FnMut(&SectorRestoreProgress) -> Result<()>,
) -> Result<SectorRestoreSummary> {
    let fit = container_fitting(sidecar, src_index, target.size, req)?;
    let device_path = target.device_path();
    let mut dev = TargetWriter::open(target, !opts.disk_already_offline)?;
    let mut read_back = || -> Result<Box<dyn crate::fat_map::RawVolume>> {
        Ok(Box::new(WinRawDevice::open_read(&device_path)?))
    };
    write_container_blocks(
        image_path,
        keys,
        sidecar,
        src_index,
        fit,
        &mut dev,
        &mut read_back,
        &device_path,
        opts,
        req,
        cancel,
        &mut progress,
    )
}
