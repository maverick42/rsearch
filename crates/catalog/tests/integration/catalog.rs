//! Integration tests for the project catalog: lifecycle, settings
//! drift detection, rename semantics, deletion order and idempotent
//! serialization.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rsearch_catalog::{Catalog, CatalogError, ProjectSettings, RootSpec};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A temp directory that removes itself on drop.
struct TempDir {
    base: PathBuf,
    src: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> TempDir {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "rsearch-cat-{}-{}-{}",
            label,
            std::process::id(),
            id
        ));
        let _ = std::fs::remove_dir_all(&base);
        let src = base.join("src");
        std::fs::create_dir_all(&src).expect("create temp dir");
        TempDir { base, src }
    }

    fn catalog_path(&self) -> PathBuf {
        self.base.join("projects.db")
    }

    fn write(&self, rel: &str, content: &str) {
        let path = self.src.join(rel);
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn settings_for(root: &Path) -> ProjectSettings {
    ProjectSettings {
        roots: vec![RootSpec::new(root.to_path_buf())],
        ..ProjectSettings::default()
    }
}

/// Full lifecycle: create → build → summary stored → settings change
/// detected → rename is invisible to `needs_rebuild` → delete removes
/// both the row and the index files.
#[test]
fn project_lifecycle_end_to_end() {
    let dir = TempDir::new("lifecycle");
    dir.write("a.txt", "alpha lifecycle content");
    dir.write("b.md", "beta lifecycle content");
    let catalog = Catalog::open(dir.catalog_path()).expect("open catalog");

    // Create: settings validated, id-derived index path, never built.
    let settings = settings_for(&dir.src);
    let project = catalog
        .create_project("demo", settings.clone())
        .expect("create");
    assert_eq!(project.name, "demo");
    assert!(project.index_db_path.ends_with("index.db"));
    assert!(
        project
            .index_db_path
            .to_string_lossy()
            .contains(&project.id),
        "index path derives from id, not name"
    );
    assert!(catalog.needs_rebuild(&project), "never built");

    // Build: a real engine run against the project's index path, then
    // record the result.
    let report =
        rsearch_engine::rebuild_index(&project.index_db_path, project.settings.to_build_options())
            .wait()
            .expect("build");
    assert_eq!(report.summary.indexed_files, 2);
    catalog
        .record_build_result(&project.id, &project.settings, &report.summary)
        .expect("record");

    let project = catalog.get_project(&project.id).expect("reload");
    assert!(!catalog.needs_rebuild(&project), "freshly built");
    let summary = project.last_build_summary.expect("summary stored");
    assert_eq!(summary.indexed_files, 2);
    assert_eq!(
        summary.top_extensions,
        vec![("md".to_string(), 1), ("txt".to_string(), 1)]
    );
    assert!(project.last_build_at.is_some());
    assert_eq!(project.last_build_settings.as_ref(), Some(&settings));

    // A settings change drifts away from the last build snapshot.
    let mut drifted = settings.clone();
    drifted.max_indexed_file_size += 1;
    catalog
        .update_project_settings(&project.id, drifted)
        .expect("update settings");
    let project = catalog.get_project(&project.id).expect("reload");
    assert!(catalog.needs_rebuild(&project), "settings changed");

    // A rename alone never triggers a rebuild.
    catalog
        .rename_project(&project.id, "renamed")
        .expect("rename");
    let renamed = catalog.get_project(&project.id).expect("reload");
    assert_eq!(renamed.name, "renamed");
    assert_eq!(renamed.index_db_path, project.index_db_path);
    assert_eq!(
        catalog.needs_rebuild(&renamed),
        catalog.needs_rebuild(&project),
        "rename must not change needs_rebuild"
    );

    // Delete: files first, row second — nothing remains anywhere.
    let index_dir = project.index_db_path.parent().unwrap().to_path_buf();
    catalog.delete_project(&project.id).expect("delete");
    assert!(!index_dir.exists(), "index directory removed");
    assert!(matches!(
        catalog.get_project(&project.id),
        Err(CatalogError::NotFound(_))
    ));
    assert!(catalog.list_projects().unwrap().is_empty());
}

/// Structural settings comparison: a settings_json rewritten with
/// different key order or whitespace describes the same settings and
/// must not trigger a rebuild.
#[test]
fn reformatted_settings_json_does_not_trigger_rebuild() {
    let dir = TempDir::new("reformat");
    dir.write("a.txt", "alpha content");
    let catalog = Catalog::open(dir.catalog_path()).expect("open");

    let project = catalog
        .create_project("demo", settings_for(&dir.src))
        .expect("create");
    let report = rsearch_engine::rebuild_index(
        &project.index_db_path,
        settings_for(&dir.src).to_build_options(),
    )
    .wait()
    .expect("build");
    catalog
        .record_build_result(&project.id, &project.settings, &report.summary)
        .expect("record");
    assert!(!catalog.needs_rebuild(&catalog.get_project(&project.id).unwrap()));

    // Rewrite last_build_settings_json with reordered keys through a
    // second connection — same settings, different serialization.
    let settings = settings_for(&dir.src);
    let reordered = format!(
        "{{\"archive_max_depth\":{},\"archives_enabled\":{},\"max_indexed_file_size\":{},\"respect_gitignore\":{},\"exclude_masks\":{},\"include_masks\":{},\"excluded_dirs\":{},\"roots\":{}}}",
        settings.archive_max_depth,
        settings.archives_enabled,
        settings.max_indexed_file_size,
        settings.respect_gitignore,
        serde_json::to_string(&settings.exclude_masks).unwrap(),
        serde_json::to_string(&settings.include_masks).unwrap(),
        serde_json::to_string(&settings.excluded_dirs).unwrap(),
        serde_json::to_string(&settings.roots).unwrap(),
    );
    let conn = rusqlite::Connection::open(dir.catalog_path()).unwrap();
    conn.execute(
        "UPDATE projects SET last_build_settings_json = ?2 WHERE id = ?1",
        rusqlite::params![project.id, reordered],
    )
    .unwrap();
    drop(conn);

    let project = catalog.get_project(&project.id).expect("reload");
    assert!(
        !catalog.needs_rebuild(&project),
        "a re-serialized identical settings object must not drift"
    );
}

/// Changing a project's name masks drifts away from the last build
/// snapshot: the masks define what the index contains, so a rebuild is
/// required. (Search masks live in saved searches, not here — they
/// never trigger a rebuild.)
#[test]
fn mask_settings_changes_trigger_rebuild() {
    let dir = TempDir::new("masks-rebuild");
    dir.write("a.txt", "alpha content");
    let catalog = Catalog::open(dir.catalog_path()).expect("open");

    let project = catalog
        .create_project("demo", settings_for(&dir.src))
        .expect("create");
    let report = rsearch_engine::rebuild_index(
        &project.index_db_path,
        settings_for(&dir.src).to_build_options(),
    )
    .wait()
    .expect("build");
    catalog
        .record_build_result(&project.id, &project.settings, &report.summary)
        .expect("record");
    assert!(!catalog.needs_rebuild(&catalog.get_project(&project.id).unwrap()));

    // Adding an include mask changes the index's membership.
    let mut drifted = settings_for(&dir.src);
    drifted.include_masks.push("*.rs".to_string());
    catalog
        .update_project_settings(&project.id, drifted)
        .expect("update settings");
    assert!(catalog.needs_rebuild(&catalog.get_project(&project.id).unwrap()));

    // Back to the built settings: no drift.
    catalog
        .update_project_settings(&project.id, settings_for(&dir.src))
        .expect("update settings");
    assert!(!catalog.needs_rebuild(&catalog.get_project(&project.id).unwrap()));

    // An exclude mask change triggers the rebuild need as well.
    let mut drifted = settings_for(&dir.src);
    drifted.exclude_masks.push("*.log".to_string());
    catalog
        .update_project_settings(&project.id, drifted)
        .expect("update settings");
    assert!(catalog.needs_rebuild(&catalog.get_project(&project.id).unwrap()));
}

/// The same root with both recursion policies is a configuration
/// conflict rejected at project creation, before any build exists.
#[test]
fn conflicting_recursive_roots_are_rejected_at_creation() {
    let dir = TempDir::new("conflict");
    let catalog = Catalog::open(dir.catalog_path()).expect("open");

    let mut settings = settings_for(&dir.src);
    settings
        .roots
        .push(RootSpec::non_recursive(dir.src.clone()));
    let err = catalog
        .create_project("bad", settings)
        .expect_err("conflict must be rejected");
    match err {
        CatalogError::InvalidSettings(m) => assert!(m.contains("recursive"), "{m}"),
        other => panic!("expected InvalidSettings, got {other:?}"),
    }
    assert!(catalog.list_projects().unwrap().is_empty());
}

/// A missing index file on disk forces `needs_rebuild` even when
/// settings are unchanged.
#[test]
fn deleted_index_file_triggers_rebuild() {
    let dir = TempDir::new("missing-index");
    dir.write("a.txt", "alpha content");
    let catalog = Catalog::open(dir.catalog_path()).expect("open");

    let project = catalog
        .create_project("demo", settings_for(&dir.src))
        .expect("create");
    let report = rsearch_engine::rebuild_index(
        &project.index_db_path,
        settings_for(&dir.src).to_build_options(),
    )
    .wait()
    .expect("build");
    catalog
        .record_build_result(&project.id, &project.settings, &report.summary)
        .expect("record");

    std::fs::remove_file(&project.index_db_path).unwrap();
    let project = catalog.get_project(&project.id).expect("reload");
    assert!(catalog.needs_rebuild(&project));
}

/// Two projects are fully independent: distinct ids, distinct index
/// paths, distinct rows.
#[test]
fn two_projects_have_distinct_ids_and_paths() {
    let dir = TempDir::new("two-projects");
    dir.write("a.txt", "alpha content");
    let catalog = Catalog::open(dir.catalog_path()).expect("open");

    let a = catalog
        .create_project("one", settings_for(&dir.src))
        .expect("create a");
    let b = catalog
        .create_project("two", settings_for(&dir.src))
        .expect("create b");

    assert_ne!(a.id, b.id);
    assert_ne!(a.index_db_path, b.index_db_path);
    assert_eq!(catalog.list_projects().unwrap().len(), 2);
}

/// Operations on a missing id report `NotFound` instead of silently
/// succeeding.
#[test]
fn missing_project_reports_not_found() {
    let dir = TempDir::new("missing");
    let catalog = Catalog::open(dir.catalog_path()).expect("open");
    let settings = settings_for(&dir.src);

    assert!(matches!(
        catalog.rename_project("nope", "x"),
        Err(CatalogError::NotFound(_))
    ));
    assert!(matches!(
        catalog.update_project_settings("nope", settings.clone()),
        Err(CatalogError::NotFound(_))
    ));
    assert!(matches!(
        catalog.delete_project("nope"),
        Err(CatalogError::NotFound(_))
    ));
}
