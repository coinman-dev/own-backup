//! Linux boot medium: the volumes the path picker offers beside `/`.
//!
//! Nothing on the medium mounts a disk by itself, and the picker only knew `/`,
//! so a copy on another disk could not be reached without a terminal. Mounting
//! every disk at boot is not the answer: a mounted partition is what the restore
//! guard refuses to overwrite (`guard_linux_volume`), and the disks here are the
//! ones being rescued. So a volume is mounted when the user opens it in the
//! picker, under `/mnt/<device>`, and the others stay free.
//!
//! Read-only first, without replaying a journal: looking for a copy must not
//! change the disk it is on. A volume becomes writable only when a folder on it
//! is chosen for writing ([`make_writable`]).

use own_backup_common::tr;
use serde_json::Value;
use std::process::Command;

const ROOT: &str = "/mnt";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    /// Kernel or device-mapper name, also the folder under [`ROOT`].
    pub name: String,
    pub device: String,
    pub fs: String,
    pub label: String,
    pub size: Option<u64>,
    pub mount_point: Option<String>,
}

/// The driver and options to mount `fs` with; `None` for what is not offered.
/// NTFS goes to the kernel's driver: a FUSE daemon would live in the desktop
/// session and take the mount with it when the session restarts.
fn mount_plan(fs: &str, writable: bool) -> Option<(&'static str, &'static str)> {
    let (driver, read_only) = match fs {
        "ext2" | "ext3" | "ext4" => ("ext4", "ro,noload"),
        "xfs" => ("xfs", "ro,norecovery"),
        "btrfs" => ("btrfs", "ro,rescue=nologreplay"),
        "ntfs" => ("ntfs3", "ro"),
        "vfat" => ("vfat", "ro"),
        "exfat" => ("exfat", "ro"),
        _ => return None,
    };
    Some((driver, if writable { "rw" } else { read_only }))
}

fn under_root(mount_point: &str) -> bool {
    mount_point == ROOT
        || mount_point
            .strip_prefix(ROOT)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Volumes out of `lsblk -J -l -b -o NAME,PATH,TYPE,FSTYPE,LABEL,SIZE,MOUNTPOINTS`.
/// What is mounted elsewhere is the medium's own (its stick, its log partition)
/// and is not offered; LUKS, BitLocker and LVM members have no filesystem to open.
fn parse_lsblk(json: &str) -> Vec<Volume> {
    let parsed: Value = serde_json::from_str(json).unwrap_or(Value::Null);
    let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).map(str::to_string);
    let Some(devices) = parsed.get("blockdevices").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for v in devices {
        let kind = text(v, "type").unwrap_or_default();
        if !matches!(kind.as_str(), "part" | "disk" | "lvm" | "crypt") && !kind.starts_with("raid")
        {
            continue;
        }
        let fs = text(v, "fstype").unwrap_or_default();
        let (Some(name), Some(device)) = (text(v, "name"), text(v, "path")) else {
            continue;
        };
        if name.contains('/') || mount_plan(&fs, false).is_none() {
            continue;
        }
        let mounts: Vec<&str> = v
            .get("mountpoints")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if !mounts.iter().all(|m| under_root(m)) {
            continue;
        }
        let size = v.get("size").and_then(|s| {
            s.as_u64()
                .or_else(|| s.as_str().and_then(|t| t.parse().ok()))
        });
        out.push(Volume {
            name,
            device,
            fs,
            label: text(v, "label").unwrap_or_default(),
            size,
            mount_point: mounts.first().map(|m| m.to_string()),
        });
    }
    out
}

fn run(program: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("{program}: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// What the picker lists. Empty off the medium: an installed system's disks are
/// its own desktop's business, and only the medium's window runs as root.
pub fn list() -> Vec<Volume> {
    if !own_backup_common::media_mode::is_linux_medium() {
        return Vec::new();
    }
    match run(
        "lsblk",
        &[
            "-J",
            "-l",
            "-b",
            "-o",
            "NAME,PATH,TYPE,FSTYPE,LABEL,SIZE,MOUNTPOINTS",
        ],
    ) {
        Ok(json) => parse_lsblk(&json),
        Err(error) => {
            tracing::warn!(%error, "picker: the volumes could not be listed");
            Vec::new()
        }
    }
}

fn find(name: &str) -> Result<Volume, String> {
    list()
        .into_iter()
        .find(|v| v.name == name)
        .ok_or_else(|| tr!("files.volume_gone", name = name).into_owned())
}

fn mount_at(v: &Volume, dir: &str, writable: bool) -> Result<(), String> {
    let (driver, options) = mount_plan(&v.fs, writable)
        .ok_or_else(|| tr!("files.volume_gone", name = v.name.as_str()).into_owned())?;
    let failed = |error: String| {
        tr!(
            "files.volume_not_mounted",
            name = v.name.as_str(),
            error = error
        )
        .into_owned()
    };
    std::fs::create_dir_all(dir).map_err(|e| failed(e.to_string()))?;
    run("mount", &["-t", driver, "-o", options, &v.device, dir])
        .map(|_| ())
        .map_err(failed)
}

/// Mount a volume read-only unless it already is mounted; returns where it is.
pub fn mount(name: &str) -> Result<String, String> {
    let v = find(name)?;
    if let Some(dir) = v.mount_point {
        return Ok(dir);
    }
    let dir = format!("{ROOT}/{}", v.name);
    mount_at(&v, &dir, false)?;
    Ok(dir)
}

/// Release a volume, so that it can be restored onto or backed up.
pub fn unmount(name: &str) -> Result<(), String> {
    let v = find(name)?;
    let Some(dir) = v.mount_point else {
        return Ok(());
    };
    run("umount", &[&dir]).map_err(|error| {
        tr!(
            "files.volume_not_unmounted",
            name = v.name.as_str(),
            error = error
        )
        .into_owned()
    })?;
    if dir != ROOT {
        let _ = std::fs::remove_dir(&dir);
    }
    Ok(())
}

/// The mounted volume `path` is on, by the longest mount point that holds it.
fn volume_holding(volumes: Vec<Volume>, path: &str) -> Option<(Volume, String)> {
    volumes
        .into_iter()
        .filter_map(|v| {
            let dir = v.mount_point.clone()?;
            let holds = path == dir
                || path
                    .strip_prefix(dir.as_str())
                    .is_some_and(|rest| rest.starts_with('/'));
            holds.then_some((v, dir))
        })
        .max_by_key(|(_, dir)| dir.len())
}

/// A folder was chosen for writing: mount its volume read-write. `noload` cannot
/// be remounted away, so it is mounted anew, and put back read-only when the
/// system refuses (a Windows left hibernated, a dirty NTFS).
pub fn make_writable(path: &str) -> Result<(), String> {
    let Some((v, dir)) = volume_holding(list(), path) else {
        return Ok(());
    };
    let options = run("findmnt", &["-n", "-o", "OPTIONS", "--mountpoint", &dir]).unwrap_or_default();
    if options.trim().split(',').next() == Some("rw") {
        return Ok(());
    }
    run("umount", &[&dir]).map_err(|error| {
        tr!(
            "files.volume_not_unmounted",
            name = v.name.as_str(),
            error = error
        )
        .into_owned()
    })?;
    mount_at(&v, &dir, true).inspect_err(|_| {
        let _ = mount_at(&v, &dir, false);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LSBLK: &str = r#"{"blockdevices":[
      {"name":"loop0","path":"/dev/loop0","type":"loop","fstype":"squashfs","label":null,"size":900,"mountpoints":["/run/live/rootfs/filesystem.squashfs"]},
      {"name":"sr0","path":"/dev/sr0","type":"rom","fstype":"iso9660","label":"OBK","size":800,"mountpoints":["/run/live/medium"]},
      {"name":"sda","path":"/dev/sda","type":"disk","fstype":null,"label":null,"size":200,"mountpoints":[null]},
      {"name":"sdb1","path":"/dev/sdb1","type":"part","fstype":"ntfs","label":"KUBUNTU-BAK","size":32,"mountpoints":[null]},
      {"name":"sdb2","path":"/dev/sdb2","type":"part","fstype":"ext4","label":null,"size":"16","mountpoints":["/mnt/sdb2"]},
      {"name":"sdc1","path":"/dev/sdc1","type":"part","fstype":"vfat","label":"OBK-LOGS","size":4,"mountpoints":["/run/own-backup-media-log"]},
      {"name":"sdc2","path":"/dev/sdc2","type":"part","fstype":"crypto_LUKS","label":null,"size":5,"mountpoints":[null]},
      {"name":"sdc3","path":"/dev/sdc3","type":"part","fstype":"BitLocker","label":null,"size":6,"mountpoints":[null]},
      {"name":"vg-home","path":"/dev/mapper/vg-home","type":"lvm","fstype":"xfs","label":"home","size":7,"mountpoints":[null]}
    ]}"#;

    #[test]
    fn only_what_can_be_opened_and_is_not_the_mediums_own_is_offered() {
        let names: Vec<String> = parse_lsblk(LSBLK).into_iter().map(|v| v.name).collect();
        assert_eq!(names, ["sdb1", "sdb2", "vg-home"]);
    }

    #[test]
    fn a_volume_carries_its_label_size_and_mount_point() {
        let all = parse_lsblk(LSBLK);
        assert_eq!(all[0].label, "KUBUNTU-BAK");
        assert_eq!(all[0].size, Some(32));
        assert_eq!(all[0].mount_point, None);
        // Older lsblk writes numbers as strings.
        assert_eq!(all[1].size, Some(16));
        assert_eq!(all[1].mount_point.as_deref(), Some("/mnt/sdb2"));
    }

    #[test]
    fn broken_output_offers_nothing() {
        assert!(parse_lsblk("not json").is_empty());
        assert!(parse_lsblk("{}").is_empty());
    }

    #[test]
    fn looking_never_replays_a_journal() {
        assert_eq!(mount_plan("ext4", false), Some(("ext4", "ro,noload")));
        assert_eq!(mount_plan("ext2", false), Some(("ext4", "ro,noload")));
        assert_eq!(mount_plan("xfs", false), Some(("xfs", "ro,norecovery")));
        assert_eq!(
            mount_plan("btrfs", false),
            Some(("btrfs", "ro,rescue=nologreplay"))
        );
        assert_eq!(mount_plan("ntfs", false), Some(("ntfs3", "ro")));
        assert_eq!(mount_plan("ntfs", true), Some(("ntfs3", "rw")));
        assert_eq!(mount_plan("crypto_LUKS", false), None);
    }

    #[test]
    fn the_longest_mount_point_holding_the_path_wins() {
        let v = |name: &str, dir: &str| Volume {
            name: name.into(),
            device: format!("/dev/{name}"),
            fs: "ext4".into(),
            label: String::new(),
            size: None,
            mount_point: Some(dir.into()),
        };
        let found = |path: &str| {
            volume_holding(vec![v("a", "/mnt"), v("b", "/mnt/b")], path).map(|(v, _)| v.name)
        };
        assert_eq!(found("/mnt/b/copies").as_deref(), Some("b"));
        assert_eq!(found("/mnt/b").as_deref(), Some("b"));
        assert_eq!(found("/mnt/bb").as_deref(), Some("a"));
        assert_eq!(found("/home"), None);
    }

    #[test]
    fn only_paths_under_the_root_count_as_ours() {
        assert!(under_root("/mnt"));
        assert!(under_root("/mnt/sdb1"));
        assert!(!under_root("/mntx"));
        assert!(!under_root("/run/live/medium"));
    }
}
