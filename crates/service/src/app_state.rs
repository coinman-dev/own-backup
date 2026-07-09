use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::broadcast;
use tokio::sync::mpsc::UnboundedSender;

use own_backup_common::config::AppConfig;
use own_backup_core::{JobStore, PasswordStore, RunQueue};

use crate::grpc_server::proto;

/// Shared state for the gRPC handlers, the queue worker and (later) the
/// scheduler. Owns the orchestrator: the job store, the serial run queue, the
/// enqueue channel, the cancel flag and the event fan-out.
pub struct AppState {
    #[allow(dead_code)] // data_dir/paths read as the service grows (scheduler, logs)
    pub config: AppConfig,
    /// Persisted jobs + activity.
    pub store: Arc<JobStore>,
    /// Serial run queue shared with the worker.
    pub queue: RunQueue,
    /// Enqueue a job id for the background worker.
    pub job_tx: UnboundedSender<String>,
    /// Set true to cancel the currently running backup.
    pub cancel: Arc<AtomicBool>,
    /// Set true to cancel the currently running direct engine op (restore /
    /// verify / disk restore). Separate from `cancel` so cancelling a restore
    /// never touches a running backup and vice-versa.
    pub engine_cancel: Arc<AtomicBool>,
    /// Fan-out of orchestration events to `StreamEvents` subscribers.
    pub events: broadcast::Sender<proto::Event>,
    /// Passwords for encrypted volumes (in-memory cache + OS-protected store).
    pub passwords: Arc<PasswordStore>,
    /// In-memory active mounts tracked by the service.
    pub active_mounts: std::sync::Mutex<Vec<own_backup_common::engine_dto::ActiveMountDto>>,
    pub start_time: Instant,
}

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: AppConfig,
        store: Arc<JobStore>,
        queue: RunQueue,
        job_tx: UnboundedSender<String>,
        cancel: Arc<AtomicBool>,
        events: broadcast::Sender<proto::Event>,
        passwords: Arc<PasswordStore>,
    ) -> Arc<Self> {
        Arc::new(Self {
            config,
            store,
            queue,
            job_tx,
            cancel,
            engine_cancel: Arc::new(AtomicBool::new(false)),
            events,
            passwords,
            active_mounts: std::sync::Mutex::new(Vec::new()),
            start_time: Instant::now(),
        })
    }

    pub fn uptime_seconds(&self) -> u64 {
        self.start_time.elapsed().as_secs()
    }
}
