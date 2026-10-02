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
//! [`RsearchApp::banners`]. Widgets only emit [`Message`]s, which are
//! applied in [`RsearchApp::update`].

mod banner;
mod prefs;
mod projects;
mod search;
mod search_job;
pub mod theme;
mod update;

use std::time::{Duration, Instant};

use iced::widget::{
    button, column, container, opaque, row, space, stack, text, text_editor, text_input,
};
use iced::{
    theme as iced_theme, Background, Color, Element, Fill, Padding, Subscription, Task, Theme,
};
use rsearch_catalog::{
    AppPreferences, Catalog, Project, ProjectSettings, SearchParams, ThemePreference,
};
use rsearch_engine::{BuildError, BuildHandle};

use crate::editor::Editor;
use crate::tr::{self, Strings};
use crate::util;

use banner::{Banner, BannerLevel};
use prefs::PrefsScreen;
use search::{FinishedSearch, SearchScreen};
use search_job::SearchJob;

/// Top-level screens reachable from the left navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Search,
    Projects,
    Preferences,
}

/// Derived display state of a project — computed from the catalog row,
/// never stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Status {
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

/// Everything the UI can tell the application — the equivalent of the
/// former `Action` enum plus the widget input callbacks.
#[derive(Debug, Clone)]
pub enum Message {
    /// Periodic tick driving build/search polling and notice expiry.
    Tick,
    /// The OS light/dark mode, reported at startup and on change.
    SystemMode(iced_theme::Mode),
    // -- Navigation ----------------------------------------------------
    Navigate(Screen),
    // -- Projects --------------------------------------------------------
    SelectProject(String),
    NewProject,
    EditProject(String),
    AskDeleteProject(String),
    StartBuild(String),
    CancelBuild,
    RetryCatalog,
    ToggleProjectSettings,
    ToggleBuildSummary,
    // -- Project editor ---------------------------------------------------
    EditorName(String),
    EditorRootPath(usize, String),
    EditorRootRecursive(usize, bool),
    EditorBrowse(usize),
    EditorRemoveRoot(usize),
    EditorAddRoot,
    EditorExcludedDirs(text_editor::Action),
    EditorExcludedExts(String),
    EditorGitignore(bool),
    EditorMaxSize(String),
    EditorArchives(bool),
    EditorArchiveDepth(u32),
    EditorSubmit,
    // -- Dialogs -----------------------------------------------------------
    /// Text input shared by the save/rename dialogs.
    DialogName(String),
    DialogConfirm,
    DialogCancel,
    // -- Search form ---------------------------------------------------------
    QueryChanged(String),
    CaseSensitive(bool),
    WholeWord(bool),
    ContextLines(usize),
    ExtensionsChanged(String),
    ToggleOptions,
    RunSearch,
    CancelSearch,
    SelectOccurrence(usize, usize),
    ToggleResultFile(usize),
    // -- Saved searches -------------------------------------------------------
    /// Loads a saved search into the form without running it.
    LoadSaved(String),
    /// Loads and immediately replays a saved search.
    RunSaved(String),
    AskSaveSearch,
    AskRenameSaved(String),
    AskDeleteSaved(String),
    // -- Notices & updates -------------------------------------------------------
    DismissNotice(usize),
    CheckUpdates,
    // -- Preferences -------------------------------------------------------------
    SetLanguage(rsearch_catalog::Language),
    SetTheme(ThemePreference),
    PrefDirs(text_editor::Action),
    PrefExtensions(String),
    PrefMaxSize(String),
    PrefCheckUpdates(bool),
    TogglePrefDefaults,
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
    /// The OS theme mode last reported (drives `ThemePreference::System`).
    system_mode: iced_theme::Mode,
    /// Whether the Projects "settings" section is expanded.
    settings_open: bool,
    /// Whether the Projects "last build" section is expanded.
    summary_open: bool,
    dialog: Option<Dialog>,
    build: Option<ActiveBuild>,
    /// Banner notices, oldest first.
    notices: Vec<Notice>,
    search_screen: SearchScreen,
    /// The search currently running on its background thread.
    search_job: Option<SearchJob>,
}

impl RsearchApp {
    /// Application startup: initial state plus the first-time tasks
    /// (system theme detection, query-field focus).
    pub fn boot() -> (Self, Task<Message>) {
        (
            RsearchApp::new(),
            Task::batch([
                iced::system::theme().map(Message::SystemMode),
                iced::widget::operation::focus(search::QUERY_ID),
            ]),
        )
    }

    pub fn new() -> Self {
        let mut app = RsearchApp {
            tr: &tr::EN,
            catalog: None,
            catalog_error: None,
            projects: Vec::new(),
            selected: None,
            screen: Screen::Search,
            prefs: AppPreferences::default(),
            prefs_screen: PrefsScreen::default(),
            system_mode: iced_theme::Mode::None,
            settings_open: true,
            summary_open: true,
            dialog: None,
            build: None,
            notices: Vec::new(),
            search_screen: SearchScreen::default(),
            search_job: None,
        };
        app.open_catalog();
        app
    }

    /// Window title.
    pub fn title(&self) -> String {
        self.tr.app_title.to_owned()
    }

    /// The active theme for this frame — resolved from the preference
    /// and the OS mode.
    pub fn theme(&self) -> Theme {
        theme::resolve(self.prefs.theme, self.system_mode)
    }

    /// Passive data sources: a fast tick while work is in flight
    /// (build or search polling), a slow one while transient notices
    /// need expiring, and system-theme changes when the preference
    /// follows the OS.
    pub fn subscription(&self) -> Subscription<Message> {
        let mut subs = Vec::new();
        if self.build.is_some() || self.search_job.is_some() {
            subs.push(tick(Duration::from_millis(100)));
        } else if self.notices.iter().any(|n| !n.sticky) {
            subs.push(tick(Duration::from_millis(500)));
        }
        if self.prefs.theme == ThemePreference::System {
            subs.push(iced::system::theme_changes().map(Message::SystemMode));
        }
        Subscription::batch(subs)
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

    /// Collects the running build's final result once the engine
    /// reports a terminal phase.
    fn poll_build(&mut self) {
        let Some(active) = &self.build else {
            return;
        };
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
    fn poll_search(&mut self) {
        let Some(job) = &self.search_job else {
            return;
        };
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
                let files = report.results.len();
                self.search_screen.last = Some(FinishedSearch {
                    project_id: job.project_id,
                    project_name,
                    query: job.query,
                    open: vec![files <= 20; files],
                    report,
                });
                self.search_screen.selected = None;
                self.push_notice(level, text, false);
            }
            Err(rsearch_engine::SearchError::Cancelled) => {
                // Partial results are never stored as a finished
                // search; the UI returns to its previous state.
                self.push_notice(
                    BannerLevel::Info,
                    self.tr.search_cancelled.to_owned(),
                    false,
                );
            }
            Err(e) => {
                let text = self.tr.search_failed(&e.to_string());
                self.push_notice(BannerLevel::Error, text, true);
            }
        }
    }

    /// Transient notices expire; sticky ones stay until dismissed.
    fn expire_notices(&mut self) {
        self.notices
            .retain(|n| n.sticky || n.at.elapsed() < NOTICE_TTL);
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
                action: Some((tr.cancel_build.to_owned(), Message::CancelBuild)),
                dismiss: None,
                spinner: true,
            });
        }

        if let Some(job) = &self.search_job {
            out.push(Banner {
                level: BannerLevel::Info,
                text: tr.banner_searching(&job.query),
                action: Some((tr.cancel.to_owned(), Message::CancelSearch)),
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
                        (tr.new_project.to_owned(), Message::NewProject)
                    } else {
                        (
                            tr.open_projects.to_owned(),
                            Message::Navigate(Screen::Projects),
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
                                Message::StartBuild(p.id.clone()),
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
                                Message::StartBuild(p.id.clone()),
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

    /// Message pump: every mutation requested by the widgets.
    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Tick => {
                self.poll_build();
                self.poll_search();
                self.expire_notices();
            }
            Message::SystemMode(mode) => self.system_mode = mode,
            Message::Navigate(screen) => {
                self.screen = screen;
                match screen {
                    Screen::Search => return iced::widget::operation::focus(search::QUERY_ID),
                    Screen::Preferences => self.prefs_screen.invalidate(),
                    Screen::Projects => {}
                }
            }
            Message::SelectProject(id) => {
                self.selected = Some(id);
                self.search_screen.loaded_saved = None;
                self.refresh_saved();
            }
            Message::NewProject => {
                self.dialog = Some(Dialog::Editor(Box::new(Editor::new_create(&self.prefs))));
            }
            Message::EditProject(id) => {
                if let Some(p) = self.projects.iter().find(|p| p.id == id) {
                    self.dialog = Some(Dialog::Editor(Box::new(Editor::new_edit(p))));
                }
            }
            Message::AskDeleteProject(id) => {
                if let Some(p) = self.projects.iter().find(|p| p.id == id) {
                    self.dialog = Some(Dialog::ConfirmDelete {
                        id,
                        name: p.name.clone(),
                    });
                }
            }
            Message::StartBuild(id) => self.start_build(&id),
            Message::CancelBuild => {
                if let Some(b) = &self.build {
                    b.handle.cancel();
                }
            }
            Message::RetryCatalog => self.open_catalog(),
            Message::ToggleProjectSettings => self.settings_open = !self.settings_open,
            Message::ToggleBuildSummary => self.summary_open = !self.summary_open,
            // -- Editor inputs ---------------------------------------
            Message::EditorName(name) => {
                if let Some(Dialog::Editor(ed)) = &mut self.dialog {
                    ed.set_name(name);
                }
            }
            Message::EditorRootPath(i, path) => {
                if let Some(Dialog::Editor(ed)) = &mut self.dialog {
                    ed.set_root_path(i, path);
                }
            }
            Message::EditorRootRecursive(i, v) => {
                if let Some(Dialog::Editor(ed)) = &mut self.dialog {
                    ed.set_root_recursive(i, v);
                }
            }
            Message::EditorBrowse(i) => {
                let tr = self.tr;
                if let Some(Dialog::Editor(ed)) = &mut self.dialog {
                    ed.browse_root(i, tr);
                }
            }
            Message::EditorRemoveRoot(i) => {
                if let Some(Dialog::Editor(ed)) = &mut self.dialog {
                    ed.remove_root(i);
                }
            }
            Message::EditorAddRoot => {
                if let Some(Dialog::Editor(ed)) = &mut self.dialog {
                    ed.add_root();
                }
            }
            Message::EditorExcludedDirs(action) => {
                if let Some(Dialog::Editor(ed)) = &mut self.dialog {
                    ed.edit_excluded_dirs(action);
                }
            }
            Message::EditorExcludedExts(s) => {
                if let Some(Dialog::Editor(ed)) = &mut self.dialog {
                    ed.set_excluded_extensions(s);
                }
            }
            Message::EditorGitignore(v) => {
                if let Some(Dialog::Editor(ed)) = &mut self.dialog {
                    ed.set_respect_gitignore(v);
                }
            }
            Message::EditorMaxSize(s) => {
                if let Some(Dialog::Editor(ed)) = &mut self.dialog {
                    ed.set_max_size_text(s);
                }
            }
            Message::EditorArchives(v) => {
                if let Some(Dialog::Editor(ed)) = &mut self.dialog {
                    ed.set_archives_enabled(v);
                }
            }
            Message::EditorArchiveDepth(v) => {
                if let Some(Dialog::Editor(ed)) = &mut self.dialog {
                    ed.set_archive_max_depth(v);
                }
            }
            Message::EditorSubmit => {
                if let Some(Dialog::Editor(mut ed)) = self.dialog.take() {
                    if let Err(msg) = self.apply_editor(&mut ed) {
                        ed.error = Some(msg);
                        self.dialog = Some(Dialog::Editor(ed));
                    }
                }
            }
            Message::DialogName(name) => match &mut self.dialog {
                Some(Dialog::SaveSearch { name: n })
                | Some(Dialog::RenameSaved { name: n, .. }) => *n = name,
                _ => {}
            },
            Message::DialogCancel => self.dialog = None,
            Message::DialogConfirm => self.confirm_dialog(),
            // -- Search form ------------------------------------------
            Message::QueryChanged(q) => self.search_screen.query = q,
            Message::CaseSensitive(v) => self.search_screen.case_sensitive = v,
            Message::WholeWord(v) => self.search_screen.whole_word = v,
            Message::ContextLines(v) => self.search_screen.context_lines = v,
            Message::ExtensionsChanged(s) => self.search_screen.extensions_text = s,
            Message::ToggleOptions => {
                self.search_screen.options_open = !self.search_screen.options_open
            }
            Message::RunSearch => self.run_search(),
            Message::CancelSearch => {
                if let Some(job) = &self.search_job {
                    job.cancel();
                }
            }
            Message::SelectOccurrence(fi, oi) => {
                let cur = self.search_screen.selected;
                self.search_screen.selected = if cur == Some((fi, oi)) {
                    None
                } else {
                    Some((fi, oi))
                };
            }
            Message::ToggleResultFile(fi) => {
                if let Some(fin) = &mut self.search_screen.last {
                    if let Some(open) = fin.open.get_mut(fi) {
                        *open = !*open;
                    }
                }
            }
            // -- Saved searches ----------------------------------------
            Message::LoadSaved(id) => self.load_saved(&id),
            Message::RunSaved(id) => {
                self.load_saved(&id);
                self.run_search();
            }
            Message::AskSaveSearch => {
                if self.selected.is_some() && self.search_screen.query_is_valid() {
                    self.dialog = Some(Dialog::SaveSearch {
                        name: self.search_screen.query.trim().to_owned(),
                    });
                }
            }
            Message::AskRenameSaved(id) => {
                if let Some(s) = self.search_screen.saved.iter().find(|s| s.id == id) {
                    self.dialog = Some(Dialog::RenameSaved {
                        id,
                        name: s.name.clone(),
                    });
                }
            }
            Message::AskDeleteSaved(id) => {
                if let Some(s) = self.search_screen.saved.iter().find(|s| s.id == id) {
                    self.dialog = Some(Dialog::ConfirmDeleteSaved {
                        id,
                        name: s.name.clone(),
                    });
                }
            }
            Message::DismissNotice(i) => {
                if i < self.notices.len() {
                    self.notices.remove(i);
                }
            }
            Message::CheckUpdates => self.check_updates(),
            // -- Preferences -------------------------------------------
            Message::SetLanguage(lang) => {
                if lang != self.prefs.language {
                    self.prefs.language = lang;
                    self.prefs_changed();
                }
            }
            Message::SetTheme(pref) => {
                if pref != self.prefs.theme {
                    self.prefs.theme = pref;
                    self.save_prefs();
                    if pref == ThemePreference::System {
                        return iced::system::theme().map(Message::SystemMode);
                    }
                }
            }
            Message::PrefDirs(action) => {
                self.prefs.default_excluded_dirs = self.prefs_screen.edit_dirs(action);
                self.save_prefs();
            }
            Message::PrefExtensions(s) => {
                self.prefs.default_excluded_extensions = self.prefs_screen.edit_exts(s);
                self.save_prefs();
            }
            Message::PrefMaxSize(s) => {
                if let Some(bytes) = self.prefs_screen.edit_max_size(s) {
                    self.prefs.default_max_indexed_file_size = bytes;
                    self.save_prefs();
                }
            }
            Message::PrefCheckUpdates(v) => {
                if v != self.prefs.check_for_updates {
                    self.prefs.check_for_updates = v;
                    self.save_prefs();
                }
            }
            Message::TogglePrefDefaults => self.prefs_screen.toggle_defaults(),
        }
        // Preference buffers reload lazily once the screen is active.
        if self.screen == Screen::Preferences {
            self.prefs_screen.sync(&self.prefs);
        }
        Task::none()
    }

    /// Applies the confirm button of whichever dialog is open.
    fn confirm_dialog(&mut self) {
        match self.dialog.take() {
            Some(Dialog::ConfirmDelete { id, name }) => self.delete_project(&id, &name),
            Some(Dialog::SaveSearch { name }) => {
                let name = name.trim().to_owned();
                if name.is_empty() {
                    self.dialog = Some(Dialog::SaveSearch { name });
                } else {
                    self.create_saved_search(&name);
                }
            }
            Some(Dialog::RenameSaved { id, name }) => {
                let new_name = name.trim().to_owned();
                if new_name.is_empty() {
                    self.dialog = Some(Dialog::RenameSaved { id, name: new_name });
                    return;
                }
                if let Some(catalog) = &self.catalog {
                    match catalog.rename_saved_search(&id, &new_name) {
                        Ok(()) => {
                            let text = self.tr.saved_renamed(&new_name);
                            self.push_notice(BannerLevel::Info, text, false);
                            self.refresh_saved();
                        }
                        Err(e) => {
                            self.push_notice(BannerLevel::Error, e.to_string(), true);
                        }
                    }
                }
            }
            Some(Dialog::ConfirmDeleteSaved { id, name }) => {
                if let Some(catalog) = &self.catalog {
                    match catalog.delete_saved_search(&id) {
                        Ok(()) => {
                            let text = self.tr.saved_deleted(&name);
                            self.push_notice(BannerLevel::Info, text, false);
                            if self.search_screen.loaded_saved.as_deref() == Some(id.as_str()) {
                                self.search_screen.loaded_saved = None;
                            }
                            self.refresh_saved();
                        }
                        Err(e) => self.push_notice(BannerLevel::Error, e.to_string(), true),
                    }
                }
            }
            other => self.dialog = other,
        }
    }

    // -- Views ------------------------------------------------------------

    pub fn view(&self) -> Element<'_, Message> {
        let base = row![self.nav_view(), self.content_view()].into();
        match &self.dialog {
            Some(dialog) => stack![base, opaque(self.dialog_view(dialog))].into(),
            None => base,
        }
    }

    /// The left navigation strip.
    fn nav_view(&self) -> Element<'_, Message> {
        let tr = self.tr;
        let items = [
            (Screen::Search, tr.nav_search),
            (Screen::Projects, tr.nav_projects),
            (Screen::Preferences, tr.nav_preferences),
        ];
        let mut col = column![container(
            text(tr.app_title)
                .size(22.0)
                .color(theme::ACCENT)
                .font(bold())
        )
        .padding(Padding::ZERO.top(18.0).bottom(24.0).left(10.0).right(10.0)),]
        .spacing(2)
        .width(190.0)
        .height(Fill);
        for (screen, label) in items {
            col = col.push(
                button(text(label).size(15.0).width(Fill))
                    .width(Fill)
                    .padding([8.0, 12.0])
                    .style(theme::nav_button(self.screen == screen))
                    .on_press(Message::Navigate(screen)),
            );
        }
        container(
            col.push(space().height(Fill)).push(
                text(format!("v{}", env!("CARGO_PKG_VERSION")))
                    .size(12.0)
                    .style(theme::weak),
            ),
        )
        .padding(Padding::ZERO.left(10.0).right(10.0).bottom(10.0))
        .style(theme::sidebar)
        .into()
    }

    /// The central area: error state, banners, then the active screen.
    fn content_view(&self) -> Element<'_, Message> {
        let tr = self.tr;
        let mut col = column![].spacing(8).width(Fill).height(Fill);

        if self.catalog.is_none() {
            let msg = self.catalog_error.clone().unwrap_or_default();
            return container(
                column![
                    text(format!("{msg}: {}", tr.catalog_unavailable))
                        .style(iced::widget::text::danger)
                        .center(),
                    container(
                        button(text(tr.retry))
                            .style(button::primary)
                            .on_press(Message::RetryCatalog)
                    )
                    .center_x(Fill),
                ]
                .spacing(10),
            )
            .padding(iced::Padding::ZERO.top(120.0))
            .center_x(Fill)
            .into();
        }

        for b in self.banners() {
            col = col.push(banner::view(&b, tr));
        }
        col = col.push(match self.screen {
            Screen::Search => self.search_view(),
            Screen::Projects => self.projects_view(),
            Screen::Preferences => self.prefs_view(),
        });
        container(col)
            .padding([14.0, 18.0])
            .width(Fill)
            .height(Fill)
            .into()
    }

    /// The modal overlay for the current dialog.
    fn dialog_view<'a>(&'a self, dialog: &'a Dialog) -> Element<'a, Message> {
        let tr = self.tr;
        let card: Element<'a, Message> = match dialog {
            Dialog::Editor(ed) => container(
                column![
                    row![
                        text(ed.title(tr)).size(18.0).font(bold()).width(Fill),
                        button(text("✕"))
                            .padding([2.0, 8.0])
                            .style(button::text)
                            .on_press(Message::DialogCancel),
                    ]
                    .align_y(iced::Alignment::Center),
                    ed.view(tr),
                ]
                .spacing(10)
                .height(Fill),
            )
            .width(620.0)
            .height(iced::Length::Fixed(560.0))
            .max_height(560.0)
            .padding(18.0)
            .style(theme::card)
            .into(),
            Dialog::ConfirmDelete { name, .. } => Self::confirm_card(
                tr.delete_project_title,
                &tr.delete_confirm(name),
                Some(tr.delete_warning),
                tr.delete,
                tr.cancel,
                tr,
            ),
            Dialog::SaveSearch { name } => Self::name_card(tr.save_search_title, name, tr.save, tr),
            Dialog::RenameSaved { name, .. } => {
                Self::name_card(tr.rename_saved_title, name, tr.rename, tr)
            }
            Dialog::ConfirmDeleteSaved { name, .. } => Self::confirm_card(
                tr.delete_saved_title,
                &tr.delete_saved_confirm(name),
                None,
                tr.delete,
                tr.cancel,
                tr,
            ),
        };
        container(card)
            .center(Fill)
            .style(|_| container::Style {
                background: Some(Background::Color(Color {
                    a: 0.45,
                    ..Color::BLACK
                })),
                ..container::Style::default()
            })
            .into()
    }

    /// A small dialog card with a question and confirm/cancel buttons.
    fn confirm_card<'a>(
        title: &'a str,
        question: &str,
        warning: Option<&'a str>,
        confirm: &'a str,
        cancel: &'a str,
        tr: &'a Strings,
    ) -> Element<'a, Message> {
        let _ = tr;
        let mut col = column![
            text(title).size(17.0).font(bold()),
            text(question.to_owned()),
        ]
        .spacing(8);
        if let Some(w) = warning {
            col = col.push(text(w).style(theme::weak).size(13.0));
        }
        container(
            col.push(
                row![
                    button(text(confirm))
                        .style(button::danger)
                        .padding([6.0, 16.0])
                        .on_press(Message::DialogConfirm),
                    button(text(cancel))
                        .padding([6.0, 16.0])
                        .on_press(Message::DialogCancel),
                ]
                .spacing(8),
            ),
        )
        .width(420.0)
        .padding(18.0)
        .style(theme::card)
        .into()
    }

    /// A small dialog card holding a single name field.
    fn name_card<'a>(
        title: &'a str,
        name: &'a str,
        confirm: &'a str,
        tr: &'a Strings,
    ) -> Element<'a, Message> {
        container(
            column![
                text(title).size(17.0).font(bold()),
                row![
                    text(tr.name).width(70.0),
                    text_input(tr.saved_name_hint, name)
                        .width(280.0)
                        .on_input(Message::DialogName)
                        .on_submit_maybe(
                            (!name.trim().is_empty()).then_some(Message::DialogConfirm)
                        ),
                ]
                .spacing(10)
                .align_y(iced::Alignment::Center),
                row![
                    button(text(confirm))
                        .style(button::primary)
                        .padding([6.0, 16.0])
                        .on_press_maybe(
                            (!name.trim().is_empty()).then_some(Message::DialogConfirm)
                        ),
                    button(text(tr.cancel))
                        .padding([6.0, 16.0])
                        .on_press(Message::DialogCancel),
                ]
                .spacing(8),
            ]
            .spacing(10),
        )
        .width(420.0)
        .padding(18.0)
        .style(theme::card)
        .into()
    }
}

/// A periodic [`Message::Tick`] source. Iced's `time` subscriptions
/// only exist behind the `smol`/`tokio` backend features; with the
/// default thread-pool executor a sleeping loop on one worker thread
/// is the equivalent — the UI thread stays free.
fn tick(period: Duration) -> Subscription<Message> {
    Subscription::run_with(period, tick_stream)
}

fn tick_stream(period: &Duration) -> impl iced::futures::Stream<Item = Message> {
    use iced::futures::SinkExt;
    let period = *period;
    iced::stream::channel::<Message>(
        1,
        move |mut sender: iced::futures::channel::mpsc::Sender<Message>| async move {
            loop {
                std::thread::sleep(period);
                if sender.send(Message::Tick).await.is_err() {
                    return;
                }
            }
        },
    )
}

/// The bold face used for headings.
fn bold() -> iced::Font {
    iced::Font {
        weight: iced::font::Weight::Bold,
        ..iced::Font::DEFAULT
    }
}
