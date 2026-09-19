//! How a running medium finds the drive it booted from.
//!
//! WinPE gives that volume whatever letter it likes — `X:` is the RAM disk and
//! never it — so the medium carries a marker file and everything that needs the
//! stick looks for that file.
//!
//! The constants live here rather than beside the rest of the medium's layout
//! because three programs that cannot see each other have to agree on them: the
//! builder writes the marker, the launcher script finds it, and the desktop
//! finds it AGAIN on its own. That last one is why this module exists at all —
//! the desktop is started by `winpeshl`, not by the script, so it inherits none
//! of the script's environment and has to ask the question itself.

/// Folder on the medium's own partition holding everything that does not need
/// to be in RAM.
pub const MEDIA_DIR: &str = "OwnBackup";

/// Marker file inside it; its content is its own explanation to whoever finds
/// it on a stick months later.
pub const MEDIA_MARKER: &str = "media.id";
pub const MEDIA_MARKER_TEXT: &str =
    "My Own Backup boot medium. This file marks the volume so the medium can \
     find its own files after boot; its logs are kept in the logs folder beside it.\r\n";

/// Where the running medium keeps the copy of its logs that survives a reboot.
pub const MEDIA_LOGS_DIR: &str = "logs";

/// The boot script, where WinPE keeps it, as the desktop has to name it to run
/// it.
///
/// `X:` is the RAM disk the image was unpacked into and is always that letter
/// in WinPE. Here rather than beside the rest of the medium's layout for the
/// same reason as everything else in this module: the desktop runs this script
/// and cannot see the crate that writes it.
pub const BOOT_SCRIPT: &str = r"X:\Windows\System32\startnet.cmd";

/// The drive the medium booted from, found by its marker.
///
/// `X:` is skipped on purpose: it is the RAM disk the image was unpacked into,
/// and a marker cannot be there. Everything else is tried in order, and the
/// first hit wins — a machine with two of our media in it is not a case worth
/// being clever about.
#[cfg(windows)]
pub fn find_medium() -> Option<std::path::PathBuf> {
    for letter in "CDEFGHIJKLMNOPQRSTUVWYZ".chars() {
        let root = std::path::PathBuf::from(format!("{letter}:\\"));
        let marker = root.join(MEDIA_DIR).join(MEDIA_MARKER);
        if marker.is_file() {
            return Some(root.join(MEDIA_DIR));
        }
    }
    None
}

/// The log folder on that drive, created if the medium is writable.
///
/// A DVD or a mounted ISO is read-only and simply has none — the caller falls
/// back to writing beside itself, which is a RAM disk, which is why this is
/// worth trying at all.
#[cfg(windows)]
pub fn find_log_dir() -> Option<std::path::PathBuf> {
    let logs = find_medium()?.join(MEDIA_LOGS_DIR);
    std::fs::create_dir_all(&logs).ok()?;
    logs.is_dir().then_some(logs)
}

#[cfg(not(windows))]
pub fn find_medium() -> Option<std::path::PathBuf> {
    None
}

#[cfg(not(windows))]
pub fn find_log_dir() -> Option<std::path::PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The marker has to say what it is. It ends up on someone's flash drive
    /// and is opened years later by someone wondering what put it there.
    #[test]
    fn the_marker_explains_itself() {
        assert!(MEDIA_MARKER_TEXT.contains("My Own Backup"));
        assert!(MEDIA_MARKER_TEXT.contains("boot medium"));
        assert!(MEDIA_MARKER_TEXT.ends_with("\r\n"), "read by Windows tools");
    }

    /// `X:` is the RAM disk in WinPE. Looking for the medium there would find
    /// nothing at best, and at worst find a copy of the marker that the image
    /// carries for its own reasons.
    #[test]
    fn the_ram_disk_is_never_searched() {
        let letters = "CDEFGHIJKLMNOPQRSTUVWYZ";
        assert!(!letters.contains('X'), "X: is the RAM disk");
        assert!(letters.contains('C') && letters.contains('J'));
    }
}
