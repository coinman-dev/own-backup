//! One engine per machine, decided before anything is touched.
//!
//! The engine owns files the whole product depends on: the job store, the
//! activity journal, the per-boot token the GUI and CLI authenticate with, and
//! the images a running backup is writing. Two copies of it are not "a bit
//! slower", they are two schedulers enqueueing the same job and two writers
//! over one image.
//!
//! There *was* a last line of defence — the named pipe is created with
//! `first_pipe_instance(true)`, so the second process loses it. But that
//! happens at the very end of startup (audit AUD-07). By the time the second
//! copy got there it had already opened the database, loaded the job store,
//! started the worker and the scheduler, and republished the token file. The
//! first process was left serving a token nobody held any more.
//!
//! So ownership is claimed FIRST, with an OS primitive that costs nothing and
//! cannot be half-acquired:
//!
//! * **Windows** — a named mutex. `Global\` so it spans sessions: the installed
//!   service lives in session 0 and a hand-started copy would be in the user's
//!   session, where a `Local\` name would not collide at all. Creating a
//!   `Global\` object needs `SeCreateGlobalPrivilege`, which LocalSystem and
//!   Administrators have and which is exactly who may run the engine; if it is
//!   refused anyway we fall back to `Local\` rather than failing to start, and
//!   say so.
//! * **Unix** — `flock(LOCK_EX | LOCK_NB)` on a file in the data directory.
//!   The lock is released by the kernel when the process dies, including when
//!   it is killed, so a crash never leaves the engine locked out of itself.
//!
//! The guard is held for the life of the process and released by the OS.

use std::path::Path;

/// Proof that this process is the one engine. Dropping it gives ownership up,
/// so it is kept alive until the process ends.
pub struct Ownership {
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    #[cfg(unix)]
    _lock: std::fs::File,
    #[cfg(windows)]
    _mutex: windows::Win32::Foundation::HANDLE,
    /// Whether this is the machine-wide claim or only the session-wide
    /// fallback, so startup can say which one it got.
    pub machine_wide: bool,
}

/// Why ownership could not be claimed.
#[derive(Debug)]
pub enum Taken {
    /// Another engine is already running.
    AlreadyRunning,
    /// The claim itself failed (no permission, unwritable data directory).
    /// Not the same answer: refusing to start is right either way, but the
    /// reason a user has to act on is different.
    CouldNotAsk(String),
}

impl std::fmt::Display for Taken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Taken::AlreadyRunning => {
                write!(f, "{}", own_backup_common::tr!("single_instance.already_running"))
            }
            Taken::CouldNotAsk(why) => write!(
                f,
                "{}",
                own_backup_common::tr!("single_instance.could_not_check", reason = why)
            ),
        }
    }
}

impl std::error::Error for Taken {}

/// Claim sole ownership, or say who has it.
#[cfg(windows)]
pub fn claim(_data_dir: &Path) -> Result<Ownership, Taken> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;

    // Machine-wide first; the session-wide name is the fallback for a host
    // that refuses the global namespace.
    for (name, machine_wide) in [
        ("Global\\OwnBackupServiceSingleInstance\0", true),
        ("Local\\OwnBackupServiceSingleInstance\0", false),
    ] {
        let wide: Vec<u16> = name.encode_utf16().collect();
        // SAFETY: `wide` is null-terminated and outlives the call. The handle
        // is kept in the guard and released by the OS at process exit.
        let handle = unsafe { CreateMutexW(None, true, PCWSTR(wide.as_ptr())) };
        match handle {
            Ok(h) => {
                // SAFETY: read immediately after the call that set it.
                if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
                    return Err(Taken::AlreadyRunning);
                }
                return Ok(Ownership { _mutex: h, machine_wide });
            }
            // Access denied on `Global\` means no SeCreateGlobalPrivilege —
            // try the session-wide name before giving up.
            Err(_) if machine_wide => continue,
            Err(e) => return Err(Taken::CouldNotAsk(e.to_string())),
        }
    }
    unreachable!("the loop returns on both arms")
}

#[cfg(unix)]
pub fn claim(data_dir: &Path) -> Result<Ownership, Taken> {
    use std::os::unix::io::AsRawFd;

    let path = data_dir.join("own-backup-service.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| Taken::CouldNotAsk(format!("{}: {e}", path.display())))?;

    // SAFETY: a valid descriptor from the File we still own.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        return match err.raw_os_error() {
            Some(libc::EWOULDBLOCK) => Err(Taken::AlreadyRunning),
            _ => Err(Taken::CouldNotAsk(err.to_string())),
        };
    }
    Ok(Ownership { _lock: file, machine_wide: true })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The second claim in the same process must fail exactly as a second
    /// process would — that is the whole contract, and it is the one thing
    /// about this module worth pinning down without two processes.
    #[cfg(unix)]
    #[test]
    fn a_second_claim_is_refused_while_the_first_is_held() {
        let dir = tempfile::tempdir().unwrap();
        let first = claim(dir.path()).expect("the first claim owns it");
        assert!(first.machine_wide);

        // flock is per open file DESCRIPTION, so a second open in the same
        // process behaves like another process's.
        match claim(dir.path()) {
            Err(Taken::AlreadyRunning) => {}
            other => panic!("a second engine must be refused, got {other:?}"),
        }

        // And giving it up lets the next one in — a restart must work.
        drop(first);
        assert!(claim(dir.path()).is_ok(), "ownership must be released with the guard");
    }

    /// A data directory that cannot be written is a different answer from
    /// "already running": the user has to do something else about it.
    #[cfg(unix)]
    #[test]
    fn an_unusable_data_directory_is_not_reported_as_another_engine() {
        match claim(Path::new("/proc/definitely/not/a/directory")) {
            Err(Taken::CouldNotAsk(_)) => {}
            other => panic!("expected a plain failure, got {other:?}"),
        }
    }
}

impl std::fmt::Debug for Ownership {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ownership").field("machine_wide", &self.machine_wide).finish()
    }
}
