//! Temporary udev rules that keep the desktop away from a restore target
//! (PLAN §6, stage 3).
//!
//! The disk session's `flock` stops udevd from probing a disk while we hold it
//! (`disk_session_linux`), but the moment the session closes — and for every
//! partition that appears afterwards — udev runs its whole rule set again, and
//! that set is not only about naming devices:
//!
//! - udisks mounts what it recognises on a removable disk and shows it on the
//!   desktop (`UDISKS_IGNORE`, `UDISKS_AUTO`);
//! - LVM autoactivates a physical volume it has not seen before, and md adds a
//!   RAID member to an array by its UUID — on the same machine the copy came
//!   from that is the source's own volume group or array
//!   (`DM_MULTIPATH_DEVICE_PATH` is the flag multipath sets to keep both off
//!   the paths it owns, and both rule sets honour it);
//! - systemd declares the device ready, so a `UUID=` line in fstab may mount
//!   the restored clone instead of the original (`SYSTEMD_READY=0`).
//!
//! A hold is one rules file in `/run/udev/rules.d`, numbered before all of
//! those rule sets, matching the target disk and its partitions by kernel name.
//! It is removed when the hold drops, and the devices are then re-announced
//! (`udevadm trigger --action=change`) so they come up normally — what the
//! person sees after a restore is their disk, not a disk the system ignores.
//! A file left behind by a service that was killed is swept at its next start
//! ([`sweep_stale_holds`]).

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};

use tracing::{debug, info, warn};

use own_backup_common::Result;

/// Where runtime udev rules go; `/run` is cleared on boot, so a hold never
/// outlives the machine's uptime even if nothing removes it.
pub const RULES_DIR: &str = "/run/udev/rules.d";

/// Before 60-persistent-storage, 64-md-raid, 69-lvm, 80-udisks2, 99-systemd.
const PREFIX: &str = "59-own-backup-hold-";

/// The rules text for `names` (kernel names of whole disks or of single
/// devices: `sdb`, `nvme0n1`, `nbd3`). A whole disk's partitions are matched
/// too, by the kernel's naming rule: `sdb1`, `nvme0n1p1`.
pub fn rules_text(names: &[&str]) -> String {
    let mut out = String::from(
        "# Own Backup: temporary hold on a restore target or a mounted backup.\n\
         # Removed when the operation ends; /run is cleared on reboot.\n",
    );
    let set = "ENV{UDISKS_IGNORE}=\"1\", ENV{UDISKS_AUTO}=\"0\", \
               ENV{DM_MULTIPATH_DEVICE_PATH}=\"1\", ENV{SYSTEMD_READY}=\"0\"";
    for name in names {
        let parts = partition_pattern(name);
        out.push_str(&format!(
            "SUBSYSTEM==\"block\", KERNEL==\"{name}\", {set}\n\
             SUBSYSTEM==\"block\", KERNEL==\"{parts}\", {set}\n"
        ));
    }
    out
}

/// Kernel names of the partitions of disk `name`: a name ending in a digit
/// takes a `p` before the partition number (`nvme0n1p2`, `loop0p1`, `nbd0p1`).
fn partition_pattern(name: &str) -> String {
    if name.ends_with(|c: char| c.is_ascii_digit()) {
        format!("{name}p[0-9]*")
    } else {
        format!("{name}[0-9]*")
    }
}

fn file_name_for(names: &[&str]) -> String {
    format!("{PREFIX}{}-{}.rules", names.join("-"), std::process::id())
}

/// One installed hold. Dropping it removes the rules and re-announces the
/// devices.
pub struct UdevHold {
    path: PathBuf,
    names: Vec<String>,
    reannounce: bool,
}

impl UdevHold {
    /// Install a hold on `names` under the real rules directory.
    pub fn install(names: &[&str]) -> Result<Self> {
        Self::install_in(Path::new(RULES_DIR), names, true)
    }

    /// Install under `dir` (tests); `reannounce` runs `udevadm` on drop.
    pub fn install_in(dir: &Path, names: &[&str], reannounce: bool) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(file_name_for(names));
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, rules_text(names))?;
        std::fs::rename(&tmp, &path)?;
        if reannounce {
            udevadm(&["control", "--reload"]);
        }
        info!(rules = %path.display(), devices = ?names, "udev hold installed");
        Ok(Self {
            path,
            names: names.iter().map(|s| s.to_string()).collect(),
            reannounce,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for UdevHold {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.path) {
            warn!(rules = %self.path.display(), error = %e, "udev hold could not be removed");
            return;
        }
        if self.reannounce {
            udevadm(&["control", "--reload"]);
            for name in &self.names {
                // The disk and every partition on it, so udisks and the
                // volume managers see them as they now are.
                udevadm(&[
                    "trigger",
                    "--action=change",
                    &format!("--sysname-match={name}*"),
                ]);
            }
        }
        debug!(rules = %self.path.display(), "udev hold released");
    }
}

/// Remove holds left by a process that no longer exists (a killed service).
/// Returns the files removed.
pub fn sweep_stale_holds(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut removed = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(rest) = name
            .strip_prefix(PREFIX)
            .and_then(|r| r.strip_suffix(".rules"))
        else {
            continue;
        };
        let pid: Option<u32> = rest.rsplit('-').next().and_then(|p| p.parse().ok());
        let alive = pid.is_some_and(|p| Path::new(&format!("/proc/{p}")).exists());
        if pid != Some(std::process::id()) && !alive && std::fs::remove_file(e.path()).is_ok() {
            removed.push(e.path());
        }
    }
    if !removed.is_empty() {
        udevadm(&["control", "--reload"]);
        info!(files = ?removed, "stale udev holds removed");
    }
    removed
}

fn udevadm(args: &[&str]) {
    match std::process::Command::new("udevadm").args(args).output() {
        Ok(out) if out.status.success() => {}
        Ok(out) => debug!(
            ?args,
            stderr = %String::from_utf8_lossy(&out.stderr).trim(),
            "udevadm refused"
        ),
        Err(e) => debug!(?args, error = %e, "udevadm not available"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_match_the_disk_and_its_partitions_by_kernel_naming() {
        let text = rules_text(&["sdb", "nvme0n1"]);
        assert!(text.contains("KERNEL==\"sdb\","));
        assert!(text.contains("KERNEL==\"sdb[0-9]*\","));
        assert!(text.contains("KERNEL==\"nvme0n1p[0-9]*\","));
        // Every line that matches sets every flag.
        for line in text.lines().filter(|l| l.starts_with("SUBSYSTEM")) {
            for flag in [
                "UDISKS_IGNORE}=\"1\"",
                "UDISKS_AUTO}=\"0\"",
                "DM_MULTIPATH_DEVICE_PATH}=\"1\"",
                "SYSTEMD_READY}=\"0\"",
            ] {
                assert!(line.contains(flag), "{line} lacks {flag}");
            }
        }
    }

    #[test]
    fn a_hold_is_installed_and_removed_with_its_owner() {
        let dir = tempfile::tempdir().unwrap();
        let hold = UdevHold::install_in(dir.path(), &["vdb"], false).unwrap();
        let path = hold.path().to_path_buf();
        assert!(path.exists());
        assert!(path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("59-own-backup-hold-vdb-"));
        drop(hold);
        assert!(!path.exists());
    }

    #[test]
    fn only_holds_of_dead_processes_are_swept() {
        let dir = tempfile::tempdir().unwrap();
        // A pid far above any pid_max names no process.
        let dead = dir.path().join(format!("{PREFIX}sdc-999999999.rules"));
        std::fs::write(&dead, "x").unwrap();
        let ours = UdevHold::install_in(dir.path(), &["sdd"], false).unwrap();
        let foreign = dir.path().join("70-someone-else.rules");
        std::fs::write(&foreign, "x").unwrap();
        let removed = sweep_stale_holds(dir.path());
        assert_eq!(removed, vec![dead.clone()]);
        assert!(!dead.exists());
        assert!(ours.path().exists());
        assert!(foreign.exists());
    }
}
