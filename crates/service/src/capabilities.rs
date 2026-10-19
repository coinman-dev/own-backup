//! What the backup engine and scheduler of this service actually carry out.
//!
//! The wizard asks this (`BackupCapabilities`) and offers only what a run will
//! do. Windows does everything listed. The Linux engine refuses, by design, a
//! job that asks for what it cannot do yet (`own_backup_core::linux_unsupported`
//! — chains, exclusions, whole-disk copies, speed limit, low priority, an
//! after-backup action): a window that offered those made jobs that never ran.
//! The two lists must change together — when the engine learns one, flip it
//! here in the same commit.

use serde::Serialize;

/// The answer, as JSON for the window.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BackupCapabilities {
    /// `windows` or `linux`: whose engine this is.
    pub os: &'static str,
    /// Backup methods a run carries out (`full`, `incremental`, `differential`).
    pub methods: Vec<&'static str>,
    /// Exclusion masks are applied (the excluded files are really removed).
    pub exclusions: bool,
    /// One image of a whole disk (`disk_source`): the real partition table with
    /// the chosen volumes at their offsets. Without it each volume gets its own
    /// image file.
    pub whole_disk: bool,
    /// `advanced.speed_limit_mbps`.
    pub speed_limit: bool,
    /// `advanced.low_priority`.
    pub low_priority: bool,
    /// `advanced.after_backup` (sleep / power off).
    pub after_backup: bool,
    /// Encrypted archives (OBK): needs the machine's key store.
    pub archive_encryption: bool,
    /// Event triggers the scheduler fires (`schedule.event_trigger`).
    pub schedule_events: Vec<&'static str>,
    /// `schedule.wake_to_run`: a wake timer is armed for the next slot.
    pub wake_to_run: bool,
    /// The machine is kept awake while a copy runs.
    pub keeps_awake: bool,
}

/// This build's answer.
pub fn current() -> BackupCapabilities {
    #[cfg(target_os = "linux")]
    {
        linux()
    }
    #[cfg(not(target_os = "linux"))]
    {
        windows()
    }
}

#[cfg_attr(target_os = "linux", allow(dead_code))] // tested everywhere, used off Linux
fn windows() -> BackupCapabilities {
    BackupCapabilities {
        os: "windows",
        methods: vec!["full", "incremental", "differential"],
        exclusions: true,
        whole_disk: true,
        speed_limit: true,
        low_priority: true,
        after_backup: true,
        archive_encryption: true,
        schedule_events: vec!["dest_connected", "logon", "logoff", "startup"],
        wake_to_run: true,
        keeps_awake: true,
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn linux() -> BackupCapabilities {
    BackupCapabilities {
        os: "linux",
        // `linux_unsupported`: only the full scheme for now (stage-1 L2).
        methods: vec!["full"],
        // L3.
        exclusions: false,
        // E6: whole disk on Linux.
        whole_disk: false,
        speed_limit: false,
        low_priority: false,
        // logind (stage 4).
        after_backup: false,
        // No key store yet: `BeginArchiveEncryption` refuses (decision 6).
        archive_encryption: false,
        // logon/logoff come from the Windows service control handler; the
        // destination is watched in the mount table, startup is a timer.
        schedule_events: vec!["dest_connected", "startup"],
        // `WakeArming` has no timer on Linux (stage 4).
        wake_to_run: false,
        // No logind inhibitor yet (stage 4).
        keeps_awake: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_offers_everything_the_wizard_has() {
        let c = windows();
        assert_eq!(c.methods, ["full", "incremental", "differential"]);
        assert!(c.exclusions && c.whole_disk && c.speed_limit && c.low_priority);
        assert!(c.after_backup && c.archive_encryption && c.wake_to_run && c.keeps_awake);
        assert_eq!(c.schedule_events.len(), 4);
    }

    /// What the Linux engine refuses must not be offered: each `false` here
    /// is one reason in `own_backup_core::linux_unsupported`, and a job built
    /// from this answer passes it (only `full`, no exclusions, no disk, no
    /// pacing, no after-backup action).
    #[test]
    fn linux_offers_only_what_its_engine_runs() {
        let c = linux();
        assert_eq!(c.os, "linux");
        assert_eq!(c.methods, ["full"]);
        assert!(!c.exclusions && !c.whole_disk && !c.speed_limit && !c.low_priority);
        assert!(!c.after_backup && !c.archive_encryption && !c.wake_to_run && !c.keeps_awake);
        assert!(!c.schedule_events.contains(&"logon"));
        assert!(!c.schedule_events.contains(&"logoff"));
    }

    #[test]
    fn current_is_this_platform() {
        let os = if cfg!(target_os = "linux") {
            "linux"
        } else {
            "windows"
        };
        assert_eq!(current().os, os);
    }
}
