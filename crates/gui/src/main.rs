// Prevents additional console window on Windows in release
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod jobs;
mod service_link;
mod state;

use std::sync::Arc;

use own_backup_common::config::AppConfig;
use service_link::ServiceLink;
use state::GuiState;

/// Diagnostic log next to the exe: `own-backup-gui.log`.
///
/// Test-phase debugging channel (decision 2026-07-02) — the user sends this
/// file back when something misbehaves. Reduce to warn/off before release
/// (plan v2, phase 7). `RUST_LOG` overrides the default filter.
fn init_logging() {
    let log_path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("own-backup-gui.log")))
        .unwrap_or_else(|| std::path::PathBuf::from("own-backup-gui.log"));

    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        "info,own_backup_gui=debug,own_backup_backup=debug,own_backup_inventory=debug,\
         own_backup_restore=debug,own_backup_metadata=debug"
            .into()
    });

    // Rotate at startup once the log grows past a few MB: keep exactly one
    // previous generation as `.old` so the file the user sends stays small
    // but the prior session is never lost.
    const ROTATE_AT: u64 = 4 * 1024 * 1024;
    if std::fs::metadata(&log_path).map(|m| m.len() > ROTATE_AT).unwrap_or(false) {
        let old = log_path.with_extension("log.old");
        let _ = std::fs::remove_file(&old);
        let _ = std::fs::rename(&log_path, &old);
    }

    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path);

    match file {
        Ok(f) => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(f))
                .init();
        }
        Err(_) => {
            // Fall back to stderr if the exe directory is not writable.
            tracing_subscriber::fmt().with_env_filter(filter).init();
        }
    }

    // Panics land in the log too — crucial for remote debugging.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!(target: "panic", "PANIC: {info}");
        default_hook(info);
    }));

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        os = std::env::consts::OS,
        "=== My Own Backup GUI started ==="
    );
}

/// If the process is not elevated, relaunch it via UAC ("runas") and exit the
/// current instance. Backups need admin rights for VSS + raw device I/O, so we
/// self-elevate at startup rather than failing later with access-denied.
#[cfg(target_os = "windows")]
fn ensure_admin() {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    // SAFETY: standard token-elevation query; handles are closed after use.
    let elevated = unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return; // can't determine — let the app try and surface errors
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
        if !ok {
            return;
        }
        elevation.TokenIsElevated != 0
    };

    if elevated {
        return;
    }

    // Relaunch elevated. The current (non-elevated) instance then exits — no
    // window has been created yet, so there is no flicker.
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let exe_w: Vec<u16> = exe.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let params: String = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    let params_w: Vec<u16> = params.encode_utf16().chain(std::iter::once(0)).collect();
    let params_ptr = if params.is_empty() {
        PCWSTR::null()
    } else {
        PCWSTR(params_w.as_ptr())
    };

    // SAFETY: all string pointers are valid NUL-terminated buffers.
    unsafe {
        ShellExecuteW(
            None,
            w!("runas"),
            PCWSTR(exe_w.as_ptr()),
            params_ptr,
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
    // Whether the user accepted (elevated instance launched) or declined the
    // UAC prompt, this non-elevated instance must not continue.
    std::process::exit(0);
}

fn main() {
    #[cfg(target_os = "windows")]
    ensure_admin();

    init_logging();

    // Load config (or use defaults)
    let config = AppConfig::default();

    // Ensure data directory exists (config.toml is written here).
    let _ = std::fs::create_dir_all(&config.data_dir);

    // The orchestrator (job store, queue, scheduler, backup worker) lives in the
    // service; connect to it, spawning a private foreground instance if none is
    // installed/running so the GUI still works standalone.
    let link = Arc::new(ServiceLink::new(&config.grpc_address, ""));
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
                        ClientEvent::Unknown { .. } => {}
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
