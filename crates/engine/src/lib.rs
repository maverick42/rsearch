//! # rsearch-engine
//!
//! The indexing engine of rsearch:
//! a parallel, bounded, GUI-independent engine that scans directory
//! trees, decodes text, processes ZIP-family archives and builds a
//! SQLite FTS5 trigram index.
//!
//! ## Core invariants
//!
//! * **Real files are the source of truth.** The index is only a
//!   candidate-selection mechanism; future searches re-open the real
//!   file for exact verification.
//! * **Snapshots only.** Rebuilds and incremental updates both produce
//!   `<index>.building` and atomically activate it. The old active
//!   index survives cancellation and failure.
//! * **Bounded everything.** Bounded channels, a byte budget on text
//!   waiting for insertion, archive security limits.
//! * **No silent data loss.** Decoding is strict; unsupported
//!   encodings, invalid content and unstable files produce structured
//!   recoverable errors.
//!
//! ## Example
//!
//! ```no_run
//! use std::path::PathBuf;
//! use rsearch_engine::{rebuild_index, BuildOptions, RootSpec};
//!
//! let opts = BuildOptions {
//!     source_directories: vec![RootSpec::new(r"C:\projects\my-app")],
//!     ..BuildOptions::default()
//! };
//! let handle = rebuild_index(PathBuf::from("index.db"), opts);
//! // A future GUI can poll handle.progress() and call handle.cancel().
//! let report = handle.wait().expect("build succeeded");
//! println!("{report}");
//! ```
//!
//! The engine never depends on GUI code; the future UI layer uses only
//! this crate's public API.

pub mod archive;
pub mod budget;
pub mod db;
pub mod decoder;
pub mod error;
pub mod fts;
pub mod longpath;
pub mod options;
pub mod pipeline;
pub mod progress;
pub mod report;
pub mod scanner;
pub mod search;
pub mod worker;
pub mod writer;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub use db::IndexInfo;
pub use error::{
    BuildError, FatalErrorKind, FileErrorCode, FileErrorRecord, IndexError, STATUS_ERROR,
    STATUS_INDEXED, STATUS_RESERVED, STATUS_SECURITY_LIMIT, STATUS_TOO_LARGE,
};
pub use options::{ArchiveOptions, BuildOptions, EncodingKind, JournalMode, RootSpec};
pub use progress::{BuildPhase, Progress, ProgressSnapshot};
pub use report::{BuildKind, BuildReport, BuildSummary, PhaseDurations, SkippedRoot, UpdateDelta};
pub use search::{
    iter_documents, search, search_events, DocumentRef, FileResult, Occurrence, SearchError,
    SearchEvent, SearchOptions, SearchReport,
};

/// Handle to a running (or finished) index build.
///
/// The handle is the only object the future GUI needs: progress
/// polling, cancellation and waiting for the final result.
pub struct BuildHandle {
    shared: Arc<pipeline::BuildShared>,
    coordinator: Mutex<Option<std::thread::JoinHandle<()>>>,
    result: Arc<Mutex<Option<Result<BuildReport, BuildError>>>>,
}

impl BuildHandle {
    /// Returns a reference to the live progress tracker. Use
    /// [`Progress::snapshot`] to obtain a GUI-friendly copy of the
    /// counters and phase.
    pub fn progress(&self) -> &Progress {
        &self.shared.progress
    }

    /// Requests cancellation. The build shuts down promptly: the scanner
    /// stops producing jobs, workers stop taking new work, the writer
    /// aborts and removes the `.building` database. The previously
    /// active index remains untouched.
    pub fn cancel(&self) {
        self.shared.request_cancel();
    }

    /// Waits for the build to finish and returns its result.
    ///
    /// All pipeline threads are joined before this returns. On success
    /// the new index has been atomically activated and validated; on
    /// cancellation or fatal failure the previous active index (when
    /// one existed) is intact.
    pub fn wait(self) -> Result<BuildReport, BuildError> {
        let handle = self
            .coordinator
            .lock()
            .unwrap()
            .take()
            .expect("wait() can only be called once");
        // Join the coordinator; a panic inside the build is reported as
        // a fatal infrastructure failure rather than unwinding here.
        if handle.join().is_err() {
            return Err(BuildError::Fatal {
                kind: FatalErrorKind::InternalError,
                message: "build coordinator thread panicked".into(),
                report: None,
            });
        }
        self.result
            .lock()
            .unwrap()
            .take()
            .expect("coordinator must leave a result")
    }
}

/// Starts a complete index rebuild of one or more source directories.
///
/// The build runs asynchronously; the returned [`BuildHandle`] provides
/// progress, cancellation and the final result. The new index is built
/// at `<index_path>.building` and atomically replaces `index_path` only
/// after it has been fully built and validated. A failed or cancelled
/// build never damages the previously active index.
///
/// Invalid options (for example no source directories) are reported
/// through [`BuildHandle::wait`] as a fatal error.
///
/// Only one build per index path may run at a time per process;
/// concurrent rebuilds of *different* Search Entries (different index
/// paths) are supported.
pub fn rebuild_index(index_path: impl AsRef<Path>, mut opts: BuildOptions) -> BuildHandle {
    let index_path: PathBuf = index_path.as_ref().to_path_buf();
    let (roots, skipped_roots) = normalize_roots(&opts.source_directories);
    opts.source_directories = roots;
    let shared = Arc::new(pipeline::BuildShared::new(
        Arc::new(opts),
        index_path,
        skipped_roots,
    ));
    let result = Arc::new(Mutex::new(None));
    let coordinator_result = Arc::clone(&result);
    let coordinator_shared = Arc::clone(&shared);

    let coordinator = std::thread::Builder::new()
        .name("rsearch-coordinator".into())
        .spawn(move || {
            let outcome = pipeline::run_build(coordinator_shared);
            *coordinator_result.lock().unwrap() = Some(outcome);
        })
        .expect("coordinator thread must spawn");

    BuildHandle {
        shared,
        coordinator: Mutex::new(Some(coordinator)),
        result,
    }
}

/// Starts an incremental update of an existing index.
///
/// Same contract as [`rebuild_index`] — asynchronous [`BuildHandle`],
/// same cancellation and atomic-activation guarantees — but the new
/// snapshot starts as a copy of the active index. Files whose stored
/// `size`/`mtime` still match keep their documents and FTS rows and
/// are never re-read; modified, new and deleted files are reprocessed
/// or removed.
///
/// The update falls back to a full rebuild when the active index is
/// missing, invalid, of an older schema version, or was built with
/// different options (option changes can invalidate rows that the
/// metadata diff cannot reason about). `size + mtime` is not a
/// cryptographic identity; see `docs/update.md`.
pub fn update_index(index_path: impl AsRef<Path>, mut opts: BuildOptions) -> BuildHandle {
    let index_path: PathBuf = index_path.as_ref().to_path_buf();
    let (roots, skipped_roots) = normalize_roots(&opts.source_directories);
    opts.source_directories = roots;
    let shared = Arc::new(pipeline::BuildShared::new(
        Arc::new(opts),
        index_path,
        skipped_roots,
    ));
    let result = Arc::new(Mutex::new(None));
    let coordinator_result = Arc::clone(&result);
    let coordinator_shared = Arc::clone(&shared);

    let coordinator = std::thread::Builder::new()
        .name("rsearch-coordinator".into())
        .spawn(move || {
            let outcome = pipeline::run_update(coordinator_shared);
            *coordinator_result.lock().unwrap() = Some(outcome);
        })
        .expect("coordinator thread must spawn");

    BuildHandle {
        shared,
        coordinator: Mutex::new(Some(coordinator)),
        result,
    }
}

/// Normalizes source roots before the scan.
///
/// Comparison uses the normalized component key from
/// [`options::root_compare_key`] (lexically absolute, case-insensitive,
/// `\\?\`/`\\.\` prefixes folded); the kept roots retain their
/// original spelling.
///
/// Exact duplicates with the same `recursive` flag and roots contained
/// in a *recursive* root are removed; each removal is reported with
/// its reason so the caller can surface it in the build report. A
/// `recursive: false` root never covers a nested root because it does
/// not descend into subdirectories. The same directory listed with
/// both recursion policies is kept verbatim: [`BuildOptions::validate`]
/// rejects that ambiguity instead of silently choosing one.
fn normalize_roots(roots: &[RootSpec]) -> (Vec<RootSpec>, Vec<SkippedRoot>) {
    use crate::options::root_compare_key;

    fn contains(container: &[String], nested: &[String]) -> bool {
        nested.len() > container.len() && nested[..container.len()] == container[..]
    }

    let mut kept: Vec<(RootSpec, Vec<String>)> = Vec::new();
    let mut skipped: Vec<SkippedRoot> = Vec::new();
    for root in roots {
        let k = root_compare_key(&root.path);
        // An exact duplicate carries the same normalized key AND the
        // same recursion policy. A key-equal root with a different
        // `recursive` flag is *not* merged here: it stays in the list
        // so that `BuildOptions::validate` rejects the conflict.
        if let Some((first, _)) = kept
            .iter()
            .find(|(r, kk)| *kk == k && r.recursive == root.recursive)
        {
            skipped.push(SkippedRoot {
                path: root.path.clone(),
                reason: format!("duplicate of source root {}", first.path.display()),
            });
            continue;
        }
        // Only a recursive container covers a nested root.
        if let Some((container, _)) = kept.iter().find(|(r, kk)| r.recursive && contains(kk, &k)) {
            skipped.push(SkippedRoot {
                path: root.path.clone(),
                reason: format!("contained in source root {}", container.path.display()),
            });
            continue;
        }
        // The new root may itself contain earlier roots — only when it
        // is recursive and therefore actually covers them.
        if root.recursive {
            let mut i = 0;
            while i < kept.len() {
                if contains(&k, &kept[i].1) {
                    let (old, _) = kept.remove(i);
                    skipped.push(SkippedRoot {
                        path: old.path.clone(),
                        reason: format!("contained in source root {}", root.path.display()),
                    });
                } else {
                    i += 1;
                }
            }
        }
        kept.push((root.clone(), k));
    }
    (kept.into_iter().map(|(r, _)| r).collect(), skipped)
}

/// Verifies that `index_path` is a complete, usable rsearch index and
/// returns a UI-facing summary ([`IndexInfo`]).
///
/// The index is opened **read-only**: this never creates nor modifies
/// the file, never touches `.building`, and is safe to call while a
/// build is running on the same index path (the previously active
/// index stays readable throughout the build).
///
/// Checks performed (same implementation as the build-time validation):
/// SQLite opens, `meta.complete = '1'`, known `schema_version`, all
/// expected tables present, and a real FTS5 trigram query executes.
pub fn verify_index(index_path: &Path) -> Result<IndexInfo, IndexError> {
    let md = std::fs::symlink_metadata(index_path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => IndexError::NotFound,
        _ => IndexError::Io(e),
    })?;
    if !md.is_file() || md.len() == 0 {
        return Err(IndexError::NotAnIndex(
            "file is empty or not a regular file".into(),
        ));
    }
    let conn = db::open_readonly(index_path)?;
    db::validate_connection(&conn)?;
    Ok(db::index_info(&conn, md.len()))
}
