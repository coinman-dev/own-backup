//! Disk inventory on Linux via `lsblk` (+ sysfs for partition offsets).
//!
//! The `/dev` analogue of [`crate::wmi`]. `lsblk -b -J -O` gives the disk/
//! partition tree with filesystem type, label, UUID and (for mounted volumes)
//! used bytes; the partition's start offset comes from
//! `/sys/class/block/<name>/start`, and for unmounted / unrecognized volumes we
//! read the superblock directly ([`crate::fs_probe::probe_partition_path`]) to
//! fill the label / used bytes and to flag encrypted (LUKS/BitLocker) volumes.

#![cfg(target_os = "linux")]

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

const GPT_TYPE_EFI_SYSTEM: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";

/// Build a `PartitionInfo` from an `lsblk` node (a `part`, or a whole-disk
/// filesystem). `offset_bytes` is filled later from sysfs.
fn build_partition(v: &Value, index: u32) -> PartitionInfo {
    let path = jstr(v, "path").unwrap_or_else(|| format!("/dev/{}", jstr(v, "name").unwrap_or_default()));
    let (filesystem, is_encrypted) = parse_fs(jstr(v, "fstype").as_deref());
    let gpt_type_guid = jstr(v, "parttype").unwrap_or_default().to_uppercase();
    let mount = jstr(v, "mountpoint");
    let is_boot = gpt_type_guid.contains(GPT_TYPE_EFI_SYSTEM)
        || matches!(mount.as_deref(), Some("/boot") | Some("/boot/efi"));
    let is_system = mount.as_deref() == Some("/");

    PartitionInfo {
        index: ju64(v, "partn").map(|n| n as u32).unwrap_or(index),
        offset_bytes: 0, // from sysfs in enrich()
        size_bytes: ju64(v, "size").unwrap_or(0),
        used_bytes: ju64(v, "fsused").unwrap_or(0),
        gpt_type_guid,
        gpt_partition_guid: jstr(v, "partuuid").unwrap_or_default().to_uppercase(),
        filesystem,
        volume_letter: None,
        // A Linux partition is addressed by its own device node.
        volume_device: path,
        volume_label: jstr(v, "label").unwrap_or_default(),
        fs_uuid: jstr(v, "uuid").unwrap_or_default(),
        is_system,
        is_boot,
        // Linux support is deferred (Windows-first), and probing an unmounted
        // partition for an installed system means mounting it. `is_system`
        // already covers the running root, which is what this build uses.
        has_os: is_system,
        is_encrypted,
    }
}

/// Build a `DiskInfo` from a top-level `lsblk` node. Returns `None` for nodes
/// that are not whole disks (`part`, `rom`, `crypt`, `lvm`, …).
fn build_disk(v: &Value) -> Option<DiskInfo> {
    match jstr(v, "type").as_deref() {
        Some("disk") | Some("loop") => {}
        _ => return None,
    }
    let name = jstr(v, "name")?;
    let path = jstr(v, "path").unwrap_or_else(|| format!("/dev/{name}"));
    let size = ju64(v, "size").unwrap_or(0);
    let partition_table = match jstr(v, "pttype").unwrap_or_default().to_ascii_lowercase().as_str() {
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
    partitions.sort_by_key(|p| p.offset_bytes);

    Some(DiskInfo {
        device_id: path,
        model: jstr(v, "model").unwrap_or_default(),
        serial: jstr(v, "serial").unwrap_or_default(),
        size_bytes: size,
        sector_size_logical: ju64(v, "log-sec").map(|s| s as u32).unwrap_or(512),
        sector_size_physical: ju64(v, "phy-sec").map(|s| s as u32).unwrap_or(512),
        media_type: if v.get("rota").and_then(Value::as_bool) == Some(true) {
            "HDD".into()
        } else {
            "SSD".into()
        },
        partition_table,
        disk_guid: jstr(v, "ptuuid").unwrap_or_else(|| format!("disk-{name}")),
        partitions,
    })
}

/// Parse `lsblk -b -J -O` JSON into disks (pure; sysfs/probe enrichment is
/// applied separately so this is testable without real devices).
fn parse_lsblk(json: &str) -> Result<Vec<DiskInfo>> {
    let root: Value = serde_json::from_str(json)
        .map_err(|e| OwnBackupError::Inventory(format!("lsblk JSON parse: {e}")))?;
    let devices = root
        .get("blockdevices")
        .and_then(|d| d.as_array())
        .ok_or_else(|| OwnBackupError::Inventory("lsblk JSON has no blockdevices".into()))?;
    Ok(devices.iter().filter_map(build_disk).collect())
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
/// (root) for unmounted volumes; degrades to what `lsblk` provided.
fn enrich(p: &mut PartitionInfo) {
    if p.offset_bytes == 0 {
        p.offset_bytes = sysfs_offset_bytes(&p.volume_device);
    }
    let need_probe = p.used_bytes == 0
        || p.volume_label.is_empty()
        || (p.filesystem == FileSystem::Unknown && !p.is_encrypted);
    if !need_probe {
        return;
    }
    if let Some(probed) = crate::fs_probe::probe_partition_path(&p.volume_device) {
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
        for disk in &mut disks {
            for part in &mut disk.partitions {
                enrich(part);
            }
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

    // Runs against the real system; `cargo test -p own-backup-inventory -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn real_enumerate_smoke() {
        use crate::traits::DiskInventory;
        let disks = LinuxInventory::new().enumerate_disks().await.unwrap();
        for d in &disks {
            eprintln!("DISK {} '{}' {} bytes {:?}", d.device_id, d.model, d.size_bytes, d.partition_table);
            for p in &d.partitions {
                eprintln!(
                    "  {} off={} size={} fs={:?} label={:?} used={} enc={}",
                    p.volume_device, p.offset_bytes, p.size_bytes, p.filesystem,
                    p.volume_label, p.used_bytes, p.is_encrypted
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
