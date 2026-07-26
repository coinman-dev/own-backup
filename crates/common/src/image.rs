//! Image manifest — the descriptive half of a backup, stored in the sidecar
//! (`backup::sidecar`) embedded in the VHDX/qcow2 container.
//!
//! These types outlived the retired `.obk` container format: the container is
//! now VHDX or qcow2, but the manifest describing *what* was captured stayed.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::disk::DiskInfo;
use crate::preflight::PreflightReport;
use crate::types::{BackupMode, BackupType, BootMode, CompressionAlgo, PartitionTable};

/// State of the volume's own encryption (BitLocker / LUKS) — i.e. encryption
/// that belongs to the source volume, not to our container.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VolumeEncryptionKind {
    /// Windows BitLocker
    BitLocker,
    /// Linux LUKS1
    Luks1,
    /// Linux LUKS2
    Luks2,
    /// Other (name stored in `note`)
    Other,
}

/// How the encrypted volume was captured in the backup.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VolumeCaptureMode {
    /// Blocks read through the OS/filesystem — backup contains plaintext data.
    /// After restore, volume encryption must be re-enabled manually.
    Plaintext,
    /// Raw encrypted sectors copied verbatim — backup preserves encryption.
    /// Original credentials needed to access data after restore.
    SectorBySector,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeEncryptionState {
    pub kind: VolumeEncryptionKind,
    pub capture_mode: VolumeCaptureMode,
    /// LUKS header backup (base64-encoded). Critical: without this a damaged
    /// LUKS header makes the entire volume permanently inaccessible.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub luks_header_backup_b64: Option<String>,
    /// Human-readable note for the restore wizard.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageManifest {
    pub backup_id: Uuid,
    pub created_at: String,
    pub backup_type: BackupType,
    pub parent_backup_id: Option<Uuid>,
    pub source_os: String,
    pub source_disk: DiskInfo,
    pub boot_mode: BootMode,
    pub partition_table: PartitionTable,
    pub compression_algo: CompressionAlgo,
    pub compression_level: i32,
    /// Archive-level encryption, from the `.obk` era. Nothing writes these
    /// today; kept so manifests already written on disk still deserialize.
    pub encryption_algo: Option<String>,
    pub encryption_salt: Option<Vec<u8>>,
    pub block_size: u32,
    pub total_blocks: u64,
    pub used_blocks: u64,
    pub backup_mode: BackupMode,
    pub checksum_algorithm: String,
    pub preflight_report: Option<PreflightReport>,
    /// State of the source volume's own encryption (BitLocker / LUKS).
    /// `None` if the source volume was not encrypted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume_encryption: Option<VolumeEncryptionState>,
}
