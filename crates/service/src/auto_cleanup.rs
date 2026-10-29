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
//!
//! Profiles (`cleanup-profiles.json`): sets saved under a name from the page
//! («Сохранить профиль как…»). A job chooses one for its cleanup before the
//! copy (`AdvancedConfig::cleanup_profile`); a job that names none cleans by
//! the automatic set, as before profiles. Each is kept with who saved it and
//! checked against that again on every run.

// The schedule runs on Windows and Linux.
#![cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]

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
const PROFILES: &str = "cleanup-profiles.json";
/// A profile's name, in characters.
const NAME_MAX: usize = 80;
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

/// A set saved under a name: what a job's cleanup before the copy cleans.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Named {
    /// Kept by the service: a new profile gets one, a profile saved again
    /// under the same name keeps its own (the jobs name it by this).
    pub id: String,
    pub name: String,
    pub items: Vec<String>,
    /// SIDs (uids on Linux).
    pub users: Vec<String>,
    pub options: Options,
    /// As [`Profile::owner`] and [`Profile::admin`]: the service's.
    pub owner: Option<String>,
    pub admin: bool,
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
    profiles_file: PathBuf,
    /// One writer of the files at a time.
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
            profiles_file: data_dir.join(PROFILES),
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
        write_json(&self.file, profile)
    }

    /// The saved profiles, in the order they were first saved.
    pub fn profiles(&self) -> Vec<Named> {
        std::fs::read(&self.profiles_file)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// Save the window's set under a name, checked as a cleanup by `asker`
    /// would be. The same name (in any case) replaces that profile, if it is
    /// `asker`'s or `asker` is an administrator.
    pub fn save_named(&self, asker: &Asker, mut wanted: Named) -> Result<Named, Refusal> {
        wanted.name = wanted.name.trim().to_string();
        if wanted.name.is_empty() || wanted.name.chars().count() > NAME_MAX {
            return Err(Refusal::BadName);
        }
        check_set(asker, &wanted.items, &wanted.users)?;
        let _one = self.lock.lock().unwrap();
        let mut all = self.profiles();
        let same = all
            .iter()
            .position(|p| p.name.to_lowercase() == wanted.name.to_lowercase());
        wanted.owner = asker.sid.clone();
        wanted.admin = asker.admin;
        match same {
            Some(at) => {
                if !may_change(asker, &all[at]) {
                    return Err(Refusal::NotYours(all[at].name.clone()));
                }
                wanted.id = all[at].id.clone();
                all[at] = wanted.clone();
            }
            None => {
                wanted.id = uuid::Uuid::new_v4().to_string();
                all.push(wanted.clone());
            }
        }
        write_json(&self.profiles_file, &all).map_err(|e| Refusal::Unknown(e.to_string()))?;
        info!(
            name = %wanted.name,
            items = wanted.items.len(),
            users = wanted.users.len(),
            "cleanup profile saved"
        );
        Ok(wanted)
    }

    /// Delete a profile: its owner's or an administrator's, and only one no
    /// job cleans by.
    pub fn delete_named(&self, asker: &Asker, id: &str) -> Result<(), Refusal> {
        let _one = self.lock.lock().unwrap();
        let mut all = self.profiles();
        let Some(at) = all.iter().position(|p| p.id == id) else {
            return Ok(());
        };
        if !may_change(asker, &all[at]) {
            return Err(Refusal::NotYours(all[at].name.clone()));
        }
        let jobs: Vec<String> = self
            .store
            .list()
            .into_iter()
            .filter(|j| j.advanced.cleanup_profile == id)
            .map(|j| j.name)
            .collect();
        if !jobs.is_empty() {
            return Err(Refusal::ProfileInUse(jobs.join(", ")));
        }
        let gone = all.remove(at);
        write_json(&self.profiles_file, &all).map_err(|e| Refusal::Unknown(e.to_string()))?;
        info!(name = %gone.name, "cleanup profile deleted");
        Ok(())
    }

    /// Save the window's set, checked as a cleanup by `asker` would be.
    pub fn save(&self, asker: &Asker, mut wanted: Profile) -> Result<Profile, Refusal> {
        check_set(asker, &wanted.items, &wanted.users)?;
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
    /// `job`: the job whose copy waits for it; it cleans by the profile it
    /// names, or by the automatic set if it names none.
    pub fn run(&self, trigger: Trigger, job: Option<&own_backup_core::Job>) -> Result<(), Refusal> {
        let chosen = job.map_or("", |j| j.advanced.cleanup_profile.as_str());
        let p = if chosen.is_empty() {
            let p = self.profile();
            Named {
                items: p.items,
                users: p.users,
                options: p.options,
                owner: p.owner,
                admin: p.admin,
                ..Named::default()
            }
        } else {
            self.profiles()
                .into_iter()
                .find(|p| p.id == chosen)
                .ok_or(Refusal::NoProfile)?
        };
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
            profile = %p.name,
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

/// What a set may hold, for `asker`: items cleaned on their own (not junk by
/// masks), users `asker` may clean, Windows' components only for an
/// administrator.
fn check_set(asker: &Asker, items: &[String], users: &[String]) -> Result<(), Refusal> {
    if let Some(id) = items
        .iter()
        .find(|id| catalog::item(id).is_some_and(|i| i.reviewed()))
    {
        return Err(Refusal::NotScheduled(catalog::label(id)));
    }
    let (items, _) = Cleaner::resolve(asker, items, users)?;
    if let Some(i) = items.iter().find(|i| i.admin_only() && !asker.admin) {
        return Err(Refusal::AdminOnly(catalog::label(i.id)));
    }
    Ok(())
}

/// A profile is changed by whoever saved it or by an administrator.
fn may_change(asker: &Asker, p: &Named) -> bool {
    asker.admin || p.owner.is_none() || p.owner == asker.sid
}

fn write_json<T: Serialize>(file: &std::path::Path, value: &T) -> std::io::Result<()> {
    let tmp = file.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, file)
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

    /// A profile saved again under its name (in another case) keeps its id;
    /// another user may not replace it; one a job cleans by is not deleted.
    #[test]
    fn profiles_are_saved_by_name_and_kept_while_a_job_uses_them() {
        let dir = tempfile::tempdir().unwrap();
        let a = auto(dir.path());
        let asker = |sid: &str, admin| Asker {
            sid: Some(sid.into()),
            admin,
        };
        let (me, other, admin) = (
            asker("S-1-5-21-1-1001", false),
            asker("S-1-5-21-1-1002", false),
            asker("S-1-5-21-1-500", true),
        );
        let named = |name: &str| Named {
            name: name.into(),
            ..Named::default()
        };
        assert!(matches!(
            a.save_named(&me, named("  ")),
            Err(Refusal::BadName)
        ));
        let first = a.save_named(&me, named(" Перед копией ")).unwrap();
        assert_eq!(first.name, "Перед копией");
        assert_eq!(first.owner, me.sid);
        assert!(!first.id.is_empty());
        let again = a.save_named(&me, named("перед КОПИЕЙ")).unwrap();
        assert_eq!(again.id, first.id, "the same name keeps its id");
        assert_eq!(a.profiles().len(), 1);
        assert!(matches!(
            a.save_named(&other, named("Перед копией")),
            Err(Refusal::NotYours(_))
        ));
        assert!(matches!(
            a.delete_named(&other, &first.id),
            Err(Refusal::NotYours(_))
        ));

        let mut job = own_backup_core::Job {
            name: "Диск C".into(),
            ..Default::default()
        };
        job.advanced.cleanup_profile = first.id.clone();
        let job = a.store.upsert(job).unwrap();
        assert!(matches!(
            a.delete_named(&admin, &first.id),
            Err(Refusal::ProfileInUse(names)) if names == "Диск C"
        ));
        a.store.delete(&job.id).unwrap();
        a.delete_named(&admin, &first.id).unwrap();
        assert!(a.profiles().is_empty());

        // A job whose profile is gone cleans nothing and says so.
        assert!(matches!(
            a.run(Trigger::BeforeBackup, Some(&job)),
            Err(Refusal::NoProfile)
        ));
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
