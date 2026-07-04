//! GUI binding to the orchestrator core.
//!
//! All backup orchestration lives in [`own_backup_core`] (owned by the service
//! side). The GUI re-exports it so existing `crate::jobs::*` paths keep
//! working, and adapts core [events](own_backup_core::EventSink) to Tauri
//! window events (`job-progress` / `queue-changed` / `job-finished`) that the
//! Svelte frontend already listens for.
//!
//! This in-process wiring is transitional: once the GUI talks to the service
//! over gRPC, the worker runs in the service and these events arrive over a
//! progress stream instead.

pub use own_backup_core::*;

use tauri::{AppHandle, Emitter};

/// Forwards core orchestration events to Tauri window events.
pub struct TauriEventSink {
    pub app: AppHandle,
}

impl own_backup_core::EventSink for TauriEventSink {
    fn job_progress(&self, progress: own_backup_core::JobProgress) {
        let _ = self.app.emit("job-progress", progress);
    }
    fn queue_changed(&self, snapshot: own_backup_core::QueueSnapshot) {
        let _ = self.app.emit("queue-changed", snapshot);
    }
    fn job_finished(&self, run: own_backup_core::JobRun) {
        let _ = self.app.emit("job-finished", run);
    }
}
