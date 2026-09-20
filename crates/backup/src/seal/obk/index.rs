//! The authenticated index: which record is the current one for a guest
//! cluster, which clusters are explicitly zero, and which are not defined here
//! at all.
//!
//! This is the structure that makes replay detectable. A data record carries
//! its own chain, purpose and guest index in its AAD, so it cannot be moved to
//! another place or another image — but an older record of the *same* cluster
//! in the *same* chain is a perfectly valid record. Only the index says which
//! of them the image means, and the index is itself sealed and named by the
//! root record.
//!
//! Chunks are fixed-stride so a lookup costs one decryption and a binary
//! search, not a full decode of the whole map.

use own_backup_common::Result;

use super::{array, bad};
use crate::seal::cluster::{RecordId, MAX_PAYLOAD_LEN, MAX_SESSION_RECORDS};

/// cluster(8) + state(1) + session(32) + nonce(12).
pub const INDEX_ENTRY_LEN: usize = 53;

/// Entries per chunk: 18 000 x 53 = 954 000 bytes, inside the 1 MiB record cap
/// with room to spare. Chunks compress well — the session identity repeats.
pub const MAX_INDEX_ENTRIES_PER_CHUNK: usize = 18_000;

const STATE_PRESENT: u8 = 1;
const STATE_ZERO: u8 = 2;

/// What an image says about one of its guest clusters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClusterState {
    /// This image does not define the cluster: read the parent, or zeros when
    /// there is no parent.
    Absent,
    /// Explicitly zero. In a child this overrides the parent's data.
    Zero,
    /// Stored here, as exactly this record and no other.
    Present(RecordId),
}

/// Encode entries for one chunk. Callers pass them in ascending cluster order;
/// [`ClusterState::Absent`] has no entry by definition.
pub fn encode_chunk(entries: &[(u64, ClusterState)]) -> Result<Vec<u8>> {
    if entries.is_empty() || entries.len() > MAX_INDEX_ENTRIES_PER_CHUNK {
        return Err(bad("index chunk holds an impossible number of entries"));
    }
    let mut out = Vec::with_capacity(entries.len() * INDEX_ENTRY_LEN);
    let mut previous: Option<u64> = None;
    for (cluster, state) in entries {
        if previous.is_some_and(|p| *cluster <= p) {
            return Err(bad("index entries are not in ascending cluster order"));
        }
        previous = Some(*cluster);
        out.extend_from_slice(&cluster.to_be_bytes());
        match state {
            ClusterState::Present(id) => {
                out.push(STATE_PRESENT);
                out.extend_from_slice(&id.session);
                out.extend_from_slice(&id.nonce);
            }
            ClusterState::Zero => {
                out.push(STATE_ZERO);
                out.extend_from_slice(&[0u8; 44]);
            }
            ClusterState::Absent => {
                return Err(bad("an absent cluster has no index entry"));
            }
        }
    }
    debug_assert!(out.len() <= MAX_PAYLOAD_LEN);
    Ok(out)
}

/// One decoded chunk, kept in its wire form: a lookup is a binary search over
/// a fixed stride, so a big index costs memory only for the chunks in use.
pub struct IndexChunk {
    bytes: Vec<u8>,
}

impl IndexChunk {
    /// Validate a chunk that has already been authenticated. `range` is the
    /// half-open span of guest clusters this chunk owns, taken from the root's
    /// chunk list — entries outside it would make two chunks answer for the
    /// same cluster, and then which one wins is the attacker's choice.
    pub fn parse(bytes: Vec<u8>, entries: u32, range: std::ops::Range<u64>) -> Result<Self> {
        if bytes.len() != entries as usize * INDEX_ENTRY_LEN {
            return Err(bad("index chunk length does not match its entry count"));
        }
        let mut previous: Option<u64> = None;
        for at in (0..bytes.len()).step_by(INDEX_ENTRY_LEN) {
            let cluster = u64::from_be_bytes(array(&bytes, at));
            if !range.contains(&cluster) {
                return Err(bad("index entry outside the chunk's declared range"));
            }
            if previous.is_some_and(|p| cluster <= p) {
                return Err(bad("index entries are not in ascending cluster order"));
            }
            previous = Some(cluster);
            match bytes[at + 8] {
                STATE_PRESENT => {
                    let nonce: [u8; 12] = array(&bytes, at + 41);
                    if nonce[..4] != [0; 4]
                        || u64::from_be_bytes(array(&nonce, 4)) >= MAX_SESSION_RECORDS
                    {
                        return Err(bad("index entry names an impossible record nonce"));
                    }
                }
                STATE_ZERO => {
                    if bytes[at + 9..at + INDEX_ENTRY_LEN].iter().any(|&b| b != 0) {
                        return Err(bad("zero cluster entry carries a record identity"));
                    }
                }
                other => return Err(bad(format!("unknown cluster state {other}"))),
            }
        }
        Ok(Self { bytes })
    }

    pub fn entries(&self) -> usize {
        self.bytes.len() / INDEX_ENTRY_LEN
    }

    /// First cluster in the chunk; used to check the root's claim about it.
    pub fn first_cluster(&self) -> Option<u64> {
        (!self.bytes.is_empty()).then(|| u64::from_be_bytes(array(&self.bytes, 0)))
    }

    pub fn lookup(&self, cluster: u64) -> ClusterState {
        let mut low = 0usize;
        let mut high = self.entries();
        while low < high {
            let mid = (low + high) / 2;
            let at = mid * INDEX_ENTRY_LEN;
            let found = u64::from_be_bytes(array(&self.bytes, at));
            match found.cmp(&cluster) {
                std::cmp::Ordering::Less => low = mid + 1,
                std::cmp::Ordering::Greater => high = mid,
                std::cmp::Ordering::Equal => {
                    return if self.bytes[at + 8] == STATE_ZERO {
                        ClusterState::Zero
                    } else {
                        ClusterState::Present(RecordId {
                            session: array(&self.bytes, at + 9),
                            nonce: array(&self.bytes, at + 41),
                        })
                    };
                }
            }
        }
        ClusterState::Absent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> RecordId {
        RecordId {
            session: [n; 32],
            nonce: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, n],
        }
    }

    fn sample() -> Vec<(u64, ClusterState)> {
        vec![
            (0, ClusterState::Present(id(1))),
            (3, ClusterState::Zero),
            (9, ClusterState::Present(id(2))),
            (1000, ClusterState::Present(id(3))),
        ]
    }

    #[test]
    fn every_entry_comes_back_and_the_gaps_stay_absent() {
        let bytes = encode_chunk(&sample()).unwrap();
        let chunk = IndexChunk::parse(bytes, 4, 0..1024).unwrap();
        assert_eq!(chunk.entries(), 4);
        assert_eq!(chunk.first_cluster(), Some(0));
        for (cluster, state) in sample() {
            assert_eq!(chunk.lookup(cluster), state, "cluster {cluster}");
        }
        for absent in [1, 2, 4, 8, 10, 999, 1001, u64::MAX] {
            assert_eq!(chunk.lookup(absent), ClusterState::Absent, "cluster {absent}");
        }
    }

    #[test]
    fn encoding_refuses_what_cannot_be_looked_up() {
        assert!(encode_chunk(&[]).is_err());
        assert!(encode_chunk(&[(5, ClusterState::Absent)]).is_err());
        assert!(encode_chunk(&[
            (5, ClusterState::Zero),
            (5, ClusterState::Present(id(1)))
        ])
        .is_err());
        assert!(encode_chunk(&[
            (6, ClusterState::Zero),
            (5, ClusterState::Present(id(1)))
        ])
        .is_err());
        let too_many: Vec<_> = (0..=MAX_INDEX_ENTRIES_PER_CHUNK as u64)
            .map(|c| (c, ClusterState::Zero))
            .collect();
        assert!(encode_chunk(&too_many).is_err());
    }

    #[test]
    fn a_chunk_that_lies_about_itself_is_refused() {
        let bytes = encode_chunk(&sample()).unwrap();
        assert!(IndexChunk::parse(bytes.clone(), 3, 0..1024).is_err(), "entry count");
        assert!(
            IndexChunk::parse(bytes.clone(), 4, 0..1000).is_err(),
            "entry outside the chunk's range"
        );
        assert!(
            IndexChunk::parse(bytes.clone(), 4, 1..1024).is_err(),
            "entry before the chunk's range"
        );

        let mut unsorted = bytes.clone();
        unsorted[..8].copy_from_slice(&5000u64.to_be_bytes());
        assert!(IndexChunk::parse(unsorted, 4, 0..1024 * 8).is_err(), "order");

        let mut zero_with_identity = bytes.clone();
        zero_with_identity[INDEX_ENTRY_LEN + 9] = 7;
        assert!(IndexChunk::parse(zero_with_identity, 4, 0..1024).is_err());

        let mut unknown_state = bytes.clone();
        unknown_state[8] = 9;
        assert!(IndexChunk::parse(unknown_state, 4, 0..1024).is_err());

        let mut bad_nonce = bytes.clone();
        bad_nonce[41] = 1; // the counter's top four bytes must be zero
        assert!(IndexChunk::parse(bad_nonce, 4, 0..1024).is_err());
    }
}
