//! NTFS/ReFS true removal for the qcow2 container (plan Q-R3).
//!
//! qcow2 has no kernel merge like VHDX, so we RW-mount the finished image over
//! our own iSCSI target ([`crate::attach_win`]) with a copy-on-write overlay
//! ([`crate::overlay::WritableOverlaySource`]), let the OS filesystem driver
//! delete the excluded files into the overlay (the base qcow2 is never
//! touched), then fold the touched clusters back into the qcow2 with
//! [`crate::qcow2::Qcow2InPlaceMerge`] and re-checksum the affected backup
//! blocks. The deletion + teardown is the same proven code the VHDX path uses
//! ([`crate::true_removal_win::delete_paths_on_disk`]).
//!
//! Transactional like the VHDX path: any failure BEFORE the merge leaves the
//! image exactly as checksummed (excluded files remain zero-filled ghosts — a
//! valid backup, returned as `Ok(None)`); the merge repoints the qcow2 header
//! last, and a merge/re-checksum failure fails the run (an image that no longer
//! matches its checksums must never ship).

#![cfg(target_os = "windows")]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing::{info, warn};

use own_backup_common::{OwnBackupError, Result};

use crate::block_source::BlockSource;
use crate::exclusions_win::ExcludedPath;
use crate::overlay::WritableOverlaySource;
use crate::qcow2::{Qcow2InPlaceMerge, Qcow2RandomReader};
pub use crate::removal_types::Qcow2RemovalOutcome;
use crate::sidecar::BlockChecksum;
use crate::win_device::WinRawDevice;

fn dev(msg: impl std::fmt::Display) -> OwnBackupError {
    OwnBackupError::Device(format!("qcow2 true removal: {msg}"))
}

fn scratch_path(image_path: &Path) -> PathBuf {
    let mut name = image_path.file_name().unwrap_or_default().to_os_string();
    name.push(".excl.scratch");
    image_path.with_file_name(name)
}

/// Apply exclusions to a finished qcow2 image whose SOURCE could not be walked
/// — the qcow2 twin of [`crate::true_removal_win::exclude_inside_image`].
///
/// A locked BitLocker volume cannot be asked which clusters a file holds, so
/// the rules are applied afterwards, to the image: AIM mounts it read-write
/// over a copy-on-write scratch, the same matcher walks it, the FS driver
/// deletes the matches, and the clusters they held are zeroed — only those the
/// filesystem still calls free. Everything lands in the scratch until the fold
/// below, so any failure before it leaves the base image exactly as checksummed.
///
/// Contract as everywhere in this pass: `Ok(None)` = nothing touched (the copy
/// stands, the rules did not run, the caller says so), `Err` = failure at or
/// after the fold, which the caller must turn into a failed backup.
#[allow(clippy::too_many_arguments)]
pub fn exclude_inside_qcow2(
    image_path: &Path,
    patterns: &[String],
    data_offset: u64,
    volume_len: u64,
    block_size: u32,
    zstd_level: Option<i32>,
    checksums: &mut Vec<BlockChecksum>,
    tick: &mut dyn FnMut(u64, u64),
) -> Result<Option<crate::true_removal_win::InsideRemoval>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let t0 = Instant::now();
    let (virtual_size, cluster_size) = {
        let r = Qcow2RandomReader::open(image_path)?;
        (r.virtual_size(), r.cluster_size())
    };
    info!(
        image = %image_path.display(),
        patterns = patterns.len(),
        cluster_size,
        "qcow2 exclusions inside the image: starting RW mount-&-scan"
    );

    let scratch = scratch_path(image_path);
    let _ = std::fs::remove_file(&scratch);
    let overlay_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&scratch)?;
    let base: Box<dyn BlockSource> = Box::new(Qcow2RandomReader::open(image_path)?);
    let overlay = Arc::new(Mutex::new(WritableOverlaySource::new(
        base,
        cluster_size,
        overlay_file,
    )));

    let mounted = mount_scan_delete(
        overlay.clone(),
        virtual_size,
        image_path,
        patterns,
        data_offset,
        volume_len,
        tick,
    );

    let scanned = match mounted {
        Ok(v) => v,
        Err(e) => {
            warn!(
                error = %e,
                "qcow2 exclusions inside the image: pass failed — the copy keeps every file, \
                 including the excluded ones"
            );
            let _ = std::fs::remove_file(&scratch);
            return Ok(None);
        }
    };

    let touched = overlay.lock().unwrap().touched_clusters();
    if scanned.deleted == 0 || touched.is_empty() {
        info!(
            failed = scanned.failed,
            files_matched = scanned.files_excluded,
            "qcow2 exclusions inside the image: nothing was deleted — discarding the scratch"
        );
        let _ = std::fs::remove_file(&scratch);
        // Nothing matched is an answer; "the pass could not run" is a different
        // one. Report the (empty) scan so the result screen can tell them apart.
        return Ok(Some(crate::true_removal_win::InsideRemoval {
            outcome: crate::true_removal_win::RemovalOutcome {
                entries_deleted: 0,
                entries_failed: scanned.failed,
                blocks_rehashed: 0,
                blocks_added: 0,
            },
            files_excluded: scanned.files_excluded,
            bytes_excluded: scanned.bytes_excluded,
            categories: scanned.categories,
            bytes_zeroed: 0,
        }));
    }

    // Fold the touched clusters into the base qcow2 (in place, header last).
    let t_merge = Instant::now();
    {
        let mut ov = overlay.lock().unwrap();
        let mut merge = Qcow2InPlaceMerge::open(image_path, zstd_level)?;
        for &c in &touched {
            let data = ov.read_cluster(c)?;
            merge.write_cluster(c, &data)?;
        }
        merge.commit()?;
    }
    let merge_ms = t_merge.elapsed().as_millis();
    let _ = std::fs::remove_file(&scratch);

    let blocks_rehashed = rehash_touched(
        image_path,
        &touched,
        cluster_size,
        data_offset,
        volume_len,
        block_size,
        checksums,
        tick,
    )?;

    info!(
        files = scanned.files_excluded,
        deleted = scanned.deleted,
        failed = scanned.failed,
        bytes_zeroed = scanned.bytes_zeroed,
        clusters = touched.len(),
        blocks_rehashed,
        merge_ms,
        secs = t0.elapsed().as_secs(),
        "qcow2 exclusions inside the image: excluded files deleted and their clusters zeroed"
    );
    Ok(Some(crate::true_removal_win::InsideRemoval {
        outcome: crate::true_removal_win::RemovalOutcome {
            entries_deleted: scanned.deleted,
            entries_failed: scanned.failed,
            blocks_rehashed,
            blocks_added: 0,
        },
        files_excluded: scanned.files_excluded,
        bytes_excluded: scanned.bytes_excluded,
        categories: scanned.categories,
        bytes_zeroed: scanned.bytes_zeroed,
    }))
}

/// Attach RW → scan + delete + zero (shared with the VHDX path) → teardown.
/// The session is logged out and the target stopped before returning, whatever
/// happened in between.
///
/// The size guard that used to live here is now in [`crate::attach_win`]: it
/// refuses anything that is not exactly this image before the handle is
/// handed over, which matters twice over here, where the pass also ZEROES
/// sectors (a resolution bug once pointed it at a LIVE host disk, 2026-07-08).
fn mount_scan_delete(
    overlay: Arc<Mutex<WritableOverlaySource>>,
    virtual_size: u64,
    image_path: &Path,
    patterns: &[String],
    data_offset: u64,
    volume_len: u64,
    tick: &mut dyn FnMut(u64, u64),
) -> Result<crate::true_removal_win::ScannedInside> {
    let _automount = crate::automount::AutoMountGuard::enable_if_disabled();
    let attached = crate::attach_win::attach_rw(
        Box::new(overlay),
        virtual_size,
        &crate::iscsi::serial_for_image(image_path),
    )?;
    let result = (|| -> Result<crate::true_removal_win::ScannedInside> {
        let mut d = open_write_retry(&attached.physical)?;
        info!(physical = %attached.physical, disk = attached.disk_number,
              "qcow2 exclusions inside the image: disk attached RW and online");
        let scanned = crate::true_removal_win::scan_delete_zero_on_disk(
            attached.disk_number,
            &mut d,
            patterns,
            data_offset,
            volume_len,
            tick,
        )?;
        drop(d);
        Ok(scanned)
    })();
    attached.detach();
    result
}

/// Delete `paths` from the finished qcow2 image, in place.
///
/// * `Ok(Some(_))` — files removed, image merged + `checksums` updated to
///   describe the final image.
/// * `Ok(None)` — nothing deleted / mount failed before any change; the image
///   is exactly as checksummed (ghosts remain), still a valid backup.
/// * `Err` — a failure at or after the merge: the caller fails the run.
pub fn remove_excluded_from_qcow2(
    image_path: &Path,
    paths: &[ExcludedPath],
    data_offset: u64,
    volume_len: u64,
    block_size: u32,
    zstd_level: Option<i32>,
    checksums: &mut Vec<BlockChecksum>,
    tick: &mut dyn FnMut(u64, u64),
) -> Result<Option<Qcow2RemovalOutcome>> {
    let t0 = Instant::now();
    let (virtual_size, cluster_size) = {
        let r = Qcow2RandomReader::open(image_path)?;
        (r.virtual_size(), r.cluster_size())
    };
    info!(
        image = %image_path.display(),
        paths = paths.len(),
        cluster_size,
        "qcow2 true removal: starting RW mount-&-delete"
    );

    // Copy-on-write scratch over the read-only base (deleted at the end).
    let scratch = scratch_path(image_path);
    let _ = std::fs::remove_file(&scratch);
    let overlay_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&scratch)?;
    let base: Box<dyn BlockSource> = Box::new(Qcow2RandomReader::open(image_path)?);
    let overlay = Arc::new(Mutex::new(WritableOverlaySource::new(
        base,
        cluster_size,
        overlay_file,
    )));

    // Attach RW, delete, tear down. Best-effort: on ANY failure here the base
    // qcow2 is untouched (writes went to the scratch), so we keep ghosts.
    let mount_del = mount_delete(overlay.clone(), virtual_size, image_path, paths, tick);

    let (deleted, failed) = match mount_del {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "qcow2 true removal: mount-&-delete failed — excluded files stay as zero-filled ghosts");
            let _ = std::fs::remove_file(&scratch);
            return Ok(None);
        }
    };

    let touched = overlay.lock().unwrap().touched_clusters();
    if deleted == 0 || touched.is_empty() {
        warn!(
            deleted,
            failed, "qcow2 true removal: nothing deleted — image left with zero-filled ghosts"
        );
        let _ = std::fs::remove_file(&scratch);
        return Ok(None);
    }

    // Fold the touched clusters into the base qcow2 (in place, header last).
    let t_merge = Instant::now();
    {
        let mut ov = overlay.lock().unwrap();
        let mut merge = Qcow2InPlaceMerge::open(image_path, zstd_level)?;
        for &c in &touched {
            let data = ov.read_cluster(c)?;
            merge.write_cluster(c, &data)?;
        }
        merge.commit()?;
    }
    let merge_ms = t_merge.elapsed().as_millis();
    let _ = std::fs::remove_file(&scratch);

    let blocks_rehashed = rehash_touched(
        image_path,
        &touched,
        cluster_size,
        data_offset,
        volume_len,
        block_size,
        checksums,
        tick,
    )?;

    info!(
        deleted,
        failed,
        clusters = touched.len(),
        blocks_rehashed,
        merge_ms,
        secs = t0.elapsed().as_secs(),
        "qcow2 true removal: excluded files deleted from the image by the FS driver"
    );
    Ok(Some(Qcow2RemovalOutcome {
        entries_deleted: deleted,
        entries_failed: failed,
        clusters_merged: touched.len() as u64,
        blocks_rehashed,
    }))
}

/// Multi-volume twin of [`remove_excluded_from_qcow2`] for a whole-disk qcow2
/// image (plan D2): RW-mount the whole image once (AIM over a CoW overlay), let
/// the FS driver delete each partition's paths on the volume at that partition's
/// disk offset, then fold the touched clusters back into the base and re-checksum
/// the affected image blocks. A whole-disk image is disk-block absolute, so the
/// checksum grid covers `0..image_size` (`data_offset = 0`). Same graceful
/// contract as [`remove_excluded_from_qcow2`].
pub fn remove_excluded_from_disk_image_qcow2(
    image_path: &Path,
    partitions: &[(u64, Vec<ExcludedPath>)],
    image_size: u64,
    block_size: u32,
    zstd_level: Option<i32>,
    checksums: &mut Vec<BlockChecksum>,
    tick: &mut dyn FnMut(u64, u64),
) -> Result<Option<Qcow2RemovalOutcome>> {
    let total_paths: usize = partitions.iter().map(|(_, p)| p.len()).sum();
    if total_paths == 0 {
        return Ok(None);
    }
    let t0 = Instant::now();
    let (virtual_size, cluster_size) = {
        let r = Qcow2RandomReader::open(image_path)?;
        (r.virtual_size(), r.cluster_size())
    };
    info!(
        image = %image_path.display(),
        partitions = partitions.iter().filter(|(_, p)| !p.is_empty()).count(),
        paths = total_paths,
        cluster_size,
        "qcow2 true removal: starting whole-disk RW mount-&-delete"
    );

    // CoW scratch over the read-only base (deleted at the end).
    let scratch = scratch_path(image_path);
    let _ = std::fs::remove_file(&scratch);
    let overlay_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&scratch)?;
    let base: Box<dyn BlockSource> = Box::new(Qcow2RandomReader::open(image_path)?);
    let overlay = Arc::new(Mutex::new(WritableOverlaySource::new(
        base,
        cluster_size,
        overlay_file,
    )));

    let mount_del = mount_delete_multi(overlay.clone(), virtual_size, image_path, partitions, tick);

    let (deleted, failed) = match mount_del {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "qcow2 true removal: whole-disk mount-&-delete failed — excluded files stay as zero-filled ghosts");
            let _ = std::fs::remove_file(&scratch);
            return Ok(None);
        }
    };

    let touched = overlay.lock().unwrap().touched_clusters();
    if deleted == 0 || touched.is_empty() {
        warn!(
            deleted,
            failed, "qcow2 true removal: nothing deleted — image left with zero-filled ghosts"
        );
        let _ = std::fs::remove_file(&scratch);
        return Ok(None);
    }

    let t_merge = Instant::now();
    {
        let mut ov = overlay.lock().unwrap();
        let mut merge = Qcow2InPlaceMerge::open(image_path, zstd_level)?;
        for &c in &touched {
            let data = ov.read_cluster(c)?;
            merge.write_cluster(c, &data)?;
        }
        merge.commit()?;
    }
    let merge_ms = t_merge.elapsed().as_millis();
    let _ = std::fs::remove_file(&scratch);

    // Whole-disk image is disk-block absolute: data_offset = 0, len = image_size.
    let blocks_rehashed = rehash_touched(
        image_path,
        &touched,
        cluster_size,
        0,
        image_size,
        block_size,
        checksums,
        tick,
    )?;

    info!(
        deleted,
        failed,
        clusters = touched.len(),
        blocks_rehashed,
        merge_ms,
        secs = t0.elapsed().as_secs(),
        "qcow2 true removal: excluded files deleted from the whole-disk image by the FS driver"
    );
    Ok(Some(Qcow2RemovalOutcome {
        entries_deleted: deleted,
        entries_failed: failed,
        clusters_merged: touched.len() as u64,
        blocks_rehashed,
    }))
}

/// AIM RW mount → online → delete (shared with the VHDX path) → teardown. The
/// AIM device is always removed before returning.
pub(crate) fn mount_delete<S: crate::overlay::SlotStore + 'static>(
    overlay: Arc<Mutex<WritableOverlaySource<S>>>,
    virtual_size: u64,
    image_path: &Path,
    paths: &[ExcludedPath],
    tick: &mut dyn FnMut(u64, u64),
) -> Result<(u64, u64)> {
    // Auto-mount is NOT disabled here (it was until 2026-07-30): `mountvol /N`
    // makes mountmgr hold every surfacing volume OFFLINE, and an offline volume
    // answers ERROR_NOT_READY to every open without a single byte of I/O — the
    // three-minute "filesystem not ready yet" freeze that deleted nothing. The
    // letter is hidden the way the VHDX path hides it instead: stripped as soon
    // as it appears (strip_drive_letters, via delete_paths_on_disk). If the
    // machine has auto-mount off, it is turned on for the mount window only.
    let _automount = crate::automount::AutoMountGuard::enable_if_disabled();
    let attached = crate::attach_win::attach_rw(
        Box::new(overlay),
        virtual_size,
        &crate::iscsi::serial_for_image(image_path),
    )?;
    let result = (|| -> Result<(u64, u64)> {
        let d = open_write_retry(&attached.physical)?;
        info!(physical = %attached.physical, disk = attached.disk_number,
              "qcow2 true removal: disk attached RW and online");
        let counts =
            crate::true_removal_win::delete_paths_on_disk(attached.disk_number, &d, paths, tick)?;
        drop(d);
        Ok(counts)
    })();
    attached.detach();
    result
}

/// Multi-volume twin of [`mount_delete`]: attach RW → per-partition delete on
/// the volume at each partition's disk offset → teardown all volumes. The
/// session is always logged out before returning.
/// Generic over the scratch the driver's writes land in, for the same reason
/// [`mount_delete`] is: an encrypted image may not have a readable one (§7.4).
pub(crate) fn mount_delete_multi<S: crate::overlay::SlotStore + 'static>(
    overlay: Arc<Mutex<WritableOverlaySource<S>>>,
    virtual_size: u64,
    image_path: &Path,
    partitions: &[(u64, Vec<ExcludedPath>)],
    tick: &mut dyn FnMut(u64, u64),
) -> Result<(u64, u64)> {
    // Auto-mount stays as the machine has it — see mount_delete above: turning
    // it off holds the image's volumes offline and the delete pass finds
    // nothing to delete. Letters are stripped after the mount instead
    // (strip_letters_on_disk, via delete_paths_on_disk_multi).
    let _automount = crate::automount::AutoMountGuard::enable_if_disabled();
    let attached = crate::attach_win::attach_rw(
        Box::new(overlay),
        virtual_size,
        &crate::iscsi::serial_for_image(image_path),
    )?;
    let result = (|| -> Result<(u64, u64)> {
        let d = open_write_retry(&attached.physical)?;
        info!(physical = %attached.physical, disk = attached.disk_number,
              "qcow2 true removal: whole-disk image attached RW and online");
        let counts = crate::true_removal_win::delete_paths_on_disk_multi(
            attached.disk_number,
            &d,
            partitions,
            tick,
        )?;
        drop(d);
        Ok(counts)
    })();
    attached.detach();
    result
}

fn open_write_retry(path: &str) -> Result<WinRawDevice> {
    let mut last: Option<OwnBackupError> = None;
    for attempt in 0..20 {
        match WinRawDevice::open_write(path) {
            Ok(d) => return Ok(d),
            Err(e) => {
                last = Some(e);
                std::thread::sleep(Duration::from_millis(100 + 50 * attempt));
            }
        }
    }
    Err(last.unwrap_or_else(|| dev(format!("cannot open {path}"))))
}

/// Recompute checksums for the backup blocks overlapping the touched clusters,
/// reading the FINAL (merged) image. Mirrors
/// [`crate::true_removal_win`]'s `rehash_touched_blocks`.
pub(crate) fn rehash_touched(
    image_path: &Path,
    touched: &[u64],
    cluster_size: u64,
    data_offset: u64,
    volume_len: u64,
    block_size: u32,
    checksums: &mut Vec<BlockChecksum>,
    tick: &mut dyn FnMut(u64, u64),
) -> Result<u64> {
    let mut reader = Qcow2RandomReader::open(image_path)?;
    rehash_touched_through(
        &mut reader,
        touched,
        cluster_size,
        data_offset,
        volume_len,
        block_size,
        checksums,
        tick,
    )
}

/// The same, over an already-open image — an encrypted one is read through its
/// own reader, with the key. Shared with the Linux fold.
pub(crate) use crate::image_fold::rehash_touched_through;
