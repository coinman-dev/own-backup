//! The `.obk` container: our qcow2 extension holding AES-256-GCM records
//! instead of plaintext clusters (plan §7.3, stage E3).
//!
//! An `.obk` file IS a qcow2 version-3 image — same header, same L1/L2 tables,
//! same refcounts — with one added **incompatible** feature bit and a feature
//! name table saying what it is. `qemu-img` and every other reader therefore
//! refuse it by name instead of handing out ciphertext dressed as data.
//!
//! What lives where:
//!
//! ```text
//! offset 0      qcow2 header + feature name table + OBKH open header + backing name
//! offset 2 MiB  second checked header slot (new images only)
//! offset 4 MiB+ one sealed record per stored guest cluster, packed byte-to-byte
//!   ...         sealed index chunks (guest cluster -> which record is current)
//!   ...         sealed metadata chunks (the sidecar: disk layout, checksums, …)
//!   ...         the sealed root record: what the index/metadata chunks must be
//!   ...         L2 tables, L1 table, refcount blocks, refcount table
//! ```
//! Legacy single-header images start records after cluster 0 and are read by
//! the same reader. Their updates continue to use a whole-file transaction.
//!
//! **The authenticated index is the truth, the L2 table is only a pointer.**
//! Every read cross-checks them: the index says what a guest cluster must be
//! (absent, an explicit zero, or exactly one record identity), the L2 entry says
//! where those bytes are, and any disagreement is an error rather than a read.
//! A record header alone is never authority for anything — that is what makes a
//! reordered, dropped or replayed-from-an-older-backup record detectable
//! (`cluster.rs`, `FORMAT.md`).
//!
//! The open header carries only what a listing needs without a key (identities,
//! sizes, the wrapped chain key and the portable password/recovery envelope).
//! It is untrusted until the root record opens and agrees with it field by
//! field. Whole-file rollback — replacing the entire image with an older, also
//! valid one — is out of scope here and needs external trusted state (§7.3).

pub mod compact;
#[cfg(test)]
pub(crate) mod container_tests;
#[cfg(test)]
mod data_faults;
pub mod fold;
pub mod header;
pub mod index;
pub mod io;
#[cfg(all(test, windows))]
mod io_faults;
pub mod listing;
pub mod metadata;
#[cfg(test)]
mod metadata_faults;
#[cfg(test)]
mod publication_faults;
pub mod reader;
pub mod rebuild;
pub mod root;
mod staging;
mod transaction;
pub mod writer;

pub use compact::{compact_if_needed, compact_image, compact_image_with_progress, CompactionStats};
pub use fold::{fold_child_into_base, repoint_with_metadata, set_backing_file, FoldStats};
pub use header::{ObkOpenHeader, ObkRootPointer};
pub use index::ClusterState;
pub use io::{ObkRandomReader, ObkStagedWriter};
pub use listing::{read_listing, ObkListing};
pub use metadata::update_sealed_metadata;
pub use reader::read_open_header;
pub use reader::{ObkChain, ObkImage};
pub use rebuild::{
    apply_changed, apply_changed_clusters, apply_changed_with, ChangedClusters, PreviousContents,
    RebuildIdentity, RebuildOptions, RebuildStats,
};
pub use root::ObkRoot;
pub use writer::{ObkCompression, ObkWriter, ObkWriterOptions};

use own_backup_common::OwnBackupError;

/// File extension of an encrypted image. The retired 2026-07-26 container
/// reused this name; only the name is reused, nothing of its format.
pub const OBK_EXTENSION: &str = "obk";

/// Incompatible feature bit 60. Bits 0..3 are the qcow2 spec's; the rest are
/// reserved, and a reader that does not know one MUST refuse the file.
pub const INCOMPAT_OBK_SEAL: u64 = 1 << 60;

/// qcow2 header extension 0x6803f857: feature name table. Without it qemu says
/// only "unsupported feature bit 60"; with it, it says what the feature is.
pub const HEXT_FEATURE_NAME_TABLE: u32 = 0x6803_f857;

/// Our own header extension, ASCII "OBKH": the open half of the header, as
/// UTF-8 JSON.
pub const HEXT_OBK_OPEN: u32 = 0x4F42_4B48;

/// Name reported through the feature name table (46 bytes at most).
pub const OBK_FEATURE_NAME: &str = "own-backup encryption v2";

/// Document identifier of the open header.
pub const OBK_OPEN_FORMAT: &str = "own-backup-obk";

/// Bound on the open header JSON, checked before parsing. It holds the portable
/// key envelope (at most 16 KiB by its own rule) plus a few hundred bytes.
pub const MAX_OPEN_HEADER_BYTES: usize = 32 * 1024;

/// AEAD records do not fit the qcow2 compressed descriptor below this cluster
/// size: the descriptor can address at most two clusters' worth of bytes, and a
/// record is one cluster plus 100 bytes of framing and tag.
pub const MIN_OBK_CLUSTER_BITS: u32 = 12;
pub const MAX_OBK_CLUSTER_BITS: u32 = 21;

/// New images reserve two maximum-sized header slots before any records.
/// Legacy images have only the header at offset zero and remain readable.
pub const SECOND_HEADER_OFFSET: u64 = 1 << MAX_OBK_CLUSTER_BITS;
pub const DUAL_HEADER_DATA_START: u64 = 2 * SECOND_HEADER_OFFSET;

/// Bytes a record adds to its payload: 84-byte header + 16-byte tag.
pub const RECORD_OVERHEAD: usize = crate::seal::cluster::HEADER_LEN + crate::seal::cluster::TAG_LEN;

/// How deep a chain may be before we call it a loop rather than a chain.
pub const MAX_CHAIN_DEPTH: usize = 512;

pub(crate) fn bad(msg: impl std::fmt::Display) -> OwnBackupError {
    OwnBackupError::ImageFormat(format!("obk: {msg}"))
}

/// Role of an image inside its chain, as stored in the root record. The wire
/// values are fixed; [`crate::sidecar::ChainRole`] is the in-memory shape the
/// rest of the program already uses.
pub(crate) fn role_to_u8(role: crate::sidecar::ChainRole) -> u8 {
    match role {
        crate::sidecar::ChainRole::Full => 1,
        crate::sidecar::ChainRole::Incremental => 2,
        crate::sidecar::ChainRole::Differential => 3,
    }
}

pub(crate) fn role_from_u8(value: u8) -> own_backup_common::Result<crate::sidecar::ChainRole> {
    match value {
        1 => Ok(crate::sidecar::ChainRole::Full),
        2 => Ok(crate::sidecar::ChainRole::Incremental),
        3 => Ok(crate::sidecar::ChainRole::Differential),
        other => Err(bad(format!("unknown chain role {other}"))),
    }
}

/// Read a fixed-size field. Every caller has already bounds-checked the slice.
pub(crate) fn array<const N: usize>(bytes: &[u8], at: usize) -> [u8; N] {
    let mut out = [0u8; N];
    out.copy_from_slice(&bytes[at..at + N]);
    out
}

/// The feature name table entry for our bit: type 0 (incompatible), the bit
/// number, then the name padded into 46 bytes.
pub(crate) fn feature_name_table() -> Vec<u8> {
    let mut entry = vec![0u8; 48];
    entry[0] = 0; // incompatible feature
    entry[1] = 60; // bit number of INCOMPAT_OBK_SEAL
    let name = OBK_FEATURE_NAME.as_bytes();
    entry[2..2 + name.len()].copy_from_slice(name);
    entry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_feature_bit_and_its_published_name_agree() {
        let table = feature_name_table();
        assert_eq!(table.len(), 48);
        assert_eq!(table[0], 0, "incompatible feature");
        assert_eq!(1u64 << table[1], INCOMPAT_OBK_SEAL, "bit number matches");
        assert!(OBK_FEATURE_NAME.len() <= 46, "name fits the entry");
        assert_eq!(
            &table[2..2 + OBK_FEATURE_NAME.len()],
            OBK_FEATURE_NAME.as_bytes()
        );
    }

    #[test]
    fn a_record_fits_the_compressed_descriptor_at_every_allowed_cluster_size() {
        for bits in MIN_OBK_CLUSTER_BITS..=MAX_OBK_CLUSTER_BITS {
            let cluster = 1usize << bits;
            let record = cluster + RECORD_OVERHEAD;
            // Worst case: the record starts one byte before a sector boundary.
            let sectors = (record + 511).div_ceil(512);
            assert!(
                sectors <= 1usize << (bits - 8),
                "cluster_bits {bits}: {sectors} sectors do not fit the descriptor"
            );
        }
        // At the smallest cluster qcow2 allows the framing genuinely does not
        // fit; 4 KiB is a policy floor above that hard limit, not the limit.
        let record = 512 + RECORD_OVERHEAD;
        assert!((record + 511).div_ceil(512) > 1usize << (9 - 8));
    }

    #[test]
    fn roles_round_trip_and_unknown_values_are_refused() {
        for role in [
            crate::sidecar::ChainRole::Full,
            crate::sidecar::ChainRole::Incremental,
            crate::sidecar::ChainRole::Differential,
        ] {
            assert_eq!(role_from_u8(role_to_u8(role)).unwrap(), role);
        }
        assert!(role_from_u8(0).is_err());
        assert!(role_from_u8(4).is_err());
    }
}
