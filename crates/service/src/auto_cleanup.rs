//! Automatic disk cleanup (PLAN §6б, stage 7): the window's «Очистка» saves a
//! set — items, users, their options, a schedule — and the service cleans by
//! it on its own: on the schedule, and before the copy of a job that asks
//! (`AdvancedConfig::cleanup_before`). No window is needed for either.
//!
//! The set is the service's file (`cleanup-auto.json` in its data folder),
//! written whole by the window through `Cleanup` (`save_profile`). Whoever
//! saved it is remembered with what they may clean; each run checks the set
//! against that again, as a request from them would be. Junk by masks is not
//! part of it: those files go only from a list someone looked through
//! ([`own_backup_cleanup::catalog::Item::reviewed`]).

// The schedule runs on Windows only: the cleanup is Windows' for now.
#![cfg_attr(not(windows), allow(dead_code))]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use own_backup_cleanup::{catalog, Mode, Options};
use own_backup_core::{JobStore, ScheduleConfig};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::cleanup::{activity_record, Asker, Cleaner, Refusal};
use crate::grpc_server::proto::Event;

const FILE: &str = "cleanup-auto.json";
const TICK: std::time::Duration = std::time::Duration::from_secs(60);

/// What the automatic cleanup cleans and when.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    /// `none`, `daily`, `weekly`, `monthly` — as a job's.
    pub schedule: ScheduleConfig,
    pub items: Vec<String>,
    /// SIDs.
    pub users: Vec<String>,
    pub options: Options,
    /// Who saved it, and whether they were an administrator of the computer
    /// then. Kept by the service: whatever the window sends is ignored.
    pub owner: Option<String>,
    pub admin: bool,
    /// When the schedule last ran it (RFC 3339).
    pub last_run: Option<String>,
}

/// What starts a run, as «Активность» names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Schedule,
    BeforeBackup,
}

impl Trigger {
    fn code(self) -> &'static str {
        match self {
            Trigger::Schedule => "schedule",
            Trigger::BeforeBackup => "before_backup",
        }
    }
}

pub struct AutoCleanup {
    pub cleaner: Arc<Cleaner>,
    store: Arc<JobStore>,
    events: broadcast::Sender<Event>,
    file: PathBuf,
    /// One writer of the file at a time.
    lock: Mutex<()>,
}

impl AutoCleanup {
    pub fn new(
        data_dir: &std::path::Path,
        store: Arc<JobStore>,
        events: broadcast::Sender<Event>,
    ) -> Self {
        Self {
            cleaner: Arc::default(),
            store,
            events,
            file: data_dir.join(FILE),
            lock: Mutex::new(()),
        }
    }

    pub fn profile(&self) -> Profile {
        std::fs::read(&self.file)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn write(&self, profile: &Profile) -> std::io::Result<()> {
        let tmp = self.file.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(profile).map_err(std::io::Error::other)?;
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &self.file)
    }

    /// Save the window's set, checked as a cleanup by `asker` would be.
    pub fn save(&self, asker: &Asker, mut wanted: Profile) -> Result<Profile, Refusal> {
        if let Some(id) = wanted
            .items
            .iter()
            .find(|id| catalog::item(id).is_some_and(|i| i.reviewed()))
        {
            return Err(Refusal::NotScheduled(catalog::label(id)));
        }
        let (items, _) = Cleaner::resolve(asker, &wanted.items, &wanted.users)?;
        if let Some(i) = items.iter().find(|i| i.admin_only() && !asker.admin) {
            return Err(Refusal::AdminOnly(catalog::label(i.id)));
        }
        if !matches!(
            wanted.schedule.kind.as_str(),
            "none" | "daily" | "weekly" | "monthly"
        ) || (wanted.schedule.kind != "none"
            && own_backup_scheduler::schedule_to_cron(&wanted.schedule).is_none())
        {
            return Err(Refusal::Unknown(wanted.schedule.kind.clone()));
        }
        let _one = self.lock.lock().unwrap();
        let before = self.profile();
        wanted.owner = asker.sid.clone();
        wanted.admin = asker.admin;
        // A new schedule counts from now: no catching up on slots it never had.
        wanted.last_run = before
            .last_run
            .or_else(|| Some(chrono::Local::now().fixed_offset().to_rfc3339()));
        self.write(&wanted)
            .map_err(|e| Refusal::Unknown(e.to_string()))?;
        info!(
            items = wanted.items.len(),
            users = wanted.users.len(),
            schedule = %wanted.schedule.kind,
            "automatic cleanup set saved"
        );
        Ok(wanted)
    }

    /// Clean by the set, on this thread, and record it in the activity.
    /// `job`: the job whose copy waits for it.
    pub fn run(&self, trigger: Trigger, job: Option<&own_backup_core::Job>) -> Result<(), Refusal> {
        let p = self.profile();
        let ids: Vec<String> = p
            .items
            .iter()
            .filter(|id| catalog::item(id).is_some_and(|i| !i.reviewed()))
            .cloned()
            .collect();
        if ids.is_empty() || p.users.is_empty() && ids.iter().all(|id| per_user(id)) {
            info!(
                trigger = trigger.code(),
                "automatic cleanup: nothing chosen"
            );
            return Ok(());
        }
        let asker = Asker {
            sid: p.owner.clone(),
            admin: p.admin,
        };
        let (items, people) = Cleaner::resolve(&asker, &ids, &p.users)?;
        let started = chrono::Utc::now();
        let mut last: Option<std::time::Instant> = None;
        let events = self.events.clone();
        let report = self.cleaner.go(
            &asker,
            &items,
            &people,
            Mode::Remove,
            p.options.clone(),
            &[],
            |progress| {
                if last.is_none_or(|t| t.elapsed().as_millis() >= 200) {
                    last = Some(std::time::Instant::now());
                    let _ = events.send(Event {
                        kind: "cleanup".into(),
                        json: serde_json::to_string(&progress).unwrap_or_default(),
                    });
                }
            },
        )?;
        let mut run = activity_record(&report, started, trigger.code());
        if let Some(job) = job {
            run.job_id = job.id.clone();
            run.job_name = job.name.clone();
        }
        info!(
            trigger = trigger.code(),
            files = report.total.files,
            bytes = report.total.bytes,
            left = report.total.left_files,
            "automatic cleanup finished"
        );
        if let Err(e) = self.store.record_run(run.clone()) {
            tracing::error!(error = %e, "the cleanup could not be recorded in the activity");
        }
        run.in_language(own_backup_common::lang::code());
        let _ = self.events.send(Event {
            kind: "finished".into(),
            json: serde_json::to_string(&run).unwrap_or_default(),
        });
        Ok(())
    }

    /// Run the set if its schedule has come due (`uptime`: since the service
    /// started, for the catch-up delay). A cleanup already going on (the
    /// window's) leaves the slot for the next minute.
    fn tick(&self, uptime: std::time::Duration) {
        let p = self.profile();
        if p.schedule.kind == "none" || p.items.is_empty() {
            return;
        }
        let Some(cron) = own_backup_scheduler::schedule_to_cron(&p.schedule) else {
            return;
        };
        let Some(sched) = own_backup_scheduler::config_schedule::parse(&cron) else {
            return;
        };
        let now = chrono::Local::now().fixed_offset();
        let last = p
            .last_run
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok());
        let grace =
            uptime >= std::time::Duration::from_secs(u64::from(p.schedule.missed_delay_min) * 60);
        let fresh = chrono::Duration::seconds(2 * TICK.as_secs() as i64);
        if !own_backup_scheduler::is_due(&sched, now, last, p.schedule.run_missed && grace, fresh) {
            return;
        }
        match self.run(Trigger::Schedule, None) {
            Err(Refusal::Busy) => {
                info!("automatic cleanup: another cleanup is going on; next minute");
                return;
            }
            Err(e) => warn!(?e, "automatic cleanup refused"),
            Ok(()) => {}
        }
        let _one = self.lock.lock().unwrap();
        let mut p = self.profile();
        p.last_run = Some(now.to_rfc3339());
        if let Err(e) = self.write(&p) {
            warn!(error = %e, "automatic cleanup: its last run could not be saved");
        }
    }
}

fn per_user(id: &str) -> bool {
    catalog::item(id).is_some_and(|i| i.per_user())
}

/// The schedule's minute tick; stops on shutdown.
pub fn spawn(auto: Arc<AutoCleanup>, mut shutdown: crate::shutdown::ShutdownSignal) {
    let started = std::time::Instant::now();
    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(TICK);
        loop {
            tokio::select! {
                _ = ticks.tick() => {}
                _ = shutdown.wait_for_shutdown() => break,
            }
            let auto = auto.clone();
            let uptime = started.elapsed();
            let _ = tokio::task::spawn_blocking(move || auto.tick(uptime)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auto(dir: &std::path::Path) -> AutoCleanup {
        let store = JobStore::try_load(dir).unwrap();
        let (events, _) = broadcast::channel(4);
        AutoCleanup::new(dir, store, events)
    }

    /// The set is kept as saved, with who saved it; the window cannot name
    /// the owner, and a schedule the service cannot read is refused.
    #[test]
    fn the_set_is_saved_with_its_owner_and_a_bad_schedule_refused() {
        let dir = tempfile::tempdir().unwrap();
        let a = auto(dir.path());
        assert_eq!(a.profile().schedule.kind, "none", "nothing saved yet");
        let me = Asker {
            sid: Some("S-1-5-21-1-1001".into()),
            admin: true,
        };
        let mut wanted = Profile::default();
        wanted.schedule.kind = "daily".into();
        wanted.schedule.time = "03:30".into();
        wanted.owner = Some("S-1-5-18".into());
        let saved = a.save(&me, wanted.clone()).unwrap();
        assert_eq!(saved.owner.as_deref(), Some("S-1-5-21-1-1001"));
        assert!(saved.admin);
        assert!(saved.last_run.is_some(), "a new schedule counts from now");
        let read = a.profile();
        assert_eq!(read.schedule.time, "03:30");
        assert_eq!(read.owner, saved.owner);

        wanted.schedule.kind = "hourly".into();
        assert!(matches!(
            a.save(&me, wanted.clone()),
            Err(Refusal::Unknown(_))
        ));
        wanted.schedule.kind = "daily".into();
        wanted.schedule.time = "25:99".into();
        assert!(matches!(a.save(&me, wanted), Err(Refusal::Unknown(_))));
        assert_eq!(a.profile().schedule.time, "03:30", "the old set stays");
    }

    /// An empty set runs nothing and records nothing.
    #[test]
    fn an_empty_set_cleans_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let a = auto(dir.path());
        assert!(a.run(Trigger::Schedule, None).is_ok());
        assert!(a.store.activity().is_empty());
    }
}
