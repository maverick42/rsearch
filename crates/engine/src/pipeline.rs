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

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::bounded;

use crate::budget::ByteBudget;
use crate::error::{BuildError, FatalErrorKind, FileErrorCode, FileErrorRecord};
use crate::options::BuildOptions;
use crate::progress::{BuildPhase, Progress};
use crate::report::{BuildReport, PhaseDurations, MAX_DETAILED_ERRORS};
use crate::scanner::{self, ScanJob, ScannerConfig, SCAN_CHANNEL_CAPACITY};
use crate::worker::{run_worker, WRITER_CHANNEL_CAPACITY};
use crate::writer::{run_writer, WriterExit};

/// State shared by every thread of one build.
pub(crate) struct BuildShared {
    pub opts: Arc<BuildOptions>,
    pub progress: Progress,
    pub cancelled: Arc<AtomicBool>,
    /// Number of pipeline threads that panicked. Any panic winds the
    /// build down and turns the final result into a fatal internal
    /// error; panics must never deadlock the coordinator.
    pub panics: AtomicU64,
    pub errors: Arc<ErrorSink>,
    pub budget: Arc<ByteBudget>,
    pub index_path: PathBuf,
}

impl BuildShared {
    pub(crate) fn new(opts: Arc<BuildOptions>, index_path: PathBuf) -> Self {
        let progress = Progress::new();
        BuildShared {
            budget: Arc::new(ByteBudget::new(opts.max_inflight_bytes)),
            errors: Arc::new(ErrorSink::new(progress.clone())),
            opts,
            progress,
            cancelled: Arc::new(AtomicBool::new(false)),
            panics: AtomicU64::new(0),
            index_path,
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

/// Context handed to every worker thread.
pub(crate) struct WorkerCtx {
    pub shared: Arc<BuildShared>,
    pub doc_tx: crossbeam_channel::Sender<crate::writer::IndexDocument>,
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

/// Runs the whole build to completion on the coordinator thread.
/// Returns the final report on success; a cancellation or fatal failure
/// otherwise. Every pipeline thread is joined before returning.
pub(crate) fn run_build(shared: Arc<BuildShared>) -> Result<BuildReport, BuildError> {
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

    // Bounded channels: A (scanner -> workers) and B (workers -> writer).
    let (job_tx, job_rx) = bounded::<ScanJob>(SCAN_CHANNEL_CAPACITY);
    let (doc_tx, doc_rx) = bounded(WRITER_CHANNEL_CAPACITY);

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
                    run_writer(shared2, doc_rx)
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
        doc_tx: doc_tx.clone(),
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
    drop(doc_tx); // Worker contexts own the remaining document senders.
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
    let (finalizing, swapping, index_size) = match &writer_exit {
        WriterExit::Success {
            finalizing,
            swapping,
        } => {
            let size = std::fs::metadata(&shared.index_path).map(|m| m.len()).ok();
            (*finalizing, *swapping, size)
        }
        _ => (Duration::ZERO, Duration::ZERO, None),
    };
    let end = Instant::now();
    let durations = PhaseDurations {
        scanning: walker_done.map_or(Duration::ZERO, |t| t - start),
        processing: workers_done.map_or(Duration::ZERO, |t| t - start),
        writing: end - start,
        finalizing,
        swapping,
        total: end - start,
    };
    let report = BuildReport {
        counters,
        total_errors: total,
        errors: records,
        omitted_errors: omitted,
        durations,
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
