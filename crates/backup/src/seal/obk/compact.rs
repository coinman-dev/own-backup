//! Publish a validated sibling containing only current encrypted records.
//! Data ciphertext/identities remain unchanged; index and metadata get fresh
//! encryption sessions. Callers hold exclusive access as for consolidation.
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::OpenOptions;
use std::io::BufWriter;
use std::path::Path;

use base64::{engine::general_purpose::STANDARD, Engine};
use own_backup_common::Result;

use zeroize::Zeroizing;

use super::rebuild::RebuildIdentity;
use super::{bad, ClusterState, ObkCompression, ObkImage, ObkWriter, ObkWriterOptions};
use crate::qcow2::{decode_l2_entry, ClusterDescriptor, OFLAG_ZERO};
use crate::seal::{wrap::WrappedChainKey, ChainKey};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionStats {
    pub before_bytes: u64,
    pub after_bytes: u64,
    pub records_copied: u64,
    pub defined_clusters: u64,
}
impl CompactionStats {
    pub fn reclaimed_bytes(&self) -> u64 {
        self.before_bytes.saturating_sub(self.after_bytes)
    }
}

pub fn compact_image(path: &Path, chain: &ChainKey) -> Result<CompactionStats> {
    compact_image_with_progress(path, chain, |_, _| Ok(()))
}

/// `done/total` covers copying and full readback (two ticks per defined cluster).
/// Cancellation/errors preserve the original and its children. The sibling
/// starts empty with the original's permissions and receives only the live
/// records: peak free space is the compacted size, not the original's.
pub fn compact_image_with_progress(
    path: &Path,
    chain: &ChainKey,
    mut progress: impl FnMut(u64, u64) -> Result<()>,
) -> Result<CompactionStats> {
    build_sibling(
        path,
        chain,
        None,
        &mut |phase, done, total| match phase {
            SiblingPhase::Copy => progress(done, total * 2),
            SiblingPhase::ReadBack => progress(total + done, total * 2),
        },
        None,
    )
}

/// What a sibling rebuild is at, for [`build_sibling`]'s progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SiblingPhase {
    /// Records being written into the sibling: `done` of `total` clusters.
    Copy,
    /// Clusters the caller's check did not read, read back and authenticated.
    ReadBack,
}

/// A check of the finished sibling before it replaces the image: given the
/// sibling's path, it reads what it wants and returns the guest clusters it
/// has read through the authenticated reader. The rest is read back after it,
/// so every record of the sibling is read exactly once.
pub type SiblingCheck<'a> = &'a mut dyn FnMut(&Path) -> Result<HashSet<u64>>;

/// An increment to lay over the image while it is rebuilt — the fold and the
/// compaction of a consolidation in one pass ([`super::fold::fold_into_fresh_base`]).
pub(super) struct Overlay<'a> {
    pub child: &'a Path,
    /// The sidecar the folded image carries: the increment's, promoted.
    pub metadata: &'a [u8],
    pub identity: RebuildIdentity,
}

/// Where a record of the sibling comes from.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Origin {
    Image,
    Child,
}

/// Rebuild `path` into an empty sibling holding only the records its current
/// state needs, check the sibling, and publish it in the image's place.
///
/// With an `overlay` the state is the image's with the increment's clusters
/// over it: every record — the image's and the increment's alike — is moved
/// whole, as ciphertext under the same chain (FORMAT.md: a complete record may
/// be copied anywhere inside its chain without a new encryption). Nothing is
/// decompressed or re-encrypted, and the image itself is not written to: until
/// the rename it is exactly what it was.
pub(super) fn build_sibling(
    path: &Path,
    chain: &ChainKey,
    overlay: Option<Overlay<'_>>,
    progress: &mut dyn FnMut(SiblingPhase, u64, u64) -> Result<()>,
    check: Option<SiblingCheck<'_>>,
) -> Result<CompactionStats> {
    progress(SiblingPhase::Copy, 0, 0)?;
    let generation = ObkImage::open_path(path, chain)?
        .open_header()
        .header_generation
        .checked_add(1)
        .ok_or_else(|| bad("compaction generation overflow"))?;
    let before = std::fs::metadata(path)?.len();
    super::transaction::replace_fresh(path, chain, |stage| {
        let mut source = ObkImage::open_path(path, chain)?;
        let header = source.file_header().clone();
        let open = header.open.clone();
        if open.header_generation.checked_add(1) != Some(generation) {
            return Err(bad("image changed before compaction started"));
        }
        let mut child = overlay
            .as_ref()
            .map(|o| ObkImage::open_path(o.child, chain))
            .transpose()?;
        // What the sibling holds, cluster by cluster.
        let mut plan: BTreeMap<u64, (Origin, ClusterState)> = source
            .defined_cluster_states()?
            .into_iter()
            .map(|(cluster, state)| (cluster, (Origin::Image, state)))
            .collect();
        if let Some(child) = child.as_mut() {
            if header.backing_file.is_some() || open.parent_backup_id.is_some() {
                return Err(bad("only a full backup can absorb an increment"));
            }
            for (cluster, state) in child.defined_cluster_states()? {
                match state {
                    ClusterState::Present(_) => {
                        plan.insert(cluster, (Origin::Child, state));
                    }
                    // An increment's explicit zero over a full backup: the
                    // full backup without a cluster there reads zeros.
                    ClusterState::Zero => {
                        plan.remove(&cluster);
                    }
                    ClusterState::Absent => {
                        return Err(bad("the increment's index listed an absent cluster"))
                    }
                }
            }
        }
        let plan: Vec<(u64, Origin, ClusterState)> =
            plan.into_iter().map(|(c, (f, s))| (c, f, s)).collect();
        let metadata = match &overlay {
            Some(o) => Zeroizing::new(o.metadata.to_vec()),
            None => source.sealed_metadata()?,
        };
        let reserve = source
            .root()
            .meta_chunks
            .iter()
            .map(|c| {
                (c.len as usize).saturating_sub(
                    crate::seal::cluster::HEADER_LEN + crate::seal::cluster::TAG_LEN,
                )
            })
            .sum::<usize>()
            .max(metadata.len());
        let mut opts = ObkWriterOptions {
            chain_id: open.chain_id,
            backup_id: open.backup_id,
            parent_backup_id: open.parent_backup_id,
            role: open.role,
            created_at: open.created_at.clone(),
            virtual_size: open.virtual_size,
            cluster_bits: open.cluster_bits,
            compression: ObkCompression::None,
            backing_file: header.backing_file.clone(),
            key_file: open.key_file.clone(),
            wrapped_chain_key: WrappedChainKey::from_bytes(
                &STANDARD
                    .decode(&open.wrapped_chain_key)
                    .map_err(|e| bad(format!("wrapped key encoding: {e}")))?,
            )?,
            source_id: open.source_id.clone(),
            consolidated_increments: open.consolidated_increments,
            chain_started_at: open.chain_started_at.clone(),
        };
        // The one rebuild that changes who the image is: the base of a
        // consolidated chain becomes the increment it absorbed.
        if let Some(o) = &overlay {
            opts.backup_id = o.identity.backup_id;
            opts.created_at = o.identity.created_at.clone();
            opts.consolidated_increments = o.identity.consolidated_increments;
            opts.chain_started_at = o.identity.chain_started_at.clone();
        }
        let has_parent = opts.parent_backup_id.is_some();
        let file = OpenOptions::new().write(true).truncate(true).open(stage)?;
        let mut writer = ObkWriter::new(
            BufWriter::with_capacity(super::COPY_WRITE_BUFFER, file),
            chain,
            opts,
        )?;
        writer.set_generation(generation);
        let total = plan.len() as u64;
        // What the sibling's index must say once written.
        let mut expected: Vec<(u64, ClusterState)> = Vec::with_capacity(plan.len());
        let mut copied = 0;
        let batch = super::copy_batch_clusters(header.cluster_size);
        for (b, part) in plan.chunks(batch).enumerate() {
            // The batch's records, read file by file in the order they lie on
            // the disk — an original image and the folds appended to it are
            // runs in cluster order each — then written: see `COPY_BATCH_BYTES`.
            let mut at = Vec::new();
            for (i, &(cluster, from, state)) in part.iter().enumerate() {
                if matches!(state, ClusterState::Present(_)) {
                    let image = match from {
                        Origin::Image => &mut source,
                        Origin::Child => child
                            .as_mut()
                            .ok_or_else(|| bad("compaction plan names a missing increment"))?,
                    };
                    let offset =
                        match decode_l2_entry(image.raw_l2_entry(cluster)?, header.cluster_bits)? {
                            ClusterDescriptor::Compressed { host_offset, .. } => host_offset,
                            _ => return Err(bad("present data has no descriptor")),
                        };
                    at.push((from, offset, i));
                }
            }
            at.sort_unstable();
            let mut records: Vec<Option<crate::seal::cluster::SealedRecord>> =
                std::iter::repeat_with(|| None).take(part.len()).collect();
            for (from, _, i) in at {
                let image = match from {
                    Origin::Image => &mut source,
                    Origin::Child => child
                        .as_mut()
                        .ok_or_else(|| bad("compaction plan names a missing increment"))?,
                };
                records[i] = Some(image.authenticated_data_record(part[i].0)?);
            }
            for (i, &(cluster, _, state)) in part.iter().enumerate() {
                progress(SiblingPhase::Copy, (b * batch + i) as u64, total)?;
                match state {
                    ClusterState::Present(_) => {
                        let record = records[i]
                            .take()
                            .ok_or_else(|| bad("compaction batch lost a record"))?;
                        let id = record.id;
                        writer.relocate_record(cluster, record)?;
                        expected.push((cluster, ClusterState::Present(id)));
                        copied += 1;
                    }
                    ClusterState::Zero => {
                        if source.raw_l2_entry(cluster)? != OFLAG_ZERO {
                            return Err(bad("zero index/L2 disagree"));
                        }
                        writer.write_zero_cluster(cluster)?;
                        if has_parent {
                            expected.push((cluster, ClusterState::Zero));
                        }
                    }
                    ClusterState::Absent => {
                        return Err(bad("compaction index listed an absent cluster"))
                    }
                }
            }
        }
        writer
            .finish_with_reserve(&metadata, reserve)?
            .into_inner()
            .map_err(|e| bad(format!("cannot flush compacted image: {e}")))?
            .sync_all()?;
        let after = std::fs::metadata(stage)?.len();
        let mut output = ObkImage::open_path(stage, chain)?;
        if output.defined_cluster_states()? != expected || output.sealed_metadata()? != metadata {
            return Err(bad("compaction changed the index or metadata"));
        }
        let covered = match check {
            Some(check) => check(stage)?,
            None => HashSet::new(),
        };
        let rest: Vec<u64> = expected
            .iter()
            .map(|&(cluster, _)| cluster)
            .filter(|cluster| !covered.contains(cluster))
            .collect();
        let rest_total = rest.len() as u64;
        for (done, &cluster) in rest.iter().enumerate() {
            progress(SiblingPhase::ReadBack, done as u64, rest_total)?;
            if output.read_cluster(cluster)?.is_none() {
                return Err(bad("compacted data is absent"));
            }
        }
        drop(output);
        drop(child);
        drop(source); // close handles before Windows publication
        progress(SiblingPhase::ReadBack, rest_total, rest_total)?;
        Ok(CompactionStats {
            before_bytes: before,
            after_bytes: after,
            records_copied: copied,
            defined_clusters: expected.len() as u64,
        })
    })
}

/// A conservative bound on the bytes the image's current state needs.
fn live_upper_bound(
    image: &mut ObkImage<std::io::BufReader<std::fs::File>>,
    states: &[(u64, ClusterState)],
    file_len: u64,
) -> Result<u128> {
    let cs = image.cluster_size();
    let header = image.file_header().clone();
    let mut l2 = BTreeSet::new();
    let mut upper: u128 = super::DUAL_HEADER_DATA_START as u128;
    for (cluster, state) in states {
        l2.insert(cluster / (cs / 8));
        if matches!(state, ClusterState::Present(_)) {
            upper += record_len(image, *cluster)? as u128;
        }
    }
    upper += states.len() as u128 * super::index::INDEX_ENTRY_LEN as u128;
    upper += (states
        .len()
        .div_ceil(super::index::MAX_INDEX_ENTRIES_PER_CHUNK) as u128
        + 2)
        * 100;
    upper += image
        .root()
        .meta_chunks
        .iter()
        .map(|c| c.len as u128)
        .sum::<u128>()
        .max(image.root().sealed_metadata_len as u128);
    upper += crate::seal::cluster::MAX_PAYLOAD_LEN as u128;
    upper += (l2.len() as u128 + (header.l1_size as u64 * 8).div_ceil(cs) as u128 + 8) * cs as u128;
    upper += file_len.div_ceil(cs) as u128 * 16; // refcounts/tables: well over 2 B/host cluster
    Ok(upper)
}

/// The stored length of a present cluster's record.
fn record_len(
    image: &mut ObkImage<std::io::BufReader<std::fs::File>>,
    cluster: u64,
) -> Result<u64> {
    let bits = image.file_header().cluster_bits;
    match decode_l2_entry(image.raw_l2_entry(cluster)?, bits)? {
        ClusterDescriptor::Compressed { length, .. } => Ok(length),
        _ => Err(bad("compaction estimate: present data has no descriptor")),
    }
}

/// The maintenance rule: at least 64 MiB and a quarter of the file.
fn worth_rebuilding(savings: u128, file_len: u128) -> bool {
    savings >= 64 * 1024 * 1024 && savings * 4 >= file_len
}

/// A conservative live-size bound: maintain only if at least 64 MiB and a
/// quarter of the file can be reclaimed. Does not sum old per-pass counters.
pub fn compact_if_needed(path: &Path, chain: &ChainKey) -> Result<Option<CompactionStats>> {
    compact_if_needed_checked(path, chain, None)
}

/// The same, with `check` reading the compacted sibling before it replaces
/// the image (see [`SiblingCheck`]).
pub fn compact_if_needed_checked(
    path: &Path,
    chain: &ChainKey,
    check: Option<SiblingCheck<'_>>,
) -> Result<Option<CompactionStats>> {
    let mut image = ObkImage::open_path(path, chain)?;
    let states = image.defined_cluster_states()?;
    let file_len = std::fs::metadata(path)?.len();
    let upper = live_upper_bound(&mut image, &states, file_len)?;
    let savings = (file_len as u128).saturating_sub(upper);
    drop(image);
    drop(states);
    if !worth_rebuilding(savings, file_len as u128) {
        return Ok(None);
    }
    build_sibling(path, chain, None, &mut |_, _, _| Ok(()), check).map(Some)
}

/// Would folding `child` into `base` in place leave the base worth
/// compacting straight away? Then the fold is better done as one rebuild
/// ([`super::fold::fold_into_fresh_base`]): the increment is written once,
/// into the new file, instead of appended to the old one and copied again.
///
/// The same rule as [`compact_if_needed`], applied to the base as the fold
/// would leave it: the records it holds now that nothing would point at,
/// plus the ones the increment replaces, against the file grown by the
/// increment's records.
pub fn fold_leaves_base_worth_compacting(
    base: &Path,
    child: &Path,
    chain: &ChainKey,
) -> Result<bool> {
    let mut image = ObkImage::open_path(base, chain)?;
    let states = image.defined_cluster_states()?;
    let file_len = std::fs::metadata(base)?.len();
    let upper = live_upper_bound(&mut image, &states, file_len)?;
    let present: HashSet<u64> = states
        .iter()
        .filter(|(_, s)| matches!(s, ClusterState::Present(_)))
        .map(|&(c, _)| c)
        .collect();
    let mut increment = ObkImage::open_path(child, chain)?;
    let mut replaced: u128 = 0;
    let mut added: u128 = 0;
    for (cluster, state) in increment.defined_cluster_states()? {
        if present.contains(&cluster) {
            replaced += record_len(&mut image, cluster)? as u128;
        }
        if matches!(state, ClusterState::Present(_)) {
            added += record_len(&mut increment, cluster)? as u128;
        }
    }
    let grown = file_len as u128 + added;
    let savings = (file_len as u128).saturating_sub(upper) + replaced;
    Ok(worth_rebuilding(savings, grown))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seal::obk::{container_tests::Fixture, ObkChain, PreviousContents, RebuildOptions};
    use crate::sidecar::ChainRole;
    use std::io::{Seek, SeekFrom, Write};
    use uuid::Uuid;

    fn build(dir: &Path, f: &Fixture) -> (std::path::PathBuf, std::path::PathBuf) {
        let base = dir.join("base.obk");
        let child = dir.join("child.obk");
        let id = Uuid::new_v4();
        let mut writer =
            ObkWriter::create(&base, &f.chain, f.options(id, ChainRole::Full, None)).unwrap();
        writer.write_cluster(0, &[0x42; 4096]).unwrap();
        writer.write_cluster(2, &[0x73; 4096]).unwrap();
        writer.finish(b"base metadata").unwrap();
        let mut writer = ObkWriter::create(
            &child,
            &f.chain,
            f.options(
                Uuid::new_v4(),
                ChainRole::Incremental,
                Some((id, "base.obk")),
            ),
        )
        .unwrap();
        writer.write_zero_cluster(0).unwrap();
        writer.write_cluster(1, &[0x85; 4096]).unwrap();
        writer.finish_with_reserve(b"child metadata", 8192).unwrap();
        for step in 0..8 {
            super::super::apply_changed_with(
                &child,
                &f.chain,
                &[1],
                |_| Ok(vec![0x90 + step; 4096]),
                RebuildOptions {
                    previous: PreviousContents::Keep,
                    ..Default::default()
                },
            )
            .unwrap();
        }
        (base, child)
    }
    fn whole(path: &Path, f: &Fixture) -> Vec<u8> {
        let mut chain = ObkChain::open(path, &f.chain).unwrap();
        (0..64)
            .flat_map(|n| chain.read_cluster(n).unwrap().to_vec())
            .collect()
    }
    fn no_stage(dir: &Path) {
        assert!(!std::fs::read_dir(dir).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".own-backup-obk-")));
    }

    #[test]
    fn current_ciphertext_identity_metadata_and_child_bytes_survive_compaction() {
        let f = Fixture::new();
        let dir = tempfile::tempdir().unwrap();
        let (base, child) = build(dir.path(), &f);
        let before = whole(&child, &f);
        let mut image = ObkImage::open_path(&child, &f.chain).unwrap();
        let states = image.defined_cluster_states().unwrap();
        let meta = image.sealed_metadata().unwrap();
        let record = image.authenticated_data_record(1).unwrap();
        let header = image.open_header().clone();
        drop(image);
        let stats = compact_image(&child, &f.chain).unwrap();
        assert!(stats.reclaimed_bytes() > 0);
        assert_eq!(whole(&child, &f), before);
        let mut image = ObkImage::open_path(&child, &f.chain).unwrap();
        assert_eq!(image.defined_cluster_states().unwrap(), states);
        assert_eq!(image.sealed_metadata().unwrap(), meta);
        assert_eq!(
            image.authenticated_data_record(1).unwrap().bytes,
            record.bytes
        );
        assert_eq!(image.open_header().backup_id, header.backup_id);
        assert_eq!(
            image.open_header().parent_backup_id,
            header.parent_backup_id
        );
        assert_eq!(
            image.open_header().wrapped_chain_key,
            header.wrapped_chain_key
        );
        assert_eq!(
            image.open_header().header_generation,
            header.header_generation + 1
        );
        drop(image);
        compact_image(&base, &f.chain).unwrap();
        assert_eq!(whole(&child, &f), before);
        no_stage(dir.path());
    }

    #[test]
    fn cancelled_copy_or_readback_preserves_the_original_bytes() {
        let f = Fixture::new();
        let dir = tempfile::tempdir().unwrap();
        let (_, child) = build(dir.path(), &f);
        let before = std::fs::read(&child).unwrap();
        for cancel_at in [1, 3, 4] {
            let err = compact_image_with_progress(&child, &f.chain, |done, total| {
                if total > 0 && done >= cancel_at {
                    Err(own_backup_common::OwnBackupError::Cancelled)
                } else {
                    Ok(())
                }
            })
            .unwrap_err();
            assert!(matches!(err, own_backup_common::OwnBackupError::Cancelled));
            assert_eq!(std::fs::read(&child).unwrap(), before);
            no_stage(dir.path());
        }
    }

    #[test]
    fn wrong_key_or_corrupted_live_record_cannot_publish() {
        let f = Fixture::new();
        let dir = tempfile::tempdir().unwrap();
        let (_, child) = build(dir.path(), &f);
        let before = std::fs::read(&child).unwrap();
        assert!(compact_image(&child, &ChainKey::generate().unwrap()).is_err());
        assert_eq!(std::fs::read(&child).unwrap(), before);
        let mut image = ObkImage::open_path(&child, &f.chain).unwrap();
        let ClusterDescriptor::Compressed { host_offset, .. } = decode_l2_entry(
            image.raw_l2_entry(1).unwrap(),
            image.file_header().cluster_bits,
        )
        .unwrap() else {
            panic!()
        };
        drop(image);
        // Flip the byte: writing a fixed value left the image unchanged
        // whenever that byte already held it (1 run in 256).
        let at = host_offset + crate::seal::cluster::HEADER_LEN as u64;
        let mut file = OpenOptions::new().write(true).open(&child).unwrap();
        file.seek(SeekFrom::Start(at)).unwrap();
        file.write_all(&[!before[at as usize]]).unwrap();
        drop(file);
        let corrupt = std::fs::read(&child).unwrap();
        assert_ne!(corrupt, before);
        assert!(compact_image(&child, &f.chain).is_err());
        assert_eq!(std::fs::read(&child).unwrap(), corrupt);
        no_stage(dir.path());
    }

    #[test]
    fn either_compacted_header_recovers_the_current_point() {
        let f = Fixture::new();
        let dir = tempfile::tempdir().unwrap();
        let (_, child) = build(dir.path(), &f);
        let before = whole(&child, &f);
        compact_image(&child, &f.chain).unwrap();
        let bytes = std::fs::read(&child).unwrap();
        for offset in [0, super::super::SECOND_HEADER_OFFSET] {
            std::fs::write(&child, &bytes).unwrap();
            let mut file = OpenOptions::new().write(true).open(&child).unwrap();
            file.seek(SeekFrom::Start(offset)).unwrap();
            file.write_all(&[0; 512]).unwrap();
            drop(file);
            assert_eq!(whole(&child, &f), before);
        }
    }

    #[cfg(unix)]
    #[test]
    fn permissions_and_symlink_are_preserved() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let f = Fixture::new();
        let dir = tempfile::tempdir().unwrap();
        let (_, child) = build(dir.path(), &f);
        std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o640)).unwrap();
        let link = dir.path().join("alias.obk");
        symlink(&child, &link).unwrap();
        let before = whole(&child, &f);
        compact_image(&link, &f.chain).unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(
            std::fs::metadata(&child).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(whole(&child, &f), before);
    }

    #[test]
    fn a_small_clean_image_is_not_rebuilt_automatically() {
        let f = Fixture::new();
        let dir = tempfile::tempdir().unwrap();
        let (base, _) = build(dir.path(), &f);
        let before = std::fs::read(&base).unwrap();
        assert_eq!(compact_if_needed(&base, &f.chain).unwrap(), None);
        assert_eq!(std::fs::read(&base).unwrap(), before);
    }

    #[test]
    fn obsolete_generations_trigger_automatic_cleanup() {
        let f = Fixture::new();
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base.obk");
        let mut w = ObkWriter::create(
            &base,
            &f.chain,
            f.options(Uuid::new_v4(), ChainRole::Full, None),
        )
        .unwrap();
        w.write_cluster(0, &[0x42; 4096]).unwrap();
        w.finish_with_reserve(b"metadata", 2 * 1024 * 1024).unwrap();
        for _ in 0..34 {
            super::super::update_sealed_metadata(&base, &f.chain, b"metadata").unwrap();
        }
        let before = whole(&base, &f);
        let stats = compact_if_needed(&base, &f.chain)
            .unwrap()
            .expect("large obsolete tail should be reclaimed");
        assert!(stats.reclaimed_bytes() >= 64 * 1024 * 1024);
        assert_eq!(whole(&base, &f), before);
        assert_eq!(compact_if_needed(&base, &f.chain).unwrap(), None);
    }

    #[test]
    fn legacy_image_compacts_into_a_readable_dual_header_image() {
        let f = Fixture::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.obk");
        let file = std::fs::File::create(&path).unwrap();
        let mut w = ObkWriter::new_legacy(
            file,
            &f.chain,
            f.options(Uuid::new_v4(), ChainRole::Full, None),
        )
        .unwrap();
        w.write_cluster(3, &[0x69; 4096]).unwrap();
        w.finish(b"legacy metadata").unwrap();
        let before = whole(&path, &f);
        compact_image(&path, &f.chain).unwrap();
        assert_eq!(whole(&path, &f), before);
        assert!(
            ObkImage::open_path(&path, &f.chain)
                .unwrap()
                .open_header()
                .dual_header
        );
    }

    #[test]
    fn process_exit_before_publication_preserves_the_original() {
        const CHILD: &str = "OWN_BACKUP_COMPACT_EXIT";
        if let Some(path) = std::env::var_os(CHILD) {
            let data = STANDARD
                .decode(std::env::var("OWN_BACKUP_COMPACT_KEY").unwrap())
                .unwrap();
            let chain = ChainKey::from_bytes(data.try_into().unwrap());
            compact_image_with_progress(Path::new(&path), &chain, |done, total| {
                if total > 0 && done == total {
                    std::process::exit(23)
                }
                Ok(())
            })
            .unwrap();
            panic!("child did not stop at the publication boundary");
        }
        let f = Fixture::new();
        let dir = tempfile::tempdir().unwrap();
        let (_, child) = build(dir.path(), &f);
        let before = std::fs::read(&child).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "seal::obk::compact::tests::process_exit_before_publication_preserves_the_original",
            ])
            .env(CHILD, &child)
            .env(
                "OWN_BACKUP_COMPACT_KEY",
                STANDARD.encode(f.chain.as_bytes()),
            )
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(23));
        assert_eq!(std::fs::read(&child).unwrap(), before);
        assert!(!whole(&child, &f).is_empty());
    }
}
