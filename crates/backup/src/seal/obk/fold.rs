//! Folding an increment into its base, inside the encrypted container.
//!
//! This is the sliding increment window ([`crate::consolidate`]) for `.obk`.
//! The plaintext containers each have one move that does it — `MergeVirtualDisk`
//! for VHDX, cluster grafting for qcow2 — and neither can be used here: both
//! read and write cluster payloads, and in an encrypted image a cluster payload
//! is a sealed record that belongs to exactly one image's authenticated index.
//!
//! So the fold is a rebuild ([`super::rebuild`]): every cluster the increment
//! defines is read through its own index, re-sealed under a fresh nonce, and
//! appended to the base, whose index, sidecar and identity are re-emitted to
//! match. The base comes out a full backup of the increment's moment in time —
//! the same thing consolidation means for a plaintext chain.
//!
//! # Why nothing is erased
//!
//! The rebuild is asked for [`PreviousContents::Keep`]. Consolidation rewrites
//! the image the rest of the chain hangs off. New images append records and
//! publish a checked alternate header slot; legacy images stage the edit in a
//! sibling file. Both preserve the previous point through a torn header write.
//!
//! The clusters the base used to hold at those positions therefore stay in the
//! file, unreferenced. They are ciphertext under the chain key and no index
//! names them, which is the same bargain the plaintext fold makes with its own
//! orphaned clusters — this is retention, not exclusion, and §4's rule about
//! removed data not surviving a pass is about the latter.
//!
//! # What identifies the base afterwards
//!
//! The base takes the increment's `backup_id`. That is not bookkeeping: the
//! chain's links are identities, not file names ([`super::reader::ObkChain`]
//! refuses a backing file whose `backup_id` is not the one the child was
//! written against), so the increment *after* the folded one goes on opening
//! without its recorded parent id changing at all. Only the backing **name**
//! has to move, which is [`set_backing_file`].

use std::path::Path;

use own_backup_common::Result;
use uuid::Uuid;

use super::header::{finish_dual_header, obk_extensions};
use super::reader::ObkImage;
use super::rebuild::{apply_changed_with, PreviousContents, RebuildIdentity, RebuildOptions};
use super::{bad, INCOMPAT_OBK_SEAL};
use crate::qcow2::layout::{build_header_cluster, HeaderParams, TablesInfo};
use crate::qcow2::{COMPRESSION_TYPE_ZSTD, INCOMPAT_COMPRESSION_TYPE};
use crate::seal::ChainKey;
use crate::sidecar::ChainRole;

/// What one fold moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FoldStats {
    /// Guest clusters the increment defined, all of them now in the base.
    pub clusters: u64,
    /// Bytes the fold left inside the base with nothing pointing at them.
    pub bytes_orphaned: u64,
    /// The base already carried this increment's identity when the fold
    /// started — a previous attempt had finished it and was interrupted before
    /// it could say so. The fold was run again, which changes nothing.
    pub already_applied: bool,
}

/// Fold `victim` into `base`, giving the base `metadata` as its sidecar and
/// `identity` as its own. The increment's clusters are re-encoded at
/// `zstd_level` — the caller passes the level the increment was written with,
/// which is what a qcow2 fold keeps too (it copies the compressed bytes as
/// they are).
///
/// `metadata` is the increment's sidecar promoted to a full backup's — the
/// caller builds it, because what a sidecar means is the backup layer's
/// business and not the container's. It is written in the same pass as the
/// clusters, since in an encrypted image the sidecar lives inside the container
/// and a rebuild is the only write that can replace it wholesale.
pub fn fold_child_into_base(
    base: &Path,
    victim: &Path,
    chain: &ChainKey,
    metadata: &[u8],
    identity: RebuildIdentity,
    zstd_level: i32,
) -> Result<FoldStats> {
    let mut child = ObkImage::open_path(victim, chain)?;
    let already_applied = {
        let parent = ObkImage::open_path(base, chain)?;
        check_fold(parent.root(), child.root())?
    };

    // Everything the increment defines — data and the explicit zeros that say
    // "this cluster was emptied since the base was taken". Both have to land in
    // the base, and the zeros land as *nothing*: a full backup with no cluster
    // there reads zeros, which is exactly what the increment meant.
    let clusters: Vec<u64> = child
        .defined_cluster_states()?
        .into_iter()
        .map(|(cluster, _)| cluster)
        .collect();

    let stats = apply_changed_with(
        base,
        chain,
        &clusters,
        move |cluster| {
            // Deliberately the increment's OWN view, not its chain's: a cluster
            // it does not define is the base's already, and reading it through
            // the chain would copy the base onto itself.
            match child.read_cluster(cluster)? {
                Some(data) => Ok(data.to_vec()),
                None => Err(bad(format!(
                    "increment stopped defining cluster {cluster} half-way through the fold"
                ))),
            }
        },
        RebuildOptions {
            metadata: Some(metadata),
            backing_file: None,
            identity: Some(identity),
            previous: PreviousContents::Keep,
            zstd_level: Some(zstd_level),
        },
    )?;

    Ok(FoldStats {
        clusters: clusters.len() as u64,
        bytes_orphaned: stats.bytes_orphaned,
        already_applied,
    })
}

/// Is this pair foldable, and has the fold already happened?
///
/// The second question is the crash path, and here it has an exact answer
/// rather than a heuristic: a fold gives the base the increment's `backup_id`,
/// so a base already carrying it is a base that already absorbed this
/// increment. (The VHDX arm has to compare block contents for the same answer,
/// because a merge there leaves no such mark.) Running the fold again on top is
/// harmless — it rewrites the same clusters with the same contents — so this is
/// reported rather than refused.
fn check_fold(base: &super::ObkRoot, victim: &super::ObkRoot) -> Result<bool> {
    if base.chain_id != victim.chain_id {
        return Err(bad("these two images belong to different chains"));
    }
    if base.role != ChainRole::Full {
        return Err(bad("only a full backup can absorb an increment"));
    }
    if victim.role != ChainRole::Incremental {
        // A differential's siblings all sit on the base, so moving the base
        // forward would silently rebase them onto a moment they were never
        // computed against. `chains::plan_consolidation` refuses such a chain
        // too; this is the container saying the same thing.
        return Err(bad(
            "only an incremental image is ever folded into its base",
        ));
    }
    if base.cluster_bits != victim.cluster_bits || base.virtual_size != victim.virtual_size {
        return Err(bad("the two images have different geometry"));
    }
    if base.backup_id == victim.backup_id {
        return Ok(true);
    }
    if victim.parent_backup_id != Some(base.backup_id) {
        return Err(bad(
            "this increment was not written against this base — refusing to fold it in",
        ));
    }
    Ok(false)
}

/// Point an image's backing file at another name, without touching anything
/// else in it.
///
/// Only the header cluster changes, and only its backing-file name: the sealed
/// root keeps naming the same parent **identity**, which after a fold is the
/// identity the base has inherited. That is why repointing an encrypted chain
/// needs no rebuild — the link was never the file name.
pub fn set_backing_file(path: &Path, chain: &ChainKey, name: &str) -> Result<()> {
    if ObkImage::open_path(path, chain)?.open_header().dual_header {
        return set_backing_file_in_place(path, chain, name);
    }
    super::transaction::replace(path, chain, |stage| {
        set_backing_file_in_place(stage, chain, name)
    })
}

/// Publish the next increment's sidecar and backing name together. The
/// consolidation marker can recover either operation, but one publication
/// avoids an intermediate mismatch and a second copy of the entire image.
pub fn repoint_with_metadata(
    path: &Path,
    chain: &ChainKey,
    name: &str,
    metadata: &[u8],
) -> Result<()> {
    if ObkImage::open_path(path, chain)?.open_header().dual_header {
        return super::rebuild::apply_changed_with(
            path,
            chain,
            &[],
            |_| unreachable!("no clusters were requested"),
            RebuildOptions {
                metadata: Some(metadata),
                backing_file: Some(name),
                previous: PreviousContents::Keep,
                ..Default::default()
            },
        )
        .map(|_| ());
    }
    super::transaction::replace(path, chain, |stage| {
        super::metadata::update_sealed_metadata_in_place(stage, chain, metadata)?;
        set_backing_file_in_place(stage, chain, name)
    })
}

fn set_backing_file_in_place(path: &Path, chain: &ChainKey, name: &str) -> Result<()> {
    use std::io::{Seek, SeekFrom, Write};

    // Opening with the key proves this is our image and that its two halves
    // still agree, before its header is rewritten from what they say.
    let header = ObkImage::open_path(path, chain)?.file_header().clone();
    if header.backing_file.is_none() {
        return Err(bad(format!(
            "{}: this image has no backing file to repoint",
            path.display()
        )));
    }
    let mut open = header.open.clone();
    if open.dual_header {
        open.header_generation = open
            .header_generation
            .checked_add(1)
            .ok_or_else(|| bad("header generation overflow"))?;
    }
    let cluster = build_header_cluster(
        HeaderParams {
            cluster_bits: header.cluster_bits,
            virtual_size: header.virtual_size,
            incompatible_features: INCOMPAT_OBK_SEAL | INCOMPAT_COMPRESSION_TYPE,
            compression_type: COMPRESSION_TYPE_ZSTD,
            backing_file: Some(name),
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
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| bad(format!("cannot open {} for writing: {e}", path.display())))?;
    file.seek(SeekFrom::Start(
        if open.dual_header && header.open.header_generation.is_multiple_of(2) {
            super::SECOND_HEADER_OFFSET
        } else {
            0
        },
    ))?;
    file.write_all(&cluster)?;
    file.sync_all()?;
    Ok(())
}

/// The identity an image carries, for a caller planning a fold.
pub fn image_identity(path: &Path, chain: &ChainKey) -> Result<(Uuid, Option<Uuid>, ChainRole)> {
    let image = ObkImage::open_path(path, chain)?;
    let root = image.root();
    Ok((root.backup_id, root.parent_backup_id, root.role))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use crate::qcow2::DEFAULT_ZSTD_LEVEL;
    use crate::seal::obk::container_tests::{cluster_bits, Fixture};
    use crate::seal::obk::{ObkChain, ObkRandomReader, ObkWriter};

    fn cluster_of(fill: u8) -> Vec<u8> {
        vec![fill; 1 << cluster_bits()]
    }

    fn identity(backup_id: Uuid, folded: u32) -> RebuildIdentity {
        RebuildIdentity {
            backup_id,
            created_at: "2026-01-02T00:00:00Z".into(),
            consolidated_increments: folded,
            chain_started_at: Some("2026-01-01T00:00:00Z".into()),
        }
    }

    #[test]
    fn a_sparse_eight_tib_chain_folds_only_its_defined_clusters() {
        let f = Fixture::new();
        let dir = tempfile::tempdir().unwrap();
        let virtual_size = 8u64 << 40;
        let high = virtual_size / (1 << 16) - 1;
        let base_id = Uuid::new_v4();
        let child_id = Uuid::new_v4();
        let base = dir.path().join("full.obk");
        let child = dir.path().join("inc.obk");

        let mut base_options = f.options(base_id, ChainRole::Full, None);
        base_options.cluster_bits = 16;
        base_options.virtual_size = virtual_size;
        let mut writer = ObkWriter::create(&base, &f.chain, base_options).unwrap();
        writer.write_cluster(0, &vec![0x11; 1 << 16]).unwrap();
        writer.finish(b"base metadata").unwrap();

        let mut child_options = f.options(
            child_id,
            ChainRole::Incremental,
            Some((base_id, "full.obk")),
        );
        child_options.cluster_bits = 16;
        child_options.virtual_size = virtual_size;
        let mut writer = ObkWriter::create(&child, &f.chain, child_options).unwrap();
        writer.write_cluster(high, &vec![0x22; 1 << 16]).unwrap();
        writer.finish(b"child metadata").unwrap();

        let stats = fold_child_into_base(
            &base,
            &child,
            &f.chain,
            b"promoted metadata",
            identity(child_id, 1),
            DEFAULT_ZSTD_LEVEL,
        )
        .unwrap();
        assert_eq!(stats.clusters, 1);
        let mut reader = ObkRandomReader::open(&base, &f.chain).unwrap();
        let mut data = vec![0u8; 1 << 16];
        reader.read_at(high * (1 << 16), &mut data).unwrap();
        assert_eq!(data, vec![0x22; 1 << 16]);
        assert!(std::fs::metadata(&base).unwrap().len() < 10 << 20);
    }

    /// base ← inc1 ← inc2, the shape a window step is planned against.
    fn chain_of_three(dir: &Path, f: &Fixture) -> (PathBuf, PathBuf, PathBuf, Uuid, Uuid) {
        let base_id = Uuid::new_v4();
        let inc1_id = Uuid::new_v4();
        let inc2_id = Uuid::new_v4();

        let base = dir.join("full.obk");
        let mut w =
            ObkWriter::create(&base, &f.chain, f.options(base_id, ChainRole::Full, None)).unwrap();
        w.write_cluster(0, &cluster_of(0x11)).unwrap();
        w.write_cluster(1, &cluster_of(0x11)).unwrap();
        w.write_cluster(2, &cluster_of(0x11)).unwrap();
        w.finish_with_reserve(b"sidecar of the full backup", 4096)
            .unwrap();

        let inc1 = dir.join("inc1.obk");
        let mut w = ObkWriter::create(
            &inc1,
            &f.chain,
            f.options(inc1_id, ChainRole::Incremental, Some((base_id, "full.obk"))),
        )
        .unwrap();
        w.write_cluster(1, &cluster_of(0x22)).unwrap();
        w.write_zero_cluster(2).unwrap();
        w.finish_with_reserve(b"sidecar of increment 1", 4096)
            .unwrap();

        let inc2 = dir.join("inc2.obk");
        let mut w = ObkWriter::create(
            &inc2,
            &f.chain,
            f.options(inc2_id, ChainRole::Incremental, Some((inc1_id, "inc1.obk"))),
        )
        .unwrap();
        w.write_cluster(3, &cluster_of(0x33)).unwrap();
        w.finish_with_reserve(b"sidecar of increment 2", 4096)
            .unwrap();

        (base, inc1, inc2, base_id, inc1_id)
    }

    /// The acceptance criterion for consolidation, in the form CLAUDE.md states
    /// it: the surviving increment reads byte for byte what it read before.
    /// Everything else — identities, links, sidecars — exists to make that true.
    #[test]
    fn folding_an_increment_leaves_the_leaf_reading_exactly_what_it_did() {
        let f = Fixture::new();
        let dir = tempfile::tempdir().unwrap();
        let (base, inc1, inc2, _, inc1_id) = chain_of_three(dir.path(), &f);

        let before: Vec<Vec<u8>> = {
            let mut chain = ObkChain::open(&inc2, &f.chain).unwrap();
            (0..4)
                .map(|c| chain.read_cluster(c).unwrap().to_vec())
                .collect()
        };

        let stats = fold_child_into_base(
            &base,
            &inc1,
            &f.chain,
            b"sidecar of increment 1, promoted to a full",
            identity(inc1_id, 1),
            DEFAULT_ZSTD_LEVEL,
        )
        .unwrap();
        assert!(!stats.already_applied);
        assert_eq!(stats.clusters, 2, "one changed cluster and one emptied one");

        // Repoint the leaf and delete the folded increment, as a step does.
        repoint_with_metadata(&inc2, &f.chain, "full.obk", b"repointed leaf sidecar").unwrap();
        std::fs::remove_file(&inc1).unwrap();

        let mut chain = ObkChain::open(&inc2, &f.chain).unwrap();
        assert_eq!(chain.chain_len(), 2, "the chain is one image shorter");
        assert_eq!(
            &chain.sealed_metadata().unwrap()[..],
            b"repointed leaf sidecar"
        );
        let after: Vec<Vec<u8>> = (0..4)
            .map(|c| chain.read_cluster(c).unwrap().to_vec())
            .collect();
        assert_eq!(
            after, before,
            "the leaf must read exactly what it read before"
        );

        // And the base is now a standalone full backup of the increment's moment.
        let mut reader = ObkRandomReader::open(&base, &f.chain).unwrap();
        assert_eq!(
            &reader.sealed_metadata().unwrap()[..],
            b"sidecar of increment 1, promoted to a full"
        );
        let (backup_id, parent, role) = image_identity(&base, &f.chain).unwrap();
        assert_eq!(backup_id, inc1_id, "the base IS that point in time now");
        assert_eq!(parent, None);
        assert_eq!(role, ChainRole::Full);
        let open = super::super::read_open_header(&base).unwrap();
        assert_eq!(open.backup_id, inc1_id);
        assert_eq!(open.consolidated_increments, 1);
        assert_eq!(
            open.chain_started_at.as_deref(),
            Some("2026-01-01T00:00:00Z")
        );
    }

    /// A fold must not damage the base if it is interrupted, so it may not
    /// change a byte of it before the header cluster that commits the whole
    /// pass. This is that property, checked on the file rather than argued.
    #[test]
    fn a_fold_only_appends_and_the_previous_image_survives_it_byte_for_byte() {
        let f = Fixture::new();
        let dir = tempfile::tempdir().unwrap();
        let (base, inc1, _, _, inc1_id) = chain_of_three(dir.path(), &f);

        let before = std::fs::read(&base).unwrap();
        fold_child_into_base(
            &base,
            &inc1,
            &f.chain,
            b"promoted",
            identity(inc1_id, 1),
            DEFAULT_ZSTD_LEVEL,
        )
        .unwrap();
        let after = std::fs::read(&base).unwrap();

        assert!(after.len() > before.len(), "a fold only ever appends");
        let data = super::super::DUAL_HEADER_DATA_START as usize;
        assert!(after[..1usize << cluster_bits()] == before[..1usize << cluster_bits()]);
        assert!(after[data..before.len()] == before[data..]);
    }

    /// The crash between a finished fold and the record of it: the step is run
    /// again, recognises its own work and does it once more, harmlessly.
    #[test]
    fn a_finished_fold_is_recognised_and_repeating_it_changes_nothing() {
        let f = Fixture::new();
        let dir = tempfile::tempdir().unwrap();
        let (base, inc1, _, _, inc1_id) = chain_of_three(dir.path(), &f);

        fold_child_into_base(
            &base,
            &inc1,
            &f.chain,
            b"promoted",
            identity(inc1_id, 1),
            DEFAULT_ZSTD_LEVEL,
        )
        .unwrap();
        let contents: Vec<Vec<u8>> = {
            let mut r = ObkRandomReader::open(&base, &f.chain).unwrap();
            (0..4)
                .map(|c| {
                    let mut buf = vec![0u8; 1 << cluster_bits()];
                    r.read_at(c * (1u64 << cluster_bits()), &mut buf).unwrap();
                    buf
                })
                .collect()
        };

        let again = fold_child_into_base(
            &base,
            &inc1,
            &f.chain,
            b"promoted",
            identity(inc1_id, 1),
            DEFAULT_ZSTD_LEVEL,
        )
        .unwrap();
        assert!(
            again.already_applied,
            "the fold must recognise its own work"
        );

        let mut r = ObkRandomReader::open(&base, &f.chain).unwrap();
        for (c, expected) in contents.iter().enumerate() {
            let mut buf = vec![0u8; 1 << cluster_bits()];
            r.read_at(c as u64 * (1u64 << cluster_bits()), &mut buf)
                .unwrap();
            assert_eq!(&buf, expected, "cluster {c} changed on a repeated fold");
        }
    }

    /// What may not be folded, and what may not do the folding.
    #[test]
    fn the_wrong_pair_and_the_wrong_key_are_both_refused() {
        let f = Fixture::new();
        let dir = tempfile::tempdir().unwrap();
        let (base, inc1, inc2, _, inc1_id) = chain_of_three(dir.path(), &f);

        // inc2 sits on inc1, not on the base.
        let e = fold_child_into_base(
            &base,
            &inc2,
            &f.chain,
            b"x",
            identity(inc1_id, 1),
            DEFAULT_ZSTD_LEVEL,
        )
        .expect_err("a grandchild is not foldable into the base");
        assert!(
            e.to_string().contains("not written against this base"),
            "{e}"
        );

        // The base is not an increment, and an increment is not a base.
        assert!(fold_child_into_base(
            &inc1,
            &base,
            &f.chain,
            b"x",
            identity(inc1_id, 1),
            DEFAULT_ZSTD_LEVEL
        )
        .is_err());

        // Another chain's key opens neither image and changes nothing.
        let stranger = ChainKey::generate().unwrap();
        let untouched = std::fs::read(&base).unwrap();
        assert!(fold_child_into_base(
            &base,
            &inc1,
            &stranger,
            b"x",
            identity(inc1_id, 1),
            DEFAULT_ZSTD_LEVEL
        )
        .is_err());
        assert!(set_backing_file(&inc2, &stranger, "full.obk").is_err());
        assert_eq!(std::fs::read(&base).unwrap(), untouched);
    }

    /// An image with no parent has no backing name to move.
    #[test]
    fn repointing_a_full_backup_is_refused() {
        let f = Fixture::new();
        let dir = tempfile::tempdir().unwrap();
        let (base, _, _, _, _) = chain_of_three(dir.path(), &f);
        assert!(set_backing_file(&base, &f.chain, "somewhere.obk").is_err());
    }
}
