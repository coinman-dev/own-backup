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
//!
//! ## The disk Windows is running from
//!
//! It cannot go offline — Windows is on it — so the whole scheme above does not
//! apply. Writing the GPT as raw sectors would still *succeed* there (those
//! sectors belong to no volume), and would be a bad idea: the partition manager
//! keeps its own picture of the layout, and a partition deleted behind its back
//! leaves a mounted volume pointing at sectors that now belong to something
//! else.
//!
//! For that disk the layout goes through `IOCTL_DISK_SET_DRIVE_LAYOUT_EX` — the
//! same call Disk Management and `diskpart` use. partmgr validates the new
//! layout, tears down the volumes that disappeared, creates the ones that
//! appeared, and tells the rest of the system. It also refuses changes that
//! would pull the ground out from under the running OS, which is exactly the
//! second opinion worth having.
//!
//! Its second opinion is not a substitute for not asking, though. partmgr acts
//! on the layout it is handed, and a layout that quietly renames the partitions
//! it was supposed to leave alone is a layout it will apply. So before anything
//! is written the spec is reconciled with the table already on the disk
//! ([`crate::table_merge`]): entries that describe partitions that are already
//! there, unmoved, take their identity back from the disk, and a table that
//! ends up identical to the disk's is not written at all.

#![cfg(target_os = "windows")]

use tracing::{debug, info, warn};
use windows::Win32::System::Ioctl::{
    IOCTL_DISK_SET_DRIVE_LAYOUT_EX, PARTITION_STYLE_GPT, PARTITION_STYLE_MBR,
};
use windows::Win32::System::IO::DeviceIoControl;

use own_backup_common::gpt::DiskLayout;
use own_backup_common::restore_plan::{PartitionKind, PartitionScheme, PartitionTableSpec};
use own_backup_common::{OwnBackupError, Result};

use crate::table_merge::{
    adopt_existing_entries, describe_disk, describe_table, entry_unchanged, same_layout,
};
use crate::table_writer::partition_table_write_ops;
use crate::win_device::WinRawDevice;

/// Parse a GUID that may or may not be wrapped in braces. An empty or unusable
/// string yields `None`, and callers generate a fresh value.
fn parse_guid(s: &str) -> Option<uuid::Uuid> {
    let t = s.trim().trim_start_matches('{').trim_end_matches('}');
    if t.is_empty() {
        return None;
    }
    uuid::Uuid::parse_str(t).ok()
}

/// An open handle to a physical disk, with its online/offline state managed.
pub struct DiskSession {
    dev: WinRawDevice,
    disk_number: u32,
    /// Whether *we* took it offline, and therefore owe it an online.
    offline: bool,
    /// The running Windows is on this disk. It cannot go offline, and its
    /// layout must be applied through the partition manager.
    live_system: bool,
}

impl DiskSession {
    /// Open `\\.\PhysicalDrive{disk_number}` for writing.
    ///
    /// `live_system` marks the disk the running Windows boots from: it cannot be
    /// taken offline, and its layout is applied through the partition manager
    /// instead of being written as raw sectors.
    pub fn open(disk_number: u32, live_system: bool) -> Result<Self> {
        let dev = WinRawDevice::open_write(&format!(r"\\.\PhysicalDrive{disk_number}"))?;
        Ok(Self { dev, disk_number, offline: false, live_system })
    }

    /// Whether this disk hosts the running Windows.
    pub fn is_live_system(&self) -> bool {
        self.live_system
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
        if self.live_system {
            // Not an error: the caller asks for isolation, and on this disk the
            // isolation comes from partmgr instead. Refusing here would block a
            // restore that is perfectly safe.
            debug!(disk = self.disk_number, "live system disk stays online — layout goes through partmgr");
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
    ///
    /// Returns whether anything was actually written: a table identical to the
    /// one on the disk is left alone, and the caller says so rather than
    /// claiming a rewrite that did not happen.
    ///
    /// `owned` names the target partitions the plan acts on. Every other entry
    /// is a bystander whose GPT identity comes from the disk — see
    /// [`crate::table_merge::adopt_existing_entries`].
    pub fn write_partition_table(
        &mut self,
        spec: &PartitionTableSpec,
        owned: &[u32],
    ) -> Result<bool> {
        if !self.offline && !self.live_system {
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

        // What is on the disk right now — the authority on the identities the
        // plan does not carry, and on whether the plan changes anything at all.
        let current = self.current_layout();
        if let Some(cur) = &current {
            for line in describe_disk(cur) {
                info!(disk = self.disk_number, "on disk now: {line}");
            }
            let adopted = adopt_existing_entries(&mut spec, cur, owned);
            if !adopted.is_empty() {
                info!(
                    disk = self.disk_number,
                    disk_id = adopted.kept_disk_id,
                    unique_guid = ?adopted.kept_guid,
                    attributes = ?adopted.kept_attributes,
                    name = ?adopted.kept_name,
                    "kept the on-disk GPT identity of partitions the plan leaves in place"
                );
            }
            if same_layout(&spec, cur) {
                info!(
                    disk = self.disk_number,
                    partitions = spec.entries.len(),
                    "the planned layout is the one already on the disk — partition table left \
                     untouched"
                );
                return Ok(false);
            }
        }
        info!(
            disk = self.disk_number,
            scheme = ?spec.scheme,
            disk_id = %spec.disk_id,
            sector = spec.sector_size,
            size = spec.disk_size_bytes,
            partitions = spec.entries.len(),
            "writing a new partition table"
        );
        for line in describe_table(&spec) {
            info!(disk = self.disk_number, "to write: {line}");
        }

        if self.live_system {
            self.set_drive_layout(&spec, current.as_ref())?;
            return Ok(true);
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
        Ok(true)
    }

    /// The GPT that is on the disk right now, read from its own sectors.
    ///
    /// Read rather than asked for: the partition manager's picture is what the
    /// plan is about to replace, while the sectors are what the identities have
    /// to come from. A disk with no readable GPT (a brand-new one, an MBR disk)
    /// yields `None`, and the caller then has nothing to reconcile against —
    /// which is correct, since there is nothing there to preserve.
    fn current_layout(&self) -> Option<DiskLayout> {
        match crate::gpt_reader::read_disk_layout(self.disk_number) {
            Ok(layout) => Some(layout),
            // An empty target is the ordinary case for a restore, not a
            // problem: there is nothing on it to reconcile against. Saying so
            // at WARN, in words about whole-disk BACKUP, had a clean run
            // reporting trouble it did not have (2026-09-20).
            Err(e) if e.to_string().contains("no GPT") => {
                debug!(
                    disk = self.disk_number,
                    "no partition table on the target — nothing to reconcile, writing the planned one"
                );
                None
            }
            Err(e) => {
                warn!(
                    disk = self.disk_number, error = %e,
                    "could not read the partition table currently on the disk — writing the \
                     planned one as it stands"
                );
                None
            }
        }
    }

    /// Apply `spec` through the partition manager (`IOCTL_DISK_SET_DRIVE_LAYOUT_EX`).
    ///
    /// This is how Disk Management and `diskpart` change a live disk. partmgr
    /// validates the layout, dismounts the volumes that disappeared, surfaces
    /// the ones that appeared, and refuses anything that would pull the ground
    /// out from under the running system — which is a second opinion worth
    /// having on the one disk where a mistake stops the machine.
    ///
    /// The structures are built by hand rather than through the `windows`
    /// crate's types: `DRIVE_LAYOUT_INFORMATION_EX` ends in a variable-length
    /// array, so it has to be laid out in a byte buffer anyway.
    ///
    /// `current` is the table on the disk, when it could be read. Entries that
    /// match it byte for byte are handed over with `RewritePartition = FALSE`,
    /// so partmgr leaves those partitions — and the volumes mounted on them —
    /// alone. On the disk the running Windows boots from, one of those volumes
    /// is `C:`.
    fn set_drive_layout(
        &mut self,
        spec: &PartitionTableSpec,
        current: Option<&DiskLayout>,
    ) -> Result<()> {
        // DRIVE_LAYOUT_INFORMATION_EX: u32 PartitionStyle, u32 PartitionCount,
        // union { GPT { Guid DiskId; i64 StartingUsableOffset; i64 UsableLength;
        //              u32 MaxPartitionCount } | MBR { u32 Signature; u32 Checksum } }
        // padded to 8, then PARTITION_INFORMATION_EX[].
        const HEADER: usize = 48;
        const ENTRY: usize = 144;
        // Offset of the style-specific union inside PARTITION_INFORMATION_EX.
        //
        // It follows `IsServicePartition` at +29, padded up to the union's own
        // 8-byte alignment — so +32, and the GPT arm's 112 bytes then fill the
        // record exactly (32 + 112 = 144 = ENTRY). This was +40 once, which put
        // a zeroed half-GUID where partmgr reads the partition type, the tail of
        // the type GUID where it reads the unique ID, and half of *that* where
        // it reads the attribute flags. Applied to a live system disk it made
        // Windows stop recognising its own boot volume: bugcheck immediately,
        // and an unbootable disk afterwards.
        const UNION: usize = 32;
        // PARTITION_INFORMATION_GPT is 112 bytes (two GUIDs, u64 attributes,
        // 36 UTF-16 units) and is the larger arm of the union, so it has to end
        // exactly where the record does. Checked here because the symptom of
        // getting it wrong is a bugcheck on the user's machine, not a test
        // failure on ours.
        const _: () = assert!(UNION + 16 + 16 + 8 + 72 == ENTRY);
        // Same for the header: DiskId at +8, then two i64 and MaxPartitionCount,
        // padded up to the first entry.
        const _: () = assert!(HEADER == 48);
        let sector = spec.sector_size.max(512) as u64;
        let count = spec.entries.len();
        let mut buf = vec![0u8; HEADER + ENTRY * count.max(1)];

        let style = match spec.scheme {
            PartitionScheme::Gpt => PARTITION_STYLE_GPT.0 as u32,
            PartitionScheme::Mbr => PARTITION_STYLE_MBR.0 as u32,
        };
        buf[0..4].copy_from_slice(&style.to_le_bytes());
        buf[4..8].copy_from_slice(&(count as u32).to_le_bytes());

        match spec.scheme {
            PartitionScheme::Gpt => {
                let disk_guid = parse_guid(&spec.disk_id).unwrap_or_else(uuid::Uuid::new_v4);
                buf[8..24].copy_from_slice(&disk_guid.to_bytes_le());
                // Usable range: past the GPT head, up to the start of its mirror.
                let total = self.dev.length() / sector;
                let first_usable = 34u64 * sector;
                let last_usable = total.saturating_sub(33) * sector;
                buf[24..32].copy_from_slice(&(first_usable as i64).to_le_bytes());
                buf[32..40]
                    .copy_from_slice(&((last_usable.saturating_sub(first_usable)) as i64).to_le_bytes());
                buf[40..44].copy_from_slice(&128u32.to_le_bytes());
            }
            PartitionScheme::Mbr => {
                let signature = u32::from_str_radix(spec.disk_id.trim(), 16)
                    .unwrap_or_else(|_| uuid::Uuid::new_v4().as_u128() as u32 | 1);
                buf[8..12].copy_from_slice(&signature.to_le_bytes());
            }
        }

        let mut rewritten: Vec<u32> = Vec::new();
        let mut untouched: Vec<u32> = Vec::new();
        for (i, e) in spec.entries.iter().enumerate() {
            // PARTITION_INFORMATION_EX: u32 PartitionStyle, pad, i64 StartingOffset,
            // i64 PartitionLength, u32 PartitionNumber, u8 RewritePartition,
            // u8 IsServicePartition, pad, then the style-specific union at UNION.
            let b = HEADER + i * ENTRY;
            buf[b..b + 4].copy_from_slice(&style.to_le_bytes());
            buf[b + 8..b + 16].copy_from_slice(&(e.offset_bytes(spec.sector_size) as i64).to_le_bytes());
            buf[b + 16..b + 24]
                .copy_from_slice(&(e.size_bytes(spec.sector_size) as i64).to_le_bytes());
            buf[b + 24..b + 28].copy_from_slice(&e.index.to_le_bytes());
            // Rewrite only what actually differs. partmgr applies an entry
            // marked TRUE and leaves a FALSE one exactly as it found it, so
            // this is the difference between "restore into the spare partition"
            // and "recreate every partition on the disk, including the one the
            // OS is running from". With no readable current layout there is
            // nothing to compare against and every entry has to be written.
            let unchanged = current.is_some_and(|cur| entry_unchanged(e, cur));
            buf[b + 28] = u8::from(!unchanged);
            if unchanged {
                untouched.push(e.index);
            } else {
                rewritten.push(e.index);
            }

            let u = b + UNION;
            match &e.kind {
                PartitionKind::Gpt { type_guid, part_guid, attributes, name } => {
                    // PARTITION_INFORMATION_GPT: Guid PartitionType, Guid PartitionId,
                    // u64 Attributes, u16 Name[36].
                    let ty = parse_guid(type_guid).ok_or_else(|| {
                        OwnBackupError::ImageFormat(format!("bad type GUID {type_guid:?}"))
                    })?;
                    let id = parse_guid(part_guid).unwrap_or_else(uuid::Uuid::new_v4);
                    buf[u..u + 16].copy_from_slice(&ty.to_bytes_le());
                    buf[u + 16..u + 32].copy_from_slice(&id.to_bytes_le());
                    buf[u + 32..u + 40].copy_from_slice(&attributes.to_le_bytes());
                    for (k, unit) in name.encode_utf16().take(36).enumerate() {
                        let o = u + 40 + k * 2;
                        buf[o..o + 2].copy_from_slice(&unit.to_le_bytes());
                    }
                }
                PartitionKind::Mbr { type_byte, bootable, .. } => {
                    // PARTITION_INFORMATION_MBR: u8 PartitionType, u8 BootIndicator,
                    // u8 RecognizedPartition, u8 pad, u32 HiddenSectors, …
                    buf[u] = *type_byte;
                    buf[u + 1] = *bootable as u8;
                    buf[u + 2] = 1; // recognized
                }
            }
        }

        info!(
            disk = self.disk_number,
            partitions = count,
            rewrite = ?rewritten,
            keep = ?untouched,
            bytes = buf.len(),
            "handing the layout to the partition manager"
        );

        // Take down what lives on the sectors this change disturbs, and hold it
        // down until partmgr has applied the new layout.
        //
        // Skipping this is what leaves a *ghost*: delete a partition whose
        // volume is still mounted and mountmgr keeps the volume object, with
        // its old extents, over sectors that now belong to a new partition.
        // Nothing can lock a volume whose partition is gone — it answers
        // "устройство не готово" forever — and Windows still refuses raw writes
        // to everything it claims. The restore then fails inside the new
        // partition with "отказано в доступе", which names neither the ghost
        // nor the deletion that created it.
        let _guards = self.dismount_disturbed_ranges(spec, current);

        let mut returned = 0u32;
        // SAFETY: the input buffer is laid out as DRIVE_LAYOUT_INFORMATION_EX
        // followed by `count` PARTITION_INFORMATION_EX records, and its length
        // is passed alongside it.
        unsafe {
            DeviceIoControl(
                self.dev.handle(),
                IOCTL_DISK_SET_DRIVE_LAYOUT_EX,
                Some(buf.as_ptr() as *const _),
                buf.len() as u32,
                None,
                0,
                Some(&mut returned),
                None,
            )
        }
        .map_err(|e| {
            OwnBackupError::Device(format!(
                "диспетчер разделов отклонил новую разметку диска {}: {e}. \
                 Обычно это значит, что один из затронутых томов занят — закройте \
                 открытые в нём файлы и повторите.",
                self.disk_number
            ))
        })?;
        self.dev.update_properties()?;
        info!(
            disk = self.disk_number,
            partitions = count,
            rewritten = rewritten.len(),
            "partition table applied through the partition manager (live system disk)"
        );
        Ok(())
    }

    /// Dismount every volume sitting on a stretch of disk this layout change
    /// disturbs, and keep it down for as long as the returned guards live.
    ///
    /// Two kinds of stretch qualify, and only these two:
    ///
    /// - a partition on the disk today that the new layout does not keep in
    ///   place — deleted outright, or moved/resized;
    /// - a partition in the new layout that is not already there — the sectors
    ///   it will occupy may still be carrying somebody else's volume.
    ///
    /// Partitions that come through the change untouched are deliberately left
    /// alone. On the disk the running Windows boots from, one of them is `C:`,
    /// and it neither can nor should be dismounted.
    fn dismount_disturbed_ranges(
        &self,
        spec: &PartitionTableSpec,
        current: Option<&DiskLayout>,
    ) -> Vec<crate::volume_win::VolumeDismountGuard> {
        let Some(cur) = current else { return Vec::new() };

        let mut ranges: Vec<(u64, u64)> = Vec::new();
        for p in &cur.partitions {
            let survives = spec
                .entries
                .iter()
                .any(|e| e.first_lba == p.first_lba && e.last_lba == p.last_lba);
            if !survives {
                let cur_sector = cur.sector_size.max(512) as u64;
                ranges.push((
                    p.first_lba * cur_sector,
                    (p.last_lba + 1 - p.first_lba) * cur_sector,
                ));
            }
        }
        for e in &spec.entries {
            let existed = cur
                .partitions
                .iter()
                .any(|p| p.first_lba == e.first_lba && p.last_lba == e.last_lba);
            if !existed {
                ranges.push((e.offset_bytes(spec.sector_size), e.size_bytes(spec.sector_size)));
            }
        }
        if ranges.is_empty() {
            return Vec::new();
        }
        info!(disk = self.disk_number, ?ranges, "dismounting what stands on the sectors being changed");
        ranges
            .into_iter()
            .map(|(offset, len)| {
                crate::volume_win::VolumeDismountGuard::for_restore_range(
                    self.disk_number,
                    offset,
                    len,
                )
            })
            .collect()
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
