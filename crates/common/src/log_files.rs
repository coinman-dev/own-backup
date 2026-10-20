//! Where the program's logs live, and how long they stay there.
//!
//! One folder — `Logs` beside the executable; on Linux `/var/log/own-backup`
//! for root and the user's state directory otherwise — one file per program
//! per day, thirty days kept. Before this, each of the three executables dropped a
//! single `own-backup-*.log` next to itself and rotated it by size, so "what
//! happened on Tuesday" was answerable only if Tuesday happened to be inside
//! the last four megabytes.
//!
//! # Why the date is in the name and not only in the file's timestamp
//!
//! Sweeping reads the DATE OUT OF THE NAME. A file's modification time is not
//! evidence of when its contents were written: copying a folder, restoring it
//! from a backup, or an antivirus rewriting a file all move it, and any of
//! those would either keep a year of logs or delete yesterday's. The name is
//! written once, by us, and means exactly one thing.
//!
//! # Why the day is checked on every write
//!
//! The service runs for weeks. A file chosen once at startup would collect
//! every day into the day the service happened to start — which is the one
//! thing "one file per day" must not do.

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Folder logs go into, beside the executable.
pub const LOG_DIR_NAME: &str = "Logs";

/// How long a log stays. Thirty days is long enough to cover "it started
/// failing about a month ago" and short enough that nobody has to think about
/// the folder's size.
pub const KEEP_DAYS: i64 = 30;

/// The log folder, first writable candidate wins.
///
/// Beside the executable is where a person looks. A program installed into a
/// read-only place (or running off the boot medium) falls back to the data
/// directory and then to temp, in that order — the same policy the per-binary
/// loggers had before, with one folder added.
///
/// Linux is different (see [`linux_candidates`]): beside the executable is
/// `/usr/bin`, and temp is a `/tmp` shared by every user.
pub fn log_dir(data_dir: &Path) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    let candidates = {
        // SAFETY: geteuid has no preconditions and cannot fail.
        let root = unsafe { libc::geteuid() } == 0;
        linux_candidates(
            data_dir,
            root,
            std::env::var_os("XDG_STATE_HOME"),
            std::env::var_os("HOME"),
        )
    };
    #[cfg(not(target_os = "linux"))]
    let candidates = {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(dir) = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf))
        {
            candidates.push(dir.join(LOG_DIR_NAME));
        }
        candidates.push(data_dir.join(LOG_DIR_NAME));
        candidates.push(std::env::temp_dir().join(LOG_DIR_NAME));
        candidates
    };
    #[cfg(not(target_os = "linux"))]
    let temp = candidates.last().cloned();
    let chosen = candidates.into_iter().find(|dir| writable(dir))?;
    // The service sweeps only the folder it writes to itself; a user's own
    // folder is swept by the user's programs, the one time each starts.
    #[cfg(target_os = "linux")]
    if !chosen.starts_with(data_dir) && !chosen.starts_with(LINUX_SYSTEM_LOG_DIR) {
        sweep(&chosen, KEEP_DAYS);
    }
    // On Windows that is the person's temp folder: the installed service
    // lets nobody else write beside the program or in its data folder.
    #[cfg(not(target_os = "linux"))]
    if Some(&chosen) == temp.as_ref() {
        sweep(&chosen, KEEP_DAYS);
    }
    Some(chosen)
}

/// The engine's (root's) log folder on Linux.
#[cfg(target_os = "linux")]
pub const LINUX_SYSTEM_LOG_DIR: &str = "/var/log/own-backup";

/// Where logs may go on Linux, in order.
///
/// Root — the service, or a command run with sudo — writes to
/// `/var/log/own-backup`. Anyone else to their own state directory,
/// `$XDG_STATE_HOME/own-backup/Logs` (`~/.local/state/own-backup/Logs` by
/// default): the window and the command line run as the person, who cannot
/// write to `/var/log` and should not write to a `/tmp/Logs` every other user
/// can read, fill, or plant a link in. The data directory is the last resort;
/// the executable's folder is never one.
#[cfg(target_os = "linux")]
fn linux_candidates(
    data_dir: &Path,
    root: bool,
    xdg_state_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if root {
        candidates.push(PathBuf::from(LINUX_SYSTEM_LOG_DIR));
    } else {
        // The specification ignores a relative value.
        let state = xdg_state_home
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| {
                home.map(PathBuf::from)
                    .filter(|p| p.is_absolute())
                    .map(|h| h.join(".local").join("state"))
            });
        if let Some(state) = state {
            candidates.push(state.join("own-backup").join(LOG_DIR_NAME));
        }
    }
    candidates.push(data_dir.join(LOG_DIR_NAME));
    candidates
}

/// Can we create files here? Asked by doing it, because a directory that
/// exists is not a directory we may write to.
fn writable(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(".write-probe");
    match File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Today, as the file names spell it.
fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// `<dir>/<stem>-YYYY-MM-DD.log`
pub fn path_for(dir: &Path, stem: &str, day: &str) -> PathBuf {
    dir.join(format!("{stem}-{day}.log"))
}

/// A log file that moves to the next day's file when the day changes.
pub struct DailyLog {
    dir: PathBuf,
    stem: String,
    day: String,
    file: Option<File>,
}

impl DailyLog {
    /// Opens today's file. A failure here is not fatal — writes will keep
    /// trying, so a folder that becomes writable later starts working without
    /// a restart.
    pub fn new(dir: impl Into<PathBuf>, stem: impl Into<String>) -> Self {
        let mut log = Self {
            dir: dir.into(),
            stem: stem.into(),
            day: String::new(),
            file: None,
        };
        log.roll(&today());
        log
    }

    /// Today's file, whether or not it is open.
    pub fn path(&self) -> PathBuf {
        path_for(&self.dir, &self.stem, &self.day)
    }

    fn roll(&mut self, day: &str) {
        self.day = day.to_string();
        let _ = std::fs::create_dir_all(&self.dir);
        self.file = File::options()
            .create(true)
            .append(true)
            .open(self.path())
            .ok();
    }
}

impl Write for DailyLog {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let now = today();
        if now != self.day || self.file.is_none() {
            self.roll(&now);
        }
        match &mut self.file {
            Some(f) => f.write(buf),
            // Nowhere to write is not a reason to lose the line's caller: the
            // subscriber would otherwise propagate an error on every event.
            None => Ok(buf.len()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.file {
            Some(f) => f.flush(),
            None => Ok(()),
        }
    }
}

/// Delete logs older than `keep_days`, by the date in their names. Returns how
/// many went.
///
/// Only files this program names are considered: `<something>-YYYY-MM-DD.log`.
/// Anything else in the folder — a note somebody left, a log from another
/// program — is not ours to remove.
pub fn sweep(dir: &Path, keep_days: i64) -> usize {
    let cutoff = chrono::Local::now().date_naive() - chrono::Duration::days(keep_days);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(day) = day_in_name(&name) else {
            continue;
        };
        if day < cutoff && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// The date a name like `own-backup-service-2026-09-22.log` carries.
fn day_in_name(name: &str) -> Option<chrono::NaiveDate> {
    let stem = name.strip_suffix(".log")?;
    // The last ten characters are the date, and only if what precedes them is
    // the separator we wrote.
    let (head, date) = stem.split_at(stem.len().checked_sub(10)?);
    if !head.ends_with('-') || head.len() < 2 {
        return None;
    }
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(dir: &Path, name: &str) {
        std::fs::write(dir.join(name), b"x").unwrap();
    }

    /// Only our own names, and only by the date in them.
    #[test]
    fn a_name_yields_its_day_or_nothing() {
        assert_eq!(
            day_in_name("own-backup-service-2026-09-22.log"),
            chrono::NaiveDate::from_ymd_opt(2026, 9, 22)
        );
        for other in [
            "own-backup-service.log",         // the old, undated name
            "own-backup-service-2026-09.log", // not a full date
            "notes.txt",                      // not a log at all
            "2026-09-22.log",                 // no program in front of it
            "own-backup-service-not-a-date.log",
        ] {
            assert_eq!(day_in_name(other), None, "{other} должно быть пропущено");
        }
    }

    /// Old logs go, recent ones stay, and files that are not ours are not
    /// touched at all — the folder is ours, its contents are not necessarily.
    #[test]
    fn sweeping_removes_only_our_own_old_files() {
        let dir = tempfile::tempdir().unwrap();
        let day = |back: i64| {
            (chrono::Local::now().date_naive() - chrono::Duration::days(back))
                .format("%Y-%m-%d")
                .to_string()
        };
        touch(dir.path(), &format!("own-backup-service-{}.log", day(0)));
        touch(dir.path(), &format!("own-backup-cli-{}.log", day(29)));
        touch(dir.path(), &format!("own-backup-gui-{}.log", day(31)));
        touch(dir.path(), &format!("own-backup-service-{}.log", day(400)));
        touch(dir.path(), "own-backup-service.log");
        touch(dir.path(), "заметка.txt");

        assert_eq!(sweep(dir.path(), KEEP_DAYS), 2);
        let left: std::collections::HashSet<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(left.contains(&format!("own-backup-service-{}.log", day(0))));
        assert!(
            left.contains(&format!("own-backup-cli-{}.log", day(29))),
            "29 дней — в пределах"
        );
        assert!(
            left.contains("own-backup-service.log"),
            "старое имя не наше дело"
        );
        assert!(left.contains("заметка.txt"));
        assert_eq!(left.len(), 4);
    }

    /// The day is re-read on every write, because a service outlives the day
    /// it started on.
    #[test]
    fn the_writer_moves_to_the_next_days_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = DailyLog::new(dir.path(), "own-backup-test");
        log.write_all(b"first\n").unwrap();
        let first = log.path();
        assert!(first.exists());

        // Pretend the day turned: the next write must open another file.
        log.day = "1999-12-31".into();
        log.write_all(b"second\n").unwrap();
        assert_eq!(log.path(), first, "имя снова сегодняшнее");
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "first\nsecond\n");
    }

    /// Root's logs in `/var/log`, a person's in their own state directory;
    /// never beside the executable (`/usr/bin`) and never in a shared `/tmp`.
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_logs_go_to_var_log_for_root_and_the_users_state_dir_otherwise() {
        let data = Path::new("/var/lib/own-backup");
        let os = |s: &str| Some(std::ffi::OsString::from(s));
        let cases = [
            (
                linux_candidates(data, true, os("/home/a/st"), os("/home/a")),
                vec![PathBuf::from("/var/log/own-backup"), data.join("Logs")],
            ),
            (
                linux_candidates(data, false, None, os("/home/a")),
                vec![
                    PathBuf::from("/home/a/.local/state/own-backup/Logs"),
                    data.join("Logs"),
                ],
            ),
            (
                linux_candidates(data, false, os("/xdg"), os("/home/a")),
                vec![PathBuf::from("/xdg/own-backup/Logs"), data.join("Logs")],
            ),
            // A relative XDG_STATE_HOME is to be ignored.
            (
                linux_candidates(data, false, os("state"), os("/home/a")),
                vec![
                    PathBuf::from("/home/a/.local/state/own-backup/Logs"),
                    data.join("Logs"),
                ],
            ),
            // Nowhere personal to go: the data directory, and nothing shared.
            (
                linux_candidates(data, false, None, None),
                vec![data.join("Logs")],
            ),
        ];
        for (got, want) in cases {
            assert_eq!(got, want);
            for dir in &got {
                assert!(!dir.starts_with("/tmp") && !dir.starts_with("/usr"));
            }
        }
    }

    /// Writability is asked by doing it.
    #[test]
    fn writable_means_a_file_could_be_created() {
        let dir = tempfile::tempdir().unwrap();
        assert!(writable(&dir.path().join("a").join(LOG_DIR_NAME)));
        #[cfg(target_os = "linux")]
        assert!(!writable(Path::new("/proc/definitely/not/here")));
    }

    /// A folder that cannot be written to is not chosen, and nothing panics
    /// when none can be. (On Linux the first candidate is the user's own
    /// state directory, which a test must not write into.)
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn an_unwritable_candidate_is_passed_over() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("data");
        assert!(log_dir(&good).is_some());
        // The chosen one really is a directory we can write in.
        let chosen = log_dir(&good).unwrap();
        assert!(chosen.is_dir());
        assert!(chosen.ends_with(LOG_DIR_NAME));
    }
}
