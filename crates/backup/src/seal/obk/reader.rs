//! Reading an `.obk`: authenticate first, disclose afterwards.
//!
//! Order of trust, and nothing may skip a step:
//!
//! 1. the qcow2 header must be exactly the shape we write (`header.rs`);
//! 2. the sealed root record must open under the chain key **and** agree with
//!    the open header about every identity and the geometry;
//! 3. an index chunk must be the record the root named, in the cluster range
//!    the root gave it;
//! 4. a data record must be the record the index named, and the L2 table must
//!    point exactly there.
//!
//! Step 4 is why a cluster table cannot be edited into pointing at another
//! valid record, and why an older record of the same cluster is refused: the
//! expected identity comes from the index, never from the record found.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use own_backup_common::{OwnBackupError, Result};
use uuid::Uuid;
use zeroize::Zeroizing;

use super::header::{ObkFileHeader, ObkOpenHeader};
use super::index::{ClusterState, IndexChunk};
use super::root::ObkRoot;
use super::{bad, MAX_CHAIN_DEPTH};
use crate::qcow2::{decode_l2_entry, ClusterDescriptor, OFFSET_MASK};
use crate::seal::cluster::{
    framed_len, open_record, Encoding, OpenedRecord, Purpose, RecordContext, RecordId, HEADER_LEN,
    MAX_PAYLOAD_LEN, TAG_LEN,
};
use crate::seal::ChainKey;
use crate::sidecar::ChainRole;

/// Cached L2 tables (one cluster each) and index chunks (under 1 MiB each).
const L2_CACHE_MAX: usize = 32;
const INDEX_CACHE_MAX: usize = 4;

pub struct ObkImage<R: Read + Seek> {
    r: R,
    header: ObkFileHeader,
    root: ObkRoot,
    chain: ChainKey,
    l1: Vec<u64>,
    l2_cache: HashMap<u64, Vec<u64>>,
    index_cache: HashMap<usize, IndexChunk>,
}

impl ObkImage<BufReader<File>> {
    pub fn open_path(path: &Path, chain: &ChainKey) -> Result<Self> {
        let file = File::open(path)
            .map_err(|e| bad(format!("cannot open {}: {e}", path.display())))?;
        Self::open(BufReader::new(file), chain)
            .map_err(|e| bad(format!("{}: {e}", path.display())))
    }
}

/// The open header on its own, for listings: no key, no authentication, and
/// therefore nothing from it may be shown as a fact about the backup's content.
pub fn read_open_header(path: &Path) -> Result<ObkOpenHeader> {
    let file = File::open(path).map_err(|e| bad(format!("cannot open {}: {e}", path.display())))?;
    let mut r = BufReader::new(file);
    Ok(ObkFileHeader::read(&mut r)?.open)
}

impl<R: Read + Seek> ObkImage<R> {
    pub fn open(mut r: R, chain: &ChainKey) -> Result<Self> {
        let header = ObkFileHeader::read(&mut r)?;
        let chain_id = *header.open.chain_id.as_bytes();

        // The root record, named by the open header and proved by the key.
        let pointer = &header.open.root;
        let bytes = read_bytes(&mut r, pointer.offset, pointer.len as usize)?;
        if bytes.len() != pointer.len as usize {
            return Err(bad("root record runs past the end of the image"));
        }
        let payload = open_chunk(
            chain,
            RecordContext {
                chain_id,
                purpose: Purpose::Manifest,
                index: 0,
            },
            pointer.record_id()?,
            &bytes,
        )?;
        let root = ObkRoot::decode(&payload)?;

        // The open half was untrusted until exactly here.
        let open = &header.open;
        if root.chain_id != open.chain_id
            || root.backup_id != open.backup_id
            || root.parent_backup_id != open.parent_backup_id
            || root.role != open.role
            || root.virtual_size != header.virtual_size
            || root.cluster_bits != header.cluster_bits
        {
            return Err(bad(
                "the image's open header and its sealed root record disagree",
            ));
        }

        let l1 = read_l1(&mut r, &header)?;
        Ok(Self {
            r,
            header,
            root,
            chain: chain.clone(),
            l1,
            l2_cache: HashMap::new(),
            index_cache: HashMap::new(),
        })
    }

    pub fn open_header(&self) -> &ObkOpenHeader {
        &self.header.open
    }

    /// The qcow2 half too, for the one caller that rewrites a header cluster.
    pub fn file_header(&self) -> &ObkFileHeader {
        &self.header
    }

    pub fn root(&self) -> &ObkRoot {
        &self.root
    }

    pub fn backup_id(&self) -> Uuid {
        self.root.backup_id
    }

    pub fn virtual_size(&self) -> u64 {
        self.header.virtual_size
    }

    pub fn cluster_size(&self) -> u64 {
        self.header.cluster_size
    }

    pub fn total_clusters(&self) -> u64 {
        self.root.guest_clusters
    }

    /// Parent file name as stored in the header. Resolving it is the chain's
    /// job; the identity it must have is in the sealed root record.
    pub fn backing_file(&self) -> Option<&str> {
        self.header.backing_file.as_deref()
    }

    /// The sealed half of the manifest: whatever the backup put there.
    pub fn sealed_metadata(&mut self) -> Result<Zeroizing<Vec<u8>>> {
        let chain_id = *self.root.chain_id.as_bytes();
        let mut out = Zeroizing::new(Vec::with_capacity(self.root.sealed_metadata_len as usize));
        for (ordinal, chunk) in self.root.meta_chunks.clone().iter().enumerate() {
            let bytes = read_bytes(&mut self.r, chunk.offset, chunk.len as usize)?;
            if bytes.len() != chunk.len as usize {
                return Err(bad("metadata chunk runs past the end of the image"));
            }
            let payload = open_chunk(
                &self.chain,
                RecordContext {
                    chain_id,
                    purpose: Purpose::Manifest,
                    index: ordinal as u64 + 1,
                },
                chunk.id,
                &bytes,
            )?;
            out.extend_from_slice(&payload);
        }
        // Reserved metadata is padded so a later sidecar of the same size can
        // replace it in place; the root says how much of it is the sidecar.
        if (out.len() as u64) < self.root.sealed_metadata_len {
            return Err(bad("sealed metadata is shorter than the declared length"));
        }
        out.truncate(self.root.sealed_metadata_len as usize);
        Ok(out)
    }

    /// What this image says about a guest cluster — before reading anything.
    pub fn cluster_state(&mut self, cluster: u64) -> Result<ClusterState> {
        if cluster >= self.root.guest_clusters {
            return Err(bad(format!("cluster {cluster} beyond the virtual disk")));
        }
        let Some(position) = self.chunk_for(cluster) else {
            return Ok(ClusterState::Absent);
        };
        Ok(self.load_index_chunk(position)?.lookup(cluster))
    }

    /// Read one guest cluster. `None` means this image does not define it: the
    /// parent answers, or the cluster is zero when there is no parent.
    pub fn read_cluster(&mut self, cluster: u64) -> Result<Option<Zeroizing<Vec<u8>>>> {
        let state = self.cluster_state(cluster)?;
        let descriptor = self.descriptor(cluster)?;
        match (state, descriptor) {
            (ClusterState::Absent, ClusterDescriptor::Unallocated) => Ok(None),
            (ClusterState::Zero, ClusterDescriptor::Zero) => Ok(Some(Zeroizing::new(vec![
                0u8;
                self.cluster_size() as usize
            ]))),
            (
                ClusterState::Present(id),
                ClusterDescriptor::Compressed {
                    host_offset,
                    length,
                },
            ) => self.read_record(cluster, id, host_offset, length).map(Some),
            // Any other pairing means the cluster table was edited, the index
            // was replaced, or the file is damaged. None of those may answer.
            _ => Err(bad(format!(
                "cluster {cluster}: the image's index and its cluster table disagree"
            ))),
        }
    }

    fn read_record(
        &mut self,
        cluster: u64,
        id: RecordId,
        host_offset: u64,
        length: u64,
    ) -> Result<Zeroizing<Vec<u8>>> {
        let cluster_size = self.cluster_size() as usize;
        // The descriptor's length is rounded up to the end of a sector, so read
        // that much and trim to the record's own framing.
        let capped = (length as usize).min(HEADER_LEN + MAX_PAYLOAD_LEN + TAG_LEN);
        let bytes = read_bytes(&mut self.r, host_offset, capped)?;
        let framed = framed_len(&bytes)?;
        if framed > bytes.len() || framed as u64 > length {
            return Err(bad(format!(
                "cluster {cluster}: record does not fit where the cluster table puts it"
            )));
        }
        let payload = open_chunk(
            &self.chain,
            RecordContext {
                chain_id: *self.root.chain_id.as_bytes(),
                purpose: Purpose::Data,
                index: cluster,
            },
            id,
            &bytes[..framed],
        )?;
        if payload.len() != cluster_size {
            return Err(bad(format!(
                "cluster {cluster}: record holds {} bytes, not a whole cluster",
                payload.len()
            )));
        }
        Ok(payload)
    }

    /// Index of the chunk that owns `cluster`, if any chunk does.
    fn chunk_for(&self, cluster: u64) -> Option<usize> {
        let chunks = &self.root.index_chunks;
        match chunks.binary_search_by(|c| c.first_cluster.cmp(&cluster)) {
            Ok(exact) => Some(exact),
            // The chunk before the first one starting after `cluster`.
            Err(0) => None,
            Err(after) => Some(after - 1),
        }
    }

    fn load_index_chunk(&mut self, position: usize) -> Result<&IndexChunk> {
        if !self.index_cache.contains_key(&position) {
            let reference = *self
                .root
                .index_chunks
                .get(position)
                .ok_or_else(|| bad("index chunk out of range"))?;
            let bytes = read_bytes(&mut self.r, reference.chunk.offset, reference.chunk.len as usize)?;
            if bytes.len() != reference.chunk.len as usize {
                return Err(bad("index chunk runs past the end of the image"));
            }
            let payload = open_chunk(
                &self.chain,
                RecordContext {
                    chain_id: *self.root.chain_id.as_bytes(),
                    purpose: Purpose::Index,
                    index: position as u64,
                },
                reference.chunk.id,
                &bytes,
            )?;
            // Chunks partition the guest clusters: this one owns everything up
            // to the next chunk's first cluster, so no two chunks can answer
            // for the same cluster and a lookup that finds nothing means
            // "absent" rather than "look further".
            let end = self
                .root
                .index_chunks
                .get(position + 1)
                .map(|next| next.first_cluster)
                .unwrap_or(self.root.guest_clusters);
            let chunk = IndexChunk::parse(
                payload.to_vec(),
                reference.entries,
                reference.first_cluster..end,
            )?;
            if chunk.first_cluster() != Some(reference.first_cluster) {
                return Err(bad("index chunk does not start where the root says"));
            }
            if self.index_cache.len() >= INDEX_CACHE_MAX {
                self.index_cache.clear();
            }
            self.index_cache.insert(position, chunk);
        }
        Ok(&self.index_cache[&position])
    }

    /// The raw cluster-table entry, for a pass that re-emits the tables
    /// (`seal::obk::rebuild`). Still only a pointer: what the cluster must be
    /// comes from the index.
    pub fn raw_l2_entry(&mut self, cluster: u64) -> Result<u64> {
        let l2_per_table = self.cluster_size() / 8;
        let l1_index = cluster / l2_per_table;
        let Some(&l1_entry) = self.l1.get(l1_index as usize) else {
            return Ok(0);
        };
        if l1_entry & OFFSET_MASK == 0 {
            return Ok(0);
        }
        self.descriptor(cluster)?; // fills the cache for this table
        Ok(self.l2_cache[&l1_index][(cluster % l2_per_table) as usize])
    }

    /// What the qcow2 cluster table says — a pointer, never an authority.
    fn descriptor(&mut self, cluster: u64) -> Result<ClusterDescriptor> {
        let l2_per_table = self.cluster_size() / 8;
        let l1_index = cluster / l2_per_table;
        let Some(&l1_entry) = self.l1.get(l1_index as usize) else {
            return Ok(ClusterDescriptor::Unallocated);
        };
        let l2_offset = l1_entry & OFFSET_MASK;
        if l2_offset == 0 {
            return Ok(ClusterDescriptor::Unallocated);
        }
        if !self.l2_cache.contains_key(&l1_index) {
            let cluster_size = self.cluster_size() as usize;
            let bytes = read_bytes(&mut self.r, l2_offset, cluster_size)?;
            if bytes.len() != cluster_size {
                return Err(bad("cluster table runs past the end of the image"));
            }
            let table: Vec<u64> = bytes
                .as_chunks::<8>()
                .0
                .iter()
                .map(|c| u64::from_be_bytes(*c))
                .collect();
            if self.l2_cache.len() >= L2_CACHE_MAX {
                self.l2_cache.clear();
            }
            self.l2_cache.insert(l1_index, table);
        }
        let entry = self.l2_cache[&l1_index][(cluster % l2_per_table) as usize];
        decode_l2_entry(entry, self.header.cluster_bits)
    }
}

/// A whole chain, newest image first, with every link authenticated.
pub struct ObkChain {
    images: Vec<ObkImage<BufReader<File>>>,
}

impl ObkChain {
    /// Open `path` and every parent it names. One chain key opens all of them:
    /// an image of another chain cannot join, and an image whose identity does
    /// not match what its child expects is refused rather than read.
    pub fn open(path: &Path, chain: &ChainKey) -> Result<Self> {
        let mut images: Vec<ObkImage<BufReader<File>>> = Vec::new();
        let mut current = path.to_path_buf();
        let mut visited: Vec<PathBuf> = Vec::new();
        loop {
            if images.len() >= MAX_CHAIN_DEPTH {
                return Err(bad(format!(
                    "backing chain longer than {MAX_CHAIN_DEPTH} images — corrupted chain?"
                )));
            }
            let here = std::fs::canonicalize(&current).unwrap_or_else(|_| current.clone());
            if visited.contains(&here) {
                return Err(bad(format!(
                    "backing chain comes back to {} — it is a loop",
                    current.display()
                )));
            }
            visited.push(here);

            let image = ObkImage::open_path(&current, chain)?;
            if let Some(child) = images.last() {
                check_link(child, &image)?;
            }
            let backing = image.backing_file().map(|name| {
                let p = Path::new(name);
                match current.parent() {
                    Some(beside) if !p.is_absolute() => beside.join(p),
                    _ => p.to_path_buf(),
                }
            });
            images.push(image);
            match backing {
                Some(parent) => current = parent,
                None => break,
            }
        }
        // The walk ends at an image with no backing file; only a full backup
        // may be that image.
        if images.last().is_some_and(|base| base.root.role != ChainRole::Full) {
            return Err(bad("the chain does not end in a full backup"));
        }
        Ok(Self { images })
    }

    pub fn chain_len(&self) -> usize {
        self.images.len()
    }

    pub fn virtual_size(&self) -> u64 {
        self.images[0].virtual_size()
    }

    pub fn cluster_size(&self) -> u64 {
        self.images[0].cluster_size()
    }

    pub fn leaf(&mut self) -> &mut ObkImage<BufReader<File>> {
        &mut self.images[0]
    }

    /// Sealed metadata of the newest image.
    pub fn sealed_metadata(&mut self) -> Result<Zeroizing<Vec<u8>>> {
        self.images[0].sealed_metadata()
    }

    /// The first image that defines the cluster wins; an explicit zero stops
    /// the walk as surely as data does.
    pub fn read_cluster(&mut self, cluster: u64) -> Result<Zeroizing<Vec<u8>>> {
        for image in &mut self.images {
            if let Some(data) = image.read_cluster(cluster)? {
                return Ok(data);
            }
        }
        Ok(Zeroizing::new(vec![0u8; self.cluster_size() as usize]))
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
        let cluster_size = self.cluster_size();
        let mut at = offset;
        let mut rest: &mut [u8] = buf;
        while !rest.is_empty() {
            let cluster = at / cluster_size;
            let within = (at % cluster_size) as usize;
            let take = (cluster_size as usize - within).min(rest.len());
            let data = self.read_cluster(cluster)?;
            rest[..take].copy_from_slice(&data[within..within + take]);
            at += take as u64;
            rest = &mut rest[take..];
        }
        Ok(())
    }
}

/// Every link in a chain is checked against the sealed identities, not against
/// the file names that led us there.
fn check_link(child: &ObkImage<BufReader<File>>, parent: &ObkImage<BufReader<File>>) -> Result<()> {
    if child.root.parent_backup_id != Some(parent.root.backup_id) {
        return Err(bad("backing file is not the parent this image was written against"));
    }
    if child.root.chain_id != parent.root.chain_id {
        return Err(bad("backing file belongs to another chain"));
    }
    if child.root.cluster_bits != parent.root.cluster_bits
        || child.root.virtual_size != parent.root.virtual_size
    {
        return Err(bad("backing file has a different geometry"));
    }
    if child.root.role == ChainRole::Differential && parent.root.role != ChainRole::Full {
        return Err(bad("a differential image must sit directly on a full backup"));
    }
    if parent.root.role == ChainRole::Differential {
        return Err(bad("a differential image is never a parent"));
    }
    Ok(())
}

/// Authenticate a record and undo its encoding, bounded by the length the
/// record itself authenticated. Nothing is decompressed before the tag checks.
fn open_chunk(
    chain: &ChainKey,
    context: RecordContext,
    id: RecordId,
    bytes: &[u8],
) -> Result<Zeroizing<Vec<u8>>> {
    decode_payload(open_record(chain, context, id, bytes)?)
}

fn decode_payload(opened: OpenedRecord) -> Result<Zeroizing<Vec<u8>>> {
    let expected = opened.decoded_len as usize;
    match opened.encoding {
        Encoding::Raw => {
            if opened.payload.len() != expected {
                return Err(bad("record payload is not its authenticated length"));
            }
            Ok(opened.payload)
        }
        Encoding::Zstd => {
            let plain = Zeroizing::new(
                zstd::bulk::decompress(&opened.payload, expected)
                    .map_err(|e| OwnBackupError::Compression(format!("sealed record: {e}")))?,
            );
            if plain.len() != expected {
                return Err(bad("decompressed record is not its authenticated length"));
            }
            Ok(plain)
        }
    }
}

/// Read up to `len` bytes; a short read is returned as-is so the caller can say
/// what exactly was missing instead of reporting a bare I/O error.
fn read_bytes<R: Read + Seek>(r: &mut R, offset: u64, len: usize) -> Result<Vec<u8>> {
    r.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; len];
    let mut filled = 0;
    while filled < len {
        match r.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    buf.truncate(filled);
    Ok(buf)
}

fn read_l1<R: Read + Seek>(r: &mut R, header: &ObkFileHeader) -> Result<Vec<u64>> {
    let bytes = read_bytes(r, header.l1_table_offset, header.l1_size as usize * 8)?;
    if bytes.len() != header.l1_size as usize * 8 {
        return Err(bad("L1 table runs past the end of the image"));
    }
    Ok(bytes
        .as_chunks::<8>()
        .0
        .iter()
        .map(|c| u64::from_be_bytes(*c))
        .collect())
}
