//! NTFS/ReFS true removal for the qcow2 container (plan Q-R3).
//!
//! qcow2 has no kernel merge like VHDX, so we RW-mount the finished image via
//! Arsenal Image Mounter over a copy-on-write scratch overlay
//! ([`crate::overlay::WritableOverlaySource`]), let the OS filesystem driver
//! delete the excluded files into the overlay (the base qcow2 is never
//! touched), then fold the touched clusters back into the qcow2 with
//! [`crate::qcow2::Qcow2InPlaceMerge`] and re-checksum the affected backup
//! blocks. The deletion + teardown is the same proven code the VHDX path uses
//! ([`crate::true_removal_win::delete_paths_on_disk`]).
//!
//! Transactional like the VHDX path: any failure BEFORE the merge leaves the
//! image exactly as checksummed (excluded files remain zero-filled ghosts — a
//! valid backup, returned as `Ok(None)`); the merge repoints the qcow2 header
//! last, and a merge/re-checksum failure fails the run (an image that no longer
//! matches its checksums must never ship).

#![cfg(target_os = "windows")]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing::{info, warn};

use own_backup_common::{OwnBackupError, Result};

use crate::block_source::BlockSource;
use crate::checksum::compute_block_checksum;
use crate::exclusions_win::ExcludedPath;
use crate::overlay::WritableOverlaySource;
use crate::qcow2::{Qcow2InPlaceMerge, Qcow2RandomReader};
use crate::sidecar::BlockChecksum;
use crate::win_device::WinRawDevice;

/// What the qcow2 removal pass did.
pub struct Qcow2RemovalOutcome {
    pub entries_deleted: u64,
    pub entries_failed: u64,
    pub clusters_merged: u64,
    pub blocks_rehashed: u64,
}

fn dev(msg: impl std::fmt::Display) -> OwnBackupError {
    OwnBackupError::Device(format!("qcow2 true removal: {msg}"))
}

fn scratch_path(image_path: &Path) -> PathBuf {
    let mut name = image_path.file_name().unwrap_or_default().to_os_string();
    name.push(".excl.scratch");
    image_path.with_file_name(name)
}

/// Delete `paths` from the finished qcow2 image, in place.
///
/// * `Ok(Some(_))` — files removed, image merged + `checksums` updated to
///   describe the final image.
/// * `Ok(None)` — nothing deleted / mount failed before any change; the image
///   is exactly as checksummed (ghosts remain), still a valid backup.
/// * `Err` — a failure at or after the merge: the caller fails the run.
pub fn remove_excluded_from_qcow2(
    image_path: &Path,
    paths: &[ExcludedPath],
    data_offset: u64,
    volume_len: u64,
    block_size: u32,
    zstd_level: Option<i32>,
    checksums: &mut Vec<BlockChecksum>,
    tick: &mut dyn FnMut(u64),
) -> Result<Option<Qcow2RemovalOutcome>> {
    let t0 = Instant::now();
    let (virtual_size, cluster_size) = {
        let r = Qcow2RandomReader::open(image_path)?;
        (r.virtual_size(), r.cluster_size())
    };
    info!(
        image = %image_path.display(),
        paths = paths.len(),
        cluster_size,
        "qcow2 true removal: starting RW mount-&-delete"
    );

    // Copy-on-write scratch over the read-only base (deleted at the end).
    let scratch = scratch_path(image_path);
    let _ = std::fs::remove_file(&scratch);
    let overlay_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&scratch)?;
    let base: Box<dyn BlockSource> = Box::new(Qcow2RandomReader::open(image_path)?);
    let overlay = Arc::new(Mutex::new(WritableOverlaySource::new(base, cluster_size, overlay_file)));

    // devio RW server on an ephemeral loopback port.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| dev(format!("bind: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| dev(format!("local_addr: {e}")))?
        .port();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_bg = stop.clone();
    let overlay_bg = overlay.clone();
    let server = std::thread::Builder::new()
        .name("aim-devio-rw".into())
        .spawn(move || {
            if let Err(e) = crate::devio::serve_listener_rw(listener, Box::new(overlay_bg), stop_bg) {
                warn!(error = %e, "qcow2 true removal: devio RW server ended with an error");
            }
        })
        .map_err(|e| dev(format!("spawn devio: {e}")))?;

    // Mount RW via AIM, delete, tear down. Best-effort: on ANY failure here the
    // base qcow2 is untouched (writes went to the scratch), so we keep ghosts.
    let mount_del = mount_delete(port, virtual_size, paths, tick);
    stop.store(true, Ordering::SeqCst);
    let _ = server.join();

    let (deleted, failed) = match mount_del {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "qcow2 true removal: mount-&-delete failed — excluded files stay as zero-filled ghosts");
            let _ = std::fs::remove_file(&scratch);
            return Ok(None);
        }
    };

    let touched = overlay.lock().unwrap().touched_clusters();
    if deleted == 0 || touched.is_empty() {
        warn!(deleted, failed, "qcow2 true removal: nothing deleted — image left with zero-filled ghosts");
        let _ = std::fs::remove_file(&scratch);
        return Ok(None);
    }

    // Fold the touched clusters into the base qcow2 (in place, header last).
    let t_merge = Instant::now();
    {
        let mut ov = overlay.lock().unwrap();
        let mut merge = Qcow2InPlaceMerge::open(image_path, zstd_level)?;
        for &c in &touched {
            let data = ov.read_cluster(c)?;
            merge.write_cluster(c, &data)?;
        }
        merge.commit()?;
    }
    let merge_ms = t_merge.elapsed().as_millis();
    let _ = std::fs::remove_file(&scratch);

    let blocks_rehashed = rehash_touched(
        image_path,
        &touched,
        cluster_size,
        data_offset,
        volume_len,
        block_size,
        checksums,
    )?;

    info!(
        deleted,
        failed,
        clusters = touched.len(),
        blocks_rehashed,
        merge_ms,
        secs = t0.elapsed().as_secs(),
        "qcow2 true removal: excluded files deleted from the image by the FS driver"
    );
    Ok(Some(Qcow2RemovalOutcome {
        entries_deleted: deleted,
        entries_failed: failed,
        clusters_merged: touched.len() as u64,
        blocks_rehashed,
    }))
}

/// Multi-volume twin of [`remove_excluded_from_qcow2`] for a whole-disk qcow2
/// image (plan D2): RW-mount the whole image once (AIM over a CoW overlay), let
/// the FS driver delete each partition's paths on the volume at that partition's
/// disk offset, then fold the touched clusters back into the base and re-checksum
/// the affected image blocks. A whole-disk image is disk-block absolute, so the
/// checksum grid covers `0..image_size` (`data_offset = 0`). Same graceful
/// contract as [`remove_excluded_from_qcow2`].
pub fn remove_excluded_from_disk_image_qcow2(
    image_path: &Path,
    partitions: &[(u64, Vec<ExcludedPath>)],
    image_size: u64,
    block_size: u32,
    zstd_level: Option<i32>,
    checksums: &mut Vec<BlockChecksum>,
    tick: &mut dyn FnMut(u64),
) -> Result<Option<Qcow2RemovalOutcome>> {
    let total_paths: usize = partitions.iter().map(|(_, p)| p.len()).sum();
    if total_paths == 0 {
        return Ok(None);
    }
    let t0 = Instant::now();
    let (virtual_size, cluster_size) = {
        let r = Qcow2RandomReader::open(image_path)?;
        (r.virtual_size(), r.cluster_size())
    };
    info!(
        image = %image_path.display(),
        partitions = partitions.iter().filter(|(_, p)| !p.is_empty()).count(),
        paths = total_paths,
        cluster_size,
        "qcow2 true removal: starting whole-disk RW mount-&-delete"
    );

    // CoW scratch over the read-only base (deleted at the end).
    let scratch = scratch_path(image_path);
    let _ = std::fs::remove_file(&scratch);
    let overlay_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&scratch)?;
    let base: Box<dyn BlockSource> = Box::new(Qcow2RandomReader::open(image_path)?);
    let overlay = Arc::new(Mutex::new(WritableOverlaySource::new(base, cluster_size, overlay_file)));

    let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| dev(format!("bind: {e}")))?;
    let port = listener.local_addr().map_err(|e| dev(format!("local_addr: {e}")))?.port();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_bg = stop.clone();
    let overlay_bg = overlay.clone();
    let server = std::thread::Builder::new()
        .name("aim-devio-rw".into())
        .spawn(move || {
            if let Err(e) = crate::devio::serve_listener_rw(listener, Box::new(overlay_bg), stop_bg) {
                warn!(error = %e, "qcow2 true removal: devio RW server ended with an error");
            }
        })
        .map_err(|e| dev(format!("spawn devio: {e}")))?;

    let mount_del = mount_delete_multi(port, virtual_size, partitions, tick);
    stop.store(true, Ordering::SeqCst);
    let _ = server.join();

    let (deleted, failed) = match mount_del {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "qcow2 true removal: whole-disk mount-&-delete failed — excluded files stay as zero-filled ghosts");
            let _ = std::fs::remove_file(&scratch);
            return Ok(None);
        }
    };

    let touched = overlay.lock().unwrap().touched_clusters();
    if deleted == 0 || touched.is_empty() {
        warn!(deleted, failed, "qcow2 true removal: nothing deleted — image left with zero-filled ghosts");
        let _ = std::fs::remove_file(&scratch);
        return Ok(None);
    }

    let t_merge = Instant::now();
    {
        let mut ov = overlay.lock().unwrap();
        let mut merge = Qcow2InPlaceMerge::open(image_path, zstd_level)?;
        for &c in &touched {
            let data = ov.read_cluster(c)?;
            merge.write_cluster(c, &data)?;
        }
        merge.commit()?;
    }
    let merge_ms = t_merge.elapsed().as_millis();
    let _ = std::fs::remove_file(&scratch);

    // Whole-disk image is disk-block absolute: data_offset = 0, len = image_size.
    let blocks_rehashed =
        rehash_touched(image_path, &touched, cluster_size, 0, image_size, block_size, checksums)?;

    info!(
        deleted,
        failed,
        clusters = touched.len(),
        blocks_rehashed,
        merge_ms,
        secs = t0.elapsed().as_secs(),
        "qcow2 true removal: excluded files deleted from the whole-disk image by the FS driver"
    );
    Ok(Some(Qcow2RemovalOutcome {
        entries_deleted: deleted,
        entries_failed: failed,
        clusters_merged: touched.len() as u64,
        blocks_rehashed,
    }))
}

/// AIM RW mount → online → delete (shared with the VHDX path) → teardown. The
/// AIM device is always removed before returning.
fn mount_delete(
    port: u16,
    virtual_size: u64,
    paths: &[ExcludedPath],
    tick: &mut dyn FnMut(u64),
) -> Result<(u64, u64)> {
    // Auto-mount left ON so the FS mounts; the drive letter is stripped after.
    let adapter = crate::aim_win::AimAdapter::open()?;
    let device_number = adapter.create_tcp_proxy(port, virtual_size, false)?;
    let result = (|| -> Result<(u64, u64)> {
        let physical = crate::aim_win::find_physical_drive(device_number)
            .map(|n| format!(r"\\.\PhysicalDrive{n}"))
            .ok_or_else(|| dev("AIM disk did not surface"))?;
        let d = open_write_retry(&physical)?;
        // Never delete on anything but our proxy disk: a resolution bug here
        // once pointed the pass at a LIVE host disk (2026-07-08). The proxy is
        // created with exactly `virtual_size` bytes, so a mismatch = wrong disk.
        if d.length() != virtual_size {
            return Err(dev(format!(
                "{physical} is {} bytes, expected {virtual_size} — refusing to touch a disk that is not the mounted image",
                d.length()
            )));
        }
        let _ = d.update_properties();
        d.set_online_this_session()?;
        let disk_number: u32 = physical
            .trim_start_matches(r"\\.\PhysicalDrive")
            .parse()
            .map_err(|_| dev(format!("unexpected physical path {physical}")))?;
        info!(physical = %physical, disk = disk_number, "qcow2 true removal: AIM disk attached RW and online");
        let counts = crate::true_removal_win::delete_paths_on_disk(disk_number, &d, paths, tick)?;
        drop(d);
        Ok(counts)
    })();
    let removed = adapter.remove_device(device_number);
    let counts = result?;
    removed?;
    Ok(counts)
}

/// Multi-volume twin of [`mount_delete`]: AIM RW mount → online → per-partition
/// delete on the volume at each partition's disk offset → teardown all volumes.
/// The AIM device is always removed before returning.
fn mount_delete_multi(
    port: u16,
    virtual_size: u64,
    partitions: &[(u64, Vec<ExcludedPath>)],
    tick: &mut dyn FnMut(u64),
) -> Result<(u64, u64)> {
    // Auto-mount left ON so the FS mounts; drive letters are stripped after.
    let adapter = crate::aim_win::AimAdapter::open()?;
    let device_number = adapter.create_tcp_proxy(port, virtual_size, false)?;
    let result = (|| -> Result<(u64, u64)> {
        let physical = crate::aim_win::find_physical_drive(device_number)
            .map(|n| format!(r"\\.\PhysicalDrive{n}"))
            .ok_or_else(|| dev("AIM disk did not surface"))?;
        let d = open_write_retry(&physical)?;
        // Never touch anything but our proxy disk (size guard — 2026-07-08).
        if d.length() != virtual_size {
            return Err(dev(format!(
                "{physical} is {} bytes, expected {virtual_size} — refusing to touch a disk that is not the mounted image",
                d.length()
            )));
        }
        let _ = d.update_properties();
        d.set_online_this_session()?;
        let disk_number: u32 = physical
            .trim_start_matches(r"\\.\PhysicalDrive")
            .parse()
            .map_err(|_| dev(format!("unexpected physical path {physical}")))?;
        info!(physical = %physical, disk = disk_number, "qcow2 true removal: whole-disk AIM disk attached RW and online");
        let counts = crate::true_removal_win::delete_paths_on_disk_multi(disk_number, &d, partitions, tick)?;
        drop(d);
        Ok(counts)
    })();
    let removed = adapter.remove_device(device_number);
    let counts = result?;
    removed?;
    Ok(counts)
}

fn open_write_retry(path: &str) -> Result<WinRawDevice> {
    let mut last: Option<OwnBackupError> = None;
    for attempt in 0..20 {
        match WinRawDevice::open_write(path) {
            Ok(d) => return Ok(d),
            Err(e) => {
                last = Some(e);
                std::thread::sleep(Duration::from_millis(100 + 50 * attempt));
            }
        }
    }
    Err(last.unwrap_or_else(|| dev(format!("cannot open {path}"))))
}

/// Recompute checksums for the backup blocks overlapping the touched clusters,
/// reading the FINAL (merged) image. Mirrors
/// [`crate::true_removal_win`]'s `rehash_touched_blocks`.
pub(crate) fn rehash_touched(
    image_path: &Path,
    touched: &[u64],
    cluster_size: u64,
    data_offset: u64,
    volume_len: u64,
    block_size: u32,
    checksums: &mut Vec<BlockChecksum>,
) -> Result<u64> {
    let bs = block_size as u64;
    // Touched clusters are disk-image offsets; the sidecar checksums are per
    // VOLUME block. Map through the volume window [data_offset, +volume_len);
    // clusters in the GPT head/tail (not checksummed) are skipped.
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

    let mut reader = Qcow2RandomReader::open(image_path)?;
    let mut map: BTreeMap<u64, u64> =
        checksums.iter().map(|c| (c.block_number, c.checksum)).collect();
    let mut buf = vec![0u8; block_size as usize];
    for &n in &affected {
        let offset = n * bs; // volume-relative
        let len = std::cmp::min(bs, volume_len - offset) as usize;
        reader.read_at(data_offset + offset, &mut buf[..len])?;
        let checksum = compute_block_checksum(&buf[..len]);
        match map.entry(n) {
            std::collections::btree_map::Entry::Occupied(mut e) => {
                e.insert(checksum);
            }
            std::collections::btree_map::Entry::Vacant(e) => {
                // A block the full backup did not store (all-zero) that the
                // driver wrote into: record it only if now non-zero.
                if buf[..len].iter().any(|&x| x != 0) {
                    e.insert(checksum);
                }
            }
        }
    }
    *checksums = map
        .into_iter()
        .map(|(block_number, checksum)| BlockChecksum { block_number, checksum })
        .collect();
    Ok(affected.len() as u64)
}
