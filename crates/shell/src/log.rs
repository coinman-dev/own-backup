//! Where the shell's own lines go, and why there is more than one place.
//!
//! Everything beside the exe on the medium is on a RAM disk: a session that
//! ended in a dark screen, a reboot or a machine that had to be switched off at
//! the wall took that disk with it, and the lines explaining why are the ones
//! that went missing. So the medium's launcher names a directory on the stick
//! it booted from ([`own_backup_common::log_mirror::LOG_MIRROR_ENV`]), and the
//! log is written to BOTH — the stick because it survives, the RAM disk because
//! it is where anyone looking at a running medium will think to look.
//!
//! The stick first, in the order below: if only one write is going to happen
//! before the power goes, it should be the one that can be read afterwards.

use std::path::{Path, PathBuf};

/// The file, in whichever directory it ends up in.
pub const LOG_NAME: &str = "own-backup-shell.log";

/// Every place one line should be written to, most durable first.
///
/// `mirror` is what the medium's launcher named, `beside_exe` is our own
/// folder, and `temp` is only reached when there is no answer to either —
/// a log nobody will find is still better than no log.
pub fn log_paths(mirror: Option<&str>, beside_exe: Option<&Path>, temp: &Path) -> Vec<PathBuf> {
    let mut places = Vec::new();
    if let Some(dir) = mirror.map(str::trim).filter(|d| !d.is_empty()) {
        places.push(Path::new(dir).join(LOG_NAME));
    }
    if let Some(dir) = beside_exe {
        places.push(dir.join(LOG_NAME));
    }
    if places.is_empty() {
        places.push(temp.join(LOG_NAME));
    }
    places
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(mirror: Option<&str>, exe: Option<&str>) -> Vec<String> {
        log_paths(mirror, exe.map(Path::new), Path::new("/tmp"))
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect()
    }

    /// Both, and the one that survives the machine first.
    #[test]
    fn the_medium_writes_to_the_stick_and_to_itself() {
        let written = paths(
            Some("E:/OwnBackup/logs"),
            Some("X:/Program Files/OwnBackup"),
        );
        assert_eq!(
            written,
            vec![
                format!("E:/OwnBackup/logs/{LOG_NAME}"),
                format!("X:/Program Files/OwnBackup/{LOG_NAME}"),
            ]
        );
    }

    #[test]
    fn an_installed_copy_just_writes_beside_itself() {
        assert_eq!(
            paths(None, Some("D:/OwnBackup")),
            vec![format!("D:/OwnBackup/{LOG_NAME}")]
        );
    }

    /// The launcher sets the variable to an empty string when it found no
    /// writable medium, and an empty string is not a directory.
    #[test]
    fn a_mirror_that_was_not_found_is_not_a_place() {
        assert_eq!(paths(Some(""), Some("X:/app")).len(), 1);
        assert_eq!(paths(Some("   "), Some("X:/app")).len(), 1);
    }

    #[test]
    fn with_nowhere_else_to_go_it_still_goes_somewhere() {
        assert_eq!(paths(None, None), vec![format!("/tmp/{LOG_NAME}")]);
    }
}
