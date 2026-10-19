//! Linux snapshot provider.
//!
//! Linux has no VSS. A volume can be copied as one consistent whole when
//! nothing writes to it — it is not mounted read-write, it is not swap, and no
//! device is stacked on it (an opened LUKS container, an LVM physical volume) —
//! or through a snapshot: an LVM snapshot when it is a logical volume, a
//! device-mapper snapshot built by hand when it is another device-mapper device
//! with the filesystem directly on it — above all an opened LUKS container
//! (`dm_snapshot.rs`). Anything else in use is refused, with the reason:
//! reading a filesystem while it is written gives an image that may not even
//! mount, and freezing it for the length of the copy — what this provider used
//! to do — can hang the whole machine when the frozen filesystem is the root,
//! where the service keeps its own data and log. A busy plain partition would
//! need a kernel module and stays refused.
//!
//! Command construction and the decisions are pure and unit-tested; `lvcreate`
//! and `lvremove` need root and are exercised on a real host.

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::Utc;
use tokio::process::Command;
use tracing::{info, warn};
use uuid::Uuid;

use own_backup_common::linux_block::{self, Usage};
use own_backup_common::{tr, OwnBackupError, Result};

use crate::dm_snapshot::{self, ContainerKey, Session, SnapState};
use crate::traits::{Snapshot, SnapshotProvider};

const PROVIDER: &str = "linux";

/// Name prefix of the snapshot volumes this provider creates — and the only
/// ones [`remove_orphaned_snapshots`] ever removes. A device-mapper snapshot
/// is `obk-snap-<id>` too, with its layer `obk-real-<id>`.
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
    /// In use, a device-mapper device that is not a logical volume and has
    /// nothing stacked on it (an opened LUKS container with the filesystem
    /// mounted from it): take a device-mapper snapshot.
    DeviceMapper { kernel: String },
    /// In use, and there is no way to hold a point in time.
    InUse(Usage),
}

/// What device-mapper says a device is: its kernel name (`dm-3`) and DM UUID
/// (`CRYPT-LUKS2-…`, `LVM-…`; empty when it has none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DmIdentity {
    pub kernel: String,
    pub uuid: String,
}

/// Decide how `volume` can be copied consistently.
pub async fn assess(volume: &str) -> Result<Assessment> {
    let usage = linux_block::device_usage(Path::new(volume))
        .map_err(|e| bad(format!("cannot tell what uses {volume}: {e}")))?;
    let Some(usage) = usage else {
        return Ok(Assessment::File);
    };
    Ok(decide(usage, lv_of(volume).await, dm_of(volume)))
}

/// The decision itself: idle and read-only devices are read directly, a busy
/// logical volume through an LVM snapshot, another busy device-mapper device
/// with nothing stacked on it through a device-mapper snapshot, anything else
/// busy is refused.
fn decide(usage: Usage, lv: Option<(String, String, bool)>, dm: Option<DmIdentity>) -> Assessment {
    if usage.idle() {
        Assessment::Idle
    } else if !usage.in_use() {
        Assessment::ReadOnly
    } else if let Some((vg, lv, thin)) = lv {
        Assessment::Lvm { vg, lv, thin }
    } else if let Some(dm) = dm.filter(|d| {
        // An LV without lvm2 to snapshot it, or one of our own layers, is not
        // taken; nor a device with others stacked on it: freezing it would
        // not freeze their filesystems.
        !d.uuid.starts_with("LVM-") && !d.uuid.starts_with("OBK-") && usage.holders.is_empty()
    }) {
        Assessment::DeviceMapper { kernel: dm.kernel }
    } else {
        Assessment::InUse(usage)
    }
}

/// The device-mapper identity of `volume`, when it is a device-mapper device.
fn dm_of(volume: &str) -> Option<DmIdentity> {
    let kernel = linux_block::kernel_name(Path::new(volume))?;
    if !kernel.starts_with("dm-") {
        return None;
    }
    let uuid =
        std::fs::read_to_string(format!("{}/{kernel}/dm/uuid", linux_block::SYS_CLASS_BLOCK))
            .map(|u| u.trim().to_string())
            .unwrap_or_default();
    Some(DmIdentity { kernel, uuid })
}

/// Was `snapshot` captured with its filesystem frozen? An LVM snapshot and a
/// device-mapper snapshot both suspend the origin with the filesystem frozen
/// for the instant they are taken, so what they show is quiesced: metadata
/// written back, a journal at most "covered". A device read directly is not.
pub fn taken_frozen(snapshot: &Snapshot) -> bool {
    matches!(
        snapshot.metadata.get("mode").map(String::as_str),
        Some("lvm" | "dm")
    )
}

/// Metadata key by which a caller vouches that the file a snapshot reads holds
/// still for the length of the copy — see [`assert_file_frozen`].
pub const FILE_FROZEN: &str = "file_frozen";

/// Vouch that the image file `snapshot` reads is not written while it is
/// copied (decision R5). Only the caller can know that: a file does not say
/// who else has it open. A snapshot of a device is left as it is.
pub fn assert_file_frozen(snapshot: &mut Snapshot) {
    if snapshot.metadata.get("mode").map(String::as_str) == Some("file") {
        snapshot.metadata.insert(FILE_FROZEN.into(), "true".into());
    }
}

/// Does what `snapshot` reads hold still while it is read, so that its raw
/// metadata (the MFT, ext, btrfs and xfs structures) may be parsed?
///
/// * `lvm`, `dm` — a snapshot: yes.
/// * `idle` — nothing used the device when it was assessed, and the reader
///   holds it `O_EXCL` for the length of the copy (decision R5), so nothing
///   can mount it meanwhile. This answer counts on that hold.
/// * `readonly` — only read-only mounts, which do not write.
/// * `file` — only when the caller vouched for it ([`assert_file_frozen`]).
///
/// Not the question [`taken_frozen`] answers: an idle device holds still, but
/// nobody froze its filesystem, and a journal on it may still say the metadata
/// is behind.
pub fn is_frozen(snapshot: &Snapshot) -> bool {
    match snapshot.metadata.get("mode").map(String::as_str) {
        Some("lvm" | "dm" | "idle" | "readonly") => true,
        Some("file") => snapshot
            .metadata
            .get(FILE_FROZEN)
            .is_some_and(|v| v == "true"),
        _ => false,
    }
}

/// [`LinuxSnapshotProvider::check_intact`] for a thread that may block and has
/// no runtime — the backup engine's, right before it publishes an image: a copy
/// read from a snapshot that broke during the copy must not become a version.
/// The kernel is asked directly (device-mapper status, `lvs`), so it works for
/// a snapshot whichever provider took it. A state that cannot be read is an
/// error, never "intact".
pub fn check_intact_blocking(snapshot: &Snapshot) -> Result<()> {
    if let Some(snap) = snapshot.metadata.get("dm_snapshot") {
        return dm_verdict(snapshot, dm_snapshot::snapshot_state(snap));
    }
    let Some(lv) = snapshot.metadata.get("lv_snapshot") else {
        return Ok(());
    };
    lv_verdict(
        snapshot,
        &run_blocking("lvs", &["--noheadings", "-o", "lv_attr", lv])?,
    )
}

/// Is a device-mapper snapshot in `state` still one point in time?
fn dm_verdict(snapshot: &Snapshot, state: std::io::Result<SnapState>) -> Result<()> {
    match state {
        Ok(s) if s.intact() => Ok(()),
        Ok(s) => Err(OwnBackupError::NoSnapshotProvider(
            tr!(
                "linux.dm_snapshot_invalid",
                volume = snapshot.source_device,
                state = format!("{s:?}")
            )
            .into_owned(),
        )),
        Err(e) => Err(bad(format!("cannot read the snapshot's state: {e}"))),
    }
}

/// Is an LVM snapshot whose `lv_attr` is `attr` still one point in time?
fn lv_verdict(snapshot: &Snapshot, attr: &str) -> Result<()> {
    if snapshot_invalid(attr) {
        return Err(OwnBackupError::NoSnapshotProvider(
            tr!("linux.snapshot_overflowed", volume = snapshot.source_device).into_owned(),
        ));
    }
    Ok(())
}

/// Snapshot provider for Linux volumes (see the module comment).
#[derive(Default)]
pub struct LinuxSnapshotProvider {
    /// Where a device-mapper snapshot keeps its copy-on-write store: the
    /// destination folder of the copy.
    cow_dir: Option<PathBuf>,
    container_key: Option<Arc<ContainerKey>>,
    /// Device-mapper snapshots held by this provider, by id.
    sessions: Mutex<HashMap<String, Session>>,
}

impl LinuxSnapshotProvider {
    /// A provider that reads idle volumes and takes LVM snapshots, but has no
    /// place for a device-mapper snapshot's store.
    pub fn new() -> Self {
        Self::default()
    }

    /// A provider that can also take a device-mapper snapshot, with its
    /// copy-on-write store in `dir` — the destination folder of the copy. The
    /// store grows with what is written to the volume during the copy and is
    /// deleted on release.
    pub fn with_cow_dir(dir: impl Into<PathBuf>) -> Self {
        LinuxSnapshotProvider {
            cow_dir: Some(dir.into()),
            ..Self::default()
        }
    }

    /// How to recover the volume key of an opened LUKS container when the
    /// kernel keeps it to itself (see [`ContainerKey`]).
    pub fn container_key(
        mut self,
        f: impl Fn(&Path) -> Result<Option<zeroize::Zeroizing<Vec<u8>>>> + Send + Sync + 'static,
    ) -> Self {
        self.container_key = Some(Arc::new(f));
        self
    }

    /// Is the snapshot still whole? A snapshot whose copy-on-write area filled
    /// up (or could not be written) during the copy is invalidated by the
    /// kernel, and what was read from it is not one point in time. Ask before
    /// releasing it.
    pub async fn check_intact(&self, snapshot: &Snapshot) -> Result<()> {
        if let Some(id) = snapshot.metadata.get("dm_id") {
            let state = {
                let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
                sessions.get(id).map(Session::state)
            };
            return match state {
                Some(state) => dm_verdict(snapshot, state),
                None => Err(bad(format!("snapshot {id} is not held by this provider"))),
            };
        }
        let Some(lv) = snapshot.metadata.get("lv_snapshot") else {
            return Ok(());
        };
        let attr = run("lvs", &["--noheadings", "-o", "lv_attr", lv]).await?;
        lv_verdict(snapshot, &attr)
    }

    async fn dm_snapshot(&self, volume: &str, kernel: String) -> Result<Snapshot> {
        let Some(dir) = self.cow_dir.clone() else {
            return Err(OwnBackupError::NoSnapshotProvider(
                tr!("linux.dm_no_cow_dir", volume = volume).into_owned(),
            ));
        };
        let key = self.container_key.clone();
        let vol = volume.to_string();
        // Blocking: suspends a device and waits for it to flush.
        let session = tokio::task::spawn_blocking(move || {
            let rw = linux_block::device_usage(Path::new(&vol))
                .ok()
                .flatten()
                .map(|u| u.mounted_rw)
                .unwrap_or_default();
            dm_snapshot::create(&vol, &kernel, &rw, &dir, key.as_deref())
        })
        .await
        .map_err(|e| bad(format!("snapshot task: {e}")))??;

        // Names and paths only: never the table or the key.
        let mut metadata = HashMap::new();
        metadata.insert("mode".into(), "dm".into());
        metadata.insert("dm_id".into(), session.id.clone());
        metadata.insert("dm_origin".into(), session.origin.clone());
        metadata.insert("dm_snapshot".into(), session.snap.clone());
        metadata.insert("cow_file".into(), session.cow_path.display().to_string());
        let snapshot = Snapshot {
            provider_name: PROVIDER.into(),
            source_device: volume.into(),
            snapshot_device_path: session.snap_path.clone(),
            created_at: Utc::now(),
            metadata,
        };
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session.id.clone(), session);
        Ok(snapshot)
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
            Assessment::DeviceMapper { kernel } => self.dm_snapshot(volume, kernel).await,
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
        if let Some(id) = snapshot.metadata.get("dm_id") {
            let session = self
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(id);
            let id = id.clone();
            return tokio::task::spawn_blocking(move || match session {
                Some(s) => s.release(),
                // Not held here (another provider created it): undo it from
                // what the kernel shows.
                None => dm_snapshot::release_by_id(&id),
            })
            .await
            .map_err(|e| bad(format!("snapshot task: {e}")))?;
        }
        if let Some(lv) = snapshot.metadata.get("lv_snapshot") {
            run("lvremove", &["-f", lv]).await?;
            info!(lv = %lv, "LVM snapshot removed");
        }
        Ok(())
    }
}

/// Remove the snapshots killed runs left behind: LVM snapshot volumes, and
/// device-mapper snapshots — each origin put back on its own table, the
/// layers removed, their loop devices detached and stores deleted (a
/// device-mapper snapshot its run still holds is left alone). Called at
/// service start, before any run can create a new one, and by `own-backup-cli
/// cleanup-snapshots`; a snapshot left in place keeps growing with every write
/// to its origin.
pub async fn remove_orphaned_snapshots() -> usize {
    let dm = tokio::task::spawn_blocking(dm_snapshot::remove_orphans)
        .await
        .unwrap_or(0);
    let Ok(listed) = run("lvs", &["--noheadings", "-o", "vg_name,lv_name"]).await else {
        return dm; // no lvm2, no permission, or no volume groups
    };
    let mut removed = dm;
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
    stdout_of(cmd, args, out)
}

/// [`run`] on a thread with no runtime.
fn run_blocking(cmd: &str, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| bad(format!("cannot run {cmd}: {e}")))?;
    stdout_of(cmd, args, out)
}

fn stdout_of(cmd: &str, args: &[&str], out: std::process::Output) -> Result<String> {
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

    fn dm(kernel: &str, uuid: &str) -> Option<DmIdentity> {
        Some(DmIdentity {
            kernel: kernel.into(),
            uuid: uuid.into(),
        })
    }

    #[test]
    fn decisions() {
        let idle = Usage::default();
        assert_eq!(decide(idle.clone(), None, None), Assessment::Idle);
        // An idle LV or LUKS mapping needs no snapshot either.
        assert_eq!(
            decide(idle.clone(), Some(("vg".into(), "lv".into(), false)), None),
            Assessment::Idle
        );
        assert_eq!(
            decide(idle, None, dm("dm-0", "CRYPT-LUKS2-x")),
            Assessment::Idle
        );
        let ro = Usage {
            mounted_ro: vec!["/mnt".into()],
            ..Usage::default()
        };
        assert_eq!(decide(ro, None, None), Assessment::ReadOnly);
        let busy = Usage {
            mounted_rw: vec!["/".into()],
            holders: vec!["dm-0".into()],
            ..Usage::default()
        };
        assert_eq!(
            decide(busy.clone(), Some(("vg".into(), "root".into(), true)), None),
            Assessment::Lvm {
                vg: "vg".into(),
                lv: "root".into(),
                thin: true
            }
        );
        // A busy plain partition: nothing can hold it.
        assert_eq!(
            decide(busy.clone(), None, None),
            Assessment::InUse(busy.clone())
        );
        // A busy device with another device stacked on it is not frozen as a
        // whole by suspending it.
        assert_eq!(
            decide(busy.clone(), None, dm("dm-0", "CRYPT-LUKS2-x")),
            Assessment::InUse(busy)
        );

        // An opened LUKS container with btrfs mounted from it.
        let luks_root = Usage {
            mounted_rw: vec!["/".into(), "/home".into()],
            ..Usage::default()
        };
        assert_eq!(
            decide(
                luks_root.clone(),
                None,
                dm("dm-0", "CRYPT-LUKS2-16e1-luks-16e1")
            ),
            Assessment::DeviceMapper {
                kernel: "dm-0".into()
            }
        );
        // A plain dm device without a UUID too.
        assert_eq!(
            decide(luks_root.clone(), None, dm("dm-4", "")),
            Assessment::DeviceMapper {
                kernel: "dm-4".into()
            }
        );
        // An LV that lvm2 cannot snapshot (no lvm2), and our own layers: refused.
        assert_eq!(
            decide(luks_root.clone(), None, dm("dm-1", "LVM-abc")),
            Assessment::InUse(luks_root.clone())
        );
        assert_eq!(
            decide(luks_root.clone(), None, dm("dm-2", "OBK-SNAP-abc")),
            Assessment::InUse(luks_root)
        );
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

    #[test]
    fn frozen_means_nothing_writes_what_is_read() {
        for mode in ["lvm", "dm", "idle", "readonly"] {
            assert!(is_frozen(&direct("/dev/sdz1", mode)), "{mode}");
        }
        // Held still is not taken frozen: nobody froze an idle device's filesystem.
        assert!(!taken_frozen(&direct("/dev/sdz1", "idle")));
        assert!(!is_frozen(&direct("/dev/sdz1", "mystery")));

        let mut file = direct("/srv/disk.img", "file");
        assert!(
            !is_frozen(&file),
            "a file holds still only on the caller's word"
        );
        assert_file_frozen(&mut file);
        assert!(is_frozen(&file));

        // The word is about files: on a device it means nothing and says nothing.
        let mut busy = direct("/dev/sdz1", "mystery");
        assert_file_frozen(&mut busy);
        assert!(!busy.metadata.contains_key(FILE_FROZEN));
        assert!(!is_frozen(&busy));
    }

    #[test]
    fn verdicts_on_snapshot_states() {
        let s = direct("/dev/vg/root", "lvm");
        assert!(lv_verdict(&s, "  swi-a-s---\n").is_ok());
        assert!(matches!(
            lv_verdict(&s, "  swi-I-s---\n"),
            Err(OwnBackupError::NoSnapshotProvider(_))
        ));
        assert!(dm_verdict(&s, Ok(SnapState::Valid { used: 8, total: 64 })).is_ok());
        for broken in [
            SnapState::Invalid,
            SnapState::Overflow,
            SnapState::Valid {
                used: 64,
                total: 64,
            },
            SnapState::Other("2 targets".into()),
        ] {
            assert!(dm_verdict(&s, Ok(broken)).is_err());
        }
        assert!(dm_verdict(&s, Err(std::io::Error::other("no permission"))).is_err());
    }

    /// Without a provider and without a runtime: a direct read has nothing to
    /// break, and a snapshot whose state cannot be read is never "intact".
    #[test]
    fn blocking_check_needs_no_provider() {
        check_intact_blocking(&direct("/srv/disk.img", "file")).unwrap();
        check_intact_blocking(&direct("/dev/sdz1", "idle")).unwrap();

        let mut dm = direct("/dev/dm-99", "dm");
        dm.metadata
            .insert("dm_snapshot".into(), "obk-snap-no-such-device".into());
        assert!(check_intact_blocking(&dm).is_err());

        let mut lv = direct("/dev/vg/root", "lvm");
        lv.metadata.insert(
            "lv_snapshot".into(),
            "obk-no-such-vg/obk-snap-000000000000".into(),
        );
        assert!(check_intact_blocking(&lv).is_err());
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
