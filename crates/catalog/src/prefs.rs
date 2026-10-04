//! Global application preferences.
//!
//! Unlike [`crate::ProjectSettings`], which describe one project's
//! scan configuration, [`AppPreferences`] are application-wide. They
//! live in a single small JSON file next to `projects.db` — no extra
//! database for a handful of values:
//!
//! ```text
//! <exe dir>/projects.db       <- the catalog
//! <exe dir>/preferences.json  <- this file
//! ```
//!
//! The document uses serde defaults, so a file written by an older
//! version loads unchanged (missing fields fall back to
//! [`AppPreferences::default`]) and keys written by a newer version
//! are ignored.

use serde::{Deserialize, Serialize};

/// Name of the preferences file inside the catalog base directory.
pub const PREFERENCES_FILE_NAME: &str = "preferences.json";

/// Version of the serialized [`AppPreferences`] document.
pub const PREFERENCES_VERSION: u32 = 3;

/// UI language. Stored as a stable ISO code; English is the default
/// and reference language of the application.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Language {
    /// English — the default.
    #[default]
    #[serde(rename = "en")]
    English,
    /// French.
    #[serde(rename = "fr")]
    French,
    /// Spanish.
    #[serde(rename = "es")]
    Spanish,
}

impl Language {
    /// Every supported language, in menu order.
    pub const ALL: [Language; 3] = [Language::English, Language::French, Language::Spanish];

    /// Stable stored code (`en`, `fr`, `es`).
    pub fn code(self) -> &'static str {
        match self {
            Language::English => "en",
            Language::French => "fr",
            Language::Spanish => "es",
        }
    }

    /// The language's own name — always displayed untranslated.
    pub fn native_name(self) -> &'static str {
        match self {
            Language::English => "English",
            Language::French => "Français",
            Language::Spanish => "Español",
        }
    }
}

/// Light/dark theme preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ThemePreference {
    /// Follow the operating system (default).
    #[default]
    #[serde(rename = "system")]
    System,
    /// Always light.
    #[serde(rename = "light")]
    Light,
    /// Always dark.
    #[serde(rename = "dark")]
    Dark,
}

/// Application-wide preferences.
///
/// The `default_*` fields seed the settings of *newly created*
/// projects; they never retroactively modify an existing project's
/// own configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppPreferences {
    /// Serialization schema version.
    pub schema_version: u32,
    /// UI language.
    pub language: Language,
    /// Theme preference.
    pub theme: ThemePreference,
    /// Directory names excluded by default when a project is created.
    pub default_excluded_dirs: Vec<String>,
    /// File-name masks included by default when a project is created
    /// (`*`/`?` wildcards, file names only). Empty = every file.
    pub default_include_masks: Vec<String>,
    /// File-name masks excluded by default when a project is created.
    pub default_exclude_masks: Vec<String>,
    /// Default maximum size of a file whose content is indexed.
    pub default_max_indexed_file_size: u64,
    /// Whether the application may look for updates on its own.
    pub check_for_updates: bool,
    /// Project of the most recent search activity — the startup tab
    /// seeds on it so the saved-searches combo picks up where the
    /// user left off. `None` until a project is picked for a search.
    pub last_search_project_id: Option<String>,
}

impl Default for AppPreferences {
    fn default() -> Self {
        AppPreferences {
            schema_version: PREFERENCES_VERSION,
            language: Language::English,
            theme: ThemePreference::System,
            default_excluded_dirs: rsearch_engine::options::default_excluded_dirs(),
            default_include_masks: Vec::new(),
            default_exclude_masks: Vec::new(),
            default_max_indexed_file_size: rsearch_engine::BuildOptions::default()
                .max_indexed_file_size,
            check_for_updates: true,
            last_search_project_id: None,
        }
    }
}

impl AppPreferences {
    /// Normalizes editable list fields: trims entries, drops empties
    /// and removes duplicates while keeping the user's order. Masks
    /// are never case-normalized — matching is case-insensitive at
    /// match time.
    pub fn normalize(&mut self) {
        fn clean(list: &mut Vec<String>) {
            let mut seen = std::collections::HashSet::new();
            list.retain_mut(|item| {
                *item = item.trim().to_owned();
                !item.is_empty() && seen.insert(item.clone())
            });
        }
        clean(&mut self.default_excluded_dirs);
        clean(&mut self.default_include_masks);
        clean(&mut self.default_exclude_masks);
    }
}
