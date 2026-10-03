//! Document listing and candidate assembly over a finished index.
//!
//! Only this submodule speaks SQL against the index database; the rest
//! of the engine sees [`DocumentRef`] values and the `Connection` never
//! leaves the crate.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::db;
use crate::error::{IndexError, STATUS_ERROR, STATUS_SECURITY_LIMIT, STATUS_TOO_LARGE};
use crate::options::EncodingKind;

/// Fallback for the verification read cap when the index predates the
/// `max_indexed_file_size` meta key. Same order of magnitude as the
/// build option default.
pub(crate) const DEFAULT_MAX_VERIFY_BYTES: u64 = 16 * 1024 * 1024;

/// One row of the `documents` table: what the verifier needs to reopen
/// the real content (`file_path`/`entry_path`), to apply an extension
/// filter, and to detect staleness since the snapshot (`size`/`mtime`).
#[derive(Debug, Clone)]
pub struct DocumentRef {
    /// Document id (also the FTS rowid for indexed documents).
    pub id: i64,
    /// Physical file path as stored by the build (UTF-8, lossless).
    pub file_path: PathBuf,
    /// Entry path inside an archive (`inner.zip!/dir/x.xml`), `None`
    /// for regular files.
    pub entry_path: Option<String>,
    /// Lowercase extension without dot, when known.
    pub ext: Option<String>,
    /// Size in bytes recorded at build time (bytes actually read for
    /// indexed documents, entry size for archive entries).
    pub size: u64,
    /// Modification time recorded at build time, nanoseconds since the
    /// Unix epoch (archive mtime for entries).
    pub mtime: Option<i64>,
    /// `documents.status` value (see the `STATUS_*` constants).
    pub status: i32,
}

/// Opens `index_path` read-only, validates it (the same checks as
/// [`crate::verify_index`]) and returns the `documents` rows whose
/// `status` is in `statuses`, ordered by path. An empty `statuses`
/// slice selects every document. The internal `Connection` is never
/// exposed.
pub fn iter_documents(index_path: &Path, statuses: &[i32]) -> Result<Vec<DocumentRef>, IndexError> {
    let conn = open_index_readonly(index_path)?;
    select_documents(&conn, statuses)
}

/// Opens the index read-only and validates it. Single entry point for
/// every read access: searching never creates nor modifies the file
/// and is safe while a build writes its separate `.building` database.
pub(crate) fn open_index_readonly(index_path: &Path) -> Result<Connection, IndexError> {
    let md = std::fs::symlink_metadata(index_path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => IndexError::NotFound,
        _ => IndexError::Io(e),
    })?;
    if !md.is_file() {
        return Err(IndexError::NotAnIndex("not a regular file".into()));
    }
    let conn = db::open_readonly(index_path)?;
    db::validate_connection(&conn)?;
    Ok(conn)
}

/// `documents` rows filtered by status (empty filter selects all rows).
pub(crate) fn select_documents(
    conn: &Connection,
    statuses: &[i32],
) -> Result<Vec<DocumentRef>, IndexError> {
    // The status list is built from `i32` values only — never from user
    // text — so the generated `IN (...)` cannot inject SQL.
    let filter = if statuses.is_empty() {
        String::new()
    } else {
        let list = statuses
            .iter()
            .map(i32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        format!("WHERE status IN ({list})")
    };
    let sql = format!(
        "SELECT id, file_path, entry_path, ext, size, mtime, status \
         FROM documents {filter} ORDER BY file_path, entry_path"
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| IndexError::Sqlite(e.to_string()))?;
    let rows = stmt
        .query_map([], document_ref_row)
        .map_err(|e| IndexError::Sqlite(e.to_string()))?;
    let mut docs = Vec::new();
    for row in rows {
        docs.push(row.map_err(|e| IndexError::Sqlite(e.to_string()))?);
    }
    Ok(docs)
}

/// The candidate set for one literal query: FTS matches unioned with
/// too-large documents, deduplicated, extension-filtered and ordered
/// by path. Error/security-limit rows are never candidates; they are
/// only counted so the caller can report "N files could not be
/// verified".
pub(crate) struct CandidateSet {
    /// Documents to verify against real content (status 0 via FTS,
    /// status 2 via the union). `from_index + too_large` entries,
    /// ordered by `(file_path, entry_path)`.
    pub documents: Vec<DocumentRef>,
    /// Documents selected through the FTS index.
    pub from_index: usize,
    /// Documents selected through the too-large union (they are never
    /// in FTS by construction; a document appearing via both paths is
    /// counted once, on the FTS side).
    pub too_large: usize,
    /// Documents with status 3 (index-time error) passing the
    /// extension filter — never attempted.
    pub index_errors: usize,
    /// Documents with status 4 (security limit) passing the extension
    /// filter — never attempted.
    pub security_limits: usize,
}

/// Assembles candidates for a `MATCH` phrase built by
/// [`super::query::to_fts5_phrase`]. `extensions` (lowercase, with or
/// without leading dot) filters every candidate class; `None` keeps
/// everything.
pub(crate) fn select_candidates(
    conn: &Connection,
    fts_phrase: &str,
    extensions: Option<&[String]>,
) -> Result<CandidateSet, IndexError> {
    let ext_filter: Option<HashSet<String>> = extensions.map(|list| {
        list.iter()
            .map(|e| normalize_ext(e))
            .collect::<HashSet<_>>()
    });
    let keep = |d: &DocumentRef| -> bool {
        match &ext_filter {
            None => true,
            Some(set) => d.ext.as_deref().is_some_and(|e| set.contains(e)),
        }
    };

    let mut seen: HashSet<i64> = HashSet::new();
    let mut documents: Vec<DocumentRef> = Vec::new();

    // 1. FTS candidates (status-0 documents have the only FTS rows).
    let mut stmt = conn
        .prepare(
            "SELECT d.id, d.file_path, d.entry_path, d.ext, d.size, d.mtime, d.status \
             FROM fts JOIN documents d ON d.id = fts.rowid \
             WHERE fts MATCH ?1 \
             ORDER BY d.file_path, d.entry_path",
        )
        .map_err(|e| IndexError::Sqlite(e.to_string()))?;
    let rows = stmt
        .query_map([fts_phrase], document_ref_row)
        .map_err(|e| IndexError::Sqlite(e.to_string()))?;
    let mut from_index = 0usize;
    for row in rows {
        let doc = row.map_err(|e| IndexError::Sqlite(e.to_string()))?;
        if keep(&doc) && seen.insert(doc.id) {
            from_index += 1;
            documents.push(doc);
        }
    }

    // 2. Too-large documents are never in FTS; they join the candidate
    //    set unconditionally (deduplicated defensively).
    let mut too_large = 0usize;
    for doc in select_documents(conn, &[STATUS_TOO_LARGE])? {
        if keep(&doc) && seen.insert(doc.id) {
            too_large += 1;
            documents.push(doc);
        }
    }

    documents.sort_by(|a, b| {
        a.file_path
            .cmp(&b.file_path)
            .then_with(|| a.entry_path.cmp(&b.entry_path))
    });

    // 3. Error and security-limit rows are counted separately, never
    //    verified.
    let mut index_errors = 0usize;
    let mut security_limits = 0usize;
    for doc in select_documents(conn, &[STATUS_ERROR, STATUS_SECURITY_LIMIT])? {
        if keep(&doc) {
            if doc.status == STATUS_ERROR {
                index_errors += 1;
            } else {
                security_limits += 1;
            }
        }
    }

    Ok(CandidateSet {
        documents,
        from_index,
        too_large,
        index_errors,
        security_limits,
    })
}

fn document_ref_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<DocumentRef> {
    Ok(DocumentRef {
        id: r.get(0)?,
        file_path: PathBuf::from(r.get::<_, String>(1)?),
        entry_path: r.get(2)?,
        ext: r.get(3)?,
        size: r.get::<_, i64>(4)? as u64,
        mtime: r.get(5)?,
        status: r.get(6)?,
    })
}

/// Normalizes a caller-provided extension filter (`".TXT"`, `"Txt"`,
/// `"txt"` are all the same extension).
fn normalize_ext(e: &str) -> String {
    e.trim_start_matches('.').to_lowercase()
}

/// Fallback encoding recorded at build time (`meta.fallback_encoding`).
/// `None` when the index predates the key or the build used none — in
/// both cases strict decoding is correct.
pub(crate) fn index_fallback_encoding(conn: &Connection) -> Option<EncodingKind> {
    match meta_value(conn, "fallback_encoding").as_deref() {
        Some("utf8") => Some(EncodingKind::Utf8),
        Some("windows1252") => Some(EncodingKind::Windows1252),
        _ => None,
    }
}

/// Build-time `max_indexed_file_size` from `meta`, used as the safety
/// cap when streaming-verifying too-large documents. Falls back to
/// [`DEFAULT_MAX_VERIFY_BYTES`] for indexes predating the key.
pub(crate) fn index_max_verify_bytes(conn: &Connection) -> u64 {
    meta_value(conn, "max_indexed_file_size")
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_MAX_VERIFY_BYTES)
}

fn meta_value(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
        .ok()
}
