//! Exact verification of candidate documents against real content.
//!
//! Every candidate is re-read from its source: regular files through
//! [`crate::longpath`], archive entries by reopening the archive and
//! walking the `outer.zip!/inner.zip!/x.xml` chain. Nothing is ever
//! extracted to disk; decoding reuses [`crate::decoder::decode_bytes`]
//! with the fallback encoding recorded in the index meta at build time,
//! so verification is byte-for-byte identical to indexing.
//!
//! Matching is the only replaceable brick: [`Matcher`] maps decoded
//! text to match spans. A future regex mode only adds a second
//! implementation; candidate assembly and file re-reading stay as they
//! are.

use std::io::{BufReader, Cursor, Read, Seek};

use unicode_casefold::{Locale, UnicodeCaseFold, Variant};
use zip::ZipArchive;

use crate::decoder::{self, BomKind};
use crate::error::STATUS_TOO_LARGE;
use crate::options::EncodingKind;
use crate::scanner::systemtime_to_nanos;

use super::indexed_search::DocumentRef;
use super::{Occurrence, SearchOptions};

/// Safety bound on the in-memory size of one intermediate nested
/// archive while resolving an `outer!/inner!/entry` chain. Build-time
/// `max_nested_size` (64 MiB by default) already bounds what can reach
/// this path; the cap is a hard stop, not a semantic limit.
const NESTED_ARCHIVE_READ_LIMIT: u64 = 256 * 1024 * 1024;

/// Byte span of one match inside decoded text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatchSpan {
    /// Byte offset of the first matched byte.
    pub start: usize,
    /// Byte offset one past the last matched byte.
    pub end: usize,
}

/// The replaceable matching brick: given decoded text, returns every
/// match span (byte offsets, non-overlapping, left to right).
///
/// A future regex mode only needs a second implementation of this
/// trait; candidate selection and file re-reading are unaffected.
pub trait Matcher {
    /// Returns all match spans in `text`.
    fn find(&self, text: &str) -> Vec<MatchSpan>;
}

/// Literal substring matcher.
///
/// Case-insensitive matching folds every character through Unicode
/// **simple case folding** (CaseFolding C+S) — the same fold the FTS5
/// trigram tokenizer applies with `case_sensitive 0`, measured on the
/// bundled SQLite: `é`↔`É`, `à`↔`À`, `Œ`↔`œ`, while `ß` stays `ß`,
/// `İ` stays `İ` and `ﬁ` stays `ﬁ` (no expansions — see
/// `docs/decisions.md`). Using the identical fold on both sides keeps
/// the verifier exactly as permissive as the index: no match the
/// index could select is lost, and nothing it could not is invented.
pub struct LiteralMatcher {
    needle_bytes: Vec<u8>,
    needle_folded: Vec<char>,
    case_sensitive: bool,
}

impl LiteralMatcher {
    /// Creates a matcher for `query`. An empty needle never matches.
    pub fn new(query: &str, case_sensitive: bool) -> Self {
        LiteralMatcher {
            needle_bytes: query.as_bytes().to_vec(),
            needle_folded: fold_chars(query),
            case_sensitive,
        }
    }
}

/// Per-character Unicode simple case folding (C+S), matching the
/// trigram tokenizer's fold on the bundled SQLite.
fn fold_chars(text: &str) -> Vec<char> {
    text.chars()
        .flat_map(|c| c.case_fold_with(Variant::Simple, Locale::NonTurkic))
        .collect()
}

impl Matcher for LiteralMatcher {
    fn find(&self, text: &str) -> Vec<MatchSpan> {
        if self.needle_bytes.is_empty() {
            return Vec::new();
        }
        if self.case_sensitive {
            return find_subslice(text.as_bytes(), &self.needle_bytes);
        }
        // Fold both sides per character. The mapping machinery stays
        // general even though simple folding is one-to-one: it is
        // cheap, and switching to a full fold later would still work.
        let (folded, orig_of_folded, byte_of_char) = fold_text(text);
        find_char_spans(&folded, &self.needle_folded)
            .into_iter()
            .map(|(fs, fe)| {
                let o_start = orig_of_folded[fs];
                let o_last = orig_of_folded[fe - 1];
                let start = byte_of_char[o_start];
                let end = byte_of_char.get(o_last + 1).copied().unwrap_or(text.len());
                MatchSpan { start, end }
            })
            .collect()
    }
}

/// Folds `text` character by character, returning the folded
/// characters, a map folded-char-index → original char-index, and a
/// map original char-index → byte offset.
fn fold_text(text: &str) -> (Vec<char>, Vec<usize>, Vec<usize>) {
    let mut folded = Vec::new();
    let mut orig_of_folded = Vec::new();
    let mut byte_of_char = Vec::new();
    for (ci, (byte_idx, c)) in text.char_indices().enumerate() {
        byte_of_char.push(byte_idx);
        for lc in c.case_fold_with(Variant::Simple, Locale::NonTurkic) {
            folded.push(lc);
            orig_of_folded.push(ci);
        }
    }
    (folded, orig_of_folded, byte_of_char)
}

/// Left-to-right, non-overlapping substring search on characters.
fn find_char_spans(hay: &[char], needle: &[char]) -> Vec<(usize, usize)> {
    let n = needle.len();
    if n == 0 {
        return Vec::new();
    }
    let first = needle[0];
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + n <= hay.len() {
        match hay[pos..].iter().position(|&c| c == first) {
            Some(off) => {
                let start = pos + off;
                if start + n > hay.len() {
                    break;
                }
                if &hay[start..start + n] == needle {
                    out.push((start, start + n));
                    pos = start + n;
                } else {
                    pos = start + 1;
                }
            }
            None => break,
        }
    }
    out
}

/// Left-to-right, non-overlapping substring search (same semantics as
/// `str::match_indices`).
fn find_subslice(hay: &[u8], needle: &[u8]) -> Vec<MatchSpan> {
    let n = needle.len();
    let first = needle[0];
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + n <= hay.len() {
        match hay[pos..].iter().position(|&b| b == first) {
            Some(off) => {
                let start = pos + off;
                if start + n > hay.len() {
                    break;
                }
                if &hay[start..start + n] == needle {
                    out.push(MatchSpan {
                        start,
                        end: start + n,
                    });
                    pos = start + n;
                } else {
                    pos = start + 1;
                }
            }
            None => break,
        }
    }
    out
}

/// What happened to one candidate during verification.
pub(crate) enum VerifyOutcome {
    /// The real content was read and decoded. `occurrences` may still
    /// be empty: trigram FTS candidates are not guaranteed matches —
    /// filtering false positives is the point of this step.
    Verified(Verified),
    /// The file or archive disappeared or changed since the snapshot.
    /// Silently dropped; counted separately from real errors.
    Stale,
    /// The candidate is still present and unchanged but could not be
    /// read or decoded.
    Failed,
}

/// A successfully read-and-decoded candidate.
pub(crate) struct Verified {
    /// All exact occurrences found in the real content.
    pub occurrences: Vec<Occurrence>,
    /// A too-large document was only read up to the safety cap; any
    /// matches beyond the cap are not covered.
    pub truncated: bool,
}

/// Verifies one candidate document against its real content.
pub(crate) fn verify_document(
    doc: &DocumentRef,
    matcher: &dyn Matcher,
    options: &SearchOptions,
    fallback: Option<EncodingKind>,
    verify_cap: u64,
) -> VerifyOutcome {
    match &doc.entry_path {
        None => verify_file(doc, matcher, options, fallback, verify_cap),
        Some(entry) => verify_archive_entry(doc, entry, matcher, options, fallback),
    }
}

/// Regular-file candidates: staleness check on size+mtime, bounded
/// read, decode, match.
fn verify_file(
    doc: &DocumentRef,
    matcher: &dyn Matcher,
    options: &SearchOptions,
    fallback: Option<EncodingKind>,
    verify_cap: u64,
) -> VerifyOutcome {
    let md = match crate::longpath::symlink_metadata(&doc.file_path) {
        Ok(m) => m,
        Err(_) => return VerifyOutcome::Stale,
    };
    if md.len() != doc.size || mtime_mismatch(&md, doc.mtime) {
        return VerifyOutcome::Stale;
    }
    let file = match crate::longpath::open(&doc.file_path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return VerifyOutcome::Stale,
        Err(_) => return VerifyOutcome::Failed,
    };

    // Too-large documents are verified under a read cap of the same
    // order of magnitude as `max_indexed_file_size`; everything else is
    // read in full (indexed documents are never bigger than that limit
    // by construction).
    let truncated = doc.status == STATUS_TOO_LARGE && doc.size > verify_cap;
    let limit = if truncated {
        verify_cap
    } else {
        doc.size.saturating_add(1)
    };
    let mut bytes = Vec::new();
    if file.take(limit).read_to_end(&mut bytes).is_err() {
        return VerifyOutcome::Failed;
    }
    if truncated {
        trim_incomplete_tail(&mut bytes);
    }
    verify_decoded(&bytes, matcher, options, fallback, truncated)
}

/// Archive-entry candidates: the archive file itself must be unchanged
/// (mtime — `doc.size` records the *entry* size, not the archive's),
/// then the `outer.zip!/inner.zip!/x.xml` chain is walked in memory.
fn verify_archive_entry(
    doc: &DocumentRef,
    entry_path: &str,
    matcher: &dyn Matcher,
    options: &SearchOptions,
    fallback: Option<EncodingKind>,
) -> VerifyOutcome {
    let md = match crate::longpath::symlink_metadata(&doc.file_path) {
        Ok(m) => m,
        Err(_) => return VerifyOutcome::Stale,
    };
    if mtime_mismatch(&md, doc.mtime) {
        return VerifyOutcome::Stale;
    }
    let file = match crate::longpath::open(&doc.file_path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return VerifyOutcome::Stale,
        Err(_) => return VerifyOutcome::Failed,
    };
    let bytes = match read_entry_chain(file, entry_path, doc.size) {
        Ok(b) => b,
        Err(outcome) => return outcome,
    };
    verify_decoded(&bytes, matcher, options, fallback, false)
}

/// Walks an `a.zip!/b.zip!/entry` chain fully in memory. The level-0
/// archive is the file on disk; each intermediate segment is the name
/// of a nested archive inside the previous one. The last segment is
/// the content entry. An entry that cannot be found means the archive
/// content no longer matches the snapshot (stale).
///
/// Note: `!/` inside a single entry name cannot be distinguished from
/// the level separator — such names are unresolvable, a documented
/// limitation identical to the build side's storage format.
fn read_entry_chain(
    file: std::fs::File,
    entry_path: &str,
    expected_size: u64,
) -> Result<Vec<u8>, VerifyOutcome> {
    let parts: Vec<&str> = entry_path.split("!/").collect();
    let mut archive: ZipArchive<Box<dyn ReadSeek>> =
        match ZipArchive::new(Box::new(BufReader::new(file)) as Box<dyn ReadSeek>) {
            Ok(a) => a,
            Err(_) => return Err(VerifyOutcome::Failed),
        };
    for (i, name) in parts.iter().enumerate() {
        let last = i + 1 == parts.len();
        // The final entry is bounded by its recorded size (+1 to
        // detect unexpected growth); intermediate archives get the
        // hard safety bound.
        let limit = if last {
            expected_size.saturating_add(1)
        } else {
            NESTED_ARCHIVE_READ_LIMIT
        };
        let mut buf = Vec::new();
        {
            let mut entry = match archive.by_name(name) {
                Ok(e) => e,
                Err(_) => return Err(VerifyOutcome::Stale),
            };
            if entry.by_ref().take(limit).read_to_end(&mut buf).is_err() {
                return Err(VerifyOutcome::Failed);
            }
        }
        if last {
            // The archive mtime matched, so the entry should have its
            // recorded size; anything larger means the content moved
            // under a coarse mtime — treat it as stale.
            if buf.len() as u64 > expected_size {
                return Err(VerifyOutcome::Stale);
            }
            return Ok(buf);
        }
        archive = match ZipArchive::new(Box::new(Cursor::new(buf)) as Box<dyn ReadSeek>) {
            Ok(a) => a,
            Err(_) => return Err(VerifyOutcome::Failed),
        };
    }
    // `entry_path` always has at least one segment.
    unreachable!("entry chain has at least one segment")
}

trait ReadSeek: Read + Seek {}
impl<T: Read + Seek> ReadSeek for T {}

/// Decodes then extracts occurrences for one candidate.
fn verify_decoded(
    bytes: &[u8],
    matcher: &dyn Matcher,
    options: &SearchOptions,
    fallback: Option<EncodingKind>,
    truncated: bool,
) -> VerifyOutcome {
    match decoder::decode_bytes(bytes, fallback) {
        Ok(decoded) => VerifyOutcome::Verified(Verified {
            occurrences: verify_text(
                &decoded.text,
                matcher,
                options.whole_word,
                options.context_lines,
            ),
            truncated,
        }),
        Err(_) => VerifyOutcome::Failed,
    }
}

/// Extracts all occurrences of `matcher` in decoded `text`, with
/// line/column and context. `whole_word` is applied here, uniformly,
/// so it also works for a future non-literal matcher.
pub(crate) fn verify_text(
    text: &str,
    matcher: &dyn Matcher,
    whole_word: bool,
    context_lines: usize,
) -> Vec<Occurrence> {
    let spans = matcher.find(text);
    if spans.is_empty() {
        return Vec::new();
    }
    let starts = line_starts(text);
    spans
        .iter()
        .filter(|&&sp| !whole_word || is_whole_word(text, sp))
        .map(|sp| occurrence_at(text, &starts, *sp, context_lines))
        .collect()
}

/// Word characters for the whole-word option: Unicode alphanumeric or
/// `_`. Anything else (whitespace, punctuation, file boundaries) is a
/// word boundary.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn is_whole_word(text: &str, span: MatchSpan) -> bool {
    let before_ok = text[..span.start]
        .chars()
        .next_back()
        .is_none_or(|c| !is_word_char(c));
    let after_ok = text[span.end..]
        .chars()
        .next()
        .is_none_or(|c| !is_word_char(c));
    before_ok && after_ok
}

/// Byte offsets of the start of every line (`\n`-separated).
fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    starts
}

/// Content bounds of line `i`: the newline(s) are stripped from the
/// end (`\n` and the `\r` of a `\r\n` pair).
fn line_bounds(text: &str, starts: &[usize], i: usize) -> (usize, usize) {
    let start = starts[i];
    let mut end = starts.get(i + 1).copied().unwrap_or(text.len());
    while end > start && matches!(text.as_bytes()[end - 1], b'\n' | b'\r') {
        end -= 1;
    }
    (start, end)
}

fn line_index(starts: &[usize], offset: usize) -> usize {
    starts.partition_point(|&s| s <= offset) - 1
}

fn occurrence_at(
    text: &str,
    starts: &[usize],
    span: MatchSpan,
    context_lines: usize,
) -> Occurrence {
    let li = line_index(starts, span.start);
    let line_text_of = |i: usize| {
        let (s, e) = line_bounds(text, starts, i);
        text[s..e].to_string()
    };
    let (line_start, line_end) = line_bounds(text, starts, li);
    Occurrence {
        line: li + 1,
        // 1-indexed character column (not bytes): what a user expects
        // to see in an editor.
        column: text[line_start..span.start].chars().count() + 1,
        line_text: text[line_start..line_end].to_string(),
        context_before: (li.saturating_sub(context_lines)..li)
            .map(line_text_of)
            .collect(),
        context_after: (li + 1..(li + 1 + context_lines).min(starts.len()))
            .map(line_text_of)
            .collect(),
    }
}

/// After a capped read, drops a trailing partial multi-byte sequence so
/// strict decoding cannot fail on the truncation boundary itself:
/// an incomplete UTF-8 sequence (`error_len() == None` means "input
/// ended mid-sequence"), or one byte of a UTF-16 code unit.
fn trim_incomplete_tail(bytes: &mut Vec<u8>) {
    match decoder::detect_bom(bytes) {
        BomKind::Utf16Le | BomKind::Utf16Be => bytes.truncate(bytes.len() & !1),
        _ => {
            if let Err(e) = std::str::from_utf8(bytes) {
                if e.error_len().is_none() {
                    bytes.truncate(e.valid_up_to());
                }
            }
        }
    }
}

fn file_mtime(md: &std::fs::Metadata) -> Option<i64> {
    md.modified().ok().and_then(systemtime_to_nanos)
}

/// A document is stale only on *evidence* of change. When the snapshot
/// recorded no mtime (`None` — nested archive entries on indexes built
/// before mtime propagation), there is nothing to compare: the
/// candidate is verified anyway since results come from real content.
fn mtime_mismatch(md: &std::fs::Metadata, recorded: Option<i64>) -> bool {
    match recorded {
        Some(mtime) => file_mtime(md) != Some(mtime),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn literal(query: &str, case_sensitive: bool) -> LiteralMatcher {
        LiteralMatcher::new(query, case_sensitive)
    }

    #[test]
    fn literal_matcher_finds_all_non_overlapping_spans() {
        let m = literal("needle", true);
        let spans = m.find("needle and needle again");
        assert_eq!(
            spans,
            vec![
                MatchSpan { start: 0, end: 6 },
                MatchSpan { start: 11, end: 17 }
            ]
        );
        // Non-overlapping semantics: "aa" in "aaa" matches once.
        let m = literal("aa", true);
        assert_eq!(m.find("aaa").len(), 1);
    }

    #[test]
    fn empty_needle_never_matches() {
        assert!(literal("", true).find("text").is_empty());
        assert!(literal("", false).find("text").is_empty());
    }

    #[test]
    fn case_insensitive_fold_matches_index_semantics() {
        // Unicode simple case folding (C+S): accented letters fold,
        // final sigma and long s fold, but ß/İ/ﬁ have no simple
        // mapping and stay unchanged — identical to the trigram
        // tokenizer measured on the bundled SQLite.
        let m = literal("mixed", false);
        assert_eq!(m.find("MiXeD").len(), 1);
        assert_eq!(m.find("MIXED").len(), 1);
        let m = literal("été", false);
        assert_eq!(m.find("ÉTÉ").len(), 1);
        let m = literal("café", false);
        assert_eq!(m.find("CAFÉ").len(), 1);
        let m = literal("χαοσ", false);
        assert_eq!(m.find("ΧΑΟΣ").len(), 1, "sigma folds");
        let m = literal("χαος", false);
        assert_eq!(m.find("ΧΑΟΣ").len(), 1, "final sigma also folds to σ");
        let m = literal("χάος", false);
        assert_eq!(m.find("ΧΑΟΣ").len(), 0, "accented alpha stays accented");
        let m = literal("congress", false);
        assert_eq!(m.find("congreſs").len(), 1, "long s folds to s");
        let m = literal("ss", false);
        assert_eq!(m.find("ß").len(), 0, "ß does not unfold to ss");
        let m = literal("istanbul", false);
        assert_eq!(m.find("İSTANBUL").len(), 0, "İ has no simple fold");
        let m = literal("İstanbul", false);
        assert_eq!(m.find("İSTANBUL").len(), 1);
        let m = literal("file", false);
        assert_eq!(m.find("ﬁle").len(), 0, "ligature ﬁ is not 'fi'");
        // Folded positions map back to correct byte offsets.
        let m = literal("été", false);
        let spans = m.find("xÉTÉy");
        assert_eq!(spans.len(), 1);
        assert_eq!(&"xÉTÉy"[spans[0].start..spans[0].end], "ÉTÉ");
    }

    #[test]
    fn whole_word_boundaries() {
        let m = literal("needle", true);
        let occ = |text: &str| verify_text(text, &m, true, 0).len();
        assert_eq!(occ("needle"), 1);
        assert_eq!(occ("needle needless"), 1);
        assert_eq!(occ("needless needle"), 1);
        assert_eq!(occ("a needle."), 1);
        assert_eq!(occ("needle2"), 0, "digit is a word char");
        assert_eq!(occ("needle_x"), 0, "underscore is a word char");
        assert_eq!(occ("needle_x needle"), 1);
        assert_eq!(occ("été needle café"), 1);
        assert_eq!(occ("日本needle語"), 0, "CJK letters are word chars");
    }

    #[test]
    fn occurrences_report_line_column_and_context() {
        let text = "l1\nl2 needle mid\nl3\nl4 needle2 needle\nl5";
        let m = literal("needle", true);
        let occ = verify_text(text, &m, false, 1);
        assert_eq!(occ.len(), 3);
        assert_eq!(occ[0].line, 2);
        assert_eq!(occ[0].column, 4);
        assert_eq!(occ[0].line_text, "l2 needle mid");
        assert_eq!(occ[0].context_before, vec!["l1".to_string()]);
        assert_eq!(occ[0].context_after, vec!["l3".to_string()]);
        assert_eq!(occ[2].line, 4);
        assert_eq!(occ[2].column, 12);
    }

    #[test]
    fn occurrence_context_is_bounded_at_file_edges() {
        let text = "needle first\nsecond";
        let m = literal("needle", true);
        let occ = verify_text(text, &m, false, 2);
        assert_eq!(occ.len(), 1);
        assert!(occ[0].context_before.is_empty());
        assert_eq!(occ[0].context_after, vec!["second".to_string()]);
    }

    #[test]
    fn crlf_lines_are_handled() {
        let text = "a\r\nb needle\r\nc";
        let m = literal("needle", true);
        let occ = verify_text(text, &m, false, 0);
        assert_eq!(occ.len(), 1);
        assert_eq!(occ[0].line, 2);
        assert_eq!(occ[0].line_text, "b needle");
    }

    #[test]
    fn unicode_columns_count_characters() {
        let text = "日本語 needle";
        let m = literal("needle", true);
        let occ = verify_text(text, &m, false, 0);
        assert_eq!(occ[0].column, 5); // 3 chars + space + 1
    }

    #[test]
    fn trim_incomplete_tail_fixes_utf8_and_utf16_cuts() {
        let mut utf8 = "abé".as_bytes().to_vec();
        utf8.truncate(3); // cuts the 2-byte é
        trim_incomplete_tail(&mut utf8);
        assert_eq!(utf8, b"ab");

        let mut utf8_invalid_mid = vec![b'a', 0xFF, b'b'];
        trim_incomplete_tail(&mut utf8_invalid_mid);
        assert_eq!(
            utf8_invalid_mid.len(),
            3,
            "genuinely invalid content is kept"
        );

        let mut utf16 = vec![0xFF, 0xFE, b'a', 0x00, b'b', 0x00, b'c'];
        trim_incomplete_tail(&mut utf16);
        assert_eq!(utf16.len(), 6);
    }
}
