use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use own_backup_common::config::AppConfig;

use crate::service_link::ServiceLink;

/// Shared state for all Tauri commands.
///
/// Orchestration (jobs, queue, activity, scheduler) lives in the service and is
/// reached through [`ServiceLink`]. What remains in-process are the direct
/// engine operations still exposed as local commands — restore, verify, mount
/// and whole-disk restore — which is why the cancel flag stays here.
pub struct GuiState {
    pub config: AppConfig,
    pub config_path: PathBuf,
    /// Connection to the backup service (job/queue/activity/status/passwords).
    pub link: Arc<ServiceLink>,
    /// Set to true to cancel the currently running in-process engine operation.
    pub cancel_flag: Arc<AtomicBool>,
}

impl GuiState {
    pub fn new(config: AppConfig, link: Arc<ServiceLink>) -> Self {
        let config_path = config.data_dir.join("config.toml");
        Self {
            config,
            config_path,
            link,
            cancel_flag: Arc::new(AtomicBool::new(false)),
        }
    }
}
