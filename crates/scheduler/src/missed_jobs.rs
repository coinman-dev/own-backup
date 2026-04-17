use chrono::Utc;
use rusqlite::Connection;
use tracing::info;

use own_backup_common::error::Result;
use own_backup_common::types::BackupStatus;
use own_backup_metadata::models::BackupJob;

use crate::cron_parser;

/// Check for jobs that should have run but haven't.
///
/// Iterates all jobs with a schedule, checks the last completed backup time,
/// and determines whether a scheduled run was missed (i.e., there should have
/// been a run between the last completed time and now, but there wasn't).
pub fn check_missed_jobs(conn: &Connection) -> Result<Vec<BackupJob>> {
    let jobs = own_backup_metadata::queries::list_jobs(conn)?;
    let mut missed = Vec::new();

    for job in jobs {
        let schedule_expr = match &job.schedule {
            Some(s) if !s.is_empty() => s.clone(),
            _ => continue,
        };

        let schedule = match cron_parser::parse_schedule(&schedule_expr) {
            Ok(s) => s,
            Err(_) => continue, // Skip jobs with invalid schedules.
        };

        // Get the last completed backup time for this job.
        let sets = own_backup_metadata::queries::list_sets_for_job(conn, &job.id)?;
        let last_completed: Option<chrono::DateTime<Utc>> = sets
            .iter()
            .filter(|s| s.status == BackupStatus::Completed)
            .filter_map(|s| s.completed_at)
            .max();

        // Check if there should have been a run since the last completed backup.
        let should_have_run = match last_completed {
            Some(last_dt) => {
                // Find the first scheduled time after the last completed backup.
                schedule
                    .after(&last_dt)
                    .next()
                    .map(|next_scheduled| next_scheduled < Utc::now())
                    .unwrap_or(false)
            }
            None => true, // Never completed before — definitely missed.
        };

        if should_have_run {
            info!(
                job_id = %job.id,
                job_name = %job.name,
                "Missed job detected"
            );
            missed.push(job);
        }
    }

    Ok(missed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};
    use own_backup_common::types::BackupType;
    use own_backup_metadata::db::open_database;
    use own_backup_metadata::models::{BackupJob, BackupSet};
    use own_backup_metadata::queries::{insert_job, insert_set, update_set_status};
    use tempfile::TempDir;
    use uuid::Uuid;

    fn setup() -> (TempDir, Connection) {
        let dir = TempDir::new().unwrap();
        let conn = open_database(&dir.path().join("test.db")).unwrap();
        (dir, conn)
    }

    #[test]
    fn job_never_run_is_missed() {
        let (_dir, conn) = setup();

        let job = BackupJob {
            id: Uuid::new_v4(),
            name: "Never Run".into(),
            source_type: "partition".into(),
            source_device: "/dev/sda1".into(),
            target_path: "/backups".into(),
            // Every minute — should definitely have run by now.
            schedule: Some("0 * * * * * *".into()),
            compression_level: 3,
            encryption_enabled: false,
            created_at: Utc::now() - Duration::hours(1),
            updated_at: Utc::now() - Duration::hours(1),
        };
        insert_job(&conn, &job).unwrap();

        let missed = check_missed_jobs(&conn).unwrap();
        assert_eq!(missed.len(), 1);
        assert_eq!(missed[0].id, job.id);
    }

    #[test]
    fn recently_completed_job_is_not_missed() {
        let (_dir, conn) = setup();

        let job = BackupJob {
            id: Uuid::new_v4(),
            name: "Just Ran".into(),
            source_type: "partition".into(),
            source_device: "/dev/sda1".into(),
            target_path: "/backups".into(),
            // Once per day at 2am — next run is tomorrow.
            schedule: Some("0 0 2 * * * *".into()),
            compression_level: 3,
            encryption_enabled: false,
            created_at: Utc::now() - Duration::hours(1),
            updated_at: Utc::now() - Duration::hours(1),
        };
        insert_job(&conn, &job).unwrap();

        // Add a completed set from just now.
        let set = BackupSet {
            id: Uuid::new_v4(),
            job_id: job.id,
            backup_type: BackupType::Full,
            parent_set_id: None,
            file_path: "/backups/test.obk".into(),
            file_size: Some(100),
            source_size: 10000,
            block_count: 10,
            changed_block_count: None,
            started_at: Some(Utc::now()),
            completed_at: None,
            status: BackupStatus::Running,
            checksum: None,
            manifest_json: None,
        };
        insert_set(&conn, &set).unwrap();
        update_set_status(
            &conn,
            &set.id,
            &BackupStatus::Completed,
            Some(Utc::now()),
            Some(100),
            None,
        )
        .unwrap();

        let missed = check_missed_jobs(&conn).unwrap();
        assert!(missed.is_empty());
    }

    #[test]
    fn job_without_schedule_is_skipped() {
        let (_dir, conn) = setup();

        let job = BackupJob {
            id: Uuid::new_v4(),
            name: "No Schedule".into(),
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

        let missed = check_missed_jobs(&conn).unwrap();
        assert!(missed.is_empty());
    }
}
