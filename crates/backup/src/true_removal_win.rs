//! True removal of excluded files from a finished NTFS/ReFS image —
//! "mount & delete via the OS driver" (decision 2026-07-07).
//!
//! The zeroing pass (`exclusions_win`) keeps excluded DATA out of the image,
//! but the file's catalog entry survives and a restored volume shows a
//! zero-filled ghost of the original size. Editing NTFS metadata ourselves is
//! forbidden ("no NTFS parser", CLAUDE.md) and impossible for ReFS (closed
//! format) — so instead the **filesystem driver itself** deletes the files,
//! not on the source volume but inside the finished image copy:
//!
//! 1. Create a TEMPORARY differencing child over the image and attach it —
//!    every write lands in the child; the image stays pristine until the
//!    merge. Any failure before the merge simply discards the child
//!    (transactional: the backup keeps its zero-filled ghosts, nothing lost).
//! 2. Bring the disk online, let the volume mount, and delete each recorded
//!    path with `FILE_FLAG_BACKUP_SEMANTICS` + POSIX semantics — the driver
//!    updates the MFT/$Bitmap/directory indexes (or their ReFS equivalents)
//!    correctly by construction.
//! 3. The child's BAT is the exact list of touched 1 MiB regions. Merge the
//!    child down (`MergeVirtualDisk`) and re-checksum only those blocks.
//!
//! No NTFS/ReFS structure is ever parsed or written by this code — "mounting
//! and FS access go through native Windows facilities only" holds.

#![cfg(target_os = "windows")]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};
use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FileDispositionInfo, FileDispositionInfoEx, SetFileAttributesW,
    SetFileInformationByHandle, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};

use own_backup_common::{OwnBackupError, Result};

use crate::checksum::compute_block_checksum;
use crate::exclusions_win::{list_dir_backup, ExcludedPath};
use crate::sidecar::BlockChecksum;
use crate::vhdx::{attach_vhdx, create_differencing_vhdx, merge_into_parent, AttachMode};
use crate::vhdx_bat::allocated_payload_blocks;
use crate::volume_win::{
    all_volume_roots_on_disk, flush_lock_dismount_volume, strip_drive_letters,
    strip_letters_on_disk, volume_root_at_offset, FileHandle,
};
use crate::win_device::{to_wide, WinRawDevice};

/// Stamped into the start log line so a stale service exe is immediately
/// visible in the log (see "Windows GUI testing gotchas").
const BUILD_TAG: &str = "2026-07-30a-volume-online";

/// How many backup blocks the re-checksum pass may pull in one read.
///
/// 16 MiB at a 1 MiB block size. Kept because coalescing adjacent blocks is
/// obviously right, NOT because it fixed anything: on the HDD it changed
/// nothing at all (see the note on [`rehash_touched_blocks`]).
const REHASH_RUN_BLOCKS: u64 = 16;

/// Standard `DELETE` access right (generic, not in the FILE_* consts).
const DELETE_ACCESS: u32 = 0x0001_0000;

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

// FileDispositionInfoEx payload (Win10 RS1+). Raw layout to stay independent
// of windows-crate binding names.
#[repr(C)]
struct FileDispositionInfoExRaw {
    flags: u32,
}
const FILE_DISPOSITION_FLAG_DELETE: u32 = 0x1;
const FILE_DISPOSITION_FLAG_POSIX_SEMANTICS: u32 = 0x2;
const FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE: u32 = 0x10;

/// Classic FileDispositionInfo payload (fallback).
#[repr(C)]
struct FileDispositionInfoRaw {
    delete_file: u8, // BOOLEAN
}

/// What the removal pass did to the image.
pub struct RemovalOutcome {
    /// Filesystem entries deleted (files + directories + reparse points,
    /// including everything inside recursively deleted directories).
    pub entries_deleted: u64,
    /// Entries that could not be deleted (kept in the image; warned).
    pub entries_failed: u64,
    /// Backup blocks whose checksum was recomputed after the merge.
    pub blocks_rehashed: u64,
    /// Checksum entries newly added (driver wrote into previously-free space,
    /// e.g. USN journal growth).
    pub blocks_added: u64,
}

fn dev_err(what: &str, e: impl std::fmt::Display) -> OwnBackupError {
    OwnBackupError::Device(format!("{what}: {e}"))
}

/// Delete every recorded excluded path from the finished image by mounting a
/// temporary differencing child and letting the FS driver do the deletions,
/// then merge the child down and update `checksums` for the touched blocks.
///
/// Returns:
/// * `Ok(Some(outcome))` — removal merged into the image, checksums updated;
/// * `Ok(None)` — removal SKIPPED before anything touched the image (attach /
///   mount / delete failure): the image is exactly as checksummed, excluded
///   files remain as zero-filled ghosts. Already logged; safe to continue.
/// * `Err(_)` — the merge or re-checksum failed: the image may be
///   inconsistent with its checksums. The caller must fail the backup.
///
/// `tick` is pumped periodically (deleted-entry count) so the UI's finalize
/// phase shows life; cancellation is deliberately ignored — the copy is done
/// and this pass is short.
pub fn remove_excluded_from_image(
    image_path: &Path,
    paths: &[ExcludedPath],
    data_offset: u64,
    volume_len: u64,
    block_size: u32,
    checksums: &mut Vec<BlockChecksum>,
    tick: &mut dyn FnMut(u64),
) -> Result<Option<RemovalOutcome>> {
    if paths.is_empty() {
        return Ok(None);
    }
    let temp = temp_child_path(image_path);
    info!(
        image = %image_path.display(),
        temp_child = %temp.display(),
        paths = paths.len(),
        build = BUILD_TAG,
        "true removal: starting mount-&-delete pass over the finished image"
    );
    let _ = std::fs::remove_file(&temp);

    // --- Phase A (transactional): all writes land in the temp child. ---
    let t_mount = Instant::now();
    let deleted = match mount_and_delete(image_path, &temp, paths, tick) {
        Ok(counts) => counts,
        Err(e) => {
            warn!(
                error = %e,
                "true removal: mount-&-delete failed — excluded files stay as zero-filled ghosts \
                 (на ReFS с integrity streams чтение таких призраков вернёт ошибку — это ожидаемо)"
            );
            let _ = std::fs::remove_file(&temp);
            return Ok(None);
        }
    };
    let mount_delete_ms = t_mount.elapsed().as_millis();
    if deleted.0 == 0 {
        // Nothing was deleted → nothing worth merging; the mount itself may
        // have dirtied a few metadata blocks in the child, but discarding it
        // keeps the image byte-identical to its checksums.
        warn!(
            failed = deleted.1,
            "true removal: no entries could be deleted — image left with zero-filled ghosts \
             (на ReFS с integrity streams чтение таких призраков вернёт ошибку — это ожидаемо)"
        );
        let _ = std::fs::remove_file(&temp);
        return Ok(None);
    }

    // The child's BAT = exact set of touched 1 MiB regions. Still fallback-able:
    // the image is untouched until the merge.
    let t_bat = Instant::now();
    let alloc = match allocated_payload_blocks(&temp) {
        Ok(a) if !a.blocks.is_empty() => a,
        Ok(_) => {
            warn!("true removal: child BAT empty after deletions — discarding");
            let _ = std::fs::remove_file(&temp);
            return Ok(None);
        }
        Err(e) => {
            warn!(error = %e, "true removal: cannot read child BAT — discarding");
            let _ = std::fs::remove_file(&temp);
            return Ok(None);
        }
    };
    let bat_ms = t_bat.elapsed().as_millis();
    info!(
        deleted = deleted.0,
        failed = deleted.1,
        touched_regions = alloc.blocks.len(),
        mount_delete_ms,
        bat_ms,
        "true removal: FS driver deleted the entries; merging child into the image"
    );

    // --- Phase B (commits): merge the deletions into the image. From here a
    // failure means image/checksum inconsistency → hard error. ---
    let t_merge = Instant::now();
    merge_into_parent(&temp)?;
    let _ = std::fs::remove_file(&temp);
    let merge_ms = t_merge.elapsed().as_millis();

    let t_rehash = Instant::now();
    let (rehashed, added) =
        rehash_touched_blocks(image_path, &alloc, data_offset, volume_len, block_size, checksums, tick)?;
    let rehash_ms = t_rehash.elapsed().as_millis();

    let outcome = RemovalOutcome {
        entries_deleted: deleted.0,
        entries_failed: deleted.1,
        blocks_rehashed: rehashed,
        blocks_added: added,
    };
    info!(
        deleted = outcome.entries_deleted,
        failed = outcome.entries_failed,
        touched_regions = alloc.blocks.len(),
        blocks_rehashed = outcome.blocks_rehashed,
        blocks_added = outcome.blocks_added,
        mount_delete_ms,
        bat_ms,
        merge_ms,
        rehash_ms,
        "true removal: excluded files deleted from the image by the FS driver"
    );
    Ok(Some(outcome))
}

/// What the "exclude inside the finished image" pass did — the removal outcome
/// plus the figures the result screen shows, which on this path can only be
/// learned from the image's own filesystem.
pub struct InsideRemoval {
    pub outcome: RemovalOutcome,
    pub files_excluded: u64,
    pub bytes_excluded: u64,
    pub categories: crate::exclusions::CategorySizes,
    /// Bytes actually overwritten with zeros inside the image.
    pub bytes_zeroed: u64,
}

/// Apply file exclusions to an image whose SOURCE could not be walked.
///
/// A locked BitLocker volume is not mounted by Windows, so nothing can be asked
/// which clusters a file occupies before the copy — which is why exclusions
/// used to be dropped for such a source, leaving twelve configured rules with
/// no effect and no word about it. The image, though, holds that volume's
/// filesystem in the clear: it can be mounted, walked, and edited by the very
/// driver that owns the format.
///
/// So the work moves after the copy, into a temporary differencing child:
///
/// 1. mount the child and walk it with the same matcher live volumes get;
/// 2. let the FS driver delete every match — the catalog entry goes, exactly as
///    the product requires ("настоящее удаление", CLAUDE.md);
/// 3. dismount, then zero the clusters those files held — but only the ones the
///    filesystem still reports as FREE, so a cluster it took back for its own
///    metadata during the deletions is never overwritten;
/// 4. merge the child down and re-checksum the touched blocks.
///
/// Step 3 is what makes this a removal rather than a rename: without it the
/// file data would sit in the image's free space, out of the catalog but fully
/// recoverable — the "zero-filled ghost" the product refuses, inside out.
///
/// Same graceful contract as [`remove_excluded_from_image`]: `Ok(None)` means
/// nothing was touched (the copy stands, exclusions simply did not happen and
/// the caller says so), `Err` means the merge or re-checksum failed and the
/// backup must fail with it.
#[allow(clippy::too_many_arguments)]
pub fn exclude_inside_image(
    image_path: &Path,
    patterns: &[String],
    data_offset: u64,
    volume_len: u64,
    block_size: u32,
    checksums: &mut Vec<BlockChecksum>,
    tick: &mut dyn FnMut(u64),
) -> Result<Option<InsideRemoval>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let temp = temp_child_path(image_path);
    info!(
        image = %image_path.display(),
        temp_child = %temp.display(),
        patterns = patterns.len(),
        build = BUILD_TAG,
        "exclusions inside the image: starting scan-&-delete pass over the finished image"
    );
    let _ = std::fs::remove_file(&temp);

    // --- Phase A (transactional): everything lands in the temp child. ---
    let t_mount = Instant::now();
    let scanned = match scan_delete_and_zero(&temp, image_path, patterns, data_offset, volume_len, tick)
    {
        Ok(s) => s,
        Err(e) => {
            warn!(
                error = %e,
                "exclusions inside the image: pass failed — the copy keeps every file, \
                 including the excluded ones"
            );
            let _ = std::fs::remove_file(&temp);
            return Ok(None);
        }
    };
    let mount_ms = t_mount.elapsed().as_millis();
    if scanned.deleted == 0 {
        info!(
            failed = scanned.failed,
            files_matched = scanned.files_excluded,
            "exclusions inside the image: nothing was deleted — discarding the child"
        );
        let _ = std::fs::remove_file(&temp);
        // Nothing matched is a legitimate answer, and a different one from
        // "the pass could not run": report the (empty) scan so the result
        // screen says "исключено 0" rather than "исключения не применялись".
        return Ok(Some(InsideRemoval {
            outcome: RemovalOutcome {
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

    let alloc = match allocated_payload_blocks(&temp) {
        Ok(a) if !a.blocks.is_empty() => a,
        Ok(_) => {
            warn!("exclusions inside the image: child BAT empty after deletions — discarding");
            let _ = std::fs::remove_file(&temp);
            return Ok(None);
        }
        Err(e) => {
            warn!(error = %e, "exclusions inside the image: cannot read child BAT — discarding");
            let _ = std::fs::remove_file(&temp);
            return Ok(None);
        }
    };

    // --- Phase B (commits): from here a failure means the image and its
    // checksums disagree, which is a failed backup, not a warning. ---
    let t_merge = Instant::now();
    merge_into_parent(&temp)?;
    let _ = std::fs::remove_file(&temp);
    let merge_ms = t_merge.elapsed().as_millis();

    let (rehashed, added) =
        rehash_touched_blocks(image_path, &alloc, data_offset, volume_len, block_size, checksums, tick)?;

    let out = InsideRemoval {
        outcome: RemovalOutcome {
            entries_deleted: scanned.deleted,
            entries_failed: scanned.failed,
            blocks_rehashed: rehashed,
            blocks_added: added,
        },
        files_excluded: scanned.files_excluded,
        bytes_excluded: scanned.bytes_excluded,
        categories: scanned.categories,
        bytes_zeroed: scanned.bytes_zeroed,
    };
    info!(
        files = out.files_excluded,
        deleted = out.outcome.entries_deleted,
        failed = out.outcome.entries_failed,
        bytes_excluded = out.bytes_excluded,
        bytes_zeroed = out.bytes_zeroed,
        blocks_rehashed = out.outcome.blocks_rehashed,
        blocks_added = out.outcome.blocks_added,
        mount_ms,
        merge_ms,
        "exclusions inside the image: excluded files deleted and their clusters zeroed"
    );
    Ok(Some(out))
}

/// Result of the in-image pass before the merge. Shared by both image formats:
/// the disk under it is a differencing VHDX child or an AIM proxy over a qcow2
/// overlay, and from here neither is distinguishable.
pub(crate) struct ScannedInside {
    pub(crate) deleted: u64,
    pub(crate) failed: u64,
    pub(crate) files_excluded: u64,
    pub(crate) bytes_excluded: u64,
    pub(crate) categories: crate::exclusions::CategorySizes,
    pub(crate) bytes_zeroed: u64,
}

/// Attach a differencing child, mount it, scan + delete, dismount, zero the
/// freed clusters, detach. Everything written lands in the child.
fn scan_delete_and_zero(
    temp: &Path,
    image_path: &Path,
    patterns: &[String],
    data_offset: u64,
    volume_len: u64,
    tick: &mut dyn FnMut(u64),
) -> Result<ScannedInside> {
    let _automount = crate::automount::AutoMountGuard::enable_if_disabled();
    create_differencing_vhdx(temp, image_path)?;
    let attached = attach_vhdx(temp, AttachMode::RawWrite)?;
    let mut dev = open_write_retry(attached.physical_path())?;
    let _ = dev.update_properties();
    dev.set_online_this_session()?;
    let disk_number = disk_number_of(attached.physical_path())?;
    let scanned =
        scan_delete_zero_on_disk(disk_number, &mut dev, patterns, data_offset, volume_len, tick)?;
    drop(dev);
    drop(attached); // detaches the child
    Ok(scanned)
}

/// Scan the filesystem inside an already-attached, online image disk, delete
/// what matches, and zero what that freed. Format-independent: the caller has
/// put a writable disk in front of us, whether by attaching a differencing
/// VHDX child or by proxying a qcow2 overlay through AIM.
pub(crate) fn scan_delete_zero_on_disk(
    disk_number: u32,
    dev: &mut WinRawDevice,
    patterns: &[String],
    data_offset: u64,
    volume_len: u64,
    tick: &mut dyn FnMut(u64),
) -> Result<ScannedInside> {
    // The volume at the offset the copy wrote it to — not merely "the first
    // volume on this disk". A per-volume image also carries the meta partition,
    // and scanning one volume while deleting on another is not a mistake worth
    // leaving to enumeration order.
    let volume_root = crate::volume_win::volume_root_at_offset(disk_number, data_offset)
        .ok_or_else(|| {
            dev_err(
                "exclusions inside the image",
                format!("no volume surfaced at offset {data_offset} of the attached image"),
            )
        })?;

    // 1. Walk the image's own filesystem with the matcher live volumes get.
    // `collect_excluded_ranges` also resolves each match's extents, which is
    // what the zero pass below needs, and counts the kept files by category —
    // the breakdown this source could not produce any other way.
    let entries = wait_for_volume_ready(&volume_root);
    if entries == 0 {
        return Err(dev_err(
            "exclusions inside the image",
            "the filesystem in the image never mounted",
        ));
    }
    // The cluster size comes from the image's own boot sector rather than from
    // Windows: it is one read, and it doubles as the check that what we are
    // about to walk really is NTFS (nothing below can read a ReFS free map).
    let cluster_size = {
        let mut window = image_volume_window(dev, data_offset, volume_len)?;
        crate::ntfs_map::ntfs_cluster_size(&mut window)?
    };
    let scan = crate::exclusions_win::collect_excluded_ranges(
        volume_root.trim_end_matches('\\'),
        cluster_size,
        patterns,
        |n| {
            tick(n);
            Ok(())
        },
    )?;
    info!(
        matched = scan.files_excluded,
        bytes = scan.bytes_excluded,
        entries_scanned = scan.entries_scanned,
        "exclusions inside the image: scan done"
    );

    // 2. The driver deletes them, then the volume is torn down — which is also
    // what keeps the detach below from waiting out the ~180 s PnP removal veto
    // (see `delete_paths_on_disk`, whose body this mirrors for a volume we
    // located ourselves).
    let (deleted, failed) = delete_paths_on_volume_root(&volume_root, &scan.paths, tick)?;
    teardown_volume(&volume_root, dev);
    let gone = wait_volume_gone(disk_number, Duration::from_secs(10));
    debug!(gone, "exclusions inside the image: volume state before the zero pass");

    // 3. With the volume dismounted, nothing is writing to the filesystem any
    // more — so the bitmap read here is final, and zeroing what it calls free
    // cannot race the driver.
    let mut bytes_zeroed = 0u64;
    if deleted > 0 && !scan.ranges.is_empty() {
        match zero_freed_clusters(dev, data_offset, volume_len, &scan.ranges, tick) {
            Ok(n) => bytes_zeroed = n,
            // The entries are gone from the catalog either way; their data
            // lingering in free space is worth a loud warning, not a lost copy.
            Err(e) => warn!(
                error = %e,
                "exclusions inside the image: could not zero the freed clusters — the deleted \
                 files' data stays in the image's free space"
            ),
        }
    }

    Ok(ScannedInside {
        deleted,
        failed,
        files_excluded: scan.files_excluded,
        bytes_excluded: scan.bytes_excluded,
        categories: scan.categories,
        bytes_zeroed,
    })
}

/// A reader over the volume INSIDE the attached image — the same bytes the
/// mounted filesystem sits on, addressed from the volume's own start.
fn image_volume_window(
    dev: &WinRawDevice,
    data_offset: u64,
    volume_len: u64,
) -> Result<crate::aligned_volume::AlignedVolume<crate::volume_window::VolumeWindow>> {
    Ok(crate::aligned_volume::AlignedVolume::new(
        crate::volume_window::VolumeWindow::new(
            Box::new(WinRawDevice::open_read(dev.path())?),
            data_offset,
            volume_len,
        )?,
    ))
}

/// Overwrite with zeros the parts of `ranges` (volume-relative) that the
/// filesystem now reports as free. Returns the number of bytes written.
///
/// Only free clusters are touched. The extent list was collected BEFORE the
/// deletions, and between then and now the filesystem has been writing — its
/// own metadata, its journal — and may have taken some of those clusters back.
/// The bitmap is the only witness to that, so it is read here, after the
/// volume was dismounted and nothing can change it any more.
fn zero_freed_clusters(
    dev: &mut WinRawDevice,
    data_offset: u64,
    volume_len: u64,
    ranges: &crate::exclusions::ByteRanges,
    tick: &mut dyn FnMut(u64),
) -> Result<u64> {
    let mut window = image_volume_window(dev, data_offset, volume_len)?;
    let free = crate::ntfs_map::ntfs_free_map(&mut window)?;
    let wanted: Vec<(u64, u64)> = ranges.iter().collect();
    let parts = free.free_parts_of(&wanted);
    let skipped = ranges.total_bytes() - parts.iter().map(|(s, e)| e - s).sum::<u64>();
    if skipped > 0 {
        info!(
            bytes = skipped,
            "exclusions inside the image: some freed clusters were taken back by the filesystem \
             and are left untouched"
        );
    }

    // Written through the handle the disk was offlined under: that offline is
    // exactly what makes raw sector writes to an attached VHDX legal (a mounted
    // volume answers ERROR_ACCESS_DENIED — see `set_offline_writable`).
    // 16 MiB: the zeroing is the one purely sequential part of the pass, and on
    // a spinning disk a bigger write is simply fewer of them.
    const CHUNK: usize = 16 * 1024 * 1024;
    let zeros = vec![0u8; CHUNK];
    let mut written = 0u64;
    for (start, end) in parts {
        let mut at = start;
        while at < end {
            let len = ((end - at) as usize).min(CHUNK);
            dev.write_at(data_offset + at, &zeros[..len])?;
            at += len as u64;
            written += len as u64;
            tick(written / CHUNK as u64);
        }
    }
    dev.flush()?;
    Ok(written)
}

fn temp_child_path(image_path: &Path) -> PathBuf {
    let mut name = image_path.file_name().unwrap_or_default().to_os_string();
    name.push(".excl.avhdx");
    image_path.with_file_name(name)
}

/// Attach a differencing child of `image_path`, mount its volume, delete the
/// paths, dismount cleanly. Returns (deleted, failed) entry counts.
fn mount_and_delete(
    image_path: &Path,
    temp: &Path,
    paths: &[ExcludedPath],
    tick: &mut dyn FnMut(u64),
) -> Result<(u64, u64)> {
    // NB: we do NOT disable auto-mount here — `mountvol /N` stops the NTFS
    // volume from mounting at all (device-not-ready), so the delete pass would
    // wait out the full 180 s and delete nothing. Instead the surfacing volume's
    // drive letter (and its remembered mapping) is stripped once it mounts (see
    // strip_drive_letters), so it only flashes briefly. A machine that has
    // auto-mount off for its own reasons gets it back on for the mount window.
    let _automount = crate::automount::AutoMountGuard::enable_if_disabled();
    create_differencing_vhdx(temp, image_path)?;
    debug!(temp_child = %temp.display(), "true removal: temp differencing child created");

    // The image's GPT GUID carries a persisted "offline" attribute, so the
    // child arrives offline (no automount race); we bring it online on purpose.
    let attached = attach_vhdx(temp, AttachMode::RawWrite)?;
    let dev = open_write_retry(attached.physical_path())?;
    let _ = dev.update_properties();
    dev.set_online_this_session()?;
    debug!(physical = %attached.physical_path(), "true removal: child attached RW and brought online");

    let disk_number = disk_number_of(attached.physical_path())?;
    let (deleted, failed) = delete_paths_on_disk(disk_number, &dev, paths, tick)?;
    drop(dev);
    let t_detach = Instant::now();
    drop(attached); // detaches the child
    info!(detach_ms = t_detach.elapsed().as_millis(), "true removal: temp child detached");

    Ok((deleted, failed))
}

/// Multi-volume twin of [`remove_excluded_from_image`] for a whole-disk image
/// (plan D2). Mounts the whole-disk VHDX ONCE (a temp differencing child), and
/// for each partition deletes its recorded paths on the volume sitting at that
/// partition's disk offset, then merges the child down and re-checksums the
/// touched image blocks. Same graceful contract as the single-volume version:
///
/// * `Ok(Some(outcome))` — deletions merged, `checksums` updated;
/// * `Ok(None)` — SKIPPED before the merge (attach/mount/delete failure), the
///   image is exactly as checksummed (excluded NTFS files stay as zero-filled
///   ghosts) — safe to continue;
/// * `Err(_)` — the merge or re-checksum failed → the caller must fail the run.
///
/// `partitions`: (partition disk byte offset, its excluded paths). `image_size`
/// is the whole-disk image size (a whole-disk image is disk-block absolute, so
/// the checksum grid covers `0..image_size`). VHDX only — a qcow2 whole-disk
/// image keeps zero-filled ghosts for now (the AIM multi-volume path is a
/// follow-up).
pub fn remove_excluded_from_disk_image(
    image_path: &Path,
    partitions: &[(u64, Vec<ExcludedPath>)],
    image_size: u64,
    block_size: u32,
    checksums: &mut Vec<BlockChecksum>,
    tick: &mut dyn FnMut(u64),
) -> Result<Option<RemovalOutcome>> {
    let total_paths: usize = partitions.iter().map(|(_, p)| p.len()).sum();
    if total_paths == 0 {
        return Ok(None);
    }
    let temp = temp_child_path(image_path);
    info!(
        image = %image_path.display(),
        temp_child = %temp.display(),
        partitions = partitions.iter().filter(|(_, p)| !p.is_empty()).count(),
        paths = total_paths,
        build = BUILD_TAG,
        "true removal: starting whole-disk mount-&-delete pass"
    );
    let _ = std::fs::remove_file(&temp);

    // --- Phase A (transactional): all writes land in the temp child. ---
    let t_mount = Instant::now();
    let deleted = match mount_and_delete_multi(image_path, &temp, partitions, tick) {
        Ok(counts) => counts,
        Err(e) => {
            warn!(error = %e, "true removal: whole-disk mount-&-delete failed — excluded files stay as zero-filled ghosts");
            let _ = std::fs::remove_file(&temp);
            return Ok(None);
        }
    };
    let mount_delete_ms = t_mount.elapsed().as_millis();
    if deleted.0 == 0 {
        warn!(failed = deleted.1, "true removal: nothing deleted — image left with zero-filled ghosts");
        let _ = std::fs::remove_file(&temp);
        return Ok(None);
    }

    let alloc = match allocated_payload_blocks(&temp) {
        Ok(a) if !a.blocks.is_empty() => a,
        Ok(_) => {
            warn!("true removal: child BAT empty after deletions — discarding");
            let _ = std::fs::remove_file(&temp);
            return Ok(None);
        }
        Err(e) => {
            warn!(error = %e, "true removal: cannot read child BAT — discarding");
            let _ = std::fs::remove_file(&temp);
            return Ok(None);
        }
    };

    // --- Phase B (commits): merge + re-checksum. A failure here means
    // image/checksum inconsistency → hard error. A whole-disk image is
    // disk-block absolute (data_offset = 0, volume_len = image_size). ---
    merge_into_parent(&temp)?;
    let _ = std::fs::remove_file(&temp);
    let (rehashed, added) =
        rehash_touched_blocks(image_path, &alloc, 0, image_size, block_size, checksums, tick)?;

    let outcome = RemovalOutcome {
        entries_deleted: deleted.0,
        entries_failed: deleted.1,
        blocks_rehashed: rehashed,
        blocks_added: added,
    };
    info!(
        deleted = outcome.entries_deleted,
        failed = outcome.entries_failed,
        touched_regions = alloc.blocks.len(),
        blocks_rehashed = outcome.blocks_rehashed,
        blocks_added = outcome.blocks_added,
        mount_delete_ms,
        "true removal: excluded files deleted from the whole-disk image by the FS driver"
    );
    Ok(Some(outcome))
}

/// Attach a differencing child of the whole-disk image, bring it online (all
/// partitions mount), delete each partition's paths on the volume at its disk
/// offset, then dismount every volume and detach. Returns (deleted, failed).
fn mount_and_delete_multi(
    image_path: &Path,
    temp: &Path,
    partitions: &[(u64, Vec<ExcludedPath>)],
    tick: &mut dyn FnMut(u64),
) -> Result<(u64, u64)> {
    // Do NOT disable auto-mount (it blocks the NTFS volume from mounting at all
    // → 180 s device-not-ready + nothing deleted). Letters are stripped after
    // mount instead (strip_letters_on_disk below); a machine that keeps
    // auto-mount off gets it back on for the mount window.
    let _automount = crate::automount::AutoMountGuard::enable_if_disabled();
    create_differencing_vhdx(temp, image_path)?;
    let attached = attach_vhdx(temp, AttachMode::RawWrite)?;
    let dev = open_write_retry(attached.physical_path())?;
    let _ = dev.update_properties();
    dev.set_online_this_session()?;
    let disk_number = disk_number_of(attached.physical_path())?;
    debug!(physical = %attached.physical_path(), disk_number, "true removal: whole-disk child attached RW and online");

    let counts = delete_paths_on_disk_multi(disk_number, &dev, partitions, tick)?;
    drop(dev);
    let t_detach = Instant::now();
    drop(attached); // detaches the child
    info!(detach_ms = t_detach.elapsed().as_millis(), "true removal: whole-disk temp child detached");
    Ok(counts)
}

/// Multi-volume delete + teardown on an already-attached, online disk
/// (`disk_number`, write handle `dev`). For each partition it finds the volume
/// at that partition's disk offset and deletes its paths; then it locks +
/// dismounts EVERY volume on the disk (invalidating foreign handles so the
/// subsequent detach avoids the ~180 s PnP removal-veto), holding all locks
/// across a single disk offline so nothing re-mounts. Shared by the whole-disk
/// VHDX child path and the qcow2 AIM path. Returns (deleted, failed).
pub(crate) fn delete_paths_on_disk_multi(
    disk_number: u32,
    dev: &WinRawDevice,
    partitions: &[(u64, Vec<ExcludedPath>)],
    tick: &mut dyn FnMut(u64),
) -> Result<(u64, u64)> {
    // Letters can land on ANY volume of the disk (also partitions we are not
    // deleting from) via remembered mappings — strip them all up front, and
    // again before teardown for stragglers assigned mid-pass.
    strip_letters_on_disk(disk_number);
    let mut deleted = 0u64;
    let mut failed = 0u64;
    for (part_offset, paths) in partitions {
        if paths.is_empty() {
            continue;
        }
        match volume_root_at_offset(disk_number, *part_offset) {
            Some(vr) => match delete_paths_on_volume_root(&vr, paths, tick) {
                Ok((d, f)) => {
                    deleted += d;
                    failed += f;
                }
                Err(e) => {
                    // This volume never mounted (e.g. a filesystem Windows can't
                    // mount) — keep its files as ghosts, continue with the rest.
                    warn!(part_offset, error = %e, "true removal: skipping a partition whose volume did not mount");
                    failed += paths.len() as u64;
                }
            },
            None => {
                warn!(part_offset, "true removal: no mounted volume at this partition offset — paths kept as ghosts");
                failed += paths.len() as u64;
            }
        }
    }

    let roots = all_volume_roots_on_disk(disk_number);
    for vr in &roots {
        strip_drive_letters(vr); // stragglers assigned while we were deleting
    }
    let handles: Vec<_> = roots.iter().filter_map(|vr| flush_lock_dismount_volume(vr)).collect();
    if let Err(e) = dev.set_offline_writable(false) {
        warn!(error = %e, "true removal: could not offline the mounted disk before detach");
    }
    let _ = dev.update_properties();
    drop(handles); // release all locks with the disk already offline
    let t_gone = Instant::now();
    let gone = wait_volume_gone(disk_number, Duration::from_secs(10));
    info!(gone, gone_ms = t_gone.elapsed().as_millis(), "true removal: volumes torn down before detach");
    Ok((deleted, failed))
}




/// Delete `paths` on one already-mounted volume (identified by its
/// `\\?\Volume{GUID}\` root), waiting for its filesystem to become ready first.
/// No teardown — the caller dismounts the volume(s). Returns (deleted, failed).
/// `Err` if the filesystem never mounted (the caller keeps zero-filled ghosts).
fn delete_paths_on_volume_root(
    volume_root: &str,
    paths: &[ExcludedPath],
    tick: &mut dyn FnMut(u64),
) -> Result<(u64, u64)> {
    // No lettered drive must surface for the image volume: strip any letter the
    // MountManager already restored from its remembered mapping, and strip again
    // after the FS mount settles (assignment can lag the volume's arrival). We
    // work through the \\?\Volume{GUID}\ path, so no letter is ever needed.
    strip_drive_letters(volume_root);
    // The volume DEVICE enumerating does not mean the filesystem is MOUNTED: a
    // natively-attached VHDX is ready instantly, but the userspace-backed
    // AIM/devio disk (qcow2) mounts asynchronously. Deleting before the FS is up
    // makes almost every path lookup fail with "not found" (root entries resolve
    // but deeper directories do not). Wait for the root to become readable and
    // stable first; the logged count also tells us from the log alone whether the
    // FS was actually mounted.
    let root_entries = wait_for_volume_ready(volume_root);
    strip_drive_letters(volume_root);
    info!(volume = %volume_root, root_entries, "true removal: image volume mounted");
    if root_entries == 0 {
        return Err(OwnBackupError::Device(format!(
            "filesystem on {volume_root} did not become ready — aborting the delete pass"
        )));
    }

    let t_del = Instant::now();
    let mut deleted = 0u64;
    let mut failed = 0u64;
    for p in paths {
        // Skip System Volume Information and anything inside it. Windows
        // automatically recreates and locks this folder upon mounting, so trying
        // to delete it is futile and causes sharing-violation warnings.
        let is_sys_vol_info = p.rel.eq_ignore_ascii_case("System Volume Information")
            || (p.rel.len() >= 26 && p.rel.as_bytes()[..26].eq_ignore_ascii_case(b"System Volume Information\\"));
        if is_sys_vol_info {
            debug!(path = %p.rel, "true removal: skipping System Volume Information to avoid sharing violations");
            continue;
        }
        let full = format!("{volume_root}{}", p.rel);
        delete_tree(&full, p.is_dir, p.is_reparse, &mut deleted, &mut failed);
        if (deleted + failed) % 64 == 0 {
            tick(deleted);
        }
    }
    tick(deleted);
    info!(
        volume = %volume_root,
        deleted,
        failed,
        delete_ms = t_del.elapsed().as_millis(),
        "true removal: deletions done on volume"
    );
    Ok((deleted, failed))
}

/// Delete `paths` from the (already online) mounted volume on `disk_number`,
/// then tear the volume down cleanly so the caller can detach/remove the disk
/// without hitting the ~180 s PnP removal-veto timeout. `dev` is a write handle
/// to the disk. Shared by the VHDX child path above and the qcow2 AIM path
/// ([`crate::qcow2_removal_win`]). Returns (deleted, failed) entry counts.
pub(crate) fn delete_paths_on_disk(
    disk_number: u32,
    dev: &WinRawDevice,
    paths: &[ExcludedPath],
    tick: &mut dyn FnMut(u64),
) -> Result<(u64, u64)> {
    let volume_root = find_volume_on_disk(disk_number)?;
    let (deleted, failed) = delete_paths_on_volume_root(&volume_root, paths, tick)?;

    // Tear the mounted volume down BEFORE detaching, or DetachVirtualDisk hits
    // the fixed ~180 s PnP removal-veto timeout (the stall the main image
    // avoids by never mounting — see engine_win / san_policy). Bringing the disk
    // online mounted the volume, and system services (storsvc writing System
    // Volume Information, Defender, the indexer) immediately opened handles on
    // it. Merely offlining does NOT invalidate them, so QUERY_REMOVE gets
    // vetoed. A successful FSCTL_LOCK_VOLUME invalidates every foreign handle —
    // take the lock (retrying while services finish their post-mount writes),
    // dismount, offline while still holding the lock, then let the caller detach.
    teardown_volume(&volume_root, dev);
    let t_gone = Instant::now();
    let gone = wait_volume_gone(disk_number, Duration::from_secs(10));
    info!(
        gone,
        gone_ms = t_gone.elapsed().as_millis(),
        "true removal: volume state before detach"
    );
    Ok((deleted, failed))
}

/// How long to wait for the filesystem inside the image to mount. A natively
/// attached VHDX is ready in under a second; the userspace-backed AIM/devio
/// disk (qcow2) decompresses a 64 KiB zstd cluster for every 4 KiB the kernel
/// reads, and an RW mount additionally replays/writes the NTFS log — minutes,
/// not seconds (the 23:55 run was still not ready at ~57 s while the same
/// image mounted fine read-only for browsing).
const VOLUME_READY_TIMEOUT: Duration = Duration::from_secs(180);

/// How long a volume may keep answering `ERROR_NOT_READY` before we stop
/// waiting. That code does not mean "still mounting" — a mount in progress
/// BLOCKS inside the open — it means the volume manager is holding the volume
/// offline and will keep doing so (auto-mount disabled is the usual reason).
/// Waiting out the full [`VOLUME_READY_TIMEOUT`] for that only turns a failed
/// pass into a three-minute freeze at the end of the backup (2026-07-30).
/// Kept above the slowest mount ever observed to still succeed (~57 s) so a
/// genuinely slow qcow2/AIM mount is never cut off by this.
const NOT_READY_GIVE_UP: Duration = Duration::from_secs(60);

/// `ERROR_NOT_READY` as it appears in a formatted `windows::core::Error`. The
/// hex code is the locale-independent part of the message ("Устройство не
/// готово. (0x80070015)" on a Russian host).
const NOT_READY_HRESULT: &str = "0x80070015";

/// Wait until the volume's filesystem is mounted and its root directory is
/// readable, so deletions do not race an incomplete mount. Polls the root entry
/// count until it is non-zero and stable across two reads, or a timeout. Returns
/// the final entry count (0 = the FS never came up — a clear diagnostic).
///
/// A volume answering `ERROR_NOT_READY` is not mounting slowly, it is being held
/// offline; the poll therefore also asks the volume manager to bring it online
/// (`IOCTL_VOLUME_ONLINE`, what diskpart's `online volume` sends) instead of
/// only watching, and gives up after [`NOT_READY_GIVE_UP`] if that never takes.
fn wait_for_volume_ready(volume_root: &str) -> usize {
    let start = Instant::now();
    let mut last = 0usize;
    let mut next_report = Duration::from_secs(15);
    let mut next_online = Duration::ZERO;
    let mut online_ok = false;
    loop {
        match list_dir_backup(Path::new(volume_root)) {
            Ok(entries) => {
                let count = entries.len();
                if count > 0 && count == last {
                    return count; // mounted and settled
                }
                last = count;
            }
            Err(e) => {
                last = 0;
                let not_ready = e.to_string().contains(NOT_READY_HRESULT);
                // Held offline → ask for it to be mounted, first straight away
                // and then every few seconds while the state persists.
                if not_ready && start.elapsed() >= next_online {
                    next_online = start.elapsed() + Duration::from_secs(5);
                    match crate::volume_win::bring_volume_online(volume_root) {
                        Ok(()) => {
                            if !online_ok {
                                info!(
                                    volume = %volume_root,
                                    "true removal: volume was offline — brought it online explicitly"
                                );
                            }
                            online_ok = true;
                        }
                        Err(err) => debug!(
                            volume = %volume_root, error = %err,
                            "true removal: could not bring the volume online"
                        ),
                    }
                }
                // Periodic breadcrumb: the exact error code distinguishes a
                // slow mount from a failed one (RAW) from a device problem.
                if start.elapsed() >= next_report {
                    info!(
                        elapsed_s = start.elapsed().as_secs(),
                        error = %e,
                        automount_disabled = ?crate::automount::is_disabled(),
                        "true removal: filesystem not ready yet"
                    );
                    next_report += Duration::from_secs(15);
                }
                // "Not ready" that survives explicit onlining is terminal:
                // report it in seconds instead of freezing the finalize phase
                // for three minutes to reach the same answer.
                if not_ready && start.elapsed() >= NOT_READY_GIVE_UP {
                    warn!(
                        volume = %volume_root,
                        elapsed_s = start.elapsed().as_secs(),
                        online_ioctl_ok = online_ok,
                        automount_disabled = ?crate::automount::is_disabled(),
                        error = %e,
                        "true removal: the volume manager keeps the image volume offline — \
                         giving up (no filesystem was ever mounted, so no I/O reached the image)"
                    );
                    return 0;
                }
            }
        }
        if start.elapsed() >= VOLUME_READY_TIMEOUT {
            if let Err(e) = list_dir_backup(Path::new(volume_root)) {
                warn!(error = %e, "true removal: filesystem never became ready");
            }
            return last;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn open_write_retry(path: &str) -> Result<WinRawDevice> {
    let mut last: Option<OwnBackupError> = None;
    for attempt in 0..20 {
        match WinRawDevice::open_write(path) {
            Ok(dev) => return Ok(dev),
            Err(e) => {
                last = Some(e);
                std::thread::sleep(Duration::from_millis(100 + 50 * attempt));
            }
        }
    }
    Err(last.unwrap_or_else(|| OwnBackupError::Device(format!("cannot open {path}"))))
}

fn disk_number_of(physical_path: &str) -> Result<u32> {
    physical_path
        .trim_start_matches(r"\\.\PhysicalDrive")
        .parse()
        .map_err(|_| OwnBackupError::Device(format!("unexpected physical path: {physical_path}")))
}

/// Enumerate volumes until one lives on `disk_number`. The volume can take a
/// moment to surface after the disk goes online, so poll for up to ~10 s.
/// Returns the `\\?\Volume{GUID}\` root (with trailing backslash).
fn find_volume_on_disk(disk_number: u32) -> Result<String> {
    for _ in 0..40 {
        if let Some(volume_root) = enumerate_volume_on_disk(disk_number) {
            return Ok(volume_root);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err(OwnBackupError::Device(format!(
        "no volume surfaced on attached disk {disk_number} within 10 s"
    )))
}

/// Wait (bounded) until no volume enumerates on `disk_number` — i.e. the
/// mounted volume's device stack has been fully torn down after an offline, so
/// the subsequent `DetachVirtualDisk` has nothing to remove and returns at
/// once. Returns `true` if the volume disappeared within `timeout`.
fn wait_volume_gone(disk_number: u32, timeout: Duration) -> bool {
    let start = Instant::now();
    loop {
        if enumerate_volume_on_disk(disk_number).is_none() {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// One sweep over all volumes; returns the `\\?\Volume{GUID}\` root of the
/// first that lives on `disk_number`, or `None` if none does.
fn enumerate_volume_on_disk(disk_number: u32) -> Option<String> {
    all_volume_roots_on_disk(disk_number).into_iter().next()
}

/// Delete one filesystem entry (recursing into non-reparse directories first).
/// Failures are counted and warned, never fatal — excluding less than asked is
/// safe, breaking the pass over one stubborn file is not.
fn delete_tree(path: &str, is_dir: bool, is_reparse: bool, deleted: &mut u64, failed: &mut u64) {
    if is_dir && !is_reparse {
        // Children first: NTFS/ReFS only delete empty directories.
        match list_dir_backup(Path::new(path)) {
            Ok(entries) => {
                for e in entries {
                    let child = format!("{path}\\{}", e.name);
                    let child_is_dir = e.attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
                    let child_is_reparse = e.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0;
                    delete_tree(&child, child_is_dir, child_is_reparse, deleted, failed);
                }
            }
            Err(e) => {
                warn!(dir = %path, error = %e, "true removal: cannot list directory");
                *failed += 1;
                return;
            }
        }
    }
    match delete_one(path) {
        Ok(()) => *deleted += 1,
        Err(e) => {
            warn!(path = %path, error = %e, "true removal: cannot delete — entry stays in the image");
            *failed += 1;
        }
    }
}

/// Delete a single entry via its handle: POSIX semantics + ignore-readonly,
/// with a classic-disposition fallback for filesystems/OS builds without
/// `FileDispositionInfoEx`.
fn delete_one(path: &str) -> Result<()> {
    let wide = to_wide(path);
    // BACKUP_SEMANTICS: directories + ACL bypass (SeBackup/SeRestore enabled
    // by the engine); OPEN_REPARSE_POINT: delete the link, never its target.
    // SAFETY: `wide` outlives the call.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            DELETE_ACCESS,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .map_err(|e| dev_err("open for delete", e))?;
    let handle = FileHandle(handle);

    let ex = FileDispositionInfoExRaw {
        flags: FILE_DISPOSITION_FLAG_DELETE
            | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
            | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    };
    // SAFETY: pointer/size describe a live FileDispositionInfoExRaw.
    let set_ex = unsafe {
        SetFileInformationByHandle(
            handle.0,
            FileDispositionInfoEx,
            &ex as *const _ as *const _,
            std::mem::size_of::<FileDispositionInfoExRaw>() as u32,
        )
    };
    if set_ex.is_ok() {
        return Ok(());
    }

    // Fallback: clear read-only, then the classic disposition.
    // SAFETY: `wide` outlives the call.
    let _ = unsafe { SetFileAttributesW(PCWSTR(wide.as_ptr()), FILE_ATTRIBUTE_NORMAL) };
    let classic = FileDispositionInfoRaw { delete_file: 1 };
    // SAFETY: pointer/size describe a live FileDispositionInfoRaw.
    unsafe {
        SetFileInformationByHandle(
            handle.0,
            FileDispositionInfo,
            &classic as *const _ as *const _,
            std::mem::size_of::<FileDispositionInfoRaw>() as u32,
        )
    }
    .map_err(|e| dev_err("set delete disposition", e))
}


/// Cleanly unmount the volume so the subsequent detach is instant:
/// flush → LOCK (retry) → DISMOUNT → offline the disk while still locked.
///
/// A *successful* lock is the whole game: it invalidates every foreign handle
/// (storsvc/Defender/indexer) that would otherwise veto the detach's
/// QUERY_REMOVE for the full ~180 s timeout. The disk is offlined while the
/// lock is still held so mountmgr cannot re-mount the filesystem the moment
/// the lock is released with the handle. Best-effort throughout: on failure
/// the detach may stall, but the backup data is already correct.
fn teardown_volume(volume_root: &str, dev: &WinRawDevice) {
    // Flush + lock + dismount, keeping the lock handle open across the disk
    // offline so the filesystem cannot re-mount before the handle drops.
    let handle = flush_lock_dismount_volume(volume_root);
    let t = Instant::now();
    if let Err(e) = dev.set_offline_writable(false) {
        warn!(error = %e, "true removal: could not re-offline the temp disk before detach");
    }
    if let Err(e) = dev.update_properties() {
        warn!(error = %e, "true removal: could not refresh the partition table after offline");
    }
    info!(offline_ms = t.elapsed().as_millis(), "true removal: temp disk offlined before detach");
    // `handle` drops here: the lock is released with the disk already offline.
    drop(handle);
}



/// Re-read every backup block overlapped by the child's touched regions from
/// the merged image and update/insert its checksum. New all-zero blocks are
/// skipped (nothing to restore for them).
///
/// Reads the VHDX FILE through [`crate::vhdx_bat::VhdxGuestReader`] — geometry
/// and BAT parsed once, then payload reads only.
///
/// **This pass is slow on a spinning disk and four attempts did not change
/// that** (2026-08-08, same machine/image every time: 3467 blocks ≈ 3.4 GiB):
///
/// | shape | time |
/// |---|---|
/// | one 1 MiB read per block, through the attached disk | 255 s |
/// | reads coalesced into 16 MiB runs, still attached | 245 s |
/// | runs read from the FILE (`read_vhdx_guest_at` per run) | 268 s |
/// | runs read through this reader (BAT parsed once) | 260 s |
///
/// So it is neither the number of reads, nor the attach, nor the per-block BAT
/// re-parse — every plausible software cause was tried and the number stayed
/// put at ~75 ms per megabyte. What is left is the drive: the destination in
/// that test was an ST2000DM008, an SMR disk, right after taking 84 GiB of
/// writes — exactly when such a drive is busy rewriting its shingled zones and
/// serves everything else at a crawl. The merge in the same run moved 7 GiB at
/// 72 MB/s, so the disk is not slow in general, only after that write load.
///
/// Do not "optimise" this again without measuring on a CMR disk or an SSD
/// first: three of the four rows above are the cost of assuming the code was
/// at fault. The current shape is kept because it is the simplest of them
/// (no attach, no per-block re-parse), not because it won.
pub(crate) fn rehash_touched_blocks(
    image_path: &Path,
    alloc: &crate::vhdx_bat::VhdxAllocation,
    data_offset: u64,
    volume_len: u64,
    block_size: u32,
    checksums: &mut Vec<BlockChecksum>,
    tick: &mut dyn FnMut(u64),
) -> Result<(u64, u64)> {
    let bs = block_size as u64;
    let mut affected: BTreeSet<u64> = BTreeSet::new();
    for &b in &alloc.blocks {
        let (start, end) = alloc.block_range(b);
        let start = start.max(data_offset);
        let end = end.min(data_offset + volume_len);
        if start >= end {
            continue; // outside the volume window (GPT head/tail)
        }
        for n in (start - data_offset) / bs..=(end - 1 - data_offset) / bs {
            affected.insert(n);
        }
    }
    if affected.is_empty() {
        return Ok((0, 0));
    }

    let mut map: BTreeMap<u64, u64> =
        checksums.iter().map(|c| (c.block_number, c.checksum)).collect();
    let mut added = 0u64;
    let t_read = Instant::now();
    let mut reader = crate::vhdx_bat::VhdxGuestReader::open(image_path)?;
    let mut buf = vec![0u8; (REHASH_RUN_BLOCKS * bs) as usize];
    let mut done = 0u64;
    let mut runs = 0u64;
    for (first, count) in
        crate::checksum::coalesce_block_runs(affected.iter().copied(), REHASH_RUN_BLOCKS)
    {
        runs += 1;
        let offset = first * bs;
        let want = std::cmp::min(count * bs, volume_len - offset) as usize;
        reader.read_at(data_offset + offset, &mut buf[..want])?;
        for i in 0..count {
            let n = first + i;
            let at = (i * bs) as usize;
            if at >= want {
                break; // the last block ran past the end of the volume
            }
            let len = std::cmp::min(bs as usize, want - at);
            let block = &buf[at..at + len];
            let checksum = compute_block_checksum(block);
            match map.entry(n) {
                std::collections::btree_map::Entry::Occupied(mut e) => {
                    e.insert(checksum);
                }
                std::collections::btree_map::Entry::Vacant(e) => {
                    // Driver wrote into previously-free space (USN growth etc.).
                    // All-zero content needs no restore entry.
                    if block.iter().any(|&x| x != 0) {
                        e.insert(checksum);
                        added += 1;
                    }
                }
            }
        }
        done += count;
        tick(done);
    }
    info!(
        blocks = affected.len(),
        runs,
        read_ms = t_read.elapsed().as_millis(),
        "re-checksum read the image file directly (no attach)"
    );

    *checksums = map
        .into_iter()
        .map(|(block_number, checksum)| BlockChecksum { block_number, checksum })
        .collect();
    Ok((affected.len() as u64, added))
}
