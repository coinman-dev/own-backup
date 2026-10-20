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
//!   the restored clone instead of the original (`SYSTEMD_READY=0`);
//! - `60-persistent-storage` points `/dev/disk/by-uuid`, `by-label` and
//!   `by-partuuid` at whichever device it saw last — a restored clone takes
//!   the machine's own links (`UDEV_DISABLE_PERSISTENT_STORAGE_RULES_FLAG`).
//!   This removes every persistent link of a held device, its `by-id` and
//!   `by-path` ones included, until the hold ends: whoever works on a held
//!   device names it by its kernel node (`/dev/sdb`), resolved before the
//!   hold, never by a `/dev/disk/by-*` link.
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
               ENV{DM_MULTIPATH_DEVICE_PATH}=\"1\", ENV{SYSTEMD_READY}=\"0\", \
               ENV{UDEV_DISABLE_PERSISTENT_STORAGE_RULES_FLAG}=\"1\"";
    for name in names {
        let parts = partition_pattern(name);
        out.push_str(&format!(
            "SUBSYSTEM==\"block\", KERNEL==\"{name}\", {set}\n\
             SUBSYSTEM==\"block\", KERNEL==\"{parts}\", {set}\n"
        ));
    }
    out
}

/// What every rule of a hold on a device the restore itself stacks on what it
/// wrote (a LUKS mapping it opens, an LV it activates, an md array it
/// assembles) sets. These devices carry the source's identities until the
/// restore has changed them, so on top of the flags above nothing may name
/// them: no `/dev/disk/by-uuid` link that would move the machine's own
/// `UUID=` to the clone, no btrfs registration.
const STACK_FLAGS: &str = "ENV{UDISKS_IGNORE}=\"1\", ENV{UDISKS_AUTO}=\"0\", \
     ENV{SYSTEMD_READY}=\"0\", ENV{DM_UDEV_DISABLE_DISK_RULES_FLAG}=\"1\", \
     ENV{DM_UDEV_DISABLE_OTHER_RULES_FLAG}=\"1\", \
     ENV{UDEV_DISABLE_PERSISTENT_STORAGE_RULES_FLAG}=\"1\"";

/// A rule holding the device-mapper devices whose name starts with `prefix`
/// (the LUKS mappings a restore opens). `55-dm.rules` has set `DM_NAME` by
/// the time it runs; `60-persistent-storage-dm.rules` honours the flags.
pub fn dm_name_rule(prefix: &str) -> String {
    format!("SUBSYSTEM==\"block\", KERNEL==\"dm-[0-9]*\", ENV{{DM_NAME}}==\"{prefix}*\", {STACK_FLAGS}\n")
}

/// A rule holding the logical volumes of the volume group with UUID
/// `vg_uuid` (dashes dropped, as device-mapper writes it into `DM_UUID`).
pub fn dm_lvm_vg_rule(vg_uuid: &str) -> String {
    let id: String = vg_uuid.chars().filter(|c| *c != '-').collect();
    format!("SUBSYSTEM==\"block\", KERNEL==\"dm-[0-9]*\", ENV{{DM_UUID}}==\"LVM-{id}*\", {STACK_FLAGS}\n")
}

/// A rule holding the md array with kernel name `name` and its partitions.
pub fn md_rule(name: &str) -> String {
    format!(
        "SUBSYSTEM==\"block\", KERNEL==\"{name}\", {STACK_FLAGS}\n\
         SUBSYSTEM==\"block\", KERNEL==\"{name}p[0-9]*\", {STACK_FLAGS}\n"
    )
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

    /// Install a hold made of `rules` (from [`dm_name_rule`],
    /// [`dm_lvm_vg_rule`], [`md_rule`]), named after `tag`. Nothing is
    /// announced again when it drops: the devices it holds are gone by then.
    pub fn install_rules(tag: &str, rules: &[String]) -> Result<Self> {
        Self::install_rules_in(Path::new(RULES_DIR), tag, rules, true)
    }

    /// [`Self::install_rules`] under `dir` (tests); `reload` runs `udevadm`.
    pub fn install_rules_in(dir: &Path, tag: &str, rules: &[String], reload: bool) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(file_name_for(&[tag]));
        let tmp = path.with_extension("tmp");
        let mut text = String::from(
            "# Own Backup: temporary hold on devices a restore stacks on what it wrote.\n\
             # Removed when the operation ends; /run is cleared on reboot.\n",
        );
        for r in rules {
            text.push_str(r);
        }
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &path)?;
        if reload {
            udevadm(&["control", "--reload"]);
        }
        info!(rules = %path.display(), tag, "udev hold installed");
        Ok(Self {
            path,
            names: Vec::new(),
            reannounce: reload,
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
            let names: Vec<&str> = self.names.iter().map(String::as_str).collect();
            reannounce(&names, &mut udevadm);
        }
        debug!(rules = %self.path.display(), "udev hold released");
    }
}

/// After holds are removed: reload the rules and announce every held device
/// again — the disk and every partition on it — so udisks and the volume
/// managers see them as they now are.
fn reannounce(names: &[&str], run: &mut dyn FnMut(&[&str])) {
    run(&["control", "--reload"]);
    for name in names {
        run(&[
            "trigger",
            "--action=change",
            &format!("--sysname-match={name}"),
            &format!("--sysname-match={}", partition_pattern(name)),
        ]);
    }
}

/// The kernel names a rules file written by [`rules_text`] holds: the exact
/// `KERNEL==` matches, not the partition patterns derived from them.
fn held_names(text: &str) -> Vec<String> {
    let mut out: Vec<String> = text
        .lines()
        .filter_map(|line| {
            let rest = &line[line.find("KERNEL==\"")? + "KERNEL==\"".len()..];
            let name = &rest[..rest.find('"')?];
            let plain = !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'));
            plain.then(|| name.to_string())
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Remove holds left by a process that no longer exists (a killed service),
/// and announce the devices they held again, as releasing a hold does.
/// Returns the files removed.
pub fn sweep_stale_holds(dir: &Path) -> Vec<PathBuf> {
    sweep_with(dir, &mut udevadm)
}

fn sweep_with(dir: &Path, run: &mut dyn FnMut(&[&str])) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut removed = Vec::new();
    let mut names: Vec<String> = Vec::new();
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
        if pid == Some(std::process::id()) || alive {
            continue;
        }
        // Read before removing: the file is the only record of what it held.
        let held = std::fs::read_to_string(e.path())
            .map(|t| held_names(&t))
            .unwrap_or_default();
        if std::fs::remove_file(e.path()).is_ok() {
            removed.push(e.path());
            names.extend(held);
        }
    }
    if !removed.is_empty() {
        names.sort();
        names.dedup();
        let held: Vec<&str> = names.iter().map(String::as_str).collect();
        reannounce(&held, run);
        info!(files = ?removed, devices = ?names, "stale udev holds removed");
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
                // No by-uuid/by-label link may move to a restored clone.
                "UDEV_DISABLE_PERSISTENT_STORAGE_RULES_FLAG}=\"1\"",
            ] {
                assert!(line.contains(flag), "{line} lacks {flag}");
            }
        }
    }

    #[test]
    fn stacked_devices_are_held_from_naming_and_scanning() {
        let dir = tempfile::tempdir().unwrap();
        let rules = vec![
            dm_name_rule("obk-42-"),
            dm_lvm_vg_rule("abcDEF-1234"),
            md_rule("md126"),
        ];
        let hold = UdevHold::install_rules_in(dir.path(), "reid", &rules, false).unwrap();
        let text = std::fs::read_to_string(hold.path()).unwrap();
        assert!(text.contains("ENV{DM_NAME}==\"obk-42-*\""));
        assert!(text.contains("ENV{DM_UUID}==\"LVM-abcDEF1234*\""));
        assert!(text.contains("KERNEL==\"md126p[0-9]*\""));
        for line in text.lines().filter(|l| l.starts_with("SUBSYSTEM")) {
            for flag in [
                "SYSTEMD_READY}=\"0\"",
                "DM_UDEV_DISABLE_DISK_RULES_FLAG}=\"1\"",
                "UDEV_DISABLE_PERSISTENT_STORAGE_RULES_FLAG}=\"1\"",
            ] {
                assert!(line.contains(flag), "{line} lacks {flag}");
            }
        }
        let path = hold.path().to_path_buf();
        drop(hold);
        assert!(!path.exists());
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
    fn a_stale_hold_is_swept_and_its_devices_announced_again() {
        let dir = tempfile::tempdir().unwrap();
        let dead = dir
            .path()
            .join(format!("{PREFIX}sdc-nvme0n1-999999999.rules"));
        std::fs::write(&dead, rules_text(&["sdc", "nvme0n1"])).unwrap();
        let mut calls: Vec<Vec<String>> = Vec::new();
        let removed = sweep_with(dir.path(), &mut |args| {
            calls.push(args.iter().map(|a| a.to_string()).collect())
        });
        assert_eq!(removed, vec![dead.clone()]);
        assert!(!dead.exists());
        assert_eq!(calls[0], ["control", "--reload"]);
        assert_eq!(
            calls[1..],
            [
                vec![
                    "trigger",
                    "--action=change",
                    "--sysname-match=nvme0n1",
                    "--sysname-match=nvme0n1p[0-9]*"
                ],
                vec![
                    "trigger",
                    "--action=change",
                    "--sysname-match=sdc",
                    "--sysname-match=sdc[0-9]*"
                ],
            ]
        );

        // Nothing stale: nothing run.
        calls.clear();
        assert!(sweep_with(dir.path(), &mut |args| calls
            .push(args.iter().map(|a| a.to_string()).collect()))
        .is_empty());
        assert!(calls.is_empty());
    }

    #[test]
    fn held_names_are_the_exact_matches_of_a_rules_file() {
        assert_eq!(
            held_names(&rules_text(&["dm-0", "sdb3", "loop0"])),
            vec!["dm-0", "loop0", "sdb3"]
        );
        assert!(held_names("# nothing\nKERNEL==\"sd*\"\n").is_empty());
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
        let removed = sweep_with(dir.path(), &mut |_| {});
        assert_eq!(removed, vec![dead.clone()]);
        assert!(!dead.exists());
        assert!(ours.path().exists());
        assert!(foreign.exists());
    }
}
