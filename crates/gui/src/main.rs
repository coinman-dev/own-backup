// Prevents additional console window on Windows in release
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod jobs;
mod state;

use own_backup_common::config::AppConfig;
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

    // Ensure data directory exists
    let _ = std::fs::create_dir_all(&config.data_dir);

    // Initialize metadata DB (legacy path; kept until the service rewrite)
    let _ = own_backup_metadata::open_database(&config.db_path());

    // Job store + serial run queue.
    let job_store = jobs::JobStore::load(&config.data_dir);
    let run_queue = jobs::RunQueue::new();
    let (job_tx, job_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    let gui_state = GuiState::new(config, job_store.clone(), run_queue.clone(), job_tx);
    let cancel_flag = gui_state.cancel_flag.clone();
    let passwords = gui_state.passwords.clone();

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(gui_state)
        .setup(move |app| {
            // Spawn the background run-queue worker, bridging core events to
            // Tauri window events via the sink.
            let sink: std::sync::Arc<dyn jobs::EventSink> =
                std::sync::Arc::new(jobs::TauriEventSink { app: app.handle().clone() });
            tauri::async_runtime::spawn(jobs::run_queue_worker(
                sink, job_store, run_queue, cancel_flag, passwords, job_rx,
            ));
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
