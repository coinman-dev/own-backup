//! Replacing the sealed sidecar of a **finished** encrypted image.
//!
//! The Windows engine learns the last of what belongs in a sidecar only after
//! the image is closed: how many files the deletion pass actually removed, and
//! the checksums of the blocks the filesystem driver touched. A plaintext image
//! keeps its sidecar in a guest meta partition sized for that and rewrites it in
//! place; an encrypted one has no such partition (§7.3), so it reserves room
//! inside the container at creation ([`super::writer::ObkWriter::finish_with_reserve`])
//! and the new sidecar takes the old one's place here.
//!
//! For a new image before its backup run is published, or within a staged
//! legacy copy, the edit is made in place:
//!
//! - the reservation is stored **uncompressed**, so a sidecar of the same size
//!   produces records of exactly the same length — nothing moves, and no data
//!   record, index chunk or cluster table is touched;
//! - every replaced record is sealed afresh, with a new session and a new
//!   nonce, so no key/nonce pair is ever reused;
//! - the old ciphertext is overwritten, not orphaned inside the file;
//! - the root record is replaced too (it names the metadata chunks), and the
//!   header cluster is rewritten to point at the new root.
//!
//! For a published dual-header image, metadata and root records are appended
//! and a checked header is written to the inactive slot. The previous records
//! stay intact until the new slot is durable. A legacy image is copied first;
//! the staged copy is checked and synced before it replaces the original.

use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

use own_backup_common::Result;
use zeroize::Zeroizing;

use super::header::{finish_dual_header, obk_extensions, ObkRootPointer};
use super::reader::ObkImage;
use super::root::ObkRoot;
use super::{bad, INCOMPAT_OBK_SEAL, SECOND_HEADER_OFFSET};
use crate::qcow2::layout::{build_header_cluster, HeaderParams, TablesInfo};
use crate::qcow2::{COMPRESSION_TYPE_ZSTD, INCOMPAT_COMPRESSION_TYPE};
use crate::seal::cluster::{Purpose, RecordWriter, HEADER_LEN, TAG_LEN};
use crate::seal::ChainKey;

/// Replace the sealed sidecar of `path` with `metadata`.
///
/// Fails without touching the image when it reserved no room, when the new
/// sidecar does not fit the reservation, or when the key does not open it.
pub fn update_sealed_metadata(path: &Path, chain: &ChainKey, metadata: &[u8]) -> Result<()> {
    if ObkImage::open_path(path, chain)?.open_header().dual_header {
        if metadata.len() > reserved_room(path, chain)? {
            return Err(bad("replacement sidecar exceeds the reserved room"));
        }
        return super::rebuild::apply_changed_with(
            path,
            chain,
            &[],
            |_| unreachable!("no clusters were requested"),
            super::rebuild::RebuildOptions {
                metadata: Some(metadata),
                previous: super::rebuild::PreviousContents::Keep,
                ..Default::default()
            },
        )
        .map(|_| ());
    }
    super::transaction::replace(path, chain, |stage| {
        update_sealed_metadata_in_place(stage, chain, metadata)
    })
}

/// Finalize a newly written image before its backup run is published. There
/// is no previous restore point at this path to preserve, and this avoids a
/// second full-size copy on every ordinary encrypted backup.
pub(crate) fn update_unpublished_sealed_metadata(
    path: &Path,
    chain: &ChainKey,
    metadata: &[u8],
) -> Result<()> {
    update_sealed_metadata_in_place(path, chain, metadata)
}

pub(super) fn update_sealed_metadata_in_place(
    path: &Path,
    chain: &ChainKey,
    metadata: &[u8],
) -> Result<()> {
    let (root, header) = {
        let file = std::fs::File::open(path)
            .map_err(|e| bad(format!("cannot open {}: {e}", path.display())))?;
        let image = ObkImage::open(std::io::BufReader::new(file), chain)
            .map_err(|e| bad(format!("{}: {e}", path.display())))?;
        (image.root().clone(), image.file_header().clone())
    };

    // Every chunk must be a Raw record, or its replacement would not be the
    // same length. That is exactly what the reservation guarantees.
    let mut room = 0usize;
    for chunk in &root.meta_chunks {
        let payload = (chunk.len as usize)
            .checked_sub(HEADER_LEN + TAG_LEN)
            .ok_or_else(|| bad("metadata chunk shorter than a record"))?;
        room += payload;
    }
    if root.meta_chunks.is_empty() || room < metadata.len() {
        return Err(bad(format!(
            "this image reserved room for {room} bytes of sidecar, not {}",
            metadata.len()
        )));
    }

    let chain_id = *root.chain_id.as_bytes();
    let writer = RecordWriter::new(chain, chain_id, Purpose::Manifest)?;
    let mut padded = Zeroizing::new(vec![0u8; room]);
    padded[..metadata.len()].copy_from_slice(metadata);

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| bad(format!("cannot open {} for writing: {e}", path.display())))?;

    // 1. The metadata chunks, each exactly where and as long as it was.
    let mut new_root = root.clone();
    new_root.sealed_metadata_len = metadata.len() as u64;
    let mut at = 0usize;
    for (ordinal, chunk) in root.meta_chunks.iter().enumerate() {
        let payload_len = chunk.len as usize - HEADER_LEN - TAG_LEN;
        let slice = &padded[at..at + payload_len];
        at += payload_len;
        let record = writer.seal(
            ordinal as u64 + 1,
            crate::seal::cluster::Encoding::Raw,
            payload_len as u32,
            slice,
        )?;
        if record.bytes.len() != chunk.len as usize {
            return Err(bad("replacement metadata record changed length"));
        }
        file.seek(SeekFrom::Start(chunk.offset))?;
        file.write_all(&record.bytes)?;
        new_root.meta_chunks[ordinal].id = record.id;
    }

    // 2. The root record, which names them.
    let payload = Zeroizing::new(new_root.encode()?);
    let root_record = writer.seal(
        0,
        crate::seal::cluster::Encoding::Raw,
        payload.len() as u32,
        &payload,
    )?;
    if root_record.bytes.len() != header.open.root.len as usize {
        return Err(bad("replacement root record changed length"));
    }
    file.seek(SeekFrom::Start(header.open.root.offset))?;
    file.write_all(&root_record.bytes)?;

    // 3. The header cluster, pointing at the new root of the staged image.
    let mut open = header.open.clone();
    if open.dual_header {
        open.header_generation = open
            .header_generation
            .checked_add(1)
            .ok_or_else(|| bad("header generation overflow"))?;
    }
    open.root = ObkRootPointer::new(
        header.open.root.offset,
        header.open.root.len as usize,
        root_record.id,
    );
    let cluster = build_header_cluster(
        HeaderParams {
            cluster_bits: header.cluster_bits,
            virtual_size: header.virtual_size,
            incompatible_features: INCOMPAT_OBK_SEAL | INCOMPAT_COMPRESSION_TYPE,
            compression_type: COMPRESSION_TYPE_ZSTD,
            backing_file: header.backing_file.as_deref(),
            extensions: &obk_extensions(&open)?,
        },
        &TablesInfo {
            l1_size: header.l1_size,
            l1_table_offset: header.l1_table_offset,
            refcount_table_offset: header.refcount_table_offset,
            refcount_table_clusters: header.refcount_table_clusters,
        },
    )?;
    let cluster = if open.dual_header {
        finish_dual_header(cluster)?
    } else {
        cluster
    };
    if open.dual_header {
        file.seek(SeekFrom::Start(SECOND_HEADER_OFFSET))?;
        file.write_all(&cluster)?;
    }
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&cluster)?;
    file.sync_all()?;
    Ok(())
}

/// The reservation an image ended up with, for tests and diagnostics.
pub fn reserved_room(path: &Path, chain: &ChainKey) -> Result<usize> {
    let file = std::fs::File::open(path)?;
    let image = ObkImage::open(std::io::BufReader::new(file), chain)?;
    Ok(image
        .root()
        .meta_chunks
        .iter()
        .map(|c| c.len as usize - HEADER_LEN - TAG_LEN)
        .sum())
}

/// Any root the image names, for callers that only need identities.
pub fn image_root(path: &Path, chain: &ChainKey) -> Result<ObkRoot> {
    let file = std::fs::File::open(path)?;
    let image = ObkImage::open(std::io::BufReader::new(file), chain)?;
    Ok(image.root().clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seal::obk::container_tests::{cluster_bits, Fixture};
    use crate::seal::obk::{ObkRandomReader, ObkWriter};
    use crate::sidecar::ChainRole;
    use uuid::Uuid;

    fn build(path: &Path, f: &Fixture, metadata: &[u8], reserve: usize) {
        let mut writer = ObkWriter::create(
            path,
            &f.chain,
            f.options(Uuid::new_v4(), ChainRole::Full, None),
        )
        .unwrap();
        writer
            .write_cluster(0, &vec![9u8; 1 << cluster_bits()])
            .unwrap();
        writer.finish_with_reserve(metadata, reserve).unwrap();
    }

    #[test]
    fn child_metadata_can_outgrow_the_parent_without_starting_a_full_chain() {
        use crate::seal::obk::{read_open_header, ObkChain};
        use crate::sidecar::{calculate_meta_partition_size, whole_disk_metadata_size};

        let f = Fixture::new();
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("full.obk");
        // Counts from the 2026-10-09 production failure: 1 MiB -> 2 MiB.
        let parent_room = calculate_meta_partition_size(57_563, 0);
        let needed = calculate_meta_partition_size(62_142, 0);
        assert_eq!(parent_room, 1 << 20);
        assert_eq!(needed, 2 << 20);
        build(&base, &f, b"parent metadata", parent_room as usize);
        let parent_header = read_open_header(&base).unwrap();
        let parent_bytes = std::fs::read(&base).unwrap();

        for role in [ChainRole::Incremental, ChainRole::Differential] {
            let child = dir.path().join(format!("{role:?}.obk"));
            let room = whole_disk_metadata_size(true, needed, Some(parent_room))
                .expect("OBK metadata growth must preserve the parent");
            let mut writer = ObkWriter::create(
                &child,
                &f.chain,
                f.options(
                    Uuid::new_v4(),
                    role,
                    Some((parent_header.backup_id, "full.obk")),
                ),
            )
            .unwrap();
            let changed = vec![7u8; 1 << cluster_bits()];
            writer.write_cluster(1, &changed).unwrap();
            writer
                .finish_with_reserve(b"provisional metadata", room as usize)
                .unwrap();
            // Exercise the same finalization used by a new Windows backup,
            // with real metadata larger than the parent's entire reservation.
            let final_metadata = vec![b'M'; parent_room as usize + 4096];
            update_sealed_metadata_in_place(&child, &f.chain, &final_metadata).unwrap();
            assert_eq!(reserved_room(&child, &f.chain).unwrap(), needed as usize);

            let header = read_open_header(&child).unwrap();
            assert_eq!(header.role, role);
            assert_eq!(header.parent_backup_id, Some(parent_header.backup_id));
            assert_eq!(header.chain_id, parent_header.chain_id);
            assert_eq!(header.virtual_size, parent_header.virtual_size);
            let mut chain = ObkChain::open(&child, &f.chain).unwrap();
            assert_eq!(chain.chain_len(), 2);
            assert_eq!(
                &chain.read_cluster(0).unwrap()[..],
                &vec![9u8; 1 << cluster_bits()][..]
            );
            assert_eq!(&chain.read_cluster(1).unwrap()[..], &changed[..]);
            assert_eq!(&chain.sealed_metadata().unwrap()[..], &final_metadata[..]);
            assert_eq!(std::fs::read(&base).unwrap(), parent_bytes);
        }
    }

    #[test]
    fn a_reserved_sidecar_is_appended_and_the_data_is_untouched() {
        let f = Fixture::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("full.obk");
        build(
            &path,
            &f,
            b"first sidecar, written before the image closed",
            4096,
        );
        assert_eq!(reserved_room(&path, &f.chain).unwrap(), 4096);

        let before = std::fs::metadata(&path).unwrap().len();
        let final_sidecar = b"final sidecar: exclusions done, checksums updated".to_vec();
        update_sealed_metadata(&path, &f.chain, &final_sidecar).unwrap();
        let after = std::fs::metadata(&path).unwrap().len();
        assert!(after > before && after - before < 1 << 20);

        let mut reader = ObkRandomReader::open(&path, &f.chain).unwrap();
        assert_eq!(&reader.sealed_metadata().unwrap()[..], &final_sidecar[..]);
        // The data records are exactly as they were.
        let mut data = vec![0u8; 1 << cluster_bits()];
        reader.read_at(0, &mut data).unwrap();
        assert_eq!(data, vec![9u8; 1 << cluster_bits()]);

        // The older sealed records remain for a torn-header fallback.
        let raw = std::fs::read(&path).unwrap();
        assert!(raw.len() as u64 > before);
    }

    #[test]
    fn a_replacement_is_refused_unless_it_fits_a_reservation_and_has_the_key() {
        let f = Fixture::new();
        let dir = tempfile::tempdir().unwrap();

        // No reservation at all: the sidecar cannot be replaced.
        let tight = dir.path().join("tight.obk");
        build(&tight, &f, b"exactly this", 0);
        let error = update_sealed_metadata(&tight, &f.chain, b"a much longer sidecar than before")
            .unwrap_err()
            .to_string();
        assert!(error.contains("reserved room"), "unexpected: {error}");

        // Reserved, but the new sidecar is too big for the reservation.
        let path = dir.path().join("full.obk");
        build(&path, &f, b"first", 64);
        assert!(update_sealed_metadata(&path, &f.chain, &[7u8; 65]).is_err());
        assert!(update_sealed_metadata(&path, &f.chain, &[7u8; 64]).is_ok());

        // Another chain's key opens nothing and changes nothing.
        let stranger = crate::seal::ChainKey::generate().unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(update_sealed_metadata(&path, &stranger, b"hostile").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn replacing_the_sidecar_twice_uses_a_new_nonce_every_time() {
        let f = Fixture::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("full.obk");
        build(&path, &f, b"first", 128);

        let mut seen = std::collections::HashSet::new();
        for round in 0..3u8 {
            update_sealed_metadata(&path, &f.chain, &[round; 100]).unwrap();
            assert_eq!(
                crate::seal::obk::read_open_header(&path)
                    .unwrap()
                    .header_generation,
                u64::from(round) + 1
            );
            let root = image_root(&path, &f.chain).unwrap();
            for chunk in &root.meta_chunks {
                assert!(seen.insert(chunk.id), "a record identity came back");
            }
            let mut reader = ObkRandomReader::open(&path, &f.chain).unwrap();
            assert_eq!(&reader.sealed_metadata().unwrap()[..], &[round; 100][..]);
        }
    }
}
