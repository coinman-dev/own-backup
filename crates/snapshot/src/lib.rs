pub mod mock;
pub mod traits;

#[cfg(target_os = "windows")]
pub mod vss;
#[cfg(target_os = "windows")]
pub mod vss_com;

pub use mock::MockSnapshotProvider;
pub use traits::{Snapshot, SnapshotProvider};
