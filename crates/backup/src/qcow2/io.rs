//! Byte-granular adapters between the engines (which write GPT structures and
//! backup blocks at arbitrary offsets) and the cluster-based write-once qcow2
//! format.
//!
//! [`Qcow2StagedWriter`] gives the engines the same `write_at` surface as
//! `WinRawDevice`/`VhdxWriter`: whole aligned clusters go straight to the
//! [`Qcow2Writer`]; partial-cluster writes (GPT head/tail, the last partial
//! backup block) are staged in memory and flushed as full clusters at finish.
//! Staging stays tiny by construction — a handful of GPT clusters plus block
//! tails — but a cap guards against misuse.
//!
//! [`Qcow2RandomReader`] is the reverse for verify/restore: `read_at` over a
//! qcow2 file, with unallocated clusters reading as zeros. Backing-file chains
//! are rejected until userspace chain resolution lands (phase Q3).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::File;
use std::io::{BufWriter, Seek, Write};
use std::path::Path;

use own_backup_common::Result;

use super::reader::Qcow2Reader;
use super::writer::{Qcow2Writer, Qcow2WriterOptions};
use super::bad;

/// Staged partial clusters are bounded by GPT structures + one tail per block
/// write; anything past this cap means the caller streams unaligned data.
const MAX_STAGED_CLUSTERS: usize = 1024;

pub struct Qcow2StagedWriter<W: Write + Seek> {
    inner: Qcow2Writer<W>,
    staged: BTreeMap<u64, Vec<u8>>,
    virtual_size: u64,
}

impl Qcow2StagedWriter<BufWriter<File>> {
    /// Create a new image file (must not exist yet) for a virtual disk of
    /// `opts.virtual_size` bytes.
    pub fn create(path: &Path, opts: Qcow2WriterOptions) -> Result<Self> {
        let file = File::create_new(path)?;
        Self::new(BufWriter::new(file), opts)
    }
}

impl<W: Write + Seek> Qcow2StagedWriter<W> {
    pub fn new(w: W, opts: Qcow2WriterOptions) -> Result<Self> {
        let inner = Qcow2Writer::new(w, opts)?;
        let virtual_size = inner.total_guest_clusters() * inner.cluster_size();
        Ok(Self { inner, staged: BTreeMap::new(), virtual_size })
    }

    /// Virtual disk size (rounded up to a whole cluster).
    pub fn length(&self) -> u64 {
        self.virtual_size
    }

    /// Write `data` at guest byte `offset`. Whole aligned clusters are written
    /// through immediately (write-once per cluster!); partial clusters are
    /// staged. Distinct writes must not overlap.
    pub fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        let cs = self.inner.cluster_size();
        let mut off = offset;
        let mut rest = data;
        while !rest.is_empty() {
            let cluster = off / cs;
            let within = off % cs;
            let take = ((cs - within) as usize).min(rest.len());
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
                        self.staged.entry(cluster).or_insert_with(|| vec![0u8; cs as usize])
                    }
                };
                buf[within as usize..within as usize + take]
                    .copy_from_slice(&rest[..take]);
            }
            off += take as u64;
            rest = &rest[take..];
        }
        Ok(())
    }

    /// Flush staged partial clusters and finalize the image (L1/L2, refcounts,
    /// header). Returns the flushed inner writer.
    pub fn finish(mut self) -> Result<W> {
        for (cluster, buf) in std::mem::take(&mut self.staged) {
            self.inner.write_cluster(cluster, &buf)?;
        }
        self.inner.finish()
    }
}

/// Longest accepted backing chain (a runaway/looping chain is corruption:
/// real chains are a full plus at most a few dozen increments).
const MAX_CHAIN_DEPTH: usize = 256;

/// `read_at` view over a qcow2 image **including its backing chain**, resolved
/// in userspace (this is what replaces the kernel's AVHDX chain walking):
/// a cluster the leaf does not carry falls through to its parent, and so on;
/// past the last layer it reads as zeros. Relative backing paths are resolved
/// against the image's directory (the qemu convention — the engines store just
/// the parent's file name, so a backup folder stays relocatable).
/// Cap on cached decompressed clusters: 1024 × 64 KiB = 64 MiB. Sequential
/// verify/restore touch each cluster once (cache-neutral); the win is the
/// mount paths (iSCSI on Windows, NBD on Linux), where the kernel reads
/// in 4 KiB chunks and would otherwise decompress the same cluster sixteen
/// times — an NTFS mount walks $MFT/$LogFile/$Bitmap this way and 8 MiB was
/// too small to hold the mount's working set.
const CLUSTER_CACHE_MAX: usize = 1024;

pub struct Qcow2RandomReader {
    /// Leaf first, chain root last.
    layers: Vec<Qcow2Reader<File>>,
    /// Chain-resolved cluster cache: guest cluster → data (None = hole).
    cache: HashMap<u64, Option<Vec<u8>>>,
    /// Insertion order for cheap FIFO eviction.
    cache_order: VecDeque<u64>,
}

impl Qcow2RandomReader {
    pub fn open(path: &Path) -> Result<Self> {
        let dir = path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
        let mut layers = Vec::new();
        let mut current = path.to_path_buf();
        loop {
            if layers.len() >= MAX_CHAIN_DEPTH {
                return Err(bad(format!(
                    "backing chain longer than {MAX_CHAIN_DEPTH} images — corrupted chain?"
                )));
            }
            let file = File::open(&current).map_err(|e| {
                bad(format!("cannot open chain image {}: {e}", current.display()))
            })?;
            let layer = Qcow2Reader::open(file)
                .map_err(|e| bad(format!("{}: {e}", current.display())))?;
            let backing = layer.backing_file().map(|b| {
                let p = Path::new(b);
                if p.is_absolute() { p.to_path_buf() } else { dir.join(p) }
            });
            layers.push(layer);
            match backing {
                Some(parent) => current = parent,
                None => break,
            }
        }
        // Mixed cluster sizes would break the per-cluster fall-through walk;
        // our writers always use the default, so this only fires on foreign
        // or corrupted images.
        let cs = layers[0].cluster_size();
        if layers.iter().any(|l| l.cluster_size() != cs) {
            return Err(bad("backing chain mixes cluster sizes — not supported"));
        }
        Ok(Self { layers, cache: HashMap::new(), cache_order: VecDeque::new() })
    }

    /// Virtual size of the leaf image (authoritative for the whole chain).
    pub fn virtual_size(&self) -> u64 {
        self.layers[0].virtual_size()
    }

    /// Cluster size (uniform across the chain — enforced in `open`).
    pub fn cluster_size(&self) -> u64 {
        self.layers[0].cluster_size()
    }

    /// Number of images in the chain (1 = standalone full).
    pub fn chain_len(&self) -> usize {
        self.layers.len()
    }

    pub fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        if offset + buf.len() as u64 > self.virtual_size() {
            return Err(bad(format!(
                "read [{offset}, +{}) beyond virtual disk ({} bytes)",
                buf.len(),
                self.virtual_size()
            )));
        }
        let cs = self.layers[0].cluster_size();
        let mut off = offset;
        let mut rest: &mut [u8] = buf;
        while !rest.is_empty() {
            let cluster = off / cs;
            let within = (off % cs) as usize;
            let take = (cs as usize - within).min(rest.len());
            match self.read_chain_cluster(cluster)? {
                Some(data) => rest[..take].copy_from_slice(&data[within..within + take]),
                None => rest[..take].fill(0),
            }
            off += take as u64;
            rest = &mut rest[take..];
        }
        Ok(())
    }

    /// First layer that carries the cluster wins; explicit zero clusters stop
    /// the walk too (they override the parent). `None` = zeros.
    fn read_chain_cluster(&mut self, cluster: u64) -> Result<Option<Vec<u8>>> {
        if let Some(cached) = self.cache.get(&cluster) {
            return Ok(cached.clone());
        }
        let mut result = None;
        for layer in &mut self.layers {
            if cluster >= layer.total_clusters() {
                // A parent smaller than the leaf: past its end = zeros there,
                // keep falling through (matches qemu semantics).
                continue;
            }
            if let Some(data) = layer.read_cluster(cluster)? {
                result = Some(data);
                break;
            }
        }
        if self.cache_order.len() >= CLUSTER_CACHE_MAX {
            if let Some(evict) = self.cache_order.pop_front() {
                self.cache.remove(&evict);
            }
        }
        self.cache.insert(cluster, result.clone());
        self.cache_order.push_back(cluster);
        Ok(result)
    }
}

// `Qcow2Reader<File>` needs File: Read + Seek — true; keep the reader seekable
// without buffering because access is random (BufReader would discard its
// buffer on every seek anyway).
impl std::fmt::Debug for Qcow2RandomReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Qcow2RandomReader")
            .field("virtual_size", &self.virtual_size())
            .field("chain_len", &self.chain_len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn opts(size: u64) -> Qcow2WriterOptions {
        Qcow2WriterOptions { virtual_size: size, ..Default::default() }
    }

    #[test]
    fn staged_writes_roundtrip_through_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("staged.qcow2");
        let mut w = Qcow2StagedWriter::create(&path, opts(4 * 1024 * 1024)).unwrap();

        // GPT-like scattered small writes into cluster 0 …
        w.write_at(0, &[0xAB; 512]).unwrap();
        w.write_at(512, &[0xCD; 512]).unwrap();
        // … an aligned full-cluster write …
        w.write_at(1024 * 1024, &vec![0x77u8; 64 * 1024]).unwrap();
        // … a write crossing a cluster boundary (full + partial tail) …
        w.write_at(2 * 1024 * 1024, &vec![0x55u8; 64 * 1024 + 4096]).unwrap();
        // … and a tail write into the image's last sectors.
        w.write_at(4 * 1024 * 1024 - 512, &[0xEE; 512]).unwrap();
        w.finish().unwrap();

        let mut r = Qcow2RandomReader::open(&path).unwrap();
        let mut buf = vec![0u8; 1024];
        r.read_at(0, &mut buf).unwrap();
        assert_eq!(&buf[..512], &[0xAB; 512]);
        assert_eq!(&buf[512..], &[0xCD; 512]);

        let mut buf = vec![0u8; 64 * 1024];
        r.read_at(1024 * 1024, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0x77));

        let mut buf = vec![0u8; 64 * 1024 + 4096];
        r.read_at(2 * 1024 * 1024, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0x55));
        // The staged tail cluster is zero-padded past the written range.
        let mut pad = vec![0u8; 4096];
        r.read_at(2 * 1024 * 1024 + 64 * 1024 + 4096, &mut pad).unwrap();
        assert!(pad.iter().all(|&b| b == 0));

        let mut buf = vec![0u8; 512];
        r.read_at(4 * 1024 * 1024 - 512, &mut buf).unwrap();
        assert_eq!(buf, vec![0xEE; 512]);

        // Reads spanning written and unallocated regions see zeros in holes.
        let mut buf = vec![0u8; 128 * 1024];
        r.read_at(1024 * 1024 - 4096, &mut buf).unwrap();
        assert!(buf[..4096].iter().all(|&b| b == 0));
        assert!(buf[4096..4096 + 64 * 1024].iter().all(|&b| b == 0x77));
        assert!(buf[4096 + 64 * 1024..].iter().all(|&b| b == 0));
    }

    #[test]
    fn in_memory_writer_works_too() {
        let mut w = Qcow2StagedWriter::new(Cursor::new(Vec::new()), opts(1024 * 1024)).unwrap();
        w.write_at(100, b"hello").unwrap();
        let image = w.finish().unwrap().into_inner();
        let mut reader =
            super::super::reader::Qcow2Reader::open(Cursor::new(image)).unwrap();
        let cluster = reader.read_cluster(0).unwrap().unwrap();
        assert_eq!(&cluster[100..105], b"hello");
    }
}
