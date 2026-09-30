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
    use tracing::{debug, warn};

    use crate::registry_win::{delete_value_hklm, read_dword_hklm, write_dword_hklm};

    /// Where the partition manager itself reads the policy from. `diskpart`'s
    /// `san policy=…` writes exactly this value; PowerShell's
    /// `Set-StorageSetting -NewDiskPolicy` ends up here too, one WMI provider
    /// later. Going to the value directly is what lets this work in WinPE,
    /// where neither tool is installed.
    const KEY: &str = r"SYSTEM\CurrentControlSet\Services\partmgr\Parameters";
    const VALUE: &str = "SanPolicy";

    /// `SAN_POLICY` values, as partmgr numbers them.
    const ONLINE_ALL: u32 = 1;
    const OFFLINE_SHARED: u32 = 2;
    const OFFLINE_ALL: u32 = 3;
    const OFFLINE_INTERNAL: u32 = 4;

    fn policy_name(value: u32) -> &'static str {
        match value {
            ONLINE_ALL => "OnlineAll",
            OFFLINE_SHARED => "OfflineShared",
            OFFLINE_ALL => "OfflineAll",
            OFFLINE_INTERNAL => "OfflineInternal",
            _ => "Unknown",
        }
    }

    /// What the machine had before we touched it: a value, or nothing at all.
    /// The difference matters on the way out — a machine that carried no
    /// explicit policy must not be left carrying one.
    #[derive(Debug, Clone, Copy)]
    enum Previous {
        Value(u32),
        Absent,
    }

    /// Restores the machine's new-disk policy when dropped.
    ///
    /// Obtain one with [`SanPolicyGuard::offline_all`] immediately before
    /// attaching a VHDX for raw writing, and drop it as soon as the disk is
    /// attached and confirmed offline.
    #[must_use = "the policy is restored when the guard is dropped"]
    pub struct SanPolicyGuard {
        /// What to put back on drop, or `None` when we changed nothing.
        restore: Option<Previous>,
    }

    impl SanPolicyGuard {
        /// Set the new-disk policy to `OfflineAll`, remembering the previous
        /// value so [`Drop`] can restore it. Best-effort: if the value cannot
        /// be written (no rights), it logs and returns a no-op guard rather
        /// than failing the backup — the detach may then stall, but the backup
        /// data is still correct.
        pub fn offline_all() -> Self {
            let previous = match read_dword_hklm(KEY, VALUE) {
                Some(v) => Previous::Value(v),
                // No value = the driver's default (OnlineAll on client
                // Windows), which is exactly the case this guard exists for.
                None => Previous::Absent,
            };
            if matches!(previous, Previous::Value(OFFLINE_ALL)) {
                debug!("new-disk policy already OfflineAll — nothing to change");
                return Self { restore: None };
            }
            if write_dword_hklm(KEY, VALUE, OFFLINE_ALL) {
                debug!(
                    ?previous,
                    "new-disk policy set to OfflineAll for the VHDX attach"
                );
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
            let Some(previous) = self.restore.take() else {
                return;
            };
            // Restoring a global machine setting matters: a stuck `OfflineAll`
            // would make the user's own disks arrive offline. Retry before
            // giving up, and make the failure impossible to miss in the log.
            for attempt in 1..=3 {
                let ok = match previous {
                    Previous::Value(v) => write_dword_hklm(KEY, VALUE, v),
                    Previous::Absent => delete_value_hklm(KEY, VALUE),
                };
                if ok {
                    debug!(?previous, "new-disk policy restored");
                    return;
                }
                warn!(?previous, attempt, "retrying the SAN policy restore");
            }
            let name = match previous {
                Previous::Value(v) => policy_name(v),
                Previous::Absent => "OnlineAll (was unset)",
            };
            warn!(
                restore_to = name,
                "FAILED to restore the new-disk (SAN) policy — the machine is left in OfflineAll; \
                 put it back with `diskpart` → `san policy=onlineall`"
            );
        }
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
