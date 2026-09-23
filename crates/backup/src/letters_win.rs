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
use windows::Win32::System::Ioctl::{
    FSCTL_EXTEND_VOLUME, IOCTL_DISK_GET_PARTITION_INFO_EX, PARTITION_INFORMATION_EX,
    PARTITION_STYLE_GPT, PARTITION_STYLE_MBR,
};
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

/// GPT "basic data": the one partition type Windows letters on its own.
const GPT_BASIC_DATA: u128 = 0xebd0a0a2_b9e5_4433_87c0_68b6b72699c7;
const GPT_ATTRIBUTE_HIDDEN: u64 = 0x4000_0000_0000_0000;
const GPT_ATTRIBUTE_NO_DRIVE_LETTER: u64 = 0x8000_0000_0000_0000;
/// MBR types Windows mounts and letters: FAT12, FAT16, FAT16B, NTFS/exFAT,
/// FAT32, FAT32 LBA, FAT16 LBA. Recovery (0x27) and the hidden variants are
/// left out, as Windows leaves them out.
const MBR_DATA_TYPES: [u8; 7] = [0x01, 0x04, 0x06, 0x07, 0x0B, 0x0C, 0x0E];

/// The partition a volume sits on, as much as deciding on a letter needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PartitionOfVolume {
    Gpt { basic_data: bool, attributes: u64 },
    Mbr { kind: u8 },
}

/// Would Windows give this partition a letter the first time it saw it? Data
/// partitions yes; EFI, MSR, recovery and anything marked hidden or "no drive
/// letter", no.
fn letters_like_windows(p: PartitionOfVolume) -> bool {
    match p {
        PartitionOfVolume::Gpt { basic_data, attributes } => {
            basic_data && attributes & (GPT_ATTRIBUTE_HIDDEN | GPT_ATTRIBUTE_NO_DRIVE_LETTER) == 0
        }
        PartitionOfVolume::Mbr { kind } => MBR_DATA_TYPES.contains(&kind),
    }
}

fn partition_of_volume(volume_root: &str) -> Option<PartitionOfVolume> {
    let device = volume_root.trim_end_matches('\\');
    let wide = to_wide(device);
    // No access rights: the partition query is FILE_ANY_ACCESS, and asking for
    // more is refused on a read-only image.
    // SAFETY: `wide` outlives the call.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
    .ok()?;
    let handle = FileHandle(handle);
    let mut info = PARTITION_INFORMATION_EX::default();
    let mut returned = 0u32;
    // SAFETY: the output buffer is a live PARTITION_INFORMATION_EX of the size passed.
    unsafe {
        DeviceIoControl(
            handle.0,
            IOCTL_DISK_GET_PARTITION_INFO_EX,
            None,
            0,
            Some(&mut info as *mut _ as *mut _),
            std::mem::size_of::<PARTITION_INFORMATION_EX>() as u32,
            Some(&mut returned),
            None,
        )
    }
    .ok()?;
    // SAFETY: the style says which half of the union the driver filled in.
    unsafe {
        if info.PartitionStyle == PARTITION_STYLE_GPT {
            let gpt = info.Anonymous.Gpt;
            Some(PartitionOfVolume::Gpt {
                basic_data: gpt.PartitionType == windows::core::GUID::from_u128(GPT_BASIC_DATA),
                attributes: gpt.Attributes.0,
            })
        } else if info.PartitionStyle == PARTITION_STYLE_MBR {
            Some(PartitionOfVolume::Mbr { kind: info.Anonymous.Mbr.PartitionType })
        } else {
            None
        }
    }
}

/// Give every data volume on `disk_number` a drive letter it does not have.
/// Returns the letters now on the disk.
///
/// For a mounted backup image, whose card promises «тома внутри получают
/// буквы». Windows does not keep that promise by itself: it letters a volume
/// only the FIRST time it sees it. A volume it has met before and that had its
/// letter taken away counts as "the user wants no letter" — and every backup
/// image met before is exactly that, because the exclusion pass mounts it and
/// strips its letters so it does not surface in Explorer mid-backup
/// ([`crate::volume_win::strip_letters_on_disk`]). The `.obk` mounted on
/// 2026-09-23 came up whole, healthy, and with no letter.
///
/// Waits for the volumes to arrive first: the attach returns before PnP has
/// built them.
pub fn letter_data_volumes_on_disk(disk_number: u32) -> Vec<String> {
    const ARRIVAL: std::time::Duration = std::time::Duration::from_secs(10);
    const STABLE: std::time::Duration = std::time::Duration::from_millis(1500);

    let started = std::time::Instant::now();
    let mut seen = crate::volume_win::all_volume_roots_on_disk(disk_number);
    let mut stable_since = std::time::Instant::now();
    while started.elapsed() < ARRIVAL {
        std::thread::sleep(std::time::Duration::from_millis(250));
        let now = crate::volume_win::all_volume_roots_on_disk(disk_number);
        if now != seen {
            seen = now;
            stable_since = std::time::Instant::now();
        } else if !seen.is_empty() && stable_since.elapsed() >= STABLE {
            break;
        }
    }

    for volume in &seen {
        if !volume_drive_letters(volume).is_empty() {
            continue;
        }
        let Some(partition) = partition_of_volume(volume) else {
            debug!(%volume, "no partition information — left without a letter");
            continue;
        };
        if !letters_like_windows(partition) {
            debug!(%volume, ?partition, "not a data partition — left without a letter, as Windows would");
            continue;
        }
        let Some(letter) = next_free_letter('D') else {
            warn!(%volume, "no free drive letter for a volume of the mounted image");
            break;
        };
        if let Err(e) = assign_letter(volume, letter) {
            warn!(%volume, %letter, error = %e, "could not letter a volume of the mounted image");
        }
    }
    crate::volume_win::drive_letters_on_disk(disk_number)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// What a Windows disk image has, and what Windows letters of it.
    #[test]
    fn only_data_partitions_get_a_letter() {
        let gpt = |basic_data, attributes| PartitionOfVolume::Gpt { basic_data, attributes };
        assert!(letters_like_windows(gpt(true, 0)), "C: of the image");
        assert!(!letters_like_windows(gpt(false, 0)), "EFI, MSR, recovery by type");
        assert!(!letters_like_windows(gpt(true, GPT_ATTRIBUTE_HIDDEN)));
        assert!(!letters_like_windows(gpt(true, GPT_ATTRIBUTE_NO_DRIVE_LETTER)));
        assert!(letters_like_windows(PartitionOfVolume::Mbr { kind: 0x07 }));
        assert!(!letters_like_windows(PartitionOfVolume::Mbr { kind: 0x27 }), "WinRE on MBR");
    }
}
