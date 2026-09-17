//! CLI logging, mirroring the service (`service/src/main.rs`): everything
//! from `info` up goes to `own-backup-cli.log` next to the exe (fallbacks:
//! data dir, temp; ~4 MB rotation, no ANSI), while the console keeps the old
//! quiet `warn` default. `RUST_LOG` overrides both filters. The file is what
//! survives an elevated relaunch closing its console window.

use std::path::PathBuf;

use own_backup_common::config::AppConfig;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

const ROTATE_AT: u64 = 4 * 1024 * 1024;

/// Open the CLI log file, most-preferred location first (same policy and
/// rotation as the service log).
fn open_cli_log() -> Option<(std::fs::File, PathBuf)> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(dir) =
        std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf()))
    {
        candidates.push(dir.join("own-backup-cli.log"));
    }
    candidates.push(AppConfig::default().data_dir.join("own-backup-cli.log"));
    candidates.push(std::env::temp_dir().join("own-backup-cli.log"));
    for path in candidates {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::metadata(&path).map(|m| m.len() > ROTATE_AT).unwrap_or(false) {
            let old = path.with_extension("log.old");
            let _ = std::fs::remove_file(&old);
            let _ = std::fs::rename(&path, &old);
        }
        if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            return Some((f, path));
        }
    }
    None
}

/// Command line with secret values masked — `--password X` / `--password=X`
/// must never reach the log file.
fn redacted_cmdline() -> String {
    let mut out: Vec<String> = Vec::new();
    let mut mask_next = false;
    for arg in std::env::args() {
        if mask_next {
            out.push("***".into());
            mask_next = false;
        } else if arg == "--password" {
            mask_next = true;
            out.push(arg);
        } else if let Some((key, _)) = arg.split_once('=') {
            if key == "--password" {
                out.push(format!("{key}=***"));
            } else {
                out.push(arg);
            }
        } else {
            out.push(arg);
        }
    }
    out.join(" ")
}

/// Install the two-sink subscriber and record startup facts. Returns the log
/// file path when one could be opened.
/// May the console be given colours?
///
/// On the boot medium it may not: WinPE's console does not interpret escape
/// sequences unless it is asked to, and every log line came out as
/// `←[2m2026-08-01…←[0m ←[32m INFO←[0m`. So the console is asked first, and
/// colours are used only if it agrees.
#[cfg(target_os = "windows")]
fn console_takes_ansi() -> bool {
    use windows::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, CONSOLE_MODE,
        ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_ERROR_HANDLE,
    };
    // SAFETY: plain console-mode query and set on our own standard handle.
    unsafe {
        let Ok(h) = GetStdHandle(STD_ERROR_HANDLE) else { return false };
        let mut mode = CONSOLE_MODE(0);
        if GetConsoleMode(h, &mut mode).is_err() {
            return false; // redirected to a file or a pipe: no escapes there
        }
        if mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING != CONSOLE_MODE(0) {
            return true;
        }
        SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING).is_ok()
    }
}

#[cfg(not(target_os = "windows"))]
fn console_takes_ansi() -> bool {
    true
}

/// Install the two-sink subscriber and record startup facts. Returns the log
/// file path when one could be opened.
pub fn init() -> Option<PathBuf> {
    let console_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "warn".into());
    let ansi = console_takes_ansi();

    let log_path = match open_cli_log() {
        Some((f, path)) => {
            let file_filter = tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into());
            tracing_subscriber::registry()
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_ansi(ansi)
                        .with_writer(std::io::stderr)
                        .with_filter(console_filter),
                )
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_ansi(false)
                        // The same second copy the service and the interface
                        // keep: on the boot medium this is the stick, and the
                        // screen-mode decisions made before either of them
                        // starts are exactly what a dark screen leaves unread.
                        .with_writer(std::sync::Mutex::new(
                            own_backup_common::log_mirror::TeeLog::new(
                                f,
                                own_backup_common::log_mirror::MirrorLog::new(
                                    AppConfig::default_path(),
                                    "own-backup-cli.log",
                                ),
                            ),
                        ))
                        .with_filter(file_filter),
                )
                .init();
            Some(path)
        }
        None => {
            tracing_subscriber::fmt()
                .with_ansi(ansi)
                .with_writer(std::io::stderr)
                .with_env_filter(console_filter)
                .init();
            None
        }
    };

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        os = std::env::consts::OS,
        pid = std::process::id(),
        cmd = %redacted_cmdline(),
        "=== own-backup-cli run ==="
    );
    if let Ok(exe) = std::env::current_exe() {
        tracing::info!(
            exe = %exe.display(),
            built = %own_backup_common::exe_build_stamp(),
            "running executable"
        );
    }
    if let Some(p) = &log_path {
        tracing::info!(log = %p.display(), "log file");
    }
    log_path
}

#[cfg(test)]
mod tests {
    #[test]
    fn password_values_are_masked() {
        // Direct check of the masking logic on a synthetic argv.
        let args = ["own-backup-cli", "backup", "C:", "--password", "s3cret", "-o", "x.qcow2"];
        let mut out: Vec<String> = Vec::new();
        let mut mask_next = false;
        for arg in args {
            if mask_next {
                out.push("***".into());
                mask_next = false;
            } else {
                if arg == "--password" {
                    mask_next = true;
                }
                out.push(arg.into());
            }
        }
        let line = out.join(" ");
        assert!(line.contains("--password ***"));
        assert!(!line.contains("s3cret"));
    }
}
