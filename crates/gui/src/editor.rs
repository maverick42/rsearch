//! Project editor form values: create or modify a project's settings.
//!
//! The dialog keeps its editable fields in the Slint properties; this
//! module holds the toolkit-independent part — the roots list the UI
//! cannot own (it is mutated by callbacks, not typed bindings) and the
//! conversion to [`ProjectSettings`]. Validation stays with
//! `ProjectSettings::validate` and the catalog — nothing is
//! re-checked here.

use std::path::PathBuf;

use rsearch_catalog::{AppPreferences, Project, ProjectSettings, RootSpec};

/// One source-root row of the editor.
#[derive(Debug, Clone)]
pub struct RootEdit {
    pub path: String,
    pub recursive: bool,
}

/// The editor form's field values. `original` identifies the project
/// being edited (`None` in create mode) so the caller can decide
/// between a pure rename and a settings update.
pub struct EditorValues {
    /// The project being edited; `None` when creating a new one.
    pub original: Option<Project>,
    pub name: String,
    pub roots: Vec<RootEdit>,
    /// Multiline text for `excluded_dirs`.
    pub excluded_dirs_text: String,
    pub excluded_exts_text: String,
    pub respect_gitignore: bool,
    /// Display buffer in MiB; invalid text produces size 0 so
    /// validation reports it instead of silently clamping.
    pub max_size_text: String,
    pub archives_enabled: bool,
    pub archive_max_depth: u32,
}

impl EditorValues {
    /// A blank form initialized with the global preference defaults
    /// (exclusions, max file size) over the engine-backed defaults.
    /// Existing projects are never affected by later preference edits.
    pub fn for_create(prefs: &AppPreferences) -> Self {
        let settings = ProjectSettings {
            excluded_dirs: prefs.default_excluded_dirs.clone(),
            excluded_extensions: prefs.default_excluded_extensions.clone(),
            max_indexed_file_size: prefs.default_max_indexed_file_size,
            ..ProjectSettings::default()
        };
        Self::from_parts(None, "", &settings)
    }

    /// A form prefilled with an existing project's name and settings.
    pub fn for_edit(project: &Project) -> Self {
        Self::from_parts(Some(project.clone()), &project.name, &project.settings)
    }

    fn from_parts(original: Option<Project>, name: &str, s: &ProjectSettings) -> Self {
        const MIB: u64 = 1024 * 1024;
        EditorValues {
            original,
            name: name.to_owned(),
            roots: s
                .roots
                .iter()
                .map(|r| RootEdit {
                    // Settings always round-trip through JSON, which
                    // only accepts Unicode paths — `to_str` cannot be
                    // `None` here, and a lossy rendering is never used.
                    path: r.path.to_str().unwrap_or_default().to_owned(),
                    recursive: r.recursive,
                })
                .collect(),
            excluded_dirs_text: s.excluded_dirs.join("\n"),
            excluded_exts_text: crate::util::join_list(&s.excluded_extensions),
            respect_gitignore: s.respect_gitignore,
            max_size_text: s.max_indexed_file_size.div_ceil(MIB).max(1).to_string(),
            archives_enabled: s.archives_enabled,
            archive_max_depth: s.archive_max_depth,
        }
    }

    /// Builds [`ProjectSettings`] from the current field values. Never
    /// fails; validating the result is the caller's job.
    pub fn settings(&self) -> ProjectSettings {
        ProjectSettings {
            roots: self
                .roots
                .iter()
                .filter(|r| !r.path.trim().is_empty())
                .map(|r| RootSpec {
                    path: PathBuf::from(r.path.trim()),
                    recursive: r.recursive,
                })
                .collect(),
            excluded_dirs: crate::util::parse_list(&self.excluded_dirs_text),
            excluded_extensions: crate::util::parse_extensions(&self.excluded_exts_text),
            respect_gitignore: self.respect_gitignore,
            max_indexed_file_size: self
                .max_size_text
                .trim()
                .parse::<u64>()
                .unwrap_or(0)
                .saturating_mul(1024 * 1024),
            archives_enabled: self.archives_enabled,
            archive_max_depth: self.archive_max_depth,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_skip_blank_roots_and_parse_lists() {
        let mut values = EditorValues::for_create(&AppPreferences::default());
        values.name = "proj".into();
        values.roots = vec![
            RootEdit {
                path: "  ".into(),
                recursive: true,
            },
            RootEdit {
                path: " C:\\src ".into(),
                recursive: false,
            },
        ];
        values.excluded_dirs_text = "target, build\n.git".into();
        values.excluded_exts_text = ".LOG; tmp".into();
        values.max_size_text = "12".into();
        let s = values.settings();
        assert_eq!(s.roots.len(), 1);
        assert_eq!(s.roots[0].path, PathBuf::from("C:\\src"));
        assert!(!s.roots[0].recursive);
        assert_eq!(s.excluded_dirs, vec!["target", "build", ".git"]);
        assert_eq!(s.excluded_extensions, vec!["log", "tmp"]);
        assert_eq!(s.max_indexed_file_size, 12 * 1024 * 1024);
    }

    #[test]
    fn invalid_max_size_becomes_zero_for_validation() {
        let mut values = EditorValues::for_create(&AppPreferences::default());
        values.max_size_text = "abc".into();
        assert_eq!(values.settings().max_indexed_file_size, 0);
    }

    #[test]
    fn for_edit_round_trips_project() {
        let project = Project {
            id: "id".into(),
            name: "name".into(),
            created_at: 0,
            settings: ProjectSettings {
                roots: vec![RootSpec::new("C:\\src")],
                max_indexed_file_size: 7 * 1024 * 1024,
                archives_enabled: true,
                archive_max_depth: 3,
                ..ProjectSettings::default()
            },
            last_build_settings: None,
            last_build_summary: None,
            last_build_at: None,
            index_db_path: PathBuf::from("index.db"),
        };
        let values = EditorValues::for_edit(&project);
        assert_eq!(values.name, "name");
        assert_eq!(values.roots.len(), 1);
        assert!(values.archives_enabled);
        assert_eq!(values.archive_max_depth, 3);
        assert_eq!(values.settings(), project.settings);
    }
}
