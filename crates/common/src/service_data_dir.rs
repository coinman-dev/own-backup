//! Where the running service keeps its data, for the root CLI on the same
//! machine.
//!
//! The service may run with a `--data-dir` of its own, and the CLI started by
//! root does not know it: a copy it makes would look for the service's key
//! directory (decision 6/E4) only in the configured place. The service writes
//! the directory into its runtime directory at start (`/run/own-backup/data-dir`,
//! root-owned, not writable by anyone else), and the CLI reads it from there.

#![cfg(target_os = "linux")]

use std::io::Write;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// The file, in the runtime directory, that names the service's data
/// directory.
pub const FILE_NAME: &str = "data-dir";

/// Write `data_dir` into `runtime_dir` for the CLI: a new file renamed over
/// the old one, 0644, so a reader never sees half of it.
pub fn publish(runtime_dir: &Path, data_dir: &Path) -> std::io::Result<()> {
    let data_dir = std::fs::canonicalize(data_dir)?;
    let tmp = runtime_dir.join(format!(".{FILE_NAME}.{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let written = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&tmp)?;
        f.write_all(data_dir.as_os_str().as_bytes())?;
        f.write_all(b"\n")?;
        f.sync_all()?;
        std::fs::rename(&tmp, runtime_dir.join(FILE_NAME))
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// The data directory the running service published, if there is one this
/// process may believe: the file and its directory owned by root (or by
/// whoever runs this, for the tests), neither writable by group or others,
/// and the path absolute. Anything else is no answer.
pub fn published() -> Option<PathBuf> {
    read(Path::new(crate::UNIX_RUNTIME_DIR))
}

/// [`published`] from `runtime_dir`.
pub fn read(runtime_dir: &Path) -> Option<PathBuf> {
    // SAFETY: geteuid has no preconditions.
    let me = unsafe { libc::geteuid() };
    let trusted = |m: &std::fs::Metadata| (m.uid() == 0 || m.uid() == me) && m.mode() & 0o022 == 0;
    let dir = std::fs::symlink_metadata(runtime_dir).ok()?;
    if !dir.is_dir() || !trusted(&dir) {
        return None;
    }
    let path = runtime_dir.join(FILE_NAME);
    let file = std::fs::symlink_metadata(&path).ok()?;
    if !file.is_file() || !trusted(&file) || file.len() > 4096 {
        return None;
    }
    let mut bytes = std::fs::read(&path).ok()?;
    while bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    let dir = PathBuf::from(std::ffi::OsString::from_vec(bytes));
    dir.is_absolute().then_some(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A runtime directory as the service makes it: 0750. Set explicitly —
    /// a tempdir follows the umask.
    fn runtime() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o750)).unwrap();
        dir
    }

    #[test]
    fn the_published_data_directory_is_read_back() {
        let run = runtime();
        let data = tempfile::tempdir().unwrap();
        assert_eq!(read(run.path()), None, "nothing published yet");
        publish(run.path(), data.path()).unwrap();
        let file = run.path().join(FILE_NAME);
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode & 0o022, 0, "{mode:o}");
        assert_eq!(
            read(run.path()),
            Some(std::fs::canonicalize(data.path()).unwrap())
        );
        // Published again (the next start): replaced, nothing left beside it.
        publish(run.path(), data.path()).unwrap();
        let names: Vec<_> = std::fs::read_dir(run.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from(FILE_NAME)]);
    }

    #[test]
    fn what_others_could_have_written_is_not_believed() {
        let run = runtime();
        let file = run.path().join(FILE_NAME);
        std::fs::write(&file, "/srv/data\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(read(run.path()), Some(PathBuf::from("/srv/data")));
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert_eq!(read(run.path()), None, "a file others may write");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::set_permissions(run.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(read(run.path()), None, "a directory others may write");
        std::fs::set_permissions(run.path(), std::fs::Permissions::from_mode(0o750)).unwrap();
        std::fs::write(&file, "relative/path\n").unwrap();
        assert_eq!(read(run.path()), None, "not an absolute path");
        std::fs::remove_file(&file).unwrap();
        std::os::unix::fs::symlink("/etc/hostname", &file).unwrap();
        assert_eq!(read(run.path()), None, "a symlink in its place");
    }
}
