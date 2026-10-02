//! Main application window: project list, project details, index
//! builds and their summaries.
//!
//! Layering: this crate only drives `rsearch-catalog` (projects,
//! persisted settings, build records) and `rsearch-engine`
//! (`rebuild_index` / `update_index` / `BuildHandle`). It never opens
//! a project index itself and never writes `projects.db` directly.

use std::time::Duration;

use eframe::egui;
use rsearch_catalog::{Catalog, Project, ProjectSettings};
use rsearch_engine::{BuildError, BuildHandle, BuildKind, BuildSummary, ProgressSnapshot};

use crate::editor::{Editor, EditorResult};
use crate::tr::{Strings, EN};
use crate::util;

/// Derived display state of a project — computed from the catalog row,
/// never stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    NeverBuilt,
    RebuildNeeded,
    UpToDate,
}

impl Status {
    fn text(self, tr: &Strings) -> &'static str {
        match self {
            Status::NeverBuilt => tr.status_never_built,
            Status::RebuildNeeded => tr.status_rebuild_needed,
            Status::UpToDate => tr.status_up_to_date,
        }
    }

    fn color(self) -> egui::Color32 {
        match self {
            Status::NeverBuilt => egui::Color32::GRAY,
            Status::RebuildNeeded => egui::Color32::GOLD,
            Status::UpToDate => egui::Color32::from_rgb(0x6C, 0xC7, 0x6C),
        }
    }
}

/// An in-flight index build for one project. The engine owns the
/// pipeline threads; the GUI only polls progress, cancels and collects
/// the result.
struct ActiveBuild {
    project_id: String,
    /// The settings actually handed to the engine; recorded on success.
    settings: ProjectSettings,
    handle: BuildHandle,
}

/// The single modal dialog currently open, if any.
enum Dialog {
    Editor(Box<Editor>),
    ConfirmDelete { id: String, name: String },
}

/// Mutations requested by UI widgets, applied after the drawing pass
/// so catalog calls never run inside widget closures.
enum Action {
    Select(String),
    NewProject,
    Edit(String),
    AskDelete(String),
    StartBuild(String),
    CancelBuild,
    RetryCatalog,
}

/// The rsearch application.
pub struct RsearchApp {
    /// Active text table — English for now; pointing this field at
    /// another `Strings` static switches the whole UI language.
    tr: &'static Strings,
    /// `None` when `projects.db` could not be opened (see
    /// `catalog_error`); the rest of the UI stays usable enough to
    /// show the error and offer a retry.
    catalog: Option<Catalog>,
    catalog_error: Option<String>,
    projects: Vec<Project>,
    selected: Option<String>,
    dialog: Option<Dialog>,
    build: Option<ActiveBuild>,
    /// Status-bar message: (is_error, text).
    notice: Option<(bool, String)>,
}

impl RsearchApp {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let mut app = RsearchApp {
            tr: &EN,
            catalog: None,
            catalog_error: None,
            projects: Vec::new(),
            selected: None,
            dialog: None,
            build: None,
            notice: None,
        };
        app.open_catalog();
        app
    }

    fn open_catalog(&mut self) {
        match Catalog::open_default() {
            Ok(catalog) => {
                self.catalog = Some(catalog);
                self.catalog_error = None;
                self.refresh();
            }
            Err(e) => {
                self.catalog = None;
                self.catalog_error = Some(e.to_string());
            }
        }
    }

    /// Rebuilds the project list from the catalog — the catalog is the
    /// single source of truth, the list is only a display cache.
    fn refresh(&mut self) {
        let Some(catalog) = &self.catalog else {
            return;
        };
        match catalog.list_projects() {
            Ok(projects) => {
                self.projects = projects;
                let still_there = self
                    .selected
                    .as_deref()
                    .is_some_and(|id| self.projects.iter().any(|p| p.id == id));
                if !still_there {
                    self.selected = None;
                }
            }
            Err(e) => self.notice = Some((true, e.to_string())),
        }
    }

    fn status(&self, p: &Project) -> Status {
        if p.last_build_settings.is_none() {
            Status::NeverBuilt
        } else if self.catalog.as_ref().is_some_and(|c| c.needs_rebuild(p)) {
            Status::RebuildNeeded
        } else {
            Status::UpToDate
        }
    }

    /// Polls the running build: repaints while it runs, collects the
    /// final result once the engine reports a terminal phase.
    fn poll_build(&mut self, ctx: &egui::Context) {
        let Some(active) = &self.build else {
            return;
        };
        // egui only repaints on input; keep ticking while a build runs.
        ctx.request_repaint_after(Duration::from_millis(100));
        let finished = active
            .handle
            .progress()
            .phase()
            .is_some_and(|p| p.is_terminal());
        if !finished {
            return;
        }
        // Terminal phase ⇒ the coordinator already stored its result;
        // `wait()` only joins the threads.
        let active = self.build.take().expect("build is Some");
        match active.handle.wait() {
            Ok(report) => {
                if let Some(catalog) = &self.catalog {
                    if let Err(e) = catalog.record_build_result(
                        &active.project_id,
                        &active.settings,
                        &report.summary,
                    ) {
                        self.notice = Some((true, e.to_string()));
                    }
                }
                self.notice = Some((
                    false,
                    self.tr
                        .build_completed(report.summary.indexed_files, report.summary.duration),
                ));
            }
            Err(BuildError::Cancelled { .. }) => {
                self.notice = Some((false, self.tr.build_cancelled.to_owned()));
            }
            Err(e) => {
                self.notice = Some((true, self.tr.build_failed(&e.to_string())));
            }
        }
        self.refresh();
    }

    fn start_build(&mut self, project_id: &str) {
        let Some(catalog) = &self.catalog else {
            return;
        };
        let project = match catalog.get_project(project_id) {
            Ok(p) => p,
            Err(e) => {
                self.notice = Some((true, e.to_string()));
                return;
            }
        };
        if let Err(msg) = project.settings.validate() {
            self.notice = Some((true, msg));
            return;
        }
        let settings = project.settings.clone();
        let options = settings.to_build_options();
        // A previous build exists: `update_index` reuses unchanged rows
        // and falls back to a full rebuild on its own when the index is
        // missing or incompatible.
        let use_update = project.last_build_settings.is_some() && project.index_db_path.exists();
        let handle = if use_update {
            rsearch_engine::update_index(&project.index_db_path, options)
        } else {
            rsearch_engine::rebuild_index(&project.index_db_path, options)
        };
        self.notice = None;
        self.build = Some(ActiveBuild {
            project_id: project.id,
            settings,
            handle,
        });
    }

    fn delete_project(&mut self, id: &str, name: &str) {
        let Some(catalog) = &self.catalog else {
            return;
        };
        match catalog.delete_project(id) {
            Ok(()) => {
                if self.selected.as_deref() == Some(id) {
                    self.selected = None;
                }
                self.notice = Some((false, self.tr.project_deleted(name)));
                self.refresh();
            }
            Err(e) => self.notice = Some((true, e.to_string())),
        }
    }

    /// Applies a submitted editor form: catalog `create_project`, or —
    /// in edit mode — `update_project_settings` and/or `rename_project`
    /// depending on what actually changed. A pure rename never touches
    /// settings, so it cannot trigger a rebuild flag.
    fn apply_editor(&mut self, ed: &mut Editor) -> Result<(), String> {
        let tr = self.tr;
        let name = ed.name.trim().to_owned();
        if name.is_empty() {
            return Err(tr.err_name_required.to_owned());
        }
        let settings = ed.settings();
        settings.validate()?;
        let catalog = self
            .catalog
            .as_ref()
            .ok_or_else(|| tr.catalog_unavailable.to_owned())?;

        match &ed.original {
            None => {
                let project = catalog
                    .create_project(name, settings)
                    .map_err(|e| e.to_string())?;
                self.notice = Some((false, tr.project_created(&project.name)));
                self.selected = Some(project.id);
            }
            Some(original) => {
                if settings != original.settings {
                    catalog
                        .update_project_settings(&original.id, settings)
                        .map_err(|e| e.to_string())?;
                }
                if name != original.name {
                    catalog
                        .rename_project(&original.id, &name)
                        .map_err(|e| e.to_string())?;
                }
                self.notice = Some((false, tr.project_updated.to_owned()));
            }
        }
        self.refresh();
        Ok(())
    }

    fn apply(&mut self, action: Action) {
        match action {
            Action::Select(id) => self.selected = Some(id),
            Action::NewProject => {
                self.dialog = Some(Dialog::Editor(Box::new(Editor::new_create())));
            }
            Action::Edit(id) => {
                if let Some(p) = self.projects.iter().find(|p| p.id == id) {
                    self.dialog = Some(Dialog::Editor(Box::new(Editor::new_edit(p))));
                }
            }
            Action::AskDelete(id) => {
                if let Some(p) = self.projects.iter().find(|p| p.id == id) {
                    self.dialog = Some(Dialog::ConfirmDelete {
                        id,
                        name: p.name.clone(),
                    });
                }
            }
            Action::StartBuild(id) => self.start_build(&id),
            Action::CancelBuild => {
                if let Some(b) = &self.build {
                    b.handle.cancel();
                }
            }
            Action::RetryCatalog => self.open_catalog(),
        }
    }
}

impl eframe::App for RsearchApp {
    /// Called before every `ui` pass — progress polling and result
    /// collection live here so they also run while the window is hidden.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_build(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let tr = self.tr;
        let mut action = None;

        if self.notice.is_some() {
            egui::Panel::bottom("notice").show(ui, |ui| {
                let (is_error, text) = self.notice.as_ref().expect("checked");
                if *is_error {
                    ui.colored_label(egui::Color32::LIGHT_RED, text);
                } else {
                    ui.label(text);
                }
            });
        }

        egui::Panel::left("projects")
            .resizable(true)
            .default_size(280.0)
            .show(ui, |ui| {
                ui.heading(tr.projects);
                ui.add_space(4.0);
                if ui
                    .add_enabled(self.catalog.is_some(), egui::Button::new(tr.new_project))
                    .clicked()
                {
                    action = Some(Action::NewProject);
                }
                ui.separator();
                self.project_list_ui(ui, &mut action);
            });

        egui::CentralPanel::default().show(ui, |ui| {
            if self.catalog.is_none() {
                ui.vertical_centered(|ui| {
                    ui.add_space(120.0);
                    let msg = self.catalog_error.clone().unwrap_or_default();
                    ui.colored_label(
                        egui::Color32::LIGHT_RED,
                        format!("{msg}: {}", tr.catalog_unavailable),
                    );
                    ui.add_space(8.0);
                    if ui.button(tr.retry).clicked() {
                        action = Some(Action::RetryCatalog);
                    }
                });
                return;
            }
            let selected = self
                .selected
                .as_deref()
                .and_then(|id| self.projects.iter().find(|p| p.id == id))
                .cloned();
            match selected {
                Some(project) => self.project_details_ui(ui, &project, &mut action),
                None => {
                    ui.vertical_centered(|ui| {
                        ui.add_space(120.0);
                        ui.weak(tr.select_project_hint);
                    });
                }
            }
        });

        self.dialog_ui(ui.ctx());

        if let Some(action) = action {
            self.apply(action);
        }
    }
}

// -- Project list -----------------------------------------------------------

impl RsearchApp {
    fn project_list_ui(&mut self, ui: &mut egui::Ui, action: &mut Option<Action>) {
        let tr = self.tr;
        if self.projects.is_empty() {
            ui.weak(tr.no_projects_hint);
            return;
        }
        egui::ScrollArea::vertical().show(ui, |ui| {
            for p in &self.projects {
                let status = self.status(p);
                let is_selected = self.selected.as_deref() == Some(p.id.as_str());
                if ui
                    .add_sized(
                        [ui.available_width(), 20.0],
                        egui::Button::selectable(
                            is_selected,
                            egui::RichText::new(&p.name).strong(),
                        ),
                    )
                    .clicked()
                {
                    *action = Some(Action::Select(p.id.clone()));
                }
                let date = p
                    .last_build_at
                    .map(util::format_unix)
                    .unwrap_or_else(|| "—".to_owned());
                ui.horizontal(|ui| {
                    ui.add_space(12.0);
                    ui.colored_label(status.color(), status.text(tr));
                    ui.weak(format!("· {date}"));
                });
                ui.add_space(6.0);
            }
        });
    }
}

// -- Project details -----------------------------------------------------------

impl RsearchApp {
    fn project_details_ui(
        &mut self,
        ui: &mut egui::Ui,
        project: &Project,
        action: &mut Option<Action>,
    ) {
        let tr = self.tr;
        let status = self.status(project);
        let busy = self.build.is_some();

        ui.horizontal(|ui| {
            ui.heading(&project.name);
            ui.colored_label(status.color(), format!("({})", status.text(tr)));
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            // A first build is `rebuild_index`; afterwards `update_index`
            // (it falls back to a full rebuild by itself when needed).
            let label = if project.last_build_settings.is_some() && project.index_db_path.exists() {
                tr.update_index
            } else {
                tr.build_index
            };
            if ui.add_enabled(!busy, egui::Button::new(label)).clicked() {
                *action = Some(Action::StartBuild(project.id.clone()));
            }
            if ui.add_enabled(!busy, egui::Button::new(tr.edit)).clicked() {
                *action = Some(Action::Edit(project.id.clone()));
            }
            if ui
                .add_enabled(!busy, egui::Button::new(tr.delete))
                .clicked()
            {
                *action = Some(Action::AskDelete(project.id.clone()));
            }
        });
        ui.separator();

        if let Some(active) = self.build.as_ref().filter(|b| b.project_id == project.id) {
            let snap = active.handle.progress().snapshot();
            Self::build_progress_ui(ui, tr, &snap, action);
            ui.separator();
        }

        egui::CollapsingHeader::new(tr.settings_section)
            .default_open(true)
            .show(ui, |ui| {
                Self::settings_ui(ui, tr, &project.settings);
            });

        match &project.last_build_summary {
            Some(summary) => {
                egui::CollapsingHeader::new(tr.last_build)
                    .default_open(true)
                    .show(ui, |ui| {
                        Self::summary_ui(ui, tr, summary);
                    });
            }
            None => {
                ui.add_space(4.0);
                ui.weak(tr.status_never_built);
            }
        }
    }

    fn build_progress_ui(
        ui: &mut egui::Ui,
        tr: &Strings,
        snap: &ProgressSnapshot,
        action: &mut Option<Action>,
    ) {
        ui.horizontal(|ui| {
            if !snap.phase.is_some_and(|p| p.is_terminal()) {
                ui.spinner();
            }
            ui.label(
                snap.phase
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| tr.starting.to_owned()),
            );
            if ui.button(tr.cancel_build).clicked() {
                *action = Some(Action::CancelBuild);
            }
        });
        egui::Grid::new(ui.id().with("progress"))
            .num_columns(4)
            .spacing([24.0, 4.0])
            .show(ui, |ui| {
                let pairs = [
                    (tr.files_seen, snap.files_seen.to_string()),
                    (tr.files_indexed, snap.files_indexed.to_string()),
                    (tr.files_ignored, snap.files_ignored.to_string()),
                    (tr.errors, snap.errors.to_string()),
                    (tr.archives, snap.archives.to_string()),
                    (tr.archive_entries, snap.archive_entries.to_string()),
                    (tr.bytes_read, util::format_bytes(snap.bytes_read)),
                ];
                for (i, (label, value)) in pairs.iter().enumerate() {
                    if i > 0 && i % 2 == 0 {
                        ui.end_row();
                    }
                    ui.weak(*label);
                    ui.label(value);
                }
                ui.end_row();
            });
    }

    fn settings_ui(ui: &mut egui::Ui, tr: &Strings, s: &ProjectSettings) {
        egui::Grid::new(ui.id().with("settings"))
            .num_columns(2)
            .spacing([32.0, 4.0])
            .show(ui, |ui| {
                ui.weak(tr.source_roots);
                ui.vertical(|ui| {
                    for root in &s.roots {
                        ui.label(format!(
                            "{}  ({})",
                            root.path.display(),
                            if root.recursive {
                                tr.root_recursive
                            } else {
                                tr.root_top_level_only
                            }
                        ));
                    }
                });
                ui.end_row();
                ui.weak(tr.excluded_dirs);
                ui.label(if s.excluded_dirs.is_empty() {
                    "—".to_owned()
                } else {
                    util::join_list(&s.excluded_dirs)
                });
                ui.end_row();
                ui.weak(tr.excluded_extensions);
                ui.label(if s.excluded_extensions.is_empty() {
                    "—".to_owned()
                } else {
                    util::join_list(&s.excluded_extensions)
                });
                ui.end_row();
                ui.weak(tr.respect_gitignore);
                ui.label(if s.respect_gitignore { tr.yes } else { tr.no });
                ui.end_row();
                ui.weak(tr.max_indexed_file_size);
                ui.label(util::format_bytes(s.max_indexed_file_size));
                ui.end_row();
                ui.weak(tr.index_archives);
                ui.label(if s.archives_enabled { tr.yes } else { tr.no });
                ui.end_row();
                if s.archives_enabled {
                    ui.weak(tr.archive_max_depth);
                    ui.label(s.archive_max_depth.to_string());
                    ui.end_row();
                }
            });
    }

    fn summary_ui(ui: &mut egui::Ui, tr: &Strings, s: &BuildSummary) {
        egui::Grid::new(ui.id().with("summary"))
            .num_columns(2)
            .spacing([32.0, 4.0])
            .show(ui, |ui| {
                let row = |ui: &mut egui::Ui, label: &str, value: String| {
                    ui.weak(label);
                    ui.label(value);
                    ui.end_row();
                };
                let kind = match s.kind {
                    BuildKind::Full => tr.kind_full.to_owned(),
                    BuildKind::Update => match &s.update_delta {
                        Some(d) => format!(
                            "{}  (+{} {} · −{} {} · ~{} {})",
                            tr.kind_update,
                            d.added,
                            tr.delta_added,
                            d.removed,
                            tr.delta_removed,
                            d.updated,
                            tr.delta_updated,
                        ),
                        None => tr.kind_update.to_owned(),
                    },
                };
                row(ui, tr.kind, kind);
                row(ui, tr.duration, util::format_duration(s.duration));
                row(ui, tr.files_indexed, s.indexed_files.to_string());
                let exts = s
                    .top_extensions
                    .iter()
                    .map(|(e, n)| format!("{e} ({n})"))
                    .collect::<Vec<_>>()
                    .join(", ");
                row(
                    ui,
                    tr.top_extensions,
                    if exts.is_empty() {
                        "—".to_owned()
                    } else {
                        exts
                    },
                );
                row(
                    ui,
                    tr.ignored_by_extension,
                    s.ignored_by_extension.to_string(),
                );
                row(ui, tr.ignored_by_sniff, s.ignored_by_sniff.to_string());
                row(ui, tr.too_large, s.too_large.to_string());
                row(ui, tr.errors, s.errors.to_string());
                row(ui, tr.security_limits, s.security_limits.to_string());
                row(ui, tr.archives_processed, s.archives_processed.to_string());
                row(
                    ui,
                    tr.archive_entries_indexed,
                    s.archive_entries_indexed.to_string(),
                );
                row(
                    ui,
                    tr.index_archives,
                    if s.archives_included {
                        tr.yes.to_owned()
                    } else {
                        tr.no.to_owned()
                    },
                );
            });
    }
}

// -- Modal dialogs -----------------------------------------------------------

impl RsearchApp {
    fn dialog_ui(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.dialog.take() else {
            return;
        };
        let tr = self.tr;
        match dialog {
            Dialog::Editor(mut ed) => match ed.show(ctx, tr) {
                EditorResult::Open => self.dialog = Some(Dialog::Editor(ed)),
                EditorResult::Cancelled => {}
                EditorResult::Submit => {
                    if let Err(msg) = self.apply_editor(&mut ed) {
                        ed.error = Some(msg);
                        self.dialog = Some(Dialog::Editor(ed));
                    }
                }
            },
            Dialog::ConfirmDelete { id, name } => {
                let mut open = true;
                let mut confirmed = false;
                let mut cancelled = false;
                egui::Window::new(tr.delete_project_title)
                    .open(&mut open)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .collapsible(false)
                    .resizable(false)
                    .show(ctx, |ui| {
                        ui.label(tr.delete_confirm(&name));
                        ui.add_space(4.0);
                        ui.weak(tr.delete_warning);
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            if ui.button(tr.delete).clicked() {
                                confirmed = true;
                            }
                            if ui.button(tr.cancel).clicked() {
                                cancelled = true;
                            }
                        });
                    });
                if confirmed {
                    self.delete_project(&id, &name);
                } else if open && !cancelled {
                    self.dialog = Some(Dialog::ConfirmDelete { id, name });
                }
            }
        }
    }
}
