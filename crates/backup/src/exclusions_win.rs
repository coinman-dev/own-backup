//! Windows half of phase-3 file exclusions.
//!
//! Walks the volume tree (usually the VSS snapshot, so the file set is
//! frozen), matches every entry against the job's exclusion patterns and
//! resolves matched files to their cluster extents via
//! `FSCTL_GET_RETRIEVAL_POINTERS`. The resulting [`ByteRanges`] are
//! subtracted from the used-sector map and zeroed inside blocks that are
//! still copied — see [`crate::exclusions`] for why both steps are needed.
//!
//! Failure policy: a file that cannot be opened or queried is *kept* in the
//! backup (warn + skip). Excluding less than asked is safe; silently breaking
//! the image would not be.

#![cfg(target_os = "windows")]

use std::path::{Path, PathBuf};

use tracing::{info, trace, warn};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_HANDLE_EOF, ERROR_MORE_DATA, ERROR_NO_MORE_FILES, HANDLE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FileFullDirectoryInfo, FileStreamInfo, GetFileInformationByHandleEx,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Ioctl::{FSCTL_GET_RETRIEVAL_POINTERS, STARTING_VCN_INPUT_BUFFER};
use windows::Win32::System::IO::DeviceIoControl;

use own_backup_common::Result;

use crate::exclusions::{parse_patterns, ByteRanges, CategorySizes};
pub use crate::exclusions::{ExcludedPath, ExclusionScan};
use crate::win_device::to_wide;

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
/// Right needed to enumerate a directory's contents.
const FILE_LIST_DIRECTORY: u32 = 0x0001;

/// One directory entry from [`list_dir_backup`].
pub struct DirEntryInfo {
    pub name: String,
    pub size: u64,
    pub attributes: u32,
}

/// List a directory using `FILE_FLAG_BACKUP_SEMANTICS` + a directory handle, so
/// ACL-restricted folders (System Volume Information, protected app data) can be
/// enumerated once SeBackupPrivilege is enabled — unlike `std::fs::read_dir`,
/// which opens without backup semantics and is denied. Sizes/attributes come
/// straight from the enumeration, so no per-file open is needed here.
pub fn list_dir_backup(dir: &std::path::Path) -> Result<Vec<DirEntryInfo>> {
    let wide = to_wide(&dir.to_string_lossy());
    // SAFETY: `wide` outlives the call.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_LIST_DIRECTORY,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
    }
    .map_err(|e| own_backup_common::OwnBackupError::Device(format!("open dir: {e}")))?;
    let handle = FileHandle(handle);

    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        // SAFETY: buffer valid for the size passed.
        let call = unsafe {
            GetFileInformationByHandleEx(
                handle.0,
                FileFullDirectoryInfo,
                buf.as_mut_ptr() as *mut _,
                buf.len() as u32,
            )
        };
        match call {
            Ok(()) => {}
            Err(e) if e.code() == ERROR_NO_MORE_FILES.to_hresult() => break,
            Err(e) => {
                return Err(own_backup_common::OwnBackupError::Device(format!(
                    "enumerate dir: {e}"
                )))
            }
        }
        // Parse the FILE_FULL_DIR_INFO chain in `buf`. Field offsets (x64):
        //   NextEntryOffset@0, EndOfFile@40, FileAttributes@56,
        //   FileNameLength(bytes)@60, FileName@68.
        let mut off = 0usize;
        loop {
            let rec = &buf[off..];
            if rec.len() < 68 {
                break;
            }
            let next = u32::from_le_bytes(rec[0..4].try_into().unwrap()) as usize;
            let size = i64::from_le_bytes(rec[40..48].try_into().unwrap()).max(0) as u64;
            let attrs = u32::from_le_bytes(rec[56..60].try_into().unwrap());
            let name_len = u32::from_le_bytes(rec[60..64].try_into().unwrap()) as usize;
            if 68 + name_len <= rec.len() {
                let name: Vec<u16> = rec[68..68 + name_len]
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                let name = String::from_utf16_lossy(&name);
                if name != "." && name != ".." {
                    out.push(DirEntryInfo {
                        name,
                        size,
                        attributes: attrs,
                    });
                }
            }
            if next == 0 {
                break;
            }
            off += next;
            if off >= buf.len() {
                break;
            }
        }
    }
    Ok(out)
}

/// Walk `volume_read_path` (`\\.\E:` or a VSS snapshot device path) and
/// collect the cluster extents of every file matching `patterns`.
///
/// `tick(entries_scanned)` is invoked periodically so the caller can report
/// progress and cancel (return `Err`).
pub fn collect_excluded_ranges(
    volume_read_path: &str,
    cluster_size: u32,
    patterns: &[String],
    tick: impl FnMut(u64) -> Result<()>,
) -> Result<ExclusionScan> {
    collect_excluded_ranges_inner(volume_read_path, cluster_size, patterns, tick, false)
}

/// Same walk, but recording per-file byte totals (see `ExclusionScan::file_bytes`).
pub fn collect_excluded_ranges_verbose(
    volume_read_path: &str,
    cluster_size: u32,
    patterns: &[String],
    tick: impl FnMut(u64) -> Result<()>,
) -> Result<ExclusionScan> {
    collect_excluded_ranges_inner(volume_read_path, cluster_size, patterns, tick, true)
}

fn collect_excluded_ranges_inner(
    volume_read_path: &str,
    cluster_size: u32,
    patterns: &[String],
    mut tick: impl FnMut(u64) -> Result<()>,
    collect_file_bytes: bool,
) -> Result<ExclusionScan> {
    let parsed = parse_patterns(patterns);
    let mut scan = ExclusionScan {
        ranges: ByteRanges::new(),
        files_excluded: 0,
        entries_scanned: 0,
        bytes_excluded: 0,
        categories: CategorySizes::default(),
        paths: Vec::new(),
        file_bytes: Vec::new(),
    };
    if parsed.is_empty() {
        scan.ranges.finalize();
        return Ok(scan);
    }

    let root = format!("{}\\", volume_read_path.trim_end_matches('\\'));
    // (directory, lowercased path components relative to root, subtree excluded)
    let mut stack: Vec<(PathBuf, Vec<String>, bool)> =
        vec![(PathBuf::from(&root), Vec::new(), false)];

    while let Some((dir, rel, subtree_excluded)) = stack.pop() {
        let entries = match list_dir_backup(&dir) {
            Ok(e) => e,
            Err(e) => {
                warn!(dir = %dir.display(), error = %e, "Exclusion scan: cannot list directory");
                continue;
            }
        };
        for info in entries {
            scan.entries_scanned += 1;
            if scan.entries_scanned % 512 == 0 {
                tick(scan.entries_scanned)?;
            }

            let name_lower = info.name.to_lowercase();
            let mut rel_child = rel.clone();
            rel_child.push(name_lower.clone());

            let is_dir = info.attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
            let is_reparse = info.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0;

            let matched =
                subtree_excluded || parsed.iter().any(|p| p.matches(&name_lower, &rel_child));

            let child = dir.join(&info.name);
            // Record every entry at its HIGHEST match point (a matched dir is
            // one record; entries below it ride along) for the true-removal
            // pass. Reparse points and unresolved/resident files are recorded
            // too: zeroing cannot touch them, deletion can.
            if matched && !subtree_excluded {
                match child.to_string_lossy().strip_prefix(&root) {
                    Some(rel_path) if !rel_path.is_empty() => scan.paths.push(ExcludedPath {
                        rel: rel_path.to_string(),
                        is_dir,
                        is_reparse,
                        whole_records: Vec::new(),
                    }),
                    _ => {
                        warn!(path = %child.display(), "matched path outside the scan root — not recorded for removal")
                    }
                }
            }
            if is_dir {
                // Never follow junctions/symlinks: their targets may live on
                // another volume, and loops are possible.
                if !is_reparse {
                    stack.push((child, rel_child, matched));
                }
            } else if matched && !is_reparse {
                match file_extent_ranges(&child, cluster_size, &mut scan.ranges) {
                    Ok(bytes) => {
                        // Its named streams (Zone.Identifier, any ADS) are its
                        // content too, in clusters of their own.
                        let bytes =
                            bytes + stream_extent_ranges(&child, cluster_size, &mut scan.ranges);
                        scan.files_excluded += 1;
                        scan.bytes_excluded += bytes;
                        if collect_file_bytes {
                            if let Some(rel) = child
                                .to_string_lossy()
                                .strip_prefix(&root)
                                .map(str::to_string)
                            {
                                scan.file_bytes.push((rel, bytes));
                            }
                        }
                    }
                    Err(e) => {
                        warn!(
                            file = %child.display(),
                            error = %e,
                            "Exclusion scan: cannot resolve extents — file stays in the backup"
                        );
                        // Could not exclude it → it stays in the archive.
                        scan.categories.add(&rel_child, info.size);
                    }
                }
            } else if !is_reparse {
                scan.categories.add(&rel_child, info.size);
            }
        }
    }

    scan.ranges.finalize();
    info!(
        volume = volume_read_path,
        files = scan.files_excluded,
        entries = scan.entries_scanned,
        excluded_mb = scan.bytes_excluded / (1024 * 1024),
        "Exclusion scan complete"
    );
    Ok(scan)
}

/// The named data streams of the open file `handle`: their path suffixes
/// (`:Zone.Identifier:$DATA`) and sizes. Needs no access right beyond the
/// open itself.
pub(crate) fn named_streams(handle: HANDLE) -> Result<Vec<(String, u64)>> {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        // SAFETY: the buffer is valid for the size passed.
        let call = unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileStreamInfo,
                buf.as_mut_ptr() as *mut _,
                buf.len() as u32,
            )
        };
        match call {
            Ok(()) => return Ok(crate::exclusions::named_streams_from_info(&buf)),
            // A file with no data stream at all (a directory without one).
            Err(e) if e.code() == ERROR_HANDLE_EOF.to_hresult() => return Ok(Vec::new()),
            Err(e) if e.code() == ERROR_MORE_DATA.to_hresult() && buf.len() < 4 << 20 => {
                let grown = buf.len() * 4;
                buf = vec![0u8; grown];
            }
            Err(e) => {
                return Err(own_backup_common::OwnBackupError::Device(format!(
                    "FileStreamInfo: {e}"
                )))
            }
        }
    }
}

/// Append the cluster extents of every named stream of `path` to `ranges`
/// and return their bytes. A stream that cannot be resolved stays in the
/// backup, said in the log — as a file that cannot be resolved does.
fn stream_extent_ranges(path: &Path, cluster_size: u32, ranges: &mut ByteRanges) -> u64 {
    let wide = to_wide(&path.to_string_lossy());
    // SAFETY: `wide` outlives the call.
    let opened = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    };
    let streams = match opened {
        Ok(h) => {
            let handle = FileHandle(h);
            named_streams(handle.0)
        }
        Err(e) => Err(own_backup_common::OwnBackupError::Device(format!(
            "CreateFileW failed: {e}"
        ))),
    };
    let streams = match streams {
        Ok(s) => s,
        Err(e) => {
            warn!(file = %path.display(), error = %e,
                  "Exclusion scan: cannot list the named streams — they stay in the backup");
            return 0;
        }
    };
    let mut bytes = 0u64;
    for (name, _) in streams {
        let stream = PathBuf::from(format!("{}{name}", path.to_string_lossy()));
        // Collected apart and merged only when complete: half a stream's
        // extents would cut it, not exclude it.
        let mut own = ByteRanges::new();
        match file_extent_ranges(&stream, cluster_size, &mut own) {
            Ok(n) => {
                for (s, e) in own.iter() {
                    ranges.add(s, e);
                }
                bytes += n;
            }
            Err(e) => warn!(stream = %stream.display(), error = %e,
                            "Exclusion scan: cannot resolve a named stream — it stays in the backup"),
        }
    }
    bytes
}

/// RAII handle for the queried file.
struct FileHandle(HANDLE);
impl Drop for FileHandle {
    fn drop(&mut self) {
        // SAFETY: handle owned by us, closed exactly once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Append the cluster extents of `path` (as byte ranges) to `ranges`.
/// Returns the number of excluded bytes. Resident files (data inside the MFT
/// record) have no extents and contribute nothing.
fn file_extent_ranges(path: &Path, cluster_size: u32, ranges: &mut ByteRanges) -> Result<u64> {
    let wide = to_wide(&path.to_string_lossy());
    // FILE_READ_ATTRIBUTES opens even exclusively-locked files (pagefile.sys);
    // BACKUP_SEMANTICS is required for directories and system files.
    // SAFETY: `wide` outlives the call.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .map_err(|e| own_backup_common::OwnBackupError::Device(format!("CreateFileW failed: {e}")))?;
    let handle = FileHandle(handle);

    let cs = cluster_size as u64;
    let mut bytes_excluded = 0u64;
    let mut starting_vcn: i64 = 0;

    loop {
        let input = STARTING_VCN_INPUT_BUFFER {
            StartingVcn: starting_vcn,
        };
        // RETRIEVAL_POINTERS_BUFFER layout (x64):
        //   u32 ExtentCount, u32 pad, i64 StartingVcn,
        //   then ExtentCount × { i64 NextVcn, i64 Lcn }.
        const HEADER: usize = 16;
        const EXTENT: usize = 16;
        let mut out = vec![0u8; HEADER + 512 * EXTENT];
        let mut returned = 0u32;

        // SAFETY: in/out pointers valid for the sizes passed.
        let call = unsafe {
            DeviceIoControl(
                handle.0,
                FSCTL_GET_RETRIEVAL_POINTERS,
                Some(&input as *const _ as *const _),
                std::mem::size_of::<STARTING_VCN_INPUT_BUFFER>() as u32,
                Some(out.as_mut_ptr() as *mut _),
                out.len() as u32,
                Some(&mut returned),
                None,
            )
        };
        let more_data = match call {
            Ok(()) => false,
            Err(e) if e.code() == ERROR_MORE_DATA.to_hresult() => true,
            // Resident or zero-length file: nothing allocated on disk.
            Err(e) if e.code() == ERROR_HANDLE_EOF.to_hresult() => break,
            Err(e) => {
                return Err(own_backup_common::OwnBackupError::Device(format!(
                    "FSCTL_GET_RETRIEVAL_POINTERS failed: {e}"
                )));
            }
        };

        if (returned as usize) < HEADER + EXTENT {
            break;
        }
        let extent_count = u32::from_le_bytes(out[0..4].try_into().unwrap()) as usize;
        let buf_extents = (returned as usize - HEADER) / EXTENT;
        let count = extent_count.min(buf_extents);
        let mut prev_vcn = i64::from_le_bytes(out[8..16].try_into().unwrap());

        for i in 0..count {
            let base = HEADER + i * EXTENT;
            let next_vcn = i64::from_le_bytes(out[base..base + 8].try_into().unwrap());
            let lcn = i64::from_le_bytes(out[base + 8..base + 16].try_into().unwrap());
            let clusters = next_vcn.saturating_sub(prev_vcn);
            // LCN -1 marks a hole in sparse/compressed files — nothing on disk.
            if lcn >= 0 && clusters > 0 {
                let start = lcn as u64 * cs;
                let end = start + clusters as u64 * cs;
                ranges.add(start, end);
                bytes_excluded += clusters as u64 * cs;
            }
            prev_vcn = next_vcn;
        }

        if !more_data || count == 0 {
            break;
        }
        starting_vcn = prev_vcn;
    }

    // trace, not debug: with `**\Temp\**`-style defaults tens of thousands of
    // files match, and one line each balloons the log file.
    trace!(file = %path.display(), bytes = bytes_excluded, "Excluded file extents collected");
    Ok(bytes_excluded)
}
