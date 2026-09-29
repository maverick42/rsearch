//! The single SQLite writer thread.
//!
//! Exactly one thread owns the database connection during a build.
//! Workers never write SQLite directly. The writer owns document ID
//! generation, uses prepared statements, batches inserts inside
//! explicit transactions (never one transaction per file), and releases
//! the byte budget after consuming each document.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use rusqlite::{params, Connection};

use crate::db::{self, building_path};
use crate::error::{
    BuildError, FatalErrorKind, STATUS_INDEXED, STATUS_SECURITY_LIMIT, STATUS_TOO_LARGE,
};
use crate::pipeline::{BuildShared, BuildTimings};
use crate::progress::BuildPhase;

/// A document destined for the `documents` table (plus FTS when it
/// carries content).
#[derive(Debug, Clone)]
pub(crate) struct IndexDocument {
    /// Physical file path (UTF-8; lossless by construction).
    pub file_path: String,
    /// Entry path inside an archive, using `!/` separators for nested
    /// archives.
    pub entry_path: Option<String>,
    /// Lowercase extension without dot.
    pub ext: Option<String>,
    /// Content size in bytes (decoded bytes for indexed documents).
    pub size: u64,
    /// Modification time, nanoseconds since the Unix epoch.
    pub mtime: Option<i64>,
    /// Status value (see the `STATUS_*` constants).
    pub status: i32,
    /// Human-readable reason for non-zero statuses.
    pub reason: Option<String>,
    /// UTF-8 text content; present only for status 0 documents.
    pub content: Option<String>,
    /// Bytes of the byte budget held by this document.
    pub budget_bytes: usize,
}

impl IndexDocument {
    /// Overwrites the size (used when a fresher size is known).
    pub fn with_size(mut self, size: u64) -> Self {
        self.size = size;
        self
    }
}

/// Outcome of the writer thread.
pub(crate) enum WriterExit {
    Success {
        /// Busy time spent inside SQLite operations (open, statement
        /// execution, commits) — channel waits excluded.
        writing: Duration,
        finalizing: Duration,
        swapping: Duration,
    },
    Fatal(BuildError),
    Cancelled,
}

/// Insert batching state: documents and text bytes accumulate inside one
/// transaction which is committed when either limit is reached.
struct BatchState {
    tx_open: bool,
    docs: usize,
    bytes: usize,
}

impl BatchState {
    fn new() -> Self {
        BatchState {
            tx_open: false,
            docs: 0,
            bytes: 0,
        }
    }

    fn ensure_tx(&mut self, conn: &Connection) -> Result<(), BuildError> {
        if !self.tx_open {
            conn.execute_batch("BEGIN;")
                .map_err(|e| db_err("begin transaction", e))?;
            self.tx_open = true;
        }
        Ok(())
    }

    fn commit(&mut self, conn: &Connection) -> Result<(), BuildError> {
        if self.tx_open {
            conn.execute_batch("COMMIT;")
                .map_err(|e| db_err("commit batch", e))?;
            self.tx_open = false;
        }
        self.docs = 0;
        self.bytes = 0;
        Ok(())
    }
}

fn db_err(what: &str, e: rusqlite::Error) -> BuildError {
    db::fatal(
        FatalErrorKind::DatabaseFailure,
        format!("{what} failed: {e}"),
    )
}

/// Runs the single SQLite writer thread.
pub(crate) fn run_writer(shared: Arc<BuildShared>, doc_rx: Receiver<IndexDocument>) -> WriterExit {
    let index_path = shared.index_path.clone();
    let build_path = building_path(&index_path);

    // Remove a stale build database left over by an earlier crashed run
    // (the build registry guarantees no other build owns this index
    // path; see `pipeline.rs` for the recovery contract).
    if build_path.exists() {
        if let Err(e) = std::fs::remove_file(&build_path) {
            return WriterExit::Fatal(db::fatal(
                FatalErrorKind::SqliteInit,
                format!("cannot remove stale build database {build_path:?}: {e}"),
            ));
        }
    }

    // Busy time inside SQLite operations; channel waits are excluded.
    let t_open = Instant::now();
    let mut conn = match db::open_build_db(&build_path, &shared.opts) {
        Ok(c) => c,
        Err(e) => return WriterExit::Fatal(e),
    };
    let mut sql_time = t_open.elapsed();
    BuildTimings::add(&shared.timings.db_open, sql_time);

    let mut batch = BatchState::new();
    enum LoopExit {
        Cancelled,
        Fatal(BuildError),
        Finalize,
    }
    let outcome = loop {
        if shared.is_cancelled() {
            break LoopExit::Cancelled;
        }
        let t_recv = Instant::now();
        let received = doc_rx.recv_timeout(Duration::from_millis(100));
        BuildTimings::add(&shared.timings.writer_recv_wait, t_recv.elapsed());
        match received {
            Ok(doc) => {
                let t = Instant::now();
                let ingested = ingest_document(&mut conn, &mut batch, &doc, &shared);
                // The byte budget is released as soon as the document is
                // consumed by the writer.
                shared.budget.release(doc.budget_bytes);
                match ingested {
                    Ok(()) => {}
                    Err(e) => break LoopExit::Fatal(e),
                }
                if batch.docs >= shared.opts.batch_max_docs
                    || batch.bytes >= shared.opts.batch_max_bytes
                {
                    let t_commit = Instant::now();
                    let committed = batch.commit(&conn);
                    BuildTimings::add(&shared.timings.batch_commit, t_commit.elapsed());
                    shared.timings.batch_commits.fetch_add(1, Ordering::Relaxed);
                    if let Err(e) = committed {
                        break LoopExit::Fatal(e);
                    }
                }
                sql_time += t.elapsed();
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                // All workers finished. A cancellation that raced with
                // the disconnect must still abort: finalizing here would
                // activate an incomplete index after the user cancelled.
                if shared.is_cancelled() {
                    break LoopExit::Cancelled;
                }
                break LoopExit::Finalize;
            }
        }
    };

    match outcome {
        LoopExit::Cancelled => {
            // The connection is dropped here (never rename/delete a
            // database file while a connection owns it).
            drop(conn);
            cleanup_building(&build_path);
            WriterExit::Cancelled
        }
        LoopExit::Fatal(e) => {
            drop(conn);
            cleanup_building(&build_path);
            WriterExit::Fatal(e)
        }
        LoopExit::Finalize => {
            // Database-side finalization, then close the connection,
            // then activate. The old active index is only replaced
            // after everything succeeded.
            let fin = finalize_database(&mut conn, &shared, &mut batch);
            drop(conn);
            match fin {
                // Cancellation checkpoint between finalization and
                // activation: a cancel observed here must not activate
                // the new snapshot.
                Ok(_) if shared.is_cancelled() => {
                    cleanup_building(&build_path);
                    WriterExit::Cancelled
                }
                Ok(finalizing) => match activate(&shared, &build_path, &index_path) {
                    Ok(ActivateOutcome::Activated(swapping)) => WriterExit::Success {
                        writing: sql_time,
                        finalizing,
                        swapping,
                    },
                    Ok(ActivateOutcome::Cancelled) => {
                        cleanup_building(&build_path);
                        WriterExit::Cancelled
                    }
                    Err(e) => {
                        cleanup_building(&build_path);
                        WriterExit::Fatal(e)
                    }
                },
                Err(e) => {
                    cleanup_building(&build_path);
                    WriterExit::Fatal(e)
                }
            }
        }
    }
}

/// Inserts one document inside the open batch transaction.
fn ingest_document(
    conn: &mut Connection,
    batch: &mut BatchState,
    doc: &IndexDocument,
    shared: &BuildShared,
) -> Result<(), BuildError> {
    let t_begin = Instant::now();
    batch.ensure_tx(conn)?;
    BuildTimings::add(&shared.timings.tx_begin, t_begin.elapsed());

    let t_doc = Instant::now();
    conn.execute(
        "INSERT INTO documents(file_path, entry_path, ext, size, mtime, status, reason)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            doc.file_path,
            doc.entry_path,
            doc.ext,
            doc.size as i64,
            doc.mtime,
            doc.status,
            doc.reason,
        ],
    )
    .map_err(|e| db_err("documents insert", e))?;
    let rowid = conn.last_insert_rowid();
    BuildTimings::add(&shared.timings.insert_documents, t_doc.elapsed());

    if let Some(content) = &doc.content {
        let t_fts = Instant::now();
        conn.execute(
            "INSERT INTO fts(rowid, content) VALUES (?1, ?2)",
            params![rowid, content],
        )
        .map_err(|e| db_err("fts insert", e))?;
        BuildTimings::add(&shared.timings.insert_fts, t_fts.elapsed());
    }
    batch.docs += 1;
    batch.bytes += doc.budget_bytes;

    match doc.status {
        STATUS_INDEXED => {
            shared.progress.inc_files_indexed(1);
            shared
                .progress
                .inc_bytes_indexed(doc.content.as_ref().map_or(0, |c| c.len()) as u64);
        }
        STATUS_TOO_LARGE => shared.progress.inc_files_too_large(1),
        STATUS_SECURITY_LIMIT => shared.progress.inc_files_security_limited(1),
        _ => {}
    }
    Ok(())
}

/// Finalizes the build database: final batch commit, FTS optimize,
/// metadata (schema version, SQLite version, timestamp, sources, build
/// options, counters) and the final `complete = 1` marker. The caller
/// closes the connection and runs [`activate`] afterwards.
fn finalize_database(
    conn: &mut Connection,
    shared: &Arc<BuildShared>,
    batch: &mut BatchState,
) -> Result<Duration, BuildError> {
    shared.progress.set_phase(BuildPhase::Finalizing);
    let t_final = Instant::now();

    // 1. Commit the final batch.
    batch.commit(conn)?;

    // 2. Merge FTS segments.
    let t_opt = Instant::now();
    conn.execute_batch("INSERT INTO fts(fts) VALUES('optimize')")
        .map_err(|e| db_err("fts optimize", e))?;
    BuildTimings::add(&shared.timings.fts_optimize, t_opt.elapsed());

    // 3-10. Metadata: schema version, SQLite version, timestamp,
    // sources, build options, counters and the final `complete = 1`
    // marker, all inside one final transaction.
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let sources = sources_json(&shared.opts);
    let options_debug = format!("{:?}", shared.opts);
    let counters = shared.progress.snapshot();
    let counters_json = format!(
        "{{\"files_seen\":{},\"files_indexed\":{},\"files_ignored\":{},\"files_too_large\":{},\"files_security_limited\":{},\"errors\":{},\"fallback_decodes\":{},\"archives\":{},\"archive_entries\":{},\"bytes_read\":{},\"bytes_indexed\":{}}}",
        counters.files_seen,
        counters.files_indexed,
        counters.files_ignored,
        counters.files_too_large,
        counters.files_security_limited,
        counters.errors,
        counters.fallback_decodes,
        counters.archives,
        counters.archive_entries,
        counters.bytes_read,
        counters.bytes_indexed
    );
    conn.execute_batch("BEGIN;")
        .map_err(|e| db_err("begin final transaction", e))?;
    write_meta(conn, "schema_version", &db::SCHEMA_VERSION.to_string())?;
    write_meta(conn, "sqlite_version", rusqlite::version())?;
    write_meta(conn, "build_timestamp", &timestamp.to_string())?;
    write_meta(conn, "source_directories", &sources)?;
    write_meta(conn, "build_options", &options_debug)?;
    write_meta(conn, "counters", &counters_json)?;
    // The `complete` marker is written last inside this transaction.
    write_meta(conn, "complete", "1")?;
    for source in &shared.opts.source_directories {
        conn.execute(
            "INSERT OR IGNORE INTO sources(path) VALUES (?1)",
            params![source.to_string_lossy()],
        )
        .map_err(|e| db_err("sources insert", e))?;
    }
    conn.execute_batch("COMMIT;")
        .map_err(|e| db_err("final commit", e))?;

    Ok(t_final.elapsed())
}

fn write_meta(conn: &Connection, key: &str, value: &str) -> Result<(), BuildError> {
    conn.execute(
        "INSERT OR REPLACE INTO meta(key, value) VALUES (?1, ?2)",
        params![key, value],
    )
    .map(|_| ())
    .map_err(|e| db_err("meta write", e))
}

/// Serializes the source directories as a JSON-like array. Individual
/// quotes are doubled, which is valid enough for diagnostics; the
/// canonical representation is documented in `docs/decisions.md`.
fn sources_json(opts: &crate::options::BuildOptions) -> String {
    let items: Vec<String> = opts
        .source_directories
        .iter()
        .map(|p| format!("\"{}\"", p.to_string_lossy().replace('\'', "''")))
        .collect();
    format!("[{}]", items.join(","))
}

/// Result of [`activate`]: either the new snapshot was swapped in, or a
/// late cancellation was observed before the rename and nothing was
/// activated.
enum ActivateOutcome {
    Activated(Duration),
    Cancelled,
}

/// Flushes the database file to disk, validates it, then atomically
/// replaces the active index. The old active index is never deleted
/// before the new one is complete; on any failure here the old index
/// survives untouched.
///
/// Cancellation is re-checked immediately before every rename attempt.
/// Residual window, honestly documented: if `cancel()` lands between
/// the last check and the `std::fs::rename` syscall itself, the rename
/// still completes — it is an atomic, non-interruptible syscall. In
/// that case the build reports success and the activated snapshot is a
/// fully valid index; the old index is only ever replaced by a
/// validated one, never corrupted.
fn activate(
    shared: &Arc<BuildShared>,
    build_path: &Path,
    index_path: &Path,
) -> Result<ActivateOutcome, BuildError> {
    shared.progress.set_phase(BuildPhase::Swapping);
    let t_swap = Instant::now();

    // Flush/sync the finished database file before activation.
    {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(build_path)
            .map_err(|e| db_activation_err(format!("cannot open {build_path:?} for sync: {e}")))?;
        file.sync_all()
            .map_err(|e| db_activation_err(format!("cannot sync {build_path:?}: {e}")))?;
    }

    // Validate before replacing the active index.
    db::validate_index(build_path)?;

    // The application-level write lock is the build registry (exactly
    // one build per index path per process); see `pipeline.rs`.

    // Atomic replacement with bounded retries: Windows can transiently
    // fail the rename while antivirus, Windows Search or another
    // process holds the old file open.
    const ATTEMPTS: u32 = 10;
    const RETRY_DELAY: Duration = Duration::from_millis(100);
    let mut last_err: Option<std::io::Error> = None;
    for _ in 0..ATTEMPTS {
        // Cancellation checkpoint: never start a rename attempt after
        // the build has been cancelled (see the function docs for the
        // residual syscall-level window).
        if shared.is_cancelled() {
            return Ok(ActivateOutcome::Cancelled);
        }
        match std::fs::rename(build_path, index_path) {
            Ok(()) => {
                last_err = None;
                break;
            }
            Err(e) => {
                last_err = Some(e);
                std::thread::sleep(RETRY_DELAY);
            }
        }
    }
    if let Some(e) = last_err {
        return Err(db_activation_err(format!(
            "cannot activate index {index_path:?}: {e}"
        )));
    }

    // Reopen and validate the active index.
    db::validate_index(index_path)?;

    Ok(ActivateOutcome::Activated(t_swap.elapsed()))
}

fn db_activation_err(message: String) -> BuildError {
    db::fatal(FatalErrorKind::ActivationFailure, message)
}

fn cleanup_building(build_path: &Path) {
    if let Err(e) = std::fs::remove_file(build_path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            // Best effort; a remaining stale file is removed by the
            // next build's stale cleanup.
        }
    }
}
