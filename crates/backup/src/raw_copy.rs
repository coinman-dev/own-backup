//! Generic used-block copy loop shared by the VHDX backup and restore engines.
//!
//! Reads every used block from a [`BlockReader`], hashes it and hands it to a
//! writer callback. Platform engines wire this to real devices; tests run it
//! on mocks.

use std::collections::HashMap;
use std::time::Instant;

use own_backup_common::progress::BackupProgress;
use own_backup_common::types::BackupStatus;
use own_backup_common::Result;

use crate::block_reader::BlockReader;
use crate::checksum::compute_block_checksum;
use crate::sidecar::BlockChecksum;
use crate::used_sectors::UsedSectorMap;

/// Result of a completed copy pass.
pub struct CopyOutcome {
    /// Blocks actually written to the image. For incremental/differential
    /// passes unchanged blocks are skipped, so this can be far below the
    /// number of used blocks.
    pub blocks_copied: u64,
    /// Bytes actually written to the image.
    pub bytes_copied: u64,
    /// Used blocks whose checksum matched the parent and were skipped.
    pub blocks_unchanged: u64,
    /// xxHash64 of EVERY used block (written or skipped), ordered by block
    /// number — the full state of the volume at backup time. An incremental
    /// sidecar therefore describes the complete chain-resolved image, and
    /// verify/restore work on it exactly like on a full backup.
    pub checksums: Vec<BlockChecksum>,
}

/// Copy used blocks from `reader` into `write_block`, collecting per-block
/// checksums.
///
/// - `write_block(block_number, data)` receives each block to store, in
///   ascending block order.
/// - `parent_checksums`: previous-version block hashes for incremental /
///   differential passes. A used block whose hash matches is NOT written (the
///   differencing image resolves it from the parent chain) but IS recorded in
///   the outcome checksums. `None` = full copy.
/// - `progress` is invoked after every scanned block; returning `Err` (e.g.
///   [`own_backup_common::OwnBackupError::Cancelled`]) aborts the copy.
pub fn copy_used_blocks(
    backup_id: &str,
    reader: &mut dyn BlockReader,
    used: &dyn UsedSectorMap,
    parent_checksums: Option<&HashMap<u64, u64>>,
    mut write_block: impl FnMut(u64, &[u8]) -> Result<()>,
    mut progress: impl FnMut(&BackupProgress) -> Result<()>,
) -> Result<CopyOutcome> {
    let total_blocks = reader.total_blocks();
    let used_blocks = used.used_block_count();

    let started = Instant::now();
    let mut blocks_scanned: u64 = 0;
    let mut blocks_copied: u64 = 0;
    let mut bytes_copied: u64 = 0;
    let mut bytes_scanned: u64 = 0;
    let mut blocks_unchanged: u64 = 0;
    let mut checksums = Vec::with_capacity(used_blocks as usize);

    for block_number in 0..total_blocks {
        if !used.is_block_used(block_number) {
            continue;
        }

        let data = reader.read_block(block_number)?;
        let checksum = compute_block_checksum(&data);

        let unchanged = parent_checksums
            .and_then(|prev| prev.get(&block_number))
            .is_some_and(|&prev| prev == checksum);
        if unchanged {
            blocks_unchanged += 1;
        } else {
            write_block(block_number, &data)?;
            blocks_copied += 1;
            bytes_copied += data.len() as u64;
        }

        blocks_scanned += 1;
        bytes_scanned += data.len() as u64;
        checksums.push(BlockChecksum {
            block_number,
            checksum,
        });

        let elapsed = started.elapsed().as_secs_f64();
        let speed_bps = if elapsed > 0.0 {
            (bytes_scanned as f64 / elapsed) as u64
        } else {
            0
        };
        progress(&BackupProgress {
            backup_id: backup_id.to_string(),
            status: BackupStatus::Running,
            blocks_done: blocks_scanned,
            blocks_total: used_blocks,
            bytes_written: bytes_copied,
            speed_bps,
            categories: None,
            excluded: None,
        })?;
    }

    Ok(CopyOutcome {
        blocks_copied,
        bytes_copied,
        blocks_unchanged,
        checksums,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_reader::MockBlockReader;
    use crate::used_sectors::{AllUsedMap, UsedSectorMap};
    use own_backup_common::OwnBackupError;

    struct EvenBlocksMap {
        total: u64,
    }
    impl UsedSectorMap for EvenBlocksMap {
        fn is_block_used(&self, n: u64) -> bool {
            n < self.total && n % 2 == 0
        }
        fn used_block_count(&self) -> u64 {
            self.total.div_ceil(2)
        }
    }

    #[test]
    fn copies_all_used_blocks_in_order() {
        let data: Vec<u8> = (0..1024u32).map(|i| (i % 251) as u8).collect();
        let mut reader = MockBlockReader::new(data.clone(), 256);
        let used = AllUsedMap::new(4);

        let mut written: Vec<(u64, Vec<u8>)> = Vec::new();
        let outcome = copy_used_blocks(
            "test",
            &mut reader,
            &used,
            None,
            |n, d| {
                written.push((n, d.to_vec()));
                Ok(())
            },
            |_| Ok(()),
        )
        .unwrap();

        assert_eq!(outcome.blocks_copied, 4);
        assert_eq!(outcome.bytes_copied, 1024);
        assert_eq!(outcome.checksums.len(), 4);
        assert_eq!(
            written.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        // Data integrity: concatenated written blocks == source.
        let concat: Vec<u8> = written.into_iter().flat_map(|(_, d)| d).collect();
        assert_eq!(concat, data);
    }

    #[test]
    fn skips_unused_blocks() {
        let data = vec![7u8; 1024];
        let mut reader = MockBlockReader::new(data, 256);
        let used = EvenBlocksMap { total: 4 };

        let mut written = Vec::new();
        let outcome = copy_used_blocks(
            "test",
            &mut reader,
            &used,
            None,
            |n, _| {
                written.push(n);
                Ok(())
            },
            |_| Ok(()),
        )
        .unwrap();

        assert_eq!(written, vec![0, 2]);
        assert_eq!(outcome.blocks_copied, 2);
        assert_eq!(outcome.checksums.len(), 2);
        assert_eq!(outcome.checksums[0].block_number, 0);
        assert_eq!(outcome.checksums[1].block_number, 2);
    }

    #[test]
    fn progress_error_aborts() {
        let data = vec![1u8; 1024];
        let mut reader = MockBlockReader::new(data, 256);
        let used = AllUsedMap::new(4);

        let result = copy_used_blocks(
            "test",
            &mut reader,
            &used,
            None,
            |_, _| Ok(()),
            |p| {
                if p.blocks_done >= 2 {
                    Err(OwnBackupError::Cancelled)
                } else {
                    Ok(())
                }
            },
        );
        assert!(matches!(result, Err(OwnBackupError::Cancelled)));
    }

    #[test]
    fn checksums_match_recomputation() {
        let data: Vec<u8> = (0..512u32).map(|i| (i * 7 % 256) as u8).collect();
        let mut reader = MockBlockReader::new(data.clone(), 128);
        let used = AllUsedMap::new(4);

        let outcome =
            copy_used_blocks("t", &mut reader, &used, None, |_, _| Ok(()), |_| Ok(())).unwrap();

        for entry in &outcome.checksums {
            let start = entry.block_number as usize * 128;
            let expected = compute_block_checksum(&data[start..start + 128]);
            assert_eq!(entry.checksum, expected);
        }
    }

    #[test]
    fn incremental_skips_unchanged_blocks() {
        // Previous state: 4 blocks of 0xAA. Current: block 2 modified.
        let mut data = vec![0xAAu8; 1024];
        data[512] = 0xBB;
        let unchanged_hash = compute_block_checksum(&vec![0xAAu8; 256]);
        let prev: HashMap<u64, u64> = (0..4u64).map(|n| (n, unchanged_hash)).collect();

        let mut reader = MockBlockReader::new(data.clone(), 256);
        let used = AllUsedMap::new(4);

        let mut written = Vec::new();
        let outcome = copy_used_blocks(
            "inc",
            &mut reader,
            &used,
            Some(&prev),
            |n, _| {
                written.push(n);
                Ok(())
            },
            |_| Ok(()),
        )
        .unwrap();

        assert_eq!(written, vec![2]);
        assert_eq!(outcome.blocks_copied, 1);
        assert_eq!(outcome.blocks_unchanged, 3);
        assert_eq!(outcome.bytes_copied, 256);
        // Checksums still describe the FULL state — all 4 used blocks.
        assert_eq!(outcome.checksums.len(), 4);
        assert_eq!(
            outcome.checksums[2].checksum,
            compute_block_checksum(&data[512..768])
        );
    }

    #[test]
    fn incremental_writes_blocks_missing_from_parent() {
        // Parent knew nothing about block 1 (was unused then).
        let data = vec![0xAAu8; 512];
        let hash = compute_block_checksum(&data[..256]);
        let prev: HashMap<u64, u64> = [(0u64, hash)].into_iter().collect();

        let mut reader = MockBlockReader::new(data, 256);
        let used = AllUsedMap::new(2);

        let mut written = Vec::new();
        let outcome = copy_used_blocks(
            "inc",
            &mut reader,
            &used,
            Some(&prev),
            |n, _| {
                written.push(n);
                Ok(())
            },
            |_| Ok(()),
        )
        .unwrap();

        assert_eq!(written, vec![1]);
        assert_eq!(outcome.blocks_unchanged, 1);
        assert_eq!(outcome.checksums.len(), 2);
    }
}
