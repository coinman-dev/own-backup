//! GPT disk-layout capture for whole-disk (bootable) backup & restore.
//!
//! A whole-disk backup records the source disk's GUID Partition Table so a
//! restore can recreate it *byte-identically* — same partition type/unique
//! GUIDs, offsets, sizes, names and attributes. Preserving the unique GUIDs is
//! what keeps a UEFI system bootable: the BCD inside the EFI System Partition
//! references partitions by GUID, so with the GUIDs restored the boot chain
//! keeps working without any BCD surgery.

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
    /// `kept`) plus the Microsoft Reserved partition — MSR has no filesystem/
    /// image but Windows needs its slot. Everything else (deselected data
    /// volumes) is dropped from the layout; on restore their space is left free.
    pub fn retain_for_backup(&mut self, kept: &std::collections::HashSet<u32>) {
        self.partitions
            .retain(|p| kept.contains(&p.index) || p.type_guid.eq_ignore_ascii_case(GPT_TYPE_MSR));
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
    let last_end = layout.partitions.iter().map(|p| p.last_lba + 1).max().unwrap_or(mib_lbas);
    let after = last_end.max(mib_lbas);
    let meta_first_lba = after.div_ceil(mib_lbas) * mib_lbas;
    let meta_lbas = meta_size.div_ceil(sector).max(1);
    let meta_last_lba = meta_first_lba + meta_lbas - 1;
    // Reserve the secondary GPT (33 sectors) after the meta, then round the whole
    // image up to a 1 MiB boundary.
    let raw_end = (meta_last_lba + 1 + 33) * sector;
    let image_size = raw_end.div_ceil(1024 * 1024) * (1024 * 1024);
    WholeDiskImagePlan { image_size, meta_first_lba, meta_last_lba }
}

// ---------------------------------------------------------------------------
// Disk-backup manifest (`.obdm`)
// ---------------------------------------------------------------------------

/// EFI System Partition GPT *type* GUID.
pub const GPT_TYPE_EFI_SYSTEM: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";
/// Microsoft Reserved Partition (MSR) GPT *type* GUID — no filesystem; its GPT
/// entry is recreated on restore but no data is backed up.
pub const GPT_TYPE_MSR: &str = "E3C9E316-0B5C-4DB8-817D-F92DF00215AE";

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

    #[test]
    fn retain_for_backup_keeps_kept_plus_msr() {
        // ESP(1), MSR(2), C:(3), D:(4). Keep ESP + C:, drop D:. MSR stays.
        let mut layout = DiskLayout {
            disk_guid: "g".into(),
            sector_size: 512,
            size_bytes: 1 << 40,
            partitions: vec![
                part(1, GPT_TYPE_EFI_SYSTEM, 2048, 4095),
                part(2, GPT_TYPE_MSR, 4096, 8191),
                part(3, "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7", 8192, 100_000),
                part(4, "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7", 100_001, 500_000),
            ],
        };
        let kept: HashSet<u32> = [1u32, 3].into_iter().collect();
        layout.retain_for_backup(&kept);
        let idx: Vec<u32> = layout.partitions.iter().map(|p| p.index).collect();
        assert_eq!(idx, vec![1, 2, 3]); // D: (4) dropped, MSR (2) kept
    }

    #[test]
    fn retain_matches_msr_case_insensitively() {
        let mut layout = DiskLayout {
            disk_guid: "g".into(),
            sector_size: 512,
            size_bytes: 1 << 30,
            partitions: vec![part(2, &GPT_TYPE_MSR.to_lowercase(), 4096, 8191)],
        };
        layout.retain_for_backup(&HashSet::new());
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
        assert_eq!(plan.meta_last_lba - plan.meta_first_lba + 1, meta_size / 512);
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
}
