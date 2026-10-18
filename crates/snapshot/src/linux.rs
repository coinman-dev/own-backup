//! Linux snapshot provider.
//!
//! Linux has no VSS. A volume can be copied as one consistent whole when
//! nothing writes to it — it is not mounted read-write, it is not swap, and no
//! device is stacked on it (an opened LUKS container, an LVM physical volume) —
//! or through an LVM snapshot when it is a logical volume. Anything else is
//! refused, with the reason: reading a filesystem while it is written gives an
//! image that may not even mount, and freezing it for the length of the copy —
//! what this provider used to do — can hang the whole machine when the frozen
//! filesystem is the root, where the service keeps its own data and log.
//! A device-mapper snapshot of an opened LUKS container is the planned next
//! step (`.ai/PLAN.md` §6).
//!
//! Command construction and the decisions are pure and unit-tested; `lvcreate`
//! and `lvremove` need root and are exercised on a real host.

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::path::Path;

use async_trait::async_trait;
use chrono::Utc;
use tokio::process::Command;
use tracing::{info, warn};
use uuid::Uuid;

use own_backup_common::linux_block::{self, Usage};
use own_backup_common::{tr, OwnBackupError, Result};

use crate::traits::{Snapshot, SnapshotProvider};

const PROVIDER: &str = "linux";

/// Name prefix of the snapshot volumes this provider creates — and the only
/// ones [`remove_orphaned_snapshots`] ever removes.
pub const SNAPSHOT_PREFIX: &str = "obk-snap-";

/// How a volume can be read as one point in time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Assessment {
    /// Not a block device (an image file): read as it is.
    File,
    /// Nothing uses it: read the device itself.
    Idle,
    /// Only read-only mounts on it: read the device itself.
    ReadOnly,
    /// In use, but a logical volume: take an LVM snapshot.
    Lvm { vg: String, lv: String, thin: bool },
    /// In use, and there is no way to hold a point in time.
    InUse(Usage),
}

/// Decide how `volume` can be copied consistently.
pub async fn assess(volume: &str) -> Result<Assessment> {
    let usage = linux_block::device_usage(Path::new(volume))
        .map_err(|e| bad(format!("cannot tell what uses {volume}: {e}")))?;
    let Some(usage) = usage else {
        return Ok(Assessment::File);
    };
    Ok(decide(usage, lv_of(volume).await))
}

/// The decision itself: idle and read-only devices are read directly, a busy
/// logical volume through a snapshot, anything else busy is refused.
fn decide(usage: Usage, lv: Option<(String, String, bool)>) -> Assessment {
    if usage.idle() {
        Assessment::Idle
    } else if !usage.in_use() {
        Assessment::ReadOnly
    } else if let Some((vg, lv, thin)) = lv {
        Assessment::Lvm { vg, lv, thin }
    } else {
        Assessment::InUse(usage)
    }
}

/// Snapshot provider for Linux volumes (see the module comment).
pub struct LinuxSnapshotProvider;

impl LinuxSnapshotProvider {
    pub fn new() -> Self {
        LinuxSnapshotProvider
    }

    /// Is the snapshot still whole? A classic LVM snapshot whose copy-on-write
    /// area filled up during the copy is invalidated by the kernel, and what
    /// was read from it is not one point in time. Ask before releasing it.
    pub async fn check_intact(&self, snapshot: &Snapshot) -> Result<()> {
        let Some(lv) = snapshot.metadata.get("lv_snapshot") else {
            return Ok(());
        };
        let attr = run("lvs", &["--noheadings", "-o", "lv_attr", lv]).await?;
        if snapshot_invalid(&attr) {
            return Err(OwnBackupError::NoSnapshotProvider(
                tr!("linux.snapshot_overflowed", volume = snapshot.source_device).into_owned(),
            ));
        }
        Ok(())
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
        match assess(volume).await? {
            Assessment::File => Ok(direct(volume, "file")),
            Assessment::Idle => Ok(direct(volume, "idle")),
            Assessment::ReadOnly => Ok(direct(volume, "readonly")),
            Assessment::Lvm { vg, lv, thin } => lvm_snapshot(volume, &vg, &lv, thin).await,
            Assessment::InUse(usage) => Err(OwnBackupError::NoSnapshotProvider(
                tr!(
                    "linux.in_use",
                    volume = volume,
                    users = describe_usage(&usage)
                )
                .into_owned(),
            )),
        }
    }

    async fn release_snapshot(&self, snapshot: &Snapshot) -> Result<()> {
        if let Some(lv) = snapshot.metadata.get("lv_snapshot") {
            run("lvremove", &["-f", lv]).await?;
            info!(lv = %lv, "LVM snapshot removed");
        }
        Ok(())
    }
}

/// Remove the snapshot volumes a killed run left behind. Called at service
/// start, before any run can create a new one; a snapshot left in place keeps
/// growing with every write to its origin until it fills its volume group.
pub async fn remove_orphaned_snapshots() -> usize {
    let Ok(listed) = run("lvs", &["--noheadings", "-o", "vg_name,lv_name"]).await else {
        return 0; // no lvm2, no permission, or no volume groups
    };
    let mut removed = 0;
    for (vg, lv) in orphaned_snapshots(&listed) {
        let name = format!("{vg}/{lv}");
        match run("lvremove", &["-f", &name]).await {
            Ok(_) => {
                info!(lv = %name, "removed an LVM snapshot left by an interrupted run");
                removed += 1;
            }
            Err(e) => warn!(lv = %name, error = %e, "cannot remove a leftover LVM snapshot"),
        }
    }
    removed
}

fn orphaned_snapshots(lvs_output: &str) -> Vec<(String, String)> {
    lvs_output
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            Some((f.next()?.to_string(), f.next()?.to_string()))
        })
        .filter(|(_, lv)| lv.starts_with(SNAPSHOT_PREFIX))
        .collect()
}

fn bad(msg: impl std::fmt::Display) -> OwnBackupError {
    OwnBackupError::Device(format!("linux snapshot: {msg}"))
}

/// The device itself is read: nothing writes to it.
fn direct(volume: &str, mode: &str) -> Snapshot {
    let mut metadata = HashMap::new();
    metadata.insert("mode".into(), mode.into());
    Snapshot {
        provider_name: PROVIDER.into(),
        source_device: volume.into(),
        snapshot_device_path: volume.into(),
        created_at: Utc::now(),
        metadata,
    }
}

/// What uses a busy volume, for the refusal: mount points, swap, and the
/// device-mapper names stacked on it.
fn describe_usage(usage: &Usage) -> String {
    let mut parts = Vec::new();
    if !usage.mounted_rw.is_empty() {
        parts.push(tr!("linux.usage_mounted", mounts = usage.mounted_rw.join(", ")).into_owned());
    }
    if !usage.swap.is_empty() {
        parts.push(tr!("linux.usage_swap").into_owned());
    }
    if !usage.holders.is_empty() {
        let names: Vec<String> = usage.holders.iter().map(|h| dm_name(h)).collect();
        parts.push(tr!("linux.usage_holders", holders = names.join(", ")).into_owned());
    }
    parts.join("; ")
}

/// `/dev/mapper` name of a device-mapper device (`dm-0` → `luks-…`), else the
/// kernel name as it is.
fn dm_name(kernel: &str) -> String {
    std::fs::read_to_string(format!("{}/{kernel}/dm/name", linux_block::SYS_CLASS_BLOCK))
        .map(|n| n.trim().to_string())
        .ok()
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| kernel.to_string())
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

/// `(vg, lv, thin)` when `volume` is an LVM logical volume; `None` otherwise
/// (including when lvm2 is not installed).
async fn lv_of(volume: &str) -> Option<(String, String, bool)> {
    let listed = run(
        "lvs",
        &["--noheadings", "-o", "vg_name,lv_name,segtype", volume],
    )
    .await
    .ok()?;
    parse_lvs(&listed)
}

/// Parse `lvs --noheadings -o vg_name,lv_name,segtype` → `(vg, lv, thin)`.
fn parse_lvs(out: &str) -> Option<(String, String, bool)> {
    let mut it = out.split_whitespace();
    let vg = it.next()?.to_string();
    let lv = it.next()?.to_string();
    let thin = it.next() == Some("thin");
    Some((vg, lv, thin))
}

async fn lvm_snapshot(volume: &str, vg: &str, lv: &str, thin: bool) -> Result<Snapshot> {
    let snap = format!(
        "{SNAPSHOT_PREFIX}{}",
        &Uuid::new_v4().simple().to_string()[..12]
    );
    let args = lvcreate_args(vg, lv, &snap, thin);
    run(
        "lvcreate",
        &args.iter().map(String::as_str).collect::<Vec<_>>(),
    )
    .await
    .map_err(|e| {
        OwnBackupError::NoSnapshotProvider(
            tr!("linux.lvm_failed", volume = volume, error = e).into_owned(),
        )
    })?;

    let dev = snapshot_device_path(vg, &snap);
    info!(origin = %format!("{vg}/{lv}"), snapshot = %dev, thin, "LVM snapshot created");
    let mut metadata = HashMap::new();
    metadata.insert("mode".into(), "lvm".into());
    metadata.insert("lv_snapshot".into(), format!("{vg}/{snap}"));
    Ok(Snapshot {
        provider_name: PROVIDER.into(),
        source_device: volume.into(),
        snapshot_device_path: dev,
        created_at: Utc::now(),
        metadata,
    })
}

/// `lvcreate` arguments. A thin snapshot takes no size (it shares the pool)
/// and is created with activation skip, so it must be asked to activate; a
/// classic one gets a copy-on-write area of 20% of the origin, which
/// [`LinuxSnapshotProvider::check_intact`] checks did not overflow.
fn lvcreate_args(vg: &str, lv: &str, snap: &str, thin: bool) -> Vec<String> {
    let mut args = vec!["--snapshot".into(), "--name".into(), snap.into()];
    if thin {
        args.extend(["--setactivationskip".into(), "n".into()]);
    } else {
        args.extend(["--extents".into(), "20%ORIGIN".into()]);
    }
    args.push(format!("{vg}/{lv}"));
    args
}

/// Device path of a snapshot LV.
fn snapshot_device_path(vg: &str, snap: &str) -> String {
    format!("/dev/{vg}/{snap}")
}

/// `lv_attr` of an invalidated (overflowed) snapshot has `I` as its fifth
/// character, the volume state.
fn snapshot_invalid(lv_attr: &str) -> bool {
    lv_attr.trim().chars().nth(4) == Some('I')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_lvs_extracts_vg_lv_and_thin() {
        assert_eq!(
            parse_lvs("  ubuntu-vg  root  linear\n"),
            Some(("ubuntu-vg".into(), "root".into(), false))
        );
        assert_eq!(
            parse_lvs("  vg0  data  thin\n"),
            Some(("vg0".into(), "data".into(), true))
        );
        assert_eq!(parse_lvs(""), None);
        assert_eq!(parse_lvs("   "), None);
    }

    #[test]
    fn lvcreate_args_classic_and_thin() {
        assert_eq!(
            lvcreate_args("vg0", "data", "obk-snap-abc", false),
            vec![
                "--snapshot",
                "--name",
                "obk-snap-abc",
                "--extents",
                "20%ORIGIN",
                "vg0/data"
            ]
        );
        assert_eq!(
            lvcreate_args("vg0", "data", "obk-snap-abc", true),
            vec![
                "--snapshot",
                "--name",
                "obk-snap-abc",
                "--setactivationskip",
                "n",
                "vg0/data"
            ]
        );
        assert_eq!(
            snapshot_device_path("vg0", "obk-snap-abc"),
            "/dev/vg0/obk-snap-abc"
        );
    }

    #[test]
    fn overflowed_snapshot_is_detected() {
        assert!(snapshot_invalid("  swi-I-s---\n"));
        assert!(!snapshot_invalid("  swi-a-s---\n"));
        assert!(!snapshot_invalid(""));
    }

    #[test]
    fn only_our_snapshots_are_orphans() {
        let lvs = "  vg0  root\n  vg0  obk-snap-0123456789ab\n  vg1  obk-snapshot-mine\n  vg1  obk-snap-ffffffffffff\n";
        assert_eq!(
            orphaned_snapshots(lvs),
            vec![
                ("vg0".to_string(), "obk-snap-0123456789ab".to_string()),
                ("vg1".to_string(), "obk-snap-ffffffffffff".to_string()),
            ]
        );
    }

    #[test]
    fn decisions() {
        let idle = Usage::default();
        assert_eq!(decide(idle.clone(), None), Assessment::Idle);
        // An idle LV needs no snapshot either.
        assert_eq!(
            decide(idle, Some(("vg".into(), "lv".into(), false))),
            Assessment::Idle
        );
        let ro = Usage {
            mounted_ro: vec!["/mnt".into()],
            ..Usage::default()
        };
        assert_eq!(decide(ro, None), Assessment::ReadOnly);
        let busy = Usage {
            mounted_rw: vec!["/".into()],
            holders: vec!["dm-0".into()],
            ..Usage::default()
        };
        assert_eq!(
            decide(busy.clone(), Some(("vg".into(), "root".into(), true))),
            Assessment::Lvm {
                vg: "vg".into(),
                lv: "root".into(),
                thin: true
            }
        );
        assert_eq!(decide(busy.clone(), None), Assessment::InUse(busy));
    }

    #[test]
    fn refusal_names_what_uses_the_volume() {
        let text = describe_usage(&Usage {
            mounted_rw: vec!["/".into(), "/home".into()],
            swap: vec!["/dev/dm-1".into()],
            holders: vec!["no-such-dm-device".into()],
            ..Usage::default()
        });
        assert!(text.contains("/, /home"), "{text}");
        assert!(text.contains("no-such-dm-device"), "{text}");
    }

    /// An image file is read as it is, and releasing that is a no-op.
    #[tokio::test]
    async fn a_file_is_read_directly() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("plain.img");
        std::fs::write(&file, [0u8; 4096]).unwrap();

        let p = LinuxSnapshotProvider::new();
        let snap = p.create_snapshot(file.to_str().unwrap()).await.unwrap();
        assert_eq!(snap.metadata.get("mode").map(String::as_str), Some("file"));
        assert_eq!(snap.snapshot_device_path, file.to_str().unwrap());
        p.check_intact(&snap).await.unwrap();
        p.release_snapshot(&snap).await.unwrap();
    }
}
