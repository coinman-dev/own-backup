//! Tauri commands for restore/verify/mount used by the Restore & Tools views
//! (plan v2). Backups themselves run through the job queue (`jobs.rs`).
//!
//! Long operations run on blocking threads and report progress through Tauri
//! events `restore-progress` / `verify-progress` (payload [`EngineProgress`]).

use serde::Serialize;
use tauri::State;

use crate::state::GuiState;

/// Progress payload emitted to the frontend.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(not(target_os = "windows"), allow(dead_code))] // constructed only by the Windows engine
pub struct EngineProgress {
    pub percent: u64,
    pub blocks_done: u64,
    pub blocks_total: u64,
    pub bytes_done: u64,
    pub speed_mbps: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RestoreSummaryDto {
    pub blocks_restored: u64,
    pub bytes_restored: u64,
    pub duration_secs: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct VerifySummaryDto {
    pub valid: bool,
    pub blocks_checked: u64,
    pub mismatched_blocks: Vec<u64>,
    pub duration_secs: f64,
}

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

#[derive(Debug, Clone, Serialize)]
pub struct DiskRestoreSummaryDto {
    pub partitions: u64,
    pub bytes_written: u64,
    pub duration_secs: f64,
}

/// Progress payload for a whole-disk restore (event `restore-disk-progress`).
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub struct DiskRestoreProgressDto {
    pub phase: String,
    pub partition_index: u64,
    pub partition_count: u64,
    pub part_label: String,
    pub percent: u64,
    pub bytes_written: u64,
}

/// List whole-disk (`.obdm`) backups found in `folder`. Cross-platform: it only
/// reads/parses the JSON manifests (restore itself is Windows-only for now).
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

/// Request cancellation of the currently running engine operation.
#[tauri::command]
pub async fn cancel_engine_op(state: State<'_, GuiState>) -> Result<(), String> {
    state
        .cancel_flag
        .store(true, std::sync::atomic::Ordering::SeqCst);
    Ok(())
}

#[cfg(target_os = "windows")]
mod win {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use tauri::{AppHandle, Emitter, State};

    use own_backup_backup::engine_win::{
        mount_image, restore_disk, restore_volume, unmount_image, verify_image, DiskRestoreRequest,
        VolumeRestoreRequest,
    };
    use own_backup_common::OwnBackupError;

    use super::*;

    fn check_cancel(flag: &AtomicBool) -> own_backup_common::Result<()> {
        if flag.swap(false, Ordering::SeqCst) {
            Err(OwnBackupError::Cancelled)
        } else {
            Ok(())
        }
    }

    #[tauri::command]
    pub async fn restore_now(
        app: AppHandle,
        state: State<'_, GuiState>,
        image_path: String,
        target: String,
    ) -> Result<RestoreSummaryDto, String> {
        state.cancel_flag.store(false, Ordering::SeqCst);
        let cancel: Arc<AtomicBool> = state.cancel_flag.clone();

        let request = VolumeRestoreRequest {
            image_path: std::path::PathBuf::from(&image_path),
            target,
            verify_checksums: true,
        };

        let summary = tauri::async_runtime::spawn_blocking(move || {
            let mut last_permille = u64::MAX;
            let mut last_emit = std::time::Instant::now();
            restore_volume(&request, |p| {
                check_cancel(&cancel)?;
                let permille = if p.blocks_total > 0 {
                    p.blocks_done * 1000 / p.blocks_total
                } else {
                    0
                };
                if permille != last_permille || last_emit.elapsed().as_millis() >= 400 {
                    last_permille = permille;
                    last_emit = std::time::Instant::now();
                    let _ = app.emit(
                        "restore-progress",
                        EngineProgress {
                            percent: permille / 10,
                            blocks_done: p.blocks_done,
                            blocks_total: p.blocks_total,
                            bytes_done: p.bytes_written,
                            speed_mbps: p.speed_bps as f64 / 1_000_000.0,
                        },
                    );
                }
                Ok(())
            })
        })
        .await
        .map_err(|e| format!("задача восстановления прервана: {e}"))?
        .map_err(|e| e.to_string())?;

        Ok(RestoreSummaryDto {
            blocks_restored: summary.blocks_restored,
            bytes_restored: summary.bytes_restored,
            duration_secs: summary.duration.as_secs_f64(),
        })
    }

    #[tauri::command]
    pub async fn verify_now(
        app: AppHandle,
        state: State<'_, GuiState>,
        image_path: String,
    ) -> Result<VerifySummaryDto, String> {
        state.cancel_flag.store(false, Ordering::SeqCst);
        let cancel: Arc<AtomicBool> = state.cancel_flag.clone();
        let path = std::path::PathBuf::from(&image_path);

        let summary = tauri::async_runtime::spawn_blocking(move || {
            let mut last_permille = u64::MAX;
            let mut last_emit = std::time::Instant::now();
            verify_image(&path, |done, total| {
                check_cancel(&cancel)?;
                let permille = if total > 0 { done * 1000 / total } else { 0 };
                if permille != last_permille || last_emit.elapsed().as_millis() >= 400 {
                    last_permille = permille;
                    last_emit = std::time::Instant::now();
                    let _ = app.emit(
                        "verify-progress",
                        EngineProgress {
                            percent: permille / 10,
                            blocks_done: done,
                            blocks_total: total,
                            bytes_done: 0,
                            speed_mbps: 0.0,
                        },
                    );
                }
                Ok(())
            })
        })
        .await
        .map_err(|e| format!("задача проверки прервана: {e}"))?
        .map_err(|e| e.to_string())?;

        Ok(VerifySummaryDto {
            valid: summary.valid(),
            blocks_checked: summary.blocks_checked,
            mismatched_blocks: summary.mismatched_blocks,
            duration_secs: summary.duration.as_secs_f64(),
        })
    }

    #[tauri::command]
    pub async fn mount_backup(image_path: String) -> Result<String, String> {
        let path = std::path::PathBuf::from(&image_path);
        tauri::async_runtime::spawn_blocking(move || mount_image(&path))
            .await
            .map_err(|e| format!("задача монтирования прервана: {e}"))?
            .map_err(|e| e.to_string())
    }

    #[tauri::command]
    pub async fn unmount_backup(image_path: String) -> Result<(), String> {
        let path = std::path::PathBuf::from(&image_path);
        tauri::async_runtime::spawn_blocking(move || unmount_image(&path))
            .await
            .map_err(|e| format!("задача отключения прервана: {e}"))?
            .map_err(|e| e.to_string())
    }

    /// Restore a WHOLE disk from a `.obdm` manifest onto `target_disk`.
    /// DESTRUCTIVE. Blocked on the live system disk unless `force` (the guard
    /// still refuses the running Windows disk outside a recovery environment).
    #[tauri::command]
    pub async fn restore_disk_now(
        app: AppHandle,
        manifest_path: String,
        target_disk: u32,
        force: bool,
    ) -> Result<DiskRestoreSummaryDto, String> {
        if !force {
            own_backup_restore::guard_target_disk(target_disk).map_err(|e| e.to_string())?;
        }
        let request = DiskRestoreRequest {
            manifest_path: std::path::PathBuf::from(&manifest_path),
            target_disk,
            verify_checksums: true,
        };

        let summary = tauri::async_runtime::spawn_blocking(move || {
            let mut last = String::new();
            restore_disk(&request, |p| {
                let percent = if p.blocks_total > 0 {
                    p.blocks_done * 100 / p.blocks_total
                } else {
                    0
                };
                let key = format!("{}:{}:{}", p.phase, p.partition_index, percent);
                if key != last {
                    last = key;
                    let _ = app.emit(
                        "restore-disk-progress",
                        DiskRestoreProgressDto {
                            phase: p.phase.clone(),
                            partition_index: p.partition_index as u64,
                            partition_count: p.partition_count as u64,
                            part_label: p.part_label.clone(),
                            percent,
                            bytes_written: p.bytes_written,
                        },
                    );
                }
                Ok(())
            })
        })
        .await
        .map_err(|e| format!("задача восстановления диска прервана: {e}"))?
        .map_err(|e| e.to_string())?;

        Ok(DiskRestoreSummaryDto {
            partitions: summary.partitions as u64,
            bytes_written: summary.bytes_written,
            duration_secs: summary.duration.as_secs_f64(),
        })
    }
}

// Glob re-export is required: #[tauri::command] generates hidden __cmd__*
// items that generate_handler! resolves at this module path.
#[cfg(target_os = "windows")]
pub use win::*;

#[cfg(not(target_os = "windows"))]
mod stub {
    use super::*;
    use tauri::{AppHandle, State};

    const MSG: &str = "Эта операция требует Windows (движок VHDX). \
                       Поддержка Linux запланирована (план v2, фаза 8).";

    #[tauri::command]
    pub async fn restore_now(
        _app: AppHandle,
        _state: State<'_, GuiState>,
        _image_path: String,
        _target: String,
    ) -> Result<RestoreSummaryDto, String> {
        Err(MSG.into())
    }

    #[tauri::command]
    pub async fn verify_now(
        _app: AppHandle,
        _state: State<'_, GuiState>,
        _image_path: String,
    ) -> Result<VerifySummaryDto, String> {
        Err(MSG.into())
    }

    #[tauri::command]
    pub async fn mount_backup(_image_path: String) -> Result<String, String> {
        Err(MSG.into())
    }

    #[tauri::command]
    pub async fn unmount_backup(_image_path: String) -> Result<(), String> {
        Err(MSG.into())
    }

    #[tauri::command]
    pub async fn restore_disk_now(
        _app: AppHandle,
        _manifest_path: String,
        _target_disk: u32,
        _force: bool,
    ) -> Result<DiskRestoreSummaryDto, String> {
        Err(MSG.into())
    }
}

// See note above: glob re-export carries the hidden __cmd__* items.
#[cfg(not(target_os = "windows"))]
pub use stub::*;
