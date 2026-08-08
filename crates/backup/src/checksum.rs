use xxhash_rust::xxh64;

/// Compute the xxHash64 checksum of a data block using seed 0.
pub fn compute_block_checksum(data: &[u8]) -> u64 {
    xxh64::xxh64(data, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_value() {
        // xxHash64 of empty input with seed 0 is a well-known constant.
        let hash = compute_block_checksum(&[]);
        assert_eq!(hash, 0xEF46DB3751D8E999);
    }

    #[test]
    fn determinism() {
        let data = b"The quick brown fox jumps over the lazy dog";
        let h1 = compute_block_checksum(data);
        let h2 = compute_block_checksum(data);
        assert_eq!(h1, h2);
    }

    #[test]
    fn empty_input() {
        let _ = compute_block_checksum(&[]);
    }

    #[test]
    fn different_data_different_hash() {
        let h1 = compute_block_checksum(b"block A");
        let h2 = compute_block_checksum(b"block B");
        assert_ne!(h1, h2);
    }
}

/// Group ascending block numbers into contiguous runs of at most `max_blocks`.
///
/// The re-checksum passes read the image one backup block at a time, and on a
/// spinning disk that is what they cost: a 3467-block pass took 255 s (~73 ms
/// per block) against 84 s for the merge that wrote the same data, because
/// every read is a separate seek with `FILE_FLAG_NO_BUFFERING` — no readahead
/// to save it (measured on the boot medium, 2026-08-08). The blocks are far
/// from random, though: zeroing a 1.9 GiB range touches one long stretch of
/// them. Reading such a stretch in one go turns thousands of seeks into a few
/// sequential transfers.
///
/// Returns `(first_block, count)` pairs in ascending order. `max_blocks` caps
/// the read buffer; a gap of even one block ends the run, so nothing outside
/// the caller's set is ever read.
pub fn coalesce_block_runs(
    blocks: impl IntoIterator<Item = u64>,
    max_blocks: u64,
) -> Vec<(u64, u64)> {
    let max_blocks = max_blocks.max(1);
    let mut runs: Vec<(u64, u64)> = Vec::new();
    for n in blocks {
        match runs.last_mut() {
            Some((first, count)) if *first + *count == n && *count < max_blocks => *count += 1,
            _ => runs.push((n, 1)),
        }
    }
    runs
}

#[cfg(test)]
mod run_tests {
    use super::coalesce_block_runs;

    #[test]
    fn adjacent_blocks_become_one_read() {
        assert_eq!(coalesce_block_runs([1, 2, 3, 4], 16), vec![(1, 4)]);
    }

    #[test]
    fn a_gap_of_one_block_still_breaks_the_run() {
        // Reading across the gap would pull in a block the caller did not ask
        // for — cheap here, but the caller's set is the contract.
        assert_eq!(coalesce_block_runs([1, 2, 4, 5], 16), vec![(1, 2), (4, 2)]);
    }

    #[test]
    fn runs_are_capped_so_the_buffer_stays_bounded() {
        assert_eq!(coalesce_block_runs([10, 11, 12, 13, 14], 2), vec![(10, 2), (12, 2), (14, 1)]);
    }

    #[test]
    fn a_zero_cap_is_treated_as_one_block_per_read() {
        assert_eq!(coalesce_block_runs([7, 8], 0), vec![(7, 1), (8, 1)]);
    }

    #[test]
    fn nothing_in_nothing_out() {
        assert!(coalesce_block_runs([], 16).is_empty());
    }

    /// The shape the passes actually feed it: one long stretch of zeroed blocks
    /// plus scattered metadata blocks around it.
    #[test]
    fn a_long_stretch_collapses_while_singles_stay_single() {
        let mut blocks: Vec<u64> = (100..=1_099).collect(); // 1000 in a row
        blocks.extend([5_000, 7_321, 9_999]);
        let runs = coalesce_block_runs(blocks, 64);
        assert_eq!(runs.len(), 16 + 3, "1000 blocks in runs of 64, plus three singles");
        assert_eq!(runs[0], (100, 64));
        assert_eq!(*runs.last().unwrap(), (9_999, 1));
    }
}
