//! Integration tests for the public `verify_index` API.

mod common;

use common::*;
use rsearch_engine::{IndexError, STATUS_INDEXED};

#[test]
fn valid_index_returns_info_and_does_not_modify_the_file() {
    let dir = TempDir::new("verify-ok");
    dir.write("a.txt", "hello verification world");
    dir.write("b.txt", "second indexed document");
    build_ok(&dir, opts_for(dir.path()));

    let index = dir.index_path();
    let bytes_before = std::fs::read(&index).unwrap();
    let mtime_before = std::fs::symlink_metadata(&index)
        .unwrap()
        .modified()
        .unwrap();

    let info = rsearch_engine::verify_index(&index).expect("valid index must verify");
    assert_eq!(info.schema_version, rsearch_engine::db::SCHEMA_VERSION);
    assert_eq!(info.indexed_files, 2);
    assert_eq!(info.sources.len(), 1);
    assert!(info.built_at_unix_secs.unwrap_or(0) > 1_700_000_000);
    assert_eq!(info.size_bytes, bytes_before.len() as u64);
    assert!(info.sqlite_version.is_some());

    // Read-only guarantee: content and mtime are identical afterwards.
    assert_eq!(std::fs::read(&index).unwrap(), bytes_before);
    assert_eq!(
        std::fs::symlink_metadata(&index)
            .unwrap()
            .modified()
            .unwrap(),
        mtime_before
    );
}

#[test]
fn missing_file_is_not_found_and_is_not_created() {
    let dir = TempDir::new("verify-missing");
    let index = dir.index_path(); // never built
    let err = rsearch_engine::verify_index(&index).unwrap_err();
    assert!(matches!(err, IndexError::NotFound), "{err:?}");
    assert!(!index.exists(), "verify_index must not create the file");
}

#[test]
fn empty_or_garbage_file_is_not_an_index() {
    let dir = TempDir::new("verify-junk");
    let empty = dir.index_path().with_file_name("empty.db");
    std::fs::write(&empty, b"").unwrap();
    let err = rsearch_engine::verify_index(&empty).unwrap_err();
    assert!(matches!(err, IndexError::NotAnIndex(_)), "{err:?}");

    let junk = dir.index_path().with_file_name("junk.db");
    std::fs::write(&junk, b"this is not a sqlite database at all").unwrap();
    let err = rsearch_engine::verify_index(&junk).unwrap_err();
    assert!(matches!(err, IndexError::NotAnIndex(_)), "{err:?}");
}

#[test]
fn missing_or_false_complete_marker_is_incomplete() {
    let dir = TempDir::new("verify-incomplete");
    for (name, value) in [("absent.db", None), ("zero.db", Some("0"))] {
        let path = dir.index_path().with_file_name(name);
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(rsearch_engine::db::SCHEMA_SQL).unwrap();
            if let Some(v) = value {
                conn.execute("INSERT INTO meta(key, value) VALUES ('complete', ?1)", [v])
                    .unwrap();
            }
            conn.execute(
                "INSERT INTO meta(key, value) VALUES ('schema_version', '1')",
                [],
            )
            .unwrap();
        }
        let err = rsearch_engine::verify_index(&path).unwrap_err();
        assert!(matches!(err, IndexError::Incomplete), "{name}: {err:?}");
    }
}

#[test]
fn unknown_schema_version_is_unsupported() {
    let dir = TempDir::new("verify-schema");
    let path = dir.index_path().with_file_name("v999.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(rsearch_engine::db::SCHEMA_SQL).unwrap();
        conn.execute(
            "INSERT INTO meta(key, value) VALUES ('complete', '1'), ('schema_version', '999')",
            [],
        )
        .unwrap();
    }
    let err = rsearch_engine::verify_index(&path).unwrap_err();
    assert!(matches!(err, IndexError::UnsupportedSchema(999)), "{err:?}");
}

#[test]
fn missing_fts_table_is_not_an_index() {
    let dir = TempDir::new("verify-nof5");
    let path = dir.index_path().with_file_name("nofts.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE sources(id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE);
             CREATE TABLE documents(id INTEGER PRIMARY KEY, file_path TEXT NOT NULL, entry_path TEXT, ext TEXT, size INTEGER NOT NULL, mtime INTEGER, status INTEGER NOT NULL, reason TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO meta(key, value) VALUES ('complete', '1'), ('schema_version', '1')",
            [],
        )
        .unwrap();
    }
    let err = rsearch_engine::verify_index(&path).unwrap_err();
    assert!(matches!(err, IndexError::NotAnIndex(_)), "{err:?}");
}

#[test]
fn fts_table_that_is_not_queryable_is_fts5_unusable() {
    let dir = TempDir::new("verify-badfts");
    let path = dir.index_path().with_file_name("badfts.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(rsearch_engine::db::SCHEMA_SQL).unwrap();
        // Replace the FTS5 virtual table with a plain table of the same
        // name: the table check passes but MATCH cannot execute.
        conn.execute_batch("DROP TABLE fts; CREATE TABLE fts(content TEXT);")
            .unwrap();
        conn.execute(
            "INSERT INTO meta(key, value) VALUES ('complete', '1'), ('schema_version', '1')",
            [],
        )
        .unwrap();
    }
    let err = rsearch_engine::verify_index(&path).unwrap_err();
    assert!(matches!(err, IndexError::Fts5Unusable(_)), "{err:?}");
}

#[test]
fn verify_index_works_while_a_build_is_running() {
    let dir = TempDir::new("verify-concurrent");
    dir.write("old.txt", "old index content");
    let v1 = build_ok(&dir, opts_for(dir.path()));
    assert_eq!(v1.counters.files_indexed, 1);

    // Start a rebuild large enough to observe an intermediate phase.
    for i in 0..400 {
        dir.write(&format!("new/f{i:04}.txt"), &format!("replacement doc {i}"));
    }
    let index = dir.index_path();
    let handle = rsearch_engine::rebuild_index(&index, opts_for(dir.path()));

    // Poll until the build is clearly in progress, then verify: the
    // read-only open must succeed against the still-active old index.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let verified_mid_build = loop {
        match handle.progress().snapshot().phase {
            Some(rsearch_engine::BuildPhase::Completed) => break false,
            Some(_) => break true,
            None => {}
        }
        assert!(
            std::time::Instant::now() < deadline,
            "build did not start in time"
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
    };
    let info = rsearch_engine::verify_index(&index).expect("old index must stay readable");
    assert_eq!(info.schema_version, rsearch_engine::db::SCHEMA_VERSION);
    if verified_mid_build {
        assert_eq!(
            info.indexed_files, 1,
            "mid-build verification sees the old index"
        );
    }

    let report = handle.wait().expect("rebuild succeeds");
    assert_eq!(report.counters.files_indexed, 401);
    let info = rsearch_engine::verify_index(&index).unwrap();
    assert_eq!(info.indexed_files, 401, "new index after activation");
}

#[test]
fn indexed_documents_keep_status_indexed() {
    // Sanity guard: verify_index sees a build whose rows are status 0.
    let dir = TempDir::new("verify-status");
    dir.write("s.txt", "status sanity content");
    build_ok(&dir, opts_for(dir.path()));
    let conn = open_index(&dir);
    let rows = documents_for(&conn, &dir.join("s.txt").to_string_lossy());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, STATUS_INDEXED);
}
