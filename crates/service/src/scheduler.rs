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
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::{debug, info, warn};

use own_backup_core::{Job, JobStore, PasswordStore, RunQueue};

use crate::shutdown::ShutdownSignal;

const TICK: std::time::Duration = std::time::Duration::from_secs(60);

/// OS events the Windows service control handler forwards to the scheduler, so
/// `kind = event` jobs with a matching `event_trigger` can fire. (USB device
/// arrival is not here — the `windows-service` crate doesn't decode device
/// events; it needs a custom notification handler.)
#[derive(Debug, Clone, Copy)]
pub enum OsEvent {
    Logon,
    Logoff,
    Shutdown,
}

/// Spawn the scheduler loop; it stops on shutdown. `os_rx` delivers OS events
/// (logon/logoff/shutdown) from the Windows service control handler; on other
/// platforms / foreground it simply never yields (its sender is kept alive).
pub fn spawn(
    store: Arc<JobStore>,
    queue: RunQueue,
    job_tx: UnboundedSender<String>,
    passwords: Arc<PasswordStore>,
    mut shutdown: ShutdownSignal,
    mut os_rx: UnboundedReceiver<OsEvent>,
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
                Some(ev) = os_rx.recv() => {
                    handle_os_event(ev, &store, &queue, &job_tx, &passwords);
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

        let due = own_backup_scheduler::is_due(&sched, now, last_fired, run_missed, fresh);
        // Diagnostic (own_backup_scheduler=debug): why a job fired or not. `now`
        // and the cron slot are in the machine's local timezone (OS setting).
        debug!(
            job = %job.name,
            kind = %job.schedule.kind,
            time = %job.schedule.time,
            run_missed,
            due,
            now = %now,
            "scheduler evaluated job"
        );
        if due {
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
    if job.schedule.event_once_per_day && ran_today(job) {
        fired_startup.insert(job.id.clone());
        return;
    }
    if !has_passwords(job, passwords) {
        return;
    }
    fired_startup.insert(job.id.clone());
    info!(job = %job.name, "Scheduler: startup event fired");
    enqueue(queue, job_tx, &job.id, &job.name);
}

/// Handle a forwarded OS event: enqueue every `event` job whose trigger matches
/// ("logon"/"logoff"/"shutdown"), respecting once-per-day and the password gate.
/// (Shutdown is best-effort — the service is stopping, so the run may not get to
/// complete; it never touches existing backups.)
fn handle_os_event(
    ev: OsEvent,
    store: &JobStore,
    queue: &RunQueue,
    job_tx: &UnboundedSender<String>,
    passwords: &PasswordStore,
) {
    let trigger = match ev {
        OsEvent::Logon => "logon",
        OsEvent::Logoff => "logoff",
        OsEvent::Shutdown => "shutdown",
    };
    for job in store.list() {
        if job.schedule.kind != "event" || job.schedule.event_trigger != trigger {
            continue;
        }
        if job.schedule.event_once_per_day && ran_today(&job) {
            continue;
        }
        if !has_passwords(&job, passwords) {
            continue;
        }
        info!(job = %job.name, trigger, "Scheduler: OS event fired");
        enqueue(queue, job_tx, &job.id, &job.name);
    }
}

/// Did the job's most recent run start today (local time)?
///
/// `started_at` is written in UTC, so its own `date_naive()` is a UTC date and
/// comparing it against a LOCAL one asks two different questions. East of
/// Greenwich the evening is already tomorrow over there: a run at 22:30 UTC is
/// 01:30 on the 19th in Moscow, and at 02:00 on that same local morning this
/// function called it yesterday's and ran the job again (audit AUD-11).
fn ran_today(job: &Job) -> bool {
    started_on_the_same_day(job, chrono::Local::now())
}

/// The comparison itself, with "now" handed in so it can be tested.
///
/// Both sides are brought into the SAME zone — the one `now` is in — before
/// the calendar day is taken. Which zone that is does not matter; that it is
/// one zone does.
fn started_on_the_same_day<Tz: chrono::TimeZone>(
    job: &Job,
    now: chrono::DateTime<Tz>,
) -> bool {
    job.last_run
        .as_ref()
        .and_then(|r| chrono::DateTime::parse_from_rfc3339(&r.started_at).ok())
        .is_some_and(|started| {
            started.with_timezone(&now.timezone()).date_naive() == now.date_naive()
        })
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
    if !queue.enqueue(id) {
        return; // already waiting or running
    }
    if job_tx.send(id.to_string()).is_ok() {
        info!(job = %name, "Scheduler: enqueued a scheduled backup");
    } else {
        // Nobody will come for it; a job left waiting for a worker that is
        // gone would sit in the queue for the rest of the session.
        queue.cancel_pending(id);
        warn!(job = %name, "Scheduler: enqueue failed (worker gone)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, TimeZone};

    fn job_started_at(utc: &str) -> Job {
        Job {
            last_run: Some(own_backup_core::JobRun {
                started_at: utc.to_string(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// The case that ran jobs twice. `started_at` is UTC; east of Greenwich the
    /// late evening there is already the next morning here, and comparing a UTC
    /// date against a local one called it yesterday's run (AUD-11).
    #[test]
    fn a_run_late_at_night_in_utc_is_today_east_of_greenwich() {
        let moscow = FixedOffset::east_opt(3 * 3600).unwrap();
        let job = job_started_at("2026-09-18T22:30:00Z"); // 19 Sep, 01:30 in Moscow
        let now = moscow.with_ymd_and_hms(2026, 9, 19, 2, 0, 0).unwrap();
        assert!(started_on_the_same_day(&job, now), "the same local day, so not again");
    }

    /// And the mirror case, which the old comparison got right by accident and
    /// must not now get wrong: west of Greenwich the early morning there is
    /// still the previous evening here.
    #[test]
    fn a_run_early_in_utc_is_yesterday_west_of_greenwich() {
        let new_york = FixedOffset::west_opt(4 * 3600).unwrap();
        let job = job_started_at("2026-09-19T02:00:00Z"); // 18 Sep, 22:00 in New York
        let now = new_york.with_ymd_and_hms(2026, 9, 19, 9, 0, 0).unwrap();
        assert!(!started_on_the_same_day(&job, now), "a different local day - run it");
    }

    #[test]
    fn the_same_local_day_is_the_same_day_whatever_the_hour() {
        let moscow = FixedOffset::east_opt(3 * 3600).unwrap();
        let job = job_started_at("2026-09-19T06:00:00Z"); // 09:00 in Moscow
        let now = moscow.with_ymd_and_hms(2026, 9, 19, 23, 59, 0).unwrap();
        assert!(started_on_the_same_day(&job, now));
    }

    #[test]
    fn a_job_that_never_ran_has_not_run_today() {
        assert!(!started_on_the_same_day(&Job::default(), chrono::Utc::now()));
    }

    /// A timestamp that cannot be read is not evidence of a run today: the
    /// safer answer is "run it" rather than "skip it".
    #[test]
    fn an_unreadable_timestamp_does_not_count_as_a_run() {
        let job = job_started_at("not a date at all");
        assert!(!started_on_the_same_day(&job, chrono::Utc::now()));
    }
}
