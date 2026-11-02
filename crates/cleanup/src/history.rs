//! Keeping the last days of a Chromium browser's history (`History`, SQLite)
//! and removing what is older — the window's «хранить историю N дней».
//!
//! The database is edited, not deleted, so it is edited the careful way:
//!
//! - never while its browser runs (the target names the browser; the engine
//!   leaves the folder) and never with a journal beside it — a journal means
//!   the browser stopped in the middle of a write, and the database is only
//!   whole together with it;
//! - on a copy: the copy is trimmed, vacuumed and checked (`quick_check`),
//!   and only then put in the original's place. Any failure leaves the
//!   original as it was;
//! - by Chromium's own tables, each only when it is there: the visits older
//!   than the date, then what only they kept — addresses with no visit left,
//!   their search terms, old downloads, old "most visited" counts. None of those
//!   tables cascades: search terms and titles of removed visits stayed in the
//!   annotations and clusters until they were asked for by name (owner's Vivaldi,
//!   2026-11-02: no visits at all, 64 such rows).
//!
//! Firefox is not edited: its database's triggers call functions only
//! Firefox has, and its history shares the file with the bookmarks.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OpenFlags};

/// Microseconds between 1601-01-01 (Chromium's epoch, Windows') and 1970.
const EPOCH_1601_TO_1970_US: i64 = 11_644_473_600_000_000;

/// A time as Chromium stores it: microseconds since 1601-01-01 UTC.
pub fn chromium_time(t: SystemTime) -> i64 {
    let since_1970 = t
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_micros() as i64;
    since_1970 + EPOCH_1601_TO_1970_US
}

/// A journal or write-ahead log beside the database: it is not whole alone.
fn has_journal(db: &Path) -> bool {
    ["-journal", "-wal"].iter().any(|s| {
        let mut p = db.as_os_str().to_owned();
        p.push(s);
        Path::new(&p).exists()
    })
}

fn table_exists(c: &Connection, name: &str) -> bool {
    c.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [name],
        |_| Ok(()),
    )
    .is_ok()
}

/// Whether `table` has `column` (false for a table that is not there).
fn column_exists(c: &Connection, table: &str, column: &str) -> bool {
    let Ok(mut q) = c.prepare(&format!("PRAGMA table_info({table})")) else {
        return false;
    };
    let names = q.query_map([], |r| r.get::<_, String>(1));
    names.is_ok_and(|mut rows| rows.any(|n| n.is_ok_and(|n| n == column)))
}

/// How many visits older than `before` the database holds.
pub fn count_old(db: &Path, before: SystemTime) -> Result<u64, String> {
    if has_journal(db) {
        return Err("a journal is beside the database".into());
    }
    let c = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| e.to_string())?;
    if !table_exists(&c, "visits") {
        return Err("no visits table".into());
    }
    c.query_row(
        "SELECT count(*) FROM visits WHERE visit_time < ?1",
        [chromium_time(before)],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n as u64)
    .map_err(|e| e.to_string())
}

/// Remove the visits older than `before`, and what only they kept; how many
/// visits went. The original is replaced only by a checked copy.
pub fn trim(db: &Path, before: SystemTime) -> Result<u64, String> {
    if has_journal(db) {
        return Err("a journal is beside the database".into());
    }
    let copy = sibling(db, ".own-backup-trim");
    let result = trim_copy(db, &copy, before);
    if result.is_err() {
        let _ = std::fs::remove_file(&copy);
    }
    result
}

fn sibling(db: &Path, suffix: &str) -> PathBuf {
    let mut p = db.as_os_str().to_owned();
    p.push(suffix);
    PathBuf::from(p)
}

/// What the database keeps about visits and addresses that are gone, and the counts
/// of those that stay. Each table and column only when it is there.
fn forget_visits(c: &Connection) -> Result<(), String> {
    let run = |sql: String| c.execute(&sql, []).map(|_| ()).map_err(|e| e.to_string());
    if table_exists(c, "visit_source") {
        run("DELETE FROM visit_source WHERE id NOT IN (SELECT id FROM visits)".into())?;
    }
    for (table, column) in [
        ("content_annotations", "visit_id"),
        ("context_annotations", "visit_id"),
        ("clusters_and_visits", "visit_id"),
        ("cluster_visit_duplicates", "visit_id"),
        ("cluster_visit_duplicates", "duplicate_visit_id"),
    ] {
        if column_exists(c, table, column) {
            run(format!(
                "DELETE FROM {table} WHERE {column} NOT IN (SELECT id FROM visits)"
            ))?;
        }
    }
    if column_exists(c, "clusters_and_visits", "cluster_id") {
        for table in ["clusters", "cluster_keywords"] {
            if column_exists(c, table, "cluster_id") {
                run(format!(
                    "DELETE FROM {table} WHERE cluster_id NOT IN \
                     (SELECT cluster_id FROM clusters_and_visits)"
                ))?;
            }
        }
    }
    if column_exists(c, "visited_links", "link_url_id") {
        run("DELETE FROM visited_links WHERE link_url_id NOT IN (SELECT id FROM urls)".into())?;
    }
    if column_exists(c, "urls", "visit_count") {
        run("UPDATE urls SET visit_count = \
             (SELECT count(*) FROM visits WHERE visits.url = urls.id)"
            .into())?;
        if column_exists(c, "urls", "typed_count") {
            run("UPDATE urls SET typed_count = min(typed_count, visit_count)".into())?;
        }
    }
    Ok(())
}

fn trim_copy(db: &Path, copy: &Path, before: SystemTime) -> Result<u64, String> {
    std::fs::copy(db, copy).map_err(|e| format!("copy: {e}"))?;
    let removed = {
        let mut c = Connection::open(copy).map_err(|e| e.to_string())?;
        // The copy has no journal of its own yet; a rollback journal while
        // it is edited, as SQLite does by default.
        let cutoff = chromium_time(before);
        let tx = c.transaction().map_err(|e| e.to_string())?;
        if !table_exists(&tx, "visits") || !table_exists(&tx, "urls") {
            return Err("not a Chromium history database".into());
        }
        let exec = |sql: &str| tx.execute(sql, params![cutoff]).map_err(|e| e.to_string());
        let plain = |sql: &str| tx.execute(sql, []).map_err(|e| e.to_string());
        if table_exists(&tx, "visit_source") {
            exec("DELETE FROM visit_source WHERE id IN (SELECT id FROM visits WHERE visit_time < ?1)")?;
        }
        let removed = exec("DELETE FROM visits WHERE visit_time < ?1")? as u64;
        if table_exists(&tx, "keyword_search_terms") {
            plain(
                "DELETE FROM keyword_search_terms WHERE url_id IN \
                 (SELECT id FROM urls WHERE id NOT IN (SELECT url FROM visits))",
            )?;
        }
        plain("DELETE FROM urls WHERE id NOT IN (SELECT url FROM visits)")?;
        forget_visits(&tx)?;
        if table_exists(&tx, "segment_usage") {
            exec("DELETE FROM segment_usage WHERE time_slot < ?1")?;
            if table_exists(&tx, "segments") {
                plain(
                    "DELETE FROM segments WHERE id NOT IN (SELECT segment_id FROM segment_usage)",
                )?;
            }
        }
        if table_exists(&tx, "downloads") {
            // A download's url chain keys on `id`, its slices on
            // `download_id` (Chromium; owner's Vivaldi, 2026-10-28) — the
            // column is asked, not assumed.
            for chained in ["downloads_url_chains", "downloads_slices"] {
                let Some(column) = ["download_id", "id"]
                    .into_iter()
                    .find(|c| column_exists(&tx, chained, c))
                else {
                    continue;
                };
                let sql = format!(
                    "DELETE FROM {chained} WHERE {column} IN \
                     (SELECT id FROM downloads WHERE start_time < ?1)"
                );
                exec(&sql)?;
            }
            exec("DELETE FROM downloads WHERE start_time < ?1")?;
        }
        tx.commit().map_err(|e| e.to_string())?;
        c.execute_batch("VACUUM").map_err(|e| e.to_string())?;
        let check: String = c
            .query_row("PRAGMA quick_check", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        if check != "ok" {
            return Err(format!("the trimmed copy does not check: {check}"));
        }
        removed
    };
    // The copy's own journal is gone with its connection; put it in place.
    if has_journal(copy) {
        return Err("the trimmed copy kept a journal".into());
    }
    std::fs::rename(copy, db).map_err(|e| format!("replace: {e}"))?;
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small database in Chromium's shape.
    fn history(dir: &Path) -> PathBuf {
        let db = dir.join("History");
        let c = Connection::open(&db).unwrap();
        c.execute_batch(
            "CREATE TABLE urls (id INTEGER PRIMARY KEY, url TEXT, title TEXT, last_visit_time INTEGER,
                                 visit_count INTEGER DEFAULT 0, typed_count INTEGER DEFAULT 0);
             CREATE TABLE content_annotations (visit_id INTEGER PRIMARY KEY, search_terms TEXT);
             CREATE TABLE context_annotations (visit_id INTEGER PRIMARY KEY, duration INTEGER);
             CREATE TABLE clusters (cluster_id INTEGER PRIMARY KEY, label TEXT);
             CREATE TABLE clusters_and_visits (cluster_id INTEGER, visit_id INTEGER, url_for_display TEXT);
             CREATE TABLE cluster_keywords (cluster_id INTEGER, keyword TEXT);
             CREATE TABLE cluster_visit_duplicates (visit_id INTEGER, duplicate_visit_id INTEGER);
             CREATE TABLE visited_links (id INTEGER PRIMARY KEY, link_url_id INTEGER, top_level_url TEXT);
             CREATE TABLE visits (id INTEGER PRIMARY KEY, url INTEGER, visit_time INTEGER);
             CREATE TABLE visit_source (id INTEGER PRIMARY KEY, source INTEGER);
             CREATE TABLE keyword_search_terms (keyword_id INTEGER, url_id INTEGER, term TEXT);
             CREATE TABLE segments (id INTEGER PRIMARY KEY, name TEXT, url_id INTEGER);
             CREATE TABLE segment_usage (id INTEGER PRIMARY KEY, segment_id INTEGER, time_slot INTEGER, visit_count INTEGER);
             CREATE TABLE downloads (id INTEGER PRIMARY KEY, start_time INTEGER, target_path TEXT);
             CREATE TABLE downloads_url_chains (id INTEGER, chain_index INTEGER, url TEXT);
             CREATE TABLE downloads_slices (download_id INTEGER, offset INTEGER, received_bytes INTEGER);",
        )
        .unwrap();
        let now = SystemTime::now();
        let old = chromium_time(now - Duration::from_secs(30 * 86400));
        let new = chromium_time(now - Duration::from_secs(86400));
        // Two sites; the old one visited only a month ago, the new one both.
        c.execute_batch(&format!(
            "INSERT INTO urls VALUES (1, 'https://old.example/', 'old', {old}, 1, 1);
             INSERT INTO urls VALUES (2, 'https://new.example/', 'new', {new}, 2, 2);
             INSERT INTO content_annotations VALUES (1, 'old search'), (3, 'new search');
             INSERT INTO context_annotations VALUES (2, 5), (3, 5);
             INSERT INTO clusters VALUES (1, 'old cluster'), (2, 'new cluster');
             INSERT INTO clusters_and_visits VALUES (1, 1, 'old.example'), (2, 3, 'new.example');
             INSERT INTO cluster_keywords VALUES (1, 'old word'), (2, 'new word');
             INSERT INTO cluster_visit_duplicates VALUES (1, 3), (3, 3);
             INSERT INTO visited_links VALUES (1, 1, 'old.example'), (2, 2, 'new.example');
             INSERT INTO visits VALUES (1, 1, {old});
             INSERT INTO visits VALUES (2, 2, {old});
             INSERT INTO visits VALUES (3, 2, {new});
             INSERT INTO visit_source VALUES (1, 0);
             INSERT INTO keyword_search_terms VALUES (1, 1, 'old search');
             INSERT INTO keyword_search_terms VALUES (1, 2, 'new search');
             INSERT INTO segments VALUES (1, 'old', 1);
             INSERT INTO segment_usage VALUES (1, 1, {old}, 3);
             INSERT INTO downloads VALUES (1, {old}, 'C:\\old.zip');
             INSERT INTO downloads VALUES (2, {new}, 'C:\\new.zip');
             INSERT INTO downloads_url_chains VALUES (1, 0, 'https://old.example/old.zip');
             INSERT INTO downloads_slices VALUES (1, 0, 100);
             INSERT INTO downloads_slices VALUES (2, 0, 200);"
        ))
        .unwrap();
        db
    }

    fn count(db: &Path, sql: &str) -> i64 {
        Connection::open(db)
            .unwrap()
            .query_row(sql, [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn the_last_days_stay_and_the_older_history_goes() {
        let dir = tempfile::tempdir().unwrap();
        let db = history(dir.path());
        let week = SystemTime::now() - Duration::from_secs(7 * 86400);
        assert_eq!(count_old(&db, week).unwrap(), 2);

        assert_eq!(trim(&db, week).unwrap(), 2);
        assert_eq!(count(&db, "SELECT count(*) FROM visits"), 1);
        assert_eq!(
            count(&db, "SELECT count(*) FROM urls"),
            1,
            "the old site is gone"
        );
        assert_eq!(count(&db, "SELECT id FROM urls"), 2);
        assert_eq!(count(&db, "SELECT count(*) FROM keyword_search_terms"), 1);
        assert_eq!(count(&db, "SELECT count(*) FROM visit_source"), 0);
        assert_eq!(count(&db, "SELECT count(*) FROM segments"), 0);
        assert_eq!(count(&db, "SELECT count(*) FROM downloads"), 1);
        assert_eq!(count(&db, "SELECT count(*) FROM downloads_url_chains"), 0);
        assert_eq!(
            count(&db, "SELECT count(*) FROM downloads_slices"),
            1,
            "the new download's slice stays"
        );
        assert!(
            !sibling(&db, ".own-backup-trim").exists(),
            "no copy left behind"
        );
        assert_eq!(count_old(&db, week).unwrap(), 0);
    }

    /// Search terms, clusters and links of the visits removed go with them, and
    /// the counts of an address that stays are its visits that stay.
    #[test]
    fn nothing_of_a_removed_visit_stays_in_the_other_tables() {
        let dir = tempfile::tempdir().unwrap();
        let db = history(dir.path());
        let week = SystemTime::now() - Duration::from_secs(7 * 86400);
        trim(&db, week).unwrap();
        for (sql, left) in [
            ("SELECT count(*) FROM content_annotations", 1),
            ("SELECT visit_id FROM content_annotations", 3),
            ("SELECT count(*) FROM context_annotations", 1),
            ("SELECT count(*) FROM clusters_and_visits", 1),
            ("SELECT count(*) FROM clusters", 1),
            ("SELECT cluster_id FROM clusters", 2),
            ("SELECT count(*) FROM cluster_keywords", 1),
            ("SELECT count(*) FROM cluster_visit_duplicates", 1),
            ("SELECT count(*) FROM visited_links", 1),
            ("SELECT link_url_id FROM visited_links", 2),
            ("SELECT visit_count FROM urls", 1),
            ("SELECT typed_count FROM urls", 1),
        ] {
            assert_eq!(count(&db, sql), left, "{sql}");
        }
        let text = std::fs::read(&db).unwrap();
        for word in ["old search", "old cluster", "old word", "old.example"] {
            assert!(
                !text.windows(word.len()).any(|w| w == word.as_bytes()),
                "{word} is still in the file"
            );
        }
    }

    /// Rows left by an earlier cleanup go too, with no old visit to remove.
    #[test]
    fn rows_of_visits_already_gone_are_removed_as_well() {
        let dir = tempfile::tempdir().unwrap();
        let db = history(dir.path());
        Connection::open(&db)
            .unwrap()
            .execute("INSERT INTO content_annotations VALUES (99, 'orphan')", [])
            .unwrap();
        let long_ago = SystemTime::now() - Duration::from_secs(90 * 86400);
        assert_eq!(trim(&db, long_ago).unwrap(), 0);
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM content_annotations WHERE visit_id = 99"
            ),
            0
        );
    }

    /// A journal beside the database: the browser stopped mid-write, and the
    /// database is not touched.
    #[test]
    fn a_database_with_a_journal_is_left_as_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let db = history(dir.path());
        std::fs::write(sibling(&db, "-journal"), b"x").unwrap();
        let before = std::fs::read(&db).unwrap();
        let week = SystemTime::now() - Duration::from_secs(7 * 86400);
        assert!(trim(&db, week).is_err());
        assert!(count_old(&db, week).is_err());
        assert_eq!(std::fs::read(&db).unwrap(), before);
    }

    /// Something that is not Chromium's history is refused, the file intact.
    #[test]
    fn another_database_is_not_edited() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("History");
        Connection::open(&db)
            .unwrap()
            .execute_batch("CREATE TABLE notes (t TEXT); INSERT INTO notes VALUES ('keep');")
            .unwrap();
        let before = std::fs::read(&db).unwrap();
        assert!(trim(&db, SystemTime::now()).is_err());
        assert_eq!(std::fs::read(&db).unwrap(), before);
        assert!(!sibling(&db, ".own-backup-trim").exists());
        // Not a database at all.
        let junk = dir.path().join("junk");
        std::fs::write(&junk, b"not sqlite").unwrap();
        assert!(trim(&junk, SystemTime::now()).is_err());
        assert_eq!(std::fs::read(&junk).unwrap(), b"not sqlite");
    }

    #[test]
    fn chromium_time_counts_from_1601() {
        assert_eq!(chromium_time(UNIX_EPOCH), EPOCH_1601_TO_1970_US);
        assert_eq!(
            chromium_time(UNIX_EPOCH + Duration::from_secs(1)),
            EPOCH_1601_TO_1970_US + 1_000_000
        );
    }
}
