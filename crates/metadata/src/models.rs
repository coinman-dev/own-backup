use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use own_backup_common::types::{BackupStatus, BackupType};

/// A configured backup job describing what to back up and where.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupJob {
    pub id: Uuid,
    pub name: String,
    pub source_type: String,
    pub source_device: String,
    pub target_path: String,
    pub schedule: Option<String>,
    pub compression_level: i32,
    pub encryption_enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A single backup set (one completed or in-progress backup run).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupSet {
    pub id: Uuid,
    pub job_id: Uuid,
    pub backup_type: BackupType,
    pub parent_set_id: Option<Uuid>,
    pub file_path: String,
    pub file_size: Option<i64>,
    pub source_size: i64,
    pub block_count: i64,
    pub changed_block_count: Option<i64>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub status: BackupStatus,
    pub checksum: Option<String>,
    pub manifest_json: Option<String>,
}

/// Retention policy for a backup job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetentionPolicy {
    pub job_id: Uuid,
    pub max_full_backups: Option<i64>,
    pub max_incremental_per_full: Option<i64>,
    pub max_age_days: Option<i64>,
    pub min_keep_count: i64,
}
