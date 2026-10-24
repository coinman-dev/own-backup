use std::path::Path;

use rusqlite::Connection;

use own_backup_common::error::{OwnBackupError, Result};

pub const SCHEMA_VERSION: u32 = 2;

/// Open (or create) the SQLite database at `path`, enable WAL mode, and run migrations.
pub fn open_database(path: &Path) -> Result<Connection> {
    #[cfg(unix)]
    restrict_to_owner(path)?;

    let conn = Connection::open(path).map_err(|e| OwnBackupError::Database(e.to_string()))?;

    // Enable WAL for better concurrent read performance.
    conn.execute_batch("PRAGMA journal_mode=WAL;")
        .map_err(|e| OwnBackupError::Database(e.to_string()))?;

    // Enable foreign keys.
    conn.execute_batch("PRAGMA foreign_keys=ON;")
        .map_err(|e| OwnBackupError::Database(e.to_string()))?;

    run_migrations(&conn)?;
    Ok(conn)
}

/// Make the database and its `-wal`/`-shm` owner-only (0600) before SQLite
/// opens them.
///
/// The database holds encryption salts in manifests, job configurations and
/// file paths. It used to be chmod-ed to 0640 only AFTER the open — which had
/// already created it with the umask — and never given a group, while the WAL
/// and the shared-memory file kept the umask's 0644. SQLite creates a new
/// `-wal`/`-shm` with the mode of the database file itself, so creating the
/// database owner-only covers them too; ones left by an earlier run are
/// narrowed here. An empty file is a valid empty database.
#[cfg(unix)]
fn restrict_to_owner(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let db = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    db.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    for suffix in ["-wal", "-shm"] {
        let mut side = path.as_os_str().to_owned();
        side.push(suffix);
        match std::fs::set_permissions(&side, std::fs::Permissions::from_mode(0o600)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

/// Open the service-only store for remembered volume passwords. The caller is
/// responsible for creating and validating its private parent directory before
/// this call; SQLite must never create this file in the public data directory.
pub fn open_volume_secret_database(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path).map_err(|e| OwnBackupError::Database(e.to_string()))?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=FULL;
         PRAGMA secure_delete=ON;
         CREATE TABLE IF NOT EXISTS volume_secrets (
             volume_key TEXT PRIMARY KEY,
             protected_blob BLOB NOT NULL,
             created_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS volume_secret_migration_pending (
             id INTEGER PRIMARY KEY CHECK (id = 1)
         );
         CREATE TABLE IF NOT EXISTS volume_secret_migration_done (
             id INTEGER PRIMARY KEY CHECK (id = 1)
         );",
    )
    .map_err(|e| OwnBackupError::Database(e.to_string()))?;
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
    if current_version < 2 {
        apply_v2(conn)?;
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

/// v2: saved passwords for encrypted (LUKS/BitLocker) volumes. The blob is
/// OS-protected (DPAPI on Windows) — never a plaintext password — so scheduled
/// backups can unlock the volume unattended.
fn apply_v2(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS volume_secrets (
            volume_key TEXT PRIMARY KEY,
            protected_blob BLOB NOT NULL,
            created_at TEXT NOT NULL
        );

        INSERT INTO schema_version (version, applied_at)
        VALUES (2, datetime('now'));",
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

    /// The database and both WAL-mode side files are owner-only — a fresh one,
    /// and one an older build left with the umask's modes.
    #[cfg(unix)]
    #[test]
    fn database_and_its_side_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("metadata.db");
        let wal = dir.path().join("metadata.db-wal");
        let shm = dir.path().join("metadata.db-shm");

        let conn = open_database(&db_path).unwrap();
        conn.execute_batch("INSERT INTO schema_version VALUES (99, 'now');")
            .unwrap();
        assert!(
            wal.exists() && shm.exists(),
            "WAL mode keeps both while open"
        );
        for p in [&db_path, &wal, &shm] {
            assert_eq!(mode(p), 0o600, "{}", p.display());
        }
        drop(conn);

        // As an older build left them: the umask's mode, and a WAL a crash
        // kept (an empty one is a valid, empty log).
        std::fs::set_permissions(&db_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::write(&wal, b"").unwrap();
        std::fs::set_permissions(&wal, std::fs::Permissions::from_mode(0o644)).unwrap();
        let _conn = open_database(&db_path).unwrap();
        assert_eq!(mode(&db_path), 0o600);
        assert_eq!(mode(&wal), 0o600);
    }
}
