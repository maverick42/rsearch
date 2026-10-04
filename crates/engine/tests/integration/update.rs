//! Integration tests for incremental index updates (`update_index`).
//!
//! An update copies the active index to `<index>.building`, applies the
//! metadata diff — files whose `size`/`mtime` still match keep their
//! rows and are never re-read — and atomically swaps, exactly like a
//! rebuild. These tests cover the diff classification, the deletion of
//! stale rows (including archives), the fallback to a full rebuild and
//! the crash/cancellation contract.

use crate::common::*;

/// Runs `update_index` on the test index and waits for it.
fn update_ok(dir: &TempDir, opts: rsearch_engine::BuildOptions) -> rsearch_engine::BuildReport {
    let index = dir.index_path();
    rsearch_engine::update_index(&index, opts)
        .wait()
        .expect("update must succeed")
}

/// Builds the index once, then updates it with the same options.
fn build_then_update(dir: &TempDir) -> rsearch_engine::BuildReport {
    build_ok(dir, opts_for(dir.path()));
    update_ok(dir, opts_for(dir.path()))
}

fn document_count(conn: &rusqlite::Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
        .unwrap()
}

/// Bumps a file's mtime so the `size + mtime` diff sees a modification
/// regardless of filesystem timestamp granularity.
fn bump_mtime(path: &std::path::Path) {
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(later)
        .unwrap();
}

#[test]
fn update_without_existing_index_behaves_like_rebuild() {
    let dir = TempDir::new("update-fresh");
    dir.write("a.txt", "alpha update marker");
    let report = update_ok(&dir, opts_for(dir.path()));
    assert_eq!(report.counters.files_indexed, 1);
    assert_eq!(report.counters.files_unchanged, 0);
    let conn = open_index(&dir);
    assert_eq!(fts_match(&conn, "alpha update").len(), 1);
}

#[test]
fn unchanged_files_are_kept_and_never_read() {
    let dir = TempDir::new("update-unchanged");
    dir.write("a.txt", "alpha shared content");
    dir.write("b.txt", "beta shared content");
    dir.write("sub/c.txt", "gamma shared content");
    let report = build_then_update(&dir);

    assert_eq!(report.counters.files_seen, 3);
    assert_eq!(report.counters.files_unchanged, 3);
    assert_eq!(report.counters.files_modified, 0);
    assert_eq!(report.counters.files_deleted, 0);
    assert_eq!(
        report.counters.files_indexed, 0,
        "no file is reindexed on a quiet update"
    );
    assert_eq!(
        report.counters.bytes_read, 0,
        "unchanged files must never reach the workers"
    );

    let conn = open_index(&dir);
    assert_eq!(fts_match(&conn, "shared content").len(), 3);
    assert_eq!(document_count(&conn), 3);
}

/// Files ignored before or during processing (name rules, known-
/// binary extensions, sniffed binary) have no previous rows yet are
/// not "added": a quiet update over a directory containing them
/// reports a zero delta.
/// Regression: the `seen - unchanged - modified` subtraction counted
/// every ignored file as added on every update.
#[test]
fn ignored_files_are_not_reported_as_added() {
    let dir = TempDir::new("update-ignored-not-added");
    dir.write("a.txt", "alpha shared content");
    // `.exe` is classified binary at scan time: ignored, never a row.
    dir.write("b.exe", "pretend binary");
    // A text extension with binary content is ignored only after the
    // worker sniffs it — the same no-row outcome, one stage later.
    dir.write_bytes("c.txt", b"real text \x00 binary");

    let report = build_then_update(&dir);
    assert_eq!(report.counters.files_seen, 3);
    assert_eq!(report.counters.files_unchanged, 1);
    assert_eq!(report.counters.files_ignored, 2);
    let delta = report
        .summary
        .update_delta
        .expect("an update carries a delta");
    assert_eq!(delta.added, 0);
    assert_eq!(delta.updated, 0);
    assert_eq!(delta.removed, 0);
}

#[test]
fn modified_file_is_reindexed() {
    let dir = TempDir::new("update-modified");
    dir.write("a.txt", "alpha old marker");
    dir.write("b.txt", "beta stays stable");
    build_ok(&dir, opts_for(dir.path()));

    // Different size alone triggers the diff.
    dir.write("a.txt", "alpha replacement marker text");
    let report = update_ok(&dir, opts_for(dir.path()));

    assert_eq!(report.counters.files_modified, 1);
    assert_eq!(report.counters.files_unchanged, 1);
    let conn = open_index(&dir);
    assert!(
        fts_match(&conn, "old marker").is_empty(),
        "stale content must disappear"
    );
    assert_eq!(fts_match(&conn, "replacement marker").len(), 1);
    assert_eq!(document_count(&conn), 2);
}

#[test]
fn modified_file_same_size_detected_by_mtime() {
    let dir = TempDir::new("update-same-size");
    dir.write("a.txt", "aaaa bbbb cccc");
    build_ok(&dir, opts_for(dir.path()));

    // Same length: only the mtime change reveals the modification.
    let path = dir.write("a.txt", "dddd eeee ffff");
    bump_mtime(&path);
    let report = update_ok(&dir, opts_for(dir.path()));

    assert_eq!(report.counters.files_modified, 1);
    let conn = open_index(&dir);
    assert!(fts_match(&conn, "aaaa").is_empty());
    assert_eq!(fts_match(&conn, "dddd").len(), 1);
}

#[test]
fn new_and_deleted_files_are_handled() {
    let dir = TempDir::new("update-add-del");
    dir.write("keep.txt", "keep this content");
    dir.write("gone.txt", "vanished marker content");
    build_ok(&dir, opts_for(dir.path()));

    std::fs::remove_file(dir.join("gone.txt")).unwrap();
    dir.write("new.txt", "brand new marker");
    let report = update_ok(&dir, opts_for(dir.path()));

    assert_eq!(report.counters.files_unchanged, 1);
    assert_eq!(report.counters.files_deleted, 1);
    let conn = open_index(&dir);
    assert!(fts_match(&conn, "vanished marker").is_empty());
    assert_eq!(fts_match(&conn, "brand new").len(), 1);
    assert_eq!(document_count(&conn), 2);
}

#[test]
fn unchanged_archive_is_not_reopened() {
    let dir = TempDir::new("update-zip-keep");
    make_zip(
        &dir.join("a.zip"),
        vec![("inner.txt", b"zip inner content".to_vec())],
    );
    dir.write("plain.txt", "plain text content");
    let report = build_then_update(&dir);

    assert_eq!(report.counters.files_unchanged, 2);
    assert_eq!(
        report.counters.archives, 0,
        "an unchanged archive is never reopened"
    );
    assert_eq!(report.counters.archive_bytes_decompressed, 0);
    let conn = open_index(&dir);
    assert_eq!(fts_match(&conn, "zip inner").len(), 1);
}

#[test]
fn modified_archive_is_reindexed() {
    let dir = TempDir::new("update-zip-mod");
    make_zip(
        &dir.join("a.zip"),
        vec![("inner.txt", b"old entry content".to_vec())],
    );
    build_ok(&dir, opts_for(dir.path()));

    // Archive rows carry the outer mtime but not the outer size; bump
    // the mtime explicitly so the diff fires deterministically.
    make_zip(
        &dir.join("a.zip"),
        vec![
            ("inner.txt", b"new entry content".to_vec()),
            ("added.txt", b"freshly added entry".to_vec()),
        ],
    );
    bump_mtime(&dir.join("a.zip"));
    let report = update_ok(&dir, opts_for(dir.path()));

    assert_eq!(report.counters.files_modified, 1);
    let conn = open_index(&dir);
    assert!(
        fts_match(&conn, "old entry").is_empty(),
        "stale archive entries must be removed"
    );
    assert_eq!(fts_match(&conn, "new entry").len(), 1);
    assert_eq!(fts_match(&conn, "freshly added").len(), 1);
    assert_eq!(document_count(&conn), 2, "a.zip rows: inner + added");
}

#[test]
fn unchanged_error_row_is_kept_without_reprocessing() {
    let dir = TempDir::new("update-err");
    // Invalid UTF-8 with no fallback -> STATUS_ERROR row, no FTS row.
    dir.write_bytes("bad.txt", b"\xc0\xaf not utf8 \xf8\xf9");
    dir.write("ok.txt", "ok marker content");
    let first = build_ok(&dir, opts_for(dir.path()));
    assert_eq!(first.counters.errors, 1);

    let report = update_ok(&dir, opts_for(dir.path()));
    assert_eq!(report.counters.files_unchanged, 2);
    assert_eq!(
        report.counters.errors, 0,
        "an unchanged error row is kept, not re-decoded"
    );
    let conn = open_index(&dir);
    assert_eq!(document_count(&conn), 2);
}

#[test]
fn changed_options_fall_back_to_full_rebuild() {
    let dir = TempDir::new("update-opts");
    dir.write("a.txt", "alpha rebuild marker");
    build_ok(&dir, opts_for(dir.path()));

    // Options differ from the recorded build_options: metadata alone
    // cannot reason about policy-derived rows, so update() rebuilds.
    let mut opts = opts_for(dir.path());
    opts.exclude_masks.push("*.txt".to_string());
    let report = update_ok(&dir, opts);

    assert_eq!(
        report.counters.files_unchanged, 0,
        "an option change must trigger a full rebuild, not a diff"
    );
    let conn = open_index(&dir);
    assert!(fts_match(&conn, "rebuild marker").is_empty());
    assert_eq!(document_count(&conn), 0);
}

#[test]
fn stale_building_file_is_replaced_by_update() {
    let dir = TempDir::new("update-stale");
    dir.write("a.txt", "alpha marker content");
    build_ok(&dir, opts_for(dir.path()));
    std::fs::write(dir.building_path(), b"junk").unwrap();

    let report = update_ok(&dir, opts_for(dir.path()));
    assert_eq!(report.counters.files_unchanged, 1);
    assert!(!dir.building_path().exists());
    let conn = open_index(&dir);
    assert_eq!(fts_match(&conn, "alpha marker").len(), 1);
}

#[test]
fn cancelled_update_preserves_the_active_index() {
    let dir = TempDir::new("update-cancel");
    for i in 0..300 {
        dir.write(&format!("f{i:04}.txt"), &format!("original content {i}"));
    }
    build_ok(&dir, opts_for(dir.path()));

    // Give the update real work: modify a slice of files.
    for i in 0..50 {
        let p = dir.write(&format!("f{i:04}.txt"), &format!("replacement content {i}"));
        bump_mtime(&p);
    }

    let index = dir.index_path();
    let handle = rsearch_engine::update_index(&index, opts_for(dir.path()));
    handle.cancel();
    let outcome = handle.wait();

    // Whether the update was cancelled in time or raced to completion,
    // the index must be a valid one.
    rsearch_engine::verify_index(&index).expect("index must stay valid");
    let conn = open_index(&dir);
    match outcome {
        Err(rsearch_engine::BuildError::Cancelled { .. }) => {
            assert_eq!(
                fts_match(&conn, "original content").len(),
                300,
                "a cancelled update leaves the previous index intact"
            );
        }
        Ok(report) => {
            assert_eq!(report.counters.files_modified, 50);
            assert_eq!(fts_match(&conn, "replacement content").len(), 50);
        }
        Err(e) => panic!("unexpected update failure: {e}"),
    }
}

#[test]
fn index_info_reports_total_documents_after_update() {
    let dir = TempDir::new("update-info");
    dir.write("a.txt", "alpha marker content");
    dir.write("b.txt", "beta marker content");
    build_ok(&dir, opts_for(dir.path()));
    dir.write("c.txt", "gamma marker content");
    update_ok(&dir, opts_for(dir.path()));

    let info = rsearch_engine::verify_index(&dir.index_path()).unwrap();
    assert_eq!(
        info.indexed_files, 3,
        "verify_index must report total indexed documents, not the update's own work"
    );
}

#[test]
fn pure_mtime_touch_reindexes_and_updates_stored_mtime() {
    let dir = TempDir::new("update-mtime");
    dir.write("a.txt", "same content marker");
    build_ok(&dir, opts_for(dir.path()));

    let stored_mtime = |conn: &rusqlite::Connection| -> i64 {
        let path = dir.join("a.txt");
        conn.query_row(
            "SELECT mtime FROM documents WHERE file_path = ?1",
            [path.to_str().unwrap()],
            |r| r.get(0),
        )
        .unwrap()
    };
    let before = stored_mtime(&open_index(&dir));

    // Content identical; only the timestamp moves.
    bump_mtime(&dir.join("a.txt"));
    let report = update_ok(&dir, opts_for(dir.path()));

    assert_eq!(report.counters.files_modified, 1);
    assert_eq!(report.counters.files_indexed, 1);
    let conn = open_index(&dir);
    assert_eq!(fts_match(&conn, "same content").len(), 1);
    assert_ne!(
        stored_mtime(&conn),
        before,
        "the stored mtime must be refreshed"
    );
}

#[test]
fn file_turned_binary_loses_its_rows() {
    let dir = TempDir::new("update-binary");
    // `.dat` is not an extension-classified type: the row existed
    // because the worker sniffed the old content as text.
    dir.write("a.dat", "plain text marker");
    dir.write("keep.txt", "keep marker");
    build_ok(&dir, opts_for(dir.path()));

    dir.write_bytes("a.dat", b"\x00\x01\x02binary\xff\x00payload");
    let report = update_ok(&dir, opts_for(dir.path()));

    assert_eq!(report.counters.files_modified, 1);
    let conn = open_index(&dir);
    assert!(
        fts_match(&conn, "plain text").is_empty(),
        "the stale text row must be gone"
    );
    assert_eq!(
        documents_like(&conn, "a.dat").len(),
        0,
        "a file sniffed binary keeps no document row"
    );
    assert_eq!(document_count(&conn), 1);
}

#[test]
fn file_turned_too_large_gets_status_row() {
    let dir = TempDir::new("update-large");
    let mut opts = opts_for(dir.path());
    opts.max_indexed_file_size = 64;

    dir.write("a.txt", "small marker");
    build_ok(&dir, opts.clone());

    dir.write("a.txt", &format!("grown marker {}", "x".repeat(200)));
    let report = update_ok(&dir, opts);

    assert_eq!(report.counters.files_modified, 1);
    assert_eq!(report.counters.files_too_large, 1);
    let conn = open_index(&dir);
    let rows = documents_like(&conn, "a.txt");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].1,
        rsearch_engine::STATUS_TOO_LARGE,
        "the new row must carry the too-large status"
    );
    assert!(fts_match(&conn, "small marker").is_empty());
    assert!(
        fts_match(&conn, "grown marker").is_empty(),
        "a too-large file has no FTS row"
    );
}

#[test]
fn file_turned_undecodable_gets_error_row() {
    let dir = TempDir::new("update-undecodable");
    dir.write("a.txt", "decodable marker");
    build_ok(&dir, opts_for(dir.path()));

    dir.write_bytes("a.txt", b"\xf8\xf9 invalid utf8 payload \xfe");
    let report = update_ok(&dir, opts_for(dir.path()));

    assert_eq!(report.counters.files_modified, 1);
    assert_eq!(report.counters.errors, 1);
    let conn = open_index(&dir);
    let rows = documents_like(&conn, "a.txt");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1, rsearch_engine::STATUS_ERROR);
    assert!(fts_match(&conn, "decodable marker").is_empty());
}

#[test]
fn previously_errored_file_heals_when_fixed() {
    let dir = TempDir::new("update-heal");
    dir.write_bytes("bad.txt", b"\xf8\xf9 invalid \xfe");
    let first = build_ok(&dir, opts_for(dir.path()));
    assert_eq!(first.counters.errors, 1);

    dir.write("bad.txt", "now it is valid text marker");
    let report = update_ok(&dir, opts_for(dir.path()));

    assert_eq!(report.counters.files_modified, 1);
    assert_eq!(report.counters.files_indexed, 1);
    let conn = open_index(&dir);
    let rows = documents_like(&conn, "bad.txt");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1, rsearch_engine::STATUS_INDEXED);
    assert_eq!(fts_match(&conn, "valid text").len(), 1);
}

#[test]
fn deleted_archive_removes_all_entry_rows() {
    let dir = TempDir::new("update-del-zip");
    // Two levels deep: nested entries share the outer `file_path`.
    let inner = zip_bytes(vec![("deep.txt", b"nested deep marker".to_vec())]);
    make_zip(
        &dir.join("outer.zip"),
        vec![
            ("one.txt", b"first entry marker".to_vec()),
            ("inner.zip", inner),
        ],
    );
    dir.write("keep.txt", "keep marker");
    build_ok(&dir, opts_for(dir.path()));
    {
        let conn = open_index(&dir);
        assert_eq!(fts_match(&conn, "nested deep").len(), 1);
    }

    std::fs::remove_file(dir.join("outer.zip")).unwrap();
    let report = update_ok(&dir, opts_for(dir.path()));

    assert_eq!(report.counters.files_deleted, 1);
    let conn = open_index(&dir);
    assert_eq!(
        documents_like(&conn, "outer.zip").len(),
        0,
        "no document row may survive the deleted archive"
    );
    assert!(fts_match(&conn, "first entry").is_empty());
    assert!(fts_match(&conn, "nested deep").is_empty());
    assert_eq!(document_count(&conn), 1);
}

#[test]
fn new_archive_is_indexed_on_update() {
    let dir = TempDir::new("update-new-zip");
    dir.write("keep.txt", "keep marker");
    build_ok(&dir, opts_for(dir.path()));

    make_zip(
        &dir.join("fresh.zip"),
        vec![("doc.txt", b"fresh zip marker".to_vec())],
    );
    let report = update_ok(&dir, opts_for(dir.path()));

    assert_eq!(report.counters.archives, 1);
    let conn = open_index(&dir);
    assert_eq!(fts_match(&conn, "fresh zip").len(), 1);
    assert_eq!(document_count(&conn), 2);
}

#[test]
fn modified_nested_archive_reindexes_every_level() {
    let dir = TempDir::new("update-nested");
    let inner_v1 = zip_bytes(vec![("deep.txt", b"nested version one".to_vec())]);
    make_zip(&dir.join("outer.zip"), vec![("inner.zip", inner_v1)]);
    build_ok(&dir, opts_for(dir.path()));

    let inner_v2 = zip_bytes(vec![
        ("deep.txt", b"nested version two".to_vec()),
        ("extra.txt", b"nested extra entry".to_vec()),
    ]);
    make_zip(&dir.join("outer.zip"), vec![("inner.zip", inner_v2)]);
    bump_mtime(&dir.join("outer.zip"));
    let report = update_ok(&dir, opts_for(dir.path()));

    assert_eq!(report.counters.files_modified, 1);
    let conn = open_index(&dir);
    assert!(fts_match(&conn, "version one").is_empty());
    assert_eq!(fts_match(&conn, "version two").len(), 1);
    assert_eq!(fts_match(&conn, "extra entry").len(), 1);
    // Outer rows: deep.txt + extra.txt entries, all under outer.zip.
    assert_eq!(documents_like(&conn, "outer.zip").len(), 2);
}

#[test]
fn engine_version_mismatch_falls_back_to_rebuild() {
    let dir = TempDir::new("update-engine");
    dir.write("a.txt", "alpha engine marker");
    dir.write("b.txt", "beta engine marker");
    build_ok(&dir, opts_for(dir.path()));

    // Simulate an index written by another engine release.
    {
        let conn = open_index(&dir);
        conn.execute(
            "UPDATE meta SET value = '0.0.0-foreign' WHERE key = 'engine_version'",
            [],
        )
        .unwrap();
    }

    let report = update_ok(&dir, opts_for(dir.path()));
    assert_eq!(
        report.counters.files_unchanged, 0,
        "a foreign engine version must force a rebuild, not a diff"
    );
    assert_eq!(report.counters.files_indexed, 2);
}

/// Windows > MAX_PATH file: the update must reopen it through the
/// verbatim path and refresh the row, like a build does.
#[test]
#[cfg(windows)]
fn long_path_file_updates_like_a_normal_file() {
    use rsearch_engine::longpath::io_path;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    const MAX_PATH: usize = 260;
    fn wide_len(p: &Path) -> usize {
        p.as_os_str().encode_wide().count()
    }

    let dir = TempDir::new("update-longpath");
    let file_name = format!("{}.txt", "l".repeat(60));
    let mut deep = dir.path().to_path_buf();
    while wide_len(&deep.join(&file_name)) < MAX_PATH && wide_len(&deep) + 45 < MAX_PATH {
        deep = deep.join("d".repeat(40));
    }
    let file_path = deep.join(&file_name);
    assert!(wide_len(&file_path) >= MAX_PATH);
    assert!(wide_len(&deep) < MAX_PATH);

    std::fs::create_dir_all(io_path(&deep).unwrap()).unwrap();
    std::fs::write(io_path(&file_path).unwrap(), "long path original marker").unwrap();
    build_ok(&dir, opts_for(dir.path()));

    // Unchanged first: the stored row must be kept without a re-read.
    let report = update_ok(&dir, opts_for(dir.path()));
    assert_eq!(report.counters.files_unchanged, 1);

    // Then modified: the new content must be reindexed.
    std::fs::write(io_path(&file_path).unwrap(), "long path replacement marker").unwrap();
    let report = update_ok(&dir, opts_for(dir.path()));
    assert_eq!(report.counters.files_modified, 1);

    let conn = open_index(&dir);
    assert!(fts_match(&conn, "original marker").is_empty());
    assert_eq!(fts_match(&conn, "replacement marker").len(), 1);
}

#[test]
fn older_schema_version_falls_back_to_rebuild() {
    let dir = TempDir::new("update-v1");
    dir.write("a.txt", "alpha schema marker");
    build_ok(&dir, opts_for(dir.path()));

    // Simulate a pre-v2 index (no contentless-delete support).
    {
        let conn = open_index(&dir);
        conn.execute(
            "UPDATE meta SET value = '1' WHERE key = 'schema_version'",
            [],
        )
        .unwrap();
    }

    let report = update_ok(&dir, opts_for(dir.path()));
    assert_eq!(
        report.counters.files_unchanged, 0,
        "an unsupported schema must force a rebuild, not a diff"
    );
    let info = rsearch_engine::verify_index(&dir.index_path()).unwrap();
    assert_eq!(info.schema_version, rsearch_engine::db::SCHEMA_VERSION);
}

/// Builds a ZIP in memory and returns its bytes (for nested archives).
fn zip_bytes(entries: Vec<(&str, Vec<u8>)>) -> Vec<u8> {
    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut cursor);
        let options: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, bytes) in &entries {
            zip.start_file(*name, options).expect("start entry");
            std::io::Write::write_all(&mut zip, bytes).expect("write entry");
        }
        zip.finish().expect("finish zip");
    }
    cursor.into_inner()
}

/// One document row without its id, for cross-index comparison.
type DocRow = (
    String,
    Option<String>,
    Option<String>,
    i64,
    Option<i64>,
    i32,
    Option<String>,
);

/// All document rows of an index except ids, in a canonical order.
/// Two indexes built by different means are compared on this dump.
fn dump_documents(conn: &rusqlite::Connection) -> Vec<DocRow> {
    let mut stmt = conn
        .prepare(
            "SELECT file_path, entry_path, ext, size, mtime, status, reason
             FROM documents ORDER BY file_path, entry_path",
        )
        .unwrap();
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        })
        .unwrap();
    rows.map(|r| r.unwrap()).collect()
}

/// The (file_path, entry_path) pairs that carry an FTS row, sorted.
fn dump_fts_membership(conn: &rusqlite::Connection) -> Vec<(String, Option<String>)> {
    let mut stmt = conn
        .prepare(
            "SELECT d.file_path, d.entry_path FROM fts f
             JOIN documents d ON d.id = f.rowid
             ORDER BY d.file_path, d.entry_path",
        )
        .unwrap();
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    rows.map(|r| r.unwrap()).collect()
}

/// A search result reduced to comparable parts: identity plus the
/// positions of verified occurrences.
type SearchHit = (String, Option<String>, Vec<(usize, usize)>);
/// The SearchReport counters, minus the wall-clock field.
type SearchCounters = (usize, usize, usize, usize, usize, usize);

/// Verified search results reduced to comparable tuples.
fn search_summary(
    index: &std::path::Path,
    query: &str,
    options: &rsearch_engine::SearchOptions,
) -> (Vec<SearchHit>, SearchCounters) {
    let report = rsearch_engine::search(
        index,
        query,
        options,
        &std::sync::atomic::AtomicBool::new(false),
    )
    .expect("search");
    let mut results: Vec<_> = report
        .results
        .iter()
        .map(|r| {
            (
                r.file_path.to_string_lossy().into_owned(),
                r.entry_path.clone(),
                r.occurrences
                    .iter()
                    .map(|o| (o.line, o.column))
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    results.sort();
    (
        results,
        (
            report.candidates_from_index,
            report.candidates_too_large,
            report.skipped_stale,
            report.skipped_index_errors,
            report.skipped_security_limits,
            report.verification_errors,
        ),
    )
}

/// The full mutation matrix: an index updated in place must end up
/// byte-for-byte equivalent (documents, FTS membership, verified
/// search results) to a fresh rebuild of the same tree.
#[test]
fn update_result_matches_full_rebuild() {
    let dir = TempDir::new("update-equiv");
    let mut opts = opts_for(dir.path());
    opts.max_indexed_file_size = 512;

    // --- Initial tree ---
    dir.write("keep.txt", "shared alpha marker keepme");
    dir.write("mod-size.txt", "gamma marker before");
    dir.write("mod-same.txt", "aaaa bbbb cccc");
    dir.write("touch.txt", "touched content marker");
    dir.write("gone.txt", "vanished marker");
    dir.write("tobinary.dat", "was plain text marker");
    dir.write("toerr.txt", "will break marker");
    dir.write("grow.txt", "tiny");
    make_zip(
        &dir.join("mod.zip"),
        vec![
            ("entry-a.txt", b"zip alpha entry".to_vec()),
            ("entry-gone.txt", b"zip removed entry".to_vec()),
        ],
    );
    let inner_v1 = zip_bytes(vec![("deep.txt", b"nested v1 marker".to_vec())]);
    make_zip(&dir.join("nest.zip"), vec![("inner.zip", inner_v1)]);
    make_zip(
        &dir.join("del.zip"),
        vec![("x.txt", b"zip deleted marker".to_vec())],
    );
    dir.write_bytes("bin.bin", b"\x00\x01binary");
    dir.write_bytes("err.txt", b"\xf8\xf9 broken marker");

    let index_a = dir.index_path();
    rsearch_engine::rebuild_index(&index_a, opts.clone())
        .wait()
        .expect("build A");

    // --- Mutations ---
    dir.write("added.txt", "brand new alpha marker");
    std::fs::remove_file(dir.join("gone.txt")).unwrap();
    dir.write("mod-size.txt", "gamma marker after with different length");
    dir.write("mod-same.txt", "dddd eeee ffff");
    bump_mtime(&dir.join("mod-same.txt"));
    bump_mtime(&dir.join("touch.txt"));
    dir.write_bytes("tobinary.dat", b"\x00\xff\x01 not text anymore");
    dir.write_bytes("toerr.txt", b"\xf8\xf9 now broken");
    dir.write("grow.txt", &format!("big marker {}", "z".repeat(600)));
    make_zip(
        &dir.join("mod.zip"),
        vec![
            ("entry-a.txt", b"zip alpha entry changed".to_vec()),
            ("entry-new.txt", b"zip new entry".to_vec()),
        ],
    );
    bump_mtime(&dir.join("mod.zip"));
    let inner_v2 = zip_bytes(vec![("deep.txt", b"nested v2 marker".to_vec())]);
    make_zip(&dir.join("nest.zip"), vec![("inner.zip", inner_v2)]);
    bump_mtime(&dir.join("nest.zip"));
    std::fs::remove_file(dir.join("del.zip")).unwrap();
    make_zip(
        &dir.join("new.zip"),
        vec![("y.txt", b"zip brand new marker".to_vec())],
    );

    // --- Update A in place, rebuild B from scratch ---
    let report = rsearch_engine::update_index(&index_a, opts.clone())
        .wait()
        .expect("update A");
    assert!(report.counters.files_unchanged > 0);
    assert!(report.counters.files_modified > 0);
    assert!(report.counters.files_deleted > 0);

    let index_b = dir.path().parent().unwrap().join("index-b.db");
    rsearch_engine::rebuild_index(&index_b, opts)
        .wait()
        .expect("rebuild B");

    // --- Compare the two indexes ---
    let conn_a = rusqlite::Connection::open(&index_a).unwrap();
    let conn_b = rusqlite::Connection::open(&index_b).unwrap();

    let docs_a = dump_documents(&conn_a);
    let docs_b = dump_documents(&conn_b);
    assert_eq!(
        docs_a.len(),
        docs_b.len(),
        "document counts differ: update={} rebuild={}",
        docs_a.len(),
        docs_b.len()
    );
    assert_eq!(docs_a, docs_b, "document rows must be identical");

    let fts_a = dump_fts_membership(&conn_a);
    let fts_b = dump_fts_membership(&conn_b);
    assert_eq!(fts_a, fts_b, "FTS membership must be identical");

    let meta = |conn: &rusqlite::Connection, key: &str| -> String {
        conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(
        meta(&conn_a, "indexed_documents"),
        meta(&conn_b, "indexed_documents")
    );
    assert_eq!(
        meta(&conn_a, "schema_version"),
        meta(&conn_b, "schema_version")
    );

    // Verified searches must agree on both indexes.
    for query in [
        "marker",
        "shared alpha",
        "zip alpha entry",
        "nested v2",
        "big marker",
        "brand new",
    ] {
        for options in [
            rsearch_engine::SearchOptions::default(),
            rsearch_engine::SearchOptions {
                case_sensitive: true,
                ..Default::default()
            },
            rsearch_engine::SearchOptions {
                include_masks: vec!["*.txt".to_string()],
                ..Default::default()
            },
        ] {
            let (res_a, cnt_a) = search_summary(&index_a, query, &options);
            let (res_b, cnt_b) = search_summary(&index_b, query, &options);
            assert_eq!(
                res_a, res_b,
                "search results differ for {query:?} ({options:?})"
            );
            assert_eq!(
                cnt_a, cnt_b,
                "search counters differ for {query:?} ({options:?})"
            );
        }
    }

    let _ = std::fs::remove_file(&index_b);
}
