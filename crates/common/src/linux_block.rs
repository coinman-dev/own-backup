//! What is using a Linux block device, and what it is stacked on.
//!
//! Linux has no VSS and no volume lock: whether a device can be read as a
//! consistent whole, or written over, is decided by what sits on it — a mounted
//! filesystem, swap, or a device-mapper/md device holding it (an opened LUKS
//! container, an LVM physical volume, a RAID member). The kernel publishes all
//! of it: `/proc/self/mountinfo`, `/proc/swaps`, and the `holders`/`slaves`
//! links under `/sys/class/block`.
//!
//! The parsing and the walk over sysfs are pure (the sysfs root is a
//! parameter) and tested on any platform; [`device_usage`] reads the live
//! system.

use std::collections::BTreeSet;
use std::path::Path;

/// Where sysfs publishes the block devices.
pub const SYS_CLASS_BLOCK: &str = "/sys/class/block";

/// One line of `/proc/self/mountinfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    /// `st_dev` of the filesystem. A real device for ext4/XFS/FAT/NTFS; an
    /// anonymous `0:N` for btrfs, tmpfs and other virtual filesystems.
    pub major: u32,
    pub minor: u32,
    pub mount_point: String,
    pub read_only: bool,
    pub fstype: String,
    /// The mount source as given (`/dev/nvme0n1p2`, `/dev/mapper/luks-…`).
    pub source: String,
}

/// Parse `/proc/self/mountinfo`. Malformed lines are skipped.
///
/// Fields: `id parent maj:min root mount-point options [optional…] - fstype
/// source super-options`; the optional fields end at a lone `-`.
pub fn parse_mountinfo(text: &str) -> Vec<MountEntry> {
    text.lines().filter_map(parse_mountinfo_line).collect()
}

fn parse_mountinfo_line(line: &str) -> Option<MountEntry> {
    let fields: Vec<&str> = line.split(' ').collect();
    let dash = fields.iter().position(|f| *f == "-")?;
    if dash < 6 || fields.len() < dash + 3 {
        return None;
    }
    let (major, minor) = fields[2].split_once(':')?;
    let read_only = fields[5].split(',').any(|o| o == "ro");
    Some(MountEntry {
        major: major.parse().ok()?,
        minor: minor.parse().ok()?,
        mount_point: unescape(fields[4]),
        read_only,
        fstype: fields[dash + 1].to_string(),
        source: unescape(fields[dash + 2]),
    })
}

/// Parse `/proc/swaps`: the active swap areas (devices or files).
pub fn parse_swaps(text: &str) -> Vec<String> {
    text.lines()
        .skip(1)
        .filter_map(|l| l.split_whitespace().next())
        .map(unescape)
        .collect()
}

/// Undo the kernel's octal escapes (`\040` for a space, `\011`, `\012`, `\134`).
pub fn unescape(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            let oct = &bytes[i + 1..i + 4];
            if oct.iter().all(|b| (b'0'..=b'7').contains(b)) {
                let v = oct
                    .iter()
                    .fold(0u32, |acc, b| acc * 8 + u32::from(b - b'0'));
                if let Ok(v) = u8::try_from(v) {
                    out.push(v);
                    i += 4;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `major:minor` of a kernel block device name, from `<sys>/<name>/dev`.
pub fn dev_numbers(sys: &Path, name: &str) -> Option<(u32, u32)> {
    let text = std::fs::read_to_string(sys.join(name).join("dev")).ok()?;
    let (major, minor) = text.trim().split_once(':')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// Is `name` a partition (sysfs publishes a `partition` number for it)?
pub fn is_partition(sys: &Path, name: &str) -> bool {
    sys.join(name).join("partition").exists()
}

/// The whole disk a partition belongs to: the sysfs directory of a partition
/// sits inside its disk's directory.
pub fn parent_disk(sys: &Path, name: &str) -> Option<String> {
    if !is_partition(sys, name) {
        return None;
    }
    let real = std::fs::canonicalize(sys.join(name)).ok()?;
    real.parent()?
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
}

/// GPT/MBR partition UUID of a partition (`PARTUUID=` in its sysfs `uevent`),
/// lowercase as the kernel prints it.
pub fn partuuid(sys: &Path, name: &str) -> Option<String> {
    let text = std::fs::read_to_string(sys.join(name).join("uevent")).ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix("PARTUUID="))
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty())
}

/// Partition type GUID (or MBR type) from the udev database
/// (`E:ID_PART_ENTRY_TYPE=` in `<udev_data>/b<major>:<minor>`), lowercase.
/// `None` when udev has no record — which is not the same as "no type".
pub fn partition_type(udev_data: &Path, major: u32, minor: u32) -> Option<String> {
    let text = std::fs::read_to_string(udev_data.join(format!("b{major}:{minor}"))).ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix("E:ID_PART_ENTRY_TYPE="))
        .map(|v| v.trim().to_ascii_lowercase())
}

/// Where udev keeps its device database.
pub const UDEV_DATA: &str = "/run/udev/data";

/// Partition type GUID of an EFI System Partition.
pub const ESP_TYPE_GUID: &str = "c12a7328-f81f-11d2-ba4b-00a0c93ec93b";

/// Partitions of a whole disk (empty for a partition or a device without any).
pub fn partitions_of(sys: &Path, disk: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(sys.join(disk)) else {
        return Vec::new();
    };
    let mut parts: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().join("partition").exists())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    parts.sort();
    parts
}

fn links(sys: &Path, name: &str, dir: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(sys.join(name).join(dir)) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// `name` and everything stacked ON it: its partitions (for a whole disk) and,
/// recursively, the device-mapper/md devices holding any of them.
pub fn stack_above(sys: &Path, name: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut todo = vec![name.to_string()];
    todo.extend(partitions_of(sys, name));
    while let Some(n) = todo.pop() {
        if !seen.insert(n.clone()) {
            continue;
        }
        todo.extend(links(sys, &n, "holders"));
    }
    seen.into_iter().collect()
}

/// `name` and everything it is stacked UNDER: the devices a device-mapper/md
/// device is built from, recursively (an LV → its PV partitions; an opened
/// LUKS container → the encrypted partition).
pub fn stack_below(sys: &Path, name: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut todo = vec![name.to_string()];
    while let Some(n) = todo.pop() {
        if !seen.insert(n.clone()) {
            continue;
        }
        todo.extend(links(sys, &n, "slaves"));
    }
    seen.into_iter().collect()
}

/// What is using a device and the devices stacked on it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage {
    /// Mount points of filesystems mounted read-write on the stack.
    pub mounted_rw: Vec<String>,
    /// Mount points of filesystems mounted read-only on the stack.
    pub mounted_ro: Vec<String>,
    /// Active swap areas on the stack.
    pub swap: Vec<String>,
    /// Devices stacked on it (opened LUKS, LVM, md) — kernel names.
    pub holders: Vec<String>,
}

impl Usage {
    /// Nothing at all sits on the device: reading it gives a consistent image
    /// and nobody else writes to it through the kernel.
    pub fn idle(&self) -> bool {
        self.mounted_rw.is_empty()
            && self.mounted_ro.is_empty()
            && self.swap.is_empty()
            && self.holders.is_empty()
    }

    /// Something may be writing to it: a read-write mount, swap, or a device
    /// stacked on it (whose own users we cannot see through).
    pub fn in_use(&self) -> bool {
        !self.mounted_rw.is_empty() || !self.swap.is_empty() || !self.holders.is_empty()
    }
}

/// Match the live mounts and swap areas against a device stack.
///
/// `stack` is the kernel names of the device and everything above it, each
/// with its `major:minor`; `root_name` is the device itself (it is not its own
/// holder). A mount matches by device number, or — for btrfs and other
/// filesystems that report an anonymous device — by resolving its source path
/// with `resolve` to a kernel name. Swap entries are resolved the same way; a
/// swap FILE resolves to nothing and is covered by its filesystem's mount.
pub fn usage_of(
    root_name: &str,
    stack: &[(String, Option<(u32, u32)>)],
    mounts: &[MountEntry],
    swaps: &[String],
    resolve: impl Fn(&str) -> Option<String>,
) -> Usage {
    let names: BTreeSet<&str> = stack.iter().map(|(n, _)| n.as_str()).collect();
    let numbers: BTreeSet<(u32, u32)> = stack.iter().filter_map(|(_, d)| *d).collect();
    let mut usage = Usage::default();
    for m in mounts {
        let by_number = m.major != 0 && numbers.contains(&(m.major, m.minor));
        let by_source = m.source.starts_with('/')
            && resolve(&m.source).is_some_and(|n| names.contains(n.as_str()));
        if by_number || by_source {
            if m.read_only {
                usage.mounted_ro.push(m.mount_point.clone());
            } else {
                usage.mounted_rw.push(m.mount_point.clone());
            }
        }
    }
    for s in swaps {
        if resolve(s).is_some_and(|n| names.contains(n.as_str())) {
            usage.swap.push(s.clone());
        }
    }
    usage.holders = stack
        .iter()
        .map(|(n, _)| n.clone())
        .filter(|n| n != root_name && !is_partition_name_of(n, root_name))
        .collect();
    usage
}

/// Partitions of the device itself are not holders: `nvme0n1p2` of `nvme0n1`,
/// `sda1` of `sda`. Their own holders and mounts still count.
fn is_partition_name_of(name: &str, disk: &str) -> bool {
    name.strip_prefix(disk).is_some_and(|rest| {
        let rest = rest.strip_prefix('p').unwrap_or(rest);
        !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
    })
}

/// Kernel name (`sda2`, `dm-0`, `nvme0n1`) of a block device path, following
/// symlinks such as `/dev/disk/by-partuuid/…` and `/dev/mapper/…`. `None` for
/// anything that is not a block device.
#[cfg(unix)]
pub fn kernel_name(path: &Path) -> Option<String> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let meta = std::fs::metadata(path).ok()?;
    if !meta.file_type().is_block_device() {
        return None;
    }
    let (major, minor) = split_dev(meta.rdev());
    let link = std::fs::read_link(format!("/sys/dev/block/{major}:{minor}")).ok()?;
    link.file_name().map(|n| n.to_string_lossy().into_owned())
}

/// `major`/`minor` of a Linux `dev_t` (the glibc encoding).
pub fn split_dev(dev: u64) -> (u32, u32) {
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & 0xffff_f000);
    let minor = (dev & 0xff) | ((dev >> 12) & 0xffff_ff00);
    (major as u32, minor as u32)
}

/// What is using the block device at `path` right now. `Ok(None)` when `path`
/// is not a block device (an image file, for example).
#[cfg(unix)]
pub fn device_usage(path: &Path) -> std::io::Result<Option<Usage>> {
    let Some(name) = kernel_name(path) else {
        return Ok(None);
    };
    let sys = Path::new(SYS_CLASS_BLOCK);
    let stack: Vec<(String, Option<(u32, u32)>)> = stack_above(sys, &name)
        .into_iter()
        .map(|n| {
            let d = dev_numbers(sys, &n);
            (n, d)
        })
        .collect();
    let mounts = parse_mountinfo(&std::fs::read_to_string("/proc/self/mountinfo")?);
    let swaps = parse_swaps(&std::fs::read_to_string("/proc/swaps").unwrap_or_default());
    Ok(Some(usage_of(&name, &stack, &mounts, &swaps, |p| {
        kernel_name(Path::new(p))
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNTINFO: &str = "\
26 1 0:24 /@ / rw,noatime shared:1 - btrfs /dev/mapper/luks-16e1 rw,ssd,subvol=/@
27 26 259:2 / /boot rw,relatime shared:2 - ext4 /dev/nvme0n1p2 rw
28 27 259:1 / /boot/efi rw,relatime shared:3 - vfat /dev/nvme0n1p1 rw,fmask=0077
40 26 0:24 /@home /home rw,noatime shared:4 - btrfs /dev/mapper/luks-16e1 rw,subvol=/@home
51 26 252:1 / /run/media/user/Work\\040Disk rw,nosuid shared:9 - ntfs3 /dev/mapper/work rw
52 26 8:17 / /mnt/ro ro,relatime shared:10 - ext4 /dev/sdb1 ro
60 26 0:55 / /tmp rw,nosuid shared:11 - tmpfs tmpfs rw
";

    #[test]
    fn mountinfo_fields_and_escapes() {
        let m = parse_mountinfo(MOUNTINFO);
        assert_eq!(m.len(), 7);
        assert_eq!(m[0].mount_point, "/");
        assert_eq!((m[0].major, m[0].minor), (0, 24));
        assert_eq!(m[0].fstype, "btrfs");
        assert_eq!(m[0].source, "/dev/mapper/luks-16e1");
        assert!(!m[0].read_only);
        assert_eq!(m[4].mount_point, "/run/media/user/Work Disk");
        assert!(m[5].read_only);
        // Optional fields may be absent.
        let bare = parse_mountinfo("30 1 8:1 / /data rw - xfs /dev/sda1 rw\n");
        assert_eq!(bare[0].fstype, "xfs");
        assert!(parse_mountinfo("garbage line\n").is_empty());
    }

    #[test]
    fn swaps_list() {
        let s = parse_swaps(
            "Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n\
             /swap/swapfile                          file\t\t8388604\t\t0\t\t-2\n\
             /dev/dm-1                               partition\t2097148\t\t0\t\t-3\n",
        );
        assert_eq!(s, vec!["/swap/swapfile", "/dev/dm-1"]);
    }

    #[test]
    fn unescape_octal() {
        assert_eq!(unescape(r"/a\040b\011c"), "/a b\tc");
        assert_eq!(unescape(r"/plain"), "/plain");
        assert_eq!(unescape(r"/trailing\04"), r"/trailing\04");
    }

    #[test]
    fn dev_t_split() {
        // makedev(259, 2) and a large minor.
        assert_eq!(split_dev((259 << 8) | 2), (259, 2));
        let dev = ((8u64 & 0xfff) << 8) | (300 & 0xff) | ((300u64 & !0xff) << 12);
        assert_eq!(split_dev(dev), (8, 300));
    }

    #[test]
    fn partition_names() {
        assert!(is_partition_name_of("sda1", "sda"));
        assert!(is_partition_name_of("nvme0n1p12", "nvme0n1"));
        assert!(!is_partition_name_of("sdab", "sda"));
        assert!(!is_partition_name_of("dm-0", "sda"));
    }

    fn resolver(p: &str) -> Option<String> {
        match p {
            "/dev/mapper/luks-16e1" => Some("dm-0".into()),
            "/dev/mapper/work" => Some("dm-1".into()),
            "/dev/nvme0n1p2" => Some("nvme0n1p2".into()),
            "/dev/dm-1" => Some("dm-1".into()),
            _ => None,
        }
    }

    #[test]
    fn luks_root_partition_is_in_use_through_its_holder() {
        // nvme0n1p3 (LUKS) → dm-0 (btrfs mounted at / and /home by source).
        let stack = vec![
            ("dm-0".to_string(), Some((252, 0))),
            ("nvme0n1p3".to_string(), Some((259, 3))),
        ];
        let mounts = parse_mountinfo(MOUNTINFO);
        let u = usage_of("nvme0n1p3", &stack, &mounts, &[], resolver);
        assert_eq!(u.mounted_rw, vec!["/", "/home"]);
        assert_eq!(u.holders, vec!["dm-0"]);
        assert!(u.in_use());
        assert!(!u.idle());
    }

    #[test]
    fn whole_disk_collects_partition_mounts_and_swap() {
        let stack = vec![
            ("dm-1".to_string(), Some((252, 1))),
            ("nvme0n1".to_string(), Some((259, 0))),
            ("nvme0n1p1".to_string(), Some((259, 1))),
            ("nvme0n1p2".to_string(), Some((259, 2))),
        ];
        let mounts = parse_mountinfo(MOUNTINFO);
        let swaps = vec!["/dev/dm-1".to_string(), "/swap/swapfile".to_string()];
        let u = usage_of("nvme0n1", &stack, &mounts, &swaps, resolver);
        assert_eq!(
            u.mounted_rw,
            vec!["/boot", "/boot/efi", "/run/media/user/Work Disk"]
        );
        assert_eq!(u.swap, vec!["/dev/dm-1"]);
        // Its own partitions are not holders; the dm device is.
        assert_eq!(u.holders, vec!["dm-1"]);
    }

    #[test]
    fn read_only_mount_is_not_in_use() {
        let stack = vec![("sdb1".to_string(), Some((8, 17)))];
        let u = usage_of("sdb1", &stack, &parse_mountinfo(MOUNTINFO), &[], resolver);
        assert_eq!(u.mounted_ro, vec!["/mnt/ro"]);
        assert!(!u.in_use());
        assert!(!u.idle());
    }

    #[test]
    fn unused_partition_is_idle() {
        let stack = vec![("sdc1".to_string(), Some((8, 33)))];
        let u = usage_of("sdc1", &stack, &parse_mountinfo(MOUNTINFO), &[], resolver);
        assert!(u.idle());
        assert!(!u.in_use());
    }

    #[cfg(unix)]
    #[test]
    fn sysfs_walks() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let devices = dir.path().join("devices");
        let sys = dir.path().join("class_block");
        std::fs::create_dir_all(&sys).unwrap();
        // nvme0n1 with partitions p1 and p3; p3 is held by dm-0 (LUKS);
        // dm-0 is held by dm-1 (an LV on the LUKS PV).
        let disk = devices.join("nvme0n1");
        for p in ["nvme0n1p1", "nvme0n1p3"] {
            std::fs::create_dir_all(disk.join(p)).unwrap();
            std::fs::write(disk.join(p).join("partition"), "1\n").unwrap();
            symlink(disk.join(p), sys.join(p)).unwrap();
        }
        std::fs::write(disk.join("dev"), "259:0\n").unwrap();
        symlink(&disk, sys.join("nvme0n1")).unwrap();
        for (dm, dev) in [("dm-0", "252:0"), ("dm-1", "252:1")] {
            std::fs::create_dir_all(devices.join(dm).join("holders")).unwrap();
            std::fs::create_dir_all(devices.join(dm).join("slaves")).unwrap();
            std::fs::write(devices.join(dm).join("dev"), format!("{dev}\n")).unwrap();
            symlink(devices.join(dm), sys.join(dm)).unwrap();
        }
        std::fs::create_dir_all(disk.join("nvme0n1p3").join("holders")).unwrap();
        symlink(devices.join("dm-0"), disk.join("nvme0n1p3/holders/dm-0")).unwrap();
        symlink(devices.join("dm-1"), devices.join("dm-0/holders/dm-1")).unwrap();
        symlink(
            disk.join("nvme0n1p3"),
            devices.join("dm-0/slaves/nvme0n1p3"),
        )
        .unwrap();
        symlink(devices.join("dm-0"), devices.join("dm-1/slaves/dm-0")).unwrap();

        assert_eq!(
            partitions_of(&sys, "nvme0n1"),
            vec!["nvme0n1p1", "nvme0n1p3"]
        );
        assert!(is_partition(&sys, "nvme0n1p3"));
        assert_eq!(parent_disk(&sys, "nvme0n1p3").as_deref(), Some("nvme0n1"));
        assert_eq!(parent_disk(&sys, "dm-0"), None);
        assert_eq!(
            stack_above(&sys, "nvme0n1"),
            vec!["dm-0", "dm-1", "nvme0n1", "nvme0n1p1", "nvme0n1p3"]
        );
        assert_eq!(stack_above(&sys, "nvme0n1p1"), vec!["nvme0n1p1"]);
        assert_eq!(stack_below(&sys, "dm-1"), vec!["dm-0", "dm-1", "nvme0n1p3"]);
        assert_eq!(dev_numbers(&sys, "dm-1"), Some((252, 1)));
        std::fs::write(
            disk.join("nvme0n1p1").join("uevent"),
            "MAJOR=259\nMINOR=2\nDEVTYPE=partition\nPARTN=1\nPARTUUID=245EF510-a193-458d-b01f-d13738760455\n",
        )
        .unwrap();
        assert_eq!(
            partuuid(&sys, "nvme0n1p1").as_deref(),
            Some("245ef510-a193-458d-b01f-d13738760455")
        );
        assert_eq!(partuuid(&sys, "nvme0n1p3"), None);
        let udev = dir.path().join("udev");
        std::fs::create_dir_all(&udev).unwrap();
        std::fs::write(
            udev.join("b259:2"),
            "S:disk/by-partuuid/245ef510\nE:ID_PART_ENTRY_TYPE=C12A7328-F81F-11D2-BA4B-00A0C93EC93B\n",
        )
        .unwrap();
        assert_eq!(
            partition_type(&udev, 259, 2).as_deref(),
            Some(ESP_TYPE_GUID)
        );
        assert_eq!(partition_type(&udev, 259, 9), None);
    }
}
