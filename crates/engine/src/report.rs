//! Structured build report returned by a completed build.

use std::time::Duration;

use crate::error::FileErrorRecord;
use crate::progress::ProgressSnapshot;

/// Maximum number of detailed error entries kept in a report. The total
/// count is always exact; only the detail list is capped.
pub const MAX_DETAILED_ERRORS: usize = 1000;

/// Wall-clock durations of the pipeline phases.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PhaseDurations {
    /// Time from build start until the walker finished producing jobs.
    pub scanning: Duration,
    /// Time from build start until all workers finished.
    pub processing: Duration,
    /// Time from build start until the writer finished committing batches.
    pub writing: Duration,
    /// Time spent in metadata, FTS optimize and final commit.
    pub finalizing: Duration,
    /// Time spent validating and atomically activating the new index.
    pub swapping: Duration,
    /// Total build duration.
    pub total: Duration,
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
            index_size: Some(1234),
            sqlite_version: "3.45.0".into(),
            cancelled: false,
        };
        let text = report.to_string();
        assert!(text.contains("files seen:            10"));
        assert!(text.contains("index size:            1234 bytes"));
    }
}
