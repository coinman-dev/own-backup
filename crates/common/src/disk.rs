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
    /// This volume is the one the CURRENTLY RUNNING system booted from. False
    /// for everything on a boot medium, where nothing on a real disk is running
    /// — see [`Self::has_os`] for "holds an installed system", which is what a
    /// whole-disk copy actually needs to know.
    pub is_system: bool,
    pub is_boot: bool,
    /// An operating system is INSTALLED here, running or not (probed by looking
    /// for its kernel on the volume).
    ///
    /// The distinction only shows up on rescue media, and it showed up as a bug:
    /// "bootable copy of this disk" auto-selected partitions by `is_system`, so
    /// booted from the medium it selected the EFI partition and left Windows
    /// itself out — a copy that could never boot. Older images and older
    /// services do not send this field, hence the default.
    #[serde(default)]
    pub has_os: bool,
    /// The partition holds an encrypted container (LUKS / BitLocker). Its
    /// filesystem shows as `Unknown`, but it can still be backed up by
    /// supplying a password to decrypt it first.
    #[serde(default)]
    pub is_encrypted: bool,
    /// How an encrypted volume can be opened, in the words of its own
    /// metadata: "пароль", "ключ восстановления (48 цифр)", "TPM + PIN"…
    ///
    /// A BitLocker volume says this about itself, and it costs one read to
    /// ask. Without it the interface asks every encrypted volume for a
    /// password and answers "не подходит" to a volume that has none — which is
    /// not a wrong password but a wrong question. Empty for LUKS and for
    /// anything we could not read.
    #[serde(default)]
    pub unlock_methods: Vec<String>,
    /// Can Own Backup open this volume with something the user can type?
    #[serde(default)]
    pub unlockable: bool,
}
