//! Write-once qcow2 image writer (pattern of `image_writer.rs` /
//! `vhdx_format.rs`): stream guest clusters in, then `finish()` lays down the
//! L2 tables, L1 table, refcount structures and the final header.
//!
//! Layout produced (all metadata after the data, header patched last):
//! ```text
//! cluster 0            header + extensions + backing file name
//! clusters 1..         guest data: compressed clusters packed byte-to-byte,
//!                      incompressible/uncompressed clusters cluster-aligned
//! ...                  L2 tables (one cluster each)
//! ...                  L1 table
//! ...                  refcount blocks, refcount table
//! ```

use std::collections::BTreeMap;
use std::io::{Seek, SeekFrom, Write};

use own_backup_common::{OwnBackupError, Result};

use super::{
    bad, encode_compressed_l2_entry, COMPRESSION_TYPE_DEFLATE, COMPRESSION_TYPE_ZSTD,
    DEFAULT_CLUSTER_BITS, HEADER_LENGTH_V3, HEXT_BACKING_FORMAT, HEXT_END,
    INCOMPAT_COMPRESSION_TYPE, OFLAG_COPIED, OFLAG_ZERO, QCOW2_MAGIC, REFCOUNT_ORDER,
};

/// Compression applied to guest clusters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Qcow2Compression {
    /// All clusters stored raw (cluster-aligned).
    None,
    /// zstd per cluster; clusters that do not shrink are stored raw
    /// (mirrors qemu behaviour and `compression.rs`).
    Zstd { level: i32 },
}

pub struct Qcow2WriterOptions {
    /// Virtual disk size in bytes; rounded up to a whole cluster internally
    /// (the manifest stays authoritative for the exact source size).
    pub virtual_size: u64,
    /// log2 of the cluster size; [`DEFAULT_CLUSTER_BITS`] = 64 KiB.
    pub cluster_bits: u32,
    pub compression: Qcow2Compression,
    /// Backing (parent) image path for incremental/differential children,
    /// stored verbatim in the header. `None` for full backups.
    pub backing_file: Option<String>,
}

impl Default for Qcow2WriterOptions {
    fn default() -> Self {
        Self {
            virtual_size: 0,
            cluster_bits: DEFAULT_CLUSTER_BITS,
            compression: Qcow2Compression::Zstd { level: super::DEFAULT_ZSTD_LEVEL },
            backing_file: None,
        }
    }
}

pub struct Qcow2Writer<W: Write + Seek> {
    w: W,
    cluster_bits: u32,
    cluster_size: u64,
    virtual_size: u64,
    total_guest_clusters: u64,
    compression: Qcow2Compression,
    backing_file: Option<String>,
    /// guest cluster index → final L2 entry.
    l2_entries: BTreeMap<u64, u64>,
    /// host cluster index → number of compressed-cluster references
    /// (packed streams share boundary clusters; needed for refcounts).
    compressed_refs: BTreeMap<u64, u64>,
    /// Next free byte in the file.
    current_offset: u64,
}

impl<W: Write + Seek> Qcow2Writer<W> {
    pub fn new(mut w: W, opts: Qcow2WriterOptions) -> Result<Self> {
        if opts.virtual_size == 0 {
            return Err(bad("virtual size must be non-zero"));
        }
        if !(9..=21).contains(&opts.cluster_bits) {
            return Err(bad(format!("cluster_bits {} out of range 9..=21", opts.cluster_bits)));
        }
        if let Some(name) = &opts.backing_file {
            if name.is_empty() || name.len() > 1023 {
                return Err(bad("backing file name empty or longer than 1023 bytes"));
            }
        }
        let cluster_size = 1u64 << opts.cluster_bits;
        let virtual_size = opts.virtual_size.div_ceil(cluster_size) * cluster_size;

        // Placeholder header cluster; the real header is written by finish().
        w.seek(SeekFrom::Start(0))?;
        w.write_all(&vec![0u8; cluster_size as usize])?;

        Ok(Self {
            w,
            cluster_bits: opts.cluster_bits,
            cluster_size,
            virtual_size,
            total_guest_clusters: virtual_size / cluster_size,
            compression: opts.compression,
            backing_file: opts.backing_file,
            l2_entries: BTreeMap::new(),
            compressed_refs: BTreeMap::new(),
            current_offset: cluster_size,
        })
    }

    pub fn cluster_size(&self) -> u64 {
        self.cluster_size
    }

    pub fn total_guest_clusters(&self) -> u64 {
        self.total_guest_clusters
    }

    /// Write one guest cluster (`data.len()` must equal the cluster size —
    /// the caller pads the tail block). All-zero clusters are not stored:
    /// they become "unallocated" in a standalone image and an explicit
    /// zero-cluster in a child (so they override the backing file).
    pub fn write_cluster(&mut self, index: u64, data: &[u8]) -> Result<()> {
        if index >= self.total_guest_clusters {
            return Err(bad(format!(
                "cluster index {index} beyond virtual disk ({} clusters)",
                self.total_guest_clusters
            )));
        }
        if data.len() as u64 != self.cluster_size {
            return Err(bad(format!(
                "cluster data is {} bytes, expected {}",
                data.len(),
                self.cluster_size
            )));
        }
        if self.l2_entries.contains_key(&index) {
            return Err(bad(format!("cluster {index} written twice (write-once writer)")));
        }

        if data.iter().all(|&b| b == 0) {
            if self.backing_file.is_some() {
                self.l2_entries.insert(index, OFLAG_ZERO);
            }
            // No backing file: leave unallocated — reads back as zeros.
            return Ok(());
        }

        if let Qcow2Compression::Zstd { level } = self.compression {
            let compressed = zstd::bulk::compress(data, level)
                .map_err(|e| OwnBackupError::Compression(e.to_string()))?;
            if (compressed.len() as u64) < self.cluster_size {
                return self.write_compressed(index, &compressed);
            }
            // Did not shrink — fall through to a raw aligned cluster.
        }
        self.write_raw(index, data)
    }

    fn write_compressed(&mut self, index: u64, compressed: &[u8]) -> Result<()> {
        let host_offset = self.current_offset;
        let len = compressed.len() as u64;
        let entry = encode_compressed_l2_entry(host_offset, len, self.cluster_bits)?;

        self.w.write_all(compressed)?;
        self.current_offset += len;

        for host_cluster in (host_offset >> self.cluster_bits)
            ..=((host_offset + len - 1) >> self.cluster_bits)
        {
            *self.compressed_refs.entry(host_cluster).or_insert(0) += 1;
        }
        self.l2_entries.insert(index, entry);
        Ok(())
    }

    fn write_raw(&mut self, index: u64, data: &[u8]) -> Result<()> {
        self.pad_to_cluster_boundary()?;
        let host_offset = self.current_offset;
        self.w.write_all(data)?;
        self.current_offset += self.cluster_size;
        self.l2_entries.insert(index, OFLAG_COPIED | host_offset);
        Ok(())
    }

    fn pad_to_cluster_boundary(&mut self) -> Result<()> {
        let rem = self.current_offset % self.cluster_size;
        if rem != 0 {
            let pad = (self.cluster_size - rem) as usize;
            self.w.write_all(&vec![0u8; pad])?;
            self.current_offset += pad as u64;
        }
        Ok(())
    }

    /// Write L2/L1/refcount metadata and the final header. Returns the inner
    /// writer (flushed).
    pub fn finish(mut self) -> Result<W> {
        let cluster_size = self.cluster_size;
        let l2_per_table = cluster_size / 8;
        let l1_entries = self.total_guest_clusters.div_ceil(l2_per_table);

        // --- L2 tables (one cluster each), grouped by L1 index. ---
        self.pad_to_cluster_boundary()?;
        let mut l1_table = vec![0u64; l1_entries as usize];
        {
            let mut groups: BTreeMap<u64, Vec<(u64, u64)>> = BTreeMap::new();
            for (&guest, &entry) in &self.l2_entries {
                groups.entry(guest / l2_per_table).or_default().push((guest % l2_per_table, entry));
            }
            for (l1_index, entries) in groups {
                let table_offset = self.current_offset;
                let mut table = vec![0u8; cluster_size as usize];
                for (slot, entry) in entries {
                    table[slot as usize * 8..slot as usize * 8 + 8]
                        .copy_from_slice(&entry.to_be_bytes());
                }
                self.w.write_all(&table)?;
                self.current_offset += cluster_size;
                l1_table[l1_index as usize] = OFLAG_COPIED | table_offset;
            }
        }

        // --- L1 table, padded to whole clusters. ---
        let l1_table_offset = self.current_offset;
        {
            let mut buf: Vec<u8> = Vec::with_capacity((l1_entries as usize) * 8);
            for entry in &l1_table {
                buf.extend_from_slice(&entry.to_be_bytes());
            }
            self.w.write_all(&buf)?;
            self.current_offset += buf.len() as u64;
            self.pad_to_cluster_boundary()?;
        }

        // --- Refcount structures: fixpoint on the total cluster count, ---
        // --- since the blocks/table must refcount themselves too.      ---
        let refs_per_block = cluster_size / 2; // 16-bit refcounts
        let data_clusters = self.current_offset / cluster_size;
        let (mut rc_blocks, mut rc_table_clusters) = (0u64, 0u64);
        loop {
            let total = data_clusters + rc_blocks + rc_table_clusters;
            let need_blocks = total.div_ceil(refs_per_block);
            let need_table = (need_blocks * 8).div_ceil(cluster_size);
            if need_blocks == rc_blocks && need_table == rc_table_clusters {
                break;
            }
            rc_blocks = need_blocks;
            rc_table_clusters = need_table;
        }
        let total_clusters = data_clusters + rc_blocks + rc_table_clusters;
        let refblocks_offset = self.current_offset;
        let refcount_table_offset = refblocks_offset + rc_blocks * cluster_size;

        // Every host cluster in the file is referenced exactly once, except
        // clusters holding packed compressed data, whose refcount equals the
        // number of compressed clusters overlapping them.
        for block in 0..rc_blocks {
            let mut buf = vec![0u8; cluster_size as usize];
            for slot in 0..refs_per_block {
                let cluster = block * refs_per_block + slot;
                if cluster >= total_clusters {
                    break;
                }
                let count = self.compressed_refs.get(&cluster).copied().unwrap_or(1);
                let count = u16::try_from(count)
                    .map_err(|_| bad(format!("refcount overflow on host cluster {cluster}")))?;
                buf[slot as usize * 2..slot as usize * 2 + 2]
                    .copy_from_slice(&count.to_be_bytes());
            }
            self.w.write_all(&buf)?;
            self.current_offset += cluster_size;
        }
        {
            let mut buf = vec![0u8; (rc_table_clusters * cluster_size) as usize];
            for block in 0..rc_blocks {
                let offset = refblocks_offset + block * cluster_size;
                buf[block as usize * 8..block as usize * 8 + 8]
                    .copy_from_slice(&offset.to_be_bytes());
            }
            self.w.write_all(&buf)?;
            self.current_offset += buf.len() as u64;
        }

        // --- Final header (+ extensions + backing file name) at offset 0. ---
        let header_cluster = self.build_header_cluster(
            l1_entries as u32,
            l1_table_offset,
            refcount_table_offset,
            rc_table_clusters as u32,
        )?;
        self.w.seek(SeekFrom::Start(0))?;
        self.w.write_all(&header_cluster)?;
        self.w.flush()?;
        Ok(self.w)
    }

    fn build_header_cluster(
        &self,
        l1_size: u32,
        l1_table_offset: u64,
        refcount_table_offset: u64,
        refcount_table_clusters: u32,
    ) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; self.cluster_size as usize];

        // Header extensions start right after the 112-byte header.
        let mut ext_off = HEADER_LENGTH_V3 as usize;
        if self.backing_file.is_some() {
            // Backing format name extension: our chains are always qcow2.
            let fmt = b"qcow2";
            buf[ext_off..ext_off + 4].copy_from_slice(&HEXT_BACKING_FORMAT.to_be_bytes());
            buf[ext_off + 4..ext_off + 8].copy_from_slice(&(fmt.len() as u32).to_be_bytes());
            buf[ext_off + 8..ext_off + 8 + fmt.len()].copy_from_slice(fmt);
            ext_off += 8 + fmt.len().div_ceil(8) * 8;
        }
        // End-of-extensions marker (type 0, length 0).
        buf[ext_off..ext_off + 4].copy_from_slice(&HEXT_END.to_be_bytes());
        ext_off += 8;

        let (backing_file_offset, backing_file_size) = match &self.backing_file {
            Some(name) => {
                let name_off = ext_off;
                if name_off + name.len() >= buf.len() {
                    return Err(bad("backing file name does not fit the header cluster"));
                }
                buf[name_off..name_off + name.len()].copy_from_slice(name.as_bytes());
                (name_off as u64, name.len() as u32)
            }
            None => (0, 0),
        };

        let (incompatible_features, compression_type) = match self.compression {
            Qcow2Compression::None => (0u64, COMPRESSION_TYPE_DEFLATE),
            Qcow2Compression::Zstd { .. } => (INCOMPAT_COMPRESSION_TYPE, COMPRESSION_TYPE_ZSTD),
        };

        let h = &mut buf[..HEADER_LENGTH_V3 as usize];
        h[0..4].copy_from_slice(&QCOW2_MAGIC.to_be_bytes());
        h[4..8].copy_from_slice(&3u32.to_be_bytes()); // version
        h[8..16].copy_from_slice(&backing_file_offset.to_be_bytes());
        h[16..20].copy_from_slice(&backing_file_size.to_be_bytes());
        h[20..24].copy_from_slice(&self.cluster_bits.to_be_bytes());
        h[24..32].copy_from_slice(&self.virtual_size.to_be_bytes());
        h[32..36].copy_from_slice(&0u32.to_be_bytes()); // crypt_method: none
        h[36..40].copy_from_slice(&l1_size.to_be_bytes());
        h[40..48].copy_from_slice(&l1_table_offset.to_be_bytes());
        h[48..56].copy_from_slice(&refcount_table_offset.to_be_bytes());
        h[56..60].copy_from_slice(&refcount_table_clusters.to_be_bytes());
        h[60..64].copy_from_slice(&0u32.to_be_bytes()); // nb_snapshots
        h[64..72].copy_from_slice(&0u64.to_be_bytes()); // snapshots_offset
        h[72..80].copy_from_slice(&incompatible_features.to_be_bytes());
        h[80..88].copy_from_slice(&0u64.to_be_bytes()); // compatible_features
        h[88..96].copy_from_slice(&0u64.to_be_bytes()); // autoclear_features
        h[96..100].copy_from_slice(&REFCOUNT_ORDER.to_be_bytes());
        h[100..104].copy_from_slice(&HEADER_LENGTH_V3.to_be_bytes());
        h[104] = compression_type; // + 7 padding bytes already zero
        Ok(buf)
    }
}
