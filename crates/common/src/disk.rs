use serde::{Deserialize, Serialize};

use crate::types::{FileSystem, PartitionTable};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskInfo {
    pub device_id: String,
    pub model: String,
    pub serial: String,
    pub size_bytes: u64,
    pub sector_size_logical: u32,
    pub sector_size_physical: u32,
    pub media_type: String,
    pub partition_table: PartitionTable,
    pub disk_guid: String,
    pub partitions: Vec<PartitionInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartitionInfo {
    pub index: u32,
    pub offset_bytes: u64,
    pub size_bytes: u64,
    /// Bytes in use within this partition's volume (`size - free`). 0 when the
    /// partition has no letter/filesystem or the figure is unavailable — the
    /// UI shows "—" in that case rather than "0 used".
    #[serde(default)]
    pub used_bytes: u64,
    pub gpt_type_guid: String,
    pub gpt_partition_guid: String,
    pub filesystem: FileSystem,
    pub volume_letter: Option<char>,
    /// Openable device path for this partition's volume: a drive letter ("C:")
    /// when it has one, else a volume GUID path (`\\?\Volume{GUID}`, no trailing
    /// backslash) for letterless system/hidden volumes (EFI, Recovery, …).
    /// Empty when the partition has no mountable volume (MSR, unknown Linux).
    #[serde(default)]
    pub volume_device: String,
    pub volume_label: String,
    pub fs_uuid: String,
    pub is_system: bool,
    pub is_boot: bool,
    /// The partition holds an encrypted container (LUKS / BitLocker). Its
    /// filesystem shows as `Unknown`, but it can still be backed up by
    /// supplying a password to decrypt it first.
    #[serde(default)]
    pub is_encrypted: bool,
}
