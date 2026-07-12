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
//! leaving the guard a no-op and a letter still appearing. `mountvol` is the
//! same kind of proven system-tool call as [`crate::san_policy`]'s PowerShell.
//!
//! **qcow2/AIM only.** The VHDX true-removal path must NOT use this guard:
//! `mountvol /N` stops the NTFS volume on an `AttachVirtualDisk` differencing
//! child from mounting at all (180 s device-not-ready, nothing deleted —
//! 2026-07-12). VHDX volumes never flash a letter anyway, so the VHDX path
//! relies solely on [`crate::true_removal_win`]'s `strip_drive_letters`. The
//! AIM-mounted qcow2 disk still surfaces its volume with auto-mount disabled,
//! so the guard works there and is the thing that keeps the letter hidden.
//!
//! [`AutoMountGuard`] is RAII and best-effort: `disable()` turns auto-mount off,
//! `Drop` turns it back on (retried, loudly logged on failure — a stuck
//! "disabled" would stop the user's own USB drives from getting letters until
//! they run `mountvol /E`). Any failure yields a no-op/loud guard; the delete
//! pass still works via the volume GUID path regardless.

#![cfg(target_os = "windows")]

use std::os::windows::process::CommandExt;
use std::process::Command;

use tracing::{debug, warn};

/// `CREATE_NO_WINDOW` — keep the helper `mountvol.exe` invisible.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Restores auto-mounting of new volumes when dropped.
#[must_use = "auto-mount is restored when the guard is dropped"]
pub struct AutoMountGuard {
    /// True when we disabled auto-mount and must re-enable it on drop.
    restore: bool,
}

impl AutoMountGuard {
    /// Turn auto-mount OFF so a surfacing volume gets no drive letter. Assumes
    /// the near-universal default (enabled) and re-enables on drop; `mountvol`
    /// has no query, so a machine that already had it disabled is re-enabled —
    /// harmless, enabled is the default.
    pub fn disable() -> Self {
        if run_mountvol("/N") {
            debug!("auto-mount disabled (mountvol /N) for the true-removal mount window");
            Self { restore: true }
        } else {
            warn!("could not disable auto-mount (mountvol /N) — a drive letter may appear briefly during removal");
            Self { restore: false }
        }
    }
}

impl Drop for AutoMountGuard {
    fn drop(&mut self) {
        if !self.restore {
            return;
        }
        // Re-enabling matters: a stuck "disabled" stops the user's own new
        // volumes from getting letters. Retry, and make failure impossible to
        // miss in the log.
        for attempt in 1..=3 {
            if run_mountvol("/E") {
                debug!("auto-mount re-enabled (mountvol /E)");
                return;
            }
            warn!(attempt, "retrying mountvol /E");
        }
        warn!("FAILED to re-enable auto-mount — run `mountvol /E` manually to restore drive-letter assignment");
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
