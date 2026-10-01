//! In-service scheduler (plan v2, phase 4).
//!
//! Ticks once a minute and enqueues every job whose schedule has come due
//! (see [`own_backup_scheduler::is_due`]). Enqueue is deduped exactly like the
//! `RunJob` RPC, so a job that is already pending or running is never queued
//! twice — even if a slow backup spans several ticks.
//!
//! `kind = event` jobs fire here too:
//! - "startup": N minutes after the service (≈ boot), at most once per boot /
//!   per day;
//! - "logon" / "logoff" / "shutdown": forwarded by the service control handler
//!   ([`OsEvent`]);
//! - "dest_connected": the drive the copy goes to appears — polled every
//!   [`DEST_TICK`] ([`watch_destinations`]).
//!
//! It also arms the wake timer for jobs that ask to wake the machine
//! (`schedule.wake_to_run`, [`next_wake`]). Keeping the machine awake WHILE a
//! copy runs is the run's own business (`core::run_queue_worker`).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::Duration as ChronoDuration;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::{debug, info, warn};

use own_backup_core::{Job, JobStore, PasswordStore, RunQueue};

use crate::shutdown::ShutdownSignal;

const TICK: std::time::Duration = std::time::Duration::from_secs(60);
/// How often destination drives are looked at for "dest_connected" jobs. Ten
/// seconds: plugging a drive in and seeing nothing happen for a minute reads
/// as "it does not work".
const DEST_TICK: std::time::Duration = std::time::Duration::from_secs(10);
/// Give fresh local probes a short chance to finish without making a missing
/// network share hold up the scheduler or the other destinations.
const DEST_PROBE_INITIAL_WAIT: std::time::Duration = std::time::Duration::from_millis(100);
/// Wake this long before the slot: the scheduler ticks once a minute.
const WAKE_EARLY_SECS: i64 = 60;

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
        // Whether each "dest_connected" job's drive was there last time it
        // was looked at. The first look only records: a drive that was
        // already connected when the service started was not just connected.
        let mut dest_seen: HashMap<String, bool> = HashMap::new();
        let mut dest_probes: HashMap<
            String,
            (Option<std::path::PathBuf>, tokio::task::JoinHandle<bool>),
        > = HashMap::new();
        let wake = WakeArming::start();
        // Let the store/worker settle before the first tick.
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        let mut interval = tokio::time::interval(TICK);
        let mut dest_interval = tokio::time::interval(DEST_TICK);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    tick(&store, &queue, &job_tx, &passwords, started.elapsed(), &mut fired_startup);
                    wake.update(&store.list());
                }
                _ = dest_interval.tick() => {
                    watch_destinations(&store, &queue, &job_tx, &passwords, &mut dest_seen, &mut dest_probes).await;
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

/// Handle a `kind = event` job on the minute tick — the `startup` trigger: N
/// minutes after the service (≈ system) starts, once per boot — and, when
/// `event_once_per_day` is set, only if it has not already run today. The other
/// triggers have their own paths ([`handle_os_event`], [`watch_destinations`]).
fn handle_event(
    job: &own_backup_core::Job,
    uptime: std::time::Duration,
    fired_startup: &mut HashSet<String>,
    queue: &RunQueue,
    job_tx: &UnboundedSender<String>,
    passwords: &PasswordStore,
) {
    if job.schedule.event_trigger != "startup" {
        return; // fired elsewhere — see above
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

/// Look at the destination drive of every "dest_connected" job and fire the
/// ones whose drive has just appeared.
async fn watch_destinations(
    store: &JobStore,
    queue: &RunQueue,
    job_tx: &UnboundedSender<String>,
    passwords: &PasswordStore,
    seen: &mut HashMap<String, bool>,
    probes: &mut HashMap<String, (Option<std::path::PathBuf>, tokio::task::JoinHandle<bool>)>,
) {
    let jobs: Vec<Job> = store
        .list()
        .into_iter()
        .filter(|j| j.schedule.is_dest_connected_trigger())
        .collect();
    seen.retain(|id, _| jobs.iter().any(|j| &j.id == id));
    // Keep a started blocking call in this map until it really finishes. A
    // timeout on its JoinHandle does not cancel std::fs::metadata; forgetting
    // the handle used to spawn another stuck thread every ten seconds.
    probes.retain(|id, _| jobs.iter().any(|j| &j.id == id));
    if jobs.is_empty() {
        return;
    }

    for job in &jobs {
        let root = dest_root(&job.dest_folder);
        if probes
            .get(&job.id)
            .is_some_and(|(old, handle)| old != &root && !handle.is_finished())
        {
            continue; // old system call still running; never start another one
        }
        if probes.get(&job.id).is_some_and(|(old, _)| old != &root) {
            probes.remove(&job.id); // stale answer for a destination the user changed
        }
        if !probes.contains_key(&job.id) {
            let query = root.clone();
            debug!(job = %job.name, root = ?root, "destination probe started");
            probes.insert(
                job.id.clone(),
                (
                    root,
                    tokio::task::spawn_blocking(move || {
                        query.is_some_and(|r| std::fs::metadata(r).is_ok())
                    }),
                ),
            );
        }
    }
    debug!(
        jobs = jobs.len(),
        probes = probes.len(),
        pending = probes.values().filter(|(_, h)| !h.is_finished()).count(),
        "destination probe slots"
    );
    tokio::time::sleep(DEST_PROBE_INITIAL_WAIT).await;
    for job in jobs {
        if !probes.get(&job.id).is_some_and(|(root, handle)| {
            *root == dest_root(&job.dest_folder) && handle.is_finished()
        }) {
            continue;
        }
        let (_, handle) = probes.remove(&job.id).expect("probe was present");
        let Ok(now_there) = handle.await else {
            continue;
        };
        debug!(job = %job.name, reachable = now_there, "destination probe finished");
        let before = seen.insert(job.id.clone(), now_there);
        if before == Some(now_there) || before.is_none() {
            continue; // unchanged, or the first look
        }
        if !now_there {
            info!(job = %job.name, dest = %job.dest_folder, "destination drive disconnected");
            continue;
        }
        info!(job = %job.name, dest = %job.dest_folder, "destination drive connected");
        if job.schedule.event_once_per_day && ran_today(&job) {
            continue;
        }
        if !has_passwords(&job, passwords) {
            continue;
        }
        enqueue(queue, job_tx, &job.id, &job.name);
    }
}

/// The root that says whether a destination is reachable at all: `E:\` for
/// `E:\Backup\Job`, `\\server\share\` for a share. The folder itself may not
/// exist yet — a first copy creates it — so the drive is what is asked about.
fn dest_root(dest: &str) -> Option<std::path::PathBuf> {
    let d = dest.trim();
    if let Some(rest) = d.strip_prefix(r"\\") {
        let mut parts = rest.split(['\\', '/']).filter(|p| !p.is_empty());
        let (server, share) = (parts.next()?, parts.next()?);
        return Some(format!(r"\\{server}\{share}\").into());
    }
    let b = d.as_bytes();
    (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
        .then(|| format!("{}:\\", (b[0] as char).to_ascii_uppercase()).into())
}

/// When the machine must be awake next: a minute before the earliest slot of
/// any daily/weekly/monthly job that asks to wake it. `None` = nothing to wake
/// for.
fn next_wake(
    jobs: &[Job],
    now: chrono::DateTime<chrono::FixedOffset>,
) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    jobs.iter()
        .filter(|j| j.schedule.wake_to_run)
        .filter_map(|j| {
            let cron = own_backup_scheduler::schedule_to_cron(&j.schedule)?;
            let sched = own_backup_scheduler::config_schedule::parse(&cron)?;
            sched.after(&now).next()
        })
        .min()
        .map(|slot| slot - ChronoDuration::seconds(WAKE_EARLY_SECS))
}

/// The wake timer, kept pointed at [`next_wake`]. Windows only, and never on
/// the boot medium (nothing there outlives the session).
struct WakeArming {
    #[cfg(target_os = "windows")]
    timer: Option<Arc<own_backup_backup::power_win::WakeTimer>>,
    armed: std::sync::Mutex<Option<chrono::DateTime<chrono::FixedOffset>>>,
}

impl WakeArming {
    fn start() -> Self {
        #[cfg(target_os = "windows")]
        let timer = if own_backup_common::media_mode::is_media_mode() {
            None
        } else {
            // Held awake five minutes after waking: the tick that starts the
            // copy comes within one, and the copy then holds it itself.
            own_backup_backup::power_win::WakeTimer::start(std::time::Duration::from_secs(300))
        };
        Self {
            #[cfg(target_os = "windows")]
            timer,
            armed: std::sync::Mutex::new(None),
        }
    }

    fn update(&self, jobs: &[Job]) {
        let now = chrono::Local::now().fixed_offset();
        // A time already passed (the slot is within the minute) needs no timer:
        // the machine is awake to see it.
        let want = next_wake(jobs, now).filter(|at| *at > now);
        let mut armed = self.armed.lock().unwrap();
        if *armed == want {
            return;
        }
        match want {
            Some(at) => info!(at = %at, "wake timer armed for a scheduled copy"),
            None if armed.is_some() => {
                info!("wake timer disarmed — no copy asks to wake the machine")
            }
            None => {}
        }
        #[cfg(target_os = "windows")]
        if let Some(timer) = &self.timer {
            match want {
                Some(at) => timer.arm(at.with_timezone(&chrono::Utc)),
                None => timer.disarm(),
            }
        }
        *armed = want;
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
fn started_on_the_same_day<Tz: chrono::TimeZone>(job: &Job, now: chrono::DateTime<Tz>) -> bool {
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

    #[tokio::test]
    async fn a_pending_network_probe_is_retained_while_a_local_edge_fires() {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::load(dir.path());
        let make = |name: &str, dest: &str| {
            let mut job = Job::default();
            job.name = name.into();
            job.dest_folder = dest.into();
            job.schedule.kind = "event".into();
            job.schedule.event_trigger = "dest_connected".into();
            job
        };
        let network = store.upsert(make("remote", r"\\nas\share\job")).unwrap();
        let local = store.upsert(make("local", r"R:\job")).unwrap();
        let queue = RunQueue::new();
        let passwords = PasswordStore::new(dir.path().join("metadata.db"));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (release, wait) = tokio::sync::oneshot::channel::<()>();
        let pending = tokio::spawn(async move {
            let _ = wait.await;
            false
        });
        let pending_id = pending.id();
        let done = tokio::spawn(async { true });
        tokio::task::yield_now().await;
        let mut probes = HashMap::from([
            (
                network.id.clone(),
                (dest_root(&network.dest_folder), pending),
            ),
            (local.id.clone(), (dest_root(&local.dest_folder), done)),
        ]);
        let mut seen = HashMap::from([(network.id.clone(), false), (local.id.clone(), false)]);
        watch_destinations(&store, &queue, &tx, &passwords, &mut seen, &mut probes).await;
        assert_eq!(rx.try_recv().unwrap(), local.id);
        for _ in 0..12 {
            watch_destinations(&store, &queue, &tx, &passwords, &mut seen, &mut probes).await;
            assert_eq!(
                probes[&network.id].1.id(),
                pending_id,
                "stalled probe was replaced"
            );
        }
        assert_eq!(probes.keys().filter(|id| *id == &network.id).count(), 1);
        let _ = release.send(());
        let (_, handle) = probes.remove(&network.id).unwrap();
        handle.await.unwrap();
    }

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
        assert!(
            started_on_the_same_day(&job, now),
            "the same local day, so not again"
        );
    }

    /// And the mirror case, which the old comparison got right by accident and
    /// must not now get wrong: west of Greenwich the early morning there is
    /// still the previous evening here.
    #[test]
    fn a_run_early_in_utc_is_yesterday_west_of_greenwich() {
        let new_york = FixedOffset::west_opt(4 * 3600).unwrap();
        let job = job_started_at("2026-09-19T02:00:00Z"); // 18 Sep, 22:00 in New York
        let now = new_york.with_ymd_and_hms(2026, 9, 19, 9, 0, 0).unwrap();
        assert!(
            !started_on_the_same_day(&job, now),
            "a different local day - run it"
        );
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
        assert!(!started_on_the_same_day(
            &Job::default(),
            chrono::Utc::now()
        ));
    }

    /// A timestamp that cannot be read is not evidence of a run today: the
    /// safer answer is "run it" rather than "skip it".
    #[test]
    fn an_unreadable_timestamp_does_not_count_as_a_run() {
        let job = job_started_at("not a date at all");
        assert!(!started_on_the_same_day(&job, chrono::Utc::now()));
    }

    #[test]
    fn a_destination_is_asked_about_by_its_drive() {
        use std::path::PathBuf;
        assert_eq!(
            dest_root(r"F:\Backup\Disk_D (Work)"),
            Some(PathBuf::from(r"F:\"))
        );
        assert_eq!(dest_root("e:"), Some(PathBuf::from(r"E:\")));
        assert_eq!(
            dest_root(r"\\nas\backups\pc1"),
            Some(PathBuf::from(r"\\nas\backups\"))
        );
        assert_eq!(
            dest_root(r"\\nas"),
            None,
            "a server with no share names no drive"
        );
        assert_eq!(dest_root("relative\\path"), None);
        assert_eq!(dest_root(""), None);
    }

    #[test]
    fn the_machine_is_woken_a_minute_before_the_earliest_slot() {
        let tz = FixedOffset::east_opt(3 * 3600).unwrap();
        let now = tz.with_ymd_and_hms(2026, 9, 26, 12, 0, 0).unwrap();
        let job = |time: &str, wake: bool| {
            let mut j = Job::default();
            j.schedule.kind = "daily".into();
            j.schedule.time = time.into();
            j.schedule.wake_to_run = wake;
            j
        };
        // Tonight 23:30 wakes; 18:00 does not ask to.
        let jobs = [job("23:30", true), job("18:00", false)];
        assert_eq!(
            next_wake(&jobs, now),
            Some(tz.with_ymd_and_hms(2026, 9, 26, 23, 29, 0).unwrap())
        );
        // Of two that ask, the earlier — and at 23:45 tonight's 23:30 is gone,
        // so it is tomorrow's, after tomorrow's 03:00.
        let late = tz.with_ymd_and_hms(2026, 9, 26, 23, 45, 0).unwrap();
        let jobs = [job("23:30", true), job("03:00", true)];
        assert_eq!(
            next_wake(&jobs, late),
            Some(tz.with_ymd_and_hms(2026, 9, 27, 2, 59, 0).unwrap())
        );
        // Nobody asks — nothing to arm; an event job never arms it.
        assert_eq!(next_wake(&[job("23:30", false)], now), None);
        let mut ev = job("23:30", true);
        ev.schedule.kind = "event".into();
        assert_eq!(next_wake(&[ev], now), None);
    }
}
