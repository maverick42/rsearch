//! Exact search over a finished index.
//!
//! The FTS5 trigram table is only a candidate selector (D1): it maps a
//! literal query to document ids. Exact results — line, column and
//! context — always come from reopening the real file (or the real
//! archive entry) and re-decoding it with the same [`crate::decoder`]
//! logic used at index time. A document selected by FTS but absent or
//! modified since the snapshot is dropped silently: the index is a
//! point-in-time view, never a replacement for the files.
//!
//! * [`query`]: user text → safe FTS5 phrase and query validation.
//! * [`indexed_search`]: document listing and candidate assembly over
//!   the index database. Only this submodule speaks SQL; nothing above
//!   it ever sees a `Connection`.
//! * [`verifier`]: exact occurrence extraction on the real content,
//!   behind a [`verifier::Matcher`] interface so a future regex mode
//!   only replaces that brick.

pub mod indexed_search;
pub mod query;
pub mod verifier;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::error::STATUS_TOO_LARGE;
use crate::options::EncodingKind;

pub use indexed_search::{iter_documents, DocumentRef};
pub use query::{to_fts5_phrase, validate_query, MIN_QUERY_CHARS};
pub use verifier::{is_whole_word, LiteralMatcher, MatchSpan, Matcher};

/// Failure of a search over an index.
#[derive(Debug)]
pub enum SearchError {
    /// The query is shorter than [`MIN_QUERY_CHARS`]. The message is a
    /// complete UI-facing sentence; display it as-is.
    QueryTooShort,
    /// The index could not be opened or is not a usable rsearch index.
    Index(crate::error::IndexError),
    /// The caller's cancellation flag was raised; the search stopped
    /// at the next check point. No report is produced — partial
    /// results are never reported as a completed search.
    Cancelled,
    /// The search worker terminated without producing a result
    /// (e.g. a panic). `search()` itself never returns this variant;
    /// it exists so callers running the synchronous engine on a
    /// worker thread can surface a silent thread death instead of
    /// waiting forever.
    Internal(String),
}

impl fmt::Display for SearchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SearchError::QueryTooShort => write!(
                f,
                "search query is too short: minimum {MIN_QUERY_CHARS} characters required"
            ),
            SearchError::Index(e) => write!(f, "{e}"),
            SearchError::Cancelled => write!(f, "search cancelled"),
            SearchError::Internal(msg) => write!(f, "internal search error: {msg}"),
        }
    }
}

impl std::error::Error for SearchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SearchError::Index(e) => Some(e),
            _ => None,
        }
    }
}

impl From<crate::error::IndexError> for SearchError {
    fn from(e: crate::error::IndexError) -> Self {
        SearchError::Index(e)
    }
}

/// User-facing search switches. All of them apply at verification
/// time only — the index and the FTS query never change shape.
#[derive(Debug, Clone)]
pub struct SearchOptions {
    /// Exact byte case when `true`; Unicode *simple* case folding
    /// (C+S) when `false` — the same fold the trigram index applies
    /// (measured on the bundled SQLite: `é`↔`É`, `ς`→`σ`, while `ß`,
    /// `İ` and `ﬁ` stay unchanged; D16).
    pub case_sensitive: bool,
    /// Require word boundaries around every match: a word character is
    /// a Unicode alphanumeric or `_`; file edges and everything else
    /// are boundaries.
    pub whole_word: bool,
    /// Lines of context kept around every occurrence (default 2).
    pub context_lines: usize,
    /// File-name masks (`*`/`?` wildcards, case-insensitive, matched
    /// against the file NAME only, never the full path) a candidate
    /// must match to be verified. An empty list keeps every candidate —
    /// the masks can only narrow what the project's own masks already
    /// let into the index.
    pub include_masks: Vec<String>,
    /// File-name masks dropping a candidate even when the include side
    /// matches. For archive entries, a mask matching the parent
    /// archive's name excludes every entry of that archive.
    pub exclude_masks: Vec<String>,
    /// After the indexed candidates, also verify oversized files
    /// (status 2) — each still under the same read cap the build
    /// used (`meta.max_indexed_file_size`). This never means "read
    /// the whole file": matches beyond the cap stay out of coverage,
    /// exactly as before. Default `false`: oversized files are
    /// counted (`candidates_too_large`) and reported as not analyzed
    /// instead of being re-read on every search.
    pub analyze_oversized: bool,
}

impl Default for SearchOptions {
    fn default() -> Self {
        SearchOptions {
            case_sensitive: false,
            whole_word: false,
            context_lines: 2,
            include_masks: Vec::new(),
            exclude_masks: Vec::new(),
            analyze_oversized: false,
        }
    }
}

/// One exact occurrence inside a verified document. Line and column
/// are 1-indexed; the column counts characters (not bytes).
#[derive(Debug, Clone)]
pub struct Occurrence {
    /// 1-indexed line of the match start.
    pub line: usize,
    /// 1-indexed character column of the match start.
    pub column: usize,
    /// The full text of the line containing the match start.
    pub line_text: String,
    /// Up to `context_lines` lines before the match, in file order.
    pub context_before: Vec<String>,
    /// Up to `context_lines` lines after the match, in file order.
    pub context_after: Vec<String>,
}

/// All occurrences found inside one real file or archive entry.
#[derive(Debug, Clone)]
pub struct FileResult {
    /// Physical file path (the archive path for entries).
    pub file_path: PathBuf,
    /// Entry path inside the archive (`inner.zip!/x.xml`), `None` for
    /// regular files.
    pub entry_path: Option<String>,
    /// Verified occurrences; never empty inside a `FileResult` — a
    /// candidate with zero real matches is simply absent.
    pub occurrences: Vec<Occurrence>,
}

/// What a search did, keeping index candidates and verified results
/// strictly separate so a caller can report "N files could not be
/// verified" without confusing it with "no results".
#[derive(Debug)]
pub struct SearchReport {
    /// Verified results: real occurrences in real content.
    pub results: Vec<FileResult>,
    /// Candidates selected through the FTS index.
    pub candidates_from_index: usize,
    /// Candidates added by the too-large union (status 2, never in
    /// FTS — verified under a read cap).
    pub candidates_too_large: usize,
    /// Candidates dropped because the file/archive disappeared or
    /// changed since the snapshot.
    pub skipped_stale: usize,
    /// Documents with status 3 (index-time error: unreadable or
    /// undecodable) passing the name masks: never attempted.
    pub skipped_index_errors: usize,
    /// Documents with status 4 (security limit hit at index time)
    /// passing the name masks: never attempted.
    pub skipped_security_limits: usize,
    /// Candidates still present and unchanged that failed to read or
    /// decode at verification time.
    pub verification_errors: usize,
    /// Too-large documents only read up to the safety cap
    /// (`meta.max_indexed_file_size`); matches beyond the cap are not
    /// covered.
    pub truncated_files: usize,
    /// Parent archives opened during verification — at most one per
    /// archive file holding candidates, however many of its entries
    /// matched (grouped verification).
    pub archives_opened: usize,
    /// Wall-clock duration of the whole search.
    pub elapsed: Duration,
}

/// Progress signals emitted while a search runs, in emission order.
///
/// Events are informational snapshots for callers that render partial
/// progress; the returned [`SearchReport`] stays the single
/// authoritative outcome of the whole search.
#[derive(Debug)]
pub enum SearchEvent {
    /// Every index-selected candidate has been verified. The carried
    /// report is partial when [`SearchOptions::analyze_oversized`] is
    /// set: `candidates_too_large` then counts the oversized files
    /// still awaiting analysis and `truncated_files` is still 0.
    IndexedDone(SearchReport),
    /// One oversized file was verified during the deep scan — `done`
    /// of `total`. `found` carries the file's verified result when it
    /// produced occurrences.
    OversizedProgress {
        /// Oversized files processed so far (1-based).
        done: usize,
        /// Oversized candidates in total.
        total: usize,
        /// Verified occurrences of the file just processed, if any.
        found: Option<FileResult>,
    },
}

/// Runs a literal search over a finished index.
///
/// Pipeline: validate the query → open the index read-only and
/// validate it → select candidates (FTS matches unioned with
/// too-large documents, deduplicated and filtered by the name masks)
/// → reopen and verify every candidate against real content.
///
/// `results` only ever contains verified occurrences; everything the
/// index promised but could not deliver is accounted for in the
/// counters.
///
/// Candidates are sorted by `(file_path, entry_path)`, so all entries
/// of one archive are contiguous: archive candidates are verified in
/// runs sharing a single open of the parent file ([`archives_opened`]
/// counts those opens).
///
/// `cancel` is a cooperative flag — raise it (e.g. from another
/// thread) and the search stops at the next check point, returning
/// [`SearchError::Cancelled`]. Check points sit between candidates,
/// so latency is bounded by one document's verification.
///
/// Equivalent to [`search_events`] with events ignored.
///
/// [`archives_opened`]: SearchReport::archives_opened
pub fn search(
    index_path: &Path,
    query: &str,
    options: &SearchOptions,
    cancel: &AtomicBool,
) -> Result<SearchReport, SearchError> {
    search_events(index_path, query, options, cancel, &mut |_| {})
}

/// [`search`], additionally emitting [`SearchEvent`]s so callers can
/// render partial progress.
///
/// The search runs in two phases: phase A verifies every
/// index-selected candidate, then [`SearchEvent::IndexedDone`] fires
/// with the intermediate report; when [`SearchOptions::analyze_oversized`]
/// is set, phase B verifies the oversized documents next, one
/// [`SearchEvent::OversizedProgress`] per file. The returned report is
/// always the complete one — sorted by `(file_path, entry_path)`,
/// identical to the single-phase result.
///
/// `events` is invoked synchronously on the caller's thread. Raising
/// `cancel` inside a callback stops the search at the next check
/// point — between phases and between oversized files included.
pub fn search_events(
    index_path: &Path,
    query: &str,
    options: &SearchOptions,
    cancel: &AtomicBool,
    events: &mut dyn FnMut(SearchEvent),
) -> Result<SearchReport, SearchError> {
    let started = Instant::now();
    query::validate_query(query)?;
    if cancel.load(Ordering::Acquire) {
        return Err(SearchError::Cancelled);
    }
    let conn = indexed_search::open_index_readonly(index_path)?;
    let name_masks = crate::masks::NameMasks::new(&options.include_masks, &options.exclude_masks);
    let candidates =
        indexed_search::select_candidates(&conn, &query::to_fts5_phrase(query), &name_masks)?;
    let fallback = indexed_search::index_fallback_encoding(&conn);
    let verify_cap = indexed_search::index_max_verify_bytes(&conn);
    let matcher = verifier::LiteralMatcher::new(query, options.case_sensitive);

    let mut results = Vec::new();
    let mut counters = OutcomeCounter::default();

    // Phase A: index-selected candidates only. Oversized documents
    // (status 2 — never in FTS, always regular files) wait for the
    // optional deep scan so indexed results can surface first. Both
    // partitions keep the global (file_path, entry_path) order.
    let (indexed_docs, oversized_docs): (Vec<DocumentRef>, Vec<DocumentRef>) = candidates
        .documents
        .into_iter()
        .partition(|d| d.status != STATUS_TOO_LARGE);

    verify_candidates(
        &indexed_docs,
        &matcher,
        options,
        fallback,
        verify_cap,
        cancel,
        &mut counters,
        &mut results,
        &mut |_| {},
    )?;

    events(SearchEvent::IndexedDone(SearchReport {
        results: results.clone(),
        candidates_from_index: candidates.from_index,
        candidates_too_large: candidates.too_large,
        skipped_stale: counters.skipped_stale,
        skipped_index_errors: candidates.index_errors,
        skipped_security_limits: candidates.security_limits,
        verification_errors: counters.verification_errors,
        truncated_files: counters.truncated_files,
        archives_opened: counters.archives_opened,
        elapsed: started.elapsed(),
    }));

    if cancel.load(Ordering::Acquire) {
        return Err(SearchError::Cancelled);
    }

    // Phase B: optional deep scan of oversized files — the same
    // verify_file path and read cap as before, one file at a time.
    if options.analyze_oversized && !oversized_docs.is_empty() {
        let total = oversized_docs.len();
        let mut done = 0usize;
        let mut on_doc = |found: Option<&FileResult>| {
            done += 1;
            events(SearchEvent::OversizedProgress {
                done,
                total,
                found: found.cloned(),
            });
        };
        verify_candidates(
            &oversized_docs,
            &matcher,
            options,
            fallback,
            verify_cap,
            cancel,
            &mut counters,
            &mut results,
            &mut on_doc,
        )?;
    }

    // Phase-B results may sort ahead of phase-A results; the canonical
    // order of a finished report is (file_path, entry_path).
    results.sort_by(|a, b| {
        a.file_path
            .cmp(&b.file_path)
            .then_with(|| a.entry_path.cmp(&b.entry_path))
    });

    Ok(SearchReport {
        results,
        candidates_from_index: candidates.from_index,
        candidates_too_large: candidates.too_large,
        skipped_stale: counters.skipped_stale,
        skipped_index_errors: candidates.index_errors,
        skipped_security_limits: candidates.security_limits,
        verification_errors: counters.verification_errors,
        truncated_files: counters.truncated_files,
        archives_opened: counters.archives_opened,
        elapsed: started.elapsed(),
    })
}

/// Accumulates the verification counters and turns outcomes into
/// verified results.
#[derive(Default)]
struct OutcomeCounter {
    /// Candidates dropped because the file disappeared or changed
    /// since the index snapshot.
    skipped_stale: usize,
    /// Candidates still present that failed to read or decode.
    verification_errors: usize,
    /// Oversized documents verified under the read cap.
    truncated_files: usize,
    /// Parent archives actually opened by grouped verification.
    archives_opened: usize,
}

impl OutcomeCounter {
    /// Counts one verification outcome and returns the verified result
    /// when the file produced occurrences — the caller decides whether
    /// to surface it (progress event) before pushing it to `results`.
    fn account(
        &mut self,
        doc: &DocumentRef,
        outcome: verifier::VerifyOutcome,
    ) -> Option<FileResult> {
        match outcome {
            verifier::VerifyOutcome::Verified(verified) => {
                if verified.truncated {
                    self.truncated_files += 1;
                }
                if verified.occurrences.is_empty() {
                    None
                } else {
                    Some(FileResult {
                        file_path: doc.file_path.clone(),
                        entry_path: doc.entry_path.clone(),
                        occurrences: verified.occurrences,
                    })
                }
            }
            verifier::VerifyOutcome::Stale => {
                self.skipped_stale += 1;
                None
            }
            verifier::VerifyOutcome::Failed => {
                self.verification_errors += 1;
                None
            }
        }
    }
}

/// Verifies an ordered slice of candidates against real content.
///
/// Regular files go through [`verifier::verify_file`]; contiguous
/// runs of archive entries share a single parent-archive open via
/// [`verifier::verify_archive_run`]. `counters` records each outcome;
/// `on_doc` observes the verified result (progress reporting) before
/// it is pushed to `results`. Returns early with
/// [`SearchError::Cancelled`] when `cancel` is raised; check points
/// sit between candidates.
#[allow(clippy::too_many_arguments)]
fn verify_candidates(
    docs: &[DocumentRef],
    matcher: &verifier::LiteralMatcher,
    options: &SearchOptions,
    fallback: Option<EncodingKind>,
    verify_cap: u64,
    cancel: &AtomicBool,
    counters: &mut OutcomeCounter,
    results: &mut Vec<FileResult>,
    on_doc: &mut dyn FnMut(Option<&FileResult>),
) -> Result<(), SearchError> {
    let mut i = 0usize;
    while i < docs.len() {
        if cancel.load(Ordering::Acquire) {
            return Err(SearchError::Cancelled);
        }
        let doc = &docs[i];
        if doc.entry_path.is_none() {
            let found = counters.account(
                doc,
                verifier::verify_file(doc, matcher, options, fallback, verify_cap),
            );
            on_doc(found.as_ref());
            if let Some(fr) = found {
                results.push(fr);
            }
            i += 1;
            continue;
        }
        // Contiguous run of entries living in the same parent archive:
        // candidates are sorted by (file_path, entry_path), so the run
        // boundary is just the next different file path.
        let mut j = i + 1;
        while j < docs.len() && docs[j].entry_path.is_some() && docs[j].file_path == doc.file_path {
            j += 1;
        }
        let mut out = |d: &DocumentRef, o: verifier::VerifyOutcome| {
            if let Some(fr) = counters.account(d, o) {
                results.push(fr);
            }
        };
        if verifier::verify_archive_run(&docs[i..j], matcher, options, fallback, cancel, &mut out) {
            counters.archives_opened += 1;
        }
        i = j;
    }
    Ok(())
}
