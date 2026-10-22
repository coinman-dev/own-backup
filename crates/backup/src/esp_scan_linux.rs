//! What loaders are on this machine's EFI System Partitions, and which of them
//! anything points at — the Linux half of [`crate::esp_scan_win`].
//!
//! The question is the same on both systems (see that module for why it is
//! asked at all); only the way to the partitions differs. Linux has no disk
//! numbers that last, so a disk is named by its kernel device (`/dev/nvme0n1`)
//! — what the inventory reports as the disk's `device_id`, which is how the
//! boot page puts a model name next to it.
//!
//! The partition table is read off the disk rather than taken from udev: the
//! LBAs a boot entry carries are in the disk's own sectors, which sysfs does
//! not give (it counts 512-byte units on every disk). The files are read
//! through the partition's existing mount when it has one (`/boot/efi`), and
//! otherwise through a short read-only mount of our own. A partition marked
//! EFI that holds no FAT — disk 3 of the owner's old Windows machine had ext4
//! there, put by an installer — is listed as unreadable, not skipped.

#![cfg(target_os = "linux")]

use std::path::Path;

use own_backup_common::linux_block;
use own_backup_common::{tr, Result};
use tracing::debug;

use crate::esp_scan::FoundLoader;

/// One `.efi` on a partition, with the entry naming it.
#[derive(Debug, Clone)]
pub struct Loader {
    pub found: FoundLoader,
    /// An entry in firmware names this file.
    pub named_by: Option<u16>,
}

/// One EFI System Partition.
#[derive(Debug, Clone)]
pub struct Esp {
    /// The whole disk, `/dev/<kernel name>`.
    pub disk: String,
    /// The partition's own node, when the kernel has one for it.
    pub device: Option<String>,
    /// Its slot in the GPT (1-based).
    pub partition: u32,
    /// Uppercase, as the Windows side and the boot entries spell it.
    pub partition_guid: String,
    pub size_bytes: u64,
    /// Why nothing could be listed, when that is the case.
    pub unreadable: Option<String>,
    pub loaders: Vec<Loader>,
}

/// Kernel block devices that are never a disk someone boots from, or that are
/// ours: an `nbd` is a backup image the program mounted, and a loader in it
/// must not be offered a boot entry.
fn not_a_boot_disk(name: &str) -> bool {
    ["loop", "ram", "zram", "nbd", "dm-", "md", "sr", "fd"]
        .iter()
        .any(|p| name.starts_with(p))
}

/// Every whole disk the kernel knows that carries partitions.
fn disks(sys: &Path) -> Vec<String> {
    let Ok(dir) = std::fs::read_dir(sys) else {
        return Vec::new();
    };
    let mut out: Vec<String> = dir
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !not_a_boot_disk(n) && !linux_block::is_partition(sys, n))
        .filter(|n| !linux_block::partitions_of(sys, n).is_empty())
        .collect();
    out.sort();
    out
}

/// The partition of `disk` whose GPT slot is `index`, by kernel name.
fn partition_node(sys: &Path, disk: &str, index: u32) -> Option<String> {
    linux_block::partitions_of(sys, disk).into_iter().find(|p| {
        std::fs::read_to_string(sys.join(p).join("partition"))
            .ok()
            .and_then(|t| t.trim().parse::<u32>().ok())
            == Some(index)
    })
}

/// The GPT on `/dev/<disk>`.
pub fn read_layout(disk_node: &str) -> Result<own_backup_common::gpt::DiskLayout> {
    let dev = crate::linux_device::LinuxRawDevice::open_read(disk_node)?;
    let sector = dev.sector_size();
    let want = (34 * sector as u64).min(dev.length()) as usize;
    let mut head = vec![0u8; want];
    dev.read_at(0, &mut head)?;
    crate::gpt_reader::parse_disk_layout(&head, sector, dev.length())
}

/// Where `major:minor` is mounted already, if anywhere.
fn mounted_at(mountinfo: &[linux_block::MountEntry], dev: (u32, u32)) -> Option<String> {
    mountinfo
        .iter()
        .find(|m| (m.major, m.minor) == dev)
        .map(|m| m.mount_point.clone())
}

/// Every ESP on every disk, with what is on it and what points at it.
///
/// `entries` is the firmware's side of the comparison; pass what
/// [`crate::efivars_linux::Efivarfs::read_config`] returned so the two lists
/// come from one moment.
pub fn scan(entries: &[crate::efivars_linux::BootEntry]) -> Result<Vec<Esp>> {
    let sys = Path::new("/sys/class/block");
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")
        .map(|t| linux_block::parse_mountinfo(&t))
        .unwrap_or_default();
    let mut out = Vec::new();
    for disk in disks(sys) {
        let disk_node = format!("/dev/{disk}");
        let layout = match read_layout(&disk_node) {
            Ok(l) => l,
            // A disk without a GPT is not a fault here: it simply has no ESP.
            Err(e) => {
                debug!(disk = %disk_node, error = %e, "esp scan: no GPT on this disk");
                continue;
            }
        };
        for part in layout
            .partitions
            .iter()
            .filter(|p| p.type_guid.eq_ignore_ascii_case(linux_block::ESP_TYPE_GUID))
        {
            let node = partition_node(sys, &disk, part.index);
            let mut esp = Esp {
                disk: disk_node.clone(),
                device: node.as_ref().map(|n| format!("/dev/{n}")),
                partition: part.index,
                partition_guid: part.part_guid.to_ascii_uppercase(),
                size_bytes: part.size_bytes(layout.sector_size),
                unreadable: None,
                loaders: Vec::new(),
            };
            match read_partition(sys, &mountinfo, node.as_deref()) {
                Ok(found) => {
                    esp.loaders = found
                        .into_iter()
                        .map(|f| Loader {
                            named_by: crate::esp_scan::naming_entry(
                                entries.iter().map(|e| (e.number, &e.option)),
                                &esp.partition_guid,
                                &f.path,
                            ),
                            found: f,
                        })
                        .collect();
                }
                Err(why) => esp.unreadable = Some(why),
            }
            out.push(esp);
        }
    }
    Ok(out)
}

/// The loaders on one partition: through its mount, or a read-only one of our
/// own for the length of the listing. The error is the text the page shows.
fn read_partition(
    sys: &Path,
    mountinfo: &[linux_block::MountEntry],
    node: Option<&str>,
) -> std::result::Result<Vec<FoundLoader>, String> {
    let node = node.ok_or_else(|| tr!("esp_scan_linux.no_partition_node").into_owned())?;
    if let Some(at) = linux_block::dev_numbers(sys, node).and_then(|d| mounted_at(mountinfo, d)) {
        return Ok(crate::esp_scan::collect_loaders(Path::new(&at)));
    }
    let dev = format!("/dev/{node}");
    // What is inside first: a partition typed EFI that holds something else
    // is said to be that, not described by mount's complaint.
    let mut boot = [0u8; 512];
    crate::linux_device::LinuxRawDevice::open_read(&dev)
        .and_then(|d| d.read_at(0, &mut boot))
        .map_err(|e| tr!("esp_scan_linux.unreadable", error = e).into_owned())?;
    if !looks_like_fat(&boot) {
        return Err(tr!("esp_scan_linux.not_fat").into_owned());
    }
    let mount = crate::linux_tools::TempMount::mount(&dev, &[("vfat", "ro")])
        .map_err(|e| tr!("esp_scan_linux.unreadable", error = e).into_owned())?;
    Ok(crate::esp_scan::collect_loaders(mount.path()))
}

/// A FAT boot sector: the 0x55AA signature and the file system type string
/// where FAT12/16 (offset 54) or FAT32 (offset 82) keep it.
fn looks_like_fat(boot: &[u8; 512]) -> bool {
    boot[510..512] == [0x55, 0xAA] && (&boot[54..57] == b"FAT" || &boot[82..85] == b"FAT")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_sys() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let mk = |p: &str, files: &[(&str, &str)]| {
            let d = root.path().join(p);
            std::fs::create_dir_all(&d).unwrap();
            for (f, v) in files {
                std::fs::write(d.join(f), v).unwrap();
            }
        };
        // nvme0n1 with two partitions, a loop device with one, an nbd with one
        // and a bare sda.
        mk("nvme0n1", &[("dev", "259:0")]);
        mk("nvme0n1/nvme0n1p1", &[("partition", "1"), ("dev", "259:1")]);
        mk("nvme0n1/nvme0n1p2", &[("partition", "2"), ("dev", "259:2")]);
        mk("loop0", &[("dev", "7:0")]);
        mk("loop0/loop0p1", &[("partition", "1")]);
        mk("nbd0", &[("dev", "43:0")]);
        mk("nbd0/nbd0p1", &[("partition", "1")]);
        mk("sda", &[("dev", "8:0")]);
        // sysfs lists partitions beside disks too; the real ones are symlinks
        // into the disk's directory.
        for (part, disk) in [
            ("nvme0n1p1", "nvme0n1"),
            ("nvme0n1p2", "nvme0n1"),
            ("loop0p1", "loop0"),
            ("nbd0p1", "nbd0"),
        ] {
            std::os::unix::fs::symlink(
                root.path().join(disk).join(part),
                root.path().join(part),
            )
            .unwrap();
        }
        root
    }

    #[test]
    fn only_real_disks_with_partitions_are_scanned() {
        let sys = fake_sys();
        assert_eq!(disks(sys.path()), vec!["nvme0n1".to_string()]);
    }

    #[test]
    fn a_gpt_slot_finds_its_kernel_partition() {
        let sys = fake_sys();
        assert_eq!(
            partition_node(sys.path(), "nvme0n1", 2).as_deref(),
            Some("nvme0n1p2")
        );
        assert_eq!(partition_node(sys.path(), "nvme0n1", 3), None);
    }

    #[test]
    fn an_existing_mount_is_found_by_device_numbers() {
        let info = linux_block::parse_mountinfo(
            "36 25 259:1 / /boot/efi rw,relatime shared:5 - vfat /dev/nvme0n1p1 rw\n",
        );
        assert_eq!(
            mounted_at(&info, (259, 1)).as_deref(),
            Some("/boot/efi")
        );
        assert_eq!(mounted_at(&info, (259, 2)), None);
    }

    #[test]
    fn a_mounted_partition_is_listed_through_its_mount() {
        let sys = fake_sys();
        let esp = tempfile::tempdir().unwrap();
        let ubuntu = esp.path().join("EFI/ubuntu");
        std::fs::create_dir_all(&ubuntu).unwrap();
        std::fs::write(ubuntu.join("shimx64.efi"), b"x").unwrap();
        std::fs::write(ubuntu.join("grubx64.efi"), b"x").unwrap();
        std::fs::write(ubuntu.join("grub.cfg"), b"x").unwrap();
        let line = format!(
            "36 25 259:1 / {} rw - vfat /dev/nvme0n1p1 rw\n",
            esp.path().display()
        );
        let info = linux_block::parse_mountinfo(&line);
        let found = read_partition(sys.path(), &info, Some("nvme0n1p1")).unwrap();
        let paths: Vec<&str> = found.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            [r"\EFI\ubuntu\grubx64.efi", r"\EFI\ubuntu\shimx64.efi"]
        );
        assert_eq!(found[1].kind, "Ubuntu · shim");
        assert_eq!(found[0].role, crate::esp_scan::Role::SecondStage);
    }

    #[test]
    fn a_fat_boot_sector_is_told_from_ext4() {
        let mut fat32 = [0u8; 512];
        fat32[82..90].copy_from_slice(b"FAT32   ");
        fat32[510] = 0x55;
        fat32[511] = 0xAA;
        assert!(looks_like_fat(&fat32));
        let mut fat16 = [0u8; 512];
        fat16[54..62].copy_from_slice(b"FAT16   ");
        fat16[510..512].copy_from_slice(&[0x55, 0xAA]);
        assert!(looks_like_fat(&fat16));
        // ext4 keeps nothing at either place; an MBR has the signature only.
        let mut mbr = [0u8; 512];
        mbr[510..512].copy_from_slice(&[0x55, 0xAA]);
        assert!(!looks_like_fat(&mbr));
        assert!(!looks_like_fat(&[0u8; 512]));
    }
}
