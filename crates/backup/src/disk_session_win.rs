//! A restore session on one physical disk (R3b, R3f).
//!
//! Writing a partition table means writing raw sectors under Windows' feet
//! while it believes it knows what is there. Two things make that safe:
//!
//! 1. **The disk goes offline.** Every volume on it is dismounted at once, no
//!    filesystem driver is left holding an opinion about those sectors, and
//!    there are no races with volumes appearing and disappearing as the table
//!    changes underneath. This is what `diskpart`'s `offline disk` does, and
//!    what the backup path already does for attached images.
//! 2. **It comes back online.** [`DiskSession`] restores the disk's state on
//!    drop, including when the restore fails partway. Leaving a user's disk
//!    offline because an operation errored would turn a failed restore into a
//!    disk that has apparently vanished.
//!
//! When the plan does *not* rewrite the table there is nothing to protect
//! against wholesale, and taking the entire disk offline would be needlessly
//! disruptive — [`crate::volume_win::VolumeDismountGuard`] takes down only the
//! volumes being overwritten (D17).

#![cfg(target_os = "windows")]

use tracing::{info, warn};

use own_backup_common::restore_plan::PartitionTableSpec;
use own_backup_common::{OwnBackupError, Result};

use crate::table_writer::partition_table_write_ops;
use crate::win_device::WinRawDevice;

/// An open handle to a physical disk, with its online/offline state managed.
pub struct DiskSession {
    dev: WinRawDevice,
    disk_number: u32,
    /// Whether *we* took it offline, and therefore owe it an online.
    offline: bool,
}

impl DiskSession {
    /// Open `\\.\PhysicalDrive{disk_number}` for writing.
    pub fn open(disk_number: u32) -> Result<Self> {
        let dev = WinRawDevice::open_write(&format!(r"\\.\PhysicalDrive{disk_number}"))?;
        Ok(Self { dev, disk_number, offline: false })
    }

    pub fn disk_number(&self) -> u32 {
        self.disk_number
    }

    /// Length reported by the device itself — the authority on where the disk
    /// ends, and therefore on where a backup GPT header has to go.
    pub fn length(&self) -> u64 {
        self.dev.length()
    }

    pub fn sector_size(&self) -> u32 {
        self.dev.sector_size()
    }

    /// Take the disk offline, dismounting every volume on it.
    ///
    /// Not persisted: the offline attribute is a property of this session, not
    /// something to leave in the registry keyed to the user's disk.
    pub fn take_offline(&mut self) -> Result<()> {
        if self.offline {
            return Ok(());
        }
        self.dev.set_offline_writable(false)?;
        self.offline = true;
        info!(disk = self.disk_number, "disk taken offline for restore");
        Ok(())
    }

    /// Bring the disk back online and let Windows re-enumerate its volumes.
    pub fn bring_online(&mut self) -> Result<()> {
        if !self.offline {
            return Ok(());
        }
        self.dev.set_online_this_session()?;
        self.offline = false;
        info!(disk = self.disk_number, "disk back online");
        Ok(())
    }

    /// Ask partmgr to re-read the partition table.
    pub fn update_properties(&self) -> Result<()> {
        self.dev.update_properties()
    }

    /// Write `spec` onto the disk.
    ///
    /// The disk must already be offline — writing a table under a mounted
    /// filesystem is refused by Windows, and would be a bad idea even if it
    /// were not.
    ///
    /// The device's own length wins over `spec.disk_size_bytes`. The spec's
    /// figure comes from an inventory query and can be a hair off; a GPT whose
    /// backup header is not at the true last sector is a GPT Windows reports as
    /// damaged.
    pub fn write_partition_table(&mut self, spec: &PartitionTableSpec) -> Result<()> {
        if !self.offline {
            return Err(OwnBackupError::Device(format!(
                "disk {} must be offline before its partition table is rewritten",
                self.disk_number
            )));
        }
        let device_sector = self.dev.sector_size();
        if spec.sector_size != device_sector {
            return Err(OwnBackupError::RestoreIncompatible(format!(
                "план рассчитан на сектор {} Б, а у диска {} — {} Б",
                spec.sector_size, self.disk_number, device_sector
            )));
        }

        let mut spec = spec.clone();
        if spec.disk_size_bytes != self.dev.length() {
            warn!(
                disk = self.disk_number,
                planned = spec.disk_size_bytes,
                actual = self.dev.length(),
                "plan's disk size differs from the device's; using the device's"
            );
            spec.disk_size_bytes = self.dev.length();
        }

        let ops = partition_table_write_ops(&spec)?;
        let total: usize = ops.iter().map(|o| o.bytes.len()).sum();
        for op in &ops {
            self.dev.write_at(op.offset, &op.bytes)?;
        }
        self.dev.flush()?;
        // partmgr must re-read before the disk comes online, or Windows will
        // surface volumes from the table that is no longer there.
        self.dev.update_properties()?;
        info!(
            disk = self.disk_number,
            writes = ops.len(),
            bytes = total,
            partitions = spec.entries.len(),
            "partition table written"
        );
        Ok(())
    }

    /// The underlying device, for callers that need raw access within the
    /// session (wiping free space, verifying what was written).
    pub fn device_mut(&mut self) -> &mut WinRawDevice {
        &mut self.dev
    }
}

impl Drop for DiskSession {
    fn drop(&mut self) {
        if self.offline {
            // Best effort, and loudly logged if it fails: a disk left offline
            // looks to the user like a disk that disappeared.
            if let Err(e) = self.dev.set_online_this_session() {
                warn!(
                    disk = self.disk_number, error = %e,
                    "could not bring the disk back online — it will need to be onlined by hand"
                );
            } else {
                info!(disk = self.disk_number, "disk brought back online on session teardown");
            }
        }
    }
}
