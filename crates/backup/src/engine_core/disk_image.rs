//! Whole-disk building blocks: what a partition's first sectors say it is, the
//! token a partition is listed under, and the reader and used-map that turn
//! several partitions at their real offsets into one disk image.

use std::path::PathBuf;

use zeroize::Zeroizing;

use own_backup_common::types::{BackupType, ImageFormat};
use own_backup_common::Result;

use crate::block_reader::BlockReader;
use crate::exclusions::ByteRanges;
use crate::used_sectors::UsedSectorMap;

/// One partition to include in a whole-disk backup.
#[derive(Clone)]
pub struct DiskPartitionBackup {
    /// Source volume device (`\\.\Volume{…}`, a drive letter, or `part:…`).
    pub device: String,
    /// GPT slot (matches [`own_backup_common::gpt::GptPartitionEntry::index`]).
    pub part_index: u32,
    /// Passphrase for a LUKS partition (its decrypted data is captured).
    pub passphrase: Option<Zeroizing<String>>,
}

/// Parameters for a whole-disk (bootable) backup into a single image.
#[derive(Clone)]
pub struct DiskBackupRequest {
    pub disk_number: u32,
    pub disk_model: String,
    pub partitions: Vec<DiskPartitionBackup>,
    pub image_path: PathBuf,
    /// Where the image goes when a child was asked for and a FULL comes out
    /// (parent unusable, disk re-partitioned): the file name says what the file
    /// is, and so does the name recorded inside it. `None` keeps `image_path`
    /// whatever happens.
    pub full_image_path: Option<PathBuf>,
    pub block_size: u32,
    pub image_format: ImageFormat,
    pub job_id: Option<String>,
    /// Present for an encrypted job: the whole-disk image is written as `.obk`,
    /// with its sidecar — including the restore manifest — sealed inside the
    /// container instead of in a guest meta partition (§7.3).
    pub encryption: Option<crate::seal::context::ArchiveKeys>,
    /// File-exclusion glob patterns for Windows-family filesystems (NTFS/ReFS/
    /// FAT), applied per partition (plan D2): NTFS/ReFS drop + zero the matched
    /// clusters (data never leaves the machine; the catalog entry stays as a
    /// zero-filled ghost); FAT32/exFAT also patch the metadata so the files
    /// fully disappear.
    pub exclusion_patterns_windows: Vec<String>,
    /// File-exclusion masks for Linux filesystems (ext/btrfs/xfs). Selected per
    /// partition by filesystem; enforcement on ext/btrfs/xfs is a follow-up
    /// (stored/passed now, applied once the Linux-FS exclusion scanner lands).
    pub exclusion_patterns_linux: Vec<String>,
    /// Full / Incremental / Differential (plan D5). A non-full kind needs
    /// `parent_image` to be the previous whole-disk image of the SAME disk; if
    /// the chain turns out unusable (missing parent, geometry changed, format
    /// mismatch) the engine falls back to a full backup and reports it in the
    /// summary.
    pub kind: BackupType,
    /// Parent whole-disk image for an incremental (the previous version) or
    /// differential (the chain's full). A differencing child is written over it.
    pub parent_image: Option<PathBuf>,
    /// Full source-disk layout snapshot (from the GUI's live inventory) to embed
    /// in the disk manifest, so a bare-metal restore can identify the archived
    /// disk without jobs.json. `None` for CLI runs (the manifest then omits it).
    pub source_disk: Option<own_backup_common::disk::DiskInfo>,
    /// The user ticked "Загрузочная копия диска" for this disk.
    ///
    /// It decides one thing here: whether the machine's UEFI boot entries go
    /// into the image. They are the firmware's, not the disk's, and a copy of a
    /// data disk has no business carrying the boot menu.
    pub bootable: bool,
    /// zstd level for qcow2 and `.obk` (see [`VolumeBackupRequest::zstd_level`]).
    pub zstd_level: i32,
}

/// Which encrypted container a volume's first sectors say it is.
///
/// Costs nothing and needs no password — LUKS puts its magic and version at
/// offset 0, BitLocker its signature in the boot sector — and it is the only
/// way to know what a container we could NOT open was. Without it a locked
/// partition copied as ciphertext and an unlocked one copied in the clear look
/// the same in the manifest, and the restore screen has to guess which of two
/// opposite things to tell the user.
pub(crate) fn container_kind(
    header: &[u8],
) -> Option<own_backup_common::image::VolumeEncryptionKind> {
    use own_backup_common::image::VolumeEncryptionKind as K;
    if crate::bitlocker::is_bitlocker(header) {
        return Some(K::BitLocker);
    }
    if crate::luks::is_luks(header) {
        // Version is the two big-endian bytes right after the magic.
        return Some(match u16::from_be_bytes([header[6], header[7]]) {
            1 => K::Luks1,
            2 => K::Luks2,
            _ => K::Other,
        });
    }
    None
}

/// Short display token for a source device (mirrors core's source_token):
/// a drive letter, `V`+8 hex of a volume GUID, or `P<disk><offsetHex>`.
pub(crate) fn disk_source_token(device: &str) -> String {
    let d = device
        .trim_start_matches(r"\\.\")
        .trim_start_matches(r"\\?\");
    if let Some(rest) = d.strip_prefix("part:") {
        let mut it = rest.split(':');
        if let (Some(disk), Some(off)) = (it.next(), it.next()) {
            if let (Ok(dn), Ok(o)) = (disk.parse::<u32>(), off.parse::<u64>()) {
                return format!("P{dn}{:X}", o);
            }
        }
    }
    let bytes = d.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return bytes[0].to_ascii_uppercase().to_string();
    }
    if let Some(pos) = d.find("Volume{") {
        let hex: String = d[pos + 7..].chars().take(8).collect();
        return format!("V{}", hex.to_uppercase());
    }
    "X".into()
}

/// One partition region inside the whole-disk image: its byte offset on the
/// disk, its decrypted length, and a reader for its (decrypted) volume data.
pub(crate) struct DiskRegion {
    pub(crate) part_offset: u64,
    pub(crate) volume_len: u64,
    pub(crate) reader: Box<dyn crate::fat_map::RawVolume + Send>,
    /// Excluded byte ranges (VOLUME-relative): zeroed after reading so excluded
    /// file data never enters the image (plan D2).
    pub(crate) zero_ranges: ByteRanges,
    /// FAT/exFAT metadata patches (VOLUME-relative offset → bytes): applied over
    /// the read data so excluded FAT files vanish from the directory/allocation.
    pub(crate) patches: Vec<(u64, Vec<u8>)>,
}

/// A [`BlockReader`] over the WHOLE disk image: block `N` (bytes `N*bs..`) is
/// assembled from the GPT write-ops and the partition regions at their real
/// offsets; bytes belonging to nothing read as zeros. This lets partitions sit
/// at any sector offset while the checksum grid stays the image's own blocks.
pub(crate) struct DiskCompositeReader {
    pub(crate) block_size: u32,
    pub(crate) image_size: u64,
    pub(crate) gpt_ops: Vec<(u64, Vec<u8>)>,
    pub(crate) regions: Vec<DiskRegion>,
}

impl BlockReader for DiskCompositeReader {
    fn read_block(&mut self, n: u64) -> Result<Vec<u8>> {
        let bs = self.block_size as u64;
        let start = n * bs;
        let len = bs.min(self.image_size - start) as usize;
        let end = start + len as u64;
        let mut buf = vec![0u8; len];
        // GPT bytes (protective MBR, primary/backup headers + entry arrays).
        for (off, bytes) in &self.gpt_ops {
            let g_end = off + bytes.len() as u64;
            let from = start.max(*off);
            let to = end.min(g_end);
            if from < to {
                buf[(from - start) as usize..(to - start) as usize]
                    .copy_from_slice(&bytes[(from - off) as usize..(to - off) as usize]);
            }
        }
        // Partition data (overwrites any overlap — partitions never overlap the
        // GPT head/tail in a valid layout, so order is immaterial).
        for r in &mut self.regions {
            let p_end = r.part_offset + r.volume_len;
            let from = start.max(r.part_offset);
            let to = end.min(p_end);
            if from < to {
                let vol_off = from - r.part_offset;
                let dst = &mut buf[(from - start) as usize..(to - start) as usize];
                r.reader.read_at(vol_off, dst)?;
                // Plan D2 — content zeroing: excluded file clusters read as zero
                // so their data never enters the image.
                r.zero_ranges.zero_overlaps(vol_off, dst);
                // FAT/exFAT metadata patches (deleted dir entries / freed
                // chains), applied over the same volume-relative window.
                let dst_end = vol_off + dst.len() as u64;
                for (poff, bytes) in &r.patches {
                    let p_start = *poff;
                    let p_end2 = poff + bytes.len() as u64;
                    let ov_start = vol_off.max(p_start);
                    let ov_end = dst_end.min(p_end2);
                    if ov_start < ov_end {
                        let d0 = (ov_start - vol_off) as usize;
                        let d1 = (ov_end - vol_off) as usize;
                        let b0 = (ov_start - p_start) as usize;
                        let b1 = b0 + (d1 - d0);
                        dst[d0..d1].copy_from_slice(&bytes[b0..b1]);
                    }
                }
            }
        }
        Ok(buf)
    }
    fn block_size(&self) -> u32 {
        self.block_size
    }
    fn total_blocks(&self) -> u64 {
        self.image_size.div_ceil(self.block_size as u64)
    }
}

/// Whole-disk used-map: a disk-image block is "used" if it overlaps a used
/// block of any partition (straddling blocks count once).
pub(crate) struct DiskUsedMap {
    used: Vec<bool>,
    pub(crate) count: u64,
}

impl DiskUsedMap {
    /// `parts`: each partition's byte offset on the disk, its volume length
    /// and its own used-map.
    pub(crate) fn build(
        parts: &[(u64, u64, &dyn UsedSectorMap)],
        bs: u64,
        total_blocks: u64,
    ) -> Self {
        let mut used = vec![false; total_blocks as usize];
        for &(part_offset, volume_len, part_used) in parts {
            let part_end = part_offset + volume_len;
            let vblocks = volume_len.div_ceil(bs);
            for vb in 0..vblocks {
                if part_used.is_block_used(vb) {
                    let d0 = part_offset + vb * bs;
                    let d1 = part_end.min(d0 + bs); // this volume block's disk byte span
                    for db in d0 / bs..=(d1 - 1) / bs {
                        if let Some(slot) = used.get_mut(db as usize) {
                            *slot = true;
                        }
                    }
                }
            }
        }
        let count = used.iter().filter(|&&u| u).count() as u64;
        Self { used, count }
    }
}

impl UsedSectorMap for DiskUsedMap {
    fn is_block_used(&self, block_number: u64) -> bool {
        self.used
            .get(block_number as usize)
            .copied()
            .unwrap_or(false)
    }
    fn used_block_count(&self) -> u64 {
        self.count
    }
}

#[cfg(test)]
mod tests {
    use own_backup_common::image::VolumeEncryptionKind as K;

    use super::*;

    /// A partition's own used-map: the listed volume blocks.
    struct Used(Vec<u64>);

    impl UsedSectorMap for Used {
        fn is_block_used(&self, block_number: u64) -> bool {
            self.0.contains(&block_number)
        }
        fn used_block_count(&self) -> u64 {
            self.0.len() as u64
        }
    }

    struct MemVolume(Vec<u8>);

    impl crate::fat_map::RawVolume for MemVolume {
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
            buf.copy_from_slice(&self.0[offset as usize..offset as usize + buf.len()]);
            Ok(())
        }
        fn len(&self) -> u64 {
            self.0.len() as u64
        }
    }

    const BS: u64 = 4096;

    #[test]
    fn a_volume_block_marks_every_disk_block_it_straddles() {
        // Partition A starts at LBA 34 (17408 = 4.25 blocks) and ends half a
        // block into its fourth volume block; B is block-aligned.
        let a = Used(vec![0, 3]);
        let b = Used(vec![1]);
        let parts: [(u64, u64, &dyn UsedSectorMap); 2] =
            [(17408, 3 * BS + 512, &a), (16 * BS, 2 * BS, &b)];

        let map = DiskUsedMap::build(&parts, BS, 20);
        // A0 = [17408, 21504) → disk blocks 4 and 5; A3 is cut short by the
        // partition end → block 7 only; B1 → block 17.
        let used: Vec<u64> = (0..20).filter(|&n| map.is_block_used(n)).collect();
        assert_eq!(used, vec![4, 5, 7, 17]);
        assert_eq!(map.used_block_count(), 4);
        assert!(!map.is_block_used(1000), "past the map is not used");

        // A block beyond the planned total is left out, not a panic.
        let short = DiskUsedMap::build(&parts, BS, 17);
        assert_eq!(short.count, 3);
    }

    #[test]
    fn neighbours_sharing_a_disk_block_count_it_once() {
        // A ends and B starts inside disk block 1.
        let a = Used(vec![0]);
        let b = Used(vec![0]);
        let parts: [(u64, u64, &dyn UsedSectorMap); 2] = [(BS / 2, BS, &a), (BS + BS / 2, BS, &b)];
        let map = DiskUsedMap::build(&parts, BS, 4);
        let used: Vec<u64> = (0..4).filter(|&n| map.is_block_used(n)).collect();
        assert_eq!(used, vec![0, 1, 2]);
        assert_eq!(map.count, 3);
    }

    #[test]
    fn the_composite_reader_assembles_gpt_partition_exclusions_and_patches() {
        let image_size = 5 * BS + 1000; // the last block is short
        let part_offset = BS + BS / 2; // not block-aligned
        let volume: Vec<u8> = (0..2 * BS).map(|i| (i % 251) as u8 + 1).collect();
        let mut zero = ByteRanges::new();
        zero.add(100, 150);
        zero.finalize();
        // A patch that crosses a disk-block boundary (volume 2400 = disk 8544).
        let patch = (2400u64, vec![0xAB; 200]);
        let gpt = vec![(512u64, b"EFI PART".to_vec()), (BS - 4, vec![0xEE; 8])];

        // The image as it must come out, built byte by byte.
        let mut want = vec![0u8; image_size as usize];
        for (off, bytes) in &gpt {
            want[*off as usize..*off as usize + bytes.len()].copy_from_slice(bytes);
        }
        let mut part = volume.clone();
        part[100..150].fill(0);
        part[2400..2600].copy_from_slice(&patch.1);
        want[part_offset as usize..(part_offset + 2 * BS) as usize].copy_from_slice(&part);

        let mut reader = DiskCompositeReader {
            block_size: BS as u32,
            image_size,
            gpt_ops: gpt,
            regions: vec![DiskRegion {
                part_offset,
                volume_len: 2 * BS,
                reader: Box::new(MemVolume(volume)),
                zero_ranges: zero,
                patches: vec![patch],
            }],
        };
        assert_eq!(reader.total_blocks(), 6);
        let mut got = Vec::new();
        for n in 0..reader.total_blocks() {
            got.extend(reader.read_block(n).unwrap());
        }
        assert_eq!(got.len(), want.len(), "the last block is the image's tail");
        assert!(got == want, "assembled image differs");
    }

    #[test]
    fn the_first_sectors_say_which_container_it_is() {
        let mut luks = vec![0u8; 4096];
        luks[..6].copy_from_slice(b"LUKS\xba\xbe");
        luks[6..8].copy_from_slice(&1u16.to_be_bytes());
        assert_eq!(container_kind(&luks), Some(K::Luks1));
        luks[6..8].copy_from_slice(&2u16.to_be_bytes());
        assert_eq!(container_kind(&luks), Some(K::Luks2));
        luks[6..8].copy_from_slice(&7u16.to_be_bytes());
        assert_eq!(container_kind(&luks), Some(K::Other));

        let mut bitlocker = vec![0u8; 4096];
        bitlocker[3..11].copy_from_slice(b"-FVE-FS-");
        assert_eq!(container_kind(&bitlocker), Some(K::BitLocker));

        let mut ntfs = vec![0u8; 4096];
        ntfs[3..11].copy_from_slice(b"NTFS    ");
        assert_eq!(container_kind(&ntfs), None);
    }

    // Not pinned here: a drive letter. `disk_source_token("C:")` gives "67" —
    // the letter's byte value, not "C" as its comment promises — and this
    // test is not the place to make that Windows output official, nor the
    // move to change it.
    #[test]
    fn a_source_device_is_listed_under_a_short_token() {
        assert_eq!(
            disk_source_token(r"\\?\Volume{1234abcd-0000-0000-0000-000000000000}\"),
            "V1234ABCD"
        );
        assert_eq!(disk_source_token("part:1:1048576"), "P1100000");
        assert_eq!(disk_source_token(r"\\.\PhysicalDrive0"), "X");
    }
}
