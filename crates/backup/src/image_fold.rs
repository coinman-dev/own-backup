//! Changing a finished image through a scratch overlay, and folding the change
//! back in userspace.
//!
//! The removal of excluded names from an image (Linux: `ntfs_names_linux`)
//! lets a filesystem driver write, but never into the image itself: the image
//! is served through a [`WritableOverlaySource`] whose scratch takes every
//! write, and the image is touched only once the pass has finished well. Then
//! the touched clusters go back by the container's own rules:
//!
//! - **qcow2** — [`Qcow2InPlaceMerge`]: new clusters appended, L2 entries
//!   repointed, the header last. A child of a chain gets whole clusters read
//!   through the chain, so the parent's bytes in a cluster the driver touched
//!   only in part are seeded, never lost;
//! - **OBK** — [`crate::seal::obk::apply_changed_with`]: new records, the old
//!   ones zeroed, index and tail re-emitted. The scratch is sealed
//!   ([`SealedSlots`]): §7.4 allows no readable scratch beside an encrypted
//!   copy;
//! - **VHDX** — [`VhdxWriter::open_existing`]: blocks written in place, a new
//!   block in a child seeded from its parent (the writer's own rule).
//!
//! Same contract as the Windows passes: a failure before the fold leaves the
//! image exactly as checksummed and is answered `Ok(None)`; a failure during
//! or after the fold is an error the caller turns into a failed run — an image
//! that no longer matches its checksums must never ship. The image is still
//! the run's private stage at this point, so a crash mid-fold publishes
//! nothing.

// The fold is Linux's; the Windows passes share only the re-checksum.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tracing::{info, warn};

use own_backup_common::types::ImageFormat;
use own_backup_common::Result;

use crate::block_source::{BlockSource, WritableBlockSource};
use crate::checksum::compute_block_checksum;
use crate::engine_core::platform::{FinishedImage, VolumeSpan};
use crate::overlay::{PlainSlots, SealedSlots, SlotStore, WritableOverlaySource};
use crate::qcow2::{Qcow2InPlaceMerge, Qcow2RandomReader};
use crate::seal::ChainKey;
use crate::sidecar::BlockChecksum;
use crate::vhdx_format::VhdxWriter;

/// The scratch granularity over a VHDX. Its blocks are a MiB or more, far
/// coarser than what a deletion touches (MFT records, index blocks, bitmap
/// words); the qcow2 default cluster keeps the scratch and the fold small.
const VHDX_SCRATCH_CLUSTER: u64 = 64 * 1024;

/// A scratch overlay, whatever store its slots are kept in.
pub(crate) trait ScratchOverlay: WritableBlockSource {
    fn cluster_size(&self) -> u64;
    /// Clusters written at least once, ascending.
    fn touched_clusters(&self) -> Vec<u64>;
    /// How many clusters were written at least once.
    fn touched_count(&self) -> u64;
    /// The final content of `cluster`, a whole cluster long.
    fn read_cluster(&mut self, cluster: u64) -> Result<Vec<u8>>;
}

impl<S: SlotStore> ScratchOverlay for WritableOverlaySource<S> {
    fn cluster_size(&self) -> u64 {
        WritableOverlaySource::cluster_size(self)
    }

    fn touched_clusters(&self) -> Vec<u64> {
        WritableOverlaySource::touched_clusters(self)
    }

    fn touched_count(&self) -> u64 {
        WritableOverlaySource::touched_count(self)
    }

    fn read_cluster(&mut self, cluster: u64) -> Result<Vec<u8>> {
        WritableOverlaySource::read_cluster(self, cluster)
    }
}

/// The scratch as the pass gets it: shared with whatever serves it to the
/// driver, and read back by the fold afterwards.
pub(crate) type SharedScratch = Arc<Mutex<dyn ScratchOverlay>>;

/// A window `[offset, offset + len)` of a shared scratch, as one device: what
/// a volume of a disk image is served as.
pub(crate) struct ScratchWindow {
    scratch: SharedScratch,
    offset: u64,
    len: u64,
}

impl ScratchWindow {
    pub(crate) fn new(scratch: SharedScratch, offset: u64, len: u64) -> Self {
        Self {
            scratch,
            offset,
            len,
        }
    }

    fn check(&self, offset: u64, len: usize) -> Result<()> {
        if offset
            .checked_add(len as u64)
            .is_none_or(|end| end > self.len)
        {
            return Err(own_backup_common::OwnBackupError::Device(format!(
                "access at {offset}+{len} past the end of a {}-byte window",
                self.len
            )));
        }
        Ok(())
    }
}

impl BlockSource for ScratchWindow {
    fn size(&self) -> u64 {
        self.len
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.check(offset, buf.len())?;
        self.scratch
            .lock()
            .unwrap()
            .read_at(self.offset + offset, buf)
    }
}

impl WritableBlockSource for ScratchWindow {
    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        self.check(offset, data.len())?;
        self.scratch
            .lock()
            .unwrap()
            .write_at(self.offset + offset, data)
    }
}

/// What a pass that changed the image reported, and what the fold did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Folded {
    pub(crate) entries_deleted: u64,
    pub(crate) entries_failed: u64,
    pub(crate) clusters: u64,
    pub(crate) blocks_rehashed: u64,
}

/// The pass itself: change what it is handed, and say how many entries it
/// deleted and how many it could not. An error means nothing it wrote counts.
pub(crate) type ScratchPass<'a> =
    &'a mut dyn FnMut(SharedScratch, &mut dyn FnMut(u64, u64)) -> Result<(u64, u64)>;

/// How the image is read, and how a change goes back into it.
enum Container {
    Vhdx,
    Qcow2 { zstd_level: i32 },
    Obk { key: ChainKey, zstd_level: i32 },
}

impl Container {
    fn of(image: &FinishedImage<'_>) -> Result<Self> {
        let zstd_level = crate::qcow2::zstd_level_or_default(image.zstd_level);
        Ok(match (image.format, image.encryption) {
            (ImageFormat::Vhdx, _) => Container::Vhdx,
            (ImageFormat::Qcow2, None) => Container::Qcow2 { zstd_level },
            (ImageFormat::Qcow2, Some(keys)) => Container::Obk {
                key: keys.open_chain_of(image.path)?.key,
                zstd_level,
            },
        })
    }

    /// A reader of the image as it reads now, its parents included.
    fn reader(&self, path: &Path) -> Result<Box<dyn BlockSource>> {
        Ok(match self {
            Container::Vhdx => Box::new(VhdxSource(crate::vhdx_bat::VhdxGuestReader::open(path)?)),
            Container::Qcow2 { .. } => Box::new(Qcow2RandomReader::open(path)?),
            Container::Obk { key, .. } => {
                Box::new(crate::seal::obk::ObkRandomReader::open(path, key)?)
            }
        })
    }
}

/// [`crate::vhdx_bat::VhdxGuestReader`] (chain-aware) as a block source.
struct VhdxSource(crate::vhdx_bat::VhdxGuestReader);

impl BlockSource for VhdxSource {
    fn size(&self) -> u64 {
        self.0.virtual_size()
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.0.read_at(offset, buf)
    }
}

/// Beside the image, under the name the Windows passes use: one pass at a time
/// per image, and the run's cleanup knows it.
fn scratch_path(image_path: &Path) -> PathBuf {
    let mut name = image_path.file_name().unwrap_or_default().to_os_string();
    name.push(".excl.scratch");
    image_path.with_file_name(name)
}

fn create_scratch(path: &Path) -> Result<std::fs::File> {
    let _ = std::fs::remove_file(path);
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create_new(true);
    // What a plain image's scratch holds is the image's own metadata: nobody
    // but the service reads it.
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    Ok(options.open(path)?)
}

/// Serve the finished `image` through a scratch, let `pass` change it, and fold
/// what it changed back into the image; then re-checksum the blocks of `grid`
/// (the volume window a per-volume image's checksums cover, or the whole image
/// of a disk) that the change reached.
///
/// `Ok(None)`: the image is untouched — the pass failed (logged here) or
/// deleted nothing. `Err`: the fold or the re-checksum failed, and the image
/// may no longer match its checksums.
pub(crate) fn change_through_scratch(
    image: &FinishedImage<'_>,
    grid: VolumeSpan,
    checksums: &mut Vec<BlockChecksum>,
    tick: &mut dyn FnMut(u64, u64),
    pass: ScratchPass<'_>,
) -> Result<Option<Folded>> {
    let t0 = Instant::now();
    let container = Container::of(image)?;
    let base = container.reader(image.path)?;
    let scratch_file_path = scratch_path(image.path);
    let file = create_scratch(&scratch_file_path)?;
    let scratch: SharedScratch = match &container {
        Container::Vhdx => Arc::new(Mutex::new(WritableOverlaySource::with_store(
            base,
            VHDX_SCRATCH_CLUSTER,
            PlainSlots::new(file, VHDX_SCRATCH_CLUSTER),
        ))),
        Container::Qcow2 { .. } => {
            let cluster = Qcow2RandomReader::open(image.path)?.cluster_size();
            Arc::new(Mutex::new(WritableOverlaySource::with_store(
                base,
                cluster,
                PlainSlots::new(file, cluster),
            )))
        }
        Container::Obk { key, .. } => {
            let cluster = crate::seal::obk::ObkRandomReader::open(image.path, key)?.cluster_size();
            let header = crate::seal::obk::read_open_header(image.path)?;
            let store = SealedSlots::new(file, cluster, key, *header.chain_id.as_bytes())?;
            Arc::new(Mutex::new(WritableOverlaySource::with_store(
                base, cluster, store,
            )))
        }
    };
    // The scratch file goes whatever happens below.
    let _cleanup = RemoveOnDrop(scratch_file_path);

    let (deleted, failed) = match pass(scratch.clone(), tick) {
        Ok(counts) => counts,
        Err(e) => {
            warn!(image = %image.path.display(), error = %e,
                  "changing the image failed — it is left exactly as written");
            return Ok(None);
        }
    };
    let (touched, cluster_size) = {
        let s = scratch.lock().unwrap();
        (s.touched_clusters(), s.cluster_size())
    };
    if deleted == 0 || touched.is_empty() {
        warn!(
            image = %image.path.display(),
            deleted,
            failed,
            clusters = touched.len(),
            "nothing was deleted — the image is left exactly as written"
        );
        return Ok(None);
    }

    let t_fold = Instant::now();
    let read_cluster = |c: u64| scratch.lock().unwrap().read_cluster(c);
    match &container {
        Container::Vhdx => {
            let mut writer = VhdxWriter::open_existing(image.path)?;
            let size = writer.length();
            for &c in &touched {
                let data = read_cluster(c)?;
                let at = c * cluster_size;
                let len = cluster_size.min(size - at) as usize;
                writer.write_at(at, &data[..len])?;
            }
            // The payload before the BAT that points at it.
            writer.flush_payload()?;
            writer.finish()?;
        }
        Container::Qcow2 { zstd_level } => {
            let mut merge = Qcow2InPlaceMerge::open(image.path, Some(*zstd_level))?;
            for &c in &touched {
                merge.write_cluster(c, &read_cluster(c)?)?;
            }
            merge.commit()?;
        }
        Container::Obk { key, zstd_level } => {
            crate::seal::obk::apply_changed_with(
                image.path,
                key,
                &touched,
                read_cluster,
                crate::seal::obk::RebuildOptions {
                    zstd_level: Some(*zstd_level),
                    ..Default::default()
                },
            )?;
        }
    }
    let fold_ms = t_fold.elapsed().as_millis();

    let mut reader = container.reader(image.path)?;
    let blocks_rehashed = rehash_touched_through(
        reader.as_mut(),
        &touched,
        cluster_size,
        grid.data_offset,
        grid.volume_len,
        image.block_size,
        checksums,
        tick,
    )?;
    info!(
        image = %image.path.display(),
        deleted,
        failed,
        clusters = touched.len(),
        blocks_rehashed,
        fold_ms,
        secs = t0.elapsed().as_secs(),
        "the change was folded into the image"
    );
    Ok(Some(Folded {
        entries_deleted: deleted,
        entries_failed: failed,
        clusters: touched.len() as u64,
        blocks_rehashed,
    }))
}

struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Recompute the checksums of the backup blocks `touched` clusters overlap,
/// reading the image as it is now (folded). Touched clusters are image
/// offsets in `cluster_size` units; the checksums are per block of the window
/// `[data_offset, data_offset + volume_len)` — clusters outside it (a GPT head
/// or tail) are not checksummed. A block the copy did not store (all zeros)
/// is recorded only if something was written into it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn rehash_touched_through(
    reader: &mut dyn BlockSource,
    touched: &[u64],
    cluster_size: u64,
    data_offset: u64,
    volume_len: u64,
    block_size: u32,
    checksums: &mut Vec<BlockChecksum>,
    tick: &mut dyn FnMut(u64, u64),
) -> Result<u64> {
    let bs = block_size as u64;
    let mut affected: BTreeSet<u64> = BTreeSet::new();
    for &c in touched {
        let start = (c * cluster_size).max(data_offset);
        let end = (c * cluster_size + cluster_size).min(data_offset + volume_len);
        if start >= end {
            continue;
        }
        for n in (start - data_offset) / bs..=(end - 1 - data_offset) / bs {
            affected.insert(n);
        }
    }
    if affected.is_empty() {
        return Ok(0);
    }

    let mut map: BTreeMap<u64, u64> = checksums
        .iter()
        .map(|c| (c.block_number, c.checksum))
        .collect();
    // Runs rather than single blocks: fewer, longer reads, and every one of
    // them decompresses whole qcow2 clusters anyway (see
    // `crate::checksum::coalesce_block_runs`).
    const RUN_BLOCKS: u64 = 16;
    let mut buf = vec![0u8; (RUN_BLOCKS * bs) as usize];
    let mut done = 0u64;
    for (first, count) in crate::checksum::coalesce_block_runs(affected.iter().copied(), RUN_BLOCKS)
    {
        let offset = first * bs; // volume-relative
        let want = std::cmp::min(count * bs, volume_len - offset) as usize;
        reader.read_at(data_offset + offset, &mut buf[..want])?;
        for i in 0..count {
            let at = (i * bs) as usize;
            if at >= want {
                break; // the last block ran past the end of the volume
            }
            let len = std::cmp::min(bs as usize, want - at);
            let block = &buf[at..at + len];
            let checksum = compute_block_checksum(block);
            match map.entry(first + i) {
                std::collections::btree_map::Entry::Occupied(mut e) => {
                    e.insert(checksum);
                }
                std::collections::btree_map::Entry::Vacant(e) => {
                    // A block the full backup did not store (all-zero) that the
                    // driver wrote into: record it only if now non-zero.
                    if block.iter().any(|&x| x != 0) {
                        e.insert(checksum);
                    }
                }
            }
        }
        done += count;
        tick(done, affected.len() as u64);
    }
    *checksums = map
        .into_iter()
        .map(|(block_number, checksum)| BlockChecksum {
            block_number,
            checksum,
        })
        .collect();
    Ok(affected.len() as u64)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::collections::HashMap;

    use own_backup_common::types::BackupType;
    use own_backup_snapshot::Snapshot;

    use super::*;
    use crate::engine_linux::{VolumeBackupRequest, DEFAULT_BLOCK_SIZE};

    const MIB: u64 = 1024 * 1024;

    fn snapshot(path: &Path) -> Snapshot {
        Snapshot {
            provider_name: "linux".into(),
            source_device: path.to_str().unwrap().into(),
            snapshot_device_path: path.to_str().unwrap().into(),
            created_at: chrono::Utc::now(),
            metadata: HashMap::from([("mode".to_string(), "file".to_string())]),
        }
    }

    fn keys() -> crate::seal::context::ArchiveKeys {
        use crate::seal::keypair::{JobKeyPair, KdfParams, SealedPrivateKey};
        use crate::seal::recovery::RecoveryKey;
        let pair = JobKeyPair::generate().unwrap();
        let recovery = RecoveryKey::generate().unwrap();
        let kdf = KdfParams {
            algorithm: "argon2id".into(),
            m_kib: 64 * 1024,
            t: 3,
            p: 1,
        };
        let envelope =
            SealedPrivateKey::seal("job-fold", &pair, "пароль", &recovery, &kdf).unwrap();
        crate::seal::context::ArchiveKeys::from_private(envelope, pair.private).unwrap()
    }

    /// Back `src` up into `image`, as a child of `parent` when one is given.
    fn backup(
        src: &Path,
        image: &Path,
        format: ImageFormat,
        keys: Option<&crate::seal::context::ArchiveKeys>,
        parent: Option<&Path>,
    ) {
        let req = VolumeBackupRequest {
            source: src.to_str().unwrap().into(),
            image_path: image.to_path_buf(),
            full_image_path: None,
            block_size: DEFAULT_BLOCK_SIZE,
            all_sectors: false,
            use_vss: true,
            kind: if parent.is_some() {
                BackupType::Incremental
            } else {
                BackupType::Full
            },
            parent_image: parent.map(Path::to_path_buf),
            exclusion_patterns: Vec::new(),
            exclusion_patterns_linux: Vec::new(),
            job_id: Some("job-fold".into()),
            passphrase: None,
            image_format: format,
            source_disk: None,
            encryption: keys.cloned(),
            zstd_level: crate::qcow2::DEFAULT_ZSTD_LEVEL,
        };
        let sum = crate::engine_linux::backup_volume(&req, &snapshot(src), |_| Ok(())).unwrap();
        assert_eq!(sum.image_path, image);
        if let Some(parent) = parent {
            // A real child, not a full copy the engine fell back to.
            assert_eq!(
                sum.backup_kind,
                BackupType::Incremental,
                "{}",
                parent.display()
            );
            assert!(
                sum.blocks_unchanged > 0 && sum.blocks_copied <= 2,
                "{} blocks copied",
                sum.blocks_copied
            );
        }
    }

    fn source(path: &Path, seed: u8) {
        let data: Vec<u8> = (0..24 * MIB)
            .map(|i| (i as u8).wrapping_mul(7).wrapping_add(seed) ^ (i >> 16) as u8)
            .collect();
        std::fs::write(path, data).unwrap();
    }

    fn read_all(container: &Container, path: &Path) -> Vec<u8> {
        let mut r = container.reader(path).unwrap();
        let mut out = vec![0u8; r.size() as usize];
        for (i, chunk) in out.chunks_mut(4 * MIB as usize).enumerate() {
            r.read_at(i as u64 * 4 * MIB, chunk).unwrap();
        }
        out
    }

    /// Writes that start and end inside clusters, cross a cluster boundary
    /// and fall into the unwritten tail: the fold must keep every byte around
    /// them as the image read before.
    const PATCHES: &[(u64, usize, u8)] = &[
        (3 * MIB + 512, 1000, 0xA1),
        (5 * MIB - 300, 600, 0xB2),
        (11 * MIB, 64 * 1024, 0x00),
        (17 * MIB + 4096 + 7, 9000, 0xC3),
    ];

    fn patch_pass(s: SharedScratch, _tick: &mut dyn FnMut(u64, u64)) -> Result<(u64, u64)> {
        let mut s = s.lock().unwrap();
        for &(at, len, byte) in PATCHES {
            s.write_at(at, &vec![byte; len])?;
        }
        Ok((PATCHES.len() as u64, 1))
    }

    fn expected(before: &[u8]) -> Vec<u8> {
        let mut want = before.to_vec();
        for &(at, len, byte) in PATCHES {
            want[at as usize..at as usize + len].fill(byte);
        }
        want
    }

    /// Fold the patches into `image`, then check the image reads as patched,
    /// that `untouched` (its parents) did not change by a byte, and that every
    /// block the change reached carries the checksum of what it holds now.
    fn fold_and_check(
        image: &Path,
        format: ImageFormat,
        keys: Option<&crate::seal::context::ArchiveKeys>,
        untouched: &[&Path],
    ) {
        let finished = FinishedImage {
            path: image,
            format,
            encryption: keys,
            block_size: DEFAULT_BLOCK_SIZE,
            zstd_level: crate::qcow2::DEFAULT_ZSTD_LEVEL,
        };
        let container = Container::of(&finished).unwrap();
        let before = read_all(&container, image);
        let parents: Vec<Vec<u8>> = untouched
            .iter()
            .map(|p| std::fs::read(p).unwrap())
            .collect();
        let grid = VolumeSpan {
            data_offset: MIB,
            volume_len: before.len() as u64 - 2 * MIB,
        };
        let mut checksums = vec![BlockChecksum {
            block_number: 20,
            checksum: 1,
        }];
        let folded = change_through_scratch(
            &finished,
            grid,
            &mut checksums,
            &mut |_, _| {},
            &mut patch_pass,
        )
        .unwrap()
        .expect("the pass deleted something");
        assert_eq!((folded.entries_deleted, folded.entries_failed), (4, 1));
        let after = read_all(&container, image);
        assert!(
            after == expected(&before),
            "{}: the image does not read as patched",
            image.display()
        );
        for (p, was) in untouched.iter().zip(&parents) {
            assert!(std::fs::read(p).unwrap() == *was, "{} changed", p.display());
        }
        assert!(!scratch_path(image).exists(), "the scratch is gone");

        let bs = DEFAULT_BLOCK_SIZE as u64;
        let by_block: HashMap<u64, u64> = checksums
            .iter()
            .map(|c| (c.block_number, c.checksum))
            .collect();
        for &(at, len, _) in PATCHES {
            for n in (at - MIB) / bs..=(at + len as u64 - 1 - MIB) / bs {
                let start = (MIB + n * bs) as usize;
                let block = &after[start..start + bs as usize];
                let want = compute_block_checksum(block);
                if block.iter().any(|&b| b != 0) {
                    assert_eq!(by_block.get(&n), Some(&want), "block {n}");
                }
            }
        }
        // A block the change did not reach keeps what it had.
        assert_eq!(by_block.get(&20), Some(&1));
        assert_eq!(folded.blocks_rehashed as usize, {
            let mut n: BTreeSet<u64> = BTreeSet::new();
            for &(at, len, _) in PATCHES {
                n.extend((at - MIB) / bs..=(at + len as u64 - 1 - MIB) / bs);
            }
            n.len()
        });
    }

    #[test]
    fn a_change_folds_into_qcow2_and_into_a_child_without_touching_its_parent() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        source(&src, 1);
        let full = dir.path().join("full.qcow2");
        backup(&src, &full, ImageFormat::Qcow2, None, None);
        // A child whose own clusters are few: most of what the patches touch
        // is the parent's, and the child must take it whole.
        let mut data = std::fs::read(&src).unwrap();
        data[(3 * MIB) as usize..(3 * MIB + 4096) as usize].fill(0x77);
        std::fs::write(&src, &data).unwrap();
        let child = dir.path().join("inc.qcow2");
        backup(&src, &child, ImageFormat::Qcow2, None, Some(&full));
        fold_and_check(&child, ImageFormat::Qcow2, None, &[&full]);
        fold_and_check(&full, ImageFormat::Qcow2, None, &[]);
    }

    #[test]
    fn a_change_folds_into_an_encrypted_image_and_its_child() {
        let dir = tempfile::tempdir().unwrap();
        let keys = keys();
        let src = dir.path().join("src.bin");
        source(&src, 2);
        let full = dir.path().join("full.obk");
        backup(&src, &full, ImageFormat::Qcow2, Some(&keys), None);
        let mut data = std::fs::read(&src).unwrap();
        data[(17 * MIB) as usize..(17 * MIB + 4096) as usize].fill(0x66);
        std::fs::write(&src, &data).unwrap();
        let child = dir.path().join("inc.obk");
        backup(&src, &child, ImageFormat::Qcow2, Some(&keys), Some(&full));
        fold_and_check(&child, ImageFormat::Qcow2, Some(&keys), &[&full]);
        fold_and_check(&full, ImageFormat::Qcow2, Some(&keys), &[]);
    }

    #[test]
    fn a_change_folds_into_vhdx_and_a_partial_block_of_a_child_keeps_the_parent() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        source(&src, 3);
        let full = dir.path().join("full.vhdx");
        backup(&src, &full, ImageFormat::Vhdx, None, None);
        fold_and_check(&full, ImageFormat::Vhdx, None, &[]);
        // Linux makes no VHDX children itself (CHECKS 12); one made by hand
        // with a single block of its own still folds through the chain.
        let child = dir.path().join("child.vhdx");
        let mut w = VhdxWriter::create_differencing(&child, &full).unwrap();
        w.write_at(9 * MIB, &vec![0x44; MIB as usize]).unwrap();
        w.finish().unwrap();
        fold_and_check(&child, ImageFormat::Vhdx, None, &[&full]);
    }

    /// A pass that fails, or deletes nothing, leaves the image as it was —
    /// byte for byte, scratch gone.
    #[test]
    fn a_failed_or_empty_pass_leaves_the_image_alone() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        source(&src, 4);
        let image = dir.path().join("full.qcow2");
        backup(&src, &image, ImageFormat::Qcow2, None, None);
        let was = std::fs::read(&image).unwrap();
        let finished = FinishedImage {
            path: &image,
            format: ImageFormat::Qcow2,
            encryption: None,
            block_size: DEFAULT_BLOCK_SIZE,
            zstd_level: 0,
        };
        let grid = VolumeSpan {
            data_offset: 0,
            volume_len: 8 * MIB,
        };
        let mut checksums = Vec::new();
        let mut failing = |s: SharedScratch, t: &mut dyn FnMut(u64, u64)| {
            patch_pass(s, t)?;
            Err(own_backup_common::OwnBackupError::Device(
                "unmount failed".into(),
            ))
        };
        let mut nothing = |s: SharedScratch, _: &mut dyn FnMut(u64, u64)| {
            s.lock().unwrap().write_at(MIB, &[1, 2, 3])?;
            Ok((0, 3))
        };
        for pass in [&mut failing as ScratchPass<'_>, &mut nothing] {
            let out = change_through_scratch(&finished, grid, &mut checksums, &mut |_, _| {}, pass)
                .unwrap();
            assert!(out.is_none());
            assert!(std::fs::read(&image).unwrap() == was);
            assert!(checksums.is_empty());
            assert!(!scratch_path(&image).exists());
        }
    }

    #[test]
    fn a_window_reads_and_writes_its_own_range_only() {
        struct Mem(Vec<u8>);
        impl BlockSource for Mem {
            fn size(&self) -> u64 {
                self.0.len() as u64
            }
            fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
                buf.copy_from_slice(&self.0[offset as usize..offset as usize + buf.len()]);
                Ok(())
            }
        }
        let file = tempfile::tempfile().unwrap();
        let scratch: SharedScratch = Arc::new(Mutex::new(WritableOverlaySource::new(
            Box::new(Mem(vec![9u8; 1 << 20])),
            4096,
            file,
        )));
        let mut w = ScratchWindow::new(scratch.clone(), 65536, 8192);
        assert_eq!(w.size(), 8192);
        w.write_at(100, &[1, 2, 3]).unwrap();
        let mut buf = [0u8; 4];
        w.read_at(99, &mut buf).unwrap();
        assert_eq!(buf, [9, 1, 2, 3]);
        assert!(w.write_at(8190, &[0; 4]).is_err());
        assert!(w.read_at(8192, &mut [0u8; 1]).is_err());
        assert_eq!(scratch.lock().unwrap().touched_clusters(), vec![16]);
    }
}
