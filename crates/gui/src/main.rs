// Prevents additional console window on Windows in release
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
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

fn main() {
    init_logging();

    // Load config (or use defaults)
    let config = AppConfig::default();

    // Ensure data directory exists
    let _ = std::fs::create_dir_all(&config.data_dir);

    // Initialize metadata DB
    let _ = own_backup_metadata::open_database(&config.db_path());

    let gui_state = GuiState::new(config);

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(gui_state)
        .invoke_handler(tauri::generate_handler![
            commands::disks::list_disks,
            commands::disks::drive_signature,
            commands::jobs::list_jobs,
            commands::jobs::create_job,
            commands::jobs::delete_job,
            commands::sets::list_sets,
            commands::sets::validate_backup,
            commands::preflight::run_preflight,
            commands::schedule::get_schedule,
            commands::schedule::set_schedule,
            commands::retention::get_retention,
            commands::retention::set_retention,
            commands::settings::get_config,
            commands::settings::save_config,
            commands::settings::get_service_status,
            commands::backup::start_backup,
            commands::backup::cancel_backup,
            commands::restore::start_restore,
            commands::engine::backup_now,
            commands::engine::restore_now,
            commands::engine::verify_now,
            commands::engine::mount_backup,
            commands::engine::unmount_backup,
            commands::engine::read_image_info,
            commands::engine::cancel_engine_op,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
