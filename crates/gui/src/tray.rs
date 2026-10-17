//! The icon in the notification area: «the service is running», and a click
//! that opens the window.
//!
//! A Windows service lives in session 0 and has no desktop to put an icon on,
//! so the icon belongs to a process of the person who is logged in. That
//! process is this same exe started as `own-backup-gui.exe --tray`: a hidden
//! window and a message loop, with neither Tauri nor a WebView behind them —
//! it sits there all day and should cost nothing.
//!
//! The icon is shown exactly while the service is RUNNING and taken away when
//! it is not, so its presence is the answer to «is my backup service up» and
//! never a decoration. The service is asked through the SCM every few seconds;
//! any user may ask that.
//!
//! Whether the icon is wanted is one person's choice about their own desktop,
//! so it is kept in their registry hive and not in `config.toml`, which is the
//! machine's and shared with the service. The logon entry (`Run`) follows that
//! choice and is rewritten at every start of the window, which also keeps it
//! pointing at the exe that is actually in use.

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, BOOL, ERROR_ALREADY_EXISTS, FALSE, HWND, LPARAM, LRESULT, POINT,
    TRUE, WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_DWORD, REG_SZ,
    RRF_RT_REG_DWORD,
};
use windows::Win32::System::Services::{
    CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceStatus, SC_MANAGER_CONNECT,
    SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_STATUS,
};
use windows::Win32::System::Threading::{
    CreateMutexW, OpenMutexW, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, SYNCHRONIZATION_SYNCHRONIZE,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, EnumWindows, FindWindowW, GetCursorPos, GetMessageW, GetSystemMetrics,
    GetWindow, GetWindowLongPtrW, GetWindowTextLengthW, GetWindowThreadProcessId, IsIconic,
    IsWindowVisible, LoadIconW, LoadImageW, PostMessageW, PostQuitMessage, RegisterClassW,
    RegisterWindowMessageW, SetForegroundWindow, SetMenuDefaultItem, SetTimer, ShowWindow,
    TrackPopupMenuEx, TranslateMessage, GWL_EXSTYLE, GW_OWNER, HICON, IDI_APPLICATION, IMAGE_ICON,
    LR_DEFAULTCOLOR, MF_SEPARATOR, MF_STRING, MSG, SM_CXSMICON, SM_CYSMICON, SW_RESTORE,
    TPM_BOTTOMALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WM_APP, WM_CLOSE, WM_DESTROY,
    WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WM_TIMER, WNDCLASSW, WS_EX_TOOLWINDOW, WS_OVERLAPPED,
};

use own_backup_common::config::AppConfig;

/// Class of the hidden window. Also how the window finds a running icon to
/// tell it to go away.
const WINDOW_CLASS: PCWSTR = w!("OwnBackupTrayWindow");
/// What the notification area sends our window about the icon.
const TRAY_MESSAGE: u32 = WM_APP + 1;
const ICON_ID: u32 = 1;
const POLL_TIMER: usize = 1;
/// How often the service is asked. Slow enough to cost nothing, fast enough
/// that the icon follows a restart of the service while someone is looking.
const POLL_MS: u32 = 5000;
const CMD_OPEN: usize = 1;
const CMD_HIDE: usize = 2;
/// The id tauri-build gives the exe's icon (`IDI_APPLICATION`).
const APP_ICON_RESOURCE: usize = 32512;

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("OwnBackupTray");
const PREF_KEY: PCWSTR = w!("Software\\OwnBackup");
const PREF_VALUE: PCWSTR = w!("TrayIcon");

/// The icon is in the notification area right now.
static SHOWN: AtomicBool = AtomicBool::new(false);
/// `TaskbarCreated`, registered at start: Explorer broadcasts it when it comes
/// back from a restart, having lost every icon it was showing.
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
/// The loaded icon, kept for the life of the process.
static ICON: AtomicIsize = AtomicIsize::new(0);
/// The tooltip as last given to the shell, to rewrite it only when the
/// language changed under us.
static TIP: Mutex<String> = Mutex::new(String::new());
/// One line in the log for an icon the shell would not take, not one per tick.
static ADD_FAILED: AtomicBool = AtomicBool::new(false);
/// When the window was last started from here, and for how long after that a
/// click starts nothing. A double click is two clicks 7 ms apart: the second
/// arrives before the first copy has existed long enough to take its mutex,
/// and used to start a second copy that could only answer «already running».
static WINDOW_STARTED: Mutex<Option<Instant>> = Mutex::new(None);
const START_GRACE: Duration = Duration::from_secs(5);

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---- the wish and the logon entry (called from the window) ----------------

/// Whether this person wants the icon. Wanted until they say otherwise.
pub fn enabled() -> bool {
    let mut value = 0u32;
    let mut bytes = std::mem::size_of::<u32>() as u32;
    // SAFETY: the buffer and its size travel together; a missing value is a
    // nonzero status and leaves `value` alone.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PREF_KEY,
            PREF_VALUE,
            RRF_RT_REG_DWORD,
            None,
            Some((&mut value as *mut u32).cast()),
            Some(&mut bytes),
        )
    };
    status.0 != 0 || value != 0
}

/// Remember the wish and act on it at once.
pub fn set_enabled(on: bool) -> Result<(), String> {
    let value = u32::from(on);
    // SAFETY: four bytes of a live u32; the key is created if it is missing.
    unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            PREF_KEY,
            PREF_VALUE,
            REG_DWORD.0,
            Some((&value as *const u32).cast()),
            std::mem::size_of::<u32>() as u32,
        )
    }
    .ok()
    .map_err(|error| error.to_string())?;
    apply();
    Ok(())
}

/// Make the logon entry and the running icon match the wish.
///
/// Called at every start of the window, not only when the setting changes: the
/// entry holds a path, and a folder that was moved or a newer copy started from
/// somewhere else would otherwise leave Windows launching an exe that is gone.
pub fn apply() {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            tracing::warn!(%error, "tray: current_exe() failed, logon entry left as it is");
            return;
        }
    };
    if enabled() {
        let command = wide(&format!("\"{}\" --tray", exe.display()));
        // SAFETY: a null-terminated UTF-16 string and its length in bytes.
        let status = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                RUN_VALUE,
                REG_SZ.0,
                Some(command.as_ptr().cast()),
                (command.len() * std::mem::size_of::<u16>()) as u32,
            )
        };
        if status.0 != 0 {
            tracing::warn!(status = status.0, "tray: could not write the logon entry");
        }
        // A second one finds the first and leaves, so this needs no check.
        if let Err(error) = std::process::Command::new(&exe).arg("--tray").spawn() {
            tracing::warn!(%error, "tray: could not start the icon");
        }
    } else {
        // SAFETY: constant strings; a value that is not there is not an error
        // worth reporting.
        unsafe {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE);
            if let Ok(window) = FindWindowW(WINDOW_CLASS, PCWSTR::null()) {
                let _ = PostMessageW(window, WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
    }
}

// ---- the icon's own process (`--tray`) ------------------------------------

/// `own-backup-gui.exe --tray`. Returns when the icon is asked to go.
pub fn run() {
    // One icon per logged-in person. Session-local on purpose: with two people
    // signed in, each desktop has its own notification area.
    // SAFETY: a constant name; the handle is leaked so the mutex lives as long
    // as the process does.
    let mutex = unsafe { CreateMutexW(None, true, w!("Local\\OwnBackupTray")) };
    if mutex.is_err() || unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return;
    }
    std::mem::forget(mutex);

    init_log();
    own_backup_common::lang::follow_config_or_system(AppConfig::default_path());
    tracing::info!(pid = std::process::id(), "tray: started");
    if let Err(error) = message_loop() {
        tracing::error!(%error, "tray: no window, so no icon");
    }
    tracing::info!("tray: stopped");
}

/// A log of its own beside the window's: the two are separate processes, and
/// «the icon did not appear» must be answerable without the other one's noise.
fn init_log() {
    let data_dir = AppConfig::default().data_dir;
    let Some(dir) = own_backup_common::log_files::log_dir(&data_dir) else {
        return;
    };
    let log = own_backup_common::log_files::DailyLog::new(dir, "own-backup-tray");
    let _ = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer(Mutex::new(log))
        .try_init();
}

fn message_loop() -> windows::core::Result<()> {
    // SAFETY: plain window-class registration and creation; every pointer is a
    // constant or outlives the call.
    let window = unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: WINDOW_CLASS,
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            return Err(windows::core::Error::from_win32());
        }
        // Never shown — and still a real top-level window rather than a
        // message-only one: `TaskbarCreated` is a broadcast, and broadcasts
        // do not reach message-only windows.
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            WINDOW_CLASS,
            w!("Own Backup tray"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            instance,
            None,
        )?
    };
    // SAFETY: a constant string.
    TASKBAR_CREATED.store(
        unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) },
        Ordering::Relaxed,
    );

    follow_service(window);
    // SAFETY: a live window of this thread.
    unsafe {
        SetTimer(window, POLL_TIMER, POLL_MS, None);
    }

    let mut msg = MSG::default();
    // -1 is an error and is not false: `> 0`, or a broken queue spins forever.
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
        // SAFETY: `msg` was just filled by GetMessageW.
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_TIMER if wparam.0 == POLL_TIMER => {
            follow_service(window);
            LRESULT(0)
        }
        TRAY_MESSAGE => {
            match lparam.0 as u32 {
                WM_LBUTTONUP => open_window(),
                WM_RBUTTONUP => menu(window),
                _ => {}
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            remove_icon(window);
            PostQuitMessage(0);
            LRESULT(0)
        }
        other if other != 0 && other == TASKBAR_CREATED.load(Ordering::Relaxed) => {
            // Explorer is back from a restart and shows nothing of ours.
            SHOWN.store(false, Ordering::Relaxed);
            follow_service(window);
            LRESULT(0)
        }
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}

/// Show the icon while the service runs, take it away when it does not.
fn follow_service(window: HWND) {
    let running = service_running();
    let shown = SHOWN.load(Ordering::Relaxed);
    if running && !shown {
        if add_icon(window) {
            SHOWN.store(true, Ordering::Relaxed);
            ADD_FAILED.store(false, Ordering::Relaxed);
            tracing::info!("tray: the service is running, icon shown");
        } else if !ADD_FAILED.swap(true, Ordering::Relaxed) {
            // Right after logon the notification area may not exist yet.
            // The next tick tries again.
            tracing::warn!("tray: the shell did not take the icon yet, retrying");
        }
    } else if !running && shown {
        remove_icon(window);
        tracing::info!("tray: the service is not running, icon hidden");
    } else if running {
        refresh_tip(window);
    }
}

fn service_running() -> bool {
    let name = wide(own_backup_common::WINDOWS_SERVICE_NAME);
    let mut running = false;
    // SAFETY: a null machine/database means the local one; `name` outlives the
    // calls and each handle is closed once.
    unsafe {
        if let Ok(scm) = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT) {
            if let Ok(service) = OpenServiceW(scm, PCWSTR(name.as_ptr()), SERVICE_QUERY_STATUS) {
                let mut status = SERVICE_STATUS::default();
                if QueryServiceStatus(service, &mut status).is_ok() {
                    running = status.dwCurrentState == SERVICE_RUNNING;
                }
                let _ = CloseServiceHandle(service);
            }
            let _ = CloseServiceHandle(scm);
        }
    }
    running
}

fn icon() -> HICON {
    let cached = ICON.load(Ordering::Relaxed);
    if cached != 0 {
        return HICON(cached as *mut core::ffi::c_void);
    }
    // SAFETY: the resource id is passed the way MAKEINTRESOURCE does it; the
    // handle is kept for the life of the process.
    let loaded = unsafe {
        let instance = GetModuleHandleW(None).unwrap_or_default();
        match LoadImageW(
            instance,
            PCWSTR(APP_ICON_RESOURCE as *const u16),
            IMAGE_ICON,
            GetSystemMetrics(SM_CXSMICON),
            GetSystemMetrics(SM_CYSMICON),
            LR_DEFAULTCOLOR,
        ) {
            Ok(handle) => HICON(handle.0),
            Err(error) => {
                tracing::warn!(%error, "tray: the exe's own icon did not load, using the stock one");
                LoadIconW(None, IDI_APPLICATION).unwrap_or_default()
            }
        }
    };
    ICON.store(loaded.0 as isize, Ordering::Relaxed);
    loaded
}

fn icon_data(window: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: window,
        uID: ICON_ID,
        ..Default::default()
    }
}

/// Write the tooltip into `data`; the shell takes at most 127 characters.
fn put_tip(data: &mut NOTIFYICONDATAW, tip: &str) {
    let text = wide(tip);
    let room = data.szTip.len() - 1;
    let length = text.len().saturating_sub(1).min(room);
    data.szTip[..length].copy_from_slice(&text[..length]);
    data.szTip[length] = 0;
}

fn add_icon(window: HWND) -> bool {
    let tip = own_backup_common::tr!("tray.tooltip").into_owned();
    let mut data = icon_data(window);
    data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    data.uCallbackMessage = TRAY_MESSAGE;
    data.hIcon = icon();
    put_tip(&mut data, &tip);
    // SAFETY: a fully initialised structure of the size it declares.
    let added = unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool();
    if added {
        if let Ok(mut last) = TIP.lock() {
            *last = tip;
        }
    }
    added
}

fn remove_icon(window: HWND) {
    if SHOWN.swap(false, Ordering::Relaxed) {
        // SAFETY: as above; removing an icon that is already gone is harmless.
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &icon_data(window));
        }
    }
}

/// The tooltip follows a change of language made in the window.
fn refresh_tip(window: HWND) {
    let tip = own_backup_common::tr!("tray.tooltip").into_owned();
    let Ok(mut last) = TIP.lock() else {
        return;
    };
    if *last == tip {
        return;
    }
    let mut data = icon_data(window);
    data.uFlags = NIF_TIP;
    put_tip(&mut data, &tip);
    // SAFETY: as in `add_icon`.
    if unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) }.as_bool() {
        *last = tip;
    }
}

fn menu(window: HWND) {
    let open = wide(&own_backup_common::tr!("tray.open"));
    let hide = wide(&own_backup_common::tr!("tray.hide"));
    // SAFETY: the strings outlive the menu, and the menu is destroyed here.
    let chosen = unsafe {
        let Ok(popup) = CreatePopupMenu() else {
            return;
        };
        let _ = AppendMenuW(popup, MF_STRING, CMD_OPEN, PCWSTR(open.as_ptr()));
        let _ = AppendMenuW(popup, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(popup, MF_STRING, CMD_HIDE, PCWSTR(hide.as_ptr()));
        // Bold: it is what a plain click does.
        let _ = SetMenuDefaultItem(popup, CMD_OPEN as u32, 0);

        let mut at = POINT::default();
        let _ = GetCursorPos(&mut at);
        // A popup whose owner is not in front does not close when the person
        // clicks past it; the WM_NULL afterwards is the other half of the
        // same documented workaround.
        let _ = SetForegroundWindow(window);
        let picked = TrackPopupMenuEx(
            popup,
            (TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN).0,
            at.x,
            at.y,
            window,
            None,
        )
        .0;
        let _ = PostMessageW(window, WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(popup);
        picked as usize
    };
    match chosen {
        CMD_OPEN => open_window(),
        // Until the next logon or the next start of the window; switching the
        // icon off for good is in the settings.
        CMD_HIDE => unsafe {
            let _ = DestroyWindow(window);
        },
        _ => {}
    }
}

/// Bring the window forward, or start it.
fn open_window() {
    if let Some(window) = gui_window() {
        // SAFETY: a window handle the system just gave us. We are answering a
        // click on our icon, so we are allowed to move the foreground.
        unsafe {
            if IsIconic(window).as_bool() {
                let _ = ShowWindow(window, SW_RESTORE);
            }
            let _ = SetForegroundWindow(window);
        }
        return;
    }
    // Started but its window is not up yet: a second click must not launch a
    // second copy, which would only answer «already running».
    if gui_holds_its_mutex() {
        return;
    }
    let mut started = WINDOW_STARTED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if started.is_some_and(|at| at.elapsed() < START_GRACE) {
        return;
    }
    match std::env::current_exe() {
        Ok(exe) => match std::process::Command::new(&exe).spawn() {
            Ok(_) => {
                *started = Some(Instant::now());
                tracing::info!(exe = %exe.display(), "tray: window started")
            }
            Err(error) => {
                tracing::error!(%error, exe = %exe.display(), "tray: window did not start")
            }
        },
        Err(error) => tracing::error!(%error, "tray: current_exe() failed"),
    }
}

fn gui_holds_its_mutex() -> bool {
    let name = wide(crate::GUI_INSTANCE_MUTEX);
    // SAFETY: a null-terminated name; the handle is closed at once.
    match unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(name.as_ptr())) } {
        Ok(handle) => {
            let _ = unsafe { CloseHandle(handle) };
            true
        }
        Err(_) => false,
    }
}

struct Search {
    own_pid: u32,
    /// File name of this exe, lower case: the window is the same file run
    /// without `--tray`.
    exe_name: String,
    found: Option<isize>,
}

/// The interface's main window, if it is open: a visible, titled, unowned
/// top-level window of another process running this same exe.
///
/// «Titled» and «not a tool window» are there for a reason found by running
/// it: the window's process also owns `Tao Thread Event Target` — visible,
/// unowned, nameless and never on screen. A minimised main window sinks below
/// it in the Z-order, so it was the one found, and the click restored nothing.
fn gui_window() -> Option<HWND> {
    let exe_name = std::env::current_exe()
        .ok()?
        .file_name()?
        .to_string_lossy()
        .to_lowercase();
    let mut search = Search {
        own_pid: std::process::id(),
        exe_name,
        found: None,
    };
    // SAFETY: `search` outlives the enumeration. The call reports an error
    // when the callback stops it early, which is how a hit ends it.
    unsafe {
        let _ = EnumWindows(Some(look), LPARAM(&mut search as *mut Search as isize));
    }
    search
        .found
        .map(|handle| HWND(handle as *mut core::ffi::c_void))
}

unsafe extern "system" fn look(window: HWND, data: LPARAM) -> BOOL {
    let search = &mut *(data.0 as *mut Search);
    // Owned windows are dialogs of the one we are after.
    if !IsWindowVisible(window).as_bool() || GetWindow(window, GW_OWNER).is_ok() {
        return TRUE;
    }
    let extended = GetWindowLongPtrW(window, GWL_EXSTYLE) as u32;
    if extended & WS_EX_TOOLWINDOW.0 != 0 || GetWindowTextLengthW(window) == 0 {
        return TRUE;
    }
    let mut pid = 0u32;
    GetWindowThreadProcessId(window, Some(&mut pid));
    if pid == 0 || pid == search.own_pid || !runs_exe(pid, &search.exe_name) {
        return TRUE;
    }
    search.found = Some(window.0 as isize);
    FALSE
}

unsafe fn runs_exe(pid: u32, exe_name: &str) -> bool {
    let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
        return false;
    };
    let mut path = [0u16; 1024];
    let mut length = path.len() as u32;
    let known = QueryFullProcessImageNameW(
        process,
        PROCESS_NAME_WIN32,
        PWSTR(path.as_mut_ptr()),
        &mut length,
    )
    .is_ok();
    let _ = CloseHandle(process);
    if !known {
        return false;
    }
    let path = String::from_utf16_lossy(&path[..length as usize]);
    std::path::Path::new(&path)
        .file_name()
        .is_some_and(|name| name.to_string_lossy().to_lowercase() == exe_name)
}
