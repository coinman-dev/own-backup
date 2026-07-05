//! Safety guard for whole-disk (bootable) restore.
//!
//! Recreating a disk's partition table and raw-writing every partition is
//! destructive. Doing it to the disk that hosts the *running* Windows would
//! corrupt the live system mid-write, so it is allowed only from a recovery
//! environment (WinPE) — or to a different disk.
//!
//! The policy is a pure, unit-tested function; the "which disk runs Windows"
//! and "are we in WinPE" detection is Windows-only and best-effort.

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
