//! The functions Total Commander calls (its "fsplugin.h", WFX interface 2.x).
//!
//! Read-only by what is NOT here: no FsPutFile, FsMkDir, FsDeleteFile,
//! FsRenMovFile, FsRemoveDir, FsSetAttr or FsSetTime — Total Commander then
//! offers none of those operations on these panels. Copying out (F5),
//! viewing (F3) and opening (Enter) go through FsGetFile.
//!
//! The Unicode functions are the real ones; the ANSI ones every plugin must
//! also export convert and call them. The plugin is one state behind a lock
//! that is only ever TRIED: a dialog of Total Commander's (the passphrase)
//! runs a message loop, and a call arriving through it must fail rather than
//! wait for a lock its own thread holds.

#![allow(non_snake_case)]

use std::ffi::{c_char, c_void, OsString};
use std::io::Write;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use own_backup_common::tr;
use windows::Win32::Foundation::{
    SetLastError, BOOL, ERROR_NO_MORE_FILES, ERROR_PATH_NOT_FOUND, HWND,
};
use windows::Win32::Globalization::{
    MultiByteToWideChar, WideCharToMultiByte, CP_ACP, MULTI_BYTE_TO_WIDE_CHAR_FLAGS,
};
use windows::Win32::Storage::FileSystem::{WIN32_FIND_DATAA, WIN32_FIND_DATAW};
use zeroize::Zeroizing;

use crate::browse::{filetime, BrowseError, Browser, Entry, Ui};
use crate::rpc::Grpc;

const FS_FILE_OK: i32 = 0;
const FS_FILE_EXISTS: i32 = 1;
const FS_FILE_NOTFOUND: i32 = 2;
const FS_FILE_READERROR: i32 = 3;
const FS_FILE_WRITEERROR: i32 = 4;
const FS_FILE_USERABORT: i32 = 5;
const FS_FILE_NOTSUPPORTED: i32 = 6;

const FS_EXEC_ERROR: i32 = 1;
const FS_EXEC_YOURSELF: i32 = -1;

const FS_COPYFLAGS_OVERWRITE: i32 = 1;
const FS_COPYFLAGS_RESUME: i32 = 2;
const FS_COPYFLAGS_MOVE: i32 = 4;

const RT_PASSWORD: i32 = 2;
const RT_MSG_OK: i32 = 8;

const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;

/// One piece of a copy: the service's limit per call.
const CHUNK: usize = 1 << 20;

const INVALID: *mut c_void = -1isize as *mut c_void;

type ProgressProcW = unsafe extern "system" fn(i32, *const u16, *const u16, i32) -> i32;
type LogProcW = unsafe extern "system" fn(i32, i32, *const u16);
type RequestProcW =
    unsafe extern "system" fn(i32, i32, *const u16, *const u16, *mut u16, i32) -> BOOL;

#[derive(Clone, Copy, Default)]
struct Callbacks {
    nr: i32,
    progress: Option<ProgressProcW>,
    request: Option<RequestProcW>,
}

#[repr(C)]
pub struct RemoteInfoStruct {
    size_low: u32,
    size_high: u32,
    last_write_time: u64,
    attr: i32,
}

static CALLBACKS: Mutex<Callbacks> = Mutex::new(Callbacks {
    nr: 0,
    progress: None,
    request: None,
});
static STATE: Mutex<Option<Browser<Grpc>>> = Mutex::new(None);
/// The last error shown, and when: a panel refreshed in a loop must not
/// bring the same message box up again and again.
static LAST_SHOWN: Mutex<Option<(String, Instant)>> = Mutex::new(None);

fn callbacks() -> Callbacks {
    CALLBACKS.lock().map(|c| *c).unwrap_or_default()
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A NUL-terminated UTF-16 string from Total Commander.
unsafe fn from_wide(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut n = 0;
    while *p.add(n) != 0 {
        n += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
}

unsafe fn from_wide_os(p: *const u16) -> OsString {
    let mut n = 0;
    while *p.add(n) != 0 {
        n += 1;
    }
    OsString::from_wide(std::slice::from_raw_parts(p, n))
}

/// An ANSI string from Total Commander, as UTF-16 (with its NUL).
unsafe fn ansi_to_wide(p: *const c_char) -> Vec<u16> {
    if p.is_null() {
        return vec![0];
    }
    let bytes = std::ffi::CStr::from_ptr(p).to_bytes();
    let n = MultiByteToWideChar(CP_ACP, MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0), bytes, None);
    let mut out = vec![0u16; n.max(0) as usize + 1];
    MultiByteToWideChar(
        CP_ACP,
        MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0),
        bytes,
        Some(&mut out[..n.max(0) as usize]),
    );
    out
}

struct TcUi(Callbacks);

impl Ui for TcUi {
    fn passphrase(&mut self, name: &str, retry: bool) -> Option<Zeroizing<String>> {
        let request = self.0.request?;
        let title = wide(&tr!("tc.title"));
        let text = if retry {
            tr!("tc.passphrase_again", name = name)
        } else {
            tr!("tc.passphrase", name = name)
        };
        let text = wide(&text);
        let mut buf = Zeroizing::new(vec![0u16; 1024]);
        // SAFETY: Total Commander's callback; the buffers outlive the call
        // and the length is the buffer's.
        let ok = unsafe {
            request(
                self.0.nr,
                RT_PASSWORD,
                title.as_ptr(),
                text.as_ptr(),
                buf.as_mut_ptr(),
                buf.len() as i32,
            )
        };
        if !ok.as_bool() {
            return None;
        }
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(Zeroizing::new(String::from_utf16_lossy(&buf[..n])))
    }
}

/// Tell the person something in Total Commander's own message box.
fn tell(text: &str) {
    if let Ok(mut last) = LAST_SHOWN.lock() {
        if matches!(&*last, Some((t, at)) if t == text && at.elapsed() < Duration::from_secs(5)) {
            return;
        }
        *last = Some((text.to_string(), Instant::now()));
    }
    let cb = callbacks();
    let Some(request) = cb.request else {
        return;
    };
    let title = wide(&tr!("tc.title"));
    let text = wide(text);
    let mut none = [0u16; 2];
    // SAFETY: as in `passphrase`.
    let _ = unsafe {
        request(
            cb.nr,
            RT_MSG_OK,
            title.as_ptr(),
            text.as_ptr(),
            none.as_mut_ptr(),
            none.len() as i32,
        )
    };
}

fn report(e: &BrowseError, path: &str) {
    match e {
        BrowseError::Cancelled => {}
        BrowseError::NotFound => tell(&tr!("tc.not_found", path = path)),
        BrowseError::Failed(why) => tell(why),
    }
}

/// Run `f` on the plugin's state: `None` when it is busy (a call through a
/// dialog's message loop) or cannot be made at all.
fn with_browser<T>(f: impl FnOnce(&mut Browser<Grpc>, &mut TcUi) -> T) -> Option<T> {
    let mut guard = STATE.try_lock().ok()?;
    if guard.is_none() {
        match Grpc::new() {
            Ok(g) => *guard = Some(Browser::new(g)),
            Err(e) => {
                drop(guard);
                tell(&tr!("tc.no_service", error = e));
                return None;
            }
        }
    }
    let mut ui = TcUi(callbacks());
    Some(f(guard.as_mut()?, &mut ui))
}

fn init(nr: i32, progress: Option<ProgressProcW>, request: Option<RequestProcW>) {
    if let Ok(mut cb) = CALLBACKS.lock() {
        *cb = Callbacks {
            nr,
            progress,
            request,
        };
    }
    let config = own_backup_common::config::AppConfig::default_path();
    let chosen = own_backup_common::lang::configured(&config);
    own_backup_common::lang::set(if chosen.trim().is_empty() {
        own_backup_common::lang::system()
    } else {
        &chosen
    });
}

// --- listing ----------------------------------------------------------------------

struct Find {
    items: Vec<Entry>,
    next: usize,
}

fn attributes(e: &Entry) -> u32 {
    FILE_ATTRIBUTE_READONLY | if e.dir { FILE_ATTRIBUTE_DIRECTORY } else { 0 }
}

unsafe fn fill_w(e: &Entry, fd: *mut WIN32_FIND_DATAW) {
    let fd = &mut *fd;
    *fd = std::mem::zeroed();
    fd.dwFileAttributes = attributes(e);
    fd.nFileSizeHigh = (e.size >> 32) as u32;
    fd.nFileSizeLow = e.size as u32;
    let ft = filetime(e.modified, e.nanos);
    fd.ftLastWriteTime.dwLowDateTime = ft as u32;
    fd.ftLastWriteTime.dwHighDateTime = (ft >> 32) as u32;
    let name: Vec<u16> = e.name.encode_utf16().take(fd.cFileName.len() - 1).collect();
    fd.cFileName[..name.len()].copy_from_slice(&name);
}

unsafe fn fill_a(e: &Entry, fd: *mut WIN32_FIND_DATAA) {
    let mut w: WIN32_FIND_DATAW = std::mem::zeroed();
    fill_w(e, &mut w);
    let fd = &mut *fd;
    *fd = std::mem::zeroed();
    fd.dwFileAttributes = w.dwFileAttributes;
    fd.nFileSizeHigh = w.nFileSizeHigh;
    fd.nFileSizeLow = w.nFileSizeLow;
    fd.ftLastWriteTime = w.ftLastWriteTime;
    let n = w.cFileName.iter().position(|&c| c == 0).unwrap_or(0);
    let out: &mut [u8] = std::slice::from_raw_parts_mut(
        fd.cFileName.as_mut_ptr().cast::<u8>(),
        fd.cFileName.len() - 1,
    );
    WideCharToMultiByte(CP_ACP, 0, &w.cFileName[..n], Some(out), None, None);
}

fn find_first(path: &str) -> Option<Box<Find>> {
    match with_browser(|b, ui| b.list(ui, path))? {
        Ok(items) if items.is_empty() => {
            // SAFETY: plain thread-local error code.
            unsafe { SetLastError(ERROR_NO_MORE_FILES) };
            None
        }
        Ok(items) => Some(Box::new(Find { items, next: 0 })),
        Err(e) => {
            report(&e, path);
            // SAFETY: as above.
            unsafe { SetLastError(ERROR_PATH_NOT_FOUND) };
            None
        }
    }
}

/// # Safety
/// Called by Total Commander with a valid path and find data.
#[no_mangle]
pub unsafe extern "system" fn FsFindFirstW(
    path: *const u16,
    fd: *mut WIN32_FIND_DATAW,
) -> *mut c_void {
    let Some(mut find) = find_first(&from_wide(path)) else {
        return INVALID;
    };
    fill_w(&find.items[0], fd);
    find.next = 1;
    Box::into_raw(find).cast()
}

/// # Safety
/// `handle` is one FsFindFirstW returned and FsFindClose has not freed.
#[no_mangle]
pub unsafe extern "system" fn FsFindNextW(handle: *mut c_void, fd: *mut WIN32_FIND_DATAW) -> BOOL {
    let find = &mut *(handle as *mut Find);
    match find.items.get(find.next) {
        Some(e) => {
            fill_w(e, fd);
            find.next += 1;
            BOOL(1)
        }
        None => BOOL(0),
    }
}

/// # Safety
/// As FsFindFirstW.
#[no_mangle]
pub unsafe extern "system" fn FsFindFirst(
    path: *const c_char,
    fd: *mut WIN32_FIND_DATAA,
) -> *mut c_void {
    let path = ansi_to_wide(path);
    let Some(mut find) = find_first(&from_wide(path.as_ptr())) else {
        return INVALID;
    };
    fill_a(&find.items[0], fd);
    find.next = 1;
    Box::into_raw(find).cast()
}

/// # Safety
/// As FsFindNextW.
#[no_mangle]
pub unsafe extern "system" fn FsFindNext(handle: *mut c_void, fd: *mut WIN32_FIND_DATAA) -> BOOL {
    let find = &mut *(handle as *mut Find);
    match find.items.get(find.next) {
        Some(e) => {
            fill_a(e, fd);
            find.next += 1;
            BOOL(1)
        }
        None => BOOL(0),
    }
}

/// # Safety
/// `handle` came from FsFindFirst(W), and is freed here once.
#[no_mangle]
pub unsafe extern "system" fn FsFindClose(handle: *mut c_void) -> i32 {
    if !handle.is_null() && handle != INVALID {
        drop(Box::from_raw(handle as *mut Find));
    }
    0
}

// --- copying out ------------------------------------------------------------------

fn get_file(remote: &str, local: PathBuf, flags: i32, info: Option<&RemoteInfoStruct>) -> i32 {
    if flags & FS_COPYFLAGS_MOVE != 0 {
        tell(&tr!("tc.read_only"));
        return FS_FILE_NOTSUPPORTED;
    }
    if flags & FS_COPYFLAGS_RESUME != 0 {
        return FS_FILE_NOTSUPPORTED;
    }
    if local.exists() && flags & FS_COPYFLAGS_OVERWRITE == 0 {
        return FS_FILE_EXISTS;
    }
    let (size, modified) = match info {
        Some(i) => (
            (u64::from(i.size_high) << 32) | u64::from(i.size_low),
            i.last_write_time,
        ),
        None => match with_browser(|b, ui| b.stat(ui, remote)) {
            Some(Ok(e)) => (e.size, filetime(e.modified, e.nanos)),
            Some(Err(e)) => {
                report(&e, remote);
                return FS_FILE_NOTFOUND;
            }
            None => return FS_FILE_READERROR,
        },
    };
    let Ok(mut file) = std::fs::File::create(&local) else {
        return FS_FILE_WRITEERROR;
    };
    let cb = callbacks();
    let (remote_w, local_w) = (wide(remote), wide(&local.to_string_lossy()));
    let mut done: u64 = 0;
    let fail = |code: i32, file: std::fs::File| {
        drop(file);
        let _ = std::fs::remove_file(&local);
        code
    };
    loop {
        let piece = match with_browser(|b, ui| b.read(ui, remote, done, CHUNK)) {
            Some(Ok(p)) => p,
            Some(Err(e)) => {
                report(&e, remote);
                return fail(FS_FILE_READERROR, file);
            }
            None => return fail(FS_FILE_READERROR, file),
        };
        if piece.is_empty() {
            break;
        }
        if file.write_all(&piece).is_err() {
            return fail(FS_FILE_WRITEERROR, file);
        }
        done += piece.len() as u64;
        let percent = (done.min(size) * 100)
            .checked_div(size)
            .map_or(100, |p| p as i32);
        if let Some(progress) = cb.progress {
            // SAFETY: Total Commander's callback with terminated strings.
            if unsafe { progress(cb.nr, remote_w.as_ptr(), local_w.as_ptr(), percent) } != 0 {
                return fail(FS_FILE_USERABORT, file);
            }
        }
        if done >= size && size > 0 {
            break;
        }
    }
    if modified > 0 {
        let since_1601 = Duration::from_nanos(modified.saturating_mul(100));
        let epoch_gap = Duration::from_secs(11_644_473_600);
        if let Some(t) = since_1601.checked_sub(epoch_gap) {
            let _ = file.set_modified(SystemTime::UNIX_EPOCH + t);
        }
    }
    FS_FILE_OK
}

/// # Safety
/// Called by Total Commander with terminated names and `ri` either null or valid.
#[no_mangle]
pub unsafe extern "system" fn FsGetFileW(
    remote: *const u16,
    local: *const u16,
    flags: i32,
    ri: *const RemoteInfoStruct,
) -> i32 {
    get_file(
        &from_wide(remote),
        PathBuf::from(from_wide_os(local)),
        flags,
        ri.as_ref(),
    )
}

/// # Safety
/// As FsGetFileW.
#[no_mangle]
pub unsafe extern "system" fn FsGetFile(
    remote: *const c_char,
    local: *const c_char,
    flags: i32,
    ri: *const RemoteInfoStruct,
) -> i32 {
    let (remote, local) = (ansi_to_wide(remote), ansi_to_wide(local));
    FsGetFileW(remote.as_ptr(), local.as_ptr(), flags, ri)
}

// --- the rest ---------------------------------------------------------------------

/// "open": Total Commander copies the file out and opens it itself. Nothing
/// else is done on these panels (no properties, no chmod).
///
/// # Safety
/// Called by Total Commander with a terminated verb.
#[no_mangle]
pub unsafe extern "system" fn FsExecuteFileW(
    _main: HWND,
    _remote: *mut u16,
    verb: *const u16,
) -> i32 {
    if from_wide(verb).to_ascii_lowercase().starts_with("open") {
        FS_EXEC_YOURSELF
    } else {
        FS_EXEC_ERROR
    }
}

/// # Safety
/// As FsExecuteFileW.
#[no_mangle]
pub unsafe extern "system" fn FsExecuteFile(
    main: HWND,
    remote: *mut c_char,
    verb: *const c_char,
) -> i32 {
    let verb = ansi_to_wide(verb);
    FsExecuteFileW(main, remote.cast(), verb.as_ptr())
}

/// # Safety
/// Called once by Total Commander with its callbacks.
#[no_mangle]
pub unsafe extern "system" fn FsInitW(
    nr: i32,
    progress: Option<ProgressProcW>,
    _log: Option<LogProcW>,
    request: Option<RequestProcW>,
) -> i32 {
    init(nr, progress, request);
    0
}

/// The ANSI initialisation: a Unicode Total Commander calls FsInitW; an older
/// one gets no dialogs (its callbacks are of the other kind).
///
/// # Safety
/// Called once by Total Commander.
#[no_mangle]
pub unsafe extern "system" fn FsInit(
    nr: i32,
    _progress: *const c_void,
    _log: *const c_void,
    _request: *const c_void,
) -> i32 {
    init(nr, None, None);
    0
}

/// The name in Total Commander's network neighbourhood.
///
/// # Safety
/// `name` holds `maxlen` bytes.
#[no_mangle]
pub unsafe extern "system" fn FsGetDefRootName(name: *mut c_char, maxlen: i32) {
    let text = b"Own Backup Linux";
    let n = text.len().min(maxlen.max(1) as usize - 1);
    std::ptr::copy_nonoverlapping(text.as_ptr(), name.cast::<u8>(), n);
    *name.add(n) = 0;
}
