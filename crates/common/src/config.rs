use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::error::{OwnBackupError, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    /// TCP address used ONLY in development/foreground mode.
    /// In production: Unix Domain Socket (Linux) or Named Pipe (Windows).
    #[serde(default = "default_grpc_addr")]
    pub grpc_address: String,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default)]
    pub default_compression_level: i32,
    #[serde(default)]
    pub hooks: HooksConfig,
    #[serde(default)]
    pub credentials: CredentialsConfig,
}

/// Pre/post snapshot hooks for application-consistent backups.
/// Hooks run around VSS snapshot creation to flush application state.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HooksConfig {
    /// Commands to run BEFORE snapshot creation (e.g., flush DB, pause containers).
    #[serde(default)]
    pub pre_snapshot: Vec<String>,
    /// Commands to run AFTER snapshot creation (e.g., resume DB, unpause containers).
    #[serde(default)]
    pub post_snapshot: Vec<String>,
    /// Seconds to wait for a hook command before timing out. Default: 60.
    #[serde(default = "default_hook_timeout")]
    pub timeout_seconds: u64,
    /// Built-in application profiles to activate (e.g., "postgresql", "mysql", "docker").
    #[serde(default)]
    pub profiles: Vec<String>,
}

fn default_hook_timeout() -> u64 {
    60
}

/// Credential storage configuration for scheduled (unattended) backups.
/// When encryption is enabled on a scheduled job, the service needs a way to
/// retrieve the encryption password without user interaction.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CredentialsConfig {
    /// On Windows: store password in Windows Credential Manager.
    /// The credential is stored under "OwnBackup/<job_id>".
    #[serde(default)]
    pub use_windows_credential_manager: bool,
    /// On Linux: path to a key file readable only by root (chmod 600).
    /// The file contains the raw password or a key derivation seed.
    #[serde(default)]
    pub key_file: Option<PathBuf>,
}

fn default_data_dir() -> PathBuf {
    // Env override lets the service run without root (tests) and lets the GUI
    // point a spawned service at a user-writable directory.
    if let Ok(dir) = std::env::var("OWN_BACKUP_DATA_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    #[cfg(target_os = "windows")]
    {
        PathBuf::from(r"C:\ProgramData\OwnBackup")
    }
    #[cfg(not(target_os = "windows"))]
    {
        PathBuf::from("/var/lib/own-backup")
    }
}

fn default_grpc_addr() -> String {
    "127.0.0.1:50051".into()
}

fn default_log_level() -> String {
    "info".into()
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            grpc_address: default_grpc_addr(),
            log_level: default_log_level(),
            default_compression_level: 3,
            hooks: HooksConfig::default(),
            credentials: CredentialsConfig::default(),
        }
    }
}

impl AppConfig {
    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("metadata.db")
    }

    pub fn load_from_file(path: &std::path::Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        toml::from_str(&content).map_err(|e| OwnBackupError::Config(e.to_string()))
    }

    pub fn save_to_file(&self, path: &std::path::Path) -> Result<()> {
        let content = toml::to_string_pretty(self)
            .map_err(|e| OwnBackupError::Config(e.to_string()))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, content)?;
        Ok(())
    }

    /// Path for the Unix Domain Socket (Linux) or Named Pipe (Windows).
    /// This is the production IPC channel — only accessible to the local admin.
    pub fn socket_path(&self) -> PathBuf {
        #[cfg(unix)]
        {
            // /run/own-backup/ is created by the service at startup.
            // Permissions set to 0o660, group "own-backup" or "root".
            PathBuf::from("/run/own-backup/service.sock")
        }
        #[cfg(not(unix))]
        {
            // Named Pipe path on Windows: \\.\pipe\OwnBackupService
            // DACL: only Administrators group has access.
            PathBuf::from(r"\\.\pipe\OwnBackupService")
        }
    }

    /// Path for the per-boot service authentication token.
    ///
    /// Unix: alongside the UDS in the runtime dir (0o640, root + own-backup
    /// group). Windows: in the data dir, ACL'd by the service so only SYSTEM /
    /// Administrators / interactive users can read it (the unelevated GUI is an
    /// interactive process, other sessions and services are not). Regenerated on
    /// every service start.
    pub fn token_path(&self) -> PathBuf {
        #[cfg(unix)]
        {
            PathBuf::from("/run/own-backup/service.token")
        }
        #[cfg(not(unix))]
        {
            self.data_dir.join("service.token")
        }
    }
}
