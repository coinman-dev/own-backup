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
//! Where it stands today: an ordinary program started from the last line of
//! `startnet.cmd`, with `cmd` still the shell. Failing here costs cosmetics.
//! The step that makes it the shell (a `winpeshl.ini` on the medium) comes last
//! and brings one obligation with it — from that point a failure must end in
//! `cmd.exe` being started, never in this process exiting, because for a shell
//! exiting means rebooting. See `.ai/winpe-shell-plan_2026-09-19.md`.

// A console window behind the strip would be noise on the medium. Debug builds
// keep theirs: that is where a panic is read by the person who caused it.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg_attr(not(windows), allow(dead_code))]
mod layout;
#[cfg_attr(not(windows), allow(dead_code))]
mod menu;
#[cfg(windows)]
mod win;

#[cfg(windows)]
fn main() {
    if let Err(error) = win::run() {
        win::log(&format!("the shell could not start: {error}"));
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("own-backup-shell is the boot medium's desktop; it only runs on Windows.");
    std::process::exit(2);
}
