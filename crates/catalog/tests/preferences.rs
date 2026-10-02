//! Integration tests for global application preferences: JSON-file
//! persistence, missing/old/corrupt file handling and normalization.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use rsearch_catalog::{AppPreferences, Catalog, CatalogError, Language, ThemePreference};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A temp directory that removes itself on drop.
struct TempDir {
    base: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> TempDir {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "rsearch-prefs-{}-{}-{}",
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

    fn prefs_path(&self) -> PathBuf {
        self.base.join("preferences.json")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// No file on disk means the application defaults.
#[test]
fn missing_file_yields_defaults() {
    let dir = TempDir::new("missing");
    let catalog = Catalog::open(dir.catalog_path()).expect("open");
    let prefs = catalog.load_preferences().expect("load");
    assert_eq!(prefs, AppPreferences::default());
    assert_eq!(prefs.language, Language::English);
    assert_eq!(prefs.theme, ThemePreference::System);
    assert!(!prefs.default_excluded_dirs.is_empty());
}

/// Save then load reproduces every field.
#[test]
fn save_then_load_round_trips() {
    let dir = TempDir::new("roundtrip");
    let catalog = Catalog::open(dir.catalog_path()).expect("open");

    let prefs = AppPreferences {
        language: Language::French,
        theme: ThemePreference::Dark,
        default_excluded_dirs: vec!["target".to_string(), "node_modules".to_string()],
        default_excluded_extensions: vec!["log".to_string()],
        default_max_indexed_file_size: 4 * 1024 * 1024,
        check_for_updates: false,
        ..AppPreferences::default()
    };
    catalog.save_preferences(&prefs).expect("save");
    assert!(
        dir.prefs_path().exists(),
        "file written next to projects.db"
    );

    let loaded = catalog.load_preferences().expect("reload");
    assert_eq!(loaded, prefs);
}

/// A file from an older version: unknown keys are ignored and missing
/// fields fall back to the defaults.
#[test]
fn old_partial_file_loads_with_defaults() {
    let dir = TempDir::new("old");
    let catalog = Catalog::open(dir.catalog_path()).expect("open");
    std::fs::write(
        dir.prefs_path(),
        "{\"language\":\"es\",\"future_setting\":{\"a\":1}}",
    )
    .unwrap();

    let prefs = catalog.load_preferences().expect("load");
    assert_eq!(prefs.language, Language::Spanish);
    assert_eq!(
        prefs,
        AppPreferences {
            language: Language::Spanish,
            ..AppPreferences::default()
        }
    );
}

/// A damaged file reports an error instead of silently resetting the
/// user's preferences.
#[test]
fn corrupt_file_reports_error() {
    let dir = TempDir::new("corrupt");
    let catalog = Catalog::open(dir.catalog_path()).expect("open");
    std::fs::write(dir.prefs_path(), "{not json").unwrap();
    assert!(matches!(
        catalog.load_preferences(),
        Err(CatalogError::Serialize(_))
    ));
}

/// Lists are normalized on save: trimmed, deduplicated, extensions
/// lowercased without their leading dot.
#[test]
fn save_normalizes_lists() {
    let dir = TempDir::new("normalize");
    let catalog = Catalog::open(dir.catalog_path()).expect("open");

    let prefs = AppPreferences {
        default_excluded_dirs: vec![
            " target ".to_string(),
            "target".to_string(),
            "".to_string(),
            "build".to_string(),
        ],
        default_excluded_extensions: vec![
            ".LOG".to_string(),
            "tmp".to_string(),
            ".log".to_string(),
        ],
        ..AppPreferences::default()
    };
    catalog.save_preferences(&prefs).expect("save");
    let loaded = catalog.load_preferences().expect("reload");
    assert_eq!(
        loaded.default_excluded_dirs,
        vec!["target".to_string(), "build".to_string()]
    );
    assert_eq!(
        loaded.default_excluded_extensions,
        vec!["log".to_string(), "tmp".to_string()]
    );
}

/// Language codes are stable storage values.
#[test]
fn language_serializes_as_codes() {
    assert_eq!(serde_json::to_string(&Language::English).unwrap(), "\"en\"");
    assert_eq!(serde_json::to_string(&Language::French).unwrap(), "\"fr\"");
    assert_eq!(serde_json::to_string(&Language::Spanish).unwrap(), "\"es\"");
    assert_eq!(
        serde_json::from_str::<Language>("\"fr\"").unwrap(),
        Language::French
    );
}
