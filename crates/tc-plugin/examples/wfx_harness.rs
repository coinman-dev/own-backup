//! Loads the built plugin the way Total Commander does and drives it: lists
//! the partitions, opens an encrypted one (a wrong passphrase first), walks
//! into a logical volume, copies a file out and checks its bytes.
//!
//! ```text
//! wfx_harness <plugin.dll> <out dir> <passphrase>
//! ```
//!
//! For a development service elsewhere set `OWN_BACKUP_ENDPOINT` (a debug
//! build of the plugin honours it). The image it expects is made by
//! `scripts/lab/linux-browse-images.sh`: `/home/user/big.bin` holds
//! `i % 251` for 5 MiB + 123 bytes.

// Function pointers from GetProcAddress, cast to what the plugin exports.
#![allow(clippy::missing_transmute_annotations)]

#[cfg(not(windows))]
fn main() {
    eprintln!("the harness loads a Windows DLL; run it on Windows");
}

#[cfg(windows)]
fn main() {
    win::run();
}

#[cfg(windows)]
mod win {
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use windows::core::{PCSTR, PCWSTR};
    use windows::Win32::Foundation::BOOL;
    use windows::Win32::Storage::FileSystem::WIN32_FIND_DATAW;
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

    type ProgressProcW = unsafe extern "system" fn(i32, *const u16, *const u16, i32) -> i32;
    type LogProcW = unsafe extern "system" fn(i32, i32, *const u16);
    type RequestProcW =
        unsafe extern "system" fn(i32, i32, *const u16, *const u16, *mut u16, i32) -> BOOL;
    type FsInitW = unsafe extern "system" fn(i32, ProgressProcW, LogProcW, RequestProcW) -> i32;
    type FsFindFirstW = unsafe extern "system" fn(*const u16, *mut WIN32_FIND_DATAW) -> *mut c_void;
    type FsFindNextW = unsafe extern "system" fn(*mut c_void, *mut WIN32_FIND_DATAW) -> BOOL;
    type FsFindClose = unsafe extern "system" fn(*mut c_void) -> i32;
    type FsGetFileW = unsafe extern "system" fn(*const u16, *const u16, i32, *const c_void) -> i32;

    static PASSWORDS: Mutex<Vec<String>> = Mutex::new(Vec::new());
    static ASKED: AtomicUsize = AtomicUsize::new(0);
    static PROGRESS: AtomicUsize = AtomicUsize::new(0);

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    unsafe fn text(p: *const u16) -> String {
        let mut n = 0;
        while *p.add(n) != 0 {
            n += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
    }

    unsafe extern "system" fn progress(_: i32, _: *const u16, _: *const u16, _: i32) -> i32 {
        PROGRESS.fetch_add(1, Ordering::SeqCst);
        0
    }
    unsafe extern "system" fn log(_: i32, _: i32, _: *const u16) {}
    unsafe extern "system" fn request(
        _: i32,
        kind: i32,
        _title: *const u16,
        body: *const u16,
        out: *mut u16,
        max: i32,
    ) -> BOOL {
        let body = text(body);
        if kind == 2 {
            ASKED.fetch_add(1, Ordering::SeqCst);
            let pw = PASSWORDS.lock().unwrap().pop();
            println!("  asked for a passphrase: {body}");
            let Some(pw) = pw else { return BOOL(0) };
            let w: Vec<u16> = pw.encode_utf16().collect();
            let n = w.len().min(max as usize - 1);
            std::ptr::copy_nonoverlapping(w.as_ptr(), out, n);
            *out.add(n) = 0;
            BOOL(1)
        } else {
            println!("  message: {body}");
            BOOL(1)
        }
    }

    struct Plugin {
        first: FsFindFirstW,
        next: FsFindNextW,
        close: FsFindClose,
        get: FsGetFileW,
    }

    impl Plugin {
        fn list(&self, path: &str) -> Vec<(String, bool, u64)> {
            let mut out = Vec::new();
            let mut fd: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
            let p = wide(path);
            let h = unsafe { (self.first)(p.as_ptr(), &mut fd) };
            if h as isize == -1 {
                return out;
            }
            loop {
                let n = fd.cFileName.iter().position(|&c| c == 0).unwrap_or(0);
                out.push((
                    String::from_utf16_lossy(&fd.cFileName[..n]),
                    fd.dwFileAttributes & 0x10 != 0,
                    (u64::from(fd.nFileSizeHigh) << 32) | u64::from(fd.nFileSizeLow),
                ));
                if !unsafe { (self.next)(h, &mut fd) }.as_bool() {
                    break;
                }
            }
            unsafe { (self.close)(h) };
            out
        }
    }

    fn check(ok: bool, what: &str, fails: &mut u32) {
        println!("{} {what}", if ok { "PASS" } else { "FAIL" });
        if !ok {
            *fails += 1;
        }
    }

    pub fn run() {
        let args: Vec<String> = std::env::args().collect();
        let (dll, out_dir, pw) = (&args[1], &args[2], &args[3]);
        *PASSWORDS.lock().unwrap() = vec![pw.clone(), "wrong passphrase".into()];
        let mut fails = 0;
        let plugin = unsafe {
            let lib = LoadLibraryW(PCWSTR(wide(dll).as_ptr())).expect("load the plugin");
            let f = |name: &str| {
                let c = std::ffi::CString::new(name).unwrap();
                GetProcAddress(lib, PCSTR(c.as_ptr().cast())).unwrap_or_else(|| panic!("{name}"))
            };
            for name in [
                "FsInit",
                "FsFindFirst",
                "FsFindNext",
                "FsGetFile",
                "FsExecuteFileW",
            ] {
                f(name);
            }
            for absent in ["FsPutFileW", "FsDeleteFileW", "FsMkDirW", "FsRenMovFileW"] {
                let c = std::ffi::CString::new(absent).unwrap();
                check(
                    GetProcAddress(lib, PCSTR(c.as_ptr().cast())).is_none(),
                    &format!("no {absent}: nothing can be written"),
                    &mut fails,
                );
            }
            let init = std::mem::transmute::<_, FsInitW>(f("FsInitW"));
            init(7, progress, log, request);
            Plugin {
                first: std::mem::transmute::<_, FsFindFirstW>(f("FsFindFirstW")),
                next: std::mem::transmute::<_, FsFindNextW>(f("FsFindNextW")),
                close: std::mem::transmute::<_, FsFindClose>(f("FsFindClose")),
                get: std::mem::transmute::<_, FsGetFileW>(f("FsGetFileW")),
            }
        };

        let top = plugin.list("\\");
        println!("partitions: {top:?}");
        let luks = top.iter().find(|(n, _, _)| n.contains("LUKS"));
        let plain = top.iter().find(|(n, _, _)| n.contains("plain.img"));
        check(
            luks.is_some() && plain.is_some(),
            "both images are offered",
            &mut fails,
        );
        let (Some(luks), Some(plain)) = (luks, plain) else {
            std::process::exit(1);
        };

        let p = format!("\\{}", plain.0);
        let files = plugin.list(&p);
        println!("{p}: {files:?}");
        check(
            files.iter().any(|(n, d, _)| n == "etc" && *d),
            "a plain partition shows its files",
            &mut fails,
        );
        check(
            ASKED.load(Ordering::SeqCst) == 0,
            "no passphrase asked for it",
            &mut fails,
        );

        let l = format!("\\{}", luks.0);
        let vols = plugin.list(&l);
        println!("{l}: {vols:?}");
        check(
            ASKED.load(Ordering::SeqCst) == 2,
            "asked twice: the wrong one, then the right one",
            &mut fails,
        );
        check(
            vols.iter().map(|v| v.0.as_str()).collect::<Vec<_>>() == ["vgobk-root"],
            "the logical volume, swap left out",
            &mut fails,
        );
        let user = format!("{l}\\vgobk-root\\home\\user");
        let files = plugin.list(&user);
        println!("{user}: {files:?}");
        let big = files.iter().find(|(n, _, _)| n == "big.bin");
        check(
            big.map(|b| b.2) == Some(5 * 1024 * 1024 + 123),
            "big.bin and its size",
            &mut fails,
        );
        check(
            files.iter().any(|(n, _, _)| n.starts_with("a\u{F03A}b")),
            "a name with a colon, moved where Windows can hold it",
            &mut fails,
        );
        check(
            files.iter().any(|(n, d, _)| n == "Документы" && *d),
            "Cyrillic folder",
            &mut fails,
        );

        std::fs::create_dir_all(out_dir).unwrap();
        let local = format!("{out_dir}\\big.bin");
        let _ = std::fs::remove_file(&local);
        let remote = wide(&format!("{user}\\big.bin"));
        let code =
            unsafe { (plugin.get)(remote.as_ptr(), wide(&local).as_ptr(), 0, std::ptr::null()) };
        check(
            code == 0,
            &format!("copied out (FS_FILE_OK, got {code})"),
            &mut fails,
        );
        let got = std::fs::read(&local).unwrap_or_default();
        let want: Vec<u8> = (0..5 * 1024 * 1024 + 123u32)
            .map(|i| (i % 251) as u8)
            .collect();
        check(got == want, "its bytes are the file's", &mut fails);
        check(
            PROGRESS.load(Ordering::SeqCst) >= 5,
            "progress reported per piece",
            &mut fails,
        );
        let again =
            unsafe { (plugin.get)(remote.as_ptr(), wide(&local).as_ptr(), 0, std::ptr::null()) };
        check(
            again == 1,
            "an existing file is not overwritten unasked (FS_FILE_EXISTS)",
            &mut fails,
        );
        let moved = unsafe {
            (plugin.get)(
                remote.as_ptr(),
                wide(&local).as_ptr(),
                1 | 4,
                std::ptr::null(),
            )
        };
        check(
            moved == 6,
            "moving (deleting the original) is refused",
            &mut fails,
        );

        let report = format!("{user}\\Документы\\отчёт.txt");
        let local2 = format!("{out_dir}\\отчёт.txt");
        let code = unsafe {
            (plugin.get)(
                wide(&report).as_ptr(),
                wide(&local2).as_ptr(),
                1,
                std::ptr::null(),
            )
        };
        check(
            code == 0 && std::fs::read(&local2).unwrap_or_default() == "привет\n".as_bytes(),
            "a Cyrillic file copied out",
            &mut fails,
        );
        println!("=== {fails} failed");
        std::process::exit(if fails == 0 { 0 } else { 1 });
    }
}
