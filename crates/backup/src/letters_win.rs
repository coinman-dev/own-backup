//! Drive letters (R3d) and growing a filesystem into its partition (R3e).
//!
//! The backup side of this project only ever *removed* drive letters — a
//! mounted image must not surface as `J:` while it is being written. Restore
//! needs the other direction: a restored volume the user expects to find at
//! `D:` has to be put there.

#![cfg(target_os = "windows")]

use tracing::{debug, info, warn};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, DeleteVolumeMountPointW, GetLogicalDrives, SetVolumeMountPointW,
    FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Ioctl::FSCTL_EXTEND_VOLUME;
use windows::Win32::System::IO::DeviceIoControl;

use own_backup_common::{OwnBackupError, Result};

use crate::volume_win::{volume_drive_letters, FileHandle};
use crate::win_device::to_wide;

/// Drive letters currently in use, uppercase, without the colon.
pub fn taken_letters() -> Vec<char> {
    // SAFETY: takes no arguments and cannot fail meaningfully.
    let mask = unsafe { GetLogicalDrives() };
    ('A'..='Z').filter(|c| mask & (1 << (*c as u8 - b'A')) != 0).collect()
}

/// First unused letter at or after `from`, wrapping round to `D`. A, B and C
/// are skipped on the wrap: Windows still treats them specially, and handing
/// out `C:` for a data volume invites confusion no user asked for.
pub fn next_free_letter(from: char) -> Option<char> {
    let taken = taken_letters();
    let start = from.to_ascii_uppercase();
    ('A'..='Z')
        .filter(|c| *c >= start)
        .chain('D'..start.min('Z'))
        .find(|c| !taken.contains(c))
}

/// Mount `volume_root` (a `\\?\Volume{…}\` path) at `letter`.
pub fn assign_letter(volume_root: &str, letter: char) -> Result<()> {
    let letter = letter.to_ascii_uppercase();
    let mount = format!("{letter}:\\");
    let mount_w = to_wide(&mount);
    let volume_w = to_wide(volume_root);
    // SAFETY: both buffers are NUL-terminated and outlive the call.
    unsafe { SetVolumeMountPointW(PCWSTR(mount_w.as_ptr()), PCWSTR(volume_w.as_ptr())) }.map_err(
        |e| OwnBackupError::Device(format!("не удалось назначить букву {letter}: для {volume_root}: {e}")),
    )?;
    info!(volume = %volume_root, %letter, "drive letter assigned");
    Ok(())
}

/// A drive letter held only for the duration of an operation.
///
/// Formatting goes through `FormatEx`, which wants a mount point. A partition
/// that has just appeared usually has none — automount is deliberately off on
/// this machine (`san_policy`), and a restored partition should not acquire a
/// letter as a side effect of being formatted. So it gets one, briefly, and
/// gives it back on drop.
pub struct TempDriveLetter {
    letter: Option<char>,
    volume_root: String,
}

impl TempDriveLetter {
    /// Give `volume_root` a mount point if it does not already have one.
    ///
    /// When the volume is already lettered nothing is assigned and nothing will
    /// be removed — [`Self::mount_point`] just returns the existing letter.
    /// Taking away a letter the user already had would be a surprising side
    /// effect of formatting.
    pub fn acquire(volume_root: &str) -> Result<Self> {
        if let Some(existing) = volume_drive_letters(volume_root).first() {
            let letter = existing.chars().next().unwrap_or('C');
            debug!(volume = %volume_root, %letter, "volume already has a letter; reusing it");
            return Ok(Self { letter: None, volume_root: format!("{letter}:\\") });
        }
        let letter = next_free_letter('D').ok_or_else(|| {
            OwnBackupError::Device(
                "нет ни одной свободной буквы диска — форматировать раздел не через что".into(),
            )
        })?;
        assign_letter(volume_root, letter)?;
        Ok(Self { letter: Some(letter), volume_root: format!("{letter}:\\") })
    }

    /// Mount point with the trailing backslash `FormatEx` expects.
    pub fn mount_point(&self) -> &str {
        &self.volume_root
    }
}

impl Drop for TempDriveLetter {
    fn drop(&mut self) {
        let Some(letter) = self.letter else { return };
        let root = format!("{letter}:\\");
        let wide = to_wide(&root);
        // SAFETY: `wide` is NUL-terminated and outlives the call.
        match unsafe { DeleteVolumeMountPointW(PCWSTR(wide.as_ptr())) } {
            Ok(()) => debug!(%letter, "temporary drive letter released"),
            Err(e) => warn!(%letter, error = %e, "could not release the temporary drive letter"),
        }
    }
}

/// Grow the filesystem on `volume_root` to fill its partition (R3e, D2).
///
/// Only NTFS implements `FSCTL_EXTEND_VOLUME`; the caller is expected to have
/// established that (`restore_check` refuses to plan it for anything else).
///
/// `new_sector_count` is the *total* size the volume should end up with, in
/// sectors — not the amount to add. Passing the delta silently shrinks a
/// volume to almost nothing, so the caller is asked for the number it can
/// compute directly from the partition table.
pub fn extend_volume(volume_root: &str, new_sector_count: u64) -> Result<()> {
    let device = volume_root.trim_end_matches('\\');
    let wide = to_wide(device);
    // SAFETY: `wide` outlives the call.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            GENERIC_READ.0 | GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
    .map_err(|e| OwnBackupError::Device(format!("не удалось открыть том {device}: {e}")))?;
    let handle = FileHandle(handle);

    let sectors = new_sector_count as i64;
    let mut returned = 0u32;
    // SAFETY: the input buffer is a live i64 of the documented size.
    unsafe {
        DeviceIoControl(
            handle.0,
            FSCTL_EXTEND_VOLUME,
            Some(&sectors as *const i64 as *const _),
            std::mem::size_of::<i64>() as u32,
            None,
            0,
            Some(&mut returned),
            None,
        )
    }
    .map_err(|e| {
        OwnBackupError::Device(format!(
            "не удалось расширить том {device} до {new_sector_count} секторов: {e}"
        ))
    })?;
    info!(volume = %device, new_sector_count, "filesystem extended into its partition");
    Ok(())
}
