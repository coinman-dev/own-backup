//! Building the Linux boot medium in the service (PLAN §6, stage 6): the
//! installed program packed into a .deb, the build script run as root, the
//! ISO saved into the person's folder and, when asked, written onto a stick.
//! The decisions — stages, percentages, refusals, names — are in
//! [`crate::media_linux`].
//!
//! **The medium carries exactly the installed program.** Its package is made
//! from what dpkg installed, the way `dpkg-repack` does it: the files of
//! `dpkg -L own-backup`, the maintainer scripts beside dpkg's list and the
//! control fields of dpkg's status. Every file is compared with dpkg's own
//! checksum on the way (`dpkg --verify`, done here): a changed program file
//! refuses the build rather than putting it on a medium someone will trust.
//! A changed configuration file (`/etc/xdg/autostart/…`) is the
//! administrator's right; it goes on as it is, and the report says so. A
//! developer's service (not installed from a package) is given a .deb
//! instead.
//!
//! **The build runs outside the service's sandbox.** The unit forbids
//! setuid files (`RestrictSUIDSGID=`), and dpkg unpacking `mount` or `su`
//! into the medium's tree needs them; so the script runs as a transient unit
//! of its own (`systemd-run`), stopped through systemd on cancel — which
//! signals every process of the build at once, and the script's trap undoes
//! its mounts and removes the half-built tree. Without systemd (a container,
//! a development run) it runs as a process group of its own.
//!
//! **The ISO goes into the person's own folder.** The service is root and the
//! folder is named by the window, so it is opened once, checked to be a
//! directory owned by the caller (`SO_PEERCRED`), and everything after goes
//! through that descriptor: a component swapped for a symlink afterwards
//! changes nothing. The file is written under a temporary name, given to the
//! caller, then renamed without replacing anything — an existing ISO keeps
//! its name and the new one gets ` (2)`.
//!
//! The builder and live-build's package cache stay in [`CACHE_DIR`] between
//! builds (about 2 GiB); [`clear_cache`] removes them.

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::ffi::CString;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use md5::{Digest, Md5};
use own_backup_common::cancel::CancelToken;
use own_backup_common::engine_dto::MediaStickIdentityDto;
use own_backup_common::{tr, OwnBackupError, Result};
use tracing::{info, warn};

use crate::media_linux::{
    classify_failure, gib, iso_file_name, need_bytes, refusal, url_host, LinuxStage, ToolFailure,
    Tracker, DEST_MIN_BYTES,
};
use crate::media_stick_linux::{write_stick, StickReport, StickStage};

/// The package the medium is made of.
pub const PACKAGE: &str = "own-backup";

/// The builder tree, live-build's package cache, the last build's log.
pub const CACHE_DIR: &str = "/var/cache/own-backup/media";

/// The shared build script as the package installs it.
pub const SCRIPT: &str = "/usr/libexec/own-backup/build-linux-media";

/// live-build's configuration (`auto/`, `config/`) as the package installs it.
pub const CONFIG_DIR: &str = "/usr/share/own-backup/media";

/// The transient unit the build runs as.
const UNIT: &str = "own-backup-media-build";

/// The Debian archive's keyring debootstrap checks the mirror with.
const KEYRING: &str = "/usr/share/keyrings/debian-archive-keyring.gpg";

/// The mirror, as the script reads it.
const DEFAULT_MIRROR: &str = "http://deb.debian.org/debian/";

/// How long a stopped build may take to undo its mounts before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(90);

/// Copy in pieces of this size.
const CHUNK: usize = 4 << 20;

// --- the installed package as a .deb ------------------------------------------------

/// One stanza of dpkg's status file: its fields in order, each with its
/// continuation lines (` …`) kept as they are.
type Stanza = Vec<(String, String)>;

fn stanzas(text: &str) -> Vec<Stanza> {
    let mut out = Vec::new();
    let mut cur: Stanza = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        } else if line.starts_with(' ') || line.starts_with('\t') {
            if let Some((_, value)) = cur.last_mut() {
                value.push('\n');
                value.push_str(line);
            }
        } else if let Some((name, value)) = line.split_once(':') {
            cur.push((name.trim().to_string(), value.trim().to_string()));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn field<'a>(stanza: &'a Stanza, name: &str) -> Option<&'a str> {
    stanza
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// The control fields a binary package carries; dpkg's own bookkeeping
/// (`Status`, `Conffiles`, `Config-Version`) is not one of them.
const CONTROL_FIELDS: &[&str] = &[
    "Package",
    "Source",
    "Version",
    "Architecture",
    "Multi-Arch",
    "Essential",
    "Protected",
    "Maintainer",
    "Installed-Size",
    "Pre-Depends",
    "Depends",
    "Recommends",
    "Suggests",
    "Enhances",
    "Breaks",
    "Conflicts",
    "Replaces",
    "Provides",
    "Built-Using",
    "Static-Built-Using",
    "Section",
    "Priority",
    "Homepage",
    "Description",
];

/// The installed package as dpkg's database describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledPackage {
    pub version: String,
    /// `DEBIAN/control` of a package with exactly these fields.
    pub control: String,
    /// Configuration files with the checksum they were installed with.
    pub conffiles: Vec<(String, String)>,
}

/// `package` as installed under `root` (`/`; a fixture in tests), or the
/// reason there is none.
pub fn installed(root: &Path, package: &str) -> Result<InstalledPackage> {
    let status_path = root.join("var/lib/dpkg/status");
    let text = std::fs::read_to_string(&status_path).map_err(|e| {
        OwnBackupError::Config(
            tr!(
                "media_linux.dpkg_unreadable",
                path = status_path.display(),
                error = e
            )
            .into_owned(),
        )
    })?;
    let mut found: Vec<Stanza> = stanzas(&text)
        .into_iter()
        .filter(|s| field(s, "Package") == Some(package))
        .collect();
    // Another architecture's entry may sit beside the installed one.
    found.sort_by_key(|s| field(s, "Status") != Some("install ok installed"));
    let not_installed = || OwnBackupError::Config(tr!("media_linux.not_installed").into_owned());
    let stanza = found.into_iter().next().ok_or_else(not_installed)?;
    let status = field(&stanza, "Status").unwrap_or("");
    if status != "install ok installed" {
        // Removed with its configuration kept, or never installed: there is
        // nothing to pack. Anything else is a package dpkg left halfway.
        let state = status.rsplit(' ').next().unwrap_or("");
        if matches!(state, "not-installed" | "config-files" | "") {
            return Err(not_installed());
        }
        return Err(OwnBackupError::Config(
            tr!("media_linux.not_installed_cleanly", status = status).into_owned(),
        ));
    }
    let version = field(&stanza, "Version").unwrap_or("").to_string();
    let mut control = String::new();
    for (name, value) in &stanza {
        if CONTROL_FIELDS.iter().any(|f| f.eq_ignore_ascii_case(name)) {
            control.push_str(&format!("{name}: {value}\n"));
        }
    }
    let conffiles = field(&stanza, "Conffiles")
        .unwrap_or("")
        .lines()
        .filter_map(|l| {
            let words: Vec<&str> = l.split_whitespace().collect();
            match words.as_slice() {
                [path, md5] => Some((path.to_string(), md5.to_string())),
                // "obsolete", "remove-on-upgrade": not part of this version.
                _ => None,
            }
        })
        .collect();
    Ok(InstalledPackage {
        version,
        control,
        conffiles,
    })
}

/// A file of dpkg's info directory: `own-backup.list`, or the multi-arch
/// `own-backup:amd64.list`.
fn info_file(root: &Path, package: &str, suffix: &str) -> Option<PathBuf> {
    let info = root.join("var/lib/dpkg/info");
    [
        format!("{package}.{suffix}"),
        format!("{package}:amd64.{suffix}"),
    ]
    .into_iter()
    .map(|n| info.join(n))
    .find(|p| p.exists())
}

/// What [`repack`] made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repacked {
    pub version: String,
    /// Configuration files that differ from the installed ones; they went on
    /// as they are now.
    pub changed_conffiles: Vec<String>,
}

fn md5_copy(src: &Path, dst: &Path, mode: u32) -> std::io::Result<String> {
    let mut input = File::open(src)?;
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dst)?;
    let mut hasher = Md5::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        output.write_all(&buf[..n])?;
    }
    output.set_permissions(std::fs::Permissions::from_mode(mode))?;
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Lay out `package` as installed under `root` in `stage` (which must not
/// exist yet) for `dpkg-deb --build`, checking every file against dpkg's
/// checksums on the way.
pub fn stage_installed(root: &Path, package: &str, stage: &Path) -> Result<Repacked> {
    let pkg = installed(root, package)?;
    let list_path = info_file(root, package, "list")
        .ok_or_else(|| OwnBackupError::Config(tr!("media_linux.not_installed").into_owned()))?;
    let list = std::fs::read_to_string(&list_path)?;
    let md5sums_path = info_file(root, package, "md5sums");
    let md5sums_text = match &md5sums_path {
        Some(p) => std::fs::read_to_string(p)?,
        None => String::new(),
    };
    // "<md5>  usr/bin/own-backup-cli"
    let sums: HashMap<String, String> = md5sums_text
        .lines()
        .filter_map(|l| {
            let (sum, path) = l.split_once(char::is_whitespace)?;
            Some((format!("/{}", path.trim_start()), sum.to_ascii_lowercase()))
        })
        .collect();
    let conffiles: HashMap<&str, &str> = pkg
        .conffiles
        .iter()
        .map(|(p, s)| (p.as_str(), s.as_str()))
        .collect();

    std::fs::create_dir_all(stage)?;
    let mut modified: Vec<String> = Vec::new();
    let mut changed_conffiles = Vec::new();
    // Modes are given to directories at the end: one without its owner's
    // write bit would refuse the files that go into it (outside root).
    let mut dir_modes: Vec<(PathBuf, u32)> = Vec::new();
    for path in list.lines().map(str::trim) {
        if path.is_empty() || path == "/." || !path.starts_with('/') {
            continue;
        }
        let rel = path.trim_start_matches('/');
        let src = root.join(rel);
        let dst = stage.join(rel);
        let meta = match std::fs::symlink_metadata(&src) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                modified.push(path.to_string());
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        let kind = meta.file_type();
        if kind.is_dir() {
            std::fs::create_dir_all(&dst)?;
            dir_modes.push((dst, meta.mode() & 0o7777));
        } else if kind.is_symlink() {
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::os::unix::fs::symlink(std::fs::read_link(&src)?, &dst)?;
        } else if kind.is_file() {
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let sum = md5_copy(&src, &dst, meta.mode() & 0o7777)?;
            if let Some(want) = conffiles.get(path) {
                if !want.eq_ignore_ascii_case(&sum) {
                    changed_conffiles.push(path.to_string());
                }
            } else if sums.get(path) != Some(&sum) {
                // Changed, or a file dpkg holds no checksum for: either way
                // not known to be what was installed.
                modified.push(path.to_string());
            }
        } else {
            modified.push(path.to_string());
        }
    }
    if !modified.is_empty() {
        let shown = modified.len().min(5);
        let mut files = modified[..shown].join(", ");
        if modified.len() > shown {
            files.push_str(&format!(" (+{})", modified.len() - shown));
        }
        return Err(OwnBackupError::Config(
            tr!("media_linux.modified", files = files).into_owned(),
        ));
    }

    // Explicit modes: the service runs with UMask=0027, and dpkg-deb refuses
    // a control directory that is not 0755..0775 (stand, 22.10).
    let debian = stage.join("DEBIAN");
    std::fs::create_dir_all(&debian)?;
    std::fs::set_permissions(&debian, std::fs::Permissions::from_mode(0o755))?;
    std::fs::set_permissions(stage, std::fs::Permissions::from_mode(0o755))?;
    let write = |name: &str, text: &str, mode: u32| -> std::io::Result<()> {
        let p = debian.join(name);
        std::fs::write(&p, text)?;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode))
    };
    write("control", &pkg.control, 0o644)?;
    if !md5sums_text.is_empty() {
        write("md5sums", &md5sums_text, 0o644)?;
    }
    if !pkg.conffiles.is_empty() {
        let list: String = pkg
            .conffiles
            .iter()
            .map(|(p, _)| format!("{p}\n"))
            .collect();
        write("conffiles", &list, 0o644)?;
    }
    for (name, mode) in [
        ("preinst", 0o755),
        ("postinst", 0o755),
        ("prerm", 0o755),
        ("postrm", 0o755),
        ("config", 0o755),
        ("templates", 0o644),
        ("triggers", 0o644),
        ("shlibs", 0o644),
        ("symbols", 0o644),
    ] {
        if let Some(src) = info_file(root, package, name) {
            write(name, &std::fs::read_to_string(src)?, mode)?;
        }
    }
    for (dir, mode) in dir_modes.into_iter().rev() {
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode))?;
    }
    Ok(Repacked {
        version: pkg.version,
        changed_conffiles,
    })
}

/// `dpkg-deb --build` of a staged tree. Ownership is root's inside the
/// archive whoever runs it, as in `scripts/build-deb.sh`.
pub fn dpkg_deb_build(stage: &Path, out: &Path) -> Result<()> {
    let output = Command::new("dpkg-deb")
        .args(["--root-owner-group", "-Zgzip", "--build"])
        .arg(stage)
        .arg(out)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| {
            OwnBackupError::Config(tr!("media_linux.repack_failed", error = e).into_owned())
        })?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(OwnBackupError::Config(
            tr!("media_linux.repack_failed", error = error).into_owned(),
        ));
    }
    Ok(())
}

/// The installed package as a .deb at `out`, staged in `stage` (removed
/// before and after).
pub fn repack(root: &Path, package: &str, stage: &Path, out: &Path) -> Result<Repacked> {
    repack_with(root, package, stage, out, &mut |s, o| dpkg_deb_build(s, o))
}

/// [`repack`] with the archive made by `pack(stage, out)`.
pub fn repack_with(
    root: &Path,
    package: &str,
    stage: &Path,
    out: &Path,
    pack: &mut dyn FnMut(&Path, &Path) -> Result<()>,
) -> Result<Repacked> {
    remove_tree(stage)?;
    let staged = stage_installed(root, package, stage);
    let built = staged.and_then(|r| pack(stage, out).map(|()| r));
    let _ = remove_tree(stage);
    built
}

/// `dpkg-deb --build` as a transient unit: inside the service's sandbox
/// (`RestrictSUIDSGID=`) its tar fails to read the staged files (stand,
/// 22.10: "Cannot stat: Function not implemented").
fn dpkg_deb_unsandboxed(stage: &Path, out: &Path) -> Result<()> {
    let program = find_program("dpkg-deb").ok_or_else(|| {
        OwnBackupError::Config(tr!("media_linux.repack_failed", error = "dpkg-deb").into_owned())
    })?;
    let args: Vec<String> = ["--root-owner-group", "-Zgzip", "--build"]
        .iter()
        .map(|s| s.to_string())
        .chain([stage.display().to_string(), out.display().to_string()])
        .collect();
    let mut lines: Vec<String> = Vec::new();
    let ended = run_program(
        &program,
        &args,
        &CancelToken::default(),
        &mut None,
        &mut |l| lines.push(l.to_string()),
    )?;
    if !ended.ok || !out.is_file() {
        let error = lines[lines.len().saturating_sub(4)..].join(" / ");
        return Err(OwnBackupError::Config(
            tr!("media_linux.repack_failed", error = error).into_owned(),
        ));
    }
    Ok(())
}

fn remove_tree(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

// --- the person's folder and files -----------------------------------------------------

fn cstr(name: &str) -> std::io::Result<CString> {
    CString::new(name).map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))
}

/// The folder the ISO goes into: an existing directory owned by the caller,
/// held open from the check on.
#[derive(Debug)]
pub struct OutputDir {
    dir: File,
    pub path: PathBuf,
    uid: u32,
    gid: u32,
}

/// Open `path` as the caller's folder: absolute, an existing directory, owned
/// by `uid` (any owner for root, who then writes the ISO as the owner's).
/// Symlinks in what was typed are resolved first and the result opened
/// without following any.
pub fn open_output_dir(path: &str, uid: u32, gid: u32) -> Result<OutputDir> {
    let typed = Path::new(path.trim());
    if !typed.is_absolute() {
        return Err(OwnBackupError::Config(
            tr!("media_linux.dest_not_absolute", path = path).into_owned(),
        ));
    }
    let real = std::fs::canonicalize(typed).map_err(|e| {
        OwnBackupError::Config(tr!("media_linux.dest_missing", path = path, error = e).into_owned())
    })?;
    let dir = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&real)
        .map_err(|e| {
            OwnBackupError::Config(
                tr!("media_linux.dest_missing", path = path, error = e).into_owned(),
            )
        })?;
    let meta = dir.metadata()?;
    if !meta.is_dir() {
        return Err(OwnBackupError::Config(
            tr!("media_linux.dest_not_dir", path = path).into_owned(),
        ));
    }
    // root (the CLI under sudo) may name any folder; the ISO then belongs
    // to whoever owns the folder, as anything else in it does.
    let (uid, gid) = if uid == 0 {
        (meta.uid(), meta.gid())
    } else if meta.uid() == uid {
        (uid, gid)
    } else {
        return Err(OwnBackupError::Config(
            tr!("media_linux.dest_not_owner", path = real.display()).into_owned(),
        ));
    };
    Ok(OutputDir {
        dir,
        path: real,
        uid,
        gid,
    })
}

/// Free bytes for an unprivileged writer on the file system of `fd`.
fn free_bytes_fd(fd: &impl AsRawFd) -> std::io::Result<u64> {
    // SAFETY: fstatvfs fills a zeroed struct for a descriptor we hold.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatvfs(fd.as_raw_fd(), &mut st) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(st.f_bavail * st.f_frsize)
}

/// Free bytes on the file system of `path`.
pub fn free_bytes(path: &Path) -> std::io::Result<u64> {
    free_bytes_fd(&File::open(path)?)
}

/// Removes a temporary file in the folder unless told it was published.
struct TempName<'a> {
    dir: &'a File,
    name: CString,
    armed: bool,
}

impl Drop for TempName<'_> {
    fn drop(&mut self) {
        if self.armed {
            // SAFETY: unlinkat on a held directory descriptor and a C string.
            unsafe { libc::unlinkat(self.dir.as_raw_fd(), self.name.as_ptr(), 0) };
        }
    }
}

impl OutputDir {
    pub fn free_bytes(&self) -> std::io::Result<u64> {
        free_bytes_fd(&self.dir)
    }

    /// Copy `src` into the folder as `OwnBackup-Linux-<version>.iso` (or
    /// ` (2)` and so on when that exists), the caller's own file. `progress`
    /// gets the percentage copied. Nothing is left behind on failure.
    pub fn save(
        &self,
        src: &Path,
        version: &str,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(u64),
    ) -> Result<PathBuf> {
        let mut input = File::open(src)?;
        let len = input.metadata()?.len();
        let tmp_name = format!(".{}.part-{}", iso_file_name(version, 1), std::process::id());
        let tmp = cstr(&tmp_name)?;
        // SAFETY: openat on a held directory descriptor; the new descriptor
        // is owned by the File below.
        let fd = unsafe {
            libc::openat(
                self.dir.as_raw_fd(),
                tmp.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o644 as libc::c_uint,
            )
        };
        if fd < 0 {
            let e = std::io::Error::last_os_error();
            return Err(self.save_error(e));
        }
        // SAFETY: fd is a fresh descriptor nobody else owns.
        let mut out = unsafe { File::from_raw_fd(fd) };
        let mut guard = TempName {
            dir: &self.dir,
            name: tmp,
            armed: true,
        };
        std::os::unix::fs::fchown(&out, Some(self.uid), Some(self.gid))
            .map_err(|e| self.save_error(e))?;
        // An ordinary file of theirs, whatever the service's UMask=0027 made it.
        out.set_permissions(std::fs::Permissions::from_mode(0o644))
            .map_err(|e| self.save_error(e))?;
        let mut buf = vec![0u8; CHUNK];
        let mut done: u64 = 0;
        let mut shown = u64::MAX;
        loop {
            if cancel.is_cancelled() {
                return Err(OwnBackupError::Cancelled);
            }
            let n = input.read(&mut buf)?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n]).map_err(|e| self.save_error(e))?;
            done += n as u64;
            let pct = (done * 100).checked_div(len).unwrap_or(100);
            if pct != shown {
                shown = pct;
                progress(pct);
            }
        }
        out.sync_all().map_err(|e| self.save_error(e))?;
        drop(out);

        for n in 1..=999 {
            let name = iso_file_name(version, n);
            let target = cstr(&name)?;
            // SAFETY: renameat2/linkat within the held directory.
            let rc = unsafe {
                libc::renameat2(
                    self.dir.as_raw_fd(),
                    guard.name.as_ptr(),
                    self.dir.as_raw_fd(),
                    target.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            };
            if rc == 0 {
                guard.armed = false;
                return Ok(self.path.join(name));
            }
            let e = std::io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::EEXIST) => continue,
                // A file system without RENAME_NOREPLACE: a hard link refuses
                // an existing name just the same.
                Some(libc::EINVAL) | Some(libc::ENOSYS) | Some(libc::EOPNOTSUPP) => {
                    let rc = unsafe {
                        libc::linkat(
                            self.dir.as_raw_fd(),
                            guard.name.as_ptr(),
                            self.dir.as_raw_fd(),
                            target.as_ptr(),
                            0,
                        )
                    };
                    if rc == 0 {
                        // The temporary name goes with the guard.
                        return Ok(self.path.join(name));
                    }
                    let e = std::io::Error::last_os_error();
                    if e.raw_os_error() == Some(libc::EEXIST) {
                        continue;
                    }
                    return Err(self.save_error(e));
                }
                _ => return Err(self.save_error(e)),
            }
        }
        Err(self.save_error(std::io::Error::from(std::io::ErrorKind::AlreadyExists)))
    }

    fn save_error(&self, e: std::io::Error) -> OwnBackupError {
        OwnBackupError::Config(
            tr!(
                "media_linux.save_failed",
                path = self.path.display(),
                error = e
            )
            .into_owned(),
        )
    }
}

/// A file the caller named (a development .deb): a regular file, opened
/// without following a symlink, owned by the caller unless the caller is root.
pub fn open_callers_file(path: &str, uid: u32) -> Result<File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|e| {
            OwnBackupError::Config(
                tr!("media_linux.deb_unreadable", path = path, error = e).into_owned(),
            )
        })?;
    let meta = file.metadata()?;
    if !meta.is_file() || (uid != 0 && meta.uid() != uid) {
        return Err(OwnBackupError::Config(
            tr!("media_linux.deb_not_owner", path = path).into_owned(),
        ));
    }
    Ok(file)
}

// --- the cache ---------------------------------------------------------------------------

/// Bytes a tree takes on disk, staying on its file system and never
/// following a symlink (the builder holds a whole Debian).
pub fn dir_bytes(path: &Path) -> u64 {
    let Ok(top) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    let dev = top.dev();
    let mut total = top.blocks() * 512;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.dev() != dev {
                continue;
            }
            total += meta.blocks() * 512;
            if meta.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    total
}

/// Where the build's script, configuration and cache are: the package's
/// places, or (a development service) `OBK_MEDIA_SCRIPT`, `OBK_MEDIA_CONFIG`
/// and `OBK_MEDIA_CACHE` — the service's own environment, set by root.
#[derive(Debug, Clone)]
pub struct Paths {
    pub script: PathBuf,
    pub config: PathBuf,
    pub cache: PathBuf,
}

impl Paths {
    pub fn locate() -> Paths {
        let env = |name: &str, default: &str| {
            std::env::var_os(name)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(default))
        };
        Paths {
            script: env("OBK_MEDIA_SCRIPT", SCRIPT),
            config: env("OBK_MEDIA_CONFIG", CONFIG_DIR),
            cache: env("OBK_MEDIA_CACHE", CACHE_DIR),
        }
    }

    /// The script and the configuration are where they should be.
    fn check(&self) -> Result<()> {
        for p in [&self.script, &self.config.join("auto/config")] {
            if !p.is_file() {
                return Err(OwnBackupError::Config(
                    tr!("media_linux.script_missing", path = p.display()).into_owned(),
                ));
            }
        }
        Ok(())
    }

    fn log(&self) -> PathBuf {
        self.cache.join("last-build.log")
    }
}

fn find_program(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain(["/usr/sbin", "/sbin", "/usr/bin", "/bin"].map(PathBuf::from))
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// What the host lacks for a build: debootstrap and the Debian keyring.
pub fn missing_tools() -> Vec<String> {
    let mut out = Vec::new();
    if find_program("debootstrap").is_none() {
        out.push("debootstrap".to_string());
    }
    if !Path::new(KEYRING).is_file() {
        out.push("debian-archive-keyring".to_string());
    }
    out
}

fn mirror_host() -> String {
    let mirror = std::env::var("OBK_MEDIA_MIRROR")
        .ok()
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| DEFAULT_MIRROR.to_string());
    url_host(&mirror).to_string()
}

/// The state the window shows before a build.
#[derive(Debug, Clone, Default)]
pub struct Status {
    pub installed_version: Option<String>,
    pub cache_dir: PathBuf,
    pub cache_bytes: u64,
    pub free_bytes: u64,
    pub need_bytes: u64,
    /// What stops a build now, in the person's words.
    pub problems: Vec<String>,
}

pub fn status() -> Status {
    let paths = Paths::locate();
    let mut problems = Vec::new();
    if let Err(e) = paths.check() {
        problems.push(e.to_string());
    }
    let missing = missing_tools();
    if !missing.is_empty() {
        problems.push(refusal("missing-tool", &missing.join(", "), &[]));
    }
    let installed_version = match installed(Path::new("/"), PACKAGE) {
        Ok(p) => Some(p.version),
        Err(e) => {
            problems.push(error_text(e));
            None
        }
    };
    let cache_bytes = dir_bytes(&paths.cache);
    // The cache's own file system: the directory, or the nearest parent that
    // exists before the first build makes it.
    let free = paths
        .cache
        .ancestors()
        .find(|p| p.exists())
        .and_then(|p| free_bytes(p).ok())
        .unwrap_or(0);
    let need = need_bytes(cache_bytes);
    if free < need {
        problems.push(no_space(&paths.cache, need, free));
    }
    Status {
        installed_version,
        cache_dir: paths.cache,
        cache_bytes,
        free_bytes: free,
        need_bytes: need,
        problems,
    }
}

fn no_space(path: &Path, need: u64, free: u64) -> String {
    tr!(
        "media_linux.no_space",
        path = path.display(),
        need = gib(need),
        free = gib(free)
    )
    .into_owned()
}

/// The message of an error without the variant's English prefix.
fn error_text(e: OwnBackupError) -> String {
    match e {
        OwnBackupError::Config(m) | OwnBackupError::Device(m) => m,
        other => other.to_string(),
    }
}

// --- running the script --------------------------------------------------------------------

/// Is there a systemd to run a transient unit with?
fn have_systemd() -> bool {
    Path::new("/run/systemd/system").is_dir() && find_program("systemd-run").is_some()
}

/// The environment the script gets: a plain PATH, the C locale (its tools'
/// output is read, see [`crate::media_linux`]) and the mirror settings.
fn script_env() -> Vec<(String, String)> {
    let mut env = vec![
        (
            "PATH".to_string(),
            "/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
        ),
        ("LANG".to_string(), "C.UTF-8".to_string()),
        ("LC_ALL".to_string(), "C.UTF-8".to_string()),
    ];
    for name in ["OBK_MEDIA_MIRROR", "OBK_MEDIA_SECURITY_MIRROR"] {
        if let Ok(v) = std::env::var(name) {
            if !v.is_empty() {
                env.push((name.to_string(), v));
            }
        }
    }
    env
}

/// The command line that runs the script as the transient unit [`UNIT`].
pub fn systemd_run_argv(script: &Path, args: &[String], env: &[(String, String)]) -> Vec<String> {
    let mut argv: Vec<String> = [
        "systemd-run",
        "--quiet",
        "--collect",
        "--wait",
        "--pipe",
        "--service-type=exec",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    argv.push(format!("--unit={UNIT}"));
    argv.push("--description=Own Backup: building the Linux boot medium".into());
    for prop in [
        "KillMode=control-group".to_string(),
        format!("TimeoutStopSec={}", STOP_GRACE.as_secs()),
        // A build in the background must not make the desktop crawl.
        "Nice=10".to_string(),
        "IOSchedulingClass=best-effort".to_string(),
        "IOSchedulingPriority=7".to_string(),
    ] {
        argv.push("--property".into());
        argv.push(prop);
    }
    for (k, v) in env {
        argv.push(format!("--setenv={k}={v}"));
    }
    argv.push(script.display().to_string());
    argv.extend(args.iter().cloned());
    argv
}

fn systemctl(args: &[&str]) {
    let _ = Command::new("systemctl")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// How the script ended.
#[derive(Debug)]
struct Ended {
    ok: bool,
    cancelled: bool,
}

/// Run the script with `args`, every output line to `on_line` (and to the
/// log). A cancel stops it the way systemd stops a unit.
fn run_script(
    paths: &Paths,
    args: &[String],
    cancel: &CancelToken,
    log: &mut Option<File>,
    on_line: &mut dyn FnMut(&str),
) -> Result<Ended> {
    run_program(&paths.script, args, cancel, log, on_line)
}

/// Run `program` with `args` outside the service's sandbox (see the module
/// header), every output line to `on_line` (and to the log).
fn run_program(
    program: &Path,
    args: &[String],
    cancel: &CancelToken,
    log: &mut Option<File>,
    on_line: &mut dyn FnMut(&str),
) -> Result<Ended> {
    let env = script_env();
    let systemd = have_systemd();
    let mut cmd = if systemd {
        // A unit left behind by a service that was killed mid-build: stopped
        // (its trap cleans up) before a new one may take the name.
        systemctl(&["stop", &format!("{UNIT}.service")]);
        systemctl(&["reset-failed", &format!("{UNIT}.service")]);
        let argv = systemd_run_argv(program, args, &env);
        let mut c = Command::new(&argv[0]);
        c.args(&argv[1..]);
        c
    } else {
        let mut c = Command::new(program);
        c.args(args).env_clear().envs(env.iter().cloned());
        // Its own group: a cancel signals the script and every tool under it.
        std::os::unix::process::CommandExt::process_group(&mut c, 0);
        c
    };
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            OwnBackupError::Config(
                tr!(
                    "media_linux.start_failed",
                    path = program.display(),
                    error = e
                )
                .into_owned(),
            )
        })?;
    let (tx, rx) = mpsc::channel::<String>();
    let readers: Vec<_> = [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    .map(|stream| {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stream);
            let mut buf = Vec::new();
            // apt and xorriso redraw a line with \r: each redraw is a line.
            loop {
                buf.clear();
                match read_until_any(&mut reader, &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let line = String::from_utf8_lossy(&buf);
                        let line = line.trim_end_matches(['\n', '\r']);
                        if !line.is_empty() && tx.send(line.to_string()).is_err() {
                            break;
                        }
                    }
                }
            }
        })
    })
    .collect();
    drop(tx);

    let pid = child.id() as i32;
    let mut stop_sent: Option<Instant> = None;
    let mut killed = false;
    // When the script has ended: its last lines are still read for a moment,
    // but a pipe someone else keeps open (systemd's copy of the unit's
    // output) does not hold the build open.
    let mut exited: Option<Instant> = None;
    let mut all_read = false;
    loop {
        match rx.recv_timeout(Duration::from_millis(300)) {
            Ok(line) => {
                if let Some(f) = log.as_mut() {
                    let _ = writeln!(f, "{line}");
                }
                on_line(&line);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                all_read = true;
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => match exited {
                Some(t) if t.elapsed() > Duration::from_secs(2) => break,
                Some(_) => {}
                None => {
                    if matches!(child.try_wait(), Ok(Some(_))) {
                        exited = Some(Instant::now());
                    }
                }
            },
        }
        if cancel.is_cancelled() && stop_sent.is_none() {
            info!("stopping the medium build");
            stop_sent = Some(Instant::now());
            if systemd {
                // systemd signals the whole build and kills what is left
                // after TimeoutStopSec; this returns when the unit is down.
                std::thread::spawn(|| systemctl(&["stop", &format!("{UNIT}.service")]));
            } else {
                // SAFETY: signalling the process group we created.
                unsafe { libc::kill(-pid, libc::SIGTERM) };
            }
        }
        if !systemd && !killed && stop_sent.is_some_and(|t| t.elapsed() > STOP_GRACE) {
            killed = true;
            // SAFETY: as above.
            unsafe { libc::kill(-pid, libc::SIGKILL) };
        }
    }
    if all_read {
        for r in readers {
            let _ = r.join();
        }
    }
    let status = child.wait()?;
    Ok(Ended {
        ok: status.success() && !cancel.is_cancelled(),
        cancelled: cancel.is_cancelled(),
    })
}

/// `read_until` that stops at `\n` or `\r`.
fn read_until_any(reader: &mut impl BufRead, out: &mut Vec<u8>) -> std::io::Result<usize> {
    let mut read = 0;
    loop {
        let (done, used) = {
            let available = match reader.fill_buf() {
                Ok(b) => b,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            match available.iter().position(|&b| b == b'\n' || b == b'\r') {
                Some(i) => {
                    out.extend_from_slice(&available[..=i]);
                    (true, i + 1)
                }
                None => {
                    out.extend_from_slice(available);
                    (available.is_empty(), available.len())
                }
            }
        };
        reader.consume(used);
        read += used;
        if done || used == 0 {
            return Ok(read);
        }
    }
}

/// The script's `clean`: the build tree goes (after its mounts are undone),
/// the builder and the package cache stay.
fn clean_build_tree(paths: &Paths) {
    let mut log = None;
    let args = vec![
        "clean".to_string(),
        "--work".to_string(),
        paths.cache.display().to_string(),
    ];
    if let Err(e) = run_script(paths, &args, &CancelToken::default(), &mut log, &mut |_| {}) {
        warn!(error = %e, "could not clean the medium's build tree");
    }
}

/// Remove the builder, the package cache and everything else of the build.
/// Returns the bytes freed.
pub fn clear_cache() -> Result<u64> {
    let paths = Paths::locate();
    paths.check()?;
    let before = dir_bytes(&paths.cache);
    if before == 0 {
        return Ok(0);
    }
    let args = vec![
        "purge".to_string(),
        "--work".to_string(),
        paths.cache.display().to_string(),
    ];
    let mut log = None;
    let ended = run_script(
        &paths,
        &args,
        &CancelToken::default(),
        &mut log,
        &mut |l| info!(target: "media", "{l}"),
    )?;
    let after = dir_bytes(&paths.cache);
    if !ended.ok {
        return Err(OwnBackupError::Config(
            tr!("media_linux.clear_failed", path = paths.cache.display()).into_owned(),
        ));
    }
    Ok(before.saturating_sub(after))
}

// --- the whole build --------------------------------------------------------------------------

/// What to build and where to.
pub struct BuildRequest {
    pub out: OutputDir,
    /// A .deb to put on the medium instead of the installed package (a
    /// development service); see [`open_callers_file`].
    pub deb: Option<File>,
    /// Also write the medium onto this stick, which must still be the one
    /// the list showed.
    pub stick: Option<(String, MediaStickIdentityDto)>,
}

/// What the build did.
#[derive(Debug, Clone)]
pub struct BuildReport {
    pub iso_path: PathBuf,
    pub iso_bytes: u64,
    pub version: String,
    /// Made from the installed package (not a .deb that was given).
    pub from_installed: bool,
    pub warnings: Vec<String>,
    pub stick: Option<StickReport>,
    pub cache_dir: PathBuf,
    pub cache_bytes: u64,
}

/// Build the medium; `progress` gets the stage, its percentage and a word
/// about it.
pub fn build(
    req: BuildRequest,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(LinuxStage, u64, &str),
) -> Result<BuildReport> {
    let paths = Paths::locate();
    paths.check()?;
    let missing = missing_tools();
    if !missing.is_empty() {
        return Err(OwnBackupError::Config(refusal(
            "missing-tool",
            &missing.join(", "),
            &[],
        )));
    }

    // Space first: a build that runs out halfway wastes ten minutes.
    std::fs::create_dir_all(&paths.cache)?;
    std::fs::set_permissions(&paths.cache, std::fs::Permissions::from_mode(0o700))?;
    let cached = dir_bytes(&paths.cache);
    let need = need_bytes(cached);
    let free = free_bytes(&paths.cache)?;
    if free < need {
        return Err(OwnBackupError::Config(no_space(&paths.cache, need, free)));
    }
    let dest_free = req.out.free_bytes()?;
    if dest_free < DEST_MIN_BYTES {
        return Err(OwnBackupError::Config(no_space(
            &req.out.path,
            DEST_MIN_BYTES,
            dest_free,
        )));
    }

    progress(LinuxStage::Builder, 0, &tr!("media_linux.detail_repack"));
    let input = paths.cache.join("input");
    remove_tree(&input)?;
    std::fs::create_dir_all(&input)?;
    let deb = input.join("own-backup.deb");
    let mut warnings = Vec::new();
    let from_installed = req.deb.is_none();
    match req.deb {
        Some(mut given) => {
            let mut out = File::create(&deb)?;
            std::io::copy(&mut given, &mut out)?;
            out.sync_all()?;
        }
        None => {
            let repacked = repack_with(
                Path::new("/"),
                PACKAGE,
                &input.join("stage"),
                &deb,
                &mut |s, o| dpkg_deb_unsandboxed(s, o),
            )?;
            info!(version = %repacked.version, "installed package packed for the medium");
            if !repacked.changed_conffiles.is_empty() {
                warnings.push(
                    tr!(
                        "media_linux.conffiles_changed",
                        files = repacked.changed_conffiles.join(", ")
                    )
                    .into_owned(),
                );
            }
        }
    }
    if cancel.is_cancelled() {
        return Err(OwnBackupError::Cancelled);
    }

    let mut log = File::create(paths.log())
        .map_err(|e| warn!(error = %e, "no build log"))
        .ok();
    let args = vec![
        "build".to_string(),
        "--config".to_string(),
        paths.config.display().to_string(),
        "--deb".to_string(),
        deb.display().to_string(),
        "--work".to_string(),
        paths.cache.display().to_string(),
    ];
    let mut tracker = Tracker::new();
    let ended = run_script(&paths, &args, cancel, &mut log, &mut |line| {
        if let Some(p) = tracker.feed(line) {
            progress(p.stage, p.percent, &p.detail);
        }
    });
    drop(log);
    let ended = match ended {
        Ok(e) => e,
        Err(e) => {
            clean_build_tree(&paths);
            return Err(e);
        }
    };
    if ended.cancelled {
        // The script removed its tree on the signal; again in case it was
        // killed before it could.
        clean_build_tree(&paths);
        return Err(OwnBackupError::Cancelled);
    }
    let iso = tracker.iso.clone().map(PathBuf::from);
    let iso = match (ended.ok, iso) {
        (true, Some(iso)) if iso.starts_with(&paths.cache) && iso.is_file() => iso,
        _ => {
            let why = failure_text(&tracker, &paths);
            warn!(%why, "the medium was not built");
            clean_build_tree(&paths);
            return Err(OwnBackupError::Config(why));
        }
    };
    let version = tracker
        .version
        .clone()
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());

    let finish = |paths: &Paths| {
        clean_build_tree(paths);
        let _ = remove_tree(&input);
    };
    progress(LinuxStage::Iso, 96, &tr!("media_linux.detail_saving"));
    let saved = req.out.save(&iso, &version, cancel, &mut |pct| {
        progress(
            LinuxStage::Iso,
            96 + pct * 3 / 100,
            &tr!("media_linux.detail_saving"),
        );
    });
    let saved = match saved {
        Ok(p) => p,
        Err(e) => {
            finish(&paths);
            return Err(e);
        }
    };
    let iso_bytes = std::fs::metadata(&iso).map(|m| m.len()).unwrap_or(0);
    info!(path = %saved.display(), mb = iso_bytes >> 20, "medium saved");
    progress(LinuxStage::Iso, 100, "");

    let stick = match &req.stick {
        None => None,
        Some((device, expect)) => {
            let written = write_stick(&iso, device, expect, cancel, &mut |stage, pct| {
                let stage = match stage {
                    StickStage::Write => LinuxStage::Write,
                    StickStage::Verify => LinuxStage::Verify,
                    StickStage::Logs => LinuxStage::Logs,
                };
                progress(stage, pct, "");
            });
            match written {
                Ok(r) => Some(r),
                Err(OwnBackupError::Cancelled) => {
                    finish(&paths);
                    return Err(OwnBackupError::Cancelled);
                }
                Err(e) => {
                    finish(&paths);
                    return Err(OwnBackupError::Device(
                        tr!(
                            "media_linux.stick_failed",
                            path = saved.display(),
                            error = error_text(e)
                        )
                        .into_owned(),
                    ));
                }
            }
        }
    };
    finish(&paths);
    Ok(BuildReport {
        iso_path: saved,
        iso_bytes,
        version,
        from_installed,
        warnings,
        stick,
        cache_bytes: dir_bytes(&paths.cache),
        cache_dir: paths.cache,
    })
}

/// Why the script failed, in the person's words: its own refusal when it
/// gave one, otherwise what the tools' last lines show.
fn failure_text(tracker: &Tracker, paths: &Paths) -> String {
    if let Some((code, detail)) = &tracker.error {
        return refusal(code, detail, &tracker.apt);
    }
    match classify_failure(tracker.tail(), &mirror_host()) {
        ToolFailure::Network(host) => refusal("network", &host, &[]),
        ToolFailure::NoSpace => {
            tr!("media_linux.no_space_during", path = paths.cache.display()).into_owned()
        }
        ToolFailure::Other => {
            let last: Vec<&str> = tracker.tail().collect();
            let last = last[last.len().saturating_sub(3)..].join(" / ");
            tr!(
                "media_linux.tools_failed",
                last = last,
                log = paths.log().display()
            )
            .into_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A dpkg database under `root` with an installed `own-backup` of three
    /// files, a symlink, a configuration file and a postinst.
    fn fake_dpkg(root: &Path) {
        let w = |rel: &str, text: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, text).unwrap();
        };
        w("usr/bin/own-backup-cli", "cli binary");
        w(
            "usr/libexec/own-backup/own-backup-service",
            "service binary",
        );
        w(
            "usr/share/own-backup/media/auto/config",
            "#!/bin/sh\nlb config\n",
        );
        w(
            "etc/xdg/autostart/own-backup-tray.desktop",
            "[Desktop Entry]\n",
        );
        std::fs::set_permissions(
            root.join("usr/bin/own-backup-cli"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::os::unix::fs::symlink("own-backup-cli", root.join("usr/bin/obk")).unwrap();
        let md5 = |text: &str| -> String {
            Md5::digest(text.as_bytes())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect()
        };
        w(
            "var/lib/dpkg/status",
            &format!(
                "Package: other\nStatus: install ok installed\nVersion: 1\n\n\
                 Package: own-backup\n\
                 Status: install ok installed\n\
                 Priority: optional\n\
                 Section: admin\n\
                 Installed-Size: 12\n\
                 Maintainer: Someone <a@b>\n\
                 Architecture: amd64\n\
                 Version: 0.1.5~beta-1\n\
                 Depends: libc6 (>= 2.39), systemd\n\
                 Recommends: lvm2,\n debootstrap\n\
                 Conffiles:\n /etc/xdg/autostart/own-backup-tray.desktop {}\n /etc/old.conf 0123 obsolete\n\
                 Description: block-level backup\n Long text.\n .\n More.\n\
                 Homepage: https://example.org\n",
                md5("[Desktop Entry]\n")
            ),
        );
        w(
            "var/lib/dpkg/info/own-backup.list",
            "/.\n/usr\n/usr/bin\n/usr/bin/own-backup-cli\n/usr/bin/obk\n/usr/libexec\n\
             /usr/libexec/own-backup\n/usr/libexec/own-backup/own-backup-service\n\
             /usr/share\n/usr/share/own-backup\n/usr/share/own-backup/media\n\
             /usr/share/own-backup/media/auto\n/usr/share/own-backup/media/auto/config\n\
             /etc\n/etc/xdg\n/etc/xdg/autostart\n/etc/xdg/autostart/own-backup-tray.desktop\n",
        );
        w(
            "var/lib/dpkg/info/own-backup.md5sums",
            &format!(
                "{}  usr/bin/own-backup-cli\n{}  usr/libexec/own-backup/own-backup-service\n\
                 {}  usr/share/own-backup/media/auto/config\n{}  etc/xdg/autostart/own-backup-tray.desktop\n",
                md5("cli binary"),
                md5("service binary"),
                md5("#!/bin/sh\nlb config\n"),
                md5("[Desktop Entry]\n")
            ),
        );
        w(
            "var/lib/dpkg/info/own-backup.postinst",
            "#!/bin/sh\nexit 0\n",
        );
    }

    #[test]
    fn the_installed_package_is_read_from_dpkg_s_status() {
        let dir = tempfile::tempdir().unwrap();
        fake_dpkg(dir.path());
        let p = installed(dir.path(), PACKAGE).unwrap();
        assert_eq!(p.version, "0.1.5~beta-1");
        assert!(p
            .control
            .starts_with("Package: own-backup\nPriority: optional\n"));
        assert!(p.control.contains("Recommends: lvm2,\n debootstrap\n"));
        assert!(p
            .control
            .contains("Description: block-level backup\n Long text.\n .\n More.\n"));
        assert!(!p.control.contains("Status:"));
        assert!(!p.control.contains("Conffiles"));
        assert_eq!(p.conffiles.len(), 1);
        assert_eq!(
            p.conffiles[0].0,
            "/etc/xdg/autostart/own-backup-tray.desktop"
        );
    }

    #[test]
    fn a_package_that_is_absent_or_half_installed_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("var/lib/dpkg")).unwrap();
        std::fs::write(
            dir.path().join("var/lib/dpkg/status"),
            "Package: own-backup\nStatus: deinstall ok config-files\nVersion: 1\n",
        )
        .unwrap();
        let e = installed(dir.path(), PACKAGE).unwrap_err().to_string();
        assert!(e.contains("--deb"), "{e}");
        std::fs::write(
            dir.path().join("var/lib/dpkg/status"),
            "Package: own-backup\nStatus: install ok half-installed\nVersion: 1\n",
        )
        .unwrap();
        let e = installed(dir.path(), PACKAGE).unwrap_err().to_string();
        assert!(e.contains("half-installed"), "{e}");
    }

    #[test]
    fn the_staged_tree_is_the_installed_package() {
        let dir = tempfile::tempdir().unwrap();
        fake_dpkg(dir.path());
        let stage = dir.path().join("stage");
        let r = stage_installed(dir.path(), PACKAGE, &stage).unwrap();
        assert_eq!(r.version, "0.1.5~beta-1");
        assert!(r.changed_conffiles.is_empty());
        assert_eq!(
            std::fs::read_to_string(stage.join("usr/bin/own-backup-cli")).unwrap(),
            "cli binary"
        );
        let mode = std::fs::metadata(stage.join("usr/bin/own-backup-cli"))
            .unwrap()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
        assert_eq!(
            std::fs::read_link(stage.join("usr/bin/obk")).unwrap(),
            Path::new("own-backup-cli")
        );
        assert_eq!(
            std::fs::read_to_string(stage.join("DEBIAN/conffiles")).unwrap(),
            "/etc/xdg/autostart/own-backup-tray.desktop\n"
        );
        assert_eq!(
            std::fs::metadata(stage.join("DEBIAN")).unwrap().mode() & 0o777,
            0o755
        );
        let postinst = stage.join("DEBIAN/postinst");
        assert_eq!(std::fs::metadata(&postinst).unwrap().mode() & 0o777, 0o755);
        assert!(stage.join("DEBIAN/md5sums").is_file());
        assert!(!stage.join("DEBIAN/prerm").exists());
    }

    #[test]
    fn a_changed_program_file_refuses_and_a_changed_conffile_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        fake_dpkg(dir.path());
        std::fs::write(
            dir.path().join("etc/xdg/autostart/own-backup-tray.desktop"),
            "[Desktop Entry]\nHidden=true\n",
        )
        .unwrap();
        let r = stage_installed(dir.path(), PACKAGE, &dir.path().join("s1")).unwrap();
        assert_eq!(
            r.changed_conffiles,
            ["/etc/xdg/autostart/own-backup-tray.desktop"]
        );

        std::fs::write(
            dir.path().join("usr/libexec/own-backup/own-backup-service"),
            "patched",
        )
        .unwrap();
        let e = stage_installed(dir.path(), PACKAGE, &dir.path().join("s2"))
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("/usr/libexec/own-backup/own-backup-service"),
            "{e}"
        );

        std::fs::remove_file(dir.path().join("usr/libexec/own-backup/own-backup-service")).unwrap();
        let e = stage_installed(dir.path(), PACKAGE, &dir.path().join("s3"))
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("/usr/libexec/own-backup/own-backup-service"),
            "{e}"
        );
    }

    #[test]
    fn a_file_dpkg_has_no_checksum_for_is_not_trusted() {
        let dir = tempfile::tempdir().unwrap();
        fake_dpkg(dir.path());
        let md5sums = dir.path().join("var/lib/dpkg/info/own-backup.md5sums");
        let text = std::fs::read_to_string(&md5sums).unwrap();
        let text: String = text
            .lines()
            .filter(|l| !l.ends_with("own-backup-cli"))
            .map(|l| format!("{l}\n"))
            .collect();
        std::fs::write(&md5sums, text).unwrap();
        let e = stage_installed(dir.path(), PACKAGE, &dir.path().join("s"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("/usr/bin/own-backup-cli"), "{e}");
    }

    /// The whole repack through the real dpkg-deb, read back with it.
    #[test]
    fn the_repacked_deb_carries_the_installed_files_and_control() {
        if find_program("dpkg-deb").is_none() {
            eprintln!("dpkg-deb not installed; skipped");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        fake_dpkg(dir.path());
        let out = dir.path().join("own-backup.deb");
        let stage = dir.path().join("stage");
        let r = repack(dir.path(), PACKAGE, &stage, &out).unwrap();
        assert_eq!(r.version, "0.1.5~beta-1");
        assert!(!stage.exists(), "the stage is removed");
        let field = |name: &str| {
            let o = Command::new("dpkg-deb")
                .arg("-f")
                .arg(&out)
                .arg(name)
                .output()
                .unwrap();
            String::from_utf8(o.stdout).unwrap().trim().to_string()
        };
        assert_eq!(field("Version"), "0.1.5~beta-1");
        assert_eq!(field("Depends"), "libc6 (>= 2.39), systemd");
        let listing = Command::new("dpkg-deb")
            .arg("-c")
            .arg(&out)
            .output()
            .unwrap();
        let listing = String::from_utf8(listing.stdout).unwrap();
        assert!(listing.contains("root/root"), "{listing}");
        assert!(
            listing.contains("./usr/libexec/own-backup/own-backup-service"),
            "{listing}"
        );
        assert!(
            listing.contains("./usr/bin/obk -> own-backup-cli"),
            "{listing}"
        );
        let control = Command::new("dpkg-deb")
            .arg("-I")
            .arg(&out)
            .output()
            .unwrap();
        let control = String::from_utf8(control.stdout).unwrap();
        assert!(control.contains("postinst"), "{control}");
        assert!(control.contains("conffiles"), "{control}");
    }

    fn me() -> (u32, u32) {
        // SAFETY: plain getters.
        unsafe { (libc::geteuid(), libc::getegid()) }
    }

    #[test]
    fn the_output_folder_must_be_the_caller_s_existing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let (uid, gid) = me();
        let path = dir.path().to_str().unwrap();
        let out = open_output_dir(path, uid, gid).unwrap();
        assert_eq!(out.path, std::fs::canonicalize(dir.path()).unwrap());

        let e = open_output_dir(path, uid + 1, gid).unwrap_err().to_string();
        assert!(e.contains(out.path.to_str().unwrap()), "{e}");
        let e = open_output_dir("relative/dir", uid, gid)
            .unwrap_err()
            .to_string();
        assert!(e.contains("relative/dir"), "{e}");
        let missing = dir.path().join("missing");
        assert!(open_output_dir(missing.to_str().unwrap(), uid, gid).is_err());
        let file = dir.path().join("file");
        std::fs::write(&file, "x").unwrap();
        assert!(open_output_dir(file.to_str().unwrap(), uid, gid).is_err());
        // A symlink to the caller's folder is that folder.
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(dir.path(), &link).unwrap();
        let out = open_output_dir(link.to_str().unwrap(), uid, gid).unwrap();
        assert_eq!(out.path, std::fs::canonicalize(dir.path()).unwrap());
        // root's own folder is not the caller's.
        if uid != 0 {
            assert!(open_output_dir("/", uid, gid).is_err());
        }
    }

    #[test]
    fn saving_never_replaces_a_file_and_leaves_nothing_on_cancel() {
        let dir = tempfile::tempdir().unwrap();
        let (uid, gid) = me();
        let src_dir = tempfile::tempdir().unwrap();
        let iso = src_dir.path().join("x.hybrid.iso");
        std::fs::write(&iso, vec![7u8; 3 * CHUNK + 5]).unwrap();
        let out = open_output_dir(dir.path().to_str().unwrap(), uid, gid).unwrap();
        let mut seen = Vec::new();
        let first = out
            .save(&iso, "0.1.5~beta-1", &CancelToken::default(), &mut |p| {
                seen.push(p)
            })
            .unwrap();
        assert_eq!(first.file_name().unwrap(), "OwnBackup-Linux-0.1.5~beta.iso");
        assert_eq!(seen.last(), Some(&100));
        let meta = std::fs::metadata(&first).unwrap();
        assert_eq!((meta.uid(), meta.len()), (uid, (3 * CHUNK + 5) as u64));
        let second = out
            .save(&iso, "0.1.5~beta-1", &CancelToken::default(), &mut |_| {})
            .unwrap();
        assert_eq!(
            second.file_name().unwrap(),
            "OwnBackup-Linux-0.1.5~beta (2).iso"
        );
        let cancel = CancelToken::default();
        cancel.cancel();
        assert!(matches!(
            out.save(&iso, "0.1.5~beta-1", &cancel, &mut |_| {}),
            Err(OwnBackupError::Cancelled)
        ));
        let mut names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "OwnBackup-Linux-0.1.5~beta (2).iso",
                "OwnBackup-Linux-0.1.5~beta.iso"
            ]
        );
    }

    #[test]
    fn a_given_deb_must_be_the_caller_s_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let (uid, _) = me();
        let deb = dir.path().join("x.deb");
        std::fs::write(&deb, "deb").unwrap();
        assert!(open_callers_file(deb.to_str().unwrap(), uid).is_ok());
        assert!(open_callers_file(deb.to_str().unwrap(), uid + 1).is_err());
        let link = dir.path().join("link.deb");
        std::os::unix::fs::symlink(&deb, &link).unwrap();
        assert!(open_callers_file(link.to_str().unwrap(), uid).is_err());
        assert!(open_callers_file(dir.path().to_str().unwrap(), uid).is_err());
    }

    #[test]
    fn the_build_runs_as_a_transient_unit_with_the_mirror_settings() {
        let argv = systemd_run_argv(
            Path::new("/usr/libexec/own-backup/build-linux-media"),
            &["build".into(), "--work".into(), "/w".into()],
            &[("OBK_MEDIA_MIRROR".into(), "http://m/debian/".into())],
        );
        let joined = argv.join(" ");
        assert!(
            joined.starts_with("systemd-run --quiet --collect --wait --pipe"),
            "{joined}"
        );
        assert!(joined.contains("--unit=own-backup-media-build"), "{joined}");
        assert!(
            joined.contains("--property KillMode=control-group"),
            "{joined}"
        );
        assert!(
            joined.contains("--setenv=OBK_MEDIA_MIRROR=http://m/debian/"),
            "{joined}"
        );
        assert!(
            joined.ends_with("/usr/libexec/own-backup/build-linux-media build --work /w"),
            "{joined}"
        );
    }

    #[test]
    fn lines_end_at_carriage_returns_too() {
        let mut r = BufReader::new(&b"a\rb\nc"[..]);
        let mut lines = Vec::new();
        loop {
            let mut buf = Vec::new();
            if read_until_any(&mut r, &mut buf).unwrap() == 0 {
                break;
            }
            lines.push(String::from_utf8(buf).unwrap());
        }
        assert_eq!(lines, ["a\r", "b\n", "c"]);
    }

    #[test]
    fn tree_sizes_count_what_is_there() {
        let dir = tempfile::tempdir().unwrap();
        assert!(dir_bytes(&dir.path().join("none")) == 0);
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        std::fs::write(dir.path().join("a/b/f"), vec![1u8; 100_000]).unwrap();
        assert!(dir_bytes(dir.path()) >= 100_000);
    }

    /// The script itself, as the service runs it without systemd: a missing
    /// argument and a run as an ordinary user are refused in its own words.
    #[test]
    fn the_script_refuses_in_its_own_words() {
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../packaging/linux/media/build-media.sh");
        let run = |args: &[&str]| {
            let o = Command::new("bash")
                .arg(&script)
                .args(args)
                .output()
                .unwrap();
            String::from_utf8_lossy(&o.stdout).into_owned()
        };
        assert!(run(&["build"]).contains("OBK-ERROR usage"));
        if me().0 != 0 {
            let out = run(&["clean", "--work", "/nonexistent"]);
            assert!(out.contains("OBK-ERROR not-root"), "{out}");
        }
    }
}
