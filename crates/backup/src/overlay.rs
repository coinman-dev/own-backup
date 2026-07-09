//! A copy-on-write scratch overlay over a read-only [`BlockSource`], the qcow2
//! analogue of the VHDX differencing child used by NTFS/ReFS true removal.
//!
//! During the RW mount the base qcow2 is served read-only; every write the OS
//! filesystem driver makes (deleting the excluded files) lands in a scratch
//! file here, tracked per qcow2 cluster. The base image is never modified.
//! After the deletions, [`WritableOverlaySource::touched_clusters`] is the
//! exact set the [`crate::qcow2::Qcow2InPlaceMerge`] folds back in, and
//! [`WritableOverlaySource::read_cluster`] yields each cluster's final content.
//!
//! The scratch layout is COMPACT: a touched cluster is stored at the next free
//! slot (slot = order of first touch), with the cluster→slot map in memory.
//! Never at its virtual offset — a plain NTFS file is not sparse, and one write
//! near the end of a 128 GiB disk would make NTFS physically zero-fill the
//! whole gap (the 2026-07-08 incident: a ~137 GB scratch, the destination
//! volume full, and the machine unresponsive). Scratch size therefore ≈ the
//! deletion churn (directory metadata / $MFT / $Bitmap), the same order as a
//! VHDX differencing child, and the file is deleted when the pass ends.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

use own_backup_common::Result;

use crate::block_source::{BlockSource, WritableBlockSource};

/// Sentinel in the cluster→slot map: cluster not touched yet.
const NO_SLOT: u32 = u32::MAX;

pub struct WritableOverlaySource {
    base: Box<dyn BlockSource>,
    /// Compact scratch file; touched cluster `c` lives at `slots[c] * cluster_size`,
    /// initialised from the base on first touch. Every slot is a whole
    /// `cluster_size` (the virtual tail cluster is zero-padded).
    overlay: File,
    virtual_size: u64,
    cluster_size: u64,
    total_clusters: u64,
    /// Cluster index → scratch slot, `NO_SLOT` when untouched.
    slots: Vec<u32>,
    slot_count: u32,
}

impl WritableOverlaySource {
    /// `overlay` is an empty read+write scratch file; `cluster_size` is the
    /// qcow2 cluster (touched-tracking + merge granularity).
    pub fn new(base: Box<dyn BlockSource>, cluster_size: u64, overlay: File) -> Self {
        let virtual_size = base.size();
        let total_clusters = virtual_size.div_ceil(cluster_size);
        Self {
            base,
            overlay,
            virtual_size,
            cluster_size,
            total_clusters,
            slots: vec![NO_SLOT; total_clusters as usize],
            slot_count: 0,
        }
    }

    pub fn touched_count(&self) -> u64 {
        self.slot_count as u64
    }

    pub fn cluster_size(&self) -> u64 {
        self.cluster_size
    }

    /// Cluster indices that received at least one write, ascending.
    pub fn touched_clusters(&self) -> Vec<u64> {
        (0..self.total_clusters).filter(|&c| self.slots[c as usize] != NO_SLOT).collect()
    }

    /// Final content of one cluster (overlay if touched, else base), padded to a
    /// whole `cluster_size` for the merge writer.
    pub fn read_cluster(&mut self, cluster: u64) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; self.cluster_size as usize];
        let slot = self.slots[cluster as usize];
        if slot != NO_SLOT {
            self.overlay.seek(SeekFrom::Start(slot as u64 * self.cluster_size))?;
            self.overlay.read_exact(&mut buf)?;
        } else {
            let start = cluster * self.cluster_size;
            let len = self.cluster_size.min(self.virtual_size - start) as usize;
            self.base.read_at(start, &mut buf[..len])?;
        }
        Ok(buf)
    }

    /// On first write to a cluster, copy the base content into a fresh scratch
    /// slot so a later partial read still sees the untouched bytes.
    fn ensure_cluster(&mut self, cluster: u64) -> Result<u32> {
        let existing = self.slots[cluster as usize];
        if existing != NO_SLOT {
            return Ok(existing);
        }
        let start = cluster * self.cluster_size;
        let len = self.cluster_size.min(self.virtual_size - start) as usize;
        let mut buf = vec![0u8; self.cluster_size as usize];
        self.base.read_at(start, &mut buf[..len])?;
        let slot = self.slot_count;
        self.overlay.seek(SeekFrom::Start(slot as u64 * self.cluster_size))?;
        self.overlay.write_all(&buf)?;
        self.slots[cluster as usize] = slot;
        self.slot_count += 1;
        Ok(slot)
    }

    fn cluster_range(&self, offset: u64, len: u64) -> (u64, u64) {
        (offset / self.cluster_size, (offset + len - 1) / self.cluster_size)
    }
}

impl BlockSource for WritableOverlaySource {
    fn size(&self) -> u64 {
        self.virtual_size
    }

    fn read_at(&mut self, offset: u64, out: &mut [u8]) -> Result<()> {
        if out.is_empty() {
            return Ok(());
        }
        // Base first, then patch in any touched clusters the range overlaps.
        self.base.read_at(offset, out)?;
        let end = offset + out.len() as u64;
        let (first, last) = self.cluster_range(offset, out.len() as u64);
        for c in first..=last {
            let slot = self.slots[c as usize];
            if slot == NO_SLOT {
                continue;
            }
            let cstart = c * self.cluster_size;
            let from = cstart.max(offset);
            let to = (cstart + self.cluster_size).min(end);
            let mut tmp = vec![0u8; (to - from) as usize];
            self.overlay
                .seek(SeekFrom::Start(slot as u64 * self.cluster_size + (from - cstart)))?;
            self.overlay.read_exact(&mut tmp)?;
            out[(from - offset) as usize..(to - offset) as usize].copy_from_slice(&tmp);
        }
        Ok(())
    }
}

impl WritableBlockSource for WritableOverlaySource {
    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        // Per touched cluster: land the piece inside that cluster's slot.
        let end = offset + data.len() as u64;
        let (first, last) = self.cluster_range(offset, data.len() as u64);
        for c in first..=last {
            let slot = self.ensure_cluster(c)?;
            let cstart = c * self.cluster_size;
            let from = cstart.max(offset);
            let to = (cstart + self.cluster_size).min(end);
            self.overlay
                .seek(SeekFrom::Start(slot as u64 * self.cluster_size + (from - cstart)))?;
            self.overlay.write_all(&data[(from - offset) as usize..(to - offset) as usize])?;
        }
        Ok(())
    }
}

/// Share a writable source across the devio server thread and the merge step:
/// the server locks it per request; after the server stops, the merge locks it
/// to read `touched_clusters` / `read_cluster`.
impl<W: WritableBlockSource> BlockSource for std::sync::Arc<std::sync::Mutex<W>> {
    fn size(&self) -> u64 {
        self.lock().unwrap().size()
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.lock().unwrap().read_at(offset, buf)
    }
}

impl<W: WritableBlockSource> WritableBlockSource for std::sync::Arc<std::sync::Mutex<W>> {
    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        self.lock().unwrap().write_at(offset, data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MemSource(Vec<u8>);
    impl BlockSource for MemSource {
        fn size(&self) -> u64 {
            self.0.len() as u64
        }
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
            let o = offset as usize;
            buf.copy_from_slice(&self.0[o..o + buf.len()]);
            Ok(())
        }
    }

    fn scratch() -> File {
        tempfile::tempfile().unwrap()
    }

    #[test]
    fn composite_read_write_and_touched_tracking() {
        // 4 clusters of 16 bytes; base is 0xAA everywhere.
        let cs = 16u64;
        let base = MemSource(vec![0xAA; 64]);
        let mut ov = WritableOverlaySource::new(Box::new(base), cs, scratch());

        // Partial write into cluster 1 (bytes 20..24) and cluster 3 (byte 60).
        ov.write_at(20, &[1, 2, 3, 4]).unwrap();
        ov.write_at(60, &[9]).unwrap();
        assert_eq!(ov.touched_clusters(), vec![1, 3]);
        assert_eq!(ov.touched_count(), 2);

        // A read spanning clusters 0..=3: untouched bytes = base, written bytes
        // = overlay, and the rest of touched clusters keeps the base fill.
        let mut out = vec![0u8; 64];
        ov.read_at(0, &mut out).unwrap();
        assert!(out[0..16].iter().all(|&b| b == 0xAA)); // cluster 0 untouched
        assert_eq!(&out[20..24], &[1, 2, 3, 4]); // written
        assert!(out[16..20].iter().all(|&b| b == 0xAA)); // rest of cluster 1 = base
        assert!(out[24..32].iter().all(|&b| b == 0xAA));
        assert!(out[32..48].iter().all(|&b| b == 0xAA)); // cluster 2 untouched
        assert_eq!(out[60], 9);
        assert!(out[48..60].iter().all(|&b| b == 0xAA)); // rest of cluster 3 = base

        // read_cluster (merge input) returns whole final clusters.
        assert_eq!(&ov.read_cluster(1).unwrap()[4..8], &[1, 2, 3, 4]);
        assert!(ov.read_cluster(0).unwrap().iter().all(|&b| b == 0xAA));
    }

    #[test]
    fn write_spanning_two_clusters_and_reoverwrite() {
        let cs = 16u64;
        let mut ov = WritableOverlaySource::new(Box::new(MemSource(vec![0; 32])), cs, scratch());
        ov.write_at(0, &[5; 8]).unwrap(); // cluster 0, bytes 0..8
        ov.write_at(12, &[7; 8]).unwrap(); // spans cluster 0 (12..16) + cluster 1 (16..20)
        assert_eq!(ov.touched_clusters(), vec![0, 1]);
        let c0 = ov.read_cluster(0).unwrap();
        assert_eq!(&c0[0..8], &[5; 8]); // first write
        assert_eq!(&c0[8..12], &[0, 0, 0, 0]); // untouched base
        assert_eq!(&c0[12..16], &[7; 4]); // second write, first half
        let c1 = ov.read_cluster(1).unwrap();
        assert_eq!(&c1[0..4], &[7; 4]); // second write, second half
        assert_eq!(&c1[4..16], &[0u8; 12]); // rest = base
    }

    #[test]
    fn scratch_stays_compact_for_high_offset_writes() {
        // 1 MiB virtual disk, 16-byte clusters. A write at the very END of the
        // disk must cost ONE cluster of scratch, not a zero-fill of the whole
        // span up to it (the 2026-07-08 full-destination incident).
        let cs = 16u64;
        let size = 1024 * 1024usize;
        let file = scratch();
        let probe = file.try_clone().unwrap();
        let mut ov = WritableOverlaySource::new(Box::new(MemSource(vec![0xAA; size])), cs, file);

        ov.write_at(size as u64 - 4, &[1, 2, 3, 4]).unwrap(); // last cluster
        ov.write_at(0, &[9; 4]).unwrap(); // first cluster
        assert_eq!(ov.touched_count(), 2);
        assert_eq!(probe.metadata().unwrap().len(), 2 * cs);

        // Slot indirection returns the right content for both extremes.
        let last = ov.read_cluster(size as u64 / cs - 1).unwrap();
        assert_eq!(&last[12..16], &[1, 2, 3, 4]);
        assert_eq!(&ov.read_cluster(0).unwrap()[..4], &[9; 4]);
        let mut out = vec![0u8; 8];
        ov.read_at(size as u64 - 8, &mut out).unwrap();
        assert_eq!(&out, &[0xAA, 0xAA, 0xAA, 0xAA, 1, 2, 3, 4]);
    }
}
