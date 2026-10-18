//! The questions of [`super`] asked of a Linux machine: which disk ranges a
//! source device or a destination folder occupies, and who a source volume
//! is.
//!
//! Everything comes from `/proc/self/mountinfo`, sysfs and the udev symlinks
//! under `/dev` — no device is opened. A device is followed through its
//! aliases (`/dev/disk/by-partuuid/…`, `/dev/mapper/…`) to its kernel name, and
//! from there down through whatever it is stacked on (LUKS, LVM, md RAID, a
//! loop device's backing file) to whole disks.
//!
//! The two roots are injectable ([`Host`]) so all of it runs against a fake
//! tree in tests.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::{Component, Path, PathBuf};

use own_backup_common::tr;

use super::{DestLocation, Extent};

/// How deep stacked devices are followed (a loop file on LVM on LUKS on RAID
/// is four). Past this the answer is "cannot tell", never a guess.
const MAX_DEPTH: u32 = 12;

/// [`Extent::disk`] of a Linux disk: its device number, `major << 20 | minor`.
///
/// Windows numbers disks 0, 1, 2…; Linux has no number like that, but the
/// device number is unique per disk for as long as the machine runs, and an
/// overlap comparison never keeps it longer than that. The kernel's `dev_t`
/// has 12 bits of major and 20 of minor, so nothing is lost.
pub fn disk_key(major: u32, minor: u32) -> u32 {
    (major << 20) | (minor & 0xF_FFFF)
}

/// One line of `/proc/self/mountinfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    pub mount_point: PathBuf,
    pub fstype: String,
    /// What was mounted: `/dev/nvme0n1p2`, `/dev/mapper/…`, `server:/export`,
    /// `tmpfs`…
    pub source: String,
    /// The filesystem's device number. A real device for a block filesystem;
    /// an anonymous `0:N` for btrfs, network and virtual filesystems — which is
    /// why the source is asked first and this only when that fails.
    pub major: u32,
    pub minor: u32,
}

/// Parse `/proc/self/mountinfo`: `id parent major:minor root mount-point
/// options [optional…] - fstype source super-options`. Lines that do not have
/// that shape are skipped.
pub fn parse_mountinfo(text: &str) -> Vec<MountEntry> {
    text.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            // Everything before " - " is at least six fields; the separator is
            // the first lone "-" after them (escaped fields never hold a space).
            let sep = fields.iter().skip(6).position(|f| *f == "-")? + 6;
            let (major, minor) = fields[2].split_once(':')?;
            Some(MountEntry {
                mount_point: PathBuf::from(unescape(fields[4])),
                fstype: fields.get(sep + 1)?.to_string(),
                source: unescape(fields.get(sep + 2)?)
                    .to_string_lossy()
                    .into_owned(),
                major: major.parse().ok()?,
                minor: minor.parse().ok()?,
            })
        })
        .collect()
}

/// Undo mountinfo's octal escapes (`\040` space, `\011` tab, `\012` newline,
/// `\134` backslash). Bytes, not chars: a mount point need not be UTF-8.
fn unescape(field: &str) -> OsString {
    let b = field.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 4 <= b.len() {
            let oct = &b[i + 1..i + 4];
            if oct.iter().all(|c| (b'0'..=b'7').contains(c)) {
                let v = oct
                    .iter()
                    .fold(0u32, |acc, c| acc * 8 + u32::from(c - b'0'));
                if v <= 0xFF {
                    out.push(v as u8);
                    i += 4;
                    continue;
                }
            }
        }
        out.push(b[i]);
        i += 1;
    }
    OsString::from_vec(out)
}

/// `a/./b/../c` → `a/c`, by the letters alone: no symlink is followed.
fn lexically_normal(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The mount `path` is under: the longest mount point that is a whole-component
/// prefix of it. Of two at the same place the later one is on top.
pub fn mount_for<'a>(mounts: &'a [MountEntry], path: &Path) -> Option<&'a MountEntry> {
    let mut best: Option<&MountEntry> = None;
    for m in mounts {
        if !path.starts_with(&m.mount_point) {
            continue;
        }
        let longer =
            best.is_none_or(|b| m.mount_point.as_os_str().len() >= b.mount_point.as_os_str().len());
        if longer {
            best = Some(m);
        }
    }
    best
}

/// Filesystems that are another machine's disk. A folder on one of these is
/// the Linux side of `\\server\share`.
fn is_network_fs(fstype: &str) -> bool {
    matches!(
        fstype,
        "nfs"
            | "nfs4"
            | "cifs"
            | "smb3"
            | "smbfs"
            | "ncpfs"
            | "9p"
            | "virtiofs"
            | "ceph"
            | "glusterfs"
            | "lustre"
            | "afs"
            | "beegfs"
            | "gpfs"
            | "orangefs"
            | "davfs"
            | "fuse.sshfs"
            | "fuse.glusterfs"
            | "fuse.ceph-fuse"
            | "fuse.rclone"
            | "fuse.s3fs"
            | "fuse.gcsfuse"
            | "fuse.davfs2"
            | "fuse.gvfsd-fuse"
    )
}

/// Filesystems that are no drive at all: memory, a view of the kernel, a mount
/// trigger. `tmpfs` is also what is left under `/run/media` once a removable
/// drive is unplugged.
fn is_placeholder_fs(fstype: &str) -> bool {
    matches!(
        fstype,
        "tmpfs"
            | "ramfs"
            | "devtmpfs"
            | "rootfs"
            | "autofs"
            | "overlay"
            | "squashfs"
            | "proc"
            | "sysfs"
            | "cgroup"
            | "cgroup2"
            | "devpts"
            | "mqueue"
            | "debugfs"
            | "tracefs"
            | "securityfs"
            | "pstore"
            | "bpf"
            | "configfs"
            | "fusectl"
            | "hugetlbfs"
            | "efivarfs"
            | "binfmt_misc"
            | "nsfs"
            | "fuse.portal"
    )
}

/// The mount point of the drive a destination folder is on, when it is on a
/// drive of its own: not `/` (the system's own disk does not come and go) and
/// not a placeholder (`tmpfs` under `/run/media` is what an unplugged drive
/// leaves). `None` otherwise — read by the scheduler as "not connected".
///
/// By the path's letters alone, with no call into the filesystem: it runs on
/// the scheduler's own thread, where a dead network mount must not be touched.
/// A destination reached through a symlink is judged by the path as written.
pub fn destination_mount(dest: &str, mounts: &[MountEntry]) -> Option<PathBuf> {
    let path = Path::new(dest.trim());
    if !path.is_absolute() {
        return None;
    }
    let m = mount_for(mounts, &lexically_normal(path))?;
    if m.mount_point == Path::new("/") || is_placeholder_fs(&m.fstype) {
        return None;
    }
    Some(m.mount_point.clone())
}

/// The machine being asked: where `/` and `/sys` are, and what is mounted.
pub struct Host {
    /// `/` itself, or the top of a fake tree. Device paths, destination
    /// folders and loop backing files are looked up under it.
    root: PathBuf,
    /// `/sys`, or its fake.
    sys: PathBuf,
    mounts: Vec<MountEntry>,
}

impl Host {
    /// This machine. A missing `/proc/self/mountinfo` leaves no mounts, and
    /// every destination then "cannot be told".
    pub fn real() -> Self {
        let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
        Host {
            root: PathBuf::from("/"),
            sys: PathBuf::from("/sys"),
            mounts: parse_mountinfo(&mountinfo),
        }
    }

    /// A made-up machine: `root` holds `dev/…`, `sys` holds `class/block/…`.
    #[cfg(test)]
    fn fake(root: &Path, sys: &Path, mountinfo: &str) -> Self {
        Host {
            root: std::fs::canonicalize(root).unwrap(),
            sys: sys.to_path_buf(),
            mounts: parse_mountinfo(mountinfo),
        }
    }

    /// Where an absolute path of this machine is in our filesystem.
    fn on_host(&self, path: &Path) -> PathBuf {
        match path.strip_prefix("/") {
            Ok(rest) if self.root != Path::new("/") => self.root.join(rest),
            _ => path.to_path_buf(),
        }
    }

    /// The reverse: a path we found back as this machine names it.
    fn as_host(&self, ours: &Path) -> Option<PathBuf> {
        if self.root == Path::new("/") {
            return Some(ours.to_path_buf());
        }
        Some(Path::new("/").join(ours.strip_prefix(&self.root).ok()?))
    }

    fn class_dir(&self, name: &str) -> PathBuf {
        self.sys.join("class/block").join(name)
    }

    /// Kernel name (`nvme0n1p2`, `dm-0`) of the block device `device` leads
    /// to, through any symlinks. Only a node in `/dev` counts: a file that
    /// happens to be called `sda` somewhere else is not the disk.
    fn block_name(&self, device: &str) -> Option<String> {
        let path = Path::new(device.trim());
        if !path.is_absolute() {
            return None;
        }
        let real = std::fs::canonicalize(self.on_host(path)).ok()?;
        let dev = std::fs::canonicalize(self.on_host(Path::new("/dev"))).ok()?;
        if real.parent() != Some(dev.as_path()) {
            return None;
        }
        let name = real.file_name()?.to_str()?.to_string();
        self.class_dir(&name).is_dir().then_some(name)
    }

    /// Kernel name of the block device with this number.
    fn block_by_number(&self, major: u32, minor: u32) -> Option<String> {
        let link = self.sys.join("dev/block").join(format!("{major}:{minor}"));
        let real = std::fs::canonicalize(link).ok()?;
        let name = real.file_name()?.to_str()?.to_string();
        self.class_dir(&name).is_dir().then_some(name)
    }

    /// What a stacked device (dm, md) sits on, by kernel name.
    fn slaves(&self, name: &str) -> Vec<String> {
        let mut out: Vec<String> = std::fs::read_dir(self.class_dir(name).join("slaves"))
            .map(|rd| {
                rd.filter_map(|e| e.ok()?.file_name().into_string().ok())
                    .collect()
            })
            .unwrap_or_default();
        out.sort();
        out
    }

    /// The file a loop device reads, if it is one with a file attached.
    fn loop_backing_file(&self, name: &str) -> Option<PathBuf> {
        let text = std::fs::read_to_string(self.class_dir(name).join("loop/backing_file")).ok()?;
        let text = text.trim_end_matches('\n');
        let text = text.strip_suffix(" (deleted)").unwrap_or(text);
        (!text.is_empty()).then(|| PathBuf::from(text))
    }

    /// The ranges of whole disks a block device occupies.
    ///
    /// * a partition — its range on its disk (`start`/`size` in sysfs are in
    ///   512-byte units whatever the sector size);
    /// * a whole disk — all of it;
    /// * a device-mapper or md device (LUKS, LVM, RAID) — everything under it.
    ///   For an LVM volume that is the whole physical volume, not the ranges
    ///   the logical volume really uses: the device-mapper table is not in
    ///   sysfs. Too much, never too little — two volumes of one physical
    ///   volume are reported as overlapping;
    /// * a loop device — the filesystem its backing file is on, likewise whole;
    /// * an NBD device — `None`: it is served by a process (our own image
    ///   mount), and what that process reads is not visible here.
    fn extents_of_block(&self, name: &str, depth: u32) -> Option<Vec<Extent>> {
        if depth > MAX_DEPTH {
            return None;
        }
        let dir = self.class_dir(name);
        if !dir.is_dir() || name.starts_with("nbd") {
            return None;
        }
        let slaves = self.slaves(name);
        if !slaves.is_empty() {
            let mut out = Vec::new();
            for s in &slaves {
                out.extend(self.extents_of_block(s, depth + 1)?);
            }
            return Some(out);
        }
        if let Some(file) = self.loop_backing_file(name) {
            return match self.locate_path(&file, depth + 1) {
                DestLocation::Local(e) => Some(e),
                DestLocation::Network | DestLocation::Unknown(_) => None,
            };
        }
        if dir.join("partition").is_file() {
            let start = read_u64(&dir.join("start"))?;
            let size = read_u64(&dir.join("size"))?;
            let disk_dir = std::fs::canonicalize(&dir).ok()?.parent()?.to_path_buf();
            let disk = disk_dir.file_name()?.to_str()?.to_string();
            // A partition of something that is itself somewhere else (a
            // partitioned loop file, an md array): all of that.
            if !self.slaves(&disk).is_empty() || self.loop_backing_file(&disk).is_some() {
                return self.extents_of_block(&disk, depth + 1);
            }
            let (major, minor) = read_dev(&disk_dir.join("dev"))?;
            return Some(vec![Extent {
                disk: disk_key(major, minor),
                offset: start.checked_mul(512)?,
                length: size.checked_mul(512)?,
            }]);
        }
        let size = read_u64(&dir.join("size"))?;
        let (major, minor) = read_dev(&dir.join("dev"))?;
        Some(vec![Extent {
            disk: disk_key(major, minor),
            offset: 0,
            length: size.checked_mul(512)?,
        }])
    }

    /// The sectors a source device occupies. `part:<disk>:<offset>:<len>` is
    /// a Windows disk number and means nothing here — `None`, so the answer
    /// is "cannot tell" rather than "clear" by comparing unrelated numbers.
    pub fn extents_of_source(&self, device: &str) -> Option<Vec<Extent>> {
        if device.trim_start().starts_with("part:") {
            return None;
        }
        let name = self.block_name(device)?;
        self.extents_of_block(&name, 0)
    }

    /// Where the destination folder is.
    pub fn locate_destination(&self, folder: &str) -> DestLocation {
        let path = Path::new(folder.trim());
        if !path.is_absolute() {
            return DestLocation::Unknown(
                tr!("source_identity.dest_not_absolute", folder = folder).into_owned(),
            );
        }
        self.locate_path(path, 0)
    }

    /// The filesystem the nearest existing ancestor of `path` is on — a
    /// folder that does not exist yet is created there — and what it is on.
    fn locate_path(&self, path: &Path, depth: u32) -> DestLocation {
        let shown = path.to_string_lossy();
        let unknown = || {
            DestLocation::Unknown(
                tr!("source_identity.dest_volume_unknown", folder = shown).into_owned(),
            )
        };
        if depth > MAX_DEPTH {
            return unknown();
        }
        let mut at = path.to_path_buf();
        let existing = loop {
            if let Ok(found) = std::fs::canonicalize(self.on_host(&at)) {
                break found;
            }
            if !at.pop() {
                return unknown();
            }
        };
        let Some(existing) = self.as_host(&existing) else {
            return unknown();
        };
        let Some(mount) = mount_for(&self.mounts, &existing) else {
            return unknown();
        };
        if is_network_fs(&mount.fstype) {
            return DestLocation::Network;
        }
        let devices = self.mount_devices(mount);
        if devices.is_empty() {
            return DestLocation::Unknown(
                tr!(
                    "source_identity.dest_fs_without_disk",
                    folder = shown,
                    fstype = mount.fstype,
                    source = mount.source
                )
                .into_owned(),
            );
        }
        let mut out = Vec::new();
        for name in &devices {
            match self.extents_of_block(name, depth + 1) {
                Some(e) => out.extend(e),
                None => return unknown(),
            }
        }
        DestLocation::Local(out)
    }

    /// The block devices a mounted filesystem is on: the device it was
    /// mounted from — by name, else by the device number when that is a real
    /// one — and, for btrfs, every other device of the same filesystem.
    fn mount_devices(&self, m: &MountEntry) -> Vec<String> {
        let first = self
            .block_name(&m.source)
            .or_else(|| (m.major != 0).then(|| self.block_by_number(m.major, m.minor))?);
        let Some(first) = first else {
            return Vec::new();
        };
        if m.fstype == "btrfs" {
            if let Some(all) = self.btrfs_devices(&first) {
                return all;
            }
        }
        vec![first]
    }

    /// Every device of the btrfs filesystem `member` belongs to: mountinfo
    /// names one, and a multi-device filesystem writes to all of them.
    fn btrfs_devices(&self, member: &str) -> Option<Vec<String>> {
        for fs in std::fs::read_dir(self.sys.join("fs/btrfs")).ok()?.flatten() {
            let Ok(devices) = std::fs::read_dir(fs.path().join("devices")) else {
                continue;
            };
            let mut names: Vec<String> = devices
                .filter_map(|e| e.ok()?.file_name().into_string().ok())
                .collect();
            if names.iter().any(|n| n == member) {
                names.sort();
                return Some(names);
            }
        }
        None
    }

    /// The udev alias in `dir` (`/dev/disk/by-partuuid`, …) that leads to the
    /// device `name`, if exactly one does. Two partitions with one PARTUUID (a
    /// cloned disk plugged in beside its original) have no alias that names
    /// either of them for sure.
    fn alias_in(&self, dir: &str, name: &str) -> Option<String> {
        let dev = std::fs::canonicalize(self.on_host(Path::new("/dev"))).ok()?;
        let target = dev.join(name);
        let mut found = None;
        for entry in std::fs::read_dir(self.on_host(Path::new(dir)))
            .ok()?
            .flatten()
        {
            if std::fs::canonicalize(entry.path()).ok().as_deref() == Some(target.as_path()) {
                if found.is_some() {
                    return None;
                }
                found = entry.file_name().into_string().ok();
            }
        }
        found
    }

    /// An identity for a source that survives a reboot, a renumbered device
    /// node and a renamed mapping:
    ///
    /// * `/dev/disk/by-partuuid/<u>` → `partuuid:<u>`, `/dev/disk/by-uuid/<u>`
    ///   → `fsuuid:<u>` — the name is the identity;
    /// * a device-mapper device → `dm:` + its device-mapper UUID (an LVM
    ///   volume's VG and LV UUIDs; a LUKS container's own UUID, without the
    ///   mapping name it was opened under). A mapping that has no UUID of its
    ///   own (BitLocker, plain dm-crypt) is named by the partition it opens;
    /// * a partition named by its kernel name (`/dev/sdb2`, a job saved before
    ///   the stable names) → the PARTUUID, else the filesystem UUID, udev
    ///   knows it by — so its old and its new name are one source.
    ///
    /// `None` when none of that can be found.
    pub fn stable_source_id(&self, device: &str) -> Option<String> {
        let d = device.trim();
        if let Some(id) = d.strip_prefix("/dev/disk/by-partuuid/") {
            return (!id.is_empty() && !id.contains('/'))
                .then(|| format!("partuuid:{}", id.to_ascii_lowercase()));
        }
        if let Some(id) = d.strip_prefix("/dev/disk/by-uuid/") {
            return (!id.is_empty() && !id.contains('/'))
                .then(|| format!("fsuuid:{}", id.to_ascii_lowercase()));
        }
        let name = self.block_name(d)?;
        self.identity_of_block(&name, 0)
    }

    fn identity_of_block(&self, name: &str, depth: u32) -> Option<String> {
        if depth > MAX_DEPTH {
            return None;
        }
        if self.class_dir(name).join("dm").is_dir() {
            return self.dm_identity(name, depth);
        }
        self.alias_in("/dev/disk/by-partuuid", name)
            .map(|id| format!("partuuid:{}", id.to_ascii_lowercase()))
            .or_else(|| {
                self.alias_in("/dev/disk/by-uuid", name)
                    .map(|id| format!("fsuuid:{}", id.to_ascii_lowercase()))
            })
    }

    fn dm_identity(&self, name: &str, depth: u32) -> Option<String> {
        let dm = self.class_dir(name).join("dm");
        let uuid = read_trimmed(&dm.join("uuid")).filter(|u| !u.is_empty())?;
        let dm_name = read_trimmed(&dm.join("name")).unwrap_or_default();
        if !uuid.starts_with("CRYPT-") {
            return Some(format!("dm:{uuid}"));
        }
        // `CRYPT-LUKS2-<container uuid>-<mapping name>`; for BitLocker and
        // plain dm-crypt `CRYPT-<TYPE>-<mapping name>`. The mapping name is
        // whatever it was opened under this time, so it goes.
        let core = if dm_name.is_empty() {
            uuid.as_str()
        } else {
            uuid.strip_suffix(&format!("-{dm_name}"))
                .unwrap_or(uuid.as_str())
        };
        let mut parts = core.splitn(3, '-');
        let (_crypt, kind, own) = (parts.next(), parts.next()?, parts.next());
        if own.is_some_and(|id| !id.is_empty()) {
            return Some(format!("dm:{core}"));
        }
        let slaves = self.slaves(name);
        let [inner] = slaves.as_slice() else {
            return None;
        };
        let inner = self.identity_of_block(inner, depth + 1)?;
        Some(format!("dm:CRYPT-{kind}:{inner}"))
    }
}

fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

fn read_u64(path: &Path) -> Option<u64> {
    read_trimmed(path)?.parse().ok()
}

/// `259:5` from a sysfs `dev` file.
fn read_dev(path: &Path) -> Option<(u32, u32)> {
    let text = read_trimmed(path)?;
    let (major, minor) = text.split_once(':')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::super::{placement, DestPlacement};
    use super::*;
    use std::os::unix::fs::symlink;

    /// The owner's machine, cut down: an NVMe disk with /boot, a LUKS
    /// container holding btrfs `/` and `/home`, a BitLocker partition opened
    /// with cryptsetup, and a removable NTFS backup disk under `/run/media`.
    const MOUNTINFO: &str = "\
30 34 0:7 / /dev rw,nosuid,relatime shared:2 - devtmpfs udev rw,size=14765460k
32 34 0:28 / /run rw,nosuid,nodev,noexec,relatime shared:5 - tmpfs tmpfs rw,size=3279048k,mode=755
34 2 0:29 /@ / rw,noatime shared:1 - btrfs /dev/mapper/luks-16e1f072-b3b5-4aa7-b296-67b738edf0ca rw,ssd,subvolid=256,subvol=/@
53 32 0:46 / /run/credentials/systemd-cryptsetup@luks\\134x2d16e1f072.service rw shared:84 - tmpfs none ro,size=1024k
57 34 0:29 /@home /home rw,noatime shared:93 - btrfs /dev/mapper/luks-16e1f072-b3b5-4aa7-b296-67b738edf0ca rw,ssd,subvolid=257,subvol=/@home
58 34 259:5 / /boot rw,relatime shared:96 - ext4 /dev/nvme0n1p2 rw
69 34 0:51 / /tmp rw,noatime shared:99 - tmpfs tmpfs rw,inode64
221 32 252:1 / /run/media/u/Work\\040Disk rw,nosuid,nodev,relatime shared:420 - ntfs3 /dev/mapper/WorkDisk rw,uid=1000
219 32 8:17 / /run/media/u/Backup rw,nosuid,nodev,relatime shared:434 - ntfs3 /dev/sdb1 rw,uid=1000
300 34 0:60 / /mnt/nas rw,relatime shared:500 - nfs4 nas:/export rw,vers=4.2
301 34 8:33 / /mnt/root-alias rw,relatime shared:501 - ext4 /dev/root rw
";

    struct Fake {
        _dir: tempfile::TempDir,
        root: PathBuf,
        sys: PathBuf,
    }

    impl Fake {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("root");
            let sys = dir.path().join("sys");
            for d in ["dev/disk/by-partuuid", "dev/disk/by-uuid", "dev/mapper"] {
                std::fs::create_dir_all(root.join(d)).unwrap();
            }
            for d in ["class/block", "dev/block"] {
                std::fs::create_dir_all(sys.join(d)).unwrap();
            }
            Fake {
                _dir: dir,
                root,
                sys,
            }
        }

        fn write(&self, path: &Path, text: &str) {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }

        fn dir(&self, host_path: &str) {
            std::fs::create_dir_all(self.root.join(host_path.trim_start_matches('/'))).unwrap();
        }

        /// A whole disk at `sys/devices/<bus>/<name>`.
        fn disk(&self, bus: &str, name: &str, dev: &str, sectors: u64) -> PathBuf {
            let at = self.sys.join("devices").join(bus).join(name);
            self.write(&at.join("dev"), &format!("{dev}\n"));
            self.write(&at.join("size"), &format!("{sectors}\n"));
            self.register(name, &at, dev);
            at
        }

        /// A partition of the disk at `disk`.
        fn part(&self, disk: &Path, name: &str, dev: &str, start: u64, sectors: u64) -> PathBuf {
            let at = disk.join(name);
            self.write(&at.join("dev"), &format!("{dev}\n"));
            self.write(&at.join("size"), &format!("{sectors}\n"));
            self.write(&at.join("start"), &format!("{start}\n"));
            self.write(&at.join("partition"), "1\n");
            self.register(name, &at, dev);
            at
        }

        /// A device-mapper device on top of `slaves`.
        fn dm(
            &self,
            name: &str,
            dev: &str,
            dm_name: &str,
            uuid: &str,
            slaves: &[&Path],
        ) -> PathBuf {
            let at = self.sys.join("devices/virtual/block").join(name);
            self.write(&at.join("dev"), &format!("{dev}\n"));
            self.write(&at.join("size"), "1000\n");
            self.write(&at.join("dm/name"), &format!("{dm_name}\n"));
            self.write(&at.join("dm/uuid"), &format!("{uuid}\n"));
            std::fs::create_dir_all(at.join("slaves")).unwrap();
            for s in slaves {
                symlink(s, at.join("slaves").join(s.file_name().unwrap())).unwrap();
            }
            self.register(name, &at, dev);
            symlink(
                format!("../{name}"),
                self.root.join("dev/mapper").join(dm_name),
            )
            .unwrap();
            at
        }

        /// The `/dev` node and the `/sys/class/block`, `/sys/dev/block` links.
        fn register(&self, name: &str, at: &Path, dev: &str) {
            std::fs::write(self.root.join("dev").join(name), b"").unwrap();
            symlink(at, self.sys.join("class/block").join(name)).unwrap();
            symlink(at, self.sys.join("dev/block").join(dev)).unwrap();
        }

        fn alias(&self, dir: &str, id: &str, name: &str) {
            symlink(
                format!("../../{name}"),
                self.root.join("dev/disk").join(dir).join(id),
            )
            .unwrap();
        }

        fn btrfs(&self, fsid: &str, members: &[&Path]) {
            let devices = self.sys.join("fs/btrfs").join(fsid).join("devices");
            std::fs::create_dir_all(&devices).unwrap();
            for m in members {
                symlink(m, devices.join(m.file_name().unwrap())).unwrap();
            }
        }

        fn host(&self, mountinfo: &str) -> Host {
            Host::fake(&self.root, &self.sys, mountinfo)
        }
    }

    const LUKS: &str = "/dev/mapper/luks-16e1f072-b3b5-4aa7-b296-67b738edf0ca";
    const BOOT: &str = "/dev/disk/by-partuuid/2d272048-0d61-4a8c-a715-08d35535ed93";
    const LUKS_PART: &str = "/dev/disk/by-partuuid/495bc881-f4b8-4a9a-8f6f-3b63a1d00227";
    const BITLOCKER_PART: &str = "/dev/disk/by-partuuid/95fa1035-0605-4d22-be37-a684b1b616d0";

    fn owner_machine() -> Fake {
        let f = Fake::new();
        let nvme = f.disk("pci/nvme", "nvme0n1", "259:0", 1_000_215_216);
        f.part(&nvme, "nvme0n1p1", "259:2", 2048, 2_684_928);
        f.part(&nvme, "nvme0n1p2", "259:5", 2_686_976, 2_097_152);
        let luks = f.part(&nvme, "nvme0n1p3", "259:6", 4_784_128, 268_435_456);
        let bitlocker = f.part(&nvme, "nvme0n1p4", "259:9", 273_219_584, 726_995_968);
        let root = f.dm(
            "dm-0",
            "252:0",
            "luks-16e1f072-b3b5-4aa7-b296-67b738edf0ca",
            "CRYPT-LUKS2-16e1f072b3b54aa7b29667b738edf0ca-luks-16e1f072-b3b5-4aa7-b296-67b738edf0ca",
            &[&luks],
        );
        f.dm(
            "dm-1",
            "252:1",
            "WorkDisk",
            "CRYPT-BITLK-WorkDisk",
            &[&bitlocker],
        );
        let usb = f.disk("usb/sdb", "sdb", "8:16", 3_907_029_168);
        f.part(&usb, "sdb1", "8:17", 2048, 3_907_024_896);
        let other = f.disk("ata/sdc", "sdc", "8:32", 1_000_000);
        f.part(&other, "sdc1", "8:33", 2048, 900_000);
        f.alias(
            "by-partuuid",
            "245ef510-a193-458d-b01f-d13738760455",
            "nvme0n1p1",
        );
        f.alias(
            "by-partuuid",
            "2d272048-0d61-4a8c-a715-08d35535ed93",
            "nvme0n1p2",
        );
        f.alias(
            "by-partuuid",
            "495bc881-f4b8-4a9a-8f6f-3b63a1d00227",
            "nvme0n1p3",
        );
        f.alias(
            "by-partuuid",
            "95fa1035-0605-4d22-be37-a684b1b616d0",
            "nvme0n1p4",
        );
        f.alias(
            "by-partuuid",
            "998a528a-2464-40a1-0ca7-b08a5ceee748",
            "sdb1",
        );
        f.alias("by-uuid", "060B-E961", "nvme0n1p1");
        f.alias(
            "by-uuid",
            "a2649700-5dbb-4f7b-833d-f57c7c812ef4",
            "nvme0n1p2",
        );
        f.btrfs("17d01ac2-2643-4e09-8773-2c59ecde4fc9", &[&root]);
        for d in [
            "/home/u/Backups",
            "/boot",
            "/run/media/u/Backup",
            "/run/media/u/Work Disk",
            "/mnt/nas/backups",
            "/mnt/root-alias",
            "/tmp/x",
            "/media/usb",
        ] {
            f.dir(d);
        }
        f
    }

    fn range(disk: (u32, u32), start: u64, sectors: u64) -> Extent {
        Extent {
            disk: disk_key(disk.0, disk.1),
            offset: start * 512,
            length: sectors * 512,
        }
    }

    #[test]
    fn mountinfo_is_read_field_by_field_with_its_escapes() {
        let mounts = parse_mountinfo(MOUNTINFO);
        assert_eq!(mounts.len(), 11);
        let root = &mounts[2];
        assert_eq!(root.mount_point, PathBuf::from("/"));
        assert_eq!(root.fstype, "btrfs");
        assert_eq!(root.source, LUKS);
        assert_eq!((root.major, root.minor), (0, 29));
        // `\040` is a space, `\134` a backslash.
        assert_eq!(
            mounts[7].mount_point,
            PathBuf::from("/run/media/u/Work Disk")
        );
        assert_eq!(
            mounts[3].mount_point,
            PathBuf::from(r"/run/credentials/systemd-cryptsetup@luks\x2d16e1f072.service")
        );
        // Optional fields — several of them or none — do not move the
        // separator; a line of another shape is skipped.
        let odd = parse_mountinfo(
            "1 2 8:1 / /a rw - ext4 /dev/sda1 rw\n\
             3 4 8:2 / /b rw shared:1 master:2 - xfs /dev/sda2 rw\n\
             garbage line\n",
        );
        assert_eq!(odd.len(), 2);
        assert_eq!(odd[0].source, "/dev/sda1");
        assert_eq!(odd[1].fstype, "xfs");
        assert_eq!(odd[1].source, "/dev/sda2");
    }

    #[test]
    fn the_mount_of_a_path_is_its_longest_whole_component_prefix() {
        let mounts = parse_mountinfo(MOUNTINFO);
        let at = |p: &str| mount_for(&mounts, Path::new(p)).map(|m| m.mount_point.clone());
        assert_eq!(at("/home/u/Backups"), Some(PathBuf::from("/home")));
        assert_eq!(at("/boot/efi"), Some(PathBuf::from("/boot")));
        assert_eq!(at("/bootstrap"), Some(PathBuf::from("/")), "not /boot");
        assert_eq!(
            at("/run/media/u/Backup/Jobs"),
            Some(PathBuf::from("/run/media/u/Backup"))
        );
        // Two mounts at one place (an automount trigger and what it mounted):
        // the later one is on top.
        let stacked = parse_mountinfo(
            "1 0 0:1 / / rw - ext4 /dev/sda1 rw\n\
             2 1 0:40 / /media/usb rw - autofs systemd-1 rw\n\
             3 2 8:17 / /media/usb rw - vfat /dev/sdb1 rw\n",
        );
        assert_eq!(
            mount_for(&stacked, Path::new("/media/usb/x"))
                .unwrap()
                .fstype,
            "vfat"
        );
    }

    #[test]
    fn a_source_is_followed_through_its_aliases_and_what_it_is_stacked_on() {
        let f = owner_machine();
        let host = f.host(MOUNTINFO);
        let boot = vec![range((259, 0), 2_686_976, 2_097_152)];
        let luks = vec![range((259, 0), 4_784_128, 268_435_456)];
        assert_eq!(host.extents_of_source(BOOT), Some(boot.clone()));
        assert_eq!(host.extents_of_source("/dev/nvme0n1p2"), Some(boot));
        // The opened LUKS volume is where its container is.
        assert_eq!(host.extents_of_source(LUKS), Some(luks.clone()));
        assert_eq!(host.extents_of_source(LUKS_PART), Some(luks));
        // A whole disk is all of it.
        assert_eq!(
            host.extents_of_source("/dev/sdb"),
            Some(vec![range((8, 16), 0, 3_907_029_168)])
        );
        // A Windows `part:` range names a Windows disk number: no answer here.
        assert_eq!(host.extents_of_source("part:0:1048576:4096"), None);
        // Nor is a device that does not exist, or a file named like one.
        assert_eq!(host.extents_of_source("/dev/sdz9"), None);
        f.dir("/home/u/sdb");
        assert_eq!(host.extents_of_source("/home/u/sdb"), None);
        assert_eq!(host.extents_of_source("sdb"), None);
    }

    #[test]
    fn a_destination_is_where_its_filesystem_is_mounted_from() {
        let f = owner_machine();
        let host = f.host(MOUNTINFO);
        let luks = vec![range((259, 0), 4_784_128, 268_435_456)];
        // A folder that does not exist yet is created on its nearest ancestor's
        // filesystem: /home, btrfs on the opened LUKS container.
        assert_eq!(
            host.locate_destination("/home/u/Backups/not-yet/there"),
            DestLocation::Local(luks)
        );
        assert_eq!(
            host.locate_destination("/run/media/u/Backup/Jobs"),
            DestLocation::Local(vec![range((8, 16), 2048, 3_907_024_896)])
        );
        // A source name that says nothing (`/dev/root`) is found by the
        // device number.
        assert_eq!(
            host.locate_destination("/mnt/root-alias/x"),
            DestLocation::Local(vec![range((8, 32), 2048, 900_000)])
        );
        assert_eq!(
            host.locate_destination("/mnt/nas/backups"),
            DestLocation::Network
        );
        // Memory is not a disk, and a relative path is nowhere yet.
        for dest in ["/tmp/x", "Backups", r"\\nas\share"] {
            assert!(
                matches!(host.locate_destination(dest), DestLocation::Unknown(_)),
                "{dest}"
            );
        }
        // Nothing mounted at all: "cannot tell", not "clear".
        let bare = f.host("");
        assert!(matches!(
            bare.locate_destination("/home/u"),
            DestLocation::Unknown(_)
        ));
    }

    /// The question RunJob asks, end to end, on the owner's layout.
    #[test]
    fn a_copy_of_the_root_volume_may_not_go_to_home_but_may_go_to_the_usb_disk() {
        let f = owner_machine();
        let host = f.host(MOUNTINFO);
        let ask = |sources: &[&str], dest: &str| {
            let sources: Vec<String> = sources.iter().map(|s| s.to_string()).collect();
            placement(&sources, host.locate_destination(dest), |s| {
                host.extents_of_source(s)
            })
        };
        assert_eq!(
            ask(&[BOOT, LUKS], "/home/u/Backups"),
            DestPlacement::OnSource(LUKS.into())
        );
        // The raw container is the same sectors as the volume opened from it.
        assert_eq!(
            ask(&[LUKS_PART], "/home/u/Backups"),
            DestPlacement::OnSource(LUKS_PART.into())
        );
        // Another partition of the same disk is a perfectly good place.
        assert_eq!(ask(&[BOOT], "/home/u/Backups"), DestPlacement::Clear);
        assert_eq!(
            ask(&[BOOT, LUKS], "/run/media/u/Backup/Jobs"),
            DestPlacement::Clear
        );
        assert_eq!(ask(&[BOOT, LUKS], "/mnt/nas/backups"), DestPlacement::Clear);
        // The BitLocker volume opened by cryptsetup is its partition.
        assert_eq!(
            ask(&[BITLOCKER_PART], "/run/media/u/Work Disk/x"),
            DestPlacement::OnSource(BITLOCKER_PART.into())
        );
        assert!(matches!(
            ask(&[BOOT], "/tmp/x"),
            DestPlacement::CannotTell(_)
        ));
        assert!(matches!(
            ask(&["part:0:1048576:4096"], "/run/media/u/Backup"),
            DestPlacement::CannotTell(_)
        ));
    }

    #[test]
    fn every_device_of_a_multi_device_btrfs_counts() {
        let f = Fake::new();
        let a = f.disk("ata/sdc", "sdc", "8:32", 1000);
        let c1 = f.part(&a, "sdc1", "8:33", 10, 900);
        let b = f.disk("ata/sdd", "sdd", "8:48", 1000);
        let d1 = f.part(&b, "sdd1", "8:49", 10, 900);
        f.btrfs("fs-1", &[&c1, &d1]);
        f.dir("/pool/data");
        let host = f.host("1 0 0:30 / /pool rw - btrfs /dev/sdc1 rw\n");
        assert_eq!(
            host.locate_destination("/pool/data"),
            DestLocation::Local(vec![range((8, 32), 10, 900), range((8, 48), 10, 900)])
        );
        // So a copy of the member mountinfo does not name is refused too.
        let sources = vec!["/dev/sdd1".to_string()];
        assert_eq!(
            placement(&sources, host.locate_destination("/pool/data"), |s| host
                .extents_of_source(s)),
            DestPlacement::OnSource("/dev/sdd1".into())
        );
    }

    #[test]
    fn a_logical_volume_is_everything_its_physical_volumes_are() {
        let f = Fake::new();
        let a = f.disk("ata/sda", "sda", "8:0", 1000);
        let pv1 = f.part(&a, "sda2", "8:2", 100, 800);
        let b = f.disk("ata/sdb", "sdb", "8:16", 1000);
        let pv2 = f.part(&b, "sdb1", "8:17", 10, 900);
        f.dm("dm-3", "252:3", "vg0-data", "LVM-AAAABBBB", &[&pv1, &pv2]);
        f.dm("dm-4", "252:4", "vg0-root", "LVM-AAAACCCC", &[&pv1]);
        f.dir("/srv");
        let host = f.host("1 0 252:3 / /srv rw - xfs /dev/mapper/vg0-data rw\n");
        assert_eq!(
            host.extents_of_source("/dev/mapper/vg0-data"),
            Some(vec![range((8, 0), 100, 800), range((8, 16), 10, 900)])
        );
        // Conservative: two LVs of one PV are reported as sharing it.
        let sources = vec!["/dev/mapper/vg0-root".to_string()];
        assert_eq!(
            placement(&sources, host.locate_destination("/srv"), |s| host
                .extents_of_source(s)),
            DestPlacement::OnSource("/dev/mapper/vg0-root".into())
        );
    }

    #[test]
    fn a_loop_device_is_where_its_backing_file_is_and_nbd_cannot_be_told() {
        let f = owner_machine();
        let lp = f.disk("virtual/loop0", "loop0", "7:0", 2048);
        f.write(
            &lp.join("loop/backing_file"),
            "/home/u/disk.img (deleted)\n",
        );
        f.part(&lp, "loop0p1", "259:20", 34, 2000);
        f.disk("virtual/nbd0", "nbd0", "43:0", 2048);
        let host = f.host(MOUNTINFO);
        let luks = vec![range((259, 0), 4_784_128, 268_435_456)];
        assert_eq!(host.extents_of_source("/dev/loop0"), Some(luks.clone()));
        assert_eq!(host.extents_of_source("/dev/loop0p1"), Some(luks));
        assert_eq!(host.extents_of_source("/dev/nbd0"), None);
    }

    #[test]
    fn a_source_is_identified_by_what_survives_a_reboot() {
        let f = owner_machine();
        let host = f.host(MOUNTINFO);
        assert_eq!(
            host.stable_source_id(BOOT).as_deref(),
            Some("partuuid:2d272048-0d61-4a8c-a715-08d35535ed93")
        );
        // The same partition under its kernel name is the same source, so a
        // job saved with `/dev/nvme0n1p2` keeps its chain under the new name.
        assert_eq!(
            host.stable_source_id("/dev/nvme0n1p2"),
            host.stable_source_id(BOOT)
        );
        assert_eq!(
            host.stable_source_id("/dev/disk/by-uuid/060B-E961")
                .as_deref(),
            Some("fsuuid:060b-e961")
        );
        // The opened LUKS volume: its own UUID, not the mapping name.
        assert_eq!(
            host.stable_source_id(LUKS).as_deref(),
            Some("dm:CRYPT-LUKS2-16e1f072b3b54aa7b29667b738edf0ca")
        );
        // BitLocker through cryptsetup has no UUID of its own: the partition
        // it opens names it.
        assert_eq!(
            host.stable_source_id("/dev/mapper/WorkDisk").as_deref(),
            Some("dm:CRYPT-BITLK:partuuid:95fa1035-0605-4d22-be37-a684b1b616d0")
        );
        assert_eq!(host.stable_source_id("/dev/sdz1"), None);
        assert_eq!(host.stable_source_id("/dev/disk/by-partuuid/"), None);
    }

    #[test]
    fn an_lvm_volume_is_its_uuids_and_a_doubled_alias_names_nobody() {
        let f = Fake::new();
        let a = f.disk("ata/sda", "sda", "8:0", 1000);
        let pv = f.part(&a, "sda2", "8:2", 100, 800);
        f.part(&a, "sda3", "8:3", 900, 50);
        f.dm("dm-3", "252:3", "vg0-root", "LVM-Vg0uuidLv0uuid", &[&pv]);
        // sda3 answers to two PARTUUID aliases (a clone plugged in beside its
        // original): neither can be trusted to be it.
        f.alias("by-partuuid", "aaaa-1", "sda3");
        f.alias("by-partuuid", "aaaa-2", "sda3");
        let b = f.disk("ata/sdb", "sdb", "8:16", 1000);
        f.part(&b, "sdb3", "8:19", 900, 50);
        f.alias("by-uuid", "Same", "sdb3");
        let host = f.host("");
        assert_eq!(
            host.stable_source_id("/dev/mapper/vg0-root").as_deref(),
            Some("dm:LVM-Vg0uuidLv0uuid")
        );
        assert_eq!(host.stable_source_id("/dev/sda3"), None);
        assert_eq!(
            host.stable_source_id("/dev/sdb3").as_deref(),
            Some("fsuuid:same")
        );
    }

    // Runs against the real machine and prints what it finds:
    // `cargo test -p own-backup-backup --lib real_machine_smoke -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real_machine_smoke() {
        let host = Host::real();
        let mut devices: Vec<String> = Vec::new();
        for dir in ["/dev/disk/by-partuuid", "/dev/disk/by-uuid", "/dev/mapper"] {
            for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                devices.push(e.path().to_string_lossy().into_owned());
            }
        }
        devices.sort();
        for d in &devices {
            eprintln!(
                "SOURCE {d}\n  token={:?} id={:?}\n  extents={:?}",
                super::super::unix_device_token(d),
                host.stable_source_id(d),
                host.extents_of_source(d)
            );
        }
        for m in &host.mounts {
            let shown = m.mount_point.to_string_lossy();
            eprintln!(
                "DEST {shown} ({} {}): {:?} drive={:?}",
                m.fstype,
                m.source,
                host.locate_destination(&shown),
                destination_mount(&shown, &host.mounts)
            );
        }
    }

    #[test]
    fn the_scheduler_asks_about_the_drive_a_destination_is_on() {
        let mounts = parse_mountinfo(MOUNTINFO);
        let at = |d: &str| destination_mount(d, &mounts);
        // Plugged in: the drive's own mount point.
        assert_eq!(
            at("/run/media/u/Backup/Jobs/PC"),
            Some(PathBuf::from("/run/media/u/Backup"))
        );
        assert_eq!(at("/mnt/nas/backups"), Some(PathBuf::from("/mnt/nas")));
        assert_eq!(at("/home/u/Backups"), Some(PathBuf::from("/home")));
        // Unplugged: what is left is tmpfs (/run/media) or the system disk (a
        // static /media/usb) — "not connected" either way.
        assert_eq!(at("/run/media/u/Gone/Jobs"), None);
        assert_eq!(at("/media/usb/Jobs"), None);
        assert_eq!(at("/srv/backups"), None, "on / itself");
        // By the letters of the path, `..` included.
        assert_eq!(at("/run/media/u/Backup/../Gone"), None);
        assert_eq!(at("Backups"), None);
        assert_eq!(at(r"E:\Backup"), None);
        assert_eq!(at(""), None);
    }
}
