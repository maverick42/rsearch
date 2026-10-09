//! Query construction and document-listing tests for `search`.
//!
//! Escaping coverage for quotes/parens/backslash/newline/apostrophes
//! already exists in `sqlite_fts.rs`; this file adds the characters and
//! operator-looking tokens that would corrupt a `MATCH` expression if
//! user text ever leaked into it unescaped, plus `iter_documents`
//! status filtering and the new `meta` keys the search layer relies on.

use crate::common::*;
use rsearch_engine::search::{self, SearchError};

/// Builds an index from `(name, content)` files and opens it.
fn build_and_open(label: &str, files: &[(&str, &str)]) -> (TempDir, rusqlite::Connection) {
    let dir = TempDir::new(label);
    for (name, content) in files {
        dir.write(name, content);
    }
    build_ok(&dir, opts_for(dir.path()));
    let conn = open_index(&dir);
    (dir, conn)
}

#[test]
fn short_queries_are_rejected_with_displayable_message() {
    for q in ["", "a", "ab"] {
        let err = search::validate_query(q).unwrap_err();
        assert!(matches!(err, SearchError::QueryTooShort));
        let msg = err.to_string();
        assert!(
            msg.contains("3"),
            "error message must state the minimum: {msg}"
        );
    }
    assert!(search::validate_query("abc").is_ok());
    // Multi-byte characters count as characters, not bytes.
    assert!(search::validate_query("日本語").is_ok());
    assert!(search::validate_query("日本").is_err());
}

#[test]
fn colon_and_operator_tokens_stay_literal() {
    // FTS5 syntax has column filters (`col:text`), AND/OR/NOT operators
    // and NEAR groups; inside a quoted phrase all of it is literal.
    let (dir, conn) = build_and_open(
        "specials",
        &[
            ("c.txt", "key:value pair a:b here"),
            ("o.txt", "the AND of OR NOT NEAR words"),
            ("s.txt", "star* plus+ minus- caret^ tilde~"),
        ],
    );
    let _ = dir;
    for needle in [
        "key:value",
        "y:val",
        "AND of",
        "OR NOT",
        "NEAR words",
        "star* pl",
        "minus- ca",
        "tilde~",
    ] {
        assert_eq!(
            fts_match(&conn, needle).len(),
            1,
            "needle {needle:?} must produce a valid literal MATCH"
        );
    }
}

#[test]
fn phrase_escaping_goes_through_single_implementation() {
    assert_eq!(
        search::to_fts5_phrase("a\"b:c"),
        rsearch_engine::fts::escape_fts_phrase("a\"b:c")
    );
}

#[test]
fn iter_documents_filters_by_status() {
    let dir = TempDir::new("iter-docs");
    dir.write("ok.txt", "small ok"); // <= 16 bytes, stays indexed
    dir.write_bytes("bad.txt", b"bad \xC0\xAF"); // <= 16 bytes, decode error
    dir.write("big.txt", &"x".repeat(128));

    let mut opts = opts_for(dir.path());
    opts.max_indexed_file_size = 16; // big.txt -> status 2
    let report = build_ok(&dir, opts);
    assert_eq!(report.counters.files_indexed, 1);
    assert_eq!(report.counters.files_too_large, 1);
    assert_eq!(report.total_errors, 1);

    let index = dir.index_path();
    let indexed = search::iter_documents(&index, &[rsearch_engine::STATUS_INDEXED]).unwrap();
    assert_eq!(indexed.len(), 1);
    assert!(indexed[0].file_path.ends_with("ok.txt"));
    assert_eq!(indexed[0].status, 0);
    assert!(indexed[0].entry_path.is_none());

    let too_large = search::iter_documents(&index, &[rsearch_engine::STATUS_TOO_LARGE]).unwrap();
    assert_eq!(too_large.len(), 1);
    assert!(too_large[0].file_path.ends_with("big.txt"));
    assert_eq!(too_large[0].size, 128);

    let errored = search::iter_documents(&index, &[rsearch_engine::STATUS_ERROR]).unwrap();
    assert_eq!(errored.len(), 1);
    assert!(errored[0].file_path.ends_with("bad.txt"));

    let union = search::iter_documents(
        &index,
        &[
            rsearch_engine::STATUS_INDEXED,
            rsearch_engine::STATUS_TOO_LARGE,
        ],
    )
    .unwrap();
    assert_eq!(union.len(), 2);

    let all = search::iter_documents(&index, &[]).unwrap();
    assert_eq!(all.len(), 3, "empty status filter selects every document");
}

#[test]
fn iter_documents_reports_missing_and_invalid_index() {
    let dir = TempDir::new("iter-missing");
    let err = search::iter_documents(&dir.index_path(), &[]).unwrap_err();
    assert!(
        matches!(err, rsearch_engine::IndexError::NotFound),
        "missing index must report NotFound, got {err:?}"
    );

    // A non-index file is rejected by validation, not by a crash.
    let bogus = dir.index_path();
    std::fs::write(&bogus, b"this is not sqlite").unwrap();
    let err = search::iter_documents(&bogus, &[]).unwrap_err();
    assert!(
        !matches!(err, rsearch_engine::IndexError::NotFound),
        "existing non-index file must fail validation, got {err:?}"
    );
}

#[test]
fn meta_records_fallback_encoding_and_size_limit() {
    let dir = TempDir::new("meta-keys");
    dir.write("a.txt", "content");
    let mut opts = opts_for(dir.path());
    opts.fallback_encoding = Some(rsearch_engine::EncodingKind::Windows1252);
    opts.max_indexed_file_size = 4096;
    build_ok(&dir, opts);

    let conn = open_index(&dir);
    let meta = |key: &str| -> String {
        conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .unwrap_or_else(|_| panic!("meta key {key} must exist"))
    };
    assert_eq!(meta("fallback_encoding"), "windows1252");
    assert_eq!(meta("max_indexed_file_size"), "4096");
}
