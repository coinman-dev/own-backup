//! The part of the initiator side that is plain logic: which disk a session
//! is.
//!
//! [`crate::iscsi_win`] asks Windows for the devices on a session and then has
//! to decide which `\\.\PhysicalDriveN` is our image. Getting that wrong is the
//! worst thing a mount can do — every later write goes to that disk — so the
//! decision lives here, where it tests on the machine the code is written on.
//!
//! **What went wrong on 2026-09-25.** Right after a login the initiator lists
//! the session's device before PnP has filled it in: the `STORAGE_DEVICE_NUMBER`
//! in that entry is still zeros. Zero is a perfectly good disk number — it was
//! the system SSD — and the only thing refused was `u32::MAX`. The size check
//! caught it before anything was written, but the attach then failed, the
//! session was left behind, and the excluded files stayed in that night's copy.
//! A zeroed entry has device type 0, so the type is what tells a disk from a
//! placeholder.

/// `FILE_DEVICE_DISK` from `winioctl.h`: what a disk reports as the device type
/// of its `STORAGE_DEVICE_NUMBER`. Anything else in a session's device list —
/// or an entry PnP has not filled in yet, whose type is 0 — is not a disk.
pub const FILE_DEVICE_DISK: u32 = 0x0000_0007;

/// One entry of the initiator's device list for a session, reduced to the two
/// numbers that decide whether it names a disk and which one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionDevice {
    pub device_type: u32,
    pub device_number: u32,
}

/// The disk numbers a session's device list names, in its order, each once.
///
/// Only entries that say they are a disk count, and `u32::MAX` ("no number
/// yet") never does. An empty result means "not yet" — the caller keeps
/// polling; it never means disk 0.
pub fn disk_candidates(devices: &[SessionDevice]) -> Vec<u32> {
    let mut out = Vec::new();
    for d in devices {
        if d.device_type == FILE_DEVICE_DISK
            && d.device_number != u32::MAX
            && !out.contains(&d.device_number)
        {
            out.push(d.device_number);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk(n: u32) -> SessionDevice {
        SessionDevice {
            device_type: FILE_DEVICE_DISK,
            device_number: n,
        }
    }

    /// The entry of 2026-09-25: PnP had not filled it in, so everything was
    /// zero — and "disk 0" was the system SSD.
    #[test]
    fn an_entry_pnp_has_not_filled_in_is_not_disk_zero() {
        let unfilled = SessionDevice {
            device_type: 0,
            device_number: 0,
        };
        assert!(disk_candidates(&[unfilled]).is_empty());
    }

    #[test]
    fn a_disk_is_found_once_pnp_has_filled_it_in() {
        let unfilled = SessionDevice {
            device_type: 0,
            device_number: 0,
        };
        assert_eq!(disk_candidates(&[unfilled, disk(5)]), vec![5]);
    }

    /// Disk 0 is a real disk when the entry says so — the type decides, not
    /// the number.
    #[test]
    fn disk_zero_counts_when_the_entry_says_it_is_a_disk() {
        assert_eq!(disk_candidates(&[disk(0)]), vec![0]);
    }

    #[test]
    fn no_number_yet_is_skipped_and_repeats_are_dropped() {
        let pending = SessionDevice {
            device_type: FILE_DEVICE_DISK,
            device_number: u32::MAX,
        };
        // FILE_DEVICE_CD_ROM: a LUN can be one, and it is not our disk.
        let cdrom = SessionDevice {
            device_type: 0x0000_0002,
            device_number: 3,
        };
        assert_eq!(
            disk_candidates(&[pending, cdrom, disk(7), disk(7), disk(8)]),
            vec![7, 8]
        );
    }
}
