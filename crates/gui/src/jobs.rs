//! GUI-owned backup jobs: rich config, JSON persistence, and a serial run
//! queue (plan v2, phase 6 + persistence slice of phase 4).
//!
//! The GUI is the source of truth for now (no service yet — decision
//! 2026-07-02: service comes last). Jobs are persisted as `jobs.json` in the
//! app data dir. Fields that the engine cannot honor yet (schedule auto-run,
//! incremental scheme, file exclusions, password) are stored but not enforced;
//! the UI marks them "в разработке".
//!
//! Runs are serialized through an mpsc queue processed by one background
//! worker, so a second "Создать копию" while a backup is running is queued
//! rather than rejected.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{error, info, warn};

// ---------------------------------------------------------------------------
// Config model (persisted)
// ---------------------------------------------------------------------------

/// Schedule tab. Only `kind`/`days`/`time` shape the (future) scheduler; the
/// rest are saved for the service phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScheduleConfig {
    /// none | daily | weekly | monthly | event | nonstop
    pub kind: String,
    /// weekday keys for `weekly`: mon,tue,wed,thu,fri,sat,sun
    pub days: Vec<String>,
    /// "HH:MM"
    pub time: String,
    pub only_when_locked: bool,
    pub wake_to_run: bool,
    pub prevent_sleep: bool,
    pub run_missed: bool,
    pub missed_delay_min: u32,
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            kind: "none".into(),
            days: vec!["mon".into(), "wed".into(), "fri".into()],
            time: "14:00".into(),
            only_when_locked: false,
            wake_to_run: true,
            prevent_sleep: true,
            run_missed: true,
            missed_delay_min: 0,
        }
    }
}

/// Scheme tab (method + retention). Saved; enforced in phase 5.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SchemeConfig {
    /// full | incremental | differential
    pub method: String,
    /// incremental: only-incremental vs full-every-N
    pub only_incremental: bool,
    pub full_every_n: u32,
    pub cleanup_older_than_days: Option<u32>,
    pub keep_chains: Option<u32>,
    pub keep_first: bool,
}

impl Default for SchemeConfig {
    fn default() -> Self {
        Self {
            method: "full".into(),
            only_incremental: false,
            full_every_n: 5,
            cleanup_older_than_days: None,
            keep_chains: Some(3),
            keep_first: true,
        }
    }
}

/// Exclusions tab. Saved; enforced in phase 3.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ExclusionsConfig {
    pub enabled: bool,
    pub patterns: Vec<String>,
}

impl Default for ExclusionsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            patterns: default_exclusions(),
        }
    }
}

/// Uppercase Windows drive letter of a path or device spec, if it has one.
/// `E:`, `E:\Foo`, `\\.\E:`, `\\?\E:\` → `Some('E')`; UNC shares
/// (`\\server\share`) and VSS/GLOBALROOT device paths → `None`.
pub fn drive_letter_of(s: &str) -> Option<char> {
    let t = s.trim();
    let t = t
        .strip_prefix(r"\\.\")
        .or_else(|| t.strip_prefix(r"\\?\"))
        .unwrap_or(t);
    let b = t.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        Some(b[0].to_ascii_uppercase() as char)
    } else {
        None
    }
}

/// True when the destination folder sits on the very volume being backed up.
/// Writing the image there would grow the source while it is being imaged —
/// forbidden. UNC/network destinations (no drive letter) are always allowed.
pub fn dest_on_source_volume(source: &str, dest_folder: &str) -> bool {
    match (drive_letter_of(source), drive_letter_of(dest_folder)) {
        (Some(src), Some(dst)) => src == dst,
        _ => false,
    }
}

pub fn default_exclusions() -> Vec<String> {
    [
        // Volatile system files — huge and pointless to image.
        "pagefile.sys",
        "hiberfil.sys",
        "swapfile.sys",
        // Recycle bin and VSS/system state — restored/rebuilt by Windows.
        "$Recycle.Bin",
        "System Volume Information",
        // Temp / scratch — any Temp/Tmp directory at any depth on the volume,
        // plus loose temp files. `**` matches any run of path components.
        "**\\Temp\\**",
        "**\\Tmp\\**",
        "*.tmp",
        "*.~",
        // Our own image files, in case the destination lives on the source.
        "*.vhdx",
        "*.avhdx",
        "*.obk",
        "*.obkm",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Advanced tab. `sector_by_sector` and `validate_after` are enforced now; the
/// rest are saved for later ("в разработке" in the UI).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AdvancedConfig {
    pub sector_by_sector: bool,
    pub validate_after: bool,
    // saved-for-later
    pub backup_unallocated: bool,
    pub password_enabled: bool,
    pub pre_command: String,
    pub post_command: String,
    pub error_handling: String,
    pub shutdown_after: bool,
    pub performance: String,
}

impl Default for AdvancedConfig {
    fn default() -> Self {
        Self {
            sector_by_sector: false,
            validate_after: false,
            backup_unallocated: false,
            password_enabled: false,
            pre_command: String::new(),
            post_command: String::new(),
            error_handling: "stop".into(),
            shutdown_after: false,
            performance: "normal".into(),
        }
    }
}

/// A single completed (or failed) run — feeds the dashboard and Activity.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct JobRun {
    pub id: String,
    pub job_id: String,
    pub job_name: String,
    pub started_at: String,
    pub finished_at: String,
    /// success | error | cancelled
    pub status: String,
    pub image_path: String,
    pub source_bytes: u64,
    pub bytes_copied: u64,
    pub used_blocks: u64,
    pub total_blocks: u64,
    pub duration_secs: f64,
    pub smart_mode: bool,
    pub vss_used: bool,
    pub error: String,
    /// full | incremental | differential — what was actually created (the
    /// engine may fall back to full when the chain is unusable). Empty in
    /// runs recorded before phase 5 (those were always full).
    pub backup_kind: String,
    /// The chain's full image this run belongs to (== image_path for fulls).
    pub base_image_path: String,
    /// 0 for a full backup, N for the N-th child since the last full.
    pub increment_index: u32,
    /// Used blocks skipped because they matched the parent version.
    pub blocks_unchanged: u64,
    /// Files excluded by patterns (phase 3) and the bytes they freed.
    pub excluded_files: u64,
    pub excluded_bytes: u64,
    /// Real per-category byte totals of the archived files (collected during
    /// the exclusion walk); None when the walk did not run.
    pub categories: Option<own_backup_backup::exclusions::CategorySizes>,
}

/// A persisted backup job (task).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Job {
    pub id: String,
    pub name: String,
    /// Source volume device, e.g. "E:".
    pub source: String,
    /// Human label for display, e.g. "E: WorkDisk-2 — 347 GB".
    pub source_label: String,
    /// Destination FOLDER; each run creates `<name>_<letter>_<stamp>.vhdx`.
    pub dest_folder: String,
    pub schedule: ScheduleConfig,
    pub scheme: SchemeConfig,
    pub exclusions: ExclusionsConfig,
    pub advanced: AdvancedConfig,
    pub created_at: String,
    pub updated_at: String,
    pub last_run: Option<JobRun>,
}

impl Default for Job {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            source: String::new(),
            source_label: String::new(),
            dest_folder: String::new(),
            schedule: ScheduleConfig::default(),
            scheme: SchemeConfig::default(),
            exclusions: ExclusionsConfig::default(),
            advanced: AdvancedConfig::default(),
            created_at: String::new(),
            updated_at: String::new(),
            last_run: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// Thread-safe, file-backed job store. Runs history is kept in a separate
/// `activity` list capped to the most recent entries.
pub struct JobStore {
    jobs_path: PathBuf,
    activity_path: PathBuf,
    jobs: Mutex<Vec<Job>>,
    activity: Mutex<Vec<JobRun>>,
}

const ACTIVITY_CAP: usize = 200;

impl JobStore {
    pub fn load(data_dir: &std::path::Path) -> Arc<Self> {
        let jobs_path = data_dir.join("jobs.json");
        let activity_path = data_dir.join("activity.json");
        let jobs = read_json(&jobs_path).unwrap_or_default();
        let activity = read_json(&activity_path).unwrap_or_default();
        Arc::new(Self {
            jobs_path,
            activity_path,
            jobs: Mutex::new(jobs),
            activity: Mutex::new(activity),
        })
    }

    pub fn list(&self) -> Vec<Job> {
        self.jobs.lock().unwrap().clone()
    }

    pub fn get(&self, id: &str) -> Option<Job> {
        self.jobs.lock().unwrap().iter().find(|j| j.id == id).cloned()
    }

    /// Insert or update a job; returns the stored job.
    pub fn upsert(&self, mut job: Job) -> Job {
        let now = chrono::Utc::now().to_rfc3339();
        {
            let mut jobs = self.jobs.lock().unwrap();
            if job.id.is_empty() {
                job.id = uuid::Uuid::new_v4().to_string();
                job.created_at = now.clone();
                job.updated_at = now;
                jobs.push(job.clone());
            } else if let Some(existing) = jobs.iter_mut().find(|j| j.id == job.id) {
                job.created_at = existing.created_at.clone();
                job.last_run = existing.last_run.clone(); // preserve run history
                job.updated_at = now;
                *existing = job.clone();
            } else {
                job.created_at = now.clone();
                job.updated_at = now;
                jobs.push(job.clone());
            }
        }
        self.persist_jobs();
        job
    }

    pub fn delete(&self, id: &str) {
        self.jobs.lock().unwrap().retain(|j| j.id != id);
        self.persist_jobs();
    }

    pub fn record_run(&self, run: JobRun) {
        {
            let mut jobs = self.jobs.lock().unwrap();
            if let Some(job) = jobs.iter_mut().find(|j| j.id == run.job_id) {
                job.last_run = Some(run.clone());
            }
        }
        {
            let mut activity = self.activity.lock().unwrap();
            activity.insert(0, run);
            activity.truncate(ACTIVITY_CAP);
        }
        self.persist_jobs();
        self.persist_activity();
    }

    pub fn activity(&self) -> Vec<JobRun> {
        self.activity.lock().unwrap().clone()
    }

    /// Most recent successful run of a job. Chain planning keys on this, not
    /// on `last_run`: a failed or cancelled run must not reset an incremental
    /// chain back to a full backup.
    pub fn last_success(&self, job_id: &str) -> Option<JobRun> {
        self.activity
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.job_id == job_id && r.status == "success")
            .cloned()
    }

    fn persist_jobs(&self) {
        if let Err(e) = write_json(&self.jobs_path, &*self.jobs.lock().unwrap()) {
            error!(error = %e, "Failed to persist jobs.json");
        }
    }

    fn persist_activity(&self) {
        if let Err(e) = write_json(&self.activity_path, &*self.activity.lock().unwrap()) {
            error!(error = %e, "Failed to persist activity.json");
        }
    }
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &std::path::Path) -> Option<T> {
    let data = std::fs::read(path).ok()?;
    match serde_json::from_slice(&data) {
        Ok(v) => Some(v),
        Err(e) => {
            warn!(path = %path.display(), error = %e, "Failed to parse store file");
            None
        }
    }
}

fn write_json<T: Serialize>(path: &std::path::Path, value: &T) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    std::fs::rename(&tmp, path)
}

// ---------------------------------------------------------------------------
// Run queue
// ---------------------------------------------------------------------------

/// Queue state shared between the `run_job` command and the worker.
#[derive(Clone)]
pub struct RunQueue {
    pub pending: Arc<Mutex<VecDeque<String>>>,
    pub current: Arc<Mutex<Option<String>>>,
}

impl RunQueue {
    pub fn new() -> Self {
        Self {
            pending: Arc::new(Mutex::new(VecDeque::new())),
            current: Arc::new(Mutex::new(None)),
        }
    }

    pub fn snapshot(&self) -> QueueSnapshot {
        QueueSnapshot {
            current: self.current.lock().unwrap().clone(),
            pending: self.pending.lock().unwrap().iter().cloned().collect(),
        }
    }
}

#[derive(Serialize, Clone)]
pub struct QueueSnapshot {
    pub current: Option<String>,
    pub pending: Vec<String>,
}

/// Progress payload emitted as `job-progress`.
#[derive(Serialize, Clone)]
#[cfg_attr(not(target_os = "windows"), allow(dead_code))] // emitted only by the Windows engine path
pub struct JobProgress {
    pub job_id: String,
    /// "backup" while copying, "verify" during post-backup validation.
    pub phase: String,
    pub percent: u64,
    pub blocks_done: u64,
    pub blocks_total: u64,
    pub speed_mbps: f64,
}

fn emit_queue(app: &AppHandle, queue: &RunQueue) {
    let _ = app.emit("queue-changed", queue.snapshot());
}

/// Background worker: pulls job ids off the channel and runs them one at a
/// time. Spawned once at startup.
pub async fn run_queue_worker(
    app: AppHandle,
    store: Arc<JobStore>,
    queue: RunQueue,
    cancel: Arc<AtomicBool>,
    mut rx: UnboundedReceiver<String>,
) {
    while let Some(job_id) = rx.recv().await {
        // Skip if it was cancelled out of the pending queue.
        {
            let mut pending = queue.pending.lock().unwrap();
            if let Some(pos) = pending.iter().position(|id| *id == job_id) {
                pending.remove(pos);
            } else {
                continue;
            }
        }
        let job = match store.get(&job_id) {
            Some(j) => j,
            None => continue,
        };

        *queue.current.lock().unwrap() = Some(job_id.clone());
        emit_queue(&app, &queue);
        info!(job = %job.name, "Starting queued backup");

        cancel.store(false, Ordering::SeqCst);
        let last_success = store.last_success(&job.id);
        let run = perform_backup(&app, &job, &cancel, last_success).await;
        store.record_run(run.clone());

        *queue.current.lock().unwrap() = None;
        emit_queue(&app, &queue);
        let _ = app.emit("job-finished", run);
    }
}

/// What the next run of a job should produce.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(target_os = "windows"), allow(dead_code))] // consumed by the Windows backup path
pub struct RunPlan {
    /// full | incremental | differential
    pub kind: &'static str,
    /// Parent image for a child run (previous version for incremental, the
    /// chain's full for differential).
    pub parent_image: Option<PathBuf>,
    /// The chain's full image a child run belongs to.
    pub base_image: Option<PathBuf>,
    /// Index of this run within its chain (0 = full).
    pub increment_index: u32,
}

impl RunPlan {
    fn full() -> Self {
        Self {
            kind: "full",
            parent_image: None,
            base_image: None,
            increment_index: 0,
        }
    }
}

/// Decide whether the next run is a full backup or a child of an existing
/// chain, based on the job scheme and the last SUCCESSFUL run (failed or
/// cancelled runs in between do not reset the chain). Any doubt (no history,
/// files missing, rollover reached) resolves to a full backup.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))] // consumed by the Windows backup path
pub fn plan_run(job: &Job, last_success: Option<&JobRun>) -> RunPlan {
    let method: &'static str = match job.scheme.method.as_str() {
        "incremental" => "incremental",
        "differential" => "differential",
        _ => return RunPlan::full(),
    };
    let Some(last) = last_success.filter(|r| r.status == "success") else {
        return RunPlan::full();
    };
    // Runs recorded before phase 5 have an empty kind — they were all full.
    let last_kind = if last.backup_kind.is_empty() { "full" } else { &last.backup_kind };

    // "Полная версия после каждых N версий": the chain holds at most N children.
    if !job.scheme.only_incremental && last.increment_index >= job.scheme.full_every_n.max(1) {
        return RunPlan::full();
    }

    let base = if last_kind == "full" {
        PathBuf::from(&last.image_path)
    } else if !last.base_image_path.is_empty() {
        PathBuf::from(&last.base_image_path)
    } else {
        return RunPlan::full();
    };
    let parent = match method {
        "incremental" => PathBuf::from(&last.image_path),
        _ => base.clone(),
    };

    // The parent must still exist with its sidecar; otherwise start fresh.
    let sidecar = own_backup_backup::sidecar_path_for(&parent);
    if !parent.exists() || !sidecar.exists() {
        return RunPlan::full();
    }

    RunPlan {
        kind: method,
        parent_image: Some(parent),
        base_image: Some(base),
        increment_index: last.increment_index + 1,
    }
}

/// Timestamped output file name for a run:
/// `<name>_<letter>_<full|inc|diff>_<stamp>.<vhdx|avhdx>`.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))] // used by the Windows backup path
fn output_path(job: &Job, kind: &str) -> PathBuf {
    let letter = job.source.chars().next().unwrap_or('X');
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let safe_name: String = job
        .name
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    let (tag, ext) = match kind {
        "incremental" => ("inc", "avhdx"),
        "differential" => ("diff", "avhdx"),
        _ => ("full", "vhdx"),
    };
    PathBuf::from(&job.dest_folder).join(format!("{safe_name}_{letter}_{tag}_{stamp}.{ext}"))
}

/// Apply the job's retention rules after a successful run. Failures are
/// logged, never propagated — a finished backup must not be reported as
/// failed because cleanup could not delete something.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))] // used by the Windows backup path
fn apply_retention(job: &Job, protect_image: &std::path::Path) {
    use own_backup_backup::chains::{delete_chain, list_job_chains, plan_retention, RetentionRules};

    let rules = RetentionRules {
        keep_chains: job.scheme.keep_chains,
        cleanup_older_than_days: job.scheme.cleanup_older_than_days,
        keep_first: job.scheme.keep_first,
    };
    if rules.keep_chains.is_none() && rules.cleanup_older_than_days.is_none() {
        return;
    }
    let folder = PathBuf::from(&job.dest_folder);
    match list_job_chains(&folder, &job.id) {
        Ok(chains) => {
            let doomed = plan_retention(&chains, &rules, chrono::Utc::now(), protect_image);
            for idx in doomed {
                info!(
                    job = %job.name,
                    base = %chains[idx].images.first().map(|i| i.image_path.display().to_string()).unwrap_or_default(),
                    images = chains[idx].images.len(),
                    "Retention: deleting old backup chain"
                );
                delete_chain(&chains[idx]);
            }
        }
        Err(e) => warn!(job = %job.name, error = %e, "Retention scan failed"),
    }
}

fn run_skeleton(job: &Job) -> JobRun {
    JobRun {
        id: uuid::Uuid::new_v4().to_string(),
        job_id: job.id.clone(),
        job_name: job.name.clone(),
        started_at: chrono::Utc::now().to_rfc3339(),
        ..Default::default()
    }
}

#[cfg(target_os = "windows")]
async fn perform_backup(
    app: &AppHandle,
    job: &Job,
    cancel: &Arc<AtomicBool>,
    last_success: Option<JobRun>,
) -> JobRun {
    use own_backup_backup::engine_win::{backup_volume, verify_image, VolumeBackupRequest};
    use own_backup_common::types::BackupType;
    use own_backup_common::OwnBackupError;

    let mut run = run_skeleton(job);
    let plan = plan_run(job, last_success.as_ref());
    let image_path = output_path(job, plan.kind);
    run.image_path = image_path.display().to_string();

    // Exclusions only make sense in smart NTFS mode; a sector-by-sector image
    // is a verbatim copy by definition.
    let exclusion_patterns = if job.exclusions.enabled && !job.advanced.sector_by_sector {
        job.exclusions.patterns.clone()
    } else {
        Vec::new()
    };

    let request = VolumeBackupRequest {
        source: job.source.clone(),
        image_path: image_path.clone(),
        block_size: own_backup_backup::engine_win::DEFAULT_BLOCK_SIZE,
        all_sectors: job.advanced.sector_by_sector,
        use_vss: true,
        kind: match plan.kind {
            "incremental" => BackupType::Incremental,
            "differential" => BackupType::Differential,
            _ => BackupType::Full,
        },
        parent_image: plan.parent_image.clone(),
        exclusion_patterns,
        job_id: Some(job.id.clone()),
    };
    let validate_after = job.advanced.validate_after;

    let app_bg = app.clone();
    let job_id = job.id.clone();
    let cancel_bg = cancel.clone();
    let job_bg = job.clone();

    let result = tauri::async_runtime::spawn_blocking(move || {
        let mut last_permille = u64::MAX;
        let mut last_emit = std::time::Instant::now();
        let summary = backup_volume(&request, |p| {
            if cancel_bg.swap(false, std::sync::atomic::Ordering::SeqCst) {
                return Err(OwnBackupError::Cancelled);
            }
            // The engine tags non-copy phases via backup_id: "prepare" for the
            // exclusion scan, "finalize" for the flush/detach at the end. Both
            // carry blocks_total == 0.
            let phase = match p.backup_id.as_str() {
                "prepare" => "prepare",
                "finalize" => "finalize",
                _ => "backup",
            };
            let permille = if p.blocks_total > 0 {
                p.blocks_done * 1000 / p.blocks_total
            } else {
                0
            };
            // "finalize" has no granular progress — always emit it so the UI
            // leaves the frozen 100% state; otherwise throttle as usual.
            if phase == "finalize"
                || permille != last_permille
                || last_emit.elapsed().as_millis() >= 400
            {
                last_permille = permille;
                last_emit = std::time::Instant::now();
                let _ = app_bg.emit(
                    "job-progress",
                    JobProgress {
                        job_id: job_id.clone(),
                        phase: phase.into(),
                        percent: permille / 10,
                        blocks_done: p.blocks_done,
                        blocks_total: p.blocks_total,
                        speed_mbps: p.speed_bps as f64 / 1_000_000.0,
                    },
                );
            }
            Ok(())
        })?;

        // Optional post-backup verification — emit its own progress so the UI
        // shows "Проверка…" instead of freezing at 100%.
        if validate_after {
            let mut v_permille = u64::MAX;
            let mut v_emit = std::time::Instant::now();
            let v = verify_image(&image_path, |done, total| {
                let permille = if total > 0 { done * 1000 / total } else { 0 };
                if permille != v_permille || v_emit.elapsed().as_millis() >= 400 {
                    v_permille = permille;
                    v_emit = std::time::Instant::now();
                    let _ = app_bg.emit(
                        "job-progress",
                        JobProgress {
                            job_id: job_id.clone(),
                            phase: "verify".into(),
                            percent: permille / 10,
                            blocks_done: done,
                            blocks_total: total,
                            speed_mbps: 0.0,
                        },
                    );
                }
                Ok(())
            })?;
            if !v.valid() {
                return Err(OwnBackupError::ImageFormat(format!(
                    "проверка после копирования не пройдена: {} повреждённых блоков",
                    v.mismatched_blocks.len()
                )));
            }
        }

        // Retention runs after a fully successful backup (+verify). Its
        // failures are logged, never surfaced as a run failure.
        apply_retention(&job_bg, &image_path);

        Ok::<_, OwnBackupError>(summary)
    })
    .await;

    run.finished_at = chrono::Utc::now().to_rfc3339();
    match result {
        Ok(Ok(summary)) => {
            use own_backup_common::types::BackupType as BT;
            run.status = "success".into();
            run.source_bytes = summary.source_bytes;
            run.bytes_copied = summary.bytes_copied;
            run.used_blocks = summary.used_blocks;
            run.total_blocks = summary.total_blocks;
            run.duration_secs = summary.duration.as_secs_f64();
            run.smart_mode = summary.smart_mode;
            run.vss_used = summary.vss_used;
            run.blocks_unchanged = summary.blocks_unchanged;
            run.excluded_files = summary.excluded_files;
            run.excluded_bytes = summary.excluded_bytes;
            run.categories = summary.categories;
            // The engine may have fallen back to a full backup.
            match summary.backup_kind {
                BT::Full => {
                    run.backup_kind = "full".into();
                    run.base_image_path = run.image_path.clone();
                    run.increment_index = 0;
                }
                kind => {
                    run.backup_kind = match kind {
                        BT::Incremental => "incremental".into(),
                        _ => "differential".into(),
                    };
                    run.base_image_path = plan
                        .base_image
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default();
                    run.increment_index = plan.increment_index;
                }
            }
        }
        Ok(Err(OwnBackupError::Cancelled)) => {
            run.status = "cancelled".into();
            run.error = "Отменено пользователем".into();
        }
        Ok(Err(e)) => {
            run.status = "error".into();
            run.error = e.to_string();
            warn!(job = %job.name, error = %run.error, "Backup failed");
        }
        Err(e) => {
            run.status = "error".into();
            run.error = format!("задача прервана: {e}");
        }
    }
    run
}

#[cfg(not(target_os = "windows"))]
async fn perform_backup(
    _app: &AppHandle,
    job: &Job,
    _cancel: &Arc<AtomicBool>,
    _last_success: Option<JobRun>,
) -> JobRun {
    let mut run = run_skeleton(job);
    run.finished_at = chrono::Utc::now().to_rfc3339();
    run.status = "error".into();
    run.error = "Движок бэкапа доступен только на Windows".into();
    run
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// A job with `method` plus a successful run that produced `kind` at
    /// `image` (image + sidecar files actually present in `dir` when `create`
    /// is set).
    fn job_with_last(
        dir: &Path,
        method: &str,
        kind: &str,
        index: u32,
        create: bool,
    ) -> (Job, JobRun, PathBuf) {
        let image = dir.join(format!("j_E_{kind}_x.vhdx"));
        let base = dir.join("j_E_full_x.vhdx");
        if create {
            for p in [&image, &base] {
                std::fs::write(p, b"img").unwrap();
                std::fs::write(own_backup_backup::sidecar_path_for(p), b"sc").unwrap();
            }
        }
        let mut job = Job::default();
        job.id = "job1".into();
        job.scheme.method = method.into();
        let run = JobRun {
            job_id: "job1".into(),
            status: "success".into(),
            image_path: image.display().to_string(),
            base_image_path: base.display().to_string(),
            backup_kind: kind.into(),
            increment_index: index,
            ..Default::default()
        };
        (job, run, image)
    }

    #[test]
    fn full_method_always_plans_full() {
        let dir = tempfile::tempdir().unwrap();
        let (job, run, _) = job_with_last(dir.path(), "full", "full", 0, true);
        assert_eq!(plan_run(&job, Some(&run)), RunPlan::full());
    }

    #[test]
    fn incremental_without_history_plans_full() {
        let mut job = Job::default();
        job.scheme.method = "incremental".into();
        assert_eq!(plan_run(&job, None), RunPlan::full());
    }

    #[test]
    fn incremental_chains_to_previous_version() {
        let dir = tempfile::tempdir().unwrap();
        let (job, run, image) = job_with_last(dir.path(), "incremental", "incremental", 2, true);
        let plan = plan_run(&job, Some(&run));
        assert_eq!(plan.kind, "incremental");
        assert_eq!(plan.parent_image.as_deref(), Some(image.as_path()));
        assert_eq!(
            plan.base_image.as_deref(),
            Some(dir.path().join("j_E_full_x.vhdx").as_path())
        );
        assert_eq!(plan.increment_index, 3);
    }

    #[test]
    fn differential_chains_to_the_full_base() {
        let dir = tempfile::tempdir().unwrap();
        let (job, run, _) = job_with_last(dir.path(), "differential", "differential", 1, true);
        let plan = plan_run(&job, Some(&run));
        assert_eq!(plan.kind, "differential");
        // Parent is the BASE, not the previous differential.
        assert_eq!(
            plan.parent_image.as_deref(),
            Some(dir.path().join("j_E_full_x.vhdx").as_path())
        );
        assert_eq!(plan.increment_index, 2);
    }

    #[test]
    fn rollover_after_n_children() {
        let dir = tempfile::tempdir().unwrap();
        let (mut job, mut run, _) = job_with_last(dir.path(), "incremental", "incremental", 5, true);
        job.scheme.full_every_n = 5;
        job.scheme.only_incremental = false;
        assert_eq!(plan_run(&job, Some(&run)), RunPlan::full());

        // Below the limit the chain continues.
        run.increment_index = 4;
        assert_eq!(plan_run(&job, Some(&run)).kind, "incremental");

        // only_incremental disables the rollover entirely.
        run.increment_index = 99;
        job.scheme.only_incremental = true;
        assert_eq!(plan_run(&job, Some(&run)).kind, "incremental");
    }

    #[test]
    fn missing_parent_files_force_full() {
        let dir = tempfile::tempdir().unwrap();
        let (job, run, _) = job_with_last(dir.path(), "incremental", "incremental", 1, false);
        assert_eq!(plan_run(&job, Some(&run)), RunPlan::full());
    }

    #[test]
    fn legacy_full_run_parents_a_new_chain() {
        // Runs recorded before phase 5 have backup_kind == "" — they were full.
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("old_E_20260701.vhdx");
        std::fs::write(&image, b"img").unwrap();
        std::fs::write(own_backup_backup::sidecar_path_for(&image), b"sc").unwrap();

        let mut job = Job::default();
        job.scheme.method = "incremental".into();
        let run = JobRun {
            status: "success".into(),
            image_path: image.display().to_string(),
            ..Default::default()
        };
        let plan = plan_run(&job, Some(&run));
        assert_eq!(plan.kind, "incremental");
        assert_eq!(plan.parent_image.as_deref(), Some(image.as_path()));
        assert_eq!(plan.base_image.as_deref(), Some(image.as_path()));
        assert_eq!(plan.increment_index, 1);
    }

    #[test]
    fn drive_letter_extraction() {
        assert_eq!(drive_letter_of("E:"), Some('E'));
        assert_eq!(drive_letter_of("e:\\OwnBackup"), Some('E'));
        assert_eq!(drive_letter_of(r"\\.\E:"), Some('E'));
        assert_eq!(drive_letter_of(r"\\?\E:\Foo"), Some('E'));
        // No drive letter: UNC shares and VSS/device paths.
        assert_eq!(drive_letter_of(r"\\server\share\Backups"), None);
        assert_eq!(drive_letter_of(r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy5"), None);
    }

    #[test]
    fn rejects_dest_on_source_volume() {
        assert!(dest_on_source_volume("E:", r"E:\OwnBackup"));
        assert!(dest_on_source_volume(r"\\.\E:", r"e:\backups")); // case-insensitive
        assert!(!dest_on_source_volume("E:", r"D:\OwnBackup"));
        // Network destination is fine even when backing up any local drive.
        assert!(!dest_on_source_volume("E:", r"\\nas\share\OwnBackup"));
    }

    #[test]
    fn failed_run_does_not_reset_the_chain() {
        // last_success skips over failed/cancelled runs, so one bad run in
        // between must not force the next backup back to full.
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::load(dir.path());
        let (_, good, _) = job_with_last(dir.path(), "incremental", "incremental", 1, true);
        store.record_run(good.clone());
        store.record_run(JobRun {
            job_id: "job1".into(),
            status: "error".into(),
            ..Default::default()
        });
        let found = store.last_success("job1").expect("success run found");
        assert_eq!(found.image_path, good.image_path);
        // Other jobs' runs are invisible.
        assert!(store.last_success("other").is_none());
    }
}
