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
//! **Everything is temporary, as on Windows.** A build gets a folder of its
//! own, `/var/tmp/own-backup-media.XXXXXX` (root's, 0700; [`WORK_PARENTS`]):
//! the builder, the package cache, the repacked .deb, the ISO. When the build
//! ends — finished, failed or cancelled — the script undoes its mounts and
//! the whole folder goes; nothing stays between builds, so every build
//! downloads its packages again. Not `/tmp`: it is usually tmpfs, and a
//! build's ~10 GiB would sit in memory. Only the log of the last build stays,
//! in `/var/log/own-backup`. A folder a killed service left behind is
//! removed at its next start ([`recover_abandoned_build`]), and so is the
//! cache older versions kept in [`LEGACY_CACHE_DIR`].

#![cfg(target_os = "linux")]

use std::collections::{HashMap, HashSet};
use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use md5::{Digest, Md5};
use own_backup_common::cancel::CancelToken;
use own_backup_common::engine_dto::MediaStickIdentityDto;
use own_backup_common::media_tools::{linux_tool_packages, LINUX_MEDIA_TOOLS};
use own_backup_common::{tr, OwnBackupError, Result};
use tracing::{info, warn};

use crate::media_linux::{
    classify_failure, gib, iso_file_name, need_bytes, refusal, url_host, LinuxStage, ToolFailure,
    Tracker, DEST_MIN_BYTES,
};
use crate::media_stick_linux::{write_stick_file, StickReport, StickStage};

/// The package the medium is made of.
pub const PACKAGE: &str = "own-backup";

/// Where a build's temporary folder is made: the first of these on a file
/// system that allows programs and device files (`/var/tmp` mounted
/// `noexec` by hardening falls back to the service's state directory).
pub const WORK_PARENTS: [&str; 2] = ["/var/tmp", "/var/lib/own-backup"];

/// The temporary folder's name: this and six random characters.
const WORK_PREFIX: &str = "own-backup-media.";

/// Where versions before 22.10 kept the builder and the package cache
/// between builds; removed at the service's start.
pub const LEGACY_CACHE_DIR: &str = "/var/cache/own-backup/media";

/// The last build's log stays here; the folder of the build does not.
pub const LOG_DIR: &str = "/var/log/own-backup";
const LOG_NAME: &str = "linux-media-build.log";

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

/// The largest .deb a caller may give: ours is about 30 MiB.
pub const MAX_DEB_BYTES: u64 = 1 << 30;

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
    /// Configuration files the administrator deleted; the medium goes
    /// without them, as the installed program does.
    pub deleted_conffiles: Vec<String>,
}

/// Where a symlink of the package may point. dpkg records no target for a
/// symlink (`md5sums` covers regular files only) and the original .deb is
/// not kept, so a changed target cannot be told from the installed one;
/// what can be refused is a link that leads out of the package and the
/// system's own directories — into a home, `/root`, `/tmp` or `/proc`.
const SYMLINK_DIRS: &[&str] = &["/usr", "/etc", "/lib", "/lib64", "/bin", "/sbin", "/run"];

/// `target` of the symlink at `link`, made absolute and without `.`/`..`,
/// the way the medium's tree will resolve it (`..` stops at the root).
fn symlink_destination(link: &str, target: &Path) -> String {
    let joined = if target.is_absolute() {
        target.to_path_buf()
    } else {
        Path::new(link)
            .parent()
            .unwrap_or(Path::new("/"))
            .join(target)
    };
    let mut parts: Vec<&OsStr> = Vec::new();
    for c in joined.components() {
        match c {
            std::path::Component::Normal(n) => parts.push(n),
            std::path::Component::ParentDir => {
                parts.pop();
            }
            _ => {}
        }
    }
    let mut out = String::new();
    for p in parts {
        out.push('/');
        out.push_str(&p.to_string_lossy());
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

/// May the package's symlink at `link` point at `target`? Into the package's
/// own paths, the system's directories ([`SYMLINK_DIRS`]) or `/dev/null` (a
/// masked unit).
fn symlink_target_allowed(link: &str, target: &Path, own: &HashSet<&str>) -> bool {
    let dest = symlink_destination(link, target);
    own.contains(dest.as_str())
        || dest == "/dev/null"
        || SYMLINK_DIRS
            .iter()
            .any(|d| dest == *d || dest.strip_prefix(d).is_some_and(|r| r.starts_with('/')))
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

    let own: HashSet<&str> = list.lines().map(str::trim).collect();

    std::fs::create_dir_all(stage)?;
    let mut modified: Vec<String> = Vec::new();
    let mut changed_conffiles = Vec::new();
    let mut deleted_conffiles: Vec<String> = Vec::new();
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
                // A configuration file is the administrator's to delete (a
                // tray that should not start, say); a program file is not.
                if conffiles.contains_key(path) {
                    deleted_conffiles.push(path.to_string());
                } else {
                    modified.push(path.to_string());
                }
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
            let target = std::fs::read_link(&src)?;
            if !symlink_target_allowed(path, &target, &own) {
                modified.push(format!("{path} -> {}", target.display()));
                continue;
            }
            std::os::unix::fs::symlink(&target, &dst)?;
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
    // A deleted configuration file is not in the package: dpkg-deb refuses a
    // conffile it cannot find, and dpkg --verify would miss its checksum.
    let deleted: HashSet<&str> = deleted_conffiles.iter().map(String::as_str).collect();
    let md5sums_kept: String = md5sums_text
        .lines()
        .filter(|l| {
            l.split_once(char::is_whitespace)
                .map(|(_, p)| format!("/{}", p.trim_start()))
                .is_none_or(|p| !deleted.contains(p.as_str()))
        })
        .map(|l| format!("{l}\n"))
        .collect();
    if !md5sums_kept.is_empty() {
        write("md5sums", &md5sums_kept, 0o644)?;
    }
    let conffiles_kept: String = pkg
        .conffiles
        .iter()
        .filter(|(p, _)| !deleted.contains(p.as_str()))
        .map(|(p, _)| format!("{p}\n"))
        .collect();
    if !conffiles_kept.is_empty() {
        write("conffiles", &conffiles_kept, 0o644)?;
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
        deleted_conffiles,
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

    /// Copy `input` (read from its start) into the folder as
    /// `OwnBackup-Linux-<version>.iso` (or ` (2)` and so on when that
    /// exists), the caller's own file. `progress` gets the percentage
    /// copied. Nothing is left behind on failure.
    pub fn save(
        &self,
        input: &File,
        version: &str,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(u64),
    ) -> Result<PathBuf> {
        let len = input.metadata()?.len();
        let mut input = input;
        input.seek(SeekFrom::Start(0))?;
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

/// May this caller give a .deb of its own? The package's maintainer scripts
/// run as root inside the build (`lb build`, with the host's `/proc`, `/sys`
/// and `/dev` bound into the builder), so a .deb is root's code: it is taken
/// from root (the CLI under sudo) over the socket, or from anyone reaching a
/// development service run in the foreground over loopback TCP
/// (`over_socket` false), which is not a protected engine anyway.
pub fn given_deb_allowed(over_socket: bool, uid: u32) -> Result<()> {
    if !over_socket || uid == 0 {
        return Ok(());
    }
    Err(OwnBackupError::Config(
        tr!("media_linux.deb_root_only").into_owned(),
    ))
}

/// A file the caller named (a development .deb): a regular file of at most
/// [`MAX_DEB_BYTES`], opened without following a symlink and without
/// blocking on a FIFO or taking a terminal, owned by the caller unless the
/// caller is root. Whether this caller may give one at all is
/// [`given_deb_allowed`].
pub fn open_callers_file(path: &str, uid: u32) -> Result<File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC)
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
    if meta.len() > MAX_DEB_BYTES {
        return Err(deb_too_large(path));
    }
    Ok(file)
}

fn deb_too_large(path: &str) -> OwnBackupError {
    OwnBackupError::Config(
        tr!(
            "media_linux.deb_too_large",
            path = path,
            max = MAX_DEB_BYTES >> 20
        )
        .into_owned(),
    )
}

/// Copy a given .deb to `out`, refusing one that grew past
/// [`MAX_DEB_BYTES`] since it was checked.
fn copy_given_deb(given: &mut File, out: &Path) -> Result<()> {
    let mut dst = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(out)?;
    let copied = std::io::copy(&mut given.take(MAX_DEB_BYTES + 1), &mut dst)?;
    if copied > MAX_DEB_BYTES {
        drop(dst);
        let _ = std::fs::remove_file(out);
        return Err(deb_too_large(&out.display().to_string()));
    }
    dst.sync_all()?;
    Ok(())
}

/// `name` in the directory `dir`, opened without following a symlink.
fn open_at(dir: &File, name: &OsStr, flags: libc::c_int) -> std::io::Result<File> {
    let name = CString::new(name.as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    // SAFETY: openat on a held directory descriptor and a C string; the new
    // descriptor is owned by the File below.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fd is a fresh descriptor nobody else owns.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// The ISO the script reported (`OBK-ISO <path>`), opened for reading — or a
/// refusal when it is not a regular file directly in `<work>/builder/build`.
/// The build runs packages' maintainer scripts as root, so its output is not
/// trusted to name a file: the path must be that directory plus one plain
/// name (no `..`, no further component), and `builder`, `build` and the
/// file are each opened from the one before without following a symlink. A
/// link to another root file — or a directory swapped for one — is refused
/// rather than copied into the person's folder or onto a stick.
pub fn open_built_iso(work: &Path, reported: &str) -> Result<File> {
    let outside =
        || OwnBackupError::Config(tr!("media_linux.iso_outside", path = reported).into_owned());
    let path = Path::new(reported);
    // `Path` drops a `.` inside the path (the same directory) but keeps `..`.
    let plain = path.is_absolute()
        && path.components().all(|c| {
            matches!(
                c,
                std::path::Component::RootDir | std::path::Component::Normal(_)
            )
        });
    let name = path.file_name().ok_or_else(outside)?;
    if !plain
        || path.parent() != Some(work.join("builder").join("build").as_path())
        || !name.as_bytes().ends_with(b".iso")
    {
        return Err(outside());
    }
    let unreadable = |e: std::io::Error| {
        OwnBackupError::Config(
            tr!("media_linux.iso_unreadable", path = reported, error = e).into_owned(),
        )
    };
    let top = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(work)
        .map_err(unreadable)?;
    let dir_flags = libc::O_RDONLY | libc::O_DIRECTORY;
    let opened = open_at(&top, OsStr::new("builder"), dir_flags)
        .and_then(|b| open_at(&b, OsStr::new("build"), dir_flags))
        .and_then(|b| open_at(&b, name, libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOCTTY));
    let file = match opened {
        Ok(f) => f,
        // A symlink where a directory or the file should be.
        Err(e) if matches!(e.raw_os_error(), Some(libc::ELOOP) | Some(libc::ENOTDIR)) => {
            return Err(outside())
        }
        Err(e) => return Err(unreadable(e)),
    };
    if !file.metadata().map_err(unreadable)?.is_file() {
        return Err(outside());
    }
    Ok(file)
}

// --- the build's folder -----------------------------------------------------------------

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

/// Where the build's script and configuration are, where its temporary
/// folder is made and where its log goes: the package's places, or (a
/// development service) `OBK_MEDIA_SCRIPT`, `OBK_MEDIA_CONFIG`,
/// `OBK_MEDIA_TMP` and `OBK_MEDIA_LOG_DIR` — the service's own environment,
/// set by root.
#[derive(Debug, Clone)]
pub struct Paths {
    pub script: PathBuf,
    pub config: PathBuf,
    /// The folder a build's temporary folder is made in.
    pub work_parent: PathBuf,
    pub log_dir: PathBuf,
}

impl Paths {
    pub fn locate() -> Paths {
        let env = |name: &str| {
            std::env::var_os(name)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        Paths {
            script: env("OBK_MEDIA_SCRIPT").unwrap_or_else(|| PathBuf::from(SCRIPT)),
            config: env("OBK_MEDIA_CONFIG").unwrap_or_else(|| PathBuf::from(CONFIG_DIR)),
            work_parent: env("OBK_MEDIA_TMP").unwrap_or_else(work_parent),
            log_dir: env("OBK_MEDIA_LOG_DIR").unwrap_or_else(|| PathBuf::from(LOG_DIR)),
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
        self.log_dir.join(LOG_NAME)
    }

    /// Where temporary folders of builds may be: this run's parent and the
    /// package's, which a development run may have changed.
    fn work_parents(&self) -> Vec<PathBuf> {
        let mut out = vec![self.work_parent.clone()];
        for p in WORK_PARENTS.map(PathBuf::from) {
            if !out.contains(&p) {
                out.push(p);
            }
        }
        out
    }
}

/// The first of [`WORK_PARENTS`] whose file system lets the builder run
/// programs and use device files; the first one when none does (the build
/// then fails in the tools' words rather than not starting).
fn work_parent() -> PathBuf {
    WORK_PARENTS
        .iter()
        .map(PathBuf::from)
        .find(|p| usable_for_build(p))
        .unwrap_or_else(|| PathBuf::from(WORK_PARENTS[0]))
}

fn usable_for_build(dir: &Path) -> bool {
    let Ok(c) = CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: a valid C string and a zeroed out-structure.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return false;
    }
    st.f_flag & (libc::ST_NOEXEC | libc::ST_NODEV | libc::ST_RDONLY) == 0
}

/// A new temporary folder for one build: root's, 0700, a name nobody could
/// have made beforehand (`mkdir` of a random name, failing if it exists — the
/// parent is world-writable).
fn make_work_dir(parent: &Path) -> Result<PathBuf> {
    let dir = tempfile::Builder::new()
        .prefix(WORK_PREFIX)
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in(parent)
        .map_err(|e| {
            OwnBackupError::Config(
                tr!(
                    "media_linux.work_failed",
                    path = parent.display(),
                    error = e
                )
                .into_owned(),
            )
        })?;
    // Never removed by `TempDir`'s drop: `remove_dir_all` would go down into
    // the builder's bind mounts (/dev, /proc, /sys). The script removes it.
    Ok(dir.keep())
}

/// Is `path` a temporary folder of a build: a real directory (not a
/// symlink) named [`WORK_PREFIX`]…, owned by root — which nobody else can
/// make in a world-writable folder.
fn is_work_dir(path: &Path) -> bool {
    let named = path
        .file_name()
        .is_some_and(|n| n.as_bytes().starts_with(WORK_PREFIX.as_bytes()));
    named && std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir() && m.uid() == 0)
}

/// The folder of the build this service is running now: not a leftover.
static RUNNING: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// What builds left behind: temporary folders of builds that did not end
/// (a killed service) and the cache of older versions.
fn leftovers(paths: &Paths) -> Vec<PathBuf> {
    let running = RUNNING.lock().map(|r| r.clone()).unwrap_or(None);
    let mut out = Vec::new();
    for parent in paths.work_parents() {
        let Ok(entries) = std::fs::read_dir(&parent) else {
            continue;
        };
        out.extend(
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| is_work_dir(p) && Some(p) != running.as_ref()),
        );
    }
    if std::fs::symlink_metadata(LEGACY_CACHE_DIR).is_ok_and(|m| m.is_dir()) {
        out.push(PathBuf::from(LEGACY_CACHE_DIR));
    }
    out
}

/// Remove a build's folder: the script undoes whatever is mounted under it
/// first (`rm` would go down into the builder's /dev and /proc), then
/// removes it. Says whether it is gone.
fn remove_work(paths: &Paths, work: &Path) -> bool {
    let args = vec![
        "purge".to_string(),
        "--work".to_string(),
        work.display().to_string(),
    ];
    let mut log = None;
    if let Err(e) = run_script(
        paths,
        &args,
        &CancelToken::default(),
        &mut log,
        &mut |l| info!(target: "media", "{l}"),
    ) {
        warn!(error = %e, path = %work.display(), "could not remove the medium's build folder");
    }
    // The old cache's own parent, when nothing else of ours is in it.
    if work == Path::new(LEGACY_CACHE_DIR) {
        let _ = std::fs::remove_dir(Path::new(LEGACY_CACHE_DIR).parent().unwrap_or(work));
    }
    !work.exists()
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
    /// Where the build's temporary folder will be made.
    pub cache_dir: PathBuf,
    /// What earlier builds left behind (removed before the next one).
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
    let left = leftovers(&paths);
    let cache_bytes = left.iter().map(|p| dir_bytes(p)).sum();
    // What is left in the same folder is removed before the build starts.
    let reclaimed: u64 = left
        .iter()
        .filter(|p| p.parent() == Some(paths.work_parent.as_path()))
        .map(|p| dir_bytes(p))
        .sum();
    let free = free_bytes(&paths.work_parent).unwrap_or(0);
    let need = need_bytes(0);
    if free.saturating_add(reclaimed) < need {
        problems.push(no_space(&paths.work_parent, need, free));
    }
    Status {
        installed_version,
        cache_dir: paths.work_parent,
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

/// What the build unit [`UNIT`], by its `ActiveState`, needs when the
/// service starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Leftover {
    /// No unit: nothing to do.
    Nothing,
    /// Still running, starting or stopping: stop it, then clean.
    Stop,
    /// Ended in failure and kept by systemd: forget it, then clean.
    Reset,
}

pub fn leftover(active_state: &str) -> Leftover {
    match active_state.trim() {
        // systemctl says "inactive" for a unit it does not know.
        "" | "inactive" => Leftover::Nothing,
        "failed" => Leftover::Reset,
        _ => Leftover::Stop,
    }
}

/// At the service's start: a build unit an earlier service left behind (it
/// was killed, or an upgrade stopped it mid-build — the unit is not in the
/// service's cgroup and outlives it) is stopped, its trap undoing its
/// mounts; then every build folder left behind, and the cache of older
/// versions, is removed. Says whether there was anything to clean.
pub fn recover_abandoned_build() -> bool {
    let mut found = false;
    if have_systemd() {
        let unit = format!("{UNIT}.service");
        let state = Command::new("systemctl")
            .args(["show", "--property=ActiveState", "--value", &unit])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        match leftover(&state) {
            Leftover::Nothing => {}
            Leftover::Stop => {
                found = true;
                systemctl(&["stop", &unit]);
                systemctl(&["reset-failed", &unit]);
            }
            Leftover::Reset => {
                found = true;
                systemctl(&["reset-failed", &unit]);
            }
        }
    }
    let paths = Paths::locate();
    let left = leftovers(&paths);
    if !left.is_empty() && paths.script.is_file() {
        found = true;
        for dir in &left {
            remove_work(&paths, dir);
        }
    }
    found
}

/// Remove what earlier builds left behind (see [`recover_abandoned_build`]).
/// Returns the bytes freed.
pub fn clear_cache() -> Result<u64> {
    let paths = Paths::locate();
    paths.check()?;
    let mut freed = 0u64;
    let mut kept = Vec::new();
    for dir in leftovers(&paths) {
        let before = dir_bytes(&dir);
        if remove_work(&paths, &dir) {
            freed += before;
        } else {
            freed += before.saturating_sub(dir_bytes(&dir));
            kept.push(dir.display().to_string());
        }
    }
    if !kept.is_empty() {
        return Err(OwnBackupError::Config(
            tr!("media_linux.clear_failed", path = kept.join(", ")).into_owned(),
        ));
    }
    Ok(freed)
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
    /// Ids of [`LINUX_MEDIA_TOOLS`] to install on the medium.
    pub tools: Vec<String>,
}

/// What the build did.
#[derive(Debug, Clone)]
pub struct BuildReport {
    pub iso_path: PathBuf,
    pub iso_bytes: u64,
    pub version: String,
    /// Made from the installed package (not a .deb that was given).
    pub from_installed: bool,
    /// Names of the tools the medium carries.
    pub tools: Vec<String>,
    pub warnings: Vec<String>,
    pub stick: Option<StickReport>,
    pub cache_dir: PathBuf,
    pub cache_bytes: u64,
}

/// Build the medium; `progress` gets the stage, its percentage and a word
/// about it. Everything is done in a temporary folder that is removed when
/// the build ends, however it ends.
pub fn build(
    req: BuildRequest,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(LinuxStage, u64, &str),
) -> Result<BuildReport> {
    // Before anything is touched: a name that is not ours never reaches the
    // package list a root build installs from.
    linux_tool_packages(&req.tools).map_err(|id| {
        OwnBackupError::Config(tr!("media_linux.unknown_tool", tool = id).into_owned())
    })?;
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

    // What earlier builds left (a killed service) goes first: its space is
    // this build's.
    for dir in leftovers(&paths) {
        remove_work(&paths, &dir);
    }
    // Space first: a build that runs out halfway wastes ten minutes.
    let need = need_bytes(0);
    let free = free_bytes(&paths.work_parent)?;
    if free < need {
        return Err(OwnBackupError::Config(no_space(
            &paths.work_parent,
            need,
            free,
        )));
    }
    let dest_free = req.out.free_bytes()?;
    if dest_free < DEST_MIN_BYTES {
        return Err(OwnBackupError::Config(no_space(
            &req.out.path,
            DEST_MIN_BYTES,
            dest_free,
        )));
    }

    let work = make_work_dir(&paths.work_parent)?;
    info!(path = %work.display(), "the medium is built in a temporary folder");
    if let Ok(mut r) = RUNNING.lock() {
        *r = Some(work.clone());
    }
    let built = build_in(req, &paths, &work, cancel, progress);
    if let Ok(mut r) = RUNNING.lock() {
        *r = None;
    }
    if !remove_work(&paths, &work) {
        warn!(path = %work.display(), "the medium's build folder was not removed completely");
    }
    built
}

fn build_in(
    req: BuildRequest,
    paths: &Paths,
    work: &Path,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(LinuxStage, u64, &str),
) -> Result<BuildReport> {
    progress(LinuxStage::Builder, 0, &tr!("media_linux.detail_repack"));
    let input = work.join("input");
    std::fs::create_dir_all(&input)?;
    let deb = input.join("own-backup.deb");
    let mut warnings = Vec::new();
    let from_installed = req.deb.is_none();
    match req.deb {
        Some(mut given) => copy_given_deb(&mut given, &deb)?,
        None => {
            let repacked = repack_with(
                Path::new("/"),
                PACKAGE,
                &input.join("stage"),
                &deb,
                &mut |s, o| dpkg_deb_unsandboxed(s, o),
            )?;
            info!(version = %repacked.version, "installed package packed for the medium");
            if !repacked.deleted_conffiles.is_empty() {
                warnings.push(
                    tr!(
                        "media_linux.conffiles_deleted",
                        files = repacked.deleted_conffiles.join(", ")
                    )
                    .into_owned(),
                );
            }
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

    let mut log = std::fs::create_dir_all(&paths.log_dir)
        .and_then(|_| File::create(paths.log()))
        .map_err(|e| warn!(error = %e, "no build log"))
        .ok();
    let packages = linux_tool_packages(&req.tools).map_err(|id| {
        OwnBackupError::Config(tr!("media_linux.unknown_tool", tool = id).into_owned())
    })?;
    let args = script_args(&paths.config, &deb, work, &packages);
    let tools: Vec<String> = LINUX_MEDIA_TOOLS
        .iter()
        .filter(|t| req.tools.iter().any(|id| id == t.id))
        .map(|t| t.name.to_string())
        .collect();
    if !tools.is_empty() {
        info!(tools = %tools.join(", "), packages = %packages.join(" "), "tools for the medium");
    }
    let mut tracker = Tracker::new();
    let ended = run_script(paths, &args, cancel, &mut log, &mut |line| {
        if let Some(p) = tracker.feed(line) {
            progress(p.stage, p.percent, &p.detail);
        }
    });
    drop(log);
    let ended = ended?;
    if ended.cancelled {
        return Err(OwnBackupError::Cancelled);
    }
    let (iso_path, iso) = match (ended.ok, tracker.iso.clone()) {
        (true, Some(reported)) => match open_built_iso(work, &reported) {
            Ok(file) => (PathBuf::from(reported), file),
            Err(e) => {
                warn!(error = %e, "the build's ISO was refused");
                return Err(e);
            }
        },
        _ => {
            let why = failure_text(&tracker, paths);
            warn!(%why, "the medium was not built");
            return Err(OwnBackupError::Config(why));
        }
    };
    let version = tracker
        .version
        .clone()
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());

    progress(LinuxStage::Iso, 96, &tr!("media_linux.detail_saving"));
    let saved = req.out.save(&iso, &version, cancel, &mut |pct| {
        progress(
            LinuxStage::Iso,
            96 + pct * 3 / 100,
            &tr!("media_linux.detail_saving"),
        );
    });
    let saved = saved?;
    let iso_bytes = iso.metadata().map(|m| m.len()).unwrap_or(0);
    info!(path = %saved.display(), mb = iso_bytes >> 20, "medium saved");
    progress(LinuxStage::Iso, 100, "");

    let stick = match &req.stick {
        None => None,
        Some((device, expect)) => {
            let written = write_stick_file(
                &iso,
                &iso_path,
                device,
                expect,
                cancel,
                &mut |stage, pct| {
                    let stage = match stage {
                        StickStage::Write => LinuxStage::Write,
                        StickStage::Verify => LinuxStage::Verify,
                        StickStage::Logs => LinuxStage::Logs,
                    };
                    progress(stage, pct, "");
                },
            );
            match written {
                Ok(r) => Some(r),
                Err(OwnBackupError::Cancelled) => return Err(OwnBackupError::Cancelled),
                Err(e) => {
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
    Ok(BuildReport {
        iso_path: saved,
        iso_bytes,
        version,
        from_installed,
        tools,
        warnings,
        stick,
        // Nothing stays: the folder is removed as this returns.
        cache_bytes: 0,
        cache_dir: paths.work_parent.clone(),
    })
}

/// The build script's arguments. `--packages` only when there are any: an
/// older script (a development checkout's) knows nothing of it, and a build
/// without tools must not fail on it.
fn script_args(config: &Path, deb: &Path, work: &Path, packages: &[&str]) -> Vec<String> {
    let mut args = vec![
        "build".to_string(),
        "--config".to_string(),
        config.display().to_string(),
        "--deb".to_string(),
        deb.display().to_string(),
        "--work".to_string(),
        work.display().to_string(),
    ];
    if !packages.is_empty() {
        args.push("--packages".to_string());
        args.push(packages.join(" "));
    }
    args
}

/// Why the script failed, in the person's words: its own refusal when it
/// gave one, otherwise what the tools' last lines show.
fn failure_text(tracker: &Tracker, paths: &Paths) -> String {
    if let Some((code, detail)) = &tracker.error {
        return refusal(code, detail, &tracker.apt);
    }
    match classify_failure(tracker.tail(), &mirror_host()) {
        ToolFailure::Network(host) => refusal("network", &host, &[]),
        ToolFailure::NoSpace => tr!(
            "media_linux.no_space_during",
            path = paths.work_parent.display()
        )
        .into_owned(),
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
        let iso = File::open(&iso).unwrap();
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
        // A FIFO neither blocks the open nor passes as a package.
        let fifo = dir.path().join("fifo.deb");
        let c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: mkfifo with a C string.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let e = open_callers_file(fifo.to_str().unwrap(), uid)
            .unwrap_err()
            .to_string();
        assert!(e.contains("fifo.deb"), "{e}");
        // Larger than any package of ours (sparse: no space taken).
        let big = dir.path().join("big.deb");
        File::create(&big)
            .unwrap()
            .set_len(MAX_DEB_BYTES + 1)
            .unwrap();
        let e = open_callers_file(big.to_str().unwrap(), uid)
            .unwrap_err()
            .to_string();
        assert!(e.contains(&format!("{}", MAX_DEB_BYTES >> 20)), "{e}");
        let exact = dir.path().join("exact.deb");
        File::create(&exact)
            .unwrap()
            .set_len(MAX_DEB_BYTES)
            .unwrap();
        assert!(open_callers_file(exact.to_str().unwrap(), uid).is_ok());
    }

    #[test]
    fn a_given_deb_is_taken_from_root_or_a_foreground_run_only() {
        assert!(given_deb_allowed(true, 0).is_ok(), "the CLI under sudo");
        assert!(given_deb_allowed(false, 1000).is_ok(), "loopback TCP");
        assert!(given_deb_allowed(false, 0).is_ok());
        let e = given_deb_allowed(true, 1000).unwrap_err().to_string();
        assert!(e.contains("--deb"), "{e}");
    }

    #[test]
    fn a_given_deb_that_grew_past_the_limit_is_not_copied() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("x.deb");
        std::fs::write(&src, b"package").unwrap();
        let out = dir.path().join("copy.deb");
        copy_given_deb(&mut File::open(&src).unwrap(), &out).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"package");
        let big = dir.path().join("big.deb");
        File::create(&big)
            .unwrap()
            .set_len(MAX_DEB_BYTES + 1)
            .unwrap();
        let out2 = dir.path().join("copy2.deb");
        assert!(copy_given_deb(&mut File::open(&big).unwrap(), &out2).is_err());
        assert!(!out2.exists());
    }

    /// `<cache>/builder/build` with a finished image in it.
    fn fake_build(cache: &Path) -> PathBuf {
        let build = cache.join("builder/build");
        std::fs::create_dir_all(&build).unwrap();
        std::fs::write(build.join("live-image-amd64.hybrid.iso"), b"iso").unwrap();
        build
    }

    #[test]
    fn the_built_iso_is_opened_only_directly_in_the_build_tree() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let build = fake_build(&cache);
        let good = build.join("live-image-amd64.hybrid.iso");
        let mut text = String::new();
        open_built_iso(&cache, good.to_str().unwrap())
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "iso");

        let refused = |reported: &str| match open_built_iso(&cache, reported) {
            Ok(_) => panic!("{reported} was accepted"),
            Err(e) => e.to_string(),
        };
        // Another root file the build named, with and without `..`.
        let secret = dir.path().join("secret.iso");
        std::fs::write(&secret, b"secret").unwrap();
        refused(secret.to_str().unwrap());
        refused(&format!("{}/../../../secret.iso", build.display()));
        // `.` names the same directory (Path drops it); `..` never passes.
        assert!(open_built_iso(
            &cache,
            &format!("{}/./live-image-amd64.hybrid.iso", build.display())
        )
        .is_ok());
        refused(&format!(
            "{}/../build/live-image-amd64.hybrid.iso",
            build.display()
        ));
        refused(&format!("{}/sub/x.iso", build.display()));
        refused("live-image-amd64.hybrid.iso");
        refused(&format!("{}/build.log", build.display()));
        // A symlink in the build tree to that file.
        let link = build.join("link.hybrid.iso");
        std::os::unix::fs::symlink(&secret, &link).unwrap();
        let e = refused(link.to_str().unwrap());
        assert!(e.contains("link.hybrid.iso"), "{e}");
        // A directory with the image's name.
        std::fs::create_dir(build.join("dir.iso")).unwrap();
        refused(&format!("{}/dir.iso", build.display()));
        // The build directory itself swapped for a symlink elsewhere.
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("live-image-amd64.hybrid.iso"), b"x").unwrap();
        std::fs::rename(&build, dir.path().join("old-build")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &build).unwrap();
        refused(good.to_str().unwrap());
    }

    #[test]
    fn every_build_gets_a_new_private_folder_and_only_root_s_count_as_left() {
        let dir = tempfile::tempdir().unwrap();
        let a = make_work_dir(dir.path()).unwrap();
        let b = make_work_dir(dir.path()).unwrap();
        assert_ne!(a, b);
        assert!(a
            .file_name()
            .unwrap()
            .as_bytes()
            .starts_with(WORK_PREFIX.as_bytes()));
        let meta = std::fs::metadata(&a).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o700);
        // The folder outlives the call: the script removes it, not a drop.
        assert!(a.is_dir() && b.is_dir());
        // A symlink with our name, and a folder with another, are never
        // taken for a build's; ours counts only when root made it.
        let link = dir.path().join(format!("{WORK_PREFIX}link"));
        std::os::unix::fs::symlink("/", &link).unwrap();
        assert!(!is_work_dir(&link));
        std::fs::create_dir(dir.path().join("other")).unwrap();
        assert!(!is_work_dir(&dir.path().join("other")));
        assert_eq!(is_work_dir(&a), me().0 == 0);
        let e = make_work_dir(&dir.path().join("none")).unwrap_err();
        assert!(e.to_string().contains("none"), "{e}");
    }

    #[test]
    fn a_leftover_build_unit_is_stopped_or_reset_at_start() {
        assert_eq!(leftover("inactive\n"), Leftover::Nothing);
        assert_eq!(leftover(""), Leftover::Nothing);
        assert_eq!(leftover("active\n"), Leftover::Stop);
        assert_eq!(leftover("activating"), Leftover::Stop);
        assert_eq!(leftover("deactivating"), Leftover::Stop);
        assert_eq!(leftover("failed\n"), Leftover::Reset);
    }

    #[test]
    fn symlinks_may_point_into_the_package_or_the_system_only() {
        let own: HashSet<&str> = ["/usr/bin/own-backup-cli", "/opt/own/x"].into();
        let ok = |link: &str, target: &str| symlink_target_allowed(link, Path::new(target), &own);
        assert!(ok("/usr/bin/obk", "own-backup-cli"));
        assert!(ok("/opt/own/y", "x"), "the package's own path");
        assert!(ok("/usr/lib/x/libfoo.so", "../../lib/libfoo.so.1"));
        assert!(ok("/etc/systemd/system/foo.service", "/dev/null"));
        assert!(ok("/usr/bin/a", "/usr/libexec/own-backup/b"));
        assert!(!ok("/usr/bin/a", "/home/someone/evil"));
        assert!(!ok("/usr/bin/a", "/root/.ssh/id_rsa"));
        assert!(!ok("/usr/bin/a", "../../../../tmp/x"));
        assert!(!ok("/usr/bin/a", "/proc/self/root/etc/shadow"));
        assert!(!ok("/usr/bin/a", "/usrx/y"));
        assert!(
            !ok("/opt/own/y", "z"),
            "beside the package's path is not it"
        );
        assert_eq!(
            symlink_destination("/usr/bin/a", Path::new("../../../../x")),
            "/x"
        );
    }

    #[test]
    fn a_symlink_out_of_the_system_refuses_the_repack() {
        let dir = tempfile::tempdir().unwrap();
        fake_dpkg(dir.path());
        let link = dir.path().join("usr/bin/obk");
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink("/home/someone/obk", &link).unwrap();
        let e = stage_installed(dir.path(), PACKAGE, &dir.path().join("s"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("/usr/bin/obk -> /home/someone/obk"), "{e}");
    }

    #[test]
    fn a_deleted_conffile_is_a_warning_and_leaves_the_package() {
        let dir = tempfile::tempdir().unwrap();
        fake_dpkg(dir.path());
        std::fs::remove_file(dir.path().join("etc/xdg/autostart/own-backup-tray.desktop")).unwrap();
        let stage = dir.path().join("s");
        let r = stage_installed(dir.path(), PACKAGE, &stage).unwrap();
        assert_eq!(
            r.deleted_conffiles,
            ["/etc/xdg/autostart/own-backup-tray.desktop"]
        );
        assert!(r.changed_conffiles.is_empty());
        assert!(!stage.join("DEBIAN/conffiles").exists());
        let md5sums = std::fs::read_to_string(stage.join("DEBIAN/md5sums")).unwrap();
        assert!(!md5sums.contains("own-backup-tray"), "{md5sums}");
        assert!(md5sums.contains("usr/bin/own-backup-cli"), "{md5sums}");
    }

    #[test]
    fn the_tools_reach_the_script_as_one_list_of_packages() {
        let (c, d, w) = (Path::new("/c"), Path::new("/w/in.deb"), Path::new("/w"));
        let plain = script_args(c, d, w, &[]);
        assert_eq!(
            plain,
            [
                "build",
                "--config",
                "/c",
                "--deb",
                "/w/in.deb",
                "--work",
                "/w"
            ]
        );
        let packages = linux_tool_packages(&["krusader"]).unwrap();
        let with = script_args(c, d, w, &packages);
        assert_eq!(with[..7], plain[..]);
        assert_eq!(
            with[7..],
            [
                "--packages",
                "krusader qt6-wayland breeze-icon-theme qt6-svg-plugins"
            ]
        );
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
