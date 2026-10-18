//! The job runner's view of an engine thread it is waiting for.
//!
//! A disk that keeps failing writes does not fail the write: Windows retries
//! each one for seconds, and a buffered writer waits behind a cache that
//! cannot be emptied. On the stand (2026-10-17) such runs stood still for 5
//! and for 30+ minutes, said nothing, and held the queue of every job; a
//! write-through writer only crawled instead (35 MB in 4 minutes). Nothing in
//! user mode interrupts that wait. So the runner watches from outside: it says
//! when the engine has gone quiet, and when the user cancels and the engine
//! does not answer, it stops waiting for it. A thread left behind learns that
//! at its next progress call — the one just before publishing included — and
//! discards its unfinished image instead of publishing it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;

use crate::{EventSink, JobProgress};

/// Quiet this long and the screen says so. Long enough for the quiet stretches
/// a healthy run has (a kernel merge of the exclusion pass, a slow flush).
pub(crate) const STALL_AFTER: Duration = Duration::from_secs(120);
/// After a cancel, how long the engine has to notice before it is left behind.
pub(crate) const CANCEL_GRACE: Duration = Duration::from_secs(20);
const TICK: Duration = Duration::from_secs(5);

pub(crate) struct EngineWatch {
    /// When the engine last called back, and the progress last shown.
    state: Mutex<(Instant, Option<JobProgress>)>,
    /// Set when the runner stops waiting. Per run on purpose: the job's cancel
    /// flag is cleared for the next run, and a thread that wakes up after that
    /// must still find out that nobody is waiting for it.
    abandoned: AtomicBool,
}

impl EngineWatch {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new((Instant::now(), None)),
            abandoned: AtomicBool::new(false),
        })
    }

    /// Every engine callback, whether it is shown or throttled away.
    pub(crate) fn alive(&self) {
        if let Ok(mut s) = self.state.lock() {
            s.0 = Instant::now();
        }
    }

    /// The progress just shown, kept to be repeated with the stall added.
    pub(crate) fn shown(&self, progress: &JobProgress) {
        if let Ok(mut s) = self.state.lock() {
            s.0 = Instant::now();
            s.1 = Some(progress.clone());
        }
    }

    /// Send `progress` to the screen and keep it for [`wait_engine`].
    pub(crate) fn show(&self, sink: &Arc<dyn EventSink>, progress: JobProgress) {
        self.shown(&progress);
        sink.job_progress(progress);
    }

    pub(crate) fn abandoned(&self) -> bool {
        self.abandoned.load(Ordering::SeqCst)
    }

    fn quiet(&self) -> (Duration, Option<JobProgress>) {
        match self.state.lock() {
            Ok(s) => (s.0.elapsed(), s.1.clone()),
            Err(_) => (Duration::ZERO, None),
        }
    }
}

pub(crate) enum Waited<T> {
    Done(Result<T, tokio::task::JoinError>),
    /// Cancelled, and the engine did not answer within [`CANCEL_GRACE`].
    LeftBehind,
}

/// Wait for the engine thread, saying so when it goes quiet, and stop waiting
/// when a cancel goes unanswered.
pub(crate) async fn wait_engine<T: Send + 'static>(
    handle: tokio::task::JoinHandle<T>,
    watch: &EngineWatch,
    cancel: &AtomicBool,
    sink: &Arc<dyn EventSink>,
) -> Waited<T> {
    tokio::pin!(handle);
    let mut cancelled_at: Option<Instant> = None;
    loop {
        tokio::select! {
            done = &mut handle => return Waited::Done(done),
            _ = tokio::time::sleep(TICK) => {}
        }
        let (idle, last) = watch.quiet();
        // Before the first progress there is nothing to repeat — and nothing
        // to blame on the destination: that is the snapshot being made.
        if idle >= STALL_AFTER {
            if let Some(mut progress) = last {
                progress.stalled_secs = Some(idle.as_secs());
                sink.job_progress(progress);
            }
        }
        if cancel.load(Ordering::SeqCst) {
            let since = *cancelled_at.get_or_insert_with(Instant::now);
            if since.elapsed() >= CANCEL_GRACE {
                watch.abandoned.store(true, Ordering::SeqCst);
                return Waited::LeftBehind;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{JobRun, QueueSnapshot};

    #[derive(Default)]
    struct Shown(Mutex<Vec<JobProgress>>);
    impl EventSink for Shown {
        fn job_progress(&self, progress: JobProgress) {
            self.0.lock().unwrap().push(progress);
        }
        fn queue_changed(&self, _: QueueSnapshot) {}
        fn job_finished(&self, _: JobRun) {}
    }

    fn progress() -> JobProgress {
        JobProgress {
            job_id: "j".into(),
            phase: "backup".into(),
            percent: 15,
            blocks_done: 89,
            blocks_total: 583,
            speed_mbps: 0.0,
            volume_index: 0,
            volume_count: 1,
            volume: String::new(),
            image_file_name: String::new(),
            categories: Vec::new(),
            excluded_files: None,
            excluded_bytes: None,
            stalled_secs: None,
        }
    }

    /// The disk stopped answering: the screen hears about it, a cancel is
    /// honoured without the engine, and the engine, when it wakes, finds out.
    #[tokio::test(start_paused = true)]
    async fn a_quiet_engine_is_reported_and_a_cancel_does_not_wait_for_it() {
        let watch = EngineWatch::new();
        watch.shown(&progress());
        let shown = Arc::new(Shown::default());
        let sink: Arc<dyn EventSink> = shown.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        // An engine that never answers. Not a blocking thread: paused time
        // does not advance while one runs, and the test would wait forever.
        let handle = tokio::spawn(std::future::pending::<()>());
        let canceller = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(STALL_AFTER + Duration::from_secs(30)).await;
            canceller.store(true, Ordering::SeqCst);
        });
        let waited = wait_engine(handle, &watch, &cancel, &sink).await;
        assert!(matches!(waited, Waited::LeftBehind));
        assert!(watch.abandoned());
        let stalls: Vec<u64> = shown
            .0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|p| p.stalled_secs)
            .collect();
        assert!(
            !stalls.is_empty() && stalls.iter().all(|s| *s >= STALL_AFTER.as_secs()),
            "{stalls:?}"
        );
    }

    /// A healthy run is waited for to the end and never called stalled.
    #[tokio::test(start_paused = true)]
    async fn an_engine_that_finishes_is_simply_waited_for() {
        let watch = EngineWatch::new();
        let sink: Arc<dyn EventSink> = Arc::new(Shown::default());
        let cancel = AtomicBool::new(false);
        let handle = tokio::spawn(async { 7 });
        match wait_engine(handle, &watch, &cancel, &sink).await {
            Waited::Done(Ok(7)) => {}
            _ => panic!("the engine's own result must come back"),
        }
        assert!(!watch.abandoned());
    }
}
