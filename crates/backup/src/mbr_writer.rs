//! MBR partition table builder — the counterpart of [`crate::gpt_writer`] (D3).
//!
//! GPT is what modern disks use, but restore targets are chosen by the user and
//! plenty of them are still MBR. Where a GPT is one flat 128-entry array, an MBR
//! is a 64-byte table with exactly four slots plus a linked list bolted on to
//! escape that limit:
//!
//! ```text
//! LBA 0                MBR: boot code, disk signature, 4 entries, 0x55AA
//!   entry → primary partition                     (absolute LBA)
//!   entry → extended partition, type 0x0F         (absolute LBA, spans all logicals)
//!
//! LBA E0               EBR: entry 0 → logical volume 1  (LBA relative to E0)
//!                           entry 1 → next EBR          (LBA relative to extended start)
//! LBA E1               EBR: entry 0 → logical volume 2
//!                           entry 1 → next EBR, or zero at the end of the chain
//! ```
//!
//! The two different relative bases in an EBR are not a typo — they are how the
//! format is defined, and getting them the wrong way round produces a table that
//! looks plausible and mounts nothing.
//!
//! ## What the caller supplies
//!
//! [`PartitionTableSpec::entries`] lists real partitions only. The extended
//! partition is *not* one of them: it is a container this writer synthesises
//! around whatever logical partitions are present, taking the lowest free
//! primary slot. Each logical partition's EBR is placed in the sector
//! immediately before its volume, so the sector before every logical volume must
//! be free.

use own_backup_common::restore_plan::{PartitionKind, PartitionScheme, PartitionSpec, PartitionTableSpec};
use own_backup_common::{OwnBackupError, Result};
use uuid::Uuid;

use crate::gpt_writer::WriteOp;

/// Extended partition, LBA-addressed (type 0x05 is the ancient CHS-addressed
/// variant, limited to 8 GiB — no reason to emit it).
pub const MBR_TYPE_EXTENDED_LBA: u8 = 0x0F;

/// Boot indicator for the active partition.
const FLAG_ACTIVE: u8 = 0x80;

/// Classic BIOS geometry. Nothing has used real CHS addressing for decades, but
/// the fields still have to hold something sane or some firmware refuses to boot.
const HEADS_PER_CYLINDER: u64 = 255;
const SECTORS_PER_TRACK: u64 = 63;

const SIG_OFFSET: usize = 0x1B8;
const TABLE_OFFSET: usize = 0x1BE;
const ENTRY_SIZE: usize = 16;

/// One partition table entry, resolved to what actually goes on disk.
struct Entry {
    active: bool,
    type_byte: u8,
    /// Absolute LBA of the partition's first sector — used for the CHS fields.
    abs_first_lba: u64,
    /// What goes in the entry's start field: absolute for a primary, relative
    /// to a base for anything inside an EBR.
    stored_first_lba: u64,
    sector_count: u64,
}

impl Entry {
    fn write_into(&self, buf: &mut [u8]) -> Result<()> {
        buf[0] = if self.active { FLAG_ACTIVE } else { 0 };
        let (h, s, c) = lba_to_chs(self.abs_first_lba);
        buf[1] = h;
        buf[2] = s;
        buf[3] = c;
        buf[4] = self.type_byte;
        let (h, s, c) = lba_to_chs(self.abs_first_lba + self.sector_count - 1);
        buf[5] = h;
        buf[6] = s;
        buf[7] = c;
        let start = u32::try_from(self.stored_first_lba).map_err(|_| {
            OwnBackupError::ImageFormat(format!(
                "MBR cannot address sector {} — it needs more than 32 bits. Use GPT.",
                self.stored_first_lba
            ))
        })?;
        let count = u32::try_from(self.sector_count).map_err(|_| {
            OwnBackupError::ImageFormat(format!(
                "MBR cannot express a {}-sector partition — it needs more than 32 bits. Use GPT.",
                self.sector_count
            ))
        })?;
        buf[8..12].copy_from_slice(&start.to_le_bytes());
        buf[12..16].copy_from_slice(&count.to_le_bytes());
        Ok(())
    }
}

/// Encode an LBA into the entry's three CHS bytes (head, sector|cylinder-high,
/// cylinder-low). Beyond what CHS can express the value is pinned to the
/// conventional "too big to say" marker, which is what every partitioning tool
/// does and what firmware expects to see on a modern disk.
fn lba_to_chs(lba: u64) -> (u8, u8, u8) {
    let cylinder = lba / (HEADS_PER_CYLINDER * SECTORS_PER_TRACK);
    if cylinder > 1023 {
        return (0xFE, 0xFF, 0xFF); // head 254, sector 63, cylinder 1023
    }
    let head = (lba / SECTORS_PER_TRACK) % HEADS_PER_CYLINDER;
    let sector = lba % SECTORS_PER_TRACK + 1;
    let s_byte = (sector as u8 & 0x3F) | (((cylinder >> 8) as u8 & 0x03) << 6);
    (head as u8, s_byte, (cylinder & 0xFF) as u8)
}

/// The 4-byte NT disk signature. An empty `disk_id` means "make one up"; the
/// value only has to be unique among the disks the machine can see, and Windows
/// keys its drive-letter assignments off it.
fn disk_signature(disk_id: &str) -> Result<u32> {
    if disk_id.trim().is_empty() {
        return Ok(Uuid::new_v4().as_u128() as u32 | 1);
    }
    u32::from_str_radix(disk_id.trim(), 16).map_err(|_| {
        OwnBackupError::ImageFormat(format!(
            "disk signature must be 8 hex digits, got {disk_id:?}"
        ))
    })
}

struct Mbr<'a> {
    primaries: Vec<&'a PartitionSpec>,
    logicals: Vec<&'a PartitionSpec>,
}

/// Split the spec's entries and reject anything MBR cannot express.
fn classify(spec: &PartitionTableSpec) -> Result<Mbr<'_>> {
    let mut primaries = Vec::new();
    let mut logicals = Vec::new();
    for e in &spec.entries {
        match &e.kind {
            PartitionKind::Mbr { logical: true, .. } => logicals.push(e),
            PartitionKind::Mbr { logical: false, .. } => primaries.push(e),
            PartitionKind::Gpt { .. } => {
                return Err(OwnBackupError::ImageFormat(format!(
                    "partition {} carries GPT attributes but the table is MBR",
                    e.index
                )))
            }
        }
    }
    primaries.sort_by_key(|e| e.index);
    logicals.sort_by_key(|e| e.first_lba);

    // The extended partition is itself a primary entry, so it eats one slot.
    let budget = if logicals.is_empty() { 4 } else { 3 };
    if primaries.len() > budget {
        return Err(OwnBackupError::ImageFormat(format!(
            "MBR holds four primary partitions; this table needs {}{}",
            primaries.len(),
            if logicals.is_empty() {
                String::new()
            } else {
                " plus an extended partition for the logical ones".into()
            }
        )));
    }
    if primaries.iter().filter(|e| is_active(e)).count() > 1 {
        return Err(OwnBackupError::ImageFormat(
            "more than one partition is marked active; BIOS boots exactly one".into(),
        ));
    }
    Ok(Mbr { primaries, logicals })
}

fn is_active(e: &PartitionSpec) -> bool {
    matches!(e.kind, PartitionKind::Mbr { bootable: true, .. })
}

fn type_byte(e: &PartitionSpec) -> u8 {
    match e.kind {
        PartitionKind::Mbr { type_byte, .. } => type_byte,
        PartitionKind::Gpt { .. } => 0,
    }
}

/// Build the raw writes that lay an MBR partition table onto a disk.
///
/// Returns one write for LBA 0 and one per EBR. Sectors that belong to
/// partition data are never touched — this only rewrites table structures.
pub fn mbr_write_ops(spec: &PartitionTableSpec) -> Result<Vec<WriteOp>> {
    if spec.scheme != PartitionScheme::Mbr {
        return Err(OwnBackupError::ImageFormat(
            "mbr_write_ops called with a GPT table".into(),
        ));
    }
    if spec.sector_size != 512 && spec.sector_size != 4096 {
        return Err(OwnBackupError::ImageFormat(format!(
            "unsupported sector size {}",
            spec.sector_size
        )));
    }
    let sector = spec.sector_size as u64;
    let total_lba = spec.disk_size_bytes / sector;
    let Mbr { primaries, logicals } = classify(spec)?;

    for e in &spec.entries {
        if e.last_lba < e.first_lba {
            return Err(OwnBackupError::ImageFormat(format!(
                "partition {} has a negative length ({}..{})",
                e.index, e.first_lba, e.last_lba
            )));
        }
        if e.end_lba_exclusive() > total_lba {
            return Err(OwnBackupError::ImageFormat(format!(
                "partition {} ends at sector {} but the disk has {total_lba}",
                e.index,
                e.last_lba
            )));
        }
        if e.first_lba == 0 {
            return Err(OwnBackupError::ImageFormat(format!(
                "partition {} starts at LBA 0, where the MBR itself lives",
                e.index
            )));
        }
    }

    let mut ops = Vec::new();
    let mut mbr = vec![0u8; spec.sector_size as usize];
    mbr[SIG_OFFSET..SIG_OFFSET + 4].copy_from_slice(&disk_signature(&spec.disk_id)?.to_le_bytes());
    mbr[510] = 0x55;
    mbr[511] = 0xAA;

    // Which of the four slots each primary occupies. Indices 1..=4 map to slots
    // 0..=3; the extended container takes the lowest one left over.
    let mut slot_taken = [false; 4];
    for e in &primaries {
        let slot = usize::try_from(e.index).ok().and_then(|i| i.checked_sub(1)).unwrap_or(usize::MAX);
        if slot >= 4 {
            return Err(OwnBackupError::ImageFormat(format!(
                "primary partition index {} is outside the four MBR slots",
                e.index
            )));
        }
        if slot_taken[slot] {
            return Err(OwnBackupError::ImageFormat(format!(
                "two primary partitions both claim MBR slot {}",
                slot + 1
            )));
        }
        slot_taken[slot] = true;
        Entry {
            active: is_active(e),
            type_byte: type_byte(e),
            abs_first_lba: e.first_lba,
            stored_first_lba: e.first_lba,
            sector_count: e.end_lba_exclusive() - e.first_lba,
        }
        .write_into(&mut mbr[TABLE_OFFSET + slot * ENTRY_SIZE..][..ENTRY_SIZE])?;
    }

    if !logicals.is_empty() {
        let ebr_ops = build_extended(spec, &logicals, &mut mbr, &mut slot_taken)?;
        ops.extend(ebr_ops);
    }

    ops.insert(0, WriteOp { offset: 0, bytes: mbr });
    Ok(ops)
}

/// Lay out the extended partition and its EBR chain, filling in the extended
/// entry in `mbr` and returning one write per EBR.
fn build_extended(
    spec: &PartitionTableSpec,
    logicals: &[&PartitionSpec],
    mbr: &mut [u8],
    slot_taken: &mut [bool; 4],
) -> Result<Vec<WriteOp>> {
    let sector = spec.sector_size as u64;

    // Each EBR goes in the sector immediately before its volume, so that sector
    // must not belong to anything else.
    let mut ebr_lbas = Vec::with_capacity(logicals.len());
    for l in logicals {
        let ebr = l.first_lba.checked_sub(1).ok_or_else(|| {
            OwnBackupError::ImageFormat(format!(
                "logical partition {} starts at LBA 0, leaving no room for its EBR",
                l.index
            ))
        })?;
        if let Some(clash) = spec.entries.iter().find(|e| ebr >= e.first_lba && ebr <= e.last_lba) {
            return Err(OwnBackupError::ImageFormat(format!(
                "logical partition {} needs sector {} for its EBR, but partition {} occupies it",
                l.index, ebr, clash.index
            )));
        }
        ebr_lbas.push(ebr);
    }

    let ext_first_lba = ebr_lbas[0];
    let ext_last_lba = logicals.last().unwrap().last_lba;

    let slot = slot_taken.iter().position(|t| !t).ok_or_else(|| {
        OwnBackupError::ImageFormat(
            "no free MBR slot for the extended partition — logical partitions need one".into(),
        )
    })?;
    slot_taken[slot] = true;
    Entry {
        active: false,
        type_byte: MBR_TYPE_EXTENDED_LBA,
        abs_first_lba: ext_first_lba,
        stored_first_lba: ext_first_lba,
        sector_count: ext_last_lba - ext_first_lba + 1,
    }
    .write_into(&mut mbr[TABLE_OFFSET + slot * ENTRY_SIZE..][..ENTRY_SIZE])?;

    let mut ops = Vec::with_capacity(logicals.len());
    for (i, l) in logicals.iter().enumerate() {
        let ebr_lba = ebr_lbas[i];
        let mut ebr = vec![0u8; spec.sector_size as usize];
        ebr[510] = 0x55;
        ebr[511] = 0xAA;

        // Entry 0 — the logical volume. Its start is relative to this EBR.
        Entry {
            active: is_active(l),
            type_byte: type_byte(l),
            abs_first_lba: l.first_lba,
            stored_first_lba: l.first_lba - ebr_lba,
            sector_count: l.end_lba_exclusive() - l.first_lba,
        }
        .write_into(&mut ebr[TABLE_OFFSET..][..ENTRY_SIZE])?;

        // Entry 1 — the link to the next EBR. Its start is relative to the
        // *extended partition*, not to this EBR: the one asymmetry in the whole
        // format, and the classic way to build a chain that mounts nothing.
        if let Some(next) = logicals.get(i + 1) {
            let next_ebr = ebr_lbas[i + 1];
            Entry {
                active: false,
                type_byte: MBR_TYPE_EXTENDED_LBA,
                abs_first_lba: next_ebr,
                stored_first_lba: next_ebr - ext_first_lba,
                sector_count: next.end_lba_exclusive() - next_ebr,
            }
            .write_into(&mut ebr[TABLE_OFFSET + ENTRY_SIZE..][..ENTRY_SIZE])?;
        }

        ops.push(WriteOp { offset: ebr_lba * sector, bytes: ebr });
    }
    Ok(ops)
}

#[cfg(test)]
mod tests {
    use super::*;
    use own_backup_common::restore_plan::PartitionScheme;

    const MIB: u64 = 1024 * 1024;

    fn part(index: u32, first_mib: u64, size_mib: u64, type_byte: u8, logical: bool) -> PartitionSpec {
        let first_lba = first_mib * MIB / 512;
        PartitionSpec {
            index,
            first_lba,
            last_lba: first_lba + size_mib * MIB / 512 - 1,
            kind: PartitionKind::Mbr { type_byte, bootable: false, logical },
        }
    }

    fn spec(entries: Vec<PartitionSpec>) -> PartitionTableSpec {
        PartitionTableSpec {
            scheme: PartitionScheme::Mbr,
            sector_size: 512,
            disk_size_bytes: 100 * 1024 * MIB,
            disk_id: "1A2B3C4D".into(),
            entries,
        }
    }

    fn u32_at(buf: &[u8], off: usize) -> u32 {
        u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
    }

    fn slot(mbr: &[u8], n: usize) -> &[u8] {
        &mbr[TABLE_OFFSET + n * ENTRY_SIZE..][..ENTRY_SIZE]
    }

    #[test]
    fn writes_the_signature_and_boot_marker() {
        let ops = mbr_write_ops(&spec(vec![part(1, 1, 100, 0x07, false)])).unwrap();
        assert_eq!(ops.len(), 1);
        let mbr = &ops[0].bytes;
        assert_eq!(ops[0].offset, 0);
        assert_eq!(u32_at(mbr, SIG_OFFSET), 0x1A2B3C4D);
        assert_eq!(mbr[510], 0x55);
        assert_eq!(mbr[511], 0xAA);
    }

    #[test]
    fn a_primary_entry_is_byte_exact() {
        let mut p = part(1, 1, 100, 0x07, false);
        p.kind = PartitionKind::Mbr { type_byte: 0x07, bootable: true, logical: false };
        let ops = mbr_write_ops(&spec(vec![p])).unwrap();
        let e = slot(&ops[0].bytes, 0);
        assert_eq!(e[0], 0x80, "active flag");
        assert_eq!(e[4], 0x07, "type byte");
        assert_eq!(u32_at(e, 8), 2048, "first LBA — absolute for a primary");
        assert_eq!(u32_at(e, 12), 100 * MIB as u32 / 512, "sector count");
    }

    #[test]
    fn partitions_land_in_the_slot_their_index_names() {
        let ops = mbr_write_ops(&spec(vec![
            part(3, 1, 10, 0x07, false),
            part(1, 20, 10, 0x83, false),
        ]))
        .unwrap();
        let mbr = &ops[0].bytes;
        assert_eq!(slot(mbr, 0)[4], 0x83, "index 1 → slot 1");
        assert_eq!(slot(mbr, 1)[4], 0x00, "slot 2 stays empty");
        assert_eq!(slot(mbr, 2)[4], 0x07, "index 3 → slot 3");
    }

    #[test]
    fn chs_is_capped_beyond_what_it_can_express() {
        // 1024 cylinders × 255 heads × 63 sectors ≈ 8 GiB.
        let (h, s, c) = lba_to_chs(64 * 1024 * 1024 * 1024 / 512);
        assert_eq!((h, s, c), (0xFE, 0xFF, 0xFF));
    }

    #[test]
    fn chs_of_the_conventional_first_partition() {
        // LBA 2048 = cylinder 0, head 32, sector 33 on 255×63 geometry.
        let (h, s, c) = lba_to_chs(2048);
        assert_eq!(h, 32);
        assert_eq!(s & 0x3F, 33);
        assert_eq!(c, 0);
        assert_eq!(s >> 6, 0, "cylinder high bits");
    }

    #[test]
    fn four_primaries_fit_but_five_do_not() {
        let four = vec![
            part(1, 1, 10, 0x07, false),
            part(2, 11, 10, 0x07, false),
            part(3, 21, 10, 0x07, false),
            part(4, 31, 10, 0x07, false),
        ];
        assert!(mbr_write_ops(&spec(four.clone())).is_ok());
        let mut five = four;
        five.push(part(5, 41, 10, 0x07, false));
        assert!(mbr_write_ops(&spec(five)).is_err());
    }

    #[test]
    fn logical_partitions_leave_only_three_primary_slots() {
        let ok = vec![
            part(1, 1, 10, 0x07, false),
            part(2, 11, 10, 0x07, false),
            part(3, 21, 10, 0x07, false),
            part(5, 41, 10, 0x07, true),
        ];
        assert!(mbr_write_ops(&spec(ok.clone())).is_ok());
        let mut too_many = ok;
        too_many.insert(3, part(4, 31, 10, 0x07, false));
        assert!(mbr_write_ops(&spec(too_many)).is_err());
    }

    #[test]
    fn the_extended_container_takes_the_lowest_free_slot() {
        let ops = mbr_write_ops(&spec(vec![
            part(1, 1, 10, 0x07, false),
            part(3, 21, 10, 0x07, false),
            part(5, 41, 10, 0x07, true),
        ]))
        .unwrap();
        let mbr = &ops[0].bytes;
        assert_eq!(slot(mbr, 1)[4], MBR_TYPE_EXTENDED_LBA, "slot 2 was the lowest free one");
        assert_eq!(slot(mbr, 3)[4], 0x00);
    }

    #[test]
    fn the_extended_container_spans_from_the_first_ebr_to_the_last_logical() {
        let l1 = part(5, 41, 10, 0x07, true);
        let l2 = part(6, 61, 10, 0x07, true);
        let last = l2.last_lba;
        let ops = mbr_write_ops(&spec(vec![part(1, 1, 10, 0x07, false), l1.clone(), l2])).unwrap();
        let e = slot(&ops[0].bytes, 1);
        let ext_first = l1.first_lba - 1;
        assert_eq!(u32_at(e, 8) as u64, ext_first);
        assert_eq!(u32_at(e, 12) as u64, last - ext_first + 1);
    }

    #[test]
    fn ebr_offsets_use_the_two_different_bases() {
        // This is the whole point of the format's asymmetry: entry 0 counts from
        // its own EBR, entry 1 counts from the extended partition's start.
        let l1 = part(5, 41, 10, 0x07, true);
        let l2 = part(6, 61, 10, 0x07, true);
        let (e1_lba, e2_lba) = (l1.first_lba - 1, l2.first_lba - 1);
        let ext_first = e1_lba;

        let ops = mbr_write_ops(&spec(vec![part(1, 1, 10, 0x07, false), l1.clone(), l2.clone()])).unwrap();
        assert_eq!(ops.len(), 3, "MBR + two EBRs");

        let ebr1 = &ops[1];
        assert_eq!(ebr1.offset, e1_lba * 512);
        let vol = &ebr1.bytes[TABLE_OFFSET..][..ENTRY_SIZE];
        assert_eq!(u32_at(vol, 8), 1, "volume sits one sector past its own EBR");
        assert_eq!(u32_at(vol, 12) as u64, l1.end_lba_exclusive() - l1.first_lba);

        let link = &ebr1.bytes[TABLE_OFFSET + ENTRY_SIZE..][..ENTRY_SIZE];
        assert_eq!(link[4], MBR_TYPE_EXTENDED_LBA);
        assert_eq!(u32_at(link, 8) as u64, e2_lba - ext_first, "link is relative to the extended start");
        assert_eq!(u32_at(link, 12) as u64, l2.end_lba_exclusive() - e2_lba);

        let ebr2 = &ops[2];
        assert_eq!(ebr2.offset, e2_lba * 512);
        let tail = &ebr2.bytes[TABLE_OFFSET + ENTRY_SIZE..][..ENTRY_SIZE];
        assert!(tail.iter().all(|b| *b == 0), "the last EBR links nowhere");
        assert_eq!(ebr2.bytes[510], 0x55);
        assert_eq!(ebr2.bytes[511], 0xAA);
    }

    #[test]
    fn an_ebr_may_not_land_inside_another_partition() {
        // A logical volume butted directly against a primary leaves the EBR
        // nowhere to go.
        let primary = part(1, 1, 40, 0x07, false);
        let logical = PartitionSpec {
            index: 5,
            first_lba: primary.last_lba + 1,
            last_lba: primary.last_lba + 1000,
            kind: PartitionKind::Mbr { type_byte: 0x07, bootable: false, logical: true },
        };
        let err = mbr_write_ops(&spec(vec![primary, logical])).unwrap_err();
        assert!(format!("{err}").contains("EBR"), "{err}");
    }

    #[test]
    fn two_active_partitions_are_refused() {
        let mut a = part(1, 1, 10, 0x07, false);
        let mut b = part(2, 11, 10, 0x07, false);
        a.kind = PartitionKind::Mbr { type_byte: 0x07, bootable: true, logical: false };
        b.kind = PartitionKind::Mbr { type_byte: 0x07, bootable: true, logical: false };
        assert!(mbr_write_ops(&spec(vec![a, b])).is_err());
    }

    #[test]
    fn a_partition_past_the_end_of_the_disk_is_refused() {
        let mut s = spec(vec![part(1, 1, 100, 0x07, false)]);
        s.disk_size_bytes = 50 * MIB;
        assert!(mbr_write_ops(&s).is_err());
    }

    #[test]
    fn a_partition_beyond_32_bit_addressing_is_refused() {
        // MBR's LBA fields are 32 bits: ~2 TiB at 512 B/sector.
        let mut s = spec(vec![PartitionSpec {
            index: 1,
            first_lba: 5_000_000_000,
            last_lba: 5_000_002_047,
            kind: PartitionKind::Mbr { type_byte: 0x07, bootable: false, logical: false },
        }]);
        s.disk_size_bytes = 4 * 1024 * 1024 * MIB;
        let err = mbr_write_ops(&s).unwrap_err();
        assert!(format!("{err}").contains("GPT"), "{err}");
    }

    #[test]
    fn an_empty_disk_signature_gets_one_generated() {
        let mut s = spec(vec![part(1, 1, 10, 0x07, false)]);
        s.disk_id = String::new();
        let ops = mbr_write_ops(&s).unwrap();
        assert_ne!(u32_at(&ops[0].bytes, SIG_OFFSET), 0);
    }

    #[test]
    fn a_gpt_entry_in_an_mbr_table_is_refused() {
        let s = PartitionTableSpec {
            scheme: PartitionScheme::Mbr,
            sector_size: 512,
            disk_size_bytes: 100 * MIB,
            disk_id: String::new(),
            entries: vec![PartitionSpec {
                index: 1,
                first_lba: 2048,
                last_lba: 4095,
                kind: PartitionKind::Gpt {
                    type_guid: String::new(),
                    part_guid: String::new(),
                    attributes: 0,
                    name: String::new(),
                },
            }],
        };
        assert!(mbr_write_ops(&s).is_err());
    }

    #[test]
    fn the_table_never_writes_into_partition_data() {
        let l1 = part(5, 41, 10, 0x07, true);
        let ops = mbr_write_ops(&spec(vec![part(1, 1, 10, 0x07, false), l1.clone()])).unwrap();
        for op in &ops {
            let start = op.offset / 512;
            let end = start + (op.bytes.len() as u64) / 512;
            assert!(
                end <= 1 || start >= l1.first_lba - 1,
                "write at LBA {start}..{end} overlaps partition data"
            );
        }
    }
}
