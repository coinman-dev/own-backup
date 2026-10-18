use async_trait::async_trait;
use chrono::{DateTime, Utc};
use own_backup_common::Result;
use std::collections::HashMap;

/// Represents a point-in-time snapshot of a volume.
///
/// The snapshot provides a stable, read-only view of the volume's blocks
/// at the moment of creation. The `snapshot_device_path` can be opened for
/// raw block reads while the snapshot is held.
///
/// On Windows this corresponds to a VSS shadow copy; on Linux it is an LVM
/// snapshot, or the device itself when nothing writes to it (`linux.rs`).
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// Name of the provider that created this snapshot (e.g. "VSS", "Mock").
    pub provider_name: String,

    /// The source volume that was snapshotted (e.g. `C:\` or `/dev/sda1`).
    pub source_device: String,

    /// Device path to the read-only snapshot.
    /// On Windows: `\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopyN`
    pub snapshot_device_path: String,

    /// When the snapshot was taken.
    pub created_at: DateTime<Utc>,

    /// Provider-specific metadata (e.g. VSS snapshot set ID, shadow copy ID).
    pub metadata: HashMap<String, String>,
}

/// Creates and manages snapshots for volumes.
///
/// On Windows this wraps VSS; on Linux see `linux.rs`; `mock.rs` is for tests.
#[async_trait]
pub trait SnapshotProvider: Send + Sync {
    /// Human-readable name of the provider (e.g. "VSS", "Mock").
    fn name(&self) -> &str;

    /// Create a point-in-time snapshot of the given volume.
    ///
    /// `volume` is a volume identifier such as `C:\` on Windows.
    /// Returns a [`Snapshot`] with a device path that can be opened for
    /// raw block reads.
    async fn create_snapshot(&self, volume: &str) -> Result<Snapshot>;

    /// Release a previously created snapshot, freeing OS resources.
    ///
    /// After this call the snapshot device path is no longer valid.
    async fn release_snapshot(&self, snapshot: &Snapshot) -> Result<()>;
}
