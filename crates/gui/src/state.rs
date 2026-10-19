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
    /// The live config. Behind a lock because `save_config` has to update it as
    /// well as the file: it used to write only the file, so the next
    /// `get_config` handed back the copy loaded at startup and every change
    /// looked lost the moment the user left the page.
    pub config: std::sync::Mutex<AppConfig>,
    /// Written by the window only on Windows; on Linux the service owns the
    /// file (see `commands::settings`).
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub config_path: PathBuf,
    /// Connection to the backup service.
    pub link: Arc<ServiceLink>,
}

impl GuiState {
    /// `config_path` is passed in rather than derived from `config.data_dir`:
    /// the file is what *says* where the data directory is, so deriving the
    /// path from a value inside it means saving to a different place than the
    /// one it was read from as soon as a user edits that setting.
    pub fn new(config: AppConfig, config_path: PathBuf, link: Arc<ServiceLink>) -> Self {
        Self {
            config: std::sync::Mutex::new(config),
            config_path,
            link,
        }
    }
}
