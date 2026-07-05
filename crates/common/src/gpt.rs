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
