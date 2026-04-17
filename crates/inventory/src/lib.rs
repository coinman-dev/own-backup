pub mod mock;
pub mod preflight;
pub mod traits;

#[cfg(target_os = "windows")]
pub mod preflight_windows;
#[cfg(target_os = "windows")]
pub mod wmi;

pub use mock::MockInventory;
pub use preflight::PreflightAnalyzer;
pub use traits::DiskInventory;

#[cfg(target_os = "windows")]
pub use wmi::WmiInventory;
