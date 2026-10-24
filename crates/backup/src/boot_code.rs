//! The BIOS boot code in LBA 0, kept across a partition-table rewrite.
//!
//! The first 440 bytes of a disk are executable code: BIOS loads sector 0 and
//! jumps into it. What lives there depends on who installed the boot loader —
//! GRUB's `boot.img` (BIOS-GPT or MBR), the Windows NT6 MBR that chains to the
//! active partition, syslinux' `mbr.bin`. [`crate::gpt_writer`] and
//! [`crate::mbr_writer`] build LBA 0 from scratch and leave that area zeroed,
//! which is right for a fresh disk and wrong for one that boots: after the table
//! is rewritten the firmware loads a sector of zeros and the machine no longer
//! starts, though every partition is intact.
//!
//! So before a table is written the sector currently at LBA 0 is read, and its
//! boot code goes into the new LBA 0. Only when it is safe to:
//!
//! - **the sector must be a partition table**, not a filesystem boot sector. A
//!   disk formatted without a table ("superfloppy") carries an NTFS/FAT/exFAT or
//!   BitLocker boot sector at LBA 0, which also ends in `55 AA`; its first 440
//!   bytes are that filesystem's own loader and mean nothing as an MBR.
//! - **the scheme must stay the same.** Boot code is written for a layout:
//!   GRUB's `boot.img` carries the absolute LBA of `core.img`, which on GPT sits
//!   in the BIOS boot partition. Moving it across MBR ↔ GPT would jump into
//!   whatever now occupies that sector — worse than not booting at all.
//! - **all-zero code is no code.**
//!
//! The disk signature (0x1B8), the table (0x1BE) and the `55 AA` marker always
//! come from the new table: those describe the layout, not the boot loader.
//! For a protective MBR the "active" flag of the 0xEE entry is kept as well —
//! some BIOSes only boot a GPT disk whose protective entry is marked active
//! (`parted … pmbr_boot on`), and dropping the flag would undo that.

use own_backup_common::restore_plan::PartitionScheme;

use crate::gpt_writer::WriteOp;

/// Bytes of executable code at the start of LBA 0, before the NT disk
/// signature at 0x1B8.
pub const BOOT_CODE_LEN: usize = 440;

const TABLE_OFFSET: usize = 0x1BE;
const ENTRY_SIZE: usize = 16;
const PROTECTIVE_TYPE: u8 = 0xEE;
const FLAG_ACTIVE: u8 = 0x80;

/// What LBA 0 of a disk holds, as far as boot code is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lba0 {
    /// No `55 AA` marker, or a sector shorter than 512 bytes.
    Blank,
    /// A filesystem boot sector (the disk has no partition table).
    FilesystemBootSector,
    /// An MBR or protective MBR.
    Table {
        scheme: PartitionScheme,
        /// `None` when the code area is all zero.
        code: Option<Box<[u8; BOOT_CODE_LEN]>>,
        /// Protective MBR whose 0xEE entry is marked active.
        protective_active: bool,
    },
}

/// Classify the sector currently at LBA 0.
pub fn inspect_lba0(sector: &[u8]) -> Lba0 {
    if sector.len() < 512 || sector[510] != 0x55 || sector[511] != 0xAA {
        return Lba0::Blank;
    }
    if is_filesystem_boot_sector(sector) {
        return Lba0::FilesystemBootSector;
    }
    let entries: Vec<&[u8]> = (0..4)
        .map(|i| &sector[TABLE_OFFSET + i * ENTRY_SIZE..][..ENTRY_SIZE])
        .collect();
    // A real table has a boot indicator of 0x00 or 0x80 in every slot. Anything
    // else is code or text that happens to sit where a table would be.
    if entries.iter().any(|e| e[0] != 0 && e[0] != FLAG_ACTIVE) {
        return Lba0::FilesystemBootSector;
    }
    let protective = entries.iter().find(|e| e[4] == PROTECTIVE_TYPE);
    let scheme = if protective.is_some() {
        PartitionScheme::Gpt
    } else {
        PartitionScheme::Mbr
    };
    let code_area = &sector[..BOOT_CODE_LEN];
    let code = code_area.iter().any(|&b| b != 0).then(|| {
        let mut c = Box::new([0u8; BOOT_CODE_LEN]);
        c.copy_from_slice(code_area);
        c
    });
    Lba0::Table {
        scheme,
        code,
        protective_active: protective.is_some_and(|e| e[0] == FLAG_ACTIVE),
    }
}

/// Filesystem boot sectors that also end in `55 AA`: their OEM name at offset
/// 3, or the FAT type strings of the two BPB variants.
fn is_filesystem_boot_sector(s: &[u8]) -> bool {
    let oem = &s[3..11];
    if oem == b"NTFS    " || oem == b"EXFAT   " || oem == b"-FVE-FS-" {
        return true;
    }
    // FAT12/16 put "FAT" at 0x36, FAT32 at 0x52; both behind a jump at 0.
    let jumps = s[0] == 0xEB || s[0] == 0xE9;
    jumps && (&s[0x36..0x39] == b"FAT" || &s[0x52..0x57] == b"FAT32")
}

/// What happened to the boot code when a table was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootCodeOutcome {
    /// The code from the disk was carried into the new LBA 0.
    Kept,
    /// There was no code to keep (blank disk, zeroed code area).
    NoneOnDisk,
    /// LBA 0 was a filesystem boot sector, not a partition table.
    NotATable,
    /// The disk had code for the other scheme; it was dropped deliberately.
    SchemeChanged,
}

/// Carry the boot code from `current` (the sector at LBA 0 before the write)
/// into the LBA 0 write among `ops`, which was built for `scheme`.
///
/// `ops` without an LBA 0 write are left alone and report
/// [`BootCodeOutcome::NoneOnDisk`].
pub fn carry_boot_code(
    ops: &mut [WriteOp],
    scheme: PartitionScheme,
    current: &[u8],
) -> BootCodeOutcome {
    let (old_scheme, code, protective_active) = match inspect_lba0(current) {
        Lba0::Blank => return BootCodeOutcome::NoneOnDisk,
        Lba0::FilesystemBootSector => return BootCodeOutcome::NotATable,
        Lba0::Table {
            scheme,
            code,
            protective_active,
        } => (scheme, code, protective_active),
    };
    if old_scheme != scheme {
        return if code.is_some() {
            BootCodeOutcome::SchemeChanged
        } else {
            BootCodeOutcome::NoneOnDisk
        };
    }
    let Some(lba0) = ops
        .iter_mut()
        .find(|op| op.offset == 0 && op.bytes.len() >= 512)
    else {
        return BootCodeOutcome::NoneOnDisk;
    };
    if scheme == PartitionScheme::Gpt && protective_active {
        if let Some(slot) = (0..4)
            .map(|i| TABLE_OFFSET + i * ENTRY_SIZE)
            .find(|&o| lba0.bytes[o + 4] == PROTECTIVE_TYPE)
        {
            lba0.bytes[slot] = FLAG_ACTIVE;
        }
    }
    match code {
        Some(code) => {
            lba0.bytes[..BOOT_CODE_LEN].copy_from_slice(&code[..]);
            BootCodeOutcome::Kept
        }
        None => BootCodeOutcome::NoneOnDisk,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mbr_with(code: u8, entry_type: u8, active: bool) -> Vec<u8> {
        let mut s = vec![0u8; 512];
        s[..BOOT_CODE_LEN].fill(code);
        s[0x1B8..0x1BC].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        s[TABLE_OFFSET] = if active { FLAG_ACTIVE } else { 0 };
        s[TABLE_OFFSET + 4] = entry_type;
        s[510] = 0x55;
        s[511] = 0xAA;
        s
    }

    fn new_lba0(scheme: PartitionScheme) -> Vec<WriteOp> {
        let entry_type = match scheme {
            PartitionScheme::Gpt => PROTECTIVE_TYPE,
            PartitionScheme::Mbr => 0x07,
        };
        let mut s = mbr_with(0, entry_type, false);
        s[0x1B8..0x1BC].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        vec![
            WriteOp {
                offset: 0,
                bytes: s,
            },
            WriteOp {
                offset: 512,
                bytes: vec![1; 512],
            },
        ]
    }

    #[test]
    fn grub_code_on_a_gpt_disk_survives_a_gpt_rewrite() {
        let current = mbr_with(0xFA, PROTECTIVE_TYPE, false);
        let mut ops = new_lba0(PartitionScheme::Gpt);
        let out = carry_boot_code(&mut ops, PartitionScheme::Gpt, &current);
        assert_eq!(out, BootCodeOutcome::Kept);
        assert!(ops[0].bytes[..BOOT_CODE_LEN].iter().all(|&b| b == 0xFA));
        // The new table and signature stay the new ones.
        assert_eq!(&ops[0].bytes[0x1B8..0x1BC], &0x1234_5678u32.to_le_bytes());
        assert_eq!(ops[0].bytes[TABLE_OFFSET + 4], PROTECTIVE_TYPE);
        assert_eq!(&ops[0].bytes[510..], &[0x55, 0xAA]);
        assert_eq!(ops[1].bytes, vec![1; 512], "other writes untouched");
    }

    #[test]
    fn windows_mbr_code_survives_an_mbr_rewrite() {
        let current = mbr_with(0x33, 0x07, true);
        let mut ops = new_lba0(PartitionScheme::Mbr);
        assert_eq!(
            carry_boot_code(&mut ops, PartitionScheme::Mbr, &current),
            BootCodeOutcome::Kept
        );
        assert!(ops[0].bytes[..BOOT_CODE_LEN].iter().all(|&b| b == 0x33));
        // The active flag of a regular MBR comes from the new spec, not the disk.
        assert_eq!(ops[0].bytes[TABLE_OFFSET], 0);
    }

    #[test]
    fn the_active_protective_entry_is_kept() {
        let current = mbr_with(0xFA, PROTECTIVE_TYPE, true);
        let mut ops = new_lba0(PartitionScheme::Gpt);
        carry_boot_code(&mut ops, PartitionScheme::Gpt, &current);
        assert_eq!(ops[0].bytes[TABLE_OFFSET], FLAG_ACTIVE);
    }

    #[test]
    fn code_is_not_carried_across_schemes() {
        let mut ops = new_lba0(PartitionScheme::Mbr);
        let current = mbr_with(0xFA, PROTECTIVE_TYPE, true);
        assert_eq!(
            carry_boot_code(&mut ops, PartitionScheme::Mbr, &current),
            BootCodeOutcome::SchemeChanged
        );
        assert!(ops[0].bytes[..BOOT_CODE_LEN].iter().all(|&b| b == 0));

        let mut ops = new_lba0(PartitionScheme::Gpt);
        let current = mbr_with(0x33, 0x07, true);
        assert_eq!(
            carry_boot_code(&mut ops, PartitionScheme::Gpt, &current),
            BootCodeOutcome::SchemeChanged
        );
        assert_eq!(ops[0].bytes[TABLE_OFFSET], 0);
    }

    #[test]
    fn filesystem_boot_sectors_are_not_tables() {
        for oem in [b"NTFS    ", b"EXFAT   ", b"-FVE-FS-"] {
            let mut s = mbr_with(0x90, 0, false);
            s[3..11].copy_from_slice(oem);
            assert_eq!(inspect_lba0(&s), Lba0::FilesystemBootSector);
            let mut ops = new_lba0(PartitionScheme::Mbr);
            assert_eq!(
                carry_boot_code(&mut ops, PartitionScheme::Mbr, &s),
                BootCodeOutcome::NotATable
            );
            assert!(ops[0].bytes[..BOOT_CODE_LEN].iter().all(|&b| b == 0));
        }
        let mut fat32 = mbr_with(0x90, 0, false);
        fat32[0] = 0xEB;
        fat32[0x52..0x57].copy_from_slice(b"FAT32");
        assert_eq!(inspect_lba0(&fat32), Lba0::FilesystemBootSector);
        let mut fat16 = mbr_with(0x90, 0, false);
        fat16[0] = 0xEB;
        fat16[0x36..0x39].copy_from_slice(b"FAT");
        assert_eq!(inspect_lba0(&fat16), Lba0::FilesystemBootSector);
    }

    #[test]
    fn garbage_where_the_table_would_be_is_not_a_table() {
        // A loader whose code runs into 0x1BE: boot indicator bytes other than
        // 0x00/0x80.
        let mut s = vec![0x41u8; 512];
        s[510] = 0x55;
        s[511] = 0xAA;
        assert_eq!(inspect_lba0(&s), Lba0::FilesystemBootSector);
    }

    #[test]
    fn blank_and_zeroed_disks_have_nothing_to_keep() {
        let mut ops = new_lba0(PartitionScheme::Gpt);
        assert_eq!(
            carry_boot_code(&mut ops, PartitionScheme::Gpt, &[0u8; 512]),
            BootCodeOutcome::NoneOnDisk
        );
        let zeroed = mbr_with(0, PROTECTIVE_TYPE, false);
        assert_eq!(
            carry_boot_code(&mut ops, PartitionScheme::Gpt, &zeroed),
            BootCodeOutcome::NoneOnDisk
        );
        assert_eq!(
            carry_boot_code(&mut ops, PartitionScheme::Gpt, &[0u8; 100]),
            BootCodeOutcome::NoneOnDisk
        );
    }

    #[test]
    fn a_4k_sector_lba0_is_handled() {
        let mut current = mbr_with(0xFA, PROTECTIVE_TYPE, false);
        current.resize(4096, 0);
        let mut ops = new_lba0(PartitionScheme::Gpt);
        ops[0].bytes.resize(4096, 0);
        assert_eq!(
            carry_boot_code(&mut ops, PartitionScheme::Gpt, &current),
            BootCodeOutcome::Kept
        );
        assert_eq!(ops[0].bytes.len(), 4096);
    }
}
