//! Removing the names of excluded files from a finished NTFS image on Linux
//! (plan §6 stage 2, decision 8).
//!
//! The MFT scan keeps the DATA of excluded files out of the copy; their
//! catalog entries come along with the rest of the MFT. Windows deletes those
//! entries afterwards with its own NTFS driver, inside the image
//! (`true_removal_win`). Here the driver is ntfs-3g — mature write support,
//! entirely in userspace — and the rest is what the Windows qcow2 pass does:
//!
//! 1. the finished image is served through a scratch overlay
//!    ([`crate::image_fold`]): every write lands in the scratch, the image is
//!    not touched;
//! 2. each NTFS volume of it is exposed as a writable block device — our own
//!    NBD server over its window of the scratch ([`NbdExposer`]);
//! 3. ntfs-3g mounts it (foreground, so the end of its process is the end of
//!    its writes), the recorded paths are deleted through the mount without
//!    ever following a link, the volume is unmounted, the device flushed,
//!    `ntfsfix -n` reads it back;
//! 4. only when every volume went through cleanly is the change folded into
//!    the image and the touched blocks re-checksummed.
//!
//! Anything that goes wrong after a volume was mounted leaves the image as it
//! was written (zero-filled entries, reported as kept). A volume that could
//! not be mounted at all — hibernated Windows, no ntfs-3g — wrote nothing; its
//! paths are counted as not deleted and the other volumes go on.
//!
//! No NTFS structure is parsed or written by this code; ntfs-3g does the
//! deleting, our code only reads the boot sector to size a volume of a disk.

#![cfg(target_os = "linux")]

use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use own_backup_common::{OwnBackupError, Result};

use crate::engine_core::platform::{FinishedImage, VolumeSpan};
use crate::exclusions::ExcludedPath;
use crate::image_fold::{change_through_scratch, ScratchWindow, SharedScratch};
use crate::sidecar::BlockChecksum;

/// Where the volumes are mounted while their names are deleted. Not under
/// `/run/own-backup/mounts`: nothing here is the user's to browse.
pub(crate) const MOUNT_BASE: &str = "/run/own-backup/ntfs-names";

/// How long ntfs-3g may take to mount, and to write everything back and exit
/// once unmounted.
const MOUNT_TIMEOUT: Duration = Duration::from_secs(60);
const EXIT_TIMEOUT: Duration = Duration::from_secs(300);

fn dev(msg: impl std::fmt::Display) -> OwnBackupError {
    OwnBackupError::Device(format!("NTFS name removal: {msg}"))
}

/// Serves a window of the scratch as a block device for as long as `f` runs.
/// When it returns `Ok`, everything `f` had written through the device is in
/// the window.
pub(crate) trait VolumeExposer: Send + Sync {
    fn expose(
        &self,
        window: ScratchWindow,
        f: &mut dyn FnMut(&str) -> Result<(u64, u64)>,
    ) -> Result<(u64, u64)>;
}

/// The production exposer: our NBD server, writable, over the window.
pub(crate) struct NbdExposer;

impl VolumeExposer for NbdExposer {
    fn expose(
        &self,
        window: ScratchWindow,
        f: &mut dyn FnMut(&str) -> Result<(u64, u64)>,
    ) -> Result<(u64, u64)> {
        let nbd = crate::nbd_linux::NbdAttachment::attach_rw(Box::new(window))?;
        let result = f(&nbd.device);
        // Whatever `f` did, the cache goes back into the scratch before the
        // device does — an error from either makes the whole pass void.
        let flushed = nbd.flush();
        let detached = nbd.detach();
        let counts = result?;
        flushed?;
        detached?;
        Ok(counts)
    }
}

/// One volume of the image whose names go.
pub(crate) struct NamedVolume<'a> {
    /// Where it starts in the image.
    pub(crate) offset: u64,
    /// Its length, when the caller knows it; otherwise the boot sector says.
    pub(crate) len: Option<u64>,
    pub(crate) paths: &'a [ExcludedPath],
}

/// Delete the catalog entries of `volumes` from the finished image. The
/// platform's answer: `Some(n)` — the change is in the image and `n` entries
/// could not be deleted; `None` — the image is as it was written (logged why).
pub(crate) fn remove_names(
    image: &FinishedImage<'_>,
    grid: VolumeSpan,
    volumes: &[NamedVolume<'_>],
    checksums: &mut Vec<BlockChecksum>,
    tick: &mut dyn FnMut(u64, u64),
    exposer: &dyn VolumeExposer,
    mount_base: &Path,
) -> Result<Option<u64>> {
    let total: u64 = volumes.iter().map(|v| v.paths.len() as u64).sum();
    if total == 0 {
        return Ok(None);
    }
    info!(
        image = %image.path.display(),
        volumes = volumes.len(),
        paths = total,
        "NTFS name removal: deleting the excluded entries inside the image with ntfs-3g"
    );
    let mut pass = |scratch: SharedScratch, tick: &mut dyn FnMut(u64, u64)| {
        let mut deleted = 0u64;
        let mut failed = 0u64;
        let mut done = 0u64;
        for v in volumes {
            let size = scratch.lock().unwrap().size();
            let len = match v.len {
                Some(len) => Some(len),
                None => ntfs_volume_len(&scratch, v.offset, size),
            };
            let Some(len) = len.filter(|&l| l > 0 && v.offset + l <= size) else {
                warn!(
                    offset = v.offset,
                    "NTFS name removal: no NTFS volume at this offset"
                );
                failed += v.paths.len() as u64;
                done += v.paths.len() as u64;
                continue;
            };
            let before = scratch.lock().unwrap().touched_count();
            let window = ScratchWindow::new(scratch.clone(), v.offset, len);
            let mut on_device = |device: &str| {
                delete_on_volume(device, mount_base, v.paths, &mut |n| tick(done + n, total))
            };
            match exposer.expose(window, &mut on_device) {
                Ok((d, f)) => {
                    deleted += d;
                    failed += f;
                }
                // Not mounted and nothing written: this volume's names stay,
                // the others' still go.
                Err(e) if scratch.lock().unwrap().touched_count() == before => {
                    warn!(offset = v.offset, error = %e,
                          "NTFS name removal: the volume was not changed — its names stay");
                    failed += v.paths.len() as u64;
                }
                Err(e) => return Err(e),
            }
            done += v.paths.len() as u64;
            tick(done, total);
        }
        Ok((deleted, failed))
    };
    let folded = change_through_scratch(image, grid, checksums, tick, &mut pass)?;
    Ok(folded.map(|f| f.entries_failed))
}

/// The length of the NTFS volume at `offset`, from its boot sector: the
/// sectors it declares plus the one behind them that holds the backup boot
/// sector. `None` when no NTFS boot sector is there.
fn ntfs_volume_len(scratch: &SharedScratch, offset: u64, size: u64) -> Option<u64> {
    if offset + 512 > size {
        return None;
    }
    let mut boot = [0u8; 512];
    scratch.lock().unwrap().read_at(offset, &mut boot).ok()?;
    if &boot[3..11] != b"NTFS    " || boot[510..512] != [0x55, 0xAA] {
        return None;
    }
    let sector = u16::from_le_bytes([boot[0x0B], boot[0x0C]]) as u64;
    let sectors = u64::from_le_bytes(boot[0x28..0x30].try_into().unwrap());
    if !(256..=4096).contains(&sector) || !sector.is_power_of_two() || sectors == 0 {
        return None;
    }
    let len = sectors.checked_add(1)?.checked_mul(sector)?;
    Some(len.min(size - offset))
}

/// The ntfs-3g binary, wherever the distribution put it.
fn ntfs3g() -> Result<PathBuf> {
    [
        "/usr/bin/ntfs-3g",
        "/bin/ntfs-3g",
        "/usr/sbin/ntfs-3g",
        "/sbin/ntfs-3g",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|p| p.exists())
    .ok_or_else(|| dev("ntfs-3g is not installed"))
}

/// Mount the NTFS volume on `device` read-write with ntfs-3g, delete `paths`
/// through the mount, unmount, wait for ntfs-3g to have written everything
/// and exited, and read the volume back with `ntfsfix -n`. Returns (deleted,
/// failed) entry counts; `tick(n)` after the n-th path.
///
/// An error means the volume may be in any state; a caller must not keep
/// what it wrote.
pub(crate) fn delete_on_volume(
    device: &str,
    mount_base: &Path,
    paths: &[ExcludedPath],
    tick: &mut dyn FnMut(u64),
) -> Result<(u64, u64)> {
    let binary = ntfs3g()?;
    sweep_stale_mounts(mount_base);
    let dir = private_mount_dir(mount_base)?;
    let mounted = Ntfs3gMount::mount(&binary, device, &dir);
    let mut mounted = match mounted {
        Ok(m) => m,
        Err(e) => {
            let _ = std::fs::remove_dir(&dir);
            return Err(e);
        }
    };
    let t = Instant::now();
    let (deleted, failed) = delete_paths(&dir, paths, tick);
    let unmounted = mounted.unmount();
    let _ = std::fs::remove_dir(&dir);
    unmounted?;
    info!(
        device,
        deleted,
        failed,
        ms = t.elapsed().as_millis(),
        "NTFS name removal: entries deleted on the volume"
    );
    check_volume(device)?;
    Ok((deleted, failed))
}

/// `<base>/<pid>-<n>`, 0700, new: the pid lets a later run tell a mount a
/// killed service left behind from one a running pass uses.
fn private_mount_dir(base: &Path) -> Result<PathBuf> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(base)?;
    for n in 0..1000u32 {
        let dir = base.join(format!("{}-{n}", std::process::id()));
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(dev(format!(
        "no free mount directory under {}",
        base.display()
    )))
}

/// Mounts a killed service left behind: unmount (lazily — their ntfs-3g died
/// with the service, or will) and remove. Directories of a live process are
/// another pass's and stay.
fn sweep_stale_mounts(base: &Path) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(pid) = name.split('-').next().and_then(|p| p.parse::<u32>().ok()) else {
            continue;
        };
        if Path::new(&format!("/proc/{pid}")).exists() {
            continue;
        }
        let path = entry.path();
        if is_mount_point(&path) {
            let _ = umount(&path, libc::MNT_DETACH);
        }
        match std::fs::remove_dir(&path) {
            Ok(()) => info!(dir = %path.display(), "NTFS name removal: a stale mount was cleared"),
            Err(e) => debug!(dir = %path.display(), error = %e, "stale mount directory stays"),
        }
    }
}

fn is_mount_point(dir: &Path) -> bool {
    let (Ok(here), Some(Ok(up))) = (
        std::fs::symlink_metadata(dir),
        dir.parent().map(std::fs::symlink_metadata),
    ) else {
        return false;
    };
    here.dev() != up.dev()
}

fn umount(dir: &Path, flags: libc::c_int) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    // SAFETY: `c` is a NUL-terminated path that outlives the call.
    if unsafe { libc::umount2(c.as_ptr(), flags) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// An ntfs-3g mount whose process runs in the foreground (`no_detach`): its
/// exit, after the unmount, is the point where it has written everything back.
pub(crate) struct Ntfs3gMount {
    child: Option<Child>,
    dir: PathBuf,
    stderr: Option<std::thread::JoinHandle<String>>,
}

impl Ntfs3gMount {
    /// Mount `device` read-write at `dir`, which must exist and be empty.
    pub(crate) fn mount(binary: &Path, device: &str, dir: &Path) -> Result<Self> {
        // No hiberfile removal, no forcing: a volume Windows left hibernated
        // or unclean is not ours to repair, and ntfs-3g refuses it.
        let mut child = Command::new(binary)
            .arg("-o")
            .arg("no_detach,noatime,nosuid,nodev,noexec")
            .arg(device)
            .arg(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| dev(format!("cannot run {}: {e}", binary.display())))?;
        let stderr = child.stderr.take().map(|mut s| {
            std::thread::spawn(move || {
                let mut text = String::new();
                let _ = s.read_to_string(&mut text);
                text
            })
        });
        let mut mount = Self {
            child: Some(child),
            dir: dir.to_path_buf(),
            stderr,
        };
        let deadline = Instant::now() + MOUNT_TIMEOUT;
        loop {
            if is_mount_point(dir) {
                return Ok(mount);
            }
            let exited = mount.child.as_mut().unwrap().try_wait()?;
            if let Some(status) = exited {
                mount.child = None;
                return Err(dev(format!(
                    "ntfs-3g did not mount {device} ({status}): {}",
                    mount.stderr_text()
                )));
            }
            if Instant::now() > deadline {
                mount.kill();
                return Err(dev(format!("ntfs-3g did not mount {device} in time")));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn stderr_text(&mut self) -> String {
        self.stderr
            .take()
            .and_then(|h| h.join().ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    }

    fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if is_mount_point(&self.dir) {
            let _ = umount(&self.dir, libc::MNT_DETACH);
        }
    }

    /// Unmount and wait for ntfs-3g to exit cleanly. Anything else — the
    /// mount busy, the process killed, a non-zero exit — is an error: what it
    /// wrote may be incomplete.
    pub(crate) fn unmount(&mut self) -> Result<()> {
        let mut last = None;
        for _ in 0..50 {
            match umount(&self.dir, 0) {
                Ok(()) => {
                    last = None;
                    break;
                }
                Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {
                    last = Some(e);
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => {
                    last = Some(e);
                    break;
                }
            }
        }
        if let Some(e) = last {
            self.kill();
            return Err(dev(format!("cannot unmount {}: {e}", self.dir.display())));
        }
        let mut child = self.child.take().expect("mounted");
        let deadline = Instant::now() + EXIT_TIMEOUT;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(dev("ntfs-3g did not finish after the unmount"));
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let text = self.stderr_text();
        if !status.success() {
            return Err(dev(format!("ntfs-3g ended with {status}: {text}")));
        }
        debug!(dir = %self.dir.display(), output = %text, "ntfs-3g finished");
        Ok(())
    }
}

impl Drop for Ntfs3gMount {
    fn drop(&mut self) {
        self.kill();
    }
}

/// `ntfsfix -n`: ntfs-3g's own reading of the volume after the deletions. A
/// complaint voids the pass. Without ntfsfix (another package on some
/// systems) the check is skipped and the log says so.
fn check_volume(device: &str) -> Result<()> {
    let out = match Command::new("ntfsfix").arg("-n").arg(device).output() {
        Ok(out) => out,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            warn!(
                device,
                "NTFS name removal: ntfsfix is not installed — no read-back check"
            );
            return Ok(());
        }
        Err(e) => return Err(dev(format!("cannot run ntfsfix: {e}"))),
    };
    if !out.status.success() {
        return Err(dev(format!(
            "ntfsfix -n found the volume inconsistent after the deletions: {}{}",
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

/// The recorded path under the mount, or why it cannot be one: the scan's
/// `\`-separated path, each component a plain name. Nothing may lead out of
/// the mount.
fn under(root: &Path, rel: &str) -> std::result::Result<PathBuf, String> {
    let mut path = root.to_path_buf();
    let mut any = false;
    for part in rel.split('\\').filter(|p| !p.is_empty()) {
        if part == "." || part == ".." || part.contains('/') || part.contains('\0') {
            return Err(format!("unusable path component {part:?}"));
        }
        path.push(part);
        any = true;
    }
    if !any {
        return Err("empty path".into());
    }
    Ok(path)
}

/// Every directory between the mount and `path` is a real directory, not a
/// link: a junction shows as a symlink through ntfs-3g, and its target may be
/// an absolute path that resolves on THIS machine.
fn ancestors_are_directories(root: &Path, path: &Path) -> bool {
    let mut at = path.parent();
    while let Some(dir) = at {
        if dir == root {
            return true;
        }
        match std::fs::symlink_metadata(dir) {
            Ok(m) if m.file_type().is_dir() => {}
            _ => return false,
        }
        at = dir.parent();
    }
    false
}

/// Delete each recorded path under the mount at `root`. Returns (deleted,
/// failed) entries, everything inside a deleted directory counted.
pub(crate) fn delete_paths(
    root: &Path,
    paths: &[ExcludedPath],
    tick: &mut dyn FnMut(u64),
) -> (u64, u64) {
    let mut deleted = 0u64;
    let mut failed = 0u64;
    for (i, p) in paths.iter().enumerate() {
        match under(root, &p.rel) {
            Ok(path) if ancestors_are_directories(root, &path) => {
                if crate::exclusions::recreated_by_windows(&p.rel) {
                    // As on Windows: the folder stays, its contents go.
                    delete_children(&path, &mut deleted, &mut failed);
                } else {
                    delete_tree(&path, p.is_reparse, &mut deleted, &mut failed);
                }
            }
            Ok(_) => {
                warn!(path = %p.rel, "NTFS name removal: a parent is not a directory here — the entry stays");
                failed += 1;
            }
            Err(why) => {
                warn!(path = %p.rel, why, "NTFS name removal: the entry stays");
                failed += 1;
            }
        }
        tick(i as u64 + 1);
    }
    (deleted, failed)
}

fn delete_children(dir: &Path, deleted: &mut u64, failed: &mut u64) {
    match std::fs::read_dir(dir) {
        Ok(entries) => {
            for entry in entries {
                match entry {
                    Ok(e) => delete_tree(&e.path(), false, deleted, failed),
                    Err(e) => {
                        warn!(dir = %dir.display(), error = %e, "NTFS name removal: cannot list");
                        *failed += 1;
                    }
                }
            }
        }
        Err(e) => {
            warn!(dir = %dir.display(), error = %e, "NTFS name removal: cannot list");
            *failed += 1;
        }
    }
}

/// Delete `path` and, if it is a real directory, everything in it first. A
/// link — a symlink or junction as ntfs-3g shows it, or an entry the scan saw
/// as a reparse point — is removed itself, never what it leads to.
fn delete_tree(path: &Path, is_reparse: bool, deleted: &mut u64, failed: &mut u64) {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "NTFS name removal: cannot delete — the entry stays");
            *failed += 1;
            return;
        }
    };
    let result = if meta.file_type().is_dir() {
        if !is_reparse {
            delete_children(path, deleted, failed);
        }
        std::fs::remove_dir(path)
    } else {
        std::fs::remove_file(path)
    };
    match result {
        Ok(()) => *deleted += 1,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "NTFS name removal: cannot delete — the entry stays");
            *failed += 1;
        }
    }
}

/// What stands in for the NBD device where the kernel has no NBD module (the
/// cloud, a container): the window is copied into a file, the file is handed
/// to ntfs-3g (which takes a plain file as readily as a device), and what
/// changed in it is written back into the window — the writes the kernel
/// would have passed on, at 4 KiB granularity.
#[cfg(test)]
pub(crate) struct FileExposer {
    pub(crate) dir: PathBuf,
}

#[cfg(test)]
impl VolumeExposer for FileExposer {
    fn expose(
        &self,
        mut window: ScratchWindow,
        f: &mut dyn FnMut(&str) -> Result<(u64, u64)>,
    ) -> Result<(u64, u64)> {
        use crate::block_source::{BlockSource, WritableBlockSource};
        let len = window.size();
        let mut volume = vec![0u8; len as usize];
        window.read_at(0, &mut volume)?;
        let file = self.dir.join("exposed-volume.img");
        std::fs::write(&file, &volume)?;
        let result = f(file.to_str().unwrap());
        let after = std::fs::read(&file)?;
        let _ = std::fs::remove_file(&file);
        let counts = result?;
        for (i, (new, old)) in after.chunks(4096).zip(volume.chunks(4096)).enumerate() {
            if new != old {
                window.write_at(i as u64 * 4096, new)?;
            }
        }
        Ok(counts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(rel: &str, is_dir: bool) -> ExcludedPath {
        ExcludedPath {
            rel: rel.into(),
            is_dir,
            is_reparse: false,
        }
    }

    #[test]
    fn a_recorded_path_never_leads_out_of_the_mount() {
        let root = Path::new("/run/m");
        assert_eq!(
            under(root, "a\\b c\\d.txt").unwrap(),
            root.join("a/b c/d.txt")
        );
        assert_eq!(
            under(root, "\\pagefile.sys").unwrap(),
            root.join("pagefile.sys")
        );
        for bad in [
            "..\\etc\\passwd",
            "a\\..\\..\\x",
            "a\\.\\b",
            "a/b",
            "",
            "\\\\",
            "a\\b\0",
        ] {
            assert!(under(root, bad).is_err(), "{bad:?}");
        }
    }

    /// Plain directories stand in for the mount: what is deleted, what is
    /// counted, and that a link is removed as itself — through a link nothing
    /// is reached, and what a link points at outside stays.
    #[test]
    fn deletion_counts_entries_and_never_follows_a_link() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("mnt");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(root.join("cache/deep/er")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(outside.join("precious"), b"stay").unwrap();
        std::fs::write(root.join("cache/a.tmp"), b"x").unwrap();
        std::fs::write(root.join("cache/deep/er/b.tmp"), b"x").unwrap();
        std::fs::write(root.join("pagefile.sys"), b"x").unwrap();
        // A junction as ntfs-3g shows one: a symlink with an absolute target.
        symlink(&outside, root.join("junction")).unwrap();
        symlink(&outside, root.join("cache/link_in_tree")).unwrap();
        std::fs::create_dir_all(root.join("System Volume Information")).unwrap();
        std::fs::write(root.join("System Volume Information/tracking.log"), b"x").unwrap();

        let paths = [
            path("cache", true),
            path("pagefile.sys", false),
            ExcludedPath {
                rel: "junction".into(),
                is_dir: true,
                is_reparse: true,
            },
            // Through the junction: must not reach `outside/precious`.
            path("junction\\precious", false),
            path("System Volume Information", true),
            path("missing.tmp", false),
            path("..\\outside\\precious", false),
        ];
        let mut ticks = Vec::new();
        let (deleted, failed) = delete_paths(&root, &paths, &mut |n| ticks.push(n));
        // cache, deep, er, a.tmp, b.tmp, link_in_tree + pagefile + junction
        // + tracking.log
        assert_eq!(deleted, 9);
        // the path through the junction, the missing one, the escaping one
        assert_eq!(failed, 3);
        assert_eq!(ticks, (1..=paths.len() as u64).collect::<Vec<_>>());
        assert!(!root.join("cache").exists() && !root.join("pagefile.sys").exists());
        assert!(std::fs::symlink_metadata(root.join("junction")).is_err());
        assert_eq!(std::fs::read(outside.join("precious")).unwrap(), b"stay");
        // The folder Windows recreates stays, empty.
        assert_eq!(
            std::fs::read_dir(root.join("System Volume Information"))
                .unwrap()
                .count(),
            0
        );
    }

    /// An NTFS volume of `size` bytes in a file, or `None` without mkntfs,
    /// ntfs-3g or root (FUSE).
    pub(crate) fn ntfs_volume(file: &Path, size: u64) -> Option<()> {
        if !crate::exclusion_corpus::is_root()
            || ntfs3g().is_err()
            || !Path::new("/dev/fuse").exists()
        {
            eprintln!("skipped: needs root, /dev/fuse and ntfs-3g");
            return None;
        }
        std::fs::File::create(file).unwrap().set_len(size).unwrap();
        let out = match Command::new("mkntfs")
            .args(["-F", "-Q", "-q"])
            .arg(file)
            .output()
        {
            Ok(out) => out,
            Err(_) => {
                eprintln!("skipped: mkntfs is not installed");
                return None;
            }
        };
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        Some(())
    }

    /// Mount, delete, unmount, check — on a file, as ntfs-3g allows; then the
    /// volume mounted afresh shows exactly what should be left.
    #[test]
    fn ntfs3g_deletes_through_its_mount_and_the_volume_stays_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let vol = tmp.path().join("v.ntfs");
        if ntfs_volume(&vol, 64 << 20).is_none() {
            return;
        }
        let mnt = tmp.path().join("m");
        std::fs::create_dir(&mnt).unwrap();
        let binary = ntfs3g().unwrap();
        {
            let mut m = Ntfs3gMount::mount(&binary, vol.to_str().unwrap(), &mnt).unwrap();
            std::fs::create_dir_all(mnt.join("Users/u/AppData/Local/Temp/sub")).unwrap();
            std::fs::write(
                mnt.join("Users/u/AppData/Local/Temp/sub/x.tmp"),
                [7u8; 70000],
            )
            .unwrap();
            std::fs::write(mnt.join("Users/u/keep.txt"), b"keep").unwrap();
            std::fs::write(mnt.join("hiberfil.sys"), [1u8; 5000]).unwrap();
            m.unmount().unwrap();
        }
        let base = tmp.path().join("base");
        let paths = [
            path("Users\\u\\AppData\\Local\\Temp", true),
            path("hiberfil.sys", false),
        ];
        let (deleted, failed) =
            delete_on_volume(vol.to_str().unwrap(), &base, &paths, &mut |_| {}).unwrap();
        assert_eq!((deleted, failed), (4, 0));
        assert_eq!(
            std::fs::read_dir(&base).unwrap().count(),
            0,
            "mount dir removed"
        );

        let mut m = Ntfs3gMount::mount(&binary, vol.to_str().unwrap(), &mnt).unwrap();
        assert!(!mnt.join("Users/u/AppData/Local/Temp").exists());
        assert!(mnt.join("Users/u/AppData/Local").is_dir());
        assert!(!mnt.join("hiberfil.sys").exists());
        assert_eq!(
            std::fs::read(mnt.join("Users/u/keep.txt")).unwrap(),
            b"keep"
        );
        m.unmount().unwrap();
        check_volume(vol.to_str().unwrap()).unwrap();
    }

    /// Not NTFS: ntfs-3g refuses, nothing is written, and the error says so.
    #[test]
    fn a_volume_ntfs3g_refuses_is_an_error_and_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let vol = tmp.path().join("v.ntfs");
        if ntfs_volume(&vol, 16 << 20).is_none() {
            return;
        }
        std::fs::write(&vol, vec![0u8; 16 << 20]).unwrap();
        let base = tmp.path().join("base");
        let err = delete_on_volume(
            vol.to_str().unwrap(),
            &base,
            &[path("a", false)],
            &mut |_| {},
        )
        .unwrap_err();
        assert!(err.to_string().contains("did not mount"), "{err}");
        assert!(std::fs::read(&vol).unwrap().iter().all(|&b| b == 0));
        assert_eq!(std::fs::read_dir(&base).unwrap().count(), 0);
    }

    /// A mount directory of a process that is gone is cleared; one of a live
    /// process is not.
    #[test]
    fn stale_mount_directories_of_dead_processes_are_swept() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        // pid_max is far below this.
        std::fs::create_dir(base.join("4294967-0")).unwrap();
        let mine = base.join(format!("{}-7", std::process::id()));
        std::fs::create_dir(&mine).unwrap();
        std::fs::create_dir(base.join("unrelated")).unwrap();
        sweep_stale_mounts(base);
        assert!(!base.join("4294967-0").exists());
        assert!(mine.exists() && base.join("unrelated").exists());
    }
}
