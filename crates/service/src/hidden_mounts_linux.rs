//! Mounting the partitions nothing has mounted, from the «Инструменты» page
//! (Linux) — the counterpart of drive letters for hidden partitions on
//! Windows. A partition goes to `/mnt/<label>`; the record of what this page
//! mounted lives in `/run/own-backup` and so ends with the boot, as the
//! mounts themselves do. Only what is in the record is ever unmounted here:
//! a mount made by the system, fstab or a file manager is not ours.

#![cfg(target_os = "linux")]

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use own_backup_common::disk::{HiddenVolume, OwnMount};
use own_backup_common::tr;
use own_backup_common::types::FileSystem;
use tracing::{info, warn};

const RECORD: &str = "/run/own-backup/hidden-mounts.json";
const BASE: &str = "/mnt";

/// What this page mounted and has not unmounted yet.
pub(crate) fn own_mounts() -> Vec<OwnMount> {
    std::fs::read(RECORD)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save(list: &[OwnMount]) -> std::io::Result<()> {
    let tmp = format!("{RECORD}.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(list)?)?;
    std::fs::rename(&tmp, RECORD)
}

/// A folder name from a label: what a person would type, without `/`, control
/// characters or a leading dot; spaces become `_`.
fn folder_name(label: &str) -> Option<String> {
    let name: String = label
        .trim()
        .chars()
        .filter(|c| !c.is_control() && *c != '/')
        .map(|c| if c.is_whitespace() { '_' } else { c })
        .collect();
    let name = name.trim_start_matches('.').to_string();
    (!name.is_empty() && name.len() <= 64).then_some(name)
}

/// The base name of the folder for `v`: its label, else what it is.
fn base_name(v: &HiddenVolume) -> String {
    let kernel = Path::new(&v.kernel_device)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "partition".into());
    folder_name(&v.label).unwrap_or_else(|| match v.kind.as_str() {
        "esp" => "EFI".into(),
        "winre" => "Recovery".into(),
        _ => kernel,
    })
}

/// Free for a mount: absent, or an empty folder that is not a mount point.
fn usable(dir: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(dir) else {
        return true;
    };
    if !meta.is_dir() {
        return false;
    }
    let empty = std::fs::read_dir(dir).is_ok_and(|mut d| d.next().is_none());
    let parent_dev = dir
        .parent()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.dev());
    empty && parent_dev == Some(meta.dev())
}

/// `/mnt/<name>`, else `/mnt/<name>-<kernel name>`, else numbered.
fn pick_dir(v: &HiddenVolume) -> Option<PathBuf> {
    let name = base_name(v);
    let kernel = Path::new(&v.kernel_device)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut tries = vec![name.clone()];
    if !kernel.is_empty() && kernel != name {
        tries.push(format!("{name}-{kernel}"));
    }
    tries.extend((2..10).map(|n| format!("{name}-{n}")));
    tries
        .into_iter()
        .map(|t| Path::new(BASE).join(t))
        .find(|d| usable(d))
}

/// Options for `fs`: never set-uid programs or device nodes from a foreign
/// partition. FAT, exFAT and NTFS have no owners of their own: the files are
/// root's, readable by everyone (the service's umask would hide them).
fn options(fs: FileSystem, read_only: bool) -> String {
    let mut o = vec![if read_only { "ro" } else { "rw" }, "nosuid", "nodev"];
    if matches!(fs, FileSystem::Fat32 | FileSystem::ExFat | FileSystem::Ntfs) {
        o.push("umask=022");
    }
    o.join(",")
}

fn run_mount(device: &str, dir: &Path, opts: &str) -> Result<(), String> {
    let out = Command::new("mount")
        .env("LC_ALL", "C")
        .args(["-o", opts, "--"])
        .arg(device)
        .arg(dir)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Mount `v` under `/mnt`. Read-write; when that is refused (a hibernated
/// Windows, a file system marked dirty), read-only — `read_only` in the list
/// says which.
pub(crate) fn mount(v: &HiddenVolume) -> Result<(), String> {
    if v.mount_point.is_some() {
        return Err(tr!("hidden_mounts.already").into_owned());
    }
    let dir = pick_dir(v).ok_or_else(|| tr!("hidden_mounts.no_folder").into_owned())?;
    let created = !dir.exists();
    std::fs::create_dir_all(&dir)
        .and_then(|_| std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)))
        .map_err(|e| {
            tr!(
                "hidden_mounts.mkdir_failed",
                dir = dir.display().to_string(),
                error = e
            )
            .into_owned()
        })?;
    let read_only = match run_mount(&v.device, &dir, &options(v.filesystem, false)) {
        Ok(()) => false,
        Err(rw) => {
            warn!(device = %v.device, error = %rw, "read-write mount refused, trying read-only");
            if let Err(ro) = run_mount(&v.device, &dir, &options(v.filesystem, true)) {
                if created {
                    let _ = std::fs::remove_dir(&dir);
                }
                return Err(tr!("hidden_mounts.mount_failed", error = ro).into_owned());
            }
            true
        }
    };
    info!(device = %v.device, dir = %dir.display(), read_only, "partition mounted from Tools");
    let mut list = own_mounts();
    list.retain(|m| m.device != v.device);
    list.push(OwnMount {
        device: v.device.clone(),
        mount_point: dir.display().to_string(),
        read_only,
    });
    if let Err(e) = save(&list) {
        warn!(?e, "the record of mounted partitions was not saved");
    }
    Ok(())
}

/// Unmount what this page mounted. A busy file system stays mounted and the
/// person is told why: a lazy unmount would pull it from under open files.
pub(crate) fn unmount(v: &HiddenVolume) -> Result<(), String> {
    let Some(dir) = v.mount_point.as_deref() else {
        return Err(tr!("hidden_mounts.not_mounted").into_owned());
    };
    let out = Command::new("umount")
        .env("LC_ALL", "C")
        .arg("--")
        .arg(dir)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if why.contains("busy") {
            tr!("hidden_mounts.busy", dir = dir).into_owned()
        } else {
            tr!("hidden_mounts.umount_failed", dir = dir, error = why).into_owned()
        });
    }
    // The folder goes only when empty: one that was there before keeps
    // whatever it is to its owner, and remove_dir refuses a non-empty one.
    let _ = std::fs::remove_dir(dir);
    info!(device = %v.device, dir, "partition unmounted from Tools");
    let mut list = own_mounts();
    list.retain(|m| m.device != v.device);
    if let Err(e) = save(&list) {
        warn!(?e, "the record of mounted partitions was not saved");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vol(label: &str, kind: &str) -> HiddenVolume {
        HiddenVolume {
            device: "/dev/disk/by-partuuid/x".into(),
            letter: None,
            kind: kind.into(),
            disk_number: 0,
            disk_model: String::new(),
            part_index: 1,
            size_bytes: 0,
            filesystem: FileSystem::Fat32,
            label: label.into(),
            mount_point: None,
            read_only: false,
            kernel_device: "/dev/nvme0n1p1".into(),
        }
    }

    #[test]
    fn folder_names_come_from_the_label_or_the_kind() {
        assert_eq!(
            base_name(&vol("Windows RE tools", "other")),
            "Windows_RE_tools"
        );
        assert_eq!(base_name(&vol("../..", "other")), "nvme0n1p1");
        assert_eq!(base_name(&vol("", "esp")), "EFI");
        assert_eq!(base_name(&vol(" ", "winre")), "Recovery");
        assert_eq!(base_name(&vol("", "other")), "nvme0n1p1");
        assert_eq!(base_name(&vol("Данные", "other")), "Данные");
    }

    #[test]
    fn foreign_file_systems_are_mounted_without_suid_and_readable() {
        assert_eq!(
            options(FileSystem::Fat32, false),
            "rw,nosuid,nodev,umask=022"
        );
        assert_eq!(options(FileSystem::Ntfs, true), "ro,nosuid,nodev,umask=022");
        assert_eq!(options(FileSystem::Ext4, false), "rw,nosuid,nodev");
    }

    #[test]
    fn only_an_empty_folder_that_is_not_a_mount_point_is_used() {
        let root = std::env::temp_dir().join(format!("obk-hm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("empty")).unwrap();
        std::fs::create_dir_all(root.join("full")).unwrap();
        std::fs::write(root.join("full/a"), b"x").unwrap();
        std::fs::write(root.join("file"), b"x").unwrap();
        assert!(usable(&root.join("absent")));
        assert!(usable(&root.join("empty")));
        assert!(!usable(&root.join("full")));
        assert!(!usable(&root.join("file")));
        std::fs::remove_dir_all(&root).unwrap();
        // /proc is a mount point of its own, and not empty anyway.
        assert!(!usable(Path::new("/proc")));
    }
}
