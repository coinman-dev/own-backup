use tauri::State;

use crate::state::GuiState;

#[tauri::command]
pub async fn start_backup(
    _state: State<'_, GuiState>,
    job_id: String,
    backup_type: String,
) -> Result<String, String> {
    // Validate inputs
    uuid::Uuid::parse_str(&job_id).map_err(|e| format!("Invalid job ID: {}", e))?;
    let valid_types = ["full", "incremental", "differential"];
    if !valid_types.contains(&backup_type.to_lowercase().as_str()) {
        return Err(format!("Invalid backup type: {}", backup_type));
    }

    // TODO: Connect to gRPC service, call StartBackup, spawn stream listener
    // that emits Tauri events for progress updates.
    // For now, return a placeholder backup ID.
    Ok(uuid::Uuid::new_v4().to_string())
}

#[tauri::command]
pub async fn cancel_backup(
    _state: State<'_, GuiState>,
    backup_id: String,
) -> Result<(), String> {
    uuid::Uuid::parse_str(&backup_id).map_err(|e| format!("Invalid backup ID: {}", e))?;

    // TODO: Connect to gRPC service, call CancelBackup
    Ok(())
}
