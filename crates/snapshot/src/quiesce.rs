//! Holding a short plain partition still for the length of its own copy — how
//! `/boot` and the ESP of a running system without LVM are copied online
//! (decision 10 of the Linux plan).
//!
//! A plain partition has no snapshot short of a kernel module. A small one (at
//! most [`SHORT_LIMIT`]) is read in seconds, so its filesystem is held still
//! instead, for exactly that long:
//!
//! - frozen with `FIFREEZE` (ext2/3/4, XFS, btrfs, f2fs …): its writers wait,
//!   its readers do not; `FITHAW` right after its copy;
//! - remounted read-only where the filesystem has no freeze (FAT, exFAT), and
//!   read-write again after its copy. With a file open for writing on it the
//!   remount fails (`EBUSY`), and the copy is refused rather than read live.
//!
//! A frozen filesystem's writers block, this process included, and the thaw is
//! this process's own. So a partition qualifies only when none of its mount
//! points is `/` and none holds a place the program writes to meanwhile — its
//! data and log folders, the destination folder, `/tmp`, `/var`, `/run`,
//! `/home`, `/usr` ([`qualify`], [`protected_paths`]).
//!
//! A run that is killed leaves the filesystem frozen (or read-only). Before it
//! holds one it writes a marker into [`MARKER_DIR`] naming the mount and what
//! is done to it, and keeps the marker locked (`flock`) until it has undone
//! it: a marker nobody holds is a dead run's, and [`undo_leftovers`] — at
//! service start and in `own-backup-cli cleanup-snapshots` — thaws or
//! remounts what it names.
//!
//! The decision, the remount flags and the marker format are pure and
//! unit-tested; the ioctls and mount(2) need root and are exercised on the
//! Linux stand (`scripts/lab/linux-shortfreeze-scenario.sh`).

#![cfg(target_os = "linux")]

use std::ffi::CString;
use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

use tracing::{info, warn};
use uuid::Uuid;

use own_backup_common::config::AppConfig;
use own_backup_common::linux_block::{self, Usage, SYS_CLASS_BLOCK};
use own_backup_common::{log_files, tr, OwnBackupError, Result};

/// Largest partition held still for its copy.
pub const SHORT_LIMIT: u64 = 4 << 30;

/// Where a run notes what it froze or remounted: one marker per hold.
pub const MARKER_DIR: &str = "/run/own-backup/frozen";

/// Places written to by others than this program that a held partition's
/// mount points must not hold either (besides the program's own, see
/// [`protected_paths`]).
const ALWAYS_PROTECTED: [&str; 5] = ["/tmp", "/var", "/run", "/home", "/usr"];

/// Filesystems `FIFREEZE` holds still.
const FREEZABLE: [&str; 8] = [
    "ext2", "ext3", "ext4", "xfs", "btrfs", "f2fs", "jfs", "nilfs2",
];
/// Filesystems without a freeze, remounted read-only for the copy instead.
const REMOUNTABLE: [&str; 3] = ["vfat", "msdos", "exfat"];

// `_IOWR('X', 119, int)` and `_IOWR('X', 120, int)` from <linux/fs.h>.
const FIFREEZE: libc::c_ulong = 0xC004_5877;
const FITHAW: libc::c_ulong = 0xC004_5878;

const MARKER_SUFFIX: &str = ".marker";
const NEW_SUFFIX: &str = ".new";
const MARKER_HEADER: &str = "own-backup-hold 1";

/// How a partition is held still.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Freeze,
    ReadOnlyRemount,
}

impl Method {
    /// The snapshot's `mode`, and the marker's `method`.
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Freeze => "freeze",
            Method::ReadOnlyRemount => "readonly-remount",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "freeze" => Some(Method::Freeze),
            "readonly-remount" => Some(Method::ReadOnlyRemount),
            _ => None,
        }
    }
}

/// How a filesystem type is held still, if it can be.
pub(crate) fn method_for(fstype: &str) -> Option<Method> {
    if FREEZABLE.contains(&fstype) {
        Some(Method::Freeze)
    } else if REMOUNTABLE.contains(&fstype) {
        Some(Method::ReadOnlyRemount)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Mounts (pure)
// ---------------------------------------------------------------------------

/// One line of `/proc/self/mountinfo`, with the options a remount must keep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub dev: (u32, u32),
    /// What of the filesystem is mounted: `/`, or a directory (a bind mount).
    pub root: String,
    pub mount_point: String,
    /// Per-mount options: `rw,nosuid,nodev,relatime`.
    pub options: String,
    pub fstype: String,
    pub source: String,
    /// Options of the filesystem itself: `rw,fmask=0077,…`.
    pub super_options: String,
}

pub(crate) fn parse_mounts(text: &str) -> Vec<Mount> {
    text.lines().filter_map(parse_mount_line).collect()
}

/// `id parent maj:min root mount-point options [optional…] - fstype source
/// super-options`.
fn parse_mount_line(line: &str) -> Option<Mount> {
    let f: Vec<&str> = line.split(' ').collect();
    let dash = f.iter().position(|x| *x == "-")?;
    if dash < 6 || f.len() < dash + 3 {
        return None;
    }
    let (major, minor) = f[2].split_once(':')?;
    Some(Mount {
        dev: (major.parse().ok()?, minor.parse().ok()?),
        root: linux_block::unescape(f[3]),
        mount_point: linux_block::unescape(f[4]),
        options: f[5].to_string(),
        fstype: f[dash + 1].to_string(),
        source: linux_block::unescape(f[dash + 2]),
        super_options: f.get(dash + 3).copied().unwrap_or_default().to_string(),
    })
}

/// The mounts of the device `kernel` (`dev`): by device number, or — btrfs
/// reports an anonymous one — by resolving the mount source to a kernel name.
pub(crate) fn mounts_of(
    all: &[Mount],
    dev: Option<(u32, u32)>,
    kernel: &str,
    resolve: impl Fn(&str) -> Option<String>,
) -> Vec<Mount> {
    all.iter()
        .filter(|m| {
            (m.dev.0 != 0 && Some(m.dev) == dev)
                || (m.source.starts_with('/') && resolve(&m.source).as_deref() == Some(kernel))
        })
        .cloned()
        .collect()
}

fn has_option(options: &str, name: &str) -> bool {
    options.split(',').any(|o| o == name)
}

/// The `mount(2)` flags that remount a mount as it is — its per-mount options
/// and the filesystem flags a remount resets. Without `MS_RDONLY`: that is
/// added for the read-only remount and left out for the way back.
pub(crate) fn remount_flags(options: &str, super_options: &str) -> libc::c_ulong {
    let mut flags: libc::c_ulong = 0;
    for o in options.split(',') {
        flags |= match o {
            "nosuid" => libc::MS_NOSUID,
            "nodev" => libc::MS_NODEV,
            "noexec" => libc::MS_NOEXEC,
            "noatime" => libc::MS_NOATIME,
            "nodiratime" => libc::MS_NODIRATIME,
            "relatime" => libc::MS_RELATIME,
            "nosymfollow" => libc::MS_NOSYMFOLLOW,
            _ => 0,
        };
    }
    // Neither noatime nor relatime: the mount updates every access time.
    if flags & (libc::MS_NOATIME | libc::MS_RELATIME) == 0 {
        flags |= libc::MS_STRICTATIME;
    }
    for o in super_options.split(',') {
        flags |= match o {
            "sync" => libc::MS_SYNCHRONOUS,
            "dirsync" => libc::MS_DIRSYNC,
            "lazytime" => libc::MS_LAZYTIME,
            "mand" => libc::MS_MANDLOCK,
            _ => 0,
        };
    }
    flags
}

// ---------------------------------------------------------------------------
// The rule (pure)
// ---------------------------------------------------------------------------

/// What [`qualify`] needs to know about a busy plain partition.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facts {
    /// Bytes, from sysfs.
    pub size: Option<u64>,
    /// Its mounts in this mount namespace.
    pub mounts: Vec<Mount>,
}

/// How a partition is held still for its copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub method: Method,
    /// The mount it is held through.
    pub mount_point: String,
    /// Every mount point of its filesystem.
    pub mount_points: Vec<String>,
    pub fstype: String,
    /// For a remount: its flags (see [`remount_flags`]).
    pub flags: libc::c_ulong,
}

/// Why a busy plain partition is not held still.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotShort {
    Swap,
    /// Devices stacked on it (an opened LUKS container, an LVM PV).
    Holders(Vec<String>),
    /// Mounted read-write, but its mount was not found.
    NoMount,
    Root,
    /// Without a destination folder nobody can tell where the copy goes.
    NoDestination,
    /// One of its mount points is (or is above) a place written meanwhile.
    Holds {
        mount_point: String,
        path: PathBuf,
    },
    TooBig(u64),
    SizeUnknown,
    /// A filesystem that can be neither frozen nor remounted read-only.
    Filesystem(String),
}

/// Does mount point `mount_point` hold `path` — is it `path` or above it?
pub(crate) fn holds(mount_point: &str, path: &Path) -> bool {
    path.starts_with(mount_point)
}

/// The rule of decision 10: a busy plain partition is held still when its
/// only users are mounts, it is short, none of its mount points is `/` and
/// none holds a `protected` place (`None`: the destination is not known), and
/// its filesystem can be frozen or remounted read-only.
pub(crate) fn qualify(
    usage: &Usage,
    facts: &Facts,
    protected: Option<&[PathBuf]>,
) -> std::result::Result<Plan, NotShort> {
    if !usage.swap.is_empty() {
        return Err(NotShort::Swap);
    }
    if !usage.holders.is_empty() {
        return Err(NotShort::Holders(usage.holders.clone()));
    }
    let Some(first) = facts.mounts.first() else {
        return Err(NotShort::NoMount);
    };
    if facts.mounts.iter().any(|m| m.mount_point == "/") {
        return Err(NotShort::Root);
    }
    let Some(protected) = protected else {
        return Err(NotShort::NoDestination);
    };
    for m in &facts.mounts {
        if let Some(p) = protected.iter().find(|p| holds(&m.mount_point, p)) {
            return Err(NotShort::Holds {
                mount_point: m.mount_point.clone(),
                path: p.clone(),
            });
        }
    }
    match facts.size {
        None => return Err(NotShort::SizeUnknown),
        Some(size) if size > SHORT_LIMIT => return Err(NotShort::TooBig(size)),
        Some(_) => {}
    }
    let method =
        method_for(&first.fstype).ok_or_else(|| NotShort::Filesystem(first.fstype.clone()))?;
    // Through a read-write mount of the whole filesystem where there is one.
    let via = facts
        .mounts
        .iter()
        .filter(|m| has_option(&m.options, "rw"))
        .min_by_key(|m| m.root != "/")
        .unwrap_or(first);
    let mut mount_points: Vec<String> =
        facts.mounts.iter().map(|m| m.mount_point.clone()).collect();
    mount_points.dedup();
    Ok(Plan {
        method,
        mount_point: via.mount_point.clone(),
        mount_points,
        fstype: via.fstype.clone(),
        flags: remount_flags(&via.options, &via.super_options),
    })
}

// ---------------------------------------------------------------------------
// What the program writes to during a copy
// ---------------------------------------------------------------------------

/// `path` made absolute against `cwd`, with `.` and `..` resolved lexically.
pub(crate) fn absolute(path: &Path, cwd: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let mut out = PathBuf::from("/");
    for c in joined.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(n) => out.push(n),
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    out
}

/// An absolute path with its symbolic links resolved as far as it exists:
/// the nearest existing ancestor canonicalized, the rest appended.
pub(crate) fn resolved(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut rest = Vec::new();
    loop {
        if let Ok(real) = std::fs::canonicalize(existing) {
            let mut out = real;
            for name in rest.iter().rev() {
                out.push(name);
            }
            return out;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                existing = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// The places this program writes to while a partition is held for a copy
/// into `dest`: the destination folder, the data and log folders (and the log
/// mirror), the marker folder, temp, a file its standard output or error goes
/// to — and `/tmp`, `/var`, `/run`, `/home`, `/usr`. Each as given and with its
/// links resolved.
pub fn protected_paths(dest: &Path) -> Vec<PathBuf> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    protected_from(dest, &program_paths(), &cwd)
}

/// [`protected_paths`] from the program's own places, each made absolute
/// against `cwd` and also with its links resolved.
pub(crate) fn protected_from(dest: &Path, program: &[PathBuf], cwd: &Path) -> Vec<PathBuf> {
    let mut raw: Vec<PathBuf> = ALWAYS_PROTECTED.iter().map(PathBuf::from).collect();
    raw.push(dest.to_path_buf());
    raw.push(PathBuf::from(MARKER_DIR));
    raw.push(PathBuf::from(log_files::LINUX_SYSTEM_LOG_DIR));
    raw.extend(program.iter().cloned());
    let mut out: Vec<PathBuf> = Vec::new();
    for p in raw {
        let abs = absolute(&p, cwd);
        let real = resolved(&abs);
        for q in [abs, real] {
            if !out.contains(&q) {
                out.push(q);
            }
        }
    }
    out
}

/// Where this program writes besides the destination: temp, the data and
/// log folders of the default and the loaded configuration, the log mirror,
/// and a file its standard output or error is redirected to.
fn program_paths() -> Vec<PathBuf> {
    let mut raw = vec![std::env::temp_dir()];
    let defaults = AppConfig::default();
    let loaded = AppConfig::load_or_default();
    for cfg in [&defaults, &loaded] {
        raw.push(cfg.data_dir.clone());
        if let Some(dir) = log_files::log_dir(&cfg.data_dir) {
            raw.push(dir);
        }
        let mirror = cfg.log_mirror_dir.trim();
        if !mirror.is_empty() {
            raw.push(PathBuf::from(mirror));
        }
    }
    // Output redirected into a file: a write to it would wait for a thaw
    // that only this process can do.
    for fd in [1, 2] {
        if let Ok(target) = std::fs::read_link(format!("/proc/self/fd/{fd}")) {
            if target.is_absolute() && target.is_file() {
                raw.push(target);
            }
        }
    }
    raw
}

/// What [`qualify`] needs to know about the device `kernel` (`sdb1`); `None`
/// unless it is a partition and not a device-mapper device.
pub(crate) fn facts_of(kernel: &str) -> io::Result<Option<Facts>> {
    let sys = Path::new(SYS_CLASS_BLOCK);
    if kernel.starts_with("dm-") || !linux_block::is_partition(sys, kernel) {
        return Ok(None);
    }
    let size = std::fs::read_to_string(sys.join(kernel).join("size"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|sectors| sectors * 512);
    let all = parse_mounts(&std::fs::read_to_string("/proc/self/mountinfo")?);
    let mounts = mounts_of(&all, linux_block::dev_numbers(sys, kernel), kernel, |p| {
        linux_block::kernel_name(Path::new(p))
    });
    Ok(Some(Facts { size, mounts }))
}

// ---------------------------------------------------------------------------
// Markers
// ---------------------------------------------------------------------------

/// What a run holds, as its marker says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Marker {
    pub method: Method,
    pub volume: String,
    pub mount_point: String,
    /// `st_dev` of the mount's root when it was held.
    pub fs_dev: (u32, u32),
    /// For a remount: the flags it goes back to read-write with.
    pub flags: libc::c_ulong,
    pub pid: u32,
    pub created: String,
}

/// A value on one line: `\` and newline as the kernel escapes them.
fn escape(s: &str) -> String {
    s.replace('\\', "\\134").replace('\n', "\\012")
}

impl Marker {
    pub fn to_text(&self) -> String {
        format!(
            "{MARKER_HEADER}\nmethod={}\nvolume={}\nmount_point={}\nfs_dev={}:{}\nflags={}\npid={}\ncreated={}\n",
            self.method.as_str(),
            escape(&self.volume),
            escape(&self.mount_point),
            self.fs_dev.0,
            self.fs_dev.1,
            self.flags,
            self.pid,
            escape(&self.created),
        )
    }

    pub fn parse(text: &str) -> Option<Marker> {
        let mut lines = text.lines();
        if lines.next()? != MARKER_HEADER {
            return None;
        }
        let mut get = std::collections::HashMap::new();
        for l in lines {
            let (k, v) = l.split_once('=')?;
            get.insert(k, linux_block::unescape(v));
        }
        let (major, minor) = get.get("fs_dev")?.split_once(':')?;
        let mount_point = get.get("mount_point")?.clone();
        if !mount_point.starts_with('/') {
            return None;
        }
        Some(Marker {
            method: Method::parse(get.get("method")?)?,
            volume: get.get("volume")?.clone(),
            mount_point,
            fs_dev: (major.parse().ok()?, minor.parse().ok()?),
            flags: get.get("flags")?.parse().ok()?,
            pid: get.get("pid")?.parse().ok()?,
            created: get.get("created").cloned().unwrap_or_default(),
        })
    }
}

fn is_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `<id>.marker`, a finished marker.
fn marker_id(name: &str) -> Option<&str> {
    name.strip_suffix(MARKER_SUFFIX).filter(|id| is_id(id))
}

/// `.<id>.new`, a marker still being written — before anything is held.
fn new_marker_id(name: &str) -> Option<&str> {
    name.strip_prefix('.')
        .and_then(|n| n.strip_suffix(NEW_SUFFIX))
        .filter(|id| is_id(id))
}

/// Only this user may create names in the marker folder: it is acted on as
/// root.
fn trusted_dir(dir: &Path) -> io::Result<()> {
    let meta = std::fs::symlink_metadata(dir)?;
    // SAFETY: geteuid has no preconditions.
    let me = unsafe { libc::geteuid() };
    if !meta.is_dir() || meta.uid() != me || meta.mode() & 0o022 != 0 {
        return Err(io::Error::other(format!(
            "{} is not a private directory of this user",
            dir.display()
        )));
    }
    Ok(())
}

fn ensure_marker_dir(dir: &Path) -> io::Result<()> {
    if let Some(parent) = dir.parent() {
        DirBuilder::new()
            .recursive(true)
            .mode(0o750)
            .create(parent)?;
    }
    match DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    trusted_dir(dir)
}

fn flock_now(file: &File) -> io::Result<()> {
    // SAFETY: flock on an open descriptor.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Write marker `id` into `dir`, locked: written in full under a temporary
/// name and only then given its own, so a marker nobody holds is complete.
pub(crate) fn write_marker(dir: &Path, id: &str, marker: &Marker) -> io::Result<(PathBuf, File)> {
    ensure_marker_dir(dir)?;
    let new = dir.join(format!(".{id}{NEW_SUFFIX}"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&new)?;
    let path = dir.join(format!("{id}{MARKER_SUFFIX}"));
    let written = (|| {
        flock_now(&file)?;
        file.write_all(marker.to_text().as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&new, &path)
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&new);
        return Err(e);
    }
    Ok((path, file))
}

/// Open `path` and lock it if no live run holds it; `None` if one does (or it
/// cannot be opened).
fn lock_if_unused(path: &Path) -> Option<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return None;
    }
    flock_now(&file).ok()?;
    Some(file)
}

/// Is the locked `file` still what `path` names? A run that releases its hold
/// removes the marker before letting go of the lock.
fn still_named(file: &File, path: &Path) -> bool {
    match (file.metadata(), std::fs::symlink_metadata(path)) {
        (Ok(f), Ok(p)) => f.nlink() > 0 && f.ino() == p.ino() && f.dev() == p.dev(),
        _ => false,
    }
}

/// What became of one leftover marker.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Thawed, or remounted read-write.
    Undone,
    /// Nothing of it was held any more (never frozen, thawed by someone else,
    /// not mounted there any more).
    Nothing,
}

/// Undo the holds of dead runs that `dir` has markers of, through `undo`
/// (which acts on the system). A marker some live run holds is left alone; a
/// marker undone or naming nothing held is deleted; one whose undo failed is
/// kept for the next cleanup. Returns how many holds were undone.
pub(crate) fn undo_leftovers_in(
    dir: &Path,
    undo: impl Fn(&Marker) -> io::Result<Outcome>,
) -> usize {
    if trusted_dir(dir).is_err() {
        return 0; // no marker folder (nothing was ever held), or not ours
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut undone = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let path = entry.path();
        if new_marker_id(name).is_some() {
            // Killed while writing its marker: nothing was held yet.
            if let Some(file) = lock_if_unused(&path) {
                if still_named(&file, &path) {
                    let _ = std::fs::remove_file(&path);
                }
            }
            continue;
        }
        if marker_id(name).is_none() {
            continue;
        }
        let Some(mut file) = lock_if_unused(&path) else {
            continue; // its run is alive
        };
        if !still_named(&file, &path) {
            continue; // released meanwhile
        }
        let mut text = String::new();
        let marker = file
            .read_to_string(&mut text)
            .ok()
            .and_then(|_| Marker::parse(&text));
        let Some(marker) = marker else {
            warn!(marker = %path.display(), "an unreadable hold marker removed");
            let _ = std::fs::remove_file(&path);
            continue;
        };
        match undo(&marker) {
            Ok(outcome) => {
                if outcome == Outcome::Undone {
                    info!(
                        volume = %marker.volume, mount_point = %marker.mount_point,
                        method = %marker.method.as_str(), pid = marker.pid,
                        "a partition held still by an interrupted run is released"
                    );
                    undone += 1;
                } else {
                    info!(
                        volume = %marker.volume, mount_point = %marker.mount_point,
                        method = %marker.method.as_str(),
                        "an interrupted run's hold marker named nothing held any more"
                    );
                }
                let _ = std::fs::remove_file(&path);
            }
            Err(e) => warn!(
                volume = %marker.volume, mount_point = %marker.mount_point,
                method = %marker.method.as_str(), error = %e,
                "cannot release a partition an interrupted run held still; its marker is kept"
            ),
        }
    }
    undone
}

/// Thaw (or remount read-write) what killed runs left frozen (or read-only).
/// Blocking. Returns how many were released.
pub fn undo_leftovers() -> usize {
    undo_leftovers_in(Path::new(MARKER_DIR), undo_marker)
}

/// Undo one dead run's hold on the live system.
fn undo_marker(marker: &Marker) -> io::Result<Outcome> {
    let dir = match open_dir(&marker.mount_point) {
        Ok(d) => d,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Outcome::Nothing),
        Err(e) => return Err(e),
    };
    if linux_block::split_dev(dir.metadata()?.dev()) != marker.fs_dev {
        warn!(
            mount_point = %marker.mount_point,
            "the filesystem an interrupted run held is not mounted there any more"
        );
        return Ok(Outcome::Nothing);
    }
    match marker.method {
        Method::Freeze => match ioctl(&dir, FITHAW) {
            Ok(()) => Ok(Outcome::Undone),
            // Not frozen: killed before the freeze, or thawed since.
            Err(e) if e.raw_os_error() == Some(libc::EINVAL) => Ok(Outcome::Nothing),
            Err(e) => Err(e),
        },
        Method::ReadOnlyRemount => {
            if !read_only(&dir)? {
                return Ok(Outcome::Nothing);
            }
            remount(&dir, marker.flags)?;
            Ok(Outcome::Undone)
        }
    }
}

// ---------------------------------------------------------------------------
// The system calls
// ---------------------------------------------------------------------------

fn open_dir(path: &str) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(path)
}

fn ioctl(dir: &File, request: libc::c_ulong) -> io::Result<()> {
    // SAFETY: FIFREEZE/FITHAW take an unused integer argument.
    if unsafe { libc::ioctl(dir.as_raw_fd(), request, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Remount the mount whose root `dir` is with `flags` (plus `MS_REMOUNT`) —
/// through `/proc/self/fd`, so it is that mount even if another was mounted
/// over its mount point meanwhile.
fn remount(dir: &File, flags: libc::c_ulong) -> io::Result<()> {
    let target =
        CString::new(format!("/proc/self/fd/{}", dir.as_raw_fd())).map_err(io::Error::other)?;
    let flags = libc::MS_REMOUNT | flags;
    // SAFETY: mount(2) with a valid path and no source, type or data.
    let rc = unsafe {
        libc::mount(
            std::ptr::null(),
            target.as_ptr(),
            std::ptr::null(),
            flags,
            std::ptr::null(),
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn read_only(dir: &File) -> io::Result<bool> {
    // SAFETY: fstatvfs into a zeroed struct.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatvfs(dir.as_raw_fd(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st.f_flag & libc::ST_RDONLY != 0)
}

/// Push what the filesystem has cached out before it is held, while its
/// writers still run: the freeze (or remount) then has little left to write.
fn sync_fs(dir: &File) {
    // SAFETY: syncfs on an open descriptor.
    unsafe { libc::syncfs(dir.as_raw_fd()) };
}

/// Drop the device's own page cache. File data reaches the disk without
/// passing through it, so a page of it cached from an earlier read of the
/// device (an earlier copy) may be stale; the copy must read the disk.
/// Metadata the filesystem keeps there and still uses stays, and is current.
fn drop_cache(volume: &str) {
    if let Ok(dev) = File::open(volume) {
        // SAFETY: posix_fadvise on an open descriptor.
        unsafe { libc::posix_fadvise(dev.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
    }
}

/// Is a filesystem still held as `method` holds it? A freeze is asked by
/// freezing again: `EBUSY` while it is frozen. Had it been thawed, that froze
/// it again — and it is thawed at once.
fn held_still(dir: &File, method: Method) -> io::Result<bool> {
    match method {
        Method::Freeze => match ioctl(dir, FIFREEZE) {
            Err(e) if e.raw_os_error() == Some(libc::EBUSY) => Ok(true),
            Ok(()) => {
                let _ = ioctl(dir, FITHAW);
                Ok(false)
            }
            Err(e) => Err(e),
        },
        Method::ReadOnlyRemount => read_only(dir),
    }
}

/// [`Hold::still_held`] without the hold: by the mount point and the
/// filesystem's `st_dev` the snapshot recorded.
pub fn still_held_at(method: Method, mount_point: &str, fs_dev: (u32, u32)) -> io::Result<bool> {
    let dir = open_dir(mount_point)?;
    if linux_block::split_dev(dir.metadata()?.dev()) != fs_dev {
        return Err(io::Error::other(format!(
            "{mount_point} is not the filesystem that was held"
        )));
    }
    held_still(&dir, method)
}

// ---------------------------------------------------------------------------
// A hold
// ---------------------------------------------------------------------------

/// A partition held still by this process. Released by [`Hold::release`]; a
/// hold dropped without it (a panic, a dropped task) is released on drop.
pub struct Hold {
    pub id: String,
    volume: String,
    method: Method,
    mount_point: String,
    fs_dev: (u32, u32),
    fstype: String,
    flags: libc::c_ulong,
    /// The mount's root, open: what the freeze, the thaw and the remount act on.
    dir: File,
    /// The marker and its lock, until the hold is undone.
    marker: Option<(PathBuf, File)>,
    since: Instant,
    held: bool,
}

fn refuse(msg: impl Into<String>) -> OwnBackupError {
    OwnBackupError::NoSnapshotProvider(msg.into())
}

fn bad(msg: impl std::fmt::Display) -> OwnBackupError {
    OwnBackupError::Device(format!("partition hold: {msg}"))
}

/// Hold `volume` still as `plan` says, its marker in `marker_dir`. Blocking.
pub(crate) fn take(volume: &str, plan: &Plan, marker_dir: &Path) -> Result<Hold> {
    let failed = |e: &dyn std::fmt::Display| {
        refuse(tr!(
            "linux.hold_failed",
            volume = volume,
            mount = plan.mount_point,
            error = e
        ))
    };
    let dir = open_dir(&plan.mount_point).map_err(|e| failed(&e))?;
    let fs_dev = linux_block::split_dev(dir.metadata().map_err(|e| failed(&e))?.dev());
    let id = Uuid::new_v4().simple().to_string();
    let marker = Marker {
        method: plan.method,
        volume: volume.to_string(),
        mount_point: plan.mount_point.clone(),
        fs_dev,
        flags: plan.flags,
        pid: std::process::id(),
        created: chrono::Utc::now().to_rfc3339(),
    };
    let marker = write_marker(marker_dir, &id, &marker).map_err(|e| {
        refuse(tr!(
            "linux.hold_marker_failed",
            volume = volume,
            dir = marker_dir.display(),
            error = e
        ))
    })?;
    let mut hold = Hold {
        id,
        volume: volume.to_string(),
        method: plan.method,
        mount_point: plan.mount_point.clone(),
        fs_dev,
        fstype: plan.fstype.clone(),
        flags: plan.flags,
        dir,
        marker: Some(marker),
        since: Instant::now(),
        held: false,
    };
    sync_fs(&hold.dir);
    let took = match plan.method {
        Method::Freeze => ioctl(&hold.dir, FIFREEZE),
        Method::ReadOnlyRemount => remount(&hold.dir, plan.flags | libc::MS_RDONLY),
    };
    if let Err(e) = took {
        // Dropped unheld: its marker goes with it.
        return Err(match (plan.method, e.raw_os_error()) {
            (Method::Freeze, Some(libc::EBUSY)) => refuse(tr!(
                "linux.freeze_busy",
                volume = volume,
                mount = plan.mount_point
            )),
            (Method::ReadOnlyRemount, Some(libc::EBUSY)) => refuse(tr!(
                "linux.readonly_busy",
                volume = volume,
                mount = plan.mount_point,
                fstype = plan.fstype
            )),
            _ => failed(&e),
        });
    }
    hold.held = true;
    hold.since = Instant::now();
    drop_cache(volume);
    info!(
        volume = %volume, mount_point = %plan.mount_point, fstype = %plan.fstype,
        mode = %plan.method.as_str(), mounts = ?plan.mount_points,
        "partition held still for its copy (if this run is killed, `own-backup-cli \
         cleanup-snapshots` releases it)"
    );
    Ok(hold)
}

impl Hold {
    pub fn method(&self) -> Method {
        self.method
    }

    pub fn mount_point(&self) -> &str {
        &self.mount_point
    }

    pub fn fs_dev(&self) -> (u32, u32) {
        self.fs_dev
    }

    pub fn fstype(&self) -> &str {
        &self.fstype
    }

    /// Is the partition still held — not thawed, not remounted read-write by
    /// someone else? A copy read after that is not one point in time.
    pub fn still_held(&self) -> io::Result<bool> {
        held_still(&self.dir, self.method)
    }

    /// Thaw it (or remount it read-write) and remove the marker. If that
    /// fails the marker stays, unlocked once this hold is gone, for the next
    /// cleanup.
    pub fn release(mut self) -> Result<()> {
        match self.undo() {
            Ok(()) => {
                self.drop_marker();
                Ok(())
            }
            Err(e) => {
                // The marker stays for the next cleanup, unlocked as its file
                // closes here; dropping the hold must not try again.
                self.held = false;
                self.marker = None;
                Err(bad(format!(
                    "{} ({}): cannot {}: {e}; `own-backup-cli cleanup-snapshots` retries it",
                    self.volume,
                    self.mount_point,
                    match self.method {
                        Method::Freeze => "thaw it",
                        Method::ReadOnlyRemount => "remount it read-write",
                    }
                )))
            }
        }
    }

    fn undo(&mut self) -> io::Result<()> {
        if !self.held {
            return Ok(());
        }
        // Nothing of what the copy cached is to be trusted once it runs again.
        drop_cache(&self.volume);
        match self.method {
            Method::Freeze => match ioctl(&self.dir, FITHAW) {
                // Already thawed by someone else (check_intact said so).
                Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {}
                other => other?,
            },
            Method::ReadOnlyRemount => remount(&self.dir, self.flags)?,
        }
        self.held = false;
        info!(
            volume = %self.volume, mount_point = %self.mount_point,
            mode = %self.method.as_str(), held_ms = self.since.elapsed().as_millis() as u64,
            "partition released after its copy"
        );
        Ok(())
    }

    /// Remove the marker, then let go of its lock — in that order, so a
    /// cleanup that gets the lock finds no name to act on.
    fn drop_marker(&mut self) {
        if let Some((path, file)) = self.marker.take() {
            if let Err(e) = std::fs::remove_file(&path) {
                warn!(marker = %path.display(), error = %e, "cannot remove a hold marker");
            }
            drop(file);
        }
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        if self.held {
            warn!(
                volume = %self.volume, mount_point = %self.mount_point,
                "a partition held for a copy is released without the copy's release"
            );
            if let Err(e) = self.undo() {
                warn!(
                    volume = %self.volume, mount_point = %self.mount_point, error = %e,
                    "cannot release a partition held still; `own-backup-cli cleanup-snapshots` \
                     retries it"
                );
                return; // the marker stays for the cleanup
            }
        }
        self.drop_marker();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNTINFO: &str = "\
26 1 0:24 /@ / rw,noatime shared:1 - btrfs /dev/mapper/luks-16e1 rw,ssd,subvol=/@
27 26 259:2 / /boot rw,relatime shared:2 - ext4 /dev/nvme0n1p2 rw
28 27 259:1 / /boot/efi rw,nosuid,nodev,relatime shared:3 - vfat /dev/nvme0n1p1 rw,fmask=0077,dmask=0077,codepage=437
29 26 8:17 / /mnt/Work\\040Disk rw,noexec - exfat /dev/sdb1 rw,sync,lazytime
30 26 0:51 / /srv/pool rw,relatime - btrfs /dev/sdc1 rw,space_cache=v2
31 26 259:2 /grub /mnt/grub-bind ro,relatime - ext4 /dev/nvme0n1p2 rw
";

    fn mount(mp: &str, fstype: &str, options: &str, root: &str) -> Mount {
        Mount {
            dev: (8, 1),
            root: root.into(),
            mount_point: mp.into(),
            options: options.into(),
            fstype: fstype.into(),
            source: "/dev/sda1".into(),
            super_options: "rw".into(),
        }
    }

    fn busy(mounts: &[&str]) -> Usage {
        Usage {
            mounted_rw: mounts.iter().map(|m| m.to_string()).collect(),
            ..Usage::default()
        }
    }

    fn protected(dest: &str) -> Vec<PathBuf> {
        let mut p: Vec<PathBuf> = ALWAYS_PROTECTED.iter().map(PathBuf::from).collect();
        p.extend(
            [
                dest,
                MARKER_DIR,
                "/var/lib/own-backup",
                "/var/log/own-backup",
            ]
            .map(PathBuf::from),
        );
        p
    }

    #[test]
    fn mountinfo_with_options() {
        let m = parse_mounts(MOUNTINFO);
        assert_eq!(m.len(), 6);
        assert_eq!(m[2].mount_point, "/boot/efi");
        assert_eq!(m[2].options, "rw,nosuid,nodev,relatime");
        assert_eq!(m[2].super_options, "rw,fmask=0077,dmask=0077,codepage=437");
        assert_eq!(m[2].dev, (259, 1));
        assert_eq!(m[3].mount_point, "/mnt/Work Disk");
        assert_eq!(m[3].fstype, "exfat");
        assert_eq!(m[5].root, "/grub");
        // No super options at all is still a mount.
        let bare = parse_mounts("30 1 8:1 / /data rw - xfs /dev/sda1\n");
        assert_eq!(bare[0].super_options, "");
        assert!(parse_mounts("garbage\n").is_empty());
    }

    #[test]
    fn mounts_of_a_device_by_number_or_source() {
        let all = parse_mounts(MOUNTINFO);
        let boot = mounts_of(&all, Some((259, 2)), "nvme0n1p2", |_| None);
        assert_eq!(
            boot.iter()
                .map(|m| m.mount_point.as_str())
                .collect::<Vec<_>>(),
            vec!["/boot", "/mnt/grub-bind"]
        );
        // btrfs reports an anonymous device: found by its source.
        let pool = mounts_of(&all, Some((8, 33)), "sdc1", |p| {
            (p == "/dev/sdc1").then(|| "sdc1".to_string())
        });
        assert_eq!(pool.len(), 1);
        assert_eq!(pool[0].mount_point, "/srv/pool");
        // An anonymous number never matches by number.
        assert!(mounts_of(&all, Some((0, 24)), "dm-0", |_| None).is_empty());
    }

    #[test]
    fn remount_keeps_what_the_mount_has() {
        let f = remount_flags("rw,nosuid,nodev,relatime", "rw,fmask=0077");
        assert_eq!(f, libc::MS_NOSUID | libc::MS_NODEV | libc::MS_RELATIME);
        let f = remount_flags(
            "rw,noexec,noatime,nodiratime,nosymfollow",
            "rw,sync,dirsync,lazytime",
        );
        assert_eq!(
            f,
            libc::MS_NOEXEC
                | libc::MS_NOATIME
                | libc::MS_NODIRATIME
                | libc::MS_NOSYMFOLLOW
                | libc::MS_SYNCHRONOUS
                | libc::MS_DIRSYNC
                | libc::MS_LAZYTIME
        );
        // Neither noatime nor relatime: strictatime, said so explicitly.
        assert_eq!(remount_flags("rw", "rw"), libc::MS_STRICTATIME);
        // Never read-only by itself: that is added for the way there only.
        assert_eq!(remount_flags("ro,relatime", "ro") & libc::MS_RDONLY, 0);
    }

    #[test]
    fn short_boot_and_esp_are_held() {
        let all = parse_mounts(MOUNTINFO);
        let facts = Facts {
            size: Some(2 << 30),
            mounts: mounts_of(&all, Some((259, 2)), "nvme0n1p2", |_| None),
        };
        let plan = qualify(&busy(&["/boot"]), &facts, Some(&protected("/mnt/backup"))).unwrap();
        assert_eq!(plan.method, Method::Freeze);
        // Through the read-write mount of the whole filesystem, not the bind.
        assert_eq!(plan.mount_point, "/boot");
        assert_eq!(plan.mount_points, vec!["/boot", "/mnt/grub-bind"]);

        let facts = Facts {
            size: Some(512 << 20),
            mounts: mounts_of(&all, Some((259, 1)), "nvme0n1p1", |_| None),
        };
        let plan = qualify(
            &busy(&["/boot/efi"]),
            &facts,
            Some(&protected("/mnt/backup")),
        )
        .unwrap();
        assert_eq!(plan.method, Method::ReadOnlyRemount);
        assert_eq!(plan.mount_point, "/boot/efi");
        assert_eq!(plan.fstype, "vfat");
        assert_eq!(
            plan.flags,
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_RELATIME
        );

        // Exactly the limit is still short.
        let facts = Facts {
            size: Some(SHORT_LIMIT),
            mounts: vec![mount("/mnt/data", "xfs", "rw,relatime", "/")],
        };
        assert!(qualify(&busy(&["/mnt/data"]), &facts, Some(&protected("/srv/b"))).is_ok());
    }

    #[test]
    fn what_is_not_held() {
        let p = protected("/mnt/backup/images");
        let one = |mp: &str, fstype: &str, size: u64| Facts {
            size: Some(size),
            mounts: vec![mount(mp, fstype, "rw,relatime", "/")],
        };
        let small = 512 << 20;
        // The root, whatever its size.
        assert_eq!(
            qualify(&busy(&["/"]), &one("/", "ext4", small), Some(&p)),
            Err(NotShort::Root)
        );
        // Too big, or a size nobody knows.
        assert_eq!(
            qualify(
                &busy(&["/mnt/big"]),
                &one("/mnt/big", "ext4", 6 << 30),
                Some(&p)
            ),
            Err(NotShort::TooBig(6 << 30))
        );
        let unknown = Facts {
            size: None,
            ..one("/mnt/x", "ext4", 0)
        };
        assert_eq!(
            qualify(&busy(&["/mnt/x"]), &unknown, Some(&p)),
            Err(NotShort::SizeUnknown)
        );
        // Above or at a place written meanwhile.
        for (mp, path) in [
            ("/var", "/var"),
            ("/var/lib", "/var/lib/own-backup"),
            ("/var/log", "/var/log/own-backup"),
            ("/home", "/home"),
            ("/run", "/run"),
            ("/mnt/backup", "/mnt/backup/images"),
            ("/mnt/backup/images", "/mnt/backup/images"),
        ] {
            assert_eq!(
                qualify(&busy(&[mp]), &one(mp, "ext4", small), Some(&p)),
                Err(NotShort::Holds {
                    mount_point: mp.into(),
                    path: path.into()
                }),
                "{mp}"
            );
        }
        // Any mount point counts, not only the first.
        let two = Facts {
            size: Some(small),
            mounts: vec![
                mount("/mnt/a", "ext4", "rw", "/"),
                mount("/mnt/backup", "ext4", "rw", "/"),
            ],
        };
        assert!(matches!(
            qualify(&busy(&["/mnt/a", "/mnt/backup"]), &two, Some(&p)),
            Err(NotShort::Holds { .. })
        ));
        // Below a protected place, or merely a similar name: fine.
        for mp in [
            "/var/lib/docker",
            "/mnt/backup-old",
            "/run/media/me/STICK",
            "/boot",
        ] {
            assert!(
                qualify(&busy(&[mp]), &one(mp, "ext4", small), Some(&p)).is_ok(),
                "{mp}"
            );
        }
        // Without a destination nobody can tell.
        assert_eq!(
            qualify(&busy(&["/boot"]), &one("/boot", "ext4", small), None),
            Err(NotShort::NoDestination)
        );
        // A filesystem with neither a freeze nor a safe remount.
        assert_eq!(
            qualify(
                &busy(&["/mnt/win"]),
                &one("/mnt/win", "ntfs3", small),
                Some(&p)
            ),
            Err(NotShort::Filesystem("ntfs3".into()))
        );
        // Swap and stacked devices are not mounts.
        let swap = Usage {
            swap: vec!["/dev/sda2".into()],
            ..Usage::default()
        };
        assert_eq!(
            qualify(&swap, &Facts::default(), Some(&p)),
            Err(NotShort::Swap)
        );
        let held = Usage {
            mounted_rw: vec!["/".into()],
            holders: vec!["dm-0".into()],
            ..Usage::default()
        };
        assert_eq!(
            qualify(&held, &one("/", "btrfs", small), Some(&p)),
            Err(NotShort::Holders(vec!["dm-0".into()]))
        );
        assert_eq!(
            qualify(&busy(&["/mnt/x"]), &Facts::default(), Some(&p)),
            Err(NotShort::NoMount)
        );
    }

    #[test]
    fn methods_by_filesystem() {
        for fs in ["ext2", "ext3", "ext4", "xfs", "btrfs", "f2fs"] {
            assert_eq!(method_for(fs), Some(Method::Freeze), "{fs}");
        }
        for fs in ["vfat", "msdos", "exfat"] {
            assert_eq!(method_for(fs), Some(Method::ReadOnlyRemount), "{fs}");
        }
        for fs in ["ntfs3", "fuseblk", "hfsplus", "tmpfs"] {
            assert_eq!(method_for(fs), None, "{fs}");
        }
        for m in [Method::Freeze, Method::ReadOnlyRemount] {
            assert_eq!(Method::parse(m.as_str()), Some(m));
        }
    }

    #[test]
    fn paths_made_absolute_and_resolved() {
        let cwd = Path::new("/srv/work");
        assert_eq!(absolute(Path::new("."), cwd), PathBuf::from("/srv/work"));
        assert_eq!(
            absolute(Path::new("../out/./x"), cwd),
            PathBuf::from("/srv/out/x")
        );
        assert_eq!(absolute(Path::new("/a/../../b"), cwd), PathBuf::from("/b"));

        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, dir.path().join("link")).unwrap();
        let canon = std::fs::canonicalize(&real).unwrap();
        assert_eq!(
            resolved(&dir.path().join("link/not/yet")),
            canon.join("not/yet")
        );
        assert_eq!(resolved(&dir.path().join("link")), canon);

        assert!(holds("/mnt/backup", Path::new("/mnt/backup/x")));
        assert!(holds("/mnt/backup", Path::new("/mnt/backup")));
        assert!(!holds("/mnt/backup", Path::new("/mnt/backup2")));
        assert!(!holds("/mnt/backup/x", Path::new("/mnt/backup")));
    }

    #[test]
    fn protected_paths_cover_the_program_and_the_destination() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real-data");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("data");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let p = protected_from(
            Path::new("../backups/./pc"),
            &[link.clone(), PathBuf::from("logs")],
            Path::new("/srv/work"),
        );
        for must in [
            "/tmp",
            "/var",
            "/run",
            "/home",
            "/usr",
            MARKER_DIR,
            "/var/log/own-backup",
            "/srv/backups/pc",
            "/srv/work/logs",
        ] {
            assert!(p.contains(&PathBuf::from(must)), "{must} in {p:?}");
        }
        // A data folder that is a link counts where it leads, too.
        assert!(p.contains(&link));
        assert!(p.contains(&std::fs::canonicalize(&real).unwrap()));
        // Each once.
        let mut sorted = p.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), p.len());
    }

    fn sample_marker() -> Marker {
        Marker {
            method: Method::ReadOnlyRemount,
            volume: "/dev/nvme0n1p1".into(),
            mount_point: "/mnt/odd name\nwith\\escapes".into(),
            fs_dev: (259, 1),
            flags: libc::MS_NOSUID | libc::MS_RELATIME,
            pid: 4242,
            created: "2026-10-19T10:00:00+00:00".into(),
        }
    }

    #[test]
    fn marker_text_round_trips() {
        let m = sample_marker();
        let text = m.to_text();
        assert_eq!(text.lines().count(), 8, "one value per line: {text}");
        assert_eq!(Marker::parse(&text), Some(m));
        let frozen = Marker {
            method: Method::Freeze,
            mount_point: "/boot".into(),
            ..sample_marker()
        };
        assert_eq!(Marker::parse(&frozen.to_text()), Some(frozen));

        assert_eq!(Marker::parse(""), None);
        assert_eq!(Marker::parse("own-backup-hold 1\nmethod=freeze\n"), None);
        let other = sample_marker()
            .to_text()
            .replace("readonly-remount", "melt");
        assert_eq!(Marker::parse(&other), None);
        let relative = sample_marker()
            .to_text()
            .replace("mount_point=/mnt", "mount_point=mnt");
        assert_eq!(Marker::parse(&relative), None);
    }

    #[test]
    fn marker_names() {
        let id = "0123456789abcdef0123456789abcdef";
        assert_eq!(marker_id(&format!("{id}.marker")), Some(id));
        assert_eq!(new_marker_id(&format!(".{id}.new")), Some(id));
        assert_eq!(marker_id("0123.marker"), None);
        assert_eq!(marker_id(&format!("{id}.marker.bak")), None);
        assert_eq!(new_marker_id(&format!("{id}.new")), None);
    }

    /// The cleanup plan without root: what a dead run left is undone and
    /// removed, what a live run holds is left alone, what failed is kept.
    #[test]
    fn leftovers_planned_and_undone() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("frozen");
        let dead = "a".repeat(32);
        let alive = "b".repeat(32);
        let failing = "c".repeat(32);
        let half = "d".repeat(32);

        let mk = |mp: &str| Marker {
            mount_point: mp.into(),
            ..sample_marker()
        };
        let (dead_path, dead_file) = write_marker(&dir, &dead, &mk("/mnt/dead")).unwrap();
        drop(dead_file); // its run is gone
        let (alive_path, alive_file) = write_marker(&dir, &alive, &mk("/mnt/alive")).unwrap();
        let (failing_path, failing_file) =
            write_marker(&dir, &failing, &mk("/mnt/failing")).unwrap();
        drop(failing_file);
        // Killed while writing its marker.
        std::fs::write(dir.join(format!(".{half}.new")), "own-backup-hold 1\n").unwrap();
        // Not ours by name, and an unreadable one of ours.
        std::fs::write(dir.join("notes.txt"), "keep").unwrap();
        let garbage = dir.join(format!("{}.marker", "e".repeat(32)));
        std::fs::write(&garbage, "nonsense").unwrap();

        let meta = std::fs::metadata(&dir).unwrap();
        assert!(meta.is_dir());
        assert_eq!(meta.mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(&dead_path).unwrap().mode() & 0o777, 0o600);

        let seen = std::sync::Mutex::new(Vec::new());
        let undone = undo_leftovers_in(&dir, |m| {
            seen.lock().unwrap().push(m.mount_point.clone());
            match m.mount_point.as_str() {
                "/mnt/dead" => Ok(Outcome::Undone),
                _ => Err(io::Error::from_raw_os_error(libc::EPERM)),
            }
        });
        assert_eq!(undone, 1);
        let mut seen = seen.into_inner().unwrap();
        seen.sort();
        assert_eq!(seen, vec!["/mnt/dead", "/mnt/failing"]);
        assert!(!dead_path.exists(), "undone: removed");
        assert!(alive_path.exists(), "held by a live run: left alone");
        assert!(
            failing_path.exists(),
            "undo failed: kept for the next cleanup"
        );
        assert!(!dir.join(format!(".{half}.new")).exists());
        assert!(!garbage.exists());
        assert!(dir.join("notes.txt").exists());

        // The live run releases: removes its marker, then lets go of the lock.
        std::fs::remove_file(&alive_path).unwrap();
        drop(alive_file);
        let undone = undo_leftovers_in(&dir, |_| Ok(Outcome::Nothing));
        assert_eq!(undone, 0);
        assert!(!failing_path.exists(), "nothing held any more: removed");
    }

    /// A marker folder someone else could write to is not acted on.
    #[test]
    fn untrusted_marker_folder_is_ignored() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("frozen");
        let id = "f".repeat(32);
        let (_, file) = write_marker(&dir, &id, &sample_marker()).unwrap();
        drop(file);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(undo_leftovers_in(&dir, |_| Ok(Outcome::Undone)), 0);
        assert!(write_marker(&dir, &"0".repeat(32), &sample_marker()).is_err());
        assert_eq!(
            undo_leftovers_in(&root.path().join("missing"), |_| Ok(Outcome::Undone)),
            0
        );
    }
}
