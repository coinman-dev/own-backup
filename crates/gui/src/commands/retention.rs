use tauri::State;

use crate::state::GuiState;

#[derive(serde::Serialize, serde::Deserialize)]
pub struct RetentionPolicyDto {
    pub job_id: String,
    pub max_full_backups: Option<i64>,
    pub max_incremental_per_full: Option<i64>,
    pub max_age_days: Option<i64>,
    pub min_keep_count: i64,
}

#[tauri::command]
pub async fn get_retention(
    state: State<'_, GuiState>,
    job_id: String,
) -> Result<Option<RetentionPolicyDto>, String> {
    let uuid = uuid::Uuid::parse_str(&job_id).map_err(|e| format!("Invalid job ID: {}", e))?;
    let conn = state.open_db()?;
    let policy = own_backup_metadata::get_retention_policy(&conn, &uuid)
        .map_err(|e| e.to_string())?;

    Ok(policy.map(|p| RetentionPolicyDto {
        job_id: p.job_id.to_string(),
        max_full_backups: p.max_full_backups,
        max_incremental_per_full: p.max_incremental_per_full,
        max_age_days: p.max_age_days,
        min_keep_count: p.min_keep_count,
    }))
}

#[tauri::command]
pub async fn set_retention(
    state: State<'_, GuiState>,
    policy: RetentionPolicyDto,
) -> Result<(), String> {
    let uuid =
        uuid::Uuid::parse_str(&policy.job_id).map_err(|e| format!("Invalid job ID: {}", e))?;
    let conn = state.open_db()?;

    let db_policy = own_backup_metadata::RetentionPolicy {
        job_id: uuid,
        max_full_backups: policy.max_full_backups,
        max_incremental_per_full: policy.max_incremental_per_full,
        max_age_days: policy.max_age_days,
        min_keep_count: policy.min_keep_count,
    };

    own_backup_metadata::set_retention_policy(&conn, &db_policy).map_err(|e| e.to_string())
}
