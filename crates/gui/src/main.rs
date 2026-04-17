// Prevents additional console window on Windows in release
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod state;

use own_backup_common::config::AppConfig;
use state::GuiState;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    // Load config (or use defaults)
    let config = AppConfig::default();

    // Ensure data directory exists
    let _ = std::fs::create_dir_all(&config.data_dir);

    // Initialize metadata DB
    let _ = own_backup_metadata::open_database(&config.db_path());

    let gui_state = GuiState::new(config);

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .manage(gui_state)
        .invoke_handler(tauri::generate_handler![
            commands::disks::list_disks,
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
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
