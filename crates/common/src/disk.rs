use serde::{Deserialize, Serialize};

use crate::types::{FileSystem, PartitionTable};

/// `#[serde(default = …)]` for a flag whose absence must read as "yes".
fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskInfo {
    pub device_id: String,
    /// Linux: the disk's name that survives a reboot, `/dev/disk/by-id/…` —
    /// what a whole-disk job keeps in `disk_source.device` (decision R1).
    /// [`Self::device_id`] stays the kernel name (`/dev/sda`), which the
    /// window shows and which can name another disk after the next boot.
    /// Empty when udev gave the disk no such name (a virtio disk without a
    /// serial, a loop device), for a device-mapper volume, and on Windows.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stable_device: String,
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
    /// GPT attribute flags of the partition entry: bit 62 "hidden" and bit 0
    /// "required by the platform" are how a vendor recovery partition typed as
    /// Basic Data keeps itself out of Explorer. 0 on MBR disks and in data
    /// saved before the field existed.
    #[serde(default)]
    pub gpt_attributes: u64,
    /// MBR partition type byte (0x07 NTFS, 0x27 hidden recovery, …); 0 on GPT.
    #[serde(default)]
    pub mbr_type: u8,
    /// Linux: the kernel's own name for this volume's device (`/dev/nvme0n1p2`,
    /// `/dev/dm-0`) — what the system's tools show, and what sysfs is asked
    /// about. [`Self::volume_device`] is then a name that survives a reboot
    /// (`/dev/disk/by-partuuid/…`, `/dev/mapper/…`), and that is what a job
    /// keeps: `sda` and `sdb` can swap places from one boot to the next.
    /// Empty on Windows, and not written then.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kernel_device: String,
    /// Linux: the volume is not a partition but a device-mapper device on top
    /// of one — `crypt` (an opened LUKS or BitLocker container: the filesystem
    /// inside it) or `lvm` (a logical volume). Empty for a partition, and
    /// always on Windows.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mapper_kind: String,
    /// Linux: an opened LUKS mapping. Its filesystem reads as it is, but a
    /// device-mapper snapshot of it while it is in use needs the container's
    /// password (after `cryptsetup open` the key lives only in the kernel),
    /// so the wizard asks for one as for an encrypted volume. Always false on
    /// Windows.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub container_password: bool,
}

/// GPT "hidden" attribute (bit 62).
pub const GPT_ATTR_HIDDEN: u64 = 1 << 62;
/// GPT "no default drive letter" attribute (bit 63). Not a sign of a hidden
/// partition on its own: the owner's data volume D: carries it and has its
/// letter all the same (2026-10-15).
pub const GPT_ATTR_NO_DRIVE_LETTER: u64 = 1 << 63;
/// GPT "required by the platform" attribute (bit 0).
pub const GPT_ATTR_REQUIRED: u64 = 1;
/// Basic Data partition GPT *type* GUID — the ordinary Windows data volume.
pub const GPT_TYPE_BASIC_DATA: &str = "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7";

/// MBR types Windows letters on its own: FAT12/16, FAT32 (CHS and LBA),
/// NTFS/exFAT. Everything else that still carries a filesystem Windows reads
/// is a hidden or vendor partition (0x27 WinRE, 0x12 OEM tools, 0x17 hidden
/// NTFS…).
const MBR_VISIBLE_TYPES: [u8; 7] = [0x01, 0x04, 0x06, 0x07, 0x0B, 0x0C, 0x0E];

impl PartitionInfo {
    /// A partition the system keeps out of sight, which Disk Management will
    /// not give a letter: the EFI system partition, the recovery environment,
    /// a vendor recovery or tools partition. Only one holding a filesystem
    /// Windows itself mounts — a letter on anything else gets the user asked
    /// to format it.
    ///
    /// Never the volume Windows runs from or one holding an installed system:
    /// their letters are what everything else is built on.
    pub fn is_hidden_system(&self, table: crate::types::PartitionTable) -> bool {
        if !self.filesystem.is_windows_family()
            || self.is_encrypted
            || self.is_system
            || self.has_os
        {
            return false;
        }
        match table {
            crate::types::PartitionTable::Gpt => {
                let basic = self.gpt_type_guid.eq_ignore_ascii_case(GPT_TYPE_BASIC_DATA);
                !basic || self.gpt_attributes & (GPT_ATTR_HIDDEN | GPT_ATTR_REQUIRED) != 0
            }
            crate::types::PartitionTable::Mbr => {
                self.mbr_type != 0 && !MBR_VISIBLE_TYPES.contains(&self.mbr_type)
            }
        }
    }
}

/// One hidden system volume, as the «Инструменты» page lists it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HiddenVolume {
    /// The volume as it can be opened now: `S:` with a letter, else
    /// `\\?\Volume{GUID}` — the key the letter operations take.
    pub device: String,
    pub letter: Option<char>,
    /// `esp` | `winre` | `other` — named in the interface's language.
    pub kind: String,
    pub disk_number: u32,
    pub disk_model: String,
    pub part_index: u32,
    pub size_bytes: u64,
    pub filesystem: FileSystem,
    pub label: String,
    /// Linux: where the «Инструменты» page mounted it; `None` while it is not
    /// mounted. Always `None` on Windows, where `letter` says the same.
    #[serde(default)]
    pub mount_point: Option<String>,
    /// Linux: mounted read-only because a read-write mount was refused (an
    /// NTFS volume of a hibernated Windows, a dirty file system).
    #[serde(default)]
    pub read_only: bool,
    /// Linux: the kernel name (`/dev/nvme0n1p1`) — what people recognise;
    /// `device` is the stable `/dev/disk/by-…` name the calls take.
    #[serde(default)]
    pub kernel_device: String,
}

/// A partition the Linux «Инструменты» page mounted, as its record keeps it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OwnMount {
    pub device: String,
    pub mount_point: String,
    #[serde(default)]
    pub read_only: bool,
}

/// Linux: the hidden system partitions nothing has mounted — EFI, Windows
/// recovery, a vendor's recovery or tools partition — and the ones the
/// «Инструменты» page itself mounted (`ours`, still mounted where it put
/// them). The same partitions Windows lists for a letter
/// ([`PartitionInfo::is_hidden_system`]), judged by the partition type alone:
/// the Linux inventory reads no GPT attributes, and an ordinary data volume
/// (Basic Data, a visible MBR type) is the file manager's to mount, not
/// ours (owner, 19.11.2026). A volume mounted by anything else (the system,
/// fstab, a file manager) is not ours to unmount and is left out, as are
/// loop devices and encrypted containers.
pub fn unmounted_volumes(disks: &[DiskInfo], ours: &[OwnMount]) -> Vec<HiddenVolume> {
    let mut out = Vec::new();
    for disk in disks {
        if disk.device_id.starts_with("/dev/loop") {
            continue;
        }
        for p in &disk.partitions {
            let device = p.volume_device.trim();
            let mountable = matches!(
                p.filesystem,
                FileSystem::Ntfs | FileSystem::Fat32 | FileSystem::ExFat
            );
            if device.is_empty() || !mountable || p.is_encrypted || p.is_system {
                continue;
            }
            // lsblk gives an MBR partition's type as `0x27` in the same field.
            let ty = p.gpt_type_guid.trim().to_ascii_uppercase();
            let hidden = match ty.strip_prefix("0X") {
                Some(hex) => u8::from_str_radix(hex, 16)
                    .is_ok_and(|t| t != 0 && !MBR_VISIBLE_TYPES.contains(&t)),
                None => !ty.is_empty() && ty != GPT_TYPE_BASIC_DATA,
            };
            if !hidden {
                continue;
            }
            let mounted_at = p.linux_last_mounted.trim();
            let own = ours
                .iter()
                .find(|m| m.device == device && m.mount_point == mounted_at);
            if !mounted_at.is_empty() && own.is_none() {
                continue;
            }
            let kind = if ty == crate::gpt::GPT_TYPE_EFI_SYSTEM || ty == "0XEF" {
                "esp"
            } else if ty == crate::gpt::GPT_TYPE_WINRE || ty == "0X27" {
                "winre"
            } else {
                "other"
            };
            out.push(HiddenVolume {
                device: device.to_string(),
                letter: None,
                kind: kind.into(),
                disk_number: 0,
                disk_model: if disk.model.trim().is_empty() {
                    disk.device_id.clone()
                } else {
                    disk.model.clone()
                },
                part_index: p.index,
                size_bytes: p.size_bytes,
                filesystem: p.filesystem,
                label: p.volume_label.clone(),
                mount_point: own.map(|m| m.mount_point.clone()),
                read_only: own.is_some_and(|m| m.read_only),
                kernel_device: if p.kernel_device.is_empty() {
                    device.to_string()
                } else {
                    p.kernel_device.clone()
                },
            });
        }
    }
    out
}

/// Every hidden system volume on these disks. Disks of mounted backup images
/// are left out: their volumes are kept letterless on purpose, and Windows
/// remembers a letter given to them for every later attach of the chain.
pub fn hidden_volumes(disks: &[DiskInfo]) -> Vec<HiddenVolume> {
    let mut out = Vec::new();
    for disk in disks {
        if disk
            .model
            .trim_start()
            .to_ascii_uppercase()
            .starts_with("OWNBACK")
        {
            continue;
        }
        let Some(number) = disk
            .device_id
            .rsplit("PhysicalDrive")
            .next()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        for p in &disk.partitions {
            // A volume Windows opens by name: a letter or a volume GUID path.
            let device = p.volume_device.trim();
            let named = device.len() == 2 && device.ends_with(':')
                || device.to_ascii_lowercase().starts_with(r"\\?\volume{");
            if !named || !p.is_hidden_system(disk.partition_table) {
                continue;
            }
            let kind = if p
                .gpt_type_guid
                .eq_ignore_ascii_case(crate::gpt::GPT_TYPE_EFI_SYSTEM)
                || p.mbr_type == 0xEF
            {
                "esp"
            } else if p
                .gpt_type_guid
                .eq_ignore_ascii_case(crate::gpt::GPT_TYPE_WINRE)
                || p.mbr_type == 0x27
            {
                "winre"
            } else {
                "other"
            };
            out.push(HiddenVolume {
                device: device.to_string(),
                letter: p.volume_letter,
                kind: kind.into(),
                disk_number: number,
                disk_model: disk.model.clone(),
                part_index: p.index,
                size_bytes: p.size_bytes,
                filesystem: p.filesystem,
                label: p.volume_label.clone(),
                mount_point: None,
                read_only: false,
                kernel_device: String::new(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpt::{GPT_TYPE_EFI_SYSTEM, GPT_TYPE_MSR, GPT_TYPE_WINRE};
    use crate::types::PartitionTable;

    fn part(index: u32, ty: &str, fs: FileSystem, dev: &str) -> PartitionInfo {
        PartitionInfo {
            index,
            offset_bytes: index as u64 * 1_048_576,
            size_bytes: 1_048_576,
            used_bytes: 0,
            gpt_type_guid: ty.into(),
            gpt_partition_guid: String::new(),
            filesystem: fs,
            volume_letter: dev
                .strip_suffix(':')
                .filter(|l| l.len() == 1)
                .and_then(|l| l.chars().next()),
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
            kernel_device: String::new(),
            mapper_kind: String::new(),
            container_password: false,
        }
    }

    fn disk(model: &str, table: PartitionTable, parts: Vec<PartitionInfo>) -> DiskInfo {
        DiskInfo {
            device_id: r"\\.\PhysicalDrive4".into(),
            stable_device: String::new(),
            model: model.into(),
            serial: String::new(),
            size_bytes: 1 << 40,
            sector_size_logical: 512,
            sector_size_physical: 4096,
            media_type: String::new(),
            partition_table: table,
            disk_guid: String::new(),
            partitions: parts,
        }
    }

    const ESP: &str = r"\\?\Volume{1d3128fc-790c-4147-b0e3-0767e8b3a4c0}";
    const RE: &str = r"\\?\Volume{f53b6290-da11-4b4f-8333-5721106d3a92}";
    const OEM: &str = r"\\?\Volume{aaaaaaaa-0000-0000-0000-000000000000}";

    #[test]
    fn the_system_partitions_of_a_windows_disk_are_listed_and_its_volumes_are_not() {
        let mut oem = part(6, GPT_TYPE_BASIC_DATA, FileSystem::Ntfs, OEM);
        oem.gpt_attributes = GPT_ATTR_HIDDEN | GPT_ATTR_NO_DRIVE_LETTER;
        // The owner's D: — "no default drive letter" set, a letter anyway, a
        // data volume all the same.
        let mut data = part(5, GPT_TYPE_BASIC_DATA, FileSystem::Ntfs, "D:");
        data.gpt_attributes = GPT_ATTR_NO_DRIVE_LETTER;
        let mut windows = part(3, GPT_TYPE_BASIC_DATA, FileSystem::Ntfs, "C:");
        windows.is_system = true;
        windows.gpt_attributes = GPT_ATTR_REQUIRED;
        let mut lettered_re = part(7, GPT_TYPE_WINRE, FileSystem::Ntfs, "R:");
        lettered_re.volume_label = "Recovery".into();
        let d = disk(
            "Samsung SSD 970 PRO 1TB",
            PartitionTable::Gpt,
            vec![
                part(1, GPT_TYPE_EFI_SYSTEM, FileSystem::Fat32, ESP),
                part(2, GPT_TYPE_MSR, FileSystem::Unknown, ""),
                windows,
                part(4, GPT_TYPE_WINRE, FileSystem::Ntfs, RE),
                data,
                oem,
                lettered_re,
            ],
        );
        let found = hidden_volumes(&[d]);
        let got: Vec<(&str, &str, Option<char>)> = found
            .iter()
            .map(|v| (v.device.as_str(), v.kind.as_str(), v.letter))
            .collect();
        assert_eq!(
            got,
            vec![
                (ESP, "esp", None),
                (RE, "winre", None),
                (OEM, "other", None),
                ("R:", "winre", Some('R')),
            ]
        );
        assert_eq!(found[0].disk_number, 4);
    }

    #[test]
    fn foreign_filesystems_and_image_disks_are_never_offered_a_letter() {
        // A Linux partition: Windows sees RAW and would offer to format it.
        let linux = part(
            3,
            "0FC63DAF-8483-4772-8E79-3D69D8477DE4",
            FileSystem::Ext4,
            OEM,
        );
        let mut locked = part(4, GPT_TYPE_WINRE, FileSystem::Ntfs, RE);
        locked.is_encrypted = true;
        let d = disk("WDC", PartitionTable::Gpt, vec![linux, locked]);
        assert!(hidden_volumes(&[d]).is_empty());

        let image = disk(
            "OWNBACK Backup Image",
            PartitionTable::Gpt,
            vec![part(1, GPT_TYPE_EFI_SYSTEM, FileSystem::Fat32, ESP)],
        );
        assert!(hidden_volumes(&[image]).is_empty());
    }

    #[test]
    fn mbr_hidden_types_count_and_ordinary_ones_do_not() {
        let mut winre = part(2, "", FileSystem::Ntfs, RE);
        winre.mbr_type = 0x27;
        let mut data = part(1, "", FileSystem::Ntfs, "C:");
        data.mbr_type = 0x07;
        let d = disk("HDD", PartitionTable::Mbr, vec![data, winre]);
        let found = hidden_volumes(&[d]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, "winre");
    }

    #[test]
    fn linux_lists_unmounted_partitions_and_only_its_own_mounts() {
        let at = |index: u32, ty: &str, fs: FileSystem, dev: &str, mounted: &str| {
            let mut p = part(index, ty, fs, dev);
            p.volume_letter = None;
            p.kernel_device = format!("/dev/sda{index}");
            p.linux_last_mounted = mounted.into();
            p
        };
        let mut luks = at(6, "", FileSystem::Unknown, "/dev/disk/by-partuuid/6", "");
        luks.is_encrypted = true;
        let mut d = disk(
            "WDC",
            PartitionTable::Gpt,
            vec![
                at(
                    1,
                    GPT_TYPE_EFI_SYSTEM,
                    FileSystem::Fat32,
                    "/dev/disk/by-partuuid/1",
                    "",
                ),
                at(
                    2,
                    GPT_TYPE_BASIC_DATA,
                    FileSystem::Ext4,
                    "/dev/disk/by-partuuid/2",
                    "/",
                ),
                at(
                    3,
                    GPT_TYPE_WINRE,
                    FileSystem::Ntfs,
                    "/dev/disk/by-partuuid/3",
                    "/mnt/Recovery",
                ),
                // A vendor's tools partition, but mounted by someone else.
                at(
                    4,
                    "BFBFAFE7-A34F-448A-9A5B-6213EB736C22",
                    FileSystem::Ntfs,
                    "/dev/disk/by-partuuid/4",
                    "/mnt/data",
                ),
                // An ordinary data volume (the owner's HDD-1Tb) and a Linux
                // one: the file manager's, not hidden.
                at(
                    7,
                    GPT_TYPE_BASIC_DATA,
                    FileSystem::Ntfs,
                    "/dev/disk/by-partuuid/7",
                    "",
                ),
                at(
                    8,
                    "0FC63DAF-8483-4772-8E79-3D69D8477DE4",
                    FileSystem::Ext4,
                    "/dev/disk/by-partuuid/8",
                    "",
                ),
                at(
                    5,
                    GPT_TYPE_BASIC_DATA,
                    FileSystem::Unknown,
                    "/dev/disk/by-partuuid/5",
                    "",
                ),
                luks,
            ],
        );
        d.device_id = "/dev/sda".into();
        let mut lp = disk(
            "",
            PartitionTable::Gpt,
            vec![at(1, "", FileSystem::Ext4, "/dev/loop0", "")],
        );
        lp.device_id = "/dev/loop0".into();
        let ours = vec![
            OwnMount {
                device: "/dev/disk/by-partuuid/3".into(),
                mount_point: "/mnt/Recovery".into(),
                read_only: true,
            },
            // Recorded, but /mnt/data is someone else's mount of partition 4.
            OwnMount {
                device: "/dev/disk/by-partuuid/4".into(),
                mount_point: "/mnt/elsewhere".into(),
                read_only: false,
            },
        ];
        let found = unmounted_volumes(&[d, lp], &ours);
        let got: Vec<(&str, &str, Option<&str>, bool)> = found
            .iter()
            .map(|v| {
                (
                    v.device.as_str(),
                    v.kind.as_str(),
                    v.mount_point.as_deref(),
                    v.read_only,
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("/dev/disk/by-partuuid/1", "esp", None, false),
                (
                    "/dev/disk/by-partuuid/3",
                    "winre",
                    Some("/mnt/Recovery"),
                    true
                ),
            ]
        );
        assert_eq!(found[0].kernel_device, "/dev/sda1");
    }

    #[test]
    fn linux_mbr_types_come_as_hex_and_only_hidden_ones_count() {
        let at = |index: u32, ty: &str, fs: FileSystem| {
            let mut p = part(index, ty, fs, &format!("/dev/disk/by-uuid/{index}"));
            p.volume_letter = None;
            p
        };
        let mut d = disk(
            "HDD",
            PartitionTable::Mbr,
            vec![
                at(1, "0x7", FileSystem::Ntfs),
                at(2, "0x27", FileSystem::Ntfs),
                at(3, "0x12", FileSystem::Fat32),
                at(4, "0xc", FileSystem::Fat32),
                at(5, "0xef", FileSystem::Fat32),
            ],
        );
        d.device_id = "/dev/sdb".into();
        let found = unmounted_volumes(&[d], &[]);
        let got: Vec<(u32, &str)> = found
            .iter()
            .map(|v| (v.part_index, v.kind.as_str()))
            .collect();
        assert_eq!(got, vec![(2, "winre"), (3, "other"), (5, "esp")]);
    }
}
