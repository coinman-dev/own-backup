use own_backup_metadata::models::BackupSet;
use tauri::State;

use crate::state::GuiState;

#[derive(serde::Serialize)]
pub struct BackupSetDto {
    pub id: String,
    pub job_id: String,
    pub backup_type: String,
    pub parent_set_id: Option<String>,
    pub file_path: String,
    pub file_size: Option<i64>,
    pub source_size: i64,
    pub block_count: i64,
    pub changed_block_count: Option<i64>,
    pub status: String,
    pub checksum: Option<String>,
    pub manifest_json: Option<String>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
}

fn set_to_dto(s: BackupSet) -> BackupSetDto {
    BackupSetDto {
        id: s.id.to_string(),
        job_id: s.job_id.to_string(),
        backup_type: format!("{:?}", s.backup_type),
        parent_set_id: s.parent_set_id.map(|u| u.to_string()),
        file_path: s.file_path,
        file_size: s.file_size,
        source_size: s.source_size,
        block_count: s.block_count,
        changed_block_count: s.changed_block_count,
        status: format!("{:?}", s.status),
        checksum: s.checksum,
        manifest_json: s.manifest_json,
        started_at: s.started_at.map(|t| t.to_rfc3339()),
        completed_at: s.completed_at.map(|t| t.to_rfc3339()),
    }
}

#[tauri::command]
pub async fn list_sets(state: State<'_, GuiState>, job_id: String) -> Result<Vec<BackupSetDto>, String> {
    let uuid = uuid::Uuid::parse_str(&job_id).map_err(|e| format!("Invalid job ID: {}", e))?;
    let conn = state.open_db()?;
    let sets = own_backup_metadata::list_sets_for_job(&conn, &uuid).map_err(|e| e.to_string())?;
    Ok(sets.into_iter().map(set_to_dto).collect())
}

#[tauri::command]
pub async fn validate_backup(file_path: String) -> Result<own_backup_restore::ValidationResult, String> {
    let path = std::path::Path::new(&file_path);
    if !path.exists() {
        return Err(format!("File not found: {}", file_path));
    }
    own_backup_restore::quick_validate(path).map_err(|e| e.to_string())
}
