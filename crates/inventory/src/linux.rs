//! Disk inventory on Linux via `lsblk` (+ sysfs for partition offsets).
//!
//! The `/dev` analogue of [`crate::wmi`]. `lsblk -b -J -O` gives the disk/
//! partition tree with filesystem type, label, UUID and (for mounted volumes)
//! used bytes; the partition's start offset comes from
//! `/sys/class/block/<name>/start`, and for unmounted / unrecognized volumes we
//! read the superblock directly ([`crate::fs_probe::probe_partition_path`]) to
//! fill the label / used bytes and to flag encrypted (LUKS/BitLocker) volumes.
//!
//! A volume is addressed by a name that survives a reboot: kernel names are
//! handed out in the order devices appear, so `sda` and `sdb` can swap places
//! from one boot to the next and a job would quietly copy the other disk. A
//! partition is `/dev/disk/by-partuuid/<PARTUUID>`, else
//! `/dev/disk/by-uuid/<filesystem UUID>`; an opened LUKS/BitLocker container
//! or an LVM volume is `/dev/mapper/<name>`. The kernel name stays in
//! `kernel_device`, for people and for sysfs.

#![cfg(target_os = "linux")]

use std::collections::HashSet;
use std::process::Command;

use async_trait::async_trait;
use serde_json::Value;

use own_backup_common::disk::{DiskInfo, PartitionInfo};
use own_backup_common::types::{FileSystem, PartitionTable};
use own_backup_common::{OwnBackupError, Result};

/// `lsblk`-backed disk inventory for Linux.
pub struct LinuxInventory {
    _private: (),
}

impl LinuxInventory {
    pub fn new() -> Self {
        LinuxInventory { _private: () }
    }
}

impl Default for LinuxInventory {
    fn default() -> Self {
        Self::new()
    }
}

fn jstr(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn ju64(v: &Value, key: &str) -> Option<u64> {
    match v.get(key) {
        Some(Value::Number(n)) => n.as_u64(),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Every place a node is mounted. `mountpoint` alone names one of them: a
/// btrfs volume holding `/`, `/home` and `/swap` as subvolumes reports `/home`
/// there, and the system root went unnoticed.
fn mount_points(v: &Value) -> Vec<String> {
    let mut out: Vec<String> = v
        .get("mountpoints")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if out.is_empty() {
        out.extend(jstr(v, "mountpoint"));
    }
    out
}

/// The node's device as the kernel names it (`/dev/nvme0n1p2`, `/dev/dm-0`).
fn kernel_path(v: &Value) -> String {
    match jstr(v, "kname") {
        Some(k) => format!("/dev/{k}"),
        None => jstr(v, "path")
            .unwrap_or_else(|| format!("/dev/{}", jstr(v, "name").unwrap_or_default())),
    }
}

/// Map an `lsblk` FSTYPE to our enum plus whether it is an encrypted container.
fn parse_fs(fstype: Option<&str>) -> (FileSystem, bool) {
    match fstype.unwrap_or("").to_ascii_lowercase().as_str() {
        "ext2" | "ext3" | "ext4" => (FileSystem::Ext4, false),
        "btrfs" => (FileSystem::Btrfs, false),
        "xfs" => (FileSystem::Xfs, false),
        "vfat" | "fat" | "fat12" | "fat16" | "fat32" | "msdos" => (FileSystem::Fat32, false),
        "exfat" => (FileSystem::ExFat, false),
        "ntfs" | "ntfs3" => (FileSystem::Ntfs, false),
        "refs" => (FileSystem::ReFs, false),
        // Encrypted containers: filesystem is Unknown until decrypted.
        "crypto_luks" | "bitlocker" => (FileSystem::Unknown, true),
        _ => (FileSystem::Unknown, false),
    }
}

/// True if `token` is a BitLocker encryption date (`DD.MM.YYYY`, `M/D/YYYY`,
/// `YYYY-MM-DD`).
fn is_bitlocker_date(token: &str) -> bool {
    let Some(sep) = ['.', '/', '-'].into_iter().find(|&c| token.contains(c)) else {
        return false;
    };
    let parts: Vec<&str> = token.split(sep).collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| (1..=4).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_digit()))
        && parts.iter().any(|p| p.len() == 4)
}

/// Extract the volume label from a BitLocker FVE description string.
///
/// The real NTFS label inside a locked BitLocker volume is encrypted, so
/// `libblkid` reports the unencrypted FVE description datum as `LABEL`. Windows
/// formats that description as `<ComputerName> [Drive:] <VolumeLabel> <Date>`
/// (e.g. `MODULATOR WorkDisk-2 30.06.2025`); only `<VolumeLabel>` is the
/// volume's name.
fn clean_bitlocker_label(raw: &str) -> String {
    let s = raw.trim();
    let Some((_, rest)) = s.split_once(char::is_whitespace) else {
        return s.to_string();
    };
    let mut rest = rest.trim();
    if is_bitlocker_date(rest) {
        return String::new();
    }
    if let Some((before_date, last)) = rest.rsplit_once(char::is_whitespace) {
        if is_bitlocker_date(last.trim()) {
            rest = before_date.trim();
        }
    }
    let is_drive_letter =
        |t: &str| t.len() == 2 && t.as_bytes()[0].is_ascii_alphabetic() && t.as_bytes()[1] == b':';
    if is_drive_letter(rest) {
        return String::new();
    }
    if let Some((first, after_letter)) = rest.split_once(char::is_whitespace) {
        if is_drive_letter(first) {
            rest = after_letter.trim();
        }
    }
    rest.to_string()
}

/// Volume label of an `lsblk` partition node. For BitLocker containers, prefer
/// the inner label of an already-opened child mapping if present, otherwise
/// strip the machine name and encryption date from the FVE description.
fn partition_label(v: &Value, fstype: Option<&str>) -> String {
    let raw = jstr(v, "label").unwrap_or_default();
    if fstype.is_some_and(|f| f.eq_ignore_ascii_case("bitlocker")) {
        if let Some(inner) = v
            .get("children")
            .and_then(Value::as_array)
            .and_then(|kids| kids.iter().find_map(|c| jstr(c, "label")))
        {
            return inner;
        }
        return clean_bitlocker_label(&raw);
    }
    raw
}

const GPT_TYPE_EFI_SYSTEM: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";

/// Build a `PartitionInfo` from an `lsblk` node (a `part`, a whole-disk
/// filesystem, a device-mapper volume). `offset_bytes` is filled later from
/// sysfs, and `volume_device` is the kernel path until [`assign_stable_names`]
/// replaces it.
fn build_partition(v: &Value, index: u32) -> PartitionInfo {
    let path = kernel_path(v);
    let fstype = jstr(v, "fstype");
    let (filesystem, is_encrypted) = parse_fs(fstype.as_deref());
    let gpt_type_guid = jstr(v, "parttype").unwrap_or_default().to_uppercase();
    let mounts = mount_points(v);
    let mounted_at = |m: &str| mounts.iter().any(|x| x == m);
    let is_boot = gpt_type_guid.contains(GPT_TYPE_EFI_SYSTEM)
        || mounted_at("/boot")
        || mounted_at("/boot/efi");
    let is_system = mounted_at("/");
    let volume_label = partition_label(v, fstype.as_deref());

    PartitionInfo {
        index: ju64(v, "partn").map(|n| n as u32).unwrap_or(index),
        offset_bytes: 0, // from sysfs in enrich()
        size_bytes: ju64(v, "size").unwrap_or(0),
        used_bytes: ju64(v, "fsused").unwrap_or(0),
        gpt_type_guid,
        gpt_partition_guid: jstr(v, "partuuid").unwrap_or_default().to_uppercase(),
        filesystem,
        volume_letter: None,
        volume_device: path.clone(),
        volume_label,
        fs_uuid: jstr(v, "uuid").unwrap_or_default(),
        is_system,
        is_boot,
        // Linux support is deferred (Windows-first), and probing an unmounted
        // partition for an installed system means mounting it. `is_system`
        // already covers the running root, which is what this build uses.
        has_os: is_system,
        // `lsblk` reports the CURRENT mount points, not the filesystem's own
        // record of the last one; the field is filled by the raw probe on
        // Windows, which is where the wizard needs it. The root, when it is
        // one of them, is the one that says what the volume is.
        linux_last_mounted: if is_system {
            "/".to_string()
        } else {
            mounts.first().cloned().unwrap_or_default()
        },
        is_encrypted,
        // Linux support is deferred; the container's own metadata is not read
        // here, so nothing is claimed about how it opens — and "not asked" must
        // not read as "cannot be opened", or the password field never appears.
        unlock_methods: Vec::new(),
        recovery_key_only: false,
        unlockable: true,
        gpt_attributes: 0,
        mbr_type: 0,
        kernel_device: path,
        mapper_kind: String::new(),
        container_password: false,
    }
}

/// Is this top-level node a disk worth offering? Not our own NBD exports (a
/// mounted backup image), not compressed swap in memory (zram), not a snap
/// package's squashfs loop — none of them is something to back up.
fn is_listed_disk(v: &Value) -> bool {
    let name = jstr(v, "name").unwrap_or_default();
    match jstr(v, "type").as_deref() {
        Some("disk") => !name.starts_with("nbd") && !name.starts_with("zram"),
        Some("loop") => !jstr(v, "fstype").is_some_and(|f| f.eq_ignore_ascii_case("squashfs")),
        _ => false,
    }
}

fn media_type(v: &Value) -> String {
    if v.get("rota").and_then(Value::as_bool) == Some(true) {
        "HDD".into()
    } else {
        "SSD".into()
    }
}

/// Build a `DiskInfo` from a top-level `lsblk` node. Returns `None` for nodes
/// that are not whole disks (`part`, `rom`, `crypt`, `lvm`, …) or not listed
/// ([`is_listed_disk`]).
fn build_disk(v: &Value) -> Option<DiskInfo> {
    if !is_listed_disk(v) {
        return None;
    }
    let name = jstr(v, "name")?;
    let path = jstr(v, "path").unwrap_or_else(|| format!("/dev/{name}"));
    let size = ju64(v, "size").unwrap_or(0);
    let partition_table = match jstr(v, "pttype")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "dos" | "mbr" => PartitionTable::Mbr,
        _ => PartitionTable::Gpt,
    };

    let mut partitions: Vec<PartitionInfo> = v
        .get("children")
        .and_then(|c| c.as_array())
        .map(|kids| {
            kids.iter()
                .filter(|c| jstr(c, "type").as_deref() == Some("part"))
                .enumerate()
                .map(|(i, c)| build_partition(c, (i + 1) as u32))
                .collect()
        })
        .unwrap_or_default();

    // Whole-disk filesystem (no partition table): one synthetic partition that
    // covers the entire disk.
    if partitions.is_empty() && jstr(v, "fstype").is_some() {
        let mut p = build_partition(v, 1);
        p.size_bytes = size;
        p.offset_bytes = 0;
        partitions.push(p);
    }

    Some(DiskInfo {
        // The disk keeps its kernel name. The window reads a disk's number
        // out of the digits of this field ("Диск N", the whole-disk copy's
        // `disk_number`), and a `/dev/disk/by-id/…` name is full of digits
        // that are not one; a stable disk identity is model + serial +
        // `disk_guid` until the disk number goes from the shared types.
        device_id: path,
        stable_device: String::new(),
        model: jstr(v, "model").unwrap_or_default(),
        serial: jstr(v, "serial").unwrap_or_default(),
        size_bytes: size,
        sector_size_logical: ju64(v, "log-sec").map(|s| s as u32).unwrap_or(512),
        sector_size_physical: ju64(v, "phy-sec").map(|s| s as u32).unwrap_or(512),
        media_type: media_type(v),
        partition_table,
        disk_guid: jstr(v, "ptuuid").unwrap_or_else(|| format!("disk-{name}")),
        partitions,
    })
}

/// `crypt` or `lvm` for a device-mapper node worth listing as a volume: one
/// that holds a filesystem, or an encrypted container (LUKS inside an LVM
/// volume). Not a physical volume of LVM, not swap, not the layers LVM puts
/// under a snapshot (`-real`, `-cow`) or our own snapshot volumes.
fn mapper_volume_kind(v: &Value) -> Option<&'static str> {
    let kind = match jstr(v, "type").as_deref() {
        Some("crypt") => "crypt",
        Some("lvm") => "lvm",
        _ => return None,
    };
    let name = jstr(v, "name").unwrap_or_default();
    // Device-mapper doubles every '-' inside a VG or LV name, so a single one
    // before `real`/`cow` is the layer's own suffix.
    let layer = |suffix: &str| {
        name.ends_with(&format!("-{suffix}")) && !name.ends_with(&format!("--{suffix}"))
    };
    if layer("real") || layer("cow") || name.contains("obk--snap--") {
        return None;
    }
    let (fs, encrypted) = parse_fs(jstr(v, "fstype").as_deref());
    (fs != FileSystem::Unknown || encrypted).then_some(kind)
}

/// A device-mapper volume as a disk of its own with one volume covering it —
/// the same shape as a whole-disk filesystem. Drawn inside the disk its
/// container is on, it would overlap that partition and add up to more than
/// the disk.
fn build_mapper_disk(v: &Value, kind: &str) -> DiskInfo {
    let size = ju64(v, "size").unwrap_or(0);
    let mut p = build_partition(v, 1);
    p.size_bytes = size;
    // `/dev/mapper/<name>`: the name it was opened or created under, which
    // does not change from boot to boot the way `dm-N` does.
    p.volume_device = jstr(v, "path").unwrap_or_else(|| p.kernel_device.clone());
    p.mapper_kind = kind.to_string();
    p.container_password = kind == "crypt" && is_luks_mapping(&p.kernel_device);
    let name = jstr(v, "name").unwrap_or_default();
    DiskInfo {
        device_id: p.volume_device.clone(),
        stable_device: String::new(),
        model: String::new(),
        serial: String::new(),
        size_bytes: size,
        sector_size_logical: ju64(v, "log-sec").map(|s| s as u32).unwrap_or(512),
        sector_size_physical: ju64(v, "phy-sec").map(|s| s as u32).unwrap_or(512),
        media_type: media_type(v),
        partition_table: PartitionTable::Gpt,
        disk_guid: format!("disk-{name}"),
        partitions: vec![p],
    }
}

/// An opened LUKS container: its device-mapper UUID says so
/// (`CRYPT-LUKS1-…`, `CRYPT-LUKS2-…`; BitLocker opened by cryptsetup is
/// `CRYPT-BITLK-…`, and its key is not one the engine recovers).
fn is_luks_mapping(kernel_device: &str) -> bool {
    let Some(name) = std::path::Path::new(kernel_device).file_name() else {
        return false;
    };
    std::fs::read_to_string(
        std::path::Path::new("/sys/class/block")
            .join(name)
            .join("dm/uuid"),
    )
    .is_ok_and(|u| luks_dm_uuid(&u))
}

fn luks_dm_uuid(uuid: &str) -> bool {
    uuid.trim().starts_with("CRYPT-LUKS")
}

/// Walk everything below a disk for device-mapper volumes. `parts` is the
/// chain of partitions above the current node; the ones a mounted `/` sits on
/// are collected in `root_parts` (root on LUKS or LVM: the partition holds
/// the running system, and nothing else would say so).
fn collect_mapper_volumes(
    v: &Value,
    parts: &mut Vec<String>,
    seen: &mut HashSet<String>,
    out: &mut Vec<DiskInfo>,
    root_parts: &mut HashSet<String>,
) {
    let is_part = jstr(v, "type").as_deref() == Some("part");
    if is_part {
        parts.push(kernel_path(v));
    }
    if let Some(kind) = mapper_volume_kind(v) {
        // An LVM volume spread over two disks is listed under both.
        if seen.insert(kernel_path(v)) {
            out.push(build_mapper_disk(v, kind));
        }
        if mount_points(v).iter().any(|m| m == "/") {
            root_parts.extend(parts.iter().cloned());
        }
    }
    for child in v
        .get("children")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        collect_mapper_volumes(child, parts, seen, out, root_parts);
    }
    if is_part {
        parts.pop();
    }
}

/// Parse `lsblk -b -J -O` JSON into disks (pure; sysfs/probe enrichment and
/// the stable names are applied separately so this is testable without real
/// devices). Device-mapper volumes follow the real disks.
fn parse_lsblk(json: &str) -> Result<Vec<DiskInfo>> {
    let root: Value = serde_json::from_str(json)
        .map_err(|e| OwnBackupError::Inventory(format!("lsblk JSON parse: {e}")))?;
    let devices = root
        .get("blockdevices")
        .and_then(|d| d.as_array())
        .ok_or_else(|| OwnBackupError::Inventory("lsblk JSON has no blockdevices".into()))?;
    let mut disks: Vec<DiskInfo> = devices.iter().filter_map(build_disk).collect();

    let mut mapped = Vec::new();
    let mut seen = HashSet::new();
    let mut root_parts = HashSet::new();
    for d in devices.iter().filter(|d| is_listed_disk(d)) {
        collect_mapper_volumes(d, &mut Vec::new(), &mut seen, &mut mapped, &mut root_parts);
    }
    for p in disks.iter_mut().flat_map(|d| d.partitions.iter_mut()) {
        if root_parts.contains(&p.kernel_device) {
            p.is_system = true;
            p.has_os = true;
        }
    }
    disks.extend(mapped);
    Ok(disks)
}

/// Replace each partition's kernel path with a name that survives a reboot:
/// `/dev/disk/by-partuuid/<PARTUUID>`, else `/dev/disk/by-uuid/<UUID>` — but
/// only one that `resolve` (symlink → the device it leads to) shows leads to
/// THIS device. Two partitions with one PARTUUID (a cloned disk plugged in
/// beside its original) share one udev link, and it names only one of them;
/// the other keeps its kernel name rather than be given its twin's.
/// Device-mapper volumes already have theirs.
fn assign_stable_names(disks: &mut [DiskInfo], resolve: &dyn Fn(&str) -> Option<String>) {
    for p in disks.iter_mut().flat_map(|d| d.partitions.iter_mut()) {
        if !p.mapper_kind.is_empty() || p.kernel_device.is_empty() {
            continue;
        }
        let candidates = [
            (!p.gpt_partition_guid.is_empty()).then(|| {
                format!(
                    "/dev/disk/by-partuuid/{}",
                    p.gpt_partition_guid.to_ascii_lowercase()
                )
            }),
            (!p.fs_uuid.is_empty()).then(|| format!("/dev/disk/by-uuid/{}", p.fs_uuid)),
        ];
        if let Some(stable) = candidates
            .into_iter()
            .flatten()
            .find(|c| resolve(c).as_deref() == Some(p.kernel_device.as_str()))
        {
            p.volume_device = stable;
        }
    }
}

/// Where udev keeps the names a disk is known by whatever order it appeared in.
const BY_ID: &str = "/dev/disk/by-id";

/// How good a `/dev/disk/by-id` name is for a whole-disk job; `None` for one
/// that never names a disk (a partition, a device-mapper or md device, an
/// LVM physical volume). The names made of the model and serial (`ata-…`,
/// `nvme-…`, `usb-…`, `virtio-…`) come first: they read as the disk they
/// are. The hardware ids (`wwn-…`, `nvme-eui.…`) name the same disk just as
/// well, and are taken when there is nothing else.
fn by_id_rank(name: &str) -> Option<u8> {
    let partition = name
        .rsplit_once("-part")
        .is_some_and(|(_, n)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    let not_a_disk = ["dm-", "md-", "lvm-"];
    if partition || not_a_disk.iter().any(|p| name.starts_with(p)) {
        return None;
    }
    let hardware = ["wwn-", "nvme-eui.", "nvme-nvme."];
    Some(u8::from(hardware.iter().any(|p| name.starts_with(p))))
}

/// The by-id name of the disk `disk` (its kernel path), out of `links`
/// (name → the device it leads to). The best rank wins, then the smallest
/// name, so the same disk gets the same name at every enumeration: an NVMe
/// disk is `nvme-<model>_<serial>` and also `…_1` for its namespace, and the
/// job must keep whichever it was saved with.
fn pick_by_id(disk: &str, links: &[(String, String)]) -> Option<String> {
    links
        .iter()
        .filter(|(_, target)| target == disk)
        .filter_map(|(name, _)| by_id_rank(name).map(|rank| (rank, name)))
        .min()
        .map(|(_, name)| format!("{BY_ID}/{name}"))
}

/// Give every real disk its by-id name ([`DiskInfo::stable_device`]); a
/// device-mapper volume listed as a disk has none.
fn assign_disk_names(disks: &mut [DiskInfo], links: &[(String, String)]) {
    for disk in disks.iter_mut() {
        let mapped = disk.partitions.len() == 1 && !disk.partitions[0].mapper_kind.is_empty();
        if !mapped {
            disk.stable_device = pick_by_id(&disk.device_id, links).unwrap_or_default();
        }
    }
}

/// `/dev/disk/by-id`, each name with the device it leads to. Empty when udev
/// keeps no such folder (a container, the boot medium before udev settled).
fn read_by_id_links() -> Vec<(String, String)> {
    let Ok(entries) = std::fs::read_dir(BY_ID) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let target = std::fs::canonicalize(e.path()).ok()?;
            Some((name, target.to_string_lossy().into_owned()))
        })
        .collect()
}

/// Partition start offset in bytes, from `/sys/class/block/<name>/start`
/// (kernel reports it in 512-byte units). 0 when unavailable.
fn sysfs_offset_bytes(dev_path: &str) -> u64 {
    let name = dev_path.rsplit('/').next().unwrap_or("");
    std::fs::read_to_string(format!("/sys/class/block/{name}/start"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|sectors| sectors * 512)
        .unwrap_or(0)
}

/// Fill the offset (sysfs) and, when missing, the label / used bytes / encrypted
/// flag by reading the superblock. Best-effort: needs read access to the device
/// (root) for unmounted volumes; degrades to what `lsblk` provided. Asked by
/// the kernel name: sysfs knows nothing of `/dev/disk/by-…`.
fn enrich(p: &mut PartitionInfo) {
    let device = if p.kernel_device.is_empty() {
        p.volume_device.clone()
    } else {
        p.kernel_device.clone()
    };
    if p.offset_bytes == 0 && p.mapper_kind.is_empty() {
        p.offset_bytes = sysfs_offset_bytes(&device);
    }
    let need_probe = p.used_bytes == 0
        || p.volume_label.is_empty()
        || (p.filesystem == FileSystem::Unknown && !p.is_encrypted);
    if !need_probe {
        return;
    }
    if let Some(probed) = crate::fs_probe::probe_partition_path(&device) {
        if p.volume_label.is_empty() {
            if let Some(l) = probed.label {
                p.volume_label = l;
            }
        }
        if p.used_bytes == 0 {
            if let Some(u) = probed.used_bytes {
                p.used_bytes = u;
            }
        }
        if probed.encrypted {
            p.is_encrypted = true;
        } else if p.filesystem == FileSystem::Unknown && probed.fs != FileSystem::Unknown {
            p.filesystem = probed.fs;
        }
    }
}

fn run_lsblk() -> Result<String> {
    let out = Command::new("lsblk")
        .args(["-b", "-J", "-O"])
        .output()
        .map_err(|e| OwnBackupError::Inventory(format!("failed to run lsblk: {e}")))?;
    if !out.status.success() {
        return Err(OwnBackupError::Inventory(format!(
            "lsblk exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

#[async_trait]
impl super::traits::DiskInventory for LinuxInventory {
    async fn enumerate_disks(&self) -> Result<Vec<DiskInfo>> {
        let mut disks = parse_lsblk(&run_lsblk()?)?;
        assign_stable_names(&mut disks, &|link| {
            std::fs::canonicalize(link)
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
        });
        assign_disk_names(&mut disks, &read_by_id_links());
        for disk in &mut disks {
            for part in &mut disk.partitions {
                enrich(part);
            }
            // In on-disk order, which is known only now that the offsets are.
            disk.partitions.sort_by_key(|p| p.offset_bytes);
        }
        Ok(disks)
    }

    async fn get_disk_details(&self, device_id: &str) -> Result<DiskInfo> {
        self.enumerate_disks()
            .await?
            .into_iter()
            .find(|d| d.device_id == device_id)
            .ok_or_else(|| OwnBackupError::Inventory(format!("disk not found: {device_id}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "blockdevices": [
        {
          "name": "sda", "path": "/dev/sda", "type": "disk", "size": 512110190592,
          "pttype": "gpt", "model": "Samsung SSD", "serial": "S123", "rota": false,
          "log-sec": 512, "phy-sec": 4096, "ptuuid": "DEAD-BEEF",
          "children": [
            { "name": "sda1", "path": "/dev/sda1", "type": "part", "size": 536870912,
              "fstype": "vfat", "label": "EFI", "uuid": "1234-ABCD", "partn": 1,
              "parttype": "c12a7328-f81f-11d2-ba4b-00a0c93ec93b", "mountpoint": "/boot/efi" },
            { "name": "sda2", "path": "/dev/sda2", "type": "part", "size": 137438953472,
              "fstype": "crypto_LUKS", "uuid": "efab-5678", "partn": 2 },
            { "name": "sda3", "path": "/dev/sda3", "type": "part", "size": 300000000000,
              "fstype": "ext4", "label": "data", "fsused": 100000000000, "partn": 3,
              "mountpoint": "/" }
          ]
        },
        { "name": "sr0", "type": "rom", "size": 0 }
      ]
    }"#;

    #[test]
    fn only_a_luks_mapping_asks_for_its_container_password() {
        assert!(luks_dm_uuid("CRYPT-LUKS2-0123456789abcdef-root\n"));
        assert!(luks_dm_uuid("CRYPT-LUKS1-0123-data"));
        assert!(!luks_dm_uuid("CRYPT-BITLK-0123-win"));
        assert!(!luks_dm_uuid("LVM-abcdef"));
        assert!(!is_luks_mapping("/dev/own-backup-no-such-dm"));
    }

    #[test]
    fn parses_disks_partitions_and_encryption() {
        let disks = parse_lsblk(SAMPLE).unwrap();
        assert_eq!(disks.len(), 1); // the rom is skipped
        let d = &disks[0];
        assert_eq!(d.device_id, "/dev/sda");
        assert_eq!(d.partition_table, PartitionTable::Gpt);
        assert_eq!(d.partitions.len(), 3);

        let efi = &d.partitions[0];
        assert_eq!(efi.filesystem, FileSystem::Fat32);
        assert_eq!(efi.volume_label, "EFI");
        assert_eq!(efi.volume_device, "/dev/sda1");
        assert!(efi.is_boot);

        let luks = &d.partitions[1];
        assert_eq!(luks.filesystem, FileSystem::Unknown);
        assert!(luks.is_encrypted);
        assert_eq!(luks.volume_device, "/dev/sda2"); // addressable for decryption

        let root = &d.partitions[2];
        assert_eq!(root.filesystem, FileSystem::Ext4);
        assert_eq!(root.used_bytes, 100_000_000_000);
        assert!(root.is_system);
    }

    /// The owner's disk, as `lsblk -b -J -O` printed it on 2026-10-18 (cut
    /// down to the fields read): ESP, /boot, LUKS holding btrfs `/`, `/home`
    /// and `/swap`, and a BitLocker partition opened with cryptsetup. Plus the
    /// devices that are not disks to back up.
    const OWNER: &str = r#"{
      "blockdevices": [
        { "name": "nvme0n1", "kname": "nvme0n1", "path": "/dev/nvme0n1", "type": "disk",
          "size": 512110190592, "pttype": "gpt", "ptuuid": "e36315e5-0757-4eea-2aab-a57b9b9ab109",
          "model": "Samsung SSD 970 PRO 512GB", "serial": "S463NF0KB00462R", "rota": false,
          "children": [
            { "name": "nvme0n1p1", "kname": "nvme0n1p1", "path": "/dev/nvme0n1p1", "type": "part",
              "size": 1375714304, "fstype": "vfat", "uuid": "060B-E961", "label": "BOOT_EFI",
              "partuuid": "245ef510-a193-458d-b01f-d13738760455", "partn": 1,
              "parttype": "c12a7328-f81f-11d2-ba4b-00a0c93ec93b",
              "mountpoint": "/boot/efi", "mountpoints": ["/boot/efi"] },
            { "name": "nvme0n1p2", "kname": "nvme0n1p2", "path": "/dev/nvme0n1p2", "type": "part",
              "size": 1073741824, "fstype": "ext4", "uuid": "a2649700-5dbb-4f7b-833d-f57c7c812ef4",
              "partuuid": "2d272048-0d61-4a8c-a715-08d35535ed93", "partn": 2,
              "mountpoint": "/boot", "mountpoints": ["/boot"] },
            { "name": "nvme0n1p3", "kname": "nvme0n1p3", "path": "/dev/nvme0n1p3", "type": "part",
              "size": 137438953472, "fstype": "crypto_LUKS", "uuid": "16e1f072-b3b5-4aa7-b296-67b738edf0ca",
              "partuuid": "495bc881-f4b8-4a9a-8f6f-3b63a1d00227", "partn": 3, "mountpoints": [null],
              "children": [
                { "name": "luks-16e1f072-b3b5-4aa7-b296-67b738edf0ca", "kname": "dm-0",
                  "path": "/dev/mapper/luks-16e1f072-b3b5-4aa7-b296-67b738edf0ca", "type": "crypt",
                  "size": 137422176256, "fstype": "btrfs", "uuid": "17d01ac2-2643-4e09-8773-2c59ecde4fc9",
                  "fsused": 40000000000, "rota": false,
                  "mountpoint": "/home", "mountpoints": ["/home", "/swap", "/"] }
              ] },
            { "name": "nvme0n1p4", "kname": "nvme0n1p4", "path": "/dev/nvme0n1p4", "type": "part",
              "size": 372221411328, "fstype": "BitLocker", "uuid": "06b2cd70-dab8-4639-8c2f-5093c5af976b",
              "label": "MODULATOR WorkDisk-2", "partuuid": "95fa1035-0605-4d22-be37-a684b1b616d0", "partn": 4,
              "children": [
                { "name": "WorkDisk-2", "kname": "dm-1", "path": "/dev/mapper/WorkDisk-2", "type": "crypt",
                  "size": 372221411328, "fstype": "ntfs", "uuid": "1CD64E52D64E2C78", "label": "WorkDisk-2",
                  "mountpoint": "/run/media/u/WorkDisk-2", "mountpoints": ["/run/media/u/WorkDisk-2"] }
              ] }
          ] },
        { "name": "sda", "kname": "sda", "path": "/dev/sda", "type": "disk", "size": 960197124096,
          "pttype": "dos", "ptuuid": "4955b993", "rota": true,
          "children": [
            { "name": "sda1", "kname": "sda1", "path": "/dev/sda1", "type": "part", "size": 960194674688,
              "fstype": "ext4", "uuid": "67ba887f-5812-4321-947c-e57178a776b5",
              "partuuid": "4955b993-01", "partn": 1 },
            { "name": "sda2", "kname": "sda2", "path": "/dev/sda2", "type": "part", "size": 1000,
              "fstype": "ext4", "uuid": "11111111-0000-0000-0000-000000000000",
              "partuuid": "4955b993-02", "partn": 2 },
            { "name": "sda3", "kname": "sda3", "path": "/dev/sda3", "type": "part", "size": 1000,
              "fstype": "LVM2_member", "partn": 3,
              "children": [
                { "name": "vg0-data", "kname": "dm-2", "path": "/dev/mapper/vg0-data", "type": "lvm",
                  "size": 900, "fstype": "xfs" },
                { "name": "vg0-swap", "kname": "dm-3", "path": "/dev/mapper/vg0-swap", "type": "lvm",
                  "size": 50, "fstype": "swap" },
                { "name": "vg0-data-real", "kname": "dm-4", "path": "/dev/mapper/vg0-data-real", "type": "lvm",
                  "size": 900, "fstype": "xfs" },
                { "name": "vg0-obk--snap--0123456789ab", "kname": "dm-5",
                  "path": "/dev/mapper/vg0-obk--snap--0123456789ab", "type": "lvm", "size": 900, "fstype": "xfs" },
                { "name": "vg0-my--real", "kname": "dm-6", "path": "/dev/mapper/vg0-my--real", "type": "lvm",
                  "size": 10, "fstype": "ext4" }
              ] }
          ] },
        { "name": "sdb", "kname": "sdb", "path": "/dev/sdb", "type": "disk", "size": 2000,
          "pttype": "gpt", "rota": true,
          "children": [
            { "name": "sdb1", "kname": "sdb1", "path": "/dev/sdb1", "type": "part", "size": 1000,
              "fstype": "LVM2_member", "partn": 1,
              "children": [
                { "name": "vg0-data", "kname": "dm-2", "path": "/dev/mapper/vg0-data", "type": "lvm",
                  "size": 900, "fstype": "xfs" }
              ] }
          ] },
        { "name": "loop0", "kname": "loop0", "path": "/dev/loop0", "type": "loop", "size": 4096,
          "fstype": "squashfs", "mountpoint": "/snap/core/1" },
        { "name": "loop1", "kname": "loop1", "path": "/dev/loop1", "type": "loop", "size": 1048576,
          "fstype": "ext4", "uuid": "aaaa0000-0000-0000-0000-000000000001" },
        { "name": "nbd0", "kname": "nbd0", "path": "/dev/nbd0", "type": "disk", "size": 1048576 },
        { "name": "zram0", "kname": "zram0", "path": "/dev/zram0", "type": "disk", "size": 1048576,
          "mountpoint": "[SWAP]" }
      ]
    }"#;

    #[test]
    fn device_mapper_volumes_are_listed_and_what_is_not_a_disk_is_not() {
        let disks = parse_lsblk(OWNER).unwrap();
        let ids: Vec<&str> = disks.iter().map(|d| d.device_id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "/dev/nvme0n1",
                "/dev/sda",
                "/dev/sdb",
                "/dev/loop1",
                "/dev/mapper/luks-16e1f072-b3b5-4aa7-b296-67b738edf0ca",
                "/dev/mapper/WorkDisk-2",
                "/dev/mapper/vg0-data",
                // `my-real` is a volume called that, not a snapshot's layer.
                "/dev/mapper/vg0-my--real",
            ],
            "no snap squashfs loop, NBD, zram, swap, LVM layer or our snapshot"
        );

        let root = &disks[4].partitions[0];
        assert_eq!(root.mapper_kind, "crypt");
        assert_eq!(
            root.volume_device,
            "/dev/mapper/luks-16e1f072-b3b5-4aa7-b296-67b738edf0ca"
        );
        assert_eq!(root.kernel_device, "/dev/dm-0");
        assert_eq!(root.filesystem, FileSystem::Btrfs);
        assert_eq!(root.size_bytes, 137_422_176_256);
        assert_eq!(root.offset_bytes, 0);
        // `/` is the third of its mount points, and still the one that counts.
        assert!(root.is_system && root.has_os);
        assert_eq!(root.linux_last_mounted, "/");
        assert_eq!(disks[6].partitions[0].mapper_kind, "lvm");

        // The LUKS partition under the running root holds the running system.
        let nvme = &disks[0];
        let luks = nvme
            .partitions
            .iter()
            .find(|p| p.kernel_device == "/dev/nvme0n1p3")
            .unwrap();
        assert!(luks.is_system && luks.is_encrypted);
        let bitlocker = nvme.partitions.iter().find(|p| p.index == 4).unwrap();
        assert!(!bitlocker.is_system);
        assert_eq!(bitlocker.volume_label, "WorkDisk-2");
        assert!(nvme.partitions.iter().filter(|p| p.is_system).count() == 1);
    }

    #[test]
    fn bitlocker_description_is_stripped_to_volume_label() {
        assert_eq!(
            clean_bitlocker_label("MODULATOR WorkDisk-2 30.06.2025"),
            "WorkDisk-2"
        );
        assert_eq!(
            clean_bitlocker_label("MODULATOR SSD-1Tb 29.06.2025"),
            "SSD-1Tb"
        );
        assert_eq!(
            clean_bitlocker_label("MODULATOR WorkDisk-1 29.06.2025"),
            "WorkDisk-1"
        );
        assert_eq!(
            clean_bitlocker_label("DESKTOP-849SVK9 Windows 16.09.2026"),
            "Windows"
        );
        assert_eq!(clean_bitlocker_label("MODULATOR WorkDisk-2"), "WorkDisk-2");
        assert_eq!(
            clean_bitlocker_label("MODULATOR D: WorkDisk-2 30.06.2025"),
            "WorkDisk-2"
        );
        assert_eq!(clean_bitlocker_label("MODULATOR 30.06.2025"), "");
        assert_eq!(clean_bitlocker_label("MODULATOR D: 30.06.2025"), "");
    }

    #[test]
    fn a_partition_is_named_by_what_survives_a_reboot() {
        let mut disks = parse_lsblk(OWNER).unwrap();
        // udev's links as they are on the machine — except the second MBR
        // partition, whose PARTUUID link leads to another device (a clone).
        let links = |link: &str| -> Option<String> {
            Some(
                match link {
                    "/dev/disk/by-partuuid/245ef510-a193-458d-b01f-d13738760455" => {
                        "/dev/nvme0n1p1"
                    }
                    "/dev/disk/by-partuuid/2d272048-0d61-4a8c-a715-08d35535ed93" => {
                        "/dev/nvme0n1p2"
                    }
                    "/dev/disk/by-partuuid/495bc881-f4b8-4a9a-8f6f-3b63a1d00227" => {
                        "/dev/nvme0n1p3"
                    }
                    "/dev/disk/by-partuuid/4955b993-01" => "/dev/sda1",
                    "/dev/disk/by-partuuid/4955b993-02" => "/dev/sdc2",
                    "/dev/disk/by-uuid/11111111-0000-0000-0000-000000000000" => "/dev/sda2",
                    "/dev/disk/by-uuid/aaaa0000-0000-0000-0000-000000000001" => "/dev/loop1",
                    _ => return None,
                }
                .to_string(),
            )
        };
        assign_stable_names(&mut disks, &links);
        let by_kernel = |k: &str| {
            disks
                .iter()
                .flat_map(|d| d.partitions.iter())
                .find(|p| p.kernel_device == k)
                .unwrap()
                .volume_device
                .clone()
        };
        assert_eq!(
            by_kernel("/dev/nvme0n1p2"),
            "/dev/disk/by-partuuid/2d272048-0d61-4a8c-a715-08d35535ed93"
        );
        assert_eq!(by_kernel("/dev/sda1"), "/dev/disk/by-partuuid/4955b993-01");
        // Its PARTUUID names another device: the filesystem UUID does not.
        assert_eq!(
            by_kernel("/dev/sda2"),
            "/dev/disk/by-uuid/11111111-0000-0000-0000-000000000000"
        );
        // No link for it at all: the kernel name stays — never a guess.
        assert_eq!(by_kernel("/dev/nvme0n1p4"), "/dev/nvme0n1p4");
        // A whole-disk filesystem by its filesystem UUID.
        assert_eq!(
            by_kernel("/dev/loop1"),
            "/dev/disk/by-uuid/aaaa0000-0000-0000-0000-000000000001"
        );
        // Mapped volumes keep their /dev/mapper name.
        assert_eq!(by_kernel("/dev/dm-1"), "/dev/mapper/WorkDisk-2");
        // And the kernel name is still there to show.
        let p2 = disks[0].partitions.iter().find(|p| p.index == 2).unwrap();
        assert_eq!(p2.kernel_device, "/dev/nvme0n1p2");
    }

    #[test]
    fn a_disk_is_named_by_its_by_id_link() {
        let mut disks = parse_lsblk(OWNER).unwrap();
        let link = |name: &str, target: &str| (name.to_string(), target.to_string());
        let links = [
            link("nvme-eui.0025385b71b0aa01", "/dev/nvme0n1"),
            link("nvme-Samsung_SSD_970_PRO_1TB_S462NF0K_1", "/dev/nvme0n1"),
            link("nvme-Samsung_SSD_970_PRO_1TB_S462NF0K", "/dev/nvme0n1"),
            link(
                "nvme-Samsung_SSD_970_PRO_1TB_S462NF0K-part2",
                "/dev/nvme0n1p2",
            ),
            link("wwn-0x5000c500a1b2c3d4", "/dev/sda"),
            link("dm-name-WorkDisk-2", "/dev/dm-1"),
        ];
        assign_disk_names(&mut disks, &links);
        let name = |kernel: &str| {
            disks
                .iter()
                .find(|d| d.device_id == kernel)
                .unwrap()
                .stable_device
                .clone()
        };
        // Model and serial before the hardware id; the shortest of the two
        // namespace spellings, every time.
        assert_eq!(
            name("/dev/nvme0n1"),
            "/dev/disk/by-id/nvme-Samsung_SSD_970_PRO_1TB_S462NF0K"
        );
        // Only the hardware id: that is the name.
        assert_eq!(name("/dev/sda"), "/dev/disk/by-id/wwn-0x5000c500a1b2c3d4");
        // No link at all: nothing, never the kernel name.
        let mapped = disks
            .iter()
            .filter(|d| d.device_id.starts_with("/dev/mapper/"));
        for d in mapped {
            assert!(d.stable_device.is_empty(), "{}", d.device_id);
        }
        assert!(disks
            .iter()
            .filter(|d| !d.device_id.starts_with("/dev/nvme0n1") && d.device_id != "/dev/sda")
            .all(|d| d.stable_device.is_empty()));
    }

    #[test]
    fn partitions_and_mapper_devices_are_not_disk_names() {
        assert_eq!(by_id_rank("ata-WDC_WD20-part1"), None);
        assert_eq!(by_id_rank("dm-uuid-CRYPT-LUKS2-abc"), None);
        assert_eq!(by_id_rank("lvm-pv-uuid-xyz"), None);
        assert_eq!(by_id_rank("md-uuid-1"), None);
        assert_eq!(by_id_rank("ata-WDC_WD20"), Some(0));
        assert_eq!(by_id_rank("usb-Kingston_DataTraveler-0:0"), Some(0));
        assert_eq!(by_id_rank("wwn-0x50014ee"), Some(1));
        // A model that happens to contain "-part" but no number after it.
        assert_eq!(by_id_rank("usb-Some-partner_Disk"), Some(0));
    }

    // Runs against the real system; `cargo test -p own-backup-inventory -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn real_enumerate_smoke() {
        use crate::traits::DiskInventory;
        let disks = LinuxInventory::new().enumerate_disks().await.unwrap();
        for d in &disks {
            eprintln!(
                "DISK {} '{}' {} bytes {:?}",
                d.device_id, d.model, d.size_bytes, d.partition_table
            );
            for p in &d.partitions {
                eprintln!(
                    "  {} ({}{}) off={} size={} fs={:?} label={:?} used={} enc={} sys={}",
                    p.volume_device,
                    p.kernel_device,
                    if p.mapper_kind.is_empty() {
                        String::new()
                    } else {
                        format!(", {}", p.mapper_kind)
                    },
                    p.offset_bytes,
                    p.size_bytes,
                    p.filesystem,
                    p.volume_label,
                    p.used_bytes,
                    p.is_encrypted,
                    p.is_system
                );
            }
        }
        assert!(!disks.is_empty());
    }

    #[test]
    fn whole_disk_filesystem_becomes_one_partition() {
        // A disk with a filesystem and no partition table (common on VMs / loop).
        let json = r#"{"blockdevices":[
          {"name":"loop0","path":"/dev/loop0","type":"loop","size":1048576,"fstype":"ext4","label":"img"}
        ]}"#;
        let disks = parse_lsblk(json).unwrap();
        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0].partitions.len(), 1);
        assert_eq!(disks[0].partitions[0].filesystem, FileSystem::Ext4);
        assert_eq!(disks[0].partitions[0].size_bytes, 1048576);
    }
}
