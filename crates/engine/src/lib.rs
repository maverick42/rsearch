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
//! * **Snapshot rebuilds only.** A rebuild builds `<index>.building`
//!   and atomically activates it. The old active index survives
//!   cancellation and failure.
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
//! use rsearch_engine::{rebuild_index, BuildOptions};
//!
//! let opts = BuildOptions {
//!     source_directories: vec![PathBuf::from(r"C:\projects\my-app")],
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
pub mod worker;
pub mod writer;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub use db::IndexInfo;
pub use error::{
    BuildError, FatalErrorKind, FileErrorCode, FileErrorRecord, IndexError, STATUS_ERROR,
    STATUS_INDEXED, STATUS_RESERVED, STATUS_SECURITY_LIMIT, STATUS_TOO_LARGE,
};
pub use options::{ArchiveOptions, BuildOptions, EncodingKind, JournalMode};
pub use progress::{BuildPhase, Progress, ProgressSnapshot};
pub use report::{BuildReport, PhaseDurations, SkippedRoot};

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

/// Normalizes source roots before the scan.
///
/// Every root is made absolute (lexically via [`std::path::absolute`],
/// no filesystem access) only for comparison; the kept roots retain
/// their original spelling. Comparison is by path *components*,
/// case-insensitive (Windows paths are case-insensitive) and with the
/// `\\?\`/`\\.\` prefixes folded onto their plain forms — so
/// `C:\a\b` is never confused with `C:\a\bc`.
///
/// Exact duplicates and roots contained in another root are removed;
/// each removal is reported with its reason so the caller can surface
/// it in the build report.
fn normalize_roots(roots: &[PathBuf]) -> (Vec<PathBuf>, Vec<SkippedRoot>) {
    use std::path::{Component, Prefix};

    fn lower(s: &std::ffi::OsStr) -> String {
        // Lossy is acceptable here: the key is a comparison artifact,
        // never used to reopen a path.
        s.to_string_lossy().to_lowercase()
    }

    /// Case-insensitive, prefix-normalized component key.
    fn key(path: &Path) -> Vec<String> {
        let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        abs.components()
            .map(|c| match c {
                Component::Prefix(p) => match p.kind() {
                    Prefix::Disk(d) | Prefix::VerbatimDisk(d) => {
                        (d as char).to_lowercase().to_string() + ":"
                    }
                    Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
                        format!("unc:{}\\{}", lower(server), lower(share))
                    }
                    Prefix::DeviceNS(d) | Prefix::Verbatim(d) => format!("dev:{}", lower(d)),
                },
                other => lower(other.as_os_str()),
            })
            .collect()
    }

    fn contains(container: &[String], nested: &[String]) -> bool {
        nested.len() > container.len() && nested[..container.len()] == container[..]
    }

    let mut kept: Vec<(PathBuf, Vec<String>)> = Vec::new();
    let mut skipped: Vec<SkippedRoot> = Vec::new();
    for root in roots {
        let k = key(root);
        if let Some((first, _)) = kept.iter().find(|(_, kk)| *kk == k) {
            skipped.push(SkippedRoot {
                path: root.clone(),
                reason: format!("duplicate of source root {}", first.display()),
            });
            continue;
        }
        if let Some((container, _)) = kept.iter().find(|(_, kk)| contains(kk, &k)) {
            skipped.push(SkippedRoot {
                path: root.clone(),
                reason: format!("contained in source root {}", container.display()),
            });
            continue;
        }
        // The new root may itself contain earlier roots: remove them.
        let mut i = 0;
        while i < kept.len() {
            if contains(&k, &kept[i].1) {
                let (old, _) = kept.remove(i);
                skipped.push(SkippedRoot {
                    path: old.clone(),
                    reason: format!("contained in source root {}", root.display()),
                });
            } else {
                i += 1;
            }
        }
        kept.push((root.clone(), k));
    }
    (kept.into_iter().map(|(p, _)| p).collect(), skipped)
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
