//! In-service scheduler (plan v2, phase 4).
//!
//! Ticks once a minute and enqueues every job whose schedule has come due
//! (see [`own_backup_scheduler::is_due`]). Enqueue is deduped exactly like the
//! `RunJob` RPC, so a job that is already pending or running is never queued
//! twice — even if a slow backup spans several ticks.
//!
//! `kind = event` with `event_trigger = "startup"` is handled here too (fire N
//! minutes after the service/boot, at most once per boot / per day). The other
//! event triggers (usb_connect / logon / logoff / shutdown) still need OS
//! session/device/shutdown hooks. Also follow-up: wake-from-sleep and
//! prevent-sleep during a backup.

use std::collections::HashSet;
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
        // "startup" event jobs already fired this boot (cleared on restart).
        let mut fired_startup: HashSet<String> = HashSet::new();
        // Let the store/worker settle before the first tick.
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        let mut interval = tokio::time::interval(TICK);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    tick(&store, &queue, &job_tx, &passwords, started.elapsed(), &mut fired_startup);
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
    fired_startup: &mut HashSet<String>,
) {
    let now = chrono::Local::now().fixed_offset();
    // A slot counts as "fresh" for two ticks, so one is never missed between
    // ticks even without catch-up.
    let fresh = ChronoDuration::seconds(2 * TICK.as_secs() as i64);

    for job in store.list() {
        // Event-triggered jobs take a separate path (not cron/time based).
        if job.schedule.kind == "event" {
            handle_event(&job, uptime, fired_startup, queue, job_tx, passwords);
            continue;
        }

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
            if !has_passwords(&job, passwords) {
                continue;
            }
            enqueue(queue, job_tx, &job.id, &job.name);
        }
    }
}

/// Handle a `kind = event` job. Only the `startup` trigger fires so far: N
/// minutes after the service (≈ system) starts, once per boot — and, when
/// `event_once_per_day` is set, only if it has not already run today. The other
/// triggers (usb_connect / logon / logoff / shutdown) need OS event hooks.
fn handle_event(
    job: &own_backup_core::Job,
    uptime: std::time::Duration,
    fired_startup: &mut HashSet<String>,
    queue: &RunQueue,
    job_tx: &UnboundedSender<String>,
    passwords: &PasswordStore,
) {
    if job.schedule.event_trigger != "startup" {
        return; // other events not wired up yet
    }
    let delay = std::time::Duration::from_secs(job.schedule.event_startup_delay_min as u64 * 60);
    if uptime < delay || fired_startup.contains(&job.id) {
        return;
    }
    // At most once per day: if the last run started today, mark as done and skip.
    if job.schedule.event_once_per_day {
        let ran_today = job
            .last_run
            .as_ref()
            .and_then(|r| chrono::DateTime::parse_from_rfc3339(&r.started_at).ok())
            .is_some_and(|dt| dt.date_naive() == chrono::Local::now().date_naive());
        if ran_today {
            fired_startup.insert(job.id.clone());
            return;
        }
    }
    if !has_passwords(job, passwords) {
        return;
    }
    fired_startup.insert(job.id.clone());
    info!(job = %job.name, "Scheduler: startup event fired");
    enqueue(queue, job_tx, &job.id, &job.name);
}

/// True unless an encrypted source is missing its saved password (an unattended
/// run cannot then decrypt the volume, so it must be skipped).
fn has_passwords(job: &own_backup_core::Job, passwords: &PasswordStore) -> bool {
    if job.requires_password
        && job
            .source_list()
            .iter()
            .any(|s| !passwords.has(&own_backup_core::source_token(s)))
    {
        warn!(job = %job.name, "Scheduler: skipping — encrypted source has no saved password");
        return false;
    }
    true
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
