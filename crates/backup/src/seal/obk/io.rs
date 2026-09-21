//! Byte-granular adapters between the engines and the cluster-based `.obk`
//! container — the encrypted twin of [`crate::qcow2::io`].
//!
//! [`ObkStagedWriter`] gives the engines the same `write_at` surface as
//! `Qcow2StagedWriter` and `VhdxWriter`: whole aligned clusters are sealed and
//! written straight through, partial writes (GPT head/tail, the last partial
//! backup block) are staged in memory and sealed as full clusters at finish.
//! Plaintext never reaches the file: staged clusters live in wiped buffers and
//! leave as records.
//!
//! [`ObkRandomReader`] is the reverse for verify, restore and mounting: reads
//! over a whole chain, with a decrypted-cluster cache. The cache is not an
//! optimisation here but a necessity — a mounted image is read in 4 KiB pieces,
//! and without it every one of them would decrypt and decompress the same
//! 64 KiB cluster again.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::File;
use std::io::{BufWriter, Seek, Write};
use std::path::Path;

use own_backup_common::Result;
use zeroize::Zeroizing;

use super::reader::ObkChain;
use super::writer::{ObkWriter, ObkWriterOptions};
use super::bad;
use crate::seal::ChainKey;

/// Staged partial clusters are bounded by GPT structures plus one tail per
/// block write; more than this means the caller streams unaligned data.
const MAX_STAGED_CLUSTERS: usize = 1024;

/// Decrypted clusters kept in memory: 1024 × 64 KiB = 64 MiB, as for qcow2.
const CLUSTER_CACHE_MAX: usize = 1024;

pub struct ObkStagedWriter<W: Write + Seek> {
    inner: ObkWriter<W>,
    staged: BTreeMap<u64, Zeroizing<Vec<u8>>>,
    virtual_size: u64,
}

impl ObkStagedWriter<BufWriter<File>> {
    /// Create a new encrypted image file (must not exist yet).
    pub fn create(path: &Path, chain: &ChainKey, opts: ObkWriterOptions) -> Result<Self> {
        Self::new(ObkWriter::create(path, chain, opts)?)
    }
}

impl<W: Write + Seek> ObkStagedWriter<W> {
    pub fn new(inner: ObkWriter<W>) -> Result<Self> {
        let virtual_size = inner.total_guest_clusters() * inner.cluster_size();
        Ok(Self {
            inner,
            staged: BTreeMap::new(),
            virtual_size,
        })
    }

    /// Virtual disk size (rounded up to a whole cluster).
    pub fn length(&self) -> u64 {
        self.virtual_size
    }

    pub fn cluster_size(&self) -> u64 {
        self.inner.cluster_size()
    }

    /// Write `data` at guest byte `offset`. Whole aligned clusters go through
    /// immediately (write-once per cluster); partial clusters are staged.
    /// Distinct writes must not overlap.
    pub fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        let cs = self.inner.cluster_size();
        let mut off = offset;
        let mut rest = data;
        while !rest.is_empty() {
            let cluster = off / cs;
            let within = (off % cs) as usize;
            let take = (cs as usize - within).min(rest.len());
            if within == 0 && take as u64 == cs {
                self.inner.write_cluster(cluster, &rest[..take])?;
            } else {
                let buf = match self.staged.get_mut(&cluster) {
                    Some(buf) => buf,
                    None => {
                        if self.staged.len() >= MAX_STAGED_CLUSTERS {
                            return Err(bad(format!(
                                "more than {MAX_STAGED_CLUSTERS} partially-written clusters staged — unaligned write pattern"
                            )));
                        }
                        self.staged
                            .entry(cluster)
                            .or_insert_with(|| Zeroizing::new(vec![0u8; cs as usize]))
                    }
                };
                buf[within..within + take].copy_from_slice(&rest[..take]);
            }
            off += take as u64;
            rest = &rest[take..];
        }
        Ok(())
    }

    /// Seal the staged clusters, then the index, the metadata and the header.
    /// `sealed_metadata` is the sidecar that belongs inside the image.
    pub fn finish(mut self, sealed_metadata: &[u8]) -> Result<W> {
        for (cluster, buf) in std::mem::take(&mut self.staged) {
            self.inner.write_cluster(cluster, &buf)?;
        }
        self.inner.finish(sealed_metadata)
    }
}

/// `read_at` over an encrypted image **and its whole chain**, with the identity
/// of every record checked against the authenticated index before its bytes are
/// used (`reader.rs`).
pub struct ObkRandomReader {
    chain: ObkChain,
    cache: HashMap<u64, Zeroizing<Vec<u8>>>,
    order: VecDeque<u64>,
}

impl ObkRandomReader {
    /// Open an encrypted image and every image behind it. One chain key opens
    /// them all; a file of another chain cannot join.
    pub fn open(path: &Path, key: &ChainKey) -> Result<Self> {
        Ok(Self {
            chain: ObkChain::open(path, key)?,
            cache: HashMap::new(),
            order: VecDeque::new(),
        })
    }

    pub fn virtual_size(&self) -> u64 {
        self.chain.virtual_size()
    }

    pub fn cluster_size(&self) -> u64 {
        self.chain.cluster_size()
    }

    pub fn chain_len(&self) -> usize {
        self.chain.chain_len()
    }

    /// The sealed half of the newest image's manifest.
    pub fn sealed_metadata(&mut self) -> Result<Zeroizing<Vec<u8>>> {
        self.chain.sealed_metadata()
    }

    pub fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or_else(|| bad("read offset overflows"))?;
        if end > self.virtual_size() {
            return Err(bad(format!(
                "read [{offset}, +{}) beyond virtual disk ({} bytes)",
                buf.len(),
                self.virtual_size()
            )));
        }
        let cs = self.cluster_size();
        let mut off = offset;
        let mut rest: &mut [u8] = buf;
        while !rest.is_empty() {
            let cluster = off / cs;
            let within = (off % cs) as usize;
            let take = (cs as usize - within).min(rest.len());
            let data = self.cluster(cluster)?;
            rest[..take].copy_from_slice(&data[within..within + take]);
            off += take as u64;
            rest = &mut rest[take..];
        }
        Ok(())
    }

    fn cluster(&mut self, cluster: u64) -> Result<&Zeroizing<Vec<u8>>> {
        if !self.cache.contains_key(&cluster) {
            let data = self.chain.read_cluster(cluster)?;
            if self.order.len() >= CLUSTER_CACHE_MAX {
                if let Some(evict) = self.order.pop_front() {
                    self.cache.remove(&evict);
                }
            }
            self.order.push_back(cluster);
            self.cache.insert(cluster, data);
        }
        Ok(&self.cache[&cluster])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seal::obk::container_tests::{cluster_bits, Fixture};
    use crate::sidecar::ChainRole;
    use uuid::Uuid;

    fn pattern(seed: u8, len: usize) -> Vec<u8> {
        (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
    }

    #[test]
    fn unaligned_writes_come_back_byte_for_byte_through_the_chain() {
        let f = Fixture::new();
        let dir = tempfile::tempdir().unwrap();
        let cluster = 1usize << cluster_bits();
        let path = dir.path().join("full.obk");

        // A GPT-shaped write pattern: a head that is not cluster-aligned, whole
        // clusters, and a tail that ends mid-cluster.
        let head = pattern(1, 512);
        let body = pattern(2, cluster * 2);
        let tail = pattern(3, 300);
        let mut writer =
            ObkStagedWriter::create(&path, &f.chain, f.options(Uuid::new_v4(), ChainRole::Full, None))
                .unwrap();
        writer.write_at(0, &head).unwrap();
        writer.write_at(cluster as u64, &body).unwrap();
        writer.write_at(cluster as u64 * 5 + 17, &tail).unwrap();
        writer.finish(b"sealed sidecar").unwrap();

        let mut reader = ObkRandomReader::open(&path, &f.chain).unwrap();
        assert_eq!(reader.chain_len(), 1);
        assert_eq!(reader.cluster_size(), cluster as u64);
        assert_eq!(&reader.sealed_metadata().unwrap()[..], b"sealed sidecar");

        let mut got = vec![0u8; head.len()];
        reader.read_at(0, &mut got).unwrap();
        assert_eq!(got, head);
        let mut got = vec![0u8; body.len()];
        reader.read_at(cluster as u64, &mut got).unwrap();
        assert_eq!(got, body);
        let mut got = vec![0u8; tail.len()];
        reader.read_at(cluster as u64 * 5 + 17, &mut got).unwrap();
        assert_eq!(got, tail);

        // Everything else reads as zeros, and a read past the disk is refused.
        let mut zeros = vec![0xFFu8; 64];
        reader.read_at(cluster as u64 * 4, &mut zeros).unwrap();
        assert!(zeros.iter().all(|&b| b == 0));
        let mut over = vec![0u8; 16];
        assert!(reader.read_at(reader.virtual_size() - 8, &mut over).is_err());
    }

    #[test]
    fn a_child_is_read_through_its_parent_at_byte_granularity() {
        let f = Fixture::new();
        let dir = tempfile::tempdir().unwrap();
        let cluster = 1usize << cluster_bits();

        let full_id = Uuid::new_v4();
        let base = pattern(7, cluster * 3);
        let mut writer = ObkStagedWriter::create(
            &dir.path().join("full.obk"),
            &f.chain,
            f.options(full_id, ChainRole::Full, None),
        )
        .unwrap();
        writer.write_at(0, &base).unwrap();
        writer.finish(b"full").unwrap();

        let changed = pattern(9, cluster);
        let mut writer = ObkStagedWriter::create(
            &dir.path().join("inc.obk"),
            &f.chain,
            f.options(
                Uuid::new_v4(),
                ChainRole::Incremental,
                Some((full_id, "full.obk")),
            ),
        )
        .unwrap();
        writer.write_at(cluster as u64, &changed).unwrap();
        writer.finish(b"inc").unwrap();

        let mut reader = ObkRandomReader::open(&dir.path().join("inc.obk"), &f.chain).unwrap();
        assert_eq!(reader.chain_len(), 2);
        // A read spanning the boundary between an inherited and an overwritten
        // cluster must stitch the two layers together.
        let mut got = vec![0u8; cluster];
        reader.read_at(cluster as u64 / 2, &mut got).unwrap();
        let mut expected = base[cluster / 2..cluster].to_vec();
        expected.extend_from_slice(&changed[..cluster / 2]);
        assert_eq!(got, expected);
        // The cache must not change any answer.
        let mut again = vec![0u8; cluster];
        reader.read_at(cluster as u64 / 2, &mut again).unwrap();
        assert_eq!(again, got);
    }

    #[test]
    fn an_unaligned_write_pattern_is_refused_rather_than_buffered_forever() {
        let f = Fixture::new();
        let dir = tempfile::tempdir().unwrap();
        let cluster = 1usize << cluster_bits();
        let mut opts = f.options(Uuid::new_v4(), ChainRole::Full, None);
        opts.virtual_size = (MAX_STAGED_CLUSTERS as u64 + 4) * cluster as u64;
        let mut writer = ObkStagedWriter::create(&dir.path().join("full.obk"), &f.chain, opts).unwrap();

        // One tiny write into each of a thousand different clusters: staged,
        // because none of them fills its cluster.
        for index in 0..MAX_STAGED_CLUSTERS as u64 {
            writer.write_at(index * cluster as u64, &[1u8; 8]).unwrap();
        }
        let Err(error) = writer.write_at(MAX_STAGED_CLUSTERS as u64 * cluster as u64, &[1u8; 8])
        else {
            panic!("an unbounded staging pattern must be refused");
        };
        assert!(error.to_string().contains("staged"), "unexpected: {error}");
    }
}
