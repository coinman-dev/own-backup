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

use windows::core::{s, w, PCWSTR};
use windows::Win32::Foundation::{
    GetLastError, BOOL, COLORREF, ERROR_ALREADY_EXISTS, HWND, LPARAM, LRESULT, POINT, RECT, SIZE,
    TRUE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, ClientToScreen, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW,
    EndPaint, EnumFontFamiliesExW, FillRect, GetDC, GetTextExtentPoint32W, InvalidateRect,
    ReleaseDC, SelectObject, SetBkMode, SetTextColor, FONT_CHARSET, HDC, HFONT, LOGFONTW,
    PAINTSTRUCT, TEXTMETRICW, DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_RIGHT, DT_SINGLELINE,
    DT_VCENTER, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyboardLayout, GetKeyboardLayoutList, HKL};
use windows::Win32::UI::Shell::{
    SHAppBarMessage, ABE_BOTTOM, ABM_NEW, ABM_QUERYPOS, ABM_REMOVE, ABM_SETPOS, ABN_POSCHANGED,
    APPBARDATA,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, EnumWindows, GetClientRect, GetForegroundWindow, GetMessageW,
    GetSystemMetrics, GetWindow, GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, LoadCursorW, MessageBoxW,
    PostMessageW,
    PostQuitMessage, RegisterClassW, SetForegroundWindow, SetProcessDPIAware, SetTimer,
    SetWindowPos, ShowWindow, SwitchToThisWindow, SystemParametersInfoW, TrackPopupMenuEx,
    TranslateMessage,
    GWL_EXSTYLE, GW_OWNER, HWND_BOTTOM, HWND_TOPMOST, IDC_ARROW, IDYES, MA_NOACTIVATE,
    MB_ICONQUESTION, MB_SETFOREGROUND, MB_YESNO, MF_SEPARATOR, MF_STRING, MSG, SC_MINIMIZE,
    SM_CXSCREEN, SM_CYSCREEN, SPIF_SENDCHANGE, SPI_GETWORKAREA, SPI_SETWORKAREA, SWP_NOACTIVATE,
    SW_MINIMIZE,
    SW_RESTORE, SW_SHOWNOACTIVATE, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, TPM_BOTTOMALIGN,
    TPM_LEFTALIGN, TPM_LEFTBUTTON, TPM_RETURNCMD, WM_APP, WM_DESTROY, WM_DISPLAYCHANGE,
    WM_ERASEBKGND, WM_INPUTLANGCHANGEREQUEST, WM_LBUTTONDOWN, WM_MOUSEACTIVATE, WM_NULL, WM_PAINT,
    WM_SYSCOMMAND, WM_TIMER, WNDCLASSW, WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_POPUP,
};

use own_backup_common::tr;

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
/// does not have to be clicked to be read. In the window's language, so asked
/// for each time rather than kept.
fn start_text() -> std::borrow::Cow<'static, str> {
    tr!("win.start")
}
/// The way out, shown only off the boot medium — see [`BarSlots::close`].
const CLOSE_TEXT: &str = "\u{2715}";

const BACKDROP: COLORREF = rgb(15, 48, 87);
const BACKDROP_TEXT: COLORREF = rgb(110, 143, 176);
const BAR_BG: COLORREF = rgb(242, 242, 242);
const BAR_EDGE: COLORREF = rgb(196, 196, 196);
const BAR_TEXT: COLORREF = rgb(26, 26, 26);
/// The Start button and the window buttons, a shade darker than the strip so
/// they read as something to click rather than as words printed on it.
const BAR_BUTTON: COLORREF = rgb(222, 222, 222);
/// The button of the window in front, darker again — the only thing on the
/// strip that says which window a keystroke would go to.
const BAR_BUTTON_FRONT: COLORREF = rgb(198, 208, 220);

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
    /// The open windows, as of the last tick, and where their buttons are.
    windows: Vec<OpenWindow>,
    window_slots: Vec<layout::Slot>,
    /// The window in front, so its button can be drawn as the one in front.
    front: HWND,
    /// What was in front when the mouse went down on the strip.
    ///
    /// Asked at `WM_MOUSEACTIVATE`, which arrives BEFORE the press has changed
    /// anything. By the time `WM_LBUTTONDOWN` is handled the desktop may
    /// already have no window in front at all, and "is this the window in
    /// front?" then answers no about the very window the person is looking at.
    front_at_press: HWND,
    /// Everything the buttons show, in one string: what the strip is repainted
    /// for. Comparing it beats comparing the list itself, and a repaint every
    /// second is a strip that flickers every second.
    windows_shown: String,
    /// The shell accepted us as an appbar and is keeping the work area for us.
    appbar: bool,
    /// The work area as we found it, when we had to set it ourselves.
    saved_work: Option<RECT>,
    /// The language the Start button was last measured in. The window can
    /// switch it while the strip is up, and the button's width is its word's.
    language: &'static str,
}

thread_local! {
    static BAR: RefCell<Bar> = RefCell::new(Bar::default());
}

/// A window someone could want to come back to.
#[derive(Clone)]
struct OpenWindow {
    handle: HWND,
    title: String,
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
            w!("Own Backup"),
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
            w!("Own Backup"),
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
    boot_the_medium();

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
        // The name is in `common` because the interface asks about it too: it
        // has a layout indicator of its own and hides it while this one is up.
        let name: Vec<u16> = own_backup_common::media_paths::DESKTOP_RUNNING
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        if CreateMutexW(None, true, PCWSTR(name.as_ptr())).is_err() {
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
        text_width(font, &start_text()) + gap * 3,
        text_width(font, LAYOUT_SAMPLE) + gap,
        text_width(font, CLOCK_SAMPLE) + gap,
        close_w,
    );
    with_bar(|b| {
        b.bar_w = width;
        b.slots = slots;
        b.placing = false;
        b.language = own_backup_common::lang::code();
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
    // The window switched language: the Start button has a new word, and a
    // new width to go with it.
    if with_bar(|b| b.language) != own_backup_common::lang::code() {
        reflow(bar);
    }
    let now = unsafe { GetLocalTime() };
    let clock = layout::clock_text(now.wHour, now.wMinute);
    let indicator = layout::layout_label(foreground_langid());
    // In the order they have been in, not the order the system happens to
    // enumerate them in — see `layout::stable_order`.
    let seen = open_windows();
    let previous: Vec<HWND> = with_bar(|b| b.windows.iter().map(|w| w.handle).collect());
    let order = layout::stable_order(&previous, &seen.iter().map(|w| w.handle).collect::<Vec<_>>());
    let windows: Vec<OpenWindow> = order
        .iter()
        .filter_map(|handle| seen.iter().find(|w| w.handle == *handle).cloned())
        .collect();

    // A desktop with nothing in front is a real state and a common one here:
    // clicking the strip costs the window in front its place. Forgetting who
    // it was would drop the mark off its button a second later, and leave the
    // next click with nothing to compare against.
    let front = {
        let now = unsafe { GetForegroundWindow() };
        if now.is_invalid() {
            with_bar(|b| b.front)
        } else {
            now
        }
    };
    let shown = windows
        .iter()
        .map(|w| format!("{:?}{}{}", w.handle.0, if w.handle == front { "*" } else { "" }, w.title))
        .collect::<Vec<_>>()
        .join("|");

    let changed = with_bar(|b| {
        let differs = b.clock != clock || b.layout != indicator || b.windows_shown != shown;
        // The state that used to take the whole process down with it: nothing
        // in front, so nothing to say about a layout. Logged because it is not
        // obvious that it happens at all, and it is what a crash here would
        // have to be explained by again.
        if differs && indicator.is_empty() && !b.layout.is_empty() {
            log("no window in front - the layout indicator has nothing to show");
        }
        b.clock = clock;
        b.layout = indicator;
        b.front = front;
        b.window_slots = layout::window_buttons(b.slots.buttons, windows.len(), b.bar_h);
        b.windows = windows;
        b.windows_shown = shown;
        differs
    });
    if changed {
        unsafe {
            let _ = InvalidateRect(bar, None, false);
        }
    }
}

/// The windows a person would expect to find on a taskbar.
///
/// The rule is the one Alt+Tab uses: visible, titled, not a tool window, not
/// owned by another window — and not CLOAKED.
///
/// That last one is not tidiness. Windows 11 keeps several windows alive and
/// hidden ("Настройки", "Интерфейс ввода Windows"); they pass every other test,
/// and they cannot be brought forward, so clicking their button leaves the
/// desktop with no foreground window at all. Buttons that do nothing would be
/// bad enough; these ones led straight into a state the strip used to die in.
fn open_windows() -> Vec<OpenWindow> {
    let mut found: Vec<OpenWindow> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(collect_window), LPARAM(&mut found as *mut _ as isize));
    }
    found
}

unsafe extern "system" fn collect_window(window: HWND, data: LPARAM) -> BOOL {
    let found = &mut *(data.0 as *mut Vec<OpenWindow>);
    if is_task_window(window) {
        if let Some(title) = window_title(window) {
            found.push(OpenWindow { handle: window, title });
        }
    }
    TRUE
}

unsafe fn is_task_window(window: HWND) -> bool {
    if !IsWindowVisible(window).as_bool() || is_cloaked(window) {
        return false;
    }
    let (bar, back) = with_bar(|b| (b.bar, b.back));
    if window == bar || window == back {
        return false;
    }
    let extended = GetWindowLongPtrW(window, GWL_EXSTYLE) as u32;
    if extended & WS_EX_APPWINDOW.0 != 0 {
        return true;
    }
    if extended & WS_EX_TOOLWINDOW.0 != 0 {
        return false;
    }
    GetWindow(window, GW_OWNER).map(|owner| owner.is_invalid()).unwrap_or(true)
}

/// Is the compositor holding this window alive but not showing it?
///
/// Asked of `dwmapi.dll` if it is there, and answered "no" if it is not. Loaded
/// by hand rather than linked, and that is the point: this program's whole job
/// is to come up when other things have not, so it must not gain a reason to
/// fail before its first line runs. WinPE has no compositor and no cloaked
/// windows, so the question does not arise there at all.
fn is_cloaked(window: HWND) -> bool {
    /// `DWMWA_CLOAKED` — non-zero means the window is there but not shown.
    const CLOAKED: u32 = 14;
    type Ask = unsafe extern "system" fn(HWND, u32, *mut c_void, u32) -> i32;
    static ASK: OnceLock<Option<Ask>> = OnceLock::new();

    let Some(ask) = ASK.get_or_init(|| unsafe {
        let library = LoadLibraryW(w!("dwmapi.dll")).ok()?;
        let found = GetProcAddress(library, s!("DwmGetWindowAttribute"))?;
        Some(std::mem::transmute::<unsafe extern "system" fn() -> isize, Ask>(found))
    }) else {
        return false;
    };

    let mut cloaked: u32 = 0;
    let asked = unsafe {
        ask(window, CLOAKED, &mut cloaked as *mut u32 as *mut c_void, size_of::<u32>() as u32)
    };
    asked == 0 && cloaked != 0
}

unsafe fn window_title(window: HWND) -> Option<String> {
    let length = GetWindowTextLengthW(window);
    if length <= 0 {
        return None;
    }
    let mut text = vec![0u16; length as usize + 1];
    let got = GetWindowTextW(window, &mut text);
    (got > 0).then(|| String::from_utf16_lossy(&text[..got as usize]))
}

/// A click on a window's button: bring it forward, or put it away if it is
/// already the window in front — what every taskbar does, and the only thing
/// anybody tries.
///
/// Unfolding a minimised window takes two goes. `ShowWindow` is the documented
/// way and it is REFUSED for a window belonging to a program running with more
/// rights than ours — Windows keeps those out of reach of anything the user
/// runs, and the Services console started from an elevated prompt is exactly
/// that. Refused silently, too: it returns and the window stays folded, which
/// is how a button that did nothing at all came about.
///
/// The second go is the route the Alt+Tab switcher itself uses, and it is
/// allowed where the first is not (verified on this machine against a minimised
/// `mmc.exe`, 2026-09-19).
///
/// **And nothing is brought to the front while it is still folded away.**
/// `SetForegroundWindow` succeeds on a minimised window — leaving the strip
/// marking it as the window in front while the screen shows no sign of it,
/// which is worse than the click having done nothing.
///
/// None of this arises on the medium, where everything runs as SYSTEM.
fn raise_or_hide(window: HWND) {
    unsafe {
        let front = window_in_front();
        log(&format!(
            "click {:?}: at press {:?}, live {:?}, last {:?}, folded {} -> {}",
            window.0,
            with_bar(|b| b.front_at_press).0,
            GetForegroundWindow().0,
            with_bar(|b| b.front).0,
            IsIconic(window).as_bool(),
            if front == window { "fold away" } else { "bring forward" }
        ));
        if front == window {
            let _ = ShowWindow(window, SW_MINIMIZE);
            if !IsIconic(window).as_bool() {
                // `ShowWindow` is us DOING something to someone else's window,
                // and it is refused for a window this process may not command.
                // A system command is us ASKING the window to fold itself
                // away — the same thing its own «Свернуть» does — and that is
                // allowed. Measured against the Services console, which the
                // first road could not touch (2026-09-19).
                let _ = PostMessageW(
                    window,
                    WM_SYSCOMMAND,
                    WPARAM(SC_MINIMIZE as usize),
                    LPARAM(0),
                );
            }
            // Remembered straight away rather than waited for: the next click
            // may come before the next tick, and it must not find this window
            // still marked as the one in front.
            with_bar(|b| b.front = HWND::default());
            return;
        }

        if IsIconic(window).as_bool() {
            let _ = ShowWindow(window, SW_RESTORE);
            if IsIconic(window).as_bool() {
                // Refused. The switcher's own route unfolds AND activates, and
                // it does so in the window's own time — so there is nothing to
                // check afterwards. Checking anyway is what made the log claim
                // three times that a window had not come back while it was in
                // the middle of coming back.
                SwitchToThisWindow(window, TRUE);
                with_bar(|b| b.front = window);
                return;
            }
        }
        // Refused means nobody is in front at all — not the window we were
        // asked for, and not the one that had the place before the press. The
        // switcher's own route is allowed where this one is not.
        if !SetForegroundWindow(window).as_bool() {
            SwitchToThisWindow(window, TRUE);
        }
        with_bar(|b| b.front = window);
    }
}

/// The window the person means when they say "the one in front".
///
/// Three answers, in the order they can be trusted. What was in front when the
/// mouse went down, because pressing on the strip costs that window its place
/// and by the time the click is handled the desktop can have nothing in front
/// at all. Then what is in front now, for anything that did not start with a
/// press. And last, the one the strip has been marking — a second old at
/// worst, and the only answer left when the desktop has none.
fn window_in_front() -> HWND {
    let at_press = with_bar(|b| b.front_at_press);
    if !at_press.is_invalid() {
        return at_press;
    }
    let now = unsafe { GetForegroundWindow() };
    if !now.is_invalid() {
        return now;
    }
    with_bar(|b| b.front)
}

/// The language of the keyboard layout of the window in front.
///
/// Of the window in FRONT, not of this one: every thread has its own layout,
/// and ours would answer a question nobody is asking.
///
/// [`window_in_front`] rather than `GetForegroundWindow`, and that is the
/// difference between an indicator and a blank space. Clicking the strip costs
/// the window in front its place, so the click meant to READ the layout — or to
/// switch it — was followed by the indicator going empty: the desktop had
/// nobody in front to ask. The last window known to have been there is a far
/// better answer than none.
fn foreground_langid() -> u16 {
    unsafe {
        let front = window_in_front();
        if front.is_invalid() || !IsWindow(front).as_bool() {
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
        let front = window_in_front();
        if front.is_invalid() {
            log("layout: nothing in front to ask");
            return;
        }
        let thread = GetWindowThreadProcessId(front, None);
        let current = GetKeyboardLayout(thread);
        let installed = installed_layouts();
        if installed.len() < 2 {
            log(&format!("layout: only {} installed, nothing to switch to", installed.len()));
            return;
        }
        let at = installed.iter().position(|l| l.0 == current.0).unwrap_or(0);
        let next = installed[(at + 1) % installed.len()];
        log(&format!(
            "layout: asking {:?} to go from {:04X} to {:04X} (installed: {})",
            front.0,
            current.0 as usize & 0xFFFF,
            next.0 as usize & 0xFFFF,
            installed
                .iter()
                .map(|l| format!("{:04X}", l.0 as usize & 0xFFFF))
                .collect::<Vec<_>>()
                .join(" ")
        ));
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
        .map(|tool| tool_path(&tool.path_on_medium()).map(|p| p.exists()).unwrap_or(false))
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
            if confirm(&tr!("win.confirm_reboot")) {
                launch("wpeutil", &["reboot"], None, CREATE_NO_WINDOW);
            }
        }
        menu::SHUTDOWN => {
            if confirm(&tr!("win.confirm_shutdown")) {
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
            let Some(tool) = menu::TOOLS.get(n as usize) else {
                log(&format!("menu line {other} means nothing here"));
                return;
            };
            let Some(path) = tool_path(&tool.path_on_medium()) else { return };
            // In its own folder: these programs keep their settings beside
            // themselves and look for them there. And with no console: they
            // have windows of their own, and a black box flashing up first is
            // not part of what was asked for. The arguments are the medium's
            // own — see `MediaTool::args`.
            let home = path.parent().map(|p| p.to_path_buf());
            launch(&path.to_string_lossy(), tool.args, home, CREATE_NO_WINDOW);
        }
    }
}

/// Start the product the way this machine expects it to be started.
///
/// On the medium that is `start-media.cmd` and not the exe: the script is what
/// finds the WebView2 runtime, points the profile at the RAM disk and turns the
/// firewall off. Starting the interface without it gives a process that runs
/// and never shows a window — the failure this medium already spent a week on.
///
/// **With no window of its own.** The script is plumbing, and a console box
/// appearing over the desktop to announce it is noise: the only thing a person
/// asked for is the interface. Its output still reaches the log on the stick,
/// which is where anyone looking for it would look.
fn start_the_interface() {
    match app_dir() {
        Some(dir) if dir.join("start-media.cmd").exists() => {
            let script = dir.join("start-media.cmd");
            // `gui` — the interface alone. The whole script would bring up a
            // SECOND engine, and each engine mints a new token over the old
            // file: the interface already running then holds the previous one
            // and every screen it opens answers "Invalid or missing token".
            launch(
                "cmd.exe",
                &["/c", &script.to_string_lossy(), "gui"],
                Some(dir),
                CREATE_NO_WINDOW,
            );
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

/// Run the medium's boot script, with no window, now that there is a desktop
/// for its results to appear on.
///
/// This is the desktop's one piece of work that is not drawing, and it is here
/// because of the order it buys. `winpeshl` runs its entries one after another
/// and waits for each, so a boot script listed ahead of us would finish — and
/// start the interface — before the desktop existed: a console window first,
/// the interface inside it, and the desktop last, underneath everything. That
/// is what the first medium built this way did.
///
/// Started and forgotten. `wpeinit`, the drivers, the keyboard layouts and the
/// screen mode all go on behind the desktop, and the screen mode changing is
/// something the strip already handles. Nothing here waits: a desktop that
/// waits is a desktop that has stopped.
///
/// Only on the medium. On an installed Windows there is no `startnet.cmd`, and
/// running one would be nonsense.
fn boot_the_medium() {
    if !own_backup_common::media_mode::is_windows_pe() {
        return;
    }
    let script = own_backup_common::media_paths::BOOT_SCRIPT;
    log(&format!("starting the medium's boot script with no window: {script}"));
    launch("cmd.exe", &["/c", script], None, CREATE_NO_WINDOW);
}

/// The way out of the one failure that costs more than it should.
///
/// On the medium this process is the shell, and winpeshl takes its exit for the
/// end of the session: a desktop that cannot start would reboot the machine
/// before anyone could read why. So it hands the session to `cmd.exe` and waits
/// there — which is exactly what the medium was before any of this existed, and
/// closing that console still means a restart, as it always did.
///
/// Off the medium there is nothing to hold: `cmd` was never the shell there,
/// and a console appearing out of nowhere on a working machine would be the
/// strangest possible way to report a failure.
pub fn hold_a_console_instead() {
    if !own_backup_common::media_mode::is_windows_pe() {
        return;
    }
    log("falling back to a console - the medium is usable, the desktop is not");
    // With the boot script — the desktop was going to run it and did not get
    // that far, and without it there is no `wpeinit`, no drivers and no screen
    // mode. `/k` keeps the prompt afterwards, which is the medium exactly as it
    // was before any of this existed.
    let script = own_backup_common::media_paths::BOOT_SCRIPT;
    let started = std::process::Command::new("cmd.exe")
        .args(["/k", script])
        .creation_flags(CREATE_NEW_CONSOLE)
        .spawn();
    match started {
        Ok(mut console) => {
            let _ = console.wait();
            log("the console was closed");
        }
        Err(error) => log(&format!("even cmd.exe would not start: {error}")),
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
    let title: Vec<u16> = "Own Backup".encode_utf16().chain(std::iter::once(0)).collect();
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

/// Typefaces to draw the strip with, best first.
///
/// A name GDI cannot find is not an error — the font mapper quietly substitutes
/// whatever it thinks is closest, and on the medium it thought an ITALIC face
/// was closest to a missing "Segoe UI". The whole strip came up slanted. So the
/// name is checked against what is really installed before it is asked for, and
/// the list ends with faces every Windows has had for thirty years.
const FACES: [&str; 4] = ["Segoe UI", "Tahoma", "Microsoft Sans Serif", "Arial"];

/// The first of [`FACES`] this machine actually has.
fn face() -> &'static str {
    static CHOSEN: OnceLock<&'static str> = OnceLock::new();
    CHOSEN.get_or_init(|| {
        let found = FACES.iter().copied().find(|name| installed(name)).unwrap_or(FACES[1]);
        log(&format!("drawing the strip in {found}"));
        found
    })
}

/// Does a typeface of this name exist?
fn installed(name: &str) -> bool {
    unsafe extern "system" fn seen(
        _font: *const LOGFONTW,
        _metrics: *const TEXTMETRICW,
        _kind: u32,
        found: LPARAM,
    ) -> i32 {
        *(found.0 as *mut bool) = true;
        0 // stop: one is all the answer needed
    }

    let mut wanted = LOGFONTW { lfCharSet: FONT_CHARSET(1), ..Default::default() };
    for (slot, unit) in wanted.lfFaceName.iter_mut().zip(name.encode_utf16()) {
        *slot = unit;
    }
    let mut found = false;
    unsafe {
        let dc = GetDC(None);
        EnumFontFamiliesExW(dc, &wanted, Some(seen), LPARAM(&mut found as *mut bool as isize), 0);
        ReleaseDC(None, dc);
    }
    found
}

fn make_font(height: i32) -> HFONT {
    let name: Vec<u16> = face().encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        CreateFontW(
            -height, // negative: the height of the characters, not of the cell
            0,
            0,
            0,
            400, // FW_NORMAL
            0,   // not italic, and the strip came up italic anyway - see FACES
            0,
            0,
            1,    // DEFAULT_CHARSET - the strip shows Russian as well
            0,    // OUT_DEFAULT_PRECIS
            0,    // CLIP_DEFAULT_PRECIS
            5,    // CLEARTYPE_QUALITY
            0x22, // VARIABLE_PITCH | FF_SWISS - what to substitute if it must
            PCWSTR(name.as_ptr()),
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
    // Nothing to draw must never reach Windows. `DrawTextW` takes a slice, and
    // an empty `Vec<u16>` has no allocation behind it: what goes across is a
    // dangling pointer with a length of zero, and user32 reads it anyway —
    // 0xC0000005 inside user32.dll, the whole process gone.
    //
    // There is exactly one empty string on the strip and it is not a freak
    // case: the layout indicator shows nothing when no window has the focus,
    // which happens whenever the last window is closed or minimised. The strip
    // crashed three times in six minutes on an ordinary desktop before this
    // line existed (2026-09-19).
    if slot.is_empty() || text.is_empty() {
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

/// Both window procedures go through this.
///
/// A panic unwinding out of an `extern "system"` function aborts the process,
/// and on the medium this process is the shell: aborting is a reboot, from a
/// bug in drawing a clock. Nothing here is worth that, so a panicking handler
/// is logged and answered as if it had not been handled at all.
unsafe fn guarded(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    handler: fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
) -> LRESULT {
    let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        handler(window, message, wparam, lparam)
    }));
    match attempt {
        Ok(answer) => answer,
        Err(_) => {
            log(&format!("a handler for message {message:#06x} panicked - carrying on"));
            DefWindowProcW(window, message, wparam, lparam)
        }
    }
}

unsafe extern "system" fn backdrop_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    guarded(window, message, wparam, lparam, |w, m, wp, lp| unsafe {
        backdrop_message(w, m, wp, lp)
    })
}

unsafe fn backdrop_message(
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
            if dc.is_invalid() {
                let _ = EndPaint(window, &paint);
                return LRESULT(0);
            }
            fill(dc, RECT { left: 0, top: 0, right: w, bottom: h }, BACKDROP);
            if !font.is_invalid() {
                SelectObject(dc, font);
                SetBkMode(dc, TRANSPARENT);
                SetTextColor(dc, BACKDROP_TEXT);
                let band = layout::Slot { x: 0, y: h / 2 - h / 20, w, h: h / 10 };
                draw_text(dc, &band, "Own Backup", DT_CENTER.0);
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
    guarded(window, message, wparam, lparam, |w, m, wp, lp| unsafe {
        bar_message(w, m, wp, lp)
    })
}

unsafe fn bar_message(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_ERASEBKGND => LRESULT(1),
        // Clickable without taking the focus. Without this the click meant to
        // switch the layout of the window in front would first make US the
        // window in front, and then switch our own.
        //
        // And this is the last moment the desktop is still as the person sees
        // it: pressing here costs the window in front its place, so whatever
        // the click turns out to be about has to be remembered now.
        WM_MOUSEACTIVATE => {
            let front = GetForegroundWindow();
            with_bar(|b| b.front_at_press = front);
            LRESULT(MA_NOACTIVATE as isize)
        }
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
            log(&format!(
                "strip click at {x},{y}: {}",
                if slots.start.contains(x, y) {
                    "start"
                } else if slots.layout.contains(x, y) {
                    "layout"
                } else if slots.close.contains(x, y) {
                    "close"
                } else if with_bar(|b| b.window_slots.iter().any(|s| s.contains(x, y))) {
                    "a window button"
                } else {
                    "nothing"
                }
            ));
            if slots.start.contains(x, y) {
                show_menu(window, &slots.start);
            } else if slots.layout.contains(x, y) {
                switch_layout();
            } else if slots.close.contains(x, y) {
                // Only ever present off the medium, and it goes through
                // WM_DESTROY so the desktop gets its work area back.
                log("closed from the strip");
                let _ = DestroyWindow(window);
            } else if let Some(wanted) = with_bar(|b| {
                b.window_slots
                    .iter()
                    .position(|slot| slot.contains(x, y))
                    .and_then(|n| b.windows.get(n).map(|w| w.handle))
            }) {
                raise_or_hide(wanted);
            }
            // Only this click was about that window. Left standing, it would
            // go on answering "what is in front" for the rest of the session,
            // and the layout indicator would report a window nobody is typing
            // into any more.
            with_bar(|b| b.front_at_press = HWND::default());
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
    let (buttons, windows, front) =
        with_bar(|b| (b.window_slots.clone(), b.windows.clone(), b.front));
    unsafe {
        let mut paint = PAINTSTRUCT::default();
        let dc = BeginPaint(window, &mut paint);
        // A window being torn down answers with nothing, and every call below
        // would then be drawing into a handle that is not one.
        if dc.is_invalid() {
            let _ = EndPaint(window, &paint);
            return;
        }
        fill(dc, RECT { left: 0, top: 0, right: width, bottom: bar_h }, BAR_BG);
        // A hairline along the top edge, so the strip reads as a strip and not
        // as the bottom of whatever window happens to be above it.
        fill(dc, RECT { left: 0, top: 0, right: width, bottom: 1 }, BAR_EDGE);
        if !slots.start.is_empty() {
            let b = &slots.start;
            fill(dc, RECT { left: b.x, top: 2, right: b.right(), bottom: bar_h }, BAR_BUTTON);
        }
        for (slot, window) in buttons.iter().zip(&windows) {
            let colour = if window.handle == front { BAR_BUTTON_FRONT } else { BAR_BUTTON };
            fill(dc, RECT { left: slot.x, top: 2, right: slot.right(), bottom: bar_h }, colour);
        }
        if !font.is_invalid() {
            SelectObject(dc, font);
            SetBkMode(dc, TRANSPARENT);
            SetTextColor(dc, BAR_TEXT);
            draw_text(dc, &slots.start, &start_text(), DT_CENTER.0);
            for (slot, window) in buttons.iter().zip(&windows) {
                // Inset, so a long title runs out of room before it reaches the
                // next button rather than after.
                let inside = layout::Slot {
                    x: slot.x + 6,
                    w: (slot.w - 12).max(0),
                    ..*slot
                };
                draw_text(dc, &inside, &window.title, DT_LEFT.0);
            }
            draw_text(dc, &slots.layout, &indicator, DT_CENTER.0);
            draw_text(dc, &slots.clock, &clock, DT_RIGHT.0);
            draw_text(dc, &slots.close, CLOSE_TEXT, DT_CENTER.0);
        }
        let _ = EndPaint(window, &paint);
    }
}

static LOG_FILES: OnceLock<Vec<PathBuf>> = OnceLock::new();

/// Every place a line is written, worked out once — the environment does not
/// change under a running process. See [`crate::log`] for why there are two.
fn log_targets() -> &'static [PathBuf] {
    LOG_FILES.get_or_init(|| {
        // The launcher script names the stick in the environment — but it
        // names it for ITS OWN children, and this process is not one of them:
        // `winpeshl` starts the desktop, and the script runs underneath it.
        // The first medium built this way left no shell log at all, because
        // everything beside the exe is on a RAM disk. So the stick is looked
        // for the same way the script looks for it, by its marker.
        let mirror = std::env::var(own_backup_common::log_mirror::LOG_MIRROR_ENV)
            .ok()
            .filter(|dir| !dir.trim().is_empty())
            .or_else(|| {
                // Only when this IS the medium. A boot stick plugged into a
                // working machine is someone's drive, not a place to start
                // writing to unasked.
                if !own_backup_common::media_mode::is_windows_pe() {
                    return None;
                }
                own_backup_common::media_paths::find_log_dir()
                    .map(|dir| dir.display().to_string())
            });
        let beside = app_dir();
        crate::log::log_paths(mirror.as_deref(), beside.as_deref(), &std::env::temp_dir())
    })
}

/// One line, with the time it happened, to each place at once.
///
/// There are only a handful of lines in a whole session, so there is nothing
/// here to rotate — and each one is pushed out of the cache before the call
/// returns. A line still sitting in a cache when the machine is switched off at
/// the wall is a line that was never written, and switching a rescue machine
/// off at the wall is an ordinary end to a session.
pub fn log(message: &str) {
    let now = unsafe { GetLocalTime() };
    let line = format!("{:02}:{:02}:{:02} {message}\r\n", now.wHour, now.wMinute, now.wSecond);
    for path in log_targets() {
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            if file.write_all(line.as_bytes()).is_ok() {
                let _ = file.sync_data();
            }
        }
    }
}
