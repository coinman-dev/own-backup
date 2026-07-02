use std::sync::Arc;

use tonic::{Request, Response, Status};
use tracing::info;

use crate::app_state::AppState;

pub mod proto {
    tonic::include_proto!("ownbackup");
}

use proto::backup_service_server::{BackupService, BackupServiceServer};
use proto::*;

pub struct BackupServiceImpl {
    state: Arc<AppState>,
    /// Expected bearer token. Empty string means auth is disabled (dev/TCP mode).
    service_token: String,
}

impl BackupServiceImpl {
    pub fn new(state: Arc<AppState>, service_token: String) -> Self {
        Self { state, service_token }
    }

    pub fn into_service(self) -> BackupServiceServer<BackupServiceImpl> {
        BackupServiceServer::new(self)
    }
}

/// Per-request auth interceptor that checks the token stored in the service instance.
/// This is a workaround since tonic's interceptor API requires static closures.
/// The auth check happens within each RPC handler via the service_token field.
/// If token is empty (dev mode), auth is skipped.

/// Authenticate an incoming gRPC request via bearer token in Authorization metadata.
///
/// If `expected_token` is empty, auth is disabled (dev/TCP mode).
/// Otherwise, the request must include `Authorization: Bearer <token>` where token matches.
/// Returns Ok(()) if auth succeeds, or Status::unauthenticated if auth fails.
fn check_auth<T>(req: &tonic::Request<T>, expected_token: &str) -> Result<(), Status> {
    // Auth is disabled in dev mode (empty token).
    if expected_token.is_empty() {
        return Ok(());
    }

    let provided = req
        .metadata()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .unwrap_or("");

    // Constant-time comparison to prevent timing oracle on the token value.
    if !constant_time_eq(provided.as_bytes(), expected_token.as_bytes()) {
        tracing::warn!(target: "security", "gRPC request rejected: invalid bearer token");
        return Err(Status::unauthenticated("Invalid or missing token"));
    }

    Ok(())
}

/// Constant-time byte slice comparison.
/// Both slices must have equal length; if not, returns false without leaking which byte differed.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Validate that a string looks like a device path for the current platform.
///
/// Windows: `\\.\PhysicalDrive0`, `\\.\C:`, or a plain volume letter `C:` / `C:\`.
/// Linux: `/dev/...`.
fn is_valid_device_path(path: &str) -> bool {
    #[cfg(target_os = "windows")]
    {
        if path.starts_with(r"\\.\") {
            return true;
        }
        let b = path.as_bytes();
        matches!(b.len(), 2 | 3)
            && b[0].is_ascii_alphabetic()
            && b[1] == b':'
            && (b.len() == 2 || b[2] == b'\\')
    }
    #[cfg(not(target_os = "windows"))]
    {
        path.starts_with("/dev/")
    }
}

#[tonic::async_trait]
impl BackupService for BackupServiceImpl {
    async fn list_disks(
        &self,
        request: Request<Empty>,
    ) -> Result<Response<DiskListResponse>, Status> {
        check_auth(&request, &self.service_token)?;
        info!("gRPC: ListDisks");

        #[cfg(target_os = "windows")]
        {
            use own_backup_inventory::DiskInventory;
            let inventory = own_backup_inventory::WmiInventory::new()
                .map_err(|e| Status::internal(e.to_string()))?;
            match inventory.enumerate_disks().await {
                Ok(disks) => {
                    let proto_disks: Vec<DiskInfo> = disks.into_iter().map(disk_to_proto).collect();
                    Ok(Response::new(DiskListResponse { disks: proto_disks }))
                }
                Err(e) => Err(Status::internal(e.to_string())),
            }
        }

        #[cfg(not(target_os = "windows"))]
        {
            use own_backup_inventory::DiskInventory;
            let inventory = own_backup_inventory::MockInventory::default();
            match inventory.enumerate_disks().await {
                Ok(disks) => {
                    let proto_disks = disks.into_iter().map(disk_to_proto).collect();
                    Ok(Response::new(DiskListResponse { disks: proto_disks }))
                }
                Err(e) => Err(Status::internal(e.to_string())),
            }
        }
    }

    async fn run_preflight(
        &self,
        request: Request<PreflightRequest>,
    ) -> Result<Response<PreflightResponse>, Status> {
        check_auth(&request, &self.service_token)?;
        let req = request.into_inner();
        info!("gRPC: RunPreflight for {}", req.source_device);

        let analyzer = own_backup_inventory::PreflightAnalyzer::new();
        match analyzer.analyze(&req.source_device) {
            Ok(report) => Ok(Response::new(PreflightResponse {
                boot_mode: format!("{}", report.boot_mode),
                partition_table: format!("{:?}", report.partition_table),
                disk_type: format!("{:?}", report.disk_type),
                filesystem: format!("{:?}", report.filesystem),
                support_tier: format!("{}", report.support_tier),
                vss_service_running: report.vss_service_running,
                vss_writers_healthy: report.vss_writers_healthy,
                free_space_on_target: report.free_space_on_target,
                warnings: report.warnings,
                blockers: report.blockers,
            })),
            Err(e) => Err(Status::internal(e.to_string())),
        }
    }

    async fn create_backup_job(
        &self,
        request: Request<CreateJobRequest>,
    ) -> Result<Response<BackupJobResponse>, Status> {
        check_auth(&request, &self.service_token)?;
        let req = request.into_inner();
        tracing::info!(
            target: "security",
            job_name = %req.name,
            source_device = %req.source_device,
            encryption_enabled = req.encryption_enabled,
            "gRPC: CreateBackupJob"
        );

        // Input validation
        if req.name.is_empty() || req.name.len() > 256 {
            return Err(Status::invalid_argument("name must be 1-256 characters"));
        }
        if req.source_device.is_empty() || req.source_device.len() > 512 {
            return Err(Status::invalid_argument("source_device must be 1-512 characters"));
        }
        // Validate source_device looks like a device path, not a filesystem path.
        // Windows: \\.\PhysicalDrive0, \\.\C: or a volume letter C: / C:\
        // Linux: /dev/...
        if !is_valid_device_path(&req.source_device) {
            return Err(Status::invalid_argument(
                "source_device must be a device path (e.g. \\\\.\\PhysicalDrive0, C: or /dev/sda)",
            ));
        }
        if req.target_path.is_empty() || req.target_path.len() > 1024 {
            return Err(Status::invalid_argument("target_path must be 1-1024 characters"));
        }
        if !req.schedule.is_empty() {
            if let Err(e) = own_backup_scheduler::parse_schedule(&req.schedule) {
                return Err(Status::invalid_argument(format!("Invalid schedule: {}", e)));
            }
        }
        if req.compression_level < 0 || req.compression_level > 19 {
            return Err(Status::invalid_argument("compression_level must be 0-19"));
        }

        let job_id = uuid::Uuid::new_v4();
        let now = chrono::Utc::now();

        let conn = own_backup_metadata::open_database(&self.state.db_path)
            .map_err(|e| Status::internal(e.to_string()))?;

        let job = own_backup_metadata::BackupJob {
            id: job_id,
            name: req.name.clone(),
            source_type: "partition".into(),
            source_device: req.source_device.clone(),
            target_path: req.target_path.clone(),
            schedule: if req.schedule.is_empty() {
                None
            } else {
                Some(req.schedule.clone())
            },
            compression_level: req.compression_level,
            encryption_enabled: req.encryption_enabled,
            created_at: now,
            updated_at: now,
        };

        own_backup_metadata::insert_job(&conn, &job)
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(BackupJobResponse {
            id: job_id.to_string(),
            name: req.name,
            source_device: req.source_device,
            target_path: req.target_path,
            schedule: req.schedule,
            created_at: now.to_rfc3339(),
        }))
    }

    async fn list_backup_jobs(
        &self,
        request: Request<Empty>,
    ) -> Result<Response<JobListResponse>, Status> {
        check_auth(&request, &self.service_token)?;
        info!("gRPC: ListBackupJobs");

        let conn = own_backup_metadata::open_database(&self.state.db_path)
            .map_err(|e| Status::internal(e.to_string()))?;

        let jobs = own_backup_metadata::list_jobs(&conn)
            .map_err(|e| Status::internal(e.to_string()))?;

        let proto_jobs = jobs
            .into_iter()
            .map(|j| BackupJobResponse {
                id: j.id.to_string(),
                name: j.name,
                source_device: j.source_device,
                target_path: j.target_path,
                schedule: j.schedule.unwrap_or_default(),
                created_at: j.created_at.to_rfc3339(),
            })
            .collect();

        Ok(Response::new(JobListResponse { jobs: proto_jobs }))
    }

    async fn start_backup(
        &self,
        request: Request<StartBackupRequest>,
    ) -> Result<Response<BackupProgressResponse>, Status> {
        check_auth(&request, &self.service_token)?;
        let req = request.into_inner();

        // Input validation
        uuid::Uuid::parse_str(&req.job_id)
            .map_err(|e| Status::invalid_argument(format!("Invalid job_id: {}", e)))?;
        let valid_backup_types = ["full", "incremental", "differential", ""];
        if !valid_backup_types.contains(&req.backup_type.to_lowercase().as_str()) {
            return Err(Status::invalid_argument(format!(
                "Invalid backup_type '{}'. Must be: full, incremental, or differential",
                req.backup_type
            )));
        }

        tracing::info!(
            target: "security",
            job_id = %req.job_id,
            backup_type = %req.backup_type,
            "gRPC: StartBackup rejected — engine not wired up yet"
        );
        Err(Status::unimplemented(
            "StartBackup is not implemented yet (backup engine wiring: plan v2, phase 4)",
        ))
    }

    async fn get_backup_progress(
        &self,
        request: Request<BackupId>,
    ) -> Result<Response<BackupProgressResponse>, Status> {
        check_auth(&request, &self.service_token)?;
        Err(Status::unimplemented(
            "GetBackupProgress is not implemented yet (plan v2, phase 4)",
        ))
    }

    async fn cancel_backup(
        &self,
        request: Request<BackupId>,
    ) -> Result<Response<Empty>, Status> {
        check_auth(&request, &self.service_token)?;
        Err(Status::unimplemented(
            "CancelBackup is not implemented yet (plan v2, phase 4)",
        ))
    }

    type StreamBackupProgressStream =
        tokio_stream::wrappers::ReceiverStream<Result<BackupProgressResponse, Status>>;

    async fn stream_backup_progress(
        &self,
        request: Request<BackupId>,
    ) -> Result<Response<Self::StreamBackupProgressStream>, Status> {
        check_auth(&request, &self.service_token)?;
        Err(Status::unimplemented(
            "StreamBackupProgress is not implemented yet (plan v2, phase 4)",
        ))
    }

    async fn list_backup_sets(
        &self,
        request: Request<JobId>,
    ) -> Result<Response<BackupSetListResponse>, Status> {
        check_auth(&request, &self.service_token)?;
        let job_id_str = request.into_inner().id;
        info!("gRPC: ListBackupSets for job {}", job_id_str);

        let job_uuid = uuid::Uuid::parse_str(&job_id_str)
            .map_err(|e| Status::invalid_argument(format!("Invalid job_id: {}", e)))?;

        let conn = own_backup_metadata::open_database(&self.state.db_path)
            .map_err(|e| Status::internal(e.to_string()))?;

        let sets = own_backup_metadata::list_sets_for_job(&conn, &job_uuid)
            .map_err(|e| Status::internal(e.to_string()))?;

        let proto_sets = sets
            .into_iter()
            .map(|s| BackupSetInfo {
                id: s.id.to_string(),
                job_id: s.job_id.to_string(),
                backup_type: format!("{:?}", s.backup_type),
                parent_set_id: s.parent_set_id.map(|u| u.to_string()).unwrap_or_default(),
                file_path: s.file_path,
                file_size: s.file_size.unwrap_or(0) as u64,
                source_size: s.source_size as u64,
                status: format!("{:?}", s.status),
                started_at: s.started_at.map(|t| t.to_rfc3339()).unwrap_or_default(),
                completed_at: s.completed_at.map(|t| t.to_rfc3339()).unwrap_or_default(),
            })
            .collect();

        Ok(Response::new(BackupSetListResponse { sets: proto_sets }))
    }

    async fn start_restore(
        &self,
        request: Request<StartRestoreRequest>,
    ) -> Result<Response<RestoreProgressResponse>, Status> {
        check_auth(&request, &self.service_token)?;
        let req = request.into_inner();

        // Input validation
        uuid::Uuid::parse_str(&req.backup_set_id)
            .map_err(|e| Status::invalid_argument(format!("Invalid backup_set_id: {}", e)))?;
        if req.target_device.is_empty() || req.target_device.len() > 512 {
            return Err(Status::invalid_argument("target_device must be 1-512 characters"));
        }
        // Validate target_device looks like a device path, not a filesystem path.
        if !is_valid_device_path(&req.target_device) {
            return Err(Status::invalid_argument(
                "target_device must be a device path (e.g. \\\\.\\PhysicalDrive0, C: or /dev/sda)",
            ));
        }

        tracing::info!(
            target: "security",
            backup_set_id = %req.backup_set_id,
            target_device = %req.target_device,
            "gRPC: StartRestore rejected — engine not wired up yet"
        );
        Err(Status::unimplemented(
            "StartRestore is not implemented yet (restore engine wiring: plan v2, phase 4)",
        ))
    }

    type StreamRestoreProgressStream =
        tokio_stream::wrappers::ReceiverStream<Result<RestoreProgressResponse, Status>>;

    async fn stream_restore_progress(
        &self,
        request: Request<RestoreId>,
    ) -> Result<Response<Self::StreamRestoreProgressStream>, Status> {
        check_auth(&request, &self.service_token)?;
        Err(Status::unimplemented(
            "StreamRestoreProgress is not implemented yet (plan v2, phase 4)",
        ))
    }

    async fn validate_backup(
        &self,
        request: Request<BackupSetId>,
    ) -> Result<Response<ValidationResponse>, Status> {
        check_auth(&request, &self.service_token)?;
        info!("gRPC: ValidateBackup rejected — not wired up yet");
        // Never report `valid: true` for a validation that did not run.
        Err(Status::unimplemented(
            "ValidateBackup is not implemented yet (plan v2, phase 4)",
        ))
    }

    async fn set_schedule(
        &self,
        request: Request<SetScheduleRequest>,
    ) -> Result<Response<Empty>, Status> {
        check_auth(&request, &self.service_token)?;
        let req = request.into_inner();

        // Input validation
        uuid::Uuid::parse_str(&req.job_id)
            .map_err(|e| Status::invalid_argument(format!("Invalid job_id: {}", e)))?;
        if !req.cron_expression.is_empty() {
            if let Err(e) = own_backup_scheduler::parse_schedule(&req.cron_expression) {
                return Err(Status::invalid_argument(format!("Invalid cron expression: {}", e)));
            }
        }

        info!("gRPC: SetSchedule job_id={} rejected — persistence not wired up yet", req.job_id);
        Err(Status::unimplemented(
            "SetSchedule is not implemented yet (plan v2, phase 4)",
        ))
    }

    async fn get_schedule(
        &self,
        request: Request<JobId>,
    ) -> Result<Response<ScheduleResponse>, Status> {
        check_auth(&request, &self.service_token)?;
        Err(Status::unimplemented(
            "GetSchedule is not implemented yet (plan v2, phase 4)",
        ))
    }

    async fn get_service_status(
        &self,
        request: Request<Empty>,
    ) -> Result<Response<ServiceStatusResponse>, Status> {
        check_auth(&request, &self.service_token)?;
        let uptime = self.state.uptime_seconds();
        Ok(Response::new(ServiceStatusResponse {
            version: env!("CARGO_PKG_VERSION").into(),
            uptime: format!("{}s", uptime),
            active_jobs: 0,
            total_backups_completed: 0,
        }))
    }
}

fn disk_to_proto(disk: own_backup_common::disk::DiskInfo) -> DiskInfo {
    DiskInfo {
        device_id: disk.device_id,
        model: disk.model,
        serial: disk.serial,
        size_bytes: disk.size_bytes,
        sector_size_logical: disk.sector_size_logical,
        sector_size_physical: disk.sector_size_physical,
        media_type: disk.media_type,
        partition_table: format!("{:?}", disk.partition_table),
        disk_guid: disk.disk_guid,
        partitions: disk
            .partitions
            .into_iter()
            .map(|p| PartitionInfo {
                index: p.index,
                offset_bytes: p.offset_bytes,
                size_bytes: p.size_bytes,
                gpt_type_guid: p.gpt_type_guid,
                gpt_partition_guid: p.gpt_partition_guid,
                filesystem: format!("{:?}", p.filesystem),
                volume_letter: p.volume_letter.map(|c| c.to_string()).unwrap_or_default(),
                volume_label: p.volume_label,
                fs_uuid: p.fs_uuid,
                is_system: p.is_system,
                is_boot: p.is_boot,
            })
            .collect(),
    }
}
