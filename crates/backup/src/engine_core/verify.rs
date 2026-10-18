//! Checking an image against the block checksums its sidecar recorded.

use std::path::Path;
use std::time::{Duration, Instant};

use tracing::{info, warn};

use own_backup_common::{tr, OwnBackupError, Result};

use super::read::{load_sidecar_with_keys, ImageReadSource};
use crate::checksum::compute_block_checksum;

/// Result of verifying an image against its sidecar checksums.
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
