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
