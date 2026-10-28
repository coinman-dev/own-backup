//! `Cleanup` — the window's «Очистка» (PLAN §6б): the catalogue, an analysis
//! of how much each item takes, the cleanup itself, its cancel.
//!
//! The service removes what the items name, for the system and for each
//! chosen user. An administrator of the computer — elevated or not: the
//! service does the work, as it does on a schedule with no window (owner,
//! 2026-10-28) — may choose any user and Windows' own components; anyone else
//! only their own folders — a cleanup removes files, and another person's
//! are not theirs to remove.
//!
//! An analysis keeps the files it found: the window lists them (`files`, a
//! page at a time — a recycle bin holds hundreds of thousands), and a cleanup
//! of the same items for the same users removes only those (owner,
//! 2026-10-28: what is shown is what goes).

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use own_backup_cleanup::{catalog, engine, users, Listing, Mode, Options, Report, Roots, User};
use serde::{Deserialize, Serialize};

/// What the window asks.
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Catalog,
    Analyze {
        items: Vec<String>,
        users: Vec<String>,
        #[serde(default)]
        options: Options,
    },
    Run {
        items: Vec<String>,
        users: Vec<String>,
        #[serde(default)]
        options: Options,
        /// Files of the analysis' list the window unticked: they stay.
        #[serde(default)]
        exclude: Vec<String>,
    },
    /// A page of the files the last analysis found for one item.
    Files {
        item: String,
        #[serde(default)]
        offset: usize,
        #[serde(default = "page")]
        limit: usize,
        /// `size` (largest first) or `path`.
        #[serde(default)]
        sort: String,
        /// Only paths containing this, case-insensitive.
        #[serde(default)]
        filter: String,
    },
    Cancel,
    /// The automatic cleanup's set ([`crate::auto_cleanup`]).
    Profile,
    /// Save the automatic cleanup's set: the window's ticks and a schedule.
    SaveProfile {
        profile: crate::auto_cleanup::Profile,
    },
}

fn page() -> usize {
    500
}

/// One page of an item's files.
#[derive(Debug, Serialize)]
pub struct FilesReply {
    /// Files matching the filter, and their size.
    pub total: usize,
    pub bytes: u64,
    /// The analysis found more than it lists ([`engine::LIST_CAP`]).
    pub truncated: bool,
    pub files: Vec<FileDto>,
}

#[derive(Debug, Serialize)]
pub struct FileDto {
    pub path: String,
    pub size: u64,
    /// RFC 3339, when known.
    pub modified: Option<String>,
}

/// The last analysis: whose, of what, and what it found.
struct Analysis {
    asker: Option<String>,
    items: Vec<String>,
    users: Vec<String>,
    options: Options,
    /// Files counted per item, listed or not.
    counted: std::collections::HashMap<String, u64>,
    listing: Listing,
}

/// A set of ids in one order and case, to compare two requests.
fn key(ids: &[String]) -> Vec<String> {
    let mut v: Vec<String> = ids.iter().map(|s| s.to_ascii_lowercase()).collect();
    v.sort();
    v.dedup();
    v
}

/// The catalogue as the window draws it.
#[derive(Debug, Serialize)]
pub struct CatalogReply {
    pub groups: Vec<catalog::GroupDto>,
    pub users: Vec<UserDto>,
    /// The window may add folders to the junk search.
    pub admin: bool,
    /// The junk masks and age offered.
    pub junk: own_backup_cleanup::junk::JunkOptions,
    /// Every mask offered, with its kind and what it finds.
    pub masks: Vec<own_backup_cleanup::junk::MaskDto>,
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
    last: Mutex<Option<Analysis>>,
}

/// Why a request was refused, for a `Status`.
#[derive(Debug)]
pub enum Refusal {
    Busy,
    NotAllowed(String),
    Unknown(String),
    /// `files` with no analysis of that item by this window's user.
    NotAnalyzed,
    /// A cleanup of an item cleaned only from its list
    /// ([`catalog::Item::reviewed`]) with no analysis to hold it to.
    ReviewFirst,
    /// A junk mask refused by [`own_backup_cleanup::junk::check_mask`].
    BadMask(String),
    /// Folders added to the junk search by someone who is not an
    /// administrator.
    FoldersAdminOnly,
    /// Windows' own components ([`catalog::Item::admin_only`]) cleaned by
    /// someone who is not an administrator: the item's name.
    AdminOnly(String),
    /// An item that is not cleaned on a schedule (junk by masks): its name.
    NotScheduled(String),
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
            admin: asker.admin,
            junk: Default::default(),
            masks: own_backup_cleanup::junk::offered(),
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

    /// A page of `item`'s files from the last analysis — the asker's own (an
    /// administrator's window may read any).
    pub fn files(
        &self,
        asker: &Asker,
        item: &str,
        offset: usize,
        limit: usize,
        sort: &str,
        filter: &str,
    ) -> Result<FilesReply, Refusal> {
        let last = self.last.lock().unwrap();
        let Some(a) = last.as_ref() else {
            return Err(Refusal::NotAnalyzed);
        };
        let theirs = match (&a.asker, &asker.sid) {
            (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
            (None, None) => true,
            _ => false,
        };
        if !theirs && !asker.admin {
            return Err(Refusal::NotAnalyzed);
        }
        let Some(found) = a.listing.get(item) else {
            return Err(Refusal::NotAnalyzed);
        };
        let needle = filter.to_lowercase();
        let mut picked: Vec<&engine::Found> = found
            .iter()
            .filter(|f| {
                needle.is_empty() || f.path.to_string_lossy().to_lowercase().contains(&needle)
            })
            .collect();
        if sort == "size" {
            picked.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path)));
        } else {
            picked.sort_by(|a, b| a.path.cmp(&b.path));
        }
        let limit = limit.clamp(1, 5000);
        Ok(FilesReply {
            total: picked.len(),
            bytes: picked.iter().map(|f| f.size).sum(),
            truncated: a.counted.get(item).copied().unwrap_or(0) > found.len() as u64,
            files: picked
                .into_iter()
                .skip(offset)
                .take(limit)
                .map(|f| FileDto {
                    path: f.path.display().to_string(),
                    size: f.size,
                    modified: f
                        .modified
                        .map(|t| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339()),
                })
                .collect(),
        })
    }

    /// Analyse or clean, on this thread (the caller runs it off the async
    /// runtime). `progress` hears of each item. An analysis is kept; a
    /// cleanup of exactly what was analysed, for the same asker, removes only
    /// the files it listed.
    #[allow(clippy::too_many_arguments)]
    pub fn go(
        &self,
        asker: &Asker,
        items: &[&catalog::Item],
        people: &[User],
        mode: Mode,
        options: Options,
        exclude: &[String],
        mut progress: impl FnMut(Progress),
    ) -> Result<Report, Refusal> {
        check_junk(asker, &options)?;
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
        let ids: Vec<String> = items.iter().map(|i| i.id.to_string()).collect();
        let sids: Vec<String> = people.iter().map(|u| u.sid.clone()).collect();
        let roots = Roots::of_this_system();
        let progress = |item: &str, tally: &engine::Tally| {
            progress(Progress {
                mode: name,
                item: item.to_string(),
                tally: *tally,
            })
        };
        let report = match mode {
            Mode::Analyze => {
                let mut report = engine::run(
                    items,
                    &roots,
                    people,
                    mode,
                    options.clone(),
                    &flag,
                    progress,
                );
                *self.last.lock().unwrap() = Some(Analysis {
                    asker: asker.sid.clone(),
                    options,
                    items: key(&ids),
                    users: key(&sids),
                    counted: report
                        .items
                        .iter()
                        .map(|i| (i.id.clone(), i.tally.files))
                        .collect(),
                    listing: std::mem::take(&mut report.listing),
                });
                report
            }
            Mode::Remove => {
                let mut held = self.analysed_files(asker, &ids, &sids, &options);
                tracing::info!(
                    held_to_analysis = held.is_some(),
                    unticked = exclude.len(),
                    "cleanup run"
                );
                if let Some(item) = items.iter().find(|i| i.admin_only() && !asker.admin) {
                    return Err(Refusal::AdminOnly(catalog::label(item.id)));
                }
                if held.is_none() && items.iter().any(|i| i.reviewed()) {
                    return Err(Refusal::ReviewFirst);
                }
                if let Some(only) = held.as_mut() {
                    for path in exclude {
                        only.remove(&PathBuf::from(path));
                    }
                }
                let report = match &held {
                    Some(only) => {
                        engine::run_only(items, &roots, people, options, &flag, progress, only)
                    }
                    None => engine::run(items, &roots, people, mode, options, &flag, progress),
                };
                // What was analysed is gone or left now: a second cleanup
                // walks again.
                *self.last.lock().unwrap() = None;
                report
            }
        };
        Ok(report)
    }

    /// The files the last analysis listed, when it was of exactly these
    /// items and users and asked by the same person.
    fn analysed_files(
        &self,
        asker: &Asker,
        ids: &[String],
        sids: &[String],
        options: &Options,
    ) -> Option<HashSet<PathBuf>> {
        let last = self.last.lock().unwrap();
        let a = last.as_ref()?;
        let same_asker = match (&a.asker, &asker.sid) {
            (Some(x), Some(y)) => x.eq_ignore_ascii_case(y),
            (None, None) => true,
            _ => false,
        };
        // The window analyses every item and cleans the ticked ones: held when
        // each of them was analysed, for the same users and options.
        let wanted = key(ids);
        if !same_asker
            || !wanted.iter().all(|id| a.items.contains(id))
            || a.users != key(sids)
            || a.options != *options
        {
            return None;
        }
        Some(
            a.listing
                .iter()
                .filter(|(id, _)| wanted.contains(&id.to_ascii_lowercase()))
                .flat_map(|(_, files)| files)
                .map(|f| f.path.clone())
                .collect(),
        )
    }
}

/// The junk search's masks and folders, checked: masks that mean junk only,
/// folders added by an administrator and named in full.
fn check_junk(asker: &Asker, options: &Options) -> Result<(), Refusal> {
    let junk = &options.junk;
    if let Some(bad) = junk
        .masks
        .iter()
        .find(|m| own_backup_cleanup::junk::check_mask(m).is_err())
    {
        return Err(Refusal::BadMask(bad.clone()));
    }
    if !junk.folders.is_empty() && !asker.admin {
        return Err(Refusal::FoldersAdminOnly);
    }
    if let Some(odd) = junk.folders.iter().find(|f| !f.is_absolute()) {
        return Err(Refusal::Unknown(odd.display().to_string()));
    }
    Ok(())
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
    trigger: &str,
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
            trigger: trigger.into(),
            files: report.total.files,
            bytes: report.total.bytes,
            left_files: report.total.left_files,
            left_bytes: report.total.left_bytes,
            entries: report.total.entries,
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
            matches!(r, Request::Run { items, users, .. } if items == ["temp_user"] && users.len() == 1)
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

    fn analysed(asker: &str) -> Cleaner {
        let found = |p: &str, size: u64| engine::Found {
            path: PathBuf::from(p),
            size,
            modified: None,
        };
        let cleaner = Cleaner::default();
        *cleaner.last.lock().unwrap() = Some(Analysis {
            asker: Some(asker.into()),
            options: Options::default(),
            items: key(&["temp_user".into(), "recycle_bin".into()]),
            users: key(&["S-1-5-21-1-1001".into()]),
            counted: [("temp_user".to_string(), 3), ("recycle_bin".to_string(), 1)].into(),
            listing: [
                (
                    "temp_user".to_string(),
                    vec![
                        found(r"C:\Users\u\AppData\Local\Temp\b.log", 10),
                        found(r"C:\Users\u\AppData\Local\Temp\_MEI1\python.dll", 3000),
                        found(r"C:\Users\u\AppData\Local\Temp\a.tmp", 200),
                    ],
                ),
                (
                    "recycle_bin".to_string(),
                    vec![found(r"C:\$Recycle.Bin\S-1-5-21-1-1001\$Rx.txt", 7)],
                ),
            ]
            .into(),
        });
        cleaner
    }

    fn me() -> Asker {
        Asker {
            sid: Some("S-1-5-21-1-1001".into()),
            admin: false,
        }
    }

    /// A cleanup of analysed items is held to their list — the ticked ones
    /// out of all the window analysed; of anything else — an item not
    /// analysed, other users, other options, another person's analysis — it
    /// walks anew.
    #[test]
    fn a_cleanup_is_held_to_the_analysis_of_the_same_request() {
        let c = analysed("S-1-5-21-1-1001");
        let items = ["recycle_bin".to_string(), "TEMP_USER".to_string()];
        let users = ["s-1-5-21-1-1001".to_string()];
        let held = c
            .analysed_files(&me(), &items, &users, &Options::default())
            .unwrap();
        assert_eq!(held.len(), 4);
        let only_bin = c
            .analysed_files(&me(), &items[..1], &users, &Options::default())
            .unwrap();
        assert_eq!(only_bin.len(), 1, "only the ticked item's files");
        assert!(c
            .analysed_files(&me(), &["shader_cache".into()], &users, &Options::default())
            .is_none());
        assert!(c
            .analysed_files(
                &me(),
                &items,
                &users,
                &Options {
                    history_keep_days: 7,
                    ..Default::default()
                }
            )
            .is_none());
        assert!(c
            .analysed_files(
                &me(),
                &items,
                &["S-1-5-21-1-1002".into()],
                &Options::default()
            )
            .is_none());
        let other = Asker {
            sid: Some("S-1-5-21-1-1002".into()),
            admin: false,
        };
        assert!(c
            .analysed_files(&other, &items, &users, &Options::default())
            .is_none());
    }

    #[test]
    fn the_files_are_listed_a_page_at_a_time_largest_first_or_by_path() {
        let c = analysed("S-1-5-21-1-1001");
        let page = c.files(&me(), "temp_user", 0, 2, "size", "").unwrap();
        assert_eq!(page.total, 3);
        assert_eq!(page.bytes, 3210);
        assert!(!page.truncated);
        assert_eq!(page.files.len(), 2);
        assert!(page.files[0].path.ends_with("python.dll"));
        assert!(page.files[1].path.ends_with("a.tmp"));
        let rest = c.files(&me(), "temp_user", 2, 2, "size", "").unwrap();
        assert_eq!(rest.files.len(), 1);
        let by_path = c.files(&me(), "temp_user", 0, 10, "path", "").unwrap();
        assert!(by_path.files[0].path.ends_with("_MEI1\\python.dll"));
        let filtered = c.files(&me(), "temp_user", 0, 10, "path", "MEI").unwrap();
        assert_eq!((filtered.total, filtered.bytes), (1, 3000));
        // Not analysed, or someone else's analysis.
        assert!(matches!(
            c.files(&me(), "shader_cache", 0, 10, "", ""),
            Err(Refusal::NotAnalyzed)
        ));
        let other = Asker {
            sid: Some("S-1-5-21-1-1002".into()),
            admin: false,
        };
        assert!(matches!(
            c.files(&other, "temp_user", 0, 10, "", ""),
            Err(Refusal::NotAnalyzed)
        ));
        assert!(Cleaner::default()
            .files(&me(), "temp_user", 0, 10, "", "")
            .is_err());
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
                    records: 0,
                    entries: 2,
                },
                running: vec![],
            }],
            total: engine::Tally {
                files: 3,
                bytes: 30,
                left_files: 1,
                left_bytes: 5,
                records: 0,
                entries: 2,
            },
            cancelled: false,
            listing: Default::default(),
        };
        let run = activity_record(&report, chrono::Utc::now(), "schedule");
        assert_eq!(run.operation, "cleanup");
        assert_eq!(run.status, "success");
        let summary = run.cleanup.unwrap();
        assert_eq!(summary.trigger, "schedule");
        assert_eq!(
            (
                summary.files,
                summary.bytes,
                summary.left_files,
                summary.entries
            ),
            (3, 30, 1, 2)
        );
        assert_eq!(summary.items[0].ru, "Корзина");
        assert_eq!(summary.items[0].en, "Recycle Bin");
    }

    /// The junk search takes masks that mean junk only, and other folders
    /// from an administrator's window only.
    #[test]
    fn junk_masks_and_folders_are_checked() {
        use own_backup_cleanup::junk::JunkOptions;
        let with = |junk: JunkOptions| Options {
            junk,
            ..Default::default()
        };
        let admin = Asker {
            sid: Some("S-1-5-21-1-1001".into()),
            admin: true,
        };
        assert!(check_junk(&me(), &Options::default()).is_ok());
        assert!(matches!(
            check_junk(
                &me(),
                &with(JunkOptions {
                    masks: vec!["*.tmp".into(), "*.*".into()],
                    ..Default::default()
                })
            ),
            Err(Refusal::BadMask(m)) if m == "*.*"
        ));
        let folders = with(JunkOptions {
            folders: vec![PathBuf::from(if cfg!(windows) {
                r"D:\Downloads"
            } else {
                "/d"
            })],
            ..Default::default()
        });
        assert!(matches!(
            check_junk(&me(), &folders),
            Err(Refusal::FoldersAdminOnly)
        ));
        assert!(check_junk(&admin, &folders).is_ok());
        let relative = with(JunkOptions {
            folders: vec![PathBuf::from("Downloads")],
            ..Default::default()
        });
        assert!(matches!(
            check_junk(&admin, &relative),
            Err(Refusal::Unknown(_))
        ));
    }
}
