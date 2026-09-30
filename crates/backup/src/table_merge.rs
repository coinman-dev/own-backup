//! Reconciling the partition table a plan carries with the one already on the
//! disk.
//!
//! The restore designer builds its table out of `PartitionInfo`, which
//! describes *volumes*: where they start, how big they are, what filesystem is
//! inside. A GPT entry holds three more things that no volume query returns:
//!
//! - the **unique partition GUID**. Until the inventory stopped going through
//!   `Get-Partition`, which does not expose it, every entry arrived empty — and
//!   an empty GUID means "generate a fresh one". Regenerating it for the
//!   partitions that are merely *sitting there* renames them out from under the
//!   BCD and the UEFI boot entries, which reference partitions by exactly this
//!   value: the machine restores fine and then does not boot.
//! - the **attribute flags**. The designer sets them to 0. Bit 0 marks the ESP
//!   and the Microsoft Reserved partition as required; bit 63 is what keeps a
//!   Recovery partition from showing up as a drive letter.
//! - the **entry name**, which is not the volume label the designer substitutes.
//!
//! So before any table is written, every entry that describes a partition
//! *already on the disk, at the same place* takes those three fields back from
//! the disk. Only what the plan genuinely knows better — a partition receiving
//! a restore, whose identity comes from the copy — keeps the plan's values.
//!
//! And once that is done, the common case falls out for free: restoring into an
//! existing partition changes nothing in the table at all, so
//! [`same_layout`] says so and the caller can skip the write. Not writing is
//! the only completely safe way to rewrite the partition table of the disk the
//! running Windows is standing on.

use own_backup_common::gpt::{DiskLayout, GptPartitionEntry};
use own_backup_common::restore_plan::{PartitionKind, PartitionSpec, PartitionTableSpec};

/// GUIDs are compared as values, not as strings: `{...}` wrappers, case and
/// stray spaces are all noise that would otherwise read as a difference.
fn same_guid(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        s.trim()
            .trim_start_matches('{')
            .trim_end_matches('}')
            .to_ascii_uppercase()
    };
    norm(a) == norm(b)
}

fn is_blank(s: &str) -> bool {
    s.trim()
        .trim_start_matches('{')
        .trim_end_matches('}')
        .trim()
        .is_empty()
}

/// The entry on the disk occupying exactly the same sectors as `spec`.
///
/// Matched by geometry rather than by number: the partition number is what the
/// plan *claims*, the start and end sectors are what the partition *is*. A
/// renumbered but unmoved partition is still the same partition.
fn same_place<'a>(spec: &PartitionSpec, disk: &'a DiskLayout) -> Option<&'a GptPartitionEntry> {
    disk.partitions
        .iter()
        .find(|d| d.first_lba == spec.first_lba && d.last_lba == spec.last_lba)
}

/// What [`adopt_existing_entries`] took back from the disk, for the log.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Adopted {
    /// The disk's own GPT GUID was kept rather than a fresh one generated.
    pub kept_disk_id: bool,
    /// Partition numbers whose unique GUID came from the disk instead of being
    /// regenerated.
    pub kept_guid: Vec<u32>,
    /// Partition numbers whose attribute flags came from the disk.
    pub kept_attributes: Vec<u32>,
    /// Partition numbers whose GPT entry name came from the disk.
    pub kept_name: Vec<u32>,
}

impl Adopted {
    pub fn is_empty(&self) -> bool {
        !self.kept_disk_id
            && self.kept_guid.is_empty()
            && self.kept_attributes.is_empty()
            && self.kept_name.is_empty()
    }
}

/// Fill in the GPT fields the designer could not know from the layout that is
/// on the disk right now.
///
/// `owned` names the target partitions the plan is actually acting on — the
/// ones receiving data from a copy or being formatted. For those, what the plan
/// says wins wherever it says anything, because it is speaking for the copy.
///
/// Every *other* entry describes a partition that is merely there, and for
/// those the disk wins outright — not just where the plan is silent. The
/// designer builds its entries out of volume information and puts the *volume
/// label* in the GPT entry name, which is a different thing entirely: it turned
/// "Basic data partition" into "Windows" on the partition the running system
/// boots from, and a changed entry is a partition the partition manager has to
/// take apart and rebuild.
///
/// MBR tables have no equivalent problem: everything an MBR entry holds is
/// already in the spec.
pub fn adopt_existing_entries(
    spec: &mut PartitionTableSpec,
    disk: &DiskLayout,
    owned: &[u32],
) -> Adopted {
    let mut out = Adopted::default();
    // The disk's own GUID, for the same reason as a partition's: an empty one
    // means "generate a fresh value", and the BCD's {bootmgr} entry names the
    // boot disk by exactly this GUID. What the designer refuses to inherit is
    // the GUID of the disk the *copy* came from — that would put two disks with
    // one identity in the same machine. Keeping the target's own is the
    // opposite of that.
    if is_blank(&spec.disk_id) && !disk.disk_guid.is_empty() {
        spec.disk_id = disk.disk_guid.clone();
        out.kept_disk_id = true;
    }
    for e in spec.entries.iter_mut() {
        let Some(current) = same_place(e, disk) else {
            continue;
        };
        let index = e.index;
        // A partition the plan is not acting on keeps everything it has, not
        // just the fields the plan happens to leave blank.
        let bystander = !owned.contains(&index);
        let PartitionKind::Gpt {
            type_guid,
            part_guid,
            attributes,
            name,
        } = &mut e.kind
        else {
            continue;
        };
        if (bystander || is_blank(type_guid)) && !current.type_guid.is_empty() {
            *type_guid = current.type_guid.clone();
        }
        if (bystander || is_blank(part_guid)) && !current.part_guid.is_empty() {
            if !same_guid(part_guid, &current.part_guid) {
                out.kept_guid.push(index);
            }
            *part_guid = current.part_guid.clone();
        }
        if bystander || *attributes == 0 {
            if *attributes != current.attributes {
                out.kept_attributes.push(index);
            }
            *attributes = current.attributes;
        }
        if (bystander || name.trim().is_empty()) && name.trim() != current.name.trim() {
            *name = current.name.clone();
            out.kept_name.push(index);
        }
    }
    out
}

/// Whether this entry describes exactly what is already on the disk.
///
/// Used twice: to decide that a whole table needs no writing, and — on the disk
/// the running Windows boots from — to decide which entries the partition
/// manager is told to rewrite and which it must leave alone.
///
/// The partition *number* is deliberately not part of the comparison. It is
/// Windows' bookkeeping — assigned by the partition manager, counted from the
/// GPT entry array, and the two disagree whenever the array has a gap. What
/// identifies a partition here is where it starts, where it ends, and the
/// identity written in its entry.
pub fn entry_unchanged(spec: &PartitionSpec, disk: &DiskLayout) -> bool {
    let Some(current) = same_place(spec, disk) else {
        return false;
    };
    match &spec.kind {
        PartitionKind::Gpt {
            type_guid,
            part_guid,
            attributes,
            name,
        } => {
            same_guid(type_guid, &current.type_guid)
                // An empty unique GUID means "generate one", which is a change.
                && !is_blank(part_guid)
                && same_guid(part_guid, &current.part_guid)
                && *attributes == current.attributes
                && name.trim() == current.name.trim()
        }
        // An MBR spec cannot be compared against a GPT layout at all.
        PartitionKind::Mbr { .. } => false,
    }
}

/// Whether writing `spec` would leave the disk exactly as it already is.
///
/// The partition table of a live system disk is the one structure where "write
/// nothing" is meaningfully safer than "write the same bytes back": the write
/// goes through the partition manager, which tears volumes down and builds them
/// up again, and the volume it might tear down is the one Windows is running
/// from. Restoring into an existing partition does not change the layout, so
/// this is the normal case and not an optimisation.
pub fn same_layout(spec: &PartitionTableSpec, disk: &DiskLayout) -> bool {
    // A blank disk GUID is a change like any other: whoever writes the table
    // would put a freshly generated one there.
    !is_blank(&spec.disk_id)
        && same_guid(&spec.disk_id, &disk.disk_guid)
        && spec.entries.len() == disk.partitions.len()
        && spec.entries.iter().all(|e| entry_unchanged(e, disk))
}

/// One line per entry, for the log that has to explain a restore after the
/// fact. Deliberately verbose: this is the record of what was applied to the
/// user's partition table, and it is the only one that survives a crash.
pub fn describe_table(spec: &PartitionTableSpec) -> Vec<String> {
    spec.entries
        .iter()
        .map(|e| match &e.kind {
            PartitionKind::Gpt {
                type_guid,
                part_guid,
                attributes,
                name,
            } => format!(
                "#{} lba {}..={} ({} B) type={} id={} attr=0x{:016x} name={:?}",
                e.index,
                e.first_lba,
                e.last_lba,
                e.size_bytes(spec.sector_size),
                type_guid,
                if is_blank(part_guid) {
                    "<new>"
                } else {
                    part_guid.as_str()
                },
                attributes,
                name
            ),
            PartitionKind::Mbr {
                type_byte,
                bootable,
                logical,
            } => format!(
                "#{} lba {}..={} ({} B) type=0x{:02x} bootable={} logical={}",
                e.index,
                e.first_lba,
                e.last_lba,
                e.size_bytes(spec.sector_size),
                type_byte,
                bootable,
                logical
            ),
        })
        .collect()
}

/// The same, for what is on the disk now — so a log shows both sides.
pub fn describe_disk(disk: &DiskLayout) -> Vec<String> {
    disk.partitions
        .iter()
        .map(|p| {
            format!(
                "#{} lba {}..={} type={} id={} attr=0x{:016x} name={:?}",
                p.index, p.first_lba, p.last_lba, p.type_guid, p.part_guid, p.attributes, p.name
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use own_backup_common::restore_plan::PartitionScheme;

    const ESP_TYPE: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";
    const DATA_TYPE: &str = "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7";

    fn disk_entry(index: u32, first: u64, last: u64, ty: &str, attr: u64) -> GptPartitionEntry {
        GptPartitionEntry {
            index,
            type_guid: ty.into(),
            part_guid: format!("11111111-2222-3333-4444-00000000000{index}"),
            first_lba: first,
            last_lba: last,
            attributes: attr,
            name: format!("part{index}"),
        }
    }

    /// The layout of the machine this was written against: ESP, MSR, Windows,
    /// data, and a spare partition at the end.
    fn live_disk() -> DiskLayout {
        DiskLayout {
            disk_guid: "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE".into(),
            sector_size: 512,
            size_bytes: 512 * 1024 * 1024 * 1024,
            partitions: vec![
                disk_entry(1, 2048, 411_647, ESP_TYPE, 1),
                disk_entry(
                    2,
                    411_648,
                    673_791,
                    "E3C9E316-0B5C-4DB8-817D-F92DF00215AE",
                    1 << 63,
                ),
                disk_entry(3, 673_792, 134_889_471, DATA_TYPE, 0),
                disk_entry(4, 134_889_472, 409_600_000, DATA_TYPE, 0),
                disk_entry(5, 409_600_001, 503_316_479, DATA_TYPE, 0),
            ],
        }
    }

    /// What the designer produces out of `PartitionInfo`: right geometry, right
    /// type GUIDs, and nothing else — no unique GUIDs, no attributes.
    fn designed_from(disk: &DiskLayout) -> PartitionTableSpec {
        PartitionTableSpec {
            scheme: PartitionScheme::Gpt,
            sector_size: disk.sector_size,
            disk_size_bytes: disk.size_bytes,
            disk_id: String::new(),
            entries: disk
                .partitions
                .iter()
                .map(|p| PartitionSpec {
                    index: p.index,
                    first_lba: p.first_lba,
                    last_lba: p.last_lba,
                    kind: PartitionKind::Gpt {
                        type_guid: p.type_guid.clone(),
                        part_guid: String::new(),
                        attributes: 0,
                        name: String::new(),
                    },
                })
                .collect(),
        }
    }

    #[test]
    fn a_table_the_designer_built_from_the_disk_adopts_back_what_it_could_not_see() {
        // The bug this guards: an empty unique GUID means "generate a fresh
        // one", so applying the designer's table renamed every partition on the
        // disk — including the ESP and the Windows partition the BCD points at.
        let disk = live_disk();
        let mut spec = designed_from(&disk);
        let adopted = adopt_existing_entries(&mut spec, &disk, &[]);

        assert!(
            adopted.kept_disk_id,
            "the disk's own GUID too — {{bootmgr}} names it"
        );
        assert_eq!(spec.disk_id, disk.disk_guid);
        assert_eq!(
            adopted.kept_guid,
            vec![1, 2, 3, 4, 5],
            "every unique GUID came back"
        );
        assert_eq!(
            adopted.kept_attributes,
            vec![1, 2],
            "only the two that have flags set"
        );
        for (e, p) in spec.entries.iter().zip(disk.partitions.iter()) {
            let PartitionKind::Gpt {
                part_guid,
                attributes,
                name,
                ..
            } = &e.kind
            else {
                panic!("expected GPT");
            };
            assert_eq!(part_guid, &p.part_guid);
            assert_eq!(*attributes, p.attributes);
            assert_eq!(name, &p.name);
        }
    }

    #[test]
    fn restoring_into_an_existing_partition_changes_nothing_in_the_table() {
        // The user's case: pick a spare partition, restore a copy into it. The
        // layout is untouched, so the table must not be rewritten at all — on a
        // live system disk that write is what takes the machine down.
        let disk = live_disk();
        let mut spec = designed_from(&disk);
        adopt_existing_entries(&mut spec, &disk, &[]);
        assert!(same_layout(&spec, &disk));
    }

    #[test]
    fn a_partition_receiving_a_copy_keeps_the_identity_the_copy_gave_it() {
        // Its GUID came out of the backup on purpose (the restored system's own
        // BCD refers to it); adopting the target's current one would undo that.
        let disk = live_disk();
        let mut spec = designed_from(&disk);
        let from_copy = "99999999-8888-7777-6666-555555555555";
        if let PartitionKind::Gpt { part_guid, .. } = &mut spec.entries[4].kind {
            *part_guid = from_copy.into();
        }
        adopt_existing_entries(&mut spec, &disk, &[5]);
        let PartitionKind::Gpt { part_guid, .. } = &spec.entries[4].kind else {
            panic!()
        };
        assert_eq!(part_guid, from_copy);
        assert!(!same_layout(&spec, &disk), "that entry does change");
        assert!(
            entry_unchanged(&spec.entries[0], &disk),
            "…and the others do not"
        );
    }

    #[test]
    fn a_partition_nobody_is_restoring_into_keeps_its_gpt_name() {
        // The designer puts the *volume label* in the GPT entry name, which is
        // not the same field: it turned "Basic data partition" into "Windows"
        // on the partition the running system boots from. That entry then reads
        // as changed, and a changed entry is one the partition manager takes
        // apart and rebuilds — on C:, while Windows is running from it.
        let disk = live_disk();
        let mut spec = designed_from(&disk);
        if let PartitionKind::Gpt { name, .. } = &mut spec.entries[2].kind {
            *name = "Windows".into();
        }
        let adopted = adopt_existing_entries(&mut spec, &disk, &[5]);
        assert!(adopted.kept_name.contains(&3), "{:?}", adopted.kept_name);
        let PartitionKind::Gpt { name, .. } = &spec.entries[2].kind else {
            panic!()
        };
        assert_eq!(name, "part3", "the name on the disk won");
        assert!(
            entry_unchanged(&spec.entries[2], &disk),
            "so C: is not rewritten"
        );
    }

    #[test]
    fn a_partition_being_restored_into_may_state_a_new_name() {
        // The other side of the same rule: this one the plan does speak for.
        let disk = live_disk();
        let mut spec = designed_from(&disk);
        if let PartitionKind::Gpt { name, .. } = &mut spec.entries[4].kind {
            *name = "kubuntu_boot".into();
        }
        adopt_existing_entries(&mut spec, &disk, &[5]);
        let PartitionKind::Gpt { name, .. } = &spec.entries[4].kind else {
            panic!()
        };
        assert_eq!(name, "kubuntu_boot");
        assert!(!entry_unchanged(&spec.entries[4], &disk));
    }

    #[test]
    fn a_new_partition_has_nothing_to_adopt_and_counts_as_a_change() {
        let disk = live_disk();
        let mut spec = designed_from(&disk);
        spec.entries.push(PartitionSpec {
            index: 6,
            first_lba: 503_316_480,
            last_lba: 600_000_000,
            kind: PartitionKind::Gpt {
                type_guid: DATA_TYPE.into(),
                part_guid: String::new(),
                attributes: 0,
                name: "New".into(),
            },
        });
        adopt_existing_entries(&mut spec, &disk, &[]);
        assert!(!entry_unchanged(&spec.entries[5], &disk));
        assert!(!same_layout(&spec, &disk));
    }

    #[test]
    fn deleting_a_partition_is_a_change_even_though_every_survivor_matches() {
        let disk = live_disk();
        let mut spec = designed_from(&disk);
        spec.entries.pop();
        adopt_existing_entries(&mut spec, &disk, &[]);
        assert!(spec.entries.iter().all(|e| entry_unchanged(e, &disk)));
        assert!(
            !same_layout(&spec, &disk),
            "the count differs — one was dropped"
        );
    }

    #[test]
    fn a_renumbered_but_unmoved_partition_is_still_the_same_partition() {
        // A GPT entry array with a gap numbers its partitions differently from
        // the partition manager, and neither is wrong. Rewriting the table over
        // that disagreement would be — so it is not a difference.
        let disk = live_disk();
        let mut spec = designed_from(&disk);
        adopt_existing_entries(&mut spec, &disk, &[]);
        for e in spec.entries.iter_mut() {
            e.index += 10;
        }
        assert!(same_layout(&spec, &disk));
    }

    #[test]
    fn a_moved_or_resized_partition_adopts_nothing() {
        // Same number, different sectors: a different partition as far as the
        // disk is concerned, and its old identity must not follow it.
        let disk = live_disk();
        let mut spec = designed_from(&disk);
        spec.entries[4].last_lba += 2048;
        let adopted = adopt_existing_entries(&mut spec, &disk, &[]);
        assert!(!adopted.kept_guid.contains(&5));
        assert!(!same_layout(&spec, &disk));
    }

    #[test]
    fn a_table_that_would_rename_the_disk_itself_is_not_the_same_layout() {
        // Same partitions, fresh disk GUID: the machine still boots off a disk
        // the BCD no longer names. Writing that is a change, and has to read as
        // one.
        let disk = live_disk();
        let mut spec = designed_from(&disk);
        adopt_existing_entries(&mut spec, &disk, &[]);
        assert!(same_layout(&spec, &disk));
        spec.disk_id = "00000000-1111-2222-3333-444444444444".into();
        assert!(!same_layout(&spec, &disk));
    }

    #[test]
    fn guids_compare_by_value_not_by_spelling() {
        let disk = live_disk();
        let mut spec = designed_from(&disk);
        adopt_existing_entries(&mut spec, &disk, &[]);
        if let PartitionKind::Gpt {
            part_guid,
            type_guid,
            ..
        } = &mut spec.entries[0].kind
        {
            *part_guid = format!("{{{}}}", part_guid.to_lowercase());
            *type_guid = type_guid.to_lowercase();
        }
        assert!(
            entry_unchanged(&spec.entries[0], &disk),
            "braces and case are not a difference"
        );
    }

    #[test]
    fn describing_a_table_names_every_field_that_matters() {
        let disk = live_disk();
        let mut spec = designed_from(&disk);
        assert!(
            describe_table(&spec)[0].contains("<new>"),
            "an unset GUID is called out"
        );
        adopt_existing_entries(&mut spec, &disk, &[]);
        let line = &describe_table(&spec)[1];
        assert!(line.contains("attr=0x8000000000000000"), "{line}");
        assert_eq!(describe_disk(&disk).len(), 5);
    }
}
