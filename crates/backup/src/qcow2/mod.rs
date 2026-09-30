//! Pure-Rust reader/writer for **qcow2** images (no QEMU dependency), the
//! second selectable backup container (decision #13, 2026-07-07):
//! zstd compression inside the format with random access, backing-file
//! chains resolved in userspace, Linux-native mounting.
//!
//! Implements the subset of the qcow2 spec (QEMU `docs/interop/qcow2.rst`)
//! we need: version-3 header, the zstd compression-type extension, L1/L2
//! tables, standard/compressed/zero cluster descriptors, 16-bit refcounts
//! and an optional backing file. Snapshots, encryption, external data files,
//! bitmaps and deflate-compressed clusters are out of scope (the reader
//! rejects them explicitly).
//!
//! Self-consistency is unit-tested; conformance is validated against
//! `qemu-img check`/`convert` in `crates/backup/tests/qcow2.rs`.
//!
//! All multi-byte integers in qcow2 are **big-endian** (unlike VHDX).

pub mod consolidate;
pub mod io;
pub(crate) mod layout;
pub mod merge;
pub mod reader;
pub mod writer;

pub use consolidate::{merge_child_into_parent, set_backing_file, Qcow2MergeStats};
pub use io::{Qcow2RandomReader, Qcow2StagedWriter};
pub use merge::Qcow2InPlaceMerge;
pub use reader::Qcow2Reader;
pub use writer::{Qcow2Compression, Qcow2Writer, Qcow2WriterOptions};

use own_backup_common::OwnBackupError;

pub const QCOW2_MAGIC: u32 = 0x5146_49FB; // "QFI\xfb"
pub const QCOW2_EXTENSION: &str = "qcow2";

/// Default cluster size: 64 KiB (`cluster_bits = 16`, the qemu default) —
/// equals the backup block size, so backup blocks map 1:1 onto clusters.
pub const DEFAULT_CLUSTER_BITS: u32 = 16;

/// `refcount_order = 4` → 16-bit refcount entries (the qemu default).
pub const REFCOUNT_ORDER: u32 = 4;

/// Default zstd level for cluster compression (pipeline default, cf.
/// `compression.rs` block tests): fast enough to keep the copy disk-bound.
pub const DEFAULT_ZSTD_LEVEL: i32 = 3;
/// The strongest level offered. zstd goes to 22, but 20+ ("ultra") needs a
/// window far larger than a 64 KiB cluster can use: slower, no smaller.
pub const MAX_ZSTD_LEVEL: i32 = 19;

/// The level a job's setting stands for. Anything outside 1..=19 — zero from a
/// config written before the setting did anything, a hand-edited value — is
/// the default, never "no compression" and never an error mid-run.
pub fn zstd_level_or_default(level: i32) -> i32 {
    if (1..=MAX_ZSTD_LEVEL).contains(&level) {
        level
    } else {
        DEFAULT_ZSTD_LEVEL
    }
}

/// Version-3 header length with the `compression_type` field (+ padding).
pub const HEADER_LENGTH_V3: u32 = 112;

/// Incompatible-feature bit 3: a non-deflate `compression_type` is in use.
pub const INCOMPAT_COMPRESSION_TYPE: u64 = 1 << 3;
/// Incompatible-feature bit 0: dirty refcounts (lazy refcounts crashed).
pub const INCOMPAT_DIRTY: u64 = 1 << 0;

/// `compression_type` header field values.
pub const COMPRESSION_TYPE_DEFLATE: u8 = 0;
pub const COMPRESSION_TYPE_ZSTD: u8 = 1;

/// Header-extension type: backing file format name (e.g. `qcow2`).
pub const HEXT_BACKING_FORMAT: u32 = 0xE279_2ACA;
/// Header-extension type: end-of-extensions marker.
pub const HEXT_END: u32 = 0;

// L1/L2 entry flag bits.
pub const OFLAG_COPIED: u64 = 1 << 63;
pub const OFLAG_COMPRESSED: u64 = 1 << 62;
pub const OFLAG_ZERO: u64 = 1 << 0;
/// Host-offset bits of a standard (non-compressed) L1/L2 entry.
pub const OFFSET_MASK: u64 = 0x00FF_FFFF_FFFF_FE00;

pub(crate) fn bad(msg: impl std::fmt::Display) -> OwnBackupError {
    OwnBackupError::ImageFormat(format!("qcow2: {msg}"))
}

/// A decoded L2 entry, in the terms the backup engine cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClusterDescriptor {
    /// No data in this file — read the backing file, or zeros without one.
    Unallocated,
    /// Reads as zeros regardless of the backing file.
    Zero,
    /// Uncompressed cluster at this host byte offset (cluster-aligned).
    Standard { host_offset: u64 },
    /// Compressed cluster: `length` bytes of compressed stream starting at
    /// the (byte-aligned) host offset.
    Compressed { host_offset: u64, length: u64 },
}

/// Decode an L2 entry (`cluster_bits` is needed for the compressed layout).
pub fn decode_l2_entry(entry: u64, cluster_bits: u32) -> Result<ClusterDescriptor, OwnBackupError> {
    if entry & OFLAG_COMPRESSED != 0 {
        // Compressed descriptor: x = 62 − (cluster_bits − 8);
        // bits [0, x) = host byte offset, bits [x, 62) = additional 512-byte
        // sectors − 1 counted from the sector containing `host_offset`.
        let x = 62 - (cluster_bits - 8);
        let host_offset = entry & ((1u64 << x) - 1);
        let nb_csectors = ((entry >> x) & ((1u64 << (cluster_bits - 8)) - 1)) + 1;
        let length = nb_csectors * 512 - (host_offset & 511);
        Ok(ClusterDescriptor::Compressed {
            host_offset,
            length,
        })
    } else if entry & OFLAG_ZERO != 0 {
        Ok(ClusterDescriptor::Zero)
    } else {
        let host_offset = entry & OFFSET_MASK;
        if host_offset == 0 {
            Ok(ClusterDescriptor::Unallocated)
        } else {
            if entry & !(OFLAG_COPIED | OFFSET_MASK) != 0 {
                return Err(bad(format!("reserved bits set in L2 entry {entry:#x}")));
            }
            Ok(ClusterDescriptor::Standard { host_offset })
        }
    }
}

/// Encode a compressed-cluster L2 entry.
pub fn encode_compressed_l2_entry(
    host_offset: u64,
    compressed_len: u64,
    cluster_bits: u32,
) -> Result<u64, OwnBackupError> {
    let x = 62 - (cluster_bits - 8);
    if host_offset >= (1u64 << x) {
        return Err(bad(format!(
            "compressed cluster host offset {host_offset:#x} too large"
        )));
    }
    // Sectors spanned by [host_offset, host_offset + compressed_len).
    let first_sector = host_offset >> 9;
    let last_sector = (host_offset + compressed_len - 1) >> 9;
    let nb_csectors = last_sector - first_sector + 1;
    if nb_csectors > (1u64 << (cluster_bits - 8)) {
        return Err(bad(format!(
            "compressed cluster spans {nb_csectors} sectors — does not fit descriptor"
        )));
    }
    Ok(OFLAG_COMPRESSED | ((nb_csectors - 1) << x) | host_offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_entry_roundtrip() {
        let entry = OFLAG_COPIED | 0x50000;
        assert_eq!(
            decode_l2_entry(entry, 16).unwrap(),
            ClusterDescriptor::Standard {
                host_offset: 0x50000
            }
        );
    }

    #[test]
    fn zero_and_unallocated() {
        assert_eq!(
            decode_l2_entry(0, 16).unwrap(),
            ClusterDescriptor::Unallocated
        );
        assert_eq!(
            decode_l2_entry(OFLAG_ZERO, 16).unwrap(),
            ClusterDescriptor::Zero
        );
    }

    #[test]
    fn compressed_entry_roundtrip() {
        // 64 KiB clusters: x = 54.
        for (offset, len) in [(0x10008u64, 1000u64), (0x123456, 65000), (512, 1), (511, 2)] {
            let entry = encode_compressed_l2_entry(offset, len, 16).unwrap();
            match decode_l2_entry(entry, 16).unwrap() {
                ClusterDescriptor::Compressed {
                    host_offset,
                    length,
                } => {
                    assert_eq!(host_offset, offset);
                    // Decoded length covers at least the real stream (it is
                    // rounded up to the end of the last 512-byte sector).
                    assert!(length >= len, "length {length} < real {len}");
                    assert!(length < len + 512);
                }
                other => panic!("expected compressed, got {other:?}"),
            }
        }
    }

    #[test]
    fn compressed_entry_never_marked_copied() {
        let entry = encode_compressed_l2_entry(0x10000, 30000, 16).unwrap();
        assert_eq!(entry & OFLAG_COPIED, 0);
    }

    #[test]
    fn a_level_outside_1_to_19_is_the_default() {
        // 0 is what a config written before the setting was read holds; it
        // must not turn into "no compression".
        for bad in [0, -1, 20, 22, i32::MAX] {
            assert_eq!(zstd_level_or_default(bad), DEFAULT_ZSTD_LEVEL, "{bad}");
        }
        for good in [1, 3, 9, 19] {
            assert_eq!(zstd_level_or_default(good), good);
        }
    }
}
