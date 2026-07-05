pub mod mock;
pub mod preflight;
pub mod traits;

pub mod fs_probe;
#[cfg(target_os = "windows")]
pub mod preflight_windows;
#[cfg(target_os = "windows")]
pub mod wmi;

pub use mock::MockInventory;
pub use preflight::PreflightAnalyzer;
pub use traits::DiskInventory;

#[cfg(target_os = "windows")]
pub use wmi::WmiInventory;

/// A cheap signature of the currently present drive letters.
///
/// Changes when a volume is mounted/removed (USB insert/eject, VHDX attach).
/// Uses `GetLogicalDrives` (a single bitmask syscall) so it can be polled
/// frequently without the cost of full WMI/PowerShell enumeration.
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
