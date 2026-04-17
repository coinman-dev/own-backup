use own_backup_metadata::models::BackupJob;
use serde::Deserialize;
use tauri::State;

use crate::state::GuiState;

#[derive(Deserialize)]
pub struct CreateJobRequest {
    pub name: String,
    pub source_type: Option<String>,
    pub source_device: String,
    pub target_path: String,
    pub schedule: Option<String>,
    pub compression_level: Option<i32>,
    pub encryption_enabled: Option<bool>,
}

#[tauri::command]
pub async fn list_jobs(state: State<'_, GuiState>) -> Result<Vec<BackupJobDto>, String> {
    let conn = state.open_db()?;
    let jobs = own_backup_metadata::list_jobs(&conn).map_err(|e| e.to_string())?;
    Ok(jobs.into_iter().map(job_to_dto).collect())
}

#[tauri::command]
pub async fn create_job(
    state: State<'_, GuiState>,
    req: CreateJobRequest,
) -> Result<BackupJobDto, String> {
    if req.name.is_empty() || req.name.len() > 256 {
        return Err("Job name must be 1-256 characters".into());
    }
    if req.source_device.is_empty() {
        return Err("Source device is required".into());
    }
    if req.target_path.is_empty() {
        return Err("Target path is required".into());
    }

    let now = chrono::Utc::now();
    let job = BackupJob {
        id: uuid::Uuid::new_v4(),
        name: req.name,
        source_type: req.source_type.unwrap_or_else(|| "partition".into()),
        source_device: req.source_device,
        target_path: req.target_path,
        schedule: req.schedule,
        compression_level: req.compression_level.unwrap_or(3).clamp(0, 19),
        encryption_enabled: req.encryption_enabled.unwrap_or(false),
        created_at: now,
        updated_at: now,
    };

    let conn = state.open_db()?;
    own_backup_metadata::insert_job(&conn, &job).map_err(|e| e.to_string())?;

    Ok(job_to_dto(job))
}

#[tauri::command]
pub async fn delete_job(state: State<'_, GuiState>, id: String) -> Result<(), String> {
    let uuid = uuid::Uuid::parse_str(&id).map_err(|e| format!("Invalid job ID: {}", e))?;
    let conn = state.open_db()?;
    own_backup_metadata::delete_job(&conn, &uuid).map_err(|e| e.to_string())
}

// DTO for serialization to frontend (Uuid/DateTime → String)
#[derive(serde::Serialize)]
pub struct BackupJobDto {
    pub id: String,
    pub name: String,
    pub source_type: String,
    pub source_device: String,
    pub target_path: String,
    pub schedule: Option<String>,
    pub compression_level: i32,
    pub encryption_enabled: bool,
    pub created_at: String,
    pub updated_at: String,
}

fn job_to_dto(j: BackupJob) -> BackupJobDto {
    BackupJobDto {
        id: j.id.to_string(),
        name: j.name,
        source_type: j.source_type,
        source_device: j.source_device,
        target_path: j.target_path,
        schedule: j.schedule,
        compression_level: j.compression_level,
        encryption_enabled: j.encryption_enabled,
        created_at: j.created_at.to_rfc3339(),
        updated_at: j.updated_at.to_rfc3339(),
    }
}
