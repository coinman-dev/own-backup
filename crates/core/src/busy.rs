//! Which operations may run together, and one place that says which.
//!
//! Before the single slot, each kind of operation guarded itself and none of
//! them guarded each other (audit AUD-02):
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
//! Then the machine did one thing at a time — safe, and too coarse: while one
//! job's image was verified, the window could not build a boot medium, verify
//! another job's copy or delete a third one's (owner, 23.10). Now an operation
//! says what it needs:
//!
//! * an **exclusive** one — a restore, anything that writes a disk the system
//!   may stand on — takes the whole machine, as before ([`Doing::new`]);
//! * a **shared** one takes the machine together with others and, exclusively,
//!   only its own resources ([`Doing::shared`]): the folder of a chain of copies
//!   (`chain:<folder>`), the boot-medium builder (`media-build`), a stick
//!   (`disk:<device>`). Two shared operations conflict only over a resource
//!   both name;
//! * a **yielding** one (a scheduled verification, [`Doing::yielding`]) steps
//!   aside: an operation a person started that needs what it holds cancels it
//!   ([`Busy::reserve_soon`]), and it runs again later.
//!
//! What keeps it atomic is unchanged: the check and the taking happen under one
//! lock, so there is no window between "is anyone in the way?" and "I hold it
//! now". Nothing ever waits while holding anything, so there is no order of
//! acquisition to get wrong.
//!
//! A reservation is released by [`Reservation`]'s `Drop`, which covers the
//! three endings that matter equally: success, an early `return` on error, and
//! a panic unwinding out of a `spawn_blocking` closure.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
    /// Takes the machine together with other shared operations, and these
    /// resources alone. `None`: the whole machine.
    shared: Option<Vec<String>>,
    /// Steps aside for an operation a person started ([`Busy::reserve_soon`]).
    yields: bool,
    /// A backup run: stopped by its queue's own cancel, not by the engine's
    /// stop button ([`Busy::cancel_whatever_is_running`]).
    backup: bool,
    /// Set when this yielding run was cancelled to make way for a person's
    /// operation ([`Reservation::stepped_aside`]).
    stepped_aside: Arc<AtomicBool>,
}

/// The reservations every destructive operation passes through.
#[derive(Debug)]
pub struct Busy {
    held: Mutex<Vec<HeldEntry>>,
    /// Woken whenever a reservation ends.
    released: tokio::sync::Notify,
    next: AtomicU64,
    /// The service is stopping, set before its cancel: whatever ends as
    /// cancelled from here on was not cancelled by a person
    /// ([`Self::cancel_for_stop`]).
    stopping: AtomicBool,
}

#[derive(Debug, Clone)]
struct HeldEntry {
    id: u64,
    doing: Doing,
}

/// Why a reservation was not given.
enum Refused {
    /// Held by these; at least one does not yield.
    Busy(Doing),
    /// Held only by operations that yield: cancelled, and about to end.
    Yielding,
}

impl Busy {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            held: Mutex::new(Vec::new()),
            released: tokio::sync::Notify::new(),
            next: AtomicU64::new(1),
            stopping: AtomicBool::new(false),
        })
    }

    /// Cancel whatever is running because the service is stopping (Windows
    /// shutting down or restarting, an update, `systemctl stop`). Marked first,
    /// so the run that ends cancelled is recorded as interrupted, not as a
    /// person's cancel, and its slot is caught up after the next start.
    pub fn cancel_for_stop(&self) -> bool {
        self.stopping.store(true, Ordering::SeqCst);
        let held = self.held.lock().unwrap();
        for h in held.iter() {
            h.doing.cancel.cancel();
        }
        !held.is_empty()
    }

    /// Whether [`Self::cancel_for_stop`] has been called.
    pub fn stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    /// Take what `doing` needs, or say who has it. Never waits — a client
    /// asking for a second restore wants an answer, not a queue.
    pub fn reserve(self: &Arc<Self>, doing: Doing) -> Result<Reservation, Doing> {
        match self.try_take(doing, false) {
            Ok(r) => Ok(r),
            Err(Refused::Busy(d)) => Err(d),
            Err(Refused::Yielding) => Err(Doing::new(tr!("busy.another_operation"), "")),
        }
    }

    /// [`Self::reserve`] for an operation a person started: what holds its
    /// resources and yields (a scheduled verification) is cancelled, and its
    /// end is waited for — up to `wait` — instead of refusing.
    pub async fn reserve_soon(
        self: &Arc<Self>,
        doing: Doing,
        wait: Duration,
    ) -> Result<Reservation, Doing> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let released = self.released.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            match self.try_take(doing.clone(), true) {
                Ok(r) => return Ok(r),
                Err(Refused::Busy(d)) => return Err(d),
                Err(Refused::Yielding) => {
                    if tokio::time::timeout_at(deadline, released).await.is_err() {
                        return Err(self
                            .conflicting(&doing)
                            .unwrap_or_else(|| Doing::new(tr!("busy.another_operation"), "")));
                    }
                }
            }
        }
    }

    /// Take what `doing` needs, waiting for it to be free. For the backup
    /// worker and the scheduled verification, whose runs are queued rather
    /// than refused: a scheduled copy that collides with a restore should
    /// happen afterwards, not be dropped.
    pub async fn acquire(self: &Arc<Self>, doing: Doing) -> Reservation {
        loop {
            let released = self.released.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            if let Ok(r) = self.try_take(doing.clone(), false) {
                return r;
            }
            released.await;
        }
    }

    /// Under the one lock: does anything held stand in the way, and if not,
    /// hold it. With `push`, holders that yield are cancelled when they are
    /// all that stands in the way.
    fn try_take(self: &Arc<Self>, doing: Doing, push: bool) -> Result<Reservation, Refused> {
        let mut held = self.held.lock().unwrap();
        let in_way: Vec<&HeldEntry> = held
            .iter()
            .filter(|h| conflicts(&h.doing, &doing))
            .collect();
        if in_way.is_empty() {
            let id = self.next.fetch_add(1, Ordering::SeqCst);
            let cancel = doing.cancel.clone();
            let stepped_aside = doing.stepped_aside.clone();
            held.push(HeldEntry { id, doing });
            return Ok(Reservation {
                id,
                cancel,
                stepped_aside,
                busy: self.clone(),
            });
        }
        if let Some(first) = in_way.iter().find(|h| !h.doing.yields || doing.yields) {
            return Err(Refused::Busy(first.doing.clone()));
        }
        if push {
            for h in &in_way {
                h.doing.stepped_aside.store(true, Ordering::SeqCst);
                h.doing.cancel.cancel();
            }
            return Err(Refused::Yielding);
        }
        Err(Refused::Busy(in_way[0].doing.clone()))
    }

    /// The first operation standing in `doing`'s way, if any.
    fn conflicting(&self, doing: &Doing) -> Option<Doing> {
        self.held
            .lock()
            .unwrap()
            .iter()
            .find(|h| conflicts(&h.doing, doing))
            .map(|h| h.doing.clone())
    }

    /// What is running — the earliest of it, if anything.
    pub fn current(&self) -> Option<Doing> {
        self.held.lock().unwrap().first().map(|h| h.doing.clone())
    }

    /// Everything running now.
    pub fn running(&self) -> Vec<Doing> {
        self.held
            .lock()
            .unwrap()
            .iter()
            .map(|h| h.doing.clone())
            .collect()
    }

    /// Cancel the run named by `id`, and say whether one was running. A "no"
    /// is worth telling the user: it means the run they were looking at has
    /// already ended, not that cancelling failed.
    pub fn cancel_run(&self, id: uuid::Uuid) -> bool {
        self.held
            .lock()
            .unwrap()
            .iter()
            .any(|h| h.doing.cancel.cancel_run(id))
    }

    /// Cancel what the engine's stop button means: every operation running
    /// that is not a backup (a backup is stopped by its queue's own cancel).
    pub fn cancel_whatever_is_running(&self) -> bool {
        let held = self.held.lock().unwrap();
        let mut any = false;
        for h in held.iter().filter(|h| !h.doing.backup) {
            h.doing.cancel.cancel();
            any = true;
        }
        any
    }

    /// Wait until nothing holds anything — before the service closes the RPC
    /// transport. A stop request first cancels what is running. RPC handlers
    /// still need transport-level graceful shutdown after this wait to send
    /// their final responses.
    pub async fn wait_until_idle(&self) {
        loop {
            let released = self.released.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            if self.held.lock().unwrap().is_empty() {
                return;
            }
            released.await;
        }
    }
}

/// Do these two need anything in common? Either taking the whole machine is
/// enough; two shared ones meet only over a resource both name.
fn conflicts(a: &Doing, b: &Doing) -> bool {
    match (&a.shared, &b.shared) {
        (Some(x), Some(y)) => x.iter().any(|k| y.contains(k)),
        _ => true,
    }
}

/// Proof of holding what an operation needs. Dropping it hands it back — on
/// success, on an early return, and on a panic.
#[derive(Debug)]
pub struct Reservation {
    id: u64,
    cancel: CancelToken,
    stepped_aside: Arc<AtomicBool>,
    busy: Arc<Busy>,
}

impl Reservation {
    /// The cancel token of this run, to hand to the engine's callbacks.
    pub fn cancel(&self) -> CancelToken {
        self.cancel.clone()
    }

    /// Set once this yielding run is cancelled to make way for a person's
    /// operation: it did not fail and was not stopped, it is to run again.
    /// A handle, so it can be read after the reservation moved away.
    pub fn stepped_aside(&self) -> Arc<AtomicBool> {
        self.stepped_aside.clone()
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.busy.held.lock().unwrap().retain(|h| h.id != self.id);
        self.busy.released.notify_waiters();
    }
}

impl Doing {
    /// A run of `what` on `which`, with a fresh cancel token, taking the whole
    /// machine.
    pub fn new(what: impl Into<String>, which: impl Into<String>) -> Self {
        Self {
            what: what.into(),
            which: which.into(),
            cancel: CancelToken::new(uuid::Uuid::new_v4()),
            shared: None,
            yields: false,
            backup: false,
            stepped_aside: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The same, but addressed by an id the caller already has — a restore plan
    /// is cancelled by its plan id, and that is what the window knows.
    pub fn with_id(what: impl Into<String>, which: impl Into<String>, id: uuid::Uuid) -> Self {
        Self {
            cancel: CancelToken::new(id),
            ..Self::new(what, which)
        }
    }

    /// Run beside other shared operations, holding only `resources`
    /// exclusively (see the module docs for their names).
    pub fn shared<I, S>(mut self, resources: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.shared = Some(resources.into_iter().map(Into::into).collect());
        self
    }

    /// Step aside for an operation a person started ([`Busy::reserve_soon`]).
    pub fn yielding(mut self) -> Self {
        self.yields = true;
        self
    }

    /// A backup run (see [`Busy::cancel_whatever_is_running`]).
    pub fn backup(mut self) -> Self {
        self.backup = true;
        self
    }

    /// Whether this one steps aside for a person's operation.
    pub fn yields(&self) -> bool {
        self.yields
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

/// The resource name of the chain of copies in `folder`: two operations on
/// copies in the same folder — a backup writing there, a verification reading
/// there, a deletion — must not run together. The folder as the system
/// resolves it (case-insensitive on Windows), or as written when it does not
/// exist yet.
pub fn chain_resource(folder: &std::path::Path) -> String {
    let resolved = std::fs::canonicalize(folder).unwrap_or_else(|_| folder.to_path_buf());
    let text = resolved.to_string_lossy().into_owned();
    if cfg!(windows) {
        format!("chain:{}", text.to_lowercase())
    } else {
        format!("chain:{text}")
    }
}

/// The resource name of the chain the image at `image` belongs to: its folder.
pub fn image_chain_resource(image: &std::path::Path) -> String {
    chain_resource(image.parent().unwrap_or(std::path::Path::new("")))
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

    /// Shared operations on different resources run together; over one
    /// resource, or beside a whole-machine one, they do not.
    #[tokio::test]
    async fn shared_operations_meet_only_over_a_resource() {
        let busy = Busy::new();
        let verify = busy
            .reserve(Doing::new("проверка", "job-a").shared(["chain:/b/a"]))
            .expect("free");
        let build = busy
            .reserve(Doing::new("сборка носителя", "").shared(["media-build"]))
            .expect("another resource: beside it");
        let other = busy
            .reserve(Doing::new("проверка", "job-b").shared(["chain:/b/b"]))
            .expect("another chain: beside it");
        let same = busy.reserve(Doing::new("удаление копий", "job-a").shared(["chain:/b/a"]));
        assert_eq!(same.expect_err("the same chain").which, "job-a");
        let restore = busy.reserve(Doing::new("восстановление", "plan"));
        assert!(restore.is_err(), "a restore takes the whole machine");
        drop((verify, build, other));
        let restore = busy
            .reserve(Doing::new("восстановление", "plan"))
            .expect("all free");
        assert!(
            busy.reserve(Doing::new("сборка носителя", "").shared(["media-build"]))
                .is_err(),
            "nothing beside a restore"
        );
        drop(restore);
    }

    /// A scheduled verification steps aside for a person: cancelled, ended,
    /// and the person's operation goes ahead.
    #[tokio::test]
    async fn a_yielding_operation_steps_aside_for_a_person() {
        let busy = Busy::new();
        let scheduled = busy
            .reserve(
                Doing::new("проверка", "job-a")
                    .shared(["chain:/b/a"])
                    .yielding(),
            )
            .unwrap();
        let token = scheduled.cancel();
        let aside = scheduled.stepped_aside();
        // The scheduled run notices its cancel and ends, as the engine does.
        let ender = tokio::spawn(async move {
            while !token.is_cancelled() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            drop(scheduled);
        });
        let person = busy
            .reserve_soon(
                Doing::new("удаление копий", "job-a").shared(["chain:/b/a"]),
                Duration::from_secs(5),
            )
            .await
            .expect("the scheduled one made way");
        ender.await.unwrap();
        assert!(
            aside.load(Ordering::SeqCst),
            "it knows it made way, not failed"
        );
        assert_eq!(busy.running().len(), 1);
        drop(person);

        // What does not yield is not cancelled.
        let backup = busy
            .reserve(
                Doing::new("копирование", "job-a")
                    .shared(["chain:/b/a"])
                    .backup(),
            )
            .unwrap();
        let refused = busy
            .reserve_soon(
                Doing::new("удаление копий", "job-a").shared(["chain:/b/a"]),
                Duration::from_millis(50),
            )
            .await;
        assert!(refused.is_err());
        assert!(!backup.cancel().is_cancelled());
    }

    /// The engine's stop button stops engine operations, not a backup.
    #[tokio::test]
    async fn the_stop_button_leaves_a_backup_alone() {
        let busy = Busy::new();
        let backup = busy
            .reserve(
                Doing::new("копирование", "job")
                    .shared(["chain:/x"])
                    .backup(),
            )
            .unwrap();
        let build = busy
            .reserve(Doing::new("сборка носителя", "").shared(["media-build"]))
            .unwrap();
        assert!(busy.cancel_whatever_is_running());
        assert!(build.cancel().is_cancelled());
        assert!(!backup.cancel().is_cancelled());
        assert!(busy.cancel_for_stop());
        assert!(
            backup.cancel().is_cancelled(),
            "the service stopping stops it too"
        );
    }

    #[test]
    fn a_chain_is_named_by_its_folder() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("job_full_1.qcow2");
        assert_eq!(image_chain_resource(&image), chain_resource(dir.path()));
        assert!(chain_resource(dir.path()).starts_with("chain:"));
    }
}
