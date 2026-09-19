//! Which keyboard layout is active right now.
//!
//! Only the boot medium shows this. WinPE has no taskbar, and therefore no
//! language indicator: the user types a BitLocker or LUKS passphrase with no
//! way of knowing which layout the keystrokes are going through, and a wrong
//! layout looks exactly like a wrong password. On an installed Windows the
//! taskbar already answers this, so the interface leaves it to the taskbar.
//!
//! And the medium has a taskbar of its own now. Its strip shows the same thing
//! — for the same reason, and about the FOREGROUND window rather than about
//! ours, which is the more useful answer. Two indicators side by side are one
//! too many, and they part company the moment the focus moves: ours would go
//! on reporting this window while somebody types into another. So the
//! interface asks whether the desktop is up and, if it is, says nothing.

#[derive(serde::Serialize)]
pub struct KeyboardLayoutDto {
    /// Short tag for the indicator: "RU", "EN", …
    pub code: String,
    /// Full native name for the tooltip: "русский", "English".
    pub name: String,
}

/// The layout of the window that currently has focus, or `None` where the
/// question has no answer (non-Windows, or nothing focused).
#[tauri::command]
pub async fn keyboard_layout() -> Result<Option<KeyboardLayoutDto>, String> {
    if own_backup_common::media_paths::desktop_is_running() {
        return Ok(None);
    }
    Ok(current_layout())
}

#[cfg(target_os = "windows")]
fn current_layout() -> Option<KeyboardLayoutDto> {
    use windows::core::PCWSTR;
    use windows::Win32::Globalization::{
        GetLocaleInfoEx, LCIDToLocaleName, LOCALE_SNATIVELANGUAGENAME,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::GetKeyboardLayout;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId, GUITHREADINFO,
    };

    // LOCALE_NAME_MAX_LENGTH.
    const NAME_MAX: usize = 85;

    unsafe {
        // The layout is per input thread — and the thread that owns our
        // top-level window is not the one typing. The text goes into a WebView2
        // child window, owned by a thread in another process, and Alt+Shift
        // changes the layout of THAT thread. Asking the foreground window's
        // thread answered "EN" while Russian was being typed into the field.
        //
        // `GetGUIThreadInfo(0, …)` describes the foreground thread's input
        // state; `hwndFocus` is the window keystrokes are going to.
        let mut gui = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        let focused = if GetGUIThreadInfo(0, &mut gui).is_ok() && !gui.hwndFocus.is_invalid() {
            gui.hwndFocus
        } else {
            GetForegroundWindow()
        };
        let tid = GetWindowThreadProcessId(focused, None);
        if tid == 0 {
            return None;
        }
        let hkl = GetKeyboardLayout(tid);
        // Low word of the HKL is the LANGID of the input language.
        let langid = (hkl.0 as usize & 0xffff) as u32;

        let mut buf = [0u16; NAME_MAX];
        let n = LCIDToLocaleName(langid, Some(&mut buf), 0);
        // The count includes the terminating NUL.
        let locale = if n > 1 {
            String::from_utf16_lossy(&buf[..(n - 1) as usize])
        } else {
            String::new()
        };

        // "ru-RU" → "RU". An unknown layout still gets a stable label rather
        // than an empty box.
        let code = locale
            .split('-')
            .next()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_uppercase())
            .unwrap_or_else(|| format!("0x{langid:04X}"));

        let name = if locale.is_empty() {
            code.clone()
        } else {
            let wide: Vec<u16> = locale.encode_utf16().chain(std::iter::once(0)).collect();
            let mut nbuf = [0u16; NAME_MAX];
            let n2 = GetLocaleInfoEx(
                PCWSTR(wide.as_ptr()),
                LOCALE_SNATIVELANGUAGENAME,
                Some(&mut nbuf),
            );
            if n2 > 1 {
                String::from_utf16_lossy(&nbuf[..(n2 - 1) as usize])
            } else {
                code.clone()
            }
        };

        Some(KeyboardLayoutDto { code, name })
    }
}

#[cfg(not(target_os = "windows"))]
fn current_layout() -> Option<KeyboardLayoutDto> {
    // X11/Wayland layout reporting is a different problem per compositor, and
    // the indicator only exists for the (Windows) boot medium. Say "unknown".
    None
}
