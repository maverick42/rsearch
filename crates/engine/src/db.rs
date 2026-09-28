//! SQLite helpers: schema, build pragmas, validation and version checks.
//!
//! The build database is created at `<index_path>.building` and only
//! becomes the active index after full validation and an atomic replace.
//! The pragmas below intentionally trade crash-safety for speed on the
//! *build* database only; they are never applied to the active searchable
//! index (a failed build only ever loses the `.building` file).

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

use crate::error::{BuildError, FatalErrorKind, IndexError};
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
/// Thin wrapper over [`validate_connection`] mapping failures to a
/// fatal build error — same checks, same implementation as
/// [`crate::verify_index`]. Opened read-only: validation never writes
/// to the index.
pub(crate) fn validate_index(path: &Path) -> Result<(), BuildError> {
    let conn = open_readonly(path).map_err(|e| {
        fatal(
            FatalErrorKind::DatabaseFailure,
            format!("cannot open index for validation: {e}"),
        )
    })?;
    validate_connection(&conn).map_err(|e| {
        fatal(
            FatalErrorKind::DatabaseFailure,
            format!("index validation failed: {e}"),
        )
    })
}

/// Opens an index database read-only. Never creates nor modifies the
/// file and does not interact with any `.building` file — safe to call
/// while a build is running on the same index path.
pub(crate) fn open_readonly(path: &Path) -> Result<Connection, IndexError> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| IndexError::Sqlite(format!("cannot open index {}: {e}", path.display())))
}

/// Validates an open index connection: `complete = 1`, supported
/// schema version, all expected tables present, and a real FTS5
/// trigram query that executes successfully.
///
/// Single implementation shared by build-time validation (via
/// [`validate_index`]) and [`crate::verify_index`].
pub(crate) fn validate_connection(conn: &Connection) -> Result<(), IndexError> {
    let complete: String = conn
        .query_row("SELECT value FROM meta WHERE key = 'complete'", [], |r| {
            r.get(0)
        })
        .map_err(meta_query_error)?;
    if complete != "1" {
        return Err(IndexError::Incomplete);
    }

    let version: i32 = conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .map_err(meta_query_error)?;
    if version != SCHEMA_VERSION {
        return Err(IndexError::UnsupportedSchema(version));
    }

    for table in ["meta", "sources", "documents", "fts"] {
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |r| r.get(0),
            )
            .map_err(meta_query_error)?;
        if exists != 1 {
            return Err(IndexError::NotAnIndex(format!("table {table} is missing")));
        }
    }

    // Basic FTS query through the trigram tokenizer. An empty result is
    // fine (the probe matches nothing); the point is that preparing and
    // running the query succeeds, proving the FTS5 path works.
    match conn.query_row(
        "SELECT rowid FROM fts WHERE fts MATCH '\"rsearch_validation_probe\"' LIMIT 1",
        [],
        |_| Ok(()),
    ) {
        Ok(()) | Err(rusqlite::Error::QueryReturnedNoRows) => Ok(()),
        Err(e) => Err(IndexError::Fts5Unusable(e.to_string())),
    }
}

/// Maps failures of the schema/meta queries. A missing `meta` row means
/// the marker is absent (incomplete index); a "not a database" or
/// missing-table failure means the file is not an index at all.
fn meta_query_error(e: rusqlite::Error) -> IndexError {
    match e {
        rusqlite::Error::QueryReturnedNoRows => IndexError::Incomplete,
        rusqlite::Error::SqliteFailure(f, ref msg) => {
            let msg = msg.clone().unwrap_or_else(|| e.to_string());
            if f.extended_code == rusqlite::ffi::SQLITE_NOTADB || msg.contains("no such table") {
                IndexError::NotAnIndex(msg)
            } else {
                IndexError::Sqlite(msg)
            }
        }
        other => IndexError::Sqlite(other.to_string()),
    }
}

/// UI-facing summary of a verified index, gathered without scanning
/// the documents table (counts come from the build counters in `meta`).
#[derive(Debug, Clone)]
pub struct IndexInfo {
    /// Index schema version (always [`SCHEMA_VERSION`] on success).
    pub schema_version: i32,
    /// SQLite version that produced the index, when recorded.
    pub sqlite_version: Option<String>,
    /// Build completion time, seconds since the Unix epoch.
    pub built_at_unix_secs: Option<i64>,
    /// Source directories recorded by the build.
    pub sources: Vec<String>,
    /// Number of indexed files, from the build counters in `meta`.
    pub indexed_files: u64,
    /// Index file size in bytes.
    pub size_bytes: u64,
}

/// Collects [`IndexInfo`] from an already-validated connection.
pub(crate) fn index_info(conn: &Connection, size_bytes: u64) -> IndexInfo {
    let meta = |key: &str| -> Option<String> {
        conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .ok()
    };
    let sources = conn
        .prepare("SELECT path FROM sources ORDER BY id")
        .and_then(|mut stmt| {
            stmt.query_map([], |r| r.get::<_, String>(0))
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();
    let counters = meta("counters").unwrap_or_default();
    IndexInfo {
        schema_version: SCHEMA_VERSION,
        sqlite_version: meta("sqlite_version"),
        built_at_unix_secs: meta("build_timestamp").and_then(|v| v.parse().ok()),
        sources,
        indexed_files: counter_json_u64(&counters, "files_indexed").unwrap_or(0),
        size_bytes,
    }
}

/// Extracts a `"key":N` integer from the flat counters JSON written by
/// the writer (no JSON parser dependency needed for a flat object).
fn counter_json_u64(json: &str, key: &str) -> Option<u64> {
    let pat = format!("\"{key}\":");
    let pos = json.find(&pat)? + pat.len();
    json[pos..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()
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
