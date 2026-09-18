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
    /// Connect to the service. `token` is sent as a bearer token on every call
    /// (empty = dev / no-auth).
    ///
    /// On Windows the production transport is a DACL-restricted named pipe (no
    /// listening TCP port); we try it first and fall back to loopback TCP at
    /// `endpoint` for foreground/dev service runs. On other platforms `endpoint`
    /// (e.g. `http://127.0.0.1:50051`) is used directly.
    pub async fn connect(endpoint: impl Into<String>, token: impl Into<String>) -> Result<Self> {
        let endpoint = endpoint.into();
        let token = token.into();
        #[cfg(windows)]
        {
            match Self::connect_pipe(token.clone()).await {
                Ok(c) => return Ok(c),
                Err(e) => {
                    tracing::debug!(error = %e, "named pipe unavailable, falling back to TCP")
                }
            }
        }
        #[cfg(unix)]
        {
            // Production transport: the service's UDS. Fall back to loopback TCP
            // for a foreground/dev service run (which binds TCP, not the socket).
            match Self::connect_uds(token.clone()).await {
                Ok(c) => return Ok(c),
                Err(e) => {
                    tracing::debug!(error = %e, "UDS unavailable, falling back to TCP")
                }
            }
        }
        Self::connect_tcp(endpoint, token).await
    }

    /// Connect over the service's Unix Domain Socket (`/run/own-backup/service.sock`).
    /// Fails fast if the socket is absent (service down or running foreground/TCP).
    #[cfg(unix)]
    async fn connect_uds(token: String) -> Result<Self> {
        use hyper_util::rt::TokioIo;
        use tokio::net::UnixStream;
        use tonic::transport::{Endpoint, Uri};
        use tower::service_fn;

        let socket_path = own_backup_common::unix_socket_path();
        if !socket_path.exists() {
            return Err("service socket not present".into());
        }

        // The URI authority is a placeholder; the connector dials the socket.
        let channel = Endpoint::try_from("http://ownbackup.uds")
            .map_err(|e| format!("bad uds endpoint: {e}"))?
            .connect_timeout(std::time::Duration::from_millis(800))
            .connect_with_connector(service_fn(move |_: Uri| {
                let socket_path = socket_path.clone();
                async move { Ok::<_, std::io::Error>(TokioIo::new(UnixStream::connect(socket_path).await?)) }
            }))
            .await
            .map_err(|e| format!("cannot reach service (uds): {e}"))?;
        Ok(Self {
            inner: BackupServiceClient::new(channel),
            token,
        })
    }

    /// Connect over loopback TCP (dev/foreground; the only transport on non-Windows).
    async fn connect_tcp(endpoint: String, token: String) -> Result<Self> {
        let channel = Channel::from_shared(endpoint)
            .map_err(|e| format!("bad endpoint: {e}"))?
            // Fail fast when nothing is listening (service down) so the GUI's
            // health check and retries stay snappy instead of hanging ~seconds.
            .connect_timeout(std::time::Duration::from_millis(800))
            .connect()
            .await
            .map_err(|e| format!("cannot reach service: {e}"))?;
        Ok(Self {
            inner: BackupServiceClient::new(channel),
            token,
        })
    }

    /// Connect over the Windows named pipe (`\\.\pipe\OwnBackup`). Fails fast if
    /// no pipe exists (service down or running foreground/TCP).
    #[cfg(windows)]
    async fn connect_pipe(token: String) -> Result<Self> {
        use hyper_util::rt::TokioIo;
        use tokio::net::windows::named_pipe::ClientOptions;
        use tonic::transport::{Endpoint, Uri};
        use tower::service_fn;

        const ERROR_PIPE_BUSY: i32 = 231;
        let pipe_path = own_backup_common::windows_pipe_path();

        // The URI authority is a placeholder; the connector below dials the pipe.
        let channel = Endpoint::try_from("http://ownbackup.pipe")
            .map_err(|e| format!("bad pipe endpoint: {e}"))?
            .connect_timeout(std::time::Duration::from_millis(800))
            .connect_with_connector(service_fn(move |_: Uri| {
                let pipe_path = pipe_path.clone();
                async move {
                    // Retry briefly if every instance is momentarily busy (a burst
                    // of connects before the service opened the next instance).
                    for _ in 0..20u32 {
                        match ClientOptions::new().open(&pipe_path) {
                            Ok(client) => return Ok(TokioIo::new(client)),
                            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                            }
                            Err(e) => return Err(e),
                        }
                    }
                    Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "named pipe busy (all instances)",
                    ))
                }
            }))
            .await
            .map_err(|e| format!("cannot reach service (pipe): {e}"))?;
        Ok(Self {
            inner: BackupServiceClient::new(channel),
            token,
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

    pub async fn delete_job(&mut self, id: &str, delete_files: bool) -> Result<()> {
        self.inner
            .delete_job(self.request(proto::DeleteJobRequest {
                id: id.into(),
                delete_files,
            }))
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

    /// Default exclusion masks per OS family: `{ "windows": [...], "linux": [...] }`.
    pub async fn default_exclusions(&mut self) -> Result<serde_json::Value> {
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

    // --- Encrypted volumes ------------------------------------------------

    /// Hand a password for an encrypted (LUKS/BitLocker) source `volume` to the
    /// service, so a backup running there can unlock it. Cached for the session;
    /// `remember` also persists it OS-protected for scheduled runs.
    pub async fn provide_volume_password(
        &mut self,
        volume: &str,
        password: &str,
        remember: bool,
    ) -> Result<own_backup_common::engine_dto::VolumeUnlockDto> {
        let json = serde_json::json!({
            "volume": volume,
            "password": password,
            "remember": remember,
        })
        .to_string();
        let reply = self
            .inner
            .provide_volume_password(self.request(proto::JsonRequest { json }))
            .await
            .map_err(status)?
            .into_inner();
        parse(&reply.json)
    }

    // --- Direct engine ops (run in the privileged service) ---------------

    /// Restore a volume image onto `target` (DESTRUCTIVE). Blocks until done;
    /// progress arrives on [`stream_events`](Self::stream_events) as `"restore"`.
    pub async fn restore_volume(
        &mut self,
        image_path: &str,
        target: &str,
    ) -> Result<own_backup_common::engine_dto::RestoreSummaryDto> {
        let json = serde_json::json!({ "image_path": image_path, "target": target }).to_string();
        let reply = self
            .inner
            .restore_volume(self.request(proto::JsonRequest { json }))
            .await
            .map_err(status)?
            .into_inner();
        parse(&reply.json)
    }

    /// Verify an image against its sidecar checksums. Progress arrives as `"verify"`.
    pub async fn verify_image(
        &mut self,
        image_path: &str,
    ) -> Result<own_backup_common::engine_dto::VerifySummaryDto> {
        let json = serde_json::json!({ "image_path": image_path }).to_string();
        let reply = self
            .inner
            .verify_image(self.request(proto::JsonRequest { json }))
            .await
            .map_err(status)?
            .into_inner();
        parse(&reply.json)
    }

    /// Mount an image; returns where it appeared.
    pub async fn mount_image(&mut self, image_path: &str, read_write: bool) -> Result<String> {
        let json = serde_json::json!({ "image_path": image_path, "read_write": read_write }).to_string();
        let reply = self
            .inner
            .mount_image(self.request(proto::JsonRequest { json }))
            .await
            .map_err(status)?
            .into_inner();
        let m: own_backup_common::engine_dto::MountResultDto = parse(&reply.json)?;
        Ok(m.mount_point)
    }

    /// Base images this machine can build a medium on, best first, each already
    /// read and vouched for by the service.
    pub async fn find_base_images(
        &mut self,
    ) -> Result<Vec<own_backup_common::engine_dto::BaseImageDto>> {
        let reply = self
            .inner
            .find_base_images(self.request(proto::Empty {}))
            .await
            .map_err(status)?
            .into_inner();
        parse(&reply.json)
    }

    /// Driver packages published on this machine, largest first — with sizes,
    /// because that is what the choice is actually about.
    pub async fn list_drivers(
        &mut self,
    ) -> Result<Vec<own_backup_common::engine_dto::DriverPackageDto>> {
        let reply = self
            .inner
            .list_drivers(self.request(proto::Empty {}))
            .await
            .map_err(status)?
            .into_inner();
        parse(&reply.json)
    }

    /// Build the boot medium and write it as an ISO. Progress arrives on the
    /// event stream as kind `media`; this returns when the file is on disk.
    pub async fn build_media(
        &mut self,
        options: &own_backup_common::engine_dto::MediaBuildOptionsDto,
    ) -> Result<own_backup_common::engine_dto::MediaBuildReportDto> {
        let json = serde_json::to_string(options).unwrap_or_default();
        let reply = self
            .inner
            .build_media(self.request(proto::JsonRequest { json }))
            .await
            .map_err(status)?
            .into_inner();
        parse(&reply.json)
    }

    pub async fn unmount_image(&mut self, image_path: &str) -> Result<()> {
        let json = serde_json::json!({ "image_path": image_path }).to_string();
        self.inner
            .unmount_image(self.request(proto::JsonRequest { json }))
            .await
            .map_err(status)?;
        Ok(())
    }

    /// What mounting an image needs on the service's machine, and whether it
    /// is in place (JSON: { platform, iscsi | linux }).
    /// Everything about how this machine boots, in one answer.
    pub async fn boot_status(&mut self) -> Result<serde_json::Value> {
        let reply = self
            .inner
            .boot_status(self.request(proto::Empty {}))
            .await
            .map_err(status)?
            .into_inner();
        parse(&reply.json)
    }

    /// Change one thing about it. The service takes the safety copy.
    pub async fn boot_action(&mut self, action: &serde_json::Value) -> Result<serde_json::Value> {
        let reply = self
            .inner
            .boot_action(self.request(proto::JsonRequest { json: action.to_string() }))
            .await
            .map_err(status)?
            .into_inner();
        parse(&reply.json)
    }

    pub async fn mount_backend_status(&mut self) -> Result<serde_json::Value> {
        let reply = self
            .inner
            .mount_backend_status(self.request(proto::Empty {}))
            .await
            .map_err(status)?
            .into_inner();
        parse(&reply.json)
    }

    /// Start whatever mounting needs here (the service does the privileged work).
    pub async fn prepare_mount_backend(&mut self) -> Result<serde_json::Value> {
        let reply = self
            .inner
            .prepare_mount_backend(self.request(proto::Empty {}))
            .await
            .map_err(status)?
            .into_inner();
        parse(&reply.json)
    }

    /// Restore a whole disk from an `.obdm` manifest onto `target_disk`
    /// (DESTRUCTIVE). Progress arrives as `"restore-disk"`.
    pub async fn restore_disk(
        &mut self,
        manifest_path: &str,
        target_disk: u32,
        force: bool,
    ) -> Result<own_backup_common::engine_dto::DiskRestoreSummaryDto> {
        let json = serde_json::json!({
            "manifest_path": manifest_path,
            "target_disk": target_disk,
            "force": force,
        })
        .to_string();
        let reply = self
            .inner
            .restore_disk(self.request(proto::JsonRequest { json }))
            .await
            .map_err(status)?
            .into_inner();
        parse(&reply.json)
    }

    // --- Restore plans (the partition designer) ---------------------------

    /// Check a plan without writing anything (D5).
    ///
    /// The reply carries the plan *back*, because the dry run resolves drive
    /// letter conflicts by editing it (D11). Applying the plan that was sent
    /// rather than the one that came back would throw that fix away.
    pub async fn dry_run_restore_plan(
        &mut self,
        plan: &own_backup_common::restore_plan::RestorePlan,
        context: &own_backup_common::restore_check::CheckContext,
    ) -> Result<serde_json::Value> {
        let json = serde_json::json!({ "plan": plan, "context": context }).to_string();
        let reply = self
            .inner
            .dry_run_restore_plan(self.request(proto::JsonRequest { json }))
            .await
            .map_err(status)?
            .into_inner();
        parse(&reply.json)
    }

    /// Execute a plan. DESTRUCTIVE. Returns immediately with the run's initial
    /// state; progress arrives on `StreamEvents` as `restore-plan`, and the
    /// current state is always readable via [`Self::get_restore_state`].
    ///
    /// `passphrases` maps a **source** partition index to the pass phrase of the
    /// container being put back (D19) — the container is identified by where it
    /// came from, not by where it is going. They travel with the request rather than
    /// inside the plan, because the plan is persisted to the restore history
    /// and a password on disk in the clear is not a trade worth making.
    pub async fn apply_restore_plan(
        &mut self,
        plan: &own_backup_common::restore_plan::RestorePlan,
        passphrases: &std::collections::HashMap<u32, String>,
    ) -> Result<own_backup_common::restore_state::RestoreRunState> {
        let json = serde_json::to_string(&serde_json::json!({
            "plan": plan,
            "passphrases": passphrases,
        }))
        .map_err(|e| format!("serialize plan failed: {e}"))?;
        let reply = self
            .inner
            .apply_restore_plan(self.request(proto::JsonRequest { json }))
            .await
            .map_err(status)?
            .into_inner();
        parse(&reply.json)
    }

    /// State of the restore in progress, or the last one that finished.
    /// `None` when no plan has been applied since the service started.
    pub async fn get_restore_state(
        &mut self,
    ) -> Result<Option<own_backup_common::restore_state::RestoreRunState>> {
        let reply = self
            .inner
            .get_restore_state(self.request(proto::Empty {}))
            .await
            .map_err(status)?
            .into_inner();
        parse(&reply.json)
    }

    /// Ask the named run to stop. The reply says whether it was accepted — a
    /// non-cancellable operation refuses outright rather than pretending.
    pub async fn cancel_restore_plan(&mut self, plan_id: &str) -> Result<serde_json::Value> {
        let reply = self
            .inner
            .cancel_restore_plan(self.request(proto::IdRequest { id: plan_id.into() }))
            .await
            .map_err(status)?
            .into_inner();
        parse(&reply.json)
    }

    /// Say the finished run's report has been read, so it stops coming back.
    pub async fn ack_restore_run(&mut self, plan_id: &str) -> Result<()> {
        self.inner
            .ack_restore_run(self.request(proto::IdRequest { id: plan_id.into() }))
            .await
            .map_err(status)?;
        Ok(())
    }

    /// Signal-cancel the running restore/verify/disk-restore.
    pub async fn cancel_engine_op(&mut self) -> Result<()> {
        self.inner
            .cancel_engine_op(self.request(proto::Empty {}))
            .await
            .map_err(status)?;
        Ok(())
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
