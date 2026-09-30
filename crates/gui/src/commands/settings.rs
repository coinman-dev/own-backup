use own_backup_common::config::AppConfig;
use tauri::State;

use crate::state::GuiState;

#[tauri::command]
pub async fn get_config(state: State<'_, GuiState>) -> Result<AppConfig, String> {
    Ok(state.config.lock().map_err(|e| e.to_string())?.clone())
}

#[tauri::command]
pub async fn save_config(state: State<'_, GuiState>, config: AppConfig) -> Result<(), String> {
    // The language is not a field of the settings form: it is switched on its
    // own (`set_ui_language`), and a form loaded before the switch would
    // otherwise save the old one back.
    let mut current = state.config.lock().map_err(|e| e.to_string())?;
    let before = current.clone();
    let saved = AppConfig::update_file(&state.config_path, |fresh| {
        if config.data_dir != before.data_dir {
            fresh.data_dir = config.data_dir.clone();
        }
        if config.grpc_address != before.grpc_address {
            fresh.grpc_address = config.grpc_address.clone();
        }
        if config.log_level != before.log_level {
            fresh.log_level = config.log_level.clone();
        }
        if config.log_mirror_dir != before.log_mirror_dir {
            fresh.log_mirror_dir = config.log_mirror_dir.clone();
        }
        if config.default_compression_level != before.default_compression_level {
            fresh.default_compression_level = config.default_compression_level;
        }
        if config.remembered_backups != before.remembered_backups {
            fresh.remembered_backups = config.remembered_backups.clone();
        }
    })
    .map_err(|e| e.to_string())?;
    // The in-memory copy has to move too. Writing only the file left
    // `get_config` handing back what was loaded at startup, so anything saved
    // came back missing as soon as the user navigated away and returned.
    *current = saved;
    Ok(())
}

/// The window's language, already resolved («Как в Windows» → `ru`/`en`).
/// This process's own messages switch at once; the config remembers it for
/// the service, which follows the file (`own_backup_common::lang`). Written
/// only when it changed — the window says it at every start.
#[tauri::command]
pub async fn set_ui_language(state: State<'_, GuiState>, lang: String) -> Result<(), String> {
    let code = if lang.trim().eq_ignore_ascii_case("en") {
        "en"
    } else {
        "ru"
    };
    let mut config = state.config.lock().map_err(|e| e.to_string())?;
    if config.ui_language == code {
        own_backup_common::lang::set(code);
        return Ok(());
    }
    let saved = AppConfig::update_file(&state.config_path, |fresh| {
        fresh.ui_language = code.to_string();
    })
    .map_err(|e| e.to_string())?;
    *config = saved;
    own_backup_common::lang::set(code);
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
    Ok(ServiceHealth {
        reachable,
        installed,
        media,
    })
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
