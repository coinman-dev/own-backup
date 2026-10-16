//! Am I running from the boot medium, or from an installed copy?
//!
//! The answer changes several decisions at once, and every one of them is wrong
//! in the other environment:
//!
//! * **Where state goes.** The installed program keeps it in
//!   `C:\ProgramData\OwnBackup`. On the medium `C:` is *the machine being backed
//!   up* — writing there means the rescue tool modifies the very disk the user
//!   booted the medium to save. State belongs on the RAM disk instead.
//! * **The service.** On the medium it runs as a plain process, and there is
//!   nowhere to install it to: WinPE's registry lives in RAM and is gone at
//!   power-off. So "the service is not installed" is not a warning there, it is
//!   the design.
//! * **Snapshots.** WinPE carries no Volume Shadow Copy Service, and it does not
//!   need one: nothing is running off the volume being read, so the copy is
//!   already consistent. See [`crate::media_mode`] users in the backup engine.
//! * **Schedules, autostart, notifications.** All of them describe a machine
//!   that keeps running. The medium is booted for one job and shut down.
//!
//! Detection is deliberately not clever. `--media` (or `OWN_BACKUP_MEDIA=1`)
//! wins, because a human said so. Otherwise we look for the marker Microsoft
//! documents for exactly this question: WinPE, and only WinPE, has the registry
//! key `HKLM\SYSTEM\CurrentControlSet\Control\MiniNT`. It is checked once and
//! remembered — the answer cannot change while the process lives.

use std::sync::OnceLock;

static MEDIA_MODE: OnceLock<bool> = OnceLock::new();

/// Environment variable form of `--media`, so a launcher can set it for every
/// program on the medium at once and child processes inherit it.
pub const MEDIA_ENV: &str = "OWN_BACKUP_MEDIA";

/// Force media mode on (or off) before anything asks. Call this from `main`
/// when the command line says `--media`; the first answer given out is final,
/// so this has no effect once [`is_media_mode`] has been consulted.
pub fn set_media_mode(on: bool) {
    let _ = MEDIA_MODE.set(on);
    if on {
        // Children (a service spawned by the GUI, a CLI called by either) must
        // reach the same conclusion without being passed the flag.
        std::env::set_var(MEDIA_ENV, "1");
    }
}

/// This machine's name, when it says something about the machine.
///
/// WinPE names every computer `MINWINPC`, so on the boot medium "the copy was
/// taken on OBK-LAB, this is MINWINPC" was said over every restore of a
/// machine's own boot entries onto that same machine (2026-10-16). There, no
/// name is the honest answer, and the comparison is not made.
pub fn meaningful_machine_name() -> Option<String> {
    if is_media_mode() {
        return None;
    }
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty() && !n.eq_ignore_ascii_case("MINWINPC"))
}

/// True when this process runs from the boot medium.
pub fn is_media_mode() -> bool {
    *MEDIA_MODE.get_or_init(detect)
}

fn detect() -> bool {
    if let Ok(v) = std::env::var(MEDIA_ENV) {
        let v = v.trim();
        if !v.is_empty() {
            return !matches!(v, "0" | "false" | "no");
        }
    }
    is_windows_pe()
}

/// The documented WinPE marker: the `MiniNT` control key. Present in WinPE and
/// in nothing else, including a normal Windows booted from a USB disk.
#[cfg(target_os = "windows")]
pub fn is_windows_pe() -> bool {
    use windows::core::w;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
    };

    // SAFETY: the key handle is closed on the one path that opens it; the key
    // path is a static wide literal.
    unsafe {
        let mut key = HKEY::default();
        let rc = RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            w!(r"SYSTEM\CurrentControlSet\Control\MiniNT"),
            0,
            KEY_READ,
            &mut key,
        );
        if rc == ERROR_SUCCESS {
            let _ = RegCloseKey(key);
            return true;
        }
    }
    false
}

/// There is no Linux equivalent of WinPE here — a Linux rescue system would be
/// told with `--media`.
#[cfg(not(target_os = "windows"))]
pub fn is_windows_pe() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The env override is the only part testable off Windows, and it is the
    /// part the medium's launcher relies on.
    #[test]
    fn env_override_is_read_and_negatable() {
        // detect() is called directly: is_media_mode() caches, and a cached
        // answer from another test would make this one meaningless.
        std::env::set_var(MEDIA_ENV, "1");
        assert!(detect());
        std::env::set_var(MEDIA_ENV, "0");
        assert!(!detect());
        std::env::set_var(MEDIA_ENV, "  ");
        assert!(
            !detect(),
            "blank must fall through to detection, not to true"
        );
        std::env::remove_var(MEDIA_ENV);
    }
}
