//! Cleanup on a real Windows, by hand.
//!
//!   cleanup_probe analyze
//!       Every item, for this user, measured where it really is — nothing is
//!       removed. Run as the user (system folders the user may not read come
//!       out smaller than the service would see).
//!
//!   cleanup_probe selftest <empty folder>
//!       Builds a profile and a Windows folder inside <folder> with junk, a
//!       junction to a folder of "documents", a file held open and a fresh
//!       file; removes the junk through the real engine on NTFS, and checks
//!       that the documents, the open file and the fresh file are still there.

#[cfg(not(windows))]
fn main() {
    eprintln!("cleanup_probe runs on Windows");
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
            Some("selftest") if args.len() == 2 => selftest(Path::new(&args[1])),
            _ => {
                eprintln!("cleanup_probe analyze | selftest <empty folder>");
                std::process::exit(2);
            }
        }
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
            if !i.running.is_empty() {
                println!("{:<16} left, open: {}", "", i.running.join(", "));
            }
            // Where an item's files are, by folder (the browsers' profiles).
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
}
