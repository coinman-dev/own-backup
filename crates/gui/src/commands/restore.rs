use tauri::State;

use crate::state::GuiState;

#[tauri::command]
pub async fn start_restore(
    _state: State<'_, GuiState>,
    backup_set_id: String,
    target_device: String,
) -> Result<String, String> {
    uuid::Uuid::parse_str(&backup_set_id)
        .map_err(|e| format!("Invalid backup set ID: {}", e))?;

    if target_device.is_empty() {
        return Err("Target device is required".into());
    }

    // TODO: Connect to gRPC service, call StartRestore, spawn stream listener
    Ok(uuid::Uuid::new_v4().to_string())
}
