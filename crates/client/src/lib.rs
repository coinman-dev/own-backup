//! Thin gRPC client for the backup service, shared by the CLI and the GUI.
//!
//! The service speaks JSON envelopes over gRPC (see `proto/ownbackup.proto`);
//! this crate hides that, exposing typed methods that return the shared
//! [`own_backup_core`] structs. Both ends being Rust, the JSON is just a
//! transport for those structs.

use tokio_stream::{Stream, StreamExt};
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tonic::Request;

use own_backup_core::{Job, JobProgress, JobRun, QueueSnapshot};

pub mod proto {
    tonic::include_proto!("ownbackup");
}
use proto::backup_service_client::BackupServiceClient;

/// Client errors are surfaced as human-readable strings — callers (CLI, GUI
/// commands) just display them.
pub type Result<T> = std::result::Result<T, String>;

/// A decoded orchestration event from [`BackupClient::stream_events`].
#[derive(Debug, Clone)]
pub enum ClientEvent {
    Progress(JobProgress),
    Queue(QueueSnapshot),
    Finished(JobRun),
    /// Unrecognized kind or undecodable payload — kept for forward-compat.
    Unknown { kind: String, json: String },
}

/// gRPC client wrapper. Cheap to clone (shares the underlying HTTP/2 channel).
#[derive(Clone)]
pub struct BackupClient {
    inner: BackupServiceClient<Channel>,
    token: String,
}

impl BackupClient {
    /// Connect to `endpoint` (e.g. `http://127.0.0.1:50051`). `token` is sent as
    /// a bearer token on every call (empty = dev / no-auth).
    pub async fn connect(endpoint: impl Into<String>, token: impl Into<String>) -> Result<Self> {
        let channel = Channel::from_shared(endpoint.into())
            .map_err(|e| format!("bad endpoint: {e}"))?
            .connect()
            .await
            .map_err(|e| format!("cannot reach service: {e}"))?;
        Ok(Self {
            inner: BackupServiceClient::new(channel),
            token: token.into(),
        })
    }

    fn request<T>(&self, msg: T) -> Request<T> {
        let mut req = Request::new(msg);
        if !self.token.is_empty() {
            if let Ok(value) = MetadataValue::try_from(format!("Bearer {}", self.token)) {
                req.metadata_mut().insert("authorization", value);
            }
        }
        req
    }

    // --- Jobs -------------------------------------------------------------

    pub async fn list_jobs(&mut self) -> Result<Vec<Job>> {
        let reply = self
            .inner
            .list_jobs(self.request(proto::Empty {}))
            .await
            .map_err(status)?
            .into_inner();
        parse(&reply.json)
    }

    pub async fn get_job(&mut self, id: &str) -> Result<Option<Job>> {
        let reply = self
            .inner
            .get_job(self.request(proto::IdRequest { id: id.into() }))
            .await
            .map_err(status)?
            .into_inner();
        parse(&reply.json)
    }

    pub async fn save_job(&mut self, job: &Job) -> Result<Job> {
        let json = serde_json::to_string(job).map_err(json_err)?;
        let reply = self
            .inner
            .save_job(self.request(proto::JsonRequest { json }))
            .await
            .map_err(status)?
            .into_inner();
        parse(&reply.json)
    }

    pub async fn delete_job(&mut self, id: &str) -> Result<()> {
        self.inner
            .delete_job(self.request(proto::IdRequest { id: id.into() }))
            .await
            .map_err(status)?;
        Ok(())
    }

    pub async fn new_job_template(&mut self) -> Result<Job> {
        let reply = self
            .inner
            .new_job_template(self.request(proto::Empty {}))
            .await
            .map_err(status)?
            .into_inner();
        parse(&reply.json)
    }

    pub async fn default_exclusions(&mut self) -> Result<Vec<String>> {
        let reply = self
            .inner
            .default_exclusions(self.request(proto::Empty {}))
            .await
            .map_err(status)?
            .into_inner();
        parse(&reply.json)
    }

    // --- Queue / runs -----------------------------------------------------

    pub async fn run_job(&mut self, id: &str) -> Result<()> {
        self.inner
            .run_job(self.request(proto::IdRequest { id: id.into() }))
            .await
            .map_err(status)?;
        Ok(())
    }

    pub async fn cancel_queued(&mut self, id: &str) -> Result<()> {
        self.inner
            .cancel_queued(self.request(proto::IdRequest { id: id.into() }))
            .await
            .map_err(status)?;
        Ok(())
    }

    pub async fn cancel_running(&mut self) -> Result<()> {
        self.inner
            .cancel_running(self.request(proto::Empty {}))
            .await
            .map_err(status)?;
        Ok(())
    }

    pub async fn get_queue(&mut self) -> Result<QueueSnapshot> {
        let reply = self
            .inner
            .get_queue(self.request(proto::Empty {}))
            .await
            .map_err(status)?
            .into_inner();
        parse(&reply.json)
    }

    pub async fn list_activity(&mut self) -> Result<Vec<JobRun>> {
        let reply = self
            .inner
            .list_activity(self.request(proto::Empty {}))
            .await
            .map_err(status)?
            .into_inner();
        parse(&reply.json)
    }

    /// Subscribe to orchestration events (progress / queue / finished).
    pub async fn stream_events(&mut self) -> Result<impl Stream<Item = ClientEvent>> {
        let stream = self
            .inner
            .stream_events(self.request(proto::Empty {}))
            .await
            .map_err(status)?
            .into_inner();
        Ok(stream.filter_map(|item| item.ok().map(decode_event)))
    }

    // --- Misc -------------------------------------------------------------

    /// Disk inventory as raw JSON (a `[DiskInfo]` array) — callers that need
    /// typed access decode against `own_backup_common::disk::DiskInfo`.
    pub async fn list_disks_json(&mut self) -> Result<String> {
        let reply = self
            .inner
            .list_disks(self.request(proto::Empty {}))
            .await
            .map_err(status)?
            .into_inner();
        Ok(reply.json)
    }

    pub async fn service_status_json(&mut self) -> Result<String> {
        let reply = self
            .inner
            .get_service_status(self.request(proto::Empty {}))
            .await
            .map_err(status)?
            .into_inner();
        Ok(reply.json)
    }
}

fn decode_event(ev: proto::Event) -> ClientEvent {
    match ev.kind.as_str() {
        "progress" => serde_json::from_str(&ev.json)
            .map(ClientEvent::Progress)
            .unwrap_or(ClientEvent::Unknown { kind: ev.kind, json: ev.json }),
        "queue" => serde_json::from_str(&ev.json)
            .map(ClientEvent::Queue)
            .unwrap_or(ClientEvent::Unknown { kind: ev.kind, json: ev.json }),
        "finished" => serde_json::from_str(&ev.json)
            .map(ClientEvent::Finished)
            .unwrap_or(ClientEvent::Unknown { kind: ev.kind, json: ev.json }),
        _ => ClientEvent::Unknown { kind: ev.kind, json: ev.json },
    }
}

fn parse<T: for<'de> serde::Deserialize<'de>>(json: &str) -> Result<T> {
    serde_json::from_str(json).map_err(json_err)
}

fn status(s: tonic::Status) -> String {
    s.message().to_string()
}

fn json_err(e: serde_json::Error) -> String {
    format!("bad JSON from service: {e}")
}
