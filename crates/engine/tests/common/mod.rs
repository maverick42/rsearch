//! Shared helpers for integration tests: unique temp directories and
//! small file-tree builders. Deliberately dependency-free.
//!
//! Each test binary compiles this module separately and uses only a
//! subset of the helpers.
#![allow(dead_code)]
//!
//! Layout of a `TempDir`:
//!
//! ```text
//! <tmp>/rsearch-test-<label>-<pid>-<n>/
//! ├── src/          <- scanned source directory (all `write` calls land here)
//! └── index.db      <- built index, OUTSIDE the scanned tree so builds
//!                      never observe their own database files
//! ```

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A temp directory that removes itself on drop.
pub struct TempDir {
    base: PathBuf,
    src: PathBuf,
}

impl TempDir {
    pub fn new(label: &str) -> TempDir {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "rsearch-test-{}-{}-{}",
            label,
            std::process::id(),
            id
        ));
        let _ = std::fs::remove_dir_all(&base);
        let src = base.join("src");
        std::fs::create_dir_all(&src).expect("create temp dir");
        TempDir { base, src }
    }

    /// The scanned source directory.
    pub fn path(&self) -> &Path {
        &self.src
    }

    /// Path inside the source tree.
    pub fn join(&self, rel: &str) -> PathBuf {
        self.src.join(rel)
    }

    /// Path of the built index (outside the source tree).
    pub fn index_path(&self) -> PathBuf {
        self.base.join("index.db")
    }

    /// Path of the temporary build database.
    pub fn building_path(&self) -> PathBuf {
        self.base.join("index.db.building")
    }

    /// Writes a UTF-8 text file into the source tree (creating parent
    /// directories).
    pub fn write(&self, rel: &str, content: &str) -> PathBuf {
        self.write_bytes(rel, content.as_bytes())
    }

    /// Writes raw bytes into the source tree (creating parent
    /// directories).
    pub fn write_bytes(&self, rel: &str, content: &[u8]) -> PathBuf {
        let path = self.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dir");
        }
        std::fs::write(&path, content).expect("write file");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Go through the long-path helper so test trees deeper than
        // MAX_PATH are removed correctly.
        let p = rsearch_engine::longpath::io_path(&self.base).unwrap_or_else(|_| self.base.clone());
        let _ = std::fs::remove_dir_all(p);
    }
}

/// Default test options for a single source directory.
pub fn opts_for(root: &Path) -> rsearch_engine::BuildOptions {
    rsearch_engine::BuildOptions {
        source_directories: vec![root.to_path_buf()],
        ..rsearch_engine::BuildOptions::default()
    }
}

/// Builds an index (outside the source tree) and waits for it.
pub fn build_ok(dir: &TempDir, opts: rsearch_engine::BuildOptions) -> rsearch_engine::BuildReport {
    let index = dir.index_path();
    let handle = rsearch_engine::rebuild_index(&index, opts);
    handle.wait().expect("build must succeed")
}

/// Opens the built index database for verification.
pub fn open_index(dir: &TempDir) -> rusqlite::Connection {
    let index = dir.index_path();
    assert!(index.exists(), "index must exist at {}", index.display());
    rusqlite::Connection::open(&index).expect("open index")
}

/// Returns document rows for a file path: (status, entry_path, reason).
pub fn documents_for(
    conn: &rusqlite::Connection,
    file_path: &str,
) -> Vec<(i32, Option<String>, Option<String>)> {
    let mut stmt = conn
        .prepare("SELECT status, entry_path, reason FROM documents WHERE file_path = ?1")
        .expect("prepare");
    let rows = stmt
        .query_map([file_path], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .expect("query");
    rows.filter_map(|r| r.ok()).collect()
}

/// Returns document rows for any file path containing `needle`.
pub fn documents_like(
    conn: &rusqlite::Connection,
    needle: &str,
) -> Vec<(String, i32, Option<String>)> {
    let mut stmt = conn
        .prepare("SELECT file_path, status, entry_path FROM documents WHERE file_path LIKE ?1")
        .expect("prepare");
    let pattern = format!("%{needle}%");
    let rows = stmt
        .query_map([pattern], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .expect("query");
    rows.filter_map(|r| r.ok()).collect()
}

/// Queries the FTS index with an escaped phrase and returns matching
/// rowids. The phrase is bound as a SQL parameter: user text is never
/// interpolated into the SQL string itself.
pub fn fts_match(conn: &rusqlite::Connection, phrase: &str) -> Vec<i64> {
    let escaped = rsearch_engine::fts::escape_fts_phrase(phrase);
    let mut stmt = conn
        .prepare("SELECT rowid FROM fts WHERE fts MATCH ?1")
        .expect("prepare fts");
    let rows = stmt
        .query_map([escaped], |r| r.get::<_, i64>(0))
        .expect("query fts");
    rows.filter_map(|r| r.ok()).collect()
}

/// Builds a ZIP archive at `path` from `(name, bytes)` entries.
pub fn make_zip<S: AsRef<str>>(path: &Path, entries: Vec<(S, Vec<u8>)>) {
    let file = std::fs::File::create(path).expect("create zip");
    let mut zip = zip::ZipWriter::new(file);
    let options: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, bytes) in &entries {
        zip.start_file(name.as_ref(), options).expect("start entry");
        std::io::Write::write_all(&mut zip, bytes).expect("write entry");
    }
    zip.finish().expect("finish zip");
}
