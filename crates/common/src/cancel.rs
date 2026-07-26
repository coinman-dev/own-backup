//! Cancellation for long-running operations (D18).
//!
//! The old scheme paired `store(false)` at the start of an operation with
//! `swap(false)` at the check. Both halves are wrong in the same way: the flag
//! is *consumed*.
//!
//! - `store(false)` at the start of every operation means a cancel that arrives
//!   between two operations is wiped by the next one starting. With a single
//!   long backup that is a narrow window. With a queue of a dozen restore
//!   operations it is most of the run.
//! - `swap(false)` at the check means the first reader clears the flag, so a
//!   second reader — a nested loop, a parallel worker — never sees it.
//!
//! The fix is to stop reusing the flag. A token is created per run and never
//! reset; cancelling sets it once and it stays set. Reads are plain loads.
//! Starting a new run means a *new* token, so nothing needs clearing and there
//! is no window in which a cancel can be lost.
//!
//! Cancellation is also addressed: [`CancelToken::id`] names the run it belongs
//! to, so a cancel aimed at a restore that has already finished cannot take
//! down the verification pass that started after it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use uuid::Uuid;

/// A one-way flag: unset until cancelled, then cancelled forever.
#[derive(Debug, Clone)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
    id: Uuid,
}

impl CancelToken {
    /// A fresh token for a new run. Never reuse one across runs — that is the
    /// bug this type exists to prevent.
    pub fn new(id: Uuid) -> Self {
        Self { flag: Arc::new(AtomicBool::new(false)), id }
    }

    /// The run this token belongs to.
    pub fn id(&self) -> Uuid {
        self.id
    }

    /// Request cancellation. Idempotent.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Relaxed);
    }

    /// Request cancellation only if this token belongs to `id`. Returns whether
    /// it matched — a caller can then tell the user "that run already finished"
    /// instead of silently cancelling something else.
    pub fn cancel_run(&self, id: Uuid) -> bool {
        if self.id != id {
            return false;
        }
        self.cancel();
        true
    }

    /// Has cancellation been requested? A plain load: reading never consumes
    /// the flag, so every checker sees it.
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }
}

impl Default for CancelToken {
    /// A token nobody can address by id — useful for tests and for callers with
    /// nothing to cancel.
    fn default() -> Self {
        Self::new(Uuid::nil())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reading_does_not_consume_the_flag() {
        // The bug this type replaces: with swap(), the second reader saw false.
        let t = CancelToken::new(Uuid::new_v4());
        t.cancel();
        assert!(t.is_cancelled());
        assert!(t.is_cancelled());
        assert!(t.is_cancelled());
    }

    #[test]
    fn clones_share_one_flag() {
        let t = CancelToken::new(Uuid::new_v4());
        let worker = t.clone();
        t.cancel();
        assert!(worker.is_cancelled());
    }

    #[test]
    fn a_new_run_gets_a_token_that_is_not_already_cancelled() {
        // The other half of the old bug: a stale cancel leaking into the next run.
        let first = CancelToken::new(Uuid::new_v4());
        first.cancel();
        let second = CancelToken::new(Uuid::new_v4());
        assert!(!second.is_cancelled());
    }

    #[test]
    fn cancelling_by_id_only_hits_the_named_run() {
        let id = Uuid::new_v4();
        let t = CancelToken::new(id);
        assert!(!t.cancel_run(Uuid::new_v4()), "a stale id must not match");
        assert!(!t.is_cancelled());
        assert!(t.cancel_run(id));
        assert!(t.is_cancelled());
    }
}
