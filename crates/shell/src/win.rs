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
use std::path::PathBuf;
use std::sync::OnceLock;

use windows::core::w;
use windows::Win32::Foundation::{
    GetLastError, COLORREF, ERROR_ALREADY_EXISTS, HWND, LPARAM, LRESULT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect, GetDC,
    GetTextExtentPoint32W, InvalidateRect, ReleaseDC, SelectObject, SetBkMode, SetTextColor, HDC,
    HFONT, PAINTSTRUCT, DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_RIGHT, DT_SINGLELINE, DT_VCENTER,
    TRANSPARENT,
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
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetForegroundWindow,
    GetMessageW, GetSystemMetrics, GetWindowThreadProcessId, LoadCursorW, PostQuitMessage,
    PostMessageW, RegisterClassW, SetProcessDPIAware, SetTimer, SetWindowPos, ShowWindow,
    SystemParametersInfoW, TranslateMessage, HWND_BOTTOM, HWND_TOPMOST, IDC_ARROW, MSG,
    SM_CXSCREEN, SM_CYSCREEN, SPIF_SENDCHANGE, SPI_GETWORKAREA, SPI_SETWORKAREA,
    SWP_NOACTIVATE, SW_SHOWNOACTIVATE, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WNDCLASSW, WM_APP,
    WM_DESTROY, WM_DISPLAYCHANGE, WM_ERASEBKGND, WM_INPUTLANGCHANGEREQUEST, WM_LBUTTONDOWN,
    WM_PAINT, WM_TIMER, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::layout::{self, BarSlots};

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
/// What sits where the Start button will go until the menu exists.
const START_TEXT: &str = "My Own Backup";

const BACKDROP: COLORREF = rgb(15, 48, 87);
const BACKDROP_TEXT: COLORREF = rgb(110, 143, 176);
const BAR_BG: COLORREF = rgb(242, 242, 242);
const BAR_EDGE: COLORREF = rgb(196, 196, 196);
const BAR_TEXT: COLORREF = rgb(26, 26, 26);

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
    bar_h: i32,
    slots: BarSlots,
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
    let bar = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
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
    // There is no way to close it from the strip, and on the medium there must
    // not be: for a shell, exiting means rebooting. On a working machine, where
    // this is only ever run to look at, the way out belongs in the log rather
    // than in a button nobody on the medium should find.
    log("to stop it from a console: taskkill /im own-backup-shell.exe");

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
    let gap = layout::gap(bar_h);
    let start_w = text_width(font, START_TEXT) + gap;
    let layout_w = text_width(font, LAYOUT_SAMPLE) + gap;
    let clock_w = text_width(font, CLOCK_SAMPLE) + gap;
    let slots = layout::bar_slots(w, bar_h, start_w, layout_w, clock_w);

    let (old_font, old_big) = with_bar(|b| {
        let old = (b.font, b.big);
        b.font = font;
        b.big = big;
        b.screen_w = w;
        b.screen_h = h;
        b.bar_h = bar_h;
        b.slots = slots;
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
            if slots.layout.contains(x, y) {
                switch_layout();
            }
            LRESULT(0)
        }
        WM_DISPLAYCHANGE => {
            let (w, h) = screen();
            log(&format!("screen changed to {w}x{h}"));
            reflow(window);
            LRESULT(0)
        }
        WM_APPBAR if wparam.0 as u32 == ABN_POSCHANGED => {
            dock(window);
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
        (b.slots, b.font, b.bar_h, b.screen_w, b.clock.clone(), b.layout.clone())
    });
    unsafe {
        let mut paint = PAINTSTRUCT::default();
        let dc = BeginPaint(window, &mut paint);
        fill(dc, RECT { left: 0, top: 0, right: width, bottom: bar_h }, BAR_BG);
        // A hairline along the top edge, so the strip reads as a strip and not
        // as the bottom of whatever window happens to be above it.
        fill(dc, RECT { left: 0, top: 0, right: width, bottom: 1 }, BAR_EDGE);
        if !font.is_invalid() {
            SelectObject(dc, font);
            SetBkMode(dc, TRANSPARENT);
            SetTextColor(dc, BAR_TEXT);
            draw_text(dc, &slots.start, START_TEXT, DT_LEFT.0);
            draw_text(dc, &slots.layout, &indicator, DT_CENTER.0);
            draw_text(dc, &slots.clock, &clock, DT_RIGHT.0);
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
