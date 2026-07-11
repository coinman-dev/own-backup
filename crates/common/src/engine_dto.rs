//! Data-transfer types for the direct engine operations (restore / verify /
//! mount / whole-disk restore) that the GUI proxies to the service over gRPC.
//!
//! They live here so the producer (service) and consumer (GUI) share one
//! definition and the JSON contract can't drift. Field names match the frontend
//! `types.ts` shapes exactly (`EngineProgress`, `RestoreSummary`, …).

use serde::{Deserialize, Serialize};

/// Progress payload for restore / verify (Tauri events `restore-progress` /
/// `verify-progress`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineProgress {
    pub percent: u64,
    pub blocks_done: u64,
    pub blocks_total: u64,
    pub bytes_done: u64,
    pub speed_mbps: f64,
}

/// Outcome of a completed volume restore.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreSummaryDto {
    pub blocks_restored: u64,
    pub bytes_restored: u64,
    pub duration_secs: f64,
}

/// Outcome of a verify pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifySummaryDto {
    pub valid: bool,
    pub blocks_checked: u64,
    pub mismatched_blocks: Vec<u64>,
    pub duration_secs: f64,
}

/// Where a mounted image appeared.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountResultDto {
    pub mount_point: String,
}

/// Result of unlocking + probing an encrypted (LUKS) source volume. The probe
/// opens the raw disk, so it runs in the privileged service (LocalSystem/root),
/// never in the unelevated GUI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeUnlockDto {
    /// The password unlocked the container.
    pub ok: bool,
    /// Inner filesystem (`ext4`/`btrfs`/`ntfs`/…), empty if not identified.
    pub filesystem: String,
    /// Inner volume label, if any.
    pub label: String,
    /// Bytes in use inside the decrypted volume, 0 if unknown.
    pub used_bytes: u64,
}

/// An active mounted image mapping.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveMountDto {
    pub image_path: String,
    pub physical_path: String,
    pub read_write: bool,
    /// Drive letters Windows assigned to the volumes inside (e.g. `["J:"]`).
    /// Empty when none are assigned yet or on non-Windows hosts.
    #[serde(default)]
    pub volume_letters: Vec<String>,
}

/// Progress payload for a whole-disk restore (Tauri event `restore-disk-progress`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskRestoreProgressDto {
    pub phase: String,
    pub partition_index: u64,
    pub partition_count: u64,
    pub part_label: String,
    pub percent: u64,
    pub bytes_written: u64,
}

/// Outcome of a completed whole-disk restore.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskRestoreSummaryDto {
    pub partitions: u64,
    pub bytes_written: u64,
    pub duration_secs: f64,
}
