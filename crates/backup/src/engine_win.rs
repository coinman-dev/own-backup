//! Windows volume backup/restore engine (plan v2, phase 1).
//!
//! Ties the building blocks together:
//! [`WinBlockReader`] → [`NtfsUsedSectorMap`] → [`copy_used_blocks`] →
//! attached VHDX ([`crate::vhdx`]) + GPT ([`crate::gpt_writer`]) + sidecar.
//!
//! v1 scope: **volume** backups (`C:`, `\\.\D:`, VSS snapshot paths). The
//! image is a GPT disk with a single Basic Data partition holding the raw
//! volume — directly mountable by Windows. Whole-disk backups and VSS
//! integration come in later phases.
//!
//! The volume copy itself is the shared one (`engine_core::volume`), run on
//! [`WinPlatform`].

#![cfg(target_os = "windows")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};
use uuid::Uuid;

use own_backup_common::gpt::DiskBackupManifest;
use own_backup_common::progress::{BackupProgress, RestoreProgress};
use own_backup_common::types::BackupStatus;
use own_backup_common::{tr, OwnBackupError, Result};

use crate::block_source::BlockSource;
use crate::checksum::compute_block_checksum;
use crate::gpt_writer::{disk_gpt_write_ops, DEFAULT_PARTITION_OFFSET};
use crate::sidecar::{read_embedded_sidecar, read_sidecar, sidecar_path_for, write_sidecar};
use crate::vhdx::{
    attach_vhdx, create_differencing_vhdx, detach_vhdx, merge_into_parent, AttachMode,
};
use crate::vhdx_bat::allocated_payload_blocks;
use crate::win_device::{normalize_device_path, WinRawDevice};

// The platform-independent half of this engine lives in `engine_core`, shared
// with the Linux engine; the `pub` names are the ones this engine's callers know.
pub use crate::engine_core::disk_image::{DiskBackupRequest, DiskPartitionBackup};
pub use crate::engine_core::disk_target::DiskBackupSummary;
pub(crate) use crate::engine_core::read::ImageReadSource;
pub use crate::engine_core::read::{load_sidecar_of, load_sidecar_with_keys};
use crate::engine_core::source::{disk_sector_size, disk_window, is_bitlocker_locked};
#[cfg(test)]
use crate::engine_core::target::ImageTarget;
pub use crate::engine_core::target::DEFAULT_BLOCK_SIZE;
pub use crate::engine_core::verify::{
    verify_image, verify_image_scoped, verify_image_with_keys, VerifyScope, VerifySummary,
};
pub use crate::engine_core::volume::{VolumeBackupRequest, VolumeBackupSummary};
use crate::engine_platform_win::WinPlatform;

#[cfg(test)]
#[path = "engine_namespace_tests.rs"]
mod new_image_namespace_tests;

/// Open a freshly-attached virtual disk for writing; the PhysicalDrive device
/// node can take a moment to appear after AttachVirtualDisk.
fn open_write_with_retry(path: &str) -> Result<WinRawDevice> {
    let mut last_err: Option<OwnBackupError> = None;
    for attempt in 0..20 {
        match WinRawDevice::open_write(path) {
            Ok(dev) => return Ok(dev),
            Err(e) => {
                last_err = Some(e);
                std::thread::sleep(Duration::from_millis(100 + 50 * attempt));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| OwnBackupError::Device(format!("cannot open {path}"))))
}

/// Back up a volume into a freshly created, directly mountable VHDX.
/// Данные исключённых файлов в копию не попадают в любом случае — они
/// вычитаются из карты ДО копирования. Записи из каталога образа убирает
/// драйвер файловой системы, и чтобы он их увидел, образ надо смонтировать:
/// VHDX монтирует сама Windows, qcow2 — наша iSCSI-цель ([`crate::attach_win`]).
/// Ставить с этим никаких условий больше не нужно: с 2026-09-18 стороннего
/// драйвера нет, а инициатор есть в любой Windows.
#[cfg(target_os = "windows")]
pub fn backup_volume(
    req: &VolumeBackupRequest,
    progress: impl FnMut(&BackupProgress) -> Result<()>,
) -> Result<VolumeBackupSummary> {
    crate::engine_core::volume::backup_volume(&WinPlatform, req, progress)
}

/// Открыть заблокированный том силами Windows и закрыть его диск на запись.
///
/// Возвращает стража: пока он жив, диск нельзя изменить. Держать его надо до
/// конца копирования — том остаётся смонтированным, и Windows пишет в него,
/// когда ей вздумается (журнал, метки времени, System Volume Information).
pub(crate) fn unlock_source_with_windows(
    read_path: &str,
    passphrase: &str,
) -> Result<crate::windows_unlock_win::ReadOnlyDisk> {
    use crate::windows_unlock_win as wu;

    let extents = crate::volume_win::volume_extents(read_path);
    let [extent] = extents.as_slice() else {
        return Err(OwnBackupError::Device(
            tr!(
                "engine_win.windows_unlock_multi_extent",
                volume = read_path,
                count = extents.len()
            )
            .into_owned(),
        ));
    };
    let guard = wu::ReadOnlyDisk::arm(extent.disk_number)?;

    // 48 цифр — это ключ восстановления, что бы ни было между ними; всё
    // остальное считаем парольной фразой.
    let digits: String = passphrase.chars().filter(char::is_ascii_digit).collect();
    let is_key = digits.len() == 48
        && passphrase
            .chars()
            .all(|c| c.is_ascii_digit() || c == '-' || c == ' ');
    let dashed;
    let secret = if is_key {
        dashed = digits
            .as_bytes()
            .chunks(6)
            .map(|c| String::from_utf8_lossy(c).to_string())
            .collect::<Vec<_>>()
            .join("-");
        wu::Secret::RecoveryPassword(&dashed)
    } else {
        wu::Secret::Passphrase(passphrase)
    };
    wu::unlock_with_windows(read_path, secret)?;

    // Разблокировка и монтирование — разные события. FVE отдаёт ключ сразу, а
    // том появляется на своём устройстве через мгновение, и читать его в этот
    // промежуток бессмысленно: устройство ещё не отвечает. Ждём, пока с него
    // прочтётся хотя бы загрузочный сектор — не дольше пятнадцати секунд.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let mut last_err = None;
    loop {
        match WinRawDevice::open_read(read_path).and_then(|mut d| {
            let mut boot = vec![0u8; 512];
            crate::fat_map::RawVolume::read_at(&mut d, 0, &mut boot)?;
            // Нули значат «устройство есть, а тома на нём ещё нет».
            if boot.iter().all(|&b| b == 0) {
                return Err(OwnBackupError::Device(
                    tr!("engine_win.volume_not_mounted_yet").into_owned(),
                ));
            }
            Ok(())
        }) {
            Ok(()) => break,
            Err(e) => {
                if std::time::Instant::now() >= deadline {
                    warn!(error = %e, "том открыт, но за 15 с так и не стал читаемым");
                    last_err = Some(e);
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
        }
    }
    if let Some(e) = last_err {
        return Err(e);
    }
    info!(
        volume = read_path,
        "том открыт силами Windows — дальше он обычный"
    );
    Ok(guard)
}

// ===========================================================================
// Whole-disk backup: one image per disk (plan D1, .ai/whole-disk-single-image)
// ===========================================================================

/// Back up a whole disk (several partitions of one physical drive) into ONE
/// bootable image with the real GPT + an embedded restore manifest. Used
/// sectors only (never a raw disk dump); each partition's data lands at its
/// real offset, so mounting the image shows every volume with a drive letter.
///
/// Scope: full + incremental/differential chains (plan D5); NTFS/FAT/ext/btrfs/
/// xfs + LUKS. File exclusions (plan D2) drop + zero excluded clusters per
/// partition, and patch FAT/exFAT metadata; NTFS/ReFS catalog true-removal
/// inside the whole-disk image (mount-&-delete across volumes) is not yet done —
/// excluded NTFS files remain as zero-filled ghosts (data still never leaves).
pub fn backup_disk(
    req: &DiskBackupRequest,
    progress: impl FnMut(&BackupProgress) -> Result<()>,
) -> Result<DiskBackupSummary> {
    crate::engine_core::disk::backup_disk(&WinPlatform, req, progress)
}

/// Parameters for a volume restore.
pub struct VolumeRestoreRequest {
    /// Backup image (`.vhdx`) with its `.obkm` sidecar next to it.
    pub image_path: PathBuf,
    /// Target volume/device to overwrite (`D:`, `\\.\D:`). DESTRUCTIVE.
    pub target: String,
    /// Verify each block's checksum while reading from the image.
    pub verify_checksums: bool,
    /// Keys for an encrypted image: a password's or the recovery key's when a
    /// person restores by hand, the service's own for an automatic check.
    pub encryption: Option<crate::seal::context::ArchiveKeys>,
}

/// Outcome of a completed restore.
pub struct VolumeRestoreSummary {
    pub blocks_restored: u64,
    pub bytes_restored: u64,
    pub duration: Duration,
}

/// Turn a restore target into the device path that can actually take the whole
/// volume, plus whatever had to be taken down to allow it.
///
/// Returns a `part:<disk>:<offset>:<len>` window when the target is a volume
/// sitting on exactly one partition, and the target unchanged otherwise — a
/// raw device path, a `part:` spec the caller already built, or a volume
/// spanning several extents (a spanned or mirrored set, which is not a
/// partition and must not be guessed at).
///
/// The returned guard holds the dismount. Dropping it early lets Windows mount
/// the volume back on top of a restore still in progress, so it is bound to the
/// write, not to this function.
#[cfg(target_os = "windows")]
fn partition_window_for(target: &str) -> (String, Option<TakenVolume>) {
    let plain = normalize_device_path(target);
    if target.starts_with("part:") {
        return (plain, None);
    }
    let Some(root) = crate::volume_win::volume_root_for_path(target)
        .or_else(|| crate::volume_win::volume_root_for_path(&format!("{target}\\")))
    else {
        return (plain, None);
    };
    let extents = crate::volume_win::volume_extents(&root);
    let [e] = extents.as_slice() else {
        warn!(
            target,
            extents = extents.len(),
            "restore: the target is not one partition — writing through the volume device"
        );
        return (plain, None);
    };
    let guard = crate::volume_win::VolumeDismountGuard::for_restore_range(
        e.disk_number,
        e.offset,
        e.length,
    );
    if !guard.failed.is_empty() {
        warn!(volumes = ?guard.failed, "restore: some volumes would not dismount");
    }
    info!(
        target,
        disk = e.disk_number,
        offset = e.offset,
        length = e.length,
        "restore: writing through the partition rather than the volume device"
    );
    let taken = TakenVolume {
        guard: Some(guard),
        give_back: crate::source_identity::direct_drive_letter(target).map(|l| (root, l)),
    };
    (
        format!("part:{}:{}:{}", e.disk_number, e.offset, e.length),
        Some(taken),
    )
}

/// A volume taken down for a restore, and the letter it is owed back.
///
/// The dismount guard strips every letter it takes down — right for a
/// whole-disk restore, whose plan assigns letters afresh afterwards, and wrong
/// here: restoring onto `H:` left a volume with the right data and NO letter.
/// Measured on the stand: `Get-Volume -DriveLetter H` empty straight after, the
/// restored file still absent from `H:\` sixty seconds later, and all thirty
/// files present when the same partition was read from outside. The person
/// would have been looking at a missing drive after a restore that worked.
///
/// On drop, in this order: release the lock, then give the letter back. The
/// order matters — a volume still locked by us cannot be mounted under it. And
/// it happens on drop so a failed restore returns the letter too: the volume
/// was theirs before we touched it.
#[cfg(target_os = "windows")]
struct TakenVolume {
    guard: Option<crate::volume_win::VolumeDismountGuard>,
    give_back: Option<(String, char)>,
}

#[cfg(target_os = "windows")]
impl Drop for TakenVolume {
    fn drop(&mut self) {
        drop(self.guard.take());
        if let Some((root, letter)) = self.give_back.take() {
            match crate::letters_win::assign_letter(&root, letter) {
                Ok(()) => info!(volume = %root, %letter, "restore: the target has its letter back"),
                Err(e) => warn!(
                    volume = %root, %letter, error = %e,
                    "restore: the target's letter could not be given back — the data is \
                     written; assign it in Disk Management"
                ),
            }
        }
    }
}

/// Restore a volume image onto a target device. Overwrites the target!
pub fn restore_volume(
    req: &VolumeRestoreRequest,
    mut progress: impl FnMut(&RestoreProgress) -> Result<()>,
) -> Result<VolumeRestoreSummary> {
    let started = Instant::now();
    let sidecar = load_sidecar_with_keys(&req.image_path, req.encryption.as_ref())?;
    let block_size = sidecar.manifest.block_size as u64;
    let volume_len = sidecar.manifest.source_disk.size_bytes;
    let data_offset = sidecar.info.volume_data_offset;

    // A volume target is written through its PARTITION, not through the volume
    // device. Measured, not assumed: writing `\\.\H:` at the last block of a
    // 1 GiB volume got `wanted 1048576 wrote 1044480`, with the device itself
    // reporting exactly 1073741824 bytes and the write ending exactly there.
    // Windows caps a raw write through a volume handle four kilobytes short of
    // the length it advertises, so the tail — NTFS's backup boot sector — can
    // never be written that way, and every restore of a full volume ended in an
    // error message about work that had in fact succeeded.
    //
    // The whole-disk road never had this: it writes through the disk at the
    // partition's offset. This takes the same road, and takes the volume down
    // first so Windows stops guarding those sectors on its behalf.
    let (target, _taken) = partition_window_for(&req.target);
    let mut target_dev = WinRawDevice::open_write(&target)?;
    info!(
        target = %target,
        target_bytes = target_dev.length(),
        source_bytes = volume_len,
        block_size,
        "restore: target opened"
    );
    // Nothing may hold the target's sectors while they are overwritten. Windows
    // refuses a raw write into a MOUNTED volume outright — `WriteFile(\\.\H:)
    // ... 0x80070005` at offset 0, before a single block — so a restore onto an
    // ordinary target failed for the ordinary reason that it was mounted. The
    // whole-disk road has always taken its volumes down (`TargetWriter`); this
    // one, the simplest operation there is, never did.
    //
    // After the open and on the SAME handle: the lock belongs to the handle
    // that holds it, and taking it on another one only locks this writer out.
    //
    // A no-op for the partition window above — that handle is the disk, whose
    // volumes the guard already took down — and the fallback for a target that
    // could not be resolved to one.
    target_dev.lock_and_dismount();
    if target_dev.length() < volume_len {
        return Err(OwnBackupError::RestoreIncompatible(format!(
            "target {} is {} bytes, smaller than the backed-up volume ({} bytes)",
            target,
            target_dev.length(),
            volume_len
        )));
    }
    if block_size % target_dev.sector_size() as u64 != 0 {
        return Err(OwnBackupError::RestoreIncompatible(format!(
            "image block size {block_size} does not match target sector size {}",
            target_dev.sector_size()
        )));
    }

    let chain_key = match (&req.encryption, sidecar.info.container.as_str()) {
        (Some(keys), "obk") => Some(keys.open_chain_of(&req.image_path)?.key),
        _ => None,
    };
    let mut image_dev =
        ImageReadSource::open_with(&req.image_path, &sidecar.info.container, chain_key.as_ref())?;

    let restore_id = Uuid::new_v4().to_string();
    let total = sidecar.checksums.len() as u64;
    let mut blocks_restored = 0u64;
    let mut bytes_restored = 0u64;
    let mut buf = vec![0u8; block_size as usize];

    for entry in &sidecar.checksums {
        let offset = entry.block_number * block_size;
        let len = std::cmp::min(block_size, volume_len - offset) as usize;

        let data = &mut buf[..len];
        image_dev.read_at(data_offset + offset, data)?;

        if req.verify_checksums {
            let actual = compute_block_checksum(data);
            if actual != entry.checksum {
                return Err(OwnBackupError::ChecksumMismatch {
                    block_index: entry.block_number,
                    expected: entry.checksum,
                    actual,
                });
            }
        }

        target_dev.write_at(offset, data)?;
        blocks_restored += 1;
        bytes_restored += len as u64;

        let elapsed = started.elapsed().as_secs_f64();
        progress(&RestoreProgress {
            restore_id: restore_id.clone(),
            status: BackupStatus::Running,
            blocks_done: blocks_restored,
            blocks_total: total,
            bytes_written: bytes_restored,
            speed_bps: if elapsed > 0.0 {
                (bytes_restored as f64 / elapsed) as u64
            } else {
                0
            },
        })?;
    }

    target_dev.flush()?;
    drop(image_dev);

    info!(
        image = %req.image_path.display(),
        target = %target,
        blocks = blocks_restored,
        "Volume restore complete"
    );

    Ok(VolumeRestoreSummary {
        blocks_restored,
        bytes_restored,
        duration: started.elapsed(),
    })
}

// ---------------------------------------------------------------------------
// Whole-disk (bootable) restore
// ---------------------------------------------------------------------------

/// Restore a whole disk from a `.obdm` manifest onto a target physical drive.
/// DESTRUCTIVE — the target is repartitioned and every partition overwritten.
pub struct DiskRestoreRequest {
    /// Path to the `.obdm` disk-backup manifest (its images live alongside).
    pub manifest_path: PathBuf,
    /// Target physical drive number (`\\.\PhysicalDrive{target_disk}`).
    pub target_disk: u32,
    pub verify_checksums: bool,
    /// Keys for an encrypted image. Opened from a password or a recovery key
    /// by whoever asked for the restore — never from the service's own store,
    /// which exists so scheduled COPIES need no one present (§7.4).
    pub encryption: Option<crate::seal::context::ArchiveKeys>,
}

/// Progress for a whole-disk restore.
pub struct DiskRestoreProgress {
    /// "gpt" while writing the partition table, "partition" while copying data.
    pub phase: String,
    /// 0-based index of the partition being restored, and the total count.
    pub partition_index: usize,
    pub partition_count: usize,
    pub part_label: String,
    pub blocks_done: u64,
    pub blocks_total: u64,
    /// Bytes written across the whole disk so far.
    pub bytes_written: u64,
}

pub struct DiskRestoreSummary {
    pub partitions: usize,
    pub bytes_written: u64,
    pub duration: Duration,
}

/// Recreate a disk from its `.obdm`: write the captured GPT, then restore each
/// partition's image into its original offset. The target is taken offline for
/// the duration so Windows does not auto-mount (and lock) partitions mid-write,
/// then brought back online. The GUID-preserving GPT keeps a UEFI system
/// bootable without touching the BCD.
///
/// The caller is responsible for the destructive confirmation and the
/// [`crate`]-external system-disk guard (`own_backup_restore::guard`).
pub fn restore_disk(
    req: &DiskRestoreRequest,
    progress: impl FnMut(&DiskRestoreProgress) -> Result<()>,
) -> Result<DiskRestoreSummary> {
    // A single whole-disk image (plan D1) carries its GPT + data + manifest
    // inside one file; the legacy path takes a `.obdm` + per-volume images.
    let ext = req
        .manifest_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase();
    if matches!(ext.as_str(), "vhdx" | "avhdx" | "qcow2") || ext == crate::seal::obk::OBK_EXTENSION
    {
        return restore_disk_from_image(req, progress);
    }
    restore_disk_legacy(req, progress)
}

/// Restore a whole disk from a single embedded-manifest image (plan D3): copy
/// the image's used blocks to the target at their real offsets, then overlay
/// the REAL GPT (real disk + partition GUIDs from the manifest — the image
/// itself carries random GUIDs to avoid colliding with the live source disk).
/// The real GUIDs keep a UEFI system bootable without touching the BCD.
fn restore_disk_from_image(
    req: &DiskRestoreRequest,
    mut progress: impl FnMut(&DiskRestoreProgress) -> Result<()>,
) -> Result<DiskRestoreSummary> {
    let started = Instant::now();
    let ext = req
        .manifest_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase();
    // An encrypted image keeps everything below inside the container, sealed:
    // the disk layout, the partition list and the checksums this restore reads
    // block by block. Without the key there is nothing here to act on, and
    // `load_sidecar_with_keys` says so by name.
    let chain_key = match (&req.encryption, ext.as_str()) {
        (Some(keys), _) => Some(keys.open_chain_of(&req.manifest_path)?.key),
        (None, _) => None,
    };
    let sidecar = match &chain_key {
        Some(key) => crate::sidecar::read_sealed_sidecar(&req.manifest_path, key)?,
        None => read_embedded_sidecar(&req.manifest_path, &ext)?,
    };
    let dm = sidecar.disk_manifest.clone().ok_or_else(|| {
        OwnBackupError::ImageFormat(
            "image has no embedded whole-disk manifest — not a bootable disk image".into(),
        )
    })?;
    let layout = &dm.layout; // real GUIDs
    let bs = sidecar.manifest.block_size as u64;
    let image_size = sidecar.manifest.source_disk.size_bytes;

    let target_path = format!(r"\\.\PhysicalDrive{}", req.target_disk);
    let target_size = WinRawDevice::open_read(&target_path)?.length();
    // Validates every partition fits the target (and rejects a smaller disk).
    let ops = disk_gpt_write_ops(layout, target_size)?;

    info!(
        target = %target_path,
        model = %dm.disk_model,
        partitions = dm.images.len(),
        "Whole-disk restore (embedded manifest): copying data"
    );

    let mut image = ImageReadSource::open_with(
        &req.manifest_path,
        &sidecar.info.container,
        chain_key.as_ref(),
    )?;
    let mut td = WinRawDevice::open_write(&target_path)?;
    td.set_offline_writable(false)?;

    // Copy every stored image block to the same offset on the target. A block
    // straddling the GPT head carries the image's random GPT bytes — harmless,
    // the real GPT below overwrites the GPT sectors while the partition data
    // stays. (data_offset is 0 for a whole-disk image → block N is at N*bs.)
    let total = sidecar.checksums.len() as u64;
    let mut done = 0u64;
    let mut bytes = 0u64;
    let mut buf = vec![0u8; bs as usize];
    for entry in &sidecar.checksums {
        let offset = entry.block_number * bs;
        let len = bs.min(image_size - offset) as usize;
        if offset + len as u64 > target_size {
            return Err(OwnBackupError::RestoreIncompatible(format!(
                "image block at {offset} exceeds the target disk size {target_size}"
            )));
        }
        let data = &mut buf[..len];
        image.read_at(offset, data)?;
        if req.verify_checksums {
            let actual = compute_block_checksum(data);
            if actual != entry.checksum {
                return Err(OwnBackupError::ChecksumMismatch {
                    block_index: entry.block_number,
                    expected: entry.checksum,
                    actual,
                });
            }
        }
        td.write_at(offset, data)?;
        done += 1;
        bytes += len as u64;
        progress(&DiskRestoreProgress {
            phase: "partition".into(),
            partition_index: 0,
            partition_count: dm.images.len(),
            part_label: tr!("engine_win.label_partition_data").into_owned(),
            blocks_done: done,
            blocks_total: total,
            bytes_written: bytes,
        })?;
    }

    // Overlay the REAL GPT last, so the restored disk carries the source's disk
    // and partition GUIDs (bootable) rather than the image's random ones.
    progress(&DiskRestoreProgress {
        phase: "gpt".into(),
        partition_index: dm.images.len(),
        partition_count: dm.images.len(),
        part_label: tr!("engine_win.label_partition_table").into_owned(),
        blocks_done: total,
        blocks_total: total,
        bytes_written: bytes,
    })?;
    for op in &ops {
        td.write_at(op.offset, &op.bytes)?;
    }
    td.flush()?;
    let _ = td.update_properties();
    drop(td);
    drop(image);

    if let Ok(td) = WinRawDevice::open_write(&target_path) {
        let _ = td.set_online_this_session();
        let _ = td.update_properties();
    }
    info!(
        target = %target_path,
        partitions = dm.images.len(),
        bytes,
        secs = started.elapsed().as_secs(),
        "Whole-disk restore (embedded manifest) complete"
    );
    Ok(DiskRestoreSummary {
        partitions: dm.images.len(),
        bytes_written: bytes,
        duration: started.elapsed(),
    })
}

/// Legacy whole-disk restore from a `.obdm` manifest + per-volume images.
fn restore_disk_legacy(
    req: &DiskRestoreRequest,
    mut progress: impl FnMut(&DiskRestoreProgress) -> Result<()>,
) -> Result<DiskRestoreSummary> {
    let started = Instant::now();

    let raw = std::fs::read(&req.manifest_path).map_err(|e| {
        OwnBackupError::ImageFormat(format!(
            "cannot read disk manifest {}: {e}",
            req.manifest_path.display()
        ))
    })?;
    let manifest: DiskBackupManifest = serde_json::from_slice(&raw)
        .map_err(|e| OwnBackupError::ImageFormat(format!("bad .obdm manifest: {e}")))?;
    let folder = req
        .manifest_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_default();
    let layout = &manifest.layout;
    let sector = layout.sector_size as u64;

    let target_path = format!(r"\\.\PhysicalDrive{}", req.target_disk);

    // Probe the target size and build the GPT (validates every partition fits).
    let target_size = WinRawDevice::open_read(&target_path)?.length();
    let ops = disk_gpt_write_ops(layout, target_size)?;

    info!(
        target = %target_path,
        model = %manifest.disk_model,
        partitions = manifest.images.len(),
        "Whole-disk restore: writing GPT"
    );
    progress(&DiskRestoreProgress {
        phase: "gpt".into(),
        partition_index: 0,
        partition_count: manifest.images.len(),
        part_label: tr!("engine_win.label_partition_table").into_owned(),
        blocks_done: 0,
        blocks_total: 0,
        bytes_written: 0,
    })?;

    // Take the target offline (session-only), write the protective MBR + GPT,
    // then refresh so the OS re-reads the (offline) table.
    {
        let mut td = WinRawDevice::open_write(&target_path)?;
        td.set_offline_writable(false)?;
        for op in &ops {
            td.write_at(op.offset, &op.bytes)?;
        }
        td.flush()?;
        let _ = td.update_properties();
    }

    // Restore each partition image into its window on the (offline) target.
    let count = manifest.images.len();
    let mut total_bytes = 0u64;
    for (i, img) in manifest.images.iter().enumerate() {
        let entry = layout
            .partitions
            .iter()
            .find(|p| p.index == img.part_index)
            .ok_or_else(|| {
                OwnBackupError::RestoreIncompatible(format!(
                    "manifest references partition {} which is not in the GPT layout",
                    img.part_index
                ))
            })?;
        let off = entry.first_lba * sector;
        let len = (entry.last_lba + 1 - entry.first_lba) * sector;
        let target = format!("part:{}:{off}:{len}", req.target_disk);
        let label = if entry.name.is_empty() {
            tr!(
                "engine_win.label_partition",
                index = img.part_index,
                fs = img.filesystem
            )
            .into_owned()
        } else {
            format!("{} ({})", entry.name, img.filesystem)
        };
        info!(
            image = %img.image_file_name,
            part = img.part_index,
            offset = off,
            "Whole-disk restore: restoring partition"
        );

        let vreq = VolumeRestoreRequest {
            image_path: folder.join(&img.image_file_name),
            target,
            verify_checksums: req.verify_checksums,
            // Whole-disk images are never encrypted (the backup path refuses
            // to make one), so their partitions need no key either.
            encryption: None,
        };
        let base = total_bytes;
        let summary = restore_volume(&vreq, |p| {
            progress(&DiskRestoreProgress {
                phase: "partition".into(),
                partition_index: i,
                partition_count: count,
                part_label: label.clone(),
                blocks_done: p.blocks_done,
                blocks_total: p.blocks_total,
                bytes_written: base + p.bytes_written,
            })
        })?;
        total_bytes += summary.bytes_restored;
    }

    // Bring the target back online and refresh so the restored volumes appear.
    if let Ok(td) = WinRawDevice::open_write(&target_path) {
        let _ = td.set_online_this_session();
        let _ = td.update_properties();
    }

    info!(
        target = %target_path,
        partitions = count,
        bytes = total_bytes,
        secs = started.elapsed().as_secs(),
        "Whole-disk restore complete"
    );
    Ok(DiskRestoreSummary {
        partitions: count,
        bytes_written: total_bytes,
        duration: started.elapsed(),
    })
}

/// What [`finalize_boot`] left behind, for the restore report.
#[derive(Debug, Default)]
pub struct BootFinalizeReport {
    /// The entry `bcdboot` left as the store's default — the one that boots the
    /// restored Windows. `None` when the store could not be read.
    pub default_id: Option<String>,
    /// Windows loader entries the store held before the menu was trimmed
    /// (recovery/ramdisk entries not counted — those never show in the menu).
    pub loaders_found: usize,
    /// The boot menu was reduced to that single entry.
    pub menu_trimmed: bool,
    /// `\Windows\bootstat.dat` was removed, so the first boot does not open the
    /// recovery screen.
    pub boot_status_cleared: bool,
    /// Why the menu was left as it was, when it was.
    pub note: Option<String>,
}

/// Recreate the boot files (and, on UEFI, the firmware NVRAM entry) for a
/// restored Windows via `bcdboot`, then leave exactly ONE entry in the boot
/// menu.
///
/// Needed only for a **fresh disk / new hardware** — a same-disk restore that
/// preserved the partition GUIDs boots as-is. `windows_volume` is the restored
/// OS volume ("C:"); `system_volume` is the EFI System Partition mount ("S:")
/// for UEFI (omit to let bcdboot pick the current one). Typically run from
/// WinPE.
///
/// The menu step is not cosmetic. The ESP is restored from the copy, so it
/// arrives carrying the SOURCE machine's BCD, whose loader entries name the
/// source disk's GPT GUIDs; the restored disk gets its own, so those entries
/// resolve to nothing. `bcdboot` adds a correct entry and makes it the default
/// but leaves the inherited ones in the display order — which is why a restored
/// Windows offered two identical "Windows 10" items, one booting and one dying
/// with `0xc000000e \Windows\system32\winload.efi` (2026-07-30). Trimming the
/// display order to the default hides them without deleting anything: the
/// entries stay in the store for anyone who wants to look.
pub fn finalize_boot(
    windows_volume: &str,
    system_volume: Option<&str>,
    uefi: bool,
) -> Result<BootFinalizeReport> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let win_dir = format!(r"{}\Windows", windows_volume.trim_end_matches(['\\', '/']));
    let run = |program: &str| -> Result<std::result::Result<(), String>> {
        let mut cmd = std::process::Command::new(program);
        cmd.arg(&win_dir);
        if let Some(esp) = system_volume {
            cmd.args(["/s", esp]);
        }
        cmd.args(["/f", if uefi { "UEFI" } else { "BIOS" }]);
        cmd.creation_flags(CREATE_NO_WINDOW);
        let out = cmd
            .output()
            .map_err(|e| OwnBackupError::Device(format!("failed to run bcdboot: {e}")))?;
        if out.status.success() {
            return Ok(Ok(()));
        }
        // bcdboot says what went wrong on STDOUT and leaves stderr empty: the
        // report used to end in "bcdboot failed (exit -2146885629): " with
        // nothing after the colon. Both streams, in the console's OEM code page.
        let said = [out.stdout.as_slice(), out.stderr.as_slice()]
            .iter()
            .map(|b| crate::win_device::oem_text(b).trim().to_string())
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        warn!(program, win_dir, uefi, exit = ?out.status.code(), output = %said, "bcdboot failed");
        Ok(Err(format!(
            "exit {}: {said}",
            out.status.code().unwrap_or(-1)
        )))
    };
    // The medium's own bcdboot first: its boot files are the newest. That is
    // also what can go wrong — the bcdboot of WinPE 26100 refused a restored
    // Windows 10 19041 with "Failure when attempting to copy boot files"
    // (0x80092003, 2026-10-16). The restored system then gets its OWN bcdboot,
    // the one that knows its boot files; Microsoft documents running it as
    // `<drive>\Windows\System32\bcdboot`.
    if let Err(first) = run("bcdboot")? {
        let own = format!(r"{win_dir}\System32\bcdboot.exe");
        if !std::path::Path::new(&own).is_file() {
            return Err(OwnBackupError::RestoreIncompatible(format!(
                "bcdboot failed ({first})"
            )));
        }
        if let Err(second) = run(&own)? {
            return Err(OwnBackupError::RestoreIncompatible(format!(
                "bcdboot failed ({first}); the restored system's own bcdboot failed too ({second})"
            )));
        }
        info!(
            win_dir,
            "bcdboot: the restored system's own bcdboot recreated the boot files"
        );
    }
    info!(win_dir, uefi, "bcdboot: boot files recreated");

    let boot_status_cleared = clear_boot_status(windows_volume);

    let mut report = match system_volume {
        Some(esp) => trim_boot_menu(esp, uefi, windows_volume),
        // Without /s we do not know which store bcdboot wrote, and the "current"
        // store belongs to whatever is running this (WinPE, or the live OS) —
        // never something to edit blind.
        None => BootFinalizeReport {
            note: Some(tr!("engine_win.note_no_esp").into_owned()),
            ..Default::default()
        },
    };
    report.boot_status_cleared = boot_status_cleared;
    Ok(report)
}

/// Remove the restored system's `\Windows\bootstat.dat`.
///
/// A copy is taken from a *running* Windows (VSS), so the shutdown it records
/// never happened: on the first boot the boot manager reads that file, decides
/// the machine "couldn't turn off properly" and stops at the recovery screen
/// with `0xc0000001`. Pressing Enter boots it and the state clears itself, but
/// a restore that ends in an error screen reads as a failed restore.
///
/// Windows recreates the file on the next boot, which is why this is preferred
/// over `bootstatuspolicy IgnoreShutdownFailures`: that setting would silence
/// genuine shutdown failures on the machine for good.
///
/// Best-effort — a copy whose Windows directory is spelled differently, or a
/// file that will not go, simply leaves the one-time recovery screen in place.
fn clear_boot_status(windows_volume: &str) -> bool {
    let path = format!(
        r"{}\Windows\bootstat.dat",
        windows_volume.trim_end_matches(['\\', '/'])
    );
    if !std::path::Path::new(&path).exists() {
        debug!(path = %path, "boot status: nothing to clear");
        return false;
    }
    if let Err(e) = std::fs::remove_file(&path) {
        // The file is hidden + system; that alone does not block a delete, but
        // a read-only attribute does.
        let wide = crate::win_device::to_wide(&path);
        // SAFETY: `wide` is NUL-terminated and outlives the call.
        let _ = unsafe {
            windows::Win32::Storage::FileSystem::SetFileAttributesW(
                windows::core::PCWSTR(wide.as_ptr()),
                windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL,
            )
        };
        if let Err(again) = std::fs::remove_file(&path) {
            warn!(
                path = %path, error = %e, retry_error = %again,
                "boot status: could not remove bootstat.dat — the first boot may stop at the \
                 recovery screen once"
            );
            return false;
        }
    }
    info!(
        path = %path,
        "boot status: removed bootstat.dat so the first boot does not open the recovery screen"
    );
    true
}

/// Run `bcdedit` with the given arguments; stdout on success, a message on
/// failure. The output is read as lossy UTF-8 on purpose: everything this code
/// looks for (identifiers, `partition=X:`) is ASCII, while bcdedit's prose is
/// localised and in the console code page.
fn bcdedit(args: &[&str]) -> std::result::Result<String, String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let out = std::process::Command::new("bcdedit")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| tr!("engine_win.bcdedit_start_failed", error = e).into_owned())?;
    if !out.status.success() {
        return Err(tr!(
            "engine_win.bcdedit_failed",
            args = args.join(" "),
            code = out.status.code().unwrap_or(-1),
            output = String::from_utf8_lossy(&out.stderr).trim()
        )
        .into_owned());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The first `{guid}` in `text`, as bcdedit prints identifiers.
fn first_identifier(text: &str) -> Option<String> {
    let start = text.find('{')?;
    let end = text[start..].find('}')? + start;
    let id = &text[start..=end];
    // 32 hex digits + 4 dashes + 2 braces; aliases like {default} are shorter.
    (id.len() == 38).then(|| id.to_string())
}

/// Loader entries in a store, as `(identifier, is_ramdisk)` pairs. A ramdisk
/// loader is the Recovery Environment: it never appears in the boot menu and
/// must not be counted with the menu entries.
fn loader_entries(store: &str) -> Vec<(String, bool)> {
    let Ok(text) = bcdedit(&["/store", store, "/enum", "osloader", "/v"]) else {
        return Vec::new();
    };
    text.split("\r\n\r\n")
        .filter_map(|block| {
            let id = first_identifier(block)?;
            Some((id, block.contains("ramdisk=")))
        })
        .collect()
}

/// Leave the store's boot menu showing only the entry bcdboot made default.
///
/// Deliberately conservative: if the default entry cannot be read, or does not
/// point at the Windows volume that was just restored, nothing is changed and
/// the reason is reported — a wrong guess here is an unbootable machine, while
/// an untouched menu is merely the extra question the user already sees.
fn trim_boot_menu(esp_volume: &str, uefi: bool, windows_volume: &str) -> BootFinalizeReport {
    let esp = esp_volume.trim_end_matches(['\\', '/']);
    let store = if uefi {
        format!(r"{esp}\EFI\Microsoft\Boot\BCD")
    } else {
        format!(r"{esp}\Boot\BCD")
    };
    if !std::path::Path::new(&store).exists() {
        warn!(store = %store, "boot menu: store not found where bcdboot should have written it");
        return BootFinalizeReport {
            note: Some(tr!("engine_win.note_store_not_found", store = store).into_owned()),
            ..Default::default()
        };
    }

    let default_block = match bcdedit(&["/store", &store, "/enum", "{default}", "/v"]) {
        Ok(text) => text,
        Err(e) => {
            warn!(error = %e, "boot menu: cannot read the default entry");
            return BootFinalizeReport {
                note: Some(tr!("engine_win.note_default_unreadable", error = e).into_owned()),
                ..Default::default()
            };
        }
    };
    let Some(default_id) = first_identifier(&default_block) else {
        warn!("boot menu: the default entry has no readable identifier");
        return BootFinalizeReport {
            note: Some(tr!("engine_win.note_default_unknown").into_owned()),
            ..Default::default()
        };
    };

    // The default must be the entry bcdboot just wrote for THIS restore: its
    // device is the restored Windows volume, which is holding a drive letter for
    // exactly this reason.
    let letter = windows_volume
        .trim_end_matches(['\\', '/'])
        .to_ascii_uppercase();
    let expected = format!("PARTITION={letter}"); // matched against an uppercased block
    let loaders = loader_entries(&store);
    let menu_loaders = loaders.iter().filter(|(_, ramdisk)| !ramdisk).count();
    if !default_block.to_ascii_uppercase().contains(&expected) {
        warn!(
            default = %default_id, expected = %expected,
            "boot menu: the default entry does not point at the restored volume — left as it is"
        );
        return BootFinalizeReport {
            default_id: Some(default_id),
            loaders_found: menu_loaders,
            note: Some(tr!("engine_win.note_default_elsewhere").into_owned()),
            menu_trimmed: false,
            ..Default::default()
        };
    }

    match bcdedit(&["/store", &store, "/displayorder", &default_id]) {
        Ok(_) => {
            info!(
                store = %store,
                default = %default_id,
                loaders = menu_loaders,
                "boot menu: display order reduced to the entry bcdboot created"
            );
            BootFinalizeReport {
                default_id: Some(default_id),
                loaders_found: menu_loaders,
                menu_trimmed: menu_loaders > 1,
                note: None,
                ..Default::default()
            }
        }
        Err(e) => {
            warn!(error = %e, "boot menu: could not set the display order");
            BootFinalizeReport {
                default_id: Some(default_id),
                loaders_found: menu_loaders,
                menu_trimmed: false,
                note: Some(tr!("engine_win.note_trim_failed", error = e).into_owned()),
                ..Default::default()
            }
        }
    }
}

/// Path of the temporary differencing child a read-write mount writes into.
/// Its existence is what says "this image is mounted RW right now" — the
/// consolidation guard reads it, because folding an increment into a base
/// somebody is writing through would corrupt both.
pub fn child_path_for(image_path: &Path) -> PathBuf {
    let mut name = image_path.file_name().unwrap_or_default().to_os_string();
    name.push(".mount_rw.avhdx");
    image_path.with_file_name(name)
}

fn scratch_path_for(image_path: &Path) -> PathBuf {
    let mut name = image_path.file_name().unwrap_or_default().to_os_string();
    name.push(".mount_rw.scratch");
    image_path.with_file_name(name)
}

/// Mount an image; volumes inside get drive letters from Windows.
/// The attachment survives process exit — use [`unmount_image`] to detach.
pub fn mount_image(image_path: &Path, read_write: bool) -> Result<String> {
    mount_image_with_keys(image_path, read_write, None)
}

/// The same, able to mount an encrypted image. Manual mounting is where a
/// person types a password (§7.4); the service passes its own keys for the
/// maintenance mounts it does inside a job.
pub fn mount_image_with_keys(
    image_path: &Path,
    read_write: bool,
    keys: Option<&crate::seal::context::ArchiveKeys>,
) -> Result<String> {
    if crate::sidecar::detect_container_format(image_path).ok()
        == Some(crate::seal::obk::OBK_EXTENSION)
    {
        return mount_obk_via_iscsi(image_path, read_write, keys);
    }
    // Dispatch by the container's OWN magic, never by sidecar metadata (new
    // backups carry no external .obkm, and a silent fall-through once sent a
    // qcow2 into vhdmp, which reported it as "file corrupted", error 1392).
    // qcow2 cannot go through vhdmp; it is served over an iSCSI target of ours
    // to Windows' own initiator (see `mount_qcow2_via_iscsi`).
    let mut magic = [0u8; 8];
    {
        use std::io::Read;
        let mut f = std::fs::File::open(image_path)
            .map_err(|e| OwnBackupError::Device(format!("open {}: {e}", image_path.display())))?;
        f.read_exact(&mut magic)
            .map_err(|e| OwnBackupError::Device(format!("read {}: {e}", image_path.display())))?;
    }
    if magic[..4] == [0x51, 0x46, 0x49, 0xFB] {
        // "QFI\xfb"
        return mount_qcow2_via_iscsi(image_path, read_write);
    }
    if &magic != b"vhdxfile" {
        return Err(OwnBackupError::ImageFormat(format!(
            "{} is neither a VHDX nor a qcow2 image (unrecognized signature)",
            image_path.display()
        )));
    }

    let attached = if read_write {
        let child_path = child_path_for(image_path);
        let _ = std::fs::remove_file(&child_path);
        create_differencing_vhdx(&child_path, image_path)?;
        attach_vhdx(&child_path, AttachMode::ReadWriteMounted)?
    } else {
        attach_vhdx(image_path, AttachMode::ReadOnlyMounted)?
    };

    // Backup images carry a persisted "offline" disk attribute (it keeps the
    // chain from auto-mounting during backups), so for browsing the disk must
    // be brought online explicitly — for this session only, the registry
    // attribute stays. Requires a read-write device handle (IOCTL
    // access rights), which vhdmp grants even on a read-only attach.
    match open_write_with_retry(attached.physical_path()) {
        Ok(dev) => {
            if let Err(e) = dev.set_online_this_session() {
                warn!(error = %e, "could not bring the mounted image online — it may show as offline in Disk Management");
            }
        }
        Err(e) => {
            warn!(error = %e, "could not open the mounted image to bring it online");
        }
    }
    letter_mounted_image(attached.physical_path());
    Ok(attached.physical_path().to_string())
}

/// Make a mounted image browsable: its disk online for this session, and its
/// data volumes lettered.
///
/// Neither happens by itself. A read-only iSCSI disk is left as the SAN policy
/// says, which on a server or the boot medium is offline; and Windows letters
/// only volumes it has never met, which a backup image rarely is — see
/// [`crate::letters_win::letter_data_volumes_on_disk`].
fn letter_mounted_image(physical: &str) {
    let Some(disk) = disk_number_from_physical(physical) else {
        return;
    };
    if let Ok(dev) = open_write_with_retry(physical) {
        if matches!(dev.is_offline_read_only(), Ok((true, _))) {
            match dev.set_online_this_session() {
                Ok(()) => info!(%physical, "mounted image: disk brought online for browsing"),
                Err(e) => warn!(%physical, error = %e, "mounted image: disk stays offline"),
            }
        }
    }
    let letters = crate::letters_win::letter_data_volumes_on_disk(disk);
    info!(%physical, ?letters, "mounted image: drive letters");
}

/// Detach a previously mounted image.
pub fn unmount_image(image_path: &Path) -> Result<()> {
    // qcow2 mounts are tracked in-process (the target dies with us).
    if qcow2_mounts().lock().unwrap().contains_key(image_path) {
        return unmount_qcow2_via_iscsi(image_path);
    }
    // A qcow2 we are not serving is simply not mounted — and must never reach
    // the VHDX road below, which hands it to `OpenVirtualDisk` and gets "the
    // file or directory is corrupted" (error 1392) for an image that is
    // perfectly fine (seen 2026-09-18, on a second press of "Отключить").
    if is_qcow2_file(image_path) {
        return Err(OwnBackupError::Config(
            tr!("engine_win.not_mounted", image = image_path.display()).into_owned(),
        ));
    }

    let child_path = child_path_for(image_path);
    if child_path.exists() {
        info!(image = %image_path.display(), "VHDX RW mount: unmounting child differencing VHDX");

        // 1. Detach child first
        detach_vhdx(&child_path)?;

        // 2. Read the touched blocks from BAT before we merge/delete it
        let alloc = match allocated_payload_blocks(&child_path) {
            Ok(a) if !a.blocks.is_empty() => Some(a),
            _ => None,
        };

        if let Some(alloc) = alloc {
            info!(image = %image_path.display(), count = alloc.blocks.len(), "VHDX RW mount: merging modified blocks back into base");

            // 3. Merge the child into the parent
            merge_into_parent(&child_path)?;
            let _ = std::fs::remove_file(&child_path);

            // 4. Update the sidecar with rehashed checksums (embedded or external)
            let ext = image_path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or_default()
                .to_lowercase();
            let mut read_from_embedded = false;
            let sidecar_attempt = match read_embedded_sidecar(image_path, &ext) {
                Ok(s) => {
                    read_from_embedded = true;
                    Ok(s)
                }
                Err(_) => {
                    let sidecar_path = sidecar_path_for(image_path);
                    read_sidecar(&sidecar_path)
                }
            };
            if let Ok(mut sidecar) = sidecar_attempt {
                let data_offset = sidecar.info.volume_data_offset;
                let volume_len = sidecar.manifest.source_disk.size_bytes;
                let block_size = sidecar.manifest.block_size;
                let _blocks_rehashed = crate::true_removal_win::rehash_touched_blocks(
                    image_path,
                    &alloc,
                    data_offset,
                    volume_len,
                    block_size,
                    &mut sidecar.checksums,
                    &mut |_, _| {}, // interactive unmount: no progress channel
                )?;
                if read_from_embedded {
                    let sector = 512u64;
                    let last_lba = (DEFAULT_PARTITION_OFFSET + volume_len).div_ceil(sector) - 1;
                    let meta_offset = (last_lba + 1) * sector;
                    let mut blob_bytes = Vec::new();
                    crate::sidecar::write_sidecar_v2_to(
                        &mut blob_bytes,
                        &sidecar.info,
                        &sidecar.manifest,
                        &sidecar.checksums,
                    )?;
                    crate::vhdx_bat::write_vhdx_guest_at(image_path, meta_offset, &blob_bytes)?;
                    info!(image = %image_path.display(), "VHDX RW mount: embedded sidecar updated successfully");
                } else {
                    let sidecar_path = sidecar_path_for(image_path);
                    write_sidecar(
                        &sidecar_path,
                        &sidecar.info,
                        &sidecar.manifest,
                        &sidecar.checksums,
                    )?;
                    info!(image = %image_path.display(), "VHDX RW mount: .obkm sidecar updated successfully");
                }
            }
        } else {
            // No blocks touched, just delete child
            let _ = std::fs::remove_file(&child_path);
        }
        Ok(())
    } else {
        detach_vhdx(image_path)
    }
}

// ---------------------------------------------------------------------------
// qcow2 mounting over our own iSCSI target
// ---------------------------------------------------------------------------

/// One qcow2 image currently shown to Windows as a disk, and the scratch
/// overlay behind it when the mount is writable.
struct MountedImage {
    attached: crate::attach_win::Attached,
    overlay: Option<std::sync::Arc<std::sync::Mutex<crate::overlay::WritableOverlaySource>>>,
}

/// qcow2 mounts served by THIS process: the target dies with the process, so
/// this only makes sense inside the long-running service (or a foreground CLI
/// command).
static QCOW2_MOUNTS: std::sync::OnceLock<std::sync::Mutex<HashMap<PathBuf, MountedImage>>> =
    std::sync::OnceLock::new();

fn qcow2_mounts() -> &'static std::sync::Mutex<HashMap<PathBuf, MountedImage>> {
    QCOW2_MOUNTS.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// Is this image (qcow2) currently served to Windows by a mount of ours?
/// Companion to [`child_path_for`] for the consolidation guard.
pub fn qcow2_mounted_here(image_path: &Path) -> bool {
    qcow2_mounts()
        .lock()
        .map(|m| m.contains_key(image_path))
        .unwrap_or(true)
}

/// Present a qcow2 backup (incl. its backing chain) to Windows as a real disk:
/// serve it over an iSCSI target of ours and have Windows' own initiator log
/// in. Windows mounts the filesystem inside and assigns a drive letter.
/// Returns the physical disk path (`\\.\PhysicalDriveN`).
fn mount_qcow2_via_iscsi(image_path: &Path, read_write: bool) -> Result<String> {
    if let Some(existing) = qcow2_mounts().lock().unwrap().get(image_path) {
        return Ok(existing.attached.physical.clone());
    }

    let (virtual_size, cluster_size) = {
        let r = crate::qcow2::Qcow2RandomReader::open(image_path)?;
        (r.virtual_size(), r.cluster_size())
    };
    let serial = crate::iscsi::serial_for_image(image_path);

    // Writable mounts never touch the image: everything lands in a scratch file
    // beside it, and only what the caller asks for is merged back on unmount.
    let (attached, overlay) = if read_write {
        let scratch = scratch_path_for(image_path);
        let _ = std::fs::remove_file(&scratch);
        let overlay_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&scratch)
            .map_err(|e| OwnBackupError::Device(format!("create scratch file: {e}")))?;
        let base: Box<dyn BlockSource> =
            Box::new(crate::qcow2::Qcow2RandomReader::open(image_path)?);
        let overlay = std::sync::Arc::new(std::sync::Mutex::new(
            crate::overlay::WritableOverlaySource::new(base, cluster_size, overlay_file),
        ));
        let attached =
            crate::attach_win::attach_rw(Box::new(overlay.clone()), virtual_size, &serial)?;
        (attached, Some(overlay))
    } else {
        let reader = crate::qcow2::Qcow2RandomReader::open(image_path)?;
        let attached = crate::attach_win::attach_ro(Box::new(reader), virtual_size, &serial)?;
        (attached, None)
    };

    let physical = attached.physical.clone();
    info!(
        image = %image_path.display(),
        physical = %physical,
        read_write,
        "qcow2 mounted over iSCSI"
    );
    letter_mounted_image(&physical);
    qcow2_mounts()
        .lock()
        .unwrap()
        .insert(image_path.to_path_buf(), MountedImage { attached, overlay });
    Ok(physical)
}

/// Serve an encrypted image read-only to Windows' own iSCSI initiator.
///
/// Read-write is refused, and that refusal is the point: a writable mount puts
/// everything the filesystem driver writes into a scratch file beside the
/// image, and for an encrypted image that file would be plaintext — the one
/// thing §7.4 says must not happen. The encrypted scratch is still to come.
fn mount_obk_via_iscsi(
    image_path: &Path,
    read_write: bool,
    keys: Option<&crate::seal::context::ArchiveKeys>,
) -> Result<String> {
    let keys = keys.ok_or_else(|| {
        OwnBackupError::Encryption(
            tr!(
                "engine_win.encrypted_needs_key",
                image = image_path.display()
            )
            .into_owned(),
        )
    })?;
    if read_write {
        return Err(OwnBackupError::Encryption(
            tr!("engine_win.encrypted_read_only").into_owned(),
        ));
    }
    if let Some(existing) = qcow2_mounts().lock().unwrap().get(image_path) {
        return Ok(existing.attached.physical.clone());
    }

    let chain = keys.open_chain_of(image_path)?;
    let reader = crate::seal::obk::ObkRandomReader::open(image_path, &chain.key)?;
    let virtual_size = reader.virtual_size();
    let serial = crate::iscsi::serial_for_image(image_path);
    let attached = crate::attach_win::attach_ro(Box::new(reader), virtual_size, &serial)?;
    let physical = attached.physical.clone();
    info!(
        image = %image_path.display(),
        physical = %physical,
        "encrypted image mounted read-only over iSCSI"
    );
    letter_mounted_image(&physical);
    qcow2_mounts().lock().unwrap().insert(
        image_path.to_path_buf(),
        MountedImage {
            attached,
            overlay: None,
        },
    );
    Ok(physical)
}

fn unmount_qcow2_via_iscsi(image_path: &Path) -> Result<()> {
    let entry = qcow2_mounts().lock().unwrap().remove(image_path);
    let Some(entry) = entry else {
        return Err(OwnBackupError::Config(format!(
            "{} is not mounted by this service",
            image_path.display()
        )));
    };
    // Dismount, log out, stop serving — in that order, and before anything is
    // merged: the overlay must not move under a disk Windows still has open.
    entry.attached.detach();
    info!(image = %image_path.display(), "qcow2 unmounted (iSCSI session logged out)");

    // If it was mounted in RW mode, we have the overlay and need to merge + rehash + update sidecar!
    if let Some(overlay_arc) = entry.overlay {
        let touched = overlay_arc.lock().unwrap().touched_clusters();
        if !touched.is_empty() {
            info!(image = %image_path.display(), count = touched.len(), "QCOW2 RW mount: merging modified clusters back into base");

            let ext = image_path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or_default()
                .to_lowercase();
            // At the level the copy was made with, from its own manifest, so
            // the merged clusters match the rest of the image. Unreadable →
            // the default (`Qcow2InPlaceMerge` falls back to it on `None`).
            let zstd_level = read_embedded_sidecar(image_path, &ext)
                .or_else(|_| read_sidecar(&sidecar_path_for(image_path)))
                .map(|s| crate::qcow2::zstd_level_or_default(s.manifest.compression_level))
                .ok();
            {
                let mut ov = overlay_arc.lock().unwrap();
                let mut merge = crate::qcow2::Qcow2InPlaceMerge::open(image_path, zstd_level)?;
                for &c in &touched {
                    let data = ov.read_cluster(c)?;
                    merge.write_cluster(c, &data)?;
                }
                merge.commit()?;
            }

            // Now rehash the touched regions and update the sidecar (embedded or external)!
            let mut read_from_embedded = false;
            let sidecar_attempt = match read_embedded_sidecar(image_path, &ext) {
                Ok(s) => {
                    read_from_embedded = true;
                    Ok(s)
                }
                Err(_) => {
                    let sidecar_path = sidecar_path_for(image_path);
                    read_sidecar(&sidecar_path)
                }
            };
            if let Ok(mut sidecar) = sidecar_attempt {
                let cluster_size = {
                    let ov = overlay_arc.lock().unwrap();
                    ov.cluster_size()
                };
                let data_offset = sidecar.info.volume_data_offset;
                let volume_len = sidecar.manifest.source_disk.size_bytes;
                let block_size = sidecar.manifest.block_size;
                let _blocks_rehashed = crate::qcow2_removal_win::rehash_touched(
                    image_path,
                    &touched,
                    cluster_size,
                    data_offset,
                    volume_len,
                    block_size,
                    &mut sidecar.checksums,
                    &mut |_, _| {}, // interactive unmount: no progress channel
                )?;
                if read_from_embedded {
                    let sector = 512u64;
                    let last_lba = (DEFAULT_PARTITION_OFFSET + volume_len).div_ceil(sector) - 1;
                    let meta_offset = (last_lba + 1) * sector;
                    let mut blob_bytes = Vec::new();
                    crate::sidecar::write_sidecar_v2_to(
                        &mut blob_bytes,
                        &sidecar.info,
                        &sidecar.manifest,
                        &sidecar.checksums,
                    )?;
                    crate::qcow2::merge::write_qcow2_guest_blob(
                        image_path,
                        meta_offset,
                        &blob_bytes,
                    )?;
                    info!(image = %image_path.display(), "QCOW2 RW mount: embedded sidecar updated successfully");
                } else {
                    let sidecar_path = sidecar_path_for(image_path);
                    write_sidecar(
                        &sidecar_path,
                        &sidecar.info,
                        &sidecar.manifest,
                        &sidecar.checksums,
                    )?;
                    info!(image = %image_path.display(), "QCOW2 RW mount: .obkm sidecar updated successfully");
                }
            }
        }
        // Delete scratch file
        let scratch = scratch_path_for(image_path);
        let _ = std::fs::remove_file(&scratch);
    }

    info!(image = %image_path.display(), "qcow2 unmounted (AIM device removed)");
    Ok(())
}

use crate::unlock_probe::probe_decrypted;
pub use crate::unlock_probe::DecryptedProbe;

/// How an encrypted partition can be opened, asked of the volume itself.
///
/// `(what it accepts, whether we can do any of it)`. Costs a few sector reads
/// and no password: BitLocker's FVE metadata lists its protectors in the clear,
/// and a LUKS header its keyslots. The interface needs it to stop asking a
/// TPM-only volume for a passphrase it does not have — the answer to which is
/// not "wrong password" but "wrong question".
///
/// When the container is neither, or its metadata cannot be read, the answer is
/// "we do not know" — an empty list and `true`. Saying `false` there would put
/// "открыть нечем" under a volume nobody has actually asked, which is exactly
/// what a LUKS partition used to get: the wrong question in the other
/// direction. Letting the user try costs one rejected passphrase; refusing to
/// ask costs them the volume.
#[cfg(target_os = "windows")]
pub fn describe_encryption(disk: u32, offset: u64, length: u64) -> (Vec<String>, bool, bool) {
    let unknown = (Vec::new(), true, false);
    let path = format!(r"\\.\PhysicalDrive{disk}");
    let Ok(mut window) = disk_window::<WinRawDevice>(&path, offset, length) else {
        return unknown;
    };
    if let Ok(info) = crate::bitlocker::inspect_bitlocker_with(
        &mut window,
        disk_sector_size::<WinRawDevice>(&path),
    ) {
        let names = info
            .protectors
            .iter()
            .map(|p| {
                if p.count > 1 {
                    format!("{} ×{}", p.name, p.count)
                } else {
                    p.name.clone()
                }
            })
            .collect();
        // Мастеру важно «есть ли что набрать», а не «справится ли наш разбор»:
        // чего не умеем мы, откроет Windows. Иначе поле пароля не показывается
        // там, где пароль отлично подошёл бы.
        return (names, info.typeable(), info.recovery_key_only());
    }
    match crate::luks::inspect_luks(&mut window) {
        Ok(info) => (info.methods, info.openable, false),
        Err(e) => {
            debug!(disk, offset, error = %e, "encrypted partition described neither as BitLocker nor as LUKS");
            unknown
        }
    }
}

/// Open a BitLocker volume through the disk it lives on, and describe what is
/// inside it.
///
/// The volume device is useless here: locked, it answers `FVE_E_LOCKED_VOLUME`
/// to everything. The partition's bytes are on `\\.\PhysicalDriveN` all the
/// same — that is how the inventory recognises such a volume in the first
/// place — so the passphrase is tried against a window onto the disk.
///
/// Confirmed on a real volume (2026-08-05): Windows 10, XTS-AES 128, password
/// protector.
#[cfg(target_os = "windows")]
fn probe_bitlocker(volume_device: &str, passphrase: &str) -> Result<DecryptedProbe> {
    let extents = crate::volume_win::volume_extents(volume_device);
    let [extent] = extents.as_slice() else {
        return Err(OwnBackupError::Device(
            tr!(
                "engine_win.bitlocker_multi_extent",
                volume = volume_device,
                count = extents.len()
            )
            .into_owned(),
        ));
    };
    let disk = format!(r"\\.\PhysicalDrive{}", extent.disk_number);
    let mut probe = disk_window::<WinRawDevice>(&disk, extent.offset, extent.length)?;
    let params = match crate::bitlocker::open_bitlocker_with(
        &mut probe,
        passphrase,
        disk_sector_size::<WinRawDevice>(&disk),
    ) {
        Ok(p) => p,
        // Wrong passphrase — a fact for the interface, not an error. AES-CCM
        // verifies its own tag, so this really does mean "not this password".
        Err(e) if crate::bitlocker::is_wrong_passphrase(&e) => {
            info!(error = %e, "BitLocker passphrase rejected");
            return Ok(DecryptedProbe {
                ok: false,
                ..Default::default()
            });
        }
        // А это НЕ про пароль: шифр вне AES-XTS (панель Windows шифрует съёмные
        // диски в режиме совместимости по умолчанию), ReFS без BPB, нечитаемый
        // заголовок. Пароль при этом мог подойти — в журнале так и написано,
        // «VMK unwrapped», — и отвечать «пароль не подходит» значит отправить
        // человека перенабирать верное. Всё это открывает сама Windows.
        Err(e) => {
            warn!(error = %e, "свой разбор BitLocker не справился — прошу открыть Windows");
            return probe_by_windows(volume_device, passphrase);
        }
    };
    info!(
        algo = format!("{:#x}", params.algorithm),
        disk = extent.disk_number,
        offset = extent.offset,
        "BitLocker source unlocked; backing up decrypted data"
    );

    let mut view = crate::bitlocker::BitLockerVolume::new(
        Box::new(disk_window::<WinRawDevice>(
            &disk,
            extent.offset,
            extent.length,
        )?),
        params,
    );
    probe_decrypted(&mut view)
}

/// Открыть том силами Windows и осмотреть то, что получилось.
///
/// После разблокировки том становится обычным смонтированным, и читается он
/// как любой другой — своей расшифровки уже не нужно. Страж держит диск
/// закрытым на запись только пока мы читаем метки: копия дальше пойдёт общей
/// дорогой, и держать диск дольше незачем.
#[cfg(target_os = "windows")]
fn probe_by_windows(volume_device: &str, passphrase: &str) -> Result<DecryptedProbe> {
    let guard = match unlock_source_with_windows(volume_device, passphrase) {
        Ok(g) => g,
        Err(e) => {
            // Windows тоже не открыла. Это может быть и неверный пароль, и
            // отсутствие компонента BitLocker на носителе — точнее сказать
            // нечего, и «не подошёл» здесь честнее, чем молчание.
            info!(error = %e, "Windows не открыла том");
            return Ok(DecryptedProbe {
                ok: false,
                ..Default::default()
            });
        }
    };
    let probed =
        WinRawDevice::open_read(volume_device).and_then(|mut dev| probe_decrypted(&mut dev));
    drop(guard);
    probed
}

/// Unlock the encrypted container at `device_path` with `passphrase` and probe
/// the decrypted filesystem — LUKS through the device itself, BitLocker through
/// the disk under it (see [`probe_bitlocker`]).
///
/// `ok = false` means the passphrase was rejected, and nothing else: an
/// unsupported cipher, an unreadable header or a KDF that could not get its
/// memory come back as errors, so the interface never answers "пароль не
/// подходит" to a question about something else. A source that is not an
/// encrypted container at all returns `ok = true` with empty info — there was
/// nothing to unlock.
pub fn unlock_and_probe(device_path: &str, passphrase: &str) -> Result<DecryptedProbe> {
    let src = normalize_device_path(device_path);
    let mut dev = WinRawDevice::open_read(&src)?;
    let mut header = vec![0u8; 4096];
    match dev.read_at(0, &mut header) {
        Ok(()) => {}
        // A locked BitLocker volume refuses every read through its own device.
        // Its ciphertext is on the DISK, which the FVE filter does not sit
        // above, so the passphrase is tried there instead.
        Err(e) if is_bitlocker_locked(&e) => return probe_bitlocker(&src, passphrase),
        Err(e) => return Err(e),
    }
    // An unlocked BitLocker volume reads as plain NTFS through its device —
    // Windows decrypts on the way — and needs nothing from us. A locked one
    // whose header we could still read (a disk moved here from elsewhere)
    // goes down the same path as above.
    if crate::bitlocker::is_bitlocker(&header) {
        return probe_bitlocker(&src, passphrase);
    }
    if !crate::luks::is_luks(&header) {
        return Ok(DecryptedProbe {
            ok: true,
            ..Default::default()
        });
    }
    let params = match crate::luks::open_luks(&mut dev, passphrase.as_bytes()) {
        Ok(p) => p,
        // No keyslot took it: a fact for the interface, not an error.
        Err(e) if crate::luks::is_wrong_passphrase(&e) => {
            info!(error = %e, "LUKS passphrase rejected");
            return Ok(DecryptedProbe {
                ok: false,
                ..Default::default()
            });
        }
        // Anything else — a cipher we do not implement, a header we could not
        // read, an Argon2 that could not get its memory — is NOT a wrong
        // password, and saying so would send the user off to re-type a password
        // that was right all along.
        Err(e) => return Err(e),
    };
    drop(dev);

    // Read the decrypted superblock region and identify the inner filesystem.
    let mut view =
        crate::decrypt::DecryptingVolume::new(Box::new(WinRawDevice::open_read(&src)?), params);
    probe_decrypted(&mut view)
}

fn get_diskpart_virtual_disks() -> Vec<(u32, String)> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    let mut results = Vec::new();

    let mut cmd = match std::process::Command::new("cmd")
        .args(["/c", "chcp 65001 >nul && diskpart"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "failed to spawn diskpart command proxy");
            return results;
        }
    };

    if let Some(mut stdin) = cmd.stdin.take() {
        use std::io::Write;
        if let Err(e) = writeln!(stdin, "list vdisk") {
            tracing::error!(error = %e, "failed to write to diskpart stdin");
        }
    }

    let mut byte_buf = Vec::new();
    if let Some(mut stdout) = cmd.stdout.take() {
        use std::io::Read;
        if let Err(e) = stdout.read_to_end(&mut byte_buf) {
            tracing::error!(error = %e, "failed to read from diskpart stdout");
        }
    }
    let output = String::from_utf8_lossy(&byte_buf).to_string();

    let mut stderr_output = String::new();
    if let Some(mut stderr) = cmd.stderr.take() {
        use std::io::Read;
        let _ = stderr.read_to_string(&mut stderr_output);
    }

    let _ = cmd.wait();

    for line in output.lines() {
        let line_trimmed = line.trim();
        if line_trimmed.is_empty() {
            continue;
        }

        let line_lower = line_trimmed.to_lowercase();
        if !line_lower.contains(".vhdx")
            && !line_lower.contains(".vhd")
            && !line_lower.contains(".avhdx")
        {
            continue;
        }

        let disk_num = (|| {
            let numeric_tokens: Vec<u32> = line_trimmed
                .split_whitespace()
                .filter_map(|t| t.parse::<u32>().ok())
                .collect();
            if numeric_tokens.len() >= 2 {
                Some(numeric_tokens[1])
            } else {
                None
            }
        })();

        let disk_num = match disk_num {
            Some(n) => n,
            None => continue,
        };

        let mut path_start = None;
        let chars: Vec<char> = line_trimmed.chars().collect();
        for i in 0..chars.len() {
            if chars[i] == ':' && i > 0 && i + 1 < chars.len() && chars[i + 1] == '\\' {
                path_start = Some(i - 1);
                break;
            }
            if chars[i] == '\\' && i + 1 < chars.len() && chars[i + 1] == '\\' {
                path_start = Some(i);
                break;
            }
        }

        if let Some(start) = path_start {
            let path = chars[start..].iter().collect::<String>().trim().to_string();
            results.push((disk_num, path));
        }
    }

    results
}

pub fn get_all_active_mounts(
    internal_mounts: &[own_backup_common::engine_dto::ActiveMountDto],
) -> Vec<own_backup_common::engine_dto::ActiveMountDto> {
    let mut result = internal_mounts.to_vec();

    for (disk_num, image_path) in get_diskpart_virtual_disks() {
        let device_path = format!(r"\\.\PhysicalDrive{disk_num}");

        let clean_item_path = image_path.replace('/', "\\");
        let exists = result.iter().any(|m| {
            let clean_m_path = m.image_path.replace('/', "\\");
            clean_m_path.eq_ignore_ascii_case(&clean_item_path)
        });

        if !exists {
            result.push(own_backup_common::engine_dto::ActiveMountDto {
                image_path,
                physical_path: device_path,
                read_write: false,
                volume_letters: Vec::new(),
            });
        }
    }

    // Resolve the drive letters Windows assigned to each mounted disk so the UI
    // can show "J:" instead of "PhysicalDrive5" / "Диск 5".
    for m in &mut result {
        if let Some(disk_num) = disk_number_from_physical(&m.physical_path) {
            m.volume_letters = crate::volume_win::drive_letters_on_disk(disk_num);
        }
    }

    result
}

/// Is this file a qcow2? By its magic, not by its extension: what a backup is
/// called is the user's business, what it is is the file's own.
fn is_qcow2_file(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 4];
    f.read_exact(&mut magic).is_ok() && magic == [0x51, 0x46, 0x49, 0xFB]
}

/// Parse `\\.\PhysicalDriveN` → `N`.
fn disk_number_from_physical(physical_path: &str) -> Option<u32> {
    physical_path
        .rsplit("PhysicalDrive")
        .next()
        .and_then(|n| n.trim().parse().ok())
}
