//! Integration tests for incremental index updates (`update_index`).
//!
//! An update copies the active index to `<index>.building`, applies the
//! metadata diff — files whose `size`/`mtime` still match keep their
//! rows and are never re-read — and atomically swaps, exactly like a
//! rebuild. These tests cover the diff classification, the deletion of
//! stale rows (including archives), the fallback to a full rebuild and
//! the crash/cancellation contract.

mod common;

use common::*;

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
    opts.excluded_extensions.push("txt".to_string());
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
