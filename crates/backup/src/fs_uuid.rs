//! Reading a filesystem's UUID out of its superblock.
//!
//! Needed to answer one question: which of the bootloader directories on a
//! restored ESP belongs to the system that was just restored? Their GRUB stubs
//! name a filesystem by UUID, so the answer is whichever UUID is actually on
//! the disk now (see [`crate::esp_fallback`]).
//!
//! Windows cannot mount ext4 or btrfs, and does not need to: a superblock is a
//! fixed offset and a magic number away, and the restore already has raw read
//! access to every partition it wrote. Pure parsing, so the layouts are checked
//! by tests rather than by trying it on a disk.

/// Bytes needed from the start of a partition to answer for every filesystem
/// here — btrfs puts its superblock at 64 KiB, which is the deepest of them.
pub const PROBE_LEN: usize = 68 * 1024;

/// The UUID of whichever filesystem `head` begins with, spelled the way GRUB
/// spells it in a `search` line.
pub fn probe(head: &[u8]) -> Option<String> {
    ext_uuid(head)
        .or_else(|| btrfs_uuid(head))
        .or_else(|| xfs_uuid(head))
        .or_else(|| fat_serial(head))
}

/// ext2/3/4: superblock at 1024, magic `0xEF53` at +56, UUID at +104.
fn ext_uuid(head: &[u8]) -> Option<String> {
    let sb = head.get(1024..1024 + 128)?;
    if u16::from_le_bytes([sb[56], sb[57]]) != 0xEF53 {
        return None;
    }
    Some(format_uuid(sb.get(104..120)?))
}

/// btrfs: superblock at 65536, magic `_BHRfS_M` at +64, fsid at +32.
fn btrfs_uuid(head: &[u8]) -> Option<String> {
    let sb = head.get(65536..65536 + 128)?;
    if &sb[64..72] != b"_BHRfS_M" {
        return None;
    }
    Some(format_uuid(sb.get(32..48)?))
}

/// XFS: superblock at 0, magic `XFSB`, UUID at +32.
fn xfs_uuid(head: &[u8]) -> Option<String> {
    if head.get(0..4)? != b"XFSB" {
        return None;
    }
    Some(format_uuid(head.get(32..48)?))
}

/// FAT16/32 have no UUID, only a 4-byte volume serial, which GRUB writes as
/// `XXXX-XXXX`. FAT32 keeps it at 0x43, FAT12/16 at 0x27.
fn fat_serial(head: &[u8]) -> Option<String> {
    let looks_like_fat = |at: usize, tag: &[u8]| {
        head.get(at..at + tag.len())
            .is_some_and(|s| s.starts_with(tag))
    };
    let at = if looks_like_fat(0x52, b"FAT32") {
        0x43
    } else if looks_like_fat(0x36, b"FAT") {
        0x27
    } else {
        return None;
    };
    let s = head.get(at..at + 4)?;
    Some(format!("{:02X}{:02X}-{:02X}{:02X}", s[3], s[2], s[1], s[0]))
}

/// The 16 raw bytes as a lowercase dashed UUID — the form GRUB, blkid and
/// `/etc/fstab` all use.
fn format_uuid(b: &[u8]) -> String {
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blank() -> Vec<u8> {
        vec![0u8; PROBE_LEN]
    }

    const RAW: [u8; 16] = [
        0x1a, 0x2b, 0x3c, 0x4d, 0x5e, 0x6f, 0x4a, 0x7b, 0x8c, 0x9d, 0x0e, 0x1f, 0x2a, 0x3b, 0x4c,
        0x5d,
    ];
    const TEXT: &str = "1a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d";

    #[test]
    fn an_ext4_boot_partition_gives_the_uuid_grub_searches_for() {
        let mut d = blank();
        d[1024 + 56..1024 + 58].copy_from_slice(&0xEF53u16.to_le_bytes());
        d[1024 + 104..1024 + 120].copy_from_slice(&RAW);
        assert_eq!(probe(&d).as_deref(), Some(TEXT));
    }

    #[test]
    fn btrfs_is_read_from_its_superblock_at_64_kib() {
        let mut d = blank();
        d[65536 + 64..65536 + 72].copy_from_slice(b"_BHRfS_M");
        d[65536 + 32..65536 + 48].copy_from_slice(&RAW);
        assert_eq!(probe(&d).as_deref(), Some(TEXT));
    }

    #[test]
    fn xfs_too() {
        let mut d = blank();
        d[0..4].copy_from_slice(b"XFSB");
        d[32..48].copy_from_slice(&RAW);
        assert_eq!(probe(&d).as_deref(), Some(TEXT));
    }

    #[test]
    fn a_fat_boot_partition_gives_its_serial_in_grubs_spelling() {
        // FAT has no UUID; GRUB names it by the volume serial, high byte first.
        let mut d = blank();
        d[0x52..0x57].copy_from_slice(b"FAT32");
        d[0x43..0x47].copy_from_slice(&[0x61, 0xE9, 0x0B, 0x06]);
        assert_eq!(probe(&d).as_deref(), Some("060B-E961"));
    }

    #[test]
    fn a_partition_with_no_filesystem_we_know_answers_nothing() {
        // A LUKS container, a swap area, empty space. Not an error: it simply
        // cannot be what a GRUB stub is pointing at.
        assert_eq!(probe(&blank()), None);
        assert_eq!(probe(&[0u8; 16]), None, "a short read must not panic");
    }
}
