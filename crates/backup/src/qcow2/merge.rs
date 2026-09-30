//! In-place merge of changed guest clusters into a finished qcow2 image.
//!
//! Used by NTFS/ReFS true removal for the qcow2 container (the VHDX path uses a
//! differencing child + `MergeVirtualDisk`; qcow2 has no kernel merge, so we do
//! it ourselves). The finished image is RW-mounted, the FS driver deletes the
//! excluded files into a raw overlay, and the touched blocks are folded back in
//! here — exactly as qemu would: a compressed cluster is read-only, so writing a
//! guest cluster **appends a new host cluster and repoints its L2 entry**; the
//! old cluster is left as dead space — committed with refcount 0, so it reads as
//! free rather than leaked (`qemu-img check`: "No errors were found"). Untouched
//! data clusters keep their original host offsets, so no full
//! rewrite happens — the file grows only by the touched clusters plus fresh
//! metadata.
//!
//! Transactional like the VHDX merge: every new cluster and the fresh
//! L2/L1/refcount metadata are appended and `fsync`'d **before** the header is
//! repointed (also `fsync`'d). A crash before the header write leaves the
//! original image fully readable (a valid backup whose excluded files are still
//! zero-filled ghosts); a crash after leaves the merged image. There is never an
//! in-between state that reads wrong data.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use own_backup_common::Result;

use super::{
    bad, decode_l2_entry, encode_compressed_l2_entry, ClusterDescriptor, COMPRESSION_TYPE_ZSTD,
    HEADER_LENGTH_V3, INCOMPAT_COMPRESSION_TYPE, OFFSET_MASK, OFLAG_COPIED, QCOW2_MAGIC,
};

pub struct Qcow2InPlaceMerge {
    file: File,
    cluster_bits: u32,
    cluster_size: u64,
    virtual_size: u64,
    total_guest_clusters: u64,
    l2_per_table: u64,
    /// Compress new clusters with zstd (matches the image's own type).
    zstd_level: Option<i32>,
    /// Leaf of a backing chain (incremental/differential child).
    has_backing: bool,
    /// Original first cluster (header + extensions + backing name), the base
    /// for the repointed header commit() writes.
    header_cluster: Vec<u8>,
    /// guest cluster → current raw L2 entry (0 = unallocated, absent = 0).
    l2: BTreeMap<u64, u64>,
    /// Next append point (kept cluster-aligned by `pad`).
    append_offset: u64,
    written: bool,
}

impl Qcow2InPlaceMerge {
    /// Open a finished, standalone (no backing file) qcow2 for in-place merge.
    pub fn open(path: &Path, zstd_level: Option<i32>) -> Result<Self> {
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;
        let mut h = [0u8; HEADER_LENGTH_V3 as usize];
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut h)?;

        if u32::from_be_bytes(h[0..4].try_into().unwrap()) != QCOW2_MAGIC {
            return Err(bad("bad magic — not a qcow2 image"));
        }
        if u32::from_be_bytes(h[4..8].try_into().unwrap()) != 3 {
            return Err(bad("in-place merge requires a version-3 image"));
        }
        // A backing chain is fine: only the LEAF's L1/L2/refcounts are loaded
        // and rewritten; the parent is never touched. Remember its presence —
        // zero-cluster writes must then SHADOW the parent (explicit zero entry)
        // instead of dropping the entry (which would resurrect parent data).
        let backing_file_offset = u64::from_be_bytes(h[8..16].try_into().unwrap());
        let has_backing = backing_file_offset != 0;
        let cluster_bits = u32::from_be_bytes(h[20..24].try_into().unwrap());
        if !(9..=21).contains(&cluster_bits) {
            return Err(bad(format!("cluster_bits {cluster_bits} out of range")));
        }
        let virtual_size = u64::from_be_bytes(h[24..32].try_into().unwrap());
        if u32::from_be_bytes(h[32..36].try_into().unwrap()) != 0 {
            return Err(bad("encrypted image not supported"));
        }
        let l1_size = u32::from_be_bytes(h[36..40].try_into().unwrap());
        let l1_table_offset = u64::from_be_bytes(h[40..48].try_into().unwrap());
        if u32::from_be_bytes(h[60..64].try_into().unwrap()) != 0 {
            return Err(bad("images with internal snapshots not supported"));
        }
        let incompatible = u64::from_be_bytes(h[72..80].try_into().unwrap());
        if incompatible & !INCOMPAT_COMPRESSION_TYPE != 0 {
            return Err(bad("unknown incompatible features — refusing to merge"));
        }
        if incompatible & INCOMPAT_COMPRESSION_TYPE != 0 && h[104] != COMPRESSION_TYPE_ZSTD {
            return Err(bad("only zstd-compressed qcow2 images are supported"));
        }

        let cluster_size = 1u64 << cluster_bits;
        let l2_per_table = cluster_size / 8;
        let total_guest_clusters = virtual_size.div_ceil(cluster_size);

        // Load the L1 table, then every referenced L2 table (raw entries — the
        // untouched clusters must keep their exact descriptors and host offsets).
        if l1_size as u64 > (1 << 25) {
            return Err(bad(format!("L1 size {l1_size} out of range")));
        }
        let mut l1_raw = vec![0u8; l1_size as usize * 8];
        file.seek(SeekFrom::Start(l1_table_offset))?;
        file.read_exact(&mut l1_raw)?;

        let mut l2: BTreeMap<u64, u64> = BTreeMap::new();
        for (l1_index, chunk) in l1_raw.chunks_exact(8).enumerate() {
            let l2_offset = u64::from_be_bytes(chunk.try_into().unwrap()) & OFFSET_MASK;
            if l2_offset == 0 {
                continue;
            }
            let mut table = vec![0u8; cluster_size as usize];
            file.seek(SeekFrom::Start(l2_offset))?;
            file.read_exact(&mut table)?;
            for (slot, e) in table.chunks_exact(8).enumerate() {
                let entry = u64::from_be_bytes(e.try_into().unwrap());
                if entry != 0 {
                    let guest = l1_index as u64 * l2_per_table + slot as u64;
                    l2.insert(guest, entry);
                }
            }
        }

        // Keep the original header cluster: commit() must preserve everything
        // it does not repoint — the backing-file link + name and the header
        // extensions live here, and a child that loses them loses its chain.
        let mut header_cluster = vec![0u8; cluster_size as usize];
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut header_cluster)?;

        let end = file.seek(SeekFrom::End(0))?;
        let append_offset = end.div_ceil(cluster_size) * cluster_size;

        Ok(Self {
            file,
            cluster_bits,
            cluster_size,
            virtual_size,
            total_guest_clusters,
            l2_per_table,
            zstd_level: if incompatible & INCOMPAT_COMPRESSION_TYPE != 0 {
                zstd_level.or(Some(super::DEFAULT_ZSTD_LEVEL))
            } else {
                None
            },
            has_backing,
            header_cluster,
            l2,
            append_offset,
            written: false,
        })
    }

    pub fn cluster_size(&self) -> u64 {
        self.cluster_size
    }

    pub fn virtual_size(&self) -> u64 {
        self.virtual_size
    }

    pub fn total_guest_clusters(&self) -> u64 {
        self.total_guest_clusters
    }

    /// True when this image stores compressed clusters as **zstd** (the
    /// `compression_type` incompatible feature). Grafting between images that
    /// disagree is refused: a zstd payload copied into an image whose header
    /// says "deflate" decodes as garbage, and nothing would notice until a
    /// restore.
    pub fn is_zstd(&self) -> bool {
        self.zstd_level.is_some()
    }

    /// What this image itself says about a guest cluster (no backing-file
    /// fall-through). Consolidation asks before grafting, to count the space
    /// the old cluster is about to stop being referenced from.
    pub fn descriptor(&self, index: u64) -> Result<ClusterDescriptor> {
        match self.l2.get(&index) {
            None => Ok(ClusterDescriptor::Unallocated),
            Some(&entry) => decode_l2_entry(entry, self.cluster_bits),
        }
    }

    fn pad(&mut self) -> Result<()> {
        let rem = self.append_offset % self.cluster_size;
        if rem != 0 {
            let padding = self.cluster_size - rem;
            self.file.seek(SeekFrom::Start(self.append_offset))?;
            self.file.write_all(&vec![0u8; padding as usize])?;
            self.append_offset += padding;
        }
        Ok(())
    }

    /// Replace guest cluster `index` with `data` (cluster-sized). The new data is
    /// appended (compressed if the image is zstd and it shrinks; else raw) and
    /// the L2 entry is repointed; the previous cluster becomes dead space.
    pub fn write_cluster(&mut self, index: u64, data: &[u8]) -> Result<()> {
        if index >= self.total_guest_clusters {
            return Err(bad(format!("cluster {index} beyond virtual disk")));
        }
        if data.len() as u64 != self.cluster_size {
            return Err(bad(format!(
                "cluster data is {} bytes, expected {}",
                data.len(),
                self.cluster_size
            )));
        }
        self.written = true;

        if data.iter().all(|&b| b == 0) {
            if self.has_backing {
                // A child must SHADOW the parent's version of the cluster: an
                // explicit zero entry reads as zeros without allocating space.
                // Dropping the entry would fall through to the parent.
                self.l2.insert(index, super::OFLAG_ZERO);
            } else {
                // No backing file → unallocated reads back as zeros.
                self.l2.remove(&index);
            }
            return Ok(());
        }

        if let Some(level) = self.zstd_level {
            let compressed = zstd::bulk::compress(data, level)
                .map_err(|e| own_backup_common::OwnBackupError::Compression(e.to_string()))?;
            if (compressed.len() as u64) < self.cluster_size {
                // New compressed clusters are written cluster-aligned (one host
                // cluster each) — no packing — so refcounts stay a simple count.
                self.pad()?;
                let host_offset = self.append_offset;
                let entry = encode_compressed_l2_entry(
                    host_offset,
                    compressed.len() as u64,
                    self.cluster_bits,
                )?;
                self.file.seek(SeekFrom::Start(host_offset))?;
                self.file.write_all(&compressed)?;
                self.append_offset += compressed.len() as u64;
                self.l2.insert(index, entry);
                return Ok(());
            }
        }
        // Raw cluster.
        self.pad()?;
        let host_offset = self.append_offset;
        self.file.seek(SeekFrom::Start(host_offset))?;
        self.file.write_all(data)?;
        self.append_offset += self.cluster_size;
        self.l2.insert(index, OFLAG_COPIED | host_offset);
        Ok(())
    }

    // --- Grafting: copy clusters in from another image of the same chain ---
    //
    // Consolidation folds a whole increment into its base, and unlike true
    // removal it must not re-encode: the child's clusters are already zstd
    // streams at the same level, so they are copied byte-for-byte. Compressed
    // grafts are also **packed** byte-to-byte, exactly as the write-once
    // writer packs them. `write_cluster` gives every compressed cluster a host
    // cluster of its own, which is fine for the handful true removal touches
    // and ruinous here: a 2 GiB increment would cost the base 64 KiB per
    // cluster — its uncompressed size — and the format's whole point with it.

    fn graft_index_check(&mut self, index: u64) -> Result<()> {
        if index >= self.total_guest_clusters {
            return Err(bad(format!("cluster {index} beyond virtual disk")));
        }
        self.written = true;
        Ok(())
    }

    /// Graft a compressed cluster verbatim. `payload` is the child's own
    /// compressed stream, as read from its host offset.
    pub fn graft_compressed(&mut self, index: u64, payload: &[u8]) -> Result<()> {
        self.graft_index_check(index)?;
        if payload.is_empty() {
            return Err(bad(format!("empty compressed payload for cluster {index}")));
        }
        let host_offset = self.append_offset;
        let entry =
            encode_compressed_l2_entry(host_offset, payload.len() as u64, self.cluster_bits)?;
        self.file.seek(SeekFrom::Start(host_offset))?;
        self.file.write_all(payload)?;
        self.append_offset += payload.len() as u64;
        self.l2.insert(index, entry);
        Ok(())
    }

    /// Graft an uncompressed (incompressible) cluster: a whole host cluster,
    /// so the append point is aligned first.
    pub fn graft_raw(&mut self, index: u64, data: &[u8]) -> Result<()> {
        self.graft_index_check(index)?;
        if data.len() as u64 != self.cluster_size {
            return Err(bad(format!(
                "raw cluster data is {} bytes, expected {}",
                data.len(),
                self.cluster_size
            )));
        }
        self.pad()?;
        let host_offset = self.append_offset;
        self.file.seek(SeekFrom::Start(host_offset))?;
        self.file.write_all(data)?;
        self.append_offset += self.cluster_size;
        self.l2.insert(index, OFLAG_COPIED | host_offset);
        Ok(())
    }

    /// Graft an explicit zero cluster — the child said "zeros here", which
    /// must override whatever the base holds. Same rule as `write_cluster`:
    /// shadow the backing file with an explicit zero entry when there is one,
    /// otherwise drop the entry (unallocated reads as zeros).
    pub fn graft_zero(&mut self, index: u64) -> Result<()> {
        self.graft_index_check(index)?;
        if self.has_backing {
            self.l2.insert(index, super::OFLAG_ZERO);
        } else {
            self.l2.remove(&index);
        }
        Ok(())
    }

    /// Write fresh L2/L1/refcount metadata at EOF, then repoint the header.
    /// No-op fast path if nothing was written.
    pub fn commit(mut self) -> Result<()> {
        if !self.written {
            return Ok(());
        }
        let cs = self.cluster_size;
        let l1_entries = self.total_guest_clusters.div_ceil(self.l2_per_table);

        // --- Fresh L2 tables (one cluster each), grouped by L1 index. ---
        self.pad()?;
        let mut l1_table = vec![0u64; l1_entries as usize];
        let mut l2_table_clusters: Vec<u64> = Vec::new();
        {
            let mut groups: BTreeMap<u64, Vec<(u64, u64)>> = BTreeMap::new();
            for (&guest, &entry) in &self.l2 {
                groups
                    .entry(guest / self.l2_per_table)
                    .or_default()
                    .push((guest % self.l2_per_table, entry));
            }
            for (l1_index, entries) in groups {
                let table_offset = self.append_offset;
                let mut table = vec![0u8; cs as usize];
                for (slot, entry) in entries {
                    table[slot as usize * 8..slot as usize * 8 + 8]
                        .copy_from_slice(&entry.to_be_bytes());
                }
                self.file.seek(SeekFrom::Start(table_offset))?;
                self.file.write_all(&table)?;
                self.append_offset += cs;
                l1_table[l1_index as usize] = OFLAG_COPIED | table_offset;
                l2_table_clusters.push(table_offset >> self.cluster_bits);
            }
        }

        // --- Fresh L1 table, padded to whole clusters. ---
        let l1_table_offset = self.append_offset;
        {
            let mut buf: Vec<u8> = Vec::with_capacity(l1_entries as usize * 8);
            for e in &l1_table {
                buf.extend_from_slice(&e.to_be_bytes());
            }
            self.file.seek(SeekFrom::Start(l1_table_offset))?;
            self.file.write_all(&buf)?;
            self.append_offset += buf.len() as u64;
            self.pad()?;
        }
        let l1_cluster_lo = l1_table_offset >> self.cluster_bits;
        let l1_cluster_hi = (self.append_offset - 1) >> self.cluster_bits;

        // --- Sparse refcount over every LIVE host cluster. ---
        // Old (overwritten) data clusters and the old metadata regions are left
        // unreferenced (count 0 = leaked), which is what makes this 1x-space.
        let mut live: BTreeMap<u64, u64> = BTreeMap::new();
        let bump = |cluster: u64, live: &mut BTreeMap<u64, u64>| {
            *live.entry(cluster).or_insert(0) += 1;
        };
        bump(0, &mut live); // header cluster
        for &entry in self.l2.values() {
            match decode_l2_entry(entry, self.cluster_bits)? {
                ClusterDescriptor::Standard { host_offset } => {
                    bump(host_offset >> self.cluster_bits, &mut live)
                }
                ClusterDescriptor::Compressed {
                    host_offset,
                    length,
                } => {
                    let first = host_offset >> self.cluster_bits;
                    let last = (host_offset + length - 1) >> self.cluster_bits;
                    for c in first..=last {
                        bump(c, &mut live);
                    }
                }
                ClusterDescriptor::Zero | ClusterDescriptor::Unallocated => {}
            }
        }
        for c in l2_table_clusters {
            bump(c, &mut live);
        }
        for c in l1_cluster_lo..=l1_cluster_hi {
            bump(c, &mut live);
        }

        // Fixpoint: the refcount blocks/table must refcount themselves too.
        let refs_per_block = cs / 2; // 16-bit refcounts
        let data_end_clusters = self.append_offset / cs;
        let (mut rc_blocks, mut rc_table_clusters) = (0u64, 0u64);
        loop {
            let total = data_end_clusters + rc_blocks + rc_table_clusters;
            let need_blocks = total.div_ceil(refs_per_block);
            let need_table = (need_blocks * 8).div_ceil(cs);
            if need_blocks == rc_blocks && need_table == rc_table_clusters {
                break;
            }
            rc_blocks = need_blocks;
            rc_table_clusters = need_table;
        }
        let total_clusters = data_end_clusters + rc_blocks + rc_table_clusters;
        let refblocks_offset = self.append_offset;
        let refcount_table_offset = refblocks_offset + rc_blocks * cs;
        for c in (refblocks_offset >> self.cluster_bits)..(total_clusters) {
            bump(c, &mut live);
        }

        // Emit refcount blocks (holes read as 0).
        self.file.seek(SeekFrom::Start(refblocks_offset))?;
        for block in 0..rc_blocks {
            let mut buf = vec![0u8; cs as usize];
            for slot in 0..refs_per_block {
                let cluster = block * refs_per_block + slot;
                if cluster >= total_clusters {
                    break;
                }
                let count = live.get(&cluster).copied().unwrap_or(0);
                let count = u16::try_from(count)
                    .map_err(|_| bad(format!("refcount overflow on cluster {cluster}")))?;
                buf[slot as usize * 2..slot as usize * 2 + 2].copy_from_slice(&count.to_be_bytes());
            }
            self.file.write_all(&buf)?;
        }
        // Emit refcount table.
        let mut tbl = vec![0u8; (rc_table_clusters * cs) as usize];
        for block in 0..rc_blocks {
            let offset = refblocks_offset + block * cs;
            tbl[block as usize * 8..block as usize * 8 + 8].copy_from_slice(&offset.to_be_bytes());
        }
        self.file.write_all(&tbl)?;

        // Durability barrier: all new data + metadata on disk BEFORE the header
        // points at it. Crash here ⇒ original image still reads correctly.
        self.file.flush()?;
        self.file.sync_all()?;

        // --- Repoint the header (written last, then fsync'd). ---
        // Start from the ORIGINAL first cluster and patch only the L1 and
        // refcount pointers: the backing-file link + name and the header
        // extensions must survive verbatim (a child stripped of them loses its
        // whole chain). Magic/version/sizes/compression were validated at open.
        let mut header = self.header_cluster.clone();
        let h = &mut header[..HEADER_LENGTH_V3 as usize];
        h[36..40].copy_from_slice(&(l1_entries as u32).to_be_bytes());
        h[40..48].copy_from_slice(&l1_table_offset.to_be_bytes());
        h[48..56].copy_from_slice(&refcount_table_offset.to_be_bytes());
        h[56..60].copy_from_slice(&(rc_table_clusters as u32).to_be_bytes());

        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&header)?;
        self.file.flush()?;
        self.file.sync_all()?;
        Ok(())
    }
}

/// Write a blob to a qcow2 guest offset in-place, appending new clusters as
/// needed and committing them.
///
/// `guest_offset` is only sector-aligned (the meta partition starts right
/// after the volume), NOT cluster-aligned — so the touched cluster range is
/// read first and the blob is overlaid at its true in-cluster position.
/// Writing chunks at cluster starts instead would both misplace the data and
/// zero the untouched remainder of the first/last clusters.
pub fn write_qcow2_guest_blob(path: &Path, guest_offset: u64, blob: &[u8]) -> Result<()> {
    if blob.is_empty() {
        return Ok(());
    }
    let (cs, region) = {
        let mut reader = super::Qcow2RandomReader::open(path)?;
        let cs = reader.cluster_size();
        let vsize = reader.virtual_size();
        let end = guest_offset
            .checked_add(blob.len() as u64)
            .filter(|&e| e <= vsize)
            .ok_or_else(|| {
                own_backup_common::OwnBackupError::ImageFormat(format!(
                    "guest blob [{guest_offset}, +{}) exceeds virtual disk ({vsize} bytes)",
                    blob.len()
                ))
            })?;
        let first = guest_offset / cs;
        let last = (end - 1) / cs;
        let start = first * cs;
        let mut region = vec![0u8; ((last - first + 1) * cs) as usize];
        // The tail cluster may reach past the virtual size; read what exists.
        let readable = region.len().min((vsize - start) as usize);
        reader.read_at(start, &mut region[..readable])?;
        (cs, region)
    };

    let first = guest_offset / cs;
    let shift = (guest_offset - first * cs) as usize;
    let mut region = region;
    region[shift..shift + blob.len()].copy_from_slice(blob);

    let mut merge = Qcow2InPlaceMerge::open(path, Some(super::DEFAULT_ZSTD_LEVEL))?;
    for (i, chunk) in region.chunks_exact(cs as usize).enumerate() {
        merge.write_cluster(first + i as u64, chunk)?;
    }
    merge.commit()?;
    Ok(())
}

#[cfg(test)]
mod guest_blob_tests {
    use super::write_qcow2_guest_blob;
    use crate::qcow2::{Qcow2RandomReader, Qcow2Writer, Qcow2WriterOptions};

    /// The meta partition is only sector-aligned, so the blob write must
    /// read-modify-write the touched clusters: data lands at the true
    /// in-cluster position and the neighbouring bytes survive.
    #[test]
    fn unaligned_blob_write_preserves_neighbours() {
        let cs = 65536u64;
        let vsize = 8 * cs;
        let mut w = Qcow2Writer::new(
            std::io::Cursor::new(Vec::new()),
            Qcow2WriterOptions {
                virtual_size: vsize,
                ..Default::default()
            },
        )
        .unwrap();
        for c in 0..8 {
            w.write_cluster(c, &vec![(c as u8) + 1; cs as usize])
                .unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("img.qcow2");
        std::fs::write(&path, w.finish().unwrap().into_inner()).unwrap();

        // Blob straddles the cluster 2 → 3 boundary at a 512-aligned offset.
        let offset = 3 * cs - 512;
        let blob: Vec<u8> = (0..4096u32).map(|i| (i % 249) as u8).collect();
        write_qcow2_guest_blob(&path, offset, &blob).unwrap();

        let mut r = Qcow2RandomReader::open(&path).unwrap();
        let mut back = vec![0u8; blob.len()];
        r.read_at(offset, &mut back).unwrap();
        assert_eq!(back, blob, "blob must read back from its true offset");

        let mut edge = vec![0u8; 16];
        r.read_at(offset - 16, &mut edge).unwrap();
        assert_eq!(edge, [3u8; 16], "bytes before the blob must survive");
        r.read_at(offset + blob.len() as u64, &mut edge).unwrap();
        assert_eq!(edge, [4u8; 16], "bytes after the blob must survive");
    }

    /// Writing a blob into a CHILD image must keep its backing link (the
    /// header commit preserves the original first cluster) and must SHADOW the
    /// parent where the blob is zero (explicit zero entries, not dropped ones).
    #[test]
    fn child_blob_write_keeps_chain_and_shadows_zeros() {
        let cs = 65536u64;
        let vsize = 8 * cs;
        let dir = tempfile::tempdir().unwrap();

        let mut pw = Qcow2Writer::new(
            std::io::Cursor::new(Vec::new()),
            Qcow2WriterOptions {
                virtual_size: vsize,
                ..Default::default()
            },
        )
        .unwrap();
        for c in 0..8 {
            pw.write_cluster(c, &vec![(c as u8) + 1; cs as usize])
                .unwrap();
        }
        std::fs::write(
            dir.path().join("parent.qcow2"),
            pw.finish().unwrap().into_inner(),
        )
        .unwrap();

        let cw = Qcow2Writer::new(
            std::io::Cursor::new(Vec::new()),
            Qcow2WriterOptions {
                virtual_size: vsize,
                backing_file: Some("parent.qcow2".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let child = dir.path().join("child.qcow2");
        std::fs::write(&child, cw.finish().unwrap().into_inner()).unwrap();

        // Blob spans clusters 2..=4 (unaligned start); everything between the
        // first and last byte is zero, so cluster 3 becomes an all-zero write.
        let offset = 2 * cs + 256;
        let mut blob = vec![0u8; (2 * cs) as usize];
        blob[0] = 0xAB;
        let blob_len = blob.len();
        blob[blob_len - 1] = 0xCD;
        write_qcow2_guest_blob(&child, offset, &blob).unwrap();

        let mut r = Qcow2RandomReader::open(&child).unwrap();
        assert_eq!(r.chain_len(), 2, "backing link must survive the commit");

        let mut back = vec![0xFFu8; blob.len()];
        r.read_at(offset, &mut back).unwrap();
        assert_eq!(
            back, blob,
            "blob (incl. its zero middle) must shadow the parent"
        );

        let mut edge = vec![0u8; 16];
        r.read_at(offset - 16, &mut edge).unwrap();
        assert_eq!(
            edge, [3u8; 16],
            "pre-blob bytes come from the parent via RMW"
        );
        r.read_at(offset + blob.len() as u64, &mut edge).unwrap();
        assert_eq!(
            edge, [5u8; 16],
            "post-blob bytes come from the parent via RMW"
        );

        // Untouched clusters still fall through to the parent.
        r.read_at(0, &mut edge).unwrap();
        assert_eq!(edge, [1u8; 16]);
    }
}
