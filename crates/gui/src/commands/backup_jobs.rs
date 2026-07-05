//! Tauri commands for the job model (plan v2, phase 6).

use tauri::State;

use crate::jobs::{
    default_exclusions, dest_collision_letter, Job, JobRun, QueueSnapshot, ScheduleConfig,
    SchemeConfig,
};
use crate::state::GuiState;

/// Reject a destination that lives on any of the volumes being backed up.
fn check_dest_not_on_source(job: &Job) -> Result<(), String> {
    if let Some(letter) = dest_collision_letter(job, &job.dest_folder) {
        return Err(format!(
            "Нельзя сохранять копию на тот же диск, который копируется ({letter}:). \
             Выберите для назначения другой диск."
        ));
    }
    Ok(())
}

#[tauri::command]
pub async fn list_backup_jobs(state: State<'_, GuiState>) -> Result<Vec<Job>, String> {
    Ok(state.job_store.list())
}

#[tauri::command]
pub async fn get_backup_job(state: State<'_, GuiState>, id: String) -> Result<Option<Job>, String> {
    Ok(state.job_store.get(&id))
}

/// A blank job pre-filled with sensible defaults (default exclusions etc.),
/// used to seed the wizard. Default name = the computer name (like Acronis).
#[tauri::command]
pub async fn new_job_template() -> Result<Job, String> {
    let name = std::env::var("COMPUTERNAME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok().filter(|s| !s.trim().is_empty()))
        .unwrap_or_else(|| "Этот компьютер".into());
    Ok(Job {
        name,
        schedule: ScheduleConfig::default(),
        scheme: SchemeConfig::default(),
        ..Default::default()
    })
}

/// The default exclusion list (for the "reset" button on the Exclusions step).
#[tauri::command]
pub async fn default_exclusion_patterns() -> Result<Vec<String>, String> {
    Ok(default_exclusions())
}

/// Create or update a job. Returns the stored job (with generated id/dates).
#[tauri::command]
pub async fn save_backup_job(state: State<'_, GuiState>, job: Job) -> Result<Job, String> {
    if job.name.trim().is_empty() {
        return Err("Укажите имя задания.".into());
    }
    if job.source_list().is_empty() {
        return Err("Выберите источник (том).".into());
    }
    if job.dest_folder.trim().is_empty() {
        return Err("Выберите папку назначения.".into());
    }
    check_dest_not_on_source(&job)?;
    Ok(state.job_store.upsert(job))
}

#[tauri::command]
pub async fn delete_backup_job(state: State<'_, GuiState>, id: String) -> Result<(), String> {
    state.job_store.delete(&id);
    Ok(())
}

/// Provide a password to unlock an encrypted (LUKS) source volume. Cached for
/// this session; `remember` also persists it OS-protected so scheduled backups
/// can use it. `volume` is the source device (e.g. a `part:` spec).
#[tauri::command]
pub async fn provide_volume_password(
    state: State<'_, GuiState>,
    volume: String,
    password: String,
    remember: bool,
) -> Result<(), String> {
    let token = own_backup_core::source_token(&volume);
    state
        .passwords
        .remember(&token, password, remember)
        .map_err(|e| e.to_string())
}

/// Enqueue a job for backup. Runs immediately if idle, otherwise queues.
#[tauri::command]
pub async fn run_backup_job(state: State<'_, GuiState>, id: String) -> Result<(), String> {
    // Guard again at run time: a job could have been saved before this rule
    // existed, or edited on disk, or point at a since-relettered drive.
    let job = state
        .job_store
        .get(&id)
        .ok_or_else(|| "Задание не найдено.".to_string())?;
    check_dest_not_on_source(&job)?;
    // Avoid enqueuing the same job twice while it is pending/running.
    {
        let pending = state.run_queue.pending.lock().unwrap();
        let current = state.run_queue.current.lock().unwrap();
        if pending.iter().any(|j| *j == id) || current.as_deref() == Some(id.as_str()) {
            return Ok(());
        }
    }
    state.run_queue.pending.lock().unwrap().push_back(id.clone());
    state
        .job_tx
        .send(id)
        .map_err(|e| format!("Не удалось поставить в очередь: {e}"))?;
    Ok(())
}

/// Remove a still-pending job from the queue (cannot cancel the running one
/// here — use cancel_engine_op for that).
#[tauri::command]
pub async fn cancel_queued_job(state: State<'_, GuiState>, id: String) -> Result<(), String> {
    state.run_queue.pending.lock().unwrap().retain(|j| *j != id);
    Ok(())
}

#[tauri::command]
pub async fn get_run_queue(state: State<'_, GuiState>) -> Result<QueueSnapshot, String> {
    Ok(state.run_queue.snapshot())
}

#[tauri::command]
pub async fn list_activity(state: State<'_, GuiState>) -> Result<Vec<JobRun>, String> {
    Ok(state.job_store.activity())
}
