use chrono::Utc;
use rusqlite::Connection;
use uuid::Uuid;

use own_backup_common::error::Result;
use own_backup_common::types::{BackupStatus, BackupType};

use crate::models::BackupSet;
use crate::queries::{get_retention_policy, list_sets_for_job};

/// Evaluate retention policy for a job and return the list of backup sets that
/// should be deleted.
///
/// Rules applied:
/// 1. Never delete sets that are still running/validating.
/// 2. Never delete below `min_keep_count` completed sets.
/// 3. Delete sets older than `max_age_days` (if set).
/// 4. Keep at most `max_full_backups` full backups (oldest first).
///
/// Note: incremental/differential sets whose parent full backup is deleted
/// should also be marked for deletion (chain is broken).
pub fn evaluate_retention(conn: &Connection, job_id: &Uuid) -> Result<Vec<BackupSet>> {
    let policy = match get_retention_policy(conn, job_id)? {
        Some(p) => p,
        None => return Ok(Vec::new()), // No policy configured — keep everything.
    };

    let all_sets = list_sets_for_job(conn, job_id)?;

    // Only consider completed sets for deletion.
    let completed: Vec<&BackupSet> = all_sets
        .iter()
        .filter(|s| s.status == BackupStatus::Completed)
        .collect();

    let total_completed = completed.len() as i64;
    let min_keep = policy.min_keep_count.max(1);

    // Collect IDs of sets to delete.
    let mut to_delete_ids = std::collections::HashSet::new();

    // Rule: max_age_days
    if let Some(max_age_days) = policy.max_age_days {
        let cutoff = Utc::now() - chrono::Duration::days(max_age_days);
        for set in &completed {
            if let Some(completed_at) = set.completed_at {
                if completed_at < cutoff {
                    to_delete_ids.insert(set.id);
                }
            }
        }
    }

    // Rule: max_full_backups
    if let Some(max_full) = policy.max_full_backups {
        let mut full_sets: Vec<&BackupSet> = completed
            .iter()
            .filter(|s| s.backup_type == BackupType::Full)
            .copied()
            .collect();

        // Sort by completed_at descending (newest first).
        full_sets.sort_by(|a, b| b.completed_at.cmp(&a.completed_at));

        if full_sets.len() as i64 > max_full {
            // Mark excess oldest full backups for deletion.
            for excess in &full_sets[max_full as usize..] {
                to_delete_ids.insert(excess.id);
            }
        }
    }

    // If a full backup is being deleted, also delete its dependent incrementals/differentials.
    let full_ids_to_delete: std::collections::HashSet<_> = to_delete_ids
        .iter()
        .filter(|id| {
            completed
                .iter()
                .any(|s| s.id == **id && s.backup_type == BackupType::Full)
        })
        .copied()
        .collect();

    for set in &completed {
        if let Some(parent_id) = set.parent_set_id {
            // If the parent is a full backup being deleted, delete this set too.
            if full_ids_to_delete.contains(&parent_id) {
                to_delete_ids.insert(set.id);
            }
            // Also check if the parent itself is being deleted (cascading).
            if to_delete_ids.contains(&parent_id) {
                to_delete_ids.insert(set.id);
            }
        }
    }

    // Enforce min_keep_count: if deleting would leave fewer than min_keep, remove
    // items from the delete set (keep the newest).
    let would_remain = total_completed - to_delete_ids.len() as i64;
    if would_remain < min_keep {
        let must_save = (min_keep - would_remain) as usize;
        // Sort the delete candidates by completed_at descending, save newest ones.
        let mut delete_vec: Vec<&BackupSet> = completed
            .iter()
            .filter(|s| to_delete_ids.contains(&s.id))
            .copied()
            .collect();
        delete_vec.sort_by(|a, b| b.completed_at.cmp(&a.completed_at));

        for set in delete_vec.iter().take(must_save) {
            to_delete_ids.remove(&set.id);
        }
    }

    // Collect the actual BackupSet objects to return.
    let result: Vec<BackupSet> = all_sets
        .into_iter()
        .filter(|s| to_delete_ids.contains(&s.id))
        .collect();

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_database;
    use crate::models::{BackupJob, BackupSet, RetentionPolicy};
    use crate::queries::{insert_job, insert_set, set_retention_policy, update_set_status};
    use chrono::{Duration, Utc};
    use tempfile::TempDir;

    fn setup() -> (TempDir, rusqlite::Connection, BackupJob) {
        let dir = TempDir::new().unwrap();
        let conn = open_database(&dir.path().join("test.db")).unwrap();
        let job = BackupJob {
            id: Uuid::new_v4(),
            name: "Retention Test".into(),
            source_type: "partition".into(),
            source_device: "/dev/sda1".into(),
            target_path: "/backups".into(),
            schedule: None,
            compression_level: 3,
            encryption_enabled: false,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        insert_job(&conn, &job).unwrap();
        (dir, conn, job)
    }

    fn add_full_set(
        conn: &rusqlite::Connection,
        job_id: Uuid,
        completed_at: chrono::DateTime<Utc>,
    ) -> Uuid {
        let set = BackupSet {
            id: Uuid::new_v4(),
            job_id,
            backup_type: BackupType::Full,
            parent_set_id: None,
            file_path: format!("/backups/{}.vhdx", Uuid::new_v4()),
            file_size: Some(1000),
            source_size: 10000,
            block_count: 10,
            changed_block_count: None,
            started_at: Some(completed_at - Duration::minutes(5)),
            completed_at: None,
            status: BackupStatus::Running,
            checksum: None,
            manifest_json: None,
        };
        insert_set(conn, &set).unwrap();
        update_set_status(
            conn,
            &set.id,
            &BackupStatus::Completed,
            Some(completed_at),
            Some(1000),
            None,
        )
        .unwrap();
        set.id
    }

    #[test]
    fn no_policy_means_no_deletion() {
        let (_dir, conn, job) = setup();
        add_full_set(&conn, job.id, Utc::now());

        let to_delete = evaluate_retention(&conn, &job.id).unwrap();
        assert!(to_delete.is_empty());
    }

    #[test]
    fn max_full_backups_enforced() {
        let (_dir, conn, job) = setup();

        // Create 3 full backups.
        let old = add_full_set(&conn, job.id, Utc::now() - Duration::days(3));
        let _mid = add_full_set(&conn, job.id, Utc::now() - Duration::days(2));
        let _new = add_full_set(&conn, job.id, Utc::now() - Duration::days(1));

        set_retention_policy(
            &conn,
            &RetentionPolicy {
                job_id: job.id,
                max_full_backups: Some(2),
                max_incremental_per_full: None,
                max_age_days: None,
                min_keep_count: 1,
            },
        )
        .unwrap();

        let to_delete = evaluate_retention(&conn, &job.id).unwrap();
        assert_eq!(to_delete.len(), 1);
        assert_eq!(to_delete[0].id, old);
    }

    #[test]
    fn min_keep_count_prevents_deletion() {
        let (_dir, conn, job) = setup();

        let _only = add_full_set(&conn, job.id, Utc::now() - Duration::days(100));

        set_retention_policy(
            &conn,
            &RetentionPolicy {
                job_id: job.id,
                max_full_backups: None,
                max_incremental_per_full: None,
                max_age_days: Some(7),
                min_keep_count: 1,
            },
        )
        .unwrap();

        let to_delete = evaluate_retention(&conn, &job.id).unwrap();
        // Should not delete the only backup even though it's older than 7 days.
        assert!(to_delete.is_empty());
    }

    #[test]
    fn max_age_days_enforced() {
        let (_dir, conn, job) = setup();

        let old = add_full_set(&conn, job.id, Utc::now() - Duration::days(60));
        let _new = add_full_set(&conn, job.id, Utc::now() - Duration::days(1));

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

        let to_delete = evaluate_retention(&conn, &job.id).unwrap();
        assert_eq!(to_delete.len(), 1);
        assert_eq!(to_delete[0].id, old);
    }
}
