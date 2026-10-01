//! User-facing per-project scan settings.
//!
//! Only the fields a user is expected to edit live here — roots with
//! their recursion policy, exclusions, size limit, archive on/off and
//! nesting depth. Low-level engine tuning (threads, batch sizes,
//! archive safety limits, SQLite pragmas) stays global to the
//! application and is never stored per project.

use rsearch_engine::{ArchiveOptions, BuildOptions, RootSpec};
use serde::{Deserialize, Serialize};

/// The persisted, user-editable scan configuration of a project.
///
/// Structural equality (`PartialEq`) is what `needs_rebuild` compares:
/// two serializations that differ only in JSON formatting describe the
/// same settings and must never trigger a spurious rebuild.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectSettings {
    /// Source directories with their recursion policy.
    pub roots: Vec<RootSpec>,
    /// Directory names excluded from the scan.
    pub excluded_dirs: Vec<String>,
    /// File extensions excluded from the scan (lowercase, no dot).
    pub excluded_extensions: Vec<String>,
    /// Whether `.gitignore` files are respected while scanning.
    pub respect_gitignore: bool,
    /// Maximum size of a file whose content is indexed.
    pub max_indexed_file_size: u64,
    /// Whether archive files are opened and their entries indexed.
    pub archives_enabled: bool,
    /// Maximum nesting depth for archives inside archives.
    pub archive_max_depth: u32,
}

impl Default for ProjectSettings {
    fn default() -> Self {
        let opts = BuildOptions::default();
        ProjectSettings {
            roots: Vec::new(),
            excluded_dirs: opts.excluded_dirs,
            excluded_extensions: opts.excluded_extensions,
            respect_gitignore: opts.respect_gitignore,
            max_indexed_file_size: opts.max_indexed_file_size,
            archives_enabled: opts.archives.enabled,
            archive_max_depth: opts.archives.max_depth,
        }
    }
}

impl ProjectSettings {
    /// Maps the stored settings onto engine [`BuildOptions`]. Fields
    /// not exposed per project (threads, batching, archive safety
    /// limits, SQLite tuning) keep the engine defaults.
    pub fn to_build_options(&self) -> BuildOptions {
        BuildOptions {
            source_directories: self.roots.clone(),
            excluded_dirs: self.excluded_dirs.clone(),
            excluded_extensions: self.excluded_extensions.clone(),
            respect_gitignore: self.respect_gitignore,
            max_indexed_file_size: self.max_indexed_file_size,
            archives: ArchiveOptions {
                enabled: self.archives_enabled,
                max_depth: self.archive_max_depth,
                ..ArchiveOptions::default()
            },
            ..BuildOptions::default()
        }
    }

    /// Validates the settings through the engine's own validation —
    /// this is where an ambiguous configuration (for example the same
    /// root listed with both recursion policies) is rejected with a
    /// clear error instead of being silently resolved later.
    pub fn validate(&self) -> Result<(), String> {
        self.to_build_options().validate()
    }
}
