//! Putting changed clusters back into a finished encrypted image.
//!
//! Two passes need this and neither can work without it: deleting the excluded
//! files' catalog entries inside the image, and folding an increment into its
//! base. Both end with "these clusters now hold something else" — which a
//! write-once container cannot simply accept.
//!
//! What happens instead:
//!
//! 1. the file is cut back to the end of its last data record, dropping the
//!    index, the metadata and the tables — they are all re-emitted below, so
//!    nothing stale is left to accumulate across passes;
//! 2. the **old** record of every changed cluster is overwritten with zeros.
//!    This is not tidiness: those records hold the previous contents of the
//!    filesystem's own metadata, and leaving them would leave the names and
//!    sizes of files the pass has just removed inside the image, readable to
//!    anyone holding the key;
//! 3. a new record is appended for each changed cluster, under a fresh nonce;
//! 4. the index, the sealed sidecar, the root, the cluster tables and the
//!    header are written again, the header last — the same commit point the
//!    image was created with. A crash before it leaves the previous image
//!    readable, because everything up to step 2 only added bytes past its end.
//!
//! Step 2 is why a crash between steps 2 and 4 is not silent damage: the old
//! records of the changed clusters are gone, so the image cannot be opened
//! with the old header either — the reader refuses a record whose bytes are
//! zeros. It is a failed pass, not a half-applied one.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use own_backup_common::Result;
use zeroize::Zeroizing;

use super::header::{obk_extensions, ObkRootPointer};
use super::index::{encode_chunk, ClusterState, MAX_INDEX_ENTRIES_PER_CHUNK};
use super::reader::ObkImage;
use super::root::{ChunkRef, IndexChunkRef, ObkRoot};
use super::writer::seal_chunk_with;
use super::{bad, INCOMPAT_OBK_SEAL};
use crate::qcow2::layout::{build_header_cluster, HeaderParams, Qcow2Tables};
use crate::qcow2::{
    decode_l2_entry, encode_compressed_l2_entry, ClusterDescriptor, COMPRESSION_TYPE_ZSTD,
    INCOMPAT_COMPRESSION_TYPE, OFLAG_ZERO,
};
use crate::seal::cluster::{framed_len, Encoding, Purpose, RecordWriter, MAX_PAYLOAD_LEN};
use crate::seal::ChainKey;

/// What a pass changed: guest cluster -> its whole new content.
pub type ChangedClusters = BTreeMap<u64, Vec<u8>>;

/// Apply `changed` to the image at `path`, optionally replacing its sealed
/// sidecar as well.
///
/// Every cluster in `changed` must be exactly one cluster long. Clusters the
/// image does not define yet may be added; a cluster whose new content is all
/// zeros becomes an explicit zero in a child image, and in a full backup is
/// dropped from the index (it then reads as zeros, which is what it is).
pub fn apply_changed_clusters(
    path: &Path,
    chain: &ChainKey,
    changed: &ChangedClusters,
    metadata: Option<&[u8]>,
) -> Result<RebuildStats> {
    // The map is already in memory, so its shapes can be checked before
    // anything is touched. The streaming form below cannot do that, and says so.
    let cluster_size = {
        let file = std::fs::File::open(path)
            .map_err(|e| bad(format!("cannot open {}: {e}", path.display())))?;
        super::header::ObkFileHeader::read(&mut std::io::BufReader::new(file))?.cluster_size
    };
    for (cluster, data) in changed {
        if data.len() as u64 != cluster_size {
            return Err(bad(format!(
                "cluster {cluster} is {} bytes, expected {cluster_size}",
                data.len()
            )));
        }
    }
    let clusters: Vec<u64> = changed.keys().copied().collect();
    apply_changed(
        path,
        chain,
        &clusters,
        |cluster| Ok(changed[&cluster].clone()),
        metadata,
    )
}

/// The same, reading each changed cluster only when it is needed.
///
/// A deletion pass touches the filesystem's own metadata, which on a large
/// volume is tens of thousands of clusters — holding them all in memory to hand
/// them over would cost gigabytes, so they are streamed from wherever the pass
/// keeps them (for an encrypted image, a sealed scratch).
///
/// The price of streaming: a cluster that turns out to be the wrong size, or a
/// reader that fails part-way, is discovered when the image has already been
/// cut back. The image is then a failed pass — not half-applied, because the
/// old records of the changed clusters are already gone — and the chain's
/// previous images are untouched.
pub fn apply_changed(
    path: &Path,
    chain: &ChainKey,
    clusters: &[u64],
    mut read_cluster: impl FnMut(u64) -> Result<Vec<u8>>,
    metadata: Option<&[u8]>,
) -> Result<RebuildStats> {
    let (root, header, mut states, entries, data_end, existing_sidecar) = read_state(path, chain)?;
    let cluster_size = header.cluster_size;
    let changed: std::collections::BTreeSet<u64> = clusters.iter().copied().collect();
    for cluster in &changed {
        if *cluster >= root.guest_clusters {
            return Err(bad(format!("cluster {cluster} beyond the virtual disk")));
        }
    }

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| bad(format!("cannot open {} for writing: {e}", path.display())))?;

    // 1. Everything past the data records goes; it is all re-emitted below.
    file.set_len(data_end)?;

    // 2. The previous bytes of every cluster this pass replaces. Their exact
    //    length comes from each record's own framing, so a neighbour packed
    //    right behind it is never touched.
    let mut zeroed = 0u64;
    for cluster in &changed {
        if let Some(ClusterState::Present(_)) = states.get(cluster) {
            if let Some(&entry) = entries.get(cluster) {
                if let ClusterDescriptor::Compressed {
                    host_offset,
                    length,
                } = decode_l2_entry(entry, header.cluster_bits)?
                {
                    zeroed += zero_record(&mut file, host_offset, length)?;
                }
            }
        }
    }

    // 3. The new records, appended after the old data.
    let chain_id = *root.chain_id.as_bytes();
    let data_writer = RecordWriter::new(chain, chain_id, Purpose::Data)?;
    let index_writer = RecordWriter::new(chain, chain_id, Purpose::Index)?;
    let meta_writer = RecordWriter::new(chain, chain_id, Purpose::Manifest)?;
    let mut tables = Qcow2Tables::resume(header.cluster_bits, root.guest_clusters, data_end);
    let has_parent = root.parent_backup_id.is_some();

    file.seek(SeekFrom::End(0))?;
    for &cluster in &changed {
        let data = read_cluster(cluster)?;
        if data.len() as u64 != cluster_size {
            return Err(bad(format!(
                "cluster {cluster} is {} bytes, expected {cluster_size}",
                data.len()
            )));
        }
        if data.iter().all(|&b| b == 0) {
            // A cluster of zeros costs no record: in a child it overrides the
            // parent, in a full backup it is simply not there.
            if has_parent {
                states.insert(cluster, ClusterState::Zero);
            } else {
                states.remove(&cluster);
            }
            continue;
        }
        let compressed = Zeroizing::new(
            zstd::bulk::compress(&data, crate::qcow2::DEFAULT_ZSTD_LEVEL)
                .map_err(|e| own_backup_common::OwnBackupError::Compression(e.to_string()))?,
        );
        let (encoding, payload) = if (compressed.len() as u64) < cluster_size {
            (Encoding::Zstd, &compressed[..])
        } else {
            (Encoding::Raw, &data[..])
        };
        let record = data_writer.seal(cluster, encoding, cluster_size as u32, payload)?;
        let host_offset = tables.write_packed(&mut file, &record.bytes)?;
        let entry =
            encode_compressed_l2_entry(host_offset, record.bytes.len() as u64, header.cluster_bits)?;
        tables.set_entry(cluster, entry)?;
        states.insert(cluster, ClusterState::Present(record.id));
    }

    // Clusters the pass did not touch keep the records they already had, and
    // their refcounts are accounted from the cluster table alone — no record
    // has to be read back for that.
    for (cluster, entry) in &entries {
        if tables.has_entry(*cluster) || changed.contains(cluster) {
            continue;
        }
        if let ClusterDescriptor::Compressed {
            host_offset,
            length,
        } = decode_l2_entry(*entry, header.cluster_bits)?
        {
            tables.note_packed(host_offset, length);
        }
        tables.set_entry(*cluster, *entry)?;
    }
    // Explicit zeros of a child that the pass did not touch.
    for (cluster, state) in &states {
        if matches!(state, ClusterState::Zero) && !tables.has_entry(*cluster) {
            tables.set_entry(*cluster, OFLAG_ZERO)?;
        }
    }

    // 4. Index, sidecar, root, tables, header — the header last.
    // Either the caller's new sidecar, or the one read before the cut.
    let sidecar: Zeroizing<Vec<u8>> = match metadata {
        Some(bytes) => Zeroizing::new(bytes.to_vec()),
        None => existing_sidecar,
    };
    let reserve = root
        .meta_chunks
        .iter()
        .map(|c| c.len as usize - crate::seal::cluster::HEADER_LEN - crate::seal::cluster::TAG_LEN)
        .sum::<usize>();
    let padded: Zeroizing<Vec<u8>> = if reserve > sidecar.len() {
        let mut buf = Zeroizing::new(vec![0u8; reserve]);
        buf[..sidecar.len()].copy_from_slice(&sidecar);
        buf
    } else {
        sidecar.clone()
    };

    let listed: Vec<(u64, ClusterState)> = states.into_iter().collect();
    let mut index_chunks = Vec::new();
    for (ordinal, slice) in listed.chunks(MAX_INDEX_ENTRIES_PER_CHUNK).enumerate() {
        let payload = Zeroizing::new(encode_chunk(slice)?);
        let chunk = seal_chunk_with(
            &index_writer,
            ordinal as u64,
            &payload,
            false,
            &mut tables,
            &mut file,
        )?;
        index_chunks.push(IndexChunkRef {
            chunk,
            entries: slice.len() as u32,
            first_cluster: slice[0].0,
        });
    }
    let mut meta_chunks: Vec<ChunkRef> = Vec::new();
    for (ordinal, slice) in padded.chunks(MAX_PAYLOAD_LEN).enumerate() {
        meta_chunks.push(seal_chunk_with(
            &meta_writer,
            ordinal as u64 + 1,
            slice,
            reserve > 0,
            &mut tables,
            &mut file,
        )?);
    }

    let new_root = ObkRoot {
        sealed_metadata_len: sidecar.len() as u64,
        defined_clusters: listed.len() as u64,
        index_chunks,
        meta_chunks,
        ..root.clone()
    };
    let root_payload = Zeroizing::new(new_root.encode()?);
    let root_ref = seal_chunk_with(
        &meta_writer,
        0,
        &root_payload,
        reserve > 0,
        &mut tables,
        &mut file,
    )?;

    let info = tables.finish_tables(&mut file)?;
    let mut open = header.open.clone();
    open.root = ObkRootPointer::new(root_ref.offset, root_ref.len as usize, root_ref.id);
    let cluster = build_header_cluster(
        HeaderParams {
            cluster_bits: header.cluster_bits,
            virtual_size: header.virtual_size,
            incompatible_features: INCOMPAT_OBK_SEAL | INCOMPAT_COMPRESSION_TYPE,
            compression_type: COMPRESSION_TYPE_ZSTD,
            backing_file: header.backing_file.as_deref(),
            extensions: &obk_extensions(&open)?,
        },
        &info,
    )?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&cluster)?;
    file.sync_all()?;

    Ok(RebuildStats {
        clusters_changed: changed.len() as u64,
        bytes_zeroed: zeroed,
        defined_clusters: new_root.defined_clusters,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RebuildStats {
    pub clusters_changed: u64,
    /// Bytes of previous records overwritten — the old contents of the changed
    /// clusters, which must not survive the pass.
    pub bytes_zeroed: u64,
    pub defined_clusters: u64,
}

/// The image as it is now: its root, its header, what its index says, what its
/// cluster table says, and where its data records end.
#[allow(clippy::type_complexity)]
fn read_state(
    path: &Path,
    chain: &ChainKey,
) -> Result<(
    ObkRoot,
    super::header::ObkFileHeader,
    BTreeMap<u64, ClusterState>,
    BTreeMap<u64, u64>,
    u64,
    Zeroizing<Vec<u8>>,
)> {
    let file = std::fs::File::open(path)
        .map_err(|e| bad(format!("cannot open {}: {e}", path.display())))?;
    let mut image = ObkImage::open(std::io::BufReader::new(file), chain)
        .map_err(|e| bad(format!("{}: {e}", path.display())))?;
    let root = image.root().clone();
    let header = image.file_header().clone();
    // Read while the metadata is still there: the pass cuts the file back to
    // the end of its data records, and everything after that goes with it.
    let sidecar = image.sealed_metadata()?;

    let mut states = BTreeMap::new();
    let mut entries = BTreeMap::new();
    // The first byte past every data record: where new ones may be appended and
    // where the metadata this pass replaces begins.
    let mut data_end = header.cluster_size;
    for cluster in 0..root.guest_clusters {
        let state = image.cluster_state(cluster)?;
        match state {
            ClusterState::Absent => continue,
            ClusterState::Zero => {
                states.insert(cluster, state);
            }
            ClusterState::Present(_) => {
                states.insert(cluster, state);
                let entry = image.raw_l2_entry(cluster)?;
                entries.insert(cluster, entry);
                if let ClusterDescriptor::Compressed {
                    host_offset,
                    length,
                } = decode_l2_entry(entry, header.cluster_bits)?
                {
                    data_end = data_end.max(host_offset + length);
                }
            }
        }
    }
    Ok((root, header, states, entries, data_end, sidecar))
}

/// Overwrite one record with zeros, by its own framing. Returns how many bytes
/// were zeroed.
fn zero_record(file: &mut std::fs::File, host_offset: u64, rounded_len: u64) -> Result<u64> {
    let capped = (rounded_len as usize).min(MAX_PAYLOAD_LEN + 256);
    let mut head = vec![0u8; capped];
    file.seek(SeekFrom::Start(host_offset))?;
    let mut filled = 0;
    while filled < head.len() {
        match file.read(&mut head[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    head.truncate(filled);
    let len = framed_len(&head)?.min(filled);
    file.seek(SeekFrom::Start(host_offset))?;
    file.write_all(&vec![0u8; len])?;
    Ok(len as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seal::obk::container_tests::{cluster_bits, Fixture};
    use crate::seal::obk::{ObkChain, ObkRandomReader, ObkWriter};
    use crate::sidecar::ChainRole;
    use uuid::Uuid;

    fn cluster_of(fill: u8) -> Vec<u8> {
        vec![fill; 1 << cluster_bits()]
    }

    /// A cluster whose content is unmistakable in a hex dump.
    fn marked(text: &[u8]) -> Vec<u8> {
        let mut data = vec![0x5A; 1 << cluster_bits()];
        data[..text.len()].copy_from_slice(text);
        data
    }

    #[test]
    fn a_changed_cluster_replaces_the_old_one_and_the_old_bytes_are_gone() {
        let f = Fixture::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("full.obk");
        let mut writer =
            ObkWriter::create(&path, &f.chain, f.options(Uuid::new_v4(), ChainRole::Full, None))
                .unwrap();
        writer.write_cluster(0, &cluster_of(0xAA)).unwrap();
        writer
            .write_cluster(1, &marked(b"catalog-entry-of-a-removed-file"))
            .unwrap();
        writer.write_cluster(2, &cluster_of(0xCC)).unwrap();
        writer.finish(b"sidecar before the pass").unwrap();

        let mut changed = ChangedClusters::new();
        changed.insert(1, marked(b"catalog-after-the-deletion"));
        let stats = apply_changed_clusters(&path, &f.chain, &changed, None).unwrap();
        assert_eq!(stats.clusters_changed, 1);
        assert!(stats.bytes_zeroed > 0, "the old record must be overwritten");

        let cs = 1usize << cluster_bits();
        let mut reader = ObkRandomReader::open(&path, &f.chain).unwrap();
        let mut buf = vec![0u8; cs];
        reader.read_at(cs as u64, &mut buf).unwrap();
        assert_eq!(buf, marked(b"catalog-after-the-deletion"));
        // Everything else is exactly as it was, and so is the sidecar.
        reader.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, cluster_of(0xAA));
        reader.read_at(cs as u64 * 2, &mut buf).unwrap();
        assert_eq!(buf, cluster_of(0xCC));
        assert_eq!(&reader.sealed_metadata().unwrap()[..], b"sidecar before the pass");

        // The point of the pass: what the cluster used to hold is no longer in
        // the file, not merely unreferenced.
        let raw = std::fs::read(&path).unwrap();
        assert!(
            !raw.windows(31).any(|w| w == b"catalog-entry-of-a-removed-file"),
            "the previous contents of the changed cluster survived"
        );
    }

    #[test]
    fn the_sidecar_can_be_replaced_in_the_same_pass_and_zeros_drop_a_cluster() {
        let f = Fixture::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("full.obk");
        let mut writer =
            ObkWriter::create(&path, &f.chain, f.options(Uuid::new_v4(), ChainRole::Full, None))
                .unwrap();
        writer.write_cluster(0, &cluster_of(0xAA)).unwrap();
        writer.write_cluster(3, &cluster_of(0xBB)).unwrap();
        writer.finish(b"before").unwrap();

        let mut changed = ChangedClusters::new();
        // A cluster emptied by the pass: in a full backup it is simply not
        // there afterwards, and reads as zeros.
        changed.insert(3, vec![0u8; 1 << cluster_bits()]);
        let stats =
            apply_changed_clusters(&path, &f.chain, &changed, Some(b"after the pass")).unwrap();
        assert_eq!(stats.defined_clusters, 1);

        let cs = 1usize << cluster_bits();
        let mut reader = ObkRandomReader::open(&path, &f.chain).unwrap();
        let mut buf = vec![0xFFu8; cs];
        reader.read_at(cs as u64 * 3, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0), "an emptied cluster reads as zeros");
        assert_eq!(&reader.sealed_metadata().unwrap()[..], b"after the pass");
    }

    #[test]
    fn a_child_keeps_inheriting_and_its_explicit_zeros_survive_a_pass() {
        let f = Fixture::new();
        let dir = tempfile::tempdir().unwrap();
        let full_id = Uuid::new_v4();
        let mut base = ObkWriter::create(
            &dir.path().join("full.obk"),
            &f.chain,
            f.options(full_id, ChainRole::Full, None),
        )
        .unwrap();
        base.write_cluster(0, &cluster_of(0x11)).unwrap();
        base.write_cluster(1, &cluster_of(0x22)).unwrap();
        base.finish(b"base").unwrap();

        let child_path = dir.path().join("inc.obk");
        let mut child = ObkWriter::create(
            &child_path,
            &f.chain,
            f.options(
                Uuid::new_v4(),
                ChainRole::Incremental,
                Some((full_id, "full.obk")),
            ),
        )
        .unwrap();
        child.write_cluster(1, &cluster_of(0x33)).unwrap();
        child.write_zero_cluster(2).unwrap();
        child.finish(b"child").unwrap();

        let mut changed = ChangedClusters::new();
        changed.insert(1, cluster_of(0x44));
        apply_changed_clusters(&child_path, &f.chain, &changed, None).unwrap();

        let mut chain = ObkChain::open(&child_path, &f.chain).unwrap();
        assert_eq!(&chain.read_cluster(0).unwrap()[..], &cluster_of(0x11)[..], "inherited");
        assert_eq!(&chain.read_cluster(1).unwrap()[..], &cluster_of(0x44)[..], "changed");
        assert!(
            chain.read_cluster(2).unwrap().iter().all(|&b| b == 0),
            "an explicit zero must not turn back into the parent's data"
        );
    }

    #[test]
    fn a_pass_can_be_repeated_and_a_foreign_key_cannot_run_one() {
        let f = Fixture::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("full.obk");
        let mut writer =
            ObkWriter::create(&path, &f.chain, f.options(Uuid::new_v4(), ChainRole::Full, None))
                .unwrap();
        writer.write_cluster(0, &cluster_of(1)).unwrap();
        writer.finish(b"sidecar").unwrap();

        for round in 2..=5u8 {
            let mut changed = ChangedClusters::new();
            changed.insert(0, cluster_of(round));
            apply_changed_clusters(&path, &f.chain, &changed, None).unwrap();
            let mut reader = ObkRandomReader::open(&path, &f.chain).unwrap();
            let mut buf = vec![0u8; 1 << cluster_bits()];
            reader.read_at(0, &mut buf).unwrap();
            assert_eq!(buf, cluster_of(round));
        }

        let stranger = crate::seal::ChainKey::generate().unwrap();
        let before = std::fs::read(&path).unwrap();
        let mut changed = ChangedClusters::new();
        changed.insert(0, cluster_of(9));
        assert!(apply_changed_clusters(&path, &stranger, &changed, None).is_err());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "a refused pass changes nothing"
        );
    }

    #[test]
    fn a_cluster_that_is_not_one_cluster_is_refused_before_anything_is_written() {
        let f = Fixture::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("full.obk");
        let mut writer =
            ObkWriter::create(&path, &f.chain, f.options(Uuid::new_v4(), ChainRole::Full, None))
                .unwrap();
        writer.write_cluster(0, &cluster_of(1)).unwrap();
        writer.finish(b"sidecar").unwrap();
        let before = std::fs::read(&path).unwrap();

        let mut changed = ChangedClusters::new();
        changed.insert(0, vec![7u8; 10]);
        assert!(apply_changed_clusters(&path, &f.chain, &changed, None).is_err());
        let mut changed = ChangedClusters::new();
        changed.insert(9_999, cluster_of(7));
        assert!(apply_changed_clusters(&path, &f.chain, &changed, None).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}
