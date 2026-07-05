//! Tauri commands for the job model (plan v2, phase 6).
//!
//! These are thin proxies onto the backup service over gRPC: the job store, run
//! queue, activity log and encrypted-volume passwords all live there, so the
//! scheduler and manual runs share one source of truth. Validation (name /
//! source / dest-not-on-source) is enforced service-side.

use tauri::State;

use crate::jobs::{Job, JobRun, QueueSnapshot};
use crate::state::GuiState;

#[tauri::command]
pub async fn list_backup_jobs(state: State<'_, GuiState>) -> Result<Vec<Job>, String> {
    state.link.client().await?.list_jobs().await
}

#[tauri::command]
pub async fn get_backup_job(state: State<'_, GuiState>, id: String) -> Result<Option<Job>, String> {
    state.link.client().await?.get_job(&id).await
}

/// A blank job pre-filled with sensible defaults (default exclusions etc.), used
/// to seed the wizard. Default name = the computer name (like Acronis).
#[tauri::command]
pub async fn new_job_template(state: State<'_, GuiState>) -> Result<Job, String> {
    state.link.client().await?.new_job_template().await
}

/// The default exclusion list (for the "reset" button on the Exclusions step).
#[tauri::command]
pub async fn default_exclusion_patterns(state: State<'_, GuiState>) -> Result<Vec<String>, String> {
    state.link.client().await?.default_exclusions().await
}

/// Create or update a job. Returns the stored job (with generated id/dates).
/// The service validates name / source / dest-not-on-source and rejects invalid
/// jobs with a message we surface verbatim.
#[tauri::command]
pub async fn save_backup_job(state: State<'_, GuiState>, job: Job) -> Result<Job, String> {
    state.link.client().await?.save_job(&job).await
}

#[tauri::command]
pub async fn delete_backup_job(state: State<'_, GuiState>, id: String) -> Result<(), String> {
    state.link.client().await?.delete_job(&id).await
}

/// Result of a decrypt attempt for the wizard: whether it unlocked, plus the
/// inner volume's filesystem / label / used bytes (to show like a normal volume).
#[derive(serde::Serialize)]
pub struct DecryptResult {
    pub ok: bool,
    pub filesystem: String,
    pub label: String,
    pub used_bytes: u64,
}

/// Unlock an encrypted (LUKS/BitLocker) source `volume` with `password` and
/// probe the decrypted filesystem, then hand the password to the service so the
/// backup — which runs there — can unlock the volume. When `remember` is set the
/// service persists it OS-protected for scheduled runs.
#[tauri::command]
pub async fn decrypt_volume(
    state: State<'_, GuiState>,
    volume: String,
    password: String,
    remember: bool,
) -> Result<DecryptResult, String> {
    // Probe the inner filesystem locally so the wizard can show it like a normal
    // volume. This needs the engine (Windows only); off Windows we skip it.
    #[cfg(target_os = "windows")]
    let result = {
        let (vol, pw) = (volume.clone(), password.clone());
        let probe = tokio::task::spawn_blocking(move || {
            own_backup_backup::engine_win::unlock_and_probe(&vol, &pw)
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
        DecryptResult {
            ok: probe.ok,
            filesystem: probe.filesystem,
            label: probe.label,
            used_bytes: probe.used_bytes,
        }
    };
    #[cfg(not(target_os = "windows"))]
    let result = DecryptResult {
        ok: true,
        filesystem: String::new(),
        label: String::new(),
        used_bytes: 0,
    };

    // Only forward a password that actually unlocked the volume.
    if result.ok {
        state
            .link
            .client()
            .await?
            .provide_volume_password(&volume, &password, remember)
            .await?;
    }
    Ok(result)
}

/// Enqueue a job for backup. Runs immediately if idle, otherwise queues.
#[tauri::command]
pub async fn run_backup_job(state: State<'_, GuiState>, id: String) -> Result<(), String> {
    state.link.client().await?.run_job(&id).await
}

/// Remove a still-pending job from the queue.
#[tauri::command]
pub async fn cancel_queued_job(state: State<'_, GuiState>, id: String) -> Result<(), String> {
    state.link.client().await?.cancel_queued(&id).await
}

/// Signal-cancel the backup currently running in the service. (In-process engine
/// ops — restore/verify — are cancelled separately via `cancel_engine_op`.)
#[tauri::command]
pub async fn cancel_running_backup(state: State<'_, GuiState>) -> Result<(), String> {
    state.link.client().await?.cancel_running().await
}

#[tauri::command]
pub async fn get_run_queue(state: State<'_, GuiState>) -> Result<QueueSnapshot, String> {
    state.link.client().await?.get_queue().await
}

#[tauri::command]
pub async fn list_activity(state: State<'_, GuiState>) -> Result<Vec<JobRun>, String> {
    state.link.client().await?.list_activity().await
}
