pub mod cancel;
pub mod config;
pub mod disk;
pub mod engine_dto;
pub mod error;
pub mod file_name;
pub mod gpt;
pub mod image;
pub mod lang;
pub mod linux_block;
pub mod log_files;
pub mod log_level;
pub mod log_mirror;
pub mod lvm_pv;
pub mod media_mode;
pub mod media_paths;
pub mod media_tools;
pub mod preflight;
pub mod progress;
pub mod restore_check;
pub mod restore_plan;
pub mod restore_state;
#[cfg(target_os = "linux")]
pub mod service_data_dir;
pub mod types;
pub mod units;
#[cfg(windows)]
pub mod win_acl;
#[cfg(windows)]
pub mod win_registry;

pub use error::{OwnBackupError, Result};

// The message catalogs (locales/*.json) — see [`lang`]. Re-exported so
// `tr!` works from any crate that depends on this one.
pub use rust_i18n;
rust_i18n::i18n!("locales", fallback = "ru");

/// Windows service registration key (used with `sc` and the SCM). No spaces so
/// `sc start OwnBackup` works unquoted.
pub const WINDOWS_SERVICE_NAME: &str = "OwnBackup";
/// Human-facing service name — shown in services.msc and usable as
/// `net start "Own Backup"`. Shared by the service, GUI and CLI so they never
/// drift apart.
pub const WINDOWS_SERVICE_DISPLAY_NAME: &str = "Own Backup";
/// systemd unit base name on Linux (`own-backup.service`).
pub const LINUX_UNIT_NAME: &str = "own-backup";

/// Windows named-pipe name for the service's gRPC transport (production).
/// The GUI/CLI dial `\\.\pipe\OwnBackup`; the service's pipe DACL grants access
/// only to SYSTEM, Administrators and the interactive desktop user — so there is
/// no listening TCP port a local process could reach. Foreground/dev runs still
/// use loopback TCP.
pub const WINDOWS_PIPE_NAME: &str = "OwnBackup";

/// Largest RPC message either side accepts. gRPC's default is 4 MiB, and on
/// the stand (2026-10-17) one job with thousands of long exclusion patterns
/// made the job list 6 MB: every request that reads the jobs then failed, so
/// the window could open none of them. Both ends set this.
pub const MAX_RPC_MESSAGE: usize = 64 * 1024 * 1024;

/// Full `\\.\pipe\OwnBackup` path built from [`WINDOWS_PIPE_NAME`].
pub fn windows_pipe_path() -> String {
    format!(r"\\.\pipe\{WINDOWS_PIPE_NAME}")
}

/// Unix group granted access to the service's UDS + token (the Linux analogue
/// of the named-pipe DACL). `--install` creates it and adds the installing
/// user; the service chowns the socket/token to it. Members reach the service
/// without root; everyone else is refused by filesystem permissions.
pub const UNIX_ACCESS_GROUP: &str = "own-backup";

/// Runtime directory holding the UDS and the per-boot token on Unix.
#[cfg(unix)]
pub const UNIX_RUNTIME_DIR: &str = "/run/own-backup";

/// Production UDS path the GUI/CLI dial on Unix (no listening TCP port).
#[cfg(unix)]
pub fn unix_socket_path() -> std::path::PathBuf {
    std::path::PathBuf::from(UNIX_RUNTIME_DIR).join("service.sock")
}

/// When the running executable was built, read off the file itself.
///
/// `BUILD_TAG`-style constants have to be bumped by hand, and the one in the
/// GUI sat at a month-old value while fresh binaries were being handed over —
/// which is worse than no marker at all, because a log that names a date is
/// read as if it meant one (2026-08-07: a medium was diagnosed as "built on the
/// 1st" from a constant nobody had touched since). The file's own timestamp
/// cannot drift from the file.
///
/// Returns `"?"` when the metadata is unreadable — a boot medium can serve the
/// exe from a read-only image whose times are odd, and a missing stamp must not
/// cost a log line.
pub fn exe_build_stamp() -> String {
    let stamp = std::env::current_exe()
        .and_then(std::fs::metadata)
        .and_then(|m| m.modified())
        .ok();
    match stamp {
        Some(t) => chrono::DateTime::<chrono::Utc>::from(t)
            .format("%Y-%m-%d %H:%M:%SZ")
            .to_string(),
        None => "?".into(),
    }
}
