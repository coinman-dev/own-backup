use own_backup_common::error::Result;
use own_backup_common::image::BlockIndexEntry;

use crate::block_reader::BlockReader;
use crate::checksum::compute_block_checksum;

/// Detects which blocks have changed between a previous backup and the
/// current state of the source device.
pub struct IncrementalEngine;

impl IncrementalEngine {
    /// Read every block from `reader`, compute its checksum, and compare with
    /// `previous_index`. Returns the list of block numbers whose checksum
    /// differs from (or is absent in) the previous index.
    ///
    /// This is the simplest change-detection strategy: full re-read of all
    /// blocks with checksum comparison. More sophisticated approaches (e.g.
    /// USN journal, block change tracking drivers) can be added later.
    pub fn detect_changed_blocks(
        reader: &mut dyn BlockReader,
        previous_index: &[BlockIndexEntry],
    ) -> Result<Vec<u64>> {
        // Build a lookup map: block_number -> checksum from the previous backup.
        let prev_checksums: std::collections::HashMap<u64, u64> = previous_index
            .iter()
            .map(|e| (e.block_number, e.checksum))
            .collect();

        let total = reader.total_blocks();
        let mut changed = Vec::new();

        for block_number in 0..total {
            let data = reader.read_block(block_number)?;
            let current_checksum = compute_block_checksum(&data);

            let is_changed = match prev_checksums.get(&block_number) {
                Some(&prev_checksum) => current_checksum != prev_checksum,
                None => true, // Block not in previous backup; treat as new.
            };

            if is_changed {
                changed.push(block_number);
            }
        }

        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_reader::MockBlockReader;
    use crate::checksum::compute_block_checksum;

    fn make_entry(block_number: u64, checksum: u64) -> BlockIndexEntry {
        BlockIndexEntry {
            block_number,
            offset_in_file: 0,
            original_size: 256,
            compressed_size: 128,
            is_compressed: true,
            checksum,
        }
    }

    #[test]
    fn no_changes_detected() {
        let data = vec![0xAAu8; 512];
        let checksum_0 = compute_block_checksum(&data[..256]);
        let checksum_1 = compute_block_checksum(&data[256..]);

        let previous = vec![
            make_entry(0, checksum_0),
            make_entry(1, checksum_1),
        ];

        let mut reader = MockBlockReader::new(data, 256);
        let changed =
            IncrementalEngine::detect_changed_blocks(&mut reader, &previous).unwrap();
        assert!(changed.is_empty());
    }

    #[test]
    fn one_block_changed() {
        let original = vec![0xAAu8; 512];
        let checksum_0 = compute_block_checksum(&original[..256]);
        let checksum_1 = compute_block_checksum(&original[256..]);

        let previous = vec![
            make_entry(0, checksum_0),
            make_entry(1, checksum_1),
        ];

        // Modify the second block.
        let mut modified = original;
        modified[256] = 0xBB;

        let mut reader = MockBlockReader::new(modified, 256);
        let changed =
            IncrementalEngine::detect_changed_blocks(&mut reader, &previous).unwrap();
        assert_eq!(changed, vec![1]);
    }

    #[test]
    fn new_blocks_are_changed() {
        let data = vec![0xAAu8; 768]; // 3 blocks
        let checksum_0 = compute_block_checksum(&data[..256]);
        let checksum_1 = compute_block_checksum(&data[256..512]);

        // Previous backup only had 2 blocks.
        let previous = vec![
            make_entry(0, checksum_0),
            make_entry(1, checksum_1),
        ];

        let mut reader = MockBlockReader::new(data, 256);
        let changed =
            IncrementalEngine::detect_changed_blocks(&mut reader, &previous).unwrap();
        // Block 2 is new.
        assert_eq!(changed, vec![2]);
    }

    #[test]
    fn empty_previous_means_all_changed() {
        let data = vec![0xAAu8; 512];
        let mut reader = MockBlockReader::new(data, 256);
        let changed =
            IncrementalEngine::detect_changed_blocks(&mut reader, &[]).unwrap();
        assert_eq!(changed, vec![0, 1]);
    }
}
