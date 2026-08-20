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

/// What this build is ABOUT, in a word — the feature under test, not a date.
///
/// It answers "which line of work is this exe from", and it is bumped by hand.
/// The question "is this the exe I just copied" is answered next to it by
/// `built = …`, the file's own timestamp (`exe_build_stamp`), because a
/// hand-bumped tag stops being true the moment someone forgets: this one sat at
/// "2026-08-01" through a week of handovers, and a log line reading it as a
/// build date sent a diagnosis down the wrong path (2026-08-07).
const BUILD_TAG: &str = "live-breakdown";

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
    let mut mirror_path = None;
    match opened {
        Some((f, _)) => {
            // Same second copy the service keeps, for the same reason: if the
            // machine goes down mid-restore, the drive the user named is where
            // the last lines are.
            let tee = own_backup_common::log_mirror::TeeLog::new(
                f,
                own_backup_common::log_mirror::MirrorLog::new(
                    own_backup_common::config::AppConfig::default_path(),
                    "own-backup-gui.log",
                ),
            );
            mirror_path = tee.mirror_path();
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(tee))
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
        Ok(exe) => tracing::info!(
            exe = %exe.display(),
            built = %own_backup_common::exe_build_stamp(),
            "running executable"
        ),
        Err(e) => tracing::warn!(error = %e, "current_exe() failed"),
    }
    if let Some(p) = &log_path {
        tracing::info!(log = %p.display(), "log file");
    }
    if let Some(p) = &mirror_path {
        tracing::info!(mirror = %p.display(), "log is duplicated here, flushed line by line");
    }
    #[cfg(windows)]
    tracing::info!(elevated = ?elevation_state(), "process elevation (expected: Some(false))");
}

/// Refuse to run a second GUI instance: a named mutex (session-global) is held
/// for the process lifetime; if it already exists, another instance is up, so
/// tell the user and exit. Prevents two GUIs fighting over the same private
/// service / config. Windows only (the packaged product is Windows-first).
#[cfg(target_os = "windows")]
fn enforce_single_instance() {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONINFORMATION, MB_OK};

    let name: Vec<u16> = "Global\\OwnBackupGuiSingleInstance\0".encode_utf16().collect();
    // SAFETY: name is null-terminated; we intentionally leak the handle so the
    // mutex lives for the whole process (released by the OS on exit).
    let handle = unsafe { CreateMutexW(None, true, PCWSTR(name.as_ptr())) };
    let already = handle.is_err() || unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    if already {
        let title: Vec<u16> = "My Own Backup\0".encode_utf16().collect();
        let text: Vec<u16> =
            "Программа уже запущена.\nОткройте существующее окно My Own Backup."
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
        // SAFETY: null-terminated strings; simple informational box.
        unsafe {
            MessageBoxW(
                None,
                PCWSTR(text.as_ptr()),
                PCWSTR(title.as_ptr()),
                MB_OK | MB_ICONINFORMATION,
            );
        }
        std::process::exit(0);
    }
    // Handle intentionally leaked: the mutex must outlive this function.
    std::mem::forget(handle);
}

fn main() {
    // Only one GUI at a time (Windows).
    #[cfg(target_os = "windows")]
    enforce_single_instance();

    // No UAC elevation at startup: backups, disk enumeration and passwords all
    // run in the privileged service (LocalSystem), which the GUI reaches over
    // gRPC. The GUI itself runs as the normal user. (In-process restore/verify
    // still need the service — see plan stage 2e.)
    init_logging();

    // Read config.toml. This used to be `AppConfig::default()` with a comment
    // claiming otherwise: settings were written to a file nothing ever opened,
    // so nothing the user changed survived a restart.
    let config_path = AppConfig::default_path();
    let config = AppConfig::load_or_default();

    // Ensure data directory exists (config.toml is written here).
    let _ = std::fs::create_dir_all(&config.data_dir);

    // The orchestrator (job store, queue, scheduler, backup worker) lives in the
    // service; connect to it, spawning a private foreground instance if none is
    // installed/running so the GUI still works standalone.
    let link = Arc::new(ServiceLink::new(&config.grpc_address, config.token_path()));
    let gui_state = GuiState::new(config, config_path, link.clone());

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(gui_state)
        .setup(move |app| {
            // The main window is built in code, not in tauri.conf.json, because
            // of WebView2 browser arguments: wry ALWAYS sets
            // additionalBrowserArguments through the WebView2 API, and the API
            // value makes the loader ignore the
            // WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS environment variable. On
            // WinPE those flags (--allow-run-as-system --no-sandbox) are the
            // difference between a window and HRESULT 0x80080005, so when the
            // variable is set it is handed to the API verbatim. Note wry's own
            // defaults (--disable-features=msWebOOUI,msPdfOOUI,
            // msSmartScreenProtection) are replaced, not extended — whoever sets
            // the variable must include everything wanted.
            let mut window =
                tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::default())
                    .title("My Own Backup")
                    .inner_size(1200.0, 800.0)
                    .min_inner_size(900.0, 600.0)
                    .resizable(true)
                    .decorations(true);
            #[cfg(windows)]
            if let Ok(extra) = std::env::var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS") {
                let extra = extra.trim().to_string();
                if !extra.is_empty() {
                    tracing::info!(
                        args = %extra,
                        "browser arguments from WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS passed via API"
                    );
                    window = window.additional_browser_args(&extra);
                }
            }
            // Same checks the medium's launcher runs, for the same reason and
            // with the same words: a missing WebView2 must read as «WebView2 не
            // найден, искал там-то», not as a bare wry HRESULT. On an ordinary
            // Windows this is the only place they run at all.
            {
                use own_backup_backup::media_preflight_win as pf;
                let dir = std::env::current_exe()
                    .ok()
                    .and_then(|p| p.parent().map(|d| d.to_path_buf()))
                    .unwrap_or_else(|| std::path::PathBuf::from("."));
                let checks = pf::run_preflight(&dir);
                for c in &checks {
                    match c.level {
                        pf::Level::Fail => tracing::error!(check = c.name, "{}", c.message),
                        pf::Level::Warn => tracing::warn!(check = c.name, "{}", c.message),
                        pf::Level::Ok => tracing::info!(check = c.name, "{}", c.message),
                    }
                }
                // Deliberately NOT a refusal here. The WebView2 loader also
                // finds a runtime through the registry, which this check does
                // not read, so a Fail may be our blind spot rather than a
                // missing runtime — and refusing then would break a machine
                // that works. The medium is a different matter: there the
                // launcher (`own-backup-cli media-preflight`) knows the layout
                // and refuses BEFORE this process is started. So here the
                // verdict only explains, and the attempt below decides.
                if pf::worst(&checks) == pf::Level::Fail {
                    tracing::error!(
                        "проверка окружения нашла помеху — пробую поднять окно всё равно"
                    );
                }
            }

            // A failed build (no WebView2, browser process refused to start)
            // aborts the app with the error in the log — better than the old
            // behaviour of running on windowless.
            window.build()?;

            // The window exists: say so where the launcher can see it. It is a
            // FILE and not a live process on purpose — a live process without a
            // window is exactly the state that once trapped the medium's user.
            {
                let marker = own_backup_backup::media_preflight_win::gui_up_marker();
                match std::fs::write(&marker, std::process::id().to_string()) {
                    Ok(()) => tracing::info!(marker = %marker.display(), "gui window is up"),
                    Err(e) => tracing::warn!(error = %e, "could not write the gui-up marker"),
                }
            }

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
            commands::engine::find_base_images,
            commands::engine::list_driver_packages,
            commands::engine::build_boot_media,
            commands::engine::aim_driver_status,
            commands::engine::install_aim_driver,
            commands::engine::uninstall_aim_driver,
            commands::engine::read_image_info,
            commands::engine::list_disk_backups,
            commands::engine::list_image_backups,
            commands::engine::restore_disk_now,
            commands::engine::cancel_engine_op,
            commands::engine::dry_run_restore_plan,
            commands::engine::apply_restore_plan,
            commands::engine::get_restore_state,
            commands::engine::cancel_restore_plan,
            commands::engine::ack_restore_run,
            commands::engine::verify_luks_password,
            // --- misc ---
            commands::settings::get_config,
            commands::settings::save_config,
            commands::settings::get_service_status,
            commands::settings::service_health,
            // The boot medium has no taskbar to say which layout is active.
            commands::keyboard::keyboard_layout,
            // Built-in file browsing: the native dialog is a shell dialog, and
            // the boot medium has no shell (see commands/files.rs).
            commands::files::list_roots,
            commands::files::list_dir,
            commands::files::create_dir,
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
                                "restore-plan" => "restore-plan-progress",
                                "media" => "media-progress",
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
