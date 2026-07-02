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

pub fn default_exclusions() -> Vec<String> {
    [
        "pagefile.sys",
        "hiberfil.sys",
        "swapfile.sys",
        "$Recycle.Bin",
        "System Volume Information",
        "*.tmp",
        "*.~",
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
        let run = perform_backup(&app, &job, &cancel).await;
        store.record_run(run.clone());

        *queue.current.lock().unwrap() = None;
        emit_queue(&app, &queue);
        let _ = app.emit("job-finished", run);
    }
}

/// Timestamped output file name for a run: `<name>_<letter>_<stamp>.vhdx`.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))] // used by the Windows backup path
fn output_path(job: &Job) -> PathBuf {
    let letter = job.source.chars().next().unwrap_or('X');
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let safe_name: String = job
        .name
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    PathBuf::from(&job.dest_folder).join(format!("{safe_name}_{letter}_{stamp}.vhdx"))
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
async fn perform_backup(app: &AppHandle, job: &Job, cancel: &Arc<AtomicBool>) -> JobRun {
    use own_backup_backup::engine_win::{backup_volume, verify_image, VolumeBackupRequest};
    use own_backup_common::OwnBackupError;

    let mut run = run_skeleton(job);
    let image_path = output_path(job);
    run.image_path = image_path.display().to_string();

    let request = VolumeBackupRequest {
        source: job.source.clone(),
        image_path: image_path.clone(),
        block_size: own_backup_backup::engine_win::DEFAULT_BLOCK_SIZE,
        all_sectors: job.advanced.sector_by_sector,
        use_vss: true,
    };
    let validate_after = job.advanced.validate_after;

    let app_bg = app.clone();
    let job_id = job.id.clone();
    let cancel_bg = cancel.clone();

    let result = tauri::async_runtime::spawn_blocking(move || {
        let mut last_permille = u64::MAX;
        let mut last_emit = std::time::Instant::now();
        let summary = backup_volume(&request, |p| {
            if cancel_bg.swap(false, std::sync::atomic::Ordering::SeqCst) {
                return Err(OwnBackupError::Cancelled);
            }
            let permille = if p.blocks_total > 0 {
                p.blocks_done * 1000 / p.blocks_total
            } else {
                0
            };
            if permille != last_permille || last_emit.elapsed().as_millis() >= 400 {
                last_permille = permille;
                last_emit = std::time::Instant::now();
                let _ = app_bg.emit(
                    "job-progress",
                    JobProgress {
                        job_id: job_id.clone(),
                        phase: "backup".into(),
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
        Ok::<_, OwnBackupError>(summary)
    })
    .await;

    run.finished_at = chrono::Utc::now().to_rfc3339();
    match result {
        Ok(Ok(summary)) => {
            run.status = "success".into();
            run.source_bytes = summary.source_bytes;
            run.bytes_copied = summary.bytes_copied;
            run.used_blocks = summary.used_blocks;
            run.total_blocks = summary.total_blocks;
            run.duration_secs = summary.duration.as_secs_f64();
            run.smart_mode = summary.smart_mode;
            run.vss_used = summary.vss_used;
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
async fn perform_backup(_app: &AppHandle, job: &Job, _cancel: &Arc<AtomicBool>) -> JobRun {
    let mut run = run_skeleton(job);
    run.finished_at = chrono::Utc::now().to_rfc3339();
    run.status = "error".into();
    run.error = "Движок бэкапа доступен только на Windows".into();
    run
}
