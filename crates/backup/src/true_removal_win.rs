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
use windows::Win32::Foundation::{CloseHandle, ERROR_NO_MORE_FILES, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FileDispositionInfo, FileDispositionInfoEx, FindFirstVolumeW, FindNextVolumeW,
    FindVolumeClose, FlushFileBuffers, SetFileAttributesW, SetFileInformationByHandle,
    FILE_ATTRIBUTE_NORMAL, FILE_FLAGS_AND_ATTRIBUTES, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, OPEN_EXISTING,
};
use windows::Win32::System::Ioctl::{FSCTL_DISMOUNT_VOLUME, FSCTL_LOCK_VOLUME};
use windows::Win32::System::IO::DeviceIoControl;

use own_backup_common::{OwnBackupError, Result};

use crate::checksum::compute_block_checksum;
use crate::exclusions_win::{list_dir_backup, ExcludedPath};
use crate::sidecar::BlockChecksum;
use crate::vhdx::{attach_vhdx, create_differencing_vhdx, merge_into_parent, AttachMode};
use crate::vhdx_bat::allocated_payload_blocks;
use crate::win_device::{to_wide, WinRawDevice};

/// Stamped into the start log line so a stale service exe is immediately
/// visible in the log (see "Windows GUI testing gotchas").
const BUILD_TAG: &str = "2026-07-09b-mount-wait";

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

/// RAII file handle.
struct FileHandle(HANDLE);
impl Drop for FileHandle {
    fn drop(&mut self) {
        // SAFETY: handle owned by us, closed exactly once.
        let _ = unsafe { CloseHandle(self.0) };
    }
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
                "true removal: mount-&-delete failed — excluded files stay as zero-filled ghosts"
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
            "true removal: no entries could be deleted — image left with zero-filled ghosts"
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
        rehash_touched_blocks(image_path, &alloc, data_offset, volume_len, block_size, checksums)?;
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
    // The volume DEVICE enumerating does not mean the filesystem is MOUNTED: a
    // natively-attached VHDX is ready instantly, but the userspace-backed
    // AIM/devio disk (qcow2) mounts asynchronously. Deleting before the FS is up
    // makes almost every path lookup fail with "not found" (root entries resolve
    // but deeper directories do not). Wait for the root to become readable and
    // stable first; the logged count also tells us from the log alone whether the
    // FS was actually mounted.
    let root_entries = wait_for_volume_ready(&volume_root);
    info!(
        volume = %volume_root,
        disk = disk_number,
        root_entries,
        "true removal: image volume mounted"
    );

    let t_del = Instant::now();
    let mut deleted = 0u64;
    let mut failed = 0u64;
    for p in paths {
        let full = format!("{volume_root}{}", p.rel);
        delete_tree(&full, p.is_dir, p.is_reparse, &mut deleted, &mut failed);
        if (deleted + failed) % 64 == 0 {
            tick(deleted);
        }
    }
    tick(deleted);
    info!(
        deleted,
        failed,
        delete_ms = t_del.elapsed().as_millis(),
        "true removal: deletions done; flushing and dismounting the volume"
    );

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

/// Wait until the volume's filesystem is mounted and its root directory is
/// readable, so deletions do not race an incomplete mount. Polls the root entry
/// count until it is non-zero and stable across two reads, or a timeout. Returns
/// the final entry count (0 = the FS never came up — a clear diagnostic).
fn wait_for_volume_ready(volume_root: &str) -> usize {
    let start = Instant::now();
    let mut last = 0usize;
    loop {
        let count = list_dir_backup(Path::new(volume_root)).map(|v| v.len()).unwrap_or(0);
        if count > 0 && count == last {
            return count; // mounted and settled
        }
        last = count;
        if start.elapsed() >= Duration::from_secs(30) {
            return count;
        }
        std::thread::sleep(Duration::from_millis(250));
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
    let mut name_buf = [0u16; 512];
    // SAFETY: buffer valid for the call; handle closed on every path below.
    let find = unsafe { FindFirstVolumeW(&mut name_buf) }.ok()?;
    let result = loop {
        let len = name_buf.iter().position(|&c| c == 0).unwrap_or(name_buf.len());
        let volume_root = String::from_utf16_lossy(&name_buf[..len]);
        if volume_on_disk(&volume_root, disk_number) {
            break Some(volume_root);
        }
        name_buf = [0u16; 512];
        // SAFETY: valid find handle and buffer.
        match unsafe { FindNextVolumeW(find, &mut name_buf) } {
            Ok(()) => {}
            Err(e) => {
                if e.code() != ERROR_NO_MORE_FILES.to_hresult() {
                    debug!(error = %e, "FindNextVolumeW");
                }
                break None;
            }
        }
    };
    // SAFETY: handle from a successful FindFirstVolumeW.
    let _ = unsafe { FindVolumeClose(find) };
    result
}

/// Does `\\?\Volume{GUID}\` live (entirely) on `disk_number`?
fn volume_on_disk(volume_root: &str, disk_number: u32) -> bool {
    let device = volume_root.trim_end_matches('\\');
    let wide = to_wide(device);
    // Access 0: IOCTL_VOLUME_QUERY_DISK_EXTENTS needs no data access.
    // SAFETY: `wide` outlives the call.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    };
    let Ok(handle) = handle else { return false };
    let handle = FileHandle(handle);

    // VOLUME_DISK_EXTENTS: u32 count, pad to 8, then 24-byte DISK_EXTENT[]
    // (u32 DiskNumber, pad, i64 StartingOffset, i64 ExtentLength).
    let mut out = [0u8; 8 + 24 * 8];
    let mut returned = 0u32;
    // SAFETY: out buffer valid for its size.
    let ok = unsafe {
        DeviceIoControl(
            handle.0,
            IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
            None,
            0,
            Some(out.as_mut_ptr() as *mut _),
            out.len() as u32,
            Some(&mut returned),
            None,
        )
    };
    if ok.is_err() || returned < 8 + 24 {
        return false;
    }
    let count = u32::from_le_bytes(out[0..4].try_into().unwrap()) as usize;
    (0..count.min(8)).all(|i| {
        let base = 8 + i * 24;
        u32::from_le_bytes(out[base..base + 4].try_into().unwrap()) == disk_number
    }) && count > 0
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

/// How long to keep retrying `FSCTL_LOCK_VOLUME` while system services finish
/// their post-mount writes and release the volume.
const LOCK_RETRY_TIMEOUT: Duration = Duration::from_secs(60);

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
    let device = volume_root.trim_end_matches('\\');
    let wide = to_wide(device);
    // SAFETY: `wide` outlives the call.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            GENERIC_READ.0 | GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    };
    let Ok(handle) = handle else {
        warn!(volume = %device, "true removal: cannot open volume for teardown");
        return;
    };
    let handle = FileHandle(handle);

    let t = Instant::now();
    // SAFETY: valid volume handle.
    if let Err(e) = unsafe { FlushFileBuffers(handle.0) } {
        warn!(error = %e, "true removal: volume flush failed");
    }
    let flush_ms = t.elapsed().as_millis();

    // Lock with retries: right after 20k+ deletions the lazy writer, storsvc
    // (System Volume Information) and friends still hold handles for a few
    // seconds; the lock succeeds once they let go.
    let mut returned = 0u32;
    let t = Instant::now();
    let mut attempts = 0u32;
    let locked = loop {
        attempts += 1;
        // SAFETY: no in/out buffers.
        match unsafe {
            DeviceIoControl(handle.0, FSCTL_LOCK_VOLUME, None, 0, None, 0, Some(&mut returned), None)
        } {
            Ok(()) => break true,
            Err(e) => {
                if t.elapsed() >= LOCK_RETRY_TIMEOUT {
                    warn!(
                        error = %e,
                        attempts,
                        "true removal: could not lock the volume — foreign handles survive, detach may stall"
                    );
                    break false;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    };
    let lock_ms = t.elapsed().as_millis();

    let t = Instant::now();
    // Forced when unlocked — still better than nothing.
    // SAFETY: no in/out buffers.
    let dismounted = unsafe {
        DeviceIoControl(handle.0, FSCTL_DISMOUNT_VOLUME, None, 0, None, 0, Some(&mut returned), None)
    }
    .map_err(|e| warn!(error = %e, "true removal: volume dismount failed"))
    .is_ok();
    let dismount_ms = t.elapsed().as_millis();

    // Offline while the lock is still held: when the handle closes and the
    // lock drops, an online disk would get its filesystem re-mounted at once.
    let t = Instant::now();
    if let Err(e) = dev.set_offline_writable(false) {
        warn!(error = %e, "true removal: could not re-offline the temp disk before detach");
    }
    if let Err(e) = dev.update_properties() {
        warn!(error = %e, "true removal: could not refresh the partition table after offline");
    }
    let offline_ms = t.elapsed().as_millis();

    info!(
        locked,
        attempts,
        dismounted,
        flush_ms,
        lock_ms,
        dismount_ms,
        offline_ms,
        "true removal: volume teardown before detach"
    );
    // `handle` drops here: the lock is released with the disk already offline.
}

/// Re-read every backup block overlapped by the child's touched regions from
/// the merged image and update/insert its checksum. New all-zero blocks are
/// skipped (nothing to restore for them).
fn rehash_touched_blocks(
    image_path: &Path,
    alloc: &crate::vhdx_bat::VhdxAllocation,
    data_offset: u64,
    volume_len: u64,
    block_size: u32,
    checksums: &mut Vec<BlockChecksum>,
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

    let attached = attach_vhdx(image_path, AttachMode::RawRead)?;
    let mut dev = open_read_retry(attached.physical_path())?;

    let mut map: BTreeMap<u64, u64> =
        checksums.iter().map(|c| (c.block_number, c.checksum)).collect();
    let mut added = 0u64;
    let mut buf = vec![0u8; block_size as usize];
    for &n in &affected {
        let offset = n * bs;
        let len = std::cmp::min(bs, volume_len - offset) as usize;
        dev.read_at(data_offset + offset, &mut buf[..len])?;
        let checksum = compute_block_checksum(&buf[..len]);
        match map.entry(n) {
            std::collections::btree_map::Entry::Occupied(mut e) => {
                e.insert(checksum);
            }
            std::collections::btree_map::Entry::Vacant(e) => {
                // Driver wrote into previously-free space (USN growth etc.).
                // All-zero content needs no restore entry.
                if buf[..len].iter().any(|&x| x != 0) {
                    e.insert(checksum);
                    added += 1;
                }
            }
        }
    }
    drop(dev);
    drop(attached);

    *checksums = map
        .into_iter()
        .map(|(block_number, checksum)| BlockChecksum { block_number, checksum })
        .collect();
    Ok((affected.len() as u64, added))
}

fn open_read_retry(path: &str) -> Result<WinRawDevice> {
    let mut last: Option<OwnBackupError> = None;
    for attempt in 0..20 {
        match WinRawDevice::open_read(path) {
            Ok(dev) => return Ok(dev),
            Err(e) => {
                last = Some(e);
                std::thread::sleep(Duration::from_millis(100 + 50 * attempt));
            }
        }
    }
    Err(last.unwrap_or_else(|| OwnBackupError::Device(format!("cannot open {path}"))))
}
