//! Cleanup on a real Windows, by hand.
//!
//!   cleanup_probe analyze
//!       Every item, for this user, measured where it really is — nothing is
//!       removed. Run as the user (system folders the user may not read come
//!       out smaller than the service would see).
//!
//!   cleanup_probe trim <copy of History> <days>
//!       Cut a copy of a Chromium history database to its last days.
//!
//!   cleanup_probe selftest <empty folder>
//!       Builds a profile and a Windows folder inside <folder> with junk, a
//!       junction to a folder of "documents", a file held open and a fresh
//!       file; removes the junk through the real engine on NTFS, and checks
//!       that the documents, the open file and the fresh file are still there.
//!
//!   cleanup_probe regtest <empty folder>
//!       Makes a registry hive file `<folder>\NTUSER.DAT` with traces and
//!       settings, as a user who is not signed in has; the real engine
//!       loads it privately, lists and removes the traces; checks that the
//!       settings stay and that a hive held by someone else is left.
//!
//!   cleanup_probe analyze          (Linux)
//!       Every Linux item for the users of this system — measured, nothing
//!       removed. As a user: that user only; as root: every user (each with
//!       their own rights) and the system's part.

#[cfg(target_os = "linux")]
fn main() {
    use std::sync::atomic::AtomicBool;

    use own_backup_cleanup::{catalog, engine, users, Mode, Roots};

    if std::env::args().nth(1).as_deref() != Some("analyze") {
        eprintln!("cleanup_probe analyze");
        std::process::exit(2);
    }
    // SAFETY: no arguments.
    let me = unsafe { libc::geteuid() };
    let people: Vec<_> = users::profiles()
        .into_iter()
        .filter(|u| me == 0 || u.sid == me.to_string())
        .collect();
    for u in &people {
        println!("user {} {} {}", u.sid, u.name, u.profile.display());
    }
    let roots = Roots::of_this_system();
    let items: Vec<_> = catalog::catalog()
        .iter()
        .filter(|i| me == 0 || !i.admin_only())
        .collect();
    let report = engine::run(
        &items,
        &roots,
        &people,
        Mode::Analyze,
        own_backup_cleanup::Options {
            history_keep_days: 7,
            ..Default::default()
        },
        &AtomicBool::new(false),
        |_, _| {},
    );
    let mib = |b: u64| b as f64 / (1024.0 * 1024.0);
    for i in &report.items {
        let t = &i.tally;
        println!(
            "{:<20} {:>8} files {:>10.1} MiB  rec {:<5} left {:<3} {}{}",
            i.id,
            t.files,
            mib(t.bytes),
            t.records,
            t.left_files,
            catalog::label(&i.id),
            if i.running.is_empty() {
                String::new()
            } else {
                format!("  (open: {})", i.running.join(", "))
            }
        );
        if i.id == "junk" {
            for f in report.listing.get(&i.id).into_iter().flatten().take(40) {
                println!("{:<20} {:>9} {}", "", f.size, f.path.display());
            }
        }
        if i.id == "browser_cache" || i.id == "editor_junk" || i.id == "dev_caches_linux" {
            let mut dirs = std::collections::BTreeMap::<String, (u64, u64)>::new();
            for f in report.listing.get(&i.id).into_iter().flatten() {
                let mut p = f.path.clone();
                while p.components().count() > 5 {
                    p.pop();
                }
                let e = dirs.entry(p.display().to_string()).or_default();
                e.0 += 1;
                e.1 += f.size;
            }
            for (d, (n, b)) in dirs {
                println!("{:<20} {:>8} files {:>10.1} MiB  {d}", "", n, mib(b));
            }
        }
    }
    println!(
        "TOTAL {} files {:.1} MiB",
        report.total.files,
        mib(report.total.bytes)
    );
}

#[cfg(not(any(windows, target_os = "linux")))]
fn main() {
    eprintln!("cleanup_probe runs on Windows and Linux");
}

#[cfg(windows)]
fn main() {
    win::main()
}

#[cfg(windows)]
mod win {
    use std::path::Path;
    use std::sync::atomic::AtomicBool;

    use own_backup_cleanup::{catalog, engine, users, Mode, Roots, User};

    pub fn main() {
        let args: Vec<String> = std::env::args().skip(1).collect();
        match args.first().map(String::as_str) {
            Some("analyze") => analyze(),
            Some("trim") if args.len() == 3 => trim(Path::new(&args[1]), args[2].parse().unwrap()),
            Some("selftest") if args.len() == 2 => selftest(Path::new(&args[1])),
            Some("regtest") if args.len() == 2 => regtest(Path::new(&args[1])),
            Some("junktest") if args.len() == 2 => junktest(Path::new(&args[1])),
            Some("volcache") => volcache(&args[1..]),
            _ => {
                eprintln!(
                    "cleanup_probe analyze | selftest <empty folder> | regtest <empty folder>"
                );
                std::process::exit(2);
            }
        }
    }

    /// Cut a COPY of a history database to its last `days`: how many visits
    /// go, and what is left. Never point it at a browser's own file.
    fn trim(db: &Path, days: u64) {
        use own_backup_cleanup::history;
        let before = std::time::SystemTime::now() - std::time::Duration::from_secs(days * 86400);
        println!(
            "older than {days} days: {:?}",
            history::count_old(db, before)
        );
        println!("trim: {:?}", history::trim(db, before));
        println!("older now: {:?}", history::count_old(db, before));
        println!(
            "all since: {:?}",
            history::count_old(db, std::time::SystemTime::now())
        );
    }

    fn mib(b: u64) -> f64 {
        b as f64 / (1024.0 * 1024.0)
    }

    fn analyze() {
        let me = std::env::var("USERPROFILE").unwrap_or_default();
        let profiles = users::profiles();
        println!("profiles found: {}", profiles.len());
        for u in &profiles {
            println!("  {}  {}  {}", u.sid, u.name, u.profile.display());
        }
        let mine: Vec<User> = profiles
            .into_iter()
            .filter(|u| u.profile.as_os_str().eq_ignore_ascii_case(&me))
            .collect();
        let roots = Roots::of_this_system();
        println!(
            "windows {}  programdata {}  volumes {:?}",
            roots.windows.display(),
            roots.program_data.display(),
            roots.volumes
        );
        let items: Vec<_> = catalog::catalog().iter().collect();
        let report = engine::run(
            &items,
            &roots,
            &mine,
            Mode::Analyze,
            own_backup_cleanup::Options {
                history_keep_days: 7,
                ..Default::default()
            },
            &AtomicBool::new(false),
            |_, _| {},
        );
        for i in &report.items {
            println!(
                "{:<16} {:>8} files {:>10.1} MiB   {}",
                i.id,
                i.tally.files,
                mib(i.tally.bytes),
                catalog::label(&i.id)
            );
            if i.tally.records > 0 {
                println!(
                    "{:<16} {} history entries older than a week",
                    "", i.tally.records
                );
            }
            if i.tally.entries > 0 {
                println!("{:<16} {} registry entries", "", i.tally.entries);
                let found = report.listing.get(&i.id).into_iter().flatten();
                for f in found.filter(|f| f.path.starts_with("HKEY_USERS")).take(12) {
                    println!("{:<16}   {}", "", f.path.display());
                }
            }
            if !i.running.is_empty() {
                println!("{:<16} left, open: {}", "", i.running.join(", "));
            }
            // Where an item's files are, by folder (the browsers' profiles).
            if i.id == "junk" {
                for f in report.listing.get(&i.id).into_iter().flatten().take(60) {
                    println!("{:<16}   {:>9} {}", "", f.size, f.path.display());
                }
            }
            if i.id.starts_with("browser") {
                let mut dirs = std::collections::BTreeMap::<String, (u64, u64)>::new();
                for f in report.listing.get(&i.id).into_iter().flatten() {
                    let shown = f
                        .path
                        .parent()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                        .replace(&me, "%USERPROFILE%");
                    let e = dirs.entry(shown).or_default();
                    e.0 += 1;
                    e.1 += f.size;
                }
                for (d, (n, b)) in dirs {
                    println!("{:<16} {:>8} files {:>10.1} MiB   {d}", "", n, mib(b));
                }
            }
        }
        println!(
            "TOTAL {} files {:.1} MiB",
            report.total.files,
            mib(report.total.bytes)
        );
    }

    fn write(p: &Path, n: usize) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, vec![1u8; n]).unwrap();
    }

    /// A file or a folder a day and a half old: written and created then.
    fn age(p: &Path) {
        use std::os::windows::fs::{FileTimesExt, OpenOptionsExt};
        const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(36 * 3600);
        let f = std::fs::OpenOptions::new()
            .access_mode(FILE_WRITE_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(p)
            .unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(old).set_created(old))
            .unwrap();
    }

    fn selftest(dir: &Path) {
        let mut failures = 0;
        let mut check = |ok: bool, what: &str| {
            println!("{} {what}", if ok { "PASS" } else { "FAIL" });
            if !ok {
                failures += 1;
            }
        };
        assert!(
            !dir.exists() || std::fs::read_dir(dir).unwrap().next().is_none(),
            "the folder must be empty"
        );
        let profile = dir.join("Users").join("tester");
        let temp = profile.join(r"AppData\Local\Temp");
        let docs = dir.join("Documents");
        write(&docs.join("work.odt"), 100);
        for i in 0..50 {
            let f = temp.join(format!("_MEI{i:04}")).join("python.dll");
            write(&f, 1000);
            age(&f);
            age(f.parent().unwrap());
        }
        write(&temp.join("installer.log"), 10);
        age(&temp.join("installer.log"));
        write(&temp.join("fresh.tmp"), 10);
        let held = temp.join("held.tmp");
        write(&held, 10);
        age(&held);
        // A junction inside Temp to the documents (mklink /J).
        let junction = temp.join("docs-link");
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&docs)
            .status()
            .unwrap();
        check(status.success(), "junction made");
        // Held open with no sharing: the removal must leave it.
        let open = {
            use std::os::windows::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(&held)
                .unwrap()
        };

        let roots = Roots {
            windows: dir.join("Windows"),
            program_data: dir.join("ProgramData"),
            volumes: vec![],
            running: Default::default(),
            hives: own_backup_cleanup::registry::open_user,
            fs_root: dir.join("root"),
            rights: own_backup_cleanup::users::as_user,
        };
        let user = User {
            sid: "S-1-5-21-0-0-0-1001".into(),
            name: "tester".into(),
            profile: profile.clone(),
        };
        let temp_item = catalog::item("temp_user").expect("temp_user");
        let seen = engine::run(
            &[temp_item],
            &roots,
            std::slice::from_ref(&user),
            Mode::Analyze,
            own_backup_cleanup::Options::default(),
            &AtomicBool::new(false),
            |_, _| {},
        );
        println!("analysis: {:?}", seen.total);
        check(seen.total.files == 52, "analysis: 50 + log + held");
        let done = engine::run(
            &[temp_item],
            &roots,
            &[user],
            Mode::Remove,
            own_backup_cleanup::Options::default(),
            &AtomicBool::new(false),
            |_, _| {},
        );
        println!("removal: {:?}", done.total);
        check(done.total.files == 51, "removed: the 50 and the log");
        check(done.total.left_files == 1, "left: the file held open");
        check(held.exists(), "the held file is still there");
        check(
            temp.join("fresh.tmp").exists(),
            "the fresh file is still there",
        );
        check(
            docs.join("work.odt").exists(),
            "the document behind the junction is still there",
        );
        check(
            std::fs::symlink_metadata(&junction).is_ok(),
            "the junction itself is still there",
        );
        check(
            !temp.join("_MEI0000").exists(),
            "the emptied _MEI folders are gone",
        );
        check(temp.is_dir(), "Temp itself stays");
        drop(open);
        let _ = std::fs::remove_dir(&junction);
        let _ = std::fs::remove_dir_all(dir);
        println!(
            "{}",
            if failures == 0 {
                "ALL PASS"
            } else {
                "FAILURES"
            }
        );
        std::process::exit(if failures == 0 { 0 } else { 1 });
    }

    mod hive {
        use std::path::Path;

        use windows::core::PCWSTR;
        use windows::Win32::Foundation::ERROR_SUCCESS;
        use windows::Win32::System::Registry::{
            RegCloseKey, RegCreateKeyExW, RegLoadAppKeyW, RegOpenKeyExW, RegSetValueExW, HKEY,
            KEY_ALL_ACCESS, KEY_READ, REG_BINARY, REG_DWORD, REG_OPTION_NON_VOLATILE,
            REG_PROCESS_APPKEY, REG_SZ, REG_VALUE_TYPE,
        };

        fn wide(s: &str) -> Vec<u16> {
            s.encode_utf16().chain(std::iter::once(0)).collect()
        }

        pub fn utf16(s: &str) -> Vec<u8> {
            s.encode_utf16()
                .chain([0])
                .flat_map(u16::to_le_bytes)
                .collect()
        }

        /// The hive file loaded (made, if there is none) for this process.
        pub fn load(file: &Path) -> HKEY {
            let name = wide(&file.to_string_lossy());
            let mut root = HKEY::default();
            // SAFETY: the name outlives the call; `root` is an out-parameter.
            let status = unsafe {
                RegLoadAppKeyW(
                    PCWSTR(name.as_ptr()),
                    &mut root,
                    KEY_ALL_ACCESS.0,
                    REG_PROCESS_APPKEY,
                    0,
                )
            };
            assert_eq!(status, ERROR_SUCCESS, "RegLoadAppKeyW {}", file.display());
            root
        }

        pub fn close(key: HKEY) {
            // SAFETY: a key we opened, closed once.
            unsafe {
                let _ = RegCloseKey(key);
            }
        }

        pub fn set(root: HKEY, path: &str, name: &str, kind: REG_VALUE_TYPE, data: &[u8]) {
            let sub = wide(path);
            let mut key = HKEY::default();
            // SAFETY: names and data outlive the calls; the key is closed.
            unsafe {
                let status = RegCreateKeyExW(
                    root,
                    PCWSTR(sub.as_ptr()),
                    0,
                    PCWSTR::null(),
                    REG_OPTION_NON_VOLATILE,
                    KEY_ALL_ACCESS,
                    None,
                    &mut key,
                    None,
                );
                assert_eq!(status, ERROR_SUCCESS, "create {path}");
                let n = wide(name);
                let status = RegSetValueExW(key, PCWSTR(n.as_ptr()), 0, kind, Some(data));
                assert_eq!(status, ERROR_SUCCESS, "set {path}\\{name}");
                let _ = RegCloseKey(key);
            }
        }

        pub fn text(root: HKEY, path: &str, name: &str, value: &str) {
            set(root, path, name, REG_SZ, &utf16(value));
        }

        pub fn dword(root: HKEY, path: &str, name: &str) {
            set(root, path, name, REG_DWORD, &1u32.to_le_bytes());
        }

        pub fn binary(root: HKEY, path: &str, name: &str, data: &[u8]) {
            set(root, path, name, REG_BINARY, data);
        }

        /// The value names of a key, None when the key is not there.
        pub fn names(root: HKEY, path: &str) -> Option<Vec<String>> {
            let sub = wide(path);
            let mut key = HKEY::default();
            // SAFETY: the name outlives the call; the key is closed below.
            let status =
                unsafe { RegOpenKeyExW(root, PCWSTR(sub.as_ptr()), 0, KEY_READ, &mut key) };
            if status != ERROR_SUCCESS {
                return None;
            }
            let mut out = Vec::new();
            for index in 0.. {
                let mut name = [0u16; 512];
                let mut len = name.len() as u32;
                // SAFETY: the buffer and its length are live.
                let status = unsafe {
                    windows::Win32::System::Registry::RegEnumValueW(
                        key,
                        index,
                        windows::core::PWSTR(name.as_mut_ptr()),
                        &mut len,
                        None,
                        None,
                        None,
                        None,
                    )
                };
                if status != ERROR_SUCCESS {
                    break;
                }
                out.push(String::from_utf16_lossy(&name[..len as usize]));
            }
            close(key);
            Some(out)
        }
    }

    /// Junk by masks on NTFS: an old `.tmp` and an Office owner file go
    /// (the latter only when not unticked), a junction is not followed,
    /// AppData and this program's own files stay.
    fn junktest(dir: &Path) {
        let mut failures = 0;
        let mut check = |ok: bool, what: &str| {
            println!("{} {what}", if ok { "PASS" } else { "FAIL" });
            if !ok {
                failures += 1;
            }
        };
        assert!(
            !dir.exists() || std::fs::read_dir(dir).unwrap().next().is_none(),
            "the folder must be empty"
        );
        let profile = dir.join("Users").join("tester");
        let docs = profile.join("Documents");
        let outside = dir.join("Elsewhere");
        let files = [
            docs.join("a.tmp"),
            docs.join("~$Отчёт.docx"),
            docs.join("Отчёт.docx"),
            docs.join(".own-backup-new-0123.tmp"),
            profile.join(r"AppData\Local\Temp\c.tmp"),
            outside.join("b.tmp"),
        ];
        for f in &files {
            write(f, 100);
            age_days(f, 10);
        }
        let junction = docs.join("link");
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&outside)
            .status()
            .unwrap();
        check(status.success(), "junction made");

        let roots = Roots {
            windows: dir.join("Windows"),
            program_data: dir.join("ProgramData"),
            volumes: vec![],
            running: Default::default(),
            hives: own_backup_cleanup::registry::open_user,
            fs_root: dir.join("root"),
            rights: own_backup_cleanup::users::as_user,
        };
        let user = User {
            sid: "S-1-5-21-0-0-0-1001".into(),
            name: "tester".into(),
            profile: profile.clone(),
        };
        let item = catalog::item("junk").unwrap();
        let seen = engine::run(
            &[item],
            &roots,
            std::slice::from_ref(&user),
            Mode::Analyze,
            Default::default(),
            &AtomicBool::new(false),
            |_, _| {},
        );
        for f in seen.listing.values().flatten() {
            println!("  listed: {}", f.path.display());
        }
        check(seen.total.files == 2, "analysis: a.tmp and ~$Отчёт.docx");
        // The window unticked the owner file.
        let only: std::collections::HashSet<_> = seen
            .listing
            .values()
            .flatten()
            .map(|f| f.path.clone())
            .filter(|p| !p.ends_with("~$Отчёт.docx"))
            .collect();
        let done = engine::run_only(
            &[item],
            &roots,
            &[user],
            Default::default(),
            &AtomicBool::new(false),
            |_, _| {},
            &only,
        );
        check(done.total.files == 1, "removed: a.tmp only");
        check(!files[0].exists(), "a.tmp is gone");
        check(files[1].exists(), "the unticked owner file stays");
        check(files[2].exists(), "the document stays");
        check(files[3].exists(), "this program's own file stays");
        check(files[4].exists(), "AppData is not entered");
        check(files[5].exists(), "the junction is not followed");
        check(
            std::fs::symlink_metadata(&junction).is_ok(),
            "the junction itself stays",
        );
        let _ = std::fs::remove_dir(&junction);
        let _ = std::fs::remove_dir_all(dir);
        println!(
            "{}",
            if failures == 0 {
                "ALL PASS"
            } else {
                "FAILURES"
            }
        );
        std::process::exit(if failures == 0 { 0 } else { 1 });
    }

    /// A file written and created `days` ago.
    fn age_days(p: &Path, days: u64) {
        use std::os::windows::fs::{FileTimesExt, OpenOptionsExt};
        const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(days * 86_400);
        let f = std::fs::OpenOptions::new()
            .access_mode(FILE_WRITE_ATTRIBUTES)
            .open(p)
            .unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(old).set_created(old))
            .unwrap();
    }

    /// Measure Windows' own cleanup handlers on C:\ — nothing is removed.
    fn volcache(names: &[String]) {
        let all = [
            "Update Cleanup",
            "Previous Installations",
            "Delivery Optimization Files",
            "Temporary Setup Files",
            "Windows Upgrade Log Files",
            "Windows Reset Log Files",
            "Upgrade Discarded Files",
            "Device Driver Packages",
            "Windows ESD installation files",
            "Windows Defender",
        ];
        let picked: Vec<String> = if names.is_empty() {
            all.iter().map(|s| s.to_string()).collect()
        } else {
            names.to_vec()
        };
        for name in picked {
            let t = std::time::Instant::now();
            let r = own_backup_cleanup::volume_cache::run(
                &name,
                r"C:\",
                false,
                &AtomicBool::new(false),
            );
            println!(
                "{name:<32} {:>8.1} s  {:?}",
                t.elapsed().as_secs_f64(),
                r.map(|o| mib(o.bytes))
            );
        }
    }

    fn regtest(dir: &Path) {
        use own_backup_cleanup::Tally;

        let mut failures = 0;
        let mut check = |ok: bool, what: &str| {
            println!("{} {what}", if ok { "PASS" } else { "FAIL" });
            if !ok {
                failures += 1;
            }
        };
        assert!(
            !dir.exists() || std::fs::read_dir(dir).unwrap().next().is_none(),
            "the folder must be empty"
        );
        std::fs::create_dir_all(dir).unwrap();
        let file = dir.join("NTUSER.DAT");
        let explorer = r"Software\Microsoft\Windows\CurrentVersion\Explorer";
        let run = format!(r"{explorer}\RunMRU");
        let recent = format!(r"{explorer}\RecentDocs");
        let tsc = r"Software\Microsoft\Terminal Server Client";

        let root = hive::load(&file);
        hive::text(root, &run, "", "the key's own");
        hive::text(root, &run, "a", r"cmd\1");
        hive::text(root, &run, "b", r"regedit\1");
        hive::text(root, &run, "MRUList", "ba");
        let mut doc = hive::utf16("Отчёт.docx");
        doc.extend([0x14, 0, 0x1f, 0x50]);
        hive::binary(root, &recent, "0", &doc);
        hive::binary(root, &format!(r"{recent}\.docx"), "0", &doc);
        hive::dword(root, &format!(r"{explorer}\Advanced"), "Hidden");
        hive::text(root, &format!(r"{tsc}\Default"), "MRU0", "host.example");
        hive::dword(root, &format!(r"{tsc}\Default"), "Other");
        hive::text(
            root,
            &format!(r"{tsc}\Servers\host.example"),
            "UsernameHint",
            r"HOST\me",
        );
        let word = r"Software\Microsoft\Office\16.0\Word\File MRU";
        hive::binary(root, word, "FOLDERID_Documents", &[1, 2, 3, 4]);
        hive::text(
            root,
            word,
            "Item 1",
            r"[F00000000][T01DB0000]*D:\Отчёт.docx",
        );
        hive::dword(root, word, "Max Display");
        hive::close(root);

        // Not signed in: no HKEY_USERS\<SID>, the profile's file.
        let user = User {
            sid: "S-1-5-21-0-0-0-4242".into(),
            name: "tester".into(),
            profile: dir.to_path_buf(),
        };
        let roots = Roots {
            windows: dir.join("Windows"),
            program_data: dir.join("ProgramData"),
            volumes: vec![],
            running: Default::default(),
            hives: own_backup_cleanup::registry::open_user,
            fs_root: dir.join("root"),
            rights: own_backup_cleanup::users::as_user,
        };
        let items: Vec<_> = ["run_history", "recent_docs", "rdp_history", "office_recent"]
            .iter()
            .map(|id| catalog::item(id).unwrap())
            .collect();
        let go = |mode| {
            engine::run(
                &items,
                &roots,
                std::slice::from_ref(&user),
                mode,
                own_backup_cleanup::Options::default(),
                &AtomicBool::new(false),
                |_, _| {},
            )
        };
        let seen = go(Mode::Analyze);
        for f in seen.listing.values().flatten() {
            println!("  listed: {}  ({} bytes)", f.path.display(), f.size);
        }
        println!("analysis: {:?}", seen.total);
        check(
            seen.total.entries == 8,
            "analysis: a, b, MRUList; RecentDocs 0 and .docx; MRU0; the server; Office Item 1",
        );
        let listed: Vec<String> = seen
            .listing
            .values()
            .flatten()
            .map(|f| f.path.display().to_string())
            .collect();
        check(
            listed.iter().any(|p| p.ends_with(r"RunMRU\a = cmd\1")),
            "the Run box's command is listed with its text",
        );
        check(
            listed
                .iter()
                .any(|p| p.ends_with(r"RecentDocs\0 = Отчёт.docx")),
            "the recent document is listed by its name",
        );

        // Someone else holds the file: the hive is left, and said so.
        {
            use std::os::windows::fs::OpenOptionsExt;
            let held = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(&file)
                .unwrap();
            let refused = go(Mode::Remove);
            check(
                refused.total.entries == 0 && refused.total.left_files == 4,
                "a hive held by another is left (once per item)",
            );
            drop(held);
        }

        let done = go(Mode::Remove);
        println!("removal: {:?}", done.total);
        check(
            done.total.entries == seen.total.entries && done.total.left_files == 0,
            "removed what the analysis counted",
        );
        let root = hive::load(&file);
        check(
            hive::names(root, &run) == Some(vec![String::new()]),
            "RunMRU: only the key's own value is left",
        );
        check(
            hive::names(root, &recent) == Some(vec![]),
            "RecentDocs: empty, the key stays",
        );
        check(
            hive::names(root, &format!(r"{recent}\.docx")).is_none(),
            "RecentDocs\\.docx is gone",
        );
        check(
            hive::names(root, &format!(r"{explorer}\Advanced")) == Some(vec!["Hidden".into()]),
            "Explorer's settings stay",
        );
        check(
            hive::names(root, &format!(r"{tsc}\Default")) == Some(vec!["Other".into()]),
            "Remote Desktop: MRU0 gone, another value stays",
        );
        check(
            hive::names(root, &format!(r"{tsc}\Servers\host.example")).is_none(),
            "Remote Desktop: the server is gone",
        );
        check(
            hive::names(root, word)
                == Some(vec!["FOLDERID_Documents".into(), "Max Display".into()]),
            r"Office: Item 1 gone (found through Office\*\*), the pinned place stays",
        );
        hive::close(root);
        let again = go(Mode::Analyze);
        check(again.total == Tally::default(), "nothing left to clean");
        let _ = std::fs::remove_dir_all(dir);
        println!(
            "{}",
            if failures == 0 {
                "ALL PASS"
            } else {
                "FAILURES"
            }
        );
        std::process::exit(if failures == 0 { 0 } else { 1 });
    }
}
