//! File-by-file restore (R4b, D1).
//!
//! The only way into a partition *smaller* than the source volume: the sector
//! path moves the filesystem verbatim and cannot shrink it, so the data has to
//! be copied as files onto a freshly made filesystem.
//!
//! ## Why this is not a file copy
//!
//! Copying the bytes is the easy half and the useless half. A Windows volume
//! restored without its metadata mounts, shows every file, and does not work:
//!
//! | What | Why losing it breaks things |
//! |---|---|
//! | ACLs, owner, audit entries | services refuse to start, profiles are inaccessible, `Program Files` becomes writable by anyone |
//! | Junctions and symlinks | `C:\Users\All Users`, `Documents and Settings` and friends are reparse points; following them instead of recreating them copies the same tree repeatedly and can recurse forever |
//! | Hard links | the WinSxS store is built from them — expanding each link into its own copy turns a few GB into tens |
//! | Sparseness | the page file and hibernation file expand to full size |
//! | Alternate data streams | zone identifiers, and anything an application kept there |
//! | Timestamps | incremental backups and build systems get confused |
//!
//! `BackupRead`/`BackupWrite` carry all of that in one stream — security
//! descriptor, extended attributes, reparse data, sparse ranges and every named
//! stream — which is exactly why they exist. Timestamps and attributes are not
//! in the stream and are applied separately.
//!
//! ## What cannot be restored
//!
//! The volume's **serial number**. It is assigned at format time and there is
//! no API to set it. Some software licences are keyed to it, and the BCD refers
//! to volumes by it, so a file-restored system volume may need its boot entries
//! rebuilt even though every file is in place. The caller is told, because the
//! alternative is a user discovering it much later.

#![cfg(target_os = "windows")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use tracing::{debug, info, warn};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    BackupRead, BackupWrite, CreateDirectoryW, CreateFileW, CreateHardLinkW,
    GetFileInformationByHandle, SetFileAttributesW, SetFileTime, SetVolumeLabelW,
    BY_HANDLE_FILE_INFORMATION, CREATE_ALWAYS, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_SPARSE_FILE, FILE_FLAGS_AND_ATTRIBUTES,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Ioctl::FSCTL_SET_SPARSE;
use windows::Win32::System::IO::DeviceIoControl;

use own_backup_common::cancel::CancelToken;
use own_backup_common::{tr, OwnBackupError, Result};
use sha2::{Digest, Sha256};

use crate::exclusions_win::list_dir_backup;
use crate::volume_win::FileHandle;
use crate::win_device::to_wide;

/// Standard right needed to read/write a SACL. Not in the `FILE_*` constants.
const ACCESS_SYSTEM_SECURITY: u32 = 0x0100_0000;
/// Standard rights for taking ownership and setting the DACL on restore.
const WRITE_DAC: u32 = 0x0004_0000;
const WRITE_OWNER: u32 = 0x0008_0000;

/// Copy buffer. Large enough that `BackupRead` is not the bottleneck, small
/// enough that a few of them in flight do not matter.
const STREAM_BUF: usize = 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub struct FileRestoreSummary {
    pub files_copied: u64,
    pub dirs_created: u64,
    pub hard_links: u64,
    pub reparse_points: u64,
    pub bytes_copied: u64,
    /// Entries that could not be copied, with the reason. Not fatal: one
    /// stubborn file must not abandon the rest of the restore, but the user has
    /// to be told which ones are missing.
    pub failures: Vec<(String, String)>,
    pub cancelled: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct FileRestoreProgress {
    pub files_done: u64,
    pub files_total: u64,
    pub bytes_done: u64,
    pub bytes_total: u64,
}

/// Result of the counting pass, so progress is a real fraction rather than a
/// spinner. Walking the tree twice costs one metadata pass; guessing costs the
/// user's trust in the progress bar.
#[derive(Debug, Clone, Copy, Default)]
pub struct TreeSize {
    pub files: u64,
    pub bytes: u64,
}

/// Count the files and bytes under `root`.
pub fn measure_tree(root: &Path, cancel: &CancelToken) -> Result<TreeSize> {
    let mut out = TreeSize::default();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if cancel.is_cancelled() {
            return Err(OwnBackupError::Cancelled);
        }
        let entries = list_dir_backup(&dir)?;
        for e in entries {
            if e.name == "." || e.name == ".." {
                continue;
            }
            let path = dir.join(&e.name);
            let is_dir = e.attributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0;
            let is_reparse = e.attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0;
            // A reparse point is one entry, never a subtree: descending into a
            // junction copies its target's contents into the wrong place, and
            // a junction pointing at its own ancestor never terminates.
            if is_dir && !is_reparse {
                stack.push(path);
            } else {
                out.files += 1;
                out.bytes += e.size;
            }
        }
    }
    Ok(out)
}

/// Verify the complete directory structure and every BackupRead stream. This
/// includes alternate streams, reparse data and security descriptors, not just
/// the ordinary file size. A failed directory walk is an error, never an empty
/// tree that might compare equal to another failed walk.
pub fn verify_tree(src_root: &Path, dst_root: &Path, cancel: &CancelToken) -> Result<TreeSize> {
    crate::privileges_win::enable_backup_privileges();
    let mut out = TreeSize::default();
    let mut stack = vec![(src_root.to_path_buf(), dst_root.to_path_buf())];
    let mut buf = vec![0u8; STREAM_BUF];
    while let Some((src_dir, dst_dir)) = stack.pop() {
        if cancel.is_cancelled() {
            return Err(OwnBackupError::Cancelled);
        }
        let mut source = list_dir_backup(&src_dir)?;
        let mut target = list_dir_backup(&dst_dir)?;
        source.retain(|e| e.name != "." && e.name != "..");
        target.retain(|e| e.name != "." && e.name != "..");
        source.sort_by(|a, b| a.name.cmp(&b.name));
        target.sort_by(|a, b| a.name.cmp(&b.name));
        if source.len() != target.len() {
            return Err(OwnBackupError::RestoreIncompatible(
                tr!(
                    "file_restore_win.verify_listing_diff",
                    path = dst_dir.display()
                )
                .into_owned(),
            ));
        }
        for (from, to) in source.into_iter().zip(target) {
            let src = src_dir.join(&from.name);
            let dst = dst_dir.join(&to.name);
            let from_kind =
                from.attributes & (FILE_ATTRIBUTE_DIRECTORY.0 | FILE_ATTRIBUTE_REPARSE_POINT.0);
            let to_kind =
                to.attributes & (FILE_ATTRIBUTE_DIRECTORY.0 | FILE_ATTRIBUTE_REPARSE_POINT.0);
            if from.name != to.name || from_kind != to_kind || from.size != to.size {
                return Err(OwnBackupError::RestoreIncompatible(
                    tr!("file_restore_win.verify_listing_diff", path = dst.display()).into_owned(),
                ));
            }
            if from_kind & FILE_ATTRIBUTE_DIRECTORY.0 != 0
                && from_kind & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0
            {
                stack.push((src, dst));
            } else {
                if backup_stream_digest(&src, cancel, &mut buf)?
                    != backup_stream_digest(&dst, cancel, &mut buf)?
                {
                    return Err(OwnBackupError::RestoreIncompatible(
                        tr!("file_restore_win.verify_content_diff", path = dst.display())
                            .into_owned(),
                    ));
                }
                out.files += 1;
                out.bytes += from.size;
            }
        }
    }
    Ok(out)
}

fn backup_stream_digest(path: &Path, cancel: &CancelToken, buf: &mut [u8]) -> Result<[u8; 32]> {
    let handle = open_source(path)?;
    let mut read_ctx: *mut core::ffi::c_void = std::ptr::null_mut();
    let result = (|| -> Result<[u8; 32]> {
        let mut digest = Sha256::new();
        loop {
            if cancel.is_cancelled() {
                return Err(OwnBackupError::Cancelled);
            }
            let mut read = 0u32;
            // SAFETY: the buffer is writable; BackupRead owns read_ctx until
            // abort_context releases it below, including on an error.
            unsafe { BackupRead(handle.0, buf, &mut read, false, true, &mut read_ctx) }.map_err(
                |e| OwnBackupError::Device(format!("BackupRead {}: {e}", path.display())),
            )?;
            if read == 0 {
                break;
            }
            digest.update(&buf[..read as usize]);
        }
        Ok(digest.finalize().into())
    })();
    abort_context(handle.0, &mut read_ctx, true);
    result
}

/// Copy everything under `src_root` to `dst_root`, preserving what makes a
/// Windows volume work.
///
/// Both roots are volume roots (`X:\` or `\\?\Volume{…}\`). The source is
/// expected to be a mounted backup image, read-only.
pub fn copy_tree(
    src_root: &Path,
    dst_root: &Path,
    total: TreeSize,
    cancel: &CancelToken,
    mut progress: impl FnMut(&FileRestoreProgress) -> Result<()>,
) -> Result<FileRestoreSummary> {
    crate::privileges_win::enable_backup_privileges();
    let started = Instant::now();
    let mut out = FileRestoreSummary::default();
    // (volume serial, file index) → the path we already created for it, so a
    // second link to the same inode becomes a hard link rather than a copy.
    let mut inodes: HashMap<(u32, u64), PathBuf> = HashMap::new();

    let mut stack = vec![(src_root.to_path_buf(), dst_root.to_path_buf())];
    while let Some((src_dir, dst_dir)) = stack.pop() {
        if cancel.is_cancelled() {
            out.cancelled = true;
            break;
        }
        let entries = match list_dir_backup(&src_dir) {
            Ok(e) => e,
            Err(e) => {
                out.failures
                    .push((src_dir.display().to_string(), e.to_string()));
                continue;
            }
        };
        for e in entries {
            if e.name == "." || e.name == ".." {
                continue;
            }
            if cancel.is_cancelled() {
                out.cancelled = true;
                break;
            }
            let src = src_dir.join(&e.name);
            let dst = dst_dir.join(&e.name);
            let is_dir = e.attributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0;
            let is_reparse = e.attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0;

            let result = copy_entry(&src, &dst, e.attributes, &mut inodes, &mut out);
            match result {
                Ok(()) => {
                    if is_dir && !is_reparse {
                        out.dirs_created += 1;
                        stack.push((src, dst));
                    } else {
                        out.files_done_tick(e.size);
                        progress(&FileRestoreProgress {
                            files_done: out.files_copied + out.hard_links + out.reparse_points,
                            files_total: total.files,
                            bytes_done: out.bytes_copied,
                            bytes_total: total.bytes,
                        })?;
                    }
                }
                Err(err) => {
                    // One unreadable file must not abandon the restore; the
                    // user gets the list at the end.
                    warn!(path = %src.display(), error = %err, "file restore: entry failed");
                    out.failures
                        .push((src.display().to_string(), err.to_string()));
                }
            }
        }
    }

    info!(
        files = out.files_copied,
        dirs = out.dirs_created,
        links = out.hard_links,
        reparse = out.reparse_points,
        bytes = out.bytes_copied,
        failures = out.failures.len(),
        cancelled = out.cancelled,
        secs = started.elapsed().as_secs(),
        "file restore complete"
    );
    Ok(out)
}

impl FileRestoreSummary {
    fn files_done_tick(&mut self, size: u64) {
        self.files_copied += 1;
        self.bytes_copied += size;
    }
}

/// Copy one entry: a directory, a reparse point, a hard link to something
/// already copied, or a file.
fn copy_entry(
    src: &Path,
    dst: &Path,
    attributes: u32,
    inodes: &mut HashMap<(u32, u64), PathBuf>,
    out: &mut FileRestoreSummary,
) -> Result<()> {
    let is_dir = attributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0;
    let is_reparse = attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0;

    // Opened without following reparse points: a junction must be recreated as
    // a junction, not resolved and copied.
    let source = open_source(src)?;
    let info = file_information(&source)?;

    // A file with more than one name is one file. WinSxS is built almost
    // entirely out of such links; copying each name separately turns a few
    // gigabytes into tens of them.
    if !is_dir && info.nNumberOfLinks > 1 {
        let key = (
            info.dwVolumeSerialNumber,
            ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
        );
        if let Some(first) = inodes.get(&key) {
            let existing = to_wide(&first.to_string_lossy());
            let new_name = to_wide(&dst.to_string_lossy());
            // SAFETY: both buffers are NUL-terminated and outlive the call.
            unsafe { CreateHardLinkW(PCWSTR(new_name.as_ptr()), PCWSTR(existing.as_ptr()), None) }
                .map_err(|e| {
                    OwnBackupError::Device(format!("CreateHardLink {}: {e}", dst.display()))
                })?;
            out.hard_links += 1;
            return Ok(());
        }
        inodes.insert(key, dst.to_path_buf());
    }

    if is_dir {
        create_dir(dst)?;
    }
    let target = open_target(dst, is_dir)?;

    // Sparseness has to be set on the target *before* the stream is written,
    // or BackupWrite materialises every hole. The page file and hibernation
    // file are the obvious victims — many gigabytes of zeroes.
    if attributes & FILE_ATTRIBUTE_SPARSE_FILE.0 != 0 && !is_dir {
        set_sparse(&target)?;
    }

    stream_copy(&source, &target)?;
    apply_times(&target, &info)?;
    drop(target);
    // Attributes go on last: setting READONLY earlier would block the write.
    set_attributes(dst, attributes)?;

    if is_reparse {
        out.reparse_points += 1;
    }
    Ok(())
}

fn open_source(path: &Path) -> Result<FileHandle> {
    let wide = to_wide(&path.to_string_lossy());
    // ACCESS_SYSTEM_SECURITY asks for the SACL, which BackupRead includes with
    // bProcessSecurity — without it the audit entries are silently dropped.
    // SAFETY: `wide` outlives the call.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            GENERIC_READ.0 | ACCESS_SYSTEM_SECURITY,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .map_err(|e| OwnBackupError::Device(format!("open source {}: {e}", path.display())))?;
    Ok(FileHandle(handle))
}

fn open_target(path: &Path, is_dir: bool) -> Result<FileHandle> {
    let wide = to_wide(&path.to_string_lossy());
    let disposition = if is_dir { OPEN_EXISTING } else { CREATE_ALWAYS };
    // WRITE_OWNER and WRITE_DAC are what let BackupWrite restore the owner and
    // the DACL; ACCESS_SYSTEM_SECURITY covers the SACL.
    // SAFETY: `wide` outlives the call.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            GENERIC_WRITE.0 | WRITE_OWNER | WRITE_DAC | ACCESS_SYSTEM_SECURITY,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            disposition,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .map_err(|e| OwnBackupError::Device(format!("create target {}: {e}", path.display())))?;
    Ok(FileHandle(handle))
}

fn create_dir(path: &Path) -> Result<()> {
    let wide = to_wide(&path.to_string_lossy());
    // SAFETY: `wide` outlives the call.
    match unsafe { CreateDirectoryW(PCWSTR(wide.as_ptr()), None) } {
        Ok(()) => Ok(()),
        // Already there is fine — the tree may be restored over an existing one.
        Err(e) if e.code() == windows::Win32::Foundation::ERROR_ALREADY_EXISTS.to_hresult() => {
            Ok(())
        }
        Err(e) => Err(OwnBackupError::Device(format!(
            "mkdir {}: {e}",
            path.display()
        ))),
    }
}

fn file_information(h: &FileHandle) -> Result<BY_HANDLE_FILE_INFORMATION> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: out pointer is a live BY_HANDLE_FILE_INFORMATION.
    unsafe { GetFileInformationByHandle(h.0, &mut info) }
        .map_err(|e| OwnBackupError::Device(format!("GetFileInformationByHandle: {e}")))?;
    Ok(info)
}

fn set_sparse(h: &FileHandle) -> Result<()> {
    let mut returned = 0u32;
    // SAFETY: no in/out buffers.
    unsafe {
        DeviceIoControl(
            h.0,
            FSCTL_SET_SPARSE,
            None,
            0,
            None,
            0,
            Some(&mut returned),
            None,
        )
    }
    .map_err(|e| OwnBackupError::Device(format!("FSCTL_SET_SPARSE: {e}")))
}

/// Pump one file through `BackupRead` into `BackupWrite`.
///
/// `bProcessSecurity = TRUE` on both sides is what carries the security
/// descriptor; without it the copy lands owned by whoever ran the restore, with
/// inherited permissions, which is how a restored `Windows` directory ends up
/// writable by standard users.
fn stream_copy(src: &FileHandle, dst: &FileHandle) -> Result<()> {
    let mut buf = vec![0u8; STREAM_BUF];
    let mut read_ctx: *mut core::ffi::c_void = std::ptr::null_mut();
    let mut write_ctx: *mut core::ffi::c_void = std::ptr::null_mut();
    let mut result = Ok(());

    loop {
        let mut read = 0u32;
        // SAFETY: buffer valid for its length; the context pointer is owned by
        // the API between calls and released by the abort calls below.
        let ok = unsafe { BackupRead(src.0, &mut buf, &mut read, false, true, &mut read_ctx) };
        if let Err(e) = ok {
            result = Err(OwnBackupError::Device(format!("BackupRead: {e}")));
            break;
        }
        if read == 0 {
            break; // end of the backup stream
        }
        let mut written = 0u32;
        // SAFETY: as above; `read` bytes are initialised.
        let ok = unsafe {
            BackupWrite(
                dst.0,
                &buf[..read as usize],
                &mut written,
                false,
                true,
                &mut write_ctx,
            )
        };
        if let Err(e) = ok {
            result = Err(OwnBackupError::Device(format!("BackupWrite: {e}")));
            break;
        }
        if written != read {
            result = Err(OwnBackupError::Device(format!(
                "BackupWrite wrote {written} of {read} bytes"
            )));
            break;
        }
    }

    // The contexts hold kernel state and must be released even on the error
    // path — a leaked context is a handle the process keeps until it exits.
    abort_context(src.0, &mut read_ctx, true);
    abort_context(dst.0, &mut write_ctx, false);
    result
}

/// Release a `BackupRead`/`BackupWrite` context (the `bAbort = TRUE` call).
fn abort_context(handle: HANDLE, ctx: &mut *mut core::ffi::c_void, reading: bool) {
    if ctx.is_null() {
        return;
    }
    let mut n = 0u32;
    // SAFETY: aborting takes no buffer; the context came from this API.
    let r = unsafe {
        if reading {
            BackupRead(handle, &mut [], &mut n, true, false, ctx)
        } else {
            BackupWrite(handle, &[], &mut n, true, false, ctx)
        }
    };
    if let Err(e) = r {
        debug!(error = %e, reading, "releasing the backup context failed");
    }
}

fn apply_times(h: &FileHandle, info: &BY_HANDLE_FILE_INFORMATION) -> Result<()> {
    // SAFETY: all three pointers are live FILETIMEs.
    unsafe {
        SetFileTime(
            h.0,
            Some(&info.ftCreationTime),
            Some(&info.ftLastAccessTime),
            Some(&info.ftLastWriteTime),
        )
    }
    .map_err(|e| OwnBackupError::Device(format!("SetFileTime: {e}")))
}

fn set_attributes(path: &Path, attributes: u32) -> Result<()> {
    let wide = to_wide(&path.to_string_lossy());
    // SAFETY: `wide` outlives the call.
    unsafe { SetFileAttributesW(PCWSTR(wide.as_ptr()), FILE_FLAGS_AND_ATTRIBUTES(attributes)) }
        .map_err(|e| OwnBackupError::Device(format!("SetFileAttributes {}: {e}", path.display())))
}

/// Copy the volume label. The serial number cannot follow it — there is no API
/// to set one, so anything keyed to it (some licences, BCD volume references)
/// will not recognise the restored volume.
pub fn set_volume_label(volume_root: &str, label: &str) -> Result<()> {
    let root = to_wide(volume_root);
    let value = to_wide(label);
    // SAFETY: both buffers are NUL-terminated and outlive the call.
    unsafe { SetVolumeLabelW(PCWSTR(root.as_ptr()), PCWSTR(value.as_ptr())) }
        .map_err(|e| OwnBackupError::Device(format!("SetVolumeLabel {volume_root}: {e}")))
}

/// Prefix a path so it is exempt from the 260-character limit.
///
/// A restored tree routinely exceeds it — deep `node_modules`, WinSxS, nested
/// user data — and without the prefix those files simply fail to open, which
/// looks like a permissions problem and is not.
pub fn long_path(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if s.starts_with(r"\\?\") {
        return path.to_path_buf();
    }
    if let Some(rest) = s.strip_prefix(r"\\") {
        // UNC paths take a different prefix: \\server\share → \\?\UNC\server\share
        return PathBuf::from(format!(r"\\?\UNC\{rest}"));
    }
    PathBuf::from(format!(r"\\?\{s}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_path_prefixes_a_drive_path() {
        assert_eq!(
            long_path(Path::new(r"C:\Windows")),
            PathBuf::from(r"\\?\C:\Windows")
        );
    }

    #[test]
    fn long_path_leaves_an_already_prefixed_path_alone() {
        let p = Path::new(r"\\?\C:\Windows");
        assert_eq!(long_path(p), p.to_path_buf());
    }

    #[test]
    fn long_path_uses_the_unc_form_for_network_paths() {
        assert_eq!(
            long_path(Path::new(r"\\server\share\dir")),
            PathBuf::from(r"\\?\UNC\server\share\dir")
        );
    }
}
