//! Integration tests for saved searches: CRUD, project association
//! semantics (rename keeps them, delete removes them) and forward-
//! compatible parameter serialization.

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
        extensions: Some(vec!["rs".to_string(), "toml".to_string()]),
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
        loaded.params.extensions.as_deref(),
        Some(&["rs".to_string(), "toml".to_string()][..])
    );

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
    // whole_word/context_lines/extensions, plus a future unknown key.
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

/// Stored parameters map faithfully to engine options, and the
/// extension list is normalized the way the engine expects.
#[test]
fn params_convert_to_engine_options() {
    let p = SearchParams {
        case_sensitive: true,
        whole_word: false,
        context_lines: 0,
        extensions: Some(vec![".RS".to_string(), "Toml".to_string(), "".to_string()]),
        ..SearchParams::default()
    };
    let opts = p.to_engine();
    assert!(opts.case_sensitive);
    assert!(!opts.whole_word);
    assert_eq!(opts.context_lines, 0);
    assert_eq!(
        opts.extensions.as_deref(),
        Some(&["rs".to_string(), "toml".to_string()][..])
    );

    // Round-trip through the stored form.
    let engine = SearchOptions {
        case_sensitive: true,
        whole_word: true,
        context_lines: 9,
        extensions: None,
    };
    let stored = SearchParams::from_engine(&engine);
    assert_eq!(stored.version, rsearch_catalog::SEARCH_PARAMS_VERSION);
    assert_eq!(stored.to_engine().case_sensitive, engine.case_sensitive);
    assert_eq!(stored.to_engine().whole_word, engine.whole_word);
    assert_eq!(stored.to_engine().context_lines, engine.context_lines);
    assert_eq!(stored.to_engine().extensions, None);
}
