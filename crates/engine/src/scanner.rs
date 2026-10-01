//! Parallel directory scanner.
//!
//! The scanner walks all source directories with the `ignore` crate
//! (never `walkdir`), classifies files by extension as a pure
//! optimization, and sends jobs through a bounded channel. The worker
//! threads perform authoritative content sniffing; extension
//! classification is never trusted as proof of content type.
//!
//! Paths stay as `PathBuf` end-to-end. `to_string_lossy` is never used
//! for a path that must later be reopened.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{Sender, TrySendError};
use ignore::{DirEntry, WalkBuilder};

use crate::error::FileErrorCode;
use crate::options::BuildOptions;
use crate::pipeline::{BuildTimings, ErrorSink};
use crate::progress::Progress;

/// Bounded capacity of the scanner -> worker channel.
///
/// Initial tuning parameter; large enough to keep workers busy, small
/// enough to bound memory when workers stall.
pub const SCAN_CHANNEL_CAPACITY: usize = 4096;

/// A file job produced by the scanner.
#[derive(Debug)]
pub(crate) struct FileJob {
    /// Lossless filesystem path.
    pub path: PathBuf,
    /// Lowercase extension without dot, when present.
    pub ext: Option<String>,
    /// File size at scan time.
    pub size: u64,
    /// Modification time at scan time, nanoseconds since the Unix epoch.
    pub mtime: Option<i64>,
}

/// Kind of job, from extension classification (optimization only).
#[derive(Debug)]
pub(crate) enum ScanJob {
    /// Possibly-text file; the worker sniffs the real content.
    File(FileJob),
    /// File with an archive extension; the worker verifies and processes it.
    Archive(FileJob),
    /// Incremental update: previous `documents` ids whose row and FTS
    /// row must be deleted (modified or disappeared files). Workers
    /// forward it to the writer untouched.
    DeleteIds(Vec<i64>),
}

/// One previously-indexed `documents` row, reduced to what the
/// metadata diff of an incremental update needs.
#[derive(Debug)]
pub(crate) struct PrevDoc {
    /// `documents.id` (which is also the FTS rowid when the row has
    /// indexed content).
    pub id: i64,
    /// True for archive entries (`entry_path` non-NULL).
    pub is_entry: bool,
    /// Stored `size`: file bytes for outer rows, entry bytes inside
    /// archives.
    pub size: i64,
    /// Stored `mtime` in nanoseconds since the Unix epoch (the outer
    /// file's mtime for every row of an archive).
    pub mtime: Option<i64>,
}

/// Previous index rows grouped by `file_path`, loaded before an
/// incremental update scan. Entries are *removed* as the scan consumes
/// them; whatever remains at the end of the walk belongs to files that
/// are gone (or no longer covered by the current options) and must be
/// deleted.
pub(crate) type PrevMap = std::collections::HashMap<String, Vec<PrevDoc>>;

/// Extensions classified as binary by the scanner (optimization only;
/// these files are counted as ignored and never sent through text
/// processing).
pub const BINARY_EXTENSIONS: &[&str] = &[
    "exe", "dll", "so", "dylib", "png", "jpg", "jpeg", "gif", "bmp", "ico", "webp", "class",
];

/// Extensions recognized as ZIP-based archives.
pub const ARCHIVE_EXTENSIONS: &[&str] = &["zip", "jar", "war", "ear", "aar", "apk"];

pub(crate) struct ScannerConfig {
    pub opts: Arc<BuildOptions>,
    pub progress: Progress,
    pub cancelled: Arc<AtomicBool>,
    pub errors: Arc<ErrorSink>,
    pub timings: Arc<BuildTimings>,
    /// Number of times each configured directory name was pruned.
    pub excluded_directory_counts: Arc<Mutex<BTreeMap<String, u64>>>,
    /// Incremental update: previous `documents` rows grouped by
    /// `file_path`. `None` on a fresh build.
    pub prev_documents: Option<Arc<Mutex<PrevMap>>>,
}

/// Scans all source directories and sends jobs to `tx`.
///
/// Returns when the whole walk is finished or cancelled. Scanner errors
/// (unreadable directories, and so on) are counted and reported but never
/// stop the walk. `tx` must be dropped by the caller after this returns
/// so workers observe the disconnect.
pub(crate) fn scan(cfg: &ScannerConfig, tx: &Sender<ScanJob>) {
    let excluded_dir_names: HashSet<String> = cfg
        .opts
        .excluded_dirs
        .iter()
        .map(|d| d.to_lowercase())
        .collect();
    let excluded_exts: HashSet<String> = cfg
        .opts
        .excluded_extensions
        .iter()
        .map(|e| e.to_lowercase())
        .collect();

    // `WalkBuilder::max_depth` applies to every root of a walker, so
    // roots split by recursion policy: recursive roots take the
    // unlimited walk, `recursive: false` roots take a second walk
    // capped at `max_depth(1)` — only their immediate level is scanned.
    let mut recursive_roots = Vec::new();
    let mut shallow_roots = Vec::new();
    for root in &cfg.opts.source_directories {
        if root.recursive {
            recursive_roots.push(root.path.clone());
        } else {
            shallow_roots.push(root.path.clone());
        }
    }
    if !recursive_roots.is_empty() {
        run_walk(
            cfg,
            tx,
            &recursive_roots,
            None,
            &excluded_dir_names,
            &excluded_exts,
        );
    }
    if !shallow_roots.is_empty() {
        run_walk(
            cfg,
            tx,
            &shallow_roots,
            Some(1),
            &excluded_dir_names,
            &excluded_exts,
        );
    }

    // Incremental update: whatever remains in the map belongs to files
    // the walk never produced — deleted, renamed away, or no longer
    // covered by the current options. Their documents and FTS rows are
    // deleted by the writer.
    if let Some(prev) = &cfg.prev_documents {
        let leftover = std::mem::take(&mut *prev.lock().unwrap());
        if !leftover.is_empty() {
            cfg.progress.inc_files_deleted(leftover.len() as u64);
            let mut ids = Vec::new();
            for rows in leftover.into_values() {
                ids.extend(rows.iter().map(|r| r.id));
            }
            for chunk in ids.chunks(DELETE_CHUNK) {
                send_job(
                    tx,
                    ScanJob::DeleteIds(chunk.to_vec()),
                    &cfg.cancelled,
                    &cfg.timings,
                );
            }
        }
    }
    // The factory's last sender clone is dropped here; the caller drops
    // `tx` when the walker driver finishes, closing the channel for
    // good.
}

/// Maximum number of rowids carried by one `DeleteIds` job; keeps
/// individual channel messages small when many files disappear.
const DELETE_CHUNK: usize = 512;

/// Runs one parallel walk over `roots` and sends jobs to `tx`.
///
/// `max_depth` is forwarded to [`WalkBuilder::max_depth`]: `None` walks
/// the whole subtrees, `Some(1)` scans only the roots' immediate level
/// (used for `recursive: false` roots). Returns when the walk is
/// finished or cancelled.
fn run_walk(
    cfg: &ScannerConfig,
    tx: &Sender<ScanJob>,
    roots: &[PathBuf],
    max_depth: Option<usize>,
    excluded_dir_names: &HashSet<String>,
    excluded_exts: &HashSet<String>,
) {
    let scan_progress = cfg.progress.clone();
    let excluded_directory_counts = Arc::clone(&cfg.excluded_directory_counts);
    let excluded_dir_names = excluded_dir_names.clone();
    let mut builder = WalkBuilder::new(&roots[0]);
    for root in &roots[1..] {
        builder.add(root);
    }
    builder
        .threads(cfg.opts.walker_threads)
        .max_depth(max_depth)
        .follow_links(false)
        .standard_filters(false)
        .hidden(false)
        .git_ignore(cfg.opts.respect_gitignore)
        .require_git(false)
        // Excluded directory names prune whole subtrees; this works for
        // the parallel walker too.
        .filter_entry(move |entry| {
            let Some(name) = excluded_dir_name(entry, &excluded_dir_names) else {
                return true;
            };
            scan_progress.inc_directories_excluded(1);
            *excluded_directory_counts
                .lock()
                .unwrap()
                .entry(name)
                .or_insert(0) += 1;
            false
        });

    let walker = builder.build_parallel();

    // `WalkParallel::run` takes a closure factory: it is invoked once
    // per walker thread and each invocation produces the entry
    // callback. All shared state is cloned per thread.
    let progress = cfg.progress.clone();
    let cancelled = Arc::clone(&cfg.cancelled);
    let timings = Arc::clone(&cfg.timings);
    let errors = Arc::clone(&cfg.errors);
    let sender = tx.clone();
    let opts = Arc::clone(&cfg.opts);
    let prev_documents = cfg.prev_documents.clone();
    let excluded_exts = excluded_exts.clone();
    walker.run(move || {
        let progress = progress.clone();
        let cancelled = Arc::clone(&cancelled);
        let errors = Arc::clone(&errors);
        let timings = Arc::clone(&timings);
        let sender = sender.clone();
        let excluded_exts = excluded_exts.clone();
        let opts = Arc::clone(&opts);
        let prev_documents = prev_documents.clone();
        Box::new(move |entry: Result<ignore::DirEntry, ignore::Error>| {
            if cancelled.load(Ordering::Acquire) {
                return ignore::WalkState::Quit;
            }
            let entry = match entry {
                Ok(e) => e,
                Err(err) => {
                    errors.push_scan_error(&err);
                    return ignore::WalkState::Continue;
                }
            };
            let ctx = WalkCtx {
                excluded_exts: &excluded_exts,
                opts: &opts,
                prev_documents: &prev_documents,
                progress: &progress,
                tx: &sender,
                cancelled: &cancelled,
                errors: &errors,
                timings: &timings,
            };
            handle_entry(&entry, &ctx);
            ignore::WalkState::Continue
        })
    });
}

/// Per-walker-thread context for [`handle_entry`]: shared references
/// cloned once per walker thread.
struct WalkCtx<'a> {
    excluded_exts: &'a HashSet<String>,
    opts: &'a BuildOptions,
    prev_documents: &'a Option<Arc<Mutex<PrevMap>>>,
    progress: &'a Progress,
    tx: &'a Sender<ScanJob>,
    cancelled: &'a AtomicBool,
    errors: &'a ErrorSink,
    timings: &'a BuildTimings,
}

fn handle_entry(entry: &DirEntry, ctx: &WalkCtx) {
    let file_type = match entry.file_type() {
        Some(ft) => ft,
        None => return,
    };
    // Symlinks and junctions are never followed. The `ignore` walker with
    // follow_links(false) does not descend into them, but defensive
    // checks also skip any symlink entries themselves.
    if file_type.is_symlink() {
        return;
    }
    if !file_type.is_file() {
        return;
    }

    let path = entry.path();
    if path.to_str().is_none() {
        // Non-Unicode paths cannot be stored in the index; they are
        // reported as a recoverable error rather than silently skipped
        // or lossily converted. The lossy string is only used for the
        // diagnostic message, never for reopening.
        ctx.progress.inc_files_seen(1);
        ctx.errors.push(
            FileErrorCode::InvalidUnicodePath,
            path.to_string_lossy().into_owned(),
            None,
            "file path is not valid Unicode".to_string(),
        );
        return;
    }

    ctx.progress.inc_files_seen(1);

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase());

    if let Some(ext) = &ext {
        if ctx.excluded_exts.contains(ext) {
            ctx.progress.inc_files_ignored(1);
            ctx.progress.inc_files_ignored_by_extension(1);
            return;
        }
    }

    // `DirEntry::metadata` uses the plain Win32 APIs and fails on paths
    // beyond MAX_PATH; retry through the verbatim path so long files
    // still carry correct size/mtime into the index.
    let (size, mtime) = match entry
        .metadata()
        .or_else(|_| crate::longpath::symlink_metadata(path))
    {
        Ok(md) => (md.len(), md.modified().ok().and_then(systemtime_to_nanos)),
        Err(_) => (0, None),
    };

    // Incremental update: compare scan-time metadata with the previous
    // index rows for this path. Unchanged files keep their documents
    // and FTS rows and are never re-read — this is where an update
    // saves its time. Modified files are reprocessed after their old
    // rows are scheduled for deletion; unknown paths are simply new.
    if let Some(prev) = ctx.prev_documents {
        // The stored file_path is a UTF-8 string produced by the same
        // lossy conversion; the key is only a comparison artifact.
        let key = path.to_string_lossy();
        let removed = prev.lock().unwrap().remove(key.as_ref());
        if let Some(rows) = removed {
            // An archive currently disabled by the options keeps no
            // rows: its previous entries are dropped unconditionally.
            let archive_disabled = ext
                .as_deref()
                .is_some_and(|e| ARCHIVE_EXTENSIONS.contains(&e))
                && !ctx.opts.archives.enabled;
            if !archive_disabled && rows_unchanged(&rows, size, mtime) {
                ctx.progress.inc_files_unchanged(1);
                return;
            }
            ctx.progress.inc_files_modified(1);
            send_job(
                ctx.tx,
                ScanJob::DeleteIds(rows.iter().map(|r| r.id).collect()),
                ctx.cancelled,
                ctx.timings,
            );
        }
    }

    let job = FileJob {
        path: path.to_path_buf(),
        ext: ext.clone(),
        size,
        mtime,
    };

    // Extension classification: an optimization only. Binary files are
    // not given a document row; unknown or missing extensions are sent
    // to a worker for authoritative sniffing.
    if let Some(ext) = &ext {
        if BINARY_EXTENSIONS.contains(&ext.as_str()) {
            ctx.progress.inc_files_ignored(1);
            ctx.progress.inc_files_ignored_by_extension(1);
            return;
        }
        if ARCHIVE_EXTENSIONS.contains(&ext.as_str()) {
            send_job(ctx.tx, ScanJob::Archive(job), ctx.cancelled, ctx.timings);
            return;
        }
    }
    send_job(ctx.tx, ScanJob::File(job), ctx.cancelled, ctx.timings);
}

/// Sends a job, blocking while the bounded channel is full, but waking
/// up regularly to observe cancellation so scanner production stops.
/// Time spent blocked on a full channel is recorded in
/// [`BuildTimings::scan_send_blocked`]; the fast path stays untimed.
fn send_job(
    tx: &Sender<ScanJob>,
    mut job: ScanJob,
    cancelled: &AtomicBool,
    timings: &BuildTimings,
) {
    let mut blocked_at = None;
    loop {
        if cancelled.load(Ordering::Acquire) {
            break;
        }
        match tx.try_send(job) {
            Ok(()) => break,
            Err(TrySendError::Full(j)) => {
                job = j;
                blocked_at.get_or_insert_with(Instant::now);
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(TrySendError::Disconnected(_)) => break,
        }
    }
    if let Some(t) = blocked_at {
        BuildTimings::add(&timings.scan_send_blocked, t.elapsed());
    }
}

/// Whether the stored rows of one `file_path` still match the
/// scan-time metadata. Outer rows (`entry_path` NULL) carry the file's
/// own size and mtime. Archive entries carry the *outer* mtime but the
/// *entry's* size, so for an archive that produced no outer row only
/// the mtime comparison is possible — a known, accepted limitation of
/// the `size + mtime` identity rule (documented in `docs/update.md`).
fn rows_unchanged(rows: &[PrevDoc], size: u64, mtime: Option<i64>) -> bool {
    match rows.iter().find(|r| !r.is_entry) {
        Some(outer) => outer.size == size as i64 && outer.mtime == mtime,
        None => rows.iter().all(|r| r.mtime == mtime),
    }
}

/// Returns the normalized excluded name when the entry is a directory
/// whose exact name is configured for pruning. Matching is
/// case-insensitive because Windows paths are case-insensitive.
fn excluded_dir_name(entry: &DirEntry, excluded_dirs: &HashSet<String>) -> Option<String> {
    if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
        return None;
    }
    let name = entry.file_name().to_str()?.to_lowercase();
    excluded_dirs.contains(&name).then_some(name)
}

/// Converts a `SystemTime` to nanoseconds since the Unix epoch.
pub(crate) fn systemtime_to_nanos(t: std::time::SystemTime) -> Option<i64> {
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_nanos()).ok(),
        Err(e) => i64::try_from(-(e.duration().as_nanos() as i128)).ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_extension_list_excludes_common_types() {
        for ext in ["exe", "dll", "png", "jpg", "ico", "webp", "class"] {
            assert!(BINARY_EXTENSIONS.contains(&ext), "{ext} missing");
        }
    }

    #[test]
    fn archive_extension_list_matches_spec() {
        for ext in ["zip", "jar", "war", "ear", "aar", "apk"] {
            assert!(ARCHIVE_EXTENSIONS.contains(&ext), "{ext} missing");
        }
    }

    #[test]
    fn scan_channel_capacity_is_documented_value() {
        assert_eq!(SCAN_CHANNEL_CAPACITY, 4096);
    }

    #[test]
    fn systemtime_conversions_round_trip() {
        let now = std::time::SystemTime::now();
        let nanos = systemtime_to_nanos(now).unwrap();
        assert!(nanos > 1_600_000_000_000_000_000);
    }
}
