//! Checking an image against the block checksums its sidecar recorded.

use std::path::Path;
use std::time::{Duration, Instant};

use tracing::{info, warn};

use own_backup_common::{tr, OwnBackupError, Result};

use super::read::{load_sidecar_with_keys, ImageReadSource};
use crate::checksum::compute_block_checksum;

/// Result of verifying an image against its sidecar checksums.
#[derive(Debug)]
pub struct VerifySummary {
    pub blocks_checked: u64,
    pub mismatched_blocks: Vec<u64>,
    pub duration: Duration,
}

impl VerifySummary {
    pub fn valid(&self) -> bool {
        self.mismatched_blocks.is_empty()
    }
}

/// Verify every block of an image against the sidecar checksums.
pub fn verify_image(
    image_path: &Path,
    progress: impl FnMut(u64, u64) -> Result<()>,
) -> Result<VerifySummary> {
    verify_image_with_keys(image_path, None, progress)
}

/// The same check for an encrypted image. With the key this is the full check
/// §7.4 asks for: the record tags and the authenticated index decide whether
/// the bytes are ours before any checksum is computed over them.
pub fn verify_image_with_keys(
    image_path: &Path,
    keys: Option<&crate::seal::context::ArchiveKeys>,
    progress: impl FnMut(u64, u64) -> Result<()>,
) -> Result<VerifySummary> {
    verify_image_scoped(image_path, keys, VerifyScope::Everything, progress)
}

/// How much of an image a check reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VerifyScope {
    /// Every block the restore point is made of, read through the whole chain.
    /// Catches damage in ANY copy the point depends on — a bad block in the
    /// base breaks every increment above it, and only this reading notices.
    #[default]
    Everything,
    /// Only the blocks THIS file stores: what the copy just written brought.
    /// An increment of 2 GiB over a 40 GiB base is checked in 2 GiB of reads,
    /// not 42. The trade is said out loud in the interface — the older copies
    /// are not read, so damage in them is not found by this check.
    /// For a full copy, which has no parent, the two are the same.
    LatestCopyOnly,
}

/// Backup block numbers whose data lives in THIS file rather than a parent.
///
/// Asked of the container itself, per cluster, without falling through to the
/// backing file: qcow2 by its L2 descriptor, `.obk` by its authenticated index,
/// VHDX by its BAT. A cluster the file marks explicitly ZERO counts as stored
/// here too — in a child that is a write (it hides the parent's data), and a
/// check that skipped it would never notice a region wrongly emptied.
///
/// A backup block covers `[data_offset + n·bs, data_offset + (n+1)·bs)` in
/// guest bytes; a stored cluster marks every block it touches. Guest bytes
/// before `data_offset` (a whole-disk image's own metadata) belong to no block.
fn blocks_stored_in_this_file(
    image_path: &Path,
    container: &str,
    chain_key: Option<&crate::seal::ChainKey>,
    data_offset: u64,
    block_size: u64,
) -> Result<std::collections::HashSet<u64>> {
    let mut out = std::collections::HashSet::new();
    let mut mark = |guest_start: u64, len: u64| {
        let end = guest_start.saturating_add(len);
        if end <= data_offset || block_size == 0 {
            return;
        }
        let from = guest_start.max(data_offset) - data_offset;
        let to = end - 1 - data_offset;
        for n in (from / block_size)..=(to / block_size) {
            out.insert(n);
        }
    };
    match container {
        "obk" => {
            let key = chain_key.ok_or_else(|| {
                OwnBackupError::Encryption(tr!("engine_win.verify_latest_needs_key").into_owned())
            })?;
            let mut image = crate::seal::obk::ObkImage::open_path(image_path, key)?;
            let cs = image.cluster_size();
            for c in 0..image.total_clusters() {
                if image.cluster_state(c)? != crate::seal::obk::ClusterState::Absent {
                    mark(c * cs, cs);
                }
            }
        }
        "qcow2" => {
            let file = std::io::BufReader::new(std::fs::File::open(image_path)?);
            let mut image = crate::qcow2::reader::Qcow2Reader::open(file)?;
            let cs = image.cluster_size();
            for c in 0..image.total_clusters() {
                if !matches!(
                    image.descriptor(c)?,
                    crate::qcow2::ClusterDescriptor::Unallocated
                ) {
                    mark(c * cs, cs);
                }
            }
        }
        "vhdx" | "avhdx" => {
            let alloc = crate::vhdx_bat::allocated_payload_blocks(image_path)?;
            for b in alloc.blocks {
                mark(b * alloc.block_size, alloc.block_size);
            }
        }
        other => {
            return Err(OwnBackupError::ImageFormat(
                tr!(
                    "engine_win.verify_latest_unknown_container",
                    container = other
                )
                .into_owned(),
            ))
        }
    }
    Ok(out)
}

/// The same check with a chosen scope — see [`VerifyScope`].
pub fn verify_image_scoped(
    image_path: &Path,
    keys: Option<&crate::seal::context::ArchiveKeys>,
    scope: VerifyScope,
    mut progress: impl FnMut(u64, u64) -> Result<()>,
) -> Result<VerifySummary> {
    let started = Instant::now();
    let sidecar = load_sidecar_with_keys(image_path, keys)?;
    let block_size = sidecar.manifest.block_size as u64;
    let volume_len = sidecar.manifest.source_disk.size_bytes;
    let data_offset = sidecar.info.volume_data_offset;

    let chain_key = match (keys, sidecar.info.container.as_str()) {
        (Some(keys), "obk") => Some(keys.open_chain_of(image_path)?.key),
        _ => None,
    };
    let mut image_dev =
        ImageReadSource::open_with(image_path, &sidecar.info.container, chain_key.as_ref())?;

    // The sidecar of an increment describes the WHOLE state, so reading all
    // of it walks every copy underneath. For "latest only" the file itself is
    // asked which blocks it holds, and only those are read.
    let only: Option<std::collections::HashSet<u64>> = match scope {
        VerifyScope::Everything => None,
        VerifyScope::LatestCopyOnly => Some(blocks_stored_in_this_file(
            image_path,
            &sidecar.info.container,
            chain_key.as_ref(),
            data_offset,
            block_size,
        )?),
    };
    let entries: Vec<_> = sidecar
        .checksums
        .iter()
        .filter(|e| {
            only.as_ref()
                .is_none_or(|set| set.contains(&e.block_number))
        })
        .collect();
    if let Some(set) = &only {
        info!(
            image = %image_path.display(),
            of_this_copy = entries.len(),
            of_the_state = sidecar.checksums.len(),
            stored_here = set.len(),
            "verify: only the blocks this copy wrote"
        );
    }

    let total = entries.len() as u64;
    let mut checked = 0u64;
    let mut mismatched = Vec::new();

    for entry in entries {
        let offset = entry.block_number * block_size;
        let len = std::cmp::min(block_size, volume_len - offset) as usize;

        let mut buf = vec![0u8; len];
        image_dev.read_at(data_offset + offset, &mut buf)?;
        let actual = compute_block_checksum(&buf);

        if actual != entry.checksum {
            // Re-read once before declaring corruption: a freshly detached and
            // re-attached image can hand back a transient bad read that a
            // second read clears. Only a checksum that stays wrong is a real
            // image defect — and we log exactly which block for diagnosis.
            let mut buf2 = vec![0u8; len];
            image_dev.read_at(data_offset + offset, &mut buf2)?;
            let actual2 = compute_block_checksum(&buf2);
            if actual2 != entry.checksum {
                // Structure hints: an all-zero block or a non-zero tail where
                // zeros were recorded each point at a specific failure mode
                // (unwritten region vs external write into the image).
                let all_zero = buf2.iter().all(|&b| b == 0);
                let tail_zero = buf2[len.saturating_sub(4096)..].iter().all(|&b| b == 0);
                warn!(
                    block = entry.block_number,
                    image_offset = data_offset + offset,
                    len,
                    expected = format!("{:016x}", entry.checksum),
                    actual = format!("{:016x}", actual2),
                    all_zero,
                    tail_4096_zero = tail_zero,
                    "verify: block checksum mismatch (image defect)"
                );
                mismatched.push(entry.block_number);
            } else {
                info!(
                    block = entry.block_number,
                    "verify: transient read mismatch cleared on re-read"
                );
            }
        }
        checked += 1;
        progress(checked, total)?;
    }

    if mismatched.is_empty() {
        info!(image = %image_path.display(), blocks = total, "verify: image OK");
    }

    Ok(VerifySummary {
        blocks_checked: checked,
        mismatched_blocks: mismatched,
        duration: started.elapsed(),
    })
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use uuid::Uuid;

    use super::*;
    use crate::engine_core::testkit::{checksums, manifest, test_keys, write_adjacent_sidecar};
    use crate::sidecar::{ChainRole, SidecarInfo};

    /// One backup block per qcow2 cluster.
    const BLOCK: u64 = 64 * 1024;
    const BLOCKS: u64 = 8;

    /// What block `n` of a full copy holds: distinct and never zero.
    fn base_fill(n: u64) -> u8 {
        (n + 1) as u8
    }

    /// A volume of whole blocks, block `n` filled with `fill(n)`.
    fn volume(fill: impl Fn(u64) -> u8) -> Vec<u8> {
        (0..BLOCKS)
            .flat_map(|n| std::iter::repeat_n(fill(n), BLOCK as usize))
            .collect()
    }

    /// A qcow2 of `BLOCKS` clusters storing `written` and nothing else. Raw
    /// clusters, so a test can find a block's bytes in the file.
    fn qcow2(path: &Path, backing: Option<&str>, written: &[(u64, u8)]) {
        let file = std::fs::File::create(path).unwrap();
        let mut w = crate::qcow2::Qcow2Writer::new(
            std::io::BufWriter::new(file),
            crate::qcow2::Qcow2WriterOptions {
                virtual_size: BLOCK * BLOCKS,
                backing_file: backing.map(str::to_string),
                compression: crate::qcow2::Qcow2Compression::None,
                ..Default::default()
            },
        )
        .unwrap();
        for &(n, fill) in written {
            w.write_cluster(n, &vec![fill; BLOCK as usize]).unwrap();
        }
        w.finish().unwrap();
    }

    fn qcow2_info(info: SidecarInfo) -> SidecarInfo {
        SidecarInfo {
            container: "qcow2".into(),
            ..info
        }
    }

    /// A full copy and an increment over it: blocks 2 and 5 changed, block 6
    /// emptied (an explicit zero cluster — a write in a child, not a hole).
    fn full_and_child(dir: &Path) -> (PathBuf, PathBuf) {
        let base = dir.join("base.qcow2");
        let child = dir.join("child.qcow2");
        let all: Vec<(u64, u8)> = (0..BLOCKS).map(|n| (n, base_fill(n))).collect();
        qcow2(&base, None, &all);
        qcow2(&child, Some("base.qcow2"), &[(2, 0xA2), (5, 0xA5), (6, 0)]);
        write_adjacent_sidecar(
            &base,
            &qcow2_info(SidecarInfo::new_full("base.qcow2", 0)),
            &volume(base_fill),
            BLOCK,
        );
        // The sidecar of an increment describes the WHOLE state.
        write_adjacent_sidecar(
            &child,
            &qcow2_info(SidecarInfo::new_child(
                "child.qcow2",
                ChainRole::Incremental,
                "base.qcow2",
                0,
            )),
            &volume(|n| match n {
                2 => 0xA2,
                5 => 0xA5,
                6 => 0,
                n => base_fill(n),
            }),
            BLOCK,
        );
        (base, child)
    }

    /// Offset in the file of the raw cluster filled with `fill`.
    fn raw_cluster_offset(image: &Path, fill: u8) -> usize {
        let raw = std::fs::read(image).unwrap();
        (0..raw.len())
            .step_by(BLOCK as usize)
            .find(|&at| {
                raw.get(at..at + BLOCK as usize)
                    .is_some_and(|c| c.iter().all(|&b| b == fill))
            })
            .expect("the cluster is stored raw")
    }

    fn flip_byte(image: &Path, at: usize) {
        let mut raw = std::fs::read(image).unwrap();
        raw[at] ^= 0xFF;
        std::fs::write(image, raw).unwrap();
    }

    #[test]
    fn the_latest_copy_check_reads_only_what_the_child_stores() {
        let dir = tempfile::tempdir().unwrap();
        let (base, child) = full_and_child(dir.path());

        let mut last = (0, 0);
        let everything =
            verify_image_scoped(&child, None, VerifyScope::Everything, |done, total| {
                last = (done, total);
                Ok(())
            })
            .unwrap();
        assert!(everything.valid(), "{:?}", everything.mismatched_blocks);
        assert_eq!(everything.blocks_checked, BLOCKS);
        assert_eq!(last, (BLOCKS, BLOCKS));

        // Blocks 2, 5 and the explicitly emptied 6 — the zero cluster counts:
        // a check that skipped it would never notice a region wrongly emptied.
        let latest =
            verify_image_scoped(&child, None, VerifyScope::LatestCopyOnly, |done, total| {
                last = (done, total);
                Ok(())
            })
            .unwrap();
        assert!(latest.valid(), "{:?}", latest.mismatched_blocks);
        assert_eq!(latest.blocks_checked, 3);
        assert_eq!(last, (3, 3), "progress counts the blocks of this copy");

        // A full copy has no parent: both scopes read all of it.
        let full =
            verify_image_scoped(&base, None, VerifyScope::LatestCopyOnly, |_, _| Ok(())).unwrap();
        assert_eq!(full.blocks_checked, BLOCKS);
    }

    #[test]
    fn damage_in_the_base_is_found_by_the_whole_check_only() {
        let dir = tempfile::tempdir().unwrap();
        let (base, child) = full_and_child(dir.path());
        // Block 0 is not rewritten by the increment: it is read from the base.
        flip_byte(&base, raw_cluster_offset(&base, base_fill(0)) + 100);

        let everything =
            verify_image_scoped(&child, None, VerifyScope::Everything, |_, _| Ok(())).unwrap();
        assert_eq!(everything.mismatched_blocks, vec![0]);
        // The trade said out loud in the interface: the older copy is not read.
        let latest =
            verify_image_scoped(&child, None, VerifyScope::LatestCopyOnly, |_, _| Ok(())).unwrap();
        assert!(latest.valid());
    }

    #[test]
    fn a_flipped_byte_is_reported_when_the_reread_agrees() {
        let dir = tempfile::tempdir().unwrap();
        let (base, _) = full_and_child(dir.path());
        flip_byte(&base, raw_cluster_offset(&base, base_fill(3)) + 4096);

        // The second read sees the same bytes, so this is an image defect and
        // not a transient read: block 3, and only block 3, is reported.
        let summary = verify_image(&base, |_, _| Ok(())).unwrap();
        assert!(!summary.valid());
        assert_eq!(summary.mismatched_blocks, vec![3]);
        assert_eq!(summary.blocks_checked, BLOCKS, "the check goes on past it");
    }

    #[test]
    fn an_encrypted_image_without_its_key_is_an_encryption_error() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("full.obk");
        let keys = test_keys();
        let chain = keys.start_chain().unwrap();
        let options = keys.writer_options(
            &chain,
            crate::seal::context::ImagePlan {
                backup_id: Uuid::new_v4(),
                role: ChainRole::Full,
                parent: None,
                created_at: "2026-10-18T00:00:00Z".into(),
                virtual_size: BLOCK * BLOCKS,
                source_id: Some("volume".into()),
                consolidated_increments: 0,
                chain_started_at: None,
                compression: crate::seal::obk::ObkCompression::Zstd { level: 3 },
            },
        );
        let mut writer =
            crate::seal::obk::ObkStagedWriter::create(&image, &chain.key, options).unwrap();
        let content = volume(base_fill);
        writer.write_at(0, &content).unwrap();
        let info = SidecarInfo {
            container: "obk".into(),
            ..SidecarInfo::new_full("full.obk", 0)
        };
        let mut sealed = Vec::new();
        crate::sidecar::write_sidecar_v2_to(
            &mut sealed,
            &info,
            &manifest(content.len() as u64, BLOCK),
            &checksums(&content, BLOCK),
        )
        .unwrap();
        writer.finish(&sealed).unwrap();

        // Without the key: refused by name, never half-read.
        assert!(matches!(
            verify_image(&image, |_, _| Ok(())),
            Err(OwnBackupError::Encryption(_))
        ));
        assert!(matches!(
            ImageReadSource::open(&image, "obk"),
            Err(OwnBackupError::Encryption(_))
        ));
        assert!(matches!(
            blocks_stored_in_this_file(&image, "obk", None, 0, BLOCK),
            Err(OwnBackupError::Encryption(_))
        ));

        // With it, the records are authenticated and every block checks out.
        let everything = verify_image_with_keys(&image, Some(&keys), |_, _| Ok(())).unwrap();
        assert!(everything.valid(), "{:?}", everything.mismatched_blocks);
        assert_eq!(everything.blocks_checked, BLOCKS);
        let latest =
            verify_image_scoped(&image, Some(&keys), VerifyScope::LatestCopyOnly, |_, _| {
                Ok(())
            })
            .unwrap();
        assert_eq!(latest.blocks_checked, BLOCKS);
    }

    #[test]
    fn a_vhdx_latest_copy_is_found_through_its_allocation_table() {
        const MIB: u64 = 1 << 20;
        // A volume image: a GPT head before the volume, which belongs to no
        // backup block, then 7 MiB of volume in half-MiB backup blocks.
        let data_offset = MIB;
        let block = MIB / 2;
        let volume_len = 7 * MIB;
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("volume.vhdx");

        let mut guest = vec![0u8; (data_offset + volume_len) as usize];
        let mut writer =
            crate::vhdx_format::VhdxWriter::create(&image, guest.len() as u64, MIB, 512).unwrap();
        let mut write = |at: u64, bytes: &[u8]| {
            writer.write_at(at, bytes).unwrap();
            guest[at as usize..at as usize + bytes.len()].copy_from_slice(bytes);
        };
        write(0, b"GPT head"); // payload block 0: before the volume
        write(MIB, &vec![0x11; MIB as usize]); // payload block 1: backup blocks 0, 1
        write(4 * MIB + 4096, &[0x44; 4096]); // payload block 4: backup blocks 6, 7
        writer.finish().unwrap();
        write_adjacent_sidecar(
            &image,
            &SidecarInfo::new_full("volume.vhdx", data_offset),
            &guest[data_offset as usize..],
            block,
        );

        let everything = verify_image(&image, |_, _| Ok(())).unwrap();
        assert!(everything.valid(), "{:?}", everything.mismatched_blocks);
        assert_eq!(everything.blocks_checked, volume_len / block);

        let mut stored: Vec<u64> =
            blocks_stored_in_this_file(&image, "vhdx", None, data_offset, block)
                .unwrap()
                .into_iter()
                .collect();
        stored.sort_unstable();
        assert_eq!(stored, vec![0, 1, 6, 7]);
        let latest =
            verify_image_scoped(&image, None, VerifyScope::LatestCopyOnly, |_, _| Ok(())).unwrap();
        assert!(latest.valid());
        assert_eq!(latest.blocks_checked, 4);
    }
}
