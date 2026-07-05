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

    async fn delete_job(&self, req: Request<IdRequest>) -> Result<Response<Empty>, Status> {
        check_auth(&req, &self.service_token)?;
        self.state.store.delete(&req.into_inner().id);
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
        json_reply(&own_backup_core::default_exclusions())
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
    ) -> Result<Response<Empty>, Status> {
        check_auth(&req, &self.service_token)?;
        #[derive(serde::Deserialize)]
        struct Payload {
            volume: String,
            password: String,
            #[serde(default)]
            remember: bool,
        }
        let p: Payload = serde_json::from_str(&req.into_inner().json)
            .map_err(|e| Status::invalid_argument(format!("bad JSON: {e}")))?;
        // Key by the same token the backup path resolves against.
        let token = own_backup_core::source_token(&p.volume);
        self.state
            .passwords
            .remember(&token, p.password, p.remember)
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(Empty {}))
    }

    // --- Inventory --------------------------------------------------------

    async fn list_disks(&self, req: Request<Empty>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        info!("gRPC: ListDisks");
        use own_backup_inventory::DiskInventory;

        #[cfg(target_os = "windows")]
        let disks = own_backup_inventory::WmiInventory::new()
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
        let status = serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "uptime_secs": self.state.uptime_seconds(),
            "queue": self.state.queue.snapshot(),
        });
        Ok(Response::new(JsonReply { json: status.to_string() }))
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
        svc.delete_job(Request::new(IdRequest { id: saved.id }))
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

        let excl: Vec<String> = serde_json::from_str(
            &svc.default_exclusions(Request::new(Empty {})).await.unwrap().into_inner().json,
        )
        .unwrap();
        assert!(excl.iter().any(|p| p.contains("Temp")));
    }
}
