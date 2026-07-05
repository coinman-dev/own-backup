//! Linux snapshot provider: LVM copy-on-write snapshots, with an `fsfreeze`
//! fallback for a mounted non-LVM filesystem, and finally a crash-consistent
//! read of the live device when neither is possible.
//!
//! This is the Linux counterpart of the Windows VSS provider. Creating an LVM
//! snapshot or freezing a filesystem needs root and the relevant tools
//! (`lvm2`, `util-linux`); when they are unavailable — or the source is a plain
//! file / unmounted device — [`create_snapshot`](LinuxSnapshotProvider::create_snapshot)
//! degrades gracefully to a crash-consistent copy (mode `none`) and never fails
//! the backup. Command construction and `/proc/mounts` parsing are pure and
//! unit-tested; the actual privileged calls are exercised on a real host.

#![cfg(target_os = "linux")]

use std::collections::HashMap;

use async_trait::async_trait;
use chrono::Utc;
use tokio::process::Command;
use tracing::{info, warn};
use uuid::Uuid;

use own_backup_common::{OwnBackupError, Result};

use crate::traits::{Snapshot, SnapshotProvider};

const PROVIDER: &str = "linux";

/// Filesystems whose driver implements the freeze ioctl (`FIFREEZE`).
const FREEZABLE: &[&str] = &["ext2", "ext3", "ext4", "xfs", "btrfs", "reiserfs", "jfs", "f2fs"];

/// Snapshot provider using LVM snapshots / `fsfreeze`.
pub struct LinuxSnapshotProvider;

impl LinuxSnapshotProvider {
    pub fn new() -> Self {
        LinuxSnapshotProvider
    }
}

impl Default for LinuxSnapshotProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SnapshotProvider for LinuxSnapshotProvider {
    fn name(&self) -> &str {
        PROVIDER
    }

    async fn create_snapshot(&self, volume: &str) -> Result<Snapshot> {
        // 1) LVM snapshot (the real, stable point-in-time view).
        match try_lvm_snapshot(volume).await {
            Ok(Some(snap)) => return Ok(snap),
            Ok(None) => {}
            Err(e) => warn!(volume, error = %e, "LVM snapshot failed; trying fsfreeze"),
        }
        // 2) Freeze a mounted filesystem so the live device reads consistently.
        match try_fsfreeze(volume).await {
            Ok(Some(snap)) => return Ok(snap),
            Ok(None) => {}
            Err(e) => warn!(volume, error = %e, "fsfreeze failed; crash-consistent copy"),
        }
        // 3) Crash-consistent copy of the live device.
        info!(volume, "no snapshot facility available — crash-consistent copy");
        Ok(none_snapshot(volume))
    }

    async fn release_snapshot(&self, snapshot: &Snapshot) -> Result<()> {
        match snapshot.metadata.get("mode").map(String::as_str) {
            Some("lvm") => {
                let lv = snapshot
                    .metadata
                    .get("lv_snapshot")
                    .ok_or_else(|| bad("lvm snapshot missing its lv_snapshot metadata"))?;
                run("lvremove", &["-f", lv]).await?;
                info!(lv = %lv, "LVM snapshot removed");
                Ok(())
            }
            Some("fsfreeze") => {
                let mount = snapshot
                    .metadata
                    .get("mount")
                    .ok_or_else(|| bad("fsfreeze snapshot missing its mount metadata"))?;
                run("fsfreeze", &["-u", mount]).await?;
                info!(mount = %mount, "filesystem thawed");
                Ok(())
            }
            _ => Ok(()), // crash-consistent: nothing to release
        }
    }
}

fn bad(msg: impl std::fmt::Display) -> OwnBackupError {
    OwnBackupError::Device(format!("linux snapshot: {msg}"))
}

/// A crash-consistent "snapshot" that simply reads the live device.
fn none_snapshot(volume: &str) -> Snapshot {
    let mut metadata = HashMap::new();
    metadata.insert("mode".into(), "none".into());
    Snapshot {
        provider_name: PROVIDER.into(),
        source_device: volume.into(),
        snapshot_device_path: volume.into(),
        created_at: Utc::now(),
        metadata,
    }
}

/// Run a command, returning its stdout on success or an error with stderr.
async fn run(cmd: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .await
        .map_err(|e| bad(format!("cannot run {cmd}: {e}")))?;
    if !out.status.success() {
        return Err(bad(format!(
            "`{cmd} {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

// ---------------------------------------------------------------------------
// LVM
// ---------------------------------------------------------------------------

/// Create an LVM snapshot of `volume` if it is a logical volume; `Ok(None)` if
/// it is not an LV (so the caller tries the next strategy).
async fn try_lvm_snapshot(volume: &str) -> Result<Option<Snapshot>> {
    // `lvs` identifies an LV and its VG; a non-LV exits non-zero.
    let listed = match run("lvs", &["--noheadings", "-o", "vg_name,lv_name", volume]).await {
        Ok(s) => s,
        Err(_) => return Ok(None), // not an LV (or lvm2 absent) → not an error
    };
    let Some((vg, lv)) = parse_lvs(&listed) else {
        return Ok(None);
    };

    let snap = format!("obk-snap-{}", &Uuid::new_v4().simple().to_string()[..12]);
    let args = lvcreate_args(&vg, &lv, &snap);
    run("lvcreate", &args.iter().map(String::as_str).collect::<Vec<_>>()).await?;

    let dev = snapshot_device_path(&vg, &snap);
    info!(origin = %format!("{vg}/{lv}"), snapshot = %dev, "LVM snapshot created");
    let mut metadata = HashMap::new();
    metadata.insert("mode".into(), "lvm".into());
    metadata.insert("lv_snapshot".into(), format!("{vg}/{snap}"));
    Ok(Some(Snapshot {
        provider_name: PROVIDER.into(),
        source_device: volume.into(),
        snapshot_device_path: dev,
        created_at: Utc::now(),
        metadata,
    }))
}

/// Parse `lvs --noheadings -o vg_name,lv_name` output → `(vg, lv)`.
fn parse_lvs(out: &str) -> Option<(String, String)> {
    let mut it = out.split_whitespace();
    let vg = it.next()?.to_string();
    let lv = it.next()?.to_string();
    if vg.is_empty() || lv.is_empty() {
        return None;
    }
    Some((vg, lv))
}

/// Build the `lvcreate` snapshot arguments. The copy-on-write area is sized at
/// 20% of the origin — enough for the writes expected during a backup.
fn lvcreate_args(vg: &str, lv: &str, snap: &str) -> Vec<String> {
    vec![
        "--snapshot".into(),
        "--name".into(),
        snap.into(),
        "--extents".into(),
        "20%ORIGIN".into(),
        format!("{vg}/{lv}"),
    ]
}

/// Device path of a snapshot LV.
fn snapshot_device_path(vg: &str, snap: &str) -> String {
    format!("/dev/{vg}/{snap}")
}

// ---------------------------------------------------------------------------
// fsfreeze
// ---------------------------------------------------------------------------

/// Freeze the filesystem mounted from `volume` (if any) so the live device
/// reads consistently. `Ok(None)` if the device is not mounted / not freezable.
async fn try_fsfreeze(volume: &str) -> Result<Option<Snapshot>> {
    let mounts = tokio::fs::read_to_string("/proc/mounts")
        .await
        .map_err(|e| bad(format!("cannot read /proc/mounts: {e}")))?;

    // Match the device path and its canonical form (e.g. /dev/mapper vs /dev/dm).
    let mut candidates = vec![volume.to_string()];
    if let Ok(canon) = tokio::fs::canonicalize(volume).await {
        candidates.push(canon.to_string_lossy().into_owned());
    }
    let Some((mount, fstype)) = find_mount(&mounts, &candidates) else {
        return Ok(None);
    };
    if !FREEZABLE.contains(&fstype.as_str()) {
        return Ok(None);
    }

    run("fsfreeze", &["-f", &mount]).await?;
    info!(mount = %mount, fstype = %fstype, "filesystem frozen");
    let mut metadata = HashMap::new();
    metadata.insert("mode".into(), "fsfreeze".into());
    metadata.insert("mount".into(), mount);
    Ok(Some(Snapshot {
        provider_name: PROVIDER.into(),
        source_device: volume.into(),
        // The device itself is read while the filesystem is frozen.
        snapshot_device_path: volume.into(),
        created_at: Utc::now(),
        metadata,
    }))
}

/// Find the mount point + filesystem type for any of `devices` in
/// `/proc/mounts` content. Fields are space-separated; the mount point may
/// contain octal escapes (`\040` for space, etc.).
fn find_mount(proc_mounts: &str, devices: &[String]) -> Option<(String, String)> {
    for line in proc_mounts.lines() {
        let mut f = line.split_whitespace();
        let dev = f.next()?;
        let mount = f.next()?;
        let fstype = f.next()?;
        if devices.iter().any(|d| d == dev) {
            return Some((decode_octal(mount), fstype.to_string()));
        }
    }
    None
}

/// Decode `/proc/mounts` octal escapes (`\040`, `\011`, `\012`, `\134`).
fn decode_octal(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            let oct = &s[i + 1..i + 4];
            if let Ok(v) = u8::from_str_radix(oct, 8) {
                out.push(v as char);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_lvs_extracts_vg_and_lv() {
        assert_eq!(
            parse_lvs("  ubuntu-vg  root  \n"),
            Some(("ubuntu-vg".into(), "root".into()))
        );
        assert_eq!(parse_lvs(""), None);
        assert_eq!(parse_lvs("   "), None);
    }

    #[test]
    fn lvcreate_args_are_well_formed() {
        let a = lvcreate_args("vg0", "data", "obk-snap-abc");
        assert_eq!(
            a,
            vec![
                "--snapshot",
                "--name",
                "obk-snap-abc",
                "--extents",
                "20%ORIGIN",
                "vg0/data"
            ]
        );
        assert_eq!(snapshot_device_path("vg0", "obk-snap-abc"), "/dev/vg0/obk-snap-abc");
    }

    #[test]
    fn find_mount_matches_device_and_decodes_spaces() {
        let mounts = "\
/dev/sda1 / ext4 rw,relatime 0 0
/dev/mapper/vg0-data /mnt/my\\040disk xfs rw 0 0
tmpfs /run tmpfs rw 0 0
";
        assert_eq!(
            find_mount(mounts, &["/dev/sda1".into()]),
            Some(("/".into(), "ext4".into()))
        );
        assert_eq!(
            find_mount(mounts, &["/dev/mapper/vg0-data".into()]),
            Some(("/mnt/my disk".into(), "xfs".into()))
        );
        assert_eq!(find_mount(mounts, &["/dev/sdz9".into()]), None);
    }

    #[test]
    fn decode_octal_handles_escapes_and_plain() {
        assert_eq!(decode_octal("/mnt/plain"), "/mnt/plain");
        assert_eq!(decode_octal("/a\\040b\\011c"), "/a b\tc");
    }

    /// On a plain file (not an LV, not mounted) the provider degrades to a
    /// crash-consistent copy without error — the path this exercises on a
    /// non-root host.
    #[tokio::test]
    async fn falls_back_to_crash_consistent_for_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("plain.img");
        std::fs::write(&file, [0u8; 4096]).unwrap();

        let p = LinuxSnapshotProvider::new();
        let snap = p.create_snapshot(file.to_str().unwrap()).await.unwrap();
        assert_eq!(snap.metadata.get("mode").map(String::as_str), Some("none"));
        assert_eq!(snap.snapshot_device_path, file.to_str().unwrap());
        // Releasing a crash-consistent snapshot is a no-op that must succeed.
        p.release_snapshot(&snap).await.unwrap();
    }
}
