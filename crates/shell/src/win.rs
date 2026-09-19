//! The shell itself: a backdrop, a strip along the bottom of the screen, a
//! clock, and the keyboard layout of whatever window has the focus.
//!
//! ## What it must not do
//!
//! Hold anything. The medium's first console trapped its user by running the
//! interface in the foreground and never coming back; the same mistake here is
//! bigger, because when this process is the shell there is nothing else on the
//! screen. So: no waiting on a child, no dialog, no state, and every failure
//! logged and survived rather than reported by dying.
//!
//! ## The layout indicator is the reason this exists
//!
//! The medium installs two keyboard layouts and shows no sign of which one is
//! active, while asking for a BitLocker passphrase and a 48-digit recovery key.
//! Typed in the wrong layout those come back as "wrong password", which sends
//! the person looking in entirely the wrong place.
//!
//! Two consequences run through this file. The strip reads the layout of the
//! FOREGROUND window's thread, not its own — each thread has its own, and our
//! own would answer a question nobody asked. And the strip is created
//! `WS_EX_NOACTIVATE`, so clicking it does not take the focus away from the
//! window being typed into: without that, the click that asks to switch layout
//! would change which window the answer is about.

use std::cell::RefCell;
use std::ffi::c_void;
use std::io::Write as _;
use std::mem::size_of;
use std::os::windows::process::CommandExt as _;
use std::path::PathBuf;
use std::sync::OnceLock;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    GetLastError, COLORREF, ERROR_ALREADY_EXISTS, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, ClientToScreen, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint,
    FillRect, GetDC, GetTextExtentPoint32W, InvalidateRect, ReleaseDC, SelectObject, SetBkMode,
    SetTextColor, HDC, HFONT, PAINTSTRUCT, DT_CENTER, DT_END_ELLIPSIS, DT_RIGHT, DT_SINGLELINE,
    DT_VCENTER, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyboardLayout, GetKeyboardLayoutList, HKL};
use windows::Win32::UI::Shell::{
    SHAppBarMessage, ABE_BOTTOM, ABM_NEW, ABM_QUERYPOS, ABM_REMOVE, ABM_SETPOS, ABN_POSCHANGED,
    APPBARDATA,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetClientRect, GetForegroundWindow, GetMessageW, GetSystemMetrics,
    GetWindowThreadProcessId, LoadCursorW, MessageBoxW, PostMessageW, PostQuitMessage,
    RegisterClassW, SetForegroundWindow, SetProcessDPIAware, SetTimer, SetWindowPos, ShowWindow,
    SystemParametersInfoW, TrackPopupMenuEx, TranslateMessage, HWND_BOTTOM, HWND_TOPMOST,
    IDC_ARROW, IDYES, MA_NOACTIVATE, MB_ICONQUESTION, MB_SETFOREGROUND, MB_YESNO, MF_SEPARATOR,
    MF_STRING, MSG, SM_CXSCREEN, SM_CYSCREEN, SPIF_SENDCHANGE, SPI_GETWORKAREA, SPI_SETWORKAREA,
    SWP_NOACTIVATE, SW_SHOWNOACTIVATE, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, TPM_BOTTOMALIGN,
    TPM_LEFTALIGN, TPM_LEFTBUTTON, TPM_RETURNCMD, WM_APP, WM_DESTROY, WM_DISPLAYCHANGE,
    WM_ERASEBKGND, WM_INPUTLANGCHANGEREQUEST, WM_LBUTTONDOWN, WM_MOUSEACTIVATE, WM_NULL, WM_PAINT,
    WM_TIMER, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::layout::{self, BarSlots};
use crate::menu;

/// The appbar's own notifications come back as this message.
const WM_APPBAR: u32 = WM_APP + 1;
/// Timer that moves the clock and re-reads the foreground window's layout.
const TICK: usize = 1;

/// Cycle to the next installed layout. `windows` 0.58 has no name for this
/// flag, so it is spelled out: `INPUTLANGCHANGE_FORWARD` from `winuser.h`.
const INPUTLANGCHANGE_FORWARD: usize = 0x0002;

/// Widest text each fixed-width part of the strip will ever have to show, so
/// the parts keep their place instead of jumping about as the text changes.
/// `0000` is the layout indicator's honest fallback — four hex digits are wider
/// than any two letters.
const LAYOUT_SAMPLE: &str = "0000";
const CLOCK_SAMPLE: &str = "00:00";
/// The Start button. The product's name is on the backdrop behind it, where it
/// does not have to be clicked to be read.
const START_TEXT: &str = "Пуск";
/// The way out, shown only off the boot medium — see [`BarSlots::close`].
const CLOSE_TEXT: &str = "\u{2715}";

const BACKDROP: COLORREF = rgb(15, 48, 87);
const BACKDROP_TEXT: COLORREF = rgb(110, 143, 176);
const BAR_BG: COLORREF = rgb(242, 242, 242);
const BAR_EDGE: COLORREF = rgb(196, 196, 196);
const BAR_TEXT: COLORREF = rgb(26, 26, 26);
/// The Start button, a shade darker than the strip so it reads as something to
/// click rather than as a word printed in the corner.
const BAR_BUTTON: COLORREF = rgb(222, 222, 222);

const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF((r as u32) | ((g as u32) << 8) | ((b as u32) << 16))
}

/// Everything the two windows share. One thread owns it — the message loop —
/// so a thread-local cell is the whole of the locking story.
#[derive(Default)]
struct Bar {
    bar: HWND,
    back: HWND,
    font: HFONT,
    big: HFONT,
    screen_w: i32,
    screen_h: i32,
    /// The width the strip actually got, which is not always the screen's.
    bar_w: i32,
    bar_h: i32,
    slots: BarSlots,
    /// Set while the strip is being placed, so the appbar notification that
    /// placing it provokes does not start the whole thing again.
    placing: bool,
    clock: String,
    layout: String,
    /// The shell accepted us as an appbar and is keeping the work area for us.
    appbar: bool,
    /// The work area as we found it, when we had to set it ourselves.
    saved_work: Option<RECT>,
}

thread_local! {
    static BAR: RefCell<Bar> = RefCell::new(Bar::default());
}

/// Read or change the shared state. Kept to short, Win32-free closures on
/// purpose: a Windows call made while this borrow is held can come straight
/// back into a window procedure and ask for it again.
fn with_bar<R>(f: impl FnOnce(&mut Bar) -> R) -> R {
    BAR.with(|b| f(&mut b.borrow_mut()))
}

/// Start the shell and run until its window closes.
pub fn run() -> windows::core::Result<()> {
    if already_running() {
        log("another shell is already on this desktop - leaving it alone");
        return Ok(());
    }
    unsafe {
        // Without this the strip is placed in virtual coordinates on a scaled
        // display and lands short of the screen's real bottom edge. WinPE runs
        // at 100%, but the same exe is run on an installed Windows to look at.
        let _ = SetProcessDPIAware();
    }

    let instance = unsafe { GetModuleHandleW(None)? };
    let backdrop_class = w!("OwnBackupShellBackdrop");
    let bar_class = w!("OwnBackupShellBar");

    unsafe {
        let cursor = LoadCursorW(None, IDC_ARROW)?;
        for (name, proc) in [
            (backdrop_class, backdrop_proc as WindowProc),
            (bar_class, bar_proc as WindowProc),
        ] {
            let class = WNDCLASSW {
                lpfnWndProc: Some(proc),
                hInstance: instance.into(),
                lpszClassName: name,
                hCursor: cursor,
                ..Default::default()
            };
            if RegisterClassW(&class) == 0 {
                return Err(windows::core::Error::from_win32());
            }
        }
    }

    let (w, h) = screen();
    let back = unsafe {
        CreateWindowExW(
            WS_EX_NOACTIVATE,
            backdrop_class,
            w!("My Own Backup"),
            WS_POPUP,
            0,
            0,
            w,
            h,
            None,
            None,
            instance,
            None,
        )?
    };
    // No `WS_EX_NOACTIVATE` here, unlike the backdrop, and the reason is the
    // Start menu: a popup menu that does not own the foreground will not close
    // when the person clicks past it. The strip is kept from stealing the focus
    // by answering WM_MOUSEACTIVATE instead, which leaves the door open for the
    // one moment the menu needs it.
    let bar = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            bar_class,
            w!("My Own Backup"),
            WS_POPUP,
            0,
            h - layout::bar_height(h),
            w,
            layout::bar_height(h),
            None,
            None,
            instance,
            None,
        )?
    };
    with_bar(|b| {
        b.back = back;
        b.bar = bar;
    });
    log(&format!("shell up on a {w}x{h} screen"));
    log(if own_backup_common::media_mode::is_windows_pe() {
        "WinPE: no way out of the strip, because here that would be a reboot"
    } else {
        "not WinPE: the X at the right end of the strip closes it"
    });

    reflow(bar);
    tick(bar);
    unsafe {
        let _ = ShowWindow(back, SW_SHOWNOACTIVATE);
        let _ = SetWindowPos(back, HWND_BOTTOM, 0, 0, w, h, SWP_NOACTIVATE);
        let _ = ShowWindow(bar, SW_SHOWNOACTIVATE);
        SetTimer(bar, TICK, 1000, None);
    }

    let mut msg = MSG::default();
    // `GetMessageW` answers -1 on error, and -1 is not false: the obvious loop
    // spins forever on a broken message queue.
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    let back = with_bar(|b| b.back);
    unsafe {
        let _ = DestroyWindow(back);
    }
    log("shell down");
    Ok(())
}

type WindowProc = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;

/// Is a shell of ours already on this desktop?
///
/// `startnet.cmd` can be run again by hand from the console — that is the whole
/// point of leaving the console usable — and a second strip would reserve the
/// work area twice and answer the same clicks twice.
fn already_running() -> bool {
    unsafe {
        // The handle is deliberately never closed: the mutex has to live
        // exactly as long as the process, and the process ending is what
        // releases it.
        if CreateMutexW(None, true, w!("OwnBackupShellSingleInstance")).is_err() {
            return false;
        }
        GetLastError() == ERROR_ALREADY_EXISTS
    }
}

fn screen() -> (i32, i32) {
    unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) }
}

/// Work out the strip's size and the place of everything on it, then put both
/// windows where they belong. Called at start and whenever the screen changes —
/// which it does on this medium, where the boot script picks the monitor's own
/// mode a moment before this program starts.
fn reflow(bar: HWND) {
    let (w, h) = screen();
    let bar_h = layout::bar_height(h);
    let font = make_font(layout::font_height(bar_h));
    let big = make_font(layout::font_height(bar_h) * 2);
    let (old_font, old_big) = with_bar(|b| {
        let old = (b.font, b.big);
        b.font = font;
        b.big = big;
        b.screen_w = w;
        b.screen_h = h;
        b.bar_h = bar_h;
        b.placing = true;
        old
    });
    for stale in [old_font, old_big] {
        if !stale.is_invalid() {
            unsafe {
                let _ = DeleteObject(stale);
            }
        }
    }

    dock(bar);

    // Laid out across the width the strip actually GOT, not the screen's. On
    // the medium the two are the same; on this working machine the shell
    // trimmed our rectangle to keep clear of its own vertical taskbar, and a
    // clock placed by screen width ended up past the window's right edge —
    // drawn, invisible, and looking exactly like a clock that never appeared.
    let mut client = RECT::default();
    let width = unsafe {
        match GetClientRect(bar, &mut client) {
            Ok(()) if client.right > client.left => client.right - client.left,
            _ => w,
        }
    };
    let gap = layout::gap(bar_h);
    let close_w = if own_backup_common::media_mode::is_windows_pe() {
        0
    } else {
        text_width(font, CLOSE_TEXT) + gap
    };
    let slots = layout::bar_slots(
        width,
        bar_h,
        // Roomier than the rest: it is a button, and a button the width of its
        // own word is a word.
        text_width(font, START_TEXT) + gap * 3,
        text_width(font, LAYOUT_SAMPLE) + gap,
        text_width(font, CLOCK_SAMPLE) + gap,
        close_w,
    );
    with_bar(|b| {
        b.bar_w = width;
        b.slots = slots;
        b.placing = false;
    });

    let back = with_bar(|b| b.back);
    unsafe {
        let _ = SetWindowPos(back, HWND_BOTTOM, 0, 0, w, h, SWP_NOACTIVATE);
        let _ = InvalidateRect(back, None, false);
        let _ = InvalidateRect(bar, None, false);
    }
}

/// Put the strip along the bottom edge and take its height out of the desktop's
/// work area, so a maximised window stops above it instead of under it.
///
/// The polite way is an appbar, which the shell manages and — importantly —
/// undoes by itself if we disappear. WinPE has no shell, so `ABM_NEW` there has
/// nobody to answer it; the work area is then set directly, which is only safe
/// because WinPE's registry is a RAM disk (see [`reserve_work_area`]).
fn dock(bar: HWND) {
    let (w, h, bar_h, known) = with_bar(|b| (b.screen_w, b.screen_h, b.bar_h, b.appbar));
    let want = RECT { left: 0, top: h - bar_h, right: w, bottom: h };
    let mut data = APPBARDATA {
        cbSize: size_of::<APPBARDATA>() as u32,
        hWnd: bar,
        uCallbackMessage: WM_APPBAR,
        uEdge: ABE_BOTTOM,
        rc: want,
        lParam: LPARAM(0),
    };

    let mut appbar = known;
    if !appbar {
        appbar = unsafe { SHAppBarMessage(ABM_NEW, &mut data) } != 0;
        with_bar(|b| b.appbar = appbar);
        log(if appbar {
            "work area: registered as an appbar, the shell keeps it for us"
        } else {
            "work area: no shell accepted an appbar - setting it ourselves"
        });
    }

    let place = if appbar {
        data.rc = want;
        unsafe { SHAppBarMessage(ABM_QUERYPOS, &mut data) };
        // Along a bottom edge only the height is ours to choose; the rest is
        // where the shell says there is room.
        data.rc.left = 0;
        data.rc.right = w;
        data.rc.top = data.rc.bottom - bar_h;
        unsafe { SHAppBarMessage(ABM_SETPOS, &mut data) };
        data.rc
    } else {
        reserve_work_area(want);
        want
    };

    unsafe {
        let _ = SetWindowPos(
            bar,
            HWND_TOPMOST,
            place.left,
            place.top,
            place.right - place.left,
            place.bottom - place.top,
            SWP_NOACTIVATE,
        );
    }
}

/// Take the strip's height out of the work area by hand.
///
/// **Only in WinPE.** The work area is a global setting with no owner: a shell
/// that dies while it is shrunk leaves every maximised window short for the
/// rest of the session, and this program is developed and looked at on a
/// working machine. In WinPE that setting lives in a registry on a RAM disk
/// that will not exist after the next power-off, so there is nothing to leave
/// behind. On an installed Windows only the appbar road is taken, because
/// explorer undoes an appbar for us if we vanish.
fn reserve_work_area(bar: RECT) {
    if !own_backup_common::media_mode::is_windows_pe() {
        log("work area: left alone - this is an installed Windows, not WinPE");
        return;
    }
    let mut before = RECT::default();
    unsafe {
        let _ = SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut before as *mut _ as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
    }
    with_bar(|b| {
        if b.saved_work.is_none() {
            b.saved_work = Some(before);
        }
    });
    let mut want = RECT { bottom: bar.top, ..before };
    unsafe {
        let _ = SystemParametersInfoW(
            SPI_SETWORKAREA,
            0,
            Some(&mut want as *mut _ as *mut c_void),
            SPIF_SENDCHANGE,
        );
    }
}

/// Give the desktop back everything we took from it.
fn undock(bar: HWND) {
    let (appbar, saved) = with_bar(|b| (b.appbar, b.saved_work.take()));
    if appbar {
        let mut data = APPBARDATA {
            cbSize: size_of::<APPBARDATA>() as u32,
            hWnd: bar,
            uCallbackMessage: 0,
            uEdge: ABE_BOTTOM,
            rc: RECT::default(),
            lParam: LPARAM(0),
        };
        unsafe { SHAppBarMessage(ABM_REMOVE, &mut data) };
        with_bar(|b| b.appbar = false);
    }
    if let Some(mut was) = saved {
        unsafe {
            let _ = SystemParametersInfoW(
                SPI_SETWORKAREA,
                0,
                Some(&mut was as *mut _ as *mut c_void),
                SPIF_SENDCHANGE,
            );
        }
    }
}

/// One second has passed: re-read the clock and the foreground window's layout,
/// and repaint only if either actually changed. The strip is repainted 60 times
/// a minute otherwise, and every one of them flickers.
fn tick(bar: HWND) {
    let now = unsafe { GetLocalTime() };
    let clock = layout::clock_text(now.wHour, now.wMinute);
    let indicator = layout::layout_label(foreground_langid());
    let changed = with_bar(|b| {
        let differs = b.clock != clock || b.layout != indicator;
        b.clock = clock;
        b.layout = indicator;
        differs
    });
    if changed {
        unsafe {
            let _ = InvalidateRect(bar, None, false);
        }
    }
}

/// The language of the keyboard layout of the window in front.
///
/// Of the window in FRONT, not of this one: every thread has its own layout,
/// and ours would answer a question nobody is asking. Zero when there is no
/// foreground window — at boot there is a moment where there is none.
fn foreground_langid() -> u16 {
    unsafe {
        let front = GetForegroundWindow();
        if front.is_invalid() {
            return 0;
        }
        let thread = GetWindowThreadProcessId(front, None);
        (GetKeyboardLayout(thread).0 as usize & 0xFFFF) as u16
    }
}

/// Ask the window in front to move to the next installed layout.
///
/// It is asked, not told: `WM_INPUTLANGCHANGEREQUEST` is a request, and a
/// program that handles the message itself may decline. Both halves of the
/// message are filled in — the "next one" flag and the layout itself — because
/// which of the two a window reads is up to the window.
fn switch_layout() {
    unsafe {
        let front = GetForegroundWindow();
        if front.is_invalid() {
            return;
        }
        let thread = GetWindowThreadProcessId(front, None);
        let current = GetKeyboardLayout(thread);
        let installed = installed_layouts();
        if installed.len() < 2 {
            return;
        }
        let at = installed.iter().position(|l| l.0 == current.0).unwrap_or(0);
        let next = installed[(at + 1) % installed.len()];
        let _ = PostMessageW(
            front,
            WM_INPUTLANGCHANGEREQUEST,
            WPARAM(INPUTLANGCHANGE_FORWARD),
            LPARAM(next.0 as isize),
        );
    }
}

/// Put the Start menu up and act on what was chosen.
///
/// Two pieces of etiquette are load-bearing here. The menu is given the
/// foreground first, or it stays on screen after the person clicks past it —
/// the workaround Microsoft documents for tray icons and appbars alike, along
/// with the empty message posted afterwards. And the foreground is handed
/// straight back, because the whole point of the strip is to report on the
/// window someone is typing into, and that has to go on being their window.
fn show_menu(window: HWND, at: &layout::Slot) {
    let on_medium = own_backup_common::media_mode::is_windows_pe();
    let found: Vec<bool> = menu::TOOLS
        .iter()
        .map(|(_, relative)| tool_path(relative).map(|p| p.exists()).unwrap_or(false))
        .collect();
    let items = menu::items(on_medium, &found);
    let previously_in_front = unsafe { GetForegroundWindow() };

    let chosen = unsafe {
        let popup = match CreatePopupMenu() {
            Ok(popup) => popup,
            Err(error) => {
                log(&format!("no Start menu: {error}"));
                return;
            }
        };
        for item in &items {
            if item.separator_above {
                let _ = AppendMenuW(popup, MF_SEPARATOR, 0, None);
            }
            let text: Vec<u16> = item.text.encode_utf16().chain(std::iter::once(0)).collect();
            let _ = AppendMenuW(popup, MF_STRING, item.id as usize, PCWSTR(text.as_ptr()));
        }

        let mut corner = POINT { x: at.x, y: at.y };
        let _ = ClientToScreen(window, &mut corner);
        let _ = SetForegroundWindow(window);
        let picked = TrackPopupMenuEx(
            popup,
            (TPM_RETURNCMD | TPM_LEFTALIGN | TPM_BOTTOMALIGN | TPM_LEFTBUTTON).0,
            corner.x,
            corner.y,
            window,
            None,
        )
        .0;
        let _ = PostMessageW(window, WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(popup);
        picked
    };

    if !previously_in_front.is_invalid() && previously_in_front != window {
        unsafe {
            let _ = SetForegroundWindow(previously_in_front);
        }
    }
    if chosen > 0 {
        chose(window, chosen as u32);
    }
}

/// Do what the chosen line says. Every arm here launches something and returns:
/// nothing in this program waits for a child, because a shell that waits is a
/// desktop that has stopped.
fn chose(window: HWND, id: u32) {
    match id {
        menu::COMMAND_PROMPT => launch("cmd.exe", &[], None, CREATE_NEW_CONSOLE),
        menu::INTERFACE => start_the_interface(),
        menu::REBOOT => {
            if confirm("Перезагрузить компьютер?") {
                launch("wpeutil", &["reboot"], None, CREATE_NO_WINDOW);
            }
        }
        menu::SHUTDOWN => {
            if confirm("Выключить компьютер?") {
                launch("wpeutil", &["shutdown"], None, CREATE_NO_WINDOW);
            }
        }
        menu::CLOSE => unsafe {
            log("closed from the Start menu");
            let _ = DestroyWindow(window);
        },
        other => {
            let Some(n) = other.checked_sub(menu::TOOL_BASE) else {
                log(&format!("menu line {other} means nothing here"));
                return;
            };
            let Some((_, relative)) = menu::TOOLS.get(n as usize) else {
                log(&format!("menu line {other} means nothing here"));
                return;
            };
            let Some(path) = tool_path(relative) else { return };
            // In its own folder: these programs keep their settings beside
            // themselves and look for them there.
            let home = path.parent().map(|p| p.to_path_buf());
            launch(&path.to_string_lossy(), &[], home, 0);
        }
    }
}

/// Start the product the way this machine expects it to be started.
///
/// On the medium that is `start-media.cmd` and not the exe: the script is what
/// finds the WebView2 runtime, points the profile at the RAM disk and turns the
/// firewall off. Starting the interface without it gives a process that runs
/// and never shows a window — the failure this medium already spent a week on.
/// `/k` rather than `/c` so the console it opens stays, the way every other
/// console on this medium does.
fn start_the_interface() {
    match app_dir() {
        Some(dir) if dir.join("start-media.cmd").exists() => {
            let script = dir.join("start-media.cmd");
            launch("cmd.exe", &["/k", &script.to_string_lossy()], Some(dir), CREATE_NEW_CONSOLE);
        }
        Some(dir) => {
            let exe = dir.join("own-backup-gui.exe");
            launch(&exe.to_string_lossy(), &[], Some(dir), 0);
        }
        None => log("cannot tell where I am, so cannot start the interface"),
    }
}

/// Start a program and forget it.
fn launch(program: &str, args: &[&str], from: Option<PathBuf>, flags: u32) {
    let mut command = std::process::Command::new(program);
    command.args(args).creation_flags(flags);
    if let Some(dir) = from {
        command.current_dir(dir);
    }
    match command.spawn() {
        Ok(child) => log(&format!("started {program} as {}", child.id())),
        Err(error) => log(&format!("could not start {program}: {error}")),
    }
}

/// A console of its own, because this process has none to give.
const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
/// No console at all, for the programs that would only flash one.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The folder this exe lives in — where the medium keeps everything of ours.
fn app_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(|d| d.to_path_buf())
}

/// Where an optional tool would be, if the build put one there: beside our own
/// folder rather than inside it, so a tool is never mistaken for a part of the
/// product.
fn tool_path(relative: &str) -> Option<PathBuf> {
    Some(app_dir()?.parent()?.join(relative))
}

/// Ask before doing something that ends the session.
///
/// This is the one question the shell asks, and it earns its place: the two
/// lines it guards are next to each other at the bottom of a menu, on a machine
/// where a restart in the middle of a copy costs the copy.
fn confirm(question: &str) -> bool {
    let text: Vec<u16> = question.encode_utf16().chain(std::iter::once(0)).collect();
    let title: Vec<u16> = "My Own Backup".encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_YESNO | MB_ICONQUESTION | MB_SETFOREGROUND,
        ) == IDYES
    }
}

fn installed_layouts() -> Vec<HKL> {
    unsafe {
        let count = GetKeyboardLayoutList(None);
        if count <= 0 {
            return Vec::new();
        }
        let mut list = vec![HKL::default(); count as usize];
        let got = GetKeyboardLayoutList(Some(&mut list));
        list.truncate(got.max(0) as usize);
        list
    }
}

fn make_font(height: i32) -> HFONT {
    unsafe {
        CreateFontW(
            -height, // negative: the height of the characters, not of the cell
            0,
            0,
            0,
            400, // FW_NORMAL
            0,
            0,
            0,
            1, // DEFAULT_CHARSET - the strip shows Russian as well
            0, // OUT_DEFAULT_PRECIS
            0, // CLIP_DEFAULT_PRECIS
            5, // CLEARTYPE_QUALITY
            0, // DEFAULT_PITCH | FF_DONTCARE
            w!("Segoe UI"),
        )
    }
}

fn text_width(font: HFONT, text: &str) -> i32 {
    unsafe {
        let dc = GetDC(None);
        let previous = SelectObject(dc, font);
        let wide: Vec<u16> = text.encode_utf16().collect();
        let mut size = SIZE::default();
        let _ = GetTextExtentPoint32W(dc, &wide, &mut size);
        SelectObject(dc, previous);
        ReleaseDC(None, dc);
        size.cx
    }
}

fn draw_text(dc: HDC, slot: &layout::Slot, text: &str, align: u32) {
    if slot.is_empty() {
        return;
    }
    let mut area = RECT {
        left: slot.x,
        top: slot.y,
        right: slot.x + slot.w,
        bottom: slot.y + slot.h,
    };
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    unsafe {
        DrawTextW(
            dc,
            &mut wide,
            &mut area,
            windows::Win32::Graphics::Gdi::DRAW_TEXT_FORMAT(align)
                | DT_SINGLELINE
                | DT_VCENTER
                | DT_END_ELLIPSIS,
        );
    }
}

fn fill(dc: HDC, area: RECT, colour: COLORREF) {
    unsafe {
        let brush = CreateSolidBrush(colour);
        FillRect(dc, &area, brush);
        let _ = DeleteObject(brush);
    }
}

unsafe extern "system" fn backdrop_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            let (w, h, font) = with_bar(|b| (b.screen_w, b.screen_h, b.big));
            let mut paint = PAINTSTRUCT::default();
            let dc = BeginPaint(window, &mut paint);
            fill(dc, RECT { left: 0, top: 0, right: w, bottom: h }, BACKDROP);
            if !font.is_invalid() {
                SelectObject(dc, font);
                SetBkMode(dc, TRANSPARENT);
                SetTextColor(dc, BACKDROP_TEXT);
                let band = layout::Slot { x: 0, y: h / 2 - h / 20, w, h: h / 10 };
                draw_text(dc, &band, "My Own Backup", DT_CENTER.0);
            }
            let _ = EndPaint(window, &paint);
            LRESULT(0)
        }
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}

unsafe extern "system" fn bar_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_ERASEBKGND => LRESULT(1),
        // Clickable without taking the focus. Without this the click meant to
        // switch the layout of the window in front would first make US the
        // window in front, and then switch our own.
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_PAINT => {
            paint_bar(window);
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TICK => {
            tick(window);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            // Low and high halves of lParam, signed: a click can land at a
            // negative coordinate on a multi-monitor desktop.
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let slots = with_bar(|b| b.slots);
            if slots.start.contains(x, y) {
                show_menu(window, &slots.start);
            } else if slots.layout.contains(x, y) {
                switch_layout();
            } else if slots.close.contains(x, y) {
                // Only ever present off the medium, and it goes through
                // WM_DESTROY so the desktop gets its work area back.
                log("closed from the strip");
                let _ = DestroyWindow(window);
            }
            LRESULT(0)
        }
        WM_DISPLAYCHANGE => {
            let (w, h) = screen();
            log(&format!("screen changed to {w}x{h}"));
            reflow(window);
            LRESULT(0)
        }
        // The shell moved something and wants us to take our place again. Our
        // own placing provokes this too, so it is ignored while that is what is
        // happening — otherwise the two feed each other.
        WM_APPBAR if wparam.0 as u32 == ABN_POSCHANGED && !with_bar(|b| b.placing) => {
            reflow(window);
            LRESULT(0)
        }
        WM_DESTROY => {
            undock(window);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}

fn paint_bar(window: HWND) {
    // Copied out before any drawing: painting calls into Windows, and Windows
    // can call back into the window procedure that would want this borrow.
    let (slots, font, bar_h, width, clock, indicator) = with_bar(|b| {
        (b.slots, b.font, b.bar_h, b.bar_w, b.clock.clone(), b.layout.clone())
    });
    unsafe {
        let mut paint = PAINTSTRUCT::default();
        let dc = BeginPaint(window, &mut paint);
        fill(dc, RECT { left: 0, top: 0, right: width, bottom: bar_h }, BAR_BG);
        // A hairline along the top edge, so the strip reads as a strip and not
        // as the bottom of whatever window happens to be above it.
        fill(dc, RECT { left: 0, top: 0, right: width, bottom: 1 }, BAR_EDGE);
        if !slots.start.is_empty() {
            let b = &slots.start;
            fill(dc, RECT { left: b.x, top: 2, right: b.right(), bottom: bar_h }, BAR_BUTTON);
        }
        if !font.is_invalid() {
            SelectObject(dc, font);
            SetBkMode(dc, TRANSPARENT);
            SetTextColor(dc, BAR_TEXT);
            draw_text(dc, &slots.start, START_TEXT, DT_CENTER.0);
            draw_text(dc, &slots.layout, &indicator, DT_CENTER.0);
            draw_text(dc, &slots.clock, &clock, DT_RIGHT.0);
            draw_text(dc, &slots.close, CLOSE_TEXT, DT_CENTER.0);
        }
        let _ = EndPaint(window, &paint);
    }
}

static LOG_FILE: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Where the shell's own few lines go.
///
/// The boot medium's launcher names a directory on the stick it booted from
/// ([`own_backup_common::log_mirror::LOG_MIRROR_ENV`]), and that is the first
/// choice for a reason: everything beside the exe is on a RAM disk, and a
/// session that ended in a dark screen took that disk with it.
fn log_target() -> Option<&'static PathBuf> {
    LOG_FILE
        .get_or_init(|| {
            let mut places: Vec<PathBuf> = Vec::new();
            if let Ok(dir) = std::env::var(own_backup_common::log_mirror::LOG_MIRROR_ENV) {
                if !dir.trim().is_empty() {
                    places.push(PathBuf::from(dir.trim()));
                }
            }
            if let Some(beside) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())) {
                places.push(beside);
            }
            places.push(std::env::temp_dir());
            places.into_iter().map(|d| d.join("own-backup-shell.log")).find(|path| {
                std::fs::OpenOptions::new().create(true).append(true).open(path).is_ok()
            })
        })
        .as_ref()
}

/// One line, with the time it happened. There are only a handful of them in a
/// whole session, so there is nothing here to rotate.
pub fn log(message: &str) {
    let Some(path) = log_target() else { return };
    let now = unsafe { GetLocalTime() };
    let line = format!(
        "{:02}:{:02}:{:02} {message}\r\n",
        now.wHour, now.wMinute, now.wSecond
    );
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(line.as_bytes());
    }
}
