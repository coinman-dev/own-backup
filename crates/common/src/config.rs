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
    /// `info` — what happened and what went wrong; `debug` — everything our
    /// code says. Applied on the fly by both processes (see
    /// [`crate::log_level`]); anything else reads as `info`. `RUST_LOG`, when
    /// set, still wins.
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// Second copy of the log, in a directory the user names — a USB stick,
    /// another disk. Empty means off.
    ///
    /// It exists for the one failure the ordinary log cannot describe: a
    /// restore that takes the machine down with it. The local log is on the
    /// disk being written to and is lost with it; lines written here are pushed
    /// out of the OS cache one by one, so the last thing the program did is on
    /// the stick even after a bugcheck (see [`crate::log_mirror`]).
    #[serde(default)]
    pub log_mirror_dir: String,
    /// zstd level (1–19) for qcow2 and `.obk` copies of every job that has not
    /// chosen its own (`Job::compression_level`). Read at each run.
    ///
    /// The default is 3, and so is an absent key: a bare `#[serde(default)]`
    /// read a config written before this key existed as level 0 — harmless
    /// while nothing read the value, wrong the moment something did.
    #[serde(default = "default_compression_level")]
    pub default_compression_level: i32,
    /// Backup images added by hand on the restore screen, so they come back on
    /// the next launch instead of having to be picked again. Paths only: the
    /// card itself is read from each image's own embedded metadata, which is
    /// what makes this work on a machine with no job database at all.
    #[serde(default)]
    pub remembered_backups: Vec<String>,
    /// `ru` or `en` — the language the window speaks, written by it (the
    /// window resolves «Как в Windows» itself). Empty in a config from before
    /// the setting: Russian. The service follows it while running
    /// ([`crate::lang`]).
    #[serde(default)]
    pub ui_language: String,
    // Removed on 2026-09-26, and an old config that still carries them loads
    // fine — unknown keys are ignored:
    // - `consolidation_enabled`, a master switch for consolidation: it is
    //   always allowed, and a job that does not want it does not choose it;
    // - `[hooks]` (pre/post snapshot commands) and `[credentials]`: saved,
    //   never read by anything. Commands run by a SYSTEM service from a file
    //   any interactive user can have the service write are not a feature to
    //   leave lying around half-built.
}

fn default_compression_level() -> i32 {
    3
}

fn default_data_dir() -> PathBuf {
    // Env override lets the service run without root (tests) and lets the GUI
    // point a spawned service at a user-writable directory.
    if let Ok(dir) = std::env::var("OWN_BACKUP_DATA_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    // On the boot medium `C:` is the machine being rescued, not ours: state
    // written there would modify the very disk the user booted the medium to
    // save (and it did — the spike left a service.token on the target's C:).
    // The RAM disk is the only volume that is genuinely ours, and losing this
    // state at power-off is correct: it describes one rescue session.
    if crate::media_mode::is_media_mode() {
        return media_data_dir();
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

/// Where state lives on the boot medium. `%SystemRoot%` is asked rather than
/// assuming `X:\Windows`, because that is what WinPE itself sets and it costs
/// nothing to be right if it ever differs.
fn media_data_dir() -> PathBuf {
    if let Ok(root) = std::env::var("SystemRoot") {
        if !root.trim().is_empty() {
            return PathBuf::from(root).join("Temp").join("OwnBackup");
        }
    }
    std::env::temp_dir().join("OwnBackup")
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
            log_mirror_dir: String::new(),
            default_compression_level: default_compression_level(),
            remembered_backups: Vec::new(),
            ui_language: String::new(),
        }
    }
}

impl AppConfig {
    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("metadata.db")
    }

    /// Where the config lives: `<data dir>/config.toml`.
    ///
    /// Derived from the *default* data directory, because the file is what says
    /// where the data directory is — it cannot be found using a value it holds.
    pub fn default_path() -> PathBuf {
        default_data_dir().join("config.toml")
    }

    /// Read [`Self::default_path`], falling back to defaults when it is absent
    /// or unreadable.
    ///
    /// A missing file is the normal first-run case, and a corrupt one should
    /// not stop the program from starting — losing settings is recoverable,
    /// refusing to launch is not. Either way the next save rewrites it.
    pub fn load_or_default() -> Self {
        let path = Self::default_path();
        match Self::load_from_file(&path) {
            Ok(c) => c,
            Err(_) => Self::default(),
        }
    }

    pub fn load_from_file(path: &std::path::Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        toml::from_str(&content).map_err(|e| OwnBackupError::Config(e.to_string()))
    }

    pub fn save_to_file(&self, path: &std::path::Path) -> Result<()> {
        Self::with_file_lock(path, || self.write_atomically(path))
    }

    /// Read and edit the newest settings under an OS file lock shared by GUI
    /// processes, then replace the file atomically.
    pub fn update_file(path: &std::path::Path, edit: impl FnOnce(&mut Self)) -> Result<Self> {
        Self::with_file_lock(path, || {
            let mut config = match Self::load_from_file(path) {
                Ok(config) => config,
                Err(OwnBackupError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                    Self::default()
                }
                Err(e) => return Err(e),
            };
            edit(&mut config);
            config.write_atomically(path)?;
            Ok(config)
        })
    }

    fn with_file_lock<T>(path: &std::path::Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path.with_extension("toml.lock"))?;
        lock.lock()?;
        let result = f();
        drop(lock);
        result
    }

    fn write_atomically(&self, path: &std::path::Path) -> Result<()> {
        use std::io::Write;

        let content =
            toml::to_string_pretty(self).map_err(|e| OwnBackupError::Config(e.to_string()))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension(format!("toml.tmp.{}", uuid::Uuid::new_v4()));
        let commit = (|| -> std::io::Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)?;
            file.write_all(content.as_bytes())?;
            file.sync_all()?;
            std::fs::rename(&tmp, path)?;
            #[cfg(unix)]
            if let Some(parent) = path.parent() {
                if let Ok(dir) = std::fs::File::open(parent) {
                    let _ = dir.sync_all();
                }
            }
            Ok(())
        })();
        if commit.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        commit?;
        Ok(())
    }

    /// Path for the Unix Domain Socket (Linux) or Named Pipe (Windows).
    /// This is the production IPC channel — only accessible to the local admin.
    pub fn socket_path(&self) -> PathBuf {
        #[cfg(unix)]
        {
            // /run/own-backup/ is created by the service at startup; socket is
            // 0o660, chowned to the `own-backup` group (see UNIX_ACCESS_GROUP).
            crate::unix_socket_path()
        }
        #[cfg(not(unix))]
        {
            // The same named pipe the service actually creates.
            PathBuf::from(crate::windows_pipe_path())
        }
    }

    /// Path for the per-boot service authentication token.
    ///
    /// Unix: alongside the UDS in the runtime dir (0o640, root + own-backup
    /// group). Windows: in the data dir, ACL'd by the service so only SYSTEM /
    /// Administrators / interactive users can read it. The named-pipe server
    /// additionally checks the active console's session and user SID before
    /// accepting a client. Regenerated on every service start.
    pub fn token_path(&self) -> PathBuf {
        #[cfg(unix)]
        {
            PathBuf::from(crate::UNIX_RUNTIME_DIR).join("service.token")
        }
        #[cfg(not(unix))]
        {
            self.data_dir.join("service.token")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_config_reads_back_the_same() {
        // The trap this guards: TOML serialises fields in declaration order, and
        // everything after a struct field lands *inside* that field's table. A
        // Vec<String> declared below `hooks` would come back as
        // `hooks.remembered_backups` — or not come back at all.
        let mut cfg = AppConfig::default();
        cfg.remembered_backups = vec![r"E:\Backup\one.vhdx".into(), r"E:\Backup\two.qcow2".into()];
        cfg.default_compression_level = 7;

        let text = toml::to_string_pretty(&cfg).expect("config must serialise");
        let back: AppConfig = toml::from_str(&text).expect("config must parse back");

        assert_eq!(back.remembered_backups, cfg.remembered_backups, "\n{text}");
        assert_eq!(back.default_compression_level, 7);
    }

    #[test]
    fn a_config_survives_a_trip_through_a_file() {
        // The bug this guards was one level up — the GUI wrote this file and
        // then never read it, so nothing a user changed outlived a restart —
        // but the file itself has to round-trip for the fix to mean anything.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let mut cfg = AppConfig::default();
        cfg.remembered_backups = vec![r"E:\Backup\one.vhdx".into()];
        cfg.log_level = "debug".into();
        cfg.save_to_file(&path).unwrap();

        let back = AppConfig::load_from_file(&path).unwrap();
        assert_eq!(back.remembered_backups, cfg.remembered_backups);
        assert_eq!(back.log_level, "debug");
    }

    #[test]
    fn concurrent_settings_updates_keep_both_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        AppConfig::default().save_to_file(&path).unwrap();
        let first = path.clone();
        let second = path.clone();
        let a = std::thread::spawn(move || {
            AppConfig::update_file(&first, |c| c.log_level = "debug".into()).unwrap();
        });
        let b = std::thread::spawn(move || {
            AppConfig::update_file(&second, |c| c.ui_language = "en".into()).unwrap();
        });
        a.join().unwrap();
        b.join().unwrap();
        let saved = AppConfig::load_from_file(&path).unwrap();
        assert_eq!(saved.log_level, "debug");
        assert_eq!(saved.ui_language, "en");
    }

    #[test]
    fn a_corrupt_config_is_not_overwritten_by_a_settings_update() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, b"broken TOML = =").unwrap();
        assert!(AppConfig::update_file(&path, |c| c.log_level = "debug".into()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"broken TOML = =");
    }

    #[test]
    fn a_missing_or_broken_config_does_not_stop_the_program() {
        // First run, or a file someone edited badly. Losing settings is
        // recoverable; refusing to start is not.
        let dir = tempfile::tempdir().unwrap();
        assert!(AppConfig::load_from_file(&dir.path().join("nope.toml")).is_err());
        let bad = dir.path().join("bad.toml");
        std::fs::write(&bad, "this is not toml = = =").unwrap();
        assert!(AppConfig::load_from_file(&bad).is_err());
    }

    #[test]
    fn a_config_from_before_the_compression_key_reads_level_3() {
        // Written before `default_compression_level` existed, and with the
        // keys removed since (the consolidation switch, `[hooks]`,
        // `[credentials]`): it must load, and the missing level must be the
        // default, not 0.
        let back: AppConfig = toml::from_str(
            "log_level = \"info\"\nconsolidation_enabled = false\n\n\
             [hooks]\npre_snapshot = [\"net stop x\"]\n\n\
             [credentials]\nuse_windows_credential_manager = true\n",
        )
        .expect("an old config must still load");
        assert_eq!(back.default_compression_level, 3);
    }

    #[test]
    fn scalars_and_arrays_come_before_the_tables() {
        // Same thing stated structurally: every plain key has to appear before
        // the first [table] header, or it belongs to that table.
        let text = toml::to_string_pretty(&AppConfig::default()).unwrap();
        let first_table = text.find("\n[").unwrap_or(text.len());
        assert!(
            !text[first_table..].contains("remembered_backups"),
            "remembered_backups fell inside a table:\n{text}"
        );
    }
}
