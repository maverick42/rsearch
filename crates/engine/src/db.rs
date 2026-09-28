//! SQLite helpers: schema, build pragmas, validation and version checks.
//!
//! The build database is created at `<index_path>.building` and only
//! becomes the active index after full validation and an atomic replace.
//! The pragmas below intentionally trade crash-safety for speed on the
//! *build* database only; they are never applied to the active searchable
//! index (a failed build only ever loses the `.building` file).

use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::error::{BuildError, FatalErrorKind};
use crate::options::{BuildOptions, JournalMode};

/// Current schema version, stored in `meta` and checked on validation.
pub const SCHEMA_VERSION: i32 = 1;

/// SQL statements creating the index schema.
pub const SCHEMA_SQL: &str = "
CREATE TABLE meta(
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE sources(
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE
);
CREATE TABLE documents(
    id INTEGER PRIMARY KEY,
    file_path TEXT NOT NULL,
    entry_path TEXT,
    ext TEXT,
    size INTEGER NOT NULL,
    mtime INTEGER,
    status INTEGER NOT NULL,
    reason TEXT
);
CREATE INDEX idx_documents_path ON documents(file_path);
CREATE VIRTUAL TABLE fts USING fts5(
    content,
    content = '',
    tokenize = 'trigram case_sensitive 0'
);
";

/// Path of the temporary build database for `index_path`.
pub fn building_path(index_path: &Path) -> PathBuf {
    let mut s = index_path.as_os_str().to_os_string();
    s.push(".building");
    PathBuf::from(s)
}

/// Opens the bundled SQLite version string (e.g. `"3.45.1"`).
pub fn bundled_sqlite_version() -> &'static str {
    rusqlite::version()
}

/// Returns a fatal `BuildError` with the given kind and message.
pub(crate) fn fatal(kind: FatalErrorKind, message: impl Into<String>) -> BuildError {
    BuildError::Fatal {
        kind,
        message: message.into(),
        report: None,
    }
}

/// Opens a new build database at `path` and applies the build pragmas and
/// schema.
pub(crate) fn open_build_db(path: &Path, opts: &BuildOptions) -> Result<Connection, BuildError> {
    let conn = Connection::open(path).map_err(|e| {
        fatal(
            FatalErrorKind::SqliteInit,
            format!("cannot open build database {path:?}: {e}"),
        )
    })?;

    let journal = match opts.sqlite_journal_mode {
        JournalMode::Memory => "MEMORY",
        JournalMode::Off => "OFF",
    };
    // Build-database-only pragmas. See module documentation.
    conn.pragma_update(None, "page_size", opts.sqlite_page_size)
        .map_err(|e| {
            fatal(
                FatalErrorKind::SqliteInit,
                format!("page_size pragma failed: {e}"),
            )
        })?;
    let pragmas: Vec<String> = [
        format!("PRAGMA journal_mode = {journal};"),
        "PRAGMA locking_mode = EXCLUSIVE;".to_string(),
        "PRAGMA synchronous = OFF;".to_string(),
        "PRAGMA temp_store = MEMORY;".to_string(),
        "PRAGMA cache_size = -200000;".to_string(),
    ]
    .to_vec();
    for pragma in pragmas {
        conn.execute_batch(&pragma).map_err(|e| {
            fatal(
                FatalErrorKind::SqliteInit,
                format!("pragma failed: {pragma} {e}"),
            )
        })?;
    }

    conn.execute_batch(SCHEMA_SQL).map_err(|e| {
        fatal(
            FatalErrorKind::SchemaCreation,
            format!("schema creation failed: {e}"),
        )
    })?;

    verify_fts5_support(&conn)?;
    Ok(conn)
}

/// Verifies that the opened connection actually supports FTS5, the
/// trigram tokenizer and contentless tables. This is checked at runtime
/// on every build database instead of being assumed from documentation.
pub(crate) fn verify_fts5_support(conn: &Connection) -> Result<(), BuildError> {
    conn.execute_batch(
        "CREATE VIRTUAL TABLE fts_probe USING fts5(content, content='', tokenize='trigram case_sensitive 0');
         INSERT INTO fts_probe(rowid, content) VALUES (1, 'probe trigram content');
         DROP TABLE fts_probe;",
    )
    .map_err(|e| {
        fatal(
            FatalErrorKind::SqliteInit,
            format!(
                "bundled SQLite {} does not support FTS5 contentless trigram tables: {e}",
                bundled_sqlite_version()
            ),
        )
    })
}

/// Validates a finished index database before or after activation.
///
/// `complete = 1` alone is not sufficient: the schema version must be
/// supported, all expected tables must exist and a real FTS query must
/// succeed.
pub(crate) fn validate_index(path: &Path) -> Result<(), BuildError> {
    let conn = Connection::open(path).map_err(|e| {
        fatal(
            FatalErrorKind::DatabaseFailure,
            format!("cannot open index for validation: {e}"),
        )
    })?;

    let complete: String = conn
        .query_row("SELECT value FROM meta WHERE key = 'complete'", [], |r| {
            r.get(0)
        })
        .map_err(|e| {
            fatal(
                FatalErrorKind::DatabaseFailure,
                format!("missing complete marker: {e}"),
            )
        })?;
    if complete != "1" {
        return Err(fatal(
            FatalErrorKind::DatabaseFailure,
            format!("index is not complete (complete = {complete})"),
        ));
    }

    let version: i32 = conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .map_err(|e| {
            fatal(
                FatalErrorKind::DatabaseFailure,
                format!("missing schema version: {e}"),
            )
        })?;
    if version != SCHEMA_VERSION {
        return Err(fatal(
            FatalErrorKind::DatabaseFailure,
            format!("unsupported schema version {version} (expected {SCHEMA_VERSION})"),
        ));
    }

    for table in ["meta", "sources", "documents", "fts"] {
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |r| r.get(0),
            )
            .map_err(|e| {
                fatal(
                    FatalErrorKind::DatabaseFailure,
                    format!("cannot inspect schema: {e}"),
                )
            })?;
        if exists != 1 {
            return Err(fatal(
                FatalErrorKind::DatabaseFailure,
                format!("table {table} is missing"),
            ));
        }
    }

    // Basic FTS query through the trigram tokenizer. An empty result is
    // fine (the probe matches nothing); the point is that preparing and
    // running the query succeeds, proving the FTS5 path works.
    let query = conn.query_row(
        "SELECT rowid FROM fts WHERE fts MATCH '\"rsearch_validation_probe\"' LIMIT 1",
        [],
        |_| Ok(()),
    );
    if let Err(e) = query {
        if e != rusqlite::Error::QueryReturnedNoRows {
            return Err(fatal(
                FatalErrorKind::DatabaseFailure,
                format!("FTS query failed during validation: {e}"),
            ));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "rsearch-dbtest-{}-{}.sqlite",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn sqlite_version_is_reported() {
        let version = bundled_sqlite_version();
        assert!(
            !version.is_empty(),
            "bundled SQLite version must be available"
        );
    }

    #[test]
    fn fts5_trigram_contentless_works_with_bundled_sqlite() {
        let path = test_db_path("fts5");
        let mut opts = BuildOptions::default();
        opts.source_directories.push(PathBuf::from("."));
        let conn = open_build_db(&path, &opts).unwrap();

        conn.execute(
            "INSERT INTO documents(file_path, entry_path, ext, size, mtime, status, reason) VALUES ('a.txt', NULL, 'txt', 1, 1, 0, NULL)",
            [],
        )
        .unwrap();
        let rowid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO fts(rowid, content) VALUES (?1, ?2)",
            rusqlite::params![rowid, "hello searchable world"],
        )
        .unwrap();

        let found: i64 = conn
            .query_row(
                "SELECT rowid FROM fts WHERE fts MATCH '\"searchable\"'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(found, rowid);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn validate_rejects_incomplete_database() {
        let path = test_db_path("incomplete");
        let mut opts = BuildOptions::default();
        opts.source_directories.push(PathBuf::from("."));
        {
            let _conn = open_build_db(&path, &opts).unwrap();
        }
        let err = validate_index(&path).unwrap_err();
        assert!(err.to_string().contains("complete"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn validate_accepts_complete_database() {
        let path = make_complete_db();
        let err = validate_index(&path);
        assert!(err.is_ok(), "{err:?}");
        let _ = std::fs::remove_file(&path);
    }

    fn make_complete_db() -> PathBuf {
        let path = test_db_path("complete");
        let mut opts = BuildOptions::default();
        opts.source_directories.push(PathBuf::from("."));
        let conn = open_build_db(&path, &opts).unwrap();
        conn.execute(
            "INSERT INTO meta(key, value) VALUES ('complete', '1'), ('schema_version', '1')",
            [],
        )
        .unwrap();
        path
    }
}
