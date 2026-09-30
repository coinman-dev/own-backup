//! One destructive operation at a time, and one place that says which.
//!
//! Before this, each kind of operation guarded itself and none of them guarded
//! each other (audit AUD-02):
//!
//! * a restore plan checked `restore_run` under one lock and then set the new
//!   state under another, so two requests arriving together could both pass the
//!   check and both start;
//! * `restore_volume`, `restore_disk` and `verify_image` shared one
//!   `AtomicBool` cancel flag and **reset it at the start of each run**, so a
//!   second one starting silently disarmed the first one's cancel — and nothing
//!   stopped the two running at once in the first place;
//! * the backup queue, retention, consolidation, "delete the job's copies" and
//!   read-write mounts knew nothing about each other at all. A flag in one GUI
//!   window has never protected the service from a second client.
//!
//! The model here is the smallest one that is actually safe: **the machine does
//! one destructive thing at a time**. Finer-grained locks per disk or per chain
//! are a later optimisation, and they need a defined acquisition order to avoid
//! the deadlock that a single slot cannot have.
//!
//! What makes it atomic is that the reservation IS the lock: a semaphore permit
//! is taken or it is not, and the description beside it is only ever written by
//! whoever holds it. There is no window between "is anyone busy?" and "I am
//! busy now".
//!
//! The permit is released by [`Reservation`]'s `Drop`, which covers the three
//! endings that matter equally: success, an early `return` on error, and a
//! panic unwinding out of a `spawn_blocking` closure.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use own_backup_common::cancel::CancelToken;
use own_backup_common::tr;

/// What is going on right now.
#[derive(Debug, Clone)]
pub struct Doing {
    /// In the user's words: "восстановление", "резервное копирование".
    pub what: String,
    /// Which one — a plan id, a job id, an image path. For the message a second
    /// caller is given.
    pub which: String,
    /// The run's own cancel token. Never reset and addressed by id, so a cancel
    /// aimed at a finished run cannot hit the one that started after it.
    pub cancel: CancelToken,
}

/// The single slot every destructive operation passes through.
#[derive(Debug)]
pub struct Busy {
    /// One permit. `Arc` because the owned-permit form is what survives being
    /// moved into a `spawn_blocking` closure — which is where the guard has to
    /// live, so that a panic in the engine releases it.
    slot: Arc<tokio::sync::Semaphore>,
    doing: Mutex<Option<Doing>>,
}

impl Busy {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            slot: Arc::new(tokio::sync::Semaphore::new(1)),
            doing: Mutex::new(None),
        })
    }

    /// Take the machine, or say who has it. Never waits — a client asking for a
    /// second restore wants an answer, not a queue.
    pub fn reserve(self: &Arc<Self>, doing: Doing) -> Result<Reservation, Doing> {
        match self.slot.clone().try_acquire_owned() {
            Ok(permit) => {
                *self.doing.lock().unwrap() = Some(doing);
                Ok(Reservation {
                    permit: Some(permit),
                    busy: self.clone(),
                })
            }
            Err(_) => Err(self.current().unwrap_or_else(|| Doing {
                what: tr!("busy.another_operation").into_owned(),
                which: String::new(),
                cancel: CancelToken::default(),
            })),
        }
    }

    /// Take the machine, waiting for it to be free. For the backup worker,
    /// whose jobs are queued rather than refused: a scheduled copy that
    /// collides with a restore should happen afterwards, not be dropped.
    pub async fn acquire(self: &Arc<Self>, doing: Doing) -> Reservation {
        let permit = self
            .slot
            .clone()
            .acquire_owned()
            .await
            .expect("the busy slot is never closed");
        *self.doing.lock().unwrap() = Some(doing);
        Reservation {
            permit: Some(permit),
            busy: self.clone(),
        }
    }

    /// What is running, if anything.
    pub fn current(&self) -> Option<Doing> {
        self.doing.lock().unwrap().clone()
    }

    /// Cancel the run named by `id`, and say whether that was the one running.
    /// A "no" is worth telling the user: it means the run they were looking at
    /// has already ended, not that cancelling failed.
    pub fn cancel_run(&self, id: uuid::Uuid) -> bool {
        self.doing
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|d| d.cancel.cancel_run(id))
    }

    /// Cancel whatever is running. For the "stop" button that has no id to
    /// give, only what it can see on screen.
    pub fn cancel_whatever_is_running(&self) -> bool {
        match self.doing.lock().unwrap().as_ref() {
            Some(d) => {
                d.cancel.cancel();
                true
            }
            None => false,
        }
    }

    /// Wait for a blocking engine operation to return its reservation before
    /// the service closes the RPC transport. A stop request first cancels the
    /// current operation. RPC handlers still need transport-level graceful
    /// shutdown after this wait to send their final responses.
    pub async fn wait_until_idle(&self) {
        let permit = self
            .slot
            .clone()
            .acquire_owned()
            .await
            .expect("the busy slot is never closed");
        drop(permit);
    }
}

/// Proof of holding the slot. Dropping it hands the machine back — on success,
/// on an early return, and on a panic.
#[derive(Debug)]
pub struct Reservation {
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
    busy: Arc<Busy>,
}

impl Reservation {
    /// The cancel token of this run, to hand to the engine's callbacks.
    pub fn cancel(&self) -> CancelToken {
        self.busy.current().map(|d| d.cancel).unwrap_or_default()
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        // Description first, then the permit: the next holder must never find
        // the previous one's description still there.
        *self.busy.doing.lock().unwrap() = None;
        self.permit.take();
    }
}

impl Doing {
    /// A run of `what` on `which`, with a fresh cancel token.
    pub fn new(what: impl Into<String>, which: impl Into<String>) -> Self {
        Self {
            what: what.into(),
            which: which.into(),
            cancel: CancelToken::new(uuid::Uuid::new_v4()),
        }
    }

    /// The same, but addressed by an id the caller already has — a restore plan
    /// is cancelled by its plan id, and that is what the window knows.
    pub fn with_id(what: impl Into<String>, which: impl Into<String>, id: uuid::Uuid) -> Self {
        Self {
            what: what.into(),
            which: which.into(),
            cancel: CancelToken::new(id),
        }
    }

    /// The sentence a second caller is shown.
    pub fn refusal(&self) -> String {
        if self.which.is_empty() {
            tr!("busy.refusal", what = self.what).into_owned()
        } else {
            tr!("busy.refusal_which", what = self.what, which = self.which).into_owned()
        }
    }
}

/// How long the current operation has been holding the slot — for diagnostics
/// only; nothing decides anything by it.
#[allow(dead_code)]
pub fn since(started: Instant) -> std::time::Duration {
    started.elapsed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    #[tokio::test]
    async fn only_one_operation_can_hold_the_machine() {
        let busy = Busy::new();
        let first = busy
            .reserve(Doing::new("восстановление", "plan-1"))
            .expect("free");
        let second = busy.reserve(Doing::new("восстановление", "plan-2"));
        let refused = second.expect_err("the second must be refused");
        assert_eq!(refused.which, "plan-1", "the refusal names who has it");

        drop(first);
        assert!(
            busy.reserve(Doing::new("проверка", "image")).is_ok(),
            "released by Drop"
        );
    }

    #[tokio::test]
    async fn shutdown_drain_waits_for_the_active_engine_operation() {
        let busy = Busy::new();
        let held = busy.reserve(Doing::new("проверка", "image.obk")).unwrap();
        let waiting = busy.clone();
        let drain = tokio::spawn(async move { waiting.wait_until_idle().await });
        tokio::task::yield_now().await;
        assert!(
            !drain.is_finished(),
            "shutdown passed an active reservation"
        );
        drop(held);
        tokio::time::timeout(std::time::Duration::from_secs(1), drain)
            .await
            .expect("shutdown did not observe the released reservation")
            .unwrap();
    }

    /// The ending that used to leave the service stuck: the task panics, the
    /// guard unwinds with it, and the machine is free again.
    #[tokio::test]
    async fn a_panic_hands_the_machine_back() {
        let busy = Busy::new();
        let held = busy.clone();
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = held
                .reserve(Doing::new("восстановление", "plan-1"))
                .unwrap();
            panic!("the engine fell over");
        }));
        assert!(panicked.is_err());
        assert!(
            busy.current().is_none(),
            "nothing should still be marked as running"
        );
        assert!(busy.reserve(Doing::new("копирование", "job-1")).is_ok());
    }

    /// The race the audit names: two requests arriving together both passing
    /// the "is anything running?" check. Exactly one may win.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_requests_arriving_together_produce_exactly_one_winner() {
        let busy = Busy::new();
        let gate = Arc::new(Barrier::new(8));
        let attempts: Vec<_> = (0..8)
            .map(|i| {
                let busy = busy.clone();
                let gate = gate.clone();
                std::thread::spawn(move || {
                    gate.wait();
                    // The reservation is RETURNED, not dropped here: a guard
                    // released at the end of the attempt would let the next
                    // thread in and the test would prove nothing.
                    busy.reserve(Doing::new("восстановление", format!("plan-{i}")))
                        .ok()
                })
            })
            .collect();
        let held: Vec<_> = attempts
            .into_iter()
            .filter_map(|h| h.join().unwrap())
            .collect();
        assert_eq!(
            held.len(),
            1,
            "exactly one of eight simultaneous requests may start"
        );
    }

    /// Cancel is addressed. A stop aimed at a run that has already ended must
    /// not take down the one that started after it — the old shared flag did
    /// exactly that, and then got reset by the next run for good measure.
    #[tokio::test]
    async fn a_cancel_aimed_at_a_finished_run_does_not_hit_the_next_one() {
        let busy = Busy::new();
        let first_id = uuid::Uuid::new_v4();
        let first = busy
            .reserve(Doing::with_id("восстановление", "plan-1", first_id))
            .unwrap();
        drop(first);

        let second_id = uuid::Uuid::new_v4();
        let second = busy
            .reserve(Doing::with_id("восстановление", "plan-2", second_id))
            .unwrap();
        assert!(
            !busy.cancel_run(first_id),
            "the old id belongs to nothing now"
        );
        assert!(
            !second.cancel().is_cancelled(),
            "and it must not have touched this run"
        );
        assert!(busy.cancel_run(second_id));
        assert!(second.cancel().is_cancelled());
    }

    /// The queued backup waits its turn rather than being dropped.
    #[tokio::test]
    async fn a_waiting_caller_gets_the_machine_when_it_is_free() {
        let busy = Busy::new();
        let held = busy
            .reserve(Doing::new("восстановление", "plan-1"))
            .unwrap();
        let waiting = {
            let busy = busy.clone();
            tokio::spawn(async move {
                busy.acquire(Doing::new("копирование", "job-1")).await
            })
        };
        // Still held, so the waiter cannot have got in.
        assert_eq!(busy.current().unwrap().which, "plan-1");
        drop(held);
        let got = waiting.await.unwrap();
        assert_eq!(busy.current().unwrap().which, "job-1");
        drop(got);
    }
}
