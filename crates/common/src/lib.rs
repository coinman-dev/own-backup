pub mod config;
pub mod disk;
pub mod error;
pub mod gpt;
pub mod image;
pub mod preflight;
pub mod progress;
pub mod types;

pub use error::{OwnBackupError, Result};

/// Windows service registration key (used with `sc` and the SCM). No spaces so
/// `sc start OwnBackup` works unquoted.
pub const WINDOWS_SERVICE_NAME: &str = "OwnBackup";
/// Human-facing service name — shown in services.msc and usable as
/// `net start "Own Backup"`. Shared by the service, GUI and CLI so they never
/// drift apart.
pub const WINDOWS_SERVICE_DISPLAY_NAME: &str = "Own Backup";
/// systemd unit base name on Linux (`own-backup.service`).
pub const LINUX_UNIT_NAME: &str = "own-backup";
