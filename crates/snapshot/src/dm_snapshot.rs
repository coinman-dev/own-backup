//! Device-mapper snapshot of a busy device-mapper device that is not an LVM
//! logical volume — above all an opened LUKS container with a filesystem
//! mounted on it, possibly the root.
//!
//! The mechanism is the one LVM uses for a classic snapshot, built by hand:
//!
//! - `obk-real-<id>`: a second device with the origin's own table (for LUKS, a
//!   second dm-crypt mapping of the same partition);
//! - `obk-snap-<id>`: `snapshot obk-real <cow> N 8`, the read-only point in
//!   time. Its copy-on-write store is a sparse file `.own-backup-cow-<id>.tmp`
//!   in the destination folder, attached as a loop device;
//! - the origin's table becomes `snapshot-origin obk-real`: every write to it
//!   first saves the chunk it overwrites into the store.
//!
//! The switch happens while the origin is suspended — flushed, its filesystem
//! frozen — and that may be the root filesystem, where a process that starts
//! a program, faults a page in from disk or writes a log line waits for ever.
//! So the switch is a few ioctls on buffers prepared in advance, with the
//! process's memory locked (`mlockall`, as LVM does), and nothing is logged
//! until the origin runs again ([`switch`]). Release is the same in reverse.
//!
//! Keys: a LUKS2 mapping usually names its key in the kernel keyring
//! (`:64:logon:cryptsetup:…`), and that key is gone once cryptsetup has
//! exited, so the copied table does not load ("Required key not available").
//! Then the container's password recovers the key from its header
//! ([`ContainerKey`]); `obk-real` gets the key itself, so a killed run can be
//! undone without it, and release puts the origin's own table back. Neither
//! a table nor a key is ever logged or kept in the snapshot's metadata.
//!
//! Ownership: our devices carry the DM UUIDs `OBK-REAL-<id>`/`OBK-SNAP-<id>`,
//! and the run that made them holds `obk-snap` open from its first table (the
//! kernel has no node for it before) until it releases it. A set whose
//! `obk-snap` nobody holds belongs to a dead run ([`remove_orphans`]); a cleanup
//! in the few milliseconds before the first table can only make that run fail
//! before its switch, never leave a volume half-switched.

#![cfg(target_os = "linux")]

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tracing::{info, warn};
use uuid::Uuid;
use zeroize::Zeroizing;

use own_backup_common::linux_block::{self, SYS_CLASS_BLOCK};
use own_backup_common::{tr, OwnBackupError, Result};

use crate::dm_ioctl::{self, flag, udev, Control, Request, Target};
use crate::loop_dev::{self, LoopDevice};

pub const REAL_PREFIX: &str = "obk-real-";
pub const SNAP_PREFIX: &str = "obk-snap-";
const REAL_UUID: &str = "OBK-REAL-";
const SNAP_UUID: &str = "OBK-SNAP-";
/// Name of the copy-on-write store: `.own-backup-cow-<32 hex>.tmp`. Hidden,
/// and with an extension no listing takes for an image.
pub const COW_PREFIX: &str = ".own-backup-cow-";
const COW_SUFFIX: &str = ".tmp";
/// Chunk of the snapshot, in sectors: 4 KiB.
const CHUNK_SECTORS: u64 = 8;

/// Mapping types that can get a second instance on the same devices without
/// harm. A stateful target (integrity, cache, raid, thin) cannot: two
/// instances would each keep their own state of one device.
const COPYABLE: [&str; 4] = ["crypt", "linear", "striped", "zero"];

/// Filesystems that are not on a local block device: a store on them cannot
/// be stacked on the origin.
const DEVICELESS: [&str; 8] = [
    "tmpfs", "ramfs", "nfs", "nfs4", "cifs", "smb3", "9p", "virtiofs",
];

/// Recovers the volume key of the encrypted container on `device` (the device
/// under an opened mapping) from the password the caller has for it. Asked
/// only when the kernel keeps the mapping's key to itself. `Ok(None)`: no
/// password is known.
pub type ContainerKey = dyn Fn(&Path) -> Result<Option<Zeroizing<Vec<u8>>>> + Send + Sync;

/// `.own-backup-cow-<id>.tmp` for a 32-hex-digit `id`.
pub fn cow_file_name(id: &str) -> String {
    format!("{COW_PREFIX}{id}{COW_SUFFIX}")
}

/// A copy-on-write store of ours, by name alone.
pub fn is_cow_name(name: &str) -> bool {
    name.strip_prefix(COW_PREFIX)
        .and_then(|rest| rest.strip_suffix(COW_SUFFIX))
        .is_some_and(is_id)
}

fn is_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn refuse(msg: impl Into<String>) -> OwnBackupError {
    OwnBackupError::NoSnapshotProvider(msg.into())
}

fn failed(volume: &str, error: impl std::fmt::Display) -> OwnBackupError {
    refuse(tr!("linux.dm_failed", volume = volume, error = error))
}

fn read_sys(kernel: &str, attr: &str) -> io::Result<String> {
    std::fs::read_to_string(Path::new(SYS_CLASS_BLOCK).join(kernel).join(attr))
        .map(|s| s.trim().to_string())
}

/// Kernel name of the block device `(major, minor)` (`dm-3`, `loop0`).
fn kernel_of(dev: (u32, u32)) -> Option<String> {
    std::fs::read_link(format!("/sys/dev/block/{}:{}", dev.0, dev.1))
        .ok()?
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
}

fn devstr(dev: (u32, u32)) -> String {
    format!("{}:{}", dev.0, dev.1)
}

fn parse_dev(s: &str) -> Option<(u32, u32)> {
    let (a, b) = s.split_once(':')?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

// ---------------------------------------------------------------------------
// Tables and status lines (pure)
// ---------------------------------------------------------------------------

/// A crypt key that lives in the kernel keyring: `:<size>:<type>:<description>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KeyRef {
    pub size: usize,
    pub ktype: String,
    pub desc: String,
}

/// The keyring reference in a crypt target's parameters
/// (`<cipher> <key> <iv_offset> <device> <offset> [<#opt> <opt>…]`), if its
/// key is one; `None` for a key given inline (hex) or none (`-`).
pub(crate) fn crypt_key_ref(params: &str) -> Option<KeyRef> {
    let key = params.split(' ').nth(1)?;
    let rest = key.strip_prefix(':')?;
    let (size, rest) = rest.split_once(':')?;
    let (ktype, desc) = rest.split_once(':')?;
    Some(KeyRef {
        size: size.parse().ok()?,
        ktype: ktype.to_string(),
        desc: desc.to_string(),
    })
}

/// The device a crypt target encrypts (`major:minor`).
pub(crate) fn crypt_device(params: &str) -> Option<(u32, u32)> {
    parse_dev(params.split(' ').nth(3)?)
}

/// The same crypt parameters with the key given inline, in hex.
pub(crate) fn with_inline_key(params: &str, key: &[u8]) -> Option<Zeroizing<String>> {
    let fields: Vec<&str> = params.split(' ').collect();
    if fields.len() < 5 {
        return None;
    }
    let mut out = Zeroizing::new(String::with_capacity(params.len() + key.len() * 2));
    for (i, f) in fields.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        if i == 1 {
            for b in key {
                out.push(char::from_digit(u32::from(b >> 4), 16).unwrap());
                out.push(char::from_digit(u32::from(b & 0xf), 16).unwrap());
            }
        } else {
            out.push_str(f);
        }
    }
    Some(out)
}

/// What can be done with a live table: copied as it is, or not at all.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TableVerdict {
    Copyable,
    /// A snapshot left by an interrupted run still sits on it.
    Leftover,
    /// A mapping type that must not get a second instance.
    Unsupported(String),
    /// The table does not cover the device as sysfs reports it.
    Mismatch,
}

pub(crate) fn judge_table(table: &[Target], size_sectors: u64) -> TableVerdict {
    if table.iter().any(|t| t.kind == "snapshot-origin") {
        return TableVerdict::Leftover;
    }
    let odd: Vec<&str> = table
        .iter()
        .map(|t| t.kind.as_str())
        .filter(|k| !COPYABLE.contains(k))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if !odd.is_empty() {
        return TableVerdict::Unsupported(odd.join(", "));
    }
    let mut at = 0;
    for t in table {
        if t.start != at {
            return TableVerdict::Mismatch;
        }
        at += t.length;
    }
    if table.is_empty() || at != size_sectors {
        return TableVerdict::Mismatch;
    }
    TableVerdict::Copyable
}

/// The status line of a `snapshot` target.
#[derive(Debug, PartialEq, Eq)]
pub enum SnapState {
    /// `<used>/<total> <metadata>` sectors of the store.
    Valid {
        used: u64,
        total: u64,
    },
    /// Invalidated — the store filled up or could not be written.
    Invalid,
    Overflow,
    Other(String),
}

pub(crate) fn parse_snapshot_status(line: &str) -> SnapState {
    let line = line.trim();
    match line {
        "Invalid" => return SnapState::Invalid,
        "Overflow" => return SnapState::Overflow,
        _ => {}
    }
    let parsed = line.split_whitespace().next().and_then(|f| {
        let (u, t) = f.split_once('/')?;
        Some((u.parse().ok()?, t.parse().ok()?))
    });
    match parsed {
        Some((used, total)) => SnapState::Valid { used, total },
        None => SnapState::Other(line.to_string()),
    }
}

impl SnapState {
    /// Did the snapshot hold one point in time until now?
    pub fn intact(&self) -> bool {
        // A full store is about to be invalidated by the next write.
        matches!(self, SnapState::Valid { used, total } if used < total)
    }
}

// ---------------------------------------------------------------------------
// Where the copy-on-write store may go (pure)
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CowPlace {
    Fine,
    /// The folder's filesystem sits on this device of the origin's stack:
    /// saving a chunk would write to the origin again.
    OnOrigin(String),
    /// Cannot tell where the folder's filesystem keeps its data, or a
    /// userspace process (FUSE) would be in the write path of the origin.
    Unknown(String),
}

/// `origin_above`/`origin_below`: the origin and what is stacked on it / what
/// it is stacked on. `dir_dev`: the kernel name of the device the folder's
/// filesystem is on, with what that is stacked on (`dir_below`).
pub(crate) fn judge_cow_dir(
    origin_above: &[String],
    origin_below: &[String],
    dir_dev: Option<&str>,
    dir_below: &[String],
    fstype: &str,
) -> CowPlace {
    if fstype == "fuse" || fstype == "fuseblk" || fstype.starts_with("fuse.") {
        return CowPlace::Unknown(fstype.to_string());
    }
    let Some(dev) = dir_dev else {
        return if DEVICELESS.contains(&fstype) {
            CowPlace::Fine
        } else {
            CowPlace::Unknown(fstype.to_string())
        };
    };
    let on = |d: &str| origin_above.iter().any(|o| o == d) || origin_below.iter().any(|o| o == d);
    if on(dev)
        || dir_below
            .iter()
            .any(|d| origin_above.iter().any(|o| o == d))
    {
        return CowPlace::OnOrigin(dev.to_string());
    }
    CowPlace::Fine
}

/// The mount `path` (absolute, resolved) is on: the longest mount point that
/// is a whole-component prefix of it; of equal ones the later (overmounting).
pub(crate) fn mount_of<'a>(
    mounts: &'a [linux_block::MountEntry],
    path: &Path,
) -> Option<&'a linux_block::MountEntry> {
    let mut best: Option<&linux_block::MountEntry> = None;
    for m in mounts {
        if path.starts_with(&m.mount_point)
            && best.is_none_or(|b| m.mount_point.len() >= b.mount_point.len())
        {
            best = Some(m);
        }
    }
    best
}

/// Refuse a store folder whose filesystem is on the origin's own stack.
fn check_cow_dir(volume: &str, origin_kernel: &str, dir: &Path) -> Result<PathBuf> {
    let sys = Path::new(SYS_CLASS_BLOCK);
    let real = std::fs::canonicalize(dir).map_err(|e| {
        refuse(tr!(
            "linux.dm_cow_dir_unusable",
            dir = dir.display(),
            error = e
        ))
    })?;
    let mounts = linux_block::parse_mountinfo(
        &std::fs::read_to_string("/proc/self/mountinfo").map_err(|e| failed(volume, e))?,
    );
    let m = mount_of(&mounts, &real).ok_or_else(|| {
        refuse(tr!(
            "linux.dm_cow_dir_unusable",
            dir = real.display(),
            error = "no mount"
        ))
    })?;
    let dev = if m.major != 0 {
        kernel_of((m.major, m.minor))
    } else if m.source.starts_with('/') {
        linux_block::kernel_name(Path::new(&m.source))
    } else {
        None
    };
    let dir_below = dev
        .as_deref()
        .map(|d| linux_block::stack_below(sys, d))
        .unwrap_or_default();
    match judge_cow_dir(
        &linux_block::stack_above(sys, origin_kernel),
        &linux_block::stack_below(sys, origin_kernel),
        dev.as_deref(),
        &dir_below,
        &m.fstype,
    ) {
        CowPlace::Fine => Ok(real),
        CowPlace::OnOrigin(_) => Err(refuse(tr!(
            "linux.dm_cow_on_source",
            dir = real.display(),
            volume = volume
        ))),
        CowPlace::Unknown(fs) => Err(refuse(tr!(
            "linux.dm_cow_dir_unknown",
            dir = real.display(),
            fstype = fs
        ))),
    }
}

// ---------------------------------------------------------------------------
// Copy-on-write store
// ---------------------------------------------------------------------------

/// Delete the stores in `dir` nobody uses any more — left by a run that was
/// killed before device-mapper held them, or by a machine that restarted
/// during a backup. A store in use is locked: its run holds an exclusive
/// `flock`, and so does the loop device that shares the run's open file.
pub fn sweep_cow_files(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !is_cow_name(name) {
            continue;
        }
        if delete_if_unused(&entry.path()) {
            info!(path = %entry.path().display(), "removed a snapshot store left by an interrupted run");
            removed += 1;
        }
    }
    removed
}

/// Delete our store at `path` if no run and no loop device holds it.
fn delete_if_unused(path: &Path) -> bool {
    let Ok(file) = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    else {
        return false;
    };
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return false;
    }
    // SAFETY: flock on an open descriptor.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return false;
    }
    std::fs::remove_file(path).is_ok()
}

/// Size of a store on a filesystem without sparse files (FAT, exFAT, NTFS
/// through ntfs3), where every byte of it is allocated up front: a fifth of
/// the origin, as LVM gives a classic snapshot, at least 256 MiB, at most half
/// the free space and what FAT holds in one file. If more is written to the
/// origin during the copy, the snapshot overflows and the copy is refused
/// ([`SnapState::intact`]).
pub(crate) fn dense_store_size(full: u64, free: u64) -> u64 {
    let chunk = CHUNK_SECTORS * 512;
    let want = (full / 5).max(256 << 20).min(full);
    let cap = (free / 2).min((4 << 30) - chunk);
    (want.min(cap) / chunk) * chunk
}

/// A new store, locked for as long as it is used: a sparse file as large as
/// the origin (one chunk per chunk of it at most), or [`dense_store_size`]
/// where the filesystem allocates what a file is extended to.
fn create_cow(dir: &Path, id: &str, origin_bytes: u64) -> io::Result<(PathBuf, File)> {
    let path = dir.join(cow_file_name(id));
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC)
        .open(&path)?;
    let made = (|| {
        // SAFETY: flock on an open descriptor.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let chunk = CHUNK_SECTORS * 512;
        let full = origin_bytes.div_ceil(chunk) * chunk + chunk;
        // Does extending allocate? Ask with 16 MiB before asking for all.
        let probe = full.min(16 << 20);
        file.set_len(probe)?;
        let allocated = std::os::unix::fs::MetadataExt::blocks(&file.metadata()?) * 512;
        if allocated < probe / 2 {
            return file.set_len(full);
        }
        // SAFETY: fstatvfs into a zeroed struct.
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        let free = if unsafe { libc::fstatvfs(file.as_raw_fd(), &mut st) } == 0 {
            st.f_bavail as u64 * st.f_frsize as u64
        } else {
            0
        };
        let size = dense_store_size(full, free);
        if size < chunk * 2 {
            return Err(io::Error::from_raw_os_error(libc::ENOSPC));
        }
        warn!(
            path = %path.display(), size,
            "the destination filesystem has no sparse files: the snapshot's store is \
             allocated in full and smaller than the volume"
        );
        file.set_len(size)
    })();
    if let Err(e) = made {
        let _ = std::fs::remove_file(&path);
        return Err(e);
    }
    Ok((path, file))
}

// ---------------------------------------------------------------------------
// Keyring (exact restore of a table that names its key there)
// ---------------------------------------------------------------------------

const KEY_SPEC_PROCESS_KEYRING: libc::c_long = -2;
const KEYCTL_INVALIDATE: libc::c_long = 21;

fn is_key_errno(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::ENOKEY | libc::EKEYEXPIRED | libc::EKEYREVOKED | libc::EKEYREJECTED)
    )
}

/// A key placed in this process's keyring for one table load, invalidated
/// when dropped.
struct KeyringKey(libc::c_long);

impl KeyringKey {
    fn add(r: &KeyRef, key: &[u8]) -> io::Result<KeyringKey> {
        let ktype = std::ffi::CString::new(r.ktype.as_str()).map_err(io::Error::other)?;
        let desc = std::ffi::CString::new(r.desc.as_str()).map_err(io::Error::other)?;
        // SAFETY: add_key(2) with valid C strings and a payload slice.
        let id = unsafe {
            libc::syscall(
                libc::SYS_add_key,
                ktype.as_ptr(),
                desc.as_ptr(),
                key.as_ptr(),
                key.len(),
                KEY_SPEC_PROCESS_KEYRING,
            )
        };
        if id < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(KeyringKey(id))
    }
}

impl Drop for KeyringKey {
    fn drop(&mut self) {
        // SAFETY: keyctl(2) on a key id this process created.
        unsafe { libc::syscall(libc::SYS_keyctl, KEYCTL_INVALIDATE, self.0) };
    }
}

// ---------------------------------------------------------------------------
// The switch
// ---------------------------------------------------------------------------

/// Requests issued while the origin is suspended, all built in advance.
struct Prepared {
    suspend: Request,
    /// Activate the snapshot (creation only).
    resume_snapshot: Option<Request>,
    /// Resume the origin with its new table.
    resume: Request,
    /// On failure: drop the origin's inactive table, then resume it as it was.
    clear: Request,
    retries: [Request; 3],
}

impl Prepared {
    fn new(origin: &str, suspend_flags: u32, snapshot: Option<&str>) -> io::Result<Self> {
        Ok(Prepared {
            suspend: dm_ioctl::suspend_request(origin, suspend_flags)?,
            resume_snapshot: snapshot
                .map(|s| dm_ioctl::resume_request(s, udev::HIDDEN))
                .transpose()?,
            resume: dm_ioctl::resume_request(origin, udev::VISIBLE)?,
            clear: dm_ioctl::clear_request(origin)?,
            retries: [
                dm_ioctl::resume_request(origin, udev::VISIBLE)?,
                dm_ioctl::resume_request(origin, udev::VISIBLE)?,
                dm_ioctl::resume_request(origin, udev::VISIBLE)?,
            ],
        })
    }
}

/// What happened while the origin was suspended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Switched {
    Done,
    /// Memory could not be locked; nothing was suspended.
    NotLocked(i32),
    /// The origin could not be suspended; it runs as before.
    NotSuspended(i32),
    /// A later step failed; `running` says whether the origin was resumed.
    Failed {
        step: &'static str,
        errno: i32,
        running: bool,
    },
}

/// The instant itself: suspend the origin, activate the snapshot if there is
/// one, resume the origin with its inactive table swapped in.
///
/// Between suspend and resume nothing here allocates, logs, starts a program
/// or touches a file: the origin may hold the root filesystem, this binary
/// and its log. Every page this process has is locked in memory first, so
/// not even a page fault waits for the suspended device.
fn switch(ctl: &Control, p: &mut Prepared) -> Switched {
    // SAFETY: plain syscalls without pointers.
    if unsafe { libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE) } != 0 {
        return Switched::NotLocked(io::Error::last_os_error().raw_os_error().unwrap_or(0));
    }
    let out = 'run: {
        if let Err(e) = ctl.issue(&mut p.suspend) {
            break 'run Switched::NotSuspended(e);
        }
        if let Some(r) = p.resume_snapshot.as_mut() {
            if let Err(errno) = ctl.issue(r) {
                let running = roll_back(ctl, p);
                break 'run Switched::Failed {
                    step: "resume snapshot",
                    errno,
                    running,
                };
            }
        }
        if let Err(errno) = ctl.issue(&mut p.resume) {
            let running = roll_back(ctl, p);
            break 'run Switched::Failed {
                step: "resume origin",
                errno,
                running,
            };
        }
        Switched::Done
    };
    // SAFETY: as above.
    unsafe { libc::munlockall() };
    out
}

/// Put a suspended origin back to work on whatever table it has.
fn roll_back(ctl: &Control, p: &mut Prepared) -> bool {
    let _ = ctl.issue(&mut p.clear);
    p.retries.iter_mut().any(|r| ctl.issue(r).is_ok())
}

fn errno_text(errno: i32) -> String {
    io::Error::from_raw_os_error(errno).to_string()
}

/// Flush what the filesystems on the origin hold in memory, so the freeze in
/// the switch has little left to write.
fn sync_mounts(mount_points: &[String]) {
    for m in mount_points {
        if let Ok(dir) = File::open(m) {
            // SAFETY: syncfs on an open descriptor.
            unsafe { libc::syncfs(dir.as_raw_fd()) };
        }
    }
}

/// Swap the origin back onto `table` (already loaded as its inactive table):
/// suspend without freezing — nothing is captured here — and resume.
fn switch_back(ctl: &Control, origin: &str) -> std::result::Result<(), String> {
    let mut p = Prepared::new(origin, flag::SKIP_LOCKFS, None).map_err(|e| e.to_string())?;
    match switch(ctl, &mut p) {
        Switched::Done => Ok(()),
        Switched::NotLocked(e) | Switched::NotSuspended(e) => {
            let _ = ctl.clear(origin);
            Err(errno_text(e))
        }
        Switched::Failed {
            step,
            errno,
            running,
        } => Err(format!(
            "{step}: {}{}",
            errno_text(errno),
            if running {
                ""
            } else {
                " (the volume stays suspended)"
            }
        )),
    }
}

/// Remove a device, waiting out a short-lived opener (udev).
fn remove_device(ctl: &Control, name: &str) -> io::Result<()> {
    let mut last = None;
    for _ in 0..20 {
        match ctl.remove(name, udev::HIDDEN) {
            Ok(()) => return Ok(()),
            Err(e) if e.raw_os_error() == Some(libc::ENXIO) => return Ok(()),
            Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {
                last = Some(e);
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::from_raw_os_error(libc::EBUSY)))
}

// ---------------------------------------------------------------------------
// A snapshot held by this process
// ---------------------------------------------------------------------------

/// Everything one snapshot holds, from creation to release.
pub struct Session {
    pub id: String,
    pub origin: String,
    pub real: String,
    pub snap: String,
    /// `/dev/dm-N` of the snapshot: what the copy reads.
    pub snap_path: String,
    pub cow_path: PathBuf,
    ctl: Control,
    /// `obk-snap` held open: the proof that this run is alive.
    holder: Option<File>,
    lo: Option<LoopDevice>,
    cow: Option<File>,
    /// The origin's table as it was, keys included.
    original: Vec<Target>,
    /// The key recovered from the container's password, when the original
    /// names a keyring key that is not reachable.
    key: Option<(KeyRef, Zeroizing<Vec<u8>>)>,
    real_made: bool,
    snap_made: bool,
    origin_loaded: bool,
}

impl Session {
    /// Undo a creation that did not reach the switch, or whose switch was
    /// rolled back: the origin runs on its own table again.
    fn abandon(&mut self) {
        if self.origin_loaded {
            let _ = self.ctl.clear(&self.origin);
            self.origin_loaded = false;
        }
        self.remove_layers();
    }

    /// Remove obk-snap, obk-real, the loop device and the store.
    fn remove_layers(&mut self) -> Vec<String> {
        let mut problems = Vec::new();
        self.holder = None;
        if self.snap_made {
            match remove_device(&self.ctl, &self.snap) {
                Ok(()) => self.snap_made = false,
                Err(e) => problems.push(format!("{}: {e}", self.snap)),
            }
        }
        if self.real_made && !self.snap_made {
            match remove_device(&self.ctl, &self.real) {
                Ok(()) => self.real_made = false,
                Err(e) => problems.push(format!("{}: {e}", self.real)),
            }
        }
        if !self.snap_made {
            if let Some(lo) = self.lo.take() {
                if let Err(e) = lo.detach() {
                    problems.push(format!("{}: {e}", lo.path()));
                }
            }
            if self.cow.take().is_some() {
                if let Err(e) = std::fs::remove_file(&self.cow_path) {
                    if e.kind() != io::ErrorKind::NotFound {
                        problems.push(format!("{}: {e}", self.cow_path.display()));
                    }
                }
            }
        }
        problems
    }

    /// Is the snapshot still one point in time?
    pub fn state(&self) -> io::Result<SnapState> {
        state_of(&self.ctl, &self.snap)
    }

    /// Put the origin back on its own table and remove everything else.
    pub fn release(mut self) -> Result<()> {
        // Load the origin's own table first, with its keyring key in reach if
        // the password gave it; else obk-real's table, which is the same
        // mapping with the key inline.
        let exact = {
            let _key = match &self.key {
                Some((r, k)) => KeyringKey::add(r, k).ok(),
                None => None,
            };
            self.ctl.load(&self.origin, &self.original, 0)
        };
        if let Err(e) = exact {
            let table = self.ctl.table(&self.real).map_err(|e2| {
                bad(format!(
                    "{}: cannot load its own table ({e}) nor read {}'s ({e2})",
                    self.origin, self.real
                ))
            })?;
            self.ctl
                .load(&self.origin, &table, 0)
                .map_err(|e| bad(format!("{}: cannot load its table back: {e}", self.origin)))?;
        }
        switch_back(&self.ctl, &self.origin)
            .map_err(|e| bad(format!("{}: cannot switch back: {e}", self.origin)))?;
        let problems = self.remove_layers();
        info!(origin = %self.origin, snapshot = %self.snap, "device-mapper snapshot removed");
        if problems.is_empty() {
            Ok(())
        } else {
            Err(bad(format!("left behind: {}", problems.join("; "))))
        }
    }
}

fn bad(msg: impl std::fmt::Display) -> OwnBackupError {
    OwnBackupError::Device(format!("device-mapper snapshot: {msg}"))
}

/// Take a snapshot of the busy device-mapper device `volume` (kernel name
/// `kernel`), its store in `cow_dir`. Blocking: call it off the async runtime.
pub fn create(
    volume: &str,
    kernel: &str,
    rw_mounts: &[String],
    cow_dir: &Path,
    container_key: Option<&ContainerKey>,
) -> Result<Session> {
    let origin = read_sys(kernel, "dm/name").map_err(|e| failed(volume, e))?;
    let size: u64 = read_sys(kernel, "size")
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| failed(volume, "no size in sysfs"))?;
    let ctl = Control::open().map_err(|e| failed(volume, format!("/dev/mapper/control: {e}")))?;
    let info = ctl.status(&origin).map_err(|e| failed(volume, e))?;
    if Some(info.dev) != linux_block::dev_numbers(Path::new(SYS_CLASS_BLOCK), kernel) {
        return Err(failed(volume, format!("{origin} is not {kernel}")));
    }
    let original = ctl.table(&origin).map_err(|e| failed(volume, e))?;
    match judge_table(&original, size) {
        TableVerdict::Copyable => {}
        TableVerdict::Leftover => {
            return Err(refuse(tr!("linux.dm_leftover", volume = volume)));
        }
        TableVerdict::Unsupported(kinds) => {
            return Err(refuse(tr!(
                "linux.dm_unsupported",
                volume = volume,
                kinds = kinds
            )));
        }
        TableVerdict::Mismatch => {
            return Err(failed(volume, "its table does not cover the device"));
        }
    }
    let cow_dir = check_cow_dir(volume, kernel, cow_dir)?;
    sweep_cow_files(&cow_dir);

    let id = Uuid::new_v4().simple().to_string();
    let (cow_path, cow) = create_cow(&cow_dir, &id, size * 512).map_err(|e| {
        refuse(tr!(
            "linux.dm_cow_failed",
            dir = cow_dir.display(),
            error = e
        ))
    })?;
    let mut s = Session {
        real: format!("{REAL_PREFIX}{id}"),
        snap: format!("{SNAP_PREFIX}{id}"),
        id,
        origin,
        snap_path: String::new(),
        cow_path,
        ctl,
        holder: None,
        lo: None,
        cow: Some(cow),
        original,
        key: None,
        real_made: false,
        snap_made: false,
        origin_loaded: false,
    };
    match build(&mut s, volume, size, rw_mounts, container_key) {
        Ok(()) => Ok(s),
        Err(e) => {
            s.abandon();
            Err(e)
        }
    }
}

fn build(
    s: &mut Session,
    volume: &str,
    size: u64,
    rw_mounts: &[String],
    container_key: Option<&ContainerKey>,
) -> Result<()> {
    let cow = s.cow.as_ref().expect("store is open");
    let lo =
        LoopDevice::attach(cow, &s.cow_path).map_err(|e| failed(volume, format!("loop: {e}")))?;
    let cow_dev = lo.dev();
    s.lo = Some(lo);

    let snap = s
        .ctl
        .create(&s.snap, &format!("{SNAP_UUID}{}", s.id))
        .map_err(|e| failed(volume, format!("{}: {e}", s.snap)))?;
    s.snap_made = true;
    let real = s
        .ctl
        .create(&s.real, &format!("{REAL_UUID}{}", s.id))
        .map_err(|e| failed(volume, format!("{}: {e}", s.real)))?;
    s.real_made = true;
    load_real(s, volume, container_key)?;
    s.ctl
        .resume(&s.real, udev::HIDDEN)
        .map_err(|e| failed(volume, format!("{}: {e}", s.real)))?;

    s.ctl
        .load(
            &s.snap,
            &[Target::new(
                0,
                size,
                "snapshot",
                format!("{} {} N {CHUNK_SECTORS}", devstr(real.dev), devstr(cow_dev)),
            )],
            flag::READONLY,
        )
        .map_err(|e| failed(volume, format!("{}: {e}", s.snap)))?;
    // The kernel registers a device-mapper disk with its first table, so only
    // now is there a node to hold. From here until release the snapshot is
    // held open: while it is, no cleanup takes this set for a dead run's.
    let snap_kernel = kernel_of(snap.dev).unwrap_or_else(|| format!("dm-{}", snap.dev.1));
    s.snap_path = format!("/dev/{snap_kernel}");
    s.holder =
        Some(open_held(&s.snap_path).map_err(|e| failed(volume, format!("{}: {e}", s.snap_path)))?);
    s.ctl
        .load(
            &s.origin,
            &[Target::new(0, size, "snapshot-origin", devstr(real.dev))],
            0,
        )
        .map_err(|e| failed(volume, format!("{}: {e}", s.origin)))?;
    s.origin_loaded = true;

    sync_mounts(rw_mounts);
    let mut p = Prepared::new(&s.origin, 0, Some(&s.snap)).map_err(|e| failed(volume, e))?;
    // ---- from here to `switch` returning: no logging, no file I/O ----
    let switched = switch(&s.ctl, &mut p);
    match switched {
        Switched::Done => {
            s.origin_loaded = false;
            info!(
                origin = %s.origin, snapshot = %s.snap, device = %s.snap_path,
                store = %s.cow_path.display(), "device-mapper snapshot created"
            );
            Ok(())
        }
        Switched::NotLocked(e) => Err(failed(volume, format!("mlockall: {}", errno_text(e)))),
        Switched::NotSuspended(e) => Err(failed(
            volume,
            format!("suspend {}: {}", s.origin, errno_text(e)),
        )),
        Switched::Failed {
            step,
            errno,
            running,
        } => {
            warn!(origin = %s.origin, step, error = %errno_text(errno), running, "device-mapper snapshot switch failed");
            // The origin may have taken the new table before failing: then it
            // goes back the way a release does. If even that fails, obk-real
            // stays (the origin runs through it) and the rest is removed.
            if running && origin_is_switched(&s.ctl, &s.origin) {
                if let Err(e) = put_back(s) {
                    warn!(origin = %s.origin, error = %e, "cannot put the origin back on its own table");
                }
            }
            Err(failed(volume, format!("{step}: {}", errno_text(errno))))
        }
    }
}

/// Open a device read-only, waiting briefly for its node.
fn open_held(path: &str) -> io::Result<File> {
    let mut last = None;
    for _ in 0..20 {
        match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC)
            .open(path)
        {
            Ok(f) => return Ok(f),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                last = Some(e);
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::from(io::ErrorKind::NotFound)))
}

/// Give obk-real the origin's table — with the key inline when the original
/// names a keyring key this process cannot reach.
fn load_real(s: &mut Session, volume: &str, container_key: Option<&ContainerKey>) -> Result<()> {
    let err = match s.ctl.load(&s.real, &s.original, 0) {
        Ok(()) => return Ok(()),
        Err(e) if is_key_errno(&e) => e,
        Err(e) => return Err(failed(volume, format!("{}: {e}", s.real))),
    };
    let refs: Vec<(KeyRef, (u32, u32))> = s
        .original
        .iter()
        .filter(|t| t.kind == "crypt")
        .filter_map(|t| Some((crypt_key_ref(&t.params)?, crypt_device(&t.params)?)))
        .collect();
    let Some((key_ref, under)) = refs.first().cloned() else {
        return Err(failed(volume, format!("{}: {err}", s.real)));
    };
    let device = kernel_of(under)
        .map(|k| PathBuf::from(format!("/dev/{k}")))
        .ok_or_else(|| failed(volume, format!("no device {}", devstr(under))))?;
    let key = match container_key.map(|f| f(&device)).transpose() {
        Ok(Some(Some(key))) => key,
        Ok(_) => return Err(refuse(tr!("linux.dm_key_needed", volume = volume))),
        Err(e) => {
            return Err(refuse(tr!(
                "linux.dm_key_failed",
                volume = volume,
                error = e
            )))
        }
    };
    if key.len() != key_ref.size {
        return Err(refuse(tr!(
            "linux.dm_key_failed",
            volume = volume,
            error = format!(
                "key of {} bytes, the mapping needs {}",
                key.len(),
                key_ref.size
            )
        )));
    }
    let mut inline = Vec::with_capacity(s.original.len());
    for t in &s.original {
        let params = if t.kind == "crypt" && crypt_key_ref(&t.params).is_some() {
            with_inline_key(&t.params, &key)
                .ok_or_else(|| failed(volume, "unexpected crypt parameters"))?
        } else {
            t.params.clone()
        };
        inline.push(Target {
            start: t.start,
            length: t.length,
            kind: t.kind.clone(),
            params,
        });
    }
    s.ctl
        .load(&s.real, &inline, 0)
        .map_err(|e| refuse(tr!("linux.dm_key_failed", volume = volume, error = e)))?;
    s.key = Some((key_ref, key));
    Ok(())
}

/// Is the origin running on `snapshot-origin`?
fn origin_is_switched(ctl: &Control, origin: &str) -> bool {
    ctl.table(origin)
        .is_ok_and(|t| t.iter().any(|t| t.kind == "snapshot-origin"))
}

/// The release's first half, for a switch that half-happened.
fn put_back(s: &mut Session) -> std::result::Result<(), String> {
    let table = s.ctl.table(&s.real).map_err(|e| e.to_string())?;
    s.ctl
        .load(&s.origin, &table, 0)
        .map_err(|e| e.to_string())?;
    switch_back(&s.ctl, &s.origin)
}

// ---------------------------------------------------------------------------
// Sets left by killed runs
// ---------------------------------------------------------------------------

/// One device-mapper device, as far as orphan cleanup needs to know it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DmEntry {
    pub name: String,
    pub uuid: String,
    pub dev: (u32, u32),
    pub open_count: i32,
    pub suspended: bool,
    /// For a device whose table is one `snapshot-origin`: its device.
    pub origin_of: Option<String>,
    /// For a device whose inactive table is built on one of our obk-real —
    /// a run killed between loading the switch and making it: that obk-real.
    pub pending_on: Option<(u32, u32)>,
    /// For a `snapshot`: its store device.
    pub cow: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Real,
    Snap,
}

/// `(role, id)` of one of our devices — by UUID, with the name to match.
fn ours(e: &DmEntry) -> Option<(Role, &str)> {
    let (role, id, prefix) = match e.uuid.strip_prefix(REAL_UUID) {
        Some(id) => (Role::Real, id, REAL_PREFIX),
        None => (Role::Snap, e.uuid.strip_prefix(SNAP_UUID)?, SNAP_PREFIX),
    };
    (is_id(id) && e.name.strip_prefix(prefix) == Some(id)).then_some((role, id))
}

/// What to undo for one dead run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OrphanPlan {
    pub id: String,
    /// Devices running on `snapshot-origin` over its obk-real: put back first.
    pub origins: Vec<String>,
    /// Devices with the switch loaded but not made, and whether they are left
    /// suspended: their inactive table is dropped and they are resumed.
    pub pending: Vec<(String, bool)>,
    pub snap: Option<String>,
    pub real: Option<String>,
    /// The store's loop device.
    pub cow: Option<(u32, u32)>,
}

/// The sets of ours whose run is gone: nobody holds their obk-snap (or it is
/// already gone). `only`: plan just this id, held or not (our own release).
pub(crate) fn plan_orphans(entries: &[DmEntry], only: Option<&str>) -> Vec<OrphanPlan> {
    let mut ids: BTreeSet<&str> = entries
        .iter()
        .filter_map(|e| ours(e).map(|o| o.1))
        .collect();
    if let Some(id) = only {
        ids.retain(|i| *i == id);
    }
    let find = |role: Role, id: &str| {
        entries
            .iter()
            .find(|e| ours(e).is_some_and(|(r, i)| r == role && i == id))
    };
    let mut plans = Vec::new();
    for id in ids {
        let snap = find(Role::Snap, id);
        let real = find(Role::Real, id);
        if only.is_none() && snap.is_some_and(|s| s.open_count > 0) {
            continue; // its run is alive
        }
        let others = || entries.iter().filter(|e| ours(e).is_none());
        let origins = real
            .map(|r| {
                let dev = devstr(r.dev);
                others()
                    .filter(|e| e.origin_of.as_deref() == Some(dev.as_str()))
                    .map(|e| e.name.clone())
                    .collect()
            })
            .unwrap_or_default();
        let pending = real
            .map(|r| {
                others()
                    .filter(|e| e.pending_on == Some(r.dev))
                    .map(|e| (e.name.clone(), e.suspended))
                    .collect()
            })
            .unwrap_or_default();
        plans.push(OrphanPlan {
            id: id.to_string(),
            origins,
            pending,
            snap: snap.map(|s| s.name.clone()),
            real: real.map(|r| r.name.clone()),
            cow: snap.and_then(|s| s.cow.as_deref()).and_then(parse_dev),
        });
    }
    plans
}

/// The device-mapper devices as orphan planning needs them. Tables are read
/// only for our snapshots and for devices built on one of our obk-real.
fn gather(ctl: &Control) -> io::Result<Vec<DmEntry>> {
    let listed = ctl.list()?;
    let reals: BTreeSet<(u32, u32)> = listed
        .iter()
        .filter(|(n, _)| n.starts_with(REAL_PREFIX))
        .map(|(_, d)| *d)
        .collect();
    let mut out = Vec::new();
    for (name, dev) in listed {
        let Ok(info) = ctl.status(&name) else {
            continue;
        };
        let mut e = DmEntry {
            name,
            uuid: info.uuid.clone(),
            dev,
            open_count: info.open_count,
            suspended: info.suspended(),
            origin_of: None,
            pending_on: None,
            cow: None,
        };
        match ours(&e) {
            Some((Role::Snap, _)) => {
                if let Ok(t) = ctl.table(&e.name) {
                    if let [t] = t.as_slice() {
                        if t.kind == "snapshot" {
                            e.cow = t.params.split(' ').nth(1).map(str::to_string);
                        }
                    }
                }
            }
            Some((Role::Real, _)) => {}
            None => {
                if info.flags & flag::INACTIVE_PRESENT != 0 {
                    e.pending_on = ctl
                        .inactive_deps(&e.name)
                        .ok()
                        .and_then(|d| d.into_iter().find(|d| reals.contains(d)));
                }
                let on_real = ctl
                    .deps(&e.name)
                    .is_ok_and(|d| d.iter().any(|d| reals.contains(d)));
                if on_real {
                    if let Ok(t) = ctl.table(&e.name) {
                        if let [t] = t.as_slice() {
                            if t.kind == "snapshot-origin" {
                                e.origin_of = Some(t.params.trim().to_string());
                            }
                        }
                    }
                }
            }
        }
        out.push(e);
    }
    Ok(out)
}

/// Carry out one plan. `Ok(true)`: nothing of it is left.
fn undo(ctl: &Control, plan: &OrphanPlan) -> bool {
    let mut whole = true;
    // Killed with the switch loaded but not made (maybe even with the origin
    // suspended): drop it, and let the origin run on its own table.
    for (origin, suspended) in &plan.pending {
        let back = ctl.clear(origin).and_then(|()| {
            if *suspended {
                ctl.resume(origin, udev::VISIBLE)
            } else {
                Ok(())
            }
        });
        match back {
            Ok(()) => {
                info!(origin = %origin, suspended, "dropped a snapshot switch an interrupted run had loaded")
            }
            Err(e) => {
                warn!(origin = %origin, error = %e, "cannot drop a snapshot switch an interrupted run had loaded");
                whole = false;
            }
        }
    }
    if let Some(real) = &plan.real {
        for origin in &plan.origins {
            let put = ctl
                .table(real)
                .map_err(|e| e.to_string())
                .and_then(|t| ctl.load(origin, &t, 0).map_err(|e| e.to_string()))
                .and_then(|()| switch_back(ctl, origin));
            match put {
                Ok(()) => info!(origin = %origin, from = %real, "volume put back on its own table"),
                Err(e) => {
                    let _ = ctl.clear(origin);
                    warn!(
                        origin = %origin, through = %real, error = %e,
                        "cannot put the volume back on its own table: it keeps running through the \
                         snapshot's layer until it is closed or the machine restarts"
                    );
                    whole = false;
                }
            }
        }
    }
    // Which file the store's loop device carries, asked before obk-snap goes:
    // the loop device detaches itself as soon as device-mapper lets go of it.
    let store = plan
        .cow
        .and_then(kernel_of)
        .filter(|k| k.starts_with("loop"))
        .and_then(|k| store_of(&k).map(|f| (k, f)));
    if let Some(snap) = &plan.snap {
        if let Err(e) = remove_device(ctl, snap) {
            warn!(device = %snap, error = %e, "cannot remove a leftover snapshot device");
            return false; // opened meanwhile: leave the rest alone
        }
    }
    if let (Some(real), true) = (&plan.real, whole) {
        if let Err(e) = remove_device(ctl, real) {
            warn!(device = %real, error = %e, "cannot remove a leftover snapshot layer");
            whole = false;
        }
    }
    if let Some((kernel, file)) = store {
        release_store(&kernel, &file);
    }
    whole
}

/// The store of ours loop device `kernel` carries, if it carries one.
fn store_of(kernel: &str) -> Option<PathBuf> {
    loop_dev::backing_file(Path::new(SYS_CLASS_BLOCK), kernel).filter(|f| {
        f.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(is_cow_name)
    })
}

/// Detach loop device `kernel` if it still carries our store `file`, and
/// delete the store once nothing holds it.
fn release_store(kernel: &str, file: &Path) {
    let sys = Path::new(SYS_CLASS_BLOCK);
    // Autoclear detaches it once device-mapper lets go; give it a moment.
    for _ in 0..30 {
        if loop_dev::backing_file(sys, kernel).as_deref() != Some(file) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if linux_block::stack_above(sys, kernel).len() <= 1 {
        let _ = loop_dev::detach_if_backed_by(sys, kernel, file);
    }
    // A detached loop device lets go of its file (and with it the lock) a
    // moment later, from a kernel worker.
    for _ in 0..30 {
        if delete_if_unused(file) {
            info!(path = %file.display(), "removed a snapshot store left by an interrupted run");
            return;
        }
        if !file.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    warn!(path = %file.display(), "a snapshot store is still held; the next run in its folder removes it");
}

/// Undo what killed runs left: put each origin back on its own table, remove
/// obk-snap-* and obk-real-*, detach their loop devices and delete their
/// stores. Sets whose obk-snap is still held belong to a live run and are left
/// alone. Returns how many sets were undone completely. Blocking.
pub fn remove_orphans() -> usize {
    let Ok(ctl) = Control::open() else {
        return 0; // no device-mapper, or no permission
    };
    let entries = match gather(&ctl) {
        Ok(e) => e,
        Err(e) => {
            warn!(error = %e, "cannot list device-mapper devices");
            return 0;
        }
    };
    let mut done = 0;
    for plan in plan_orphans(&entries, None) {
        info!(id = %plan.id, origins = ?plan.origins, "undoing a device-mapper snapshot left by an interrupted run");
        if undo(&ctl, &plan) {
            done += 1;
        }
    }
    // Stores whose loop device nothing is stacked on any more.
    let sys = Path::new(SYS_CLASS_BLOCK);
    if let Ok(entries) = std::fs::read_dir(sys) {
        for e in entries.flatten() {
            let kernel = e.file_name().to_string_lossy().into_owned();
            if kernel.starts_with("loop") && linux_block::stack_above(sys, &kernel).len() <= 1 {
                if let Some(file) = store_of(&kernel) {
                    release_store(&kernel, &file);
                }
            }
        }
    }
    done
}

/// The state of the snapshot device `snap`.
fn state_of(ctl: &Control, snap: &str) -> io::Result<SnapState> {
    let status = ctl.target_status(snap)?;
    Ok(match status.as_slice() {
        [t] if t.kind == "snapshot" => parse_snapshot_status(&t.params),
        _ => SnapState::Other(format!("{} targets", status.len())),
    })
}

/// The state of the snapshot device `snap` (`obk-snap-…`) as the kernel shows
/// it, for a check without the [`Session`] that holds it.
pub fn snapshot_state(snap: &str) -> io::Result<SnapState> {
    state_of(&Control::open()?, snap)
}

/// Release snapshot `id` from what the kernel shows, for a release without
/// the session that created it.
pub fn release_by_id(id: &str) -> Result<()> {
    let ctl = Control::open().map_err(|e| bad(format!("/dev/mapper/control: {e}")))?;
    let entries = gather(&ctl).map_err(bad)?;
    match plan_orphans(&entries, Some(id)).first() {
        None => Ok(()),
        Some(plan) => {
            if undo(&ctl, plan) {
                Ok(())
            } else {
                Err(bad(format!("snapshot {id} only partly removed")))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn store_names() {
        let name = cow_file_name(ID);
        assert_eq!(name, ".own-backup-cow-0123456789abcdef0123456789abcdef.tmp");
        assert!(is_cow_name(&name));
        assert!(!is_cow_name(".own-backup-cow-0123.tmp"));
        assert!(!is_cow_name(
            ".own-backup-cow-0123456789ABCDEF0123456789abcdef.tmp"
        ));
        assert!(!is_cow_name(
            ".own-backup-new-0123456789abcdef0123456789abcdef.tmp.vhdx"
        ));
        assert!(!is_cow_name(&format!("{name}.vhdx")));
    }

    #[test]
    fn crypt_parameters() {
        let keyring = "aes-xts-plain64 :64:logon:cryptsetup:96b2c399-8a5f-4204-aa55-b808aa90e365-d0 0 253:16 32768";
        assert_eq!(
            crypt_key_ref(keyring),
            Some(KeyRef {
                size: 64,
                ktype: "logon".into(),
                desc: "cryptsetup:96b2c399-8a5f-4204-aa55-b808aa90e365-d0".into()
            })
        );
        assert_eq!(crypt_device(keyring), Some((253, 16)));
        let inline = with_inline_key(keyring, &[0x00, 0xab, 0x1f]).unwrap();
        assert_eq!(inline.as_str(), "aes-xts-plain64 00ab1f 0 253:16 32768");
        assert_eq!(crypt_key_ref(&inline), None);
        // Optional parameters are kept as they are.
        let opts = "aes-xts-plain64 :32:logon:k 0 8:3 4096 2 allow_discards sector_size:4096";
        assert_eq!(
            with_inline_key(opts, &[1, 2]).unwrap().as_str(),
            "aes-xts-plain64 0102 0 8:3 4096 2 allow_discards sector_size:4096"
        );
        assert_eq!(crypt_key_ref("aes-cbc-essiv:sha256 00ff 0 8:3 0"), None);
        assert_eq!(crypt_key_ref("cipher - 0 8:3 0"), None);
        assert!(with_inline_key("too short", &[1]).is_none());
    }

    #[test]
    fn tables_that_can_be_copied() {
        let crypt = [Target::new(
            0,
            100,
            "crypt",
            "aes-xts-plain64 :64:logon:k 0 8:3 32768",
        )];
        assert_eq!(judge_table(&crypt, 100), TableVerdict::Copyable);
        assert_eq!(judge_table(&crypt, 101), TableVerdict::Mismatch);
        assert_eq!(judge_table(&[], 0), TableVerdict::Mismatch);
        let two = [
            Target::new(0, 50, "linear", "8:1 0"),
            Target::new(50, 50, "striped", "2 8 8:2 0 8:3 0"),
        ];
        assert_eq!(judge_table(&two, 100), TableVerdict::Copyable);
        let gap = [
            Target::new(0, 50, "linear", "8:1 0"),
            Target::new(60, 40, "linear", "8:1 50"),
        ];
        assert_eq!(judge_table(&gap, 100), TableVerdict::Mismatch);
        let left = [Target::new(0, 100, "snapshot-origin", "252:3")];
        assert_eq!(judge_table(&left, 100), TableVerdict::Leftover);
        let integrity = [
            Target::new(0, 50, "integrity", "8:3 0 4 J 0"),
            Target::new(50, 50, "thin", "252:9 1"),
        ];
        assert_eq!(
            judge_table(&integrity, 100),
            TableVerdict::Unsupported("integrity, thin".into())
        );
    }

    #[test]
    fn snapshot_status_lines() {
        assert_eq!(
            parse_snapshot_status("16/8388616 16"),
            SnapState::Valid {
                used: 16,
                total: 8388616
            }
        );
        assert!(parse_snapshot_status("16/8388616 16").intact());
        assert!(!parse_snapshot_status("8388616/8388616 16").intact());
        assert_eq!(parse_snapshot_status("Invalid"), SnapState::Invalid);
        assert!(!SnapState::Invalid.intact());
        assert_eq!(parse_snapshot_status("Overflow\n"), SnapState::Overflow);
        assert!(!parse_snapshot_status("Merge failed").intact());
        assert!(!parse_snapshot_status("").intact());
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn store_folder_placement() {
        // Origin dm-0 (LUKS on nvme0n1p3); dm-1 is an LV on it.
        let above = names(&["dm-0", "dm-1"]);
        let below = names(&["dm-0", "nvme0n1p3"]);
        // Another partition of the same disk is fine.
        assert_eq!(
            judge_cow_dir(
                &above,
                &below,
                Some("nvme0n1p4"),
                &names(&["nvme0n1p4"]),
                "ext4"
            ),
            CowPlace::Fine
        );
        // The origin itself (a folder on the backed-up root).
        assert_eq!(
            judge_cow_dir(
                &above,
                &below,
                Some("dm-0"),
                &names(&["dm-0", "nvme0n1p3"]),
                "btrfs"
            ),
            CowPlace::OnOrigin("dm-0".into())
        );
        // Something stacked on the origin.
        assert_eq!(
            judge_cow_dir(
                &above,
                &below,
                Some("dm-1"),
                &names(&["dm-1", "dm-0"]),
                "xfs"
            ),
            CowPlace::OnOrigin("dm-1".into())
        );
        // A device the origin is built on.
        assert_eq!(
            judge_cow_dir(
                &above,
                &below,
                Some("nvme0n1p3"),
                &names(&["nvme0n1p3"]),
                "ext4"
            ),
            CowPlace::OnOrigin("nvme0n1p3".into())
        );
        // A filesystem on a device stacked on the origin through other layers.
        assert_eq!(
            judge_cow_dir(
                &above,
                &below,
                Some("md0"),
                &names(&["md0", "dm-1"]),
                "ext4"
            ),
            CowPlace::OnOrigin("md0".into())
        );
        // No block device: memory and network filesystems are fine, others not.
        assert_eq!(
            judge_cow_dir(&above, &below, None, &[], "tmpfs"),
            CowPlace::Fine
        );
        assert_eq!(
            judge_cow_dir(&above, &below, None, &[], "nfs4"),
            CowPlace::Fine
        );
        assert_eq!(
            judge_cow_dir(&above, &below, None, &[], "overlay"),
            CowPlace::Unknown("overlay".into())
        );
        // A userspace filesystem is never in the origin's write path.
        assert_eq!(
            judge_cow_dir(&above, &below, Some("sdb1"), &names(&["sdb1"]), "fuseblk"),
            CowPlace::Unknown("fuseblk".into())
        );
    }

    #[test]
    fn mount_lookup() {
        let mounts = linux_block::parse_mountinfo(
            "26 1 0:24 /@ / rw - btrfs /dev/mapper/luks-1 rw\n\
             40 26 0:24 /@home /home rw - btrfs /dev/mapper/luks-1 rw\n\
             51 26 8:17 / /mnt/images rw - ext4 /dev/sdb1 rw\n\
             52 26 8:33 / /mnt/images rw - xfs /dev/sdc1 rw\n\
             53 26 8:49 / /mnt/img rw - ext4 /dev/sdd1 rw\n",
        );
        let at = |p: &str| mount_of(&mounts, Path::new(p)).map(|m| m.source.clone());
        assert_eq!(at("/home/user/x").as_deref(), Some("/dev/mapper/luks-1"));
        // Overmounted: the later mount wins.
        assert_eq!(at("/mnt/images/a").as_deref(), Some("/dev/sdc1"));
        // Whole components only: /mnt/imagesX is on /.
        assert_eq!(at("/mnt/imagesX").as_deref(), Some("/dev/mapper/luks-1"));
        assert_eq!(at("/mnt/img").as_deref(), Some("/dev/sdd1"));
    }

    fn entry(name: &str, uuid: &str, dev: (u32, u32), open: i32) -> DmEntry {
        DmEntry {
            name: name.into(),
            uuid: uuid.into(),
            dev,
            open_count: open,
            suspended: false,
            origin_of: None,
            pending_on: None,
            cow: None,
        }
    }

    #[test]
    fn orphan_plans() {
        let other = "fedcba9876543210fedcba9876543210";
        let mut origin = entry("luks-x", "CRYPT-LUKS2-abc-luks-x", (252, 0), 2);
        origin.origin_of = Some("252:5".into());
        let real = entry(
            &format!("obk-real-{ID}"),
            &format!("OBK-REAL-{ID}"),
            (252, 5),
            1,
        );
        let mut snap = entry(
            &format!("obk-snap-{ID}"),
            &format!("OBK-SNAP-{ID}"),
            (252, 6),
            0,
        );
        snap.cow = Some("7:0".into());
        // A live run: its snapshot is held open.
        let live_real = entry(
            &format!("obk-real-{other}"),
            &format!("OBK-REAL-{other}"),
            (252, 7),
            1,
        );
        let live_snap = entry(
            &format!("obk-snap-{other}"),
            &format!("OBK-SNAP-{other}"),
            (252, 8),
            1,
        );
        // Look-alikes that are not ours: an LVM snapshot LV, a name without our UUID.
        let lvm = entry("vg-obk--snap--x", "LVM-abc", (252, 9), 0);
        let fake = entry(&format!("obk-real-{ID}x"), "CRYPT-PLAIN-y", (252, 10), 0);
        // Killed with the switch of a second volume loaded but not made, and
        // that volume left suspended.
        let mut stuck = entry("luks-y", "CRYPT-LUKS2-def-luks-y", (252, 11), 1);
        stuck.pending_on = Some((252, 5));
        stuck.suspended = true;
        // A suspended volume of someone else's is not ours to resume.
        let mut foreign = entry("luks-z", "CRYPT-LUKS2-ghi-luks-z", (252, 12), 1);
        foreign.pending_on = Some((252, 99));
        foreign.suspended = true;
        let entries = vec![
            origin, real, snap, live_real, live_snap, lvm, fake, stuck, foreign,
        ];

        let plans = plan_orphans(&entries, None);
        assert_eq!(
            plans,
            vec![OrphanPlan {
                id: ID.into(),
                origins: vec!["luks-x".into()],
                pending: vec![("luks-y".into(), true)],
                snap: Some(format!("obk-snap-{ID}")),
                real: Some(format!("obk-real-{ID}")),
                cow: Some((7, 0)),
            }]
        );
        // Our own release plans its set even though it is held.
        let mine = plan_orphans(&entries, Some(other));
        assert_eq!(mine.len(), 1);
        assert_eq!(
            mine[0].snap.as_deref(),
            Some(format!("obk-snap-{other}").as_str())
        );
        assert!(mine[0].origins.is_empty());
    }

    #[test]
    fn half_made_sets_are_planned_too() {
        // Killed between creating obk-snap and obk-real, or after obk-snap
        // was already removed by an earlier cleanup.
        let snap_only = vec![entry(
            &format!("obk-snap-{ID}"),
            &format!("OBK-SNAP-{ID}"),
            (252, 6),
            0,
        )];
        let p = plan_orphans(&snap_only, None);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].real, None);
        let real_only = vec![entry(
            &format!("obk-real-{ID}"),
            &format!("OBK-REAL-{ID}"),
            (252, 5),
            0,
        )];
        let p = plan_orphans(&real_only, None);
        assert_eq!(p[0].snap, None);
        assert_eq!(
            p[0].real.as_deref(),
            Some(format!("obk-real-{ID}").as_str())
        );
    }

    #[test]
    fn store_size_without_sparse_files() {
        const G: u64 = 1 << 30;
        // A fifth of a large volume, when there is room for it.
        assert_eq!(dense_store_size(10 * G, 100 * G), 2 * G);
        // Never more than FAT holds in one file, nor half the free space.
        assert_eq!(dense_store_size(100 * G, 100 * G), 4 * G - 4096);
        assert_eq!(dense_store_size(10 * G, G), G / 2);
        // At least 256 MiB, but never more than the volume.
        assert_eq!(dense_store_size(G, 100 * G), 256 << 20);
        assert_eq!(dense_store_size(100 << 20, 100 * G), 100 << 20);
        // Chunk-aligned.
        assert_eq!(dense_store_size(10 * G, 3 * G + 1234) % 4096, 0);
    }

    #[test]
    fn unused_stores_are_swept_and_used_ones_kept() {
        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join(cow_file_name(ID));
        std::fs::write(&stale, b"").unwrap();
        let other = "fedcba9876543210fedcba9876543210";
        let (busy, held) = create_cow(dir.path(), other, 1 << 20).unwrap();
        // Sparse, and one chunk larger than the origin rounded to chunks.
        assert_eq!(held.metadata().unwrap().len(), (1 << 20) + 4096);
        let image = dir.path().join("job_full_20261019.vhdx");
        std::fs::write(&image, b"image").unwrap();
        let lookalike = dir.path().join(".own-backup-cow-notours.tmp");
        std::fs::write(&lookalike, b"").unwrap();

        assert_eq!(sweep_cow_files(dir.path()), 1);
        assert!(!stale.exists());
        assert!(busy.exists(), "a store its run holds stays");
        assert!(image.exists() && lookalike.exists());
        drop(held);
        // The lock lives on the open file description, and a process another
        // test forks at this moment holds a copy of the descriptor until its
        // exec closes it (O_CLOEXEC): the store is free a moment later, not
        // necessarily at once. The service's sweep runs again at its next
        // start; here it is asked again for a while.
        let mut swept = 0;
        for _ in 0..100 {
            swept = sweep_cow_files(dir.path());
            if swept > 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(swept, 1);
        assert!(!busy.exists());
        // A second store of the same name is never made over one in place.
        let (_p, _f) = create_cow(dir.path(), ID, 4096).unwrap();
        assert!(create_cow(dir.path(), ID, 4096).is_err());
    }
}
