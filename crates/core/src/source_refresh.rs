//! Re-binding a job's partitions to the disk as it is NOW.
//!
//! A job names each partition by a device — `C:`, or `\\?\Volume{GUID}` for a
//! letterless one (ESP, WinRE) — and keeps a snapshot of the disk it saw when
//! it was saved. Both go stale without anyone touching the job:
//!
//! * Windows Update services WinRE by shrinking C: and recreating the recovery
//!   partition, with a NEW partition and volume GUID (KB5127648, 2026-10-14).
//!   The job then names a volume that no longer exists, and every run fails
//!   with "CreateFileW … не удается найти указанный файл" — a job the user
//!   cannot repair, since a saved job's source is read-only in the wizard;
//! * `PhysicalDriveN` is a slot: plugging a disk in can renumber the others.
//!
//! Before every run the job is therefore matched against the live inventory:
//! the disk by its GPT disk GUID, each partition by its unique GUID; failing
//! that, by its device; and, for partitions the SYSTEM creates and recreates
//! (ESP, MSR, WinRE), by type on the same disk. A data partition is never
//! guessed — a different volume in its place may be somebody else's data.
//! Whatever changed is written back into the job, and said in the run record.

// Only the Windows runner asks the live disks; elsewhere the tests do.
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

use std::collections::HashMap;

use own_backup_common::disk::{DiskInfo, PartitionInfo};
use own_backup_common::gpt::{GPT_TYPE_EFI_SYSTEM, GPT_TYPE_MSR, GPT_TYPE_WINRE};
use own_backup_common::tr;

use crate::Job;

/// What matching the job against the live disks came to.
#[derive(Debug)]
pub enum Refreshed {
    /// Everything the job names is where the job says.
    Unchanged,
    /// The job has to be updated before it runs.
    Updated(Box<SourceRefresh>),
    /// Something the job copies is gone, and nothing safe replaces it — why,
    /// in words for the run record.
    Missing(String),
}

#[derive(Debug)]
pub struct SourceRefresh {
    /// The job with its devices, disk number and disk snapshot brought up to date.
    pub job: Job,
    /// The partitions the job copies were moved, resized or recreated — said
    /// to the user, because their copy chain starts again with a full. Empty
    /// when only names changed (a renumbered disk, a new volume path).
    pub layout_note: String,
    /// Old device → new device, for the log.
    pub renamed: Vec<(String, String)>,
}

/// Partition types Windows creates and recreates on its own.
fn system_managed(type_guid: &str) -> bool {
    [GPT_TYPE_EFI_SYSTEM, GPT_TYPE_MSR, GPT_TYPE_WINRE]
        .iter()
        .any(|t| t.eq_ignore_ascii_case(type_guid))
}

/// Trailing number of `\\.\PhysicalDriveN` — the same parse the runner uses.
fn drive_number(device_id: &str) -> Option<u32> {
    let tail: String = device_id
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    tail.chars().rev().collect::<String>().parse().ok()
}

/// The inventory names a disk without a GUID `disk-N`: a slot, not an identity.
fn real_disk_guid(guid: &str) -> Option<&str> {
    let g = guid.trim();
    (!g.is_empty() && !g.starts_with("disk-")).then_some(g)
}

fn find_live_disk<'a>(saved: &DiskInfo, live: &'a [DiskInfo]) -> Option<&'a DiskInfo> {
    if let Some(guid) = real_disk_guid(&saved.disk_guid) {
        let same: Vec<&DiskInfo> = live
            .iter()
            .filter(|d| d.disk_guid.eq_ignore_ascii_case(guid))
            .collect();
        return match same.as_slice() {
            [] => None,
            [one] => Some(one),
            // Two disks with one GUID: a clone Windows has not yet resolved.
            // The slot the job knew is the better guess than either.
            many => many
                .iter()
                .find(|d| d.device_id == saved.device_id)
                .copied(),
        };
    }
    live.iter().find(|d| {
        d.device_id == saved.device_id && d.model == saved.model && d.serial == saved.serial
    })
}

/// The live partition that IS `saved`, or `None`. `taken` holds indexes other
/// partitions of this job already claimed.
fn find_live_partition<'a>(
    saved: &PartitionInfo,
    disk: &'a DiskInfo,
    taken: &[u32],
) -> Option<&'a PartitionInfo> {
    let free = |p: &&PartitionInfo| !taken.contains(&p.index);
    // 1. Its unique GUID: survives a resize, a move and a new drive letter.
    if !saved.gpt_partition_guid.trim().is_empty() {
        if let Some(p) = disk.partitions.iter().filter(free).find(|p| {
            p.gpt_partition_guid
                .eq_ignore_ascii_case(&saved.gpt_partition_guid)
        }) {
            return Some(p);
        }
    }
    // 2. Its device, still on this disk (an MBR disk has no unique GUIDs).
    if let Some(p) = disk
        .partitions
        .iter()
        .filter(free)
        .find(|p| !p.volume_device.is_empty() && p.volume_device == saved.volume_device)
    {
        return Some(p);
    }
    // 3. Recreated by Windows: the same kind of system partition on the same
    // disk — in the same slot if there is one, else the only one of its kind.
    if system_managed(&saved.gpt_type_guid) {
        let kind: Vec<&PartitionInfo> = disk
            .partitions
            .iter()
            .filter(free)
            .filter(|p| p.gpt_type_guid.eq_ignore_ascii_case(&saved.gpt_type_guid))
            .collect();
        if let Some(p) = kind.iter().find(|p| p.index == saved.index) {
            return Some(p);
        }
        if let [only] = kind.as_slice() {
            return Some(only);
        }
    }
    None
}

/// How a partition is called in a message: its letter or label, and its number.
fn part_name(p: &PartitionInfo) -> String {
    let kind = if let Some(l) = p.volume_letter {
        format!("{l}:")
    } else if !p.volume_label.trim().is_empty() {
        p.volume_label.clone()
    } else if p.gpt_type_guid.eq_ignore_ascii_case(GPT_TYPE_WINRE) {
        tr!("source_refresh.winre").into_owned()
    } else if p.gpt_type_guid.eq_ignore_ascii_case(GPT_TYPE_EFI_SYSTEM) {
        "EFI".to_string()
    } else {
        String::new()
    };
    if kind.is_empty() {
        tr!("source_refresh.part_number", index = p.index).into_owned()
    } else {
        tr!("source_refresh.part_named", index = p.index, name = kind).into_owned()
    }
}

/// What the snapshot has to agree on for nothing to be rewritten. Used space
/// changes every day and is no reason to touch the job.
fn layout_signature(d: &DiskInfo) -> Vec<(u32, u64, u64, String, String, String)> {
    let mut v: Vec<_> = d
        .partitions
        .iter()
        .map(|p| {
            (
                p.index,
                p.offset_bytes,
                p.size_bytes,
                p.gpt_type_guid.to_ascii_uppercase(),
                p.gpt_partition_guid.to_ascii_uppercase(),
                p.volume_device.clone(),
            )
        })
        .collect();
    v.sort();
    v
}

/// Match `job` against `live` (the inventory as it is now). Pure: everything
/// it knows comes in through its arguments.
pub fn reconcile_sources(job: &Job, live: &[DiskInfo]) -> Refreshed {
    if job.source_disks.is_empty() {
        return Refreshed::Unchanged; // a job from before snapshots: nothing to go by
    }
    let sources = job.source_list();
    let mut next = job.clone();
    let mut renamed: Vec<(String, String)> = Vec::new();
    // old part index → new, per old disk number (for `disk_source`).
    let mut reindexed: HashMap<(u32, u32), u32> = HashMap::new();
    let mut moved_disks: HashMap<u32, u32> = HashMap::new();
    let mut changed_parts: Vec<String> = Vec::new();
    let mut changed_disk_model = String::new();

    for (slot, saved) in job.source_disks.iter().enumerate() {
        let copied: Vec<&PartitionInfo> = saved
            .partitions
            .iter()
            .filter(|p| !p.volume_device.is_empty() && sources.contains(&p.volume_device))
            .collect();
        if copied.is_empty() {
            continue;
        }
        let Some(disk) = find_live_disk(saved, live) else {
            return Refreshed::Missing(
                tr!("source_refresh.disk_missing", disk = saved.model).into_owned(),
            );
        };
        let old_number = drive_number(&saved.device_id);
        let new_number = drive_number(&disk.device_id);
        if let (Some(o), Some(n)) = (old_number, new_number) {
            if o != n {
                moved_disks.insert(o, n);
            }
        }

        let mut taken: Vec<u32> = Vec::new();
        for p in copied {
            let Some(now) = find_live_partition(p, disk, &taken) else {
                return Refreshed::Missing(
                    tr!(
                        "source_refresh.partition_missing",
                        part = part_name(p),
                        disk = saved.model
                    )
                    .into_owned(),
                );
            };
            if now.volume_device.is_empty() {
                return Refreshed::Missing(
                    tr!(
                        "source_refresh.partition_unreadable",
                        part = part_name(now),
                        disk = saved.model
                    )
                    .into_owned(),
                );
            }
            taken.push(now.index);
            if now.volume_device != p.volume_device {
                renamed.push((p.volume_device.clone(), now.volume_device.clone()));
            }
            if let Some(o) = old_number {
                reindexed.insert((o, p.index), now.index);
            }
            if now.index != p.index
                || now.offset_bytes != p.offset_bytes
                || now.size_bytes != p.size_bytes
            {
                changed_parts.push(part_name(now));
                changed_disk_model = disk.model.clone();
            }
        }

        // The runner finds the snapshot by drive number, so a renumbered disk
        // needs it rewritten as much as a re-partitioned one does.
        if saved.device_id != disk.device_id || layout_signature(saved) != layout_signature(disk) {
            // The snapshot follows the disk; what only the service can tell
            // about a volume (how it unlocks) stays from the saved one.
            let mut fresh = disk.clone();
            for p in &mut fresh.partitions {
                if let Some(old) = saved.partitions.iter().find(|o| {
                    o.gpt_partition_guid
                        .eq_ignore_ascii_case(&p.gpt_partition_guid)
                }) {
                    p.unlock_methods = old.unlock_methods.clone();
                    p.recovery_key_only = old.recovery_key_only;
                    p.unlockable = old.unlockable;
                }
            }
            next.source_disks[slot] = fresh;
        }
    }

    let rename = |d: &str| {
        renamed
            .iter()
            .find(|(old, _)| old == d)
            .map(|(_, new)| new.clone())
            .unwrap_or_else(|| d.to_string())
    };
    next.sources = next.sources.iter().map(|d| rename(d)).collect();
    next.source = rename(&next.source);
    if let Some(ds) = next.disk_source.as_mut() {
        let old_number = ds.disk_number;
        if let Some(&n) = moved_disks.get(&old_number) {
            ds.disk_number = n;
        }
        for part in &mut ds.parts {
            if let Some(&i) = reindexed.get(&(old_number, part.part_index)) {
                part.part_index = i;
            }
            part.device = rename(&part.device);
        }
    }

    let same = serde_json::to_value(&next).ok() == serde_json::to_value(job).ok();
    if same {
        return Refreshed::Unchanged;
    }
    let layout_note = if changed_parts.is_empty() {
        String::new()
    } else {
        tr!(
            "source_refresh.layout_changed",
            disk = changed_disk_model,
            parts = changed_parts.join(", ")
        )
        .into_owned()
    };
    Refreshed::Updated(Box::new(SourceRefresh {
        job: next,
        layout_note,
        renamed,
    }))
}

/// [`reconcile_sources`] against the disks attached right now.
#[cfg(target_os = "windows")]
pub async fn reconcile_with_live_disks(job: &Job) -> Refreshed {
    use own_backup_inventory::DiskInventory;

    if job.source_disks.is_empty() {
        return Refreshed::Unchanged;
    }
    let live = match own_backup_inventory::WindowsInventory::new() {
        Ok(inv) => inv.enumerate_disks().await,
        Err(e) => Err(e),
    };
    match live {
        Ok(live) => reconcile_sources(job, &live),
        Err(e) => {
            // Not knowing is not a reason to stop the copy: the engine checks
            // every partition against the disk itself before reading it.
            tracing::warn!(job = %job.name, error = %e,
                "disk inventory failed — the job's partitions were not re-checked");
            Refreshed::Unchanged
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DiskPartSpec, DiskSourceSpec};
    use own_backup_common::types::{FileSystem, PartitionTable};

    const BASIC: &str = "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7";

    fn part(index: u32, ty: &str, guid: &str, off: u64, size: u64, dev: &str) -> PartitionInfo {
        PartitionInfo {
            index,
            offset_bytes: off,
            size_bytes: size,
            used_bytes: 0,
            gpt_type_guid: ty.into(),
            gpt_partition_guid: guid.into(),
            filesystem: FileSystem::Ntfs,
            volume_letter: dev
                .strip_suffix(':')
                .and_then(|l| l.chars().next())
                .filter(|_| dev.len() == 2),
            volume_device: dev.into(),
            volume_label: String::new(),
            fs_uuid: String::new(),
            is_system: false,
            is_boot: false,
            has_os: false,
            linux_last_mounted: String::new(),
            is_encrypted: false,
            unlock_methods: Vec::new(),
            recovery_key_only: false,
            unlockable: true,
            gpt_attributes: 0,
            mbr_type: 0,
        }
    }

    fn disk(number: u32, parts: Vec<PartitionInfo>) -> DiskInfo {
        DiskInfo {
            device_id: format!(r"\\.\PhysicalDrive{number}"),
            model: "Samsung SSD 970 PRO 1TB".into(),
            serial: "S".into(),
            size_bytes: 1_024_209_543_168,
            sector_size_logical: 512,
            sector_size_physical: 4096,
            media_type: String::new(),
            partition_table: PartitionTable::Gpt,
            disk_guid: "CEFE2DB3-0CFE-4F78-D1C8-5A9915267A71".into(),
            partitions: parts,
        }
    }

    const ESP: &str = r"\\?\Volume{1d3128fc-790c-4147-b0e3-0767e8b3a4c0}";
    const RE_OLD: &str = r"\\?\Volume{fb002532-838c-4818-8f14-7824d64763b7}";
    const RE_NEW: &str = r"\\?\Volume{f53b6290-da11-4b4f-8333-5721106d3a92}";

    /// The owner's system disk before KB5127648.
    fn before() -> DiskInfo {
        disk(
            4,
            vec![
                part(
                    1,
                    GPT_TYPE_EFI_SYSTEM,
                    "1D3128FC",
                    1_048_576,
                    104_857_600,
                    ESP,
                ),
                part(2, GPT_TYPE_MSR, "509FCA78", 105_906_176, 16_777_216, ""),
                part(3, BASIC, "19538B68", 122_683_392, 136_620_015_616, "C:"),
                part(
                    4,
                    GPT_TYPE_WINRE,
                    "FB002532",
                    136_742_699_008,
                    672_137_216,
                    RE_OLD,
                ),
                part(5, BASIC, "341EC36D", 137_415_884_800, 886_793_306_112, "D:"),
            ],
        )
    }

    /// …and after: C: shrank, WinRE was recreated behind it with a new GUID.
    fn after() -> DiskInfo {
        let mut d = before();
        d.partitions[2].size_bytes = 136_331_657_216;
        d.partitions[3] = part(
            4,
            GPT_TYPE_WINRE,
            "F53B6290",
            136_454_340_608,
            960_495_616,
            RE_NEW,
        );
        d
    }

    fn job_on(snapshot: DiskInfo) -> Job {
        let sources = vec![ESP.to_string(), "C:".into(), RE_OLD.to_string()];
        Job {
            name: "Disk_C_Win11".into(),
            source: ESP.into(),
            sources: sources.clone(),
            disk_source: Some(DiskSourceSpec {
                disk_number: 4,
                disk_model: snapshot.model.clone(),
                parts: vec![
                    DiskPartSpec {
                        part_index: 1,
                        device: ESP.into(),
                        filesystem: "fat32".into(),
                    },
                    DiskPartSpec {
                        part_index: 3,
                        device: "C:".into(),
                        filesystem: "ntfs".into(),
                    },
                    DiskPartSpec {
                        part_index: 4,
                        device: RE_OLD.into(),
                        filesystem: "ntfs".into(),
                    },
                ],
                bootable: false,
            }),
            source_disks: vec![snapshot],
            ..Default::default()
        }
    }

    #[test]
    fn an_unchanged_disk_leaves_the_job_alone() {
        let mut live = before();
        live.partitions[2].used_bytes = 12345; // usage moves daily — not a change
        assert!(matches!(
            reconcile_sources(&job_on(before()), &[live]),
            Refreshed::Unchanged
        ));
    }

    #[test]
    fn a_recreated_winre_is_followed_and_reported() {
        let Refreshed::Updated(r) = reconcile_sources(&job_on(before()), &[after()]) else {
            panic!("the job must be updated");
        };
        assert_eq!(
            r.job.sources,
            vec![ESP.to_string(), "C:".into(), RE_NEW.to_string()]
        );
        let ds = r.job.disk_source.as_ref().unwrap();
        assert_eq!(ds.parts[2].device, RE_NEW);
        assert_eq!(ds.parts[2].part_index, 4);
        assert_eq!(r.job.source_disks[0].partitions[3].volume_device, RE_NEW);
        assert_eq!(r.renamed, vec![(RE_OLD.to_string(), RE_NEW.to_string())]);
        // C: kept its name but not its size; WinRE moved — both are said.
        assert!(r.layout_note.contains("C:"), "{}", r.layout_note);
        assert!(!r.layout_note.is_empty());
        // Applied once, the job is current.
        assert!(matches!(
            reconcile_sources(&r.job, &[after()]),
            Refreshed::Unchanged
        ));
    }

    #[test]
    fn a_renumbered_disk_moves_the_job_with_it() {
        let mut live = before();
        live.device_id = r"\\.\PhysicalDrive2".into();
        let Refreshed::Updated(r) = reconcile_sources(&job_on(before()), &[live]) else {
            panic!("the disk number must follow the disk");
        };
        assert_eq!(r.job.disk_source.as_ref().unwrap().disk_number, 2);
        assert!(r.layout_note.is_empty(), "nothing moved on the disk itself");
    }

    #[test]
    fn a_vanished_data_partition_is_never_guessed() {
        let mut live = before();
        // C: deleted and a different data volume created in its slot.
        live.partitions[2] = part(3, BASIC, "AAAAAAAA", 122_683_392, 136_620_015_616, "X:");
        let Refreshed::Missing(why) = reconcile_sources(&job_on(before()), &[live]) else {
            panic!("a data partition must not be substituted");
        };
        assert!(why.contains("C:"), "{why}");
    }

    #[test]
    fn a_missing_disk_is_named() {
        let mut other = before();
        other.disk_guid = "11111111-0000-0000-0000-000000000000".into();
        assert!(matches!(
            reconcile_sources(&job_on(before()), &[other]),
            Refreshed::Missing(_)
        ));
    }

    #[test]
    fn a_job_without_a_snapshot_is_left_as_it_was() {
        let mut job = job_on(before());
        job.source_disks.clear();
        assert!(matches!(
            reconcile_sources(&job, &[after()]),
            Refreshed::Unchanged
        ));
    }
}
