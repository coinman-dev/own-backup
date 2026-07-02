//! Tauri commands that drive the VHDX backup engine directly (plan v2).
//!
//! For now the GUI calls the engine in-process; the gRPC service split comes
//! in phase 4. Long operations run on blocking threads and report progress
//! through Tauri events:
//! - `backup-progress`, `restore-progress`, `verify-progress` — payload
//!   [`EngineProgress`].

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

/// Result of a completed backup, for the summary panel.
#[derive(Debug, Clone, Serialize)]
pub struct BackupSummaryDto {
    pub image_path: String,
    pub sidecar_path: String,
    pub source_bytes: u64,
    pub total_blocks: u64,
    pub used_blocks: u64,
    pub bytes_copied: u64,
    pub duration_secs: f64,
    /// False = sector-by-sector (requested or non-NTFS fallback).
    pub smart_mode: bool,
    /// True = data was read from a VSS shadow copy.
    pub vss_used: bool,
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
        backup_volume, mount_image, restore_volume, unmount_image, verify_image,
        VolumeBackupRequest, VolumeRestoreRequest,
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
    pub async fn backup_now(
        app: AppHandle,
        state: State<'_, GuiState>,
        source: String,
        output_path: String,
        all_sectors: bool,
    ) -> Result<BackupSummaryDto, String> {
        state.cancel_flag.store(false, Ordering::SeqCst);
        let cancel: Arc<AtomicBool> = state.cancel_flag.clone();

        let request = VolumeBackupRequest {
            source,
            image_path: std::path::PathBuf::from(&output_path),
            block_size: own_backup_backup::engine_win::DEFAULT_BLOCK_SIZE,
            all_sectors,
            use_vss: true, // auto-fallback to live volume inside the engine
        };

        let summary = tauri::async_runtime::spawn_blocking(move || {
            let mut last_percent = u64::MAX;
            backup_volume(&request, |p| {
                check_cancel(&cancel)?;
                let percent = if p.blocks_total > 0 {
                    p.blocks_done * 100 / p.blocks_total
                } else {
                    0
                };
                if percent != last_percent {
                    last_percent = percent;
                    let _ = app.emit(
                        "backup-progress",
                        EngineProgress {
                            percent,
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
        .map_err(|e| format!("задача бэкапа прервана: {e}"))?
        .map_err(|e| e.to_string())?;

        Ok(BackupSummaryDto {
            image_path: summary.image_path.display().to_string(),
            sidecar_path: summary.sidecar_path.display().to_string(),
            source_bytes: summary.source_bytes,
            total_blocks: summary.total_blocks,
            used_blocks: summary.used_blocks,
            bytes_copied: summary.bytes_copied,
            duration_secs: summary.duration.as_secs_f64(),
            smart_mode: summary.smart_mode,
            vss_used: summary.vss_used,
        })
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
            let mut last_percent = u64::MAX;
            restore_volume(&request, |p| {
                check_cancel(&cancel)?;
                let percent = if p.blocks_total > 0 {
                    p.blocks_done * 100 / p.blocks_total
                } else {
                    0
                };
                if percent != last_percent {
                    last_percent = percent;
                    let _ = app.emit(
                        "restore-progress",
                        EngineProgress {
                            percent,
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
            let mut last_percent = u64::MAX;
            verify_image(&path, |done, total| {
                check_cancel(&cancel)?;
                let percent = if total > 0 { done * 100 / total } else { 0 };
                if percent != last_percent {
                    last_percent = percent;
                    let _ = app.emit(
                        "verify-progress",
                        EngineProgress {
                            percent,
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
    pub async fn backup_now(
        _app: AppHandle,
        _state: State<'_, GuiState>,
        _source: String,
        _output_path: String,
        _all_sectors: bool,
    ) -> Result<BackupSummaryDto, String> {
        Err(MSG.into())
    }

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
}

// See note above: glob re-export carries the hidden __cmd__* items.
#[cfg(not(target_os = "windows"))]
pub use stub::*;
