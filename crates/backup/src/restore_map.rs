//! Working out *which* image bytes belong to *which* target bytes (R4a).
//!
//! An image comes in two shapes and the restore has to read both:
//!
//! - A **volume image** holds one volume. Block N lives at
//!   `volume_data_offset + N × block_size`, and block 0 is the start of the
//!   volume.
//! - A **whole-disk image** holds a disk. `volume_data_offset` is 0 and block N
//!   sits at the offset it had on the source disk — so extracting one partition
//!   means taking only the blocks that fall inside that partition's LBA range,
//!   and rebasing them to the target partition's start.
//!
//! The awkward part is that block boundaries and partition boundaries do not
//! have to line up. A 1 MiB-aligned partition and a 1 MiB block size agree, but
//! an EFI System Partition starting at LBA 34 — legal GPT, and what the source
//! disk in front of me actually does — does not. So the first and last blocks
//! of a partition can be partial, and a copy plan has to say which *slice* of
//! the block to write. Checksums are still verified over the whole block,
//! because that is what they were computed over.
//!
//! This module is pure arithmetic: no I/O, no platform. [`crate::restore_win`]
//! does the reading and writing.

use own_backup_common::{tr, OwnBackupError, Result};

/// The stretch of the image's address space that belongs to one source volume.
///
/// Addresses are relative to `volume_data_offset`, i.e. block N starts at
/// `N × block_size` — the same space the sidecar's block numbers live in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceWindow {
    pub start: u64,
    pub len: u64,
}

impl SourceWindow {
    pub fn end(&self) -> u64 {
        self.start + self.len
    }

    /// The whole image, for a volume image where the image *is* the volume.
    pub fn whole(len: u64) -> Self {
        Self { start: 0, len }
    }

    /// One partition of a whole-disk image, from its GPT entry.
    pub fn partition(first_lba: u64, last_lba: u64, sector_size: u32) -> Self {
        let sector = sector_size.max(512) as u64;
        Self {
            start: first_lba * sector,
            len: (last_lba + 1 - first_lba) * sector,
        }
    }
}

/// What to do with one stored block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockCopy {
    /// Byte offset in the image file to read from.
    pub image_offset: u64,
    /// How many bytes of this block actually exist (the last block is short).
    pub block_len: usize,
    /// Where inside the block the wanted slice starts.
    pub slice_start: usize,
    pub slice_len: usize,
    /// Offset within the *target partition* to write the slice to.
    pub dst_offset: u64,
}

impl BlockCopy {
    pub fn dst_end(&self) -> u64 {
        self.dst_offset + self.slice_len as u64
    }
}

/// Work out what block `block_number` contributes to the target, or `None` when
/// it falls entirely outside `window`.
///
/// `image_len` is the image's logical size — the last block is short by however
/// much it overhangs.
pub fn plan_block_copy(
    block_number: u64,
    block_size: u64,
    image_len: u64,
    data_offset: u64,
    window: SourceWindow,
) -> Option<BlockCopy> {
    let a_start = block_number.checked_mul(block_size)?;
    if a_start >= image_len {
        return None; // a block past the end of the image: nothing to read
    }
    let a_end = (a_start + block_size).min(image_len);

    let overlap_start = a_start.max(window.start);
    let overlap_end = a_end.min(window.end());
    if overlap_start >= overlap_end {
        return None;
    }

    Some(BlockCopy {
        image_offset: data_offset + a_start,
        block_len: (a_end - a_start) as usize,
        slice_start: (overlap_start - a_start) as usize,
        slice_len: (overlap_end - overlap_start) as usize,
        dst_offset: overlap_start - window.start,
    })
}

/// Check the copy fits the target partition. A slice running past the end means
/// the plan and the partition table disagree, which the dry run should have
/// caught — reaching this point means writing past the partition into whatever
/// follows it.
pub fn check_fits(copy: &BlockCopy, dst_size: u64) -> Result<()> {
    if copy.dst_end() > dst_size {
        return Err(OwnBackupError::RestoreIncompatible(
            tr!(
                "restore_map.block_does_not_fit",
                start = copy.dst_offset,
                end = copy.dst_end(),
                size = dst_size
            )
            .into_owned(),
        ));
    }
    Ok(())
}

/// Length of the decrypted payload a copy holds for one encrypted partition
/// (D19) — the number of bytes that have to be re-encrypted on restore.
///
/// The two image shapes measure their window differently, and the difference
/// is exactly one header:
///
/// - a **whole-disk** image's window is the partition's full GPT slot, header
///   included; the backup wrote the decrypted payload starting at the
///   partition's first byte and simply never touched the tail, so the payload
///   is the window minus the header;
/// - a **per-volume** image's window *is* the payload — `source_disk.size_bytes`
///   was already computed as `device_len - data_offset` at backup time.
///
/// Getting this wrong is not loud: taking the window's length in the whole-disk
/// case makes the restore demand a target one header larger than the source
/// partition, and refuse a same-size disk for no visible reason.
pub fn payload_len(window_len: u64, data_offset: u64, whole_disk_image: bool) -> Result<u64> {
    if !whole_disk_image {
        return Ok(window_len);
    }
    window_len.checked_sub(data_offset).ok_or_else(|| {
        OwnBackupError::ImageFormat(
            tr!(
                "restore_map.partition_smaller_than_header",
                window = window_len,
                header = data_offset
            )
            .into_owned(),
        )
    })
}

/// The stretches of the target that no block covers.
///
/// A sector restore writes only the blocks the backup stored — the gaps between
/// them keep whatever the disk held before. The filesystem cannot reach that
/// data, since the used-block map arrives from the image, but the bytes are
/// physically there. This computes the complement so
/// `wipe_free_space` can zero it (D16).
///
/// `covered` need not be sorted or disjoint.
pub fn uncovered_ranges(covered: &mut [(u64, u64)], total: u64) -> Vec<(u64, u64)> {
    covered.sort_unstable();
    let mut gaps = Vec::new();
    let mut cursor = 0u64;
    for &(start, len) in covered.iter() {
        if start > cursor {
            gaps.push((cursor, start - cursor));
        }
        cursor = cursor.max(start + len);
    }
    if cursor < total {
        gaps.push((cursor, total - cursor));
    }
    gaps
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn a_volume_image_maps_block_zero_to_the_start_of_the_target() {
        let w = SourceWindow::whole(100 * MIB);
        let c = plan_block_copy(0, MIB, 100 * MIB, MIB, w).unwrap();
        assert_eq!(c.image_offset, MIB, "block 0 sits at volume_data_offset");
        assert_eq!(c.block_len, MIB as usize);
        assert_eq!(c.slice_start, 0);
        assert_eq!(c.slice_len, MIB as usize);
        assert_eq!(c.dst_offset, 0);
    }

    #[test]
    fn the_last_block_of_an_image_is_short() {
        // A 100.5 MiB volume: block 100 holds half a mebibyte.
        let len = 100 * MIB + MIB / 2;
        let w = SourceWindow::whole(len);
        let c = plan_block_copy(100, MIB, len, 0, w).unwrap();
        assert_eq!(c.block_len, (MIB / 2) as usize);
        assert_eq!(c.slice_len, (MIB / 2) as usize);
    }

    #[test]
    fn a_block_past_the_end_of_the_image_is_skipped() {
        let w = SourceWindow::whole(10 * MIB);
        assert_eq!(plan_block_copy(10, MIB, 10 * MIB, 0, w), None);
    }

    #[test]
    fn a_whole_disk_image_rebases_a_partition_to_the_target_start() {
        // Partition at 1 MiB on the source disk, restored to offset 0 of its
        // target partition.
        let w = SourceWindow::partition(2048, 2048 + 10 * MIB / 512 - 1, 512);
        assert_eq!(w.start, MIB);
        assert_eq!(w.len, 10 * MIB);

        let c = plan_block_copy(1, MIB, 500 * MIB, 0, w).unwrap();
        assert_eq!(
            c.image_offset, MIB,
            "read from where it sat on the source disk"
        );
        assert_eq!(
            c.dst_offset, 0,
            "…and write to the start of the target partition"
        );
    }

    #[test]
    fn blocks_outside_the_partition_are_skipped() {
        let w = SourceWindow::partition(2048, 2048 + 10 * MIB / 512 - 1, 512);
        assert_eq!(
            plan_block_copy(0, MIB, 500 * MIB, 0, w),
            None,
            "the GPT head"
        );
        assert_eq!(
            plan_block_copy(11, MIB, 500 * MIB, 0, w),
            None,
            "past the partition"
        );
        assert!(
            plan_block_copy(10, MIB, 500 * MIB, 0, w).is_some(),
            "the last block inside"
        );
    }

    #[test]
    fn a_partition_not_aligned_to_the_block_size_gets_partial_first_and_last_blocks() {
        // LBA 34 — legal GPT, and exactly where the source disk in front of me
        // puts its EFI System Partition. Block 0 contributes only its tail.
        let first_lba = 34u64;
        let sector = 512u64;
        let start = first_lba * sector; // 17408
        let w = SourceWindow::partition(first_lba, first_lba + 200 - 1, 512);

        let first = plan_block_copy(0, MIB, 500 * MIB, 0, w).unwrap();
        assert_eq!(
            first.slice_start, start as usize,
            "skip the GPT ahead of it"
        );
        assert_eq!(first.slice_len, (200 * sector) as usize);
        assert_eq!(first.dst_offset, 0);
        // The checksum still covers the whole block, so the read is whole too.
        assert_eq!(first.block_len, MIB as usize);
    }

    #[test]
    fn a_partition_spanning_a_block_boundary_splits_correctly() {
        // Starts at 17408 and runs 2 MiB — so it touches blocks 0, 1 and 2.
        let first_lba = 34u64;
        let w = SourceWindow::partition(first_lba, first_lba + 2 * MIB / 512 - 1, 512);
        let copies: Vec<BlockCopy> = (0..4)
            .filter_map(|n| plan_block_copy(n, MIB, 500 * MIB, 0, w))
            .collect();
        assert_eq!(copies.len(), 3);
        // The slices tile the target exactly, with no gap and no overlap.
        assert_eq!(copies[0].dst_offset, 0);
        assert_eq!(copies[0].dst_end(), copies[1].dst_offset);
        assert_eq!(copies[1].dst_end(), copies[2].dst_offset);
        assert_eq!(copies[2].dst_end(), w.len);
    }

    #[test]
    fn a_copy_running_past_the_target_partition_is_refused() {
        let c = BlockCopy {
            image_offset: 0,
            block_len: MIB as usize,
            slice_start: 0,
            slice_len: MIB as usize,
            dst_offset: 9 * MIB,
        };
        assert!(check_fits(&c, 10 * MIB).is_ok());
        assert!(check_fits(&c, 9 * MIB).is_err());
    }

    #[test]
    fn a_whole_disk_window_holds_the_header_as_well_as_the_payload() {
        // 128 MiB partition with a 16 MiB LUKS2 header: 112 MiB of payload.
        assert_eq!(payload_len(128 * MIB, 16 * MIB, true).unwrap(), 112 * MIB);
    }

    #[test]
    fn a_volume_image_window_is_already_the_payload() {
        // Here the backup subtracted the header when it measured the volume,
        // so subtracting it again would lose a header's worth of data.
        assert_eq!(payload_len(112 * MIB, 16 * MIB, false).unwrap(), 112 * MIB);
    }

    #[test]
    fn a_partition_smaller_than_its_own_header_is_refused() {
        assert!(payload_len(8 * MIB, 16 * MIB, true).is_err());
    }

    #[test]
    fn uncovered_ranges_finds_the_gaps_between_written_blocks() {
        let mut covered = vec![(0, MIB), (3 * MIB, MIB)];
        let gaps = uncovered_ranges(&mut covered, 6 * MIB);
        assert_eq!(gaps, vec![(MIB, 2 * MIB), (4 * MIB, 2 * MIB)]);
    }

    #[test]
    fn uncovered_ranges_handles_unsorted_and_touching_input() {
        let mut covered = vec![(2 * MIB, MIB), (0, MIB), (MIB, MIB)];
        assert_eq!(
            uncovered_ranges(&mut covered, 4 * MIB),
            vec![(3 * MIB, MIB)]
        );
    }

    #[test]
    fn a_fully_covered_target_has_no_gaps() {
        let mut covered = vec![(0, 4 * MIB)];
        assert!(uncovered_ranges(&mut covered, 4 * MIB).is_empty());
    }

    #[test]
    fn an_empty_image_leaves_the_whole_target_uncovered() {
        let mut covered = vec![];
        assert_eq!(uncovered_ranges(&mut covered, 4 * MIB), vec![(0, 4 * MIB)]);
    }
}
