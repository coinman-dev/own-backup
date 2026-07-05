//! Backup orchestrator core (plan v2, phase 4).
//!
//! The single owner of backup orchestration: the rich [`Job`] config model,
//! JSON persistence ([`JobStore`]), a serial run queue ([`RunQueue`] +
//! [`run_queue_worker`]) and the per-run driver ([`perform_backup`]) that
//! calls the Windows engine, plus chain planning ([`plan_run`]) and retention.
//!
//! This crate is host-agnostic: progress and lifecycle events are delivered
//! through the [`EventSink`] trait, so the Tauri GUI (window events) and the
//! service (gRPC streams) each adapt it their own way. Nothing here depends on
//! Tauri. Runs are serialized through an mpsc queue processed by one worker,
//! so a second "run" while a backup is in progress is queued, not rejected.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{error, info, warn};

// ---------------------------------------------------------------------------
// Config model (persisted)
// ---------------------------------------------------------------------------

/// Schedule tab. Only `kind`/`days`/`time` shape the scheduler; the rest tune
/// how the service fires runs (wake, missed, etc.).
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

/// Scheme tab (method + retention).
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

/// Exclusions tab.
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

/// For a multi-volume job: the drive letter of the first source that collides
/// with the destination folder, or `None` if the destination is safe for all
/// sources. Used to forbid writing the image onto any volume being imaged.
pub fn dest_collision_letter(job: &Job, dest_folder: &str) -> Option<char> {
    let dst = drive_letter_of(dest_folder)?;
    job.source_list()
        .iter()
        .filter_map(|s| drive_letter_of(s))
        .find(|&src| src == dst)
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
/// Whole-disk (bootable) backup metadata attached to a [`Job`]: which physical
/// disk it captures and how each backed-up partition maps to a GPT slot. Used
/// after the run to write the `.obdm` disk-restore manifest.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DiskSourceSpec {
    /// Physical drive number (`\\.\PhysicalDrive{disk_number}`).
    pub disk_number: u32,
    pub disk_model: String,
    pub parts: Vec<DiskPartSpec>,
}

/// One partition of a whole-disk backup: its GPT slot and its source device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskPartSpec {
    /// GPT partition number (matches the entry index in the disk's GPT).
    pub part_index: u32,
    /// The partition's source device (letter "C:", volume GUID path, or
    /// `part:<disk>:<off>:<len>`) — the join key to the backed-up image.
    pub device: String,
    /// Filesystem name for display / the manifest.
    pub filesystem: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Job {
    pub id: String,
    pub name: String,
    /// Primary source volume device, e.g. "E:". Kept for file naming and
    /// backward compatibility; `sources` is the full multi-volume selection.
    pub source: String,
    /// Human label for display, e.g. "E: WorkDisk-2 — 347 GB".
    pub source_label: String,
    /// All selected source volumes for a multi-volume job. When empty, the job
    /// is single-volume and `source` is the one volume (old jobs deserialize
    /// this way). Each volume produces its own image/chain within one run.
    #[serde(default)]
    pub sources: Vec<String>,
    /// Display labels paralleling `sources` (same length, same order).
    #[serde(default)]
    pub source_labels: Vec<String>,
    /// At least one source volume is encrypted (LUKS), so the job needs a
    /// password to run. The scheduler skips such a job when no password is
    /// saved (the GUI then makes schedule setup read-only until one is).
    #[serde(default)]
    pub requires_password: bool,
    /// Set when this is a whole-disk (bootable) backup: which physical disk and
    /// how its partitions map to the backed-up images. `sources` still holds the
    /// per-partition devices (so the backup loop is unchanged); this adds the
    /// GPT/partition metadata needed to write the disk-restore manifest.
    #[serde(default)]
    pub disk_source: Option<DiskSourceSpec>,
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
            sources: Vec::new(),
            source_labels: Vec::new(),
            requires_password: false,
            disk_source: None,
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

impl Job {
    /// The volumes this job backs up, normalized across the old single-volume
    /// model (`source`) and the new multi-volume one (`sources`). Never empty
    /// for a valid job; each entry gets its own image/chain within a run.
    pub fn source_list(&self) -> Vec<String> {
        if !self.sources.is_empty() {
            self.sources.iter().filter(|s| !s.trim().is_empty()).cloned().collect()
        } else if !self.source.trim().is_empty() {
            vec![self.source.clone()]
        } else {
            Vec::new()
        }
    }

    /// Display label for a source device, from the parallel `source_labels`
    /// (falling back to the primary label or the device itself).
    pub fn label_for(&self, device: &str) -> String {
        if let Some(idx) = self.sources.iter().position(|s| s == device) {
            if let Some(l) = self.source_labels.get(idx) {
                if !l.trim().is_empty() {
                    return l.clone();
                }
            }
        }
        if self.source == device && !self.source_label.trim().is_empty() {
            return self.source_label.clone();
        }
        device.to_string()
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
// Run queue + events
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

impl Default for RunQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct QueueSnapshot {
    pub current: Option<String>,
    pub pending: Vec<String>,
}

/// Progress payload for a running backup.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct JobProgress {
    pub job_id: String,
    /// prepare | backup | finalize | verify
    pub phase: String,
    /// Percent of the CURRENT volume (0..100). Multi-volume jobs report each
    /// volume's own progress; the UI shows which volume via the fields below.
    pub percent: u64,
    pub blocks_done: u64,
    pub blocks_total: u64,
    pub speed_mbps: f64,
    /// 0-based index of the volume being copied in a multi-volume job.
    #[serde(default)]
    pub volume_index: u64,
    /// Total number of volumes in this job run (1 for single-volume jobs).
    #[serde(default)]
    pub volume_count: u64,
    /// Human label of the current volume, e.g. "C: Windows".
    #[serde(default)]
    pub volume: String,
}

/// Sink for orchestration events. The GUI forwards these to Tauri window
/// events; the service will push them onto gRPC progress streams.
pub trait EventSink: Send + Sync + 'static {
    fn job_progress(&self, progress: JobProgress);
    fn queue_changed(&self, snapshot: QueueSnapshot);
    fn job_finished(&self, run: JobRun);
}

/// Supplies passphrases for encrypted source volumes at backup time. The
/// service backs this with an in-memory cache and the OS-protected store; a
/// `None` means no password is available (an encrypted volume then cannot be
/// decrypted, so it is backed up as raw sectors or the run is skipped).
pub trait PasswordResolver: Send + Sync + 'static {
    fn resolve(&self, volume_token: &str) -> Option<zeroize::Zeroizing<String>>;
}

/// A [`PasswordResolver`] that never has a password — used where encryption is
/// not wired (tests, the non-Windows stub).
pub struct NoPasswords;
impl PasswordResolver for NoPasswords {
    fn resolve(&self, _volume_token: &str) -> Option<zeroize::Zeroizing<String>> {
        None
    }
}

/// Password store for encrypted volumes, shared by the service (scheduled runs)
/// and the GUI worker. Passwords are cached in memory for the process lifetime;
/// those the user chose to remember are additionally persisted OS-protected
/// (DPAPI) in the metadata DB so scheduled backups can unlock the volume with no
/// one present.
pub struct PasswordStore {
    cache: std::sync::Mutex<std::collections::HashMap<String, zeroize::Zeroizing<String>>>,
    db_path: PathBuf,
    /// Key-file path for the OS secret store (used only off Windows).
    key_path: PathBuf,
}

impl PasswordStore {
    pub fn new(db_path: PathBuf) -> Self {
        let key_path = db_path
            .parent()
            .map(|d| d.join("secret.key"))
            .unwrap_or_else(|| PathBuf::from("secret.key"));
        Self {
            cache: std::sync::Mutex::new(std::collections::HashMap::new()),
            db_path,
            key_path,
        }
    }

    /// Record a password for `volume_token` (as computed by [`source_token`]).
    /// Always cached in memory; when `remember` is set it is also persisted
    /// OS-protected. `remember` on a platform without a secret store errors.
    pub fn remember(
        &self,
        volume_token: &str,
        password: String,
        remember: bool,
    ) -> own_backup_common::Result<()> {
        if remember {
            let blob = own_backup_backup::secrets::default_store(&self.key_path)
                .protect(password.as_bytes())?;
            let conn = own_backup_metadata::open_database(&self.db_path)?;
            own_backup_metadata::upsert_volume_secret(&conn, volume_token, &blob)?;
        }
        self.cache
            .lock()
            .unwrap()
            .insert(volume_token.to_string(), zeroize::Zeroizing::new(password));
        Ok(())
    }

    /// Forget a password (in-memory and persisted).
    pub fn forget(&self, volume_token: &str) -> own_backup_common::Result<()> {
        self.cache.lock().unwrap().remove(volume_token);
        let conn = own_backup_metadata::open_database(&self.db_path)?;
        own_backup_metadata::delete_volume_secret(&conn, volume_token)?;
        Ok(())
    }

    /// Whether a password is available (cached or persisted) for `volume_token`.
    /// Used by the scheduler to decide if an encrypted job can run unattended.
    pub fn has(&self, volume_token: &str) -> bool {
        self.resolve(volume_token).is_some()
    }
}

impl PasswordResolver for PasswordStore {
    fn resolve(&self, volume_token: &str) -> Option<zeroize::Zeroizing<String>> {
        if let Some(p) = self.cache.lock().unwrap().get(volume_token) {
            return Some(p.clone());
        }
        // Fall back to the OS-protected store (a remembered password).
        let conn = own_backup_metadata::open_database(&self.db_path).ok()?;
        let blob = own_backup_metadata::get_volume_secret(&conn, volume_token).ok()??;
        let plain = own_backup_backup::secrets::default_store(&self.key_path)
            .unprotect(&blob)
            .ok()?;
        Some(zeroize::Zeroizing::new(String::from_utf8(plain.to_vec()).ok()?))
    }
}

/// Background worker: pulls job ids off the channel and runs them one at a
/// time. Spawned once at startup.
pub async fn run_queue_worker(
    sink: Arc<dyn EventSink>,
    store: Arc<JobStore>,
    queue: RunQueue,
    cancel: Arc<AtomicBool>,
    passwords: Arc<dyn PasswordResolver>,
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
        sink.queue_changed(queue.snapshot());
        info!(job = %job.name, "Starting queued backup");

        cancel.store(false, Ordering::SeqCst);
        let last_success = store.last_success(&job.id);
        let run = perform_backup(&sink, &job, &cancel, last_success, &passwords).await;
        store.record_run(run.clone());

        *queue.current.lock().unwrap() = None;
        sink.queue_changed(queue.snapshot());
        sink.job_finished(run);
    }
}

// ---------------------------------------------------------------------------
// Chain planning
// ---------------------------------------------------------------------------

/// What the next run of a job should produce.
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// Uppercase drive letter of a source device string ("C:" / `\\.\C:` → 'C').
/// A short, filename-safe, underscore-free token identifying a source volume
/// within a job's images/chains. A lettered volume is its letter ("C:" → "C").
/// A letterless volume addressed by a GUID path (`\\?\Volume{1a2b…}`) becomes
/// `V` + the first 8 hex digits of the GUID — unique per volume, so several
/// letterless partitions (e.g. two EFI parts) never collide. Falls back to "X".
/// Stable key for a source volume, used both for chain planning and to look up
/// a saved password. The GUI/service compute the same token to store passwords.
pub fn source_token(device: &str) -> String {
    let d = device.trim();
    // Windowed partition source `part:<disk>:<offset>:<len>` → P<disk> + offset
    // in hex — unique per partition (offsets differ), so several letterless
    // system partitions never collide.
    if let Some(rest) = d.strip_prefix("part:") {
        let mut it = rest.split(':');
        if let (Some(disk), Some(off)) = (it.next(), it.next()) {
            if let (Ok(disk), Ok(off)) = (disk.parse::<u32>(), off.parse::<u64>()) {
                return format!("P{disk}{off:X}");
            }
        }
    }
    // Volume GUID path → V + first 8 hex of the GUID.
    if let Some(rest) = d.strip_prefix(r"\\?\Volume{").or_else(|| d.strip_prefix(r"\\.\Volume{")) {
        let hex: String = rest
            .chars()
            .filter(|c| c.is_ascii_hexdigit())
            .take(8)
            .collect::<String>()
            .to_ascii_uppercase();
        if !hex.is_empty() {
            return format!("V{hex}");
        }
    }
    // Lettered volume ("C:", "\\.\C:") → the letter.
    d.trim_start_matches(r"\\.\")
        .chars()
        .next()
        .filter(|c| c.is_ascii_alphabetic())
        .map(|c| c.to_ascii_uppercase().to_string())
        .unwrap_or_else(|| "X".to_string())
}

/// Filename identifier for a source volume. Same as [`source_token`] for
/// lettered / GUID volumes; a letterless (encrypted) `part:` source uses its
/// sanitized volume label when the job knows one — so its image is named
/// `Job_kubuntu_full_…` like a lettered `Job_C_full_…`, not `Job_P4AB…`. The
/// result is underscore-free (the token is delimited by `_` in the file name).
/// Distinct from [`source_token`], which stays the stable device key used for
/// passwords and cross-run identity.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))] // used by the Windows backup path
fn file_token(job: &Job, device: &str) -> String {
    if device.trim_start().starts_with("part:") {
        let label = job.label_for(device);
        if label != device {
            let safe: String = label.chars().filter(|c| c.is_alphanumeric()).collect();
            if !safe.is_empty() {
                return safe;
            }
        }
    }
    source_token(device)
}

/// Recover the source token that an image file name encodes. Names are
/// `<name>_<token>_<tag>_<stamp>.<ext>`; token/tag/stamp are always the last
/// three underscore-separated segments, so we read from the end — robust even
/// when `<name>` itself contains underscores.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))] // used by the Windows backup path
fn image_token(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let mut it = stem.rsplit('_');
    let _stamp = it.next()?;
    let _tag = it.next()?;
    let token = it.next()?;
    if token.is_empty() {
        None
    } else {
        Some(token.to_ascii_uppercase())
    }
}

/// Timestamped output file name for one volume of a run:
/// `<name>_<token>_<full|inc|diff>_<stamp>.<vhdx|avhdx>`.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))] // used by the Windows backup path
fn output_path_for(job: &Job, device: &str, kind: &str) -> PathBuf {
    let token = file_token(job, device);
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
    PathBuf::from(&job.dest_folder).join(format!("{safe_name}_{token}_{tag}_{stamp}.{ext}"))
}

/// Decide the next run's plan for one specific volume, reading the on-disk
/// chains for that volume's drive letter (the sidecars are the source of
/// truth). This makes planning correct for multi-volume jobs — each volume has
/// its own independent chain — without relying on a single run-history record.
/// Any doubt (no chain, rollover reached, parent missing) resolves to a full.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))] // used by the Windows backup path
pub fn plan_run_for_volume(job: &Job, folder: &Path, device: &str) -> RunPlan {
    use own_backup_backup::chains::list_job_chains;

    let method: &'static str = match job.scheme.method.as_str() {
        "incremental" => "incremental",
        "differential" => "differential",
        _ => return RunPlan::full(),
    };

    let token = file_token(job, device);
    let chains = match list_job_chains(folder, &job.id) {
        Ok(c) => c,
        Err(_) => return RunPlan::full(),
    };
    // Chains are oldest-first; keep only this volume's, then take the newest.
    // Case-insensitive: the token read back from a file name is upper-cased.
    let Some(chain) = chains
        .into_iter()
        .filter(|c| {
            c.images
                .first()
                .and_then(|i| image_token(&i.image_path))
                .is_some_and(|t| t.eq_ignore_ascii_case(&token))
        })
        .next_back()
    else {
        return RunPlan::full();
    };

    let base = match chain.images.first() {
        Some(img) => img.image_path.clone(),
        None => return RunPlan::full(),
    };
    let child_count = chain.images.len() as u32 - 1;

    // "Full version after every N": start a fresh chain once it holds N children.
    if !job.scheme.only_incremental && child_count >= job.scheme.full_every_n.max(1) {
        return RunPlan::full();
    }

    // Incremental parents the previous (newest) image; differential parents the
    // chain's full/root.
    let parent = match method {
        "incremental" => chain
            .images
            .last()
            .map(|i| i.image_path.clone())
            .unwrap_or_else(|| base.clone()),
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
        increment_index: child_count + 1,
    }
}

/// Apply the job's retention rules after a fully successful run, independently
/// per source volume (identified by its image token). "Keep N chains" means N
/// chains *for each volume* — a two-volume job never lets one volume's backups
/// evict the other's. Failures are logged, never propagated. `protected` lists
/// the images just created (one per volume); their chains are always exempt.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))] // used by the Windows backup path
fn apply_retention(job: &Job, protected: &[(String, PathBuf)]) {
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
    let all_chains = match list_job_chains(&folder, &job.id) {
        Ok(c) => c,
        Err(e) => {
            warn!(job = %job.name, error = %e, "Retention scan failed");
            return;
        }
    };

    // One retention pass per distinct volume token, over that volume's chains.
    let mut seen = std::collections::HashSet::new();
    for (token, protect_image) in protected {
        if !seen.insert(token.clone()) {
            continue;
        }
        let vol_chains: Vec<_> = all_chains
            .iter()
            .filter(|c| {
                c.images
                    .first()
                    .and_then(|i| image_token(&i.image_path))
                    .is_some_and(|t| t.eq_ignore_ascii_case(token))
            })
            .cloned()
            .collect();
        if vol_chains.is_empty() {
            continue;
        }
        let doomed = plan_retention(&vol_chains, &rules, chrono::Utc::now(), protect_image);
        for idx in doomed {
            info!(
                job = %job.name,
                volume = %token,
                base = %vol_chains[idx].images.first().map(|i| i.image_path.display().to_string()).unwrap_or_default(),
                images = vol_chains[idx].images.len(),
                "Retention: deleting old backup chain"
            );
            delete_chain(&vol_chains[idx]);
        }
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

/// Sum two optional category breakdowns field-by-field (multi-volume runs
/// combine each volume's walk into one dashboard total).
#[cfg(target_os = "windows")]
fn merge_categories(
    acc: Option<own_backup_backup::exclusions::CategorySizes>,
    add: Option<own_backup_backup::exclusions::CategorySizes>,
) -> Option<own_backup_backup::exclusions::CategorySizes> {
    use own_backup_backup::exclusions::CategorySizes;
    match (acc, add) {
        (None, x) | (x, None) => x,
        (Some(a), Some(b)) => Some(CategorySizes {
            images: a.images + b.images,
            video: a.video + b.video,
            audio: a.audio + b.audio,
            documents: a.documents + b.documents,
            system: a.system + b.system,
            other: a.other + b.other,
        }),
    }
}

#[cfg(target_os = "windows")]
async fn perform_backup(
    sink: &Arc<dyn EventSink>,
    job: &Job,
    cancel: &Arc<AtomicBool>,
    _last_success: Option<JobRun>,
    passwords: &Arc<dyn PasswordResolver>,
) -> JobRun {
    use own_backup_backup::engine_win::{backup_volume, verify_image, VolumeBackupRequest};
    use own_backup_common::types::BackupType;
    use own_backup_common::OwnBackupError;

    let mut run = run_skeleton(job);

    // A job may back up several volumes; each produces its own image/chain and
    // is planned independently from the on-disk chains for its drive letter.
    let volumes = job.source_list();
    if volumes.is_empty() {
        run.finished_at = chrono::Utc::now().to_rfc3339();
        run.status = "error".into();
        run.error = "У задания не выбран ни один том-источник".into();
        return run;
    }
    let folder = PathBuf::from(&job.dest_folder);
    let count = volumes.len() as u64;

    // Exclusions only make sense in smart NTFS mode; a sector-by-sector image
    // is a verbatim copy by definition.
    let exclusion_patterns = if job.exclusions.enabled && !job.advanced.sector_by_sector {
        job.exclusions.patterns.clone()
    } else {
        Vec::new()
    };

    // Aggregates across all volumes of this run.
    let mut protected: Vec<(String, PathBuf)> = Vec::new();
    // (source device, image path) per success — used to build the whole-disk
    // restore manifest, joining images to GPT partitions by device.
    let mut run_images: Vec<(String, PathBuf)> = Vec::new();
    let mut kinds: Vec<&'static str> = Vec::new();
    let mut total_dur = 0.0f64;
    let mut all_smart = true;
    let mut fatal: Option<(String, String)> = None; // (status, message)

    for (idx, device) in volumes.iter().enumerate() {
        let plan = plan_run_for_volume(job, &folder, device);
        let image_path = output_path_for(job, device, plan.kind);
        let token = source_token(device);
        let vol_label = job.label_for(device);
        let vidx = idx as u64;

        let request = VolumeBackupRequest {
            source: device.clone(),
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
            exclusion_patterns: exclusion_patterns.clone(),
            job_id: Some(job.id.clone()),
            // A saved/cached passphrase for this volume, if any. The engine uses
            // it only when the source is actually a LUKS container.
            passphrase: passwords.resolve(&token),
        };
        let validate_after = job.advanced.validate_after;

        let sink_bg = sink.clone();
        let job_id = job.id.clone();
        let cancel_bg = cancel.clone();
        let image_for_verify = image_path.clone();
        let vol_label_bg = vol_label.clone();

        let result = tokio::task::spawn_blocking(move || {
            let mut last_permille = u64::MAX;
            let mut last_emit = std::time::Instant::now();
            let summary = backup_volume(&request, |p| {
                if cancel_bg.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    return Err(OwnBackupError::Cancelled);
                }
                // The engine tags non-copy phases via backup_id: "prepare" for
                // the exclusion scan, "finalize" for the flush/detach at the
                // end. Both carry blocks_total == 0.
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
                    sink_bg.job_progress(JobProgress {
                        job_id: job_id.clone(),
                        phase: phase.into(),
                        percent: permille / 10,
                        blocks_done: p.blocks_done,
                        blocks_total: p.blocks_total,
                        speed_mbps: p.speed_bps as f64 / 1_000_000.0,
                        volume_index: vidx,
                        volume_count: count,
                        volume: vol_label_bg.clone(),
                    });
                }
                Ok(())
            })?;

            // Optional post-backup verification — emit its own progress so the
            // UI shows "Проверка…" instead of freezing at 100%.
            if validate_after {
                let mut v_permille = u64::MAX;
                let mut v_emit = std::time::Instant::now();
                let v = verify_image(&image_for_verify, |done, total| {
                    let permille = if total > 0 { done * 1000 / total } else { 0 };
                    if permille != v_permille || v_emit.elapsed().as_millis() >= 400 {
                        v_permille = permille;
                        v_emit = std::time::Instant::now();
                        sink_bg.job_progress(JobProgress {
                            job_id: job_id.clone(),
                            phase: "verify".into(),
                            percent: permille / 10,
                            blocks_done: done,
                            blocks_total: total,
                            speed_mbps: 0.0,
                            volume_index: vidx,
                            volume_count: count,
                            volume: vol_label_bg.clone(),
                        });
                    }
                    Ok(())
                })?;
                if !v.valid() {
                    return Err(OwnBackupError::ImageFormat(format!(
                        "проверка после копирования не пройдена ({}): {} повреждённых блоков",
                        vol_label_bg,
                        v.mismatched_blocks.len()
                    )));
                }
            }

            Ok::<_, OwnBackupError>(summary)
        })
        .await;

        match result {
            Ok(Ok(summary)) => {
                use own_backup_common::types::BackupType as BT;
                run.source_bytes += summary.source_bytes;
                run.bytes_copied += summary.bytes_copied;
                run.used_blocks += summary.used_blocks;
                run.total_blocks += summary.total_blocks;
                run.blocks_unchanged += summary.blocks_unchanged;
                run.excluded_files += summary.excluded_files;
                run.excluded_bytes += summary.excluded_bytes;
                total_dur += summary.duration.as_secs_f64();
                all_smart &= summary.smart_mode;
                run.vss_used |= summary.vss_used;
                run.categories = merge_categories(run.categories.take(), summary.categories);

                let actual_kind = match summary.backup_kind {
                    BT::Full => "full",
                    BT::Incremental => "incremental",
                    _ => "differential",
                };
                kinds.push(actual_kind);

                // The primary (first) volume drives the single-value dashboard
                // fields (image to open, chain base, increment index).
                if idx == 0 {
                    run.image_path = image_path.display().to_string();
                    match summary.backup_kind {
                        BT::Full => {
                            run.base_image_path = image_path.display().to_string();
                            run.increment_index = 0;
                        }
                        _ => {
                            run.base_image_path = plan
                                .base_image
                                .as_ref()
                                .map(|p| p.display().to_string())
                                .unwrap_or_default();
                            run.increment_index = plan.increment_index;
                        }
                    }
                }
                // Retention matches images by their file-name token (label for
                // encrypted volumes), not the device password key.
                run_images.push((device.clone(), image_path.clone()));
                protected.push((file_token(job, device), image_path));
            }
            Ok(Err(OwnBackupError::Cancelled)) => {
                fatal = Some(("cancelled".into(), "Отменено пользователем".into()));
                break;
            }
            Ok(Err(e)) => {
                let msg = e.to_string();
                warn!(job = %job.name, volume = %device, error = %msg, "Backup failed");
                fatal = Some(("error".into(), msg));
                break;
            }
            Err(e) => {
                fatal = Some(("error".into(), format!("задача прервана: {e}")));
                break;
            }
        }
    }

    run.finished_at = chrono::Utc::now().to_rfc3339();
    run.duration_secs = total_dur;
    run.smart_mode = !protected.is_empty() && all_smart;

    // A single volume failure fails the whole run; images already written stay
    // on disk (never fake success), and retention does not run on a partial set.
    if let Some((status, msg)) = fatal {
        run.status = status;
        run.error = msg;
        return run;
    }

    run.status = "success".into();
    // Combined kind: the shared kind if every volume agreed, else "mixed".
    run.backup_kind = match kinds.first() {
        Some(first) if kinds.iter().all(|k| k == first) => (*first).into(),
        Some(_) => "mixed".into(),
        None => "full".into(),
    };

    // Whole-disk (bootable) backup: capture the GPT and write the `.obdm`
    // manifest that links it to the per-partition images. Best-effort — a good
    // per-volume backup is not discarded if only the layout capture fails.
    if let Some(ds) = &job.disk_source {
        if let Err(e) = write_disk_manifest(job, ds, &folder, &run_images) {
            warn!(job = %job.name, disk = ds.disk_number, error = %e,
                "Whole-disk backup: failed to write the disk restore manifest (.obdm)");
        }
    }

    // Retention runs after a fully successful backup, independently per volume.
    // Its failures are logged, never surfaced as a run failure.
    apply_retention(job, &protected);

    run
}

/// Read the source disk's GPT and write the `.obdm` disk-restore manifest that
/// ties the captured layout to the per-partition images just created.
#[cfg(target_os = "windows")]
fn write_disk_manifest(
    job: &Job,
    ds: &DiskSourceSpec,
    folder: &Path,
    run_images: &[(String, PathBuf)],
) -> Result<(), Box<dyn std::error::Error>> {
    use own_backup_common::gpt::{DiskBackupManifest, DiskImageRef};

    let layout = own_backup_backup::gpt_reader::read_disk_layout(ds.disk_number)?;

    // Join each backed-up image (by source device) to its GPT partition slot.
    let mut images = Vec::new();
    for (device, image_path) in run_images {
        let Some(part) = ds.parts.iter().find(|p| p.device == *device) else {
            continue; // not part of this disk selection
        };
        images.push(DiskImageRef {
            part_index: part.part_index,
            token: file_token(job, device),
            image_file_name: image_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            filesystem: part.filesystem.clone(),
        });
    }

    let safe_name: String = job
        .name
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let path = folder.join(format!(
        "{safe_name}_disk{}_{stamp}.{}",
        ds.disk_number,
        DiskBackupManifest::EXTENSION
    ));

    let manifest = DiskBackupManifest {
        format_version: DiskBackupManifest::FORMAT_VERSION,
        created_at: chrono::Utc::now().to_rfc3339(),
        job_id: Some(job.id.clone()),
        disk_number: ds.disk_number,
        disk_model: ds.disk_model.clone(),
        layout,
        images,
    };
    std::fs::write(&path, serde_json::to_vec_pretty(&manifest)?)?;
    info!(job = %job.name, path = %path.display(), "Whole-disk restore manifest written");
    Ok(())
}

/// Linux backup driver: full backups into raw images (one per source volume),
/// with progress + cancellation and optional post-backup verification. No chain
/// planning / retention yet — those follow with the dynamic-VHDX writer.
#[cfg(target_os = "linux")]
async fn perform_backup(
    sink: &Arc<dyn EventSink>,
    job: &Job,
    cancel: &Arc<AtomicBool>,
    _last_success: Option<JobRun>,
    passwords: &Arc<dyn PasswordResolver>,
) -> JobRun {
    use own_backup_backup::engine_linux::{
        backup_volume, verify_image, LinuxBackupRequest, DEFAULT_BLOCK_SIZE,
    };
    use own_backup_common::progress::BackupProgress;
    use own_backup_common::OwnBackupError;
    use own_backup_snapshot::SnapshotProvider;

    let mut run = run_skeleton(job);
    let volumes = job.source_list();
    if volumes.is_empty() {
        run.finished_at = chrono::Utc::now().to_rfc3339();
        run.status = "error".into();
        run.error = "У задания не выбран ни один том-источник".into();
        return run;
    }
    let folder = PathBuf::from(&job.dest_folder);
    let count = volumes.len() as u64;
    let started = std::time::Instant::now();

    for (idx, device) in volumes.iter().enumerate() {
        let vidx = idx as u64;
        let token = source_token(device);
        let vol_label = job.label_for(device);
        let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
        let safe: String = job
            .name
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '_' })
            .collect();
        let image_path = folder.join(format!("{safe}_{}_full_{stamp}.vhdx", file_token(job, device)));

        // Take a point-in-time snapshot (LVM/fsfreeze) so a live filesystem is
        // read consistently; degrades to a crash-consistent copy of the device
        // when snapshotting isn't possible (non-root / no LVM / unmounted).
        let snap_provider = own_backup_snapshot::LinuxSnapshotProvider::new();
        let snapshot = snap_provider.create_snapshot(device).await.ok();
        let read_source = snapshot
            .as_ref()
            .map(|s| s.snapshot_device_path.clone())
            .unwrap_or_else(|| device.clone());

        let req = LinuxBackupRequest {
            source: read_source,
            image_path: image_path.clone(),
            block_size: DEFAULT_BLOCK_SIZE,
            all_sectors: job.advanced.sector_by_sector,
            passphrase: passwords.resolve(&token),
            job_id: Some(job.id.clone()),
        };
        let validate_after = job.advanced.validate_after;
        let sink_bg = sink.clone();
        let job_id = job.id.clone();
        let cancel_bg = cancel.clone();
        let vol_label_bg = vol_label.clone();
        let image_for_verify = image_path.clone();

        let result = tokio::task::spawn_blocking(move || {
            let mut last_permille = u64::MAX;
            let mut last_emit = std::time::Instant::now();
            let summary = backup_volume(&req, |p: &BackupProgress| {
                if cancel_bg.load(Ordering::SeqCst) {
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
                    sink_bg.job_progress(JobProgress {
                        job_id: job_id.clone(),
                        phase: "backup".into(),
                        percent: permille / 10,
                        blocks_done: p.blocks_done,
                        blocks_total: p.blocks_total,
                        speed_mbps: p.speed_bps as f64 / 1_000_000.0,
                        volume_index: vidx,
                        volume_count: count,
                        volume: vol_label_bg.clone(),
                    });
                }
                Ok(())
            })?;
            if validate_after {
                let v = verify_image(&image_for_verify, |_, _| Ok(()))?;
                if !v.valid() {
                    return Err(OwnBackupError::ImageFormat(format!(
                        "проверка после копирования не пройдена: {} повреждённых блоков",
                        v.mismatched_blocks.len()
                    )));
                }
            }
            Ok(summary)
        })
        .await;

        // Release the snapshot (thaw / lvremove) regardless of the outcome.
        if let Some(s) = &snapshot {
            if let Err(e) = snap_provider.release_snapshot(s).await {
                warn!(error = %e, "snapshot release failed");
            }
        }

        match result {
            Ok(Ok(summary)) => {
                run.image_path = summary.image_path.display().to_string();
                run.base_image_path = run.image_path.clone();
                run.source_bytes += summary.source_bytes;
                run.bytes_copied += summary.bytes_copied;
                run.used_blocks += summary.used_blocks;
                run.total_blocks += summary.total_blocks;
                run.smart_mode = summary.smart_mode;
                run.backup_kind = "full".into();
            }
            Ok(Err(OwnBackupError::Cancelled)) => {
                run.status = "cancelled".into();
                run.error = "Отменено пользователем".into();
                break;
            }
            Ok(Err(e)) => {
                warn!(job = %job.name, volume = %device, error = %e, "Backup failed");
                run.status = "error".into();
                run.error = e.to_string();
                break;
            }
            Err(e) => {
                run.status = "error".into();
                run.error = format!("задача прервана: {e}");
                break;
            }
        }
    }

    if run.status.is_empty() {
        run.status = "success".into();
    }
    run.duration_secs = started.elapsed().as_secs_f64();
    run.finished_at = chrono::Utc::now().to_rfc3339();
    run
}

/// Backup engine is unavailable on this platform (only Windows and Linux).
#[cfg(not(any(target_os = "windows", target_os = "linux")))]
async fn perform_backup(
    _sink: &Arc<dyn EventSink>,
    job: &Job,
    _cancel: &Arc<AtomicBool>,
    _last_success: Option<JobRun>,
    _passwords: &Arc<dyn PasswordResolver>,
) -> JobRun {
    let mut run = run_skeleton(job);
    run.finished_at = chrono::Utc::now().to_rfc3339();
    run.status = "error".into();
    run.error = "Движок бэкапа доступен только на Windows и Linux".into();
    run
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // Password persistence round-trips through the OS secret store; on non-Windows
    // that is the key-file backend (testable here), on Windows it is DPAPI.
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn password_store_remembers_across_instances() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("metadata.db");
        PasswordStore::new(db.clone())
            .remember("V123", "secret".into(), true)
            .unwrap();

        // A fresh store (cold cache) recovers it from the DB via the key file.
        let store = PasswordStore::new(db.clone());
        assert!(store.has("V123"));
        assert_eq!(store.resolve("V123").map(|z| z.to_string()), Some("secret".into()));

        store.forget("V123").unwrap();
        assert!(!PasswordStore::new(db).has("V123"));
    }

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

    // ---- multi-volume (per-letter) planning ------------------------------

    #[test]
    fn source_and_image_token_parsing() {
        assert_eq!(source_token("C:"), "C");
        assert_eq!(source_token("e:\\"), "E");
        assert_eq!(source_token(r"\\.\D:"), "D");
        assert_eq!(source_token(r"\\?\GLOBALROOT\Device\X"), "X"); // no letter → "X"
        // Letterless volume GUID path → V + first 8 hex of the GUID.
        assert_eq!(source_token(r"\\?\Volume{1a2b3c4d-5e6f-7089-abcd-ef0123456789}"), "V1A2B3C4D");
        // Physical-offset partition → P<disk><offsetHex> (0x52000000 = 1375731712).
        assert_eq!(source_token("part:3:1375731712:1073741824"), "P352000000");

        // Names are `<name>_<token>_<tag>_<stamp>`; token is read from the end
        // so underscores inside <name> do not confuse it.
        assert_eq!(image_token(Path::new("job_C_full_20260101-000000.vhdx")).as_deref(), Some("C"));
        assert_eq!(
            image_token(Path::new("My_Big_Job_V1A2B3C4D_inc_20260101-000000.avhdx")).as_deref(),
            Some("V1A2B3C4D")
        );
        assert_eq!(image_token(Path::new("no_underscores.vhdx")), None);
    }

    #[test]
    fn file_token_uses_label_for_encrypted_sources() {
        let dev = "part:4:1048576:1000";
        let mut job = Job::default();
        job.sources = vec![dev.into()];
        // A label with an underscore is stripped to an underscore-free token.
        job.source_labels = vec!["kubuntu_boot".into()];
        let token = file_token(&job, dev);
        assert_eq!(token, "kubuntuboot");
        // The file name round-trips: image_token reads it back (case-insensitive).
        let name = output_path_for(&job, dev, "full");
        assert!(image_token(&name).unwrap().eq_ignore_ascii_case(&token));

        // No label → falls back to the stable device token.
        let bare = Job::default();
        assert_eq!(file_token(&bare, dev), source_token(dev));
        // A lettered volume is unaffected by any label.
        let mut lettered = Job::default();
        lettered.sources = vec!["C:".into()];
        lettered.source_labels = vec!["Windows".into()];
        assert_eq!(file_token(&lettered, "C:"), "C");
    }

    #[test]
    fn source_list_normalizes_old_and_new() {
        // Old single-volume job: only `source` set.
        let mut old = Job::default();
        old.source = "E:".into();
        old.source_label = "E: Data".into();
        assert_eq!(old.source_list(), vec!["E:".to_string()]);
        assert_eq!(old.label_for("E:"), "E: Data");

        // New multi-volume job: `sources` takes precedence, blanks dropped.
        let mut multi = Job::default();
        multi.source = "C:".into();
        multi.sources = vec!["C:".into(), "".into(), "E:".into()];
        multi.source_labels = vec!["C: Win".into(), "".into(), "E: Data".into()];
        assert_eq!(multi.source_list(), vec!["C:".to_string(), "E:".to_string()]);
        assert_eq!(multi.label_for("E:"), "E: Data");
        assert_eq!(multi.label_for("Z:"), "Z:"); // unknown → device itself
    }

    /// Write a real (parseable) sidecar + dummy image so `list_job_chains`
    /// discovers it. `name` must follow `<n>_<letter>_<tag>_<stamp>.<ext>`.
    fn put_real_image(
        dir: &Path,
        name: &str,
        role: own_backup_backup::sidecar::ChainRole,
        parent: Option<&str>,
        job_id: &str,
        created_at: &str,
    ) {
        use own_backup_backup::sidecar::SidecarInfo;
        use own_backup_backup::{sidecar_path_for, write_sidecar};
        use own_backup_common::disk::DiskInfo;
        use own_backup_common::image::ImageManifest;
        use own_backup_common::types::{
            BackupMode, BackupType, BootMode, CompressionAlgo, PartitionTable,
        };
        use uuid::Uuid;

        let image = dir.join(name);
        std::fs::write(&image, b"img").unwrap();
        let mut info = match parent {
            None => SidecarInfo::new_full(name, 0),
            Some(p) => SidecarInfo::new_child(name, role, p, 0),
        };
        info.job_id = Some(job_id.to_string());
        let manifest = ImageManifest {
            backup_id: Uuid::new_v4(),
            created_at: created_at.into(),
            backup_type: BackupType::Full,
            parent_backup_id: None,
            source_os: "windows".into(),
            source_disk: DiskInfo {
                device_id: r"\\.\C:".into(),
                model: String::new(),
                serial: String::new(),
                size_bytes: 1024,
                sector_size_logical: 512,
                sector_size_physical: 4096,
                media_type: "Volume".into(),
                partition_table: PartitionTable::Gpt,
                disk_guid: Uuid::nil().to_string(),
                partitions: vec![],
            },
            boot_mode: BootMode::Uefi,
            partition_table: PartitionTable::Gpt,
            compression_algo: CompressionAlgo::None,
            compression_level: 0,
            encryption_algo: None,
            encryption_salt: None,
            block_size: 512,
            total_blocks: 2,
            used_blocks: 2,
            backup_mode: BackupMode::UsedSectors,
            checksum_algorithm: "xxhash64".into(),
            preflight_report: None,
            volume_encryption: None,
        };
        write_sidecar(&sidecar_path_for(&image), &info, &manifest, &[]).unwrap();
    }

    #[test]
    fn per_volume_plan_isolates_by_drive_letter() {
        use own_backup_backup::sidecar::ChainRole;
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        // Two volumes, each with its own full backup.
        put_real_image(d, "job_C_full_20260101-000000.vhdx", ChainRole::Full, None, "job1", "2026-01-01T00:00:00Z");
        put_real_image(d, "job_E_full_20260101-000000.vhdx", ChainRole::Full, None, "job1", "2026-01-01T00:00:00Z");

        let mut job = Job::default();
        job.id = "job1".into();
        job.scheme.method = "incremental".into();
        job.scheme.only_incremental = true; // no rollover

        let plan_c = plan_run_for_volume(&job, d, "C:");
        assert_eq!(plan_c.kind, "incremental");
        // C: must parent C:'s full, never E:'s.
        assert_eq!(
            plan_c.parent_image.as_deref(),
            Some(d.join("job_C_full_20260101-000000.vhdx").as_path())
        );
        let plan_e = plan_run_for_volume(&job, d, "E:");
        assert_eq!(
            plan_e.parent_image.as_deref(),
            Some(d.join("job_E_full_20260101-000000.vhdx").as_path())
        );
        assert_eq!(plan_c.increment_index, 1);
    }

    #[test]
    fn per_volume_incremental_parents_newest_and_rolls_over() {
        use own_backup_backup::sidecar::ChainRole;
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        put_real_image(d, "job_C_full_20260101-000000.vhdx", ChainRole::Full, None, "job1", "2026-01-01T00:00:00Z");
        put_real_image(
            d,
            "job_C_inc_20260102-000000.avhdx",
            ChainRole::Incremental,
            Some("job_C_full_20260101-000000.vhdx"),
            "job1",
            "2026-01-02T00:00:00Z",
        );

        let mut job = Job::default();
        job.id = "job1".into();
        job.scheme.method = "incremental".into();
        job.scheme.only_incremental = false;
        job.scheme.full_every_n = 5;

        // Incremental parents the NEWEST image; base stays the full.
        let plan = plan_run_for_volume(&job, d, "C:");
        assert_eq!(plan.kind, "incremental");
        assert_eq!(
            plan.parent_image.as_deref(),
            Some(d.join("job_C_inc_20260102-000000.avhdx").as_path())
        );
        assert_eq!(
            plan.base_image.as_deref(),
            Some(d.join("job_C_full_20260101-000000.vhdx").as_path())
        );
        assert_eq!(plan.increment_index, 2);

        // Rollover: with N=1 the one-child chain is full already → new full.
        job.scheme.full_every_n = 1;
        assert_eq!(plan_run_for_volume(&job, d, "C:"), RunPlan::full());
    }
}
