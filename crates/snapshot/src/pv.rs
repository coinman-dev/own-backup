//! Snapshot of a partition that is an LVM physical volume in use — the PV of
//! a disk whose logical volumes are mounted (Ubuntu Server's default layout:
//! ESP, /boot, and a PV with the root LV), for a whole-disk copy.
//!
//! A PV has no snapshot of its own, and `lvcreate --snapshot` would write the
//! volume group's metadata — onto the very PV being copied. Instead every
//! active logical volume on it gets the device-mapper snapshot an opened LUKS
//! container gets (`dm_snapshot.rs`), all of them in one suspend
//! ([`dm_snapshot::create_group`]): one point in time across the volumes, and
//! not a byte of LVM metadata written. The copy then reads each volume's
//! extents from its snapshot and the rest — the label, the metadata area, the
//! extents of inactive volumes — from the partition itself, where only LVM
//! writes, and LVM only when the volume group changes.
//!
//! Whether it changed is checked before the image is published ([`check`]):
//! every snapshot still whole, every volume still on its snapshot-origin
//! table, nothing new stacked on the partition, and the label and metadata
//! areas as they were when the snapshots were taken.
//!
//! Supported: logical volumes made of `linear` targets with nothing stacked
//! on them. Thin pools, RAID, mirrors, caches, LUKS on an LV and a PV inside
//! a LUKS container are refused with the reason.

#![cfg(target_os = "linux")]

use std::collections::BTreeSet;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;

use tracing::{info, warn};

use own_backup_common::linux_block::{self, SYS_CLASS_BLOCK};
use own_backup_common::lvm_pv::{self, PvSegment, PvSnapshot, PvSnapshotVolume};
use own_backup_common::{tr, OwnBackupError, Result};

use crate::dm_ioctl::{Control, Target};
use crate::dm_snapshot::{self, Origin, Session};

/// What a device stacked on the partition is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HolderFacts {
    /// Kernel name (`dm-1`).
    pub kernel: String,
    /// Device-mapper name (`ubuntu--vg-ubuntu--lv`).
    pub name: String,
    /// Device-mapper UUID (`LVM-…`, `CRYPT-…`).
    pub uuid: String,
    pub size_sectors: u64,
    /// Kernel names of the devices stacked on it in turn.
    pub stacked: Vec<String>,
    /// Their device-mapper UUIDs.
    pub stacked_uuids: Vec<String>,
    /// Its live table, without keys.
    pub table: Vec<Target>,
}

/// One logical volume to snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LvPlan {
    pub kernel: String,
    pub name: String,
    pub segments: Vec<PvSegment>,
}

/// Why a PV in use is not copied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PvRefusal {
    /// The partition is a LUKS container with a PV inside.
    OnLuks { container: String },
    /// Another device is stacked on a logical volume (LUKS on an LV, a thin
    /// pool's data volume, a classic snapshot's origin).
    Stacked { lv: String, holders: Vec<String> },
    /// A logical volume of a kind other than linear.
    Unsupported { lv: String, kinds: String },
    /// Something other than a logical volume holds the partition.
    Foreign { holder: String },
    /// A snapshot layer of an interrupted run still sits on it.
    Leftover,
    /// A table that does not cover its volume, or two volumes on one extent.
    Mismatch { lv: String },
}

/// What a busy partition is, as far as this module is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PvJudgement {
    /// Not an LVM physical volume in use.
    NotPv,
    Plan(Vec<LvPlan>),
    Refuse(PvRefusal),
}

/// `vg/lv` of a device-mapper name of a logical volume (`ubuntu--vg-root` →
/// `ubuntu-vg/root`); a name that is not one is returned as it is.
pub fn lv_display(dm_name: &str) -> String {
    let mut out = String::with_capacity(dm_name.len());
    let mut chars = dm_name.chars().peekable();
    let mut split = false;
    while let Some(c) = chars.next() {
        if c == '-' {
            if chars.peek() == Some(&'-') {
                chars.next();
                out.push('-');
            } else if !split {
                split = true;
                out.push('/');
            } else {
                out.push('-');
            }
        } else {
            out.push(c);
        }
    }
    if split {
        out
    } else {
        dm_name.to_string()
    }
}

fn parse_dev(s: &str) -> Option<(u32, u32)> {
    let (a, b) = s.split_once(':')?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

/// The parts of `table` that lie on the device `part` (`linear` targets
/// onto it), in bytes.
pub(crate) fn segments_on(table: &[Target], part: (u32, u32)) -> Vec<PvSegment> {
    table
        .iter()
        .filter(|t| t.kind == "linear")
        .filter_map(|t| {
            let mut p = t.params.split_whitespace();
            let dev = parse_dev(p.next()?)?;
            let offset: u64 = p.next()?.parse().ok()?;
            (dev == part).then_some(PvSegment {
                pv_offset: offset * 512,
                lv_offset: t.start * 512,
                length: t.length * 512,
            })
        })
        .collect()
}

/// The decision, from facts alone: `part` is the partition's `major:minor`,
/// `is_pv` whether it carries an LVM2 label, `holders` what is stacked on it.
pub fn judge(part: (u32, u32), is_pv: bool, holders: &[HolderFacts]) -> PvJudgement {
    if holders.is_empty() {
        return PvJudgement::NotPv;
    }
    if !holders.iter().any(|h| h.uuid.starts_with("LVM-")) {
        if let Some(c) = holders.iter().find(|h| {
            h.uuid.starts_with("CRYPT-") && h.stacked_uuids.iter().any(|u| u.starts_with("LVM-"))
        }) {
            return PvJudgement::Refuse(PvRefusal::OnLuks {
                container: c.name.clone(),
            });
        }
        return PvJudgement::NotPv;
    }
    if !is_pv {
        return PvJudgement::NotPv;
    }
    let mut plans = Vec::new();
    for h in holders {
        if h.uuid.starts_with("OBK-") {
            return PvJudgement::Refuse(PvRefusal::Leftover);
        }
        if !h.uuid.starts_with("LVM-") {
            return PvJudgement::Refuse(PvRefusal::Foreign {
                holder: h.name.clone(),
            });
        }
        let lv = lv_display(&h.name);
        if !h.stacked.is_empty() {
            return PvJudgement::Refuse(PvRefusal::Stacked {
                lv,
                holders: h.stacked.clone(),
            });
        }
        let odd: BTreeSet<&str> = h
            .table
            .iter()
            .map(|t| t.kind.as_str())
            .filter(|k| !matches!(*k, "linear" | "zero"))
            .collect();
        if !odd.is_empty() {
            return PvJudgement::Refuse(PvRefusal::Unsupported {
                lv,
                kinds: odd.into_iter().collect::<Vec<_>>().join(", "),
            });
        }
        if dm_snapshot::judge_table(&h.table, h.size_sectors) != dm_snapshot::TableVerdict::Copyable
        {
            return PvJudgement::Refuse(PvRefusal::Mismatch { lv });
        }
        let segments = segments_on(&h.table, part);
        if segments.is_empty() {
            return PvJudgement::Refuse(PvRefusal::Mismatch { lv });
        }
        plans.push(LvPlan {
            kernel: h.kernel.clone(),
            name: h.name.clone(),
            segments,
        });
    }
    if let Some(lv) = overlapping(&plans) {
        return PvJudgement::Refuse(PvRefusal::Mismatch { lv });
    }
    PvJudgement::Plan(plans)
}

/// A volume one of whose parts overlaps another's on the PV.
fn overlapping(plans: &[LvPlan]) -> Option<String> {
    let mut all: Vec<(u64, u64, &str)> = plans
        .iter()
        .flat_map(|p| {
            p.segments
                .iter()
                .map(move |s| (s.pv_offset, s.pv_offset + s.length, p.name.as_str()))
        })
        .collect();
    all.sort_unstable();
    all.windows(2)
        .find(|w| w[1].0 < w[0].1)
        .map(|w| lv_display(w[1].2))
}

/// The refusal in words, naming the partition `volume`.
pub fn describe(volume: &str, why: &PvRefusal) -> String {
    match why {
        PvRefusal::OnLuks { container } => {
            tr!("linux.pv_on_luks", volume = volume, container = container)
        }
        PvRefusal::Stacked { lv, holders } => tr!(
            "linux.pv_lv_stacked",
            volume = volume,
            lv = lv,
            holders = holders.join(", ")
        ),
        PvRefusal::Unsupported { lv, kinds } => tr!(
            "linux.pv_lv_unsupported",
            volume = volume,
            lv = lv,
            kinds = kinds
        ),
        PvRefusal::Foreign { holder } => {
            tr!("linux.pv_foreign", volume = volume, holder = holder)
        }
        PvRefusal::Leftover => tr!("linux.dm_leftover", volume = volume),
        PvRefusal::Mismatch { lv } => tr!("linux.pv_mismatch", volume = volume, lv = lv),
    }
    .into_owned()
}

// ---------------------------------------------------------------------------
// The live system
// ---------------------------------------------------------------------------

fn read_sys(kernel: &str, attr: &str) -> String {
    std::fs::read_to_string(Path::new(SYS_CLASS_BLOCK).join(kernel).join(attr))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Kernel names of what is stacked on `kernel`.
fn holders_of(kernel: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(Path::new(SYS_CLASS_BLOCK).join(kernel).join("holders"))
    else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Read `len` bytes at `offset` of the device `path`.
fn read_device(path: &str, offset: u64, len: u64) -> io::Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    let mut buf = vec![0u8; len as usize];
    file.read_exact_at(&mut buf, offset)?;
    Ok(buf)
}

/// Is the partition (kernel name) an LVM2 PV, and what is stacked on it.
pub fn facts(kernel: &str) -> io::Result<(bool, (u32, u32), Vec<HolderFacts>)> {
    let sys = Path::new(SYS_CLASS_BLOCK);
    let dev = linux_block::dev_numbers(sys, kernel)
        .ok_or_else(|| io::Error::other(format!("{kernel}: no device number")))?;
    let names = holders_of(kernel);
    if names.is_empty() {
        return Ok((false, dev, Vec::new()));
    }
    let is_pv = read_device(
        &format!("/dev/{kernel}"),
        0,
        lvm_pv::LABEL_SCAN_BYTES as u64,
    )
    .ok()
    .and_then(|head| lvm_pv::parse_label(&head))
    .is_some();
    let ctl = Control::open()?;
    let mut holders = Vec::new();
    for h in names {
        let name = read_sys(&h, "dm/name");
        let stacked = holders_of(&h);
        let stacked_uuids = stacked.iter().map(|s| read_sys(s, "dm/uuid")).collect();
        let table = if name.is_empty() {
            Vec::new()
        } else {
            ctl.table_without_keys(&name).unwrap_or_default()
        };
        holders.push(HolderFacts {
            uuid: read_sys(&h, "dm/uuid"),
            size_sectors: read_sys(&h, "size").parse().unwrap_or(0),
            name: if name.is_empty() { h.clone() } else { name },
            kernel: h,
            stacked,
            stacked_uuids,
            table,
        });
    }
    Ok((is_pv, dev, holders))
}

/// What the PV's label and metadata areas hold now: their ranges and digest.
fn guard_of(device: &str) -> Result<(Vec<(u64, u64)>, String)> {
    let head = read_device(device, 0, lvm_pv::LABEL_SCAN_BYTES as u64)
        .map_err(|e| bad(format!("{device}: {e}")))?;
    let label = lvm_pv::parse_label(&head)
        .ok_or_else(|| bad(format!("{device}: the LVM label is gone")))?;
    let size = std::fs::File::open(device)
        .and_then(|f| device_size(&f))
        .map_err(|e| bad(format!("{device}: {e}")))?;
    let ranges = lvm_pv::guard_ranges(&label, size);
    let mut chunks = Vec::with_capacity(ranges.len());
    for &(offset, len) in &ranges {
        chunks.push(read_device(device, offset, len).map_err(|e| bad(format!("{device}: {e}")))?);
    }
    let digest = lvm_pv::guard_digest(chunks.iter().map(Vec::as_slice));
    Ok((ranges, digest))
}

fn device_size(file: &std::fs::File) -> io::Result<u64> {
    use std::io::{Seek, SeekFrom};
    let mut f = file;
    f.seek(SeekFrom::End(0))
}

fn bad(msg: impl std::fmt::Display) -> OwnBackupError {
    OwnBackupError::Device(format!("PV snapshot: {msg}"))
}

fn changed(volume: &str, reason: String) -> OwnBackupError {
    OwnBackupError::NoSnapshotProvider(
        tr!("linux.pv_changed", volume = volume, reason = reason).into_owned(),
    )
}

/// Does the device-mapper device `name` run on `snapshot-origin` over the
/// device `real` — the table our switch gave it, not one LVM reloaded since?
fn switched_onto(ctl: &Control, name: &str, real: (u32, u32)) -> Result<bool> {
    let table = ctl
        .table_without_keys(name)
        .map_err(|e| bad(format!("{name}: {e}")))?;
    Ok(matches!(table.as_slice(),
        [t] if t.kind == "snapshot-origin" && parse_dev(t.params.trim()) == Some(real)))
}

/// Kernel name of the block device `dev`.
fn kernel_of(dev: (u32, u32)) -> Option<String> {
    std::fs::read_link(format!("/sys/dev/block/{}:{}", dev.0, dev.1))
        .ok()?
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
}

/// Snapshot every logical volume of `plan` at one point in time; `volume` is
/// the partition as named (`kernel` its kernel name). Blocking. On success
/// the sessions are the caller's to release.
pub fn create(
    volume: &str,
    kernel: &str,
    plan: &[LvPlan],
    cow_dir: &Path,
) -> Result<(Vec<Session>, PvSnapshot)> {
    let part = linux_block::dev_numbers(Path::new(SYS_CLASS_BLOCK), kernel)
        .ok_or_else(|| bad(format!("{volume}: no device number")))?;
    let labels: Vec<String> = plan
        .iter()
        .map(|lv| format!("/dev/mapper/{}", lv.name))
        .collect();
    let mounts: Vec<Vec<String>> = plan
        .iter()
        .map(|lv| {
            linux_block::device_usage(Path::new(&format!("/dev/{}", lv.kernel)))
                .ok()
                .flatten()
                .map(|u| u.mounted_rw)
                .unwrap_or_default()
        })
        .collect();
    let origins: Vec<Origin<'_>> = plan
        .iter()
        .zip(&labels)
        .zip(&mounts)
        .map(|((lv, label), rw)| Origin {
            volume: label,
            kernel: &lv.kernel,
            rw_mounts: rw,
        })
        .collect();
    let sessions = dm_snapshot::create_group(&origins, cow_dir, None)?;
    match describe_taken(volume, kernel, part, plan, &sessions) {
        Ok(layout) => {
            info!(
                volume,
                volumes = layout.volumes.len(),
                guard = ?layout.guard,
                "every logical volume of the physical volume is held by its snapshot"
            );
            Ok((sessions, layout))
        }
        Err(e) => {
            for s in sessions {
                if let Err(e) = s.release() {
                    warn!(volume, error = %e, "a snapshot of a logical volume was not released");
                }
            }
            Err(e)
        }
    }
}

/// What the snapshots just taken hold: the parts of each volume on the PV,
/// from the table it had when it was switched, and the guard — after making
/// sure nothing but our layers is stacked on the partition now.
fn describe_taken(
    volume: &str,
    kernel: &str,
    part: (u32, u32),
    plan: &[LvPlan],
    sessions: &[Session],
) -> Result<PvSnapshot> {
    let ours: BTreeSet<String> = sessions
        .iter()
        .filter_map(|s| kernel_of(s.real_dev))
        .collect();
    let now: BTreeSet<String> = holders_of(kernel).into_iter().collect();
    if now != ours {
        let extra: Vec<String> = now.difference(&ours).cloned().collect();
        return Err(changed(
            volume,
            tr!("linux.pv_changed_volumes", holders = extra.join(", ")).into_owned(),
        ));
    }
    let ctl = Control::open().map_err(|e| bad(format!("/dev/mapper/control: {e}")))?;
    let mut volumes = Vec::with_capacity(sessions.len());
    for (lv, s) in plan.iter().zip(sessions) {
        let segments = segments_on(s.original_table(), part);
        if segments.is_empty() || !switched_onto(&ctl, &s.origin, s.real_dev)? {
            return Err(changed(
                volume,
                tr!("linux.pv_changed_table", lv = lv_display(&lv.name)).into_owned(),
            ));
        }
        let size = s
            .original_table()
            .iter()
            .map(|t| t.start + t.length)
            .max()
            .unwrap_or(0)
            * 512;
        volumes.push(PvSnapshotVolume {
            name: lv.name.clone(),
            origin: format!("/dev/{}", lv.kernel),
            snapshot: s.snap_path.clone(),
            dm_id: s.id.clone(),
            size,
            segments,
        });
    }
    let device = format!("/dev/{kernel}");
    let (guard, guard_digest) = guard_of(&device)?;
    Ok(PvSnapshot {
        device: volume.to_string(),
        volumes,
        guard,
        guard_digest,
    })
}

/// Did the copy read one point in time? Every snapshot still whole, every
/// volume still switched onto its snapshot, nothing else stacked on the
/// partition, and the label and metadata areas unchanged. Blocking; reads
/// the kernel's state, so it works without the sessions.
pub fn check(pv: &PvSnapshot) -> Result<()> {
    let volume = pv.device.as_str();
    let kernel = linux_block::kernel_name(Path::new(volume))
        .ok_or_else(|| bad(format!("{volume}: not a block device any more")))?;
    let ctl = Control::open().map_err(|e| bad(format!("/dev/mapper/control: {e}")))?;
    let mut ours = BTreeSet::new();
    for v in &pv.volumes {
        let snap = format!("{}{}", dm_snapshot::SNAP_PREFIX, v.dm_id);
        let real = format!("{}{}", dm_snapshot::REAL_PREFIX, v.dm_id);
        let state = dm_snapshot::snapshot_state(&snap)
            .map_err(|e| bad(format!("{snap}: cannot read the snapshot's state: {e}")))?;
        if !state.intact() {
            return Err(OwnBackupError::NoSnapshotProvider(
                tr!(
                    "linux.dm_snapshot_invalid",
                    volume = format!("/dev/mapper/{}", v.name),
                    state = format!("{state:?}")
                )
                .into_owned(),
            ));
        }
        let real_dev = ctl
            .status(&real)
            .map_err(|e| bad(format!("{real}: {e}")))?
            .dev;
        if !switched_onto(&ctl, &v.name, real_dev)? {
            return Err(changed(
                volume,
                tr!("linux.pv_changed_table", lv = lv_display(&v.name)).into_owned(),
            ));
        }
        if let Some(k) = kernel_of(real_dev) {
            ours.insert(k);
        }
    }
    let extra: Vec<String> = holders_of(&kernel)
        .into_iter()
        .filter(|h| !ours.contains(h))
        .collect();
    if !extra.is_empty() {
        return Err(changed(
            volume,
            tr!("linux.pv_changed_volumes", holders = extra.join(", ")).into_owned(),
        ));
    }
    let (_, digest) = guard_of(&format!("/dev/{kernel}"))?;
    if digest != pv.guard_digest {
        return Err(changed(
            volume,
            tr!("linux.pv_changed_metadata").into_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PART: (u32, u32) = (252, 3);

    fn lv(kernel: &str, name: &str, size: u64, table: Vec<Target>) -> HolderFacts {
        HolderFacts {
            kernel: kernel.into(),
            name: name.into(),
            uuid: format!("LVM-{name}"),
            size_sectors: size,
            stacked: Vec::new(),
            stacked_uuids: Vec::new(),
            table,
        }
    }

    fn ubuntu() -> Vec<HolderFacts> {
        vec![
            lv(
                "dm-0",
                "ubuntu--vg-ubuntu--lv",
                20480,
                vec![Target::new(0, 20480, "linear", "252:3 2048")],
            ),
            // Two segments, one of them on another PV.
            lv(
                "dm-1",
                "ubuntu--vg-data",
                16384,
                vec![
                    Target::new(0, 8192, "linear", "252:3 30720"),
                    Target::new(8192, 8192, "linear", "8:17 2048"),
                ],
            ),
        ]
    }

    #[test]
    fn logical_volume_names_read_as_vg_slash_lv() {
        assert_eq!(lv_display("ubuntu--vg-ubuntu--lv"), "ubuntu-vg/ubuntu-lv");
        assert_eq!(lv_display("vg0-root"), "vg0/root");
        assert_eq!(lv_display("vg0-my-data"), "vg0/my-data");
        assert_eq!(lv_display("luks-1234"), "luks/1234");
        assert_eq!(lv_display("plain"), "plain");
    }

    #[test]
    fn the_volumes_of_a_pv_in_use_are_planned_with_their_parts_on_it() {
        let PvJudgement::Plan(plans) = judge(PART, true, &ubuntu()) else {
            panic!("{:?}", judge(PART, true, &ubuntu()));
        };
        assert_eq!(plans.len(), 2);
        assert_eq!(
            plans[0].segments,
            vec![PvSegment {
                pv_offset: 2048 * 512,
                lv_offset: 0,
                length: 20480 * 512
            }]
        );
        assert_eq!(
            plans[1].segments,
            vec![PvSegment {
                pv_offset: 30720 * 512,
                lv_offset: 0,
                length: 8192 * 512
            }],
            "the part on the other PV is not this partition's"
        );
    }

    #[test]
    fn what_is_not_a_plain_pv_in_use_is_said() {
        assert_eq!(judge(PART, true, &[]), PvJudgement::NotPv);
        // A LUKS container with a filesystem on it: not ours to judge.
        let mut crypt = lv(
            "dm-0",
            "luks-1",
            100,
            vec![Target::new(0, 100, "crypt", "aes 0 252:3 32768")],
        );
        crypt.uuid = "CRYPT-LUKS2-1".into();
        assert_eq!(judge(PART, false, &[crypt.clone()]), PvJudgement::NotPv);
        // …and with a PV inside: refused in words.
        crypt.stacked = vec!["dm-1".into()];
        crypt.stacked_uuids = vec!["LVM-abc".into()];
        assert_eq!(
            judge(PART, false, &[crypt]),
            PvJudgement::Refuse(PvRefusal::OnLuks {
                container: "luks-1".into()
            })
        );
        // LVM holders without a label: not believed to be a PV.
        assert_eq!(judge(PART, false, &ubuntu()), PvJudgement::NotPv);

        let mut stacked = ubuntu();
        stacked[0].stacked = vec!["dm-5".into()];
        assert!(matches!(
            judge(PART, true, &stacked),
            PvJudgement::Refuse(PvRefusal::Stacked { .. })
        ));
        let mut thin = ubuntu();
        thin[1].table = vec![Target::new(0, 16384, "thin-pool", "x")];
        assert_eq!(
            judge(PART, true, &thin),
            PvJudgement::Refuse(PvRefusal::Unsupported {
                lv: "ubuntu-vg/data".into(),
                kinds: "thin-pool".into()
            })
        );
        let mut foreign = ubuntu();
        foreign[1].uuid = "mpath-x".into();
        assert!(matches!(
            judge(PART, true, &foreign),
            PvJudgement::Refuse(PvRefusal::Foreign { .. })
        ));
        let mut leftover = ubuntu();
        leftover[1].uuid = "OBK-REAL-0123".into();
        assert_eq!(
            judge(PART, true, &leftover),
            PvJudgement::Refuse(PvRefusal::Leftover)
        );
        let mut short = ubuntu();
        short[0].size_sectors = 99999;
        assert!(matches!(
            judge(PART, true, &short),
            PvJudgement::Refuse(PvRefusal::Mismatch { .. })
        ));
        let mut twice = ubuntu();
        twice[1].table[0] = Target::new(0, 8192, "linear", "252:3 4096");
        assert_eq!(
            judge(PART, true, &twice),
            PvJudgement::Refuse(PvRefusal::Mismatch {
                lv: "ubuntu-vg/data".into()
            })
        );
    }

    #[test]
    fn refusals_name_the_partition_and_the_volume() {
        let cases = [
            PvRefusal::OnLuks {
                container: "luks-1".into(),
            },
            PvRefusal::Stacked {
                lv: "vg/root".into(),
                holders: vec!["dm-9".into()],
            },
            PvRefusal::Unsupported {
                lv: "vg/root".into(),
                kinds: "raid1".into(),
            },
            PvRefusal::Foreign {
                holder: "mpath".into(),
            },
            PvRefusal::Leftover,
            PvRefusal::Mismatch {
                lv: "vg/root".into(),
            },
        ];
        for why in cases {
            let text = describe("/dev/vda3", &why);
            assert!(text.contains("/dev/vda3"), "{why:?}: {text}");
            assert!(!text.contains("%{") && !text.contains("linux."), "{text}");
        }
        let text = changed("/dev/vda3", tr!("linux.pv_changed_metadata").into_owned()).to_string();
        assert!(
            text.contains("/dev/vda3") && !text.contains("linux."),
            "{text}"
        );
    }
}
