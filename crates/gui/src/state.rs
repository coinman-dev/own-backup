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
    /// The live config. Behind a lock because `save_config` has to update it as
    /// well as the service's file: it used to write only the file, so the next
    /// `get_config` handed back the copy loaded at startup and every change
    /// looked lost the moment the user left the page. The file itself is the
    /// service's to write (see `commands::settings`).
    pub config: std::sync::Mutex<AppConfig>,
    /// Connection to the backup service.
    pub link: Arc<ServiceLink>,
}

impl GuiState {
    pub fn new(config: AppConfig, link: Arc<ServiceLink>) -> Self {
        Self {
            config: std::sync::Mutex::new(config),
            link,
        }
    }
}
