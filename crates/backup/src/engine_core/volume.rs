//! A volume backup: one volume, or a snapshot of one, copied into a fresh
//! image — what is asked of it and what it reports. The pipeline itself still
//! lives in `engine_win` and moves here step by step.

use std::path::PathBuf;
use std::time::Duration;

use zeroize::Zeroizing;

use own_backup_common::types::{BackupType, ImageFormat};

/// Parameters for a volume backup.
#[derive(Clone)]
pub struct VolumeBackupRequest {
    /// Source volume: `C:`, `\\.\C:` or a VSS snapshot device path.
    pub source: String,
    /// Output image path (`.vhdx`/`.avhdx`). Must not exist yet.
    pub image_path: PathBuf,
    /// Where the image goes when a child was asked for and a FULL comes out
    /// (parent unusable, disk re-partitioned): the file name says what the file
    /// is, and so does the name recorded inside it. `None` keeps `image_path`
    /// whatever happens.
    pub full_image_path: Option<PathBuf>,
    /// Backup block size in bytes; use [`DEFAULT_BLOCK_SIZE`](super::target::DEFAULT_BLOCK_SIZE).
    pub block_size: u32,
    /// Copy every block even when an NTFS bitmap is available.
    pub all_sectors: bool,
    /// Take a VSS shadow copy of the volume and read from it (consistent
    /// backup of a live volume). Falls back to reading the live volume with a
    /// warning if VSS fails. Ignored for non-volume sources.
    pub use_vss: bool,
    /// Full / Incremental / Differential (phase 5). Non-full kinds need
    /// `parent_image`; if the chain turns out unusable (parent missing,
    /// volume resized, block size changed) the engine falls back to a full
    /// backup — the summary reports what was actually done.
    pub kind: BackupType,
    /// Parent image for incremental (previous version) or differential (the
    /// chain's full backup).
    pub parent_image: Option<PathBuf>,
    /// File-exclusion glob patterns (phase 3). Applied only in smart NTFS
    /// mode; excluded clusters are dropped from the used map and zeroed
    /// inside blocks that are still copied.
    pub exclusion_patterns: Vec<String>,
    /// GUI job id, stamped into the sidecar so retention can find this
    /// job's chains later.
    pub job_id: Option<String>,
    /// Passphrase for an encrypted (LUKS) source. When present and the source
    /// is a LUKS container, the volume is unlocked and its *decrypted* data is
    /// backed up (used blocks only); on restore, encryption must be re-applied
    /// manually. Ignored for non-encrypted sources.
    pub passphrase: Option<Zeroizing<String>>,
    /// Image container format (decision #13): VHDX — kernel-written, Windows-
    /// native mounting, NTFS/ReFS true removal, AVHDX chains; qcow2 — written
    /// in userspace with per-cluster zstd compression, no attach at all.
    /// qcow2 limits for now: full backups only, exclusions are zeroing-only.
    pub image_format: ImageFormat,
    /// Full source-disk layout snapshot (the parent physical disk of this
    /// volume, from the GUI's live inventory) to embed in the sidecar, so a
    /// restore can show the whole disk and flag the backed-up partition without
    /// jobs.json. `None` for CLI runs (the sidecar then omits it).
    pub source_disk: Option<own_backup_common::disk::DiskInfo>,
    /// Present for an encrypted job: the keys the service opened before this
    /// run started. Nothing inside the run asks a human anything (§7.4), and
    /// the image written is `.obk` whatever `image_format` says.
    pub encryption: Option<crate::seal::context::ArchiveKeys>,
    /// zstd level for qcow2 and `.obk` (1–19; anything else is the default,
    /// see [`crate::qcow2::zstd_level_or_default`]). VHDX is not compressed.
    pub zstd_level: i32,
}

/// Outcome of a completed volume backup.
pub struct VolumeBackupSummary {
    pub image_path: PathBuf,
    pub sidecar_path: PathBuf,
    pub source_bytes: u64,
    pub total_blocks: u64,
    pub used_blocks: u64,
    pub blocks_copied: u64,
    pub bytes_copied: u64,
    pub duration: Duration,
    /// True when the NTFS used-sector bitmap was applied; false = every
    /// block was copied (explicit sector-by-sector or non-NTFS fallback).
    pub smart_mode: bool,
    /// True when the data was read from a VSS shadow copy.
    pub vss_used: bool,
    /// What was actually produced — may differ from the requested kind when
    /// the engine fell back to a full backup.
    pub backup_kind: BackupType,
    /// Used blocks skipped because they match the parent version.
    pub blocks_unchanged: u64,
    /// Files whose extents were excluded (phase 3).
    pub excluded_files: u64,
    /// Bytes excluded from the image by exclusion patterns.
    pub excluded_bytes: u64,
    /// Per-category byte totals of the files in the archive; None when no
    /// exclusion walk ran (exclusions disabled / non-NTFS / sector mode).
    pub categories: Option<crate::exclusions::CategorySizes>,
    /// Why the file walk did not run, in words for the result screen. `None`
    /// when it did run.
    ///
    /// One walk produces both the exclusions and the by-type breakdown, so one
    /// sentence explains both absences. It exists because a copy that quietly
    /// ignored twelve exclusion rules and then reported "Исключено файлов: 0"
    /// tells the user the rules matched nothing — when in fact they were never
    /// applied. That difference is the whole point of having the rules.
    pub file_scan_skipped: Option<String>,
    /// Why the excluded files' catalog entries were NOT deleted from the
    /// image, in words for the result screen — `None` when they were, or when
    /// there was nothing to delete. See `engine_win::DiskBackupSummary::excluded_entries_kept`.
    pub excluded_entries_kept: Option<String>,
}
