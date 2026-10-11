//! Integration tests for saved searches: CRUD, project association
//! semantics (rename keeps them, delete removes them), multi-project
//! selections and forward-compatible parameter serialization.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use rsearch_catalog::{Catalog, CatalogError, Project, ProjectSettings, RootSpec, SearchParams};
use rsearch_engine::SearchOptions;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A temp directory that removes itself on drop.
struct TempDir {
    base: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> TempDir {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "rsearch-saved-{}-{}-{}",
            label,
            std::process::id(),
            id
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("create temp dir");
        TempDir { base }
    }

    fn catalog_path(&self) -> PathBuf {
        self.base.join("projects.db")
    }

    fn project(&self, catalog: &Catalog, name: &str) -> Project {
        catalog
            .create_project(
                name,
                ProjectSettings {
                    roots: vec![RootSpec::new(self.base.join("src"))],
                    ..ProjectSettings::default()
                },
            )
            .expect("create project")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn params() -> SearchParams {
    SearchParams {
        case_sensitive: true,
        whole_word: true,
        context_lines: 5,
        include_masks: vec!["*.rs".to_string(), "*.toml".to_string()],
        exclude_masks: vec!["Test*".to_string()],
        ..SearchParams::default()
    }
}

/// Create → list → get → rename → delete, with every field surviving
/// the round-trip through SQLite.
#[test]
fn saved_search_crud_round_trip() {
    let dir = TempDir::new("crud");
    let catalog = Catalog::open(dir.catalog_path()).expect("open");
    let project = dir.project(&catalog, "demo");

    let saved = catalog
        .create_saved_search(&project.id, "find foo", "foo_bar", params())
        .expect("create");
    assert_eq!(saved.project_id, project.id);
    assert_eq!(saved.name, "find foo");
    assert_eq!(saved.query, "foo_bar");
    assert_eq!(saved.params, params());
    assert!(saved.created_at > 0);

    let list = catalog.list_saved_searches(&project.id).expect("list");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, saved.id);

    let loaded = catalog.get_saved_search(&saved.id).expect("get");
    assert_eq!(loaded.name, "find foo");
    assert_eq!(loaded.params.context_lines, 5);
    assert_eq!(
        loaded.params.include_masks,
        vec!["*.rs".to_string(), "*.toml".to_string()]
    );
    assert_eq!(loaded.params.exclude_masks, vec!["Test*".to_string()]);

    catalog
        .rename_saved_search(&saved.id, "find bar")
        .expect("rename");
    let loaded = catalog.get_saved_search(&saved.id).expect("get");
    assert_eq!(loaded.name, "find bar");

    catalog.delete_saved_search(&saved.id).expect("delete");
    assert!(catalog.list_saved_searches(&project.id).unwrap().is_empty());
    assert!(matches!(
        catalog.get_saved_search(&saved.id),
        Err(CatalogError::NotFound(_))
    ));
}

/// Renaming a project must not break its saved searches: the link is
/// the project id, which a rename never touches.
#[test]
fn renaming_project_keeps_saved_searches() {
    let dir = TempDir::new("rename");
    let catalog = Catalog::open(dir.catalog_path()).expect("open");
    let project = dir.project(&catalog, "before");

    let saved = catalog
        .create_saved_search(&project.id, "s", "needle", SearchParams::default())
        .expect("create");
    catalog
        .rename_project(&project.id, "after")
        .expect("rename project");

    let list = catalog
        .list_saved_searches(&project.id)
        .expect("list after rename");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, saved.id);
    assert_eq!(list[0].project_id, project.id);
}

/// Deleting a project removes its saved searches; searches of other
/// projects are untouched.
#[test]
fn deleting_project_removes_its_saved_searches() {
    let dir = TempDir::new("delete");
    let catalog = Catalog::open(dir.catalog_path()).expect("open");
    let a = dir.project(&catalog, "a");
    let b = dir.project(&catalog, "b");

    catalog
        .create_saved_search(&a.id, "sa", "qa", SearchParams::default())
        .expect("create a");
    let sb = catalog
        .create_saved_search(&b.id, "sb", "qb", SearchParams::default())
        .expect("create b");

    catalog.delete_project(&a.id).expect("delete project");
    assert!(catalog.list_saved_searches(&a.id).unwrap().is_empty());
    assert_eq!(catalog.list_saved_searches(&b.id).unwrap().len(), 1);
    assert_eq!(catalog.get_saved_search(&sb.id).unwrap().name, "sb");
}

/// Missing ids and invalid names produce errors, never silent
/// successes or orphan rows.
#[test]
fn saved_search_error_cases() {
    let dir = TempDir::new("errors");
    let catalog = Catalog::open(dir.catalog_path()).expect("open");
    let project = dir.project(&catalog, "demo");

    assert!(matches!(
        catalog.create_saved_search("no-such-project", "s", "q", SearchParams::default()),
        Err(CatalogError::NotFound(_))
    ));
    assert!(matches!(
        catalog.create_saved_search(&project.id, "   ", "q", SearchParams::default()),
        Err(CatalogError::InvalidInput(_))
    ));
    assert!(matches!(
        catalog.rename_saved_search("nope", "x"),
        Err(CatalogError::NotFound(_))
    ));
    assert!(matches!(
        catalog.delete_saved_search("nope"),
        Err(CatalogError::NotFound(_))
    ));
    assert!(catalog.list_saved_searches(&project.id).unwrap().is_empty());
}

/// A `params_json` written by an older version (no `version` key,
/// missing fields, unknown keys) must still decode with defaults.
#[test]
fn old_params_documents_decode_with_defaults() {
    let dir = TempDir::new("old-params");
    let catalog = Catalog::open(dir.catalog_path()).expect("open");
    let project = dir.project(&catalog, "demo");

    let saved = catalog
        .create_saved_search(&project.id, "s", "q", SearchParams::default())
        .expect("create");

    // Rewrite params_json as an "old" document: no version, missing
    // whole_word/context_lines/masks, plus a future unknown key.
    let conn = rusqlite::Connection::open(dir.catalog_path()).unwrap();
    conn.execute(
        "UPDATE saved_searches SET params_json = ?2 WHERE id = ?1",
        rusqlite::params![saved.id, "{\"case_sensitive\":true,\"future_option\":42}"],
    )
    .unwrap();
    drop(conn);

    let loaded = catalog.get_saved_search(&saved.id).expect("reload");
    assert!(loaded.params.case_sensitive);
    assert_eq!(
        loaded.params,
        SearchParams {
            case_sensitive: true,
            ..SearchParams::default()
        }
    );
}

/// Stored parameters map faithfully to engine options; masks are kept
/// verbatim (matching is case-insensitive at match time).
#[test]
fn params_convert_to_engine_options() {
    let p = SearchParams {
        case_sensitive: true,
        whole_word: false,
        context_lines: 0,
        include_masks: vec!["*.java".to_string(), "Test*.kt".to_string()],
        exclude_masks: vec!["*Generated*".to_string()],
        ..SearchParams::default()
    };
    let opts = p.to_engine();
    assert!(opts.case_sensitive);
    assert!(!opts.whole_word);
    assert_eq!(opts.context_lines, 0);
    assert_eq!(
        opts.include_masks,
        vec!["*.java".to_string(), "Test*.kt".to_string()]
    );
    assert_eq!(opts.exclude_masks, vec!["*Generated*".to_string()]);

    // Round-trip through the stored form.
    let engine = SearchOptions {
        case_sensitive: true,
        whole_word: true,
        context_lines: 9,
        include_masks: vec!["*.rs".to_string()],
        exclude_masks: vec!["*_test.rs".to_string()],
        analyze_oversized: true,
    };
    let stored = SearchParams::from_engine(&engine);
    assert_eq!(stored.version, rsearch_catalog::SEARCH_PARAMS_VERSION);
    assert_eq!(stored.to_engine().case_sensitive, engine.case_sensitive);
    assert_eq!(stored.to_engine().whole_word, engine.whole_word);
    assert_eq!(stored.to_engine().context_lines, engine.context_lines);
    assert_eq!(stored.to_engine().include_masks, engine.include_masks);
    assert_eq!(stored.to_engine().exclude_masks, engine.exclude_masks);
    assert_eq!(
        stored.to_engine().analyze_oversized,
        engine.analyze_oversized
    );
}

/// A params document written before `analyze_oversized` existed
/// deserializes with the option off — serde defaults keep old saved
/// searches compatible without a `SEARCH_PARAMS_VERSION` bump.
#[test]
fn params_without_analyze_oversized_default_to_off() {
    let p: SearchParams = serde_json::from_str(
        r#"{"version":1,"case_sensitive":true,"whole_word":true,"context_lines":2,"include_masks":["*.rs"]}"#,
    )
    .unwrap();
    assert!(!p.analyze_oversized);
    assert!(p.case_sensitive);
    assert!(!p.to_engine().analyze_oversized);
}

// -- Multi-project selections -------------------------------------------------

/// A params document written before `project_ids` existed (v2 and
/// earlier) still decodes; the empty selection falls back to the
/// owning row's `project_id`, and a stored selection takes precedence
/// over the owner column.
#[test]
fn params_without_project_ids_decode_with_owner_fallback() {
    let old: SearchParams = serde_json::from_str(
        r#"{"version":2,"case_sensitive":true,"whole_word":false,"context_lines":2,"include_masks":[],"exclude_masks":[],"analyze_oversized":false}"#,
    )
    .unwrap();
    assert!(old.project_ids.is_empty());
    assert_eq!(old.selected_project_ids("owner-id"), vec!["owner-id"]);

    let multi = SearchParams {
        project_ids: vec!["a".into(), "b".into()],
        ..old
    };
    assert_eq!(multi.selected_project_ids("owner-id"), vec!["a", "b"]);
}

/// A saved search carrying several project ids keeps them through the
/// SQLite round-trip, alongside every other parameter.
#[test]
fn multi_project_params_round_trip() {
    let dir = TempDir::new("multi-params");
    let catalog = Catalog::open(dir.catalog_path()).expect("open");
    let a = dir.project(&catalog, "a");
    let b = dir.project(&catalog, "b");

    let params = SearchParams {
        case_sensitive: true,
        context_lines: 5,
        include_masks: vec!["*.rs".to_string()],
        project_ids: vec![a.id.clone(), b.id.clone()],
        ..SearchParams::default()
    };
    let saved = catalog
        .create_saved_search(&a.id, "multi", "needle", params.clone())
        .expect("create");

    let loaded = catalog.get_saved_search(&saved.id).expect("get");
    assert_eq!(
        loaded.project_id, a.id,
        "the column keeps the primary project"
    );
    assert_eq!(loaded.params, params);
    assert_eq!(
        loaded.params.version,
        rsearch_catalog::SEARCH_PARAMS_VERSION
    );
    assert_eq!(
        loaded.params.selected_project_ids(&loaded.project_id),
        vec![a.id.clone(), b.id.clone()]
    );
}

/// `list_all_saved_searches` spans every project, in the usual
/// creation order.
#[test]
fn list_all_saved_searches_covers_every_project() {
    let dir = TempDir::new("list-all");
    let catalog = Catalog::open(dir.catalog_path()).expect("open");
    let a = dir.project(&catalog, "a");
    let b = dir.project(&catalog, "b");

    let s1 = catalog
        .create_saved_search(&a.id, "s1", "q", SearchParams::default())
        .expect("s1");
    let s2 = catalog
        .create_saved_search(&b.id, "s2", "q", SearchParams::default())
        .expect("s2");
    let s3 = catalog
        .create_saved_search(
            &a.id,
            "s3",
            "q",
            SearchParams {
                project_ids: vec![a.id.clone(), b.id.clone()],
                ..SearchParams::default()
            },
        )
        .expect("s3");

    let all = catalog.list_all_saved_searches().expect("list all");
    assert_eq!(
        all.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
        vec![s1.id, s2.id, s3.id]
    );
    assert_eq!(
        all.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        vec!["s1", "s2", "s3"]
    );
    // The per-project list still only sees its own project's entries.
    assert_eq!(catalog.list_saved_searches(&b.id).unwrap().len(), 1);
}

/// Deleting a project removes its saved searches; searches of other
/// projects are untouched — now through the selection-aware path.
#[test]
fn deleting_project_removes_only_its_own_searches() {
    let dir = TempDir::new("delete-mono");
    let catalog = Catalog::open(dir.catalog_path()).expect("open");
    let a = dir.project(&catalog, "a");
    let b = dir.project(&catalog, "b");

    // Explicitly mono via project_ids, same outcome as a legacy row.
    let gone = catalog
        .create_saved_search(
            &a.id,
            "sa",
            "qa",
            SearchParams {
                project_ids: vec![a.id.clone()],
                ..SearchParams::default()
            },
        )
        .expect("create a");
    let sb = catalog
        .create_saved_search(&b.id, "sb", "qb", SearchParams::default())
        .expect("create b");

    catalog.delete_project(&a.id).expect("delete project");
    assert_eq!(catalog.list_all_saved_searches().unwrap().len(), 1);
    assert!(matches!(
        catalog.get_saved_search(&gone.id),
        Err(CatalogError::NotFound(_))
    ));
    assert_eq!(catalog.get_saved_search(&sb.id).unwrap().name, "sb");
}

/// Deleting one project of a multi-project search only removes that
/// id: the row survives with its other parameters and metadata intact,
/// and the owner column is re-pointed to the first remaining project
/// when it held the deleted one.
#[test]
fn deleting_a_project_trims_multi_project_searches() {
    let dir = TempDir::new("delete-trim");
    let catalog = Catalog::open(dir.catalog_path()).expect("open");
    let a = dir.project(&catalog, "a");
    let b = dir.project(&catalog, "b");

    // Owned by a, searching [a, b].
    let multi = catalog
        .create_saved_search(
            &a.id,
            "multi",
            "needle",
            SearchParams {
                case_sensitive: true,
                include_masks: vec!["*.asp".to_string()],
                project_ids: vec![a.id.clone(), b.id.clone()],
                ..SearchParams::default()
            },
        )
        .expect("create multi");

    // Deleting the *other* project trims the selection only.
    catalog.delete_project(&b.id).expect("delete b");
    let s = catalog.get_saved_search(&multi.id).expect("kept");
    assert_eq!(s.project_id, a.id, "the surviving owner is untouched");
    assert_eq!(s.params.project_ids, vec![a.id.clone()]);
    assert!(s.params.case_sensitive);
    assert_eq!(s.params.include_masks, vec!["*.asp".to_string()]);
    assert_eq!(s.name, "multi");
    assert_eq!(s.query, "needle");
    assert_eq!(s.created_at, multi.created_at);
}

/// Deleting the *owner* of a multi-project search keeps the row: the
/// id is trimmed and the owner column is healed to the first
/// remaining project — it must never dangle.
#[test]
fn deleting_the_owner_repoints_the_column() {
    let dir = TempDir::new("delete-owner");
    let catalog = Catalog::open(dir.catalog_path()).expect("open");
    let a = dir.project(&catalog, "a");
    let b = dir.project(&catalog, "b");

    let multi = catalog
        .create_saved_search(
            &a.id,
            "multi",
            "needle",
            SearchParams {
                project_ids: vec![a.id.clone(), b.id.clone()],
                ..SearchParams::default()
            },
        )
        .expect("create multi");

    catalog.delete_project(&a.id).expect("delete a");
    let s = catalog.get_saved_search(&multi.id).expect("kept");
    assert_eq!(s.params.project_ids, vec![b.id.clone()]);
    assert_eq!(
        s.project_id, b.id,
        "owner healed to the first remaining project"
    );
    assert_eq!(catalog.list_saved_searches(&b.id).unwrap().len(), 1);
}

/// A multi-project search whose last project disappears is deleted —
/// an empty selection could never run.
#[test]
fn deleting_projects_drops_searches_left_without_projects() {
    let dir = TempDir::new("delete-empty");
    let catalog = Catalog::open(dir.catalog_path()).expect("open");
    let a = dir.project(&catalog, "a");
    let b = dir.project(&catalog, "b");

    let multi = catalog
        .create_saved_search(
            &a.id,
            "multi",
            "q",
            SearchParams {
                project_ids: vec![a.id.clone(), b.id.clone()],
                ..SearchParams::default()
            },
        )
        .expect("create");

    catalog.delete_project(&a.id).expect("delete a");
    assert_eq!(
        catalog.get_saved_search(&multi.id).unwrap().project_id,
        b.id
    );
    catalog.delete_project(&b.id).expect("delete b");
    assert!(
        matches!(
            catalog.get_saved_search(&multi.id),
            Err(CatalogError::NotFound(_))
        ),
        "a selection left empty is deleted"
    );
}

/// A row whose stored selection does not contain the deleted project
/// but whose owner column does: the row survives untouched except for
/// the healed owner.
#[test]
fn deleting_a_project_heals_a_dangling_owner() {
    let dir = TempDir::new("delete-heal");
    let catalog = Catalog::open(dir.catalog_path()).expect("open");
    let a = dir.project(&catalog, "a");
    let b = dir.project(&catalog, "b");

    // Selection [b] only, but the owner column is forced to a —
    // possible only for a stored selection that never contained a.
    let s = catalog
        .create_saved_search(
            &b.id,
            "s",
            "q",
            SearchParams {
                project_ids: vec![b.id.clone()],
                ..SearchParams::default()
            },
        )
        .expect("create");
    let conn = rusqlite::Connection::open(dir.catalog_path()).unwrap();
    conn.execute(
        "UPDATE saved_searches SET project_id = ?2 WHERE id = ?1",
        rusqlite::params![s.id, a.id],
    )
    .unwrap();
    let params_json_before: String = conn
        .query_row(
            "SELECT params_json FROM saved_searches WHERE id = ?1",
            rusqlite::params![s.id],
            |r| r.get(0),
        )
        .unwrap();
    drop(conn);

    catalog.delete_project(&a.id).expect("delete a");
    let s = catalog.get_saved_search(&s.id).expect("kept");
    assert_eq!(s.project_id, b.id, "owner healed to the selection");
    assert_eq!(s.params.project_ids, vec![b.id.clone()]);
    // params_json itself was not rewritten — the selection was intact.
    let conn = rusqlite::Connection::open(dir.catalog_path()).unwrap();
    let params_json_after: String = conn
        .query_row(
            "SELECT params_json FROM saved_searches WHERE id = ?1",
            rusqlite::params![s.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(params_json_after, params_json_before);
}

/// A SQL failure mid-deletion rolls back every relational change: the
/// project row and all saved-search rows stay exactly as they were.
/// The index directory itself is already gone — file removal happens
/// first and cannot be transactional (see `delete_project`).
#[test]
fn delete_project_rolls_back_partial_saved_search_changes() {
    let dir = TempDir::new("delete-rollback");
    let catalog = Catalog::open(dir.catalog_path()).expect("open");
    let a = dir.project(&catalog, "a");
    let b = dir.project(&catalog, "b");

    // Inserted first: updated (kept) during the deletion…
    let multi = catalog
        .create_saved_search(
            &a.id,
            "multi",
            "q",
            SearchParams {
                project_ids: vec![a.id.clone(), b.id.clone()],
                ..SearchParams::default()
            },
        )
        .expect("create multi");
    // …inserted second: deleted — the statement the trigger aborts.
    let mono = catalog
        .create_saved_search(&a.id, "mono", "q", SearchParams::default())
        .expect("create mono");

    let conn = rusqlite::Connection::open(dir.catalog_path()).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER fail_saved_delete
         BEFORE DELETE ON saved_searches
         BEGIN SELECT RAISE(ABORT, 'boom'); END;",
    )
    .unwrap();
    drop(conn);

    assert!(catalog.delete_project(&a.id).is_err());

    // Nothing relational moved: the project row, the about-to-be
    // updated multi row and the aborted mono row are all intact.
    assert!(catalog.get_project(&a.id).is_ok());
    let s = catalog.get_saved_search(&multi.id).expect("multi kept");
    assert_eq!(s.params.project_ids, vec![a.id.clone(), b.id.clone()]);
    assert_eq!(s.project_id, a.id);
    assert_eq!(catalog.get_saved_search(&mono.id).unwrap().name, "mono");
    // The index directory was still removed — files go first and are
    // outside the transaction.
    assert!(!dir.base.join("projects").join(&a.id).exists());
}
