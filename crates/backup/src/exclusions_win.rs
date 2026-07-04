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
use windows::Win32::Foundation::{CloseHandle, ERROR_HANDLE_EOF, ERROR_MORE_DATA, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Ioctl::{FSCTL_GET_RETRIEVAL_POINTERS, STARTING_VCN_INPUT_BUFFER};
use windows::Win32::System::IO::DeviceIoControl;

use own_backup_common::Result;

use crate::exclusions::{parse_patterns, ByteRanges, CategorySizes};
use crate::win_device::to_wide;

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// Outcome of an exclusion scan over a volume.
pub struct ExclusionScan {
    /// Merged excluded byte ranges (volume offsets), finalized.
    pub ranges: ByteRanges,
    pub files_excluded: u64,
    pub entries_scanned: u64,
    /// Total bytes of excluded extents (before merging).
    pub bytes_excluded: u64,
    /// Byte totals per file category for the files that STAY in the backup
    /// (excluded files are not part of the archive). Powers the dashboard
    /// breakdown — the walk visits every file anyway, so this is free.
    pub categories: CategorySizes,
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
    mut tick: impl FnMut(u64) -> Result<()>,
) -> Result<ExclusionScan> {
    let parsed = parse_patterns(patterns);
    let mut scan = ExclusionScan {
        ranges: ByteRanges::new(),
        files_excluded: 0,
        entries_scanned: 0,
        bytes_excluded: 0,
        categories: CategorySizes::default(),
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
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => {
                warn!(dir = %dir.display(), error = %e, "Exclusion scan: cannot list directory");
                continue;
            }
        };
        for entry in entries.flatten() {
            scan.entries_scanned += 1;
            if scan.entries_scanned % 512 == 0 {
                tick(scan.entries_scanned)?;
            }

            let name_lower = entry.file_name().to_string_lossy().to_lowercase();
            let mut rel_child = rel.clone();
            rel_child.push(name_lower.clone());

            // DirEntry::metadata does not traverse symlinks on Windows.
            let md = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let is_reparse = {
                use std::os::windows::fs::MetadataExt;
                md.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            };

            let matched = subtree_excluded
                || parsed.iter().any(|p| p.matches(&name_lower, &rel_child));

            if md.is_dir() {
                // Never follow junctions/symlinks: their targets may live on
                // another volume, and loops are possible.
                if !is_reparse {
                    stack.push((entry.path(), rel_child, matched));
                }
            } else if matched && !is_reparse {
                match file_extent_ranges(&entry.path(), cluster_size, &mut scan.ranges) {
                    Ok(bytes) => {
                        scan.files_excluded += 1;
                        scan.bytes_excluded += bytes;
                    }
                    Err(e) => {
                        warn!(
                            file = %entry.path().display(),
                            error = %e,
                            "Exclusion scan: cannot resolve extents — file stays in the backup"
                        );
                        // Could not exclude it → it stays in the archive.
                        scan.categories.add(&name_lower, md.len());
                    }
                }
            } else if !is_reparse {
                scan.categories.add(&name_lower, md.len());
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
    .map_err(|e| {
        own_backup_common::OwnBackupError::Device(format!("CreateFileW failed: {e}"))
    })?;
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
