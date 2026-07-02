use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use own_backup_common::config::AppConfig;

/// Shared state for all Tauri commands.
pub struct GuiState {
    pub config: AppConfig,
    pub db_path: PathBuf,
    pub config_path: PathBuf,
    /// Cached gRPC connection status
    #[allow(dead_code)] // read once the GUI gets a real gRPC client (plan v2, phase 6)
    pub service_connected: Mutex<bool>,
    /// Set to true to cancel the currently running engine operation.
    pub cancel_flag: Arc<AtomicBool>,
}

impl GuiState {
    pub fn new(config: AppConfig) -> Self {
        let db_path = config.db_path();
        let config_path = config.data_dir.join("config.toml");
        Self {
            config,
            db_path,
            config_path,
            service_connected: Mutex::new(false),
            cancel_flag: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Open a new database connection (SQLite with WAL handles concurrent readers).
    pub fn open_db(&self) -> Result<rusqlite::Connection, String> {
        own_backup_metadata::open_database(&self.db_path).map_err(|e| e.to_string())
    }
}
