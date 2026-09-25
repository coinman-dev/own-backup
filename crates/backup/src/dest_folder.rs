//! The destination folder of a copy, created before anything else runs.
//!
//! On 2026-09-25 a scheduled run died with «I/O error: Устройство не готово.
//! (os error 21)» and nothing more. The backup drive had changed its letter,
//! F: now belonged to an empty DVD drive, and the message named neither the
//! folder nor the drive — the person had to go and find out which "device"
//! was meant. Creating the folder is the first thing a copy does, so this is
//! where "the destination is not there" gets said in words that point at it.

use std::path::Path;

use own_backup_common::{OwnBackupError, Result};

/// Create `dir` and its parents, or fail with an error that names the folder
/// and, when it can tell, why the drive under it is not usable.
pub fn create(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(|e| OwnBackupError::Io(describe(dir, &e)))
}

/// `ERROR_NOT_READY`: a drive letter with nothing behind it that can be read —
/// an optical or card-reader slot with no media, a disk that went away.
const ERROR_NOT_READY: i32 = 21;

fn describe(dir: &Path, e: &std::io::Error) -> std::io::Error {
    let hint = match drive_letter(dir) {
        Some(d) if e.raw_os_error() == Some(ERROR_NOT_READY) => format!(
            " Диск {d}: не готов — в нём нет носителя, или у диска назначения сменилась буква. \
             Проверьте место назначения в параметрах задания."
        ),
        Some(d) if !Path::new(&format!("{d}:\\")).exists() => format!(
            " Диска {d}: нет — он отключён, или у него сменилась буква. \
             Проверьте место назначения в параметрах задания."
        ),
        _ => String::new(),
    };
    std::io::Error::new(
        e.kind(),
        format!("папка назначения {} недоступна: {e}.{hint}", dir.display()),
    )
}

/// The drive letter a Windows path starts with (`F:\…`, `\\?\F:\…`), if any.
/// Parsed from the text rather than `Path::components`, which knows drive
/// prefixes only when compiled for Windows — this has to be testable on Linux.
fn drive_letter(dir: &Path) -> Option<char> {
    let s = dir.to_string_lossy();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    let mut chars = s.chars();
    match (chars.next(), chars.next()) {
        (Some(c), Some(':')) if c.is_ascii_alphabetic() => Some(c.to_ascii_uppercase()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_the_folder() {
        let e = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "Отказано в доступе");
        let msg = describe(Path::new("/srv/backup/job"), &e).to_string();
        assert!(msg.contains("/srv/backup/job"), "{msg}");
        assert!(msg.contains("Отказано в доступе"), "{msg}");
        // Not a drive letter, so nothing is guessed about a drive.
        assert!(!msg.contains("Диск"), "{msg}");
    }

    #[test]
    fn not_ready_drive_is_explained() {
        let e = std::io::Error::from_raw_os_error(ERROR_NOT_READY);
        let msg = describe(Path::new(r"F:\Backup\Disk_C_Win11"), &e).to_string();
        assert!(msg.contains(r"F:\Backup\Disk_C_Win11"), "{msg}");
        assert!(msg.contains("Диск F: не готов"), "{msg}");
        assert!(msg.contains("сменилась буква"), "{msg}");
    }

    #[test]
    fn missing_drive_is_explained() {
        // No `Q:\` exists on the machines this runs on (and none can on Linux).
        let e = std::io::Error::from(std::io::ErrorKind::NotFound);
        let msg = describe(Path::new(r"Q:\Backup\x"), &e).to_string();
        assert!(msg.contains("Диска Q: нет"), "{msg}");
    }

    #[test]
    fn drive_letter_forms() {
        assert_eq!(drive_letter(Path::new(r"f:\x")), Some('F'));
        assert_eq!(drive_letter(Path::new(r"\\?\G:\Backup")), Some('G'));
        assert_eq!(drive_letter(Path::new(r"\\server\share\x")), None);
        assert_eq!(drive_letter(Path::new("/mnt/f")), None);
    }
}
