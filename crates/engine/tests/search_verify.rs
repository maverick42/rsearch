//! End-to-end `search()` tests: candidate assembly plus real-content
//! verification on files and archives.

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use common::*;
use rsearch_engine::search::{self, SearchOptions};

/// A never-cancelled token for searches that must run to completion.
fn never_cancel() -> AtomicBool {
    AtomicBool::new(false)
}

fn search_ok(index: &Path, needle: &str) -> rsearch_engine::SearchReport {
    search::search(index, needle, &SearchOptions::default(), &never_cancel())
        .expect("search must succeed")
}

/// Serializes a ZIP archive to bytes (for nested-archive fixtures).
fn zip_bytes<S: AsRef<str>>(entries: Vec<(S, &[u8])>) -> Vec<u8> {
    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut cursor);
        let options: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, bytes) in &entries {
            zip.start_file(name.as_ref(), options).expect("start entry");
            std::io::Write::write_all(&mut zip, bytes).expect("write entry");
        }
        zip.finish().expect("finish zip");
    }
    cursor.into_inner()
}

fn result_for<'a>(
    report: &'a rsearch_engine::SearchReport,
    name: &str,
) -> Option<&'a rsearch_engine::FileResult> {
    report
        .results
        .iter()
        .find(|r| r.file_path.ends_with(name) && r.entry_path.is_none())
}

#[test]
fn literal_search_finds_positions_and_context() {
    let dir = TempDir::new("search-basic");
    dir.write(
        "a.txt",
        "l1 alpha\nl2 needle mid\nl3\nl4 needle2 needle\nl5 tail",
    );
    build_ok(&dir, opts_for(dir.path()));

    let report = search_ok(&dir.index_path(), "needle");
    assert_eq!(report.candidates_from_index, 1);
    assert_eq!(report.candidates_too_large, 0);
    assert_eq!(report.results.len(), 1);
    let occ = &report.results[0].occurrences;
    assert_eq!(occ.len(), 3);
    assert_eq!((occ[0].line, occ[0].column), (2, 4));
    assert_eq!(occ[0].line_text, "l2 needle mid");
    assert_eq!(occ[0].context_before, vec!["l1 alpha".to_string()]);
    assert_eq!(
        occ[0].context_after,
        vec!["l3".to_string(), "l4 needle2 needle".to_string()],
        "default context is 2 lines"
    );
    assert_eq!((occ[2].line, occ[2].column), (4, 12));
}

#[test]
fn short_and_missing_queries_are_rejected() {
    let dir = TempDir::new("search-short");
    dir.write("a.txt", "some content");
    build_ok(&dir, opts_for(dir.path()));
    for q in ["", "a", "ab"] {
        assert!(matches!(
            search::search(
                &dir.index_path(),
                q,
                &SearchOptions::default(),
                &never_cancel()
            ),
            Err(search::SearchError::QueryTooShort)
        ));
    }
}

#[test]
fn case_options_behave() {
    let dir = TempDir::new("search-case");
    dir.write("a.txt", "MiXeD needle NEEDLE content");
    build_ok(&dir, opts_for(dir.path()));
    let index = dir.index_path();

    let ci = search_ok(&index, "needle");
    let total: usize = ci.results[0].occurrences.len();
    assert_eq!(total, 2, "case-insensitive finds needle and NEEDLE");

    let cs = search::search(
        &index,
        "needle",
        &SearchOptions {
            case_sensitive: true,
            ..SearchOptions::default()
        },
        &never_cancel(),
    )
    .unwrap();
    assert_eq!(cs.results[0].occurrences.len(), 1);
}

#[test]
fn trigram_index_folds_unicode_and_search_stays_consistent() {
    // Characterization test against the bundled SQLite (measured, not
    // assumed — see docs/decisions.md): the trigram tokenizer with
    // `case_sensitive 0` applies Unicode *simple* case folding (C+S).
    // Accented letters fold (é↔É, à↔À), final sigma and long s fold
    // (ς→σ, ſ→s), but ß stays ß, İ stays İ and ﬁ stays ﬁ (they only
    // have *full* fold mappings, which trigram does not apply). The
    // verifier uses the same simple fold, so `search` agrees with the
    // index on every one of these cases.
    let dir = TempDir::new("search-unicode-fold");
    dir.write("upper.txt", "ÉTÉ MAJUSCULE À BIENTÔT CAFÉ ŒUVRE");
    dir.write("mixed.txt", "MiXeD CaSe CoNtEnT");
    dir.write("sharp.txt", "STRASSE ß GROSSE İSTANBUL ﬁle");
    dir.write("fold.txt", "ΧΑΟΣ congreſs χαοσ");
    build_ok(&dir, opts_for(dir.path()));
    let index = dir.index_path();
    let conn = open_index(&dir);

    // FTS-level characterization on this exact SQLite version.
    assert_eq!(fts_match(&conn, "été").len(), 1, "é folds to É");
    assert_eq!(fts_match(&conn, "à bientôt").len(), 1);
    assert_eq!(fts_match(&conn, "café").len(), 1);
    assert_eq!(fts_match(&conn, "œuvre").len(), 1);
    assert_eq!(fts_match(&conn, "mixed case").len(), 1);
    assert_eq!(fts_match(&conn, "ß grosse").len(), 1, "ß stays ß");
    assert_eq!(fts_match(&conn, "ss grosse").len(), 0, "ß is not ss");
    assert_eq!(
        fts_match(&conn, "istanbul").len(),
        0,
        "İ has no simple fold"
    );
    assert_eq!(fts_match(&conn, "i\u{307}stanbul").len(), 0);
    assert_eq!(fts_match(&conn, "İstanbul").len(), 1);
    assert_eq!(fts_match(&conn, "file").len(), 0, "ﬁ ligature is not fi");
    assert_eq!(
        fts_match(&conn, "χαος").len(),
        1,
        "final sigma folds to sigma"
    );
    assert_eq!(
        fts_match(&conn, "χάος").len(),
        0,
        "an accented alpha does not fold to plain alpha"
    );
    assert_eq!(fts_match(&conn, "congress").len(), 1, "long s folds");

    // Search-level consistency on the same cases.
    for (needle, hits) in [
        ("été", 1),
        ("ÉTÉ", 1),
        ("à bientôt", 1),
        ("café", 1),
        ("œuvre", 1),
        ("mixed case", 1),
        ("ß grosse", 1),
        ("ss grosse", 0),
        ("istanbul", 0),
        ("i\u{307}stanbul", 0),
        ("İstanbul", 1),
        ("χαοσ", 1), // ΧΑΟΣ and χαοσ, same file
        ("χαος", 1), // ς folds to σ
        ("χάος", 0), // accented alpha stays accented
        ("congress", 1),
        ("congreſs", 1),
    ] {
        assert_eq!(
            search_ok(&index, needle).results.len(),
            hits,
            "search {needle:?} must agree with the index fold"
        );
    }
    // Case-sensitive search for the exact stored form always works.
    let cs = search::search(
        &index,
        "À BIENTÔT",
        &SearchOptions {
            case_sensitive: true,
            ..SearchOptions::default()
        },
        &never_cancel(),
    )
    .unwrap();
    assert_eq!(cs.results.len(), 1);
}

#[test]
fn too_large_documents_are_verified_with_safety_cap() {
    let dir = TempDir::new("search-toolarge");
    let mut content = String::from("head needle ");
    content.push_str(&"x".repeat(180));
    content.push_str(" tail-needle");
    dir.write("big.txt", &content); // ~200 bytes, status 2
    dir.write("small.txt", "needle here");
    let mut opts = opts_for(dir.path());
    opts.max_indexed_file_size = 64;
    build_ok(&dir, opts);

    let report = search_ok(&dir.index_path(), "needle");
    // big.txt is a candidate via the status-2 union even though FTS
    // has no row for it.
    assert_eq!(report.candidates_too_large, 1);
    assert_eq!(report.candidates_from_index, 1, "only small.txt via FTS");
    // The early needle is found; the one beyond the 64-byte cap is not
    // (documented cap behavior).
    let big = result_for(&report, "big.txt").expect("big.txt must yield a result");
    assert_eq!(big.occurrences.len(), 1);
    assert_eq!(big.occurrences[0].column, 6);
    assert_eq!(report.truncated_files, 1);
}

#[test]
fn error_and_limit_documents_are_counted_never_verified() {
    let dir = TempDir::new("search-unverifiable");
    dir.write_bytes("bad.txt", b"bad \xC0\xAF needle");
    dir.write("ok.txt", "needle ok");
    // A status-4 archive entry: entry larger than max_entry_size.
    make_zip(
        &dir.join("limited.zip"),
        vec![("huge.txt", vec![b'z'; 256])],
    );
    let mut opts = opts_for(dir.path());
    opts.archives.max_entry_size = 32;
    opts.fallback_encoding = None;
    build_ok(&dir, opts);

    let report = search_ok(&dir.index_path(), "needle");
    assert_eq!(report.skipped_unverifiable, 2, "status 3 + status 4");
    assert!(result_for(&report, "bad.txt").is_none());
    assert!(result_for(&report, "limited.zip").is_none());
    assert_eq!(result_for(&report, "ok.txt").unwrap().occurrences.len(), 1);
}

#[test]
fn stale_files_are_dropped_silently() {
    let dir = TempDir::new("search-stale");
    let gone = dir.write("gone.txt", "needle in a doomed file");
    let changed = dir.write("changed.txt", "needle in a file");
    dir.write("stable.txt", "needle stable");
    build_ok(&dir, opts_for(dir.path()));

    std::fs::remove_file(&gone).unwrap();
    std::fs::write(&changed, "needle but different content now").unwrap();

    let report = search_ok(&dir.index_path(), "needle");
    assert_eq!(report.skipped_stale, 2);
    assert_eq!(report.results.len(), 1);
    assert!(result_for(&report, "stable.txt").is_some());
}

#[test]
fn archive_entries_and_nested_entries_are_verified() {
    let dir = TempDir::new("search-archives");
    let inner = zip_bytes(vec![("deep.txt", &b"needle deep entry"[..])]);
    make_zip(
        &dir.join("outer.zip"),
        vec![
            ("docs/a.txt", b"needle in archive entry".to_vec()),
            ("inner.zip", inner),
        ],
    );
    dir.write("plain.txt", "needle in a plain file");
    build_ok(&dir, opts_for(dir.path()));

    let report = search_ok(&dir.index_path(), "needle");
    let entry = report
        .results
        .iter()
        .find(|r| r.entry_path.as_deref() == Some("docs/a.txt"))
        .expect("archive entry must be a result");
    assert!(entry.file_path.ends_with("outer.zip"));
    assert_eq!(entry.occurrences[0].line_text, "needle in archive entry");

    let nested = report
        .results
        .iter()
        .find(|r| r.entry_path.as_deref() == Some("inner.zip!/deep.txt"))
        .expect("nested archive entry must be a result");
    assert_eq!(nested.occurrences[0].line_text, "needle deep entry");

    assert!(result_for(&report, "plain.txt").is_some());
}

#[test]
fn stale_archive_is_dropped() {
    let dir = TempDir::new("search-stale-arc");
    make_zip(
        &dir.join("a.zip"),
        vec![("x.txt", b"needle inside".to_vec())],
    );
    build_ok(&dir, opts_for(dir.path()));
    std::fs::remove_file(dir.join("a.zip")).unwrap();

    let report = search_ok(&dir.index_path(), "needle");
    assert_eq!(report.skipped_stale, 1);
    assert!(report.results.is_empty());
}

#[test]
fn fts_hit_on_too_large_document_is_deduplicated() {
    let dir = TempDir::new("search-dedup");
    dir.write("big.txt", "needle plus enough bytes to be too large");
    let mut opts = opts_for(dir.path());
    opts.max_indexed_file_size = 16;
    build_ok(&dir, opts);

    // Force the impossible: an FTS row pointing at a status-2 document.
    let conn = open_index(&dir);
    let id: i64 = conn
        .query_row("SELECT id FROM documents WHERE status = 2", [], |r| {
            r.get(0)
        })
        .unwrap();
    conn.execute(
        "INSERT INTO fts(rowid, content) VALUES (?1, 'needle')",
        rusqlite::params![id],
    )
    .unwrap();
    drop(conn);

    let report = search_ok(&dir.index_path(), "needle");
    assert_eq!(
        report.candidates_from_index + report.candidates_too_large,
        1,
        "the same document must count once"
    );
    assert_eq!(report.results.len(), 1);
}

#[test]
fn extension_filter_scopes_everything() {
    let dir = TempDir::new("search-ext");
    dir.write("a.txt", "needle in txt");
    dir.write("b.log", "needle in log");
    dir.write_bytes("bad.log", b"bad \xC0\xAF needle");
    dir.write("noext", "needle without extension");
    build_ok(&dir, opts_for(dir.path()));

    let report = search::search(
        &dir.index_path(),
        "needle",
        &SearchOptions {
            extensions: Some(vec![".TXT".to_string()]), // normalization
            ..SearchOptions::default()
        },
        &never_cancel(),
    )
    .unwrap();
    assert_eq!(report.results.len(), 1);
    assert!(result_for(&report, "a.txt").is_some());
    assert_eq!(
        report.skipped_unverifiable, 0,
        "the bad .log is outside the extension scope"
    );
}

#[test]
fn whole_word_option() {
    let dir = TempDir::new("search-whole");
    dir.write("a.txt", "needle needless needle. needle2");
    build_ok(&dir, opts_for(dir.path()));

    let report = search::search(
        &dir.index_path(),
        "needle",
        &SearchOptions {
            whole_word: true,
            ..SearchOptions::default()
        },
        &never_cancel(),
    )
    .unwrap();
    assert_eq!(report.results[0].occurrences.len(), 2);
}

#[test]
fn decoding_parity_utf16_and_windows1252() {
    let dir = TempDir::new("search-parity");
    // UTF-16 LE with BOM.
    let mut utf16 = vec![0xFF, 0xFE];
    utf16.extend("needle utf16".encode_utf16().flat_map(u16::to_le_bytes));
    dir.write_bytes("u16.txt", &utf16);
    // Windows-1252 "café needle" (0xE9 = é).
    dir.write_bytes("latin.txt", b"caf\xe9 needle");
    let mut opts = opts_for(dir.path());
    opts.fallback_encoding = Some(rsearch_engine::EncodingKind::Windows1252);
    build_ok(&dir, opts);

    let report = search_ok(&dir.index_path(), "needle");
    assert!(result_for(&report, "u16.txt").is_some());
    let latin = result_for(&report, "latin.txt").expect("windows-1252 file must verify");
    assert_eq!(latin.occurrences[0].line_text, "café needle");
}

#[test]
fn every_real_substring_is_found_end_to_end() {
    // The fundamental property, through the whole pipeline: any
    // substring >= 3 chars of an indexed document must be found by
    // `search` — in plain files and in (nested) archive entries.
    let mut rng = Rng::new(0x5150_5EED_0000_0003);
    let dir = TempDir::new("search-property");
    let doc_count = 10;
    let mut docs = Vec::new();
    for i in 0..doc_count {
        let doc = random_document(&mut rng);
        dir.write(&format!("doc{i:02}.txt"), &doc);
        docs.push(doc);
    }
    let inner = zip_bytes(vec![("deep.txt", b"nested needle payload".as_slice())]);
    make_zip(
        &dir.join("corpus.zip"),
        vec![
            ("plain_entry.txt", b"archive needle payload".to_vec()),
            ("inner.zip", inner),
        ],
    );
    build_ok(&dir, opts_for(dir.path()));
    let index = dir.index_path();

    let case_sensitive = SearchOptions {
        case_sensitive: true,
        ..SearchOptions::default()
    };
    let mut checked = 0usize;
    for (i, doc) in docs.iter().enumerate() {
        let chars: Vec<char> = doc.chars().collect();
        if chars.len() < 4 {
            continue;
        }
        for _ in 0..25 {
            let start = rng.below(chars.len() - 3);
            let end = start + 3 + rng.below(chars.len() - start - 2);
            let needle: String = chars[start..end].iter().collect();
            let report = search::search(&index, &needle, &case_sensitive, &never_cancel()).unwrap();
            let found = report
                .results
                .iter()
                .any(|r| r.file_path.ends_with(format!("doc{i:02}.txt")));
            assert!(
                found,
                "substring {needle:?} of doc{i} must be found (report: \
                 {} results, {} stale)",
                report.results.len(),
                report.skipped_stale
            );
            checked += 1;
        }
    }
    assert!(checked > 100, "meaningful coverage: {checked} substrings");

    for (needle, entry) in [
        ("needle payload", "plain_entry.txt"),
        ("needle payload", "inner.zip!/deep.txt"),
        ("sted needle", "inner.zip!/deep.txt"),
    ] {
        let report = search_ok(&index, needle);
        assert!(
            report
                .results
                .iter()
                .any(|r| r.entry_path.as_deref() == Some(entry)),
            "needle {needle:?} must be found in {entry}"
        );
    }
}

/// Manual performance gate: candidate selection on a large real index
/// must stay in the millisecond range. Run with:
///   set RSEARCH_PERF_INDEX=C:\path\to\index.db
///   cargo test -p rsearch-engine --test search_verify -- --ignored
#[test]
#[ignore = "requires a real index (RSEARCH_PERF_INDEX)"]
fn candidate_selection_is_fast_on_a_real_index() {
    let index = PathBuf::from(
        std::env::var("RSEARCH_PERF_INDEX").expect("set RSEARCH_PERF_INDEX to a real index path"),
    );
    // A needle unlikely to appear keeps verification work near zero,
    // so elapsed is dominated by candidate selection.
    let report = search_ok(&index, "zzq_vendored_unlikely");
    assert!(
        report.elapsed.as_secs() < 1,
        "candidate selection took {:?}, regression suspected",
        report.elapsed
    );
}

// ---------------------------------------------------------------------------
// Archive grouping: one open per parent archive, ordering preserved
// ---------------------------------------------------------------------------

/// Many candidate entries spread over two archives plus a nested
/// archive and a plain file: every entry is verified exactly once, each
/// parent archive is opened once, and the emitted order still follows
/// the candidate order `(file_path, entry_path)`.
#[test]
fn archive_runs_open_each_parent_once_and_keep_order() {
    let dir = TempDir::new("search-group");
    let many: Vec<(String, Vec<u8>)> = (0..40)
        .map(|i| {
            (
                format!("e{i:03}.txt"),
                format!("needle entry {i}").into_bytes(),
            )
        })
        .collect();
    make_zip(&dir.join("a_many.zip"), many);
    let inner = zip_bytes(vec![
        ("deep1.txt", &b"needle nested one"[..]),
        ("deep2.txt", &b"needle nested two"[..]),
    ]);
    make_zip(
        &dir.join("b_nested.zip"),
        vec![("inner.zip", inner), ("top.txt", b"needle top".to_vec())],
    );
    dir.write("z_plain.txt", "needle in plain file");
    build_ok(&dir, opts_for(dir.path()));

    let report = search_ok(&dir.index_path(), "needle");
    // 40 flat entries + 2 nested entries + 1 top-level entry + 1 file.
    assert_eq!(report.results.len(), 44);
    assert_eq!(
        report.archives_opened, 2,
        "each parent archive must be opened exactly once"
    );
    // Both entries inside the same nested archive are verified —
    // consecutive siblings reuse the open inner archive.
    for entry in ["inner.zip!/deep1.txt", "inner.zip!/deep2.txt"] {
        assert!(
            report
                .results
                .iter()
                .any(|r| r.entry_path.as_deref() == Some(entry)),
            "{entry} must be verified"
        );
    }
    // Grouping is invisible in the output: results still come out in
    // the sorted candidate order.
    let keys: Vec<_> = report
        .results
        .iter()
        .map(|r| (r.file_path.clone(), r.entry_path.clone()))
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted, "grouping must preserve candidate ordering");
}

/// The stale-archive path does not even open the file, so the counter
/// stays at zero.
#[test]
fn deleted_archive_reports_stale_without_opening() {
    let dir = TempDir::new("search-stale-open");
    make_zip(
        &dir.join("a.zip"),
        vec![("x.txt", b"needle inside".to_vec())],
    );
    build_ok(&dir, opts_for(dir.path()));
    std::fs::remove_file(dir.join("a.zip")).unwrap();

    let report = search_ok(&dir.index_path(), "needle");
    assert_eq!(report.skipped_stale, 1);
    assert_eq!(report.archives_opened, 0);
    assert!(report.results.is_empty());
}

/// An archive corrupted after indexing (same mtime) fails its entries
/// without panicking or aborting the rest of the search.
#[test]
fn corrupted_archive_fails_its_entries_only() {
    let dir = TempDir::new("search-badzip");
    make_zip(
        &dir.join("bad.zip"),
        vec![
            ("a.txt", b"needle one".to_vec()),
            ("b.txt", b"needle two".to_vec()),
        ],
    );
    dir.write("ok.txt", "needle outside");
    build_ok(&dir, opts_for(dir.path()));

    // Corrupt the archive in place but keep the recorded mtime so the
    // run reaches the ZIP parse instead of the stale path.
    let zip_path = dir.join("bad.zip");
    let mtime = std::fs::metadata(&zip_path)
        .and_then(|m| m.modified())
        .unwrap();
    std::fs::write(&zip_path, b"definitely not a zip file").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&zip_path)
        .unwrap()
        .set_modified(mtime)
        .unwrap();

    let report = search_ok(&dir.index_path(), "needle");
    assert_eq!(report.archives_opened, 1, "the file was opened once");
    assert_eq!(report.verification_errors, 2);
    assert_eq!(report.results.len(), 1);
    assert!(result_for(&report, "ok.txt").is_some());
}

// ---------------------------------------------------------------------------
// Cancellation
// ---------------------------------------------------------------------------

/// A flag already raised is observed at the first check point and the
/// search returns the dedicated `Cancelled` error, not a result and
/// not a generic failure.
#[test]
fn cancelled_before_work_returns_cancelled() {
    let dir = TempDir::new("search-cancel-pre");
    dir.write("a.txt", "needle here");
    build_ok(&dir, opts_for(dir.path()));

    let cancel = AtomicBool::new(true);
    let err = search::search(
        &dir.index_path(),
        "needle",
        &SearchOptions::default(),
        &cancel,
    )
    .expect_err("a pre-cancelled search must not produce a report");
    assert!(matches!(err, search::SearchError::Cancelled));
}

/// A cancelled flag does not poison searches that have nothing to do:
/// zero candidates means zero check points, hence a normal empty
/// report.
#[test]
fn cancelled_flag_with_no_candidates_still_completes() {
    let dir = TempDir::new("search-cancel-empty");
    dir.write("a.txt", "nothing relevant");
    build_ok(&dir, opts_for(dir.path()));

    let cancel = AtomicBool::new(true);
    let report = search::search(
        &dir.index_path(),
        "zzz-absent",
        &SearchOptions::default(),
        &cancel,
    )
    .expect("no candidates => nothing to interrupt");
    assert!(report.results.is_empty());
}

/// Flipping the flag while a large regular-file verification is in
/// flight stops the search promptly instead of running to completion.
#[test]
fn cancelled_during_many_files_stops_early() {
    let dir = TempDir::new("search-cancel-files");
    for i in 0..1000 {
        dir.write(&format!("f{i:04}.txt"), &format!("needle number {i}"));
    }
    build_ok(&dir, opts_for(dir.path()));

    let cancel = std::sync::Arc::new(AtomicBool::new(false));
    let flag = std::sync::Arc::clone(&cancel);
    let index = dir.index_path();
    let worker = std::thread::spawn(move || {
        search::search(&index, "needle", &SearchOptions::default(), &flag)
    });
    cancel.store(true, std::sync::atomic::Ordering::Release);
    let outcome = worker.join().expect("search thread must not panic");
    assert!(
        matches!(outcome, Err(search::SearchError::Cancelled)),
        "cancellation must be observed mid-verification, got {outcome:?}"
    );
}

/// Same guarantee inside archive processing: the flag is checked
/// between entries of the open archive, so a cancelled run does not
/// grind through every entry.
#[test]
fn cancelled_during_archive_run_stops_early() {
    let dir = TempDir::new("search-cancel-zip");
    let many: Vec<(String, Vec<u8>)> = (0..3000)
        .map(|i| {
            (
                format!("e{i:05}.txt"),
                format!("needle entry {i}").into_bytes(),
            )
        })
        .collect();
    make_zip(&dir.join("big.zip"), many);
    build_ok(&dir, opts_for(dir.path()));

    let cancel = std::sync::Arc::new(AtomicBool::new(false));
    let flag = std::sync::Arc::clone(&cancel);
    let index = dir.index_path();
    let worker = std::thread::spawn(move || {
        search::search(&index, "needle", &SearchOptions::default(), &flag)
    });
    cancel.store(true, std::sync::atomic::Ordering::Release);
    let outcome = worker.join().expect("search thread must not panic");
    assert!(
        matches!(outcome, Err(search::SearchError::Cancelled)),
        "cancellation must be observed inside the archive run, got {outcome:?}"
    );
}

/// Cancellation is scoped to one search: a fresh flag on the same
/// index runs normally and produces the complete result set.
#[test]
fn new_search_after_cancellation_succeeds() {
    let dir = TempDir::new("search-cancel-again");
    dir.write("a.txt", "needle alpha");
    dir.write("b.txt", "needle beta");
    build_ok(&dir, opts_for(dir.path()));

    let cancelled = AtomicBool::new(true);
    assert!(matches!(
        search::search(
            &dir.index_path(),
            "needle",
            &SearchOptions::default(),
            &cancelled
        ),
        Err(search::SearchError::Cancelled)
    ));

    let report = search_ok(&dir.index_path(), "needle");
    assert_eq!(report.results.len(), 2);
    assert!(result_for(&report, "a.txt").is_some());
    assert!(result_for(&report, "b.txt").is_some());
}
