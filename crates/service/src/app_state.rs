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
    /// The machine's single slot for destructive work: restores, direct engine
    /// operations, backups, read-write mounts and deleting a job's copies all
    /// pass through it, so two of them can never overlap (audit AUD-02).
    ///
    /// It also owns the CANCEL TOKEN of whatever is running. The flag it
    /// replaces was one `AtomicBool` shared by every direct operation and
    /// **reset at the start of each one**, so a second run silently disarmed
    /// the first one's cancel; the token here is created per run, never reset,
    /// and addressed by id.
    pub busy: Arc<own_backup_core::busy::Busy>,
    /// Fan-out of orchestration events to `StreamEvents` subscribers.
    pub events: broadcast::Sender<proto::Event>,
    /// Passwords for encrypted volumes (in-memory cache + OS-protected store).
    pub passwords: Arc<PasswordStore>,
    /// The disk cleanup: the window's, the scheduled one, the one before a
    /// copy — one at a time ([`crate::auto_cleanup`]).
    pub auto: Arc<crate::auto_cleanup::AutoCleanup>,
    /// Archive-encryption authorizations prepared but not yet confirmed
    /// (plan §7.5). Deliberately in memory only: nothing about a pending one
    /// should survive a service restart, least of all the private key in it.
    pub archive_setup: Arc<own_backup_core::archive_setup::ArchiveSetup>,
    /// In-memory active mounts tracked by the service.
    pub active_mounts: std::sync::Mutex<Vec<own_backup_common::engine_dto::ActiveMountDto>>,
    /// The restore plan being executed, or the last one that finished (D6).
    ///
    /// Kept rather than only broadcast: a restore is not something a user
    /// should be able to lose by closing a window. `GetRestoreState` hands this
    /// over, so reopening the screen shows the run in progress.
    pub restore_run: std::sync::Mutex<Option<own_backup_common::restore_state::RestoreRunState>>,
    pub start_time: Instant,
}

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: AppConfig,
        store: Arc<JobStore>,
        queue: RunQueue,
        busy: Arc<own_backup_core::busy::Busy>,
        job_tx: UnboundedSender<String>,
        cancel: Arc<AtomicBool>,
        events: broadcast::Sender<proto::Event>,
        passwords: Arc<PasswordStore>,
    ) -> Arc<Self> {
        let auto = Arc::new(crate::auto_cleanup::AutoCleanup::new(
            &config.data_dir,
            store.clone(),
            events.clone(),
        ));
        Arc::new(Self {
            auto,
            config,
            store,
            queue,
            job_tx,
            cancel,
            busy,
            events,
            passwords,
            archive_setup: Arc::new(own_backup_core::archive_setup::ArchiveSetup::new()),
            active_mounts: std::sync::Mutex::new(Vec::new()),
            restore_run: std::sync::Mutex::new(None),
            start_time: Instant::now(),
        })
    }

    pub fn uptime_seconds(&self) -> u64 {
        self.start_time.elapsed().as_secs()
    }
}
