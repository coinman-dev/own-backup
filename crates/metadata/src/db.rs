use std::path::Path;

use rusqlite::Connection;

use own_backup_common::error::{OwnBackupError, Result};

pub const SCHEMA_VERSION: u32 = 1;

/// Open (or create) the SQLite database at `path`, enable WAL mode, and run migrations.
pub fn open_database(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)
        .map_err(|e| OwnBackupError::Database(e.to_string()))?;

    // Restrict DB file to owner (root) and group (own-backup) only (0o640).
    // The database contains encryption_salt in manifest_json, job configs, and file paths.
    // If world-readable (0o644), any local user could extract sensitive information.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o640))?;
    }

    // Enable WAL for better concurrent read performance.
    conn.execute_batch("PRAGMA journal_mode=WAL;")
        .map_err(|e| OwnBackupError::Database(e.to_string()))?;

    // Enable foreign keys.
    conn.execute_batch("PRAGMA foreign_keys=ON;")
        .map_err(|e| OwnBackupError::Database(e.to_string()))?;

    run_migrations(&conn)?;
    Ok(conn)
}

fn run_migrations(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (
            version INTEGER PRIMARY KEY,
            applied_at TEXT NOT NULL
        );",
    )
    .map_err(|e| OwnBackupError::Database(e.to_string()))?;

    let current_version: u32 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |row| row.get(0),
        )
        .map_err(|e| OwnBackupError::Database(e.to_string()))?;

    if current_version < 1 {
        apply_v1(conn)?;
    }

    Ok(())
}

fn apply_v1(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS backup_jobs (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            source_type TEXT NOT NULL DEFAULT 'partition',
            source_device TEXT NOT NULL,
            target_path TEXT NOT NULL,
            schedule TEXT,
            compression_level INTEGER DEFAULT 3,
            encryption_enabled INTEGER DEFAULT 0,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS backup_sets (
            id TEXT PRIMARY KEY,
            job_id TEXT NOT NULL REFERENCES backup_jobs(id),
            backup_type TEXT NOT NULL,
            parent_set_id TEXT,
            file_path TEXT NOT NULL,
            file_size INTEGER,
            source_size INTEGER NOT NULL,
            block_count INTEGER NOT NULL,
            changed_block_count INTEGER,
            started_at TEXT,
            completed_at TEXT,
            status TEXT NOT NULL DEFAULT 'running',
            checksum TEXT,
            manifest_json TEXT
        );

        CREATE TABLE IF NOT EXISTS retention_policies (
            job_id TEXT PRIMARY KEY REFERENCES backup_jobs(id),
            max_full_backups INTEGER,
            max_incremental_per_full INTEGER,
            max_age_days INTEGER,
            min_keep_count INTEGER NOT NULL DEFAULT 1
        );

        INSERT INTO schema_version (version, applied_at)
        VALUES (1, datetime('now'));",
    )
    .map_err(|e| OwnBackupError::Database(e.to_string()))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn open_creates_tables() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let conn = open_database(&db_path).unwrap();

        // Verify tables exist by querying them
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM backup_jobs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM backup_sets", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);

        let version: u32 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn idempotent_open() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");

        // Open twice — migrations should be idempotent.
        let _conn1 = open_database(&db_path).unwrap();
        drop(_conn1);
        let conn2 = open_database(&db_path).unwrap();

        let version: u32 = conn2
            .query_row("SELECT MAX(version) FROM schema_version", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }
}
