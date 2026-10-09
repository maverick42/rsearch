//! SQLite/FTS5 correctness tests against real built indexes.
//!
//! These tests prove that the bundled SQLite supports FTS5 with the
//! trigram tokenizer, that contentless tables work, that the document
//! ID / FTS rowid mapping holds, and — most importantly — that valid
//! substrings of length >= 3 are never lost as candidates.

use crate::common::*;
use rsearch_engine::fts::escape_fts_phrase;

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
fn bundled_sqlite_reports_version() {
    let version = rsearch_engine::db::bundled_sqlite_version();
    let major: u32 = version.split('.').next().unwrap().parse().unwrap();
    assert!(major >= 3, "unexpected SQLite version {version}");
}

#[test]
fn fts5_trigram_and_contentless_tables_work() {
    let (dir, conn) = build_and_open(
        "fts-support",
        &[("probe.txt", "the quick brown fox jumps over the lazy dog")],
    );
    let _ = dir;
    // The fts table exists as a contentless FTS5 table.
    let sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name = 'fts'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(sql.contains("fts5"), "fts table must be FTS5: {sql}");
    assert!(
        sql.contains("trigram"),
        "fts table must use the trigram tokenizer: {sql}"
    );
    assert!(
        sql.contains("content=''") || sql.contains("content = ''"),
        "fts table must be contentless: {sql}"
    );
    assert!(
        sql.contains("case_sensitive 0"),
        "fts must be case-insensitive: {sql}"
    );
    // Trigram queries work.
    assert_eq!(fts_match(&conn, "quick brown").len(), 1);
}

#[test]
fn document_id_is_the_fts_rowid() {
    let (dir, conn) = build_and_open(
        "rowid-map",
        &[
            ("a.txt", "alpha document content"),
            ("b.txt", "beta document content"),
            ("c.txt", "gamma document content"),
        ],
    );
    let _ = dir;
    let mut stmt = conn
        .prepare(
            "SELECT d.id FROM documents d JOIN fts ON fts.rowid = d.id
             WHERE fts MATCH '\"beta document\"'",
        )
        .unwrap();
    let ids: Vec<i64> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();
    assert_eq!(
        ids.len(),
        1,
        "exactly one document must join with its FTS row"
    );
}

#[test]
fn ascii_substrings_are_discoverable() {
    let (dir, conn) = build_and_open("ascii", &[("a.txt", "hello searchable world")]);
    let _ = dir;
    for needle in [
        "hello",
        "searchable",
        "world",
        "hello searchable",
        "lo se",
        "rld",
        "abc",
    ] {
        let expected = if needle == "abc" { 0 } else { 1 };
        assert_eq!(fts_match(&conn, needle).len(), expected, "needle: {needle}");
    }
}

#[test]
fn exactly_three_character_substrings_match() {
    let (dir, conn) = build_and_open("three", &[("a.txt", "abc def ghi jkl")]);
    let _ = dir;
    assert_eq!(fts_match(&conn, "abc").len(), 1);
    assert_eq!(fts_match(&conn, "def").len(), 1);
    assert_eq!(fts_match(&conn, "ghi").len(), 1);
    assert_eq!(fts_match(&conn, "jkl").len(), 1);
    // Non-present trigrams do not match.
    assert_eq!(fts_match(&conn, "xyz").len(), 0);
}

#[test]
fn substrings_shorter_than_three_do_not_match() {
    let (dir, conn) = build_and_open("short", &[("a.txt", "ab cd")]);
    let _ = dir;
    // The trigram tokenizer cannot answer these; the future search layer
    // must verify such strings directly against files.
    assert_eq!(fts_match(&conn, "ab").len(), 0);
    assert_eq!(fts_match(&conn, "a").len(), 0);
}

#[test]
fn unicode_substrings_are_discoverable() {
    let (dir, conn) = build_and_open(
        "unicode-fts",
        &[("u.txt", "héllo wörld 日本語のテキスト café")],
    );
    let _ = dir;
    assert_eq!(fts_match(&conn, "héllo").len(), 1);
    assert_eq!(fts_match(&conn, "wörld").len(), 1);
    assert_eq!(fts_match(&conn, "日本語").len(), 1);
    assert_eq!(fts_match(&conn, "のテキスト").len(), 1);
    assert_eq!(fts_match(&conn, "café").len(), 1);
    assert_eq!(fts_match(&conn, "日本語の").len(), 1);
}

#[test]
fn case_insensitive_matching() {
    let (dir, conn) = build_and_open("case", &[("a.txt", "MiXeD CaSe CoNtEnT")]);
    let _ = dir;
    assert_eq!(fts_match(&conn, "mixed case").len(), 1);
    assert_eq!(fts_match(&conn, "MIXED CASE").len(), 1);
    assert_eq!(fts_match(&conn, "Mixed Case").len(), 1);
    assert_eq!(fts_match(&conn, "cOnTeNt").len(), 1);
}

#[test]
fn punctuation_and_spaces_in_substrings() {
    let (dir, conn) = build_and_open(
        "punct",
        &[("p.txt", "foo.bar baz,qux;quux (paren) [bracket] {brace}")],
    );
    let _ = dir;
    assert_eq!(fts_match(&conn, "foo.bar").len(), 1);
    assert_eq!(fts_match(&conn, "baz,qux").len(), 1);
    assert_eq!(fts_match(&conn, "qux;quux").len(), 1);
    assert_eq!(fts_match(&conn, "(paren)").len(), 1);
    assert_eq!(fts_match(&conn, "[bracket]").len(), 1);
    assert_eq!(fts_match(&conn, "r baz,qu").len(), 1);
}

#[test]
fn quoted_and_escaped_content_is_discoverable() {
    let (dir, conn) = build_and_open(
        "quotes",
        &[("q.txt", "she said \"hello\" and 'goodbye' to him")],
    );
    let _ = dir;
    assert_eq!(fts_match(&conn, "\"hello\"").len(), 1);
    assert_eq!(fts_match(&conn, "'goodbye'").len(), 1);
    assert_eq!(fts_match(&conn, "said \"hello\" and").len(), 1);
    // The escaping helper keeps user input literal.
    assert_eq!(escape_fts_phrase("a\"b"), "\"a\"\"b\"");
}

#[test]
fn backslash_newline_and_apostrophe_substrings_are_discoverable() {
    let content = "prefix C:\\Users\\name middle line\nbreak end 'quoted' tail";
    let (dir, conn) = build_and_open("tricky", &[("t.txt", content)]);
    let _ = dir;
    // Substring containing a backslash (from C:\Users\name).
    assert_eq!(
        fts_match(&conn, "s\\name").len(),
        1,
        "backslash in substring"
    );
    assert_eq!(fts_match(&conn, "C:\\User").len(), 1);
    // Substring containing a real newline (line\nbreak).
    assert_eq!(
        fts_match(&conn, "line\nbreak").len(),
        1,
        "newline in substring"
    );
    assert_eq!(fts_match(&conn, "e\nbr").len(), 1);
    // Substring containing apostrophes.
    assert_eq!(fts_match(&conn, "'quoted'").len(), 1);
    assert_eq!(fts_match(&conn, "d 'quo").len(), 1);
}

#[test]
fn repeated_substrings_and_longer_queries() {
    let (dir, conn) = build_and_open(
        "repeat",
        &[("r.txt", "abababababab the whole content is repetitive")],
    );
    let _ = dir;
    assert_eq!(fts_match(&conn, "abab").len(), 1);
    assert_eq!(fts_match(&conn, "babab").len(), 1);
    assert_eq!(fts_match(&conn, "the whole content is repetitive").len(), 1);
    // A substring spanning repeated regions.
    assert_eq!(fts_match(&conn, "abab the whole").len(), 1);
}

#[test]
fn mixed_unicode_and_ascii_substrings() {
    let (dir, conn) = build_and_open(
        "mixed",
        &[("m.txt", "ASCII prefix 日本語 mixed middle ASCII suffix")],
    );
    let _ = dir;
    assert_eq!(fts_match(&conn, "prefix 日本").len(), 1);
    assert_eq!(fts_match(&conn, "語 mixed").len(), 1);
    assert_eq!(fts_match(&conn, "middle ASC").len(), 1);
}

#[test]
fn candidate_retrieval_returns_document_ids_for_verification() {
    let (dir, conn) = build_and_open(
        "candidates",
        &[
            ("one.txt", "the needle is in this file"),
            ("two.txt", "nothing relevant here"),
            ("three.txt", "also contains the needle again"),
        ],
    );
    let _ = dir;
    let rowids = fts_match(&conn, "the needle");
    assert_eq!(rowids.len(), 2);
    let mut stmt = conn
        .prepare("SELECT file_path FROM documents WHERE id = ?1")
        .unwrap();
    for rowid in rowids {
        let path: String = stmt.query_row([rowid], |r| r.get(0)).unwrap();
        assert!(path.ends_with("one.txt") || path.ends_with("three.txt"));
    }
}

#[test]
fn rebuild_creates_fresh_index_without_old_documents() {
    let dir = TempDir::new("rebuild");
    dir.write("old.txt", "old content that should disappear");
    build_ok(&dir, opts_for(dir.path()));

    std::fs::remove_file(dir.join("old.txt")).unwrap();
    dir.write("new.txt", "new content that should be found");
    let report = build_ok(&dir, opts_for(dir.path()));
    assert_eq!(report.counters.files_indexed, 1);

    let conn = open_index(&dir);
    assert_eq!(fts_match(&conn, "should disappear").len(), 0);
    assert_eq!(fts_match(&conn, "should be found").len(), 1);
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1, "rebuild must replace, not merge");
}
