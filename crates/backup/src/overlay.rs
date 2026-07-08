//! A copy-on-write scratch overlay over a read-only [`BlockSource`], the qcow2
//! analogue of the VHDX differencing child used by NTFS/ReFS true removal.
//!
//! During the RW mount the base qcow2 is served read-only; every write the OS
//! filesystem driver makes (deleting the excluded files) lands in a sparse
//! scratch file here, tracked per qcow2 cluster. The base image is never
//! modified. After the deletions, [`WritableOverlaySource::touched_clusters`]
//! is the exact set the [`crate::qcow2::Qcow2InPlaceMerge`] folds back in, and
//! [`WritableOverlaySource::read_cluster`] yields each cluster's final content.
//!
//! Scratch size ≈ the deletion churn (directory metadata / $MFT / $Bitmap), not
//! the whole image — the same order as a VHDX differencing child, and deleted
//! when the pass ends.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

use own_backup_common::Result;

use crate::block_source::{BlockSource, WritableBlockSource};

pub struct WritableOverlaySource {
    base: Box<dyn BlockSource>,
    /// Sparse scratch file; touched clusters are stored here at their real
    /// volume offset, initialised from the base on first touch.
    overlay: File,
    virtual_size: u64,
    cluster_size: u64,
    total_clusters: u64,
    touched: Vec<bool>,
    touched_count: u64,
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
            touched: vec![false; total_clusters as usize],
            touched_count: 0,
        }
    }

    pub fn touched_count(&self) -> u64 {
        self.touched_count
    }

    /// Cluster indices that received at least one write, ascending.
    pub fn touched_clusters(&self) -> Vec<u64> {
        (0..self.total_clusters).filter(|&c| self.touched[c as usize]).collect()
    }

    /// Final content of one cluster (overlay if touched, else base), padded to a
    /// whole `cluster_size` for the merge writer.
    pub fn read_cluster(&mut self, cluster: u64) -> Result<Vec<u8>> {
        let start = cluster * self.cluster_size;
        let len = self.cluster_size.min(self.virtual_size - start) as usize;
        let mut buf = vec![0u8; len];
        if self.touched[cluster as usize] {
            self.overlay.seek(SeekFrom::Start(start))?;
            self.overlay.read_exact(&mut buf)?;
        } else {
            self.base.read_at(start, &mut buf)?;
        }
        buf.resize(self.cluster_size as usize, 0);
        Ok(buf)
    }

    /// On first write to a cluster, copy the base content into the overlay so a
    /// later partial read still sees the untouched bytes.
    fn ensure_cluster(&mut self, cluster: u64) -> Result<()> {
        if self.touched[cluster as usize] {
            return Ok(());
        }
        let start = cluster * self.cluster_size;
        let len = self.cluster_size.min(self.virtual_size - start) as usize;
        let mut buf = vec![0u8; len];
        self.base.read_at(start, &mut buf)?;
        self.overlay.seek(SeekFrom::Start(start))?;
        self.overlay.write_all(&buf)?;
        self.touched[cluster as usize] = true;
        self.touched_count += 1;
        Ok(())
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
            if !self.touched[c as usize] {
                continue;
            }
            let cstart = c * self.cluster_size;
            let from = cstart.max(offset);
            let to = (cstart + self.cluster_size).min(end);
            let mut tmp = vec![0u8; (to - from) as usize];
            self.overlay.seek(SeekFrom::Start(from))?;
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
        let (first, last) = self.cluster_range(offset, data.len() as u64);
        for c in first..=last {
            self.ensure_cluster(c)?;
        }
        self.overlay.seek(SeekFrom::Start(offset))?;
        self.overlay.write_all(data)?;
        Ok(())
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
}
