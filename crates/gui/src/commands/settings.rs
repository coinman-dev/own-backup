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
pub async fn get_service_status(
    state: State<'_, GuiState>,
) -> Result<ServiceStatusDto, String> {
    // Check if the gRPC service is reachable by attempting a TCP connection.
    let addr = &state.config.grpc_address;
    let connected = tokio::net::TcpStream::connect(addr).await.is_ok();

    Ok(ServiceStatusDto {
        connected,
        version: env!("CARGO_PKG_VERSION").into(),
        uptime: if connected { "running".into() } else { "not connected".into() },
        active_jobs: 0,
        total_backups_completed: 0,
    })
}
