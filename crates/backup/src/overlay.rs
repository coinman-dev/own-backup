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

/// Where the bytes of a touched cluster are kept while the pass runs.
///
/// Two stores, one mapping: a plaintext scratch for an ordinary image, and a
/// sealed one for an encrypted image, where §7.4 forbids a scratch anyone could
/// read. The slot arithmetic, the base fall-through and the merge interface do
/// not care which is in use.
pub trait SlotStore: Send {
    /// Bytes one slot occupies in the scratch file.
    fn slot_len(&self) -> u64;
    /// Write `data` into `slot`, `within` bytes from that cluster's start.
    fn write_in_slot(&mut self, slot: u32, cluster: u64, within: u64, data: &[u8]) -> Result<()>;
    /// Read `out.len()` bytes from `slot`, `within` bytes in.
    fn read_in_slot(&mut self, slot: u32, cluster: u64, within: u64, out: &mut [u8]) -> Result<()>;
}

/// The ordinary scratch: one cluster per slot, written where it lands.
pub struct PlainSlots {
    file: File,
    cluster_size: u64,
}

impl PlainSlots {
    pub fn new(file: File, cluster_size: u64) -> Self {
        Self { file, cluster_size }
    }
}

impl SlotStore for PlainSlots {
    fn slot_len(&self) -> u64 {
        self.cluster_size
    }

    fn write_in_slot(&mut self, slot: u32, _cluster: u64, within: u64, data: &[u8]) -> Result<()> {
        self.file
            .seek(SeekFrom::Start(slot as u64 * self.cluster_size + within))?;
        self.file.write_all(data)?;
        Ok(())
    }

    fn read_in_slot(
        &mut self,
        slot: u32,
        _cluster: u64,
        within: u64,
        out: &mut [u8],
    ) -> Result<()> {
        self.file
            .seek(SeekFrom::Start(slot as u64 * self.cluster_size + within))?;
        self.file.read_exact(out)?;
        Ok(())
    }
}

/// The scratch of an **encrypted** image: every slot is one AEAD record.
///
/// §7.4 is explicit that a writable service mount of an encrypted image may not
/// leave a plaintext scratch beside it. So each slot holds a sealed record of
/// its cluster instead: written under the chain key with the Scratch purpose,
/// which keeps it out of the key space the image's own data uses, and re-sealed
/// with a fresh nonce on every rewrite.
///
/// The record identity of each slot lives in memory, and that is deliberate:
/// it is this pass's authenticated index, and it is the reason a scratch file
/// carried over from another run — or edited between writes — cannot be read
/// back as if it were ours. Nothing here survives a crash, and nothing should:
/// the pass starts again from the image.
pub struct SealedSlots {
    file: File,
    cluster_size: u64,
    slot_len: u64,
    chain: crate::seal::ChainKey,
    chain_id: [u8; 16],
    writer: crate::seal::cluster::RecordWriter,
    /// slot -> the record that slot must contain.
    ids: std::collections::HashMap<u32, crate::seal::cluster::RecordId>,
}

impl SealedSlots {
    pub fn new(
        file: File,
        cluster_size: u64,
        chain: &crate::seal::ChainKey,
        chain_id: [u8; 16],
    ) -> Result<Self> {
        Ok(Self {
            file,
            cluster_size,
            // A slot is a whole record: the cluster, its 84-byte header and its
            // 16-byte tag. Fixed length keeps the compact slot arithmetic.
            slot_len: cluster_size + crate::seal::obk::RECORD_OVERHEAD as u64,
            chain: chain.clone(),
            chain_id,
            writer: crate::seal::cluster::RecordWriter::new(
                chain,
                chain_id,
                crate::seal::cluster::Purpose::Scratch,
            )?,
            ids: std::collections::HashMap::new(),
        })
    }

    fn context(&self, cluster: u64) -> crate::seal::cluster::RecordContext {
        crate::seal::cluster::RecordContext {
            chain_id: self.chain_id,
            purpose: crate::seal::cluster::Purpose::Scratch,
            index: cluster,
        }
    }

    /// The whole cluster a slot holds, authenticated.
    fn open_slot(&mut self, slot: u32, cluster: u64) -> Result<zeroize::Zeroizing<Vec<u8>>> {
        let id = *self.ids.get(&slot).ok_or_else(|| {
            own_backup_common::OwnBackupError::Encryption(
                "scratch slot was never written by this pass".into(),
            )
        })?;
        let mut bytes = vec![0u8; self.slot_len as usize];
        self.file
            .seek(SeekFrom::Start(slot as u64 * self.slot_len))?;
        self.file.read_exact(&mut bytes)?;
        let opened =
            crate::seal::cluster::open_record(&self.chain, self.context(cluster), id, &bytes)?;
        if opened.payload.len() != self.cluster_size as usize {
            return Err(own_backup_common::OwnBackupError::Encryption(
                "scratch record is not a whole cluster".into(),
            ));
        }
        Ok(opened.payload)
    }

    fn seal_slot(&mut self, slot: u32, cluster: u64, data: &[u8]) -> Result<()> {
        let record = self.writer.seal(
            cluster,
            crate::seal::cluster::Encoding::Raw,
            self.cluster_size as u32,
            data,
        )?;
        self.file
            .seek(SeekFrom::Start(slot as u64 * self.slot_len))?;
        self.file.write_all(&record.bytes)?;
        self.ids.insert(slot, record.id);
        Ok(())
    }
}

impl SlotStore for SealedSlots {
    fn slot_len(&self) -> u64 {
        self.slot_len
    }

    fn write_in_slot(&mut self, slot: u32, cluster: u64, within: u64, data: &[u8]) -> Result<()> {
        if within == 0 && data.len() as u64 == self.cluster_size {
            return self.seal_slot(slot, cluster, data);
        }
        // A record cannot be written into piecewise: read the cluster back,
        // patch it, and seal it again — under a new nonce, as §7.3 requires of
        // every rewrite.
        let mut buf = if self.ids.contains_key(&slot) {
            self.open_slot(slot, cluster)?
        } else {
            zeroize::Zeroizing::new(vec![0u8; self.cluster_size as usize])
        };
        buf[within as usize..within as usize + data.len()].copy_from_slice(data);
        self.seal_slot(slot, cluster, &buf)
    }

    fn read_in_slot(&mut self, slot: u32, cluster: u64, within: u64, out: &mut [u8]) -> Result<()> {
        let buf = self.open_slot(slot, cluster)?;
        out.copy_from_slice(&buf[within as usize..within as usize + out.len()]);
        Ok(())
    }
}

impl<S: SlotStore> WritableOverlaySource<S> {
    /// Bytes one touched cluster costs in the scratch file.
    pub fn slot_len(&self) -> u64 {
        self.overlay.slot_len()
    }
}

pub struct WritableOverlaySource<S: SlotStore = PlainSlots> {
    base: Box<dyn BlockSource>,
    /// Compact scratch; touched cluster `c` lives in slot `slots[c]`, seeded
    /// from the base on first touch. Every slot holds a whole `cluster_size`
    /// (the virtual tail cluster is zero-padded).
    overlay: S,
    virtual_size: u64,
    cluster_size: u64,
    total_clusters: u64,
    /// Cluster index → scratch slot, `NO_SLOT` when untouched.
    slots: Vec<u32>,
    slot_count: u32,
}

impl WritableOverlaySource<PlainSlots> {
    /// `overlay` is an empty read+write scratch file; `cluster_size` is the
    /// qcow2 cluster (touched-tracking + merge granularity).
    pub fn new(base: Box<dyn BlockSource>, cluster_size: u64, overlay: File) -> Self {
        Self::with_store(base, cluster_size, PlainSlots::new(overlay, cluster_size))
    }
}

impl<S: SlotStore> WritableOverlaySource<S> {
    /// The same over any slot store.
    pub fn with_store(base: Box<dyn BlockSource>, cluster_size: u64, overlay: S) -> Self {
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
        (0..self.total_clusters)
            .filter(|&c| self.slots[c as usize] != NO_SLOT)
            .collect()
    }

    /// Final content of one cluster (overlay if touched, else base), padded to a
    /// whole `cluster_size` for the merge writer.
    pub fn read_cluster(&mut self, cluster: u64) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; self.cluster_size as usize];
        let slot = self.slots[cluster as usize];
        if slot != NO_SLOT {
            self.overlay.read_in_slot(slot, cluster, 0, &mut buf)?;
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
        self.overlay.write_in_slot(slot, cluster, 0, &buf)?;
        self.slots[cluster as usize] = slot;
        self.slot_count += 1;
        Ok(slot)
    }

    fn cluster_range(&self, offset: u64, len: u64) -> (u64, u64) {
        (
            offset / self.cluster_size,
            (offset + len - 1) / self.cluster_size,
        )
    }
}

impl<S: SlotStore> BlockSource for WritableOverlaySource<S> {
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
                .read_in_slot(slot, c, from - cstart, &mut tmp)?;
            out[(from - offset) as usize..(to - offset) as usize].copy_from_slice(&tmp);
        }
        Ok(())
    }
}

impl<S: SlotStore> WritableBlockSource for WritableOverlaySource<S> {
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
            self.overlay.write_in_slot(
                slot,
                c,
                from - cstart,
                &data[(from - offset) as usize..(to - offset) as usize],
            )?;
        }
        Ok(())
    }
}

/// Share a writable source across the target's session threads and the merge
/// step: a session locks it per request; after the target stops, the merge
/// locks it to read `touched_clusters` / `read_cluster`.
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

    /// The scratch of an encrypted image must be unreadable on disk, and it
    /// must still behave exactly like the plaintext one.
    #[test]
    fn a_sealed_scratch_holds_no_plaintext_and_reads_back_the_same() {
        let cs = 4096u64;
        let chain = crate::seal::ChainKey::generate().unwrap();
        let file = scratch();
        let probe = file.try_clone().unwrap();
        let secret: Vec<u8> = b"secret-payload--".repeat(cs as usize / 16);
        let base = MemSource(vec![0xAA; cs as usize * 4]);
        let store = SealedSlots::new(file, cs, &chain, [3; 16]).unwrap();
        let mut ov = WritableOverlaySource::with_store(Box::new(base), cs, store);

        ov.write_at(cs, &secret).unwrap();
        ov.write_at(cs * 2 + 8, &[1, 2, 3, 4]).unwrap();
        assert_eq!(ov.touched_clusters(), vec![1, 2]);

        // Reads: through the overlay, across cluster boundaries, and whole
        // clusters for the merge — all the same answers as in the clear.
        let mut out = vec![0u8; cs as usize];
        ov.read_at(cs, &mut out).unwrap();
        assert_eq!(out, secret);
        let mut span = vec![0u8; 16];
        ov.read_at(cs * 2 + 4, &mut span).unwrap();
        assert_eq!(&span[4..8], &[1, 2, 3, 4]);
        assert!(span[..4].iter().all(|&b| b == 0xAA), "untouched base bytes");
        assert_eq!(&ov.read_cluster(1).unwrap()[..], &secret[..]);
        assert!(ov.read_cluster(0).unwrap().iter().all(|&b| b == 0xAA));

        // On disk: two slots, each a record, and nothing readable in them.
        assert_eq!(probe.metadata().unwrap().len(), 2 * ov.slot_len());
        let mut raw = Vec::new();
        {
            use std::io::Read;
            let mut f = probe.try_clone().unwrap();
            f.seek(SeekFrom::Start(0)).unwrap();
            f.read_to_end(&mut raw).unwrap();
        }
        assert!(
            !raw.windows(16).any(|w| w == b"secret-payload--"),
            "plaintext survived into the scratch file"
        );
        assert!(raw.starts_with(b"OBKR"), "a slot is a sealed record");
    }

    #[test]
    fn a_sealed_scratch_reseals_on_every_rewrite_and_refuses_edited_bytes() {
        let cs = 4096u64;
        let chain = crate::seal::ChainKey::generate().unwrap();
        let file = scratch();
        let probe = file.try_clone().unwrap();
        let store = SealedSlots::new(file, cs, &chain, [3; 16]).unwrap();
        let mut ov = WritableOverlaySource::with_store(
            Box::new(MemSource(vec![0u8; cs as usize * 2])),
            cs,
            store,
        );

        // Every rewrite of the same cluster is a new record: same key, never
        // the same nonce (§7.3).
        let mut nonces = std::collections::HashSet::new();
        for round in 0..3u8 {
            ov.write_at(0, &[round; 32]).unwrap();
            let mut header = [0u8; 84];
            let mut f = probe.try_clone().unwrap();
            f.seek(SeekFrom::Start(0)).unwrap();
            use std::io::Read;
            f.read_exact(&mut header).unwrap();
            assert!(nonces.insert(header[40..84].to_vec()), "a nonce came back");
            let mut out = vec![0u8; 32];
            ov.read_at(0, &mut out).unwrap();
            assert_eq!(out, vec![round; 32]);
        }

        // Editing the scratch is caught when the cluster is read back.
        {
            use std::io::Write as _;
            let mut f = probe.try_clone().unwrap();
            f.seek(SeekFrom::Start(200)).unwrap();
            f.write_all(&[0xFF]).unwrap();
            f.flush().unwrap();
        }
        let mut out = vec![0u8; 32];
        assert!(
            ov.read_at(0, &mut out).is_err(),
            "an edited scratch must not be read back as ours"
        );
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
