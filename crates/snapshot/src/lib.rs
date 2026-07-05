pub mod mock;
pub mod traits;

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "windows")]
pub mod vss_raw;
#[cfg(target_os = "windows")]
pub mod vss_sync;

pub use mock::MockSnapshotProvider;
pub use traits::{Snapshot, SnapshotProvider};

#[cfg(target_os = "linux")]
pub use linux::LinuxSnapshotProvider;

#[cfg(target_os = "windows")]
pub use vss_sync::VssSnapshotSession;
