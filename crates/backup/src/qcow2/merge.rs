//! In-place merge of changed guest clusters into a finished qcow2 image.
//!
//! Used by NTFS/ReFS true removal for the qcow2 container (the VHDX path uses a
//! differencing child + `MergeVirtualDisk`; qcow2 has no kernel merge, so we do
//! it ourselves). The finished image is RW-mounted, the FS driver deletes the
//! excluded files into a raw overlay, and the touched blocks are folded back in
//! here — exactly as qemu would: a compressed cluster is read-only, so writing a
//! guest cluster **appends a new host cluster and repoints its L2 entry**; the
//! old cluster is left as dead space (a benign `qemu-img check` "leaked cluster"
//! warning). Untouched data clusters keep their original host offsets, so no full
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
    HEADER_LENGTH_V3, HEXT_END, INCOMPAT_COMPRESSION_TYPE, OFLAG_COPIED, OFFSET_MASK,
    QCOW2_MAGIC, REFCOUNT_ORDER,
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
        let backing_file_offset = u64::from_be_bytes(h[8..16].try_into().unwrap());
        if backing_file_offset != 0 {
            return Err(bad("in-place merge of a backing-chain qcow2 is not supported"));
        }
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

        let end = file.seek(SeekFrom::End(0))?;
        let append_offset = end.div_ceil(cluster_size) * cluster_size;

        Ok(Self {
            file,
            cluster_bits,
            cluster_size,
            virtual_size,
            total_guest_clusters,
            l2_per_table,
            zstd_level: if incompatible & INCOMPAT_COMPRESSION_TYPE != 0 { zstd_level.or(Some(super::DEFAULT_ZSTD_LEVEL)) } else { None },
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
            return Err(bad(format!("cluster data is {} bytes, expected {}", data.len(), self.cluster_size)));
        }
        self.written = true;

        if data.iter().all(|&b| b == 0) {
            // No backing file → unallocated reads back as zeros; drop the entry.
            self.l2.remove(&index);
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
                let entry = encode_compressed_l2_entry(host_offset, compressed.len() as u64, self.cluster_bits)?;
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
                groups.entry(guest / self.l2_per_table).or_default().push((guest % self.l2_per_table, entry));
            }
            for (l1_index, entries) in groups {
                let table_offset = self.append_offset;
                let mut table = vec![0u8; cs as usize];
                for (slot, entry) in entries {
                    table[slot as usize * 8..slot as usize * 8 + 8].copy_from_slice(&entry.to_be_bytes());
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
                ClusterDescriptor::Standard { host_offset } => bump(host_offset >> self.cluster_bits, &mut live),
                ClusterDescriptor::Compressed { host_offset, length } => {
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
                let count = u16::try_from(count).map_err(|_| bad(format!("refcount overflow on cluster {cluster}")))?;
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
        let mut header = vec![0u8; cs as usize];
        let incompat = if self.zstd_level.is_some() { INCOMPAT_COMPRESSION_TYPE } else { 0 };
        // Header extensions: end-of-extensions marker only (no backing file).
        header[HEADER_LENGTH_V3 as usize..HEADER_LENGTH_V3 as usize + 4].copy_from_slice(&HEXT_END.to_be_bytes());
        let h = &mut header[..HEADER_LENGTH_V3 as usize];
        h[0..4].copy_from_slice(&QCOW2_MAGIC.to_be_bytes());
        h[4..8].copy_from_slice(&3u32.to_be_bytes());
        h[8..16].copy_from_slice(&0u64.to_be_bytes()); // backing_file_offset
        h[16..20].copy_from_slice(&0u32.to_be_bytes()); // backing_file_size
        h[20..24].copy_from_slice(&self.cluster_bits.to_be_bytes());
        h[24..32].copy_from_slice(&self.virtual_size.to_be_bytes());
        h[32..36].copy_from_slice(&0u32.to_be_bytes()); // crypt_method
        h[36..40].copy_from_slice(&(l1_entries as u32).to_be_bytes());
        h[40..48].copy_from_slice(&l1_table_offset.to_be_bytes());
        h[48..56].copy_from_slice(&refcount_table_offset.to_be_bytes());
        h[56..60].copy_from_slice(&(rc_table_clusters as u32).to_be_bytes());
        h[60..64].copy_from_slice(&0u32.to_be_bytes()); // nb_snapshots
        h[64..72].copy_from_slice(&0u64.to_be_bytes()); // snapshots_offset
        h[72..80].copy_from_slice(&incompat.to_be_bytes());
        h[96..100].copy_from_slice(&REFCOUNT_ORDER.to_be_bytes());
        h[100..104].copy_from_slice(&HEADER_LENGTH_V3.to_be_bytes());
        h[104] = if self.zstd_level.is_some() { COMPRESSION_TYPE_ZSTD } else { 0 };

        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&header)?;
        self.file.flush()?;
        self.file.sync_all()?;
        Ok(())
    }
}
