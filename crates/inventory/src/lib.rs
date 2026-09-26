// Messages in Russian and English (locales/*.json), through
// own_backup_common::tr! — see common/src/lang.rs.
rust_i18n::i18n!("locales", fallback = "ru");

pub mod mock;
pub mod preflight;
pub mod traits;

pub mod fs_probe;
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "windows")]
pub mod preflight_windows;
/// Pure parsing of the structures Windows answers with — not gated, so its
/// tests run wherever this project's tests run.
pub mod win_parse;
#[cfg(target_os = "windows")]
pub mod win_disks;

pub use mock::MockInventory;
pub use preflight::PreflightAnalyzer;
pub use traits::DiskInventory;

#[cfg(target_os = "linux")]
pub use linux::LinuxInventory;
#[cfg(target_os = "windows")]
pub use win_disks::WindowsInventory;

/// A cheap signature of the currently present drive letters.
///
/// Changes when a volume is mounted/removed (USB insert/eject, VHDX attach).
/// Uses `GetLogicalDrives` (a single bitmask syscall) so it can be polled as
/// often as the interface likes.
#[cfg(target_os = "windows")]
pub fn drive_letters_signature() -> u32 {
    // SAFETY: GetLogicalDrives takes no arguments and cannot fail meaningfully.
    unsafe { windows::Win32::Storage::FileSystem::GetLogicalDrives() }
}

/// Non-Windows stub: no cheap equivalent wired up yet, so report a constant
/// (callers fall back to explicit refresh).
#[cfg(not(target_os = "windows"))]
pub fn drive_letters_signature() -> u32 {
    0
}
