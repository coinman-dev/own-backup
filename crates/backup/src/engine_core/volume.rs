//! A volume backup: one volume, or a snapshot of one, copied into a fresh
//! image — what is asked of it, the pipeline that does it on any platform,
//! and what it reports.
//!
//! The pipeline reads like the steps it takes: open the source
//! ([`super::source`]), settle the parent chain, find the used blocks
//! ([`super::volume_maps`]), apply the exclusions ([`super::volume_exclusions`]),
//! write the image ([`super::volume_write`]), then let the snapshot go, delete
//! what the exclusions left in the image and write the final sidecar.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tracing::{info, warn};
use uuid::Uuid;
use zeroize::Zeroizing;

use own_backup_common::disk::DiskInfo;
use own_backup_common::image::ImageManifest;
use own_backup_common::progress::BackupProgress;
use own_backup_common::types::{
    BackupMode, BackupStatus, BackupType, BootMode, CompressionAlgo, ImageFormat, PartitionTable,
};
use own_backup_common::{tr, OwnBackupError, Result};

use super::chain::{child_refusal, validate_parent};
use super::platform::{EnginePlatform, FinishedImage, SourceDevice, SourceSession, VolumeSpan};
use super::source::VolumeSource;
use super::target::{
    check_encrypted_target, may_publish, refuse_existing_outputs, settled_image_path,
};
use super::volume_exclusions::{resolve_exclusions, VolumeExclusions};
use super::volume_maps::VolumeMaps;
use super::volume_write::{
    create_volume_target, finish_volume_image, volume_block_reader, write_volume_gpt,
    VolumeImageFacts, MIB,
};
use crate::gpt_writer::DEFAULT_PARTITION_OFFSET;
use crate::raw_copy::copy_used_blocks;
use crate::sidecar::{sidecar_path_for, ChainRole, Sidecar, SidecarInfo};

/// Parameters for a volume backup.
#[derive(Clone)]
pub struct VolumeBackupRequest {
    /// Source volume: `C:`, `\\.\C:` or a VSS snapshot device path; on Linux
    /// the device the job names (`/dev/…`), never the snapshot of it — the
    /// platform's session reads that, and the image's identity comes from
    /// this.
    pub source: String,
    /// Output image path (`.vhdx`/`.avhdx`). Must not exist yet.
    pub image_path: PathBuf,
    /// Where the image goes when a child was asked for and a FULL comes out
    /// (parent unusable, disk re-partitioned): the file name says what the file
    /// is, and so does the name recorded inside it. `None` keeps `image_path`
    /// whatever happens.
    pub full_image_path: Option<PathBuf>,
    /// Backup block size in bytes; use [`DEFAULT_BLOCK_SIZE`](super::target::DEFAULT_BLOCK_SIZE).
    pub block_size: u32,
    /// Copy every block even when an NTFS bitmap is available.
    pub all_sectors: bool,
    /// Take a VSS shadow copy of the volume and read from it (consistent
    /// backup of a live volume). Falls back to reading the live volume with a
    /// warning if VSS fails. Ignored for non-volume sources, and on Linux,
    /// where the caller prepared the snapshot before the run.
    pub use_vss: bool,
    /// Full / Incremental / Differential (phase 5). Non-full kinds need
    /// `parent_image`; if the chain turns out unusable (parent missing,
    /// volume resized, block size changed) the engine falls back to a full
    /// backup — the summary reports what was actually done.
    pub kind: BackupType,
    /// Parent image for incremental (previous version) or differential (the
    /// chain's full backup).
    pub parent_image: Option<PathBuf>,
    /// File-exclusion glob patterns (phase 3). Applied only in smart NTFS
    /// mode; excluded clusters are dropped from the used map and zeroed
    /// inside blocks that are still copied.
    pub exclusion_patterns: Vec<String>,
    /// The job's rules for Linux filesystems (ext, btrfs, xfs): case as
    /// written, `/` as the separator, applied only where the session holds
    /// still. Windows never sets them (decision R6): its volume path does not
    /// apply Linux rules, as before.
    pub exclusion_patterns_linux: Vec<String>,
    /// GUI job id, stamped into the sidecar so retention can find this
    /// job's chains later.
    pub job_id: Option<String>,
    /// Passphrase for an encrypted (LUKS) source. When present and the source
    /// is a LUKS container, the volume is unlocked and its *decrypted* data is
    /// backed up (used blocks only); on restore, encryption must be re-applied
    /// manually. Ignored for non-encrypted sources.
    pub passphrase: Option<Zeroizing<String>>,
    /// Image container format (decision #13): VHDX — kernel-written, Windows-
    /// native mounting, NTFS/ReFS true removal, AVHDX chains; qcow2 — written
    /// in userspace with per-cluster zstd compression, no attach at all.
    /// qcow2 limits for now: full backups only, exclusions are zeroing-only.
    pub image_format: ImageFormat,
    /// Full source-disk layout snapshot (the parent physical disk of this
    /// volume, from the GUI's live inventory) to embed in the sidecar, so a
    /// restore can show the whole disk and flag the backed-up partition without
    /// jobs.json. `None` for CLI runs (the sidecar then omits it).
    pub source_disk: Option<own_backup_common::disk::DiskInfo>,
    /// Present for an encrypted job: the keys the service opened before this
    /// run started. Nothing inside the run asks a human anything (§7.4), and
    /// the image written is `.obk` whatever `image_format` says.
    pub encryption: Option<crate::seal::context::ArchiveKeys>,
    /// zstd level for qcow2 and `.obk` (1–19; anything else is the default,
    /// see [`crate::qcow2::zstd_level_or_default`]). VHDX is not compressed.
    pub zstd_level: i32,
}

/// Outcome of a completed volume backup.
pub struct VolumeBackupSummary {
    pub image_path: PathBuf,
    pub sidecar_path: PathBuf,
    pub source_bytes: u64,
    pub total_blocks: u64,
    pub used_blocks: u64,
    pub blocks_copied: u64,
    pub bytes_copied: u64,
    pub duration: Duration,
    /// True when the NTFS used-sector bitmap was applied; false = every
    /// block was copied (explicit sector-by-sector or non-NTFS fallback).
    pub smart_mode: bool,
    /// True when the data was read from a VSS shadow copy.
    pub vss_used: bool,
    /// What was actually produced — may differ from the requested kind when
    /// the engine fell back to a full backup.
    pub backup_kind: BackupType,
    /// Used blocks skipped because they match the parent version.
    pub blocks_unchanged: u64,
    /// Files whose extents were excluded (phase 3).
    pub excluded_files: u64,
    /// Bytes excluded from the image by exclusion patterns.
    pub excluded_bytes: u64,
    /// Per-category byte totals of the files in the archive; None when no
    /// exclusion walk ran (exclusions disabled / non-NTFS / sector mode).
    pub categories: Option<crate::exclusions::CategorySizes>,
    /// Why the file walk did not run, in words for the result screen. `None`
    /// when it did run.
    ///
    /// One walk produces both the exclusions and the by-type breakdown, so one
    /// sentence explains both absences. It exists because a copy that quietly
    /// ignored twelve exclusion rules and then reported "Исключено файлов: 0"
    /// tells the user the rules matched nothing — when in fact they were never
    /// applied. That difference is the whole point of having the rules.
    pub file_scan_skipped: Option<String>,
    /// Why the excluded files' catalog entries were NOT deleted from the
    /// image, in words for the result screen — `None` when they were, or when
    /// there was nothing to delete. See `engine_win::DiskBackupSummary::excluded_entries_kept`.
    pub excluded_entries_kept: Option<String>,
    /// The service's key directory was on this volume and is not in the copy
    /// (decision 6/E4) — said in the summary, since it is not one of the
    /// job's own rules.
    pub key_store_excluded: bool,
    /// The removal pass cleared resident content of excluded files through a
    /// driver that keeps its journal: the old bytes may remain in the image's
    /// `$LogFile` (see `EnginePlatform::journal_keeps_cleared_content`).
    pub resident_in_journal: bool,
}

/// Back up a volume into a fresh image, on `platform`.
///
/// The image is written under a private name and becomes a version under its
/// settled name only once the whole pipeline has finished and the caller,
/// asked last ([`may_publish`]), still wants it; a run that fails or is
/// cancelled leaves nothing under a public name.
pub(crate) fn backup_volume<P: EnginePlatform>(
    platform: &P,
    req: &VolumeBackupRequest,
    mut progress: impl FnMut(&BackupProgress) -> Result<()>,
) -> Result<VolumeBackupSummary> {
    check_encrypted_target(req.encryption.as_ref(), req.image_format, &req.image_path)?;
    refuse_existing_outputs(&req.image_path, req.full_image_path.as_deref())?;
    if let Some(parent) = req.image_path.parent() {
        if !parent.as_os_str().is_empty() {
            crate::dest_folder::create(parent)?;
        }
    }

    // Enable SeBackupPrivilege so the exclusion scan can read/list ACL-protected
    // paths (System Volume Information, protected app data). Best-effort.
    platform.prepare_privileges();

    if let Some(keys) = &req.encryption {
        let pending = crate::seal::obk::new_image::NewImage::create(&req.image_path, keys)?;
        let staged = VolumeBackupRequest {
            image_path: pending.path().to_path_buf(),
            ..req.clone()
        };
        let mut summary = backup_volume_inner(
            platform,
            &staged,
            Some(pending.writer_file()?),
            &req.image_path,
            &mut progress,
        )?;
        let published = settled_image_path(
            &req.image_path,
            req.full_image_path.as_deref(),
            req.kind,
            summary.backup_kind,
        );
        may_publish(&mut progress, summary.bytes_copied)?;
        pending.publish_as(&published)?;
        summary.sidecar_path = sidecar_path_for(&published);
        summary.image_path = published;
        return Ok(summary);
    }
    // Written privately and published last, like the encrypted one above (see
    // `pending_image`); a failed run's image goes when `pending` is dropped.
    let pending = crate::pending_image::PendingImage::new(&req.image_path)?;
    let staged = VolumeBackupRequest {
        image_path: pending.path().to_path_buf(),
        ..req.clone()
    };
    let mut summary = backup_volume_inner(platform, &staged, None, &req.image_path, &mut progress)?;
    let published = settled_image_path(
        &req.image_path,
        req.full_image_path.as_deref(),
        req.kind,
        summary.backup_kind,
    );
    may_publish(&mut progress, summary.bytes_copied)?;
    pending.publish_as(&published)?;
    summary.sidecar_path = sidecar_path_for(&published);
    summary.image_path = published;
    Ok(summary)
}

fn backup_volume_inner<P: EnginePlatform>(
    platform: &P,
    req: &VolumeBackupRequest,
    owned_file: Option<std::fs::File>,
    published_path: &Path,
    mut progress: impl FnMut(&BackupProgress) -> Result<()>,
) -> Result<VolumeBackupSummary> {
    let started = Instant::now();
    let source = platform.normalize_source(&req.source);

    // --- VSS shadow copy (live lettered volumes only) ---
    let mut session = platform.open_volume_session(&source, req.use_vss)?;
    // A physical volume held through its logical volumes is assembled by the
    // whole-disk copy only; read here as one device it would not be one
    // point in time.
    if session.physical_volume().is_some() {
        return Err(OwnBackupError::NoSnapshotProvider(
            tr!("engine.pv_volume_copy", volume = source).into_owned(),
        ));
    }
    let read_path = session.read_path().to_string();
    let vss_used = session.snapshot_used();

    let opened = VolumeSource::open(platform, &read_path, req)?;
    let (luks, bitlocker) = (&opened.luks, &opened.bitlocker);
    let (volume_len, sector_size) = (opened.volume_len, opened.sector_size);

    if req.block_size == 0 || !req.block_size.is_multiple_of(sector_size) {
        return Err(OwnBackupError::Config(format!(
            "block size {} is not a multiple of the source sector size {sector_size}",
            req.block_size
        )));
    }
    let total_blocks = volume_len.div_ceil(req.block_size as u64);
    // The container's header is stored in the sidecar (D19), and it is measured
    // in megabytes — the meta partition has to be reserved with that in mind or
    // the metadata write at the very end has nowhere to go.
    let luks_reserve = crate::sidecar::luks_reserve_for(luks.as_ref().map(|p| p.data_offset));
    let meta_size = crate::sidecar::calculate_meta_partition_size(total_blocks, luks_reserve);

    // --- Parent chain (incremental / differential) ---
    // A broken chain must not fail the run: fall back to a full backup and
    // report the fallback in the summary.
    let mut kind = req.kind;
    let mut parent: Option<(PathBuf, Sidecar)> = None;
    if kind != BackupType::Full {
        match &req.parent_image {
            None => {
                warn!("no parent image supplied for a {kind} backup — creating a full backup");
                kind = BackupType::Full;
            }
            Some(parent_path) => {
                match validate_parent(
                    parent_path,
                    req.block_size,
                    volume_len,
                    req.encryption.as_ref(),
                ) {
                    Ok(sidecar) => match child_refusal(
                        req.image_format,
                        req.encryption.is_some(),
                        req.block_size,
                    ) {
                        None => parent = Some((parent_path.clone(), sidecar)),
                        Some(why) => {
                            warn!(
                                parent = %parent_path.display(),
                                reason = %why,
                                "not continuing this chain — creating a full backup instead"
                            );
                            kind = BackupType::Full;
                        }
                    },
                    Err(e) => {
                        warn!(
                            parent = %parent_path.display(),
                            error = %e,
                            "parent chain unusable — creating a full backup instead"
                        );
                        kind = BackupType::Full;
                    }
                }
            }
        }
    }
    // A chain must never cross container formats (decision 2026-07-08): the
    // format of an existing archive cannot be changed — that is a hard error,
    // not a fallback. The job runners refuse such runs before reaching the
    // engine; this guards direct callers.
    let want_container = if req.encryption.is_some() {
        crate::seal::obk::OBK_EXTENSION
    } else {
        req.image_format.extension()
    };
    if let Some((parent_path, parent_sidecar)) = &parent {
        if parent_sidecar.info.container != want_container {
            return Err(OwnBackupError::Config(format!(
                "parent image {} is '{}' but the requested format is '{}' — \
                 changing the format of an existing backup chain is not allowed; \
                 create a new job (or remove the old backups)",
                parent_path.display(),
                parent_sidecar.info.container,
                want_container
            )));
        }
    }

    // The kind is settled: from here on the image is named inside as what it
    // is. Every container is written to its private stage and only published
    // under this name (see `backup_volume`).
    let published = settled_image_path(
        published_path,
        req.full_image_path.as_deref(),
        req.kind,
        kind,
    );
    let published_path: &Path = &published;

    let mut maps = VolumeMaps::build(&opened, &session, req)?;
    let VolumeExclusions {
        mut excluded_files,
        mut excluded_bytes,
        zero_ranges,
        meta_patches,
        mut categories,
        mut file_scan_skipped,
        removal_paths,
        in_image_exclusions,
        entries_kept,
        key_store_excluded,
    } = resolve_exclusions(
        platform,
        &session,
        &read_path,
        req,
        &opened,
        &mut maps,
        &mut progress,
    )?;

    let smart_mode = maps.smart_mode;
    let detected_fs = maps.detected_fs.take();
    let used = maps.into_used(total_blocks);
    let used_blocks = used.used_block_count();

    // --- Create the image target (and attach it, for VHDX) ---
    // Disk = 1 MiB GPT head + volume + meta partition + tail for the backup GPT.
    let disk_size = DEFAULT_PARTITION_OFFSET + volume_len.div_ceil(MIB) * MIB + meta_size + MIB;
    // The image's identity is settled before it is created: an encrypted image
    // seals it into its own root record, and the sidecar repeats it.
    let backup_id = Uuid::new_v4();
    let (mut target, data_offset) = create_volume_target(
        platform,
        req,
        owned_file,
        &parent,
        kind,
        backup_id,
        disk_size,
        sector_size,
    )?;
    let disk_guid = write_volume_gpt(
        &mut target,
        req,
        &parent,
        volume_len,
        sector_size,
        meta_size,
    )?;

    // --- Copy used blocks into the partition ---
    // For child images only blocks that differ from the parent version are
    // written; everything else resolves through the VHDX chain.
    let parent_hashes: Option<HashMap<u64, u64>> = parent.as_ref().map(|(_, sidecar)| {
        sidecar
            .checksums
            .iter()
            .map(|c| (c.block_number, c.checksum))
            .collect()
    });

    let mut reader = volume_block_reader(
        platform,
        &opened,
        &read_path,
        req,
        zero_ranges,
        meta_patches,
    )?;

    let outcome = copy_used_blocks(
        &backup_id.to_string(),
        reader.as_mut(),
        used.as_ref(),
        parent_hashes.as_ref(),
        |block_number, data| {
            target.write_at(data_offset + block_number * req.block_size as u64, data)
        },
        &mut progress,
    )?;

    finish_volume_image::<P>(
        target,
        VolumeImageFacts {
            req,
            source: &source,
            published_path,
            parent: &parent,
            kind,
            backup_id,
            volume_len,
            sector_size,
            total_blocks,
            used_blocks,
            smart_mode,
            data_offset,
            disk_guid: &disk_guid,
            meta_size,
            outcome: &outcome,
        },
        &mut progress,
    )?;

    // Tell the snapshot the copy succeeded and release it: VSS writers'
    // BackupComplete on Windows, the dm/LVM snapshot or freeze on Linux.
    let t = Instant::now();
    if let Some(Err(e)) = session.complete() {
        warn!(error = %e, "snapshot completion failed (backup data is already safe)");
    }
    info!(
        snapshot_release_ms = t.elapsed().as_millis(),
        "snapshot released"
    );

    // --- True removal of excluded files (NTFS/ReFS) ---
    // The FS driver deletes the recorded paths inside the image copy (the
    // source is never touched): catalog entries disappear and the space is
    // reclaimed on a restored volume. Failures before the merge fall back to
    // the zero-filled ghosts — still a valid backup; failures after the merge
    // fail the run (image/checksum mismatch must never be shipped). Checksums
    // of the driver-touched blocks are updated in place, so the sidecar below
    // describes the final image.
    let mut checksums = outcome.checksums;
    let mut used_blocks = used_blocks;
    let bytes_copied_so_far = outcome.bytes_copied;
    // The exclusion passes below run for MINUTES on a big volume (this run:
    // scan 34 s, delete 10 s, zero ~40 s, merge ~1.5 min, re-checksum ~4 min on
    // an HDD) — under the "сброс кэша" label that read as a hang. They get
    // their own phase id, so the interface can say what is actually happening.
    let mut phase_tick = move |id: &'static str, entries: u64, total: u64| {
        let _ = progress(&BackupProgress {
            backup_id: id.into(),
            status: BackupStatus::Running,
            blocks_done: entries,
            blocks_total: total,
            bytes_written: bytes_copied_so_far,
            speed_bps: 0,
            categories: None,
            excluded: None,
        });
    };

    // The source could not be walked, but the IMAGE can be: it holds the
    // volume's NTFS in the clear, so the exclusions happen there instead —
    // matched, deleted by the FS driver, and their clusters zeroed. This is the
    // only path on which a locked BitLocker volume honours the rules at all,
    // and it is where the figures for the result screen come from too.
    let image = FinishedImage {
        path: &req.image_path,
        format: req.image_format,
        encryption: req.encryption.as_ref(),
        block_size: req.block_size,
        zstd_level: req.zstd_level,
    };
    let span = VolumeSpan {
        data_offset,
        volume_len,
    };
    if in_image_exclusions {
        let tick = &mut |n, total| phase_tick("exclusions", n, total);
        let done = platform.exclude_inside_image(
            &image,
            span,
            &req.exclusion_patterns,
            &mut checksums,
            tick,
        )?;
        match done {
            Some(done) => {
                info!(
                    files = done.files_excluded,
                    deleted = done.outcome.entries_deleted,
                    failed = done.outcome.entries_failed,
                    bytes_zeroed = done.bytes_zeroed,
                    "exclusions applied inside the image of an encrypted volume"
                );
                used_blocks = checksums.len() as u64;
                excluded_files = done.files_excluded;
                excluded_bytes = done.bytes_excluded;
                categories = Some(done.categories);
                file_scan_skipped = None;
            }
            None => {
                // Already logged in detail; the result screen must still say it.
                file_scan_skipped = Some(tr!("engine_win.skip_in_image_failed").into_owned());
            }
        }
    }

    let mut excluded_entries_kept: Option<String> = entries_kept;
    let mut resident_in_journal = false;
    if !removal_paths.is_empty() {
        let tick = &mut |n, total| phase_tick("exclusions", n, total);
        let removed =
            platform.remove_from_image(&image, span, &removal_paths, &mut checksums, tick)?;
        if let Some(failed) = removed {
            used_blocks = checksums.len() as u64;
            resident_in_journal =
                super::platform::resident_content_may_remain_in_journal(platform, &removal_paths);
            if failed > 0 {
                excluded_entries_kept =
                    Some(tr!("engine_win.entries_partly_kept", count = failed).into_owned());
            }
        } else {
            excluded_entries_kept = Some(platform.entries_kept_reason());
        }
    }

    // --- Sidecar ---
    // Child sidecars carry the FULL state checksums (every used block, written
    // or inherited), so verify/restore/mount treat the leaf exactly like a
    // full image — the VHDX chain resolves inherited blocks.
    let image_file_name = published_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let manifest = ImageManifest {
        backup_id,
        created_at: chrono::Utc::now().to_rfc3339(),
        backup_type: kind,
        parent_backup_id: parent.as_ref().map(|(_, s)| s.manifest.backup_id),
        source_os: P::SOURCE_OS.into(),
        source_disk: DiskInfo {
            device_id: source.clone(),
            stable_device: String::new(),
            model: String::new(),
            serial: String::new(),
            size_bytes: volume_len,
            sector_size_logical: sector_size,
            sector_size_physical: std::cmp::max(sector_size, 4096),
            media_type: "Volume".into(),
            partition_table: PartitionTable::Gpt,
            disk_guid,
            partitions: vec![],
        },
        boot_mode: BootMode::Uefi, // populated properly with inventory in phase 4
        partition_table: PartitionTable::Gpt,
        compression_algo: match req.image_format {
            ImageFormat::Vhdx => CompressionAlgo::None,
            ImageFormat::Qcow2 => CompressionAlgo::Zstd,
        },
        compression_level: match req.image_format {
            ImageFormat::Vhdx => 0,
            ImageFormat::Qcow2 => crate::qcow2::zstd_level_or_default(req.zstd_level),
        },
        encryption_algo: None,
        encryption_salt: None,
        block_size: req.block_size,
        total_blocks,
        used_blocks,
        backup_mode: if smart_mode {
            BackupMode::UsedSectors
        } else {
            BackupMode::SectorBySector
        },
        checksum_algorithm: "xxhash64".into(),
        preflight_report: None,
        // Encrypted source captured in the clear (used blocks only): record the
        // kind and that encryption must be re-applied after restore. The LUKS
        // header is intentionally not stored — a plaintext capture restores to
        // an unencrypted volume, so the original header is not needed.
        volume_encryption: {
            use own_backup_common::image::{VolumeCaptureMode, VolumeEncryptionState};
            // The restore screen reads this: without it a decrypted BitLocker
            // volume would be shown as plain NTFS with no hint that the
            // original was encrypted and the restored copy will not be (D14).
            let kind = luks
                .as_ref()
                .map(|p| p.kind.clone())
                .or_else(|| bitlocker.as_ref().map(|b| b.params.kind.clone()));
            kind.map(|kind| VolumeEncryptionState {
                kind,
                capture_mode: VolumeCaptureMode::Plaintext,
                luks_header_backup_b64: None,
                note: Some("Decrypted at backup; re-enable encryption after restore".into()),
            })
        },
    };

    let mut sidecar_info = match &parent {
        None => SidecarInfo::new_full(image_file_name, data_offset),
        Some((parent_path, _)) => {
            let role = match kind {
                BackupType::Differential => ChainRole::Differential,
                _ => ChainRole::Incremental,
            };
            let parent_file_name = parent_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            SidecarInfo::new_child(image_file_name, role, parent_file_name, data_offset)
        }
    };
    sidecar_info.container = if req.encryption.is_some() {
        crate::seal::obk::OBK_EXTENSION.into()
    } else {
        req.image_format.extension().into()
    };
    // The job id is what ties images into this job's chains: list_job_chains
    // filters on it, so without it every next run plans a FULL backup.
    sidecar_info.job_id = req.job_id.clone();
    // Who this volume IS, not what it is called: a label or a drive letter is
    // for display, and two volumes can share both (audit AUD-10).
    sidecar_info.source_id = crate::source_identity::stable_source_id(&req.source);
    sidecar_info.source_disk_layout = req.source_disk.clone();
    sidecar_info.categories = categories;
    sidecar_info.excluded_files = excluded_files;
    sidecar_info.excluded_bytes = excluded_bytes;
    sidecar_info.filesystem = detected_fs;
    // The container's own header, kept so a restore can put the encryption
    // back exactly as it was (D19). Failing to read it is not fatal — the
    // copy is still a good copy — but it does mean this image can only be
    // restored decrypted, so it is logged loudly.
    let luks_headers: Vec<own_backup_common::image::LuksHeaderBackup> = match &luks {
        Some(p) => {
            match P::Dev::open_read(&read_path)
                .and_then(|mut d| crate::luks::read_header_area(&mut d, p))
            {
                Ok(bytes) => {
                    use base64::Engine;
                    info!(
                        bytes = bytes.len(),
                        "LUKS header captured — restore can re-encrypt"
                    );
                    vec![own_backup_common::image::LuksHeaderBackup {
                        part_index: 0,
                        data_offset: p.data_offset,
                        sector_size: p.sector_size,
                        kind: p.kind.clone(),
                        header_b64: base64::engine::general_purpose::STANDARD.encode(&bytes),
                    }]
                }
                Err(e) => {
                    warn!(error = %e, "could not read the LUKS header — this copy will only restore decrypted");
                    Vec::new()
                }
            }
        }
        None => Vec::new(),
    };
    sidecar_info.luks_headers = luks_headers;
    let sidecar_path = sidecar_path_for(&req.image_path);

    // M0: Write embedded sidecar v2 to the meta partition in the finished image file
    let sector = sector_size as u64;
    let last_lba = (DEFAULT_PARTITION_OFFSET + volume_len).div_ceil(sector) - 1;
    let meta_offset = (last_lba + 1) * sector;
    let mut blob_bytes = Vec::new();
    crate::sidecar::write_sidecar_v2_to(&mut blob_bytes, &sidecar_info, &manifest, &checksums)?;
    match (&req.encryption, req.image_format) {
        (Some(keys), _) => {
            // No guest meta partition to write into: the sidecar replaces the
            // reserved one inside the container, sealed under the same chain
            // key and with a fresh nonce.
            let chain = keys.open_chain_of(&req.image_path)?;
            crate::seal::obk::metadata::update_unpublished_sealed_metadata(
                &req.image_path,
                &chain.key,
                &blob_bytes,
            )?;
        }
        (None, ImageFormat::Vhdx) => {
            crate::vhdx_bat::write_vhdx_guest_at(&req.image_path, meta_offset, &blob_bytes)?;
        }
        (None, ImageFormat::Qcow2) => {
            crate::qcow2::merge::write_qcow2_guest_blob(&req.image_path, meta_offset, &blob_bytes)?;
        }
    }

    info!(
        image = %req.image_path.display(),
        kind = %kind,
        blocks_written = outcome.blocks_copied,
        blocks_unchanged = outcome.blocks_unchanged,
        bytes = outcome.bytes_copied,
        excluded_files,
        secs = started.elapsed().as_secs(),
        "Volume backup complete"
    );

    Ok(VolumeBackupSummary {
        image_path: req.image_path.clone(),
        sidecar_path,
        source_bytes: volume_len,
        total_blocks,
        used_blocks,
        blocks_copied: outcome.blocks_copied,
        bytes_copied: outcome.bytes_copied,
        duration: started.elapsed(),
        smart_mode,
        vss_used,
        backup_kind: kind,
        blocks_unchanged: outcome.blocks_unchanged,
        excluded_files,
        excluded_bytes,
        categories,
        file_scan_skipped,
        excluded_entries_kept,
        key_store_excluded,
        resident_in_journal,
    })
}

#[cfg(test)]
#[path = "volume_tests.rs"]
mod tests;
