//! POC: FTS5 `contentless_delete` tables (SQLite >= 3.43).
//!
//! Validates the building block behind `update_index`:
//! rows can be deleted from — and replaced in — a contentless
//! (`content = ''`) FTS5 table using only rowids, without storing or
//! re-supplying the old document text.
//!
//! Semantics pinned down by these tests:
//!
//! - `DELETE FROM fts WHERE rowid = ?` removes a row's candidates and
//!   is a harmless no-op for unknown rowids.
//! - `INSERT OR REPLACE` and `UPDATE fts SET content = ?` (all user
//!   columns supplied) replace a row's indexed text: terms present
//!   only in the old content stop matching, so no stale candidates
//!   survive.
//! - A bare re-`INSERT` over a live rowid does NOT replace: old and
//!   new terms both match. Replacements must use `OR REPLACE` or an
//!   explicit `DELETE` first.
//! - The legacy `'delete'` insert-command is rejected: that is the
//!   protocol of plain contentless tables, not contentless-delete
//!   ones.
//! - Deletes are transactional: `ROLLBACK` restores the row.
//!
//! The standalone tests above use a minimal in-memory table; the last
//! two exercise the same cycle against `SCHEMA_SQL` and a real index
//! built by the engine.

mod common;

use common::*;
use rsearch_engine::db::SCHEMA_SQL;
use rusqlite::{params, Connection};

/// Creates the index FTS table plus `contentless_delete = 1`, in memory.
fn fts_delete_table() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE VIRTUAL TABLE fts USING fts5(
            content,
            content = '',
            contentless_delete = 1,
            tokenize = 'trigram case_sensitive 0'
        );",
    )
    .unwrap();
    conn
}

fn insert(conn: &Connection, rowid: i64, content: &str) {
    conn.execute(
        "INSERT INTO fts(rowid, content) VALUES (?1, ?2)",
        params![rowid, content],
    )
    .unwrap();
}

/// Sorted rowids matching `phrase` (rowid order is deterministic).
fn matches(conn: &Connection, phrase: &str) -> Vec<i64> {
    let mut ids = fts_match(conn, phrase);
    ids.sort_unstable();
    ids
}

#[test]
fn bundled_sqlite_supports_contentless_delete() {
    // Merely creating the table requires SQLite >= 3.43; insert + match
    // prove the trigram path still works with the option enabled.
    let conn = fts_delete_table();
    insert(&conn, 1, "alpha shared marker");
    insert(&conn, 2, "beta shared marker");
    assert_eq!(matches(&conn, "shared marker"), [1, 2]);
    assert_eq!(matches(&conn, "alpha"), [1]);
    assert_eq!(matches(&conn, "beta"), [2]);
}

#[test]
fn delete_by_rowid_removes_candidates() {
    let conn = fts_delete_table();
    insert(&conn, 1, "alpha shared marker");
    insert(&conn, 2, "beta shared marker");

    conn.execute("DELETE FROM fts WHERE rowid = 1", []).unwrap();

    assert_eq!(matches(&conn, "shared marker"), [2]);
    assert!(matches(&conn, "alpha").is_empty());
    assert_eq!(matches(&conn, "beta"), [2]);
}

#[test]
fn delete_of_unknown_rowid_is_a_noop() {
    // update() will delete by rowid sets computed from metadata;
    // deleting an absent row must not fail.
    let conn = fts_delete_table();
    insert(&conn, 1, "alpha shared marker");
    conn.execute("DELETE FROM fts WHERE rowid = 99", [])
        .unwrap();
    assert_eq!(matches(&conn, "shared marker"), [1]);
}

#[test]
fn insert_or_replace_same_rowid_drops_old_terms() {
    // The core update() operation: new content under an existing
    // rowid via INSERT OR REPLACE must not leave stale candidates.
    let conn = fts_delete_table();
    insert(&conn, 1, "alpha shared marker");

    conn.execute(
        "INSERT OR REPLACE INTO fts(rowid, content) VALUES (1, 'gamma shared marker')",
        [],
    )
    .unwrap();

    assert_eq!(matches(&conn, "gamma"), [1]);
    assert!(
        matches(&conn, "alpha").is_empty(),
        "old terms must not produce stale candidates"
    );
}

#[test]
fn delete_then_insert_same_rowid_drops_old_terms() {
    // Explicit DELETE + INSERT (the other replacement path) behaves
    // the same: old terms are shadowed after the delete.
    let conn = fts_delete_table();
    insert(&conn, 1, "alpha shared marker");
    conn.execute("DELETE FROM fts WHERE rowid = 1", []).unwrap();
    insert(&conn, 1, "gamma shared marker");

    assert_eq!(matches(&conn, "gamma"), [1]);
    assert!(matches(&conn, "alpha").is_empty());
}

#[test]
fn bare_reinsert_over_live_rowid_keeps_old_terms() {
    // Documents why replacement must go through OR REPLACE or an
    // explicit DELETE: a bare INSERT over a live rowid adds postings
    // without removing the old ones, so old terms keep matching.
    // Stale candidates are filtered later by file verification, but
    // they still cost index space and search work.
    let conn = fts_delete_table();
    insert(&conn, 1, "alpha shared marker");
    insert(&conn, 1, "gamma shared marker");

    assert_eq!(matches(&conn, "gamma"), [1]);
    assert_eq!(matches(&conn, "alpha"), [1], "observed stale candidate");
}

#[test]
fn update_statement_replaces_content() {
    // UPDATE is supported on contentless-delete tables when every
    // user column is supplied (here: the single `content` column).
    // This lets update() keep documents.id / fts.rowid stable.
    let conn = fts_delete_table();
    insert(&conn, 1, "alpha shared marker");
    conn.execute(
        "UPDATE fts SET content = 'delta shared marker' WHERE rowid = 1",
        [],
    )
    .unwrap();

    assert_eq!(matches(&conn, "delta"), [1]);
    assert!(matches(&conn, "alpha").is_empty());
}

#[test]
fn legacy_delete_command_is_rejected() {
    // The 'delete' insert-command is the protocol of *plain*
    // contentless tables. On contentless-delete tables it is an
    // error — DELETE / UPDATE / INSERT OR REPLACE are the forms.
    let conn = fts_delete_table();
    insert(&conn, 1, "alpha shared marker");

    let err = conn
        .execute(
            "INSERT INTO fts(fts, rowid, content) VALUES ('delete', 1, NULL)",
            [],
        )
        .unwrap_err();
    assert!(err.to_string().contains("delete"), "unexpected: {err}");
    assert_eq!(matches(&conn, "alpha"), [1]);
}

#[test]
fn deletes_are_transactional() {
    let conn = fts_delete_table();
    insert(&conn, 1, "alpha shared marker");

    conn.execute_batch("BEGIN").unwrap();
    conn.execute("DELETE FROM fts WHERE rowid = 1", []).unwrap();
    assert!(matches(&conn, "alpha").is_empty());
    conn.execute_batch("ROLLBACK").unwrap();
    assert_eq!(
        matches(&conn, "alpha"),
        [1],
        "rollback must restore deleted candidates"
    );

    conn.execute_batch("BEGIN").unwrap();
    conn.execute("DELETE FROM fts WHERE rowid = 1", []).unwrap();
    conn.execute_batch("COMMIT").unwrap();
    assert!(matches(&conn, "alpha").is_empty());
}

#[test]
fn real_index_supports_delete() {
    // An index built by the real pipeline now has a contentless-delete
    // fts table: rows can be deleted by rowid. The documents row is
    // untouched — update() manages the two tables separately.
    let dir = TempDir::new("real-index-delete");
    dir.write("a.txt", "alpha shared marker");
    build_ok(&dir, opts_for(dir.path()));
    let conn = open_index(&dir);

    let rowid: i64 = conn
        .query_row("SELECT id FROM documents", [], |r| r.get(0))
        .unwrap();
    assert_eq!(matches(&conn, "alpha"), [rowid]);

    conn.execute("DELETE FROM fts WHERE rowid = ?1", params![rowid])
        .unwrap();
    assert!(matches(&conn, "alpha").is_empty());

    let doc_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
        .unwrap();
    assert_eq!(doc_rows, 1);
}

#[test]
fn engine_schema_supports_the_full_cycle() {
    // The real schema (documents + fts, SCHEMA_SQL as shipped) runs
    // the whole update cycle: documents join, delete, reindex.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(SCHEMA_SQL).unwrap();

    conn.execute(
        "INSERT INTO documents(file_path, entry_path, ext, size, mtime, status, reason)
         VALUES ('a.txt', NULL, 'txt', 10, 1, 0, NULL)",
        [],
    )
    .unwrap();
    let rowid = conn.last_insert_rowid();
    insert(&conn, rowid, "alpha shared marker");

    // The documents.id == fts.rowid join still works.
    let found: i64 = conn
        .query_row(
            "SELECT d.id FROM documents d JOIN fts ON fts.rowid = d.id
             WHERE fts MATCH '\"alpha\"'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(found, rowid);

    // Full update cycle: metadata update + FTS row replacement.
    conn.execute("DELETE FROM fts WHERE rowid = ?1", params![rowid])
        .unwrap();
    conn.execute(
        "UPDATE documents SET size = 11 WHERE id = ?1",
        params![rowid],
    )
    .unwrap();
    insert(&conn, rowid, "gamma shared marker");

    assert_eq!(matches(&conn, "gamma"), [rowid]);
    assert!(matches(&conn, "alpha").is_empty());
}
