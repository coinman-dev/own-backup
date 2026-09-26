use own_backup_common::config::AppConfig;
use tauri::State;

use crate::state::GuiState;

#[tauri::command]
pub async fn get_config(state: State<'_, GuiState>) -> Result<AppConfig, String> {
    Ok(state.config.lock().map_err(|e| e.to_string())?.clone())
}

#[tauri::command]
pub async fn save_config(state: State<'_, GuiState>, config: AppConfig) -> Result<(), String> {
    config.save_to_file(&state.config_path).map_err(|e| e.to_string())?;
    // The in-memory copy has to move too. Writing only the file left
    // `get_config` handing back what was loaded at startup, so anything saved
    // came back missing as soon as the user navigated away and returned.
    *state.config.lock().map_err(|e| e.to_string())? = config;
    Ok(())
}

#[derive(serde::Serialize)]
pub struct ServiceStatusDto {
    pub connected: bool,
    pub version: String,
    /// Seconds since the service started; the screen words it. 0 when not
    /// connected.
    pub uptime_secs: u64,
    pub active_jobs: u32,
    // `total_backups_completed` was here, always 0 — nothing counted it. The
    // screen showed «Total Completed: 0» next to a job list full of copies.
    pub active_mounts: Vec<own_backup_common::engine_dto::ActiveMountDto>,
}

/// Startup health of the backup service, so the GUI can warn the user when it
/// isn't running (installed-but-stopped) or isn't installed at all.
#[derive(serde::Serialize)]
pub struct ServiceHealth {
    /// gRPC endpoint answers right now.
    pub reachable: bool,
    /// The service is registered with the OS (SCM / systemd), regardless of state.
    pub installed: bool,
    /// Running from the boot medium. Then `installed: false` is not a fault to
    /// report but the design: WinPE's registry lives in RAM, so there is nowhere
    /// to install a service to, and a schedule would not outlive the session.
    pub media: bool,
}

#[tauri::command]
pub async fn service_health(state: State<'_, GuiState>) -> Result<ServiceHealth, String> {
    let reachable = state.link.is_reachable().await;
    let media = own_backup_common::media_mode::is_media_mode();
    // `sc query` blocks — keep it off the async runtime. On the medium there is
    // no SCM worth asking, so the call is skipped entirely.
    let installed = if media {
        false
    } else {
        tokio::task::spawn_blocking(crate::service_link::service_installed)
            .await
            .unwrap_or(false)
    };
    tracing::info!(reachable, installed, media, "service_health check");
    Ok(ServiceHealth { reachable, installed, media })
}

#[tauri::command]
pub async fn get_service_status(state: State<'_, GuiState>) -> Result<ServiceStatusDto, String> {
    let disconnected = || ServiceStatusDto {
        connected: false,
        version: env!("CARGO_PKG_VERSION").into(),
        uptime_secs: 0,
        active_jobs: 0,
        active_mounts: Vec::new(),
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
    let active_mounts = v
        .get("active_mounts")
        .and_then(|m| serde_json::from_value(m.clone()).ok())
        .unwrap_or_default();

    Ok(ServiceStatusDto {
        connected: true,
        version,
        uptime_secs,
        active_jobs,
        active_mounts,
    })
}
