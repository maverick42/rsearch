//! Build orchestration: the coordinator thread, shared state, the error
//! sink and the active-build registry.
//!
//! Pipeline layout:
//!
//! ```text
//! parallel scanner (N threads)
//!        |
//!   bounded channel A
//!        |
//! M worker threads (read / sniff / decode / archives)
//!        |
//!   bounded channel B + byte budget
//!        |
//! one SQLite writer
//! ```
//!
//! All threads are joined before the build result is delivered. The
//! previously active index is only replaced after the new snapshot has
//! been completely built and validated.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::bounded;

use crate::budget::ByteBudget;
use crate::error::{BuildError, FatalErrorKind, FileErrorCode, FileErrorRecord};
use crate::options::BuildOptions;
use crate::progress::{BuildPhase, Progress};
use crate::report::{
    BuildReport, PhaseDurations, PipelineTimings, SkippedRoot, MAX_DETAILED_ERRORS,
};
use crate::scanner::{self, PrevMap, ScanJob, ScannerConfig, SCAN_CHANNEL_CAPACITY};
use crate::worker::{run_worker, WRITER_CHANNEL_CAPACITY};
use crate::writer::{run_writer, WriterExit, WriterOp};

/// Per-operation nanosecond counters aggregated across all pipeline
/// threads of a build. Lock-free instrumentation: each field mirrors a
/// [`PipelineTimings`] field.
#[derive(Debug, Default)]
pub(crate) struct BuildTimings {
    pub scan_send_blocked: AtomicU64,
    pub worker_recv_wait: AtomicU64,
    pub worker_io: AtomicU64,
    pub worker_decode: AtomicU64,
    pub worker_archive: AtomicU64,
    pub worker_send_wait: AtomicU64,
    pub worker_budget_wait: AtomicU64,
    pub writer_recv_wait: AtomicU64,
    pub db_open: AtomicU64,
    pub tx_begin: AtomicU64,
    pub insert_documents: AtomicU64,
    pub insert_fts: AtomicU64,
    pub batch_commit: AtomicU64,
    pub batch_commits: AtomicU64,
    pub fts_optimize: AtomicU64,
}

impl BuildTimings {
    /// Adds `elapsed` to a counter. A few tens of nanoseconds per call;
    /// negligible next to file I/O and SQLite work.
    pub(crate) fn add(field: &AtomicU64, elapsed: Duration) {
        field.fetch_add(elapsed.as_nanos() as u64, Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self) -> PipelineTimings {
        let d = |f: &AtomicU64| Duration::from_nanos(f.load(Ordering::Relaxed));
        PipelineTimings {
            scan_send_blocked: d(&self.scan_send_blocked),
            worker_recv_wait: d(&self.worker_recv_wait),
            worker_io: d(&self.worker_io),
            worker_decode: d(&self.worker_decode),
            worker_archive: d(&self.worker_archive),
            worker_send_wait: d(&self.worker_send_wait),
            worker_budget_wait: d(&self.worker_budget_wait),
            writer_recv_wait: d(&self.writer_recv_wait),
            db_open: d(&self.db_open),
            tx_begin: d(&self.tx_begin),
            insert_documents: d(&self.insert_documents),
            insert_fts: d(&self.insert_fts),
            batch_commit: d(&self.batch_commit),
            batch_commits: self.batch_commits.load(Ordering::Relaxed),
            fts_optimize: d(&self.fts_optimize),
        }
    }
}

/// State shared by every thread of one build.
pub(crate) struct BuildShared {
    pub opts: Arc<BuildOptions>,
    pub progress: Progress,
    /// Aggregated pipeline timing counters.
    pub timings: Arc<BuildTimings>,
    pub cancelled: Arc<AtomicBool>,
    /// Number of pipeline threads that panicked. Any panic winds the
    /// build down and turns the final result into a fatal internal
    /// error; panics must never deadlock the coordinator.
    pub panics: AtomicU64,
    pub errors: Arc<ErrorSink>,
    pub budget: Arc<ByteBudget>,
    pub index_path: PathBuf,
    /// Source roots excluded before the scan (duplicates or contained
    /// in another root), computed by `rebuild_index`.
    pub skipped_roots: Vec<SkippedRoot>,
    /// Number of times each configured directory name was pruned by the
    /// walker.
    pub excluded_directories: Arc<Mutex<BTreeMap<String, u64>>>,
}

impl BuildShared {
    pub(crate) fn new(
        opts: Arc<BuildOptions>,
        index_path: PathBuf,
        skipped_roots: Vec<SkippedRoot>,
    ) -> Self {
        let progress = Progress::new();
        BuildShared {
            timings: Arc::new(BuildTimings::default()),
            budget: Arc::new(ByteBudget::new(opts.max_inflight_bytes)),
            errors: Arc::new(ErrorSink::new(progress.clone())),
            opts,
            progress,
            cancelled: Arc::new(AtomicBool::new(false)),
            panics: AtomicU64::new(0),
            index_path,
            skipped_roots,
            excluded_directories: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Whether the build has been cancelled (by the user or by a fatal
    /// writer failure winding the pipeline down).
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Requests cancellation: stops scanner production and wakes channel
    /// and byte-budget waiters.
    pub(crate) fn request_cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.budget.cancel();
    }
}

/// How the writer produces the new snapshot at `<index>.building`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PipelineMode {
    /// `.building` is created empty and every scanned file is indexed.
    Rebuild,
    /// `.building` starts as a byte copy of the active index; the
    /// scanner diffs metadata and only changed or new files are
    /// reprocessed.
    Update,
}

/// Context handed to every worker thread.
pub(crate) struct WorkerCtx {
    pub shared: Arc<BuildShared>,
    pub doc_tx: crossbeam_channel::Sender<WriterOp>,
}

/// Collects recoverable error records for the build report. The total
/// count is exact; only the detailed list is capped at
/// [`MAX_DETAILED_ERRORS`] entries.
pub(crate) struct ErrorSink {
    records: Mutex<Vec<FileErrorRecord>>,
    total: AtomicU64,
    progress: Progress,
}

impl ErrorSink {
    pub(crate) fn new(progress: Progress) -> Self {
        ErrorSink {
            records: Mutex::new(Vec::new()),
            total: AtomicU64::new(0),
            progress,
        }
    }

    pub(crate) fn push(
        &self,
        code: FileErrorCode,
        file_path: String,
        entry_path: Option<String>,
        message: String,
    ) {
        self.total.fetch_add(1, Ordering::Relaxed);
        self.progress.inc_errors(1);
        let mut records = self.records.lock().unwrap();
        if records.len() < MAX_DETAILED_ERRORS {
            records.push(FileErrorRecord {
                code,
                file_path,
                entry_path,
                message,
            });
        }
    }

    /// Records a walker error (unreadable directory, and so on).
    pub(crate) fn push_scan_error(&self, err: &ignore::Error) {
        let path = match err {
            ignore::Error::WithPath { ref path, .. } => path.to_string_lossy().into_owned(),
            _ => "(walker)".to_string(),
        };
        self.push(FileErrorCode::Scan, path, None, err.to_string());
    }

    pub(crate) fn finish(&self) -> (Vec<FileErrorRecord>, u64, u64) {
        let records = self.records.lock().unwrap().clone();
        let total = self.total.load(Ordering::Relaxed);
        let omitted = total.saturating_sub(records.len() as u64);
        (records, total, omitted)
    }
}

/// Registry of index paths with a currently running build. This is the
/// application-level write lock for Step 1: exactly one build per index
/// path per process. It also makes stale `.building` cleanup safe: a
/// `.building` file whose index path is registered belongs to a live
/// build and must not be removed.
static ACTIVE_BUILDS: LazyLock<Mutex<HashSet<PathBuf>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Guard removing an index path from the registry when dropped.
struct BuildSlot {
    path: PathBuf,
}

impl Drop for BuildSlot {
    fn drop(&mut self) {
        ACTIVE_BUILDS.lock().unwrap().remove(&self.path);
    }
}

/// Acquires the build slot for an index path, or fails when a build is
/// already running for it.
fn acquire_build_slot(index_path: &Path) -> Result<BuildSlot, BuildError> {
    let key = normalize_index_path(index_path);
    let mut registry = ACTIVE_BUILDS.lock().unwrap();
    if registry.contains(&key) {
        return Err(BuildError::Fatal {
            kind: FatalErrorKind::InvalidOptions,
            message: format!("a build is already running for index {}", key.display()),
            report: None,
        });
    }
    registry.insert(key.clone());
    Ok(BuildSlot { path: key })
}

/// Best-effort canonical key for the registry: canonicalizes the parent
/// directory and appends the file name so that different spellings of
/// the same index map to the same slot.
fn normalize_index_path(index_path: &Path) -> PathBuf {
    let file_name = index_path.file_name().map(|n| n.to_os_string());
    match (index_path.parent(), file_name) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => {
            let parent = std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
            parent.join(name)
        }
        _ => index_path.to_path_buf(),
    }
}

/// Completion messages the coordinator waits for.
enum Done {
    Walker,
    Worker,
    Writer(WriterExit),
}

/// Runs a full rebuild on the coordinator thread: `.building` starts
/// empty. See [`run_pipeline`].
pub(crate) fn run_build(shared: Arc<BuildShared>) -> Result<BuildReport, BuildError> {
    run_pipeline(shared, PipelineMode::Rebuild)
}

/// Runs an incremental update on the coordinator thread. When the
/// active index cannot serve as an update base (missing, invalid,
/// older schema, or built with different options) this silently falls
/// back to a full rebuild.
pub(crate) fn run_update(shared: Arc<BuildShared>) -> Result<BuildReport, BuildError> {
    run_pipeline(shared, PipelineMode::Update)
}

/// Loads the previous index's `documents` rows grouped by `file_path`
/// when the active index is a usable update base. `None` — meaning the
/// caller falls back to a full rebuild — when the index is missing,
/// invalid, of an older schema version, or was built with different
/// options: policy-derived rows (exclusions, decode fallbacks, archive
/// limits) cannot be reasoned about from metadata alone.
fn prepare_update_base(index_path: &Path, opts: &BuildOptions) -> Option<PrevMap> {
    let conn = crate::db::open_readonly(index_path).ok()?;
    crate::db::validate_connection(&conn).ok()?;
    let stored_opts: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'build_options'",
            [],
            |r| r.get(0),
        )
        .ok();
    if stored_opts.as_deref() != Some(format!("{opts:?}").as_str()) {
        return None;
    }
    let mut stmt = conn
        .prepare("SELECT id, file_path, entry_path, size, mtime FROM documents")
        .ok()?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(1)?,
                crate::scanner::PrevDoc {
                    id: r.get(0)?,
                    is_entry: r.get::<_, Option<String>>(2)?.is_some(),
                    size: r.get(3)?,
                    mtime: r.get(4)?,
                },
            ))
        })
        .ok()?;
    let mut map: PrevMap = Default::default();
    for row in rows {
        match row {
            Ok((path, doc)) => map.entry(path).or_default().push(doc),
            Err(_) => return None,
        }
    }
    Some(map)
}

/// Runs the whole build to completion on the coordinator thread.
/// Returns the final report on success; a cancellation or fatal failure
/// otherwise. Every pipeline thread is joined before returning.
fn run_pipeline(
    shared: Arc<BuildShared>,
    requested_mode: PipelineMode,
) -> Result<BuildReport, BuildError> {
    let start = Instant::now();
    let opts = Arc::clone(&shared.opts);

    // Validate options before touching anything.
    if let Err(message) = opts.validate() {
        shared.progress.set_phase(BuildPhase::Failed);
        return Err(BuildError::Fatal {
            kind: FatalErrorKind::InvalidOptions,
            message,
            report: None,
        });
    }

    // Application-level write lock for this index path.
    let _slot = acquire_build_slot(&shared.index_path)?;

    // Update mode: with the slot held, the active index cannot be
    // swapped by another in-process build — the metadata map read here
    // describes exactly the file the writer will copy.
    let (mode, prev_documents) = match requested_mode {
        PipelineMode::Update => match prepare_update_base(&shared.index_path, &opts) {
            Some(map) => (PipelineMode::Update, Some(map)),
            None => (PipelineMode::Rebuild, None),
        },
        PipelineMode::Rebuild => (PipelineMode::Rebuild, None),
    };

    // Bounded channels: A (scanner -> workers) and B (workers -> writer).
    let (job_tx, job_rx) = bounded::<ScanJob>(SCAN_CHANNEL_CAPACITY);
    let (op_tx, op_rx) = bounded::<WriterOp>(WRITER_CHANNEL_CAPACITY);

    // Completion messages.
    let (done_tx, done_rx) = std::sync::mpsc::channel::<Done>();

    // Single SQLite writer. A panic inside the writer is converted into
    // a fatal exit so the coordinator always receives a `Done` message;
    // a panic must never deadlock the build.
    let writer_handle = {
        let shared = Arc::clone(&shared);
        let done_tx = done_tx.clone();
        std::thread::Builder::new()
            .name("rsearch-writer".into())
            .spawn(move || {
                let shared2 = Arc::clone(&shared);
                let exit = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_writer(shared2, op_rx, mode)
                }))
                .unwrap_or_else(|_| {
                    shared.panics.fetch_add(1, Ordering::AcqRel);
                    WriterExit::Fatal(BuildError::Fatal {
                        kind: FatalErrorKind::InternalError,
                        message: "writer thread panicked".to_string(),
                        report: None,
                    })
                });
                let _ = done_tx.send(Done::Writer(exit));
            })
            .expect("writer thread must spawn")
    };

    // Worker threads. A panicking worker is counted as finished and
    // winds the build down (remaining jobs are abandoned; the old index
    // is preserved).
    let worker_ctx = Arc::new(WorkerCtx {
        shared: Arc::clone(&shared),
        doc_tx: op_tx.clone(),
    });
    let mut worker_handles = Vec::with_capacity(opts.worker_threads);
    for i in 0..opts.worker_threads {
        let ctx = Arc::clone(&worker_ctx);
        let rx = job_rx.clone();
        let done_tx = done_tx.clone();
        let handle = std::thread::Builder::new()
            .name(format!("rsearch-worker-{i}"))
            .spawn(move || {
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_worker(Arc::clone(&ctx), rx)
                }))
                .is_err()
                {
                    ctx.shared.panics.fetch_add(1, Ordering::AcqRel);
                    ctx.shared.request_cancel();
                }
                let _ = done_tx.send(Done::Worker);
            })
            .expect("worker thread must spawn");
        worker_handles.push(handle);
    }
    drop(op_tx); // Worker contexts own the remaining document senders.
                 // Drop the coordinator's worker context *before* waiting: it holds a
                 // `doc_tx` clone, and the writer only finalizes once every sender
                 // is dropped (workers drop theirs when they exit). Holding it here
                 // would deadlock the pipeline.
    drop(worker_ctx);

    // Walker driver thread (the `ignore` walker joins its own internal
    // threads inside `scan`).
    let walker_handle = {
        let cfg = ScannerConfig {
            opts: Arc::clone(&opts),
            progress: shared.progress.clone(),
            cancelled: Arc::clone(&shared.cancelled),
            errors: Arc::clone(&shared.errors),
            timings: Arc::clone(&shared.timings),
            excluded_directory_counts: Arc::clone(&shared.excluded_directories),
            prev_documents: prev_documents.map(|m| Arc::new(Mutex::new(m))),
        };
        let job_tx = job_tx.clone();
        let done_tx = done_tx.clone();
        let shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("rsearch-walker".into())
            .spawn(move || {
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    scanner::scan(&cfg, &job_tx)
                }))
                .is_err()
                {
                    shared.panics.fetch_add(1, Ordering::AcqRel);
                    shared.request_cancel();
                }
                let _ = done_tx.send(Done::Walker);
            })
            .expect("walker thread must spawn")
    };
    drop(job_tx);
    drop(done_tx);

    shared.progress.set_phase(BuildPhase::Scanning);

    // Wait for completion of all stages. A fatal writer failure winds
    // the whole pipeline down promptly; the old index stays untouched.
    let mut walker_done: Option<Instant> = None;
    let mut workers_done: Option<Instant> = None;
    let mut workers_reported = 0usize;
    let mut writer_exit: Option<WriterExit> = None;
    let mut writer_panicked = false;
    while !(walker_done.is_some() && workers_done.is_some() && writer_exit.is_some()) {
        match done_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Done::Walker) => {
                walker_done = Some(Instant::now());
                if !shared.is_cancelled() {
                    shared.progress.set_phase(BuildPhase::Processing);
                }
            }
            Ok(Done::Worker) => {
                // Each worker sends exactly one message when it
                // finishes; counting messages is precise (unlike
                // polling JoinHandle::is_finished, which can lag behind
                // the message).
                workers_reported += 1;
                if workers_reported == opts.worker_threads {
                    workers_done = Some(Instant::now());
                    if !shared.is_cancelled() {
                        shared.progress.set_phase(BuildPhase::Writing);
                    }
                }
            }
            Ok(Done::Writer(exit)) => {
                if matches!(exit, WriterExit::Fatal(_)) {
                    shared.request_cancel();
                }
                writer_exit = Some(exit);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    // Join every thread before returning: `wait()` must only return
    // after the build has completely shut down.
    let _ = walker_handle.join();
    for handle in worker_handles {
        let _ = handle.join();
    }
    let writer_join = writer_handle.join();
    if writer_join.is_err() {
        writer_panicked = true;
    }

    let writer_exit = match writer_exit {
        Some(e) => e,
        None => {
            shared.progress.set_phase(BuildPhase::Failed);
            let message = if writer_panicked {
                "writer thread panicked".to_string()
            } else {
                "writer thread did not report an exit".to_string()
            };
            return Err(BuildError::Fatal {
                kind: FatalErrorKind::WriterInitialization,
                message,
                report: None,
            });
        }
    };

    let (records, total, omitted) = shared.errors.finish();
    let counters = shared.progress.snapshot();
    let (writing, finalizing, swapping, index_size) = match &writer_exit {
        WriterExit::Success {
            writing,
            finalizing,
            swapping,
        } => {
            let size = std::fs::metadata(&shared.index_path).map(|m| m.len()).ok();
            (*writing, *finalizing, *swapping, size)
        }
        _ => (Duration::ZERO, Duration::ZERO, Duration::ZERO, None),
    };
    let end = Instant::now();
    let durations = PhaseDurations {
        scanning: walker_done.map_or(Duration::ZERO, |t| t - start),
        processing: workers_done.map_or(Duration::ZERO, |t| t - start),
        writing,
        finalizing,
        swapping,
        total: end - start,
    };
    let report = BuildReport {
        counters,
        total_errors: total,
        errors: records,
        omitted_errors: omitted,
        skipped_roots: shared.skipped_roots.clone(),
        excluded_directories: shared.excluded_directories.lock().unwrap().clone(),
        durations,
        timings: shared.timings.snapshot(),
        index_size,
        sqlite_version: rusqlite::version().to_string(),
        cancelled: false,
    };

    let panicked = shared.panics.load(Ordering::Acquire);
    match writer_exit {
        WriterExit::Fatal(e) => {
            shared.progress.set_phase(BuildPhase::Failed);
            Err(match e {
                BuildError::Fatal { kind, message, .. } => BuildError::Fatal {
                    kind,
                    message,
                    report: Some(Box::new(report)),
                },
                other => other,
            })
        }
        // A panicking pipeline thread unwinds the whole build through
        // cancellation; report it as a fatal internal error, never as a
        // user cancellation or a success.
        _ if panicked > 0 => {
            shared.progress.set_phase(BuildPhase::Failed);
            Err(BuildError::Fatal {
                kind: FatalErrorKind::InternalError,
                message: format!("{panicked} pipeline thread(s) panicked during the build"),
                report: Some(Box::new(report)),
            })
        }
        WriterExit::Success { .. } => {
            shared.progress.set_phase(BuildPhase::Completed);
            Ok(report)
        }
        WriterExit::Cancelled => {
            shared.progress.set_phase(BuildPhase::Cancelled);
            let mut report = report;
            report.cancelled = true;
            Err(BuildError::Cancelled {
                report: Box::new(report),
            })
        }
    }
}
