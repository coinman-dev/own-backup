//! Writing the Linux boot medium's ISO onto a USB stick: the decisions, apart
//! from the device I/O (`media_stick_linux`), so they can be tested anywhere.
//!
//! The ISO is written byte for byte, as `dd` would: an isohybrid image carries
//! its own MBR (BIOS boot code and a partition table that points UEFI firmware
//! at the EFI image inside it), so a copy of it boots the way the ISO itself
//! boots in a virtual machine — which is what the stand checks. Nothing in the
//! image is rearranged.
//!
//! What is added is one partition in the free space behind the image, FAT32
//! labelled [`LOG_LABEL`]: the medium is read-only ISO 9660, and a rescue that
//! goes wrong must leave its log somewhere that survives the power-off. The
//! medium mounts that label at boot and points the engine's log copy at it
//! (`packaging/linux/media`). Debian's live ISOs (live-build, xorriso) carry a
//! plain MBR with a free slot — the image partition (type 0x17, or 0x00 under
//! `-isohybrid-gpt-basdat`) and the EFI image (0xEF) — so the new entry goes
//! into the first empty slot and nothing else in LBA 0 changes. A protective
//! MBR (a real GPT), an extended partition or a full table is left alone: the
//! stick still boots, and the report says the log stays in memory.

use own_backup_common::{tr, OwnBackupError, Result};

/// Volume label of the log partition; the medium looks for exactly this one.
/// FAT labels are at most 11 characters.
pub const LOG_LABEL: &str = "OBK-LOGS";

/// The log partition starts on this boundary (1 MiB, as partitioning tools do).
const ALIGN_BYTES: u64 = 1 << 20;

/// Smaller than this, FAT32 cannot be made (65525 clusters at 512 bytes is
/// 32 MiB) and a log partition is not worth the slot.
pub const MIN_LOG_BYTES: u64 = 64 << 20;

/// MBR fields are 32-bit sector counts.
const MBR_MAX_SECTORS: u64 = u32::MAX as u64;

/// FAT32 with LBA addressing.
const FAT32_LBA: u8 = 0x0C;

/// ISO 9660's primary volume descriptor: sector 16 of 2048 bytes.
const PVD_OFFSET: usize = 16 * 2048;

/// How many bytes of the image's start [`inspect_iso`] needs.
pub const ISO_HEAD_BYTES: usize = PVD_OFFSET + 2048;

/// What the ISO says about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsoInfo {
    /// The volume identifier (`OWNBACKUP` for ours), trimmed.
    pub volume_id: String,
    pub bytes: u64,
}

/// Is `head` (the first [`ISO_HEAD_BYTES`] of a `len`-byte file) an ISO that
/// boots from a stick? A CD-only ISO has no MBR: written to a stick it is a
/// disk no firmware starts from, so it is refused before anything is erased.
pub fn inspect_iso(head: &[u8], len: u64) -> Result<IsoInfo> {
    let pvd = head.get(PVD_OFFSET..PVD_OFFSET + 2048);
    let Some(pvd) = pvd.filter(|p| p[0] == 1 && &p[1..6] == b"CD001") else {
        return Err(OwnBackupError::ImageFormat(
            tr!("media_stick.not_an_iso").into_owned(),
        ));
    };
    if head[510..512] != [0x55, 0xAA] {
        return Err(OwnBackupError::ImageFormat(
            tr!("media_stick.not_hybrid").into_owned(),
        ));
    }
    if !len.is_multiple_of(512) {
        return Err(OwnBackupError::ImageFormat(
            tr!("media_stick.odd_length", bytes = len).into_owned(),
        ));
    }
    let volume_id = String::from_utf8_lossy(&pvd[40..72]).trim().to_string();
    Ok(IsoInfo {
        volume_id,
        bytes: len,
    })
}

/// One of the four MBR slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MbrSlot {
    pub kind: u8,
    pub first_lba: u32,
    pub sectors: u32,
}

impl MbrSlot {
    fn is_empty(&self) -> bool {
        self.kind == 0 && self.first_lba == 0 && self.sectors == 0
    }

    fn end_lba(&self) -> u64 {
        self.first_lba as u64 + self.sectors as u64
    }
}

/// The four primary slots of `lba0`.
pub fn mbr_slots(lba0: &[u8]) -> [MbrSlot; 4] {
    std::array::from_fn(|i| {
        let e = &lba0[446 + 16 * i..462 + 16 * i];
        MbrSlot {
            kind: e[4],
            first_lba: u32::from_le_bytes([e[8], e[9], e[10], e[11]]),
            sectors: u32::from_le_bytes([e[12], e[13], e[14], e[15]]),
        }
    })
}

/// What can be done about the log partition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogPartitionPlan {
    /// Write `lba0` (the image's own LBA 0 with one slot filled) and the
    /// kernel then shows `partitions`: (number, first LBA, sectors), in
    /// 512-byte units, every non-empty slot as Linux registers it.
    Add {
        slot: usize,
        first_lba: u64,
        sectors: u64,
        lba0: Vec<u8>,
        partitions: Vec<(u32, u64, u64)>,
    },
    /// Not done; the reason is for the report, the stick boots regardless.
    Skip(String),
}

/// Plan the log partition behind an image of `image_bytes` on a disk of
/// `disk_bytes` with logical sectors of `sector` bytes, from the image's own
/// LBA 0 as written.
pub fn plan_log_partition(
    lba0: &[u8],
    image_bytes: u64,
    disk_bytes: u64,
    sector: u32,
) -> LogPartitionPlan {
    if sector != 512 {
        // An isohybrid MBR counts in 512-byte sectors; on a 4Kn device its own
        // entries are already wrong, and a fifth guess would not help.
        return LogPartitionPlan::Skip(tr!("media_stick.log_sector_size", bytes = sector).into());
    }
    if lba0.len() < 512 || lba0[510..512] != [0x55, 0xAA] {
        return LogPartitionPlan::Skip(tr!("media_stick.log_no_mbr").into());
    }
    let slots = mbr_slots(lba0);
    if slots.iter().any(|s| s.kind == 0xEE) {
        return LogPartitionPlan::Skip(tr!("media_stick.log_gpt").into());
    }
    if slots.iter().any(|s| matches!(s.kind, 0x05 | 0x0F | 0x85)) {
        return LogPartitionPlan::Skip(tr!("media_stick.log_extended").into());
    }
    let Some(slot) = slots.iter().position(MbrSlot::is_empty) else {
        return LogPartitionPlan::Skip(tr!("media_stick.log_no_slot").into());
    };
    // Behind the image and behind everything the table already describes.
    let used_end = slots
        .iter()
        .filter(|s| !s.is_empty())
        .map(MbrSlot::end_lba)
        .fold(image_bytes.div_ceil(512), u64::max);
    let align = ALIGN_BYTES / 512;
    let first = used_end.div_ceil(align) * align;
    let disk_sectors = (disk_bytes / 512).min(MBR_MAX_SECTORS);
    let sectors = disk_sectors.saturating_sub(first);
    if sectors * 512 < MIN_LOG_BYTES || first > MBR_MAX_SECTORS {
        return LogPartitionPlan::Skip(
            tr!(
                "media_stick.log_no_room",
                free_mib = (sectors * 512) >> 20,
                need_mib = MIN_LOG_BYTES >> 20
            )
            .into(),
        );
    }
    let mut new = lba0[..512].to_vec();
    let e = &mut new[446 + 16 * slot..462 + 16 * slot];
    // Not the active partition: the image's stays the one. CHS fields past
    // the 8 GiB they can address: "use the LBA fields".
    e[0] = 0x00;
    e[1..4].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
    e[4] = FAT32_LBA;
    e[5..8].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
    e[8..12].copy_from_slice(&(first as u32).to_le_bytes());
    e[12..16].copy_from_slice(&(sectors as u32).to_le_bytes());
    let partitions = mbr_slots(&new)
        .iter()
        .enumerate()
        .filter(|(_, s)| s.sectors != 0)
        .map(|(i, s)| (i as u32 + 1, s.first_lba as u64, s.sectors as u64))
        .collect();
    LogPartitionPlan::Add {
        slot,
        first_lba: first,
        sectors,
        lba0: new,
        partitions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DISK: u64 = 8 << 30;

    fn slot(lba0: &mut [u8], i: usize, boot: u8, kind: u8, first: u32, count: u32) {
        let e = &mut lba0[446 + 16 * i..462 + 16 * i];
        e[0] = boot;
        e[4] = kind;
        e[8..12].copy_from_slice(&first.to_le_bytes());
        e[12..16].copy_from_slice(&count.to_le_bytes());
    }

    fn signed() -> Vec<u8> {
        let mut lba0 = vec![0u8; 512];
        lba0[..4].copy_from_slice(&[0x33, 0xED, 0xFA, 0x8E]); // boot code stays
        lba0[440..444].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        lba0[510] = 0x55;
        lba0[511] = 0xAA;
        lba0
    }

    /// LBA 0 as xorriso writes it for live-build: `-isohybrid-mbr
    /// -partition_offset 16`, the EFI image appended as partition 2 (dumped
    /// from a real 12.9 MB image, 2026-10-20).
    fn live_build_like() -> (Vec<u8>, u64) {
        let mut lba0 = signed();
        slot(&mut lba0, 0, 0x80, 0x17, 64, 16320);
        slot(&mut lba0, 1, 0x00, 0xEF, 16384, 8192);
        (lba0, 12_890_112)
    }

    #[test]
    fn the_log_partition_goes_behind_the_image_into_the_first_free_slot() {
        let (lba0, image) = live_build_like();
        let LogPartitionPlan::Add {
            slot,
            first_lba,
            sectors,
            lba0: new,
            partitions,
        } = plan_log_partition(&lba0, image, DISK, 512)
        else {
            panic!("expected a plan");
        };
        assert_eq!(slot, 2);
        // 12.9 MB image, 1 MiB aligned: LBA 26624 (13 MiB).
        assert_eq!(first_lba, 26624);
        assert_eq!(sectors, DISK / 512 - 26624);
        // Only the 16 bytes of slot 3 changed: boot code, signature, the
        // image's own entries and the boot flag are the image's.
        let changed: Vec<usize> = (0..512).filter(|&i| lba0[i] != new[i]).collect();
        assert!(
            changed.iter().all(|&i| (478..494).contains(&i)),
            "{changed:?}"
        );
        let s = mbr_slots(&new)[2];
        assert_eq!((s.kind, s.first_lba as u64), (0x0C, 26624));
        assert_eq!(new[478], 0, "the log partition is not the active one");
        assert_eq!(
            partitions,
            vec![
                (1, 64, 16320),
                (2, 16384, 8192),
                (3, 26624, DISK / 512 - 26624)
            ]
        );
    }

    #[test]
    fn a_type_zero_entry_with_extents_is_not_a_free_slot() {
        // `-isohybrid-gpt-basdat`: slot 1 is type 0x00 but describes the image.
        let mut lba0 = signed();
        slot(&mut lba0, 0, 0x80, 0x00, 64, 16320);
        slot(&mut lba0, 1, 0x00, 0xEF, 268, 8192);
        let LogPartitionPlan::Add {
            slot, partitions, ..
        } = plan_log_partition(&lba0, 8_388_608, DISK, 512)
        else {
            panic!("expected a plan");
        };
        assert_eq!(slot, 2);
        assert_eq!(partitions.len(), 3, "Linux registers the type-0 slot too");
    }

    #[test]
    fn the_table_decides_where_free_space_starts_not_only_the_file_length() {
        // A partition reaching past the file's end (an appended image the
        // file was cut short of) must not be overlapped.
        let mut lba0 = signed();
        slot(&mut lba0, 0, 0x80, 0x17, 0, 100_000);
        let LogPartitionPlan::Add { first_lba, .. } = plan_log_partition(&lba0, 1 << 20, DISK, 512)
        else {
            panic!("expected a plan");
        };
        assert_eq!(first_lba, 100_352);
    }

    #[test]
    fn what_is_not_a_plain_mbr_with_room_is_left_alone() {
        let skip = |p: LogPartitionPlan| matches!(p, LogPartitionPlan::Skip(_));
        let (lba0, image) = live_build_like();
        // A stick barely larger than the image.
        assert!(skip(plan_log_partition(
            &lba0,
            image,
            image + (32 << 20),
            512
        )));
        assert!(skip(plan_log_partition(&lba0, image, DISK, 4096)));
        let mut gpt = signed();
        slot(&mut gpt, 0, 0x00, 0xEE, 1, u32::MAX);
        assert!(skip(plan_log_partition(&gpt, image, DISK, 512)));
        let mut ext = lba0.clone();
        slot(&mut ext, 2, 0x00, 0x0F, 30_000, 1000);
        assert!(skip(plan_log_partition(&ext, image, DISK, 512)));
        let mut full = lba0.clone();
        slot(&mut full, 2, 0x00, 0x83, 30_000, 1000);
        slot(&mut full, 3, 0x00, 0x83, 40_000, 1000);
        assert!(skip(plan_log_partition(&full, image, DISK, 512)));
        let mut unsigned = lba0.clone();
        unsigned[511] = 0;
        assert!(skip(plan_log_partition(&unsigned, image, DISK, 512)));
    }

    #[test]
    fn past_two_tib_the_partition_stops_where_mbr_can_count() {
        let (lba0, image) = live_build_like();
        let LogPartitionPlan::Add {
            first_lba, sectors, ..
        } = plan_log_partition(&lba0, image, 4 << 40, 512)
        else {
            panic!("expected a plan");
        };
        assert_eq!(first_lba + sectors, u32::MAX as u64);
    }

    fn iso_head(volume: &str, mbr: bool) -> Vec<u8> {
        let mut head = vec![0u8; ISO_HEAD_BYTES];
        if mbr {
            head[510] = 0x55;
            head[511] = 0xAA;
        }
        head[PVD_OFFSET] = 1;
        head[PVD_OFFSET + 1..PVD_OFFSET + 6].copy_from_slice(b"CD001");
        let id = &mut head[PVD_OFFSET + 40..PVD_OFFSET + 72];
        id.fill(b' ');
        id[..volume.len()].copy_from_slice(volume.as_bytes());
        head
    }

    #[test]
    fn only_an_isohybrid_image_is_written() {
        let ok = inspect_iso(&iso_head("OWNBACKUP", true), 1 << 30).unwrap();
        assert_eq!(ok.volume_id, "OWNBACKUP");
        // A CD-only ISO, not an ISO at all, a torn file.
        assert!(inspect_iso(&iso_head("OWNBACKUP", false), 1 << 30).is_err());
        assert!(inspect_iso(&vec![0u8; ISO_HEAD_BYTES], 1 << 30).is_err());
        assert!(inspect_iso(&iso_head("OWNBACKUP", true), (1 << 30) + 7).is_err());
    }
}
