use async_trait::async_trait;
use chrono::Utc;
use own_backup_common::{OwnBackupError, Result};
use std::collections::HashMap;
use uuid::Uuid;

use crate::traits::{Snapshot, SnapshotProvider};

/// Mock snapshot provider for testing the backup pipeline on Linux.
///
/// "Creates" a snapshot by generating a unique temp file path. No actual
/// copy-on-write occurs -- this is purely for exercising the pipeline logic
/// (create snapshot -> read blocks -> release snapshot) without a real OS
/// snapshot subsystem.
pub struct MockSnapshotProvider;

impl MockSnapshotProvider {
    pub fn new() -> Self {
        MockSnapshotProvider
    }
}

impl Default for MockSnapshotProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SnapshotProvider for MockSnapshotProvider {
    fn name(&self) -> &str {
        "Mock"
    }

    async fn create_snapshot(&self, volume: &str) -> Result<Snapshot> {
        let snapshot_id = Uuid::new_v4();
        let snapshot_set_id = Uuid::new_v4();

        // Build a fake device path that resembles a VSS shadow copy path
        // but lives in /tmp so it could theoretically be used as a real file
        // for integration testing.
        let snapshot_device_path = format!(
            "/tmp/own-backup-mock-snapshot-{}",
            snapshot_id.as_hyphenated()
        );

        let mut metadata = HashMap::new();
        metadata.insert("snapshot_id".into(), snapshot_id.to_string());
        metadata.insert("snapshot_set_id".into(), snapshot_set_id.to_string());
        metadata.insert("provider".into(), "mock".into());

        Ok(Snapshot {
            provider_name: "Mock".into(),
            source_device: volume.into(),
            snapshot_device_path,
            created_at: Utc::now(),
            metadata,
        })
    }

    async fn release_snapshot(&self, snapshot: &Snapshot) -> Result<()> {
        // In a real implementation we would delete the shadow copy.
        // Here we just verify the snapshot looks like one we created.
        if snapshot.provider_name != "Mock" {
            return Err(OwnBackupError::Vss(format!(
                "Cannot release snapshot from provider '{}' with MockSnapshotProvider",
                snapshot.provider_name,
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn create_and_release_snapshot() {
        let provider = MockSnapshotProvider::new();
        let snapshot = provider.create_snapshot(r"C:\").await.unwrap();

        assert_eq!(snapshot.provider_name, "Mock");
        assert_eq!(snapshot.source_device, r"C:\");
        assert!(snapshot
            .snapshot_device_path
            .starts_with("/tmp/own-backup-mock-snapshot-"));
        assert!(snapshot.metadata.contains_key("snapshot_id"));
        assert!(snapshot.metadata.contains_key("snapshot_set_id"));

        // Release should succeed.
        provider.release_snapshot(&snapshot).await.unwrap();
    }

    #[tokio::test]
    async fn release_rejects_foreign_snapshot() {
        let provider = MockSnapshotProvider::new();

        let foreign = Snapshot {
            provider_name: "SomeOtherProvider".into(),
            source_device: r"C:\".into(),
            snapshot_device_path: "/dev/null".into(),
            created_at: Utc::now(),
            metadata: HashMap::new(),
        };

        let result = provider.release_snapshot(&foreign).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn snapshots_have_unique_paths() {
        let provider = MockSnapshotProvider::new();

        let snap1 = provider.create_snapshot(r"C:\").await.unwrap();
        let snap2 = provider.create_snapshot(r"C:\").await.unwrap();

        assert_ne!(snap1.snapshot_device_path, snap2.snapshot_device_path);
        assert_ne!(
            snap1.metadata.get("snapshot_id"),
            snap2.metadata.get("snapshot_id"),
        );

        provider.release_snapshot(&snap1).await.unwrap();
        provider.release_snapshot(&snap2).await.unwrap();
    }
}
