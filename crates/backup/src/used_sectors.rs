/// Trait for querying which blocks on a volume are actually used by the
/// filesystem, allowing smart-sector backups to skip free space.
///
/// Implementations consult the filesystem's allocation bitmap (e.g.
/// `FSCTL_GET_VOLUME_BITMAP` on NTFS) to determine which blocks contain data.
pub trait UsedSectorMap: Send {
    /// Returns `true` if `block_number` contains filesystem data that should
    /// be included in the backup.
    fn is_block_used(&self, block_number: u64) -> bool;

    /// Total number of used blocks.
    fn used_block_count(&self) -> u64;
}

/// Fallback implementation that treats every block as used.
///
/// Used for sector-by-sector backups or when no filesystem bitmap is available.
pub struct AllUsedMap {
    total: u64,
}

impl AllUsedMap {
    /// Create an `AllUsedMap` where all `total` blocks are considered used.
    pub fn new(total: u64) -> Self {
        Self { total }
    }
}

impl UsedSectorMap for AllUsedMap {
    fn is_block_used(&self, block_number: u64) -> bool {
        block_number < self.total
    }

    fn used_block_count(&self) -> u64 {
        self.total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_used_map_basic() {
        let map = AllUsedMap::new(10);
        assert_eq!(map.used_block_count(), 10);

        for i in 0..10 {
            assert!(map.is_block_used(i));
        }
        assert!(!map.is_block_used(10));
        assert!(!map.is_block_used(100));
    }

    #[test]
    fn all_used_map_zero() {
        let map = AllUsedMap::new(0);
        assert_eq!(map.used_block_count(), 0);
        assert!(!map.is_block_used(0));
    }
}
