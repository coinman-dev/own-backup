//! A separate queue for scheduled verification, sharing the machine's busy
//! reservation with backups/restores. Timer ticks continue during long checks.
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::Duration,
};

use own_backup_common::tr;
use own_backup_core::{
    busy::{Busy, Doing},
    BackgroundOperation, EventSink, Job, JobProgress, JobRun, JobStore, RunQueue,
};
use tokio::sync::{mpsc, watch};
use tracing::{error, info};

pub(crate) fn is_due(
    job: &Job,
    now: chrono::DateTime<chrono::FixedOffset>,
    uptime: Duration,
) -> bool {
    let cfg = &job.verification.schedule;
    let Some(cron) = own_backup_scheduler::schedule_to_cron(cfg) else {
        return false;
    };
    let Some(schedule) = own_backup_scheduler::config_schedule::parse(&cron) else {
        return false;
    };
    let last = job
        .last_verification
        .as_ref()
        .and_then(|r| chrono::DateTime::parse_from_rfc3339(&r.started_at).ok());
    let catch_up =
        cfg.run_missed && uptime >= Duration::from_secs(u64::from(cfg.missed_delay_min) * 60);
    own_backup_scheduler::is_due(
        &schedule,
        now,
        last,
        catch_up,
        chrono::Duration::seconds(120),
    )
}

/// A check that made way for a person's operation runs again this much later.
const RETRY_AFTER_MAKING_WAY: Duration = Duration::from_secs(10 * 60);

pub fn spawn(
    store: Arc<JobStore>,
    busy: Arc<Busy>,
    queue: RunQueue,
    sink: Arc<dyn EventSink>,
    mut stop: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if own_backup_common::media_mode::is_media_mode() {
            return;
        }
        let (tx, mut rx) = mpsc::unbounded_channel::<(String, BackgroundOperation)>();
        // A check that made way for a person's operation runs again later.
        let retry_tx = tx.clone();
        let retry_queue = queue.clone();
        let pending = Arc::new(Mutex::new(HashSet::<String>::new()));
        let timer_store = store.clone();
        let timer_pending = pending.clone();
        let mut timer_stop = stop.clone();
        let timer = tokio::spawn(async move {
            let started = tokio::time::Instant::now();
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        if *timer_stop.borrow() { break; }
                        let now = chrono::Local::now().fixed_offset();
                        for job in timer_store.list() {
                            if is_due(&job, now, started.elapsed()) && timer_pending.lock().unwrap().insert(job.id.clone()) {
                                info!(job = %job.name, "Scheduler: enqueued an integrity check");
                                if tx.send((job.id.clone(), queue.register_background_operation())).is_err() { break; }
                            }
                        }
                    }
                    _ = timer_stop.changed() => break,
                }
            }
        });
        loop {
            let (id, _background) = tokio::select! {
                next = rx.recv() => match next { Some(id) => id, None => break },
                _ = stop.changed() => break,
            };
            if *stop.borrow() {
                break;
            }
            // The job's folder is what it reads: other jobs' copies, a boot
            // medium, may be worked on beside it. And it yields: an operation
            // a person starts over the same folder cancels it, and it runs
            // again ten minutes later.
            let Some(folder) = store.get(&id).map(|j| {
                own_backup_core::busy::chain_resource(std::path::Path::new(&j.dest_folder))
            }) else {
                pending.lock().unwrap().remove(&id);
                continue;
            };
            let slot = tokio::select! {
                slot = busy.acquire(
                    Doing::new(tr!("grpc_server.busy_validation"), id.clone())
                        .shared([folder])
                        .yielding(),
                ) => slot,
                _ = stop.changed() => break,
            };
            if *stop.borrow() {
                break;
            }
            // Fetch AFTER waiting: edits/deletion while queued must be honored.
            let Some(job) = store.get(&id).filter(|j| {
                own_backup_scheduler::schedule_to_cron(&j.verification.schedule).is_some()
            }) else {
                pending.lock().unwrap().remove(&id);
                continue;
            };
            let cancel = slot.cancel();
            let stepped_aside = slot.stepped_aside();
            let made_way = stepped_aside.clone();
            let task_store = store.clone();
            let task_sink = sink.clone();
            let task_job = job.clone();
            let outcome = tokio::task::spawn_blocking(move || {
                // The blocking task owns the guard until all file handles and
                // callbacks are finished, even when verification panics.
                let _slot = slot;
                #[cfg(target_os = "windows")]
                let _awake = own_backup_backup::power_win::KeepAwake::new(&task_job.name);
                #[cfg(target_os = "linux")]
                let _awake = own_backup_backup::power_linux::KeepAwake::new(&task_job.name);
                task_sink.job_progress(JobProgress {
                    job_id: task_job.id.clone(), phase: "scheduled-verify".into(), percent: 0,
                    blocks_done: 0, blocks_total: 0, speed_mbps: 0.0,
                    volume_index: 0, volume_count: 0, volume: task_job.name.clone(),
                    image_file_name: String::new(), categories: vec![], excluded_files: None, excluded_bytes: None,
                    stalled_secs: None,
                });
                let started_at = chrono::Utc::now().to_rfc3339();
                let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(||
                    own_backup_core::verification::perform(&task_store, &task_job, &cancel, task_sink.as_ref())
                )).unwrap_or_else(|_| JobRun {
                    operation: "verify".into(), id: uuid::Uuid::new_v4().to_string(),
                    job_id: task_job.id.clone(), job_name: task_job.name.clone(), started_at,
                    finished_at: chrono::Utc::now().to_rfc3339(), status: "error".into(),
                    error: tr!("scheduled_verification.panicked").into_owned(), ..Default::default()
                });
                // Made way for a person: not a result, and not this slot's
                // run — nothing is recorded, and it runs again later.
                if stepped_aside.load(std::sync::atomic::Ordering::SeqCst) {
                    info!(job = %task_job.name, "Scheduled integrity check made way for an operation started by a person; again in 10 minutes");
                    task_sink.job_finished(run.clone());
                    return run;
                }
                // Persist while holding the reservation: the next backup must
                // see failed-image state before it chooses a parent.
                if let Err(e) = task_store.record_run(run.clone()) {
                    error!(job = %task_job.name, error = %e, "integrity-check history could not be saved");
                }
                info!(job = %task_job.name, status = %run.status, error = %run.error, "Scheduled integrity check finished");
                task_sink.job_finished(run.clone());
                run
            }).await;
            match outcome {
                Ok(_) => {}
                Err(e) => {
                    let now = chrono::Utc::now().to_rfc3339();
                    let run = JobRun {
                        operation: "verify".into(),
                        id: uuid::Uuid::new_v4().to_string(),
                        job_id: id.clone(),
                        job_name: job.name.clone(),
                        started_at: now.clone(),
                        finished_at: now,
                        status: "error".into(),
                        error: e.to_string(),
                        ..Default::default()
                    };
                    if let Err(e) = store.record_run(run.clone()) {
                        error!(error = %e, "integrity-check failure could not be saved");
                    }
                    sink.job_finished(run);
                }
            }
            pending.lock().unwrap().remove(&id);
            if *stop.borrow() {
                break;
            }
            if made_way.load(std::sync::atomic::Ordering::SeqCst)
                && pending.lock().unwrap().insert(id.clone())
            {
                let (tx, queue, mut stop) = (retry_tx.clone(), retry_queue.clone(), stop.clone());
                tokio::spawn(async move {
                    tokio::select! {
                        _ = tokio::time::sleep(RETRY_AFTER_MAKING_WAY) => {
                            let _ = tx.send((id, queue.register_background_operation()));
                        }
                        _ = stop.changed() => {}
                    }
                });
            }
        }
        timer.abort();
        let _ = timer.await;
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use own_backup_core::QueueSnapshot;

    #[test]
    fn verification_has_its_own_last_run_and_uses_local_slots() {
        let now = chrono::FixedOffset::east_opt(3 * 3600)
            .unwrap()
            .with_ymd_and_hms(2026, 10, 7, 15, 1, 0)
            .unwrap();
        let mut job = Job::default();
        job.verification.schedule.kind = "weekly".into();
        job.verification.schedule.days = vec!["wed".into()];
        job.verification.schedule.time = "15:00".into();
        job.last_run = Some(JobRun {
            started_at: now.to_rfc3339(),
            ..Default::default()
        });
        assert!(is_due(&job, now, Duration::from_secs(120)));
        job.last_verification = Some(JobRun {
            started_at: "2026-10-07T12:00:05Z".into(),
            ..Default::default()
        });
        assert!(!is_due(&job, now, Duration::from_secs(120)));
        job.last_verification = Some(JobRun {
            started_at: "2026-10-07T11:59:05Z".into(),
            ..Default::default()
        });
        assert!(is_due(&job, now, Duration::from_secs(120)));
        job.verification.schedule.kind = "none".into();
        assert!(!is_due(&job, now, Duration::from_secs(120)));
    }

    #[test]
    fn missed_checks_wait_for_startup_grace_and_monthly_checks_fire() {
        let now = chrono::FixedOffset::east_opt(0)
            .unwrap()
            .with_ymd_and_hms(2026, 10, 7, 16, 0, 0)
            .unwrap();
        let mut job = Job::default();
        job.verification.schedule.kind = "monthly".into();
        job.verification.schedule.day_of_month = 7;
        job.verification.schedule.time = "15:00".into();
        job.verification.schedule.missed_delay_min = 5;
        assert!(!is_due(&job, now, Duration::from_secs(299)));
        assert!(is_due(&job, now, Duration::from_secs(300)));
        job.verification.schedule.run_missed = false;
        assert!(!is_due(&job, now, Duration::from_secs(300)));
    }

    struct Sink {
        finished: Mutex<Vec<JobRun>>,
        notify: tokio::sync::Notify,
    }
    impl EventSink for Sink {
        fn job_progress(&self, _: JobProgress) {}
        fn queue_changed(&self, _: QueueSnapshot) {}
        fn job_finished(&self, run: JobRun) {
            self.finished.lock().unwrap().push(run);
            self.notify.notify_one();
        }
    }
    fn scheduled(store: &JobStore) -> Job {
        let mut job = Job {
            name: "scheduled-check".into(),
            ..Default::default()
        };
        job.verification.schedule.kind = "daily".into();
        job.verification.schedule.time = chrono::Local::now().format("%H:%M").to_string();
        job.verification.schedule.run_missed = false;
        store.upsert(job).unwrap()
    }

    #[tokio::test]
    async fn a_check_waits_for_restore_and_records_a_failure_instead_of_empty_success() {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::load(dir.path());
        let job = scheduled(&store);
        let busy = Busy::new();
        let restore = busy.reserve(Doing::new("restore", "test-disk")).unwrap();
        let sink = Arc::new(Sink {
            finished: Mutex::new(vec![]),
            notify: tokio::sync::Notify::new(),
        });
        let (stop, stop_rx) = watch::channel(false);
        let queue = RunQueue::new();
        let worker = spawn(
            store.clone(),
            busy.clone(),
            queue.clone(),
            sink.clone(),
            stop_rx,
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(store.activity().is_empty());
        assert!(
            !queue.is_idle(),
            "a waiting integrity check must prevent after-backup sleep"
        );
        assert_eq!(busy.current().unwrap().what, "restore");
        drop(restore);
        tokio::time::timeout(Duration::from_secs(2), sink.notify.notified())
            .await
            .unwrap();
        let persisted = store.get(&job.id).unwrap();
        assert!(persisted.last_run.is_none());
        assert_eq!(persisted.last_verification.unwrap().status, "error");
        assert_eq!(store.activity().len(), 1);
        assert!(!is_due(
            &store.get(&job.id).unwrap(),
            chrono::Local::now().fixed_offset(),
            Duration::from_secs(120)
        ));
        stop.send_replace(true);
        worker.await.unwrap();
        assert!(queue.is_idle(), "completion releases the power hold");
    }

    #[tokio::test]
    async fn shutdown_cancels_a_check_waiting_for_the_slot() {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::load(dir.path());
        scheduled(&store);
        let busy = Busy::new();
        let _restore = busy.reserve(Doing::new("restore", "test-disk")).unwrap();
        let sink = Arc::new(Sink {
            finished: Mutex::new(vec![]),
            notify: tokio::sync::Notify::new(),
        });
        let (stop, stop_rx) = watch::channel(false);
        let worker = spawn(store.clone(), busy, RunQueue::new(), sink, stop_rx);
        tokio::time::sleep(Duration::from_millis(30)).await;
        stop.send_replace(true);
        tokio::time::timeout(Duration::from_secs(2), worker)
            .await
            .unwrap()
            .unwrap();
        assert!(store.activity().is_empty());
    }
}
