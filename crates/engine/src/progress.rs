//! Build progress tracking, designed for future GUI consumption.
//!
//! Counters are updated with atomics from the scanner, worker and writer
//! threads. No fake exact percentage is exposed because the total amount
//! of work is unknown until the scan completes; consumers get meaningful
//! counters plus the current phase instead.

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

/// Coarse pipeline phase, exposed for progress display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BuildPhase {
    /// The directory walker is producing file jobs.
    Scanning = 0,
    /// Workers are reading, sniffing and decoding files.
    Processing = 1,
    /// The writer is committing the final batches.
    Writing = 2,
    /// Metadata, FTS optimize and final commit are running.
    Finalizing = 3,
    /// The new snapshot is being atomically activated.
    Swapping = 4,
    /// The build finished successfully.
    Completed = 5,
    /// The build was cancelled; shutdown completed.
    Cancelled = 6,
    /// The build failed; shutdown completed.
    Failed = 7,
}

impl BuildPhase {
    /// Whether this phase is one of the terminal phases.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            BuildPhase::Completed | BuildPhase::Cancelled | BuildPhase::Failed
        )
    }
}

impl std::fmt::Display for BuildPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            BuildPhase::Scanning => "Scanning",
            BuildPhase::Processing => "Processing",
            BuildPhase::Writing => "Writing",
            BuildPhase::Finalizing => "Finalizing",
            BuildPhase::Swapping => "Swapping",
            BuildPhase::Completed => "Completed",
            BuildPhase::Cancelled => "Cancelled",
            BuildPhase::Failed => "Failed",
        };
        f.write_str(name)
    }
}

/// Snapshot of all progress counters at a point in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProgressSnapshot {
    /// Current pipeline phase.
    pub phase: Option<BuildPhase>,
    /// Regular files seen by the scanner.
    pub files_seen: u64,
    /// Files intentionally ignored (excluded or classified binary).
    pub files_ignored: u64,
    /// Documents successfully indexed into FTS.
    pub files_indexed: u64,
    /// Files larger than the configured size limit.
    pub files_too_large: u64,
    /// Archive entries that hit a security limit.
    pub files_security_limited: u64,
    /// Recoverable errors (per-file) plus scanner errors.
    pub errors: u64,
    /// Decodes that used the configured fallback encoding.
    pub fallback_decodes: u64,
    /// Archive files opened for processing.
    pub archives: u64,
    /// Entries seen inside archives (including nested archives).
    pub archive_entries: u64,
    /// Archive entries producing indexed documents.
    pub archive_entries_indexed: u64,
    /// Archive entries skipped by known binary extension before decompression.
    pub archive_entries_skipped_by_extension: u64,
    /// Archive entries ignored after reading and sniffing binary content.
    pub archive_entries_ignored_by_sniff: u64,
    /// Archive entries producing recoverable error rows.
    pub archive_entries_errored: u64,
    /// Archive entries producing security-limit rows.
    pub archive_entries_security_limited: u64,
    /// Actual bytes decompressed from archive entries, including nested archives.
    pub archive_bytes_decompressed: u64,
    /// Bytes read from disk (file prefixes and full file reads).
    pub bytes_read: u64,
    /// UTF-8 text bytes accepted for FTS insertion.
    pub bytes_indexed: u64,
}

/// Shared progress state. Cheap to clone; all methods are lock-free.
#[derive(Debug, Clone)]
pub struct Progress {
    inner: Arc<ProgressInner>,
}

#[derive(Debug, Default)]
struct ProgressInner {
    phase: AtomicU8,
    has_phase: AtomicU8,
    files_seen: AtomicU64,
    files_ignored: AtomicU64,
    files_indexed: AtomicU64,
    files_too_large: AtomicU64,
    files_security_limited: AtomicU64,
    errors: AtomicU64,
    fallback_decodes: AtomicU64,
    archives: AtomicU64,
    archive_entries: AtomicU64,
    archive_entries_indexed: AtomicU64,
    archive_entries_skipped_by_extension: AtomicU64,
    archive_entries_ignored_by_sniff: AtomicU64,
    archive_entries_errored: AtomicU64,
    archive_entries_security_limited: AtomicU64,
    archive_bytes_decompressed: AtomicU64,
    bytes_read: AtomicU64,
    bytes_indexed: AtomicU64,
}

impl Default for Progress {
    fn default() -> Self {
        Progress {
            inner: Arc::new(ProgressInner::default()),
        }
    }
}

impl Progress {
    /// Creates a zeroed progress tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes a consistent-enough snapshot of the current counters and phase.
    pub fn snapshot(&self) -> ProgressSnapshot {
        let i = &self.inner;
        ProgressSnapshot {
            phase: if i.has_phase.load(Ordering::Acquire) == 1 {
                Some(phase_from_u8(i.phase.load(Ordering::Acquire)))
            } else {
                None
            },
            files_seen: i.files_seen.load(Ordering::Relaxed),
            files_ignored: i.files_ignored.load(Ordering::Relaxed),
            files_indexed: i.files_indexed.load(Ordering::Relaxed),
            files_too_large: i.files_too_large.load(Ordering::Relaxed),
            files_security_limited: i.files_security_limited.load(Ordering::Relaxed),
            errors: i.errors.load(Ordering::Relaxed),
            fallback_decodes: i.fallback_decodes.load(Ordering::Relaxed),
            archives: i.archives.load(Ordering::Relaxed),
            archive_entries: i.archive_entries.load(Ordering::Relaxed),
            archive_entries_indexed: i.archive_entries_indexed.load(Ordering::Relaxed),
            archive_entries_skipped_by_extension: i
                .archive_entries_skipped_by_extension
                .load(Ordering::Relaxed),
            archive_entries_ignored_by_sniff: i
                .archive_entries_ignored_by_sniff
                .load(Ordering::Relaxed),
            archive_entries_errored: i.archive_entries_errored.load(Ordering::Relaxed),
            archive_entries_security_limited: i
                .archive_entries_security_limited
                .load(Ordering::Relaxed),
            archive_bytes_decompressed: i.archive_bytes_decompressed.load(Ordering::Relaxed),
            bytes_read: i.bytes_read.load(Ordering::Relaxed),
            bytes_indexed: i.bytes_indexed.load(Ordering::Relaxed),
        }
    }

    /// Sets the current pipeline phase.
    pub fn set_phase(&self, phase: BuildPhase) {
        let i = &self.inner;
        i.phase.store(phase as u8, Ordering::Release);
        i.has_phase.store(1, Ordering::Release);
    }

    /// Returns the current phase, if one has been set yet.
    pub fn phase(&self) -> Option<BuildPhase> {
        self.snapshot().phase
    }

    // Internal increment helpers used by the pipeline. Kept `pub(crate)`
    // so integration tests inside the crate cannot mutate foreign builds.
    pub(crate) fn inc_files_seen(&self, n: u64) {
        self.inner.files_seen.fetch_add(n, Ordering::Relaxed);
    }
    pub(crate) fn inc_files_ignored(&self, n: u64) {
        self.inner.files_ignored.fetch_add(n, Ordering::Relaxed);
    }
    pub(crate) fn inc_files_indexed(&self, n: u64) {
        self.inner.files_indexed.fetch_add(n, Ordering::Relaxed);
    }
    pub(crate) fn inc_files_too_large(&self, n: u64) {
        self.inner.files_too_large.fetch_add(n, Ordering::Relaxed);
    }
    pub(crate) fn inc_files_security_limited(&self, n: u64) {
        self.inner
            .files_security_limited
            .fetch_add(n, Ordering::Relaxed);
    }
    pub(crate) fn inc_errors(&self, n: u64) {
        self.inner.errors.fetch_add(n, Ordering::Relaxed);
    }
    pub(crate) fn inc_fallback_decodes(&self, n: u64) {
        self.inner.fallback_decodes.fetch_add(n, Ordering::Relaxed);
    }
    pub(crate) fn inc_archives(&self, n: u64) {
        self.inner.archives.fetch_add(n, Ordering::Relaxed);
    }
    pub(crate) fn inc_archive_entries(&self, n: u64) {
        self.inner.archive_entries.fetch_add(n, Ordering::Relaxed);
    }
    pub(crate) fn inc_archive_entries_indexed(&self, n: u64) {
        self.inner
            .archive_entries_indexed
            .fetch_add(n, Ordering::Relaxed);
    }
    pub(crate) fn inc_archive_entries_skipped_by_extension(&self, n: u64) {
        self.inner
            .archive_entries_skipped_by_extension
            .fetch_add(n, Ordering::Relaxed);
    }
    pub(crate) fn inc_archive_entries_ignored_by_sniff(&self, n: u64) {
        self.inner
            .archive_entries_ignored_by_sniff
            .fetch_add(n, Ordering::Relaxed);
    }
    pub(crate) fn inc_archive_entries_errored(&self, n: u64) {
        self.inner
            .archive_entries_errored
            .fetch_add(n, Ordering::Relaxed);
    }
    pub(crate) fn inc_archive_entries_security_limited(&self, n: u64) {
        self.inner
            .archive_entries_security_limited
            .fetch_add(n, Ordering::Relaxed);
    }
    pub(crate) fn inc_archive_bytes_decompressed(&self, n: u64) {
        self.inner
            .archive_bytes_decompressed
            .fetch_add(n, Ordering::Relaxed);
    }
    pub(crate) fn inc_bytes_read(&self, n: u64) {
        self.inner.bytes_read.fetch_add(n, Ordering::Relaxed);
    }
    pub(crate) fn inc_bytes_indexed(&self, n: u64) {
        self.inner.bytes_indexed.fetch_add(n, Ordering::Relaxed);
    }
}

fn phase_from_u8(v: u8) -> BuildPhase {
    match v {
        0 => BuildPhase::Scanning,
        1 => BuildPhase::Processing,
        2 => BuildPhase::Writing,
        3 => BuildPhase::Finalizing,
        4 => BuildPhase::Swapping,
        5 => BuildPhase::Completed,
        6 => BuildPhase::Cancelled,
        _ => BuildPhase::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_starts_empty() {
        let p = Progress::new();
        let s = p.snapshot();
        assert_eq!(s.phase, None);
        assert_eq!(s.files_seen, 0);
        assert_eq!(s.bytes_indexed, 0);
    }

    #[test]
    fn counters_add_up() {
        let p = Progress::new();
        p.inc_files_seen(3);
        p.inc_bytes_read(10);
        p.inc_bytes_read(5);
        let s = p.snapshot();
        assert_eq!(s.files_seen, 3);
        assert_eq!(s.bytes_read, 15);
    }

    #[test]
    fn phase_round_trip() {
        let p = Progress::new();
        assert_eq!(p.phase(), None);
        p.set_phase(BuildPhase::Scanning);
        assert_eq!(p.phase(), Some(BuildPhase::Scanning));
        p.set_phase(BuildPhase::Completed);
        assert_eq!(p.phase(), Some(BuildPhase::Completed));
        assert!(p.phase().unwrap().is_terminal());
    }
}
