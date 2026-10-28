use std::fs;
use std::path::Path;
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
        targets: &[
            Target {
                base: Base::Windows,
                path: "",
                masks: &["MEMORY.DMP"],
                recursive: false,
                min_age_hours: 0,
                keep: &[],
            },
            Target {
                base: Base::Windows,
                path: "Minidump",
                masks: &["*.dmp"],
                recursive: false,
                min_age_hours: 0,
                keep: &[],
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
        targets: &[Target {
            base: Base::RecycleBin,
            path: "",
            masks: &[],
            recursive: true,
            min_age_hours: 0,
            keep: &["desktop.ini"],
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
