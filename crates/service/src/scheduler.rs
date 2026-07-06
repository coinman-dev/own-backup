//! In-service scheduler (plan v2, phase 4).
//!
//! Ticks once a minute and enqueues every job whose schedule has come due
//! (see [`own_backup_scheduler::is_due`]). Enqueue is deduped exactly like the
//! `RunJob` RPC, so a job that is already pending or running is never queued
//! twice — even if a slow backup spans several ticks.
//!
//! Not yet handled (follow-up): wake-from-sleep (needs a waitable timer or a
//! companion OS scheduled task) and prevent-sleep during a backup; `event` /
//! `nonstop` schedule kinds (need OS event hooks / an interval field).

use std::sync::Arc;

use chrono::Duration as ChronoDuration;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};

use own_backup_core::{JobStore, PasswordStore, RunQueue};

use crate::shutdown::ShutdownSignal;

const TICK: std::time::Duration = std::time::Duration::from_secs(60);

/// Spawn the scheduler loop; it stops on shutdown.
pub fn spawn(
    store: Arc<JobStore>,
    queue: RunQueue,
    job_tx: UnboundedSender<String>,
    passwords: Arc<PasswordStore>,
    mut shutdown: ShutdownSignal,
) {
    info!("Scheduler started (tick {}s)", TICK.as_secs());
    tokio::spawn(async move {
        // Service start ≈ system boot (auto-start service). `uptime` measured
        // from here gates each job's missed-run catch-up (`missed_delay_min`).
        let started = tokio::time::Instant::now();
        // Let the store/worker settle before the first tick.
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        let mut interval = tokio::time::interval(TICK);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    tick(&store, &queue, &job_tx, &passwords, started.elapsed());
                }
                _ = shutdown.wait_for_shutdown() => {
                    info!("Scheduler stopping");
                    break;
                }
            }
        }
    });
}

fn tick(
    store: &JobStore,
    queue: &RunQueue,
    job_tx: &UnboundedSender<String>,
    passwords: &PasswordStore,
    uptime: std::time::Duration,
) {
    let now = chrono::Local::now().fixed_offset();
    // A slot counts as "fresh" for two ticks, so one is never missed between
    // ticks even without catch-up.
    let fresh = ChronoDuration::seconds(2 * TICK.as_secs() as i64);

    for job in store.list() {
        let Some(cron) = own_backup_scheduler::schedule_to_cron(&job.schedule) else {
            continue;
        };
        let Some(sched) = own_backup_scheduler::config_schedule::parse(&cron) else {
            continue;
        };
        let last_fired = job
            .last_run
            .as_ref()
            .and_then(|r| chrono::DateTime::parse_from_rfc3339(&r.started_at).ok());

        // Catching up missed runs waits `missed_delay_min` after startup so the
        // machine can settle after boot; on-time slots still fire immediately.
        let grace_passed =
            uptime >= std::time::Duration::from_secs(job.schedule.missed_delay_min as u64 * 60);
        let run_missed = job.schedule.run_missed && grace_passed;

        if own_backup_scheduler::is_due(&sched, now, last_fired, run_missed, fresh) {
            // Gate encrypted jobs: without a saved password the run cannot
            // decrypt the volume unattended, so skip it (the user must save a
            // password to enable scheduling).
            if job.requires_password
                && job
                    .source_list()
                    .iter()
                    .any(|s| !passwords.has(&own_backup_core::source_token(s)))
            {
                warn!(
                    job = %job.name,
                    "Scheduler: skipping — encrypted source has no saved password"
                );
                continue;
            }
            enqueue(queue, job_tx, &job.id, &job.name);
        }
    }
}

/// Enqueue a job unless it is already pending or running (mirrors `RunJob`).
fn enqueue(queue: &RunQueue, job_tx: &UnboundedSender<String>, id: &str, name: &str) {
    {
        let pending = queue.pending.lock().unwrap();
        let current = queue.current.lock().unwrap();
        if pending.iter().any(|j| j == id) || current.as_deref() == Some(id) {
            return;
        }
    }
    queue.pending.lock().unwrap().push_back(id.to_string());
    if job_tx.send(id.to_string()).is_ok() {
        info!(job = %name, "Scheduler: enqueued a scheduled backup");
    } else {
        warn!(job = %name, "Scheduler: enqueue failed (worker gone)");
    }
}
