//! GPT disk-layout capture for whole-disk (bootable) backup & restore.
//!
//! A whole-disk backup records the source disk's GUID Partition Table so a
//! restore can recreate it *byte-identically* — same partition type/unique
//! GUIDs, offsets, sizes, names and attributes. Preserving the unique GUIDs is
//! what keeps a UEFI system bootable: the BCD inside the EFI System Partition
//! references partitions by GUID, so with the GUIDs restored the boot chain
//! keeps working without any BCD surgery.

use crate::disk::DiskInfo;
use serde::{Deserialize, Serialize};

/// One partition as recorded in the source disk's GPT.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GptPartitionEntry {
    /// 1-based partition number (its slot in the GPT entry array).
    pub index: u32,
    /// Partition *type* GUID (e.g. EFI System / Microsoft Reserved / Basic Data).
    pub type_guid: String,
    /// Partition *unique* GUID — the identity the BCD/UEFI reference. Preserved
    /// verbatim on restore.
    pub part_guid: String,
    pub first_lba: u64,
    pub last_lba: u64,
    /// GPT attribute flags (bit 0 = required/system, bit 60 = read-only, …).
    pub attributes: u64,
    /// Partition name (UTF-16 on disk, up to 36 code units).
    pub name: String,
}

impl GptPartitionEntry {
    /// Byte offset of the partition on the disk (for the given sector size).
    pub fn offset_bytes(&self, sector_size: u32) -> u64 {
        self.first_lba * sector_size as u64
    }

    /// Partition length in bytes (inclusive LBA range).
    pub fn size_bytes(&self, sector_size: u32) -> u64 {
        (self.last_lba + 1 - self.first_lba) * sector_size as u64
    }
}

/// The full GPT of a physical disk, captured so a restore can rebuild it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiskLayout {
    /// Disk GUID from the GPT header.
    pub disk_guid: String,
    pub sector_size: u32,
    /// Whole-disk size in bytes.
    pub size_bytes: u64,
    pub partitions: Vec<GptPartitionEntry>,
}

impl DiskLayout {
    /// Variant B: keep only the partitions being backed up (their index is in
    /// `kept`), plus the Microsoft Reserved partition when the copy carries
    /// Windows ([`Self::copy_carries_windows`]) — MSR has no filesystem/image
    /// but Windows needs its slot. Everything else (deselected volumes) is
    /// dropped from the layout; on restore their space is left free.
    ///
    /// `source` is the disk as the inventory saw it; it is what tells the
    /// Windows volume from a data volume, which share one GPT type.
    pub fn retain_for_backup(
        &mut self,
        kept: &std::collections::HashSet<u32>,
        source: Option<&DiskInfo>,
    ) {
        let keep_msr = self.copy_carries_windows(kept, source);
        self.partitions.retain(|p| {
            kept.contains(&p.index) || (keep_msr && p.type_guid.eq_ignore_ascii_case(GPT_TYPE_MSR))
        });
    }

    /// Does a copy of the `kept` partitions carry Windows — the system the
    /// Microsoft Reserved partition belongs to?
    ///
    /// MSR used to go into EVERY whole-disk copy. A copy of the data volume D:
    /// alone then carried Windows' reserved slot too, and the wizard showed it
    /// ticked next to D: with no way to untick it (2026-09-25). It belongs to
    /// the Windows side of the disk: the EFI System Partition, WinRE, and the
    /// volume Windows is installed on — which only the inventory can name
    /// (`is_system` / `has_os`), since C: and D: are both "Basic Data".
    ///
    /// With no inventory to ask, keep it, as before: 16 MiB of empty slot is
    /// cheap, a Windows restored without its MSR is not.
    pub fn copy_carries_windows(
        &self,
        kept: &std::collections::HashSet<u32>,
        source: Option<&DiskInfo>,
    ) -> bool {
        let Some(source) = source else { return true };
        self.partitions
            .iter()
            .filter(|p| kept.contains(&p.index))
            .any(|p| {
                p.type_guid.eq_ignore_ascii_case(GPT_TYPE_EFI_SYSTEM)
                    || p.type_guid.eq_ignore_ascii_case(GPT_TYPE_WINRE)
                    || source
                        .partitions
                        .iter()
                        .find(|sp| sp.index == p.index)
                        .is_some_and(|sp| sp.is_system || sp.has_os)
            })
    }

    /// Do both layouts put the same partitions in the same places?
    ///
    /// A differencing child inherits its parent's GPT and writes each partition
    /// at the offset the disk has NOW. When the two disagree, the chain would
    /// restore today's bytes into yesterday's slots. Windows Update does exactly
    /// that while servicing WinRE: on 2026-10-14 it shrank C: by 275 MiB and
    /// recreated the recovery partition behind it, leaving the end of the last
    /// partition — and so the image size — where it was.
    ///
    /// Unique GUIDs are not compared: a recreated partition gets a new one, but
    /// the child records the current GUIDs in its own manifest; only where the
    /// data sits has to match.
    pub fn same_partition_geometry(&self, other: &DiskLayout) -> bool {
        if self.sector_size != other.sector_size || self.partitions.len() != other.partitions.len()
        {
            return false;
        }
        fn sorted(l: &DiskLayout) -> Vec<&GptPartitionEntry> {
            let mut v: Vec<&GptPartitionEntry> = l.partitions.iter().collect();
            v.sort_by_key(|p| p.index);
            v
        }
        sorted(self).iter().zip(sorted(other)).all(|(a, b)| {
            a.index == b.index
                && a.first_lba == b.first_lba
                && a.last_lba == b.last_lba
                && a.type_guid.eq_ignore_ascii_case(&b.type_guid)
        })
    }
}

/// Geometry of a single **whole-disk** backup image: the real partitions sit at
/// their original offsets, and our `OwnBackupMeta` partition (embedded sidecar)
/// is appended after the last one. Restore ignores the meta partition — it is
/// not part of the source disk's layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WholeDiskImagePlan {
    /// Total virtual size of the image (partitions + meta + GPT tail), 1 MiB-rounded.
    pub image_size: u64,
    /// First LBA of the appended `OwnBackupMeta` partition.
    pub meta_first_lba: u64,
    /// Last LBA of the `OwnBackupMeta` partition.
    pub meta_last_lba: u64,
}

/// Plan a whole-disk image for `layout` (already filtered to the kept
/// partitions) with an `OwnBackupMeta` partition of `meta_size` bytes appended
/// 1 MiB-aligned after the last real partition. `layout.partitions` may be empty
/// (then the meta sits right after the GPT head).
pub fn plan_whole_disk_image(layout: &DiskLayout, meta_size: u64) -> WholeDiskImagePlan {
    let sector = layout.sector_size.max(512) as u64;
    let mib_lbas = (1024 * 1024) / sector;
    // Start of usable space (past the 1 MiB GPT head) or right after the last
    // real partition, whichever is later — then 1 MiB-align.
    let last_end = layout
        .partitions
        .iter()
        .map(|p| p.last_lba + 1)
        .max()
        .unwrap_or(mib_lbas);
    let after = last_end.max(mib_lbas);
    let meta_first_lba = after.div_ceil(mib_lbas) * mib_lbas;
    let meta_lbas = meta_size.div_ceil(sector).max(1);
    let meta_last_lba = meta_first_lba + meta_lbas - 1;
    // Reserve the secondary GPT (33 sectors) after the meta, then round the whole
    // image up to a 1 MiB boundary.
    let raw_end = (meta_last_lba + 1 + 33) * sector;
    let image_size = raw_end.div_ceil(1024 * 1024) * (1024 * 1024);
    WholeDiskImagePlan {
        image_size,
        meta_first_lba,
        meta_last_lba,
    }
}

// ---------------------------------------------------------------------------
// Disk-backup manifest (`.obdm`)
// ---------------------------------------------------------------------------

/// EFI System Partition GPT *type* GUID.
pub const GPT_TYPE_EFI_SYSTEM: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";
/// Microsoft Reserved Partition (MSR) GPT *type* GUID — no filesystem; its GPT
/// entry is recreated on restore but no data is backed up.
pub const GPT_TYPE_MSR: &str = "E3C9E316-0B5C-4DB8-817D-F92DF00215AE";
/// Windows Recovery Environment partition GPT *type* GUID.
pub const GPT_TYPE_WINRE: &str = "DE94BBA4-06D1-4D40-A16A-BFD50179D6AC";

/// One backed-up partition inside a whole-disk backup: the GPT slot it fills
/// and the per-partition image that holds its data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskImageRef {
    /// Matches [`GptPartitionEntry::index`] in the layout.
    pub part_index: u32,
    /// Source-volume token (also the token in the image file name).
    pub token: String,
    /// Image file name (relative to the destination folder).
    pub image_file_name: String,
    /// Filesystem name for display (e.g. "ntfs", "fat32", "ext4").
    pub filesystem: String,
    /// The source volume's own encryption (BitLocker/LUKS) and how this image
    /// captured it — decrypted, or verbatim ciphertext. `None` when the source
    /// was not an encrypted container, or for images written before the field
    /// existed.
    ///
    /// The restore screen says opposite things in those two cases — one volume
    /// comes back readable but unprotected, the other comes back exactly as it
    /// was and needs its password — so guessing between them is not an option.
    #[serde(default)]
    pub encryption: Option<crate::image::VolumeEncryptionState>,
}

/// Sidecar written once per whole-disk backup run (`<name>_diskN_<stamp>.obdm`).
/// Restore reads it to recreate the GPT and lay each partition's image back at
/// its original offset. Partitions with no image (e.g. MSR) are recreated as
/// empty GPT entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskBackupManifest {
    pub format_version: u32,
    pub created_at: String,
    #[serde(default)]
    pub job_id: Option<String>,
    /// Source physical drive number (`\\.\PhysicalDrive{disk_number}`).
    pub disk_number: u32,
    pub disk_model: String,
    pub layout: DiskLayout,
    pub images: Vec<DiskImageRef>,
    /// Full snapshot of the source disk with rich per-partition detail (labels,
    /// drive letters, used bytes, boot/system flags) captured from the live
    /// inventory at backup time. `layout` above is enough to *recreate* the disk
    /// byte-identically; this is what a restore UI / Boot-CD shows to identify
    /// the archived disk WITHOUT any external job database (`jobs.json`). `None`
    /// for CLI runs or backups taken before snapshots were captured.
    #[serde(default)]
    pub source_disk: Option<DiskInfo>,
}

impl DiskBackupManifest {
    pub const FORMAT_VERSION: u32 = 1;
    /// File extension for the disk-backup manifest.
    pub const EXTENSION: &'static str = "obdm";
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn part(index: u32, type_guid: &str, first: u64, last: u64) -> GptPartitionEntry {
        GptPartitionEntry {
            index,
            type_guid: type_guid.into(),
            part_guid: format!("{index:08x}-0000-0000-0000-000000000000"),
            first_lba: first,
            last_lba: last,
            attributes: 0,
            name: String::new(),
        }
    }

    const BASIC_DATA: &str = "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7";

    /// ESP(1), MSR(2), C:(3), D:(4) — a Windows system disk.
    fn windows_disk() -> DiskLayout {
        DiskLayout {
            disk_guid: "g".into(),
            sector_size: 512,
            size_bytes: 1 << 40,
            partitions: vec![
                part(1, GPT_TYPE_EFI_SYSTEM, 2048, 4095),
                part(2, GPT_TYPE_MSR, 4096, 8191),
                part(3, BASIC_DATA, 8192, 100_000),
                part(4, BASIC_DATA, 100_001, 500_000),
            ],
        }
    }

    /// The same disk as the inventory sees it: Windows lives on partition 3.
    fn windows_inventory(layout: &DiskLayout) -> DiskInfo {
        DiskInfo {
            device_id: r"\\.\PhysicalDrive0".into(),
            model: "m".into(),
            serial: String::new(),
            size_bytes: layout.size_bytes,
            sector_size_logical: 512,
            sector_size_physical: 512,
            media_type: String::new(),
            partition_table: crate::types::PartitionTable::Gpt,
            disk_guid: layout.disk_guid.clone(),
            partitions: layout
                .partitions
                .iter()
                .map(|p| crate::disk::PartitionInfo {
                    index: p.index,
                    offset_bytes: p.offset_bytes(512),
                    size_bytes: p.size_bytes(512),
                    used_bytes: 0,
                    gpt_type_guid: p.type_guid.clone(),
                    gpt_partition_guid: p.part_guid.clone(),
                    filesystem: crate::types::FileSystem::Unknown,
                    volume_letter: None,
                    volume_device: String::new(),
                    volume_label: String::new(),
                    fs_uuid: String::new(),
                    is_system: p.index == 3,
                    has_os: p.index == 3,
                    is_boot: false,
                    linux_last_mounted: String::new(),
                    is_encrypted: false,
                    unlock_methods: Vec::new(),
                    recovery_key_only: false,
                    unlockable: true,
                    gpt_attributes: 0,
                    mbr_type: 0,
                    kernel_device: String::new(),
                    mapper_kind: String::new(),
                })
                .collect(),
        }
    }

    fn retained(kept: &[u32], with_inventory: bool) -> Vec<u32> {
        let mut layout = windows_disk();
        let inv = windows_inventory(&layout);
        let kept: HashSet<u32> = kept.iter().copied().collect();
        layout.retain_for_backup(&kept, with_inventory.then_some(&inv));
        layout.partitions.iter().map(|p| p.index).collect()
    }

    #[test]
    fn retain_for_backup_keeps_kept_plus_msr() {
        // Keep ESP + C:, drop D:. MSR stays: the copy carries Windows.
        assert_eq!(retained(&[1, 3], true), vec![1, 2, 3]);
    }

    #[test]
    fn msr_follows_the_windows_volume_not_the_data_one() {
        // D: alone is not Windows: its copy has no business with the MSR.
        assert_eq!(retained(&[4], true), vec![4]);
        // C: alone is: the MSR is Windows' slot.
        assert_eq!(retained(&[3], true), vec![2, 3]);
        // So is the ESP alone.
        assert_eq!(retained(&[1], true), vec![1, 2]);
    }

    #[test]
    fn msr_kept_when_nothing_says_which_volume_is_windows() {
        // No inventory: C: and D: are both Basic Data, so keep it, as before.
        assert_eq!(retained(&[4], false), vec![2, 4]);
    }

    #[test]
    fn retain_matches_msr_case_insensitively() {
        let mut layout = DiskLayout {
            disk_guid: "g".into(),
            sector_size: 512,
            size_bytes: 1 << 30,
            partitions: vec![part(2, &GPT_TYPE_MSR.to_lowercase(), 4096, 8191)],
        };
        layout.retain_for_backup(&HashSet::new(), None);
        assert_eq!(layout.partitions.len(), 1); // MSR kept despite lowercase GUID
    }

    #[test]
    fn plan_whole_disk_image_appends_meta_after_last_partition() {
        // Last partition ends at LBA 500_000; meta after it, 1 MiB-aligned.
        let layout = DiskLayout {
            disk_guid: "g".into(),
            sector_size: 512,
            size_bytes: 1 << 40,
            partitions: vec![
                part(1, GPT_TYPE_EFI_SYSTEM, 2048, 4095),
                part(3, "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7", 8192, 500_000),
            ],
        };
        let meta_size = 2 * 1024 * 1024; // 2 MiB
        let plan = plan_whole_disk_image(&layout, meta_size);
        let mib = 2048u64; // 1 MiB in 512-b sectors
                           // meta starts 1 MiB-aligned at/after LBA 500_001.
        assert_eq!(plan.meta_first_lba % mib, 0);
        assert!(plan.meta_first_lba >= 500_001);
        assert_eq!(
            plan.meta_last_lba - plan.meta_first_lba + 1,
            meta_size / 512
        );
        // image size is 1 MiB-rounded and past the meta + secondary GPT.
        assert_eq!(plan.image_size % (1024 * 1024), 0);
        assert!(plan.image_size >= (plan.meta_last_lba + 1) * 512);
    }

    #[test]
    fn plan_whole_disk_image_empty_layout_meta_after_head() {
        let layout = DiskLayout {
            disk_guid: "g".into(),
            sector_size: 512,
            size_bytes: 1 << 30,
            partitions: vec![],
        };
        let plan = plan_whole_disk_image(&layout, 1024 * 1024);
        assert_eq!(plan.meta_first_lba, 2048); // right after the 1 MiB GPT head
    }

    /// The 2026-10-14 Windows Update: C: shrinks, WinRE is recreated in the
    /// freed space with a new GUID, and the end of the last partition stays put.
    #[test]
    fn a_shrunk_c_and_moved_winre_is_a_different_geometry() {
        let before = DiskLayout {
            disk_guid: "g".into(),
            sector_size: 512,
            size_bytes: 1 << 40,
            partitions: vec![
                part(1, GPT_TYPE_EFI_SYSTEM, 2048, 206_847),
                part(3, BASIC_DATA, 239_616, 267_073_535),
                part(4, GPT_TYPE_WINRE, 267_073_536, 268_386_303),
            ],
        };
        let mut after = before.clone();
        after.partitions[1].last_lba = 266_510_335;
        after.partitions[2].first_lba = 266_510_336;
        after.partitions[2].part_guid = "new".into();
        assert!(!before.same_partition_geometry(&after));

        // Only GUIDs differ: same places, same geometry.
        let mut renamed = before.clone();
        renamed.partitions[2].part_guid = "new".into();
        renamed.partitions.reverse();
        assert!(before.same_partition_geometry(&renamed));

        // A partition fewer is not the same disk either.
        let mut fewer = before.clone();
        fewer.partitions.pop();
        assert!(!before.same_partition_geometry(&fewer));
    }
}
