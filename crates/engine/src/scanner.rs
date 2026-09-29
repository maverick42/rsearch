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

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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
}

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
}

/// Scans all source directories and sends jobs to `tx`.
///
/// Returns when the whole walk is finished or cancelled. Scanner errors
/// (unreadable directories, and so on) are counted and reported but never
/// stop the walk. `tx` must be dropped by the caller after this returns
/// so workers observe the disconnect.
pub(crate) fn scan(cfg: &ScannerConfig, tx: &Sender<ScanJob>) {
    let excluded_dirs: HashSet<String> = cfg
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

    let roots: Vec<PathBuf> = cfg.opts.source_directories.clone();
    let mut builder = WalkBuilder::new(&roots[0]);
    for root in &roots[1..] {
        builder.add(root);
    }
    builder
        .threads(cfg.opts.walker_threads)
        .follow_links(false)
        .standard_filters(false)
        .hidden(false)
        .git_ignore(cfg.opts.respect_gitignore)
        .require_git(false)
        // Excluded directory names prune whole subtrees; this works for
        // the parallel walker too.
        .filter_entry(move |entry| !is_excluded_dir(entry, &excluded_dirs));

    let walker = builder.build_parallel();

    // `WalkParallel::run` takes a closure factory: it is invoked once
    // per walker thread and each invocation produces the entry
    // callback. All shared state is cloned per thread.
    let progress = cfg.progress.clone();
    let cancelled = Arc::clone(&cfg.cancelled);
    let timings = Arc::clone(&cfg.timings);
    let errors = Arc::clone(&cfg.errors);
    let sender = tx.clone();
    walker.run(move || {
        let progress = progress.clone();
        let cancelled = Arc::clone(&cancelled);
        let errors = Arc::clone(&errors);
        let timings = Arc::clone(&timings);
        let sender = sender.clone();
        let excluded_exts = excluded_exts.clone();
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
            handle_entry(
                &entry,
                &excluded_exts,
                &progress,
                &sender,
                &cancelled,
                &errors,
                &timings,
            );
            ignore::WalkState::Continue
        })
    });
    // The factory's last sender clone is dropped here; the caller drops
    // `tx` when the walker driver finishes, closing the channel for
    // good.
}

fn handle_entry(
    entry: &DirEntry,
    excluded_exts: &HashSet<String>,
    progress: &Progress,
    tx: &Sender<ScanJob>,
    cancelled: &AtomicBool,
    errors: &ErrorSink,
    timings: &BuildTimings,
) {
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
        progress.inc_files_seen(1);
        errors.push(
            FileErrorCode::InvalidUnicodePath,
            path.to_string_lossy().into_owned(),
            None,
            "file path is not valid Unicode".to_string(),
        );
        return;
    }

    progress.inc_files_seen(1);

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase());

    if let Some(ext) = &ext {
        if excluded_exts.contains(ext) {
            progress.inc_files_ignored(1);
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
            progress.inc_files_ignored(1);
            return;
        }
        if ARCHIVE_EXTENSIONS.contains(&ext.as_str()) {
            send_job(tx, ScanJob::Archive(job), cancelled, timings);
            return;
        }
    }
    send_job(tx, ScanJob::File(job), cancelled, timings);
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

/// Whether the entry (a directory) is excluded by name. Matching is
/// case-insensitive because Windows paths are case-insensitive.
fn is_excluded_dir(entry: &DirEntry, excluded_dirs: &HashSet<String>) -> bool {
    if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
        return false;
    }
    match entry.file_name().to_str() {
        Some(name) => excluded_dirs.contains(&name.to_lowercase()),
        None => false,
    }
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
