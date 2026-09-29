//! Structured build report returned by a completed build.

use std::path::PathBuf;
use std::time::Duration;

use crate::error::FileErrorRecord;
use crate::progress::ProgressSnapshot;

/// A source directory excluded before the scan: an exact duplicate of,
/// or contained in, another source root after path normalization.
#[derive(Debug, Clone)]
pub struct SkippedRoot {
    /// The excluded root as given in `BuildOptions::source_directories`.
    pub path: PathBuf,
    /// Why it was excluded (for example "contained in source root C:\a").
    pub reason: String,
}

/// Maximum number of detailed error entries kept in a report. The total
/// count is always exact; only the detail list is capped.
pub const MAX_DETAILED_ERRORS: usize = 1000;

/// Wall-clock measurements of the pipeline stages.
///
/// The pipeline stages overlap: `scanning`, `processing` and `writing`
/// run concurrently, so these are per-stage measurements — the moment
/// (or the accumulated busy time) of each stage — **not** disjoint
/// slices of `total`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PhaseDurations {
    /// Time from build start until the walker finished producing jobs.
    pub scanning: Duration,
    /// Time from build start until all workers finished.
    pub processing: Duration,
    /// Busy time the writer spent inside SQLite operations: database
    /// open, statement execution and batch commits. Excludes the time
    /// spent waiting on the document channel.
    pub writing: Duration,
    /// Time spent in metadata, FTS optimize and final commit.
    pub finalizing: Duration,
    /// Time spent validating and atomically activating the new index.
    pub swapping: Duration,
    /// Total build duration.
    pub total: Duration,
}

/// Fine-grained busy/wait breakdown of the pipeline, aggregated over
/// all threads of a stage.
///
/// These are per-operation sums measured inside each stage — they
/// overlap in wall-clock time (stages run concurrently) and multipliers
/// apply (worker fields are summed across all worker threads). They are
/// not disjoint slices of [`PhaseDurations::total`]; they exist to
/// answer "where does the time go inside stage X".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PipelineTimings {
    /// Walker threads blocked pushing jobs into a full scanner channel
    /// (channel A backpressure).
    pub scan_send_blocked: Duration,
    /// Worker threads idle waiting for scan jobs (starvation).
    pub worker_recv_wait: Duration,
    /// Worker time in filesystem I/O: open, prefix read, full read,
    /// metadata calls.
    pub worker_io: Duration,
    /// Worker time in strict text decoding (`decode_bytes`).
    pub worker_decode: Duration,
    /// Worker time processing archives.
    pub worker_archive: Duration,
    /// Worker time blocked sending documents into a full writer channel
    /// (channel B backpressure).
    pub worker_send_wait: Duration,
    /// Worker time blocked acquiring the in-flight byte budget.
    pub worker_budget_wait: Duration,
    /// Writer idle waiting on the document channel.
    pub writer_recv_wait: Duration,
    /// Writer time opening and initializing the build database
    /// (pragmas + schema + FTS5 probe).
    pub db_open: Duration,
    /// Writer time in `BEGIN` statements.
    pub tx_begin: Duration,
    /// Writer time in `INSERT INTO documents` (bind + step + rowid).
    pub insert_documents: Duration,
    /// Writer time in `INSERT INTO fts` (bind + step; includes FTS5
    /// tokenization and index updates).
    pub insert_fts: Duration,
    /// Writer time in `COMMIT` statements during the build.
    pub batch_commit: Duration,
    /// Number of batch commits performed.
    pub batch_commits: u64,
    /// Writer time in `INSERT INTO fts(fts) VALUES('optimize')` during
    /// finalization (part of [`PhaseDurations::finalizing`]).
    pub fts_optimize: Duration,
}

/// Final outcome summary of a build.
#[derive(Debug, Clone)]
pub struct BuildReport {
    /// All progress counters at the end of the build.
    pub counters: ProgressSnapshot,
    /// Total number of recoverable errors (including errors whose details
    /// were dropped from [`Self::errors`]).
    pub total_errors: u64,
    /// Detailed error records, capped at [`MAX_DETAILED_ERRORS`] entries.
    pub errors: Vec<FileErrorRecord>,
    /// Number of error records omitted from [`Self::errors`].
    pub omitted_errors: u64,
    /// Phase durations.
    pub durations: PhaseDurations,
    /// Per-operation pipeline timing breakdown.
    pub timings: PipelineTimings,
    /// Source roots skipped before the scan (duplicates or roots
    /// contained in another root), each with its reason.
    pub skipped_roots: Vec<SkippedRoot>,
    /// Size of the final active index file, when the build completed.
    pub index_size: Option<u64>,
    /// SQLite version used to build the index.
    pub sqlite_version: String,
    /// Whether the build was cancelled by the user.
    pub cancelled: bool,
}

impl BuildReport {
    /// Number of successfully indexed documents (status 0).
    pub fn indexed_documents(&self) -> u64 {
        self.counters.files_indexed
    }

    /// Number of documents stored with status 2 (too large).
    pub fn too_large_documents(&self) -> u64 {
        self.counters.files_too_large
    }

    /// Number of documents stored with status 4 (security limit).
    pub fn security_limited_documents(&self) -> u64 {
        self.counters.files_security_limited
    }
}

impl std::fmt::Display for BuildReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let c = &self.counters;
        writeln!(f, "files seen:            {}", c.files_seen)?;
        writeln!(f, "files indexed:        {}", c.files_indexed)?;
        writeln!(f, "files ignored:        {}", c.files_ignored)?;
        writeln!(f, "files too large:      {}", c.files_too_large)?;
        writeln!(f, "security limited:     {}", c.files_security_limited)?;
        writeln!(
            f,
            "errors:               {} ({} detailed)",
            c.errors,
            self.errors.len()
        )?;
        writeln!(f, "fallback decodes:     {}", c.fallback_decodes)?;
        writeln!(f, "archives:             {}", c.archives)?;
        writeln!(f, "archive entries:      {}", c.archive_entries)?;
        writeln!(f, "bytes read:           {}", c.bytes_read)?;
        writeln!(f, "bytes indexed:        {}", c.bytes_indexed)?;
        writeln!(
            f,
            "total time:           {:.3}s",
            self.durations.total.as_secs_f64()
        )?;
        if let Some(size) = self.index_size {
            writeln!(f, "index size:            {size} bytes")?;
        }
        if !self.skipped_roots.is_empty() {
            writeln!(f, "skipped roots:        {}", self.skipped_roots.len())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_display_lists_counters() {
        let report = BuildReport {
            counters: ProgressSnapshot {
                files_seen: 10,
                files_indexed: 8,
                ..ProgressSnapshot::default()
            },
            total_errors: 0,
            errors: Vec::new(),
            omitted_errors: 0,
            durations: PhaseDurations::default(),
            timings: PipelineTimings::default(),
            skipped_roots: Vec::new(),
            index_size: Some(1234),
            sqlite_version: "3.45.0".into(),
            cancelled: false,
        };
        let text = report.to_string();
        assert!(text.contains("files seen:            10"));
        assert!(text.contains("index size:            1234 bytes"));
    }
}
