use std::path::Path;

use rusqlite::Connection;
use tracing::{info, warn};
use uuid::Uuid;

use own_backup_common::error::{OwnBackupError, Result};

use crate::queries;
use crate::retention;

/// Clean up old backups according to retention policy.
///
/// Safety properties:
/// - **Fail-closed path check**: if canonicalization fails for any reason,
///   the file is NOT deleted (protects against path traversal via symlinks etc.)
/// - **File-then-DB order**: DB record deleted only after file is confirmed deleted,
///   preventing phantom files (files exist but no DB record).
/// - **DB deletions are transactional**: all-or-nothing, no partial DB state.
///
/// Returns the count of deleted backup sets.
pub fn cleanup_old_backups(conn: &Connection, job_id: &Uuid) -> Result<u32> {
    let to_delete = retention::evaluate_retention(conn, job_id)?;

    if to_delete.is_empty() {
        return Ok(0);
    }

    let job = queries::get_job(conn, job_id)?;
    let target_dir_str = job.as_ref().map(|j| j.target_path.clone());

    // Phase 1: Delete files (fail-closed path validation, no transaction needed for FS).
    // Collect IDs of sets whose files were successfully deleted.
    let mut successfully_deleted_ids: Vec<Uuid> = Vec::new();

    for set in &to_delete {
        let path = Path::new(&set.file_path);

        // --- Fail-closed path validation ---
        if let Some(ref target_dir_str) = target_dir_str {
            let target_dir = Path::new(target_dir_str);

            match (path.canonicalize(), target_dir.canonicalize()) {
                (Ok(canonical_file), Ok(canonical_target)) => {
                    if !canonical_file.starts_with(&canonical_target) {
                        warn!(
                            set_id = %set.id,
                            path = %set.file_path,
                            "File path escapes target directory — refusing to delete"
                        );
                        continue; // skip this entry entirely
                    }
                    // Path is safe to delete.
                }
                (Err(e), _) | (_, Err(e)) => {
                    // Canonicalization failed: path may not exist, may be a broken symlink,
                    // or target_dir is invalid. Fail closed — do NOT delete.
                    warn!(
                        set_id = %set.id,
                        path = %set.file_path,
                        error = %e,
                        "Cannot canonicalize path for safety check — skipping deletion"
                    );
                    continue;
                }
            }
        }

        // --- Delete file from disk ---
        if path.exists() {
            match std::fs::remove_file(path) {
                Ok(()) => {
                    info!(set_id = %set.id, path = %set.file_path, "Deleted backup file");
                    successfully_deleted_ids.push(set.id);
                }
                Err(e) => {
                    warn!(
                        set_id = %set.id,
                        path = %set.file_path,
                        error = %e,
                        "Failed to delete backup file — skipping DB cleanup for this set"
                    );
                    // Don't add to successfully_deleted_ids — DB record stays.
                }
            }
        } else {
            // File already gone (e.g., manual deletion). Safe to clean DB record.
            info!(set_id = %set.id, "Backup file already absent, removing DB record");
            successfully_deleted_ids.push(set.id);
        }
    }

    if successfully_deleted_ids.is_empty() {
        return Ok(0);
    }

    // Phase 2: Delete DB records transactionally for sets whose files are confirmed deleted.
    // If ANY DB deletion fails, the whole batch rolls back — files are gone but records remain.
    // On next cleanup run the "already absent" path above will clean them.
    conn.execute("BEGIN", [])
        .map_err(|e| OwnBackupError::Database(e.to_string()))?;

    let mut deleted = 0u32;
    for id in &successfully_deleted_ids {
        match queries::delete_set(conn, id) {
            Ok(()) => {
                deleted += 1;
                info!(set_id = %id, "Removed DB record for deleted backup set");
            }
            Err(e) => {
                // Roll back all DB deletions in this batch.
                let _ = conn.execute("ROLLBACK", []);
                return Err(OwnBackupError::Database(format!(
                    "Cleanup DB transaction failed at set {}: {}. All DB deletions rolled back.",
                    id, e
                )));
            }
        }
    }

    conn.execute("COMMIT", [])
        .map_err(|e| OwnBackupError::Database(e.to_string()))?;

    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_database;
    use crate::models::{BackupJob, BackupSet, RetentionPolicy};
    use crate::queries::{
        get_set, insert_job, insert_set, set_retention_policy, update_set_status,
    };
    use chrono::{Duration, Utc};
    use own_backup_common::types::{BackupStatus, BackupType};
    use tempfile::TempDir;

    #[test]
    fn cleanup_deletes_old_sets_and_files() {
        let dir = TempDir::new().unwrap();
        let conn = open_database(&dir.path().join("test.db")).unwrap();

        let job = BackupJob {
            id: Uuid::new_v4(),
            name: "Cleanup Test".into(),
            source_type: "partition".into(),
            source_device: "/dev/sda1".into(),
            target_path: dir.path().to_string_lossy().into(),
            schedule: None,
            compression_level: 3,
            encryption_enabled: false,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        insert_job(&conn, &job).unwrap();

        let backup_file = dir.path().join("old.vhdx");
        std::fs::write(&backup_file, b"fake backup data").unwrap();

        let old_set = BackupSet {
            id: Uuid::new_v4(),
            job_id: job.id,
            backup_type: BackupType::Full,
            parent_set_id: None,
            file_path: backup_file.to_string_lossy().into(),
            file_size: Some(100),
            source_size: 10000,
            block_count: 10,
            changed_block_count: None,
            started_at: Some(Utc::now() - Duration::days(100)),
            completed_at: None,
            status: BackupStatus::Running,
            checksum: None,
            manifest_json: None,
        };
        insert_set(&conn, &old_set).unwrap();
        update_set_status(
            &conn,
            &old_set.id,
            &BackupStatus::Completed,
            Some(Utc::now() - Duration::days(100)),
            Some(100),
            None,
        )
        .unwrap();

        // Recent set to satisfy min_keep_count
        let new_set_path = dir.path().join("new.vhdx");
        std::fs::write(&new_set_path, b"new backup").unwrap();
        let new_set = BackupSet {
            id: Uuid::new_v4(),
            job_id: job.id,
            backup_type: BackupType::Full,
            parent_set_id: None,
            file_path: new_set_path.to_string_lossy().into(),
            file_size: Some(200),
            source_size: 10000,
            block_count: 10,
            changed_block_count: None,
            started_at: Some(Utc::now()),
            completed_at: None,
            status: BackupStatus::Running,
            checksum: None,
            manifest_json: None,
        };
        insert_set(&conn, &new_set).unwrap();
        update_set_status(
            &conn,
            &new_set.id,
            &BackupStatus::Completed,
            Some(Utc::now()),
            Some(200),
            None,
        )
        .unwrap();

        set_retention_policy(
            &conn,
            &RetentionPolicy {
                job_id: job.id,
                max_full_backups: None,
                max_incremental_per_full: None,
                max_age_days: Some(30),
                min_keep_count: 1,
            },
        )
        .unwrap();

        let count = cleanup_old_backups(&conn, &job.id).unwrap();
        assert_eq!(count, 1);

        assert!(!backup_file.exists(), "Old backup file should be deleted");
        assert!(
            get_set(&conn, &old_set.id).unwrap().is_none(),
            "Old DB record should be gone"
        );
        assert!(
            get_set(&conn, &new_set.id).unwrap().is_some(),
            "New set should remain"
        );
    }

    #[test]
    fn path_outside_target_dir_is_rejected() {
        let dir = TempDir::new().unwrap();
        let conn = open_database(&dir.path().join("test.db")).unwrap();

        let job = BackupJob {
            id: Uuid::new_v4(),
            name: "Path Traversal Test".into(),
            source_type: "partition".into(),
            source_device: "/dev/sda1".into(),
            target_path: dir.path().to_string_lossy().into(),
            schedule: None,
            compression_level: 3,
            encryption_enabled: false,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        insert_job(&conn, &job).unwrap();

        // A file path that is outside the target directory
        let outside_path = "/tmp/definitely_not_in_target.vhdx";
        // Create the file so canonicalize won't fail due to missing file
        std::fs::write(outside_path, b"important system file").unwrap();

        let bad_set = BackupSet {
            id: Uuid::new_v4(),
            job_id: job.id,
            backup_type: BackupType::Full,
            parent_set_id: None,
            file_path: outside_path.into(),
            file_size: Some(100),
            source_size: 10000,
            block_count: 10,
            changed_block_count: None,
            started_at: Some(Utc::now() - Duration::days(100)),
            completed_at: None,
            status: BackupStatus::Running,
            checksum: None,
            manifest_json: None,
        };
        insert_set(&conn, &bad_set).unwrap();
        update_set_status(
            &conn,
            &bad_set.id,
            &BackupStatus::Completed,
            Some(Utc::now() - Duration::days(100)),
            Some(100),
            None,
        )
        .unwrap();

        // Add recent set to satisfy min_keep_count
        let legit_file = dir.path().join("legit.vhdx");
        std::fs::write(&legit_file, b"legit").unwrap();
        let legit_set = BackupSet {
            id: Uuid::new_v4(),
            job_id: job.id,
            backup_type: BackupType::Full,
            parent_set_id: None,
            file_path: legit_file.to_string_lossy().into(),
            file_size: Some(50),
            source_size: 5000,
            block_count: 5,
            changed_block_count: None,
            started_at: Some(Utc::now()),
            completed_at: None,
            status: BackupStatus::Running,
            checksum: None,
            manifest_json: None,
        };
        insert_set(&conn, &legit_set).unwrap();
        update_set_status(
            &conn,
            &legit_set.id,
            &BackupStatus::Completed,
            Some(Utc::now()),
            Some(50),
            None,
        )
        .unwrap();

        set_retention_policy(
            &conn,
            &RetentionPolicy {
                job_id: job.id,
                max_full_backups: None,
                max_incremental_per_full: None,
                max_age_days: Some(30),
                min_keep_count: 1,
            },
        )
        .unwrap();

        let _ = cleanup_old_backups(&conn, &job.id);

        // The outside file must NOT be deleted
        assert!(
            std::path::Path::new(outside_path).exists(),
            "File outside target directory must not be deleted"
        );

        // Cleanup
        let _ = std::fs::remove_file(outside_path);
    }
}
