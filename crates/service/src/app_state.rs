use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use own_backup_common::config::AppConfig;

/// Shared application state accessible from gRPC handlers and scheduler.
pub struct AppState {
    #[allow(dead_code)] // read once real backup orchestration lands (plan v2, phase 4)
    pub config: AppConfig,
    pub db_path: PathBuf,
    #[allow(dead_code)] // read once real backup orchestration lands (plan v2, phase 4)
    pub active_backup_id: Mutex<Option<String>>,
    pub start_time: std::time::Instant,
}

impl AppState {
    pub fn new(config: AppConfig) -> Arc<Self> {
        let db_path = config.db_path();
        Arc::new(Self {
            config,
            db_path,
            active_backup_id: Mutex::new(None),
            start_time: std::time::Instant::now(),
        })
    }

    pub fn uptime_seconds(&self) -> u64 {
        self.start_time.elapsed().as_secs()
    }
}
