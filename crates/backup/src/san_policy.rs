//! Windows "new disk" (SAN) policy control.
//!
//! Windows' default policy — `OnlineAll` — brings every freshly attached disk
//! *online* and auto-mounts its volumes. A **differencing VHDX** inherits a
//! complete NTFS filesystem from its parent, so the instant a child image is
//! attached for writing, Windows mounts that inherited volume. Our raw-write
//! path then takes the disk back offline, but the volume's device stack has
//! already been built (filters, indexer, Defender attach to it). When the
//! backup finishes, `DetachVirtualDisk` has to tear that stack down and blocks
//! on a fixed **~180 s PnP volume-removal timeout** — the stall observed only
//! for the 2nd and later (differencing) backups; a full image starts blank, so
//! nothing ever mounts and it detaches in milliseconds.
//!
//! The cure is to stop the auto-mount from ever happening: flip the machine's
//! new-disk policy to `OfflineAll` for the brief window in which the image is
//! attached, so the child arrives offline and the volume never surfaces.
//! [`SanPolicyGuard`] does that and restores the previous policy on drop
//! (including on the error/unwind path). It is a global machine setting, so the
//! window is kept as small as possible and restoration is retried and logged
//! loudly on failure.

pub use imp::SanPolicyGuard;

#[cfg(target_os = "windows")]
mod imp {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Output};

    use tracing::{debug, warn};

    /// `CREATE_NO_WINDOW` — keep the helper `powershell.exe` invisible.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    /// Restores the machine's new-disk policy when dropped.
    ///
    /// Obtain one with [`SanPolicyGuard::offline_all`] immediately before
    /// attaching a VHDX for raw writing, and drop it as soon as the disk is
    /// attached and confirmed offline.
    #[must_use = "the policy is restored when the guard is dropped"]
    pub struct SanPolicyGuard {
        /// Policy name to restore on drop (e.g. `"OnlineAll"`), or `None` when
        /// we changed nothing — query/set failed, or it was already offline.
        restore: Option<String>,
    }

    impl SanPolicyGuard {
        /// Set the new-disk policy to `OfflineAll`, remembering the previous
        /// value so [`Drop`] can restore it. Best-effort: if the policy cannot
        /// be read or set (missing cmdlet, not elevated), it logs and returns a
        /// no-op guard rather than failing the backup — the detach may then
        /// stall, but the backup data is still correct.
        pub fn offline_all() -> Self {
            let previous = match query_policy() {
                Some(p) => p,
                None => {
                    warn!(
                        "could not read the new-disk (SAN) policy — leaving it unchanged; \
                         a differencing detach may stall for ~3 min"
                    );
                    return Self { restore: None };
                }
            };
            if previous.eq_ignore_ascii_case("OfflineAll") {
                debug!("new-disk policy already OfflineAll — nothing to change");
                return Self { restore: None };
            }
            if set_policy("OfflineAll") {
                debug!(previous = %previous, "new-disk policy set to OfflineAll for VHDX attach");
                Self {
                    restore: Some(previous),
                }
            } else {
                warn!(
                    "could not set the new-disk (SAN) policy to OfflineAll; \
                     a differencing detach may stall for ~3 min"
                );
                Self { restore: None }
            }
        }
    }

    impl Drop for SanPolicyGuard {
        fn drop(&mut self) {
            let Some(policy) = self.restore.take() else {
                return;
            };
            // Restoring a global machine setting matters: a stuck `OfflineAll`
            // would make the user's own disks arrive offline. Retry before
            // giving up, and make the failure impossible to miss in the log.
            for attempt in 1..=3 {
                if set_policy(&policy) {
                    debug!(restored = %policy, "new-disk policy restored");
                    return;
                }
                warn!(policy = %policy, attempt, "retrying SAN policy restore");
            }
            warn!(
                policy = %policy,
                "FAILED to restore the new-disk (SAN) policy — the machine is left in \
                 OfflineAll; run `Set-StorageSetting -NewDiskPolicy {}` manually",
                policy
            );
        }
    }

    /// `(Get-StorageSetting).NewDiskPolicy` → e.g. `OnlineAll` / `OfflineAll`.
    fn query_policy() -> Option<String> {
        let out = run_ps("(Get-StorageSetting).NewDiskPolicy").ok()?;
        if !out.status.success() {
            warn!(stderr = %String::from_utf8_lossy(&out.stderr), "Get-StorageSetting failed");
            return None;
        }
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }

    /// `Set-StorageSetting -NewDiskPolicy <policy>`; `true` on success.
    fn set_policy(policy: &str) -> bool {
        match run_ps(&format!("Set-StorageSetting -NewDiskPolicy {policy}")) {
            Ok(o) if o.status.success() => true,
            Ok(o) => {
                warn!(
                    policy,
                    stderr = %String::from_utf8_lossy(&o.stderr),
                    "Set-StorageSetting failed"
                );
                false
            }
            Err(e) => {
                warn!(policy, error = %e, "failed to launch powershell for Set-StorageSetting");
                false
            }
        }
    }

    fn run_ps(command: &str) -> std::io::Result<Output> {
        Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", command])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
    }
}

#[cfg(not(target_os = "windows"))]
mod imp {
    /// No-op on non-Windows targets (Linux/mocks): there is no SAN policy and
    /// [`crate::engine_win`] is not compiled there anyway.
    #[must_use]
    pub struct SanPolicyGuard;

    impl SanPolicyGuard {
        pub fn offline_all() -> Self {
            SanPolicyGuard
        }
    }
}
