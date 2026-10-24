//! Duplicating the log onto a drive that survives the machine.
//!
//! The log that explains a restore lives on the disk the restore is writing to.
//! When a restore goes wrong badly enough — the machine bugchecks, the volume
//! never comes back — that log goes with it, and the one question worth
//! answering ("what had it done when it stopped?") has no evidence left.
//!
//! So the user may name a second place for it: a USB stick, another disk,
//! anything that is not the patient. Every line goes there too, and — this is
//! the whole point — is pushed out of the OS cache before the write returns.
//! An ordinary log write only reaches a cache the bugcheck discards; a line
//! that has been `sync_data`d is on the stick when the power goes.
//!
//! Two consequences shape the code:
//!
//! - **Nothing here may log.** It runs *inside* the subscriber's writer; a
//!   `tracing` call from here re-enters it while its lock is held. Failures are
//!   swallowed and retried, and the only thing that reports them is the mirror
//!   file's own absence.
//! - **The setting is re-read while running.** The service reads its config at
//!   startup, but a user who turns this on does so precisely because they are
//!   about to run the restore that might take the machine down — after which a
//!   "restart the service first" instruction is worth nothing. The path is
//!   re-checked every few seconds, so switching it on takes effect on the next
//!   line written.
//!
//! On the boot medium there is no setting to read — its config lives on a RAM
//! disk that starts empty — so the launcher names the target instead, in
//! [`LOG_MIRROR_ENV`]: the stick the medium booted from. That is where the logs
//! of a session that ended in a dark screen or a reboot can still be read.
//!
//! On Linux the service is root and the folder is one a user named (through
//! the service, which checks it — `crates/service/src/settings.rs`). So the
//! file is reached there without following a symbolic link at any step, is
//! never a second name of another file, and a folder named in the config is
//! not created: a missing one is a stick that is not plugged in, and creating
//! its path would put directories on the root filesystem in its place.
//!
//! On Windows the service is SYSTEM, and every user may create files in
//! `ProgramData\OwnBackup` — so `config.toml` used to belong to whoever made it
//! first. The folder is now taken from the file only when SYSTEM, the
//! Administrators or this process's own user owns it (the service re-owns an
//! old file at start, `crates/service/src/settings.rs`), it is not created
//! either, and the file is opened by a handle that is checked to be a plain
//! file at exactly the named path ([`crate::win_acl::open_append_checked`]).

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::config::AppConfig;

/// Environment variable naming the mirror directory outright. Takes precedence
/// over the config file; set by the boot medium's launcher.
pub const LOG_MIRROR_ENV: &str = "OWN_BACKUP_LOG_MIRROR";

/// How often the config file is consulted for a changed mirror path.
const RECHECK: Duration = Duration::from_secs(3);

/// Rotate the mirror at this size, keeping one previous file. A flash drive is
/// small and a log that fills it helps no one.
const ROTATE_AT: u64 = 4 * 1024 * 1024;

/// A log file on a drive the user chose, kept in step with the config.
pub struct MirrorLog {
    /// Where to read the setting from. Re-read, not cached: it is edited by
    /// another process (the GUI writes the config, the service reads it).
    config_path: PathBuf,
    /// File name to create inside the chosen directory.
    file_name: String,
    /// A directory named by [`LOG_MIRROR_ENV`], read once at start: the
    /// process's environment does not change under it.
    fixed_dir: Option<PathBuf>,
    /// The directory currently in force, and the handle open in it.
    dir: Option<PathBuf>,
    file: Option<File>,
    next_check: Instant,
}

impl MirrorLog {
    /// A mirror for `file_name`, reading its target from `config_path`.
    pub fn new(config_path: PathBuf, file_name: impl Into<String>) -> Self {
        let fixed = std::env::var(LOG_MIRROR_ENV)
            .ok()
            .map(|d| d.trim().to_string())
            .filter(|d| !d.is_empty())
            .map(PathBuf::from);
        Self::with_fixed_dir(config_path, file_name, fixed)
    }

    /// [`Self::new`] with the environment's answer passed in — tests cannot
    /// set a process-wide variable without racing each other.
    fn with_fixed_dir(
        config_path: PathBuf,
        file_name: impl Into<String>,
        fixed_dir: Option<PathBuf>,
    ) -> Self {
        let mut this = Self {
            config_path,
            file_name: file_name.into(),
            fixed_dir,
            dir: None,
            file: None,
            // Checked on the first line rather than here: at construction time
            // the drive may not be plugged in yet.
            next_check: Instant::now(),
        };
        this.refresh();
        this
    }

    /// Where the mirror is writing right now, if anywhere. For the startup log
    /// line that tells the user the option took effect.
    pub fn active_path(&self) -> Option<PathBuf> {
        let dir = self.dir.as_ref()?;
        self.file.as_ref().map(|_| dir.join(&self.file_name))
    }

    /// Re-read the setting if enough time has passed, and open or close the
    /// mirror to match it.
    fn refresh(&mut self) {
        let now = Instant::now();
        if now < self.next_check {
            return;
        }
        self.next_check = now + RECHECK;

        let wanted = self.fixed_dir.clone().or_else(|| {
            #[cfg(windows)]
            if !config_may_steer(&self.config_path) {
                return None;
            }
            AppConfig::load_from_file(&self.config_path)
                .ok()
                .map(|c| c.log_mirror_dir)
                .filter(|d| !d.trim().is_empty())
                .map(|d| PathBuf::from(d.trim()))
        });

        if wanted != self.dir {
            self.dir = wanted;
            self.file = None; // reopened below, under the new directory
        }
        // Retried on every check while the directory is set but unopenable: a
        // stick that is plugged in halfway through starts receiving lines.
        if self.file.is_none() {
            if let Some(dir) = self.dir.clone() {
                // Only the launcher's own choice is created; see the module
                // note on Linux.
                let create = self.fixed_dir.is_some();
                self.file = open_mirror(&dir, &self.file_name, create);
            }
        }
    }

    /// Append one formatted event, and get it out of the cache.
    ///
    /// Best effort throughout: a stick pulled mid-restore closes the mirror and
    /// leaves the primary log untouched.
    pub fn write_event(&mut self, buf: &[u8]) {
        self.refresh();
        let Some(file) = self.file.as_mut() else {
            return;
        };
        // `sync_data` rather than `flush`: flushing a File is a no-op — the
        // bytes are already in the OS cache, which is exactly what a bugcheck
        // throws away.
        if file.write_all(buf).is_err() || file.sync_data().is_err() {
            self.file = None;
            // Retry on the next check rather than on the next line: a drive
            // that has gone away fails slowly, and a restore writes a lot of
            // lines.
            self.next_check = Instant::now() + RECHECK;
        }
    }
}

/// Whether the settings file at `path` may name the mirror folder for this
/// process (see the module note on Windows).
#[cfg(windows)]
fn config_may_steer(path: &Path) -> bool {
    crate::win_acl::owner_sid(path)
        .ok()
        .flatten()
        .is_some_and(|owner| crate::win_acl::owner_may_steer(&owner))
}

/// Open (and rotate) `<dir>\<name>` by a checked handle, or `None` if it is not
/// there or is not a plain file of its own at exactly that path.
#[cfg(windows)]
fn open_mirror(dir: &Path, name: &str, create: bool) -> Option<File> {
    use crate::win_acl::{open_append_checked, rename_in_place};

    if create {
        std::fs::create_dir_all(dir).ok()?;
    }
    let path = dir.join(name);
    let old = Path::new(name).with_extension("log.old");
    let old = old.to_str()?;
    // The second round only follows a rotation.
    for _ in 0..2 {
        let (file, _) = open_append_checked(&path).ok()?;
        if file
            .metadata()
            .map(|m| m.len() > ROTATE_AT)
            .unwrap_or(false)
        {
            // Renamed through the handle, within its own folder.
            rename_in_place(&file, old).ok()?;
            continue;
        }
        return Some(file);
    }
    None
}

/// Open (and rotate) `<dir>/<name>`, or `None` if the drive is not there.
#[cfg(not(any(target_os = "linux", windows)))]
fn open_mirror(dir: &Path, name: &str, _create: bool) -> Option<File> {
    std::fs::create_dir_all(dir).ok()?;
    let path = dir.join(name);
    if std::fs::metadata(&path)
        .map(|m| m.len() > ROTATE_AT)
        .unwrap_or(false)
    {
        let old = path.with_extension("log.old");
        let _ = std::fs::remove_file(&old);
        let _ = std::fs::rename(&path, &old);
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()
}

/// Open (and rotate) `<dir>/<name>` without following a link anywhere on the
/// way, or `None` if it is not there or is not a plain file of its own.
#[cfg(target_os = "linux")]
fn open_mirror(dir: &Path, name: &str, create: bool) -> Option<File> {
    if create {
        std::fs::create_dir_all(dir).ok()?;
    }
    let at = nofollow::open_dir(dir).ok()?;
    nofollow::rotate(&at, name, ROTATE_AT);
    nofollow::open_append(&at, name).ok()
}

/// Reaching a file by a path that a user may have built, from a process that
/// may be root.
///
/// Checking a path and then opening it by name leaves a gap: whoever owns a
/// folder along the way can swap it for a link in between. So every step is
/// opened relative to the one before it, with `O_NOFOLLOW`, and the file is
/// judged by the descriptor that was actually opened.
#[cfg(target_os = "linux")]
mod nofollow {
    use std::ffi::{CString, OsStr};
    use std::fs::File;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Component, Path};

    fn c_name(name: &OsStr) -> io::Result<CString> {
        CString::new(name.as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
    }

    fn open_at(dir: libc::c_int, name: &CString, flags: libc::c_int) -> io::Result<OwnedFd> {
        // SAFETY: `name` is NUL-terminated and outlives the call; a
        // non-negative result is a new descriptor owned by nobody else.
        let fd = unsafe { libc::openat(dir, name.as_ptr(), flags | libc::O_CLOEXEC, 0o644) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: just opened, see above.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// `path` opened as a directory, refusing a link at any component and
    /// any `.`/`..`: an absolute path that means exactly what it says.
    pub(super) fn open_dir(path: &Path) -> io::Result<OwnedFd> {
        if !path.is_absolute() {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        let mut at = open_at(
            libc::AT_FDCWD,
            &c_name(OsStr::new("/"))?,
            libc::O_RDONLY | libc::O_DIRECTORY,
        )?;
        for part in path.components() {
            match part {
                Component::RootDir => {}
                Component::Normal(name) => {
                    at = open_at(
                        at.as_raw_fd(),
                        &c_name(name)?,
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                    )?;
                }
                _ => return Err(io::Error::from(io::ErrorKind::InvalidInput)),
            }
        }
        Ok(at)
    }

    /// Append to `name` in `dir`, creating it. A link, a FIFO, a device, or a
    /// file that has a second name somewhere else is refused: appending to
    /// any of those writes somewhere other than this folder.
    pub(super) fn open_append(dir: &OwnedFd, name: &str) -> io::Result<File> {
        let file = File::from(open_at(
            dir.as_raw_fd(),
            &c_name(OsStr::new(name))?,
            // Non-blocking so a FIFO planted under the name fails at once
            // instead of stalling every log line waiting for a reader.
            libc::O_WRONLY | libc::O_APPEND | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )?);
        let meta = file.metadata()?;
        if !meta.file_type().is_file() || meta.nlink() != 1 {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        Ok(file)
    }

    /// Keep one previous file when `name` has grown past `limit`. Renaming and
    /// removing act on the names in `dir` themselves, never on what a link
    /// points to.
    pub(super) fn rotate(dir: &OwnedFd, name: &str, limit: u64) {
        let Ok(cur) = c_name(OsStr::new(name)) else {
            return;
        };
        let old_name = Path::new(name).with_extension("log.old");
        let Ok(old) = c_name(old_name.as_os_str()) else {
            return;
        };
        // SAFETY: an all-zero `stat` is a valid value for fstatat to overwrite.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: valid descriptor, NUL-terminated name, writable `st`.
        let rc = unsafe {
            libc::fstatat(
                dir.as_raw_fd(),
                cur.as_ptr(),
                &mut st,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if rc != 0 || st.st_mode & libc::S_IFMT != libc::S_IFREG || (st.st_size as u64) <= limit {
            return;
        }
        // SAFETY: as above; both act on entries of `dir` only.
        unsafe {
            libc::unlinkat(dir.as_raw_fd(), old.as_ptr(), 0);
            libc::renameat(dir.as_raw_fd(), cur.as_ptr(), dir.as_raw_fd(), old.as_ptr());
        }
    }
}

/// The subscriber's writer: the ordinary log file, plus the mirror.
///
/// Wrapped in a `Mutex` by the caller, which is what `tracing-subscriber`
/// accepts as a writer factory.
///
/// Generic over the primary because that is now a writer that changes files at
/// midnight ([`crate::log_files::DailyLog`]) rather than one open `File`.
pub struct TeeLog<W: Write = File> {
    primary: W,
    mirror: MirrorLog,
}

impl<W: Write> TeeLog<W> {
    pub fn new(primary: W, mirror: MirrorLog) -> Self {
        Self { primary, mirror }
    }

    pub fn mirror_path(&self) -> Option<PathBuf> {
        self.mirror.active_path()
    }
}

impl<W: Write> Write for TeeLog<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // The primary comes first and its error is the one that propagates: it
        // is the log that must exist. The mirror is an extra copy, and losing
        // it must not cost the user the original.
        let n = self.primary.write(buf)?;
        self.mirror.write_event(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.primary.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;

    /// A config file naming `dir` as the mirror target.
    fn config_at(path: &Path, dir: Option<&Path>) {
        let mut cfg = AppConfig::default();
        cfg.log_mirror_dir = dir.map(|d| d.display().to_string()).unwrap_or_default();
        cfg.save_to_file(path).unwrap();
    }

    #[test]
    fn a_configured_directory_receives_every_line() {
        let home = tempfile::tempdir().unwrap();
        let stick = tempfile::tempdir().unwrap();
        let cfg = home.path().join("config.toml");
        config_at(&cfg, Some(stick.path()));

        let mut mirror = MirrorLog::new(cfg, "test.log");
        mirror.write_event(b"first line\n");
        mirror.write_event(b"second line\n");

        let text = std::fs::read_to_string(stick.path().join("test.log")).unwrap();
        assert_eq!(text, "first line\nsecond line\n");
        assert!(mirror.active_path().is_some());
    }

    /// The boot medium's case: no config at all, the launcher names the stick.
    #[test]
    fn a_directory_named_by_the_environment_needs_no_config() {
        let home = tempfile::tempdir().unwrap();
        let stick = tempfile::tempdir().unwrap();
        let mut mirror = MirrorLog::with_fixed_dir(
            home.path().join("nope.toml"),
            "test.log",
            Some(stick.path().join("logs")),
        );
        mirror.write_event(b"from the medium\n");
        let text = std::fs::read_to_string(stick.path().join("logs").join("test.log")).unwrap();
        assert_eq!(text, "from the medium\n");
    }

    #[test]
    fn no_setting_means_no_file_and_no_error() {
        let home = tempfile::tempdir().unwrap();
        let cfg = home.path().join("config.toml");
        config_at(&cfg, None);

        let mut mirror = MirrorLog::new(cfg, "test.log");
        mirror.write_event(b"nowhere to go\n");
        assert!(mirror.active_path().is_none());
    }

    #[test]
    fn a_missing_config_file_is_simply_off() {
        // First run, or a data directory that does not exist yet. Logging must
        // not care.
        let home = tempfile::tempdir().unwrap();
        let mut mirror = MirrorLog::new(home.path().join("nope.toml"), "test.log");
        mirror.write_event(b"ignored\n");
        assert!(mirror.active_path().is_none());
    }

    #[test]
    fn turning_the_setting_on_takes_effect_without_a_restart() {
        // The case this exists for: the user switches it on and starts the
        // restore that may take the machine down, with no chance to restart the
        // service in between.
        let home = tempfile::tempdir().unwrap();
        let stick = tempfile::tempdir().unwrap();
        let cfg = home.path().join("config.toml");
        config_at(&cfg, None);

        let mut mirror = MirrorLog::new(cfg.clone(), "test.log");
        mirror.write_event(b"before\n");
        assert!(!stick.path().join("test.log").exists());

        config_at(&cfg, Some(stick.path()));
        // The re-check is time-based; pretend the interval has passed rather
        // than sleeping through it.
        mirror.next_check = Instant::now();
        mirror.write_event(b"after\n");

        assert_eq!(
            std::fs::read_to_string(stick.path().join("test.log")).unwrap(),
            "after\n"
        );
    }

    #[test]
    fn an_unwritable_target_leaves_the_primary_log_alone() {
        let home = tempfile::tempdir().unwrap();
        let cfg = home.path().join("config.toml");
        let blocked = home.path().join("not-a-directory");
        std::fs::write(&blocked, b"keep this file").unwrap();
        config_at(&cfg, Some(&blocked.join("own-backup")));

        let primary_path = home.path().join("primary.log");
        let primary = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&primary_path)
            .unwrap();
        let mut tee = TeeLog::new(primary, MirrorLog::new(cfg, "test.log"));
        tee.write_all(b"still logged\n").unwrap();

        assert_eq!(
            std::fs::read_to_string(&primary_path).unwrap(),
            "still logged\n"
        );
        assert!(tee.mirror_path().is_none());
        assert_eq!(std::fs::read(&blocked).unwrap(), b"keep this file");
    }

    #[test]
    fn the_mirror_is_rotated_rather_than_left_to_fill_the_drive() {
        let home = tempfile::tempdir().unwrap();
        let stick = tempfile::tempdir().unwrap();
        let cfg = home.path().join("config.toml");
        config_at(&cfg, Some(stick.path()));

        let path = stick.path().join("test.log");
        std::fs::write(&path, vec![b'x'; (ROTATE_AT + 1) as usize]).unwrap();

        let mut mirror = MirrorLog::new(cfg, "test.log");
        mirror.write_event(b"fresh\n");

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fresh\n");
        assert!(
            stick.path().join("test.log.old").exists(),
            "the previous one is kept"
        );
    }

    /// The service is root, and the folder is one a user named: a link
    /// anywhere on the way must not carry its writes somewhere else.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_link_on_the_way_or_under_the_name_is_not_followed() {
        use std::os::unix::fs::symlink;

        let home = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let cfg = home.path().join("config.toml");

        // The folder itself is a link.
        let linked = home.path().join("stick");
        symlink(elsewhere.path(), &linked).unwrap();
        config_at(&cfg, Some(&linked));
        let mut mirror = MirrorLog::new(cfg.clone(), "test.log");
        mirror.write_event(b"through a link\n");
        assert!(mirror.active_path().is_none());
        assert!(!elsewhere.path().join("test.log").exists());

        // A real folder, with the file name planted as a link.
        let real = home.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let victim = elsewhere.path().join("victim");
        std::fs::write(&victim, b"keep").unwrap();
        symlink(&victim, real.join("test.log")).unwrap();
        config_at(&cfg, Some(&real));
        let mut mirror = MirrorLog::new(cfg.clone(), "test.log");
        mirror.write_event(b"into the victim\n");
        assert!(mirror.active_path().is_none());
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep");

        // The name as a second name of another file.
        std::fs::remove_file(real.join("test.log")).unwrap();
        std::fs::hard_link(&victim, real.join("test.log")).unwrap();
        let mut mirror = MirrorLog::new(cfg, "test.log");
        mirror.write_event(b"into the victim\n");
        assert!(mirror.active_path().is_none());
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
    }

    /// A folder named in the config that is not there is a stick that is not
    /// plugged in; its path is not built on the root filesystem instead.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_missing_configured_folder_is_not_created() {
        let home = tempfile::tempdir().unwrap();
        let cfg = home.path().join("config.toml");
        let absent = home.path().join("media").join("STICK");
        config_at(&cfg, Some(&absent));
        let mut mirror = MirrorLog::new(cfg, "test.log");
        mirror.write_event(b"nowhere\n");
        assert!(mirror.active_path().is_none());
        assert!(!home.path().join("media").exists());
    }
}
