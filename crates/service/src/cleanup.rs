//! `Cleanup` — the window's «Очистка» (PLAN §6б): the catalogue, an analysis
//! of how much each item takes, the cleanup itself, its cancel.
//!
//! The service removes what the items name, for the system and for each
//! chosen user. A window run by an administrator may choose any user of the
//! computer; anyone else only themselves — a cleanup removes files, and
//! another person's are not theirs to remove.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use own_backup_cleanup::{catalog, engine, users, Mode, Report, Roots, User};
use serde::{Deserialize, Serialize};

/// What the window asks.
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Catalog,
    Analyze {
        items: Vec<String>,
        users: Vec<String>,
    },
    Run {
        items: Vec<String>,
        users: Vec<String>,
    },
    Cancel,
}

/// The catalogue as the window draws it.
#[derive(Debug, Serialize)]
pub struct CatalogReply {
    pub groups: Vec<catalog::GroupDto>,
    pub users: Vec<UserDto>,
}

#[derive(Debug, Serialize)]
pub struct UserDto {
    pub sid: String,
    pub name: String,
    /// The person at the window: ticked by default.
    pub me: bool,
    /// Whether this window may clean this user's folders.
    pub allowed: bool,
}

/// Who asks: their SID, and whether they may choose others.
#[derive(Debug, Clone)]
pub struct Asker {
    pub sid: Option<String>,
    pub admin: bool,
}

impl Asker {
    fn may_clean(&self, sid: &str) -> bool {
        self.admin
            || self
                .sid
                .as_deref()
                .is_some_and(|me| me.eq_ignore_ascii_case(sid))
    }
}

/// Progress of a running analysis or cleanup, for the window.
#[derive(Debug, Clone, Serialize)]
pub struct Progress {
    pub mode: &'static str,
    pub item: String,
    #[serde(flatten)]
    pub tally: engine::Tally,
}

/// One cleanup or analysis at a time.
#[derive(Default)]
pub struct Cleaner {
    running: Mutex<Option<Arc<AtomicBool>>>,
}

/// Why a request was refused, for a `Status`.
pub enum Refusal {
    Busy,
    NotAllowed(String),
    Unknown(String),
}

impl Cleaner {
    pub fn catalog(&self, asker: &Asker) -> CatalogReply {
        CatalogReply {
            groups: catalog::groups(),
            users: users::profiles()
                .into_iter()
                .map(|u| UserDto {
                    me: asker
                        .sid
                        .as_deref()
                        .is_some_and(|me| me.eq_ignore_ascii_case(&u.sid)),
                    allowed: asker.may_clean(&u.sid),
                    sid: u.sid,
                    name: u.name,
                })
                .collect(),
        }
    }

    pub fn cancel(&self) {
        if let Some(flag) = self.running.lock().unwrap().as_ref() {
            flag.store(true, Ordering::Relaxed);
        }
    }

    /// The items and users of a request, checked: known items, existing
    /// users the asker may clean.
    pub fn resolve(
        asker: &Asker,
        items: &[String],
        sids: &[String],
    ) -> Result<(Vec<&'static catalog::Item>, Vec<User>), Refusal> {
        let mut chosen = Vec::new();
        for id in items {
            match catalog::item(id) {
                Some(item) => chosen.push(item),
                None => return Err(Refusal::Unknown(id.clone())),
            }
        }
        let all = users::profiles();
        let mut people = Vec::new();
        for sid in sids {
            let Some(user) = all.iter().find(|u| u.sid.eq_ignore_ascii_case(sid)) else {
                return Err(Refusal::Unknown(sid.clone()));
            };
            if !asker.may_clean(&user.sid) {
                return Err(Refusal::NotAllowed(user.name.clone()));
            }
            people.push(user.clone());
        }
        Ok((chosen, people))
    }

    /// Analyse or clean, on this thread (the caller runs it off the async
    /// runtime). `progress` hears of each item.
    pub fn go(
        &self,
        items: &[&catalog::Item],
        people: &[User],
        mode: Mode,
        mut progress: impl FnMut(Progress),
    ) -> Result<Report, Refusal> {
        let flag = {
            let mut slot = self.running.lock().unwrap();
            if slot.is_some() {
                return Err(Refusal::Busy);
            }
            let flag = Arc::new(AtomicBool::new(false));
            *slot = Some(flag.clone());
            flag
        };
        let _free = Free(&self.running);
        let name = match mode {
            Mode::Analyze => "analyze",
            Mode::Remove => "run",
        };
        Ok(engine::run(
            items,
            &Roots::of_this_system(),
            people,
            mode,
            &flag,
            |item, tally| {
                progress(Progress {
                    mode: name,
                    item: item.to_string(),
                    tally: *tally,
                })
            },
        ))
    }
}

/// Frees the slot however the run ends.
struct Free<'a>(&'a Mutex<Option<Arc<AtomicBool>>>);

impl Drop for Free<'_> {
    fn drop(&mut self) {
        *self.0.lock().unwrap() = None;
    }
}

/// The run as «Активность» keeps it.
pub fn activity_record(
    report: &Report,
    started: chrono::DateTime<chrono::Utc>,
) -> own_backup_core::JobRun {
    let finished = chrono::Utc::now();
    own_backup_core::JobRun {
        operation: "cleanup".into(),
        id: uuid::Uuid::new_v4().to_string(),
        started_at: started.to_rfc3339(),
        finished_at: finished.to_rfc3339(),
        duration_secs: (finished - started).num_milliseconds() as f64 / 1000.0,
        status: if report.cancelled {
            "cancelled"
        } else {
            "success"
        }
        .into(),
        cleanup: Some(own_backup_core::CleanupSummary {
            files: report.total.files,
            bytes: report.total.bytes,
            left_files: report.total.left_files,
            left_bytes: report.total.left_bytes,
            items: report
                .items
                .iter()
                .map(|i| catalog::label_both(&i.id))
                .collect(),
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_read_as_the_window_sends_them() {
        let r: Request = serde_json::from_str(r#"{"op":"catalog"}"#).unwrap();
        assert!(matches!(r, Request::Catalog));
        let r: Request = serde_json::from_str(
            r#"{"op":"run","items":["temp_user"],"users":["S-1-5-21-1-1001"]}"#,
        )
        .unwrap();
        assert!(
            matches!(r, Request::Run { items, users } if items == ["temp_user"] && users.len() == 1)
        );
    }

    #[test]
    fn only_an_administrator_cleans_another_users_folders() {
        let me = Asker {
            sid: Some("S-1-5-21-1-1001".into()),
            admin: false,
        };
        assert!(me.may_clean("s-1-5-21-1-1001"));
        assert!(!me.may_clean("S-1-5-21-1-1002"));
        let admin = Asker {
            sid: None,
            admin: true,
        };
        assert!(admin.may_clean("S-1-5-21-1-1002"));
        // Nobody known: nobody's folders.
        let nobody = Asker {
            sid: None,
            admin: false,
        };
        assert!(!nobody.may_clean("S-1-5-21-1-1001"));
    }

    #[test]
    fn an_unknown_item_is_refused_not_ignored() {
        let admin = Asker {
            sid: None,
            admin: true,
        };
        assert!(matches!(
            Cleaner::resolve(&admin, &["no_such".into()], &[]),
            Err(Refusal::Unknown(id)) if id == "no_such"
        ));
    }

    #[test]
    fn the_history_names_the_items_in_both_languages() {
        let report = Report {
            items: vec![engine::ItemReport {
                id: "recycle_bin".into(),
                tally: engine::Tally {
                    files: 3,
                    bytes: 30,
                    left_files: 1,
                    left_bytes: 5,
                },
            }],
            total: engine::Tally {
                files: 3,
                bytes: 30,
                left_files: 1,
                left_bytes: 5,
            },
            cancelled: false,
        };
        let run = activity_record(&report, chrono::Utc::now());
        assert_eq!(run.operation, "cleanup");
        assert_eq!(run.status, "success");
        let summary = run.cleanup.unwrap();
        assert_eq!(
            (summary.files, summary.bytes, summary.left_files),
            (3, 30, 1)
        );
        assert_eq!(summary.items[0].ru, "Корзина");
        assert_eq!(summary.items[0].en, "Recycle Bin");
    }
}
