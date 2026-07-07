// Prevents additional console window on Windows in release
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod jobs;
mod service_link;
mod state;

use std::path::PathBuf;
use std::sync::Arc;

use own_backup_common::config::AppConfig;
use service_link::ServiceLink;
use state::GuiState;

/// Bump this on any build you hand over for testing. It is printed at the top of
/// the log so we can tell from the log alone WHICH binary actually ran (a common
/// trap: an old exe left in another folder still gets launched).
const BUILD_TAG: &str = "diag-2026-07-07c-namedpipe";

const ROTATE_AT: u64 = 4 * 1024 * 1024;

/// Candidate log locations, most-preferred first. Next to the exe is what the
/// user expects, but that can be read-only (a network / WSL / Program Files
/// path), so we fall back to a per-user dir and finally the temp dir.
fn candidate_log_paths() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())) {
        v.push(dir.join("own-backup-gui.log"));
    }
    #[cfg(windows)]
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        if !local.trim().is_empty() {
            v.push(PathBuf::from(local).join("OwnBackup").join("own-backup-gui.log"));
        }
    }
    v.push(std::env::temp_dir().join("own-backup-gui.log"));
    v
}

/// Open the first writable log location, rotating it if large. Returns the open
/// file and the path used.
fn open_log() -> Option<(std::fs::File, PathBuf)> {
    for path in candidate_log_paths() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::metadata(&path).map(|m| m.len() > ROTATE_AT).unwrap_or(false) {
            let old = path.with_extension("log.old");
            let _ = std::fs::remove_file(&old);
            let _ = std::fs::rename(&path, &old);
        }
        if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            return Some((f, path));
        }
    }
    None
}

/// Best-effort elevation check (Windows) so the log records whether the process
/// is running elevated — a plain unelevated GUI is the expected state.
#[cfg(windows)]
fn elevation_state() -> Option<bool> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    // SAFETY: standard token-elevation query; the handle is closed after use.
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return None;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut ret_len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut _),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret_len,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok.then_some(elevation.TokenIsElevated != 0)
    }
}

/// Diagnostic log (test-phase channel, decision 2026-07-02). The user sends this
/// file back when something misbehaves. `RUST_LOG` overrides the default filter.
fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        "info,own_backup_gui=debug,own_backup_backup=debug,own_backup_inventory=debug,\
         own_backup_restore=debug,own_backup_metadata=debug"
            .into()
    });

    let opened = open_log();
    let log_path = opened.as_ref().map(|(_, p)| p.clone());
    match opened {
        Some((f, _)) => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(f))
                .init();
        }
        None => {
            tracing_subscriber::fmt().with_env_filter(filter).init();
        }
    }

    // Panics land in the log too — crucial for remote debugging.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!(target: "panic", "PANIC: {info}");
        default_hook(info);
    }));

    // Startup diagnostics: which binary, from where, elevated or not. If the
    // build_tag here is not the one we handed over, an old exe is being run.
    tracing::info!("=== Own Backup GUI started ===");
    tracing::info!(
        build_tag = BUILD_TAG,
        version = env!("CARGO_PKG_VERSION"),
        os = std::env::consts::OS,
        pid = std::process::id(),
        "build / runtime"
    );
    match std::env::current_exe() {
        Ok(exe) => tracing::info!(exe = %exe.display(), "running executable"),
        Err(e) => tracing::warn!(error = %e, "current_exe() failed"),
    }
    if let Some(p) = &log_path {
        tracing::info!(log = %p.display(), "log file");
    }
    #[cfg(windows)]
    tracing::info!(elevated = ?elevation_state(), "process elevation (expected: Some(false))");
}

fn main() {
    // No UAC elevation at startup: backups, disk enumeration and passwords all
    // run in the privileged service (LocalSystem), which the GUI reaches over
    // gRPC. The GUI itself runs as the normal user. (In-process restore/verify
    // still need the service — see plan stage 2e.)
    init_logging();

    // Load config (or use defaults)
    let config = AppConfig::default();

    // Ensure data directory exists (config.toml is written here).
    let _ = std::fs::create_dir_all(&config.data_dir);

    // The orchestrator (job store, queue, scheduler, backup worker) lives in the
    // service; connect to it, spawning a private foreground instance if none is
    // installed/running so the GUI still works standalone.
    let link = Arc::new(ServiceLink::new(&config.grpc_address, config.token_path()));
    let gui_state = GuiState::new(config, link.clone());

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(gui_state)
        .setup(move |app| {
            // Bridge the service's event stream (progress / queue / finished) to
            // the Tauri window events the frontend already listens for.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(bridge_service_events(handle, link.clone()));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::disks::list_disks,
            commands::disks::drive_signature,
            // --- new job model (phase 6) ---
            commands::backup_jobs::list_backup_jobs,
            commands::backup_jobs::get_backup_job,
            commands::backup_jobs::new_job_template,
            commands::backup_jobs::default_exclusion_patterns,
            commands::backup_jobs::save_backup_job,
            commands::backup_jobs::delete_backup_job,
            commands::backup_jobs::decrypt_volume,
            commands::backup_jobs::run_backup_job,
            commands::backup_jobs::cancel_queued_job,
            commands::backup_jobs::cancel_running_backup,
            commands::backup_jobs::get_run_queue,
            commands::backup_jobs::list_activity,
            // --- engine ops used by Restore/Tools ---
            commands::engine::restore_now,
            commands::engine::verify_now,
            commands::engine::mount_backup,
            commands::engine::unmount_backup,
            commands::engine::read_image_info,
            commands::engine::list_disk_backups,
            commands::engine::restore_disk_now,
            commands::engine::cancel_engine_op,
            // --- misc ---
            commands::settings::get_config,
            commands::settings::save_config,
            commands::settings::get_service_status,
            commands::settings::service_health,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// Long-lived task: subscribe to the service's orchestration event stream and
/// re-emit each event as the Tauri window event the frontend listens for
/// (`job-progress` / `queue-changed` / `job-finished`). Reconnects if the stream
/// ends (e.g. the service restarts).
async fn bridge_service_events(app: tauri::AppHandle, link: Arc<ServiceLink>) {
    use own_backup_client::ClientEvent;
    use tauri::Emitter;
    use tokio_stream::StreamExt;

    loop {
        let mut client = match link.client().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "event bridge: service unreachable, retrying");
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                continue;
            }
        };
        match client.stream_events().await {
            Ok(stream) => {
                let mut stream = Box::pin(stream);
                while let Some(ev) = stream.next().await {
                    match ev {
                        ClientEvent::Progress(p) => {
                            let _ = app.emit("job-progress", p);
                        }
                        ClientEvent::Queue(q) => {
                            let _ = app.emit("queue-changed", q);
                        }
                        ClientEvent::Finished(r) => {
                            let _ = app.emit("job-finished", r);
                        }
                        // Direct-engine progress (restore / verify / disk restore):
                        // forward the raw JSON payload to the matching Tauri event.
                        ClientEvent::Unknown { kind, json } => {
                            let tauri_event = match kind.as_str() {
                                "restore" => "restore-progress",
                                "verify" => "verify-progress",
                                "restore-disk" => "restore-disk-progress",
                                _ => continue,
                            };
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&json) {
                                let _ = app.emit(tauri_event, v);
                            }
                        }
                    }
                }
                tracing::info!("service event stream ended; reconnecting");
            }
            Err(e) => tracing::warn!(error = %e, "stream_events failed"),
        }
        // Stream closed — the cached channel may be dead; drop it and retry.
        link.invalidate().await;
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}
