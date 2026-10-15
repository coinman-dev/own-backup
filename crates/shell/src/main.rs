//! `own-backup-shell` — the desktop of the boot medium.
//!
//! WinPE ships without a shell. What it does instead is run `cmd /k
//! startnet.cmd`, and that console IS the desktop: behind it there is black,
//! there is no taskbar, nothing says which window is open or which keyboard
//! layout is active, and closing the console reboots the machine. This program
//! is the missing piece — a backdrop, a strip along the bottom, a clock and a
//! layout indicator.
//!
//! **It launches things and draws. That is all.** No state, no logic, nothing
//! held. When this process becomes the shell itself, a crash in it is a machine
//! that looks dead, so the less it does the better.
//!
//! On the medium this IS the shell: `winpeshl.ini` runs the boot script and
//! then this program, and when it exits the machine reboots. That is winpeshl's
//! rule, and it is the same rule the console always had here.
//!
//! It brings one obligation, and it is the whole of what is different about
//! being the shell: **a failure must end in a console, never in a return.**
//! Returning from here is a reboot, and a machine that reboots seconds after it
//! booted is a machine nobody can even read the error on. So when the desktop
//! cannot be drawn, `cmd.exe` is started and waited for — which leaves the
//! medium exactly as usable as it was before any of this existed.

// A console window behind the strip would be noise on the medium. Debug builds
// keep theirs: that is where a panic is read by the person who caused it.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// Messages in Russian and English (locales/*.json), through
// own_backup_common::tr! — see common/src/lang.rs.
rust_i18n::i18n!("locales", fallback = "ru");

#[cfg_attr(not(windows), allow(dead_code))]
mod layout;
#[cfg_attr(not(windows), allow(dead_code))]
mod log;
#[cfg_attr(not(windows), allow(dead_code))]
mod menu;
#[cfg(windows)]
mod win;

#[cfg(windows)]
fn main() {
    // The strip speaks the window's language: `ui_language` in config.toml,
    // followed for as long as the strip is up — and, before the window has
    // written one, the language the window will choose (see the function).
    own_backup_common::lang::follow_config_or_system(
        own_backup_common::config::AppConfig::default_path(),
    );
    if let Err(error) = win::run() {
        win::log(&format!("the desktop could not start: {error}"));
        win::hold_a_console_instead();
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("own-backup-shell is the boot medium's desktop; it only runs on Windows.");
    std::process::exit(2);
}
