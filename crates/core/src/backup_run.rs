//! The run around a backup engine, whatever the engine copied.
//!
//! The engine copies; the run around it turns the engine's progress into the
//! screen's and reads the finished image back when the job asks for it. The
//! per-volume and the whole-disk drivers each carried their own, nearly
//! identical copy of these steps, and the Linux driver needs the same ones —
//! so they are here, once.

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

#[cfg(target_os = "linux")]
use own_backup_backup::engine_linux::{verify_image_scoped, VerifyScope};
#[cfg(target_os = "windows")]
use own_backup_backup::engine_win::{verify_image_scoped, VerifyScope};
use own_backup_backup::seal::context::ArchiveKeys;
use own_backup_common::progress::BackupProgress;
use own_backup_common::{tr, OwnBackupError};

use crate::engine_watch::EngineWatch;
use crate::{backup_callback_checkpoint, EventSink, Job, JobProgress, Pace, VerifyRate};

/// The run a progress report belongs to and where it goes — the same for the
/// copy and for the check that reads the copy back.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
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
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) enum ProgressLabel {
    /// A volume, by the label the job has for it.
    Volume(String),
    /// A whole disk, by number and model — worded each time it is shown.
    Disk { number: u32, model: String },
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
impl ProgressLabel {
    fn text(&self) -> String {
        match self {
            Self::Volume(label) => label.clone(),
            Self::Disk { number, model } => {
                tr!("lib.disk_label", disk = number, model = model).into_owned()
            }
        }
    }
}

/// The copy's progress callback: the engine's progress in, the screen's out,
/// the job's speed limit held, a cancel answered. A whole-disk copy reports
/// the same phases, its exclusion scan once per partition.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) struct CopyProgress<'a> {
    progress: &'a RunProgress,
    pace: Pace,
    last_permille: u64,
    last_emit: std::time::Instant,
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
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
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
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
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
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
