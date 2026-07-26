//! What the running system will not let you overwrite.
//!
//! The first version of this refused the whole disk Windows boots from, which
//! is too blunt: a spare partition on that disk is a perfectly ordinary restore
//! target — it is how anyone puts Linux next to an existing Windows. What
//! actually cannot be touched is a much smaller set: the volume Windows is
//! running from, and the ESP it booted through.
//!
//! Two layers, and they answer different questions:
//!
//! - [`check_whole_disk_restore`] — may the **whole** disk be recreated? Only
//!   from a recovery environment, since that rewrites everything including the
//!   running system.
//! - [`protected_partitions`] — which partitions on this disk is the running
//!   system standing on? Everything else on it is fair game, provided the
//!   partition table is applied through the partition manager rather than
//!   written behind its back (see `disk_session_win`).

use own_backup_common::{OwnBackupError, Result};

/// Decide whether wiping and restoring onto `target_disk` is safe.
///
/// - `system_disk`: the physical drive that hosts the running Windows, if known.
/// - `is_boot_media`: true when running from a recovery environment (WinPE),
///   where no disk hosts a live OS being written to.
///
/// Denied only when we are confident the target IS the live system disk and we
/// are NOT on boot media. Unknown system disk ⇒ allowed (the caller still
/// requires an explicit destructive confirmation).
pub fn check_whole_disk_restore(
    target_disk: u32,
    system_disk: Option<u32>,
    is_boot_media: bool,
) -> Result<()> {
    if !is_boot_media {
        if let Some(sys) = system_disk {
            if sys == target_disk {
                return Err(OwnBackupError::RestoreIncompatible(
                    "Нельзя восстанавливать диск, на котором работает Windows, «на живую». \
                     Загрузитесь с носителя восстановления (WinPE) или выберите другой диск."
                        .into(),
                ));
            }
        }
    }
    Ok(())
}

/// The physical drive number that hosts the running Windows (`%SystemDrive%`),
/// or `None` if it can't be determined.
#[cfg(target_os = "windows")]
pub fn system_disk_number() -> Option<u32> {
    use std::os::windows::process::CommandExt;
    use std::process::Command;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let sysdrive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    let letter = sysdrive.chars().next().filter(|c| c.is_ascii_alphabetic())?;
    let out = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!("(Get-Partition -DriveLetter {letter}).DiskNumber"),
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse::<u32>().ok()
}

/// True when running under a Windows recovery environment (WinPE), which boots
/// from X: rather than a real system drive.
#[cfg(target_os = "windows")]
pub fn is_boot_media() -> bool {
    std::env::var("SystemDrive")
        .map(|d| d.eq_ignore_ascii_case("X:"))
        .unwrap_or(false)
}

/// Guard a whole-disk restore using live Windows detection.
#[cfg(target_os = "windows")]
pub fn guard_target_disk(target_disk: u32) -> Result<()> {
    check_whole_disk_restore(target_disk, system_disk_number(), is_boot_media())
}

/// Partition indices on `disk` that the running Windows is standing on.
///
/// Empty when the disk hosts no running system, when we are on boot media, or
/// when nothing could be determined — an empty list means "nothing to protect
/// here", and the caller still requires an explicit destructive confirmation.
///
/// Two things qualify:
/// - the volume `%SystemRoot%` lives on — overwrite it and the machine stops
///   mid-write;
/// - the EFI System Partition on the same disk, which is how it boots at all.
///
/// A data volume with open files is deliberately *not* on this list: it will
/// refuse to dismount when the restore reaches it, which is a clearer and more
/// specific failure than a rule guessing in advance.
#[cfg(target_os = "windows")]
pub fn protected_partitions(disk: u32) -> Vec<u32> {
    if is_boot_media() {
        return Vec::new(); // nothing here is running
    }
    if system_disk_number() != Some(disk) {
        return Vec::new();
    }
    let Ok(layout) = own_backup_backup::gpt_reader::read_disk_layout(disk) else {
        return Vec::new();
    };
    let sector = layout.sector_size.max(512) as u64;

    let mut out = Vec::new();
    // The volume the running Windows lives on, located by byte offset rather
    // than by drive letter: a letter can be absent or reassigned, an offset
    // cannot.
    if let Some(offset) = system_volume_offset(disk) {
        if let Some(p) = layout.partitions.iter().find(|p| p.first_lba * sector == offset) {
            out.push(p.index);
        }
    }
    // The ESP on this disk — without it the machine does not start.
    for p in &layout.partitions {
        if p.type_guid.eq_ignore_ascii_case(own_backup_common::gpt::GPT_TYPE_EFI_SYSTEM)
            && !out.contains(&p.index)
        {
            out.push(p.index);
        }
    }
    out.sort_unstable();
    out
}

/// Byte offset on `disk` of the volume holding `%SystemRoot%`.
#[cfg(target_os = "windows")]
fn system_volume_offset(disk: u32) -> Option<u64> {
    let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    let root = own_backup_backup::volume_win::volume_root_for_path(&system_root)?;
    own_backup_backup::volume_win::volume_extents(&root)
        .into_iter()
        .find(|e| e.disk_number == disk)
        .map(|e| e.offset)
}

#[cfg(not(target_os = "windows"))]
pub fn protected_partitions(_disk: u32) -> Vec<u32> {
    Vec::new()
}

#[cfg(not(target_os = "windows"))]
pub fn system_disk_number() -> Option<u32> {
    None
}
#[cfg(not(target_os = "windows"))]
pub fn is_boot_media() -> bool {
    false
}
#[cfg(not(target_os = "windows"))]
pub fn guard_target_disk(_target_disk: u32) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_live_system_disk_restore() {
        // Target IS the running system disk, not on boot media → denied.
        assert!(check_whole_disk_restore(0, Some(0), false).is_err());
    }

    #[test]
    fn allows_a_different_disk() {
        assert!(check_whole_disk_restore(1, Some(0), false).is_ok());
    }

    #[test]
    fn allows_system_disk_from_boot_media() {
        // In WinPE the OS is not running from the target — restoring it is fine.
        assert!(check_whole_disk_restore(0, Some(0), true).is_ok());
    }

    #[test]
    fn allows_when_system_disk_unknown() {
        // Detection failed — don't block; the caller still confirms destruction.
        assert!(check_whole_disk_restore(0, None, false).is_ok());
    }
}
