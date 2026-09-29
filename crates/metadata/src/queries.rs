use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use uuid::Uuid;

use own_backup_common::error::{OwnBackupError, Result};
use own_backup_common::types::{BackupStatus, BackupType};

use crate::models::{BackupJob, BackupSet, RetentionPolicy};

// ---------------------------------------------------------------------------
// Volume secrets (OS-protected passwords for encrypted volumes)
// ---------------------------------------------------------------------------

/// Store (or replace) the OS-protected password blob for `volume_key`.
/// The blob must already be encrypted (e.g. via DPAPI) — never a plaintext
/// password.
pub fn upsert_volume_secret(conn: &Connection, volume_key: &str, blob: &[u8]) -> Result<()> {
    conn.execute(
        "INSERT INTO volume_secrets (volume_key, protected_blob, created_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(volume_key) DO UPDATE SET
             protected_blob = excluded.protected_blob,
             created_at = excluded.created_at",
        params![volume_key, blob, Utc::now().to_rfc3339()],
    )
    .map_err(|e| OwnBackupError::Database(e.to_string()))?;
    Ok(())
}

/// Fetch the protected password blob for `volume_key`, if one is saved.
pub fn get_volume_secret(conn: &Connection, volume_key: &str) -> Result<Option<Vec<u8>>> {
    conn.query_row(
        "SELECT protected_blob FROM volume_secrets WHERE volume_key = ?1",
        params![volume_key],
        |row| row.get::<_, Vec<u8>>(0),
    )
    .map(Some)
    .or_else(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => Ok(None),
        other => Err(OwnBackupError::Database(other.to_string())),
    })
}

/// Remove any saved password for `volume_key` (idempotent).
pub fn delete_volume_secret(conn: &Connection, volume_key: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM volume_secrets WHERE volume_key = ?1",
        params![volume_key],
    )
    .map_err(|e| OwnBackupError::Database(e.to_string()))?;
    Ok(())
}

/// Read the legacy rows before moving them to the service-only database.
pub fn list_volume_secrets(conn: &Connection) -> Result<Vec<(String, Vec<u8>)>> {
    let mut stmt = conn
        .prepare("SELECT volume_key, protected_blob FROM volume_secrets ORDER BY volume_key")
        .map_err(|e| OwnBackupError::Database(e.to_string()))?;
    let rows = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(|e| OwnBackupError::Database(e.to_string()))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| OwnBackupError::Database(e.to_string()))
}

/// Keep a durable marker until the public database's WAL and freed pages have
/// been cleaned. A crash after deleting its last row then resumes the cleanup.
pub fn mark_volume_secret_migration_pending(conn: &Connection) -> Result<()> {
    conn.execute("INSERT OR IGNORE INTO volume_secret_migration_pending (id) VALUES (1)", [])
        .map_err(|e| OwnBackupError::Database(e.to_string()))?;
    Ok(())
}

pub fn volume_secret_migration_pending(conn: &Connection) -> Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM volume_secret_migration_pending WHERE id = 1)",
        [],
        |row| row.get(0),
    )
    .map_err(|e| OwnBackupError::Database(e.to_string()))
}

pub fn clear_volume_secret_migration_pending(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM volume_secret_migration_pending WHERE id = 1", [])
        .map_err(|e| OwnBackupError::Database(e.to_string()))?;
    Ok(())
}

pub fn volume_secret_migration_done(conn: &Connection) -> Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM volume_secret_migration_done WHERE id = 1)",
        [],
        |row| row.get(0),
    )
    .map_err(|e| OwnBackupError::Database(e.to_string()))
}

pub fn mark_volume_secret_migration_done(conn: &Connection) -> Result<()> {
    conn.execute("INSERT OR IGNORE INTO volume_secret_migration_done (id) VALUES (1)", [])
        .map_err(|e| OwnBackupError::Database(e.to_string()))?;
    Ok(())
}

/// Run only after every legacy secret has a verified private copy. The delete
/// pragma scrubs cells as rows are removed; VACUUM and a truncated WAL remove
/// old copies in the current database files. Older filesystem snapshots remain
/// outside SQLite's reach.
pub fn purge_legacy_volume_secret_remnants(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "PRAGMA wal_checkpoint(TRUNCATE);
         VACUUM;
         PRAGMA wal_checkpoint(TRUNCATE);",
    )
    .map_err(|e| OwnBackupError::Database(e.to_string()))
}

// ---------------------------------------------------------------------------
// BackupJob CRUD
// ---------------------------------------------------------------------------

pub fn insert_job(conn: &Connection, job: &BackupJob) -> Result<()> {
    conn.execute(
        "INSERT INTO backup_jobs
            (id, name, source_type, source_device, target_path, schedule,
             compression_level, encryption_enabled, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            job.id.to_string(),
            job.name,
            job.source_type,
            job.source_device,
            job.target_path,
            job.schedule,
            job.compression_level,
            job.encryption_enabled as i32,
            job.created_at.to_rfc3339(),
            job.updated_at.to_rfc3339(),
        ],
    )
    .map_err(|e| OwnBackupError::Database(e.to_string()))?;
    Ok(())
}

pub fn get_job(conn: &Connection, id: &Uuid) -> Result<Option<BackupJob>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, name, source_type, source_device, target_path, schedule,
                    compression_level, encryption_enabled, created_at, updated_at
             FROM backup_jobs WHERE id = ?1",
        )
        .map_err(|e| OwnBackupError::Database(e.to_string()))?;

    let result = stmt
        .query_row(params![id.to_string()], |row| {
            Ok(row_to_job(row))
        })
        .optional()
        .map_err(|e| OwnBackupError::Database(e.to_string()))?;

    match result {
        Some(job) => Ok(Some(job?)),
        None => Ok(None),
    }
}

pub fn list_jobs(conn: &Connection) -> Result<Vec<BackupJob>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, name, source_type, source_device, target_path, schedule,
                    compression_level, encryption_enabled, created_at, updated_at
             FROM backup_jobs ORDER BY created_at DESC",
        )
        .map_err(|e| OwnBackupError::Database(e.to_string()))?;

    let rows = stmt
        .query_map([], |row| Ok(row_to_job(row)))
        .map_err(|e| OwnBackupError::Database(e.to_string()))?;

    let mut jobs = Vec::new();
    for row in rows {
        let job = row.map_err(|e| OwnBackupError::Database(e.to_string()))??;
        jobs.push(job);
    }
    Ok(jobs)
}

fn row_to_job(row: &rusqlite::Row) -> Result<BackupJob> {
    let id_str: String = row
        .get(0)
        .map_err(|e| OwnBackupError::Database(e.to_string()))?;
    let encryption_int: i32 = row
        .get(7)
        .map_err(|e| OwnBackupError::Database(e.to_string()))?;
    let created_str: String = row
        .get(8)
        .map_err(|e| OwnBackupError::Database(e.to_string()))?;
    let updated_str: String = row
        .get(9)
        .map_err(|e| OwnBackupError::Database(e.to_string()))?;

    Ok(BackupJob {
        id: Uuid::parse_str(&id_str)
            .map_err(|e| OwnBackupError::Database(e.to_string()))?,
        name: row
            .get(1)
            .map_err(|e| OwnBackupError::Database(e.to_string()))?,
        source_type: row
            .get(2)
            .map_err(|e| OwnBackupError::Database(e.to_string()))?,
        source_device: row
            .get(3)
            .map_err(|e| OwnBackupError::Database(e.to_string()))?,
        target_path: row
            .get(4)
            .map_err(|e| OwnBackupError::Database(e.to_string()))?,
        schedule: row
            .get(5)
            .map_err(|e| OwnBackupError::Database(e.to_string()))?,
        compression_level: row
            .get(6)
            .map_err(|e| OwnBackupError::Database(e.to_string()))?,
        encryption_enabled: encryption_int != 0,
        created_at: DateTime::parse_from_rfc3339(&created_str)
            .map_err(|e| OwnBackupError::Database(e.to_string()))?
            .with_timezone(&Utc),
        updated_at: DateTime::parse_from_rfc3339(&updated_str)
            .map_err(|e| OwnBackupError::Database(e.to_string()))?
            .with_timezone(&Utc),
    })
}

// ---------------------------------------------------------------------------
// BackupSet CRUD
// ---------------------------------------------------------------------------

pub fn insert_set(conn: &Connection, set: &BackupSet) -> Result<()> {
    conn.execute(
        "INSERT INTO backup_sets
            (id, job_id, backup_type, parent_set_id, file_path, file_size,
             source_size, block_count, changed_block_count, started_at,
             completed_at, status, checksum, manifest_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            set.id.to_string(),
            set.job_id.to_string(),
            backup_type_to_str(&set.backup_type),
            set.parent_set_id.map(|u| u.to_string()),
            set.file_path,
            set.file_size,
            set.source_size,
            set.block_count,
            set.changed_block_count,
            set.started_at.map(|dt| dt.to_rfc3339()),
            set.completed_at.map(|dt| dt.to_rfc3339()),
            backup_status_to_str(&set.status),
            set.checksum,
            set.manifest_json,
        ],
    )
    .map_err(|e| OwnBackupError::Database(e.to_string()))?;
    Ok(())
}

pub fn update_set_status(
    conn: &Connection,
    id: &Uuid,
    status: &BackupStatus,
    completed_at: Option<DateTime<Utc>>,
    file_size: Option<i64>,
    checksum: Option<&str>,
) -> Result<()> {
    conn.execute(
        "UPDATE backup_sets
         SET status = ?1, completed_at = ?2, file_size = ?3, checksum = ?4
         WHERE id = ?5",
        params![
            backup_status_to_str(status),
            completed_at.map(|dt| dt.to_rfc3339()),
            file_size,
            checksum,
            id.to_string(),
        ],
    )
    .map_err(|e| OwnBackupError::Database(e.to_string()))?;
    Ok(())
}

pub fn get_set(conn: &Connection, id: &Uuid) -> Result<Option<BackupSet>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, job_id, backup_type, parent_set_id, file_path, file_size,
                    source_size, block_count, changed_block_count, started_at,
                    completed_at, status, checksum, manifest_json
             FROM backup_sets WHERE id = ?1",
        )
        .map_err(|e| OwnBackupError::Database(e.to_string()))?;

    let result = stmt
        .query_row(params![id.to_string()], |row| Ok(row_to_set(row)))
        .optional()
        .map_err(|e| OwnBackupError::Database(e.to_string()))?;

    match result {
        Some(set) => Ok(Some(set?)),
        None => Ok(None),
    }
}

pub fn list_sets_for_job(conn: &Connection, job_id: &Uuid) -> Result<Vec<BackupSet>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, job_id, backup_type, parent_set_id, file_path, file_size,
                    source_size, block_count, changed_block_count, started_at,
                    completed_at, status, checksum, manifest_json
             FROM backup_sets WHERE job_id = ?1 ORDER BY started_at DESC",
        )
        .map_err(|e| OwnBackupError::Database(e.to_string()))?;

    let rows = stmt
        .query_map(params![job_id.to_string()], |row| Ok(row_to_set(row)))
        .map_err(|e| OwnBackupError::Database(e.to_string()))?;

    let mut sets = Vec::new();
    for row in rows {
        let set = row.map_err(|e| OwnBackupError::Database(e.to_string()))??;
        sets.push(set);
    }
    Ok(sets)
}

/// Walk the parent_set_id chain back to the full backup, returning the entire chain
/// ordered from the full backup (first) to the given set (last).
pub fn get_backup_chain(conn: &Connection, set_id: &Uuid) -> Result<Vec<BackupSet>> {
    let mut chain = Vec::new();
    let mut current_id = Some(*set_id);

    while let Some(id) = current_id {
        let set = get_set(conn, &id)?
            .ok_or_else(|| OwnBackupError::ChainBroken(format!("Set {} not found in chain", id)))?;
        current_id = set.parent_set_id;
        chain.push(set);
    }

    chain.reverse();
    Ok(chain)
}

/// Get the latest completed full backup set for a job.
pub fn get_latest_full_set(
    conn: &Connection,
    job_id: &Uuid,
) -> Result<Option<BackupSet>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, job_id, backup_type, parent_set_id, file_path, file_size,
                    source_size, block_count, changed_block_count, started_at,
                    completed_at, status, checksum, manifest_json
             FROM backup_sets
             WHERE job_id = ?1 AND backup_type = 'full' AND status = 'completed'
             ORDER BY completed_at DESC LIMIT 1",
        )
        .map_err(|e| OwnBackupError::Database(e.to_string()))?;

    let result = stmt
        .query_row(params![job_id.to_string()], |row| Ok(row_to_set(row)))
        .optional()
        .map_err(|e| OwnBackupError::Database(e.to_string()))?;

    match result {
        Some(set) => Ok(Some(set?)),
        None => Ok(None),
    }
}

pub fn delete_set(conn: &Connection, id: &Uuid) -> Result<()> {
    conn.execute(
        "DELETE FROM backup_sets WHERE id = ?1",
        params![id.to_string()],
    )
    .map_err(|e| OwnBackupError::Database(e.to_string()))?;
    Ok(())
}

pub fn delete_job(conn: &Connection, id: &Uuid) -> Result<()> {
    // First delete all related backup_sets
    conn.execute("DELETE FROM backup_sets WHERE job_id = ?1", params![id.to_string()])
        .map_err(|e| OwnBackupError::Database(e.to_string()))?;
    // Delete retention policy
    conn.execute("DELETE FROM retention_policies WHERE job_id = ?1", params![id.to_string()])
        .map_err(|e| OwnBackupError::Database(e.to_string()))?;
    // Delete the job
    conn.execute("DELETE FROM backup_jobs WHERE id = ?1", params![id.to_string()])
        .map_err(|e| OwnBackupError::Database(e.to_string()))?;
    Ok(())
}

pub fn update_job(conn: &Connection, job: &BackupJob) -> Result<()> {
    conn.execute(
        "UPDATE backup_jobs SET name = ?2, source_device = ?3, target_path = ?4, schedule = ?5, compression_level = ?6, encryption_enabled = ?7, updated_at = ?8 WHERE id = ?1",
        params![
            job.id.to_string(),
            job.name,
            job.source_device,
            job.target_path,
            job.schedule,
            job.compression_level,
            job.encryption_enabled as i32,
            job.updated_at.to_rfc3339(),
        ],
    ).map_err(|e| OwnBackupError::Database(e.to_string()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// RetentionPolicy CRUD
// ---------------------------------------------------------------------------

pub fn set_retention_policy(conn: &Connection, policy: &RetentionPolicy) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO retention_policies
            (job_id, max_full_backups, max_incremental_per_full, max_age_days, min_keep_count)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            policy.job_id.to_string(),
            policy.max_full_backups,
            policy.max_incremental_per_full,
            policy.max_age_days,
            policy.min_keep_count,
        ],
    )
    .map_err(|e| OwnBackupError::Database(e.to_string()))?;
    Ok(())
}

pub fn get_retention_policy(
    conn: &Connection,
    job_id: &Uuid,
) -> Result<Option<RetentionPolicy>> {
    let mut stmt = conn
        .prepare(
            "SELECT job_id, max_full_backups, max_incremental_per_full,
                    max_age_days, min_keep_count
             FROM retention_policies WHERE job_id = ?1",
        )
        .map_err(|e| OwnBackupError::Database(e.to_string()))?;

    let result = stmt
        .query_row(params![job_id.to_string()], |row| {
            let job_id_str: String = row.get(0)?;
            Ok((job_id_str, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
        })
        .optional()
        .map_err(|e| OwnBackupError::Database(e.to_string()))?;

    match result {
        Some((job_id_str, max_full, max_inc, max_age, min_keep)) => Ok(Some(RetentionPolicy {
            job_id: Uuid::parse_str(&job_id_str)
                .map_err(|e| OwnBackupError::Database(e.to_string()))?,
            max_full_backups: max_full,
            max_incremental_per_full: max_inc,
            max_age_days: max_age,
            min_keep_count: min_keep,
        })),
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Use rusqlite optional extension.
use rusqlite::OptionalExtension;

fn row_to_set(row: &rusqlite::Row) -> Result<BackupSet> {
    let id_str: String = row
        .get(0)
        .map_err(|e| OwnBackupError::Database(e.to_string()))?;
    let job_id_str: String = row
        .get(1)
        .map_err(|e| OwnBackupError::Database(e.to_string()))?;
    let type_str: String = row
        .get(2)
        .map_err(|e| OwnBackupError::Database(e.to_string()))?;
    let parent_str: Option<String> = row
        .get(3)
        .map_err(|e| OwnBackupError::Database(e.to_string()))?;
    let started_str: Option<String> = row
        .get(9)
        .map_err(|e| OwnBackupError::Database(e.to_string()))?;
    let completed_str: Option<String> = row
        .get(10)
        .map_err(|e| OwnBackupError::Database(e.to_string()))?;
    let status_str: String = row
        .get(11)
        .map_err(|e| OwnBackupError::Database(e.to_string()))?;

    Ok(BackupSet {
        id: Uuid::parse_str(&id_str)
            .map_err(|e| OwnBackupError::Database(e.to_string()))?,
        job_id: Uuid::parse_str(&job_id_str)
            .map_err(|e| OwnBackupError::Database(e.to_string()))?,
        backup_type: str_to_backup_type(&type_str)?,
        parent_set_id: parent_str
            .map(|s| Uuid::parse_str(&s))
            .transpose()
            .map_err(|e| OwnBackupError::Database(e.to_string()))?,
        file_path: row
            .get(4)
            .map_err(|e| OwnBackupError::Database(e.to_string()))?,
        file_size: row
            .get(5)
            .map_err(|e| OwnBackupError::Database(e.to_string()))?,
        source_size: row
            .get(6)
            .map_err(|e| OwnBackupError::Database(e.to_string()))?,
        block_count: row
            .get(7)
            .map_err(|e| OwnBackupError::Database(e.to_string()))?,
        changed_block_count: row
            .get(8)
            .map_err(|e| OwnBackupError::Database(e.to_string()))?,
        started_at: started_str
            .map(|s| {
                DateTime::parse_from_rfc3339(&s)
                    .map(|dt| dt.with_timezone(&Utc))
                    .map_err(|e| OwnBackupError::Database(e.to_string()))
            })
            .transpose()?,
        completed_at: completed_str
            .map(|s| {
                DateTime::parse_from_rfc3339(&s)
                    .map(|dt| dt.with_timezone(&Utc))
                    .map_err(|e| OwnBackupError::Database(e.to_string()))
            })
            .transpose()?,
        status: str_to_backup_status(&status_str)?,
        checksum: row
            .get(12)
            .map_err(|e| OwnBackupError::Database(e.to_string()))?,
        manifest_json: row
            .get(13)
            .map_err(|e| OwnBackupError::Database(e.to_string()))?,
    })
}

fn backup_type_to_str(bt: &BackupType) -> &'static str {
    match bt {
        BackupType::Full => "full",
        BackupType::Incremental => "incremental",
        BackupType::Differential => "differential",
    }
}

fn str_to_backup_type(s: &str) -> Result<BackupType> {
    match s {
        "full" => Ok(BackupType::Full),
        "incremental" => Ok(BackupType::Incremental),
        "differential" => Ok(BackupType::Differential),
        other => Err(OwnBackupError::Database(format!(
            "Unknown backup type: {}",
            other
        ))),
    }
}

fn backup_status_to_str(bs: &BackupStatus) -> &'static str {
    match bs {
        BackupStatus::Running => "running",
        BackupStatus::Completed => "completed",
        BackupStatus::Failed => "failed",
        BackupStatus::Validating => "validating",
    }
}

fn str_to_backup_status(s: &str) -> Result<BackupStatus> {
    match s {
        "running" => Ok(BackupStatus::Running),
        "completed" => Ok(BackupStatus::Completed),
        "failed" => Ok(BackupStatus::Failed),
        "validating" => Ok(BackupStatus::Validating),
        other => Err(OwnBackupError::Database(format!(
            "Unknown backup status: {}",
            other
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_database;
    use tempfile::TempDir;

    fn setup_db() -> (TempDir, Connection) {
        let dir = TempDir::new().unwrap();
        let conn = open_database(&dir.path().join("test.db")).unwrap();
        (dir, conn)
    }

    #[test]
    fn volume_secret_roundtrip() {
        let (_d, conn) = setup_db();
        assert_eq!(get_volume_secret(&conn, "vol1").unwrap(), None);

        upsert_volume_secret(&conn, "vol1", b"blob-A").unwrap();
        assert_eq!(get_volume_secret(&conn, "vol1").unwrap().as_deref(), Some(&b"blob-A"[..]));

        // Upsert replaces.
        upsert_volume_secret(&conn, "vol1", b"blob-B").unwrap();
        assert_eq!(get_volume_secret(&conn, "vol1").unwrap().as_deref(), Some(&b"blob-B"[..]));

        delete_volume_secret(&conn, "vol1").unwrap();
        assert_eq!(get_volume_secret(&conn, "vol1").unwrap(), None);
        // Delete is idempotent.
        delete_volume_secret(&conn, "vol1").unwrap();
    }

    fn make_job() -> BackupJob {
        BackupJob {
            id: Uuid::new_v4(),
            name: "Test Job".into(),
            source_type: "partition".into(),
            source_device: "/dev/sda1".into(),
            target_path: "/backups".into(),
            schedule: Some("0 0 * * *".into()),
            compression_level: 3,
            encryption_enabled: false,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn make_set(job_id: Uuid, backup_type: BackupType, parent: Option<Uuid>) -> BackupSet {
        BackupSet {
            id: Uuid::new_v4(),
            job_id,
            backup_type,
            parent_set_id: parent,
            file_path: "/backups/test.vhdx".into(),
            file_size: None,
            source_size: 1_000_000,
            block_count: 100,
            changed_block_count: None,
            started_at: Some(Utc::now()),
            completed_at: None,
            status: BackupStatus::Running,
            checksum: None,
            manifest_json: None,
        }
    }

    #[test]
    fn insert_and_get_job() {
        let (_dir, conn) = setup_db();
        let job = make_job();
        insert_job(&conn, &job).unwrap();

        let loaded = get_job(&conn, &job.id).unwrap().unwrap();
        assert_eq!(loaded.id, job.id);
        assert_eq!(loaded.name, "Test Job");
        assert_eq!(loaded.source_device, "/dev/sda1");
    }

    #[test]
    fn list_jobs_returns_all() {
        let (_dir, conn) = setup_db();
        insert_job(&conn, &make_job()).unwrap();
        insert_job(&conn, &make_job()).unwrap();

        let jobs = list_jobs(&conn).unwrap();
        assert_eq!(jobs.len(), 2);
    }

    #[test]
    fn insert_and_get_set() {
        let (_dir, conn) = setup_db();
        let job = make_job();
        insert_job(&conn, &job).unwrap();

        let set = make_set(job.id, BackupType::Full, None);
        insert_set(&conn, &set).unwrap();

        let loaded = get_set(&conn, &set.id).unwrap().unwrap();
        assert_eq!(loaded.id, set.id);
        assert_eq!(loaded.backup_type, BackupType::Full);
        assert_eq!(loaded.status, BackupStatus::Running);
    }

    #[test]
    fn update_set_status_works() {
        let (_dir, conn) = setup_db();
        let job = make_job();
        insert_job(&conn, &job).unwrap();

        let set = make_set(job.id, BackupType::Full, None);
        insert_set(&conn, &set).unwrap();

        let now = Utc::now();
        update_set_status(
            &conn,
            &set.id,
            &BackupStatus::Completed,
            Some(now),
            Some(500_000),
            Some("abc123"),
        )
        .unwrap();

        let loaded = get_set(&conn, &set.id).unwrap().unwrap();
        assert_eq!(loaded.status, BackupStatus::Completed);
        assert_eq!(loaded.file_size, Some(500_000));
        assert_eq!(loaded.checksum.as_deref(), Some("abc123"));
    }

    #[test]
    fn list_sets_for_job_filters_correctly() {
        let (_dir, conn) = setup_db();
        let job1 = make_job();
        let job2 = make_job();
        insert_job(&conn, &job1).unwrap();
        insert_job(&conn, &job2).unwrap();

        insert_set(&conn, &make_set(job1.id, BackupType::Full, None)).unwrap();
        insert_set(&conn, &make_set(job1.id, BackupType::Incremental, None)).unwrap();
        insert_set(&conn, &make_set(job2.id, BackupType::Full, None)).unwrap();

        let sets = list_sets_for_job(&conn, &job1.id).unwrap();
        assert_eq!(sets.len(), 2);
    }

    #[test]
    fn get_backup_chain_walks_parents() {
        let (_dir, conn) = setup_db();
        let job = make_job();
        insert_job(&conn, &job).unwrap();

        let full = make_set(job.id, BackupType::Full, None);
        insert_set(&conn, &full).unwrap();

        let inc1 = make_set(job.id, BackupType::Incremental, Some(full.id));
        insert_set(&conn, &inc1).unwrap();

        let inc2 = make_set(job.id, BackupType::Incremental, Some(inc1.id));
        insert_set(&conn, &inc2).unwrap();

        let chain = get_backup_chain(&conn, &inc2.id).unwrap();
        assert_eq!(chain.len(), 3);
        assert_eq!(chain[0].id, full.id);
        assert_eq!(chain[1].id, inc1.id);
        assert_eq!(chain[2].id, inc2.id);
    }

    #[test]
    fn get_latest_full_set_returns_newest() {
        let (_dir, conn) = setup_db();
        let job = make_job();
        insert_job(&conn, &job).unwrap();

        let full1 = make_set(job.id, BackupType::Full, None);
        insert_set(&conn, &full1).unwrap();
        update_set_status(
            &conn,
            &full1.id,
            &BackupStatus::Completed,
            Some(Utc::now()),
            Some(100),
            None,
        )
        .unwrap();

        let full2 = make_set(job.id, BackupType::Full, None);
        insert_set(&conn, &full2).unwrap();
        update_set_status(
            &conn,
            &full2.id,
            &BackupStatus::Completed,
            Some(Utc::now()),
            Some(200),
            None,
        )
        .unwrap();

        let latest = get_latest_full_set(&conn, &job.id).unwrap().unwrap();
        assert_eq!(latest.id, full2.id);
    }

    #[test]
    fn delete_set_removes_record() {
        let (_dir, conn) = setup_db();
        let job = make_job();
        insert_job(&conn, &job).unwrap();

        let set = make_set(job.id, BackupType::Full, None);
        insert_set(&conn, &set).unwrap();
        delete_set(&conn, &set.id).unwrap();

        assert!(get_set(&conn, &set.id).unwrap().is_none());
    }

    #[test]
    fn retention_policy_crud() {
        let (_dir, conn) = setup_db();
        let job = make_job();
        insert_job(&conn, &job).unwrap();

        let policy = RetentionPolicy {
            job_id: job.id,
            max_full_backups: Some(5),
            max_incremental_per_full: Some(10),
            max_age_days: Some(30),
            min_keep_count: 1,
        };
        set_retention_policy(&conn, &policy).unwrap();

        let loaded = get_retention_policy(&conn, &job.id).unwrap().unwrap();
        assert_eq!(loaded.max_full_backups, Some(5));
        assert_eq!(loaded.max_age_days, Some(30));
        assert_eq!(loaded.min_keep_count, 1);
    }
}
