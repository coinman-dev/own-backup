use serde::{Deserialize, Serialize};

use crate::types::{FileSystem, PartitionTable};

/// `#[serde(default = …)]` for a flag whose absence must read as "yes".
fn yes() -> bool {
    true
}

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
    /// Where a Linux filesystem says it was last mounted (`s_last_mounted`,
    /// ext2/3/4). `"/"` is the system root, `"/home"` or `"/mnt/data"` is data.
    ///
    /// The only thing on a Linux volume that says what it is FOR, and it costs
    /// one superblock read from Windows — no mounting. Everything else about a
    /// Linux partition (its GPT type, its filesystem) says the same for a root
    /// and for a data volume, which is how a "bootable copy" ends up either
    /// dragging in a data disk or leaving the system out. Empty when the
    /// filesystem is not ext, has never been mounted, or could not be read.
    #[serde(default)]
    pub linux_last_mounted: String,
    /// The partition holds an encrypted container (LUKS / BitLocker). Its
    /// filesystem shows as `Unknown`, but it can still be backed up by
    /// supplying a password to decrypt it first.
    #[serde(default)]
    pub is_encrypted: bool,
    /// How an encrypted volume can be opened, in the words of its own
    /// metadata: "пароль", "ключ восстановления (48 цифр)", "TPM + PIN"…
    ///
    /// A BitLocker volume says this about itself, and a LUKS container says it
    /// through its keyslots; both cost one read to ask. Without it the
    /// interface asks every encrypted volume for a password and answers "не
    /// подходит" to a volume that has none — which is not a wrong password but
    /// a wrong question. Empty for anything we could not read.
    #[serde(default)]
    pub unlock_methods: Vec<String>,
    /// The only way in a person can type is the 48-digit recovery key — a
    /// BitLocker volume with no password protector, which is the normal state
    /// of a system volume. Decided from the protectors' codes, not from the
    /// names in `unlock_methods`: those are shown in the window's language,
    /// and the wizard used to find «пароль» in them to decide what to ask for.
    #[serde(default)]
    pub recovery_key_only: bool,
    /// Can Own Backup open this volume with something the user can type?
    ///
    /// Defaults to `true`, and the inventory leaves it there: only a positive
    /// answer from the container's own metadata ("TPM only", "a cipher we do
    /// not implement") may set it to `false`. An unread volume is an unknown
    /// one, and refusing to even offer the password field on a "don't know" is
    /// how a perfectly openable LUKS partition ended up marked "открыть нечем".
    #[serde(default = "yes")]
    pub unlockable: bool,
}
