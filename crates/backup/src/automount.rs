//! Suppress automatic drive-letter assignment while a backup image is mounted
//! for the true-removal delete pass.
//!
//! The delete pass has to bring the mounted image's disk online so the
//! filesystem driver can delete the excluded files — but Windows' MountManager
//! then auto-assigns a drive letter to every surfacing volume, so a letter
//! briefly flashes in Explorer even though nothing user-facing happened. We
//! reach the volume through its `\\?\Volume{GUID}\` path, which needs no drive
//! letter at all, so we turn auto-mounting of new volumes OFF for the short
//! mount window and turn it back on afterwards.
//!
//! `mountvol /N` / `/E` is used rather than the `IOCTL_MOUNTMGR_*_AUTO_MOUNT`
//! ioctls: the query ioctl was observed to fail on a real host (2026-07-11),
//! leaving the guard a no-op and a letter still appearing. `mountvol.exe` is a
//! system tool that exists in WinPE too, so it stays; reading the setting goes
//! through the registry ([`is_disabled`]).
//!
//! **No true-removal path uses the guard any more (2026-07-30).** `mountvol /N`
//! does not merely withhold a letter: mountmgr holds every *new* volume OFFLINE,
//! and an offline volume answers `ERROR_NOT_READY` (0x80070015) to every open
//! without a single byte of I/O reaching the disk. The VHDX path learned that
//! on 2026-07-12 (180 s device-not-ready, nothing deleted); the qcow2/AIM path
//! was believed to be immune until a VMware guest reproduced exactly the same
//! three-minute freeze on 2026-07-30. Both paths now let the machine's setting
//! stand, strip the letter as soon as it appears
//! ([`crate::volume_win`]'s `strip_drive_letters`) and, when a volume does come
//! up offline anyway — the setting is global and may be the user's own — mount
//! it explicitly with `bring_volume_online`. [`is_disabled`] reports the
//! machine's setting for the log so that state is never a guess again.
//!
//! [`AutoMountGuard`] is kept for callers that legitimately want no letter at
//! all (mounting an image for browsing), and is RAII and best-effort:
//! `disable()` turns auto-mount off,
//! `Drop` turns it back on (retried, loudly logged on failure — a stuck
//! "disabled" would stop the user's own USB drives from getting letters until
//! they run `mountvol /E`). Any failure yields a no-op/loud guard; the delete
//! pass still works via the volume GUID path regardless.

#![cfg(target_os = "windows")]

use std::os::windows::process::CommandExt;
use std::process::Command;

use tracing::{debug, info, warn};

/// `CREATE_NO_WINDOW` — keep the helper `mountvol.exe` invisible.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Puts the machine's auto-mount setting back when dropped.
#[must_use = "the setting is put back when the guard is dropped"]
pub struct AutoMountGuard {
    /// `mountvol` flag to run on drop (`/E` or `/N`), or `None` for a guard
    /// that changed nothing.
    restore: Option<&'static str>,
}

impl AutoMountGuard {
    /// Turn auto-mount OFF so a surfacing volume gets no drive letter — and no
    /// mounted filesystem either, which is why the true-removal paths do not
    /// use this (see the module docs). Restores the previous state on drop.
    pub fn disable() -> Self {
        let previous = is_disabled();
        if run_mountvol("/N") {
            debug!("auto-mount disabled (mountvol /N)");
            Self { restore: (previous != Some(true)).then_some("/E") }
        } else {
            warn!("could not disable auto-mount (mountvol /N) — a drive letter may appear briefly");
            Self { restore: None }
        }
    }

    /// Turn auto-mount ON for the duration when — and only when — the machine
    /// has it off, restoring "off" on drop.
    ///
    /// With auto-mount disabled, mountmgr keeps every arriving volume offline:
    /// the image we just attached to delete the excluded files from would
    /// answer `ERROR_NOT_READY` to every open and the pass would delete nothing.
    /// The setting is machine-global, so it may be the admin's deliberate
    /// choice — hence a window as short as the mount, and the original state
    /// back afterwards.
    pub fn enable_if_disabled() -> Self {
        if is_disabled() != Some(true) {
            return Self { restore: None };
        }
        if run_mountvol("/E") {
            info!(
                "auto-mount was disabled on this machine — enabled for the mount window so the \
                 image's volumes can mount (restored on the way out)"
            );
            Self { restore: Some("/N") }
        } else {
            warn!(
                "auto-mount is disabled on this machine and could not be enabled (mountvol /E) — \
                 the image's volumes may refuse to mount"
            );
            Self { restore: None }
        }
    }
}

impl Drop for AutoMountGuard {
    fn drop(&mut self) {
        let Some(flag) = self.restore else { return };
        // Restoring matters both ways: a stuck "disabled" stops the user's own
        // new volumes from getting letters, a stuck "enabled" undoes a policy
        // they chose. Retry, and make failure impossible to miss in the log.
        for attempt in 1..=3 {
            if run_mountvol(flag) {
                debug!(flag, "auto-mount setting restored");
                return;
            }
            warn!(attempt, flag, "retrying mountvol");
        }
        warn!(flag, "FAILED to restore the auto-mount setting — run `mountvol {flag}` manually");
    }
}

/// Is automatic mounting of new volumes currently OFF on this machine?
///
/// `Some(true)` means new volumes surface OFFLINE: their device enumerates but
/// every open fails with `ERROR_NOT_READY` until something onlines them
/// explicitly (see `volume_win::bring_volume_online`). The setting is global and
/// persistent — it can be the user's own, or the leftover of a run that died
/// with the guard still applied — so the true-removal pass logs it whenever a
/// mounted image refuses to come up. `None` = could not be read.
///
/// `mountvol` has no query, so the value mountmgr itself reads is consulted
/// directly. An absent value is the default: auto-mount enabled.
pub fn is_disabled() -> Option<bool> {
    // The value mountmgr itself reads. `reg query` would do as well and exists
    // in WinPE, but a process launch to read one DWORD is a process launch.
    const MOUNTMGR: &str = r"SYSTEM\CurrentControlSet\Services\mountmgr";
    match crate::registry_win::read_dword_hklm(MOUNTMGR, "NoAutoMount") {
        Some(v) => Some(v != 0),
        // Absent = mountmgr's default, which is auto-mount enabled.
        None => Some(false),
    }
}

/// Run `mountvol <flag>`; `true` on success. `/N` disables, `/E` enables
/// automatic mounting of new basic volumes.
fn run_mountvol(flag: &str) -> bool {
    match Command::new("mountvol")
        .arg(flag)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(o) if o.status.success() => true,
        Ok(o) => {
            warn!(
                flag,
                stderr = %String::from_utf8_lossy(&o.stderr).trim(),
                "mountvol failed"
            );
            false
        }
        Err(e) => {
            warn!(flag, error = %e, "failed to launch mountvol");
            false
        }
    }
}
