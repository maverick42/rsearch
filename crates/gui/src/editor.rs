//! Project editor form: create or modify a project's settings.
//!
//! The form edits plain field values and produces a
//! [`ProjectSettings`] on submit; validation stays with
//! `ProjectSettings::validate` and the catalog — nothing is
//! re-checked here.

use std::path::PathBuf;

use eframe::egui;
use rsearch_catalog::{Project, ProjectSettings, RootSpec};

use crate::tr::Strings;
use crate::util;

/// One source-root row of the editor.
#[derive(Debug, Clone)]
struct RootRow {
    path: String,
    recursive: bool,
}

/// What the editor window decided this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorResult {
    /// Still open, keep showing it.
    Open,
    /// The user asked to save; the caller validates and applies.
    Submit,
    /// Closed or cancelled without saving.
    Cancelled,
}

/// The project form. In create mode `original` is `None`; in edit mode
/// it holds the project being modified so the caller can decide
/// between a pure rename and a settings update.
pub struct Editor {
    /// The project being edited; `None` when creating a new one.
    pub original: Option<Project>,
    pub name: String,
    roots: Vec<RootRow>,
    excluded_dirs: String,
    excluded_extensions: String,
    respect_gitignore: bool,
    max_size_mb: u64,
    archives_enabled: bool,
    archive_max_depth: u32,
    /// Last validation or catalog error, shown inside the dialog.
    pub error: Option<String>,
}

impl Editor {
    /// A blank form initialized with the engine-backed defaults.
    pub fn new_create() -> Self {
        Self::from_parts(None, "", &ProjectSettings::default())
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
            excluded_dirs: util::join_list(&s.excluded_dirs),
            excluded_extensions: util::join_list(&s.excluded_extensions),
            respect_gitignore: s.respect_gitignore,
            max_size_mb: s.max_indexed_file_size.div_ceil(MIB).max(1),
            archives_enabled: s.archives_enabled,
            archive_max_depth: s.archive_max_depth,
            error: None,
        }
    }

    /// Window title for this editor.
    fn title<'a>(&self, tr: &'a Strings) -> &'a str {
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
            excluded_dirs: util::parse_list(&self.excluded_dirs),
            excluded_extensions: util::parse_extensions(&self.excluded_extensions),
            respect_gitignore: self.respect_gitignore,
            max_indexed_file_size: self.max_size_mb.saturating_mul(1024 * 1024),
            archives_enabled: self.archives_enabled,
            archive_max_depth: self.archive_max_depth,
        }
    }

    /// Draws the editor window on `ctx` and reports the user's choice.
    pub fn show(&mut self, ctx: &egui::Context, tr: &Strings) -> EditorResult {
        let mut open = true;
        let mut result = EditorResult::Open;
        egui::Window::new(self.title(tr))
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .collapsible(false)
            .resizable(true)
            .default_size([560.0, 480.0])
            .show(ctx, |ui| {
                self.form_ui(ui, tr);
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button(tr.save).clicked() {
                        result = EditorResult::Submit;
                    }
                    if ui.button(tr.cancel).clicked() {
                        result = EditorResult::Cancelled;
                    }
                });
            });
        if !open && result == EditorResult::Open {
            EditorResult::Cancelled
        } else {
            result
        }
    }

    /// The scrollable form body.
    fn form_ui(&mut self, ui: &mut egui::Ui, tr: &Strings) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(tr.name);
                ui.add(
                    egui::TextEdit::singleline(&mut self.name)
                        .desired_width(300.0)
                        .hint_text(tr.project_name_hint),
                );
            });
            ui.add_space(8.0);
            ui.label(egui::RichText::new(tr.source_roots).strong());
            self.roots_ui(ui, tr);
            ui.add_space(8.0);
            ui.label(tr.excluded_dirs);
            ui.add(
                egui::TextEdit::multiline(&mut self.excluded_dirs)
                    .desired_rows(3)
                    .desired_width(f32::INFINITY)
                    .hint_text(tr.excluded_dirs_hint),
            );
            ui.add_space(4.0);
            ui.label(tr.excluded_extensions);
            ui.add(
                egui::TextEdit::singleline(&mut self.excluded_extensions)
                    .desired_width(f32::INFINITY)
                    .hint_text(tr.excluded_extensions_hint),
            );
            ui.add_space(8.0);
            ui.checkbox(&mut self.respect_gitignore, tr.respect_gitignore);
            ui.horizontal(|ui| {
                ui.label(tr.max_indexed_file_size);
                ui.add(
                    egui::DragValue::new(&mut self.max_size_mb)
                        .range(1..=1_048_576)
                        .suffix(" MiB"),
                );
            });
            ui.add_space(4.0);
            ui.checkbox(&mut self.archives_enabled, tr.index_archives);
            ui.add_enabled_ui(self.archives_enabled, |ui| {
                ui.horizontal(|ui| {
                    ui.add_space(20.0);
                    ui.label(tr.archive_max_depth);
                    ui.add(egui::DragValue::new(&mut self.archive_max_depth).range(0..=8));
                });
            });
            if let Some(err) = &self.error {
                ui.add_space(8.0);
                ui.colored_label(egui::Color32::LIGHT_RED, err);
            }
        });
    }

    /// The editable list of source roots with per-row browse and
    /// recursion checkbox.
    fn roots_ui(&mut self, ui: &mut egui::Ui, tr: &Strings) {
        let mut browse_at = None;
        let mut remove_at = None;
        for (i, row) in self.roots.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut row.path)
                        .desired_width(280.0)
                        .hint_text(tr.root_path_hint),
                );
                if ui.button(tr.browse).clicked() {
                    browse_at = Some(i);
                }
                ui.checkbox(&mut row.recursive, tr.root_recursive);
                if ui.button("✕").on_hover_text(tr.remove_root).clicked() {
                    remove_at = Some(i);
                }
            });
        }
        if let Some(i) = remove_at {
            self.roots.remove(i);
        }
        if let Some(i) = browse_at {
            if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                // The path is reopened by the engine — refuse non-Unicode
                // instead of storing a lossy rendering.
                match dir.to_str() {
                    Some(s) => self.roots[i].path = s.to_owned(),
                    None => self.error = Some(tr.err_non_unicode_path.to_owned()),
                }
            }
        }
        if ui.button(tr.add_root).clicked() {
            self.roots.push(RootRow {
                path: String::new(),
                recursive: true,
            });
        }
    }
}
