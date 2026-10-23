//! The run around a backup engine, whatever the engine copied.
//!
//! The engine copies; the run around it turns the engine's progress into the
//! screen's, reads the finished image back when the job asks for it, adds
//! what the copy produced to the run's record and ends the run. The
//! per-volume and the whole-disk drivers each carried their own, nearly
//! identical copy of these steps, and the Linux driver needs the same ones —
//! so they are here, once.
//!
//! A copy's summary — a volume's or a whole disk's — is the shared engine's
//! on both systems.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

#[cfg(target_os = "linux")]
use own_backup_backup::engine_linux::{
    verify_image_scoped, DiskBackupSummary, VerifyScope, VolumeBackupSummary,
};
#[cfg(target_os = "windows")]
use own_backup_backup::engine_win::{
    verify_image_scoped, DiskBackupSummary, VerifyScope, VolumeBackupSummary,
};
use own_backup_backup::seal::context::ArchiveKeys;
use own_backup_common::progress::BackupProgress;
use own_backup_common::{tr, OwnBackupError};

use crate::engine_watch::EngineWatch;
use crate::{
    apply_consolidation, apply_retention, backup_callback_checkpoint, EventSink, Job, JobProgress,
    JobRun, Pace, VerifyRate,
};

/// The run a progress report belongs to and where it goes — the same for the
/// copy and for the check that reads the copy back.
pub(crate) struct RunProgress {
    pub(crate) sink: Arc<dyn EventSink>,
    pub(crate) watch: Arc<EngineWatch>,
    pub(crate) cancel: Arc<AtomicBool>,
    pub(crate) job_id: String,
    /// Which volume of how many: 0 of 1 for a whole disk.
    pub(crate) volume_index: u64,
    pub(crate) volume_count: u64,
    pub(crate) volume: ProgressLabel,
    pub(crate) image_file_name: String,
}

/// What the progress line calls the thing being copied.
pub(crate) enum ProgressLabel {
    /// A volume, by the label the job has for it.
    Volume(String),
    /// A whole disk, by its name (Windows: its number; Linux: its device)
    /// and model — worded each time it is shown.
    Disk { name: String, model: String },
}

impl ProgressLabel {
    fn text(&self) -> String {
        match self {
            Self::Volume(label) => label.clone(),
            Self::Disk { name, model } => {
                tr!("lib.disk_label", disk = name, model = model).into_owned()
            }
        }
    }
}

/// The copy's progress callback: the engine's progress in, the screen's out,
/// the job's speed limit held, a cancel answered. A whole-disk copy reports
/// the same phases, its exclusion scan once per partition.
pub(crate) struct CopyProgress<'a> {
    progress: &'a RunProgress,
    pace: Pace,
    last_permille: u64,
    last_emit: std::time::Instant,
}

impl<'a> CopyProgress<'a> {
    pub(crate) fn new(progress: &'a RunProgress, speed_limit_mbps: u32) -> Self {
        Self {
            progress,
            pace: Pace::new(speed_limit_mbps),
            last_permille: u64::MAX,
            last_emit: std::time::Instant::now(),
        }
    }

    /// One engine callback.
    pub(crate) fn report(&mut self, p: &BackupProgress) -> own_backup_common::Result<()> {
        let progress = self.progress;
        let RunProgress {
            sink: sink_bg,
            watch: watch_bg,
            cancel: cancel_bg,
            job_id,
            ..
        } = progress;
        if cancel_bg.load(std::sync::atomic::Ordering::SeqCst) || watch_bg.abandoned() {
            return Err(OwnBackupError::Cancelled);
        }
        watch_bg.alive();
        // The engine tags non-copy phases via backup_id: "prepare" for
        // the exclusion scan, "finalize" for the flush/detach at the
        // end. Both carry blocks_total == 0.
        let phase = match p.backup_id.as_str() {
            "prepare" => "prepare",
            "finalize" => "finalize",
            // The post-copy exclusion passes (true removal, and the
            // in-image pass for an encrypted source) — minutes of real
            // work that used to hide under "сброс кэша" and read as a
            // hang.
            "exclusions" => "exclusions",
            // Asked just before the image is published, so a run that
            // was cancelled or left behind never publishes.
            "publish" => "finalize",
            _ => "backup",
        };
        if phase == "backup" {
            self.pace.after(p.blocks_done);
        }
        let permille = if p.blocks_total > 0 {
            p.blocks_done * 1000 / p.blocks_total
        } else {
            0
        };
        // "finalize" has no granular progress — always emit it so the UI
        // leaves the frozen 100% state; otherwise throttle as usual.
        // Сообщение со сводкой по типам приходит одно и в единственный
        // момент — сразу после скана. Через троттлинг его пропускать
        // нельзя: другого такого не будет.
        let has_categories = p.categories.is_some();
        if has_categories
            || phase == "finalize"
            || phase == "exclusions"
            || permille != self.last_permille
            || self.last_emit.elapsed().as_millis() >= 400
        {
            self.last_permille = permille;
            self.last_emit = std::time::Instant::now();
            watch_bg.show(
                sink_bg,
                JobProgress {
                    job_id: job_id.clone(),
                    phase: phase.into(),
                    percent: permille / 10,
                    blocks_done: p.blocks_done,
                    blocks_total: p.blocks_total,
                    speed_mbps: p.speed_bps as f64 / 1_000_000.0,
                    volume_index: progress.volume_index,
                    volume_count: progress.volume_count,
                    volume: progress.volume.text(),
                    image_file_name: progress.image_file_name.clone(),
                    categories: p.categories.clone().unwrap_or_default(),
                    excluded_files: p.excluded.map(|(f, _)| f),
                    excluded_bytes: p.excluded.map(|(_, b)| b),
                    stalled_secs: None,
                },
            );
        }
        #[cfg(all(target_os = "windows", feature = "a07-lab-gate"))]
        crate::a07_lab_gate(job_id, phase, p.blocks_done, p.blocks_total)?;
        backup_callback_checkpoint(cancel_bg)
    }
}

/// Only the blocks this copy wrote, or everything the restore point
/// stands on — the job says which (default: everything).
pub(crate) fn verify_scope(job: &Job) -> VerifyScope {
    if job.advanced.verify_latest_only {
        VerifyScope::LatestCopyOnly
    } else {
        VerifyScope::Everything
    }
}

/// Read the finished image back, reporting progress as the copy did, and fail
/// on any checksum mismatch. `Ok(true)` when the user cancelled the check
/// rather than the copy.
pub(crate) fn verify_copy(
    progress: &RunProgress,
    image: &Path,
    keys: Option<&ArchiveKeys>,
    scope: VerifyScope,
) -> own_backup_common::Result<bool> {
    let RunProgress {
        sink: sink_bg,
        watch: watch_bg,
        cancel: cancel_bg,
        job_id,
        ..
    } = progress;
    let mut verify_cancelled = false;
    let mut v_permille = u64::MAX;
    let mut v_emit = std::time::Instant::now();
    let mut v_rate = VerifyRate::new(image, keys);
    let verified = verify_image_scoped(image, keys, scope, |done, total| {
        // Cancel reaches the check too, not just the copy (audit
        // AUD-12). Reading back a whole image is minutes of work;
        // before this, pressing Отмена during it did nothing and
        // the run then finished as a success.
        if cancel_bg.load(std::sync::atomic::Ordering::SeqCst) || watch_bg.abandoned() {
            return Err(OwnBackupError::Cancelled);
        }
        watch_bg.alive();
        let mbps = v_rate.sample(done);
        let permille = if total > 0 { done * 1000 / total } else { 0 };
        if permille != v_permille || v_emit.elapsed().as_millis() >= 400 {
            v_permille = permille;
            v_emit = std::time::Instant::now();
            watch_bg.show(
                sink_bg,
                JobProgress {
                    job_id: job_id.clone(),
                    phase: "verify".into(),
                    percent: permille / 10,
                    blocks_done: done,
                    blocks_total: total,
                    speed_mbps: mbps,
                    volume_index: progress.volume_index,
                    volume_count: progress.volume_count,
                    volume: progress.volume.text(),
                    image_file_name: progress.image_file_name.clone(),
                    categories: Vec::new(),
                    excluded_files: None,
                    excluded_bytes: None,
                    stalled_secs: None,
                },
            );
        }
        #[cfg(all(target_os = "windows", feature = "a07-lab-gate"))]
        crate::a07_lab_gate(job_id, "verify", done, total)?;
        backup_callback_checkpoint(cancel_bg)
    });
    match verified {
        Ok(v) if !v.valid() => {
            return Err(OwnBackupError::ImageFormat(match &progress.volume {
                ProgressLabel::Volume(vol_label) => tr!(
                    "lib.verify_failed_volume",
                    volume = vol_label,
                    count = v.mismatched_blocks.len()
                )
                .into_owned(),
                ProgressLabel::Disk { .. } => {
                    tr!("lib.verify_failed", count = v.mismatched_blocks.len()).into_owned()
                }
            }))
        }
        Ok(_) => {}
        // The copy itself is finished and on disk; only the reading
        // back was stopped. That is a different outcome from a
        // cancelled copy, and the image must survive it.
        Err(OwnBackupError::Cancelled) => verify_cancelled = true,
        Err(e) => return Err(e),
    }
    Ok(verify_cancelled)
}

/// What a run over volumes adds up as each of them finishes, for the end of
/// the run.
pub(crate) struct VolumeTotals {
    /// Each finished copy by its file-name token: the chains retention and
    /// consolidation must leave alone.
    pub(crate) protected: Vec<(String, PathBuf)>,
    kinds: Vec<&'static str>,
    total_dur: f64,
    all_smart: bool,
    volume_count: usize,
}

impl VolumeTotals {
    pub(crate) fn new(volume_count: usize) -> Self {
        Self {
            protected: Vec::new(),
            kinds: Vec::new(),
            total_dur: 0.0,
            all_smart: true,
            volume_count,
        }
    }

    /// Add one finished volume to the run's record; returns the image the
    /// engine published.
    pub(crate) fn record(
        &mut self,
        run: &mut JobRun,
        job: &Job,
        plan: &crate::RunPlan,
        idx: usize,
        device: &str,
        summary: VolumeBackupSummary,
    ) -> PathBuf {
        use crate::{file_token, merge_categories, RunImage};
        use own_backup_common::types::BackupType as BT;
        // The name the engine published: a fallback to a full renames
        // the planned child.
        let image_path = summary.image_path.clone();
        run.source_bytes += summary.source_bytes;
        run.bytes_copied += summary.bytes_copied;
        run.used_blocks += summary.used_blocks;
        run.total_blocks += summary.total_blocks;
        run.blocks_unchanged += summary.blocks_unchanged;
        run.excluded_files += summary.excluded_files;
        run.excluded_bytes += summary.excluded_bytes;
        self.total_dur += summary.duration.as_secs_f64();
        self.all_smart &= summary.smart_mode;
        run.vss_used |= summary.vss_used;
        run.categories = merge_categories(run.categories.take(), summary.categories);
        if summary.resident_in_journal {
            note_resident_in_journal(run);
        }
        for note in summary.notes {
            let line = if self.volume_count > 1 {
                format!("{device}: {note}")
            } else {
                note
            };
            if !run.notes.contains(&line) {
                run.notes.push(line);
            }
        }
        // One line per volume that went unwalked; a run over three
        // volumes must not report the reason of whichever came last.

        if let Some(why) = summary.file_scan_skipped {
            let line = if self.volume_count > 1 {
                format!("{device}: {why}")
            } else {
                why
            };
            if !run.file_scan_skipped.is_empty() {
                run.file_scan_skipped.push_str("; ");
            }
            run.file_scan_skipped.push_str(&line);
        }
        if let Some(why) = summary.excluded_entries_kept {
            let line = if self.volume_count > 1 {
                format!("{device}: {why}")
            } else {
                why
            };
            if !run.excluded_entries_kept.is_empty() {
                run.excluded_entries_kept.push_str("; ");
            }
            run.excluded_entries_kept.push_str(&line);
        }

        let actual_kind = match summary.backup_kind {
            BT::Full => "full",
            BT::Incremental => "incremental",
            _ => "differential",
        };
        self.kinds.push(actual_kind);

        // Where this volume's copy went, and which chain it joined.
        // Computed for EVERY volume: the two single-value fields below
        // describe the first one only, and for years that was the whole
        // record of a multi-volume run (audit AUD-09).
        let base_of_this_volume = match summary.backup_kind {
            BT::Full => image_path.display().to_string(),
            _ => plan
                .base_image
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        };
        run.images.push(RunImage {
            source: device.to_string(),
            image_path: image_path.display().to_string(),
            base_image_path: base_of_this_volume.clone(),
            kind: actual_kind.into(),
        });

        // The primary (first) volume drives the single-value dashboard
        // fields (image to open, chain base, increment index).
        if idx == 0 {
            run.image_path = image_path.display().to_string();
            run.base_image_path = base_of_this_volume;
            run.increment_index = match summary.backup_kind {
                BT::Full => 0,
                _ => plan.increment_index,
            };
        }
        // Retention matches images by their file-name token (label for
        // encrypted volumes), not the device password key.
        self.protected
            .push((file_token(job, device), image_path.clone()));
        image_path
    }

    /// End the run once every volume has finished or one of them stopped it.
    pub(crate) fn finish(
        self,
        mut run: JobRun,
        fatal: Option<(String, String)>,
        job: &Job,
        keys: Option<&ArchiveKeys>,
    ) -> JobRun {
        run.finished_at = chrono::Utc::now().to_rfc3339();
        run.duration_secs = self.total_dur;
        run.smart_mode = !self.protected.is_empty() && self.all_smart;

        // A single volume failure fails the whole run; images already written stay
        // on disk (never fake success), and retention does not run on a partial set.
        if let Some((status, msg)) = fatal {
            run.status = status;
            run.error = msg;
            return run;
        }

        run.status = "success".into();
        // Combined kind: the shared kind if every volume agreed, else "mixed".
        run.backup_kind = match self.kinds.first() {
            Some(first) if self.kinds.iter().all(|k| k == first) => (*first).into(),
            Some(_) => "mixed".into(),
            None => "full".into(),
        };

        // (Whole-disk bootable copies never reach here — they take the single
        // whole-disk-image path, which embeds the restore manifest.)

        // Retention runs after a fully successful backup, independently per volume.
        // Its failures are logged, never surfaced as a run failure.
        retain_and_consolidate(&mut run, job, &self.protected, keys);

        run
    }
}

/// Resident content of excluded files may remain in the copy's NTFS journal
/// — erased incompletely, and said so (CLAUDE.md). Once per run.
fn note_resident_in_journal(run: &mut JobRun) {
    let note = own_backup_common::tr!("lib.resident_in_journal").into_owned();
    if !run.notes.contains(&note) {
        run.notes.push(note);
    }
}

/// Record a finished whole-disk copy in its run; returns the image the
/// engine published. `disk_token` is the disk's file-name token
/// ([`crate::DiskSourceSpec::file_tag`]).
pub(crate) fn record_disk(
    run: &mut JobRun,
    job: &Job,
    plan: &crate::RunPlan,
    disk_token: &str,
    summary: DiskBackupSummary,
) -> PathBuf {
    use crate::RunImage;
    use own_backup_common::types::BackupType;
    use tracing::info;

    // The name the engine published: a fallback to a full renames the
    // planned child.
    let image_path = summary.image_path.clone();
    run.image_path = image_path.display().to_string();
    run.base_image_path = run.image_path.clone();
    run.status = "success".into();
    run.bytes_copied = summary.bytes_copied;
    run.used_blocks = summary.blocks_copied;
    run.blocks_unchanged = summary.blocks_unchanged;
    run.excluded_files = summary.excluded_files;
    run.excluded_bytes = summary.excluded_bytes;
    run.categories = summary.categories;
    run.file_scan_skipped = summary.file_scan_skipped.unwrap_or_default();
    run.excluded_entries_kept = summary.excluded_entries_kept.unwrap_or_default();
    run.notes.extend(summary.notes);
    if summary.resident_in_journal {
        note_resident_in_journal(run);
    }
    run.duration_secs = summary.duration.as_secs_f64();
    // Record what was ACTUALLY produced (the engine may have fallen back
    // to a full when the parent chain was unusable) so the next run plans
    // correctly off the history.
    let (kind_str, is_child) = match summary.backup_kind {
        BackupType::Incremental => ("incremental", true),
        BackupType::Differential => ("differential", true),
        _ => ("full", false),
    };
    run.backup_kind = kind_str.into();
    if is_child {
        if let Some(base) = &plan.base_image {
            run.base_image_path = base.display().to_string();
        }
        run.increment_index = plan.increment_index;
    }
    // One image for the whole disk, but the list is filled in all the
    // same: "everything this job wrote" must be answerable the same way
    // whatever the job's shape (audit AUD-09).
    run.images.push(RunImage {
        source: disk_token.to_string(),
        image_path: run.image_path.clone(),
        base_image_path: run.base_image_path.clone(),
        kind: kind_str.into(),
    });
    info!(job = %job.name, image = %image_path.display(), kind = kind_str,
        partitions = summary.partitions, unchanged = summary.blocks_unchanged,
        "Whole-disk backup complete");
    image_path
}

/// After a fully successful run: retention first, then consolidation (see
/// [`apply_consolidation`] for why in that order). `protected` holds the
/// images the run just wrote, by file-name token.
pub(crate) fn retain_and_consolidate(
    run: &mut JobRun,
    job: &Job,
    protected: &[(String, PathBuf)],
    keys: Option<&ArchiveKeys>,
) {
    apply_retention(job, protected);
    run.consolidated_increments = apply_consolidation(job, protected, keys);
}
