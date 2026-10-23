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
//!    a file the scan saw go under every name it has (its 8.3 short name is
//!    no name of its own) has its resident content — the data of a small
//!    file and its short named streams, kept inside its MFT record —
//!    overwritten with zeros through the mount first: deleting a file frees
//!    its record but does not clear it, here or on Windows;
//! 4. every cluster the deletions freed is erased — `$Bitmap` before and
//!    after, as the Windows in-image pass does it. The scan subtracted the
//!    files' data before the copy — every `$DATA` stream, named ones
//!    (`Zone.Identifier`, anything an application hides in an ADS)
//!    included — but not everything a deleted file held: what the scan had
//!    to keep (a torn run list, an unreadable record) and the file's own
//!    metadata clusters (a non-resident attribute list, an index) were
//!    copied, and would linger in the image's free space;
//! 5. only when every volume went through cleanly is the change folded into
//!    the image and the touched blocks re-checksummed.
//!
//! Anything that goes wrong after a volume was mounted leaves the image as it
//! was written (zero-filled entries, reported as kept). A volume that could
//! not be mounted at all — hibernated Windows, no ntfs-3g — wrote nothing; its
//! paths are counted as not deleted and the other volumes go on.
//!
//! No NTFS structure is written by this code; ntfs-3g does the deleting. Ours
//! only reads: the boot sector to size a volume of a disk, and `$Bitmap`
//! through the engine's own reader (`ntfs_map`) to know what was freed.

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
    pub(crate) len: VolumeLen,
    pub(crate) paths: &'a [ExcludedPath],
}

/// How long a [`NamedVolume`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VolumeLen {
    /// Exactly this long: the image is the volume.
    Exact(u64),
    /// As long as its boot sector says, inside a partition this long: a
    /// volume that claims more than its partition is not one to write to.
    WithinPartition(u64),
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
                VolumeLen::Exact(len) => Some(len),
                VolumeLen::WithinPartition(part) => {
                    ntfs_volume_len(&scratch, v.offset, size.min(v.offset.saturating_add(part)))
                }
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
            let free_before = match crate::ntfs_map::ntfs_free_map(&mut ScratchWindow::new(
                scratch.clone(),
                v.offset,
                len,
            )) {
                Ok(map) => map,
                Err(e) => {
                    warn!(offset = v.offset, error = %e,
                          "NTFS name removal: the volume's bitmap cannot be read — its names stay");
                    failed += v.paths.len() as u64;
                    done += v.paths.len() as u64;
                    continue;
                }
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
                    let mut window = ScratchWindow::new(scratch.clone(), v.offset, len);
                    let erased = erase_freed(&mut window, &free_before)?;
                    info!(
                        offset = v.offset,
                        bytes = erased,
                        "NTFS name removal: what the deleted entries freed is erased"
                    );
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

/// Zero every cluster that is free now and was not before the deletions,
/// where it still holds anything. Clusters the filesystem took back for itself
/// meanwhile read as allocated and are not touched. Returns the bytes zeroed.
fn erase_freed(window: &mut ScratchWindow, before: &crate::ntfs_map::NtfsFreeMap) -> Result<u64> {
    use crate::block_source::{BlockSource, WritableBlockSource};
    let after = crate::ntfs_map::ntfs_free_map(window)?;
    let cs = after.cluster_size;
    if cs != before.cluster_size {
        return Err(dev("the cluster size changed under the deletions"));
    }
    // Runs of newly freed clusters, read a few MiB at a time: most of them
    // held excluded data the copy already left out, and read as zeros.
    const RUN: u64 = 1024;
    let mut zeroed = 0u64;
    let mut buf = vec![0u8; (RUN * cs) as usize];
    let zeros = vec![0u8; cs as usize];
    let mut cluster = 0u64;
    while cluster < after.total_clusters {
        if !(after.is_free(cluster) && !before.is_free(cluster)) {
            cluster += 1;
            continue;
        }
        let mut end = cluster + 1;
        while end < after.total_clusters
            && end - cluster < RUN
            && after.is_free(end)
            && !before.is_free(end)
        {
            end += 1;
        }
        let bytes = ((end - cluster) * cs) as usize;
        window.read_at(cluster * cs, &mut buf[..bytes])?;
        for (i, c) in buf[..bytes].chunks(cs as usize).enumerate() {
            if c.iter().any(|&b| b != 0) {
                window.write_at((cluster + i as u64) * cs, &zeros)?;
                zeroed += cs;
            }
        }
        cluster = end;
    }
    Ok(zeroed)
}

/// The length of the NTFS volume at `offset`, from its boot sector: the
/// sectors it declares plus the one behind them that holds the backup boot
/// sector, within `end` (the end of its partition). `None` when no NTFS boot
/// sector is there, or it declares more sectors than fit before `end`.
fn ntfs_volume_len(scratch: &SharedScratch, offset: u64, end: u64) -> Option<u64> {
    let mut boot = [0u8; 512];
    if offset.checked_add(512)? > end {
        return None;
    }
    scratch.lock().unwrap().read_at(offset, &mut boot).ok()?;
    boot_sector_len(&boot, end - offset)
}

/// [`ntfs_volume_len`] of the boot sector `boot` of a volume with `room`
/// bytes to its partition's end.
fn boot_sector_len(boot: &[u8; 512], room: u64) -> Option<u64> {
    if &boot[3..11] != b"NTFS    " || boot[510..512] != [0x55, 0xAA] {
        return None;
    }
    let sector = u16::from_le_bytes([boot[0x0B], boot[0x0C]]) as u64;
    let sectors = u64::from_le_bytes(boot[0x28..0x30].try_into().unwrap());
    if !(256..=4096).contains(&sector) || !sector.is_power_of_two() || sectors == 0 {
        return None;
    }
    let declared = sectors.checked_mul(sector)?;
    if declared > room {
        warn!(
            declared,
            room, "NTFS name removal: the boot sector claims more than its partition holds"
        );
        return None;
    }
    // The backup boot sector behind the declared ones, where the partition
    // has room for it.
    Some(declared.checked_add(sector)?.min(room))
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
        // Not asked first: a mount whose ntfs-3g is gone answers ENOTCONN to
        // a stat, and a directory that is no mount point just refuses this.
        let _ = umount(&path, libc::MNT_DETACH);
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
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                // Not left running, nor unreaped: it may still be writing.
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(dev(format!("cannot wait for ntfs-3g: {e}")));
                }
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
                let whole = &p.whole_records;
                if crate::exclusions::recreated_by_windows(&p.rel) {
                    // As on Windows: the folder stays, its contents go.
                    delete_children(&path, whole, &mut deleted, &mut failed);
                } else {
                    delete_tree(&path, p.is_reparse, whole, &mut deleted, &mut failed);
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

fn delete_children(dir: &Path, whole: &[u64], deleted: &mut u64, failed: &mut u64) {
    match std::fs::read_dir(dir) {
        Ok(entries) => {
            for entry in entries {
                match entry {
                    Ok(e) => delete_tree(&e.path(), false, whole, deleted, failed),
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

/// Whether the content of the file `meta` describes is cleared before its
/// name goes: only when the scan saw every name of its record excluded. Not
/// by its link count: ntfs-3g counts a separate 8.3 short name as a link, so
/// a long-named file Windows made would never be cleared; and a file with a
/// name outside the exclusion lives on under it — only the name goes, the
/// content is not ours to clear.
fn clears_content(meta: &std::fs::Metadata, whole: &[u64]) -> bool {
    meta.file_type().is_file() && whole.binary_search(&meta.ino()).is_ok()
}

/// What can live inside an MFT record (1 KiB, or 4 KiB on a volume of
/// 4K-sector disks, minus its headers): a value this short may be resident.
/// Anything longer is in clusters, which the scan left out of the copy —
/// unnamed data and named streams alike — and the deletion frees and
/// [`erase_freed`] clears all the same.
const RESIDENT_MAX: u64 = crate::exclusions::NTFS_RESIDENT_MAX;

/// Overwrite, in place and through the mount, the content of the file at
/// `path` and of its named streams (ntfs-3g's `user.*` attributes) that may
/// be resident in its MFT record. The record is freed by the deletion, never
/// cleared, and the copy took it whole. Best effort: what cannot be
/// overwritten is said in the log, and the deletion goes on.
fn wipe_resident(path: &Path, len: u64) {
    use std::io::Write;
    if len > 0 && len <= RESIDENT_MAX {
        let zeros = vec![0u8; len as usize];
        let written = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .and_then(|mut f| f.write_all(&zeros).and_then(|()| f.sync_data()));
        if let Err(e) = written {
            warn!(path = %path.display(), error = %e,
                  "NTFS name removal: a small file's content could not be cleared before its deletion");
        }
    }
    for name in user_xattrs(path) {
        let Some(len) = xattr_len(path, &name) else {
            continue;
        };
        if len == 0 || len as u64 > RESIDENT_MAX {
            continue;
        }
        if let Err(e) = set_xattr(path, &name, &vec![0u8; len]) {
            warn!(path = %path.display(), stream = %name.to_string_lossy(), error = %e,
                  "NTFS name removal: a named stream could not be cleared before its deletion");
        }
    }
}

fn cpath(path: &Path) -> Option<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes()).ok()
}

/// The `user.*` attribute names of `path` itself (never a link's target).
fn user_xattrs(path: &Path) -> Vec<std::ffi::CString> {
    let Some(p) = cpath(path) else {
        return Vec::new();
    };
    // SAFETY: a NUL-terminated path; a null buffer asks for the size only.
    let size = unsafe { libc::llistxattr(p.as_ptr(), std::ptr::null_mut(), 0) };
    if size <= 0 {
        return Vec::new();
    }
    let mut buf = vec![0u8; size as usize];
    // SAFETY: the buffer is `buf.len()` bytes long and outlives the call.
    let got = unsafe { libc::llistxattr(p.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
    if got <= 0 {
        return Vec::new();
    }
    buf.truncate(got as usize);
    buf.split(|&b| b == 0)
        .filter(|n| n.starts_with(b"user."))
        .filter_map(|n| std::ffi::CString::new(n).ok())
        .collect()
}

fn xattr_len(path: &Path, name: &std::ffi::CStr) -> Option<usize> {
    let p = cpath(path)?;
    // SAFETY: NUL-terminated strings; a null buffer asks for the size only.
    let size = unsafe { libc::lgetxattr(p.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0) };
    (size >= 0).then_some(size as usize)
}

fn set_xattr(path: &Path, name: &std::ffi::CStr, value: &[u8]) -> std::io::Result<()> {
    let p = cpath(path)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in path"))?;
    // SAFETY: NUL-terminated strings; the value outlives the call.
    let rc = unsafe {
        libc::lsetxattr(
            p.as_ptr(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            libc::XATTR_REPLACE,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Delete `path` and, if it is a real directory, everything in it first. A
/// link — a symlink or junction as ntfs-3g shows it, or an entry the scan saw
/// as a reparse point — is removed itself, never what it leads to. `whole`:
/// the MFT records the scan saw go under every name (ntfs-3g gives the record
/// number as the inode number).
fn delete_tree(path: &Path, is_reparse: bool, whole: &[u64], deleted: &mut u64, failed: &mut u64) {
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
            delete_children(path, whole, deleted, failed);
        }
        std::fs::remove_dir(path)
    } else {
        if clears_content(&meta, whole) {
            wipe_resident(path, meta.len());
        }
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
            ..Default::default()
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
                ..Default::default()
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

    /// Whose content is cleared before the name goes is the scan's word,
    /// not the link count. Plain directories stand in for the mount, a hard
    /// link outside for what is left of a file after its names are gone: a
    /// record the scan saw go whole is cleared even with a second link (what
    /// ntfs-3g makes of an 8.3 short name), inside an excluded directory too;
    /// one it did not list keeps its content for the name that stays.
    #[test]
    fn content_is_cleared_by_the_scans_word_not_by_the_link_count() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("mnt");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(root.join("cache")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let ino = |p: &Path| std::fs::symlink_metadata(p).unwrap().ino();
        std::fs::write(root.join("Long name.tmp"), b"SHORT-NAMED-SECRET").unwrap();
        std::fs::hard_link(root.join("Long name.tmp"), outside.join("LONGNA~1.TMP")).unwrap();
        std::fs::write(root.join("cache/inner.tmp"), b"INNER-SECRET").unwrap();
        std::fs::hard_link(root.join("cache/inner.tmp"), outside.join("inner")).unwrap();
        std::fs::write(root.join("twin.tmp"), b"KEPT-TWIN").unwrap();
        std::fs::hard_link(root.join("twin.tmp"), outside.join("twin.txt")).unwrap();
        let paths = [
            ExcludedPath {
                rel: "Long name.tmp".into(),
                whole_records: vec![ino(&root.join("Long name.tmp"))],
                ..Default::default()
            },
            ExcludedPath {
                rel: "cache".into(),
                is_dir: true,
                whole_records: vec![ino(&root.join("cache/inner.tmp"))],
                ..Default::default()
            },
            path("twin.tmp", false),
        ];
        let (deleted, failed) = delete_paths(&root, &paths, &mut |_| {});
        assert_eq!((deleted, failed), (4, 0));
        assert_eq!(
            std::fs::read(outside.join("LONGNA~1.TMP")).unwrap(),
            [0u8; 18]
        );
        assert_eq!(std::fs::read(outside.join("inner")).unwrap(), [0u8; 12]);
        assert_eq!(
            std::fs::read(outside.join("twin.txt")).unwrap(),
            b"KEPT-TWIN"
        );
    }

    /// A volume of a disk is as long as its boot sector says, plus the backup
    /// boot sector where the partition has room for it — and one that claims
    /// more than its partition is no volume to write to.
    #[test]
    fn a_boot_sector_is_believed_only_within_its_partition() {
        let mut boot = [0u8; 512];
        boot[3..11].copy_from_slice(b"NTFS    ");
        boot[0x0B..0x0D].copy_from_slice(&512u16.to_le_bytes());
        boot[0x28..0x30].copy_from_slice(&2047u64.to_le_bytes());
        boot[510..512].copy_from_slice(&[0x55, 0xAA]);
        assert_eq!(boot_sector_len(&boot, 1 << 20), Some(1 << 20));
        assert_eq!(boot_sector_len(&boot, 4 << 20), Some(1 << 20));
        assert_eq!(boot_sector_len(&boot, 2047 * 512), Some(2047 * 512));
        assert_eq!(boot_sector_len(&boot, 2046 * 512), None, "claims more");
        boot[0x28..0x30].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(boot_sector_len(&boot, u64::MAX), None);
        boot[3] = b'X';
        assert_eq!(boot_sector_len(&boot, 4 << 20), None, "not NTFS");
    }

    /// An NTFS volume of `size` bytes in `file` and a mount of it that
    /// ntfs-3g lets this user make (its setuid build mounts a file the user
    /// owns); `None` — said why — where that is not possible.
    fn user_ntfs_volume(file: &Path, mnt: &Path, size: u64) -> Option<Ntfs3gMount> {
        let Ok(binary) = ntfs3g() else {
            eprintln!("skipped: ntfs-3g is not installed");
            return None;
        };
        std::fs::File::create(file).unwrap().set_len(size).unwrap();
        match Command::new("mkntfs")
            .args(["-F", "-Q", "-q"])
            .arg(file)
            .output()
        {
            Ok(out) if out.status.success() => {}
            _ => {
                eprintln!("skipped: mkntfs is missing or failed");
                return None;
            }
        }
        std::fs::create_dir_all(mnt).unwrap();
        match Ntfs3gMount::mount(&binary, file.to_str().unwrap(), mnt) {
            Ok(m) => Some(m),
            Err(e) => {
                eprintln!("skipped: ntfs-3g cannot mount here: {e}");
                None
            }
        }
    }

    /// Unmount a mount this user made, as FUSE lets a user (`umount2` is
    /// root's), and wait for its ntfs-3g to have written everything and exited.
    fn user_unmount(m: &mut Ntfs3gMount) {
        let out = Command::new("fusermount3")
            .arg("-u")
            .arg(&m.dir)
            .output()
            .or_else(|_| Command::new("fusermount").arg("-u").arg(&m.dir).output())
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let status = m.child.take().unwrap().wait().unwrap();
        assert!(status.success(), "ntfs-3g: {status}: {}", m.stderr_text());
    }

    /// The case ntfs-3g's link count got wrong, end to end: a long-named
    /// small file with a separate 8.3 short name — what Windows makes on a
    /// volume with short names — shows two links, yet the scan sees one name
    /// and its resident content is cleared before the record is freed. A
    /// small file with a hard link that stays keeps its content.
    #[test]
    fn a_long_named_file_with_a_short_name_loses_its_resident_content() {
        let tmp = tempfile::tempdir().unwrap();
        let vol = tmp.path().join("v.ntfs");
        let mnt = tmp.path().join("m");
        let Some(mut m) = user_ntfs_volume(&vol, &mnt, 16 << 20) else {
            return;
        };
        let long = mnt.join("Long secret name.tmp");
        std::fs::write(&long, b"SHORT-NAMED-SECRET-0123456789").unwrap();
        crate::exclusion_corpus::setxattr(&long, "system.ntfs_dos_name", b"LONGSE~1.TMP");
        std::fs::write(mnt.join("twin.tmp"), b"TWIN-KEPT-CONTENT").unwrap();
        // The kept name in a directory: the excluded one, in the root, is
        // the record's first name, the one the scan matches by.
        std::fs::create_dir(mnt.join("keep")).unwrap();
        std::fs::hard_link(mnt.join("twin.tmp"), mnt.join("keep/twin.bin")).unwrap();
        user_unmount(&mut m);

        let scan = crate::ntfs_scan::scan_mft(
            &mut std::fs::File::open(&vol).unwrap(),
            &["*.tmp".to_string()],
        )
        .unwrap();
        let paths: Vec<ExcludedPath> = scan
            .paths
            .iter()
            .map(|p| ExcludedPath {
                rel: p.rel.clone(),
                is_dir: p.is_dir,
                is_reparse: p.is_reparse,
                whole_records: p.whole_records.clone(),
            })
            .collect();
        assert_eq!(paths.len(), 2, "{paths:?}");
        // `delete_on_volume`, with the unmount a user may make.
        let mut m = Ntfs3gMount::mount(&ntfs3g().unwrap(), vol.to_str().unwrap(), &mnt).unwrap();
        let shown = std::fs::metadata(&long).unwrap();
        assert_eq!(shown.nlink(), 2, "ntfs-3g counts the short name as a link");
        assert!(scan.paths.iter().any(|p| p.whole_records == [shown.ino()]));
        assert_eq!(delete_paths(&mnt, &paths, &mut |_| {}), (2, 0));
        user_unmount(&mut m);
        check_volume(vol.to_str().unwrap()).unwrap();

        let data = std::fs::read(&vol).unwrap();
        let has = |needle: &[u8]| data.windows(needle.len()).any(|w| w == needle);
        assert!(
            !has(b"SHORT-NAMED-SECRET"),
            "the freed record kept the content"
        );
        assert!(has(b"TWIN-KEPT-CONTENT"));
        drop(data);
        let mut m = Ntfs3gMount::mount(&ntfs3g().unwrap(), vol.to_str().unwrap(), &mnt).unwrap();
        assert!(std::fs::symlink_metadata(&long).is_err());
        assert!(std::fs::symlink_metadata(mnt.join("twin.tmp")).is_err());
        assert_eq!(
            std::fs::read(mnt.join("keep/twin.bin")).unwrap(),
            b"TWIN-KEPT-CONTENT"
        );
        user_unmount(&mut m);
    }

    /// Named streams of excluded files, end to end as far as a user can go:
    /// the corpus' stream files written through ntfs-3g, the scan, what the
    /// copy does with its ranges (they read as zeros), the deletion through
    /// the mount. No excluded byte is left — a stream in clusters of its own,
    /// a short one in the record, thirty in extension records behind an
    /// attribute list — and the kept file keeps its streams.
    #[test]
    fn named_streams_of_excluded_files_leave_the_volume() {
        use std::os::unix::fs::FileExt;
        let tmp = tempfile::tempdir().unwrap();
        let vol = tmp.path().join("v.ntfs");
        let mnt = tmp.path().join("m");
        let Some(mut m) = user_ntfs_volume(&vol, &mnt, 32 << 20) else {
            return;
        };
        let set = crate::exclusion_corpus::setxattr;
        std::fs::create_dir(mnt.join("cache")).unwrap();
        std::fs::create_dir(mnt.join("keep")).unwrap();
        let big = mnt.join("cache/ads_big.bin");
        std::fs::write(&big, [0xAB; 8192]).unwrap();
        set(&big, "user.big", &[0xAB; 20_000]);
        set(&big, "user.Zone.Identifier", b"ZoneId=3 ADS-ZONE-SECRET");
        let many = mnt.join("cache/ads_many.bin");
        std::fs::write(&many, [0xAB; 4096]).unwrap();
        for i in 0..30 {
            let mut v = format!("ADS-LIST-SECRET-{i:02}-").into_bytes();
            v.resize(100, b'q');
            set(&many, &format!("user.s{i:02}"), &v);
        }
        set(&many, "user.bigx", &[0xAB; 40_000]);
        let small = mnt.join("small.tmp");
        std::fs::write(&small, b"RESIDENT-SECRET").unwrap();
        set(&small, "user.bulk", &[0xAB; 12_000]);
        let kept = mnt.join("keep/kept.bin");
        let kept_stream = crate::exclusion_corpus::pattern(904, 20_000);
        std::fs::write(&kept, crate::exclusion_corpus::pattern(903, 8192)).unwrap();
        set(&kept, "user.keepstream", &kept_stream);
        user_unmount(&mut m);

        let scan = crate::ntfs_scan::scan_mft(
            &mut std::fs::File::open(&vol).unwrap(),
            &["cache".to_string(), "*.tmp".to_string()],
        )
        .unwrap();
        // What the copy does: the subtracted ranges are not in the image.
        {
            let f = std::fs::OpenOptions::new().write(true).open(&vol).unwrap();
            for (s, e) in scan.ranges.iter() {
                f.write_all_at(&vec![0u8; (e - s) as usize], s).unwrap();
            }
        }
        let paths: Vec<ExcludedPath> = scan
            .paths
            .iter()
            .map(|p| ExcludedPath {
                rel: p.rel.clone(),
                is_dir: p.is_dir,
                is_reparse: p.is_reparse,
                whole_records: p.whole_records.clone(),
            })
            .collect();
        let cache = paths.iter().find(|p| p.rel == "cache").expect("cache");
        assert_eq!(cache.whole_records.len(), 2, "both stream files go whole");
        let mut m = Ntfs3gMount::mount(&ntfs3g().unwrap(), vol.to_str().unwrap(), &mnt).unwrap();
        assert_eq!(delete_paths(&mnt, &paths, &mut |_| {}), (4, 0));
        user_unmount(&mut m);
        check_volume(vol.to_str().unwrap()).unwrap();

        let data = std::fs::read(&vol).unwrap();
        let runs = data
            .chunks(4096)
            .filter(|c| c.iter().all(|&b| b == 0xAB))
            .count();
        assert_eq!(runs, 0, "an excluded stream's clusters are left");
        for secret in [
            &b"ADS-ZONE-SECRET"[..],
            b"ADS-LIST-SECRET",
            b"RESIDENT-SECRET",
        ] {
            assert!(
                !data.windows(secret.len()).any(|w| w == secret),
                "{} is left",
                String::from_utf8_lossy(secret)
            );
        }
        drop(data);
        let mut m = Ntfs3gMount::mount(&ntfs3g().unwrap(), vol.to_str().unwrap(), &mnt).unwrap();
        assert!(!mnt.join("cache").exists() && !small.exists());
        assert_eq!(
            std::fs::read(&kept).unwrap(),
            crate::exclusion_corpus::pattern(903, 8192)
        );
        assert_eq!(
            crate::exclusion_corpus::getxattr(&kept, "user.keepstream"),
            kept_stream
        );
        user_unmount(&mut m);
    }

    /// A hard link that stays, whose name lives in an extension record: a
    /// file with many names spills them out of its base record, and the kept
    /// one was the last. The scan reads every name through the attribute
    /// list, sees the kept one and leaves the data alone; the deletion takes
    /// the excluded names only. (A reader that stayed in the base record saw
    /// two excluded names, called the file whole and cut the kept file's data
    /// out of the copy.)
    #[test]
    fn a_kept_hard_link_in_an_extension_record_keeps_the_data() {
        use std::os::unix::fs::FileExt;
        let tmp = tempfile::tempdir().unwrap();
        let vol = tmp.path().join("v.ntfs");
        let mnt = tmp.path().join("m");
        let Some(mut m) = user_ntfs_volume(&vol, &mnt, 32 << 20) else {
            return;
        };
        std::fs::create_dir(mnt.join("cache")).unwrap();
        std::fs::create_dir(mnt.join("keep")).unwrap();
        let content = vec![0x3C; 20_000];
        std::fs::write(mnt.join("cache/h.bin"), &content).unwrap();
        for i in 0..12 {
            let long = format!("L{i:02}{}", "x".repeat(200));
            std::fs::hard_link(mnt.join("cache/h.bin"), mnt.join("cache").join(long)).unwrap();
        }
        std::fs::hard_link(mnt.join("cache/h.bin"), mnt.join("keep/kept.bin")).unwrap();
        user_unmount(&mut m);
        // The kept name really is outside the base record.
        if let Ok(out) = Command::new("ntfsinfo")
            .args(["-F", "/keep/kept.bin"])
            .arg(&vol)
            .output()
        {
            let dump = String::from_utf8_lossy(&out.stdout);
            let at = dump.find("'kept.bin'").expect("kept.bin in the dump");
            let header = dump[..at]
                .rfind("Dumping attribute $FILE_NAME")
                .map(|h| &dump[h..at])
                .unwrap();
            let base = dump
                .find("Dumping attribute $STANDARD_INFORMATION")
                .map(|h| &dump[h..])
                .and_then(|d| d.lines().next())
                .unwrap();
            let record = |line: &str| line.rsplit("from mft record ").next().map(str::to_owned);
            assert_ne!(
                record(header.lines().next().unwrap()),
                record(base),
                "the kept name is not in an extension record:\n{dump}"
            );
        }

        let scan = crate::ntfs_scan::scan_mft(
            &mut std::fs::File::open(&vol).unwrap(),
            &["cache".to_string()],
        )
        .unwrap();
        assert!(
            scan.kept
                .iter()
                .any(|(_, r)| *r == crate::ntfs_scan::KeptReason::OtherNameKept),
            "{:?}",
            scan.kept
        );
        // What the copy does with the ranges, then the deletion.
        {
            let f = std::fs::OpenOptions::new().write(true).open(&vol).unwrap();
            for (s, e) in scan.ranges.iter() {
                f.write_all_at(&vec![0u8; (e - s) as usize], s).unwrap();
            }
        }
        let paths: Vec<ExcludedPath> = scan
            .paths
            .iter()
            .map(|p| ExcludedPath {
                rel: p.rel.clone(),
                is_dir: p.is_dir,
                is_reparse: p.is_reparse,
                whole_records: p.whole_records.clone(),
            })
            .collect();
        let mut m = Ntfs3gMount::mount(&ntfs3g().unwrap(), vol.to_str().unwrap(), &mnt).unwrap();
        let (_, failed) = delete_paths(&mnt, &paths, &mut |_| {});
        assert_eq!(failed, 0);
        user_unmount(&mut m);
        check_volume(vol.to_str().unwrap()).unwrap();
        let mut m = Ntfs3gMount::mount(&ntfs3g().unwrap(), vol.to_str().unwrap(), &mnt).unwrap();
        assert!(!mnt.join("cache").exists());
        assert!(
            std::fs::read(mnt.join("keep/kept.bin")).unwrap() == content,
            "the kept name lost its data"
        );
        user_unmount(&mut m);
    }

    /// ntfs-3g empties `$LogFile` when it mounts a volume read-write, so the
    /// journal of an image the Linux pass went through holds none of what
    /// was there before — the old values of the records it cleared included.
    /// (Windows replays and keeps its journal; see `true_removal_win`.)
    #[test]
    fn ntfs3g_empties_the_journal_on_a_read_write_mount() {
        use std::os::unix::fs::FileExt;
        let tmp = tempfile::tempdir().unwrap();
        let vol = tmp.path().join("v.ntfs");
        let mnt = tmp.path().join("m");
        let Some(mut m) = user_ntfs_volume(&vol, &mnt, 16 << 20) else {
            return;
        };
        user_unmount(&mut m);
        // Where $LogFile (record 2) lives.
        let (at, len) = {
            let mut f = std::fs::File::open(&vol).unwrap();
            let ntfs = ntfs::Ntfs::new(&mut f).unwrap();
            let log = ntfs.file(&mut f, 2).unwrap();
            let mut found = None;
            for attr in log.attributes_raw() {
                let attr = attr.unwrap();
                if attr.ty().ok() == Some(ntfs::NtfsAttributeType::Data) {
                    if let Ok(ntfs::attribute_value::NtfsAttributeValue::NonResident(v)) =
                        attr.value(&mut f)
                    {
                        let run = v.data_runs().next().unwrap().unwrap();
                        found = Some((
                            run.data_position().value().unwrap().get(),
                            run.allocated_size(),
                        ));
                    }
                }
            }
            found.expect("$LogFile's data")
        };
        // Old journal content past the restart pages.
        let secret = b"JOURNAL-SECRET-0123456789".repeat(1000);
        assert!(len > 65_536 + secret.len() as u64);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&vol)
            .unwrap()
            .write_all_at(&secret, at + 65_536)
            .unwrap();
        let mut m = Ntfs3gMount::mount(&ntfs3g().unwrap(), vol.to_str().unwrap(), &mnt).unwrap();
        std::fs::write(mnt.join("x.txt"), b"x").unwrap();
        user_unmount(&mut m);
        let mut log = vec![0u8; len as usize];
        std::fs::File::open(&vol)
            .unwrap()
            .read_exact_at(&mut log, at)
            .unwrap();
        assert!(
            !log.windows(14).any(|w| w == b"JOURNAL-SECRET"),
            "the journal kept its old content"
        );
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

        // A real one: its ntfs-3g killed, the mount answering ENOTCONN.
        let vol = tmp.path().join("v.ntfs");
        if ntfs_volume(&vol, 16 << 20).is_none() {
            return;
        }
        let dead = base.join("4294968-0");
        std::fs::create_dir(&dead).unwrap();
        let mut m = Ntfs3gMount::mount(&ntfs3g().unwrap(), vol.to_str().unwrap(), &dead).unwrap();
        let mut child = m.child.take().unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        std::mem::forget(m);
        assert!(std::fs::symlink_metadata(&dead).is_err(), "ENOTCONN");
        sweep_stale_mounts(base);
        assert!(!dead.exists() && std::fs::symlink_metadata(&dead).is_err());
    }
}

#[cfg(test)]
#[path = "ntfs_names_acceptance.rs"]
mod acceptance;
