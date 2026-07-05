use own_backup_common::config::AppConfig;
use tauri::State;

use crate::state::GuiState;

#[tauri::command]
pub async fn get_config(state: State<'_, GuiState>) -> Result<AppConfig, String> {
    Ok(state.config.clone())
}

#[tauri::command]
pub async fn save_config(state: State<'_, GuiState>, config: AppConfig) -> Result<(), String> {
    config
        .save_to_file(&state.config_path)
        .map_err(|e| e.to_string())
}

#[derive(serde::Serialize)]
pub struct ServiceStatusDto {
    pub connected: bool,
    pub version: String,
    pub uptime: String,
    pub active_jobs: u32,
    pub total_backups_completed: u64,
}

#[tauri::command]
pub async fn get_service_status(state: State<'_, GuiState>) -> Result<ServiceStatusDto, String> {
    let disconnected = || ServiceStatusDto {
        connected: false,
        version: env!("CARGO_PKG_VERSION").into(),
        uptime: "not connected".into(),
        active_jobs: 0,
        total_backups_completed: 0,
    };

    // Ask the service directly (version / uptime / queue). Failure to reach it is
    // reported as "not connected" rather than an error, so the UI can show state.
    let mut client = match state.link.client().await {
        Ok(c) => c,
        Err(_) => return Ok(disconnected()),
    };
    let json = match client.service_status_json().await {
        Ok(j) => j,
        Err(_) => return Ok(disconnected()),
    };
    let v: serde_json::Value = serde_json::from_str(&json).unwrap_or_default();
    let version = v
        .get("version")
        .and_then(|x| x.as_str())
        .unwrap_or(env!("CARGO_PKG_VERSION"))
        .to_string();
    let uptime_secs = v.get("uptime_secs").and_then(|x| x.as_u64()).unwrap_or(0);
    // active jobs = 1 while a run is in progress (queue.current is non-null).
    let active_jobs = v
        .get("queue")
        .and_then(|q| q.get("current"))
        .map(|c| if c.is_null() { 0 } else { 1 })
        .unwrap_or(0);

    Ok(ServiceStatusDto {
        connected: true,
        version,
        uptime: format!("{uptime_secs}s"),
        active_jobs,
        total_backups_completed: 0,
    })
}
