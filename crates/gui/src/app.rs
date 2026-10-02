//! Main application shell: left navigation (Search / Projects /
//! Preferences), a contextual banner strip on top of the content, and
//! the modal dialogs.
//!
//! Layering: this crate only drives `rsearch-catalog` (projects,
//! persisted settings, saved searches, preferences, build records) and
//! `rsearch-engine` (`rebuild_index` / `update_index` / `search`). It
//! never opens a project index itself and never writes `projects.db`
//! directly.
//!
//! The context decision for banners lives in a single place:
//! [`RsearchApp::banners`]. Widgets only collect [`Action`]s, which are
//! applied after the drawing pass.

mod banner;
mod prefs;
mod projects;
mod search;
mod search_job;
mod theme;
mod update;

use std::time::{Duration, Instant};

use eframe::egui;
use rsearch_catalog::{AppPreferences, Catalog, Project, ProjectSettings, SearchParams};
use rsearch_engine::{BuildError, BuildHandle};

use crate::editor::{Editor, EditorResult};
use crate::tr::{self, Strings};
use crate::util;

use banner::{Banner, BannerLevel};
use prefs::PrefsScreen;
use search::{FinishedSearch, SearchScreen};
use search_job::SearchJob;

/// Top-level screens reachable from the left navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Search,
    Projects,
    Preferences,
}

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
            Status::RebuildNeeded => egui::Color32::from_rgb(0xD9, 0xA0, 0x00),
            Status::UpToDate => egui::Color32::from_rgb(0x22, 0xA3, 0x55),
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

/// A transient or sticky message shown as a dismissible banner.
struct Notice {
    level: BannerLevel,
    text: String,
    at: Instant,
    /// Sticky notices stay until dismissed; the rest fade after
    /// [`NOTICE_TTL`].
    sticky: bool,
}

/// How long a transient notice stays visible.
const NOTICE_TTL: Duration = Duration::from_secs(8);

/// The single modal dialog currently open, if any.
enum Dialog {
    Editor(Box<Editor>),
    ConfirmDelete {
        id: String,
        name: String,
    },
    /// Name a new saved search.
    SaveSearch {
        name: String,
    },
    RenameSaved {
        id: String,
        name: String,
    },
    ConfirmDeleteSaved {
        id: String,
        name: String,
    },
}

/// Mutations requested by UI widgets, applied after the drawing pass
/// so catalog calls never run inside widget closures.
#[derive(Debug, Clone)]
enum Action {
    Navigate(Screen),
    Select(String),
    NewProject,
    Edit(String),
    AskDelete(String),
    StartBuild(String),
    CancelBuild,
    RetryCatalog,
    RunSearch,
    /// Loads a saved search into the form without running it.
    LoadSaved(String),
    /// Loads and immediately replays a saved search.
    RunSaved(String),
    AskSaveSearch,
    RenameSaved(String),
    AskDeleteSaved(String),
    DismissNotice(usize),
    CheckUpdates,
}

/// The rsearch application.
pub struct RsearchApp {
    /// Active text table, switched by the language preference.
    tr: &'static Strings,
    /// `None` when `projects.db` could not be opened (see
    /// `catalog_error`); the rest of the UI stays usable enough to
    /// show the error and offer a retry.
    catalog: Option<Catalog>,
    catalog_error: Option<String>,
    projects: Vec<Project>,
    /// Selected project id — shared by the Search picker and the
    /// Projects list.
    selected: Option<String>,
    screen: Screen,
    /// Global application preferences (`preferences.json`).
    prefs: AppPreferences,
    prefs_screen: PrefsScreen,
    /// The theme palette currently applied (`Some(dark)`); `None`
    /// forces re-application next frame.
    applied_theme: Option<bool>,
    dialog: Option<Dialog>,
    build: Option<ActiveBuild>,
    /// Banner notices, oldest first.
    notices: Vec<Notice>,
    search_screen: SearchScreen,
    /// The search currently running on its background thread.
    search_job: Option<SearchJob>,
}

impl RsearchApp {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let mut app = RsearchApp {
            tr: &tr::EN,
            catalog: None,
            catalog_error: None,
            projects: Vec::new(),
            selected: None,
            screen: Screen::Search,
            prefs: AppPreferences::default(),
            prefs_screen: PrefsScreen::default(),
            applied_theme: None,
            dialog: None,
            build: None,
            notices: Vec::new(),
            search_screen: SearchScreen {
                want_focus: true,
                ..SearchScreen::default()
            },
            search_job: None,
        };
        app.open_catalog();
        app
    }

    fn open_catalog(&mut self) {
        match Catalog::open_default() {
            Ok(catalog) => {
                match catalog.load_preferences() {
                    Ok(prefs) => self.prefs = prefs,
                    Err(e) => {
                        let msg = e.to_string();
                        let text = self.tr.prefs_load_failed(&msg);
                        self.push_notice(BannerLevel::Error, text, true);
                    }
                }
                self.tr = tr::for_language(self.prefs.language);
                self.prefs_screen.invalidate();
                self.applied_theme = None;
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
            Err(e) => {
                let msg = e.to_string();
                self.push_notice(BannerLevel::Error, msg, true);
            }
        }
        self.refresh_saved();
    }

    /// Reloads the saved-searches cache for the selected project.
    fn refresh_saved(&mut self) {
        let Some(catalog) = &self.catalog else {
            return;
        };
        match &self.selected {
            Some(id) => match catalog.list_saved_searches(id) {
                Ok(saved) => {
                    if self
                        .search_screen
                        .loaded_saved
                        .as_deref()
                        .is_some_and(|l| saved.iter().all(|s| s.id != l))
                    {
                        self.search_screen.loaded_saved = None;
                    }
                    self.search_screen.saved = saved;
                }
                Err(e) => {
                    let msg = e.to_string();
                    self.push_notice(BannerLevel::Error, msg, true);
                }
            },
            None => {
                self.search_screen.saved.clear();
                self.search_screen.loaded_saved = None;
            }
        }
    }

    fn selected_project(&self) -> Option<&Project> {
        self.selected
            .as_deref()
            .and_then(|id| self.projects.iter().find(|p| p.id == id))
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

    fn push_notice(&mut self, level: BannerLevel, text: String, sticky: bool) {
        self.notices.push(Notice {
            level,
            text,
            at: Instant::now(),
            sticky,
        });
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
                        self.push_notice(BannerLevel::Error, e.to_string(), true);
                    }
                }
                let text = self
                    .tr
                    .build_completed(report.summary.indexed_files, report.summary.duration);
                self.push_notice(BannerLevel::Success, text, false);
            }
            Err(BuildError::Cancelled { .. }) => {
                let text = self.tr.build_cancelled.to_owned();
                self.push_notice(BannerLevel::Info, text, false);
            }
            Err(e) => {
                let text = self.tr.build_failed(&e.to_string());
                self.push_notice(BannerLevel::Error, text, true);
            }
        }
        self.refresh();
    }

    /// Collects the finished search, tagging results with the project
    /// they ran on — the search screen shows that provenance instead
    /// of silently attaching them to whatever is selected now.
    fn poll_search(&mut self, ctx: &egui::Context) {
        let Some(job) = &self.search_job else {
            return;
        };
        ctx.request_repaint_after(Duration::from_millis(100));
        let Some(result) = job.poll() else {
            return;
        };
        let job = self.search_job.take().expect("job is Some");
        match result {
            Ok(report) => {
                let matches: usize = report.results.iter().map(|r| r.occurrences.len()).sum();
                let text = if matches == 0 {
                    self.tr.no_results_hint.to_owned()
                } else {
                    self.tr
                        .search_done(matches, report.results.len(), report.elapsed)
                };
                let level = if matches == 0 {
                    BannerLevel::Info
                } else {
                    BannerLevel::Success
                };
                let project_name = self
                    .projects
                    .iter()
                    .find(|p| p.id == job.project_id)
                    .map(|p| p.name.clone())
                    .unwrap_or_else(|| job.project_id.clone());
                self.search_screen.last = Some(FinishedSearch {
                    project_id: job.project_id,
                    project_name,
                    query: job.query,
                    report,
                });
                self.search_screen.selected = None;
                self.push_notice(level, text, false);
            }
            Err(e) => {
                let text = self.tr.search_failed(&e.to_string());
                self.push_notice(BannerLevel::Error, text, true);
            }
        }
    }

    /// Transient notices expire; sticky ones stay until dismissed.
    fn expire_notices(&mut self, ctx: &egui::Context) {
        self.notices
            .retain(|n| n.sticky || n.at.elapsed() < NOTICE_TTL);
        if self.notices.iter().any(|n| !n.sticky) {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }

    /// Starts a build or update for one project.
    fn start_build(&mut self, project_id: &str) {
        let Some(catalog) = &self.catalog else {
            return;
        };
        let project = match catalog.get_project(project_id) {
            Ok(p) => p,
            Err(e) => {
                self.push_notice(BannerLevel::Error, e.to_string(), true);
                return;
            }
        };
        if let Err(msg) = project.settings.validate() {
            self.push_notice(BannerLevel::Error, msg, true);
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
        self.build = Some(ActiveBuild {
            project_id: project.id,
            settings,
            handle,
        });
    }

    /// Launches the form's query on a background thread against the
    /// selected project's index.
    fn run_search(&mut self) {
        if self.search_job.is_some() || !self.search_screen.query_is_valid() {
            return;
        }
        let Some(project) = self.selected_project().cloned() else {
            return;
        };
        if !project.index_db_path.exists() {
            return;
        }
        let options = self.search_screen.options();
        let query = self.search_screen.query.clone();
        self.search_screen.selected = None;
        self.search_job = Some(SearchJob::start(&project, query, options));
    }

    /// Copies a saved search's query and options into the form.
    fn load_saved(&mut self, search_id: &str) {
        let Some(catalog) = &self.catalog else {
            return;
        };
        match catalog.get_saved_search(search_id) {
            Ok(s) => {
                self.search_screen.query = s.query;
                self.search_screen.case_sensitive = s.params.case_sensitive;
                self.search_screen.whole_word = s.params.whole_word;
                self.search_screen.context_lines = s.params.context_lines;
                self.search_screen.extensions_text = s
                    .params
                    .extensions
                    .map(|e| util::join_list(&e))
                    .unwrap_or_default();
                self.search_screen.loaded_saved = Some(s.id);
                self.search_screen.selected = None;
            }
            Err(e) => self.push_notice(BannerLevel::Error, e.to_string(), true),
        }
    }

    fn check_updates(&mut self) {
        match update::check_now() {
            update::UpdateCheck::NotConfigured => {
                self.push_notice(
                    BannerLevel::Info,
                    self.tr.update_not_configured.to_owned(),
                    true,
                );
            }
            update::UpdateCheck::UpToDate => {
                self.push_notice(
                    BannerLevel::Info,
                    self.tr.update_up_to_date.to_owned(),
                    true,
                );
            }
            update::UpdateCheck::Available { version } => {
                self.push_notice(BannerLevel::Info, self.tr.update_available(&version), true);
            }
        }
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
                let text = self.tr.project_deleted(name);
                self.push_notice(BannerLevel::Info, text, false);
                self.refresh();
            }
            Err(e) => self.push_notice(BannerLevel::Error, e.to_string(), true),
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
                let text = tr.project_created(&project.name);
                self.selected = Some(project.id);
                self.push_notice(BannerLevel::Success, text, false);
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
                self.push_notice(BannerLevel::Info, tr.project_updated.to_owned(), false);
            }
        }
        self.refresh();
        Ok(())
    }

    /// The banner list, computed in one place from the current state.
    /// Order: ongoing work first, then screen context, then the
    /// newest notices (capped so events never bury the content).
    fn banners(&self) -> Vec<Banner> {
        let tr = self.tr;
        let mut out = Vec::new();

        if let Some(b) = &self.build {
            let name = self
                .projects
                .iter()
                .find(|p| p.id == b.project_id)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| b.project_id.clone());
            out.push(Banner {
                level: BannerLevel::Info,
                text: tr.banner_building(&name),
                action: Some((tr.cancel_build.to_owned(), Action::CancelBuild)),
                dismiss: None,
                spinner: true,
            });
        }
        if let Some(job) = &self.search_job {
            out.push(Banner {
                level: BannerLevel::Info,
                text: tr.banner_searching(&job.query),
                action: None,
                dismiss: None,
                spinner: true,
            });
        }

        if self.screen == Screen::Search && self.catalog.is_some() {
            match self.selected_project() {
                None => out.push(Banner {
                    level: BannerLevel::Info,
                    text: tr.banner_no_project.to_owned(),
                    action: Some(if self.projects.is_empty() {
                        (tr.new_project.to_owned(), Action::NewProject)
                    } else {
                        (
                            tr.open_projects.to_owned(),
                            Action::Navigate(Screen::Projects),
                        )
                    }),
                    dismiss: None,
                    spinner: false,
                }),
                Some(p) => {
                    let building = self.build.as_ref().is_some_and(|b| b.project_id == p.id);
                    if !building && !p.index_db_path.exists() {
                        out.push(Banner {
                            level: BannerLevel::Info,
                            text: tr.banner_never_built.to_owned(),
                            action: Some((
                                tr.build_index.to_owned(),
                                Action::StartBuild(p.id.clone()),
                            )),
                            dismiss: None,
                            spinner: false,
                        });
                    } else if !building
                        && p.last_build_settings.is_some()
                        && self.catalog.as_ref().is_some_and(|c| c.needs_rebuild(p))
                    {
                        out.push(Banner {
                            level: BannerLevel::Warning,
                            text: tr.banner_needs_rebuild.to_owned(),
                            action: Some((
                                tr.update_index.to_owned(),
                                Action::StartBuild(p.id.clone()),
                            )),
                            dismiss: None,
                            spinner: false,
                        });
                    }
                }
            }
        }

        for (i, n) in self.notices.iter().enumerate().rev().take(3) {
            out.push(Banner {
                level: n.level,
                text: n.text.clone(),
                action: None,
                dismiss: Some(i),
                spinner: false,
            });
        }
        out
    }

    fn apply(&mut self, action: Action) {
        match action {
            Action::Navigate(screen) => {
                self.screen = screen;
                match screen {
                    Screen::Search => self.search_screen.want_focus = true,
                    Screen::Preferences => self.prefs_screen.invalidate(),
                    Screen::Projects => {}
                }
            }
            Action::Select(id) => {
                self.selected = Some(id);
                self.search_screen.loaded_saved = None;
                self.refresh_saved();
            }
            Action::NewProject => {
                self.dialog = Some(Dialog::Editor(Box::new(Editor::new_create(&self.prefs))));
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
            Action::RunSearch => self.run_search(),
            Action::LoadSaved(id) => self.load_saved(&id),
            Action::RunSaved(id) => {
                self.load_saved(&id);
                self.run_search();
            }
            Action::AskSaveSearch => {
                if self.selected.is_some() && self.search_screen.query_is_valid() {
                    self.dialog = Some(Dialog::SaveSearch {
                        name: self.search_screen.query.trim().to_owned(),
                    });
                }
            }
            Action::RenameSaved(id) => {
                if let Some(s) = self.search_screen.saved.iter().find(|s| s.id == id) {
                    self.dialog = Some(Dialog::RenameSaved {
                        id,
                        name: s.name.clone(),
                    });
                }
            }
            Action::AskDeleteSaved(id) => {
                if let Some(s) = self.search_screen.saved.iter().find(|s| s.id == id) {
                    self.dialog = Some(Dialog::ConfirmDeleteSaved {
                        id,
                        name: s.name.clone(),
                    });
                }
            }
            Action::DismissNotice(i) => {
                if i < self.notices.len() {
                    self.notices.remove(i);
                }
            }
            Action::CheckUpdates => self.check_updates(),
        }
    }

    /// A checkable name field shared by the save/rename dialogs.
    /// Returns `Some(trimmed_name)` when the user confirmed.
    fn name_dialog(
        ctx: &egui::Context,
        tr: &Strings,
        title: &str,
        name: &mut String,
        confirm: &str,
    ) -> Option<Option<String>> {
        let mut open = true;
        let mut outcome = None;
        egui::Window::new(title)
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(tr.name);
                    ui.add(
                        egui::TextEdit::singleline(name)
                            .desired_width(280.0)
                            .hint_text(tr.saved_name_hint),
                    );
                });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(!name.trim().is_empty(), egui::Button::new(confirm))
                        .clicked()
                    {
                        outcome = Some(Some(name.trim().to_owned()));
                    }
                    if ui.button(tr.cancel).clicked() {
                        outcome = Some(None);
                    }
                });
            });
        if !open && outcome.is_none() {
            Some(None)
        } else {
            outcome
        }
    }
}

impl eframe::App for RsearchApp {
    /// Called before every `ui` pass — theme application, progress
    /// polling and result collection live here so they also run while
    /// the window is hidden.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        theme::apply(ctx, self.prefs.theme, &mut self.applied_theme);
        self.poll_build(ctx);
        self.poll_search(ctx);
        self.expire_notices(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let tr = self.tr;
        let mut actions: Vec<Action> = Vec::new();

        // -- Left navigation ------------------------------------------
        egui::Panel::left("nav")
            .exact_size(190.0)
            .frame(egui::Frame::new().fill(ui.visuals().extreme_bg_color))
            .show(ui, |ui| {
                ui.add_space(18.0);
                ui.horizontal(|ui| {
                    ui.add_space(18.0);
                    ui.label(
                        egui::RichText::new(tr.app_title)
                            .size(22.0)
                            .strong()
                            .color(theme::ACCENT),
                    );
                });
                ui.add_space(24.0);
                let items = [
                    (Screen::Search, tr.nav_search),
                    (Screen::Projects, tr.nav_projects),
                    (Screen::Preferences, tr.nav_preferences),
                ];
                for (screen, label) in items {
                    if Self::nav_item(ui, label, self.screen == screen) {
                        actions.push(Action::Navigate(screen));
                    }
                    ui.add_space(2.0);
                }
                // Bottom of the sidebar: version.
                ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        ui.add_space(18.0);
                        ui.weak(format!("v{}", env!("CARGO_PKG_VERSION")));
                    });
                });
            });

        // -- Central content ------------------------------------------
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(ui.visuals().panel_fill)
                    .inner_margin(egui::Margin::symmetric(18, 14)),
            )
            .show(ui, |ui| {
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
                            actions.push(Action::RetryCatalog);
                        }
                    });
                    return;
                }

                for banner in self.banners() {
                    banner::show(ui, tr, &banner, &mut actions);
                    ui.add_space(6.0);
                }

                match self.screen {
                    Screen::Search => self.search_ui(ui, &mut actions),
                    Screen::Projects => self.projects_ui(ui, &mut actions),
                    Screen::Preferences => self.prefs_ui(ui, &mut actions),
                }
            });

        self.dialog_ui(ui.ctx());

        for action in actions {
            self.apply(action);
        }
    }
}

// -- Left navigation ------------------------------------------------------------

impl RsearchApp {
    /// One navigation entry: rounded highlight when active or hovered.
    fn nav_item(ui: &mut egui::Ui, label: &str, active: bool) -> bool {
        let width = ui.available_width();
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, 34.0), egui::Sense::click());
        let visuals = ui.visuals().clone();
        let inner = rect.shrink2(egui::vec2(8.0, 2.0));
        if active {
            ui.painter()
                .rect_filled(inner, egui::CornerRadius::same(7), theme::ACCENT);
        } else if resp.hovered() {
            ui.painter().rect_filled(
                inner,
                egui::CornerRadius::same(7),
                visuals.widgets.hovered.weak_bg_fill,
            );
        }
        let color = if active {
            egui::Color32::WHITE
        } else {
            visuals.widgets.inactive.fg_stroke.color
        };
        ui.painter().text(
            inner.left_center() + egui::vec2(12.0, 0.0),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::proportional(14.5),
            color,
        );
        resp.clicked()
    }
}

// -- Modal dialogs --------------------------------------------------------------

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
                match Self::confirm_dialog(
                    ctx,
                    tr.delete_project_title,
                    &tr.delete_confirm(&name),
                    Some(tr.delete_warning),
                    tr.delete,
                    tr.cancel,
                ) {
                    Some(true) => self.delete_project(&id, &name),
                    Some(false) => {}
                    None => self.dialog = Some(Dialog::ConfirmDelete { id, name }),
                }
            }
            Dialog::SaveSearch { mut name } => {
                match Self::name_dialog(ctx, tr, tr.save_search_title, &mut name, tr.save) {
                    Some(Some(name)) => self.create_saved_search(&name),
                    Some(None) => {}
                    None => self.dialog = Some(Dialog::SaveSearch { name }),
                }
            }
            Dialog::RenameSaved { id, mut name } => {
                match Self::name_dialog(ctx, tr, tr.rename_saved_title, &mut name, tr.rename) {
                    Some(Some(new_name)) => {
                        if let Some(catalog) = &self.catalog {
                            match catalog.rename_saved_search(&id, &new_name) {
                                Ok(()) => {
                                    let text = tr.saved_renamed(&new_name);
                                    self.push_notice(BannerLevel::Info, text, false);
                                    self.refresh_saved();
                                }
                                Err(e) => {
                                    self.push_notice(BannerLevel::Error, e.to_string(), true);
                                }
                            }
                        }
                    }
                    Some(None) => {}
                    None => self.dialog = Some(Dialog::RenameSaved { id, name }),
                }
            }
            Dialog::ConfirmDeleteSaved { id, name } => {
                match Self::confirm_dialog(
                    ctx,
                    tr.delete_saved_title,
                    &tr.delete_saved_confirm(&name),
                    None,
                    tr.delete,
                    tr.cancel,
                ) {
                    Some(true) => {
                        if let Some(catalog) = &self.catalog {
                            match catalog.delete_saved_search(&id) {
                                Ok(()) => {
                                    let text = tr.saved_deleted(&name);
                                    self.push_notice(BannerLevel::Info, text, false);
                                    if self.search_screen.loaded_saved.as_deref()
                                        == Some(id.as_str())
                                    {
                                        self.search_screen.loaded_saved = None;
                                    }
                                    self.refresh_saved();
                                }
                                Err(e) => self.push_notice(BannerLevel::Error, e.to_string(), true),
                            }
                        }
                    }
                    Some(false) => {}
                    None => self.dialog = Some(Dialog::ConfirmDeleteSaved { id, name }),
                }
            }
        }
    }

    /// A simple confirm dialog. Returns `Some(true)` on confirm,
    /// `Some(false)` on cancel/close, `None` while still open.
    fn confirm_dialog(
        ctx: &egui::Context,
        title: &str,
        question: &str,
        warning: Option<&str>,
        confirm: &str,
        cancel: &str,
    ) -> Option<bool> {
        let mut open = true;
        let mut outcome = None;
        egui::Window::new(title)
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label(question);
                if let Some(w) = warning {
                    ui.add_space(4.0);
                    ui.weak(w);
                }
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button(confirm).clicked() {
                        outcome = Some(true);
                    }
                    if ui.button(cancel).clicked() {
                        outcome = Some(false);
                    }
                });
            });
        match outcome {
            Some(v) => Some(v),
            None if !open => Some(false),
            None => None,
        }
    }

    /// Persists the form's query + options as a new saved search on the
    /// selected project.
    fn create_saved_search(&mut self, name: &str) {
        let Some(catalog) = &self.catalog else {
            return;
        };
        let Some(project_id) = self.selected.clone() else {
            return;
        };
        let params = SearchParams::from_engine(&self.search_screen.options());
        match catalog.create_saved_search(&project_id, name, &self.search_screen.query, params) {
            Ok(saved) => {
                let text = self.tr.saved_created(&saved.name);
                self.push_notice(BannerLevel::Success, text, false);
                self.search_screen.loaded_saved = Some(saved.id);
                self.refresh_saved();
            }
            Err(e) => self.push_notice(BannerLevel::Error, e.to_string(), true),
        }
    }
}
