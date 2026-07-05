use std::path::PathBuf;
use std::sync::Arc;

use own_backup_common::config::AppConfig;

use crate::service_link::ServiceLink;

/// Shared state for all Tauri commands.
///
/// The GUI is a thin client: jobs, queue, activity, disk enumeration, passwords
/// and the direct engine ops (restore/verify/mount) all run in the service and
/// are reached through [`ServiceLink`]. The GUI keeps only its config and that
/// link.
pub struct GuiState {
    pub config: AppConfig,
    pub config_path: PathBuf,
    /// Connection to the backup service.
    pub link: Arc<ServiceLink>,
}

impl GuiState {
    pub fn new(config: AppConfig, link: Arc<ServiceLink>) -> Self {
        let config_path = config.data_dir.join("config.toml");
        Self {
            config,
            config_path,
            link,
        }
    }
}
