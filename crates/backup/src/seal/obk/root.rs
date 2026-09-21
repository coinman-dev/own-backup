//! The root record: the one sealed structure that says which other records an
//! image is made of.
//!
//! Opening an `.obk` starts here. The open header points at the root, the root
//! names every index chunk and every metadata chunk — identity, offset and
//! length — and the index names every data record. Nothing below that chain is
//! trusted on its own say-so, which is what turns "this record decrypts" into
//! "this record belongs at this place in this image".

use uuid::Uuid;

use own_backup_common::Result;

use super::index::MAX_INDEX_ENTRIES_PER_CHUNK;
use super::{array, bad, role_from_u8, role_to_u8};
use crate::seal::cluster::{RecordId, HEADER_LEN, MAX_PAYLOAD_LEN, TAG_LEN};
use crate::sidecar::ChainRole;

pub const ROOT_MAGIC: &[u8; 4] = b"OBKT";
pub const ROOT_VERSION: u16 = 2;
pub const ROOT_FIXED_LEN: usize = 100;
pub const INDEX_REF_LEN: usize = 68;
pub const META_REF_LEN: usize = 56;

/// Caps that keep a damaged or hostile root from asking for unbounded work.
/// With 64 KiB clusters and full chunks this covers a 16 TiB virtual disk; a
/// larger disk needs a larger cluster, or a format version with a deeper root.
pub const MAX_INDEX_CHUNKS: usize = 15_000;
pub const MAX_META_CHUNKS: usize = 4_096;

/// One sealed metadata chunk: what it is and where it lies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkRef {
    pub id: RecordId,
    pub offset: u64,
    pub len: u32,
}

/// An index chunk also says how many entries it holds and where it starts, so
/// the reader can pick the right chunk without decrypting the others.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexChunkRef {
    pub chunk: ChunkRef,
    pub entries: u32,
    pub first_cluster: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObkRoot {
    pub role: ChainRole,
    pub chain_id: Uuid,
    pub backup_id: Uuid,
    pub parent_backup_id: Option<Uuid>,
    pub virtual_size: u64,
    pub cluster_bits: u32,
    pub guest_clusters: u64,
    /// Guest clusters this image defines itself (stored or explicitly zero).
    pub defined_clusters: u64,
    /// Total decoded length of the sealed metadata, across its chunks.
    pub sealed_metadata_len: u64,
    pub index_chunks: Vec<IndexChunkRef>,
    pub meta_chunks: Vec<ChunkRef>,
}

fn record_len_ok(len: u32) -> bool {
    let len = len as usize;
    (HEADER_LEN + TAG_LEN..=HEADER_LEN + MAX_PAYLOAD_LEN + TAG_LEN).contains(&len)
}

impl ObkRoot {
    pub fn encoded_len(index_chunks: usize, meta_chunks: usize) -> usize {
        ROOT_FIXED_LEN + index_chunks * INDEX_REF_LEN + meta_chunks * META_REF_LEN
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        self.check()?;
        let mut out = Vec::with_capacity(Self::encoded_len(
            self.index_chunks.len(),
            self.meta_chunks.len(),
        ));
        out.extend_from_slice(ROOT_MAGIC);
        out.extend_from_slice(&ROOT_VERSION.to_be_bytes());
        out.push(role_to_u8(self.role));
        out.push(0); // reserved
        out.extend_from_slice(self.chain_id.as_bytes());
        out.extend_from_slice(self.backup_id.as_bytes());
        out.extend_from_slice(self.parent_backup_id.unwrap_or(Uuid::nil()).as_bytes());
        out.extend_from_slice(&self.virtual_size.to_be_bytes());
        out.extend_from_slice(&self.cluster_bits.to_be_bytes());
        out.extend_from_slice(&self.guest_clusters.to_be_bytes());
        out.extend_from_slice(&self.defined_clusters.to_be_bytes());
        out.extend_from_slice(&self.sealed_metadata_len.to_be_bytes());
        out.extend_from_slice(&(self.index_chunks.len() as u32).to_be_bytes());
        out.extend_from_slice(&(self.meta_chunks.len() as u32).to_be_bytes());
        debug_assert_eq!(out.len(), ROOT_FIXED_LEN);
        for chunk in &self.index_chunks {
            out.extend_from_slice(&chunk.chunk.id.session);
            out.extend_from_slice(&chunk.chunk.id.nonce);
            out.extend_from_slice(&chunk.chunk.offset.to_be_bytes());
            out.extend_from_slice(&chunk.chunk.len.to_be_bytes());
            out.extend_from_slice(&chunk.entries.to_be_bytes());
            out.extend_from_slice(&chunk.first_cluster.to_be_bytes());
        }
        for chunk in &self.meta_chunks {
            out.extend_from_slice(&chunk.id.session);
            out.extend_from_slice(&chunk.id.nonce);
            out.extend_from_slice(&chunk.offset.to_be_bytes());
            out.extend_from_slice(&chunk.len.to_be_bytes());
        }
        if out.len() > MAX_PAYLOAD_LEN {
            return Err(bad(
                "image needs more index/metadata chunks than one root record holds",
            ));
        }
        Ok(out)
    }

    /// Decode an already authenticated root payload. Authentication says the
    /// bytes are ours; these checks say they describe a possible image.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < ROOT_FIXED_LEN {
            return Err(bad("root record truncated"));
        }
        if &bytes[..4] != ROOT_MAGIC || u16::from_be_bytes(array(bytes, 4)) != ROOT_VERSION {
            return Err(bad("unknown root record format/version"));
        }
        if bytes[7] != 0 {
            return Err(bad("reserved root byte is set"));
        }
        let index_count = u32::from_be_bytes(array(bytes, 92)) as usize;
        let meta_count = u32::from_be_bytes(array(bytes, 96)) as usize;
        if index_count > MAX_INDEX_CHUNKS || meta_count > MAX_META_CHUNKS {
            return Err(bad("root record declares too many chunks"));
        }
        if bytes.len() != Self::encoded_len(index_count, meta_count) {
            return Err(bad("root record length does not match its chunk counts"));
        }

        let parent = Uuid::from_bytes(array(bytes, 40));
        let root = Self {
            role: role_from_u8(bytes[6])?,
            chain_id: Uuid::from_bytes(array(bytes, 8)),
            backup_id: Uuid::from_bytes(array(bytes, 24)),
            parent_backup_id: (!parent.is_nil()).then_some(parent),
            virtual_size: u64::from_be_bytes(array(bytes, 56)),
            cluster_bits: u32::from_be_bytes(array(bytes, 64)),
            guest_clusters: u64::from_be_bytes(array(bytes, 68)),
            defined_clusters: u64::from_be_bytes(array(bytes, 76)),
            sealed_metadata_len: u64::from_be_bytes(array(bytes, 84)),
            index_chunks: (0..index_count)
                .map(|i| {
                    let at = ROOT_FIXED_LEN + i * INDEX_REF_LEN;
                    IndexChunkRef {
                        chunk: ChunkRef {
                            id: RecordId {
                                session: array(bytes, at),
                                nonce: array(bytes, at + 32),
                            },
                            offset: u64::from_be_bytes(array(bytes, at + 44)),
                            len: u32::from_be_bytes(array(bytes, at + 52)),
                        },
                        entries: u32::from_be_bytes(array(bytes, at + 56)),
                        first_cluster: u64::from_be_bytes(array(bytes, at + 60)),
                    }
                })
                .collect(),
            meta_chunks: (0..meta_count)
                .map(|i| {
                    let at = ROOT_FIXED_LEN + index_count * INDEX_REF_LEN + i * META_REF_LEN;
                    ChunkRef {
                        id: RecordId {
                            session: array(bytes, at),
                            nonce: array(bytes, at + 32),
                        },
                        offset: u64::from_be_bytes(array(bytes, at + 44)),
                        len: u32::from_be_bytes(array(bytes, at + 52)),
                    }
                })
                .collect(),
        };
        root.check()?;
        Ok(root)
    }

    /// Invariants shared by encoding and decoding: geometry that adds up, a
    /// parent exactly when the role has one, chunks that could exist.
    fn check(&self) -> Result<()> {
        if self.cluster_bits < super::MIN_OBK_CLUSTER_BITS
            || self.cluster_bits > super::MAX_OBK_CLUSTER_BITS
        {
            return Err(bad(format!("cluster_bits {} out of range", self.cluster_bits)));
        }
        let cluster_size = 1u64 << self.cluster_bits;
        if self.virtual_size == 0 || !self.virtual_size.is_multiple_of(cluster_size) {
            return Err(bad("virtual size is not a whole number of clusters"));
        }
        if self.guest_clusters != self.virtual_size / cluster_size {
            return Err(bad("cluster count does not match the virtual size"));
        }
        if self.chain_id.is_nil() || self.backup_id.is_nil() {
            return Err(bad("nil chain/backup identity"));
        }
        match (self.role, self.parent_backup_id) {
            (ChainRole::Full, None) => {}
            (ChainRole::Incremental | ChainRole::Differential, Some(parent)) => {
                if parent == self.backup_id {
                    return Err(bad("image claims to be its own parent"));
                }
            }
            _ => return Err(bad("chain role and parent identity disagree")),
        }
        if self.index_chunks.len() > MAX_INDEX_CHUNKS || self.meta_chunks.len() > MAX_META_CHUNKS {
            return Err(bad("too many chunks for one root record"));
        }
        if self.sealed_metadata_len == 0 && !self.meta_chunks.is_empty() {
            return Err(bad("metadata chunks without metadata"));
        }
        if self.sealed_metadata_len != 0 && self.meta_chunks.is_empty() {
            return Err(bad("metadata without any chunk holding it"));
        }
        if self.sealed_metadata_len > (self.meta_chunks.len() * MAX_PAYLOAD_LEN) as u64 {
            return Err(bad("metadata longer than its chunks can hold"));
        }
        // Chunks may hold MORE than the sidecar: an image that reserved room
        // for a larger one pads the rest (`finish_with_reserve`).
        if self.sealed_metadata_len == 0 && !self.meta_chunks.is_empty() {
            return Err(bad("metadata chunks without metadata"));
        }
        for chunk in &self.meta_chunks {
            if chunk.offset == 0 || !record_len_ok(chunk.len) {
                return Err(bad("metadata chunk outside the image"));
            }
        }

        let mut total = 0u64;
        let mut previous_end: Option<u64> = None;
        for chunk in &self.index_chunks {
            if chunk.chunk.offset == 0 || !record_len_ok(chunk.chunk.len) {
                return Err(bad("index chunk outside the image"));
            }
            if chunk.entries == 0 || chunk.entries as usize > MAX_INDEX_ENTRIES_PER_CHUNK {
                return Err(bad("index chunk holds an impossible number of entries"));
            }
            if chunk.first_cluster >= self.guest_clusters {
                return Err(bad("index chunk starts beyond the virtual disk"));
            }
            // Chunks cover ascending, non-overlapping ranges of guest clusters,
            // so a lookup can pick one chunk and a reordered set is refused.
            if let Some(previous) = previous_end {
                if chunk.first_cluster <= previous {
                    return Err(bad("index chunks are not in ascending cluster order"));
                }
            }
            previous_end = Some(chunk.first_cluster);
            total += u64::from(chunk.entries);
        }
        if total != self.defined_clusters {
            return Err(bad("index chunk entries do not add up to the declared count"));
        }
        if self.defined_clusters > self.guest_clusters {
            return Err(bad("more defined clusters than the disk has"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One way of breaking a root record, applied to a good one.
    type Edit = Box<dyn Fn(&mut ObkRoot)>;

    fn chunk(n: u8) -> ChunkRef {
        ChunkRef {
            id: RecordId {
                session: [n; 32],
                nonce: [n; 12],
            },
            offset: 65536 * u64::from(n + 1),
            len: (HEADER_LEN + 100 + TAG_LEN) as u32,
        }
    }

    fn sample() -> ObkRoot {
        ObkRoot {
            role: ChainRole::Incremental,
            chain_id: Uuid::from_bytes([3; 16]),
            backup_id: Uuid::from_bytes([4; 16]),
            parent_backup_id: Some(Uuid::from_bytes([5; 16])),
            virtual_size: 1 << 24,
            cluster_bits: 16,
            guest_clusters: 256,
            defined_clusters: 7,
            sealed_metadata_len: 1234,
            index_chunks: vec![
                IndexChunkRef {
                    chunk: chunk(1),
                    entries: 4,
                    first_cluster: 0,
                },
                IndexChunkRef {
                    chunk: chunk(2),
                    entries: 3,
                    first_cluster: 100,
                },
            ],
            meta_chunks: vec![chunk(3)],
        }
    }

    #[test]
    fn a_root_round_trips_byte_for_byte() {
        let root = sample();
        let bytes = root.encode().unwrap();
        assert_eq!(bytes.len(), ObkRoot::encoded_len(2, 1));
        assert_eq!(ObkRoot::decode(&bytes).unwrap(), root);
        // A full backup keeps its parent field nil rather than omitting it.
        let full = ObkRoot {
            role: ChainRole::Full,
            parent_backup_id: None,
            ..sample()
        };
        assert_eq!(ObkRoot::decode(&full.encode().unwrap()).unwrap(), full);
    }

    #[test]
    fn a_root_that_does_not_describe_a_possible_image_is_refused() {
        let cases: Vec<(&str, Edit)> = vec![
            ("geometry", Box::new(|r: &mut ObkRoot| r.guest_clusters += 1)),
            ("cluster bits", Box::new(|r: &mut ObkRoot| r.cluster_bits = 9)),
            ("nil chain", Box::new(|r: &mut ObkRoot| r.chain_id = Uuid::nil())),
            ("self parent", Box::new(|r: &mut ObkRoot| r.parent_backup_id = Some(r.backup_id))),
            ("role", Box::new(|r: &mut ObkRoot| r.role = ChainRole::Full)),
            ("entry total", Box::new(|r: &mut ObkRoot| r.defined_clusters = 6)),
            (
                "chunk order",
                Box::new(|r: &mut ObkRoot| r.index_chunks[1].first_cluster = 0),
            ),
            (
                "empty chunk",
                Box::new(|r: &mut ObkRoot| {
                    r.index_chunks[0].entries = 0;
                    r.defined_clusters = 3;
                }),
            ),
            (
                "chunk in the header",
                Box::new(|r: &mut ObkRoot| r.index_chunks[0].chunk.offset = 0),
            ),
            (
                "metadata without chunks",
                Box::new(|r: &mut ObkRoot| r.meta_chunks.clear()),
            ),
            (
                "chunks without metadata",
                Box::new(|r: &mut ObkRoot| r.sealed_metadata_len = 0),
            ),
            (
                "impossible record length",
                Box::new(|r: &mut ObkRoot| r.meta_chunks[0].len = 1),
            ),
        ];
        for (what, break_it) in cases {
            let mut root = sample();
            break_it(&mut root);
            assert!(root.encode().is_err(), "{what} must be refused");
        }
    }

    #[test]
    fn decoding_refuses_truncation_padding_and_impossible_counts() {
        let bytes = sample().encode().unwrap();
        assert!(ObkRoot::decode(&bytes[..bytes.len() - 1]).is_err());
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(ObkRoot::decode(&longer).is_err());
        let mut wrong_magic = bytes.clone();
        wrong_magic[0] ^= 1;
        assert!(ObkRoot::decode(&wrong_magic).is_err());
        let mut wrong_version = bytes.clone();
        wrong_version[5] = 99;
        assert!(ObkRoot::decode(&wrong_version).is_err());
        let mut reserved = bytes.clone();
        reserved[7] = 1;
        assert!(ObkRoot::decode(&reserved).is_err());
        let mut huge = bytes.clone();
        huge[92..96].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(ObkRoot::decode(&huge).is_err());
    }
}
