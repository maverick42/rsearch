//! Structured build report returned by a completed build.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

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

/// Maximum number of entries in [`BuildSummary::top_extensions`].
pub const MAX_TOP_EXTENSIONS: usize = 5;

/// What a build actually did. `Update` is the *effective* mode: an
/// `update_index` call that had to fall back to a full rebuild reports
/// [`BuildKind::Full`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BuildKind {
    /// A complete rebuild (or an update that fell back to one).
    Full,
    /// An incremental update on top of the previous index.
    Update,
}

/// File-level delta of an incremental update, derived from the same
/// progress counters the pipeline tests already verify.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UpdateDelta {
    /// Files seen by the scan that had no previous index rows.
    pub added: usize,
    /// Files whose previous rows were deleted because the scan no
    /// longer produced them (deleted, moved out, newly excluded).
    pub removed: usize,
    /// Files whose previous rows were deleted because `size`/`mtime`
    /// changed; they were reprocessed normally.
    pub updated: usize,
}

/// Small, stable, serializable summary of one build — designed to be
/// stored as-is by the project catalog so a UI can display the last
/// build without reopening the index.
///
/// Every field is a direct projection of the counters the pipeline
/// already produces: nothing is recomputed through a second path.
/// Deliberately absent: the index file's size and mtime — those are
/// read live from the filesystem at display time, never stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildSummary {
    /// Documents indexed into FTS this run (files and archive entries
    /// with status 0) — the `files_indexed` counter.
    pub indexed_files: usize,
    /// Extension profile of the documents indexed this run: at most
    /// [`MAX_TOP_EXTENSIONS`] `(extension, count)` pairs ordered by
    /// descending count, ties broken by alphabetical extension order.
    /// Counted by the writer while it inserts document rows — for an
    /// incremental update this reflects the rows written in *this* run,
    /// not the whole index.
    pub top_extensions: Vec<(String, usize)>,
    /// Files rejected by a name rule: an exclude mask (or include list
    /// that does not match), or a known-binary extension. Field-level
    /// serde default so summaries written before the rename still load.
    #[serde(default)]
    pub ignored_by_name: usize,
    /// Files ignored after content sniffing classified them as binary.
    pub ignored_by_sniff: usize,
    /// Files over the configured size limit.
    pub too_large: usize,
    /// Recoverable per-file errors.
    pub errors: usize,
    /// Documents rejected by archive security limits.
    pub security_limits: usize,
    /// Archive files processed.
    pub archives_processed: usize,
    /// Archive entries indexed this run.
    pub archive_entries_indexed: usize,
    /// Wall-clock duration of the whole build.
    pub duration: Duration,
    /// Whether archive processing was enabled for *this* build.
    pub archives_included: bool,
    /// What the build actually did.
    pub kind: BuildKind,
    /// File-level delta; `Some` only when `kind` is
    /// [`BuildKind::Update`].
    pub update_delta: Option<UpdateDelta>,
}

/// Projects a per-extension document counter into the top-5 list for
/// [`BuildSummary::top_extensions`]: descending count, ties broken by
/// ascending extension. The total ordering makes the result
/// deterministic.
pub(crate) fn top_extensions(counts: &BTreeMap<String, u64>) -> Vec<(String, usize)> {
    let mut ranked: Vec<(String, u64)> = counts.iter().map(|(ext, &n)| (ext.clone(), n)).collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.truncate(MAX_TOP_EXTENSIONS);
    ranked
        .into_iter()
        .map(|(ext, n)| (ext, usize::try_from(n).unwrap_or(usize::MAX)))
        .collect()
}

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
    /// Directory names pruned by `BuildOptions::excluded_dirs`, normalized
    /// to lowercase, with the number of matching directories encountered.
    pub excluded_directories: BTreeMap<String, u64>,
    /// Size of the final active index file, when the build completed.
    pub index_size: Option<u64>,
    /// SQLite version used to build the index.
    pub sqlite_version: String,
    /// Whether the build was cancelled by the user.
    pub cancelled: bool,
    /// Compact serializable summary of the build, suitable for being
    /// stored by the project catalog. On a cancelled or failed build it
    /// reflects the partial state at shutdown time.
    pub summary: BuildSummary,
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
        writeln!(f, "files ignored by name: {}", c.files_ignored_by_name)?;
        writeln!(f, "files ignored by sniff:{}", c.files_ignored_by_sniff)?;
        writeln!(f, "dirs excluded:        {}", c.directories_excluded)?;
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
        if c.files_unchanged + c.files_modified + c.files_deleted > 0 {
            writeln!(f, "files unchanged:      {}", c.files_unchanged)?;
            writeln!(f, "files modified:       {}", c.files_modified)?;
            writeln!(f, "files deleted:        {}", c.files_deleted)?;
        }
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
            excluded_directories: BTreeMap::new(),
            index_size: Some(1234),
            sqlite_version: "3.45.0".into(),
            cancelled: false,
            summary: BuildSummary {
                indexed_files: 8,
                top_extensions: vec![("rs".into(), 8)],
                ignored_by_name: 0,
                ignored_by_sniff: 0,
                too_large: 0,
                errors: 0,
                security_limits: 0,
                archives_processed: 0,
                archive_entries_indexed: 0,
                duration: Duration::from_secs(1),
                archives_included: true,
                kind: BuildKind::Full,
                update_delta: None,
            },
        };
        let text = report.to_string();
        assert!(text.contains("files seen:            10"));
        assert!(text.contains("index size:            1234 bytes"));
    }

    #[test]
    fn top_extensions_orders_by_count_then_extension() {
        let mut counts = BTreeMap::new();
        // Voluntary tie: txt and md both appear 3 times; md sorts first.
        counts.insert("txt".to_string(), 3);
        counts.insert("md".to_string(), 3);
        counts.insert("rs".to_string(), 10);
        counts.insert("log".to_string(), 1);
        counts.insert("json".to_string(), 7);
        counts.insert("toml".to_string(), 2);
        counts.insert("yml".to_string(), 5);

        let top = top_extensions(&counts);
        assert_eq!(
            top,
            vec![
                ("rs".to_string(), 10),
                ("json".to_string(), 7),
                ("yml".to_string(), 5),
                ("md".to_string(), 3),
                ("txt".to_string(), 3),
            ],
            "5 entries max, count descending, ties alphabetical"
        );
    }

    #[test]
    fn top_extensions_handles_empty_and_small_maps() {
        assert!(top_extensions(&BTreeMap::new()).is_empty());
        let mut counts = BTreeMap::new();
        counts.insert("rs".to_string(), 2);
        assert_eq!(top_extensions(&counts), vec![("rs".to_string(), 2)]);
    }

    #[test]
    fn build_summary_json_round_trips() {
        let summary = BuildSummary {
            indexed_files: 42,
            top_extensions: vec![("rs".into(), 30), ("toml".into(), 12)],
            ignored_by_name: 3,
            ignored_by_sniff: 1,
            too_large: 2,
            errors: 4,
            security_limits: 1,
            archives_processed: 7,
            archive_entries_indexed: 55,
            duration: Duration::from_millis(1234),
            archives_included: true,
            kind: BuildKind::Update,
            update_delta: Some(UpdateDelta {
                added: 5,
                removed: 2,
                updated: 3,
            }),
        };
        let json = serde_json::to_string(&summary).expect("serialize");
        let back: BuildSummary = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(summary, back);
    }
}
