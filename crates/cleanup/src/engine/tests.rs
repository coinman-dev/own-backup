use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime};

use super::*;
use crate::catalog::{Base, Item, Target};

const fn t(base: Base, path: &'static str, recursive: bool, age: u32) -> Target {
    Target {
        base,
        path,
        masks: &[],
        recursive,
        min_age_hours: age,
        keep: &[],
        programs: &[],
        trim: None,
    }
}

fn write(p: &Path, bytes: usize) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, vec![7u8; bytes]).unwrap();
}

fn roots(dir: &Path) -> Roots {
    Roots {
        windows: dir.join("Windows"),
        program_data: dir.join("ProgramData"),
        volumes: vec![dir.join("C"), dir.join("D")],
        running: Default::default(),
        hives: fake_hives,
    }
}

fn user(dir: &Path) -> User {
    User {
        sid: "S-1-5-21-1-2-3-1001".into(),
        name: "u".into(),
        profile: dir.join("Users").join("u"),
    }
}

/// Analysis or removal of one item, `later` after now.
fn go(item: &Item, dir: &Path, mode: Mode, later: Duration) -> Report {
    run_at(
        &[item],
        &roots(dir),
        &[user(dir)],
        mode,
        &AtomicBool::new(false),
        |_, _| {},
        SystemTime::now() + later,
    )
}

const TWO_DAYS: Duration = Duration::from_secs(48 * 3600);

static TEMP: Item = Item {
    id: "temp_user",
    group: "temp",
    default_on: true,
    keys: &[],
    targets: &[t(Base::LocalAppData, "Temp", true, 24)],
};

#[test]
fn the_analysis_counts_what_the_removal_then_removes() {
    let dir = tempfile::tempdir().unwrap();
    let temp = dir.path().join(r"Users/u/AppData/Local/Temp");
    write(&temp.join("a.tmp"), 100);
    write(&temp.join("_MEI1234/python.dll"), 1000);
    write(&temp.join("_MEI1234/lib/x.pyd"), 10);

    let seen = go(&TEMP, dir.path(), Mode::Analyze, TWO_DAYS);
    assert_eq!(seen.total.files, 3);
    assert_eq!(seen.total.bytes, 1110);
    assert!(temp.join("a.tmp").exists(), "analysis removes nothing");

    let done = go(&TEMP, dir.path(), Mode::Remove, TWO_DAYS);
    assert_eq!(done.total, seen.total);
    assert!(!temp.join("_MEI1234").exists(), "an emptied folder goes");
    assert!(temp.is_dir(), "the target folder itself stays");
    assert_eq!(fs::read_dir(&temp).unwrap().count(), 0);
}

/// What a running program wrote today is not touched — nor the folder it is
/// in (another cleaner removed a live session's files minutes after they
/// were written, 2026-10-28).
#[test]
fn files_younger_than_the_item_allows_stay() {
    let dir = tempfile::tempdir().unwrap();
    let temp = dir.path().join(r"Users/u/AppData/Local/Temp");
    write(&temp.join("session/output.txt"), 5);

    let done = go(&TEMP, dir.path(), Mode::Remove, Duration::ZERO);
    assert_eq!(done.total.files, 0);
    assert!(temp.join("session/output.txt").exists());

    // An empty folder made just now stays as well: something is about to
    // write into it.
    fs::create_dir_all(temp.join("fresh")).unwrap();
    go(&TEMP, dir.path(), Mode::Remove, Duration::ZERO);
    assert!(temp.join("fresh").is_dir());
}

#[cfg(unix)]
#[test]
fn links_are_neither_followed_nor_removed() {
    let dir = tempfile::tempdir().unwrap();
    let temp = dir.path().join(r"Users/u/AppData/Local/Temp");
    let outside = dir.path().join("documents");
    write(&outside.join("work.odt"), 50);
    write(&temp.join("junk.tmp"), 1);
    std::os::unix::fs::symlink(&outside, temp.join("docs")).unwrap();
    std::os::unix::fs::symlink(outside.join("work.odt"), temp.join("work-link")).unwrap();

    let done = go(&TEMP, dir.path(), Mode::Remove, TWO_DAYS);
    assert_eq!(done.total.files, 1, "only junk.tmp");
    assert!(outside.join("work.odt").exists());
    assert!(fs::symlink_metadata(temp.join("docs")).is_ok());
    assert!(fs::symlink_metadata(temp.join("work-link")).is_ok());

    // A target folder that is itself a link is somebody else's folder.
    let other = dir.path().join("other");
    write(&other.join("x"), 1);
    let users_temp_parent = dir.path().join(r"Users/v/AppData/Local");
    fs::create_dir_all(&users_temp_parent).unwrap();
    std::os::unix::fs::symlink(&other, users_temp_parent.join("Temp")).unwrap();
    let v = User {
        sid: "S-1-5-21-1-2-3-1002".into(),
        name: "v".into(),
        profile: dir.path().join("Users/v"),
    };
    let report = run_at(
        &[&TEMP],
        &roots(dir.path()),
        &[v],
        Mode::Remove,
        &AtomicBool::new(false),
        |_, _| {},
        SystemTime::now() + TWO_DAYS,
    );
    assert_eq!(report.total.files, 0);
    assert!(other.join("x").exists());
}

#[test]
fn masks_and_flat_targets_take_only_their_files() {
    static DUMPS: Item = Item {
        id: "crash_dumps",
        group: "reports",
        default_on: true,
        keys: &[],
        targets: &[
            Target {
                base: Base::Windows,
                path: "",
                masks: &["MEMORY.DMP"],
                recursive: false,
                min_age_hours: 0,
                keep: &[],
                programs: &[],
                trim: None,
            },
            Target {
                base: Base::Windows,
                path: "Minidump",
                masks: &["*.dmp"],
                recursive: false,
                min_age_hours: 0,
                keep: &[],
                programs: &[],
                trim: None,
            },
        ],
    };
    let dir = tempfile::tempdir().unwrap();
    let win = dir.path().join("Windows");
    write(&win.join("memory.dmp"), 30);
    write(&win.join("explorer.exe"), 1);
    write(&win.join("System32/MEMORY.DMP"), 1);
    write(&win.join("Minidump/102826-01.dmp"), 3);
    write(&win.join("Minidump/readme.txt"), 1);

    let done = go(&DUMPS, dir.path(), Mode::Remove, Duration::ZERO);
    assert_eq!(done.total.files, 2);
    assert_eq!(done.total.bytes, 33);
    assert!(win.join("explorer.exe").exists());
    assert!(
        win.join("System32/MEMORY.DMP").exists(),
        "not below a flat target"
    );
    assert!(win.join("Minidump/readme.txt").exists());
}

#[test]
fn the_recycle_bin_is_emptied_on_every_volume_but_its_desktop_ini() {
    static BIN: Item = Item {
        id: "recycle_bin",
        group: "temp",
        default_on: true,
        keys: &[],
        targets: &[Target {
            base: Base::RecycleBin,
            path: "",
            masks: &[],
            recursive: true,
            min_age_hours: 0,
            keep: &["desktop.ini"],
            programs: &[],
            trim: None,
        }],
    };
    let dir = tempfile::tempdir().unwrap();
    let sid = "S-1-5-21-1-2-3-1001";
    for v in ["C", "D"] {
        let bin = dir.path().join(v).join("$Recycle.Bin").join(sid);
        write(&bin.join("desktop.ini"), 1);
        write(&bin.join("$IABC.txt"), 2);
        write(&bin.join("$RABC.txt"), 20);
        write(&bin.join("$RDIR/inside.bin"), 5);
    }
    // Someone else's bin is not ours.
    write(&dir.path().join("C/$Recycle.Bin/S-1-5-21-9/$Rx"), 9);

    let done = go(&BIN, dir.path(), Mode::Remove, Duration::ZERO);
    assert_eq!(done.total.files, 6);
    for v in ["C", "D"] {
        let bin = dir.path().join(v).join("$Recycle.Bin").join(sid);
        assert!(bin.join("desktop.ini").exists());
        assert_eq!(fs::read_dir(&bin).unwrap().count(), 1);
    }
    assert!(dir.path().join("C/$Recycle.Bin/S-1-5-21-9/$Rx").exists());
}

#[test]
fn a_missing_target_is_nothing_to_clean() {
    let dir = tempfile::tempdir().unwrap();
    let report = go(&TEMP, dir.path(), Mode::Remove, TWO_DAYS);
    assert_eq!(report.total, Tally::default());
    assert!(!report.cancelled);
}

#[test]
fn a_cancelled_run_stops_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join(r"Users/u/AppData/Local/Temp/a"), 1);
    let report = run_at(
        &[&TEMP, &TEMP],
        &roots(dir.path()),
        &[user(dir.path())],
        Mode::Remove,
        &AtomicBool::new(true),
        |_, _| {},
        SystemTime::now() + TWO_DAYS,
    );
    assert!(report.cancelled);
    assert_eq!(report.items.len(), 1, "no item after the cancel");
    assert!(dir.path().join(r"Users/u/AppData/Local/Temp/a").exists());
}

#[test]
fn a_read_only_file_is_removed_too() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join(r"Users/u/AppData/Local/Temp/ro.tmp");
    write(&f, 4);
    let mut p = fs::metadata(&f).unwrap().permissions();
    p.set_readonly(true);
    fs::set_permissions(&f, p).unwrap();
    let done = go(&TEMP, dir.path(), Mode::Remove, TWO_DAYS);
    assert_eq!(done.total.files, 1);
    assert!(!f.exists());
}

#[test]
fn masks_match_as_windows_does() {
    assert!(matches_mask("*.dmp", "Explorer.exe.7156.DMP"));
    assert!(matches_mask("thumbcache_*.db", "thumbcache_256.db"));
    assert!(!matches_mask("thumbcache_*.db", "iconcache_256.db"));
    assert!(matches_mask("MEMORY.DMP", "memory.dmp"));
    assert!(!matches_mask("MEMORY.DMP", "memory.dmp.bak"));
    assert!(matches_mask("?.log", "a.log"));
    assert!(!matches_mask("?.log", "ab.log"));
    assert!(matches_mask("*", ""));
    assert!(matches_mask("*.log", "Логи.log"));
}

#[test]
fn folders_are_per_user_and_per_volume_where_they_should_be() {
    let dir = tempfile::tempdir().unwrap();
    let r = roots(dir.path());
    let u = user(dir.path());
    let bin = Target {
        base: Base::RecycleBin,
        path: "",
        masks: &[],
        recursive: true,
        min_age_hours: 0,
        keep: &[],
        programs: &[],
        trim: None,
    };
    let got = folders(&bin, &r, std::slice::from_ref(&u));
    assert_eq!(got.len(), 2);
    assert!(got[0].ends_with(Path::new("C/$Recycle.Bin/S-1-5-21-1-2-3-1001")));
    let local = folders(&t(Base::LocalAppData, r"NVIDIA\DXCache", true, 0), &r, &[u]);
    assert!(local[0].ends_with(Path::new("Users/u/AppData/Local/NVIDIA/DXCache")));
    assert!(folders(&t(Base::LocalAppData, "Temp", true, 0), &r, &[]).is_empty());
    let sys = folders(&t(Base::Windows, r"Logs\CBS", false, 0), &r, &[]);
    assert!(sys[0].ends_with(Path::new("Windows/Logs/CBS")));
}

/// What the window listed is what goes: a file that turned up after the
/// analysis stays, though the rules would take it.
#[test]
fn the_cleanup_removes_only_what_the_analysis_listed() {
    let dir = tempfile::tempdir().unwrap();
    let temp = dir.path().join(r"Users/u/AppData/Local/Temp");
    write(&temp.join("a.tmp"), 10);
    write(&temp.join("sub/b.tmp"), 20);

    let seen = go(&TEMP, dir.path(), Mode::Analyze, TWO_DAYS);
    let listed = &seen.listing["temp_user"];
    assert_eq!(listed.len(), 2);
    assert_eq!(listed.iter().map(|f| f.size).sum::<u64>(), 30);
    let only: std::collections::HashSet<_> = listed.iter().map(|f| f.path.clone()).collect();

    write(&temp.join("late.tmp"), 5);
    let done = run_at_with(
        &[&TEMP],
        &roots(dir.path()),
        &[user(dir.path())],
        Mode::Remove,
        Options::default(),
        &AtomicBool::new(false),
        |_, _| {},
        SystemTime::now() + TWO_DAYS,
        Some(&only),
    );
    assert_eq!(done.total.files, 2);
    assert!(!temp.join("a.tmp").exists());
    assert!(!temp.join("sub").exists());
    assert!(temp.join("late.tmp").exists(), "not listed, not removed");
    assert!(done.listing.is_empty(), "a cleanup lists nothing");
}

/// A browser's profiles are found by the mask in the path; an open browser's
/// folders are left, and the report says which browser to close.
#[test]
fn browser_profiles_are_found_and_an_open_browser_is_left() {
    let dir = tempfile::tempdir().unwrap();
    let ud = dir
        .path()
        .join(r"Users/u/AppData/Local/Google/Chrome/User Data");
    write(&ud.join("Default/Cache/Cache_Data/f_000001"), 100);
    write(&ud.join("Profile 1/Cache/Cache_Data/f_000002"), 200);
    write(&ud.join("Default/History"), 7);
    write(&ud.join("Default/Bookmarks"), 9);
    let edge = dir
        .path()
        .join(r"Users/u/AppData/Local/Microsoft/Edge/User Data");
    write(&edge.join("Default/Cache/Cache_Data/f_000003"), 300);
    // A junction-like link among the profiles is not a profile.
    #[cfg(unix)]
    std::os::unix::fs::symlink(dir.path().join("elsewhere"), ud.join("Linked")).unwrap();

    let cache = crate::catalog::item("browser_cache").unwrap();
    let mut r = roots(dir.path());
    let seen = run_at(
        &[cache],
        &r,
        &[user(dir.path())],
        Mode::Analyze,
        &AtomicBool::new(false),
        |_, _| {},
        SystemTime::now(),
    );
    assert_eq!((seen.total.files, seen.total.bytes), (3, 600));
    assert!(seen.items[0].running.is_empty());

    // Chrome is open: its folders are left, Edge's are cleaned.
    r.running.insert("chrome.exe".into());
    let done = run_at(
        &[cache],
        &r,
        &[user(dir.path())],
        Mode::Remove,
        &AtomicBool::new(false),
        |_, _| {},
        SystemTime::now(),
    );
    assert_eq!((done.total.files, done.total.bytes), (1, 300));
    assert_eq!(done.items[0].running, ["Google Chrome"]);
    assert!(ud.join("Default/Cache/Cache_Data/f_000001").exists());
    assert!(!edge.join("Default/Cache/Cache_Data/f_000003").exists());

    // History takes the history files and nothing else of the profile.
    r.running.clear();
    let history = crate::catalog::item("browser_history").unwrap();
    let gone = run_at(
        &[history],
        &r,
        &[user(dir.path())],
        Mode::Remove,
        &AtomicBool::new(false),
        |_, _| {},
        SystemTime::now(),
    );
    assert_eq!(gone.total.files, 1);
    assert!(!ud.join("Default/History").exists());
    assert!(ud.join("Default/Bookmarks").exists(), "bookmarks stay");
}

/// Keeping the last days: the history database stays, cut down; what is
/// derived from it goes; the bookmarks are not touched.
#[test]
fn history_keeps_its_last_days_when_asked() {
    use rusqlite::Connection;
    let dir = tempfile::tempdir().unwrap();
    let profile = dir
        .path()
        .join(r"Users/u/AppData/Local/Vivaldi/User Data/Default");
    std::fs::create_dir_all(&profile).unwrap();
    let db = profile.join("History");
    let now = SystemTime::now();
    let old = crate::history::chromium_time(now - Duration::from_secs(30 * 86400));
    let new = crate::history::chromium_time(now - Duration::from_secs(3600));
    Connection::open(&db)
        .unwrap()
        .execute_batch(&format!(
            "CREATE TABLE urls (id INTEGER PRIMARY KEY, url TEXT);
             CREATE TABLE visits (id INTEGER PRIMARY KEY, url INTEGER, visit_time INTEGER);
             INSERT INTO urls VALUES (1, 'a'), (2, 'b');
             INSERT INTO visits VALUES (1, 1, {old}), (2, 2, {new});"
        ))
        .unwrap();
    write(&profile.join("Visited Links"), 3);
    write(&profile.join("Bookmarks"), 9);

    let history = crate::catalog::item("browser_history").unwrap();
    let keep = Options {
        history_keep_days: 7,
    };
    let go = |mode| {
        run_at_with(
            &[history],
            &roots(dir.path()),
            &[user(dir.path())],
            mode,
            keep,
            &AtomicBool::new(false),
            |_, _| {},
            now,
            None,
        )
    };
    let seen = go(Mode::Analyze);
    assert_eq!(seen.total.records, 1, "one visit is older than a week");
    assert_eq!(seen.total.files, 1, "Visited Links");
    let done = go(Mode::Remove);
    assert_eq!(done.total.records, 1);
    assert!(db.exists(), "the database stays");
    let left: i64 = Connection::open(&db)
        .unwrap()
        .query_row("SELECT count(*) FROM visits", [], |r| r.get(0))
        .unwrap();
    assert_eq!(left, 1);
    assert!(!profile.join("Visited Links").exists());
    assert!(profile.join("Bookmarks").exists());
}

// --- the registry -------------------------------------------------------

type Keys = std::collections::BTreeMap<String, Vec<crate::registry::Value>>;

thread_local! {
    /// The users' registries of a test: SID → key → its values.
    static HIVES: std::cell::RefCell<HashMap<String, Keys>> = Default::default();
}

/// A user whose registry cannot be reached (held by another process).
const REFUSED: &str = "S-1-5-21-9-9-9-1009";

struct FakeHive(String);

impl FakeHive {
    fn with<T>(&self, f: impl FnOnce(&mut Keys) -> T) -> T {
        HIVES.with(|h| f(h.borrow_mut().entry(self.0.clone()).or_default()))
    }
}

impl crate::registry::Hive for FakeHive {
    fn values(&self, key: &str) -> Vec<crate::registry::Value> {
        self.with(|keys| keys.get(key).cloned().unwrap_or_default())
    }

    fn subkeys(&self, key: &str) -> Vec<crate::registry::Subkey> {
        let prefix = if key.is_empty() {
            String::new()
        } else {
            format!(r"{key}\")
        };
        self.with(|keys| {
            let mut subs: std::collections::BTreeMap<String, u64> = Default::default();
            for (path, values) in keys.iter() {
                if let Some(rest) = path.strip_prefix(&prefix) {
                    let name = rest.split('\\').next().unwrap().to_string();
                    *subs.entry(name).or_default() += values.iter().map(|v| v.size).sum::<u64>();
                }
            }
            subs.into_iter()
                .map(|(name, size)| crate::registry::Subkey { name, size })
                .collect()
        })
    }

    fn subkey_names(&self, key: &str) -> Vec<String> {
        self.subkeys(key).into_iter().map(|s| s.name).collect()
    }

    fn delete_value(&self, key: &str, name: &str) -> std::io::Result<()> {
        self.with(|keys| {
            let values = keys.get_mut(key).ok_or(std::io::ErrorKind::NotFound)?;
            let before = values.len();
            values.retain(|v| v.name != name);
            if values.len() == before {
                return Err(std::io::ErrorKind::NotFound.into());
            }
            Ok(())
        })
    }

    fn delete_subkey(&self, key: &str, name: &str) -> std::io::Result<()> {
        let gone = format!(r"{key}\{name}");
        let below = format!(r"{gone}\");
        self.with(|keys| keys.retain(|path, _| path != &gone && !path.starts_with(&below)));
        Ok(())
    }
}

fn fake_hives(user: &User, _write: bool) -> std::io::Result<Box<dyn crate::registry::Hive>> {
    if user.sid == REFUSED {
        return Err(std::io::ErrorKind::PermissionDenied.into());
    }
    if HIVES.with(|h| h.borrow().contains_key(&user.sid)) {
        Ok(Box::new(FakeHive(user.sid.clone())))
    } else {
        Err(std::io::ErrorKind::NotFound.into())
    }
}

const EXPLORER: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer";

fn value(name: &str, size: u64, text: Option<&str>) -> crate::registry::Value {
    crate::registry::Value {
        name: name.into(),
        size,
        text: text.map(Into::into),
    }
}

/// Put `values` in a key of a user's registry.
fn set_key(sid: &str, key: &str, values: Vec<crate::registry::Value>) {
    HIVES.with(|h| {
        h.borrow_mut()
            .entry(sid.to_string())
            .or_default()
            .insert(key.to_string(), values)
    });
}

fn key_values(sid: &str, key: &str) -> Vec<String> {
    HIVES.with(|h| {
        h.borrow()
            .get(sid)
            .and_then(|k| k.get(key))
            .map(|v| v.iter().map(|v| v.name.clone()).collect())
            .unwrap_or_default()
    })
}

fn key_exists(sid: &str, key: &str) -> bool {
    HIVES.with(|h| h.borrow().get(sid).is_some_and(|k| k.contains_key(key)))
}

fn traces(ids: &[&str], dir: &Path, mode: Mode, only: Option<&HashSet<PathBuf>>) -> Report {
    let items: Vec<&Item> = ids
        .iter()
        .map(|id| crate::catalog::item(id).unwrap())
        .collect();
    run_at_with(
        &items,
        &roots(dir),
        &[user(dir)],
        mode,
        Options::default(),
        &AtomicBool::new(false),
        |_, _| {},
        SystemTime::now(),
        only,
    )
}

/// The Run box's and the recent documents' lists: the analysis counts and
/// lists them with what they say, the cleanup empties them; the keys
/// themselves, their default values and the keys around them stay.
#[test]
fn registry_traces_are_listed_then_removed_and_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let sid = user(dir.path()).sid;
    let run = format!(r"{EXPLORER}\RunMRU");
    let recent = format!(r"{EXPLORER}\RecentDocs");
    set_key(
        &sid,
        &run,
        vec![
            value("", 4, Some("default")),
            value("a", 12, Some(r"cmd\1")),
            value("b", 18, Some(r"regedit\1")),
            value("MRUList", 6, Some("ba")),
        ],
    );
    set_key(
        &sid,
        &recent,
        vec![
            value("0", 90, Some("Отчёт.docx")),
            value("MRUListEx", 8, None),
        ],
    );
    set_key(
        &sid,
        &format!(r"{recent}\.docx"),
        vec![value("0", 90, Some("Отчёт.docx"))],
    );
    set_key(
        &sid,
        &format!(r"{EXPLORER}\Advanced"),
        vec![value("Hidden", 4, None)],
    );
    write(
        &dir.path()
            .join("Users/u/AppData/Roaming/Microsoft/Windows/Recent/Отчёт.docx.lnk"),
        500,
    );

    let seen = traces(
        &["run_history", "recent_docs"],
        dir.path(),
        Mode::Analyze,
        None,
    );
    assert_eq!(
        seen.items[0].tally.entries, 3,
        "a, b, MRUList; not the default"
    );
    assert_eq!(seen.items[1].tally.entries, 3, "0, MRUListEx and .docx");
    assert_eq!(seen.items[1].tally.files, 1, "the shortcut in Recent");
    let listed: Vec<String> = seen.listing["run_history"]
        .iter()
        .map(|f| f.path.to_string_lossy().into_owned())
        .collect();
    assert!(
        listed.contains(&format!(r"HKEY_USERS\{sid}\{run}\a = cmd\1")),
        "{listed:?}"
    );
    assert!(seen.listing["recent_docs"]
        .iter()
        .any(|f| f.path.to_string_lossy() == format!(r"HKEY_USERS\{sid}\{recent}\.docx\")));
    assert_eq!(key_values(&sid, &run).len(), 4, "analysis removes nothing");

    let done = traces(
        &["run_history", "recent_docs"],
        dir.path(),
        Mode::Remove,
        None,
    );
    assert_eq!(done.total.entries, 6);
    assert_eq!(done.total.left_files, 0);
    assert_eq!(
        key_values(&sid, &run),
        vec![String::new()],
        "the default value stays"
    );
    assert!(key_values(&sid, &recent).is_empty());
    assert!(!key_exists(&sid, &format!(r"{recent}\.docx")));
    assert_eq!(
        key_values(&sid, &format!(r"{EXPLORER}\Advanced")),
        vec!["Hidden"]
    );
}

/// A cleanup held to the analysis removes the entries it listed: one whose
/// text changed since (the Run box's `a`, now another command) and one that
/// turned up stay.
#[test]
fn registry_cleanup_is_held_to_what_the_analysis_showed() {
    let dir = tempfile::tempdir().unwrap();
    let sid = user(dir.path()).sid;
    let run = format!(r"{EXPLORER}\RunMRU");
    set_key(
        &sid,
        &run,
        vec![
            value("a", 12, Some(r"cmd\1")),
            value("b", 18, Some(r"regedit\1")),
        ],
    );
    let seen = traces(&["run_history"], dir.path(), Mode::Analyze, None);
    let only: HashSet<PathBuf> = seen.listing["run_history"]
        .iter()
        .map(|f| f.path.clone())
        .collect();

    set_key(
        &sid,
        &run,
        vec![
            value("a", 20, Some(r"powershell\1")),
            value("b", 18, Some(r"regedit\1")),
            value("c", 10, Some(r"calc\1")),
        ],
    );
    let done = traces(&["run_history"], dir.path(), Mode::Remove, Some(&only));
    assert_eq!(done.total.entries, 1);
    assert_eq!(key_values(&sid, &run), vec!["a", "c"]);
}

/// Remote Desktop: the servers' list and the last connection's file go; a
/// value of the key that is not a list entry, and a saved connection, stay.
#[test]
fn remote_desktop_history_goes_but_other_settings_stay() {
    let dir = tempfile::tempdir().unwrap();
    let sid = user(dir.path()).sid;
    let tsc = r"Software\Microsoft\Terminal Server Client";
    set_key(
        &sid,
        &format!(r"{tsc}\Default"),
        vec![
            value("MRU0", 30, Some("host.example")),
            value("MRU1", 30, Some("10.0.0.5")),
            value("Other", 4, None),
        ],
    );
    set_key(
        &sid,
        &format!(r"{tsc}\Servers\host.example"),
        vec![value("UsernameHint", 20, Some(r"HOST\me"))],
    );
    let documents = dir.path().join("Users/u/Documents");
    write(&documents.join("Default.rdp"), 2000);
    write(&documents.join("work.rdp"), 2000);

    let done = traces(&["rdp_history"], dir.path(), Mode::Remove, None);
    assert_eq!(done.total.entries, 3, "MRU0, MRU1, the server");
    assert_eq!(done.total.files, 1);
    assert_eq!(key_values(&sid, &format!(r"{tsc}\Default")), vec!["Other"]);
    assert!(!key_exists(&sid, &format!(r"{tsc}\Servers\host.example")));
    assert!(!documents.join("Default.rdp").exists());
    assert!(
        documents.join("work.rdp").exists(),
        "a saved connection stays"
    );
}

/// A registry that cannot be reached is counted as left; a user with none
/// has nothing to clean.
#[test]
fn an_unreachable_registry_is_left_and_said_so() {
    let dir = tempfile::tempdir().unwrap();
    let refused = User {
        sid: REFUSED.into(),
        ..user(dir.path())
    };
    let none = User {
        sid: "S-1-5-21-8-8-8-1008".into(),
        ..user(dir.path())
    };
    let report = run_at_with(
        &[crate::catalog::item("run_history").unwrap()],
        &roots(dir.path()),
        &[refused, none],
        Mode::Remove,
        Options::default(),
        &AtomicBool::new(false),
        |_, _| {},
        SystemTime::now(),
        None,
    );
    assert_eq!(report.total.left_files, 1);
    assert_eq!(report.total.entries, 0);
}

/// Jump lists go but Explorer's own, which keep the folders pinned to Quick
/// access.
#[test]
fn jump_lists_go_but_quick_access_pins_stay() {
    let dir = tempfile::tempdir().unwrap();
    let auto = dir
        .path()
        .join("Users/u/AppData/Roaming/Microsoft/Windows/Recent/AutomaticDestinations");
    write(&auto.join("9b9cdc69c1c24e2b.automaticDestinations-ms"), 100);
    write(&auto.join("f01b4d95cf55d32a.automaticDestinations-ms"), 100);
    write(&auto.join("5f7b5f1e01b83767.automaticDestinations-ms"), 100);
    let done = traces(&["jump_lists"], dir.path(), Mode::Remove, None);
    assert_eq!(done.total.files, 1);
    assert!(auto
        .join("f01b4d95cf55d32a.automaticDestinations-ms")
        .exists());
    assert!(auto
        .join("5f7b5f1e01b83767.automaticDestinations-ms")
        .exists());
}

// --- programs ---------------------------------------------------------------

fn run_items(ids: &[&str], dir: &Path, mode: Mode, running: &[&str]) -> Report {
    let items: Vec<&Item> = ids
        .iter()
        .map(|id| crate::catalog::item(id).unwrap())
        .collect();
    let mut r = roots(dir);
    r.running = running.iter().map(|p| p.to_string()).collect();
    run_at_with(
        &items,
        &r,
        &[user(dir)],
        mode,
        Options::default(),
        &AtomicBool::new(false),
        |_, _| {},
        SystemTime::now() + TWO_DAYS * 4,
        None,
    )
}

/// Office's recent files of every version and application, signed-in
/// account's lists too: `Item N` go, the places pinned beside them and the
/// list's settings stay. While Word runs, the lists are left.
#[test]
fn office_lists_lose_their_items_but_keep_pinned_places() {
    let dir = tempfile::tempdir().unwrap();
    let sid = user(dir.path()).sid;
    let word = r"Software\Microsoft\Office\16.0\Word\File MRU";
    let account = r"Software\Microsoft\Office\16.0\Excel\User MRU\ADAL_1234\Place MRU";
    let options = r"Software\Microsoft\Office\16.0\Word\Options";
    set_key(
        &sid,
        word,
        vec![
            value("FOLDERID_Documents", 40, None),
            value(
                "Item 1",
                90,
                Some(r"[F00000000][T01DB0000][O00000000]*D:\Отчёт.docx"),
            ),
            value("Max Display", 4, None),
        ],
    );
    set_key(
        &sid,
        account,
        vec![value("Item 1", 60, Some(r"D:\Таблицы\"))],
    );
    set_key(&sid, options, vec![value("Item 1", 4, None)]);

    let open = run_items(
        &["office_recent"],
        dir.path(),
        Mode::Remove,
        &["winword.exe"],
    );
    assert_eq!(open.items[0].running, vec!["Microsoft Word"]);
    assert_eq!(open.total.entries, 0, "Word is open: its lists stay");

    let seen = run_items(&["office_recent"], dir.path(), Mode::Analyze, &[]);
    assert_eq!(seen.total.entries, 2, "Word's Item 1 and Excel's account's");
    let done = run_items(&["office_recent"], dir.path(), Mode::Remove, &[]);
    assert_eq!(done.total.entries, 2);
    assert_eq!(
        key_values(&sid, word),
        vec!["FOLDERID_Documents", "Max Display"]
    );
    assert!(key_values(&sid, account).is_empty());
    assert_eq!(key_values(&sid, options), vec!["Item 1"], "not a list");
}

/// The programs' recent lists take their own values: 7-Zip's folder
/// shortcuts and MPC-HC's settings stay.
#[test]
fn program_lists_take_only_their_values() {
    let dir = tempfile::tempdir().unwrap();
    let sid = user(dir.path()).sid;
    set_key(
        &sid,
        r"Software\7-Zip\FM",
        vec![
            value("FolderHistory", 80, None),
            value("PanelPath1", 20, Some(r"D:\Archives\")),
            value("FolderShortcuts", 30, None),
            value("ShowGrid", 4, None),
        ],
    );
    let history = r"Software\MPC-HC\MPC-HC\MediaHistory";
    set_key(&sid, history, vec![value("LastAdded", 8, None)]);
    set_key(
        &sid,
        &format!(r"{history}\+2wow5YesELh"),
        vec![value("Filename", 80, Some(r"H:\Video\DSCF1652.MOV"))],
    );
    set_key(
        &sid,
        r"Software\MPC-HC\MPC-HC\Settings",
        vec![value("ShowOSD", 4, None)],
    );

    let done = run_items(&["apps_recent"], dir.path(), Mode::Remove, &[]);
    assert_eq!(
        done.total.entries, 4,
        "two 7-Zip values, LastAdded, the file"
    );
    assert_eq!(
        key_values(&sid, r"Software\7-Zip\FM"),
        vec!["FolderShortcuts", "ShowGrid"]
    );
    assert!(!key_exists(&sid, &format!(r"{history}\+2wow5YesELh")));
    assert_eq!(
        key_values(&sid, r"Software\MPC-HC\MPC-HC\Settings"),
        vec!["ShowOSD"]
    );
}

/// Logs a week old go; a fresh log and DaVinci Resolve's project library
/// beside the logs stay. Telegram's caches of every account go, its
/// account data stays. NVIDIA's unpacked driver on the system drive goes.
#[test]
fn program_logs_and_caches_go_and_their_data_stays() {
    let dir = tempfile::tempdir().unwrap();
    let support = dir
        .path()
        .join("ProgramData/Blackmagic Design/DaVinci Resolve/Support");
    write(&support.join("logs/ResolveDebug.txt"), 100);
    write(
        &support.join("Resolve Project Library/Resolve Projects/project.db"),
        50,
    );
    let roaming = dir.path().join("Users/u/AppData/Roaming");
    let tdata = roaming.join("Telegram Desktop/tdata");
    write(&tdata.join("user_data/cache/0/AB/file"), 300);
    write(&tdata.join("user_data#2/media_cache/x"), 200);
    write(&tdata.join("D877F783D5D3EF8C/maps"), 10);
    write(&tdata.join("key_datas"), 10);
    write(
        &dir.path().join("NVIDIA/DisplayDriver/572.16/setup.exe"),
        1000,
    );
    write(&dir.path().join("NVIDIA/notes.txt"), 5);

    let done = run_items(
        &["app_logs", "telegram_cache", "driver_installers"],
        dir.path(),
        Mode::Remove,
        &[],
    );
    assert_eq!(done.items[0].tally.files, 1, "the log");
    assert_eq!(done.items[1].tally.files, 2, "both accounts' caches");
    assert_eq!(done.items[2].tally.files, 1, "the unpacked driver");
    assert!(!support.join("logs/ResolveDebug.txt").exists());
    assert!(support
        .join("Resolve Project Library/Resolve Projects/project.db")
        .exists());
    assert!(tdata.join("D877F783D5D3EF8C/maps").exists());
    assert!(tdata.join("key_datas").exists());
    assert!(dir.path().join("NVIDIA/notes.txt").exists());

    // A log written now stays.
    write(&support.join("logs/fresh.txt"), 10);
    let again = run_at(
        &[crate::catalog::item("app_logs").unwrap()],
        &roots(dir.path()),
        &[user(dir.path())],
        Mode::Remove,
        &AtomicBool::new(false),
        |_, _| {},
        SystemTime::now(),
    );
    assert_eq!(again.total.files, 0);
    assert!(support.join("logs/fresh.txt").exists());
}
