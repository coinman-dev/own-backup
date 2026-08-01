//! gRPC surface over the orchestrator core (plan v2, phase 4).
//!
//! Handlers are thin: they delegate to [`own_backup_core`] (jobs, queue,
//! activity) and serialize the shared Rust types to JSON envelopes. Progress
//! and lifecycle events are fanned out to `StreamEvents` subscribers via a
//! broadcast channel fed by [`GrpcEventSink`].

use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};
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

/// Bridges core orchestration events onto the broadcast channel that
/// `StreamEvents` reads. `send` errors only when there are no subscribers,
/// which is fine — the events are simply dropped until a client connects.
pub struct GrpcEventSink {
    pub tx: broadcast::Sender<proto::Event>,
}

impl own_backup_core::EventSink for GrpcEventSink {
    fn job_progress(&self, progress: own_backup_core::JobProgress) {
        let _ = self.tx.send(Event {
            kind: "progress".into(),
            json: serde_json::to_string(&progress).unwrap_or_default(),
        });
    }
    fn queue_changed(&self, snapshot: own_backup_core::QueueSnapshot) {
        let _ = self.tx.send(Event {
            kind: "queue".into(),
            json: serde_json::to_string(&snapshot).unwrap_or_default(),
        });
    }
    fn job_finished(&self, run: own_backup_core::JobRun) {
        let _ = self.tx.send(Event {
            kind: "finished".into(),
            json: serde_json::to_string(&run).unwrap_or_default(),
        });
    }
}

/// Authenticate an incoming request via bearer token. Empty expected token =
/// auth disabled (dev/TCP mode). Constant-time comparison avoids a timing oracle.
fn check_auth<T>(req: &Request<T>, expected: &str) -> Result<(), Status> {
    if expected.is_empty() {
        return Ok(());
    }
    let provided = req
        .metadata()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .unwrap_or("");
    if !constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
        tracing::warn!(target: "security", "gRPC request rejected: invalid bearer token");
        return Err(Status::unauthenticated("Invalid or missing token"));
    }
    Ok(())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Serialize any value into a `JsonReply`.
fn json_reply<T: serde::Serialize>(value: &T) -> Result<Response<JsonReply>, Status> {
    let json = serde_json::to_string(value)
        .map_err(|e| Status::internal(format!("serialize failed: {e}")))?;
    Ok(Response::new(JsonReply { json }))
}

/// Drive letters in use on this machine. A dry run needs them to resolve
/// conflicts (D11), and only the service can see them — the GUI runs
/// unelevated and would be guessing.
fn taken_drive_letters() -> Vec<char> {
    #[cfg(target_os = "windows")]
    {
        own_backup_backup::letters_win::taken_letters()
    }
    #[cfg(not(target_os = "windows"))]
    {
        Vec::new()
    }
}

/// Whether `disk` is the one the running OS boots from.
///
/// Informational now, not a refusal: what gets refused is a plan that touches
/// the partitions that system is standing on (`protected_partitions`).
fn is_system_disk(disk: u32) -> bool {
    own_backup_restore::system_disk_number() == Some(disk)
        && !own_backup_restore::is_boot_media()
}

/// Replace the plan's idea of the target disk's size with the device's own.
///
/// The designer gets the figure from the inventory, and an inventory figure can
/// be a little short of the truth. Every bound in the checks — where the backup
/// GPT goes, whether the last partition fits — is measured against it, so a
/// disk reported a few megabytes small makes a perfectly valid layout look as
/// if its last partition runs off the end.
///
/// The executor already resolves this the same way when it writes the table
/// (`DiskSession::write_partition_table`). Doing it here too means the dry run
/// checks the same geometry that will actually be written, rather than a second
/// opinion about it.
fn correct_disk_size(plan: &mut own_backup_common::restore_plan::RestorePlan) {
    #[cfg(target_os = "windows")]
    {
        use own_backup_common::restore_plan::RestoreOp;
        let path = format!(r"\\.\PhysicalDrive{}", plan.target_disk);
        let Ok(dev) = own_backup_backup::win_device::WinRawDevice::open_read(&path) else {
            return; // not reachable right now; the checks fall back to the plan's figure
        };
        let actual = dev.length();
        for op in plan.ops.iter_mut() {
            if let RestoreOp::WritePartitionTable { table } = op {
                if table.disk_size_bytes != actual {
                    tracing::debug!(
                        disk = plan.target_disk,
                        planned = table.disk_size_bytes,
                        actual,
                        "corrected the plan's disk size from the device"
                    );
                    table.disk_size_bytes = actual;
                }
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = plan;
    }
}

/// Message returned by the direct engine RPCs when built off Windows (the VHDX
/// engine is Windows-only for now — see plan phase 8).
#[cfg(not(target_os = "windows"))]
const ENGINE_MSG: &str = "Эта операция требует Windows (движок VHDX). \
                          Поддержка Linux запланирована (план v2, фаза 8).";

/// Broadcast one engine-progress event (`restore` / `verify` / `restore-disk`)
/// to `StreamEvents` subscribers. Called from the blocking engine thread.
#[cfg(target_os = "windows")]
fn emit_engine<T: serde::Serialize>(
    events: &broadcast::Sender<proto::Event>,
    kind: &str,
    payload: &T,
) {
    let _ = events.send(Event {
        kind: kind.into(),
        json: serde_json::to_string(payload).unwrap_or_default(),
    });
}

/// Shared save/run validation: name + at least one source, dest non-empty, and
/// the destination not on ANY of the job's source volumes.
fn validate_job(job: &own_backup_core::Job) -> Result<(), Status> {
    if job.name.trim().is_empty() {
        return Err(Status::invalid_argument("Укажите имя задания."));
    }
    if job.source_list().is_empty() {
        return Err(Status::invalid_argument("Выберите источник (том)."));
    }
    if job.dest_folder.trim().is_empty() {
        return Err(Status::invalid_argument("Выберите папку назначения."));
    }
    if let Some(letter) = own_backup_core::dest_collision_letter(job, &job.dest_folder) {
        return Err(Status::invalid_argument(format!(
            "Нельзя сохранять копию на тот же диск, который копируется ({letter}:). \
             Выберите для назначения другой диск."
        )));
    }
    // The image format must never change over existing archives (decision
    // 2026-07-08): reject the save right away when the destination already
    // holds this job's backups in a different container. The runners repeat
    // the same check before every run (defence in depth).
    let folder = std::path::Path::new(&job.dest_folder);
    if folder.is_dir() {
        if let Ok(chains) = own_backup_backup::chains::list_job_chains(folder, &job.id) {
            let want = job.image_format.extension();
            if chains.iter().flat_map(|c| c.images.iter()).any(|i| i.container != want) {
                return Err(Status::invalid_argument(
                    "Нельзя изменить формат образа: у задания уже есть резервные копии \
                     в другом формате. Создайте новое задание (или удалите существующие \
                     копии этого задания).",
                ));
            }
        }
    }
    Ok(())
}

#[tonic::async_trait]
impl BackupService for BackupServiceImpl {
    // --- Jobs -------------------------------------------------------------

    async fn list_jobs(&self, req: Request<Empty>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        json_reply(&self.state.store.list())
    }

    async fn get_job(&self, req: Request<IdRequest>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        json_reply(&self.state.store.get(&req.into_inner().id))
    }

    async fn save_job(&self, req: Request<JsonRequest>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        let job: own_backup_core::Job = serde_json::from_str(&req.into_inner().json)
            .map_err(|e| Status::invalid_argument(format!("bad job JSON: {e}")))?;
        validate_job(&job)?;
        json_reply(&self.state.store.upsert(job))
    }

    async fn delete_job(&self, req: Request<DeleteJobRequest>) -> Result<Response<Empty>, Status> {
        check_auth(&req, &self.service_token)?;
        let DeleteJobRequest { id, delete_files } = req.into_inner();
        if delete_files {
            // Erase the images this job wrote (image + adjacent `.obkm` sidecar).
            // Best-effort: a missing file is fine; we never fail the delete on it.
            for f in self.state.store.job_image_files(&id) {
                let sc = own_backup_backup::sidecar_path_for(&f);
                if let Err(e) = std::fs::remove_file(&f) {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        tracing::warn!(file = %f.display(), error = %e, "delete_job: could not remove image file");
                    }
                }
                let _ = std::fs::remove_file(&sc);
            }
        }
        self.state.store.delete(&id);
        Ok(Response::new(Empty {}))
    }

    async fn new_job_template(&self, req: Request<Empty>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        let name = std::env::var("COMPUTERNAME")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| std::env::var("HOSTNAME").ok().filter(|s| !s.trim().is_empty()))
            .unwrap_or_else(|| "Этот компьютер".into());
        json_reply(&own_backup_core::Job {
            name,
            ..Default::default()
        })
    }

    async fn default_exclusions(&self, req: Request<Empty>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        // JSON: { "windows": [string], "linux": [string] } — one default list
        // per OS family for the Windows/Linux exclusion tabs.
        json_reply(&serde_json::json!({
            "windows": own_backup_core::default_exclusions(),
            "linux": own_backup_core::default_linux_exclusions(),
        }))
    }

    // --- Queue / runs -----------------------------------------------------

    async fn run_job(&self, req: Request<IdRequest>) -> Result<Response<Empty>, Status> {
        check_auth(&req, &self.service_token)?;
        let id = req.into_inner().id;
        let job = self
            .state
            .store
            .get(&id)
            .ok_or_else(|| Status::not_found("Задание не найдено."))?;
        // Guard against writing onto any source volume (mirrors save-time check).
        if let Some(letter) = own_backup_core::dest_collision_letter(&job, &job.dest_folder) {
            return Err(Status::failed_precondition(format!(
                "Копия пишется на тот же диск ({letter}:), что и источник — запуск отменён."
            )));
        }
        // Skip if already pending or running.
        {
            let pending = self.state.queue.pending.lock().unwrap();
            let current = self.state.queue.current.lock().unwrap();
            if pending.iter().any(|j| *j == id) || current.as_deref() == Some(id.as_str()) {
                return Ok(Response::new(Empty {}));
            }
        }
        self.state.queue.pending.lock().unwrap().push_back(id.clone());
        self.state
            .job_tx
            .send(id)
            .map_err(|e| Status::internal(format!("enqueue failed: {e}")))?;
        Ok(Response::new(Empty {}))
    }

    async fn cancel_queued(&self, req: Request<IdRequest>) -> Result<Response<Empty>, Status> {
        check_auth(&req, &self.service_token)?;
        let id = req.into_inner().id;
        self.state.queue.pending.lock().unwrap().retain(|j| *j != id);
        Ok(Response::new(Empty {}))
    }

    async fn cancel_running(&self, req: Request<Empty>) -> Result<Response<Empty>, Status> {
        check_auth(&req, &self.service_token)?;
        self.state
            .cancel
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(Response::new(Empty {}))
    }

    async fn get_queue(&self, req: Request<Empty>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        json_reply(&self.state.queue.snapshot())
    }

    async fn list_activity(&self, req: Request<Empty>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        json_reply(&self.state.store.activity())
    }

    type StreamEventsStream = Pin<Box<dyn Stream<Item = Result<Event, Status>> + Send>>;

    async fn stream_events(
        &self,
        req: Request<Empty>,
    ) -> Result<Response<Self::StreamEventsStream>, Status> {
        check_auth(&req, &self.service_token)?;
        // Drop "lagged" notices silently; a slow client just misses old events.
        let stream = BroadcastStream::new(self.state.events.subscribe())
            .filter_map(|r| r.ok().map(Ok));
        Ok(Response::new(Box::pin(stream)))
    }

    // --- Encrypted volumes ------------------------------------------------

    async fn provide_volume_password(
        &self,
        req: Request<JsonRequest>,
    ) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        use own_backup_common::engine_dto::VolumeUnlockDto;
        #[derive(serde::Deserialize)]
        struct Payload {
            volume: String,
            password: String,
            #[serde(default)]
            remember: bool,
        }
        let p: Payload = serde_json::from_str(&req.into_inner().json)
            .map_err(|e| Status::invalid_argument(format!("bad JSON: {e}")))?;

        // Unlock + probe HERE (LocalSystem/root has raw-disk access; the GUI does
        // not — doing it there fails with CreateFileW access-denied on the disk).
        #[cfg(target_os = "windows")]
        let dto = {
            let (vol, pw) = (p.volume.clone(), p.password.clone());
            let probe = tokio::task::spawn_blocking(move || {
                own_backup_backup::engine_win::unlock_and_probe(&vol, &pw)
            })
            .await
            .map_err(|e| Status::internal(format!("probe task panicked: {e}")))?
            .map_err(|e| Status::internal(e.to_string()))?;
            VolumeUnlockDto {
                ok: probe.ok,
                filesystem: probe.filesystem,
                label: probe.label,
                used_bytes: probe.used_bytes,
            }
        };
        #[cfg(not(target_os = "windows"))]
        let dto = VolumeUnlockDto {
            ok: true,
            filesystem: String::new(),
            label: String::new(),
            used_bytes: 0,
        };

        // Only cache a password that actually unlocked the volume. Key by the
        // same token the backup path resolves against.
        if dto.ok {
            let token = own_backup_core::source_token(&p.volume);
            self.state
                .passwords
                .remember(&token, p.password, p.remember)
                .map_err(|e| Status::internal(e.to_string()))?;
        }
        json_reply(&dto)
    }

    // --- Inventory --------------------------------------------------------

    async fn list_disks(&self, req: Request<Empty>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        info!("gRPC: ListDisks");
        use own_backup_inventory::DiskInventory;

        #[cfg(target_os = "windows")]
        let disks = own_backup_inventory::WindowsInventory::new()
            .map_err(|e| Status::internal(e.to_string()))?
            .enumerate_disks()
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        #[cfg(target_os = "linux")]
        let disks = own_backup_inventory::LinuxInventory::new()
            .enumerate_disks()
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        let disks = own_backup_inventory::MockInventory::default()
            .enumerate_disks()
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        json_reply(&disks)
    }

    async fn get_service_status(
        &self,
        req: Request<Empty>,
    ) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        let internal_mounts = self.state.active_mounts.lock().unwrap().clone();

        #[cfg(target_os = "windows")]
        let active_mounts = own_backup_backup::engine_win::get_all_active_mounts(&internal_mounts);

        #[cfg(target_os = "linux")]
        let active_mounts = own_backup_backup::engine_linux::get_all_active_mounts(&internal_mounts);

        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        let active_mounts = internal_mounts;

        let status = serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "uptime_secs": self.state.uptime_seconds(),
            "queue": self.state.queue.snapshot(),
            "active_mounts": active_mounts,
        });
        Ok(Response::new(JsonReply { json: status.to_string() }))
    }

    // --- Direct engine operations ----------------------------------------

    async fn restore_volume(
        &self,
        req: Request<JsonRequest>,
    ) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        #[cfg(target_os = "windows")]
        {
            use std::sync::atomic::Ordering;
            use own_backup_backup::engine_win::{restore_volume, VolumeRestoreRequest};
            use own_backup_common::engine_dto::{EngineProgress, RestoreSummaryDto};

            #[derive(serde::Deserialize)]
            struct In {
                image_path: String,
                target: String,
            }
            let p: In = serde_json::from_str(&req.into_inner().json)
                .map_err(|e| Status::invalid_argument(format!("bad JSON: {e}")))?;

            self.state.engine_cancel.store(false, Ordering::SeqCst);
            let cancel = self.state.engine_cancel.clone();
            let events = self.state.events.clone();
            let request = VolumeRestoreRequest {
                image_path: std::path::PathBuf::from(&p.image_path),
                target: p.target,
                verify_checksums: true,
            };

            let summary = tokio::task::spawn_blocking(move || {
                let mut last_permille = u64::MAX;
                let mut last_emit = std::time::Instant::now();
                restore_volume(&request, |pr| {
                    if cancel.swap(false, Ordering::SeqCst) {
                        return Err(own_backup_common::OwnBackupError::Cancelled);
                    }
                    let permille = if pr.blocks_total > 0 {
                        pr.blocks_done * 1000 / pr.blocks_total
                    } else {
                        0
                    };
                    if permille != last_permille || last_emit.elapsed().as_millis() >= 400 {
                        last_permille = permille;
                        last_emit = std::time::Instant::now();
                        emit_engine(
                            &events,
                            "restore",
                            &EngineProgress {
                                percent: permille / 10,
                                blocks_done: pr.blocks_done,
                                blocks_total: pr.blocks_total,
                                bytes_done: pr.bytes_written,
                                speed_mbps: pr.speed_bps as f64 / 1_000_000.0,
                            },
                        );
                    }
                    Ok(())
                })
            })
            .await
            .map_err(|e| Status::internal(format!("restore task panicked: {e}")))?
            .map_err(|e| Status::internal(e.to_string()))?;

            json_reply(&RestoreSummaryDto {
                blocks_restored: summary.blocks_restored,
                bytes_restored: summary.bytes_restored,
                duration_secs: summary.duration.as_secs_f64(),
            })
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = req;
            Err(Status::unimplemented(ENGINE_MSG))
        }
    }

    async fn verify_image(&self, req: Request<JsonRequest>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        #[cfg(target_os = "windows")]
        {
            use std::sync::atomic::Ordering;
            use own_backup_backup::engine_win::verify_image;
            use own_backup_common::engine_dto::{EngineProgress, VerifySummaryDto};

            #[derive(serde::Deserialize)]
            struct In {
                image_path: String,
            }
            let p: In = serde_json::from_str(&req.into_inner().json)
                .map_err(|e| Status::invalid_argument(format!("bad JSON: {e}")))?;

            self.state.engine_cancel.store(false, Ordering::SeqCst);
            let cancel = self.state.engine_cancel.clone();
            let events = self.state.events.clone();
            let path = std::path::PathBuf::from(&p.image_path);

            let summary = tokio::task::spawn_blocking(move || {
                let mut last_permille = u64::MAX;
                let mut last_emit = std::time::Instant::now();
                verify_image(&path, |done, total| {
                    if cancel.swap(false, Ordering::SeqCst) {
                        return Err(own_backup_common::OwnBackupError::Cancelled);
                    }
                    let permille = if total > 0 { done * 1000 / total } else { 0 };
                    if permille != last_permille || last_emit.elapsed().as_millis() >= 400 {
                        last_permille = permille;
                        last_emit = std::time::Instant::now();
                        emit_engine(
                            &events,
                            "verify",
                            &EngineProgress {
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
            .map_err(|e| Status::internal(format!("verify task panicked: {e}")))?
            .map_err(|e| Status::internal(e.to_string()))?;

            json_reply(&VerifySummaryDto {
                valid: summary.valid(),
                blocks_checked: summary.blocks_checked,
                mismatched_blocks: summary.mismatched_blocks,
                duration_secs: summary.duration.as_secs_f64(),
            })
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = req;
            Err(Status::unimplemented(ENGINE_MSG))
        }
    }

    async fn mount_image(&self, req: Request<JsonRequest>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        #[cfg(target_os = "windows")]
        {
            use own_backup_backup::engine_win::mount_image;
            use own_backup_common::engine_dto::MountResultDto;

            #[derive(serde::Deserialize)]
            struct In {
                image_path: String,
                read_write: Option<bool>,
            }
            let p: In = serde_json::from_str(&req.into_inner().json)
                .map_err(|e| Status::invalid_argument(format!("bad JSON: {e}")))?;
            let path = std::path::PathBuf::from(&p.image_path);
            let rw = p.read_write.unwrap_or(false);
            let image_path = p.image_path.clone();
            let mount_point = tokio::task::spawn_blocking(move || mount_image(&path, rw))
                .await
                .map_err(|e| Status::internal(format!("mount task panicked: {e}")))?
                .map_err(|e| Status::internal(e.to_string()))?;

            {
                let mut mounts = self.state.active_mounts.lock().unwrap();
                mounts.retain(|m| m.image_path != image_path);
                mounts.push(own_backup_common::engine_dto::ActiveMountDto {
                    image_path,
                    physical_path: mount_point.clone(),
                    read_write: rw,
                    volume_letters: Vec::new(),
                });
            }

            json_reply(&MountResultDto { mount_point })
        }
        #[cfg(target_os = "linux")]
        {
            use own_backup_backup::engine_linux::mount_image;
            use own_backup_common::engine_dto::MountResultDto;

            #[derive(serde::Deserialize)]
            struct In {
                image_path: String,
                read_write: Option<bool>,
            }
            let p: In = serde_json::from_str(&req.into_inner().json)
                .map_err(|e| Status::invalid_argument(format!("bad JSON: {e}")))?;
            let path = std::path::PathBuf::from(&p.image_path);
            let rw = p.read_write.unwrap_or(false);
            let image_path = p.image_path.clone();
            let mount_point = tokio::task::spawn_blocking(move || mount_image(&path, rw))
                .await
                .map_err(|e| Status::internal(format!("mount task panicked: {e}")))?
                .map_err(|e| Status::internal(e.to_string()))?;

            {
                let mut mounts = self.state.active_mounts.lock().unwrap();
                mounts.retain(|m| m.image_path != image_path);
                mounts.push(own_backup_common::engine_dto::ActiveMountDto {
                    image_path,
                    physical_path: mount_point.clone(),
                    read_write: rw,
                    volume_letters: Vec::new(),
                });
            }

            json_reply(&MountResultDto { mount_point })
        }
        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        {
            let _ = req;
            Err(Status::unimplemented(ENGINE_MSG))
        }
    }

    async fn find_base_images(&self, req: Request<Empty>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        media_build_unavailable()?;
        #[cfg(target_os = "windows")]
        {
            use own_backup_common::engine_dto::BaseImageDto;
            // Reads several files off disk; keep it off the async runtime.
            let found = tokio::task::spawn_blocking(own_backup_backup::winre_find::find_base_images)
                .await
                .map_err(|e| Status::internal(format!("base image search panicked: {e}")))?;
            let out: Vec<BaseImageDto> = found
                .into_iter()
                .map(|b| BaseImageDto {
                    path: b.path.display().to_string(),
                    origin: b.origin.describe().to_string(),
                    name: b.info.name,
                    build: b.info.build,
                    language: b.info.language,
                    architecture: b.info.architecture,
                    total_bytes: b.info.total_bytes,
                    warnings: b.warnings,
                })
                .collect();
            info!(count = out.len(), "base images offered");
            return json_reply(&out);
        }
        #[cfg(not(target_os = "windows"))]
        Err(Status::unimplemented(
            "Загрузочный носитель собирается только на Windows",
        ))
    }

    async fn list_drivers(&self, req: Request<Empty>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        media_build_unavailable()?;
        #[cfg(target_os = "windows")]
        {
            use own_backup_common::engine_dto::DriverPackageDto;
            let list = tokio::task::spawn_blocking(
                own_backup_backup::media_build_win::list_driver_packages,
            )
            .await
            .map_err(|e| Status::internal(format!("driver enumeration panicked: {e}")))?;
            let out: Vec<DriverPackageDto> = list
                .into_iter()
                .map(|d| DriverPackageDto {
                    inf: d.inf,
                    original_name: d.original_name,
                    class_name: d.class_name,
                    provider: d.provider,
                    version: d.version,
                    size_bytes: d.size_bytes,
                    recommended: d.recommended,
                    role: d.role,
                    device: d.device,
                })
                .collect();
            return json_reply(&out);
        }
        #[cfg(not(target_os = "windows"))]
        Err(Status::unimplemented("Драйверы перечисляются только на Windows"))
    }

    async fn build_media(&self, req: Request<JsonRequest>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        media_build_unavailable()?;
        #[cfg(target_os = "windows")]
        {
            use own_backup_backup::media_build_win::{
                build_media, clean_up, BuildStage, MediaBuildRequest,
            };
            use own_backup_common::engine_dto::{
                MediaBuildOptionsDto, MediaBuildReportDto, MediaProgressDto, MediaStageDto,
            };

            let opts: MediaBuildOptionsDto = serde_json::from_str(&req.into_inner().json)
                .map_err(|e| Status::invalid_argument(e.to_string()))?;
            if opts.output_iso.trim().is_empty() {
                return Err(Status::invalid_argument("не указан файл ISO"));
            }

            // The base is either what the user picked or the best candidate —
            // the search has already sorted them, best first.
            let base = if opts.base_wim.trim().is_empty() {
                tokio::task::spawn_blocking(own_backup_backup::winre_find::find_base_images)
                    .await
                    .map_err(|e| Status::internal(format!("base image search panicked: {e}")))?
                    .into_iter()
                    .next()
                    .map(|b| b.path)
                    .ok_or_else(|| {
                        Status::failed_precondition(
                            "На этой машине не нашлось образа среды восстановления. \
                             Включите её (reagentc /enable) или укажите Winre.wim вручную.",
                        )
                    })?
            } else {
                std::path::PathBuf::from(&opts.base_wim)
            };

            let program_dir = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.to_path_buf()))
                .ok_or_else(|| Status::internal("не удалось определить папку программы"))?;
            let webview2 = find_webview2_runtime(&program_dir).ok_or_else(|| {
                Status::failed_precondition(
                    "Не найден рантайм WebView2. Установите Microsoft Edge WebView2 Runtime \
                     или положите папку WebView2 рядом с программой.",
                )
            })?;

            let out_iso = std::path::PathBuf::from(&opts.output_iso);
            let work_root = own_backup_common::config::AppConfig::load_or_default()
                .data_dir
                .join("media");
            let events = self.state.events.clone();
            let extra = (!opts.extra_drivers.trim().is_empty())
                .then(|| std::path::PathBuf::from(&opts.extra_drivers));

            let report = tokio::task::spawn_blocking(move || -> own_backup_common::Result<_> {
                let staging = work_root.join("out");
                let work = work_root.join("work");
                let stages: Vec<MediaStageDto> = BuildStage::plan(!opts.drivers.is_empty())
                    .into_iter()
                    .map(|s| MediaStageDto { id: s.id().into(), title: s.title().into() })
                    .collect();
                let index_of = |id: &str| stages.iter().position(|s| s.id == id).unwrap_or(0);
                let emit = |stage_id: &str, percent: u64, detail: &str, finished: bool| {
                    emit_engine(
                        &events,
                        "media",
                        &MediaProgressDto {
                            stages: stages.clone(),
                            stage_index: index_of(stage_id),
                            percent,
                            detail: detail.to_string(),
                            finished,
                        },
                    );
                };

                let build = MediaBuildRequest {
                    base_wim: base,
                    program_dir,
                    webview2_dir: webview2,
                    out_dir: staging.clone(),
                    work_dir: work.clone(),
                    drivers: opts.drivers.clone(),
                    extra_drivers: extra,
                    runtime_in_image: opts.runtime_in_image,
                };

                // A build that fails still leaves gigabytes behind, so the
                // clean-up runs either way — the first run left 3 GB in
                // ProgramData because it only ran on success.
                let built = match build_media(&build, |r| {
                    emit(r.stage.id(), r.percent, r.detail, false);
                }) {
                    Ok(b) => b,
                    Err(e) => {
                        emit("clean", 0, "после ошибки", false);
                        clean_up(&work, &staging);
                        return Err(e);
                    }
                };

                emit("iso", 0, "сборка образа", false);
                let iso_bytes = match write_media_iso(&built.media_dir, &out_iso) {
                    Ok(n) => n,
                    Err(e) => {
                        emit("clean", 0, "после ошибки", false);
                        clean_up(&work, &staging);
                        return Err(e);
                    }
                };
                emit("iso", 100, "", false);

                emit("clean", 0, "удаление временных файлов", false);
                let freed = clean_up(&work, &staging);
                emit("clean", 100, "", true);

                Ok(MediaBuildReportDto {
                    iso_path: out_iso.display().to_string(),
                    iso_bytes,
                    base_name: built.base_name,
                    base_build: built.base_build,
                    base_language: built.base_language,
                    unpacked: built.unpacked,
                    drivers_added: built.drivers_added,
                    runtime_files_trimmed: built.runtime_files_trimmed,
                    temp_bytes_freed: freed,
                    warnings: built.warnings,
                })
            })
            .await
            .map_err(|e| Status::internal(format!("media build panicked: {e}")))?
            .map_err(|e| Status::internal(e.to_string()))?;

            info!(
                iso = %report.iso_path,
                mb = report.iso_bytes / 1_048_576,
                drivers = report.drivers_added,
                freed_mb = report.temp_bytes_freed / 1_048_576,
                "boot medium built"
            );
            return json_reply(&report);
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = req;
            Err(Status::unimplemented(
                "Загрузочный носитель собирается только на Windows",
            ))
        }
    }

    async fn unmount_image(&self, req: Request<JsonRequest>) -> Result<Response<Empty>, Status> {
        check_auth(&req, &self.service_token)?;
        #[cfg(target_os = "windows")]
        {
            use own_backup_backup::engine_win::unmount_image;
            #[derive(serde::Deserialize)]
            struct In {
                image_path: String,
            }
            let p: In = serde_json::from_str(&req.into_inner().json)
                .map_err(|e| Status::invalid_argument(format!("bad JSON: {e}")))?;
            let image_path = p.image_path.clone();
            let path = std::path::PathBuf::from(&p.image_path);
            tokio::task::spawn_blocking(move || unmount_image(&path))
                .await
                .map_err(|e| Status::internal(format!("unmount task panicked: {e}")))?
                .map_err(|e| Status::internal(e.to_string()))?;

            {
                let mut mounts = self.state.active_mounts.lock().unwrap();
                mounts.retain(|m| m.image_path != image_path);
            }

            Ok(Response::new(Empty {}))
        }
        #[cfg(target_os = "linux")]
        {
            use own_backup_backup::engine_linux::unmount_image;
            #[derive(serde::Deserialize)]
            struct In {
                image_path: String,
            }
            let p: In = serde_json::from_str(&req.into_inner().json)
                .map_err(|e| Status::invalid_argument(format!("bad JSON: {e}")))?;
            let image_path = p.image_path.clone();
            let path = std::path::PathBuf::from(&p.image_path);
            tokio::task::spawn_blocking(move || unmount_image(&path))
                .await
                .map_err(|e| Status::internal(format!("unmount task panicked: {e}")))?
                .map_err(|e| Status::internal(e.to_string()))?;

            {
                let mut mounts = self.state.active_mounts.lock().unwrap();
                mounts.retain(|m| m.image_path != image_path);
            }

            Ok(Response::new(Empty {}))
        }
        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        {
            let _ = req;
            Err(Status::unimplemented(ENGINE_MSG))
        }
    }

    async fn restore_disk(&self, req: Request<JsonRequest>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        #[cfg(target_os = "windows")]
        {
            use std::sync::atomic::Ordering;
            use own_backup_backup::engine_win::{restore_disk, DiskRestoreRequest};
            use own_backup_common::engine_dto::{DiskRestoreProgressDto, DiskRestoreSummaryDto};

            #[derive(serde::Deserialize)]
            struct In {
                manifest_path: String,
                target_disk: u32,
                #[serde(default)]
                force: bool,
            }
            let p: In = serde_json::from_str(&req.into_inner().json)
                .map_err(|e| Status::invalid_argument(format!("bad JSON: {e}")))?;

            // Guard the live system disk (unless forced) where the restore runs.
            if !p.force {
                own_backup_restore::guard_target_disk(p.target_disk)
                    .map_err(|e| Status::failed_precondition(e.to_string()))?;
            }

            self.state.engine_cancel.store(false, Ordering::SeqCst);
            let cancel = self.state.engine_cancel.clone();
            let events = self.state.events.clone();
            let request = DiskRestoreRequest {
                manifest_path: std::path::PathBuf::from(&p.manifest_path),
                target_disk: p.target_disk,
                verify_checksums: true,
            };

            let summary = tokio::task::spawn_blocking(move || {
                let mut last = String::new();
                restore_disk(&request, |pr| {
                    if cancel.swap(false, Ordering::SeqCst) {
                        return Err(own_backup_common::OwnBackupError::Cancelled);
                    }
                    let percent = if pr.blocks_total > 0 {
                        pr.blocks_done * 100 / pr.blocks_total
                    } else {
                        0
                    };
                    let key = format!("{}:{}:{}", pr.phase, pr.partition_index, percent);
                    if key != last {
                        last = key;
                        emit_engine(
                            &events,
                            "restore-disk",
                            &DiskRestoreProgressDto {
                                phase: pr.phase.clone(),
                                partition_index: pr.partition_index as u64,
                                partition_count: pr.partition_count as u64,
                                part_label: pr.part_label.clone(),
                                percent,
                                bytes_written: pr.bytes_written,
                            },
                        );
                    }
                    Ok(())
                })
            })
            .await
            .map_err(|e| Status::internal(format!("disk restore task panicked: {e}")))?
            .map_err(|e| Status::internal(e.to_string()))?;

            json_reply(&DiskRestoreSummaryDto {
                partitions: summary.partitions as u64,
                bytes_written: summary.bytes_written,
                duration_secs: summary.duration.as_secs_f64(),
            })
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = req;
            Err(Status::unimplemented(ENGINE_MSG))
        }
    }

    async fn cancel_engine_op(&self, req: Request<Empty>) -> Result<Response<Empty>, Status> {
        check_auth(&req, &self.service_token)?;
        self.state
            .engine_cancel
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(Response::new(Empty {}))
    }

    // --- Restore plans (the partition designer) --------------------------

    async fn dry_run_restore_plan(
        &self,
        req: Request<JsonRequest>,
    ) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        use own_backup_common::restore_check::{check_plan, resolve_letter_conflicts, CheckContext};
        use own_backup_common::restore_plan::RestorePlan;

        #[derive(serde::Deserialize)]
        struct In {
            plan: RestorePlan,
            #[serde(default)]
            context: CheckContext,
        }
        let mut p: In = serde_json::from_str(&req.into_inner().json)
            .map_err(|e| Status::invalid_argument(format!("bad JSON: {e}")))?;

        // Letters the machine has taken are a service-side fact; a client that
        // sent none gets the real list rather than an empty one.
        let mut ctx = p.context;
        if ctx.taken_letters.is_empty() {
            ctx.taken_letters = taken_drive_letters();
        }
        // Being on the system disk is no longer a veto — a spare partition
        // there is an ordinary target. What is refused is touching the
        // partitions the running system stands on, and only the service can
        // work out which those are.
        ctx.target_is_system_disk =
            ctx.target_is_system_disk || is_system_disk(p.plan.target_disk);
        if ctx.protected_parts.is_empty() {
            ctx.protected_parts = own_backup_restore::protected_partitions(p.plan.target_disk);
        }

        // Resolving letters mutates the plan, so the caller gets the adjusted
        // plan back and applies *that* — otherwise the fix would be lost
        // between the dry run and the real one.
        let letter_changes = resolve_letter_conflicts(&mut p.plan, &ctx.taken_letters);
        correct_disk_size(&mut p.plan);
        let results = check_plan(&p.plan, &ctx);

        // The dry run is where a restore usually stops, and until now it left no
        // trace at all: the user saw a red box in a dialog and the log had
        // nothing to say about it. The plan that was checked and every finding
        // go in, so the refusal can be read afterwards.
        tracing::info!(
            plan = %p.plan.id,
            target_disk = p.plan.target_disk,
            image = %p.plan.image_path.display(),
            ops = p.plan.ops.len(),
            findings = results.len(),
            "dry run: checking a restore plan"
        );
        if let Some(t) = p.plan.table() {
            tracing::info!(
                disk_size = t.disk_size_bytes,
                sector = t.sector_size,
                partitions = t.entries.len(),
                "dry run: proposed table"
            );
            for line in own_backup_backup::table_merge::describe_table(t) {
                tracing::info!("dry run: {line}");
            }
        }
        for r in &results {
            match r.level {
                own_backup_common::restore_check::CheckLevel::Error => {
                    tracing::warn!(code = ?r.code, part = ?r.part_index, "dry run refuses: {}", r.message)
                }
                _ => tracing::info!(code = ?r.code, part = ?r.part_index, "dry run notes: {}", r.message),
            }
        }

        json_reply(&serde_json::json!({
            "plan": p.plan,
            "results": results,
            "letter_changes": letter_changes,
        }))
    }

    async fn apply_restore_plan(
        &self,
        req: Request<JsonRequest>,
    ) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        use own_backup_common::cancel::CancelToken;
        use own_backup_common::restore_plan::RestorePlan;
        use own_backup_common::restore_state::RunStatus;

        // Passphrases arrive with the request, not inside the plan: the plan is
        // persisted to the restore history, and a password written to disk in
        // the clear is not a trade worth making (D19).
        #[derive(serde::Deserialize)]
        struct In {
            plan: RestorePlan,
            #[serde(default)]
            passphrases: std::collections::HashMap<u32, String>,
        }
        let raw = req.into_inner().json;
        // A bare plan is still accepted — nothing needs a passphrase unless it
        // asks to put a container back.
        let In { plan, passphrases } = serde_json::from_str::<In>(&raw)
            .or_else(|_| {
                serde_json::from_str::<RestorePlan>(&raw)
                    .map(|plan| In { plan, passphrases: Default::default() })
            })
            .map_err(|e| Status::invalid_argument(format!("bad plan JSON: {e}")))?;

        // One restore at a time. Two plans writing the same disk would be a
        // race with no possible good outcome.
        if let Some(run) = self.state.restore_run.lock().unwrap().as_ref() {
            if run.status == RunStatus::Running {
                return Err(Status::failed_precondition(format!(
                    "восстановление уже выполняется (план {})",
                    run.plan_id
                )));
            }
        }

        // A fresh token per run: nothing is ever reset, so a cancel cannot leak
        // from the previous run into this one (D18).
        let cancel = CancelToken::new(plan.id);
        *self.state.restore_cancel.lock().unwrap() = Some(cancel.clone());
        let initial = own_backup_core::restore_exec::initial_state(&plan);
        *self.state.restore_run.lock().unwrap() = Some(initial.clone());

        let state = self.state.clone();
        let events = self.state.events.clone();
        tokio::task::spawn_blocking(move || {
            let mut last_emit = std::time::Instant::now();
            let outcome = own_backup_core::restore_exec::execute_plan(&plan, &passphrases, &cancel, |run| {
                *state.restore_run.lock().unwrap() = Some(run.clone());
                // Throttle the wire, never the stored state: the state is what
                // a reconnecting window reads, and it must always be current.
                if run.is_finished() || last_emit.elapsed().as_millis() >= 300 {
                    last_emit = std::time::Instant::now();
                    let _ = events.send(Event {
                        kind: "restore-plan".into(),
                        json: serde_json::to_string(run).unwrap_or_default(),
                    });
                }
            });
            // Persisted as soon as it ends: a service restart must not erase
            // the record of what was done to the disk (D5).
            if let Ok(final_state) = &outcome {
                own_backup_core::restore_exec::append_history(
                    &state.config.data_dir,
                    final_state,
                );
            }
            if let Err(e) = outcome {
                // The plan never started, so there is no run state describing
                // it — say so instead of leaving the last run's state on screen.
                tracing::error!(error = %e, "restore plan could not start");
                let mut slot = state.restore_run.lock().unwrap();
                if let Some(run) = slot.as_mut() {
                    run.status = RunStatus::Failed;
                    run.cancellable = false;
                    run.follow_ups.push(format!("Восстановление не запустилось: {e}"));
                    let _ = events.send(Event {
                        kind: "restore-plan".into(),
                        json: serde_json::to_string(&*run).unwrap_or_default(),
                    });
                }
            }
        });

        json_reply(&initial)
    }

    async fn get_restore_state(&self, req: Request<Empty>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        let run = self.state.restore_run.lock().unwrap().clone();
        // Nothing in memory means the service was restarted, not that nothing
        // ever happened — the last run comes back off disk so a reopened window
        // still shows what the disk went through.
        let run = run.or_else(|| {
            own_backup_core::restore_exec::read_history(&self.state.config.data_dir)
                .into_iter()
                .next()
        });
        // …but only until it has been read. After that the report belongs to
        // the history, not to the restore page.
        let run = run.filter(|r| !r.acknowledged);
        json_reply(&run)
    }

    async fn ack_restore_run(&self, req: Request<IdRequest>) -> Result<Response<Empty>, Status> {
        check_auth(&req, &self.service_token)?;
        let id = req.into_inner().id;
        let plan_id = uuid::Uuid::parse_str(id.trim())
            .map_err(|e| Status::invalid_argument(format!("bad plan id {id:?}: {e}")))?;

        // Both copies, because either one alone would bring the report back:
        // the in-memory run is what a window reads first, the history is what
        // answers after a service restart.
        if let Some(run) = self.state.restore_run.lock().unwrap().as_mut() {
            if run.plan_id == plan_id {
                run.acknowledged = true;
            }
        }
        let found =
            own_backup_core::restore_exec::acknowledge_in_history(&self.state.config.data_dir, plan_id);
        tracing::info!(plan = %plan_id, in_history = found, "restore report acknowledged");
        Ok(Response::new(Empty {}))
    }

    async fn cancel_restore_plan(
        &self,
        req: Request<IdRequest>,
    ) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        let id = req.into_inner().id;
        let plan_id = uuid::Uuid::parse_str(&id)
            .map_err(|e| Status::invalid_argument(format!("bad plan id: {e}")))?;

        // Refuse honestly while a non-cancellable operation runs (D12/D18).
        // Accepting the request and doing nothing is how a user ends up
        // pressing a dead button and believing the restore is stopping.
        let cancellable = self
            .state
            .restore_run
            .lock()
            .unwrap()
            .as_ref()
            .map(|r| r.cancellable)
            .unwrap_or(false);
        if !cancellable {
            return json_reply(&serde_json::json!({
                "cancelled": false,
                "reason": "Сейчас идёт операция, которую нельзя прервать без потери данных.",
            }));
        }

        let matched = self
            .state
            .restore_cancel
            .lock()
            .unwrap()
            .as_ref()
            .map(|t| t.cancel_run(plan_id))
            .unwrap_or(false);

        json_reply(&serde_json::json!({
            "cancelled": matched,
            "reason": if matched { "" } else { "Этот запуск уже завершён." },
        }))
    }

    // --- Arsenal Image Mounter driver management (Windows) ---

    async fn aim_driver_status(&self, req: Request<Empty>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        // The GUI reads `platform` to pick the right card: Windows shows the AIM
        // driver installer; Linux shows the (no-driver) NBD/NTFS status.
        #[cfg(target_os = "windows")]
        {
            let st = own_backup_backup::aim_install::status();
            json_reply(&serde_json::json!({
                "platform": "windows",
                "installed": st.installed,
                "version": st.version,
            }))
        }
        #[cfg(target_os = "linux")]
        {
            json_reply(&own_backup_backup::engine_linux::mount_support_status())
        }
        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        {
            Err(Status::unimplemented(ENGINE_MSG))
        }
    }

    async fn install_aim_driver(&self, req: Request<Empty>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        #[cfg(target_os = "windows")]
        {
            // Download + SetupAPI install can take a few seconds; run it off the
            // async runtime.
            tokio::task::spawn_blocking(own_backup_backup::aim_install::install)
                .await
                .map_err(|e| Status::internal(format!("install task panicked: {e}")))?
                .map_err(|e| Status::internal(e.to_string()))?;
            let st = own_backup_backup::aim_install::status();
            json_reply(&serde_json::json!({
                "platform": "windows",
                "installed": st.installed,
                "version": st.version,
            }))
        }
        #[cfg(target_os = "linux")]
        {
            // No driver to install — just load the in-tree NBD module (root).
            tokio::task::spawn_blocking(own_backup_backup::engine_linux::ensure_nbd_loaded)
                .await
                .map_err(|e| Status::internal(format!("modprobe task panicked: {e}")))?
                .map_err(|e| Status::internal(e.to_string()))?;
            json_reply(&own_backup_backup::engine_linux::mount_support_status())
        }
        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        {
            Err(Status::unimplemented(ENGINE_MSG))
        }
    }

    async fn uninstall_aim_driver(&self, req: Request<Empty>) -> Result<Response<Empty>, Status> {
        check_auth(&req, &self.service_token)?;
        #[cfg(target_os = "windows")]
        {
            tokio::task::spawn_blocking(own_backup_backup::aim_install::uninstall)
                .await
                .map_err(|e| Status::internal(format!("uninstall task panicked: {e}")))?
                .map_err(|e| Status::internal(e.to_string()))?;
            Ok(Response::new(Empty {}))
        }
        #[cfg(not(target_os = "windows"))]
        {
            Err(Status::unimplemented(
                "The Arsenal Image Mounter driver is a Windows component.",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_service(dir: &std::path::Path) -> BackupServiceImpl {
        let mut config = own_backup_common::config::AppConfig::default();
        config.data_dir = dir.to_path_buf();
        let store = own_backup_core::JobStore::load(dir);
        let queue = own_backup_core::RunQueue::new();
        let (job_tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (events, _r) = broadcast::channel(16);
        let passwords =
            std::sync::Arc::new(own_backup_core::PasswordStore::new(dir.join("meta.db")));
        let state = AppState::new(config, store, queue, job_tx, cancel, events, passwords);
        BackupServiceImpl::new(state, String::new())
    }

    fn job(name: &str, source: &str, dest: &str) -> String {
        serde_json::to_string(&own_backup_core::Job {
            name: name.into(),
            source: source.into(),
            dest_folder: dest.into(),
            ..Default::default()
        })
        .unwrap()
    }

    #[tokio::test]
    async fn jobs_roundtrip_over_grpc() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());

        // Empty to start.
        let json = svc.list_jobs(Request::new(Empty {})).await.unwrap().into_inner().json;
        assert_eq!(json, "[]");

        // Save a job, get a generated id back.
        let saved = svc
            .save_job(Request::new(JsonRequest { json: job("T", "E:", r"D:\bk") }))
            .await
            .unwrap()
            .into_inner()
            .json;
        let saved: own_backup_core::Job = serde_json::from_str(&saved).unwrap();
        assert!(!saved.id.is_empty());

        // It is listed and fetchable.
        let listed: Vec<own_backup_core::Job> = serde_json::from_str(
            &svc.list_jobs(Request::new(Empty {})).await.unwrap().into_inner().json,
        )
        .unwrap();
        assert_eq!(listed.len(), 1);
        let got = svc
            .get_job(Request::new(IdRequest { id: saved.id.clone() }))
            .await
            .unwrap()
            .into_inner()
            .json;
        assert!(got.contains(&saved.id));

        // dest-on-source is rejected at the boundary.
        let err = svc
            .save_job(Request::new(JsonRequest { json: job("B", "E:", r"E:\bk") }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);

        // Delete it.
        svc.delete_job(Request::new(DeleteJobRequest { id: saved.id, delete_files: false }))
            .await
            .unwrap();
        let json = svc.list_jobs(Request::new(Empty {})).await.unwrap().into_inner().json;
        assert_eq!(json, "[]");
    }

    #[tokio::test]
    async fn run_missing_job_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        let err = svc
            .run_job(Request::new(IdRequest { id: "nope".into() }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn status_and_template_and_exclusions() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());

        let status = svc.get_service_status(Request::new(Empty {})).await.unwrap().into_inner().json;
        assert!(status.contains("version"));

        let tmpl: own_backup_core::Job = serde_json::from_str(
            &svc.new_job_template(Request::new(Empty {})).await.unwrap().into_inner().json,
        )
        .unwrap();
        assert!(!tmpl.name.is_empty());

        let excl: serde_json::Value = serde_json::from_str(
            &svc.default_exclusions(Request::new(Empty {})).await.unwrap().into_inner().json,
        )
        .unwrap();
        assert!(excl["windows"].as_array().unwrap().iter().any(|p| p.as_str().unwrap().contains("Temp")));
        assert!(excl["linux"].as_array().unwrap().iter().any(|p| p.as_str().unwrap().contains("tmp")));
    }
}

/// A boot medium cannot build a boot medium, and the reasons are not one but
/// several: `X:` is a RAM disk with no room to unpack a multi-gigabyte image
/// into, the driver store the medium would copy from does not exist there (which
/// is why the driver scan comes back empty), and WinPE carries a reduced DISM
/// that cannot service an image at all.
///
/// Refused here rather than only in the interface, because the interface is not
/// the only way in — and because the failure it produced was a DISM error code
/// halfway through the build, which explains nothing.
fn media_build_unavailable() -> Result<(), Status> {
    if own_backup_common::media_mode::is_media_mode() {
        return Err(Status::failed_precondition(
            "Собрать загрузочный носитель можно только из установленной Windows. \
             Здесь, на самом носителе, нет ни хранилища драйверов, ни места для \
             распаковки образа: X: — это диск в оперативной памяти.",
        ));
    }
    Ok(())
}

/// Where the WebView2 runtime lives on this machine.
///
/// The installed Evergreen runtime keeps its version in the folder name, so the
/// newest is taken. A `WebView2` folder beside the program wins over all of it —
/// that is how a machine without Edge is served, and how a specific runtime can
/// be forced.
#[cfg(target_os = "windows")]
fn find_webview2_runtime(program_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let beside = program_dir.join("WebView2");
    if beside.join("msedgewebview2.exe").exists() {
        return Some(beside);
    }
    let mut best: Option<(String, std::path::PathBuf)> = None;
    for base in ["ProgramFiles(x86)", "ProgramFiles"] {
        let Ok(root) = std::env::var(base) else { continue };
        let apps = std::path::PathBuf::from(root).join(r"Microsoft\EdgeWebView\Application");
        let Ok(rd) = std::fs::read_dir(&apps) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.starts_with(|c: char| c.is_ascii_digit()) {
                continue;
            }
            if !e.path().join("msedgewebview2.exe").exists() {
                continue;
            }
            if best.as_ref().map(|(n, _)| name > *n).unwrap_or(true) {
                best = Some((name, e.path()));
            }
        }
    }
    best.map(|(_, p)| p)
}

/// Turn a finished medium folder into a bootable ISO.
///
/// The El Torito image carries the loader and the BCD it looks for beside
/// itself; everything else is read from the ISO9660 tree, which is why the whole
/// medium goes in as files as well.
#[cfg(target_os = "windows")]
fn write_media_iso(
    media_dir: &std::path::Path,
    out_iso: &std::path::Path,
) -> own_backup_common::Result<u64> {
    use own_backup_backup::fat_writer::{build_fat16, FatFile};
    use own_backup_backup::iso9660::{write_iso, IsoBoot, IsoFile};

    fn walk(dir: &std::path::Path, base: &std::path::Path, out: &mut Vec<IsoFile>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, base, out);
            } else if let Ok(rel) = p.strip_prefix(base) {
                out.push(IsoFile {
                    path: rel.to_string_lossy().replace('\\', "/"),
                    source: p.clone(),
                });
            }
        }
    }
    let mut files = Vec::new();
    walk(media_dir, media_dir, &mut files);

    let mut boot_files = Vec::new();
    for rel in ["EFI/BOOT/BOOTX64.EFI", "EFI/Microsoft/Boot/BCD", "boot/boot.sdi"] {
        let src = media_dir.join(rel.replace('/', "\\"));
        if src.exists() {
            boot_files.push(FatFile {
                path: rel.replace('/', "\\"),
                data: std::fs::read(&src)?,
            });
        }
    }
    let efisys = media_dir.join("boot").join("efisys.bin");
    std::fs::write(&efisys, build_fat16(&boot_files, "OWNBOOT")?)?;
    files.push(IsoFile { path: "boot/efisys.bin".into(), source: efisys.clone() });

    if let Some(parent) = out_iso.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let len = write_iso(&files, "OWNBACKUP", Some(IsoBoot { image_path: "boot/efisys.bin".into() }), out_iso)?;
    let _ = std::fs::remove_file(&efisys);
    Ok(len)
}
