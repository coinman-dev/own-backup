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
    pub gpt_type_guid: String,
    pub gpt_partition_guid: String,
    pub filesystem: FileSystem,
    pub volume_letter: Option<char>,
    pub volume_label: String,
    pub fs_uuid: String,
    pub is_system: bool,
    pub is_boot: bool,
}
