//! Making a restored ESP bootable on a machine that has never seen it (R11).
//!
//! The reading half of this lives in [`crate::esp_fallback`]; here is the part
//! that touches a disk: find the ESP's volume, look at what is on it, work out
//! which bootloader directory belongs to the system that was just restored, and
//! copy it to `\EFI\BOOT\BOOTX64.EFI`.
//!
//! The ESP is FAT32, so Windows mounts it like any other volume — it simply has
//! no drive letter. A volume GUID path (`\\?\Volume{…}\`) opens it without one,
//! which is what the rest of the restore already uses.

#![cfg(target_os = "windows")]

use std::path::Path;

use tracing::{info, warn};

use own_backup_common::restore_plan::PartRef;
use own_backup_common::{tr, OwnBackupError, Result};

use crate::esp_fallback_tree::ensure_at_root;

pub use crate::esp_fallback_tree::FallbackOutcome;

/// Ensure `\EFI\BOOT\BOOTX64.EFI` exists on the ESP at `esp.offset`.
///
/// `restored` are the partitions this run wrote; their filesystem UUIDs are
/// what identifies the live bootloader directory among the leftovers of every
/// system the machine has ever had.
///
/// Never overwrites an existing fallback loader.
pub fn ensure_fallback_bootloader(
    disk_number: u32,
    esp: PartRef,
    restored: &[PartRef],
) -> Result<FallbackOutcome> {
    let root =
        crate::volume_win::volume_root_at_offset(disk_number, esp.offset).ok_or_else(|| {
            OwnBackupError::Device(
                tr!("esp_fallback_win.no_volume", offset = esp.offset).into_owned(),
            )
        })?;
    ensure_at_root(
        Path::new(&root),
        &restored_filesystem_uuids(disk_number, restored),
        &format!("disk {disk_number} @ {}", esp.offset),
    )
}

/// Filesystem UUIDs of the partitions this restore wrote.
///
/// Read from the target disk rather than from the copy's manifest: on Windows
/// the inventory cannot read an ext4 or btrfs UUID at all, and the bytes are
/// right here.
fn restored_filesystem_uuids(disk_number: u32, restored: &[PartRef]) -> Vec<String> {
    let mut out = Vec::new();
    for p in restored {
        let path = format!("part:{disk_number}:{}:{}", p.offset, p.size);
        let mut dev = match crate::win_device::WinRawDevice::open_read(&path) {
            Ok(d) => d,
            Err(e) => {
                warn!(part = p.index, error = %e, "could not open a restored partition to read its filesystem UUID");
                continue;
            }
        };
        let want = crate::fs_uuid::PROBE_LEN.min(p.size as usize);
        let mut head = vec![0u8; want];
        if let Err(e) = dev.read_at(0, &mut head) {
            warn!(part = p.index, error = %e, "could not read the head of a restored partition");
            continue;
        }
        if let Some(uuid) = crate::fs_uuid::probe(&head) {
            info!(part = p.index, %uuid, "restored filesystem identified");
            out.push(uuid);
        }
    }
    out
}
