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

pub use indexed_search::{iter_documents, DocumentRef};
pub use query::{to_fts5_phrase, validate_query, MIN_QUERY_CHARS};
pub use verifier::{LiteralMatcher, MatchSpan, Matcher};

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
    /// Restrict results to these extensions (lowercase, with or
    /// without leading dot). `None` searches every document.
    pub extensions: Option<Vec<String>>,
}

impl Default for SearchOptions {
    fn default() -> Self {
        SearchOptions {
            case_sensitive: false,
            whole_word: false,
            context_lines: 2,
            extensions: None,
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
    /// Documents with status 3/4 (error / security limit) matching
    /// the extension filter: never attempted.
    pub skipped_unverifiable: usize,
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

/// Runs a literal search over a finished index.
///
/// Pipeline: validate the query → open the index read-only and
/// validate it → select candidates (FTS matches unioned with
/// too-large documents, deduplicated and extension-filtered) →
/// reopen and verify every candidate against real content.
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
/// [`archives_opened`]: SearchReport::archives_opened
pub fn search(
    index_path: &Path,
    query: &str,
    options: &SearchOptions,
    cancel: &AtomicBool,
) -> Result<SearchReport, SearchError> {
    let started = Instant::now();
    query::validate_query(query)?;
    let conn = indexed_search::open_index_readonly(index_path)?;
    let candidates = indexed_search::select_candidates(
        &conn,
        &query::to_fts5_phrase(query),
        options.extensions.as_deref(),
    )?;
    let fallback = indexed_search::index_fallback_encoding(&conn);
    let verify_cap = indexed_search::index_max_verify_bytes(&conn);
    let matcher = verifier::LiteralMatcher::new(query, options.case_sensitive);

    let mut results = Vec::new();
    let mut skipped_stale = 0usize;
    let mut verification_errors = 0usize;
    let mut truncated_files = 0usize;
    let mut archives_opened = 0usize;
    let mut account = |doc: &DocumentRef, outcome: verifier::VerifyOutcome| match outcome {
        verifier::VerifyOutcome::Verified(verified) => {
            if verified.truncated {
                truncated_files += 1;
            }
            if !verified.occurrences.is_empty() {
                results.push(FileResult {
                    file_path: doc.file_path.clone(),
                    entry_path: doc.entry_path.clone(),
                    occurrences: verified.occurrences,
                });
            }
        }
        verifier::VerifyOutcome::Stale => skipped_stale += 1,
        verifier::VerifyOutcome::Failed => verification_errors += 1,
    };

    let docs = &candidates.documents;
    let mut i = 0usize;
    while i < docs.len() {
        if cancel.load(Ordering::Acquire) {
            return Err(SearchError::Cancelled);
        }
        let doc = &docs[i];
        if doc.entry_path.is_none() {
            account(
                doc,
                verifier::verify_file(doc, &matcher, options, fallback, verify_cap),
            );
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
        if verifier::verify_archive_run(
            &docs[i..j],
            &matcher,
            options,
            fallback,
            cancel,
            &mut account,
        ) {
            archives_opened += 1;
        }
        i = j;
    }

    Ok(SearchReport {
        results,
        candidates_from_index: candidates.from_index,
        candidates_too_large: candidates.too_large,
        skipped_stale,
        skipped_unverifiable: candidates.unverifiable,
        verification_errors,
        truncated_files,
        archives_opened,
        elapsed: started.elapsed(),
    })
}
