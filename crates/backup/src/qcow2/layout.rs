//! Physical qcow2 layout: byte-packed payloads, cluster padding and the
//! L2/L1/refcount tables that follow the data.
//!
//! Extracted so the plaintext [`super::writer::Qcow2Writer`] and the encrypted
//! `.obk` writer (`seal::obk`) share one implementation. The refcount fixpoint
//! and the header-cluster layout must stay identical between them: `.obk` is a
//! qcow2 extension, not a second container, and a drifting copy of this code
//! would only show up as a reader refusing to open an image.

use std::collections::BTreeMap;
use std::io::Write;

use own_backup_common::Result;

use super::{
    bad, HEADER_LENGTH_V3, HEXT_BACKING_FORMAT, HEXT_END, OFLAG_COPIED, QCOW2_MAGIC,
    REFCOUNT_ORDER,
};

/// Where [`Qcow2Tables::finish_tables`] put the metadata the header points at.
pub(crate) struct TablesInfo {
    pub l1_size: u32,
    pub l1_table_offset: u64,
    pub refcount_table_offset: u64,
    pub refcount_table_clusters: u32,
}

/// Write-once bookkeeping for one image file: which guest cluster maps to which
/// L2 entry, how many packed payloads share a host cluster, and where the next
/// byte goes.
pub(crate) struct Qcow2Tables {
    cluster_bits: u32,
    cluster_size: u64,
    total_guest_clusters: u64,
    /// guest cluster index -> final L2 entry.
    l2_entries: BTreeMap<u64, u64>,
    /// host cluster index -> number of packed payloads touching it.
    compressed_refs: BTreeMap<u64, u64>,
    /// Next free byte in the file.
    current_offset: u64,
}

impl Qcow2Tables {
    /// The caller has already written the placeholder header cluster, so the
    /// first free byte is the end of cluster 0.
    pub(crate) fn new(cluster_bits: u32, total_guest_clusters: u64) -> Self {
        let cluster_size = 1u64 << cluster_bits;
        Self {
            cluster_bits,
            cluster_size,
            total_guest_clusters,
            l2_entries: BTreeMap::new(),
            compressed_refs: BTreeMap::new(),
            current_offset: cluster_size,
        }
    }

    /// Continue writing into a file that already holds data records, for a
    /// pass that appends to a finished image (`seal::obk::rebuild`).
    pub(crate) fn resume(cluster_bits: u32, total_guest_clusters: u64, at: u64) -> Self {
        let mut tables = Self::new(cluster_bits, total_guest_clusters);
        tables.current_offset = at;
        tables
    }

    /// Account for a payload that is already in the file: its host clusters
    /// carry a reference even though this pass did not write them.
    ///
    /// The length may be the cluster table's rounded one rather than the
    /// record's exact length. A refcount one too high on a boundary cluster
    /// costs nothing — the structures only have to be consistent, and for
    /// `.obk` no foreign tool reads them at all.
    pub(crate) fn note_packed(&mut self, host_offset: u64, len: u64) {
        if len == 0 {
            return;
        }
        for host_cluster in
            (host_offset >> self.cluster_bits)..=((host_offset + len - 1) >> self.cluster_bits)
        {
            *self.compressed_refs.entry(host_cluster).or_insert(0) += 1;
        }
    }

    pub(crate) fn cluster_size(&self) -> u64 {
        self.cluster_size
    }

    pub(crate) fn total_guest_clusters(&self) -> u64 {
        self.total_guest_clusters
    }

    pub(crate) fn has_entry(&self, guest: u64) -> bool {
        self.l2_entries.contains_key(&guest)
    }

    /// Validate a guest cluster write before a single byte reaches the file:
    /// index in range, exactly one cluster of data, not already written.
    pub(crate) fn check_new_cluster(&self, guest: u64, data_len: usize) -> Result<()> {
        if guest >= self.total_guest_clusters {
            return Err(bad(format!(
                "cluster index {guest} beyond virtual disk ({} clusters)",
                self.total_guest_clusters
            )));
        }
        if data_len as u64 != self.cluster_size {
            return Err(bad(format!(
                "cluster data is {data_len} bytes, expected {}",
                self.cluster_size
            )));
        }
        if self.has_entry(guest) {
            return Err(bad(format!(
                "cluster {guest} written twice (write-once writer)"
            )));
        }
        Ok(())
    }

    /// Refuse a second write of the same guest cluster: both writers are
    /// write-once, and the second copy would leave the first one unreachable
    /// inside the file with a refcount still claiming it.
    pub(crate) fn set_entry(&mut self, guest: u64, entry: u64) -> Result<()> {
        if guest >= self.total_guest_clusters {
            return Err(bad(format!(
                "cluster index {guest} beyond virtual disk ({} clusters)",
                self.total_guest_clusters
            )));
        }
        if self.l2_entries.insert(guest, entry).is_some() {
            return Err(bad(format!(
                "cluster {guest} written twice (write-once writer)"
            )));
        }
        Ok(())
    }

    /// Append bytes at the current offset without alignment (compressed
    /// clusters and `.obk` records are packed byte-to-byte). Returns the host
    /// offset the payload starts at.
    pub(crate) fn write_packed<W: Write>(&mut self, w: &mut W, bytes: &[u8]) -> Result<u64> {
        if bytes.is_empty() {
            return Err(bad("refusing to pack an empty payload"));
        }
        let host_offset = self.current_offset;
        let len = bytes.len() as u64;
        w.write_all(bytes)?;
        self.current_offset += len;
        for host_cluster in
            (host_offset >> self.cluster_bits)..=((host_offset + len - 1) >> self.cluster_bits)
        {
            *self.compressed_refs.entry(host_cluster).or_insert(0) += 1;
        }
        Ok(host_offset)
    }

    /// Append one cluster-aligned cluster of data. Returns its host offset.
    pub(crate) fn write_aligned<W: Write>(&mut self, w: &mut W, data: &[u8]) -> Result<u64> {
        if data.len() as u64 != self.cluster_size {
            return Err(bad(format!(
                "cluster data is {} bytes, expected {}",
                data.len(),
                self.cluster_size
            )));
        }
        self.pad_to_cluster_boundary(w)?;
        let host_offset = self.current_offset;
        w.write_all(data)?;
        self.current_offset += self.cluster_size;
        Ok(host_offset)
    }

    pub(crate) fn pad_to_cluster_boundary<W: Write>(&mut self, w: &mut W) -> Result<()> {
        let rem = self.current_offset % self.cluster_size;
        if rem != 0 {
            let pad = (self.cluster_size - rem) as usize;
            w.write_all(&vec![0u8; pad])?;
            self.current_offset += pad as u64;
        }
        Ok(())
    }

    /// Write the L2 tables, the L1 table and the refcount structures, in that
    /// order, after everything the caller has already written.
    pub(crate) fn finish_tables<W: Write>(&mut self, w: &mut W) -> Result<TablesInfo> {
        let cluster_size = self.cluster_size;
        let l2_per_table = cluster_size / 8;
        let l1_entries = self.total_guest_clusters.div_ceil(l2_per_table);

        // --- L2 tables (one cluster each), grouped by L1 index. ---
        self.pad_to_cluster_boundary(w)?;
        let mut l1_table = vec![0u64; l1_entries as usize];
        {
            let mut groups: BTreeMap<u64, Vec<(u64, u64)>> = BTreeMap::new();
            for (&guest, &entry) in &self.l2_entries {
                groups
                    .entry(guest / l2_per_table)
                    .or_default()
                    .push((guest % l2_per_table, entry));
            }
            for (l1_index, entries) in groups {
                let table_offset = self.current_offset;
                let mut table = vec![0u8; cluster_size as usize];
                for (slot, entry) in entries {
                    table[slot as usize * 8..slot as usize * 8 + 8]
                        .copy_from_slice(&entry.to_be_bytes());
                }
                w.write_all(&table)?;
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
            w.write_all(&buf)?;
            self.current_offset += buf.len() as u64;
            self.pad_to_cluster_boundary(w)?;
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
        // clusters holding packed payloads, whose refcount equals the number of
        // payloads overlapping them.
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
                buf[slot as usize * 2..slot as usize * 2 + 2].copy_from_slice(&count.to_be_bytes());
            }
            w.write_all(&buf)?;
            self.current_offset += cluster_size;
        }
        {
            let mut buf = vec![0u8; (rc_table_clusters * cluster_size) as usize];
            for block in 0..rc_blocks {
                let offset = refblocks_offset + block * cluster_size;
                buf[block as usize * 8..block as usize * 8 + 8]
                    .copy_from_slice(&offset.to_be_bytes());
            }
            w.write_all(&buf)?;
            self.current_offset += buf.len() as u64;
        }

        Ok(TablesInfo {
            l1_size: l1_entries as u32,
            l1_table_offset,
            refcount_table_offset,
            refcount_table_clusters: rc_table_clusters as u32,
        })
    }
}

/// Everything the final header cluster needs beyond [`TablesInfo`].
pub(crate) struct HeaderParams<'h> {
    pub cluster_bits: u32,
    pub virtual_size: u64,
    pub incompatible_features: u64,
    pub compression_type: u8,
    pub backing_file: Option<&'h str>,
    /// Extra header extensions (type, data), written before the end marker.
    pub extensions: &'h [(u32, Vec<u8>)],
}

/// Build the whole header cluster: version-3 header, header extensions and the
/// backing file name. It is written at offset 0 last, so a crash before that
/// leaves a file which is not a qcow2 image at all rather than a half-valid one.
pub(crate) fn build_header_cluster(p: HeaderParams<'_>, tables: &TablesInfo) -> Result<Vec<u8>> {
    let cluster_size = 1usize << p.cluster_bits;
    let mut buf = vec![0u8; cluster_size];

    // Header extensions start right after the 112-byte header.
    let mut ext_off = HEADER_LENGTH_V3 as usize;
    let mut extensions: Vec<(u32, &[u8])> = Vec::new();
    if p.backing_file.is_some() {
        // Backing format name extension: our chains never mix formats.
        extensions.push((HEXT_BACKING_FORMAT, b"qcow2"));
    }
    extensions.extend(p.extensions.iter().map(|(kind, data)| (*kind, &data[..])));
    for (kind, data) in extensions {
        let padded = data.len().div_ceil(8) * 8;
        // The end marker and, for a child, the backing file name still follow.
        if ext_off + 8 + padded + 8 >= buf.len() {
            return Err(bad("header extensions do not fit the header cluster"));
        }
        buf[ext_off..ext_off + 4].copy_from_slice(&kind.to_be_bytes());
        buf[ext_off + 4..ext_off + 8].copy_from_slice(&(data.len() as u32).to_be_bytes());
        buf[ext_off + 8..ext_off + 8 + data.len()].copy_from_slice(data);
        ext_off += 8 + padded;
    }
    // End-of-extensions marker (type 0, length 0).
    buf[ext_off..ext_off + 4].copy_from_slice(&HEXT_END.to_be_bytes());
    ext_off += 8;

    let (backing_file_offset, backing_file_size) = match p.backing_file {
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

    let h = &mut buf[..HEADER_LENGTH_V3 as usize];
    h[0..4].copy_from_slice(&QCOW2_MAGIC.to_be_bytes());
    h[4..8].copy_from_slice(&3u32.to_be_bytes()); // version
    h[8..16].copy_from_slice(&backing_file_offset.to_be_bytes());
    h[16..20].copy_from_slice(&backing_file_size.to_be_bytes());
    h[20..24].copy_from_slice(&p.cluster_bits.to_be_bytes());
    h[24..32].copy_from_slice(&p.virtual_size.to_be_bytes());
    h[32..36].copy_from_slice(&0u32.to_be_bytes()); // crypt_method: none
    h[36..40].copy_from_slice(&tables.l1_size.to_be_bytes());
    h[40..48].copy_from_slice(&tables.l1_table_offset.to_be_bytes());
    h[48..56].copy_from_slice(&tables.refcount_table_offset.to_be_bytes());
    h[56..60].copy_from_slice(&tables.refcount_table_clusters.to_be_bytes());
    h[60..64].copy_from_slice(&0u32.to_be_bytes()); // nb_snapshots
    h[64..72].copy_from_slice(&0u64.to_be_bytes()); // snapshots_offset
    h[72..80].copy_from_slice(&p.incompatible_features.to_be_bytes());
    h[80..88].copy_from_slice(&0u64.to_be_bytes()); // compatible_features
    h[88..96].copy_from_slice(&0u64.to_be_bytes()); // autoclear_features
    h[96..100].copy_from_slice(&REFCOUNT_ORDER.to_be_bytes());
    h[100..104].copy_from_slice(&HEADER_LENGTH_V3.to_be_bytes());
    h[104] = p.compression_type; // + 7 padding bytes already zero
    Ok(buf)
}
