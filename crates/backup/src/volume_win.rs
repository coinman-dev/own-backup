//! Windows volume primitives shared by backup and restore (R3f).
//!
//! These grew inside [`crate::true_removal_win`], where they were private. The
//! restore path needs exactly the same things — find the volumes sitting on a
//! disk, take their drive letters away, flush and dismount them before writing
//! raw sectors underneath — so they live here now.
//!
//! ## The lock is bound to a handle
//!
//! `FSCTL_LOCK_VOLUME` grants exclusive access **to the handle it was issued
//! on**, not to the process. Lock with one handle, write with another, and the
//! second one is refused. That is why [`flush_lock_dismount_volume`] *returns*
//! the handle instead of closing it: the caller has to keep it alive for the
//! whole write. Closing it early releases the lock, the filesystem re-mounts,
//! and the next raw write fails with `ERROR_ACCESS_DENIED` — or worse,
//! succeeds while the filesystem driver has its own idea of what those sectors
//! contain.
//!
//! [`VolumeDismountGuard`] wraps that lifetime in a value: hold it while
//! writing, drop it when done.

#![cfg(target_os = "windows")]

use std::time::{Duration, Instant};

use tracing::{debug, info, warn};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, DeleteVolumeMountPointW, FindFirstVolumeW, FindNextVolumeW, FindVolumeClose,
    FlushFileBuffers, GetVolumePathNamesForVolumeNameW, FILE_FLAGS_AND_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
    OPEN_EXISTING,
};
use windows::Win32::System::Ioctl::{FSCTL_DISMOUNT_VOLUME, FSCTL_LOCK_VOLUME};
use windows::Win32::System::IO::DeviceIoControl;

use crate::win_device::to_wide;

/// How long to keep retrying `FSCTL_LOCK_VOLUME`. After a large batch of
/// deletions the lazy writer and storsvc still hold handles for a few seconds.
const LOCK_RETRY_TIMEOUT: Duration = Duration::from_secs(60);

/// The same, for a restore.
///
/// A minute of retries is right when the volume is one we have been writing to
/// and expect to get back. It is wrong here: the volumes in a restore's way are
/// usually about to be overwritten anyway, and one of them is routinely a
/// *ghost* — a volume object left over a partition that no longer exists, which
/// answers "устройство не готово" for as long as anyone keeps asking. Sixty
/// seconds of that before the real failure buries the cause.
const RESTORE_LOCK_TIMEOUT: Duration = Duration::from_secs(6);

/// RAII wrapper closing a Win32 handle exactly once.
pub struct FileHandle(pub HANDLE);

impl Drop for FileHandle {
    fn drop(&mut self) {
        // SAFETY: handle owned by us, closed exactly once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Where one volume physically lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VolumeExtent {
    pub disk_number: u32,
    pub offset: u64,
    pub length: u64,
}

/// Every `\\?\Volume{GUID}\` root the system knows about.
pub fn all_volume_roots() -> Vec<String> {
    let mut out = Vec::new();
    let mut name_buf = [0u16; 512];
    // SAFETY: buffer valid for the call; handle closed below.
    let Ok(find) = (unsafe { FindFirstVolumeW(&mut name_buf) }) else {
        return out;
    };
    loop {
        let len = name_buf
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(name_buf.len());
        out.push(String::from_utf16_lossy(&name_buf[..len]));
        name_buf = [0u16; 512];
        // SAFETY: valid find handle and buffer.
        if unsafe { FindNextVolumeW(find, &mut name_buf) }.is_err() {
            break;
        }
    }
    // SAFETY: handle from a successful FindFirstVolumeW.
    let _ = unsafe { FindVolumeClose(find) };
    out
}

/// Physical extents of `volume_root` (`IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS`).
/// Empty when the volume cannot be opened — which is normal for volumes that
/// belong to a disk currently offline.
pub fn volume_extents(volume_root: &str) -> Vec<VolumeExtent> {
    let device = volume_root.trim_end_matches('\\');
    let wide = to_wide(device);
    // Opened with no access rights: enough for the IOCTL, and it does not
    // disturb whatever else holds the volume.
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
    let Ok(handle) = handle else {
        return Vec::new();
    };
    let handle = FileHandle(handle);

    // VOLUME_DISK_EXTENTS: u32 count, pad to 8, then 24-byte DISK_EXTENT[]
    // (u32 DiskNumber, pad, i64 StartingOffset, i64 ExtentLength).
    const MAX_EXTENTS: usize = 16;
    let mut out = [0u8; 8 + 24 * MAX_EXTENTS];
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
        return Vec::new();
    }
    let count = (u32::from_le_bytes(out[0..4].try_into().unwrap()) as usize).min(MAX_EXTENTS);
    (0..count)
        .map(|i| {
            let b = 8 + i * 24;
            VolumeExtent {
                disk_number: u32::from_le_bytes(out[b..b + 4].try_into().unwrap()),
                offset: i64::from_le_bytes(out[b + 8..b + 16].try_into().unwrap()).max(0) as u64,
                length: i64::from_le_bytes(out[b + 16..b + 24].try_into().unwrap()).max(0) as u64,
            }
        })
        .collect()
}

/// The `\\?\Volume{…}\` root of the volume a path lives on.
///
/// Answers "which volume is `C:\Windows` on" without going through a drive
/// letter, which can be absent or reassigned.
pub fn volume_root_for_path(path: &str) -> Option<String> {
    use windows::Win32::Storage::FileSystem::{
        GetVolumeNameForVolumeMountPointW, GetVolumePathNameW,
    };

    let wide = to_wide(path);
    let mut mount = [0u16; 512];
    // SAFETY: both buffers are valid for their length; `wide` is NUL-terminated.
    unsafe { GetVolumePathNameW(PCWSTR(wide.as_ptr()), &mut mount) }.ok()?;

    let mut name = [0u16; 512];
    // SAFETY: `mount` came back NUL-terminated from the call above.
    unsafe { GetVolumeNameForVolumeMountPointW(PCWSTR(mount.as_ptr()), &mut name) }.ok()?;
    let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    Some(String::from_utf16_lossy(&name[..len]))
}

/// Whether every extent of `volume_root` lives on `disk_number`.
pub fn volume_on_disk(volume_root: &str, disk_number: u32) -> bool {
    let extents = volume_extents(volume_root);
    !extents.is_empty() && extents.iter().all(|e| e.disk_number == disk_number)
}

/// `(disk_number, starting_offset_bytes)` of a single-extent volume, or `None`
/// for a spanned/striped one (never one of our image partitions) or on error.
pub fn volume_start_on_disk(volume_root: &str) -> Option<(u32, u64)> {
    let extents = volume_extents(volume_root);
    match extents.as_slice() {
        [e] => Some((e.disk_number, e.offset)),
        _ => None,
    }
}

/// Every `\\?\Volume{GUID}\` root that lives on `disk_number`.
pub fn all_volume_roots_on_disk(disk_number: u32) -> Vec<String> {
    all_volume_roots()
        .into_iter()
        .filter(|vr| volume_on_disk(vr, disk_number))
        .collect()
}

/// Volume root of the volume starting exactly at `part_offset` on
/// `disk_number`, or `None` if none does. Retries for ~10 s: a freshly attached
/// disk's volumes take a moment to enumerate.
pub fn volume_root_at_offset(disk_number: u32, part_offset: u64) -> Option<String> {
    for _ in 0..40 {
        for vr in all_volume_roots_on_disk(disk_number) {
            if volume_start_on_disk(&vr) == Some((disk_number, part_offset)) {
                return Some(vr);
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    None
}

/// Path names mapped to `volume_root`, reduced to bare drive letters (`"X:"`).
/// Mount-point folder paths are ignored — a volume may have several names.
pub fn volume_drive_letters(volume_root: &str) -> Vec<String> {
    let wide = to_wide(volume_root); // keeps the required trailing backslash
    let mut buf = vec![0u16; 512];
    let mut needed = 0u32;
    // SAFETY: `wide` outlives the call; buffer sized by `buf.len()`.
    let ok = unsafe {
        GetVolumePathNamesForVolumeNameW(
            PCWSTR(wide.as_ptr()),
            Some(buf.as_mut_slice()),
            &mut needed,
        )
    };
    if ok.is_err() {
        // Grow once if the buffer was too small, then give up quietly.
        if needed as usize > buf.len() {
            buf = vec![0u16; needed as usize];
            // SAFETY: as above, larger buffer.
            if unsafe {
                GetVolumePathNamesForVolumeNameW(
                    PCWSTR(wide.as_ptr()),
                    Some(buf.as_mut_slice()),
                    &mut needed,
                )
            }
            .is_err()
            {
                return Vec::new();
            }
        } else {
            return Vec::new();
        }
    }
    // Double-null-terminated list of null-separated path names.
    buf.split(|&c| c == 0)
        .filter(|s| !s.is_empty())
        .filter_map(|s| {
            let path = String::from_utf16_lossy(s);
            let bytes = path.as_bytes();
            // "X:\" → "X:"; ignore mount-point folder paths.
            if bytes.len() == 3
                && bytes[1] == b':'
                && bytes[2] == b'\\'
                && bytes[0].is_ascii_alphabetic()
            {
                Some(format!("{}:", (bytes[0] as char).to_ascii_uppercase()))
            } else {
                None
            }
        })
        .collect()
}

/// Remove every drive letter of `volume_root`.
///
/// `DeleteVolumeMountPointW` also deletes the REMEMBERED `MountedDevices`
/// mapping (the `mountvol X: /D` semantics). That is what makes it stick: a
/// differencing chain reuses its parent's partition GUIDs, so once a letter has
/// ever been assigned to the chain's volume, mountmgr restores it from the
/// database on every later attach, bypassing the auto-mount setting. Stripping
/// once heals the chain for good. Folder mount points are never touched.
pub fn strip_drive_letters(volume_root: &str) {
    for letter in volume_drive_letters(volume_root) {
        let root = format!("{letter}\\"); // "J:" → "J:\"
        let wide = to_wide(&root);
        // SAFETY: `wide` is NUL-terminated and outlives the call.
        match unsafe { DeleteVolumeMountPointW(PCWSTR(wide.as_ptr())) } {
            Ok(()) => info!(
                letter = %letter,
                volume = %volume_root,
                "removed the volume's drive letter (and its remembered mapping)"
            ),
            Err(e) => warn!(
                letter = %letter, volume = %volume_root, error = %e,
                "could not remove the drive letter"
            ),
        }
    }
}

/// `IOCTL_VOLUME_ONLINE` — absent from the Windows SDK metadata the `windows`
/// crate is generated from, so it is spelled out here:
/// `CTL_CODE(IOCTL_VOLUME_BASE = 0x56, 2, METHOD_BUFFERED, FILE_READ_ACCESS |
/// FILE_WRITE_ACCESS)`.
const IOCTL_VOLUME_ONLINE: u32 = 0x0056_C008;

/// Ask the volume manager to bring `volume_root` ONLINE — i.e. mount its
/// filesystem — and report what happened.
///
/// A volume that surfaces while "automatic mounting of new volumes" is off
/// (`mountvol /N`, `diskpart automount disable`) is deliberately left offline by
/// mountmgr: the volume device enumerates and answers
/// `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS`, but every open of its root fails with
/// `ERROR_NOT_READY` (0x80070015) and **no I/O ever reaches the disk**. That is
/// exactly what a backup image mounted for the true-removal pass looked like on
/// 2026-07-30: 180 s of "filesystem not ready yet", 14 reads served in total,
/// nothing deleted. The setting is machine-global and persistent, so it can also
/// be the user's own (or the leftover of a run that died before restoring it) —
/// which is why the delete pass must not merely avoid turning it off, it has to
/// mount the volume explicitly.
///
/// This is the IOCTL behind diskpart's `online volume`; it mounts the volume
/// whatever the machine's automount setting says. Best-effort: an error is
/// returned for logging, never fatal.
pub fn bring_volume_online(volume_root: &str) -> std::result::Result<(), String> {
    let device = volume_root.trim_end_matches('\\');
    let wide = to_wide(device);
    // SAFETY: `wide` outlives the call.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            GENERIC_READ.0 | GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
    .map_err(|e| format!("open volume: {e}"))?;
    let handle = FileHandle(handle);

    let mut returned = 0u32;
    // SAFETY: no in/out buffers; valid volume handle.
    unsafe {
        DeviceIoControl(
            handle.0,
            IOCTL_VOLUME_ONLINE,
            None,
            0,
            None,
            0,
            Some(&mut returned),
            None,
        )
    }
    .map_err(|e| format!("IOCTL_VOLUME_ONLINE: {e}"))
}

/// Strip drive letters from every volume on `disk_number`.
pub fn strip_letters_on_disk(disk_number: u32) {
    for vr in all_volume_roots_on_disk(disk_number) {
        strip_drive_letters(&vr);
    }
}

/// Drive letters of every volume on `disk_number`. Empty if Windows has not
/// assigned any yet — a freshly attached disk can need a moment.
pub fn drive_letters_on_disk(disk_number: u32) -> Vec<String> {
    all_volume_roots_on_disk(disk_number)
        .iter()
        .flat_map(|vr| volume_drive_letters(vr))
        .collect()
}

/// Partition device objects that still exist on `disk_number`, as openable
/// `\\?\GLOBALROOT\Device\HarddiskN\PartitionM` paths.
///
/// [`all_volume_roots_on_disk`] is BLIND to a disk that is offline — it filters
/// by [`volume_extents`], and that IOCTL fails for the volumes of an offline
/// disk (see its doc comment). So the moment we take an image offline we lose
/// the ability to notice that a volume surfaced from it, which is exactly the
/// state a stalled detach is diagnosed in. This probe answers the question that
/// still matters there: does a partition device stack exist on this disk at
/// all? A live object here after the disk went offline means the teardown was
/// vetoed and `DetachVirtualDisk` is about to wait ~180 s for it.
///
/// The partition objects are what a dismount has to reach anyway, so these
/// paths are also usable with [`flush_lock_dismount_volume`].
pub fn partition_devices_on_disk(disk_number: u32) -> Vec<String> {
    // Partition numbering starts at 1; our images carry a handful of them, and
    // an unused index simply fails to open.
    (1..=16u32)
        .map(|p| format!(r"\\?\GLOBALROOT\Device\Harddisk{disk_number}\Partition{p}"))
        .filter(|path| {
            let wide = to_wide(path);
            // Opened with no access rights: enough to prove the device object
            // exists, and it disturbs nothing that holds the partition.
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
            match handle {
                Ok(h) => {
                    drop(FileHandle(h));
                    true
                }
                Err(_) => false,
            }
        })
        .collect()
}

/// Dismount one volume WITHOUT flushing or locking it first — a forced
/// dismount, which DISCARDS whatever the filesystem driver still had cached
/// instead of committing it.
///
/// For a volume that surfaced from a BACKUP IMAGE this is the only correct
/// teardown. [`flush_lock_dismount_volume`] is built for real volumes, where
/// losing cached writes would be data loss; pointed at an image it does the
/// opposite of what is wanted — on 2026-08-21 it pushed the FAT32 driver's
/// dirty boot sector (LBA 34, 35 and 66) into a freshly written incremental,
/// so the image no longer matched the checksums the backup had just computed
/// for it. An image must come out of a backup byte-identical to the source;
/// nothing Windows happened to think while it briefly had the volume mounted
/// belongs in it.
///
/// Returns whether the dismount succeeded.
pub fn dismount_volume_forced(volume_root: &str) -> bool {
    let device = volume_root.trim_end_matches('\\');
    let wide = to_wide(device);
    let open = |access: u32| {
        // SAFETY: `wide` outlives the call.
        unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        }
    };
    let Ok(handle) = open(GENERIC_READ.0 | GENERIC_WRITE.0).or_else(|_| open(GENERIC_READ.0))
    else {
        warn!(volume = %device, "cannot open the image's volume to dismount it");
        return false;
    };
    let handle = FileHandle(handle);
    let mut returned = 0u32;
    let t = Instant::now();
    // SAFETY: valid volume handle; no in/out buffers.
    let ok = unsafe {
        DeviceIoControl(
            handle.0,
            FSCTL_DISMOUNT_VOLUME,
            None,
            0,
            None,
            0,
            Some(&mut returned),
            None,
        )
    }
    .map_err(|e| warn!(error = %e, volume = %device, "image volume dismount failed"))
    .is_ok();
    info!(volume = %device, dismounted = ok, ms = t.elapsed().as_millis(), "image volume dismounted (nothing flushed)");
    ok
}

/// Flush, lock (with retries) and dismount one volume, returning the still-open
/// lock handle so the caller decides when the lock releases.
///
/// For a volume that came out of a backup image use
/// [`dismount_volume_forced`] instead — flushing writes the filesystem's
/// cached state INTO the image.
///
/// Do not discard the return value. See the module docs: the lock belongs to
/// this handle, and dropping it lets the filesystem mount straight back.
#[must_use = "the lock lives only as long as the returned handle"]
pub fn flush_lock_dismount_volume(volume_root: &str) -> Option<FileHandle> {
    flush_lock_dismount_volume_within(volume_root, LOCK_RETRY_TIMEOUT)
}

/// [`flush_lock_dismount_volume`] with a caller-chosen patience for the lock.
#[must_use = "the lock lives only as long as the returned handle"]
pub fn flush_lock_dismount_volume_within(
    volume_root: &str,
    lock_timeout: Duration,
) -> Option<FileHandle> {
    let device = volume_root.trim_end_matches('\\');
    let wide = to_wide(device);
    let open = |access: u32| {
        // SAFETY: `wide` outlives the call.
        unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        }
    };
    // Read/write first (a writable handle can also flush), but fall back to
    // read: both FSCTLs are FILE_ANY_ACCESS, and the volumes of a read-only
    // attached image refuse a writable open — the case where the teardown is
    // needed most is the one where the RW open cannot succeed.
    let handle = open(GENERIC_READ.0 | GENERIC_WRITE.0).or_else(|_| open(GENERIC_READ.0));
    let Ok(handle) = handle else {
        warn!(volume = %device, "cannot open volume for teardown");
        return None;
    };
    let handle = FileHandle(handle);

    let t = Instant::now();
    // SAFETY: valid volume handle.
    if let Err(e) = unsafe { FlushFileBuffers(handle.0) } {
        warn!(error = %e, "volume flush failed");
    }
    let flush_ms = t.elapsed().as_millis();

    let mut returned = 0u32;
    let t = Instant::now();
    let mut attempts = 0u32;
    let locked = loop {
        attempts += 1;
        // SAFETY: no in/out buffers.
        match unsafe {
            DeviceIoControl(
                handle.0,
                FSCTL_LOCK_VOLUME,
                None,
                0,
                None,
                0,
                Some(&mut returned),
                None,
            )
        } {
            Ok(()) => break true,
            Err(e) => {
                if t.elapsed() >= lock_timeout {
                    warn!(
                        error = %e, attempts,
                        "could not lock the volume — foreign handles survive"
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
        DeviceIoControl(
            handle.0,
            FSCTL_DISMOUNT_VOLUME,
            None,
            0,
            None,
            0,
            Some(&mut returned),
            None,
        )
    }
    .map_err(|e| warn!(error = %e, "volume dismount failed"))
    .is_ok();
    let dismount_ms = t.elapsed().as_millis();

    info!(
        volume = %device, locked, attempts, dismounted,
        flush_ms, lock_ms, dismount_ms,
        "volume flushed/locked/dismounted"
    );
    Some(handle)
}

/// Keeps the volumes overlapping a byte range on a disk dismounted for as long
/// as this value is alive (D17, the targeted alternative to taking the whole
/// disk offline).
///
/// Data is written through `part:<disk>:<offset>:<len>`, which addresses the
/// *physical drive* — a different device object, untouched by a volume lock.
/// What the guard buys is the filesystem driver letting go: once dismounted it
/// stops caching, stops writing back, and stops disagreeing with the bytes we
/// put there.
///
/// The guard does not fail when a volume refuses to lock. It reports what it
/// managed, because the useful part — the dismount — usually succeeds anyway,
/// and a hard failure here would block a restore for a volume that no longer
/// matters (it is about to be overwritten).
pub struct VolumeDismountGuard {
    /// Lock handles, alive until this guard drops. Never read; that is the point.
    _handles: Vec<FileHandle>,
    /// Volume roots taken down, for logging and for the caller's report.
    pub volumes: Vec<String>,
    /// Volumes that were found but could not be opened for teardown.
    pub failed: Vec<String>,
}

impl VolumeDismountGuard {
    /// Take down every volume that overlaps `[offset, offset + length)` on
    /// `disk_number`.
    pub fn for_range(disk_number: u32, offset: u64, length: u64) -> Self {
        Self::for_range_within(disk_number, offset, length, LOCK_RETRY_TIMEOUT)
    }

    /// [`Self::for_range`] with a chosen patience for each volume's lock.
    pub fn for_range_within(
        disk_number: u32,
        offset: u64,
        length: u64,
        lock_timeout: Duration,
    ) -> Self {
        let mut handles = Vec::new();
        let mut volumes = Vec::new();
        let mut failed = Vec::new();

        for vr in volumes_overlapping(disk_number, offset, length) {
            strip_drive_letters(&vr);
            match flush_lock_dismount_volume_within(&vr, lock_timeout) {
                Some(h) => {
                    handles.push(h);
                    volumes.push(vr);
                }
                None => failed.push(vr),
            }
        }
        // At info, not debug: when a raw write is refused, what the guard did
        // and did not manage to take down is the first thing worth reading, and
        // by then the machine may be in no state to reproduce it.
        info!(
            disk = disk_number, offset, length,
            taken = ?volumes, failed = ?failed,
            "dismount guard established"
        );
        Self {
            _handles: handles,
            volumes,
            failed,
        }
    }

    /// Take down every volume on the whole disk.
    pub fn for_disk(disk_number: u32) -> Self {
        Self::for_range(disk_number, 0, u64::MAX)
    }

    /// The patience a restore uses — see [`RESTORE_LOCK_TIMEOUT`].
    pub fn for_restore_range(disk_number: u32, offset: u64, length: u64) -> Self {
        Self::for_range_within(disk_number, offset, length, RESTORE_LOCK_TIMEOUT)
    }
}

/// Every volume whose extents intersect `[offset, offset + length)` on `disk`.
///
/// Includes volumes whose partition is already gone: mountmgr can keep a volume
/// object — with its old extents — after the partition under it has been
/// deleted, and Windows still refuses raw writes to the sectors it claims.
pub fn volumes_overlapping(disk: u32, offset: u64, length: u64) -> Vec<String> {
    let end = offset.saturating_add(length);
    all_volume_roots()
        .into_iter()
        .filter(|vr| {
            volume_extents(vr).iter().any(|e| {
                e.disk_number == disk
                    && e.offset < end
                    && e.offset.saturating_add(e.length) > offset
            })
        })
        .collect()
}

/// Wait until Windows has finished rebuilding its idea of `disk` after the
/// partition table changed.
///
/// Rewriting the table tears volumes down and builds others up, and none of
/// that is finished when the IOCTL returns: the first raw write after it came
/// back "указано несуществующее устройство" 23 ms later. Settled means the set
/// of volumes covering the range stopped changing between two looks, so the
/// dismount guard that follows sees what is really there — including a volume
/// Windows created for the partition we are about to write into.
///
/// Best effort: on timeout the caller carries on, because a range with no
/// volume on it at all is the normal case for a partition that has just been
/// created and never formatted, and waiting longer would not change that.
pub fn wait_for_volumes_to_settle(disk: u32, offset: u64, length: u64, timeout: Duration) {
    // Long enough for a volume that is about to arrive to have arrived. Without
    // it, the seconds between "the table changed" and "Windows created the
    // volume" look perfectly stable — the range is empty and stays empty — and
    // the wait ends just in time to be wrong.
    const MINIMUM: Duration = Duration::from_millis(1500);

    let started = Instant::now();
    let mut previous = volumes_overlapping(disk, offset, length);
    let mut stable_since = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(250));
        let now = volumes_overlapping(disk, offset, length);
        if now != previous {
            previous = now;
            stable_since = Instant::now();
        } else if stable_since.elapsed() >= Duration::from_millis(750)
            && started.elapsed() >= MINIMUM
        {
            info!(
                disk, offset, length, volumes = ?previous,
                waited_ms = started.elapsed().as_millis(),
                "volume topology settled"
            );
            return;
        }
        if started.elapsed() >= timeout {
            warn!(
                disk, offset, length, volumes = ?previous,
                "volumes over the target range are still appearing and disappearing — \
                 writing anyway"
            );
            return;
        }
    }
}

impl Drop for VolumeDismountGuard {
    fn drop(&mut self) {
        if !self.volumes.is_empty() {
            debug!(volumes = ?self.volumes, "dismount guard released");
        }
    }
}
