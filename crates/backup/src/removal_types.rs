//! What the passes that delete excluded files from a finished image report.
//!
//! The passes mount the image and are Windows-only; what they found is plain
//! data, and the engine carries it on every platform.

/// What the removal pass did to the image.
pub struct RemovalOutcome {
    /// Filesystem entries deleted (files + directories + reparse points,
    /// including everything inside recursively deleted directories).
    pub entries_deleted: u64,
    /// Entries that could not be deleted (kept in the image; warned).
    pub entries_failed: u64,
    /// Backup blocks whose checksum was recomputed after the merge.
    pub blocks_rehashed: u64,
    /// Checksum entries newly added (driver wrote into previously-free space,
    /// e.g. USN journal growth).
    pub blocks_added: u64,
}

/// What the "exclude inside the finished image" pass did — the removal outcome
/// plus the figures the result screen shows, which on this path can only be
/// learned from the image's own filesystem.
pub struct InsideRemoval {
    pub outcome: RemovalOutcome,
    pub files_excluded: u64,
    pub bytes_excluded: u64,
    pub categories: crate::exclusions::CategorySizes,
    /// Bytes actually overwritten with zeros inside the image.
    pub bytes_zeroed: u64,
}

/// What the qcow2 removal pass did.
pub struct Qcow2RemovalOutcome {
    pub entries_deleted: u64,
    pub entries_failed: u64,
    pub clusters_merged: u64,
    pub blocks_rehashed: u64,
}
