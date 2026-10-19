//! Linux: one window and one tray icon per person, and a way to reach them.
//!
//! Windows does this with named mutexes and by finding the window. Here each
//! of the two holds an `flock` on a file in the person's runtime directory
//! (`$XDG_RUNTIME_DIR/own-backup`, private to them and gone at logout) and
//! listens on a socket beside it. A second window started from the menu tells
//! the first to come forward instead of answering «already running»; the
//! settings page tells the icon to go; the icon tells the window to show.
//!
//! The lock decides who owns the socket, so a socket left behind by a killed
//! process is removed by the next owner and never by a second copy. Nothing
//! here is shared with the service or with other people: two people logged in
//! each have their own window and their own icon.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::Duration;

/// This process owns the name, and keeps it while the value lives.
pub struct Instance {
    name: &'static str,
    lock: File,
}

pub enum Claim {
    Ours(Instance),
    /// Another process of this person holds it.
    Taken,
}

/// `$XDG_RUNTIME_DIR/own-backup`, made private if it is not there. Without
/// a runtime directory (a bare `su` shell), a folder of this user's own under
/// the temporary directory — one that someone else made first is refused.
pub fn runtime_dir() -> std::io::Result<PathBuf> {
    // SAFETY: no arguments, cannot fail.
    let uid = unsafe { libc::getuid() };
    let dir = match std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        Some(base) => PathBuf::from(base).join("own-backup"),
        None => std::env::temp_dir().join(format!("own-backup-{uid}")),
    };
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let meta = std::fs::symlink_metadata(&dir)?;
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{} is not a private folder of this user", dir.display()),
        ));
    }
    Ok(dir)
}

fn lock_path(name: &str) -> std::io::Result<PathBuf> {
    Ok(runtime_dir()?.join(format!("{name}.lock")))
}

fn socket_path(name: &str) -> std::io::Result<PathBuf> {
    Ok(runtime_dir()?.join(format!("{name}.sock")))
}

/// Take `name` (`gui`, `tray`) for this process, or learn that it is taken.
pub fn claim(name: &'static str) -> std::io::Result<Claim> {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC)
        .open(lock_path(name)?)?;
    // SAFETY: a descriptor this function owns.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = std::io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::EWOULDBLOCK) => Ok(Claim::Taken),
            _ => Err(error),
        };
    }
    Ok(Claim::Ours(Instance { name, lock }))
}

impl Instance {
    /// Answer messages for the rest of the process's life, each one handed to
    /// `on_message` on a thread of its own. The lock is kept by that thread;
    /// a socket that cannot be made leaves the lock held all the same, so a
    /// second copy still finds the name taken.
    pub fn listen(self, on_message: impl Fn(&str) + Send + 'static) {
        let listener = socket_path(self.name).and_then(|path| {
            // Ours by the lock: whatever is there was left by a dead owner.
            let _ = std::fs::remove_file(&path);
            UnixListener::bind(&path)
        });
        let listener = match listener {
            Ok(listener) => listener,
            Err(error) => {
                tracing::warn!(%error, name = self.name, "instance: no socket, nobody can reach this one");
                std::mem::forget(self.lock);
                return;
            }
        };
        let name = self.name;
        let spawned = std::thread::Builder::new()
            .name(format!("instance-{name}"))
            .spawn(move || {
                let _lock = self.lock;
                for stream in listener.incoming().flatten() {
                    if let Some(message) = read_message(stream) {
                        tracing::debug!(name, message, "instance: message");
                        on_message(&message);
                    }
                }
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, name, "instance: listener thread did not start");
        }
    }
}

fn read_message(stream: UnixStream) -> Option<String> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut text = String::new();
    stream.take(64).read_to_string(&mut text).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// Hand `message` to whoever holds `name`. `false` when nobody listens —
/// not running, or still starting.
pub fn tell(name: &str, message: &str) -> bool {
    let Ok(path) = socket_path(name) else {
        return false;
    };
    let Ok(mut stream) = UnixStream::connect(path) else {
        return false;
    };
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    stream.write_all(message.as_bytes()).is_ok()
}

/// Whether someone holds `name` right now, without taking it.
pub fn held(name: &str) -> bool {
    let Ok(path) = lock_path(name) else {
        return false;
    };
    let Ok(file) = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(path)
    else {
        return false;
    };
    // SAFETY: a descriptor this function owns; a shared lock taken here is
    // released when the file closes at the end of the function.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) != 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// Every test that changes `XDG_RUNTIME_DIR` holds this.
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn the_second_claim_finds_it_taken_and_reaches_the_first() {
        let _env = ENV.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_RUNTIME_DIR", dir.path());

        let Claim::Ours(first) = claim("test-a").unwrap() else {
            panic!("free name taken");
        };
        assert!(held("test-a"));
        assert!(matches!(claim("test-a").unwrap(), Claim::Taken));
        assert!(!tell("test-a", "show"), "nobody listens yet");

        let (sent, got) = mpsc::channel();
        first.listen(move |m| sent.send(m.to_string()).unwrap());
        assert!(tell("test-a", "show\n"));
        assert_eq!(got.recv_timeout(Duration::from_secs(5)).unwrap(), "show");
        // Still taken: the listener keeps the lock.
        assert!(matches!(claim("test-a").unwrap(), Claim::Taken));
        let mode = std::fs::metadata(dir.path().join("own-backup"))
            .unwrap()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn a_dead_owner_leaves_the_name_free() {
        let _env = ENV.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_RUNTIME_DIR", dir.path());
        match claim("test-b").unwrap() {
            Claim::Ours(instance) => drop(instance),
            Claim::Taken => panic!("free name taken"),
        }
        assert!(!held("test-b"));
        assert!(matches!(claim("test-b").unwrap(), Claim::Ours(_)));
    }

    #[test]
    fn someone_elses_open_folder_is_not_used() {
        let _env = ENV.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("own-backup");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::os::unix::fs::PermissionsExt::from_mode(0o777))
            .unwrap();
        std::env::set_var("XDG_RUNTIME_DIR", dir.path());
        assert!(runtime_dir().is_err());
    }
}
