//! True removal inside an **encrypted** image: the same mount-and-delete pass
//! the plaintext qcow2 container gets, with nothing plaintext left behind.
//!
//! The shape is [`crate::qcow2_removal_win`]'s, and deliberately so — the
//! filesystem driver does the deleting either way, and what it sits on is the
//! container's business. Two things differ, and both are the point:
//!
//! - the scratch the driver's writes land in is **sealed**
//!   ([`crate::overlay::SealedSlots`]): §7.4 does not allow a readable scratch
//!   beside an encrypted copy, and a deletion pass writes exactly the parts of
//!   the filesystem that name the files being removed;
//! - the changed clusters cannot be folded back in place, because the container
//!   is written once. [`crate::seal::obk::apply_changed`] appends them as new
//!   records, overwrites the old ones with zeros and re-emits the index and the
//!   tail — so the previous contents of those clusters do not survive the pass.
//!
//! Same graceful contract as the plaintext pass, with one exception: a failure
//! **before** the rebuild leaves the image exactly as it was (the writes are in
//! the scratch), and the caller keeps the copy with its catalog untouched; a
//! failure during the rebuild fails the run, because an image whose records
//! have been zeroed is not a copy anyone may ship.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tracing::{info, warn};

use own_backup_common::Result;

use crate::exclusions_win::ExcludedPath;
use crate::overlay::{SealedSlots, WritableOverlaySource};
use crate::qcow2_removal_win::{mount_delete, rehash_touched_through, Qcow2RemovalOutcome};
use crate::seal::obk::{apply_changed, ObkRandomReader};
use crate::seal::ChainKey;
use crate::sidecar::BlockChecksum;

fn scratch_path(image_path: &Path) -> std::path::PathBuf {
    let mut p = image_path.to_path_buf();
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "image".into());
    p.set_file_name(format!("{name}.excl.scratch"));
    p
}

/// Delete `paths` inside the encrypted image and put the result back into it.
///
/// `Ok(None)` means the pass did not happen and the image is untouched — the
/// caller decides what to say about the exclusion rules.
#[allow(clippy::too_many_arguments)]
pub fn remove_excluded_from_obk(
    image_path: &Path,
    chain: &ChainKey,
    paths: &[ExcludedPath],
    data_offset: u64,
    volume_len: u64,
    block_size: u32,
    checksums: &mut Vec<BlockChecksum>,
    tick: &mut dyn FnMut(u64, u64),
) -> Result<Option<Qcow2RemovalOutcome>> {
    if paths.is_empty() {
        return Ok(None);
    }
    let t0 = Instant::now();
    let (virtual_size, cluster_size, chain_id) = {
        let reader = ObkRandomReader::open(image_path, chain)?;
        let header = crate::seal::obk::read_open_header(image_path)?;
        (
            reader.virtual_size(),
            reader.cluster_size(),
            *header.chain_id.as_bytes(),
        )
    };
    info!(
        image = %image_path.display(),
        paths = paths.len(),
        cluster_size,
        "obk true removal: starting RW mount-&-delete over a sealed scratch"
    );

    let scratch = scratch_path(image_path);
    let _ = std::fs::remove_file(&scratch);
    let scratch_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&scratch)?;
    let base: Box<dyn crate::block_source::BlockSource> =
        Box::new(ObkRandomReader::open(image_path, chain)?);
    let store = SealedSlots::new(scratch_file, cluster_size, chain, chain_id)?;
    let overlay = Arc::new(Mutex::new(WritableOverlaySource::with_store(
        base,
        cluster_size,
        store,
    )));

    // Attach RW, delete, tear down. On any failure here the image is untouched:
    // everything the driver wrote is in the sealed scratch.
    let (deleted, failed) = match mount_delete(overlay.clone(), virtual_size, image_path, paths, tick)
    {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "obk true removal: mount-&-delete failed — the image is unchanged");
            let _ = std::fs::remove_file(&scratch);
            return Ok(None);
        }
    };

    let touched = overlay.lock().unwrap().touched_clusters();
    if deleted == 0 || touched.is_empty() {
        warn!(deleted, failed, "obk true removal: nothing deleted — the image is unchanged");
        let _ = std::fs::remove_file(&scratch);
        return Ok(None);
    }

    // Put the changed clusters back: new records, the old ones zeroed, the
    // index and the tail re-emitted. Streamed out of the scratch, because the
    // driver touches more of the filesystem's metadata than fits in memory.
    let t_rebuild = Instant::now();
    let rebuild = {
        let overlay = overlay.clone();
        apply_changed(
            image_path,
            chain,
            &touched,
            move |cluster| overlay.lock().unwrap().read_cluster(cluster),
            None,
        )
    };
    let _ = std::fs::remove_file(&scratch);
    let stats = match rebuild {
        Ok(stats) => stats,
        Err(e) => {
            // Past this point the image's old records are gone, so there is no
            // "keep the ghosts" to fall back on: the run must fail.
            return Err(e);
        }
    };
    let rebuild_ms = t_rebuild.elapsed().as_millis();

    let mut reader = ObkRandomReader::open(image_path, chain)?;
    let blocks_rehashed = rehash_touched_through(
        &mut reader,
        &touched,
        cluster_size,
        data_offset,
        volume_len,
        block_size,
        checksums,
        tick,
    )?;

    info!(
        deleted,
        failed,
        clusters = touched.len(),
        bytes_zeroed = stats.bytes_zeroed,
        blocks_rehashed,
        rebuild_ms,
        secs = t0.elapsed().as_secs(),
        "obk true removal: excluded files deleted from the encrypted image"
    );
    Ok(Some(Qcow2RemovalOutcome {
        entries_deleted: deleted,
        entries_failed: failed,
        clusters_merged: touched.len() as u64,
        blocks_rehashed,
    }))
}
