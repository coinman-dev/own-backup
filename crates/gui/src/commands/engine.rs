//! Tauri commands for restore / verify / mount used by the Restore & Tools views.
//!
//! These operations need admin (raw device I/O, VHDX attach), so they run in the
//! privileged service and the (unelevated) GUI proxies them over gRPC. Progress
//! arrives on the service event stream and is re-emitted by `bridge_service_events`
//! as `restore-progress` / `verify-progress` / `restore-disk-progress`.
//!
//! `read_image_info` and `list_disk_backups` are pure file reads (no privileges)
//! and stay in-process, cross-platform.

use serde::Serialize;
use tauri::State;

use own_backup_backup::exclusions::CategorySizes;
use own_backup_common::disk::DiskInfo;
use own_backup_common::engine_dto::{DiskRestoreSummaryDto, RestoreSummaryDto, VerifySummaryDto};
use own_backup_common::restore_check::CheckContext;
use own_backup_common::restore_plan::RestorePlan;
use own_backup_common::restore_state::RestoreRunState;

use crate::state::GuiState;

/// One partition of a whole-disk (`.obdm`) backup, for the restore list.
#[derive(Debug, Clone, Serialize)]
pub struct DiskBackupPartDto {
    pub index: u32,
    pub name: String,
    pub filesystem: String,
    pub size_bytes: u64,
}

/// The filesystem the image records for one partition.
#[derive(Debug, Clone, Serialize)]
pub struct PartFsDto {
    pub index: u32,
    pub filesystem: String,
}

/// A whole-disk backup discovered in the destination folder (from a `.obdm`).
#[derive(Debug, Clone, Serialize)]
pub struct DiskBackupDto {
    pub manifest_path: String,
    pub disk_model: String,
    pub disk_number: u32,
    pub created_at: String,
    pub total_bytes: u64,
    pub partitions: Vec<DiskBackupPartDto>,
}

/// List whole-disk backups found in `folder`: single whole-disk IMAGES that
/// carry an embedded manifest (plan D1), and legacy `.obdm` manifests. The
/// returned `manifest_path` is what to hand to restore (the image itself for an
/// embedded backup, the `.obdm` for a legacy one). Cross-platform read-only.
#[tauri::command]
pub async fn list_disk_backups(folder: String) -> Result<Vec<DiskBackupDto>, String> {
    use own_backup_common::gpt::DiskBackupManifest;

    let dir = std::path::Path::new(&folder);
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(Vec::new()), // missing folder → no disk backups
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_lowercase();
        // Whole-disk image (embedded manifest) OR legacy .obdm; skip the rest.
        let m: DiskBackupManifest = if ext == DiskBackupManifest::EXTENSION {
            let Ok(raw) = std::fs::read(&path) else { continue };
            let Ok(m) = serde_json::from_slice::<DiskBackupManifest>(&raw) else {
                continue;
            };
            m
        } else if matches!(ext.as_str(), "vhdx" | "avhdx" | "qcow2") {
            match own_backup_backup::read_embedded_sidecar(&path, &ext) {
                Ok(sc) => match sc.disk_manifest {
                    Some(m) => m,           // a whole-disk image
                    None => continue,       // a plain per-volume image
                },
                Err(_) => continue,
            }
        } else {
            continue;
        };
        let sector = m.layout.sector_size;
        let partitions: Vec<DiskBackupPartDto> = m
            .images
            .iter()
            .filter_map(|img| {
                m.layout
                    .partitions
                    .iter()
                    .find(|p| p.index == img.part_index)
                    .map(|p| DiskBackupPartDto {
                        index: p.index,
                        name: p.name.clone(),
                        filesystem: img.filesystem.clone(),
                        size_bytes: p.size_bytes(sector),
                    })
            })
            .collect();
        let total_bytes = partitions.iter().map(|p| p.size_bytes).sum();
        out.push(DiskBackupDto {
            manifest_path: path.display().to_string(),
            disk_model: m.disk_model,
            disk_number: m.disk_number,
            created_at: m.created_at,
            total_bytes,
            partitions,
        });
    }
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(out)
}

/// Metadata about an image, read from its `.obkm` sidecar (works on any OS).
#[derive(Debug, Clone, Serialize)]
pub struct ImageInfoDto {
    /// Full path to the image file (what restore is handed).
    pub image_path: String,
    /// Job id that produced this image (ties copies of one archive together).
    pub job_id: Option<String>,
    pub image_file_name: String,
    pub chain_role: String,
    pub created_at: String,
    pub source_device: String,
    pub source_bytes: u64,
    pub block_size: u32,
    pub used_blocks: u64,
    pub total_blocks: u64,
    pub backup_mode: String,
    /// Full physical-disk snapshot embedded at backup time (whole disk with its
    /// partitions), so the restore card renders from the image alone — no
    /// jobs.json. `None` for images written before snapshots existed.
    pub source_disk_layout: Option<DiskInfo>,
    /// Partition indices (into `source_disk_layout.partitions`) that this backup
    /// actually contains — the ones the restore card highlights.
    pub backed_up_parts: Vec<u32>,
    /// Filesystem inside each backed-up partition, **as the image records it**.
    ///
    /// For a LUKS or BitLocker source this is the decrypted filesystem, because
    /// that is what the image holds. The disk snapshot alongside says `Unknown`
    /// and flags the partition encrypted, which is the only honest reading of a
    /// container nobody has opened — but for a partition that is *in the copy*
    /// the manifest knows better, and a padlock over what restores as plain
    /// btrfs would be a lie (D14).
    pub part_filesystems: Vec<PartFsDto>,
    /// Partitions whose encrypted container's header this copy kept, so a
    /// restore can put the encryption back exactly as it was (D19). Empty for
    /// copies taken before headers were stored — those restore decrypted only.
    pub luks_header_parts: Vec<u32>,
    /// File-type breakdown embedded in the image (for "по типам файлов").
    pub categories: Option<CategorySizes>,
    /// Files/bytes excluded from this image (for the "Исключено" legend line).
    pub excluded_files: u64,
    pub excluded_bytes: u64,
    /// Logical sector size the source disk's GPT was captured with. A restore
    /// onto a disk with a different sector size cannot reproduce the layout:
    /// the stored LBAs would address different byte offsets.
    pub sector_size: u32,
    /// True when this image is a whole-disk (bootable) backup.
    pub is_whole_disk: bool,
}

/// Read one image's embedded metadata into an [`ImageInfoDto`] (direct file
/// read: embedded meta partition first, then an adjacent `.obkm`). Shared by
/// `read_image_info` (single file) and `list_image_backups` (folder scan).
fn read_image_info_at(path: &std::path::Path) -> Result<ImageInfoDto, String> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_lowercase();

    let sidecar = match own_backup_backup::read_embedded_sidecar(path, &ext) {
        Ok(s) => s,
        Err(_) => {
            let sidecar_path = own_backup_backup::sidecar_path_for(path);
            if !sidecar_path.exists() {
                return Err(format!(
                    "В образе нет встроенных метаданных и рядом не найден файл метаданных: {}",
                    sidecar_path.display()
                ));
            }
            own_backup_backup::read_sidecar(&sidecar_path).map_err(|e| e.to_string())?
        }
    };

    // Derived (compute before moving fields out of the sidecar): the archived
    // disk snapshot and which of its partitions are in this backup — whole-disk
    // images list them in the embedded disk manifest; a per-volume image holds
    // the single partition matching its source device.
    let source_disk_layout = sidecar.info.source_disk_layout.clone();
    let is_whole_disk = sidecar.disk_manifest.is_some();
    let src_dev = sidecar.manifest.source_disk.device_id.clone();
    let backed_up_parts: Vec<u32> = if let Some(dm) = &sidecar.disk_manifest {
        dm.images.iter().map(|i| i.part_index).collect()
    } else if let Some(layout) = &source_disk_layout {
        layout
            .partitions
            .iter()
            .filter(|p| p.volume_device == src_dev)
            .map(|p| p.index)
            .collect()
    } else {
        Vec::new()
    };

    // Whole-disk images carry a filesystem per partition in the disk manifest;
    // a per-volume image carries the one filesystem it holds.
    let part_filesystems: Vec<PartFsDto> = if let Some(dm) = &sidecar.disk_manifest {
        dm.images
            .iter()
            .filter(|i| !i.filesystem.is_empty())
            .map(|i| PartFsDto { index: i.part_index, filesystem: i.filesystem.clone() })
            .collect()
    } else {
        match (&sidecar.info.filesystem, backed_up_parts.first()) {
            (Some(fs), Some(&index)) if !fs.is_empty() => {
                vec![PartFsDto { index, filesystem: fs.clone() }]
            }
            _ => Vec::new(),
        }
    };

    // A per-volume image records index 0 (there was nothing to disambiguate at
    // backup time), so its lone header answers for the partition it holds.
    let luks_header_parts: Vec<u32> = if sidecar.disk_manifest.is_none()
        && sidecar.info.luks_headers.len() == 1
    {
        backed_up_parts.clone()
    } else {
        sidecar.info.luks_headers.iter().map(|h| h.part_index).collect()
    };

    Ok(ImageInfoDto {
        image_path: path.display().to_string(),
        job_id: sidecar.info.job_id.clone(),
        image_file_name: sidecar.info.image_file_name,
        chain_role: format!("{:?}", sidecar.info.chain_role),
        created_at: sidecar.manifest.created_at,
        source_device: src_dev,
        source_bytes: sidecar.manifest.source_disk.size_bytes,
        block_size: sidecar.manifest.block_size,
        used_blocks: sidecar.manifest.used_blocks,
        total_blocks: sidecar.manifest.total_blocks,
        backup_mode: format!("{:?}", sidecar.manifest.backup_mode),
        source_disk_layout,
        backed_up_parts,
        part_filesystems,
        luks_header_parts,
        categories: sidecar.info.categories,
        excluded_files: sidecar.info.excluded_files,
        excluded_bytes: sidecar.info.excluded_bytes,
        // The GPT's own sector size is authoritative for a whole-disk image;
        // otherwise fall back to the volume's.
        sector_size: sidecar
            .disk_manifest
            .as_ref()
            .map(|dm| dm.layout.sector_size)
            .unwrap_or(sidecar.manifest.source_disk.sector_size_logical),
        is_whole_disk,
    })
}

#[tauri::command]
pub async fn read_image_info(image_path: String) -> Result<ImageInfoDto, String> {
    read_image_info_at(&std::path::PathBuf::from(&image_path))
}

/// Scan `folder` for backup IMAGES (per-volume AND whole-disk) and return each
/// one's embedded metadata. Direct file reads, no service / no jobs.json — the
/// "Открыть папку с копиями" entry for a bare-system (Boot-CD) restore. Files
/// without a readable sidecar are skipped; newest first.
#[tauri::command]
pub async fn list_image_backups(folder: String) -> Result<Vec<ImageInfoDto>, String> {
    let dir = std::path::Path::new(&folder);
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(Vec::new()),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_lowercase();
        if !matches!(ext.as_str(), "vhdx" | "avhdx" | "qcow2") {
            continue;
        }
        if let Ok(info) = read_image_info_at(&path) {
            out.push(info);
        }
    }
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(out)
}

// --- Proxies to the service's privileged engine --------------------------

/// Restore a volume image onto `target` (DESTRUCTIVE). Progress streams from the
/// service as `restore-progress`.
#[tauri::command]
pub async fn restore_now(
    state: State<'_, GuiState>,
    image_path: String,
    target: String,
) -> Result<RestoreSummaryDto, String> {
    state
        .link
        .client()
        .await?
        .restore_volume(&image_path, &target)
        .await
}

/// Verify an image against its sidecar checksums. Progress streams as `verify-progress`.
#[tauri::command]
pub async fn verify_now(
    state: State<'_, GuiState>,
    image_path: String,
) -> Result<VerifySummaryDto, String> {
    state.link.client().await?.verify_image(&image_path).await
}

#[tauri::command]
pub async fn mount_backup(
    state: State<'_, GuiState>,
    image_path: String,
    read_write: Option<bool>,
) -> Result<String, String> {
    state
        .link
        .client()
        .await?
        .mount_image(&image_path, read_write.unwrap_or(false))
        .await
}

#[tauri::command]
pub async fn unmount_backup(state: State<'_, GuiState>, image_path: String) -> Result<(), String> {
    state.link.client().await?.unmount_image(&image_path).await
}

// --- Arsenal Image Mounter driver (needed to mount qcow2 on Windows) ---

#[tauri::command]
pub async fn aim_driver_status(state: State<'_, GuiState>) -> Result<serde_json::Value, String> {
    state.link.client().await?.aim_driver_status().await
}

#[tauri::command]
pub async fn install_aim_driver(state: State<'_, GuiState>) -> Result<serde_json::Value, String> {
    state.link.client().await?.install_aim_driver().await
}

#[tauri::command]
pub async fn uninstall_aim_driver(state: State<'_, GuiState>) -> Result<(), String> {
    state.link.client().await?.uninstall_aim_driver().await
}

/// Restore a WHOLE disk from a `.obdm` manifest onto `target_disk`. DESTRUCTIVE.
/// The service's guard still refuses the running system disk unless `force` (and
/// even then only outside a recovery environment). Progress: `restore-disk-progress`.
#[tauri::command]
pub async fn restore_disk_now(
    state: State<'_, GuiState>,
    manifest_path: String,
    target_disk: u32,
    force: bool,
) -> Result<DiskRestoreSummaryDto, String> {
    state
        .link
        .client()
        .await?
        .restore_disk(&manifest_path, target_disk, force)
        .await
}

/// Request cancellation of the running restore/verify/disk-restore in the service.
#[tauri::command]
pub async fn cancel_engine_op(state: State<'_, GuiState>) -> Result<(), String> {
    state.link.client().await?.cancel_engine_op().await
}

// ---------------------------------------------------------------------------
// Restore plans (the partition designer)
// ---------------------------------------------------------------------------

/// Check a plan without writing anything (D5).
///
/// The reply contains the plan **back again**, because the dry run resolves
/// drive-letter conflicts by editing it (D11). The frontend must apply the plan
/// it got back, not the one it sent, or that fix is thrown away.
#[tauri::command]
pub async fn dry_run_restore_plan(
    state: State<'_, GuiState>,
    plan: RestorePlan,
    context: Option<CheckContext>,
) -> Result<serde_json::Value, String> {
    state
        .link
        .client()
        .await?
        .dry_run_restore_plan(&plan, &context.unwrap_or_default())
        .await
}

/// Execute a restore plan. DESTRUCTIVE — the caller is responsible for having
/// shown the dry run's findings and got a confirmation.
///
/// Returns immediately with the run's initial state; progress arrives as
/// `restore-plan-progress`, and [`get_restore_state`] gives the current state
/// at any time (including after the window was closed and reopened).
#[tauri::command]
pub async fn apply_restore_plan(
    state: State<'_, GuiState>,
    plan: RestorePlan,
    passphrases: Option<std::collections::HashMap<u32, String>>,
) -> Result<RestoreRunState, String> {
    state
        .link
        .client()
        .await?
        .apply_restore_plan(&plan, &passphrases.unwrap_or_default())
        .await
}

/// The restore in progress, or the last one that finished. `None` when no plan
/// has run since the service started.
#[tauri::command]
pub async fn get_restore_state(
    state: State<'_, GuiState>,
) -> Result<Option<RestoreRunState>, String> {
    state.link.client().await?.get_restore_state().await
}

/// Ask the named run to stop. The reply says whether it was accepted: while a
/// non-cancellable operation runs the service refuses outright rather than
/// accepting the request and ignoring it.
#[tauri::command]
pub async fn cancel_restore_plan(
    state: State<'_, GuiState>,
    plan_id: String,
) -> Result<serde_json::Value, String> {
    state.link.client().await?.cancel_restore_plan(&plan_id).await
}
