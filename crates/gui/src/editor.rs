//! Project editor form: create or modify a project's settings.
//!
//! The form edits plain field values and produces a
//! [`ProjectSettings`] on submit; validation stays with
//! `ProjectSettings::validate` and the catalog — nothing is
//! re-checked here.

use std::path::PathBuf;

use iced::widget::{
    button, checkbox, column, row, scrollable, slider, text, text_editor, text_input,
};
use iced::{Alignment, Element, Fill};
use rsearch_catalog::{AppPreferences, Project, ProjectSettings, RootSpec};

use crate::app::Message;
use crate::tr::Strings;
use crate::util;

/// One source-root row of the editor.
#[derive(Debug, Clone)]
struct RootRow {
    path: String,
    recursive: bool,
}

/// The project form. In create mode `original` is `None`; in edit mode
/// it holds the project being modified so the caller can decide
/// between a pure rename and a settings update.
pub struct Editor {
    /// The project being edited; `None` when creating a new one.
    pub original: Option<Project>,
    pub name: String,
    roots: Vec<RootRow>,
    /// Multiline buffer for `excluded_dirs`.
    excluded_dirs: text_editor::Content,
    excluded_extensions: String,
    respect_gitignore: bool,
    /// Display buffer in MiB; invalid text fails `settings()` so the
    /// caller reports it instead of silently clamping.
    max_size_text: String,
    archives_enabled: bool,
    archive_max_depth: u32,
    /// Last validation or catalog error, shown inside the dialog.
    pub error: Option<String>,
}

impl Editor {
    /// A blank form initialized with the global preference defaults
    /// (exclusions, max file size) over the engine-backed defaults.
    /// Existing projects are never affected by later preference edits.
    pub fn new_create(prefs: &AppPreferences) -> Self {
        let settings = ProjectSettings {
            excluded_dirs: prefs.default_excluded_dirs.clone(),
            excluded_extensions: prefs.default_excluded_extensions.clone(),
            max_indexed_file_size: prefs.default_max_indexed_file_size,
            ..ProjectSettings::default()
        };
        Self::from_parts(None, "", &settings)
    }

    /// A form prefilled with an existing project's name and settings.
    pub fn new_edit(project: &Project) -> Self {
        Self::from_parts(Some(project.clone()), &project.name, &project.settings)
    }

    fn from_parts(original: Option<Project>, name: &str, s: &ProjectSettings) -> Self {
        const MIB: u64 = 1024 * 1024;
        Editor {
            original,
            name: name.to_owned(),
            roots: s
                .roots
                .iter()
                .map(|r| RootRow {
                    // Settings always round-trip through JSON, which
                    // only accepts Unicode paths — `to_str` cannot be
                    // `None` here, and a lossy rendering is never used.
                    path: r.path.to_str().unwrap_or_default().to_owned(),
                    recursive: r.recursive,
                })
                .collect(),
            excluded_dirs: text_editor::Content::with_text(&s.excluded_dirs.join("\n")),
            excluded_extensions: util::join_list(&s.excluded_extensions),
            respect_gitignore: s.respect_gitignore,
            max_size_text: s.max_indexed_file_size.div_ceil(MIB).max(1).to_string(),
            archives_enabled: s.archives_enabled,
            archive_max_depth: s.archive_max_depth,
            error: None,
        }
    }

    /// Dialog title for this editor.
    pub fn title<'a>(&self, tr: &'a Strings) -> &'a str {
        if self.original.is_some() {
            tr.edit_project_title
        } else {
            tr.new_project_title
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
            excluded_dirs: util::parse_list(&self.excluded_dirs.text()),
            excluded_extensions: util::parse_extensions(&self.excluded_extensions),
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

    /// Mutators driven by messages — one per editable field.
    pub fn set_name(&mut self, name: String) {
        self.name = name;
    }

    pub fn set_root_path(&mut self, index: usize, path: String) {
        if let Some(row) = self.roots.get_mut(index) {
            row.path = path;
        }
    }

    pub fn set_root_recursive(&mut self, index: usize, recursive: bool) {
        if let Some(row) = self.roots.get_mut(index) {
            row.recursive = recursive;
        }
    }

    /// Fills `roots[index]` from a native folder dialog. The path is
    /// reopened by the engine — non-Unicode paths are refused instead
    /// of storing a lossy rendering.
    pub fn browse_root(&mut self, index: usize, tr: &Strings) {
        if self.roots.get(index).is_none() {
            return;
        }
        if let Some(dir) = rfd::FileDialog::new().pick_folder() {
            match dir.to_str() {
                Some(s) => self.roots[index].path = s.to_owned(),
                None => self.error = Some(tr.err_non_unicode_path.to_owned()),
            }
        }
    }

    pub fn remove_root(&mut self, index: usize) {
        if index < self.roots.len() {
            self.roots.remove(index);
        }
    }

    pub fn add_root(&mut self) {
        self.roots.push(RootRow {
            path: String::new(),
            recursive: true,
        });
    }

    pub fn edit_excluded_dirs(&mut self, action: text_editor::Action) {
        self.excluded_dirs.perform(action);
    }

    pub fn set_excluded_extensions(&mut self, text: String) {
        self.excluded_extensions = text;
    }

    pub fn set_respect_gitignore(&mut self, value: bool) {
        self.respect_gitignore = value;
    }

    pub fn set_max_size_text(&mut self, text: String) {
        self.max_size_text = text;
    }

    pub fn set_archives_enabled(&mut self, value: bool) {
        self.archives_enabled = value;
    }

    pub fn set_archive_max_depth(&mut self, value: u32) {
        self.archive_max_depth = value;
    }

    /// The scrollable form body + footer actions, drawn as a dialog
    /// card by the caller.
    pub fn view(&self, tr: &Strings) -> Element<'_, Message> {
        let mut form = column![
            row![
                text(tr.name).width(90.0),
                text_input(tr.project_name_hint, &self.name)
                    .width(300.0)
                    .on_input(Message::EditorName),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
            text(tr.source_roots).font(bold()),
            self.roots_view(tr),
            text(tr.excluded_dirs),
            text_editor(&self.excluded_dirs)
                .placeholder(tr.excluded_dirs_hint)
                .height(84.0)
                .on_action(Message::EditorExcludedDirs),
            text(tr.excluded_extensions),
            text_input(tr.excluded_extensions_hint, &self.excluded_extensions)
                .width(Fill)
                .on_input(Message::EditorExcludedExts),
            checkbox(self.respect_gitignore)
                .label(tr.respect_gitignore)
                .on_toggle(Message::EditorGitignore),
            row![
                text(tr.max_indexed_file_size),
                text_input("0", &self.max_size_text)
                    .width(120.0)
                    .on_input(Message::EditorMaxSize),
                text("MiB").style(crate::app::theme::weak),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
            checkbox(self.archives_enabled)
                .label(tr.index_archives)
                .on_toggle(Message::EditorArchives),
        ]
        .spacing(10);

        if self.archives_enabled {
            form = form.push(
                row![
                    text(tr.archive_max_depth),
                    slider(0..=8, self.archive_max_depth, Message::EditorArchiveDepth).width(160.0),
                    text(self.archive_max_depth.to_string()).width(24.0),
                ]
                .spacing(10)
                .align_y(Alignment::Center),
            );
        }
        if let Some(err) = &self.error {
            form = form.push(text(err.clone()).style(iced::widget::text::danger));
        }

        column![
            scrollable(form).height(Fill),
            row![
                button(text(tr.save))
                    .style(button::primary)
                    .on_press(Message::EditorSubmit),
                button(text(tr.cancel)).on_press(Message::DialogCancel),
            ]
            .spacing(8),
        ]
        .spacing(10)
        .into()
    }

    /// The editable list of source roots with per-row browse and
    /// recursion checkbox.
    fn roots_view(&self, tr: &Strings) -> Element<'_, Message> {
        let mut col = column![].spacing(6);
        for (i, row_) in self.roots.iter().enumerate() {
            col = col.push(
                row![
                    text_input(tr.root_path_hint, &row_.path)
                        .width(300.0)
                        .on_input(move |s| Message::EditorRootPath(i, s)),
                    button(text(tr.browse)).on_press(Message::EditorBrowse(i)),
                    checkbox(row_.recursive)
                        .label(tr.root_recursive)
                        .on_toggle(move |v| Message::EditorRootRecursive(i, v)),
                    iced::widget::tooltip(
                        button(text("✕"))
                            .style(button::danger)
                            .padding([4.0, 8.0])
                            .on_press(Message::EditorRemoveRoot(i)),
                        tr.remove_root,
                        iced::widget::tooltip::Position::Top,
                    ),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            );
        }
        col.push(
            button(text(tr.add_root))
                .style(button::secondary)
                .on_press(Message::EditorAddRoot),
        )
        .into()
    }
}

/// The bold face used for section labels.
fn bold() -> iced::Font {
    iced::Font {
        weight: iced::font::Weight::Bold,
        ..iced::Font::DEFAULT
    }
}
