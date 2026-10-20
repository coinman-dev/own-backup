//! The observable state of a restore run (D6).
//!
//! Progress used to be broadcast as events and nothing more: close the window
//! mid-restore and the run became invisible. A restore is not something a user
//! should be able to lose track of, so the service keeps this structure and
//! hands it over on request — a reopened window picks up where it left off.
//!
//! It is also the run's report. When something fails there is no rollback, so
//! the only useful thing left to do is say exactly what happened: which
//! operations finished, which one broke, and what the user has to fix by hand.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Completed,
    /// An operation failed; everything after it was not attempted.
    Failed,
    /// The user cancelled. Whatever was already written stays written.
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpStatus {
    Pending,
    Running,
    Done,
    Failed,
    /// Not attempted, because the run stopped before reaching it.
    Skipped,
}

/// One operation as the user sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpRecord {
    /// Position in the executed order, which is not the order it was clicked in.
    pub index: usize,
    /// Stable slug from `RestoreOp::kind`.
    pub kind: String,
    /// Human-readable line from `RestoreOp::title`.
    pub title: String,
    /// Partition on the target this operation acts on.
    pub part_index: Option<u32>,
    pub status: OpStatus,
    pub percent: u32,
    pub bytes_done: u64,
    pub bytes_total: u64,
    /// Error text on failure, or a note worth keeping.
    pub message: String,
    pub cancellable: bool,
    /// From here on there is no going back.
    pub destroys_data: bool,
    /// Housekeeping the user did not ask for and does not need to watch.
    ///
    /// The fallback-loader step is one: it exists to cover a case that may not
    /// apply, it usually finds nothing to do, and listing it in the queue
    /// invites the question "why is my restore doing something to my
    /// bootloader?". It still runs, and still reports — but only when it
    /// actually changed something.
    #[serde(default)]
    pub hidden: bool,
}

/// Everything known about a run in progress or just finished.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreRunState {
    pub plan_id: Uuid,
    pub target_disk: u32,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub status: RunStatus,
    pub ops: Vec<OpRecord>,
    /// Index into `ops` of what is running right now.
    pub current: Option<usize>,
    /// Whether a cancel request would be honoured at this instant. The button
    /// follows this flag; the RPC refuses outright when it is false, rather
    /// than accepting the request and quietly doing nothing (D12, D18).
    pub cancellable: bool,
    /// Things the restore cannot do and the user must (re-encrypt LUKS, fix
    /// crypttab, run bcdboot by hand). Written as they are discovered.
    pub follow_ups: Vec<String>,
    /// The user has read the report and closed it.
    ///
    /// A finished run is deliberately sticky: a window closed mid-restore, or a
    /// service restarted under it, must come back to the report rather than to
    /// an empty screen. That is right up until the user dismisses it, after
    /// which the same report reappearing on every visit to the page is just
    /// noise — and worse, reads as if the restore had run again. The run itself
    /// stays in the history either way.
    #[serde(default)]
    pub acknowledged: bool,
    /// File name of the copy this run restored — what the «Активность» list
    /// names it by. Empty in runs recorded before 2026-09-26.
    #[serde(default)]
    pub image_file: String,
    /// Identities the run gave the restored copy (Linux, when the person
    /// chose to change the duplicates), and the files of the restored system
    /// it edited to match.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub identity_changes: Vec<crate::engine_dto::IdentityChangeDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edited_files: Vec<String>,
}

impl RestoreRunState {
    pub fn is_finished(&self) -> bool {
        !matches!(self.status, RunStatus::Running)
    }

    /// Ops that completed — the honest half of a failure report.
    pub fn completed(&self) -> impl Iterator<Item = &OpRecord> {
        self.ops.iter().filter(|o| o.status == OpStatus::Done)
    }

    /// The operation that broke, if one did.
    pub fn failure(&self) -> Option<&OpRecord> {
        self.ops.iter().find(|o| o.status == OpStatus::Failed)
    }

    /// Overall progress in percent, weighted by nothing clever: operations
    /// differ wildly in duration, so the count is only a rough shape. The
    /// in-flight operation's own percentage carries the detail.
    pub fn percent(&self) -> u32 {
        if self.ops.is_empty() {
            return 100;
        }
        let done = self
            .ops
            .iter()
            .filter(|o| o.status == OpStatus::Done)
            .count() as u64;
        (done * 100 / self.ops.len() as u64) as u32
    }
}
