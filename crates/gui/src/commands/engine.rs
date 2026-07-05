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

use own_backup_common::engine_dto::{DiskRestoreSummaryDto, RestoreSummaryDto, VerifySummaryDto};

use crate::state::GuiState;

/// One partition of a whole-disk (`.obdm`) backup, for the restore list.
#[derive(Debug, Clone, Serialize)]
pub struct DiskBackupPartDto {
    pub index: u32,
    pub name: String,
    pub filesystem: String,
    pub size_bytes: u64,
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

/// List whole-disk (`.obdm`) backups found in `folder`. Cross-platform: it only
/// reads/parses the JSON manifests.
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
        if path.extension().and_then(|e| e.to_str()) != Some(DiskBackupManifest::EXTENSION) {
            continue;
        }
        let Ok(raw) = std::fs::read(&path) else { continue };
        let Ok(m) = serde_json::from_slice::<DiskBackupManifest>(&raw) else {
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
    pub image_file_name: String,
    pub chain_role: String,
    pub created_at: String,
    pub source_device: String,
    pub source_bytes: u64,
    pub block_size: u32,
    pub used_blocks: u64,
    pub total_blocks: u64,
    pub backup_mode: String,
}

/// Read sidecar info for an image path — used by the Restore/Tools views.
#[tauri::command]
pub async fn read_image_info(image_path: String) -> Result<ImageInfoDto, String> {
    let path = std::path::PathBuf::from(&image_path);
    let sidecar_path = own_backup_backup::sidecar_path_for(&path);
    if !sidecar_path.exists() {
        return Err(format!(
            "Рядом с образом не найден файл метаданных: {}",
            sidecar_path.display()
        ));
    }
    let sidecar = own_backup_backup::read_sidecar(&sidecar_path).map_err(|e| e.to_string())?;
    Ok(ImageInfoDto {
        image_file_name: sidecar.info.image_file_name,
        chain_role: format!("{:?}", sidecar.info.chain_role),
        created_at: sidecar.manifest.created_at,
        source_device: sidecar.manifest.source_disk.device_id,
        source_bytes: sidecar.manifest.source_disk.size_bytes,
        block_size: sidecar.manifest.block_size,
        used_blocks: sidecar.manifest.used_blocks,
        total_blocks: sidecar.manifest.total_blocks,
        backup_mode: format!("{:?}", sidecar.manifest.backup_mode),
    })
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
pub async fn mount_backup(state: State<'_, GuiState>, image_path: String) -> Result<String, String> {
    state.link.client().await?.mount_image(&image_path).await
}

#[tauri::command]
pub async fn unmount_backup(state: State<'_, GuiState>, image_path: String) -> Result<(), String> {
    state.link.client().await?.unmount_image(&image_path).await
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
