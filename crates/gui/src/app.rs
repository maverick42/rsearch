//! Application state and logic for the rsearch GUI.
//!
//! Layering: this crate only drives `rsearch-catalog` (projects,
//! persisted settings, saved searches, preferences, build records) and
//! `rsearch-engine` (`rebuild_index` / `update_index` /
//! `search_events`). It never opens a project index itself and never
//! writes `projects.db` directly.
//!
//! [`App`] is toolkit-independent: it is mutated by the Slint
//! controller in `crate::ui` (one method per callback) and exposes
//! plain data the controller pushes into the UI afterwards. It never
//! touches a widget itself.

pub mod search_job;
pub mod update;

use std::rc::Rc;
use std::time::{Duration, Instant};

use rsearch_catalog::{
    AppPreferences, Catalog, CatalogError, Project, ProjectSettings, SavedSearch, SearchParams,
    ThemePreference,
};
use rsearch_engine::{
    BuildError, BuildHandle, BuildKind, BuildReport, FileResult, SearchError, SearchReport,
};

use crate::editor::EditorValues;
use crate::results::{ResultList, ResultsModel};
use crate::tr::{self, Strings};
use crate::util;
use crate::viewer::{self, ViewerLines};

use self::search_job::{SearchJob, SearchMsg};

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
pub enum Status {
    NeverBuilt,
    RebuildNeeded,
    UpToDate,
}

impl Status {
    /// Numeric value matching `status-kind` / `sel-status` in
    /// `state.slint` (0 never built, 1 rebuild needed, 2 up to date).
    pub fn kind(self) -> i32 {
        match self {
            Status::NeverBuilt => 0,
            Status::RebuildNeeded => 1,
            Status::UpToDate => 2,
        }
    }

    pub fn text(self, tr: &Strings) -> &'static str {
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
pub struct ActiveBuild {
    pub project_id: String,
    /// The settings actually handed to the engine; recorded on success.
    pub settings: ProjectSettings,
    pub handle: BuildHandle,
}

/// Severity of a notice/banner — matches `BannerRow.level` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BannerLevel {
    Info = 0,
    Success = 1,
    Warning = 2,
    Error = 3,
}

/// A transient or sticky message shown as a dismissible banner.
pub struct Notice {
    pub level: BannerLevel,
    pub text: String,
    pub at: Instant,
    /// Sticky notices stay until dismissed; the rest fade after
    /// [`NOTICE_TTL`].
    pub sticky: bool,
}

/// How long a transient notice stays visible.
const NOTICE_TTL: Duration = Duration::from_secs(8);

/// An action a banner button can trigger.
#[derive(Debug, Clone)]
pub enum BannerAction {
    CancelBuild,
    /// Cancels the running search of the given tab.
    CancelSearch(TabId),
    NewProject,
    OpenProjects,
    StartBuild(String),
}

/// One banner line as produced by [`App::banners`].
pub struct Banner {
    pub level: BannerLevel,
    pub text: String,
    pub action: Option<(String, BannerAction)>,
    /// Index into `notices` — `Some` makes the banner dismissible.
    pub dismiss: Option<usize>,
    /// Whether the banner reports ongoing work.
    pub working: bool,
}

/// The single modal dialog currently open, if any.
///
/// For `Editor` the text fields live in the Slint properties (read on
/// submit); `roots` is owned here because root rows are mutated by
/// callbacks, not by two-way bindings.
pub enum Dialog {
    Editor {
        /// Boxed: a `Project` is ~450 bytes and would inflate the enum.
        original: Option<Box<Project>>,
        values_roots: Vec<crate::editor::RootEdit>,
    },
    ConfirmDelete {
        id: String,
        name: String,
    },
    ConfirmDeleteSaved {
        id: String,
        name: String,
    },
    /// Name a search before saving it to the catalog.
    SaveSearch,
    /// Give a search tab a fixed title.
    RenameTab {
        id: TabId,
    },
    /// Confirmation before starting a build — the only path to
    /// [`App::start_build`], so a build never starts silently. Shows
    /// the duration reference (the last build's, or unknown) and the
    /// archive cost when the settings enable archives.
    ConfirmBuild {
        id: String,
        /// `true` when the pending action is an update — drives the
        /// title and confirm label wording.
        update: bool,
        /// The last build's duration as the estimate reference; `None`
        /// for a never-built project.
        estimate: Option<Duration>,
        /// Whether the settings about to be used enable archives.
        archives: bool,
    },
}

/// The kind of dialog for `AppState.dialog-kind`: 0 none, 1 editor,
/// 2 name field, 3 confirm, 4 build confirmation.
pub fn dialog_kind(dialog: &Option<Dialog>) -> i32 {
    match dialog {
        None => 0,
        Some(Dialog::Editor { .. }) => 1,
        Some(Dialog::SaveSearch) | Some(Dialog::RenameTab { .. }) => 2,
        Some(Dialog::ConfirmDelete { .. }) | Some(Dialog::ConfirmDeleteSaved { .. }) => 3,
        Some(Dialog::ConfirmBuild { .. }) => 4,
    }
}

/// Internal file-viewer state — `None` while the overlay is closed.
/// The line rows themselves live in [`ViewerLines`], shared with the
/// Slint model, so a finished load paints without a full resync.
pub struct Viewer {
    /// Header text: the file path and the focused line.
    pub title: String,
    /// 1-indexed line scrolled into view once loaded.
    pub focus_line: usize,
    /// The loader thread is still reading the file.
    pub loading: bool,
    /// Load failure, formatted for display.
    pub error: Option<String>,
    /// Only the head of a large file is shown.
    pub truncated: bool,
    /// Every match occurrence, ascending by (line, column) — the
    /// prev/next toolbar walks this list.
    pub matches: Vec<viewer::MatchPos>,
    /// Index into `matches` of the focused occurrence.
    pub match_idx: usize,
    /// Flipped on every prev/next: the Slint side swaps between two
    /// list instances so the freshly created one scrolls to the new
    /// match through its own first-layout bindings.
    pub nav_flip: bool,
}

/// The editable search form state. Bound to the UI properties; the
/// catalog's saved searches are loaded into it.
#[derive(Default)]
pub struct SearchForm {
    pub query: String,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub context_lines: usize,
    /// User-entered include masks (`;`/newline separated, `*`/`?`
    /// wildcards on file names).
    pub include_text: String,
    /// User-entered exclude masks.
    pub exclude_text: String,
    pub analyze_oversized: bool,
    /// The project this tab searches. Each tab keeps its own
    /// selection, independent of the Projects screen's; `None` means
    /// nothing picked yet.
    pub project_id: Option<String>,
}

/// Stable identifier of a search tab — survives tab creation and
/// removal, never reused within a session.
pub type TabId = u64;

/// Maximum characters of a tab title before visual truncation.
const TAB_TITLE_CHARS: usize = 24;

/// One independent search workspace — one tab of the Search screen.
///
/// Everything a search owns lives here so tabs can never share state:
/// the editable form, the result list model shown while the tab is
/// active, the in-flight job (its channel can only ever write into
/// this tab) and the file-viewer state.
pub struct SearchTab {
    /// Unique within the session.
    pub id: TabId,
    /// `Some` once the user renamed the tab — a fixed title that no
    /// longer follows the query. `None` = automatic title.
    pub custom_title: Option<String>,
    /// Name of the saved search this tab was loaded/saved with:
    /// shown while the query is still the saved one — an automatic
    /// title, not a custom one.
    seed_name: Option<String>,
    /// The query `seed_name` applies to.
    seed_query: String,
    /// The editable search form of this tab.
    pub form: SearchForm,
    /// Id of the catalog saved search this tab is associated with —
    /// set by Load or Save, cleared when the entry is deleted.
    /// `None` means "not tied to a saved search": Enregistrer then
    /// creates one.
    pub loaded_saved_id: Option<String>,
    /// The flattened result list of this tab.
    pub results: Rc<ResultsModel>,
    /// The search currently running for this tab, if any.
    pub job: Option<SearchJob>,
    /// The internal file viewer of this tab, when open.
    pub viewer: Option<Viewer>,
    /// Line rows of this tab's viewer overlay.
    pub viewer_lines: Rc<ViewerLines>,
    /// Loader thread of the pending view, dropped to abandon it.
    viewer_rx: Option<std::sync::mpsc::Receiver<viewer::ViewerOutcome>>,
}

impl SearchTab {
    /// A fresh, empty tab.
    fn new(id: TabId) -> Self {
        SearchTab {
            id,
            custom_title: None,
            seed_name: None,
            seed_query: String::new(),
            form: SearchForm::default(),
            loaded_saved_id: None,
            results: Rc::new(ResultsModel::default()),
            job: None,
            viewer: None,
            viewer_lines: ViewerLines::shared(),
            viewer_rx: None,
        }
    }

    /// Loads a saved search into this tab: query and every option
    /// are copied and the tab is associated with the entry's id. Its
    /// automatic title starts on the saved name — it still follows
    /// the query once the user edits it.
    fn fill_saved(&mut self, saved: &SavedSearch) {
        self.form.project_id = Some(saved.project_id.clone());
        self.form.query = saved.query.clone();
        self.form.case_sensitive = saved.params.case_sensitive;
        self.form.whole_word = saved.params.whole_word;
        self.form.context_lines = saved.params.context_lines;
        self.form.include_text = saved.params.include_masks.join(";");
        self.form.exclude_text = saved.params.exclude_masks.join(";");
        self.form.analyze_oversized = saved.params.analyze_oversized;
        self.loaded_saved_id = Some(saved.id.clone());
        self.seed_name = Some(saved.name.clone());
        self.seed_query = saved.query.clone();
        self.results.clear_selection();
    }

    /// The untruncated title: the custom name when the tab was
    /// renamed, else the saved-search name while its query stands,
    /// else the query itself, else `default_name` (the translated
    /// "Search") when the form is empty.
    pub fn title(&self, default_name: &str) -> String {
        if let Some(title) = &self.custom_title {
            return title.clone();
        }
        if let Some(name) = &self.seed_name {
            if self.form.query == self.seed_query && !name.trim().is_empty() {
                return name.clone();
            }
        }
        let query = self.form.query.trim();
        if query.is_empty() {
            default_name.to_owned()
        } else {
            query.to_owned()
        }
    }

    /// The tab-strip title — [`SearchTab::title`] truncated for
    /// display; the underlying query is never touched.
    pub fn display_title(&self, default_name: &str) -> String {
        util::ellipsize(&self.title(default_name), TAB_TITLE_CHARS)
    }

    /// Requests cancellation of a running job before the tab drops:
    /// the engine stops at its next check point and its remaining
    /// messages die with the dropped channel — they can never reach
    /// another tab.
    fn shutdown(&mut self) {
        if let Some(job) = self.job.take() {
            job.cancel();
        }
        self.viewer_rx = None;
    }
}

impl SearchForm {
    /// Engine options built from the current form state.
    pub fn options(&self) -> rsearch_engine::SearchOptions {
        rsearch_engine::SearchOptions {
            case_sensitive: self.case_sensitive,
            whole_word: self.whole_word,
            context_lines: self.context_lines,
            include_masks: rsearch_engine::parse_masks(&self.include_text),
            exclude_masks: rsearch_engine::parse_masks(&self.exclude_text),
            analyze_oversized: self.analyze_oversized,
        }
    }

    /// Whether the query passes the engine's minimum length.
    pub fn query_is_valid(&self) -> bool {
        self.query.chars().count() >= rsearch_engine::search::MIN_QUERY_CHARS
    }
}

/// The rsearch application.
pub struct App {
    /// Active text table, switched by the language preference.
    pub tr: &'static Strings,
    /// `None` when `projects.db` could not be opened (see
    /// `catalog_error`); the rest of the UI stays usable enough to
    /// show the error and offer a retry.
    pub catalog: Option<Catalog>,
    pub catalog_error: Option<String>,
    pub projects: Vec<Project>,
    /// Selected project id — the Projects screen's editing and
    /// building selection. Search tabs keep their own project
    /// (`SearchForm::project_id`); the two never sync.
    pub selected: Option<String>,
    pub screen: Screen,
    /// Global application preferences (`preferences.json`).
    pub prefs: AppPreferences,
    pub dialog: Option<Dialog>,
    pub build: Option<ActiveBuild>,
    /// Report of the last finished build, keyed by project id so the
    /// Projects detail never shows another project's report. `None`
    /// until a first build finishes in this session.
    pub last_report: Option<(String, BuildReport)>,
    /// Banner notices, oldest first.
    pub notices: Vec<Notice>,
    /// The search tabs of the Search screen — never empty.
    pub tabs: Vec<SearchTab>,
    /// Index into `tabs` of the tab on screen.
    pub active_tab: usize,
    /// Next value handed out by `alloc_tab_id`.
    next_tab_id: TabId,
    /// Saved searches of the active tab's project — a display cache
    /// of the catalog.
    pub saved: Vec<SavedSearch>,
    /// The saved search highlighted in the bottom-line combo — the
    /// target of Charger/Supprimer. Distinct from the tabs'
    /// `loaded_saved_id`: merely selecting in the combo never
    /// associates a tab.
    pub selected_saved: Option<String>,
}

impl App {
    /// Application startup: opens the catalog and loads preferences.
    pub fn new() -> Self {
        let mut app = App {
            tr: &tr::EN,
            catalog: None,
            catalog_error: None,
            projects: Vec::new(),
            selected: None,
            screen: Screen::Search,
            prefs: AppPreferences::default(),
            dialog: None,
            build: None,
            last_report: None,
            notices: Vec::new(),
            tabs: vec![SearchTab::new(0)],
            active_tab: 0,
            next_tab_id: 1,
            saved: Vec::new(),
            selected_saved: None,
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

    pub fn retry_catalog(&mut self) {
        self.open_catalog();
    }

    /// Rebuilds the project list from the catalog — the catalog is the
    /// single source of truth, the list is only a display cache.
    pub fn refresh(&mut self) {
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
                    // No selection means "search disabled" even though
                    // the picker always paints its first row — select
                    // it for real instead of leaving a dead state.
                    self.selected = self.projects.first().map(|p| p.id.clone());
                }
                // Tabs that never picked a project start on the healed
                // selection. A tab cannot reference a deleted project:
                // deletion is refused while any tab does.
                for tab in &mut self.tabs {
                    if tab.form.project_id.is_none() {
                        tab.form.project_id = self.selected.clone();
                    }
                }
            }
            Err(e) => {
                let msg = e.to_string();
                self.push_notice(BannerLevel::Error, msg, true);
            }
        }
        self.refresh_saved();
    }

    /// Reloads the saved-searches cache for the active tab's project —
    /// the saved list belongs to the search being edited, not to the
    /// Projects screen's selection.
    fn refresh_saved(&mut self) {
        let Some(catalog) = &self.catalog else {
            return;
        };
        let project_id = self.tab().form.project_id.clone();
        match project_id {
            Some(id) => match catalog.list_saved_searches(&id) {
                Ok(saved) => {
                    // Forget ids that no longer exist — only for the
                    // tabs of this project; the other tabs'
                    // associations belong to their own project's list.
                    for tab in &mut self.tabs {
                        if tab.form.project_id.as_deref() == Some(id.as_str())
                            && tab
                                .loaded_saved_id
                                .as_deref()
                                .is_some_and(|l| saved.iter().all(|s| s.id != l))
                        {
                            tab.loaded_saved_id = None;
                        }
                    }
                    if self
                        .selected_saved
                        .as_deref()
                        .is_some_and(|l| saved.iter().all(|s| s.id != l))
                    {
                        self.selected_saved = None;
                    }
                    self.saved = saved;
                }
                Err(e) => {
                    let msg = e.to_string();
                    self.push_notice(BannerLevel::Error, msg, true);
                }
            },
            None => {
                self.saved.clear();
                self.selected_saved = None;
                for tab in &mut self.tabs {
                    if tab.form.project_id.is_none() {
                        tab.loaded_saved_id = None;
                    }
                }
            }
        }
    }

    pub fn selected_project(&self) -> Option<&Project> {
        self.selected
            .as_deref()
            .and_then(|id| self.projects.iter().find(|p| p.id == id))
    }

    /// The active tab's search project. Each tab keeps its own
    /// selection, independent of the Projects screen's
    /// ([`Self::selected_project`]).
    pub fn search_project(&self) -> Option<&Project> {
        self.tab()
            .form
            .project_id
            .as_deref()
            .and_then(|id| self.projects.iter().find(|p| p.id == id))
    }

    /// Index of the active tab's project in `projects` (the picker),
    /// -1 for none.
    pub fn search_project_index(&self) -> i32 {
        self.tab()
            .form
            .project_id
            .as_deref()
            .and_then(|id| self.projects.iter().position(|p| p.id == id))
            .map(|i| i as i32)
            .unwrap_or(-1)
    }

    pub fn status(&self, p: &Project) -> Status {
        if p.last_build_settings.is_none() {
            Status::NeverBuilt
        } else if self.catalog.as_ref().is_some_and(|c| c.needs_rebuild(p)) {
            Status::RebuildNeeded
        } else {
            Status::UpToDate
        }
    }

    /// Selects a project in the Projects screen list — the editing
    /// and building selection. It never touches the search tabs:
    /// each tab keeps its own project
    /// ([`Self::select_search_project`]).
    pub fn select_project(&mut self, index: i32) {
        if let Some(p) = self.projects.get(index as usize) {
            self.selected = Some(p.id.clone());
        }
    }

    /// Selects a project for the active search tab (the Search
    /// screen's picker). Only this tab is affected; its saved-search
    /// association belongs to the previous project and is dropped,
    /// and the saved list reloads for the new one.
    pub fn select_search_project(&mut self, index: i32) {
        let Some(p) = self.projects.get(index as usize) else {
            return;
        };
        let id = p.id.clone();
        let tab = self.tab_mut();
        if tab.form.project_id.as_deref() == Some(id.as_str()) {
            return;
        }
        tab.form.project_id = Some(id);
        tab.loaded_saved_id = None;
        self.selected_saved = None;
        self.refresh_saved();
    }

    /// Left-navigation selection; `index` matches the `Screen` order.
    pub fn navigate(&mut self, index: i32) {
        self.screen = match index {
            1 => Screen::Projects,
            2 => Screen::Preferences,
            _ => Screen::Search,
        };
    }

    pub fn push_notice(&mut self, level: BannerLevel, text: String, sticky: bool) {
        self.notices.push(Notice {
            level,
            text,
            at: Instant::now(),
            sticky,
        });
    }

    /// Transient notices expire; sticky ones stay until dismissed.
    /// Returns `true` when a notice was removed.
    fn expire_notices(&mut self) -> bool {
        let before = self.notices.len();
        self.notices
            .retain(|n| n.sticky || n.at.elapsed() < NOTICE_TTL);
        self.notices.len() != before
    }

    // -- Builds -------------------------------------------------------

    /// Opens the build-confirmation dialog for the selected project.
    /// Every build start goes through here (Projects button and
    /// Search-screen banner) — a build never starts silently.
    pub fn ask_start_build(&mut self) {
        let selected = self.selected.clone();
        self.ask_start_build_for(selected);
    }

    /// Opens the build-confirmation dialog for the project chosen by
    /// id (banner action) and selects it. Refused while any build
    /// runs — same guard as [`App::start_build`], so the dialog never
    /// opens for a build that would be refused anyway.
    pub fn ask_start_build_for(&mut self, project_id: Option<String>) {
        if self.build.is_some() {
            let text = self.tr.build_already_running.to_owned();
            self.push_notice(BannerLevel::Warning, text, true);
            return;
        }
        let Some(id) = project_id else {
            return;
        };
        let Some(project) = self.projects.iter().find(|p| p.id == id) else {
            return;
        };
        let update = project.last_build_settings.is_some() && project.index_db_path.exists();
        let estimate = project.last_build_summary.as_ref().map(|s| s.duration);
        let archives = project.settings.archives_enabled;
        self.selected = Some(id.clone());
        self.dialog = Some(Dialog::ConfirmBuild {
            id,
            update,
            estimate,
            archives,
        });
    }

    /// Starts a build or update for the selected project.
    ///
    /// Refused while any build is running: a second start would
    /// overwrite [`Self::build`], orphaning the first build — it would
    /// keep running detached, its result never recorded. Covers both
    /// entry points (Projects button and Search-screen banner).
    pub fn start_build(&mut self) {
        if self.build.is_some() {
            let text = self.tr.build_already_running.to_owned();
            self.push_notice(BannerLevel::Warning, text, true);
            return;
        }
        if self.catalog.is_none() {
            return;
        }
        let Some(project_id) = self.selected.clone() else {
            return;
        };
        // The projects cache is refreshed after every mutation — no
        // need to re-read the whole catalog for one row.
        let Some(project) = self.projects.iter().find(|p| p.id == project_id) else {
            let text = self.tr.project_not_found.to_owned();
            self.push_notice(BannerLevel::Error, text, true);
            return;
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
            project_id,
            settings,
            handle,
        });
    }

    pub fn cancel_build(&mut self) {
        if let Some(b) = &self.build {
            b.handle.cancel();
        }
    }

    /// Collects the running build's final result once the engine
    /// reports a terminal phase. Returns `true` while a build is
    /// active — progress counters change between ticks.
    fn poll_build(&mut self) -> bool {
        let Some(active) = &self.build else {
            return false;
        };
        let finished = active
            .handle
            .progress()
            .snapshot()
            .phase
            .is_some_and(|p| p.is_terminal());
        if !finished {
            return true;
        }
        // Terminal phase ⇒ the coordinator already stored its result;
        // `wait()` only joins the threads.
        let ActiveBuild {
            project_id,
            settings,
            handle,
        } = self.build.take().expect("build is Some");
        let result = handle.wait();
        self.finish_build(&project_id, &settings, result);
        self.refresh();
        true
    }

    /// Records and announces a finished build. A full build that
    /// indexed zero files is announced as a sticky warning — an empty
    /// index after a "successful" build must never pass unnoticed,
    /// whatever the cause (empty roots, an invalid size limit, …). A
    /// no-op incremental update legitimately indexes nothing and stays
    /// a plain success.
    fn finish_build(
        &mut self,
        project_id: &str,
        settings: &ProjectSettings,
        result: Result<BuildReport, BuildError>,
    ) {
        match result {
            Ok(report) => {
                self.store_report(project_id, &report);
                if let Some(catalog) = &self.catalog {
                    if let Err(e) =
                        catalog.record_build_result(project_id, settings, &report.summary)
                    {
                        self.push_notice(BannerLevel::Error, e.to_string(), true);
                    }
                }
                if report.summary.indexed_files == 0 && report.summary.kind == BuildKind::Full {
                    let text = self.tr.build_zero_files(report.summary.duration);
                    self.push_notice(BannerLevel::Warning, text, true);
                } else {
                    let text = self
                        .tr
                        .build_completed(report.summary.indexed_files, report.summary.duration);
                    self.push_notice(BannerLevel::Success, text, false);
                }
                self.push_build_issue_notice(&report);
            }
            Err(BuildError::Cancelled { report }) => {
                self.store_report(project_id, &report);
                let text = self.tr.build_cancelled.to_owned();
                self.push_notice(BannerLevel::Info, text, false);
            }
            Err(e) => {
                if let BuildError::Fatal {
                    report: Some(report),
                    ..
                } = &e
                {
                    self.store_report(project_id, report);
                }
                let text = self.tr.build_failed(&e.to_string());
                self.push_notice(BannerLevel::Error, text, true);
            }
        }
    }

    /// Keeps the report of the last finished build for the Projects
    /// detail display, keyed by project id.
    fn store_report(&mut self, project_id: &str, report: &BuildReport) {
        self.last_report = Some((project_id.to_owned(), report.clone()));
    }

    /// Sticky warning summarizing what a finished build dropped or
    /// failed on — file errors, omitted error details, skipped source
    /// roots — so the counts never live only in the collapsed summary.
    fn push_build_issue_notice(&mut self, report: &BuildReport) {
        let mut parts: Vec<String> = Vec::new();
        if report.total_errors > 0 {
            parts.push(self.tr.build_file_errors(report.total_errors as usize));
        }
        if report.omitted_errors > 0 {
            parts.push(self.tr.build_errors_omitted(report.omitted_errors as usize));
        }
        if !report.skipped_roots.is_empty() {
            parts.push(self.tr.build_skipped_roots(report.skipped_roots.len()));
        }
        if !parts.is_empty() {
            self.push_notice(BannerLevel::Warning, parts.join(" "), true);
        }
    }

    // -- Search tabs ----------------------------------------------------

    fn alloc_tab_id(&mut self) -> TabId {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        id
    }

    /// The tab currently on screen — `tabs` is never empty.
    pub fn tab(&self) -> &SearchTab {
        &self.tabs[self.active_tab]
    }

    pub fn tab_mut(&mut self) -> &mut SearchTab {
        &mut self.tabs[self.active_tab]
    }

    fn tab_by_id_mut(&mut self, id: TabId) -> Option<&mut SearchTab> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }

    /// Current title of a tab by id — the rename dialog's prefill.
    pub fn tab_title(&self, id: TabId) -> Option<String> {
        self.tabs
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.title(self.tr.nav_search))
    }

    /// Opens a fresh, empty search tab and activates it. The new tab
    /// starts on the project of the tab it leaves (or the Projects
    /// screen's selection) — a snapshot, never a live link.
    pub fn new_tab(&mut self) {
        let id = self.alloc_tab_id();
        let project_id = self
            .tab()
            .form
            .project_id
            .clone()
            .or_else(|| self.selected.clone());
        let mut tab = SearchTab::new(id);
        tab.form.project_id = project_id;
        self.tabs.push(tab);
        self.active_tab = self.tabs.len() - 1;
        self.refresh_saved();
    }

    /// Switches the visible tab — nothing else: results, running jobs
    /// and viewer state of the other tabs stay untouched. The saved
    /// list follows the incoming tab's project.
    pub fn activate_tab(&mut self, index: i32) {
        if index >= 0 && (index as usize) < self.tabs.len() {
            self.active_tab = index as usize;
            self.refresh_saved();
        }
    }

    /// Closes one tab: a running search is cancelled and the whole
    /// tab state dropped — a late job message can then no longer
    /// reach any tab, its channel is gone. Closing the last tab
    /// leaves a fresh empty one behind.
    pub fn close_tab(&mut self, index: i32) {
        if index < 0 || index as usize >= self.tabs.len() {
            return;
        }
        let index = index as usize;
        self.tabs[index].shutdown();
        self.tabs.remove(index);
        if self.tabs.is_empty() {
            let id = self.alloc_tab_id();
            let mut tab = SearchTab::new(id);
            tab.form.project_id = self.selected.clone();
            self.tabs.push(tab);
            self.active_tab = 0;
        } else {
            if self.active_tab > index {
                self.active_tab -= 1;
            }
            self.active_tab = self.active_tab.min(self.tabs.len() - 1);
        }
        self.refresh_saved();
    }

    /// Opens the rename dialog for one tab.
    pub fn ask_rename_tab(&mut self, index: i32) {
        if let Some(tab) = self.tabs.get(index.max(0) as usize) {
            self.dialog = Some(Dialog::RenameTab { id: tab.id });
        }
    }

    /// True while the rename dialog targets a tab with a custom
    /// title — resetting only makes sense then.
    pub fn dialog_can_reset(&self) -> bool {
        matches!(
            &self.dialog,
            Some(Dialog::RenameTab { id })
                if self
                    .tabs
                    .iter()
                    .any(|t| t.id == *id && t.custom_title.is_some())
        )
    }

    /// The name dialog's "automatic name" action (tab rename only):
    /// drops the custom title so the tab follows the query again.
    pub fn dialog_reset_name(&mut self) {
        if let Some(Dialog::RenameTab { id }) = self.dialog {
            if let Some(tab) = self.tab_by_id_mut(id) {
                tab.custom_title = None;
            }
            self.dialog = None;
        }
    }

    // -- Search --------------------------------------------------------

    /// Whether the active tab's form state can launch a search.
    pub fn can_search(&self) -> bool {
        if self.tab().job.is_some() || !self.tab().form.query_is_valid() {
            return false;
        }
        self.search_project()
            .is_some_and(|p| p.index_db_path.exists())
    }

    /// Launches the active tab's query on a background thread against
    /// the tab's selected project's index.
    pub fn run_search(&mut self) {
        if !self.can_search() {
            return;
        }
        let Some(project) = self.search_project().cloned() else {
            return;
        };
        let tab = self.tab_mut();
        let options = tab.form.options();
        let query = tab.form.query.clone();
        // Results of the previous search are replaced by this job's —
        // partial results then belong unambiguously to it.
        tab.results.clear();
        tab.job = SearchJob::start(&project, tab.id, query, options);
        if tab.job.is_none() {
            let text = self.tr.search_thread_failed.to_owned();
            self.push_notice(BannerLevel::Error, text, true);
        }
    }

    /// Cancels the active tab's running search.
    pub fn cancel_search(&mut self) {
        if let Some(job) = &self.tab().job {
            job.cancel();
        }
    }

    /// Cancels the running search of one specific tab (a banner
    /// action — the tab may not be the active one).
    pub fn cancel_tab_search(&mut self, tab_id: TabId) {
        if let Some(job) = self
            .tabs
            .iter()
            .find(|t| t.id == tab_id)
            .and_then(|t| t.job.as_ref())
        {
            job.cancel();
        }
    }

    /// Collects search progress and results for every tab: each job
    /// only writes into its own tab's list, so results of a search
    /// running in the background never leak into the visible tab.
    /// Returns `true` while a job is in flight or when a message was
    /// processed this tick.
    fn poll_search(&mut self) -> bool {
        let mut changed = false;
        let mut running = false;
        for i in 0..self.tabs.len() {
            let Some(job) = &self.tabs[i].job else {
                continue;
            };
            debug_assert_eq!(job.tab_id, self.tabs[i].id);
            let msgs = job.poll();
            for msg in msgs {
                changed = true;
                match msg {
                    SearchMsg::Initial(report) => self.search_initial(i, report),
                    SearchMsg::Progress { done, total, found } => {
                        self.search_progress(i, done, total, found)
                    }
                    SearchMsg::Done(result) => {
                        self.search_done(i, result);
                        break;
                    }
                }
            }
            running |= self.tabs[i].job.is_some();
        }
        changed || running
    }

    /// Phase-A report: all indexed candidates are verified — show them
    /// now. With the deep scan enabled the job keeps running and
    /// oversized files still pending stay counted in
    /// `candidates_too_large`.
    fn search_initial(&mut self, tab_index: usize, report: SearchReport) {
        let Some(job) = &self.tabs[tab_index].job else {
            return;
        };
        let project_name = self
            .projects
            .iter()
            .find(|p| p.id == job.project_id)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| job.project_id.clone());
        let oversized_total = report.candidates_too_large;
        let list = ResultList::new(
            report,
            crate::results::ResultContext {
                project_id: job.project_id.clone(),
                project_name,
                query: job.query.clone(),
                case_sensitive: job.case_sensitive,
                whole_word: job.whole_word,
            },
            job.analyze_oversized,
            0,
            oversized_total,
            true,
        );
        self.tabs[tab_index].results.replace(list);
    }

    /// One oversized file was verified during the deep scan; the
    /// owning tab's model merges its result into the canonical order.
    fn search_progress(
        &mut self,
        tab_index: usize,
        done: usize,
        total: usize,
        found: Option<FileResult>,
    ) {
        self.tabs[tab_index]
            .results
            .insert_oversized(done, total, found);
    }

    /// Terminal message: complete — or stopped. Results that already
    /// arrived stay on screen but a cancelled search is labeled
    /// incomplete, never finished.
    fn search_done(&mut self, tab_index: usize, result: Result<SearchReport, SearchError>) {
        let Some(job) = self.tabs[tab_index].job.take() else {
            return;
        };
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
                let oversized_total = report.candidates_too_large;
                self.tabs[tab_index].results.replace(ResultList::new(
                    report,
                    crate::results::ResultContext {
                        project_id: job.project_id,
                        project_name,
                        query: job.query,
                        case_sensitive: job.case_sensitive,
                        whole_word: job.whole_word,
                    },
                    job.analyze_oversized,
                    oversized_total,
                    oversized_total,
                    false,
                ));
                self.push_notice(level, text, false);
            }
            Err(SearchError::Cancelled) => {
                // Partial results stay displayed — labeled cancelled,
                // never finished.
                self.tabs[tab_index].results.finish(true);
                self.push_notice(
                    BannerLevel::Info,
                    self.tr.search_cancelled.to_owned(),
                    false,
                );
            }
            Err(e) => {
                // The job is over: any partial list it produced is no
                // longer in flight; the sticky error banner carries
                // the failure.
                self.tabs[tab_index].results.finish(false);
                let text = self.tr.search_failed(&e.to_string());
                self.push_notice(BannerLevel::Error, text, true);
            }
        }
    }

    /// Periodic work driven by the UI timer: collect engine progress,
    /// expire notices. Returns `true` when something changed — an idle
    /// tick must not resync the UI, or every model push recreates the
    /// list delegates and eats mid-gesture clicks.
    pub fn tick(&mut self) -> bool {
        self.poll_build() | self.poll_search() | self.poll_viewer() | self.expire_notices()
    }

    // -- Internal file viewer --------------------------------------------------

    /// Opens the viewer on the file/occurrence of a result row of the
    /// active tab; the file itself is read and decoded on a worker
    /// thread — the overlay shows "loading" immediately and never
    /// blocks the UI.
    pub fn open_viewer(&mut self, file: usize, occ: usize) {
        let Some((path, entry, line, col)) = self.tab().results.with(|l| {
            let fr = l.file(file)?;
            let o = l.occurrence(file, occ)?;
            Some((
                fr.file_path.clone(),
                fr.entry_path.clone(),
                o.line,
                o.column,
            ))
        }) else {
            return;
        };
        if entry.is_some() {
            // Archive entries have no filesystem path to read; the
            // archive API stays an indexing concern for now.
            self.push_notice(
                BannerLevel::Info,
                self.tr.viewer_archive_unavailable.to_owned(),
                false,
            );
            return;
        }
        let (query, case_sensitive, whole_word) = self
            .tab()
            .results
            .with(|l| (l.query.clone(), l.case_sensitive, l.whole_word));
        let project_id = self.tab().results.with(|l| l.project_id.clone());
        let fallback = self
            .projects
            .iter()
            .find(|p| p.id == project_id)
            .and_then(|p| p.settings.to_build_options().fallback_encoding);

        let tr = self.tr;
        let tab = self.tab_mut();
        tab.viewer = Some(Viewer {
            title: format!("{}:{}", path.display(), line),
            focus_line: line,
            loading: true,
            error: None,
            truncated: false,
            matches: Vec::new(),
            match_idx: 0,
            nav_flip: false,
        });
        tab.viewer_lines.clear();
        tab.viewer_rx =
            viewer::start_load(path, query, case_sensitive, whole_word, fallback, line, col);
        if tab.viewer_rx.is_none() {
            // The loader thread could not be spawned: report it in the
            // overlay instead of leaving a stuck "loading" state.
            if let Some(v) = &mut tab.viewer {
                v.loading = false;
                v.error = Some(tr.viewer_thread_failed.to_owned());
            }
        }
    }

    /// Closes the overlay; a pending load is abandoned (its sender
    /// dies with the receiver).
    pub fn close_viewer(&mut self) {
        let tab = self.tab_mut();
        tab.viewer = None;
        tab.viewer_rx = None;
        tab.viewer_lines.clear();
    }

    /// Moves the focused match among the file's occurrences —
    /// `dir` is -1/+1 and wraps around both ends. Two hits sharing a
    /// line are two stops: only the green marker moves then.
    pub fn viewer_navigate(&mut self, dir: i32) {
        let tab = self.tab_mut();
        let Some(v) = &mut tab.viewer else {
            return;
        };
        let n = v.matches.len();
        if n == 0 {
            return;
        }
        let old = v.matches[v.match_idx];
        v.match_idx = (v.match_idx as i32 + dir).rem_euclid(n as i32) as usize;
        let new = v.matches[v.match_idx];
        v.focus_line = new.line;
        v.nav_flip = !v.nav_flip;
        tab.viewer_lines.set_focus(old, new);
    }

    /// Picks up each tab's loader-thread outcome once per load.
    /// Returns `true` only when a viewer state actually changed — the
    /// timer must not resync while a load is merely pending.
    fn poll_viewer(&mut self) -> bool {
        let mut changed = false;
        for i in 0..self.tabs.len() {
            let Some(rx) = &self.tabs[i].viewer_rx else {
                continue;
            };
            let outcome = match rx.try_recv() {
                Ok(o) => o,
                Err(std::sync::mpsc::TryRecvError::Empty) => continue,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.tabs[i].viewer_rx = None;
                    let text = self.tr.viewer_error("loader stopped unexpectedly");
                    if let Some(v) = &mut self.tabs[i].viewer {
                        v.loading = false;
                        v.error = Some(text);
                    }
                    changed = true;
                    continue;
                }
            };
            self.tabs[i].viewer_rx = None;
            match outcome {
                viewer::ViewerOutcome::Loaded(content) => {
                    let focus = content.matches.get(content.match_idx).copied();
                    self.tabs[i].viewer_lines.set_lines(content.lines, focus);
                    self.tabs[i].viewer = Some(Viewer {
                        title: content.title,
                        focus_line: content.focus_line,
                        loading: false,
                        error: None,
                        truncated: content.truncated,
                        matches: content.matches,
                        match_idx: content.match_idx,
                        nav_flip: false,
                    });
                }
                viewer::ViewerOutcome::Failed(msg) => {
                    let text = self.tr.viewer_error(&msg);
                    if let Some(v) = &mut self.tabs[i].viewer {
                        v.loading = false;
                        v.error = Some(text);
                    }
                }
            }
            changed = true;
        }
        changed
    }

    // -- Saved searches ---------------------------------------------------

    /// The combo selection moved in the bottom line: only records
    /// which saved search Charger/Supprimer acts on. It never touches
    /// a tab — row 0 is the "select a search" placeholder and clears
    /// the selection.
    pub fn select_saved(&mut self, index: i32) {
        self.selected_saved = if index <= 0 {
            None
        } else {
            self.saved.get(index as usize - 1).map(|s| s.id.clone())
        };
    }

    /// Combo index of the currently selected saved search — row 0 is
    /// the placeholder, so a real selection starts at 1.
    pub fn saved_index(&self) -> i32 {
        self.selected_saved
            .as_deref()
            .and_then(|id| self.saved.iter().position(|s| s.id == id))
            .map(|i| i as i32 + 1)
            .unwrap_or(0)
    }

    /// Loads the combo-selected saved search. When a tab is already
    /// associated with its id (`loaded_saved_id`), that tab is simply
    /// activated; otherwise the search fills a NEW tab — the current
    /// one is never overwritten. The search is NOT launched either
    /// way.
    pub fn load_saved(&mut self) {
        let Some(catalog) = &self.catalog else {
            return;
        };
        let Some(id) = self.selected_saved.clone() else {
            return;
        };
        match catalog.get_saved_search(&id) {
            Ok(s) => {
                if let Some(i) = self
                    .tabs
                    .iter()
                    .position(|t| t.loaded_saved_id.as_deref() == Some(id.as_str()))
                {
                    self.activate_tab(i as i32);
                } else {
                    self.new_tab();
                    self.tab_mut().fill_saved(&s);
                }
                self.selected_saved = Some(id);
            }
            Err(e) => self.push_notice(BannerLevel::Error, e.to_string(), true),
        }
    }

    /// Opens the save dialog: the name field starts on the associated
    /// entry's name (so Enregistrer updates it by default) or on the
    /// tab title for an unassociated tab.
    pub fn ask_save_search(&mut self) {
        if self.search_project().is_none() || !self.tab().form.query_is_valid() {
            return;
        }
        self.dialog = Some(Dialog::SaveSearch);
    }

    /// Initial content of the save dialog's name field: the associated
    /// entry's name when the tab has one, else the tab title.
    pub fn suggested_saved_name(&self) -> String {
        if let Some(id) = &self.tab().loaded_saved_id {
            if let Some(s) = self.saved.iter().find(|s| &s.id == id) {
                return s.name.clone();
            }
        }
        self.tab().title(self.tr.nav_search)
    }

    /// The save dialog shows a Dupliquer button — only when the tab
    /// is associated with a saved search (duplicating an unassociated
    /// tab would just be a plain create).
    pub fn dialog_can_duplicate(&self) -> bool {
        matches!(self.dialog, Some(Dialog::SaveSearch)) && self.tab().loaded_saved_id.is_some()
    }

    /// Saves the active tab's search under `name` (the save dialog's
    /// field). `loaded_saved_id` is the only reference of truth:
    /// * `Some(id)` → UPDATE that entry in place — same id, whatever
    ///   the new name is (renaming is a modification, not a copy);
    ///   falls back to INSERT when the entry vanished meanwhile;
    /// * `None` → INSERT a new entry and associate the tab with it.
    pub fn save_saved(&mut self, name: &str) {
        let Some(catalog) = &self.catalog else {
            return;
        };
        let name = name.trim().to_owned();
        if name.is_empty() {
            self.dialog = Some(Dialog::SaveSearch);
            return;
        }
        let Some(project_id) = self.tab().form.project_id.clone() else {
            return;
        };
        if !self.tab().form.query_is_valid() {
            return;
        }
        let query = self.tab().form.query.clone();
        let params = SearchParams::from_engine(&self.tab().form.options());
        let outcome = match self.tab().loaded_saved_id.as_deref() {
            Some(id) => match catalog.update_saved_search(id, &name, &query, params.clone()) {
                Ok(()) => Ok((id.to_owned(), false)),
                // The entry was deleted while associated — insert a
                // fresh one rather than failing.
                Err(CatalogError::NotFound(_)) => catalog
                    .create_saved_search(&project_id, &name, &query, params)
                    .map(|s| (s.id, true)),
                Err(e) => Err(e),
            },
            None => catalog
                .create_saved_search(&project_id, &name, &query, params)
                .map(|s| (s.id, true)),
        };
        self.finish_saved(outcome, &name);
    }

    /// Duplicates the form under `name` (the save dialog's field):
    /// always INSERTs a new entry — same name or not — leaves any
    /// associated entry untouched and switches the tab to the copy.
    pub fn duplicate_saved(&mut self, name: &str) {
        let Some(catalog) = &self.catalog else {
            return;
        };
        let name = name.trim().to_owned();
        if name.is_empty() {
            self.dialog = Some(Dialog::SaveSearch);
            return;
        }
        let Some(project_id) = self.tab().form.project_id.clone() else {
            return;
        };
        if !self.tab().form.query_is_valid() {
            return;
        }
        let query = self.tab().form.query.clone();
        let params = SearchParams::from_engine(&self.tab().form.options());
        let outcome = catalog
            .create_saved_search(&project_id, &name, &query, params)
            .map(|s| (s.id, true));
        self.finish_saved(outcome, &name);
    }

    /// Common post-save bookkeeping: association, seed title, combo
    /// selection, cache refresh and the success/error notice.
    fn finish_saved(&mut self, outcome: Result<(String, bool), CatalogError>, name: &str) {
        match outcome {
            Ok((id, created)) => {
                let text = if created {
                    self.tr.saved_created(name)
                } else {
                    self.tr.saved_updated(name)
                };
                self.push_notice(BannerLevel::Success, text, false);
                let tab = self.tab_mut();
                tab.loaded_saved_id = Some(id.clone());
                tab.seed_name = Some(name.to_owned());
                tab.seed_query = tab.form.query.clone();
                self.selected_saved = Some(id);
                self.refresh_saved();
            }
            Err(e) => self.push_notice(BannerLevel::Error, e.to_string(), true),
        }
    }

    pub fn ask_delete_saved(&mut self) {
        if let Some(id) = self.selected_saved.clone() {
            if let Some(s) = self.saved.iter().find(|s| s.id == id) {
                self.dialog = Some(Dialog::ConfirmDeleteSaved {
                    id,
                    name: s.name.clone(),
                });
            }
        }
    }

    // -- Projects / editor -------------------------------------------------

    /// Prepares a create-mode editor: field values for the UI plus the
    /// dialog state.
    pub fn new_project(&mut self) -> EditorValues {
        let values = EditorValues::for_create(&self.prefs);
        self.dialog = Some(Dialog::Editor {
            original: None,
            values_roots: values.roots.clone(),
        });
        values
    }

    /// Prepares an edit-mode editor for the selected project.
    pub fn edit_project(&mut self) -> Option<EditorValues> {
        let project = self.selected_project()?.clone();
        let values = EditorValues::for_edit(&project);
        self.dialog = Some(Dialog::Editor {
            original: Some(Box::new(project)),
            values_roots: values.roots.clone(),
        });
        Some(values)
    }

    pub fn ask_delete_project(&mut self) {
        let Some(p) = self.selected_project() else {
            return;
        };
        let id = p.id.clone();
        let name = p.name.clone();
        // A search tab still points at this project — deleting it
        // would orphan the tab's whole search context. Refuse until
        // the user closes (or re-points) every tab using it.
        if self
            .tabs
            .iter()
            .any(|t| t.form.project_id.as_deref() == Some(id.as_str()))
        {
            let text = self.tr.project_in_use.to_owned();
            self.push_notice(BannerLevel::Warning, text, true);
            return;
        }
        self.dialog = Some(Dialog::ConfirmDelete { id, name });
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

    /// Mutations of the editor's root list while the dialog is open.
    /// Returns false (ignored) when no editor is open.
    pub fn editor_root_path(&mut self, index: usize, path: String) -> bool {
        if let Some(Dialog::Editor { values_roots, .. }) = &mut self.dialog {
            if let Some(row) = values_roots.get_mut(index) {
                row.path = path;
                return true;
            }
        }
        false
    }

    pub fn editor_root_recursive(&mut self, index: usize, recursive: bool) -> bool {
        if let Some(Dialog::Editor { values_roots, .. }) = &mut self.dialog {
            if let Some(row) = values_roots.get_mut(index) {
                row.recursive = recursive;
                return true;
            }
        }
        false
    }

    /// Fills `roots[index]` from a native folder dialog. The path is
    /// reopened by the engine — non-Unicode paths are refused instead
    /// of storing a lossy rendering. `None` means cancelled (nothing
    /// to show); `Some(Err)` carries the message to display.
    pub fn editor_browse_root(&mut self, index: usize) -> Option<Result<String, String>> {
        let Some(Dialog::Editor { values_roots, .. }) = &mut self.dialog else {
            return None;
        };
        values_roots.get(index)?;
        let dir = rfd::FileDialog::new().pick_folder()?;
        Some(match dir.to_str() {
            Some(s) => {
                values_roots[index].path = s.to_owned();
                Ok(s.to_owned())
            }
            None => Err(self.tr.err_non_unicode_path.to_owned()),
        })
    }

    pub fn editor_remove_root(&mut self, index: usize) -> bool {
        if let Some(Dialog::Editor { values_roots, .. }) = &mut self.dialog {
            if index < values_roots.len() {
                values_roots.remove(index);
                return true;
            }
        }
        false
    }

    pub fn editor_add_root(&mut self) -> bool {
        if let Some(Dialog::Editor { values_roots, .. }) = &mut self.dialog {
            values_roots.push(crate::editor::RootEdit {
                path: String::new(),
                recursive: true,
            });
            return true;
        }
        false
    }

    /// Applies a submitted editor form: catalog `create_project`, or —
    /// in edit mode — `update_project_settings` and/or `rename_project`
    /// depending on what actually changed. A pure rename never touches
    /// settings, so it cannot trigger a rebuild flag.
    pub fn apply_editor(&mut self, mut values: EditorValues) -> Result<(), String> {
        let tr = self.tr;
        let name = values.name.trim().to_owned();
        if name.is_empty() {
            return Err(tr.err_name_required.to_owned());
        }
        // The roots the UI edited live in the dialog state.
        if let Some(Dialog::Editor { values_roots, .. }) = &self.dialog {
            values.roots = values_roots.clone();
        }
        let original = match &self.dialog {
            Some(Dialog::Editor { original, .. }) => original.clone(),
            _ => None,
        };
        let settings = values.settings();
        if values.max_size_mib().is_none() {
            return Err(tr.err_max_size_invalid.to_owned());
        }
        settings.validate()?;
        let catalog = self
            .catalog
            .as_ref()
            .ok_or_else(|| tr.catalog_unavailable.to_owned())?;

        match &original {
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
        self.dialog = None;
        self.refresh();
        Ok(())
    }

    // -- Dialogs ------------------------------------------------------------

    pub fn dialog_cancel(&mut self) {
        self.dialog = None;
    }

    /// Applies the Dupliquer button of the save dialog — always
    /// creates a new entry under `name` and leaves the associated
    /// entry untouched.
    pub fn dialog_duplicate(&mut self, name: &str) {
        if matches!(self.dialog.take(), Some(Dialog::SaveSearch)) {
            self.duplicate_saved(name);
        }
    }

    /// Applies the confirm button of whichever dialog is open. `name`
    /// is the current content of the name field (name dialogs only).
    pub fn dialog_confirm(&mut self, name: &str) {
        match self.dialog.take() {
            Some(Dialog::ConfirmDelete { id, name }) => self.delete_project(&id, &name),
            Some(Dialog::ConfirmBuild { id, .. }) => {
                self.selected = Some(id);
                self.start_build();
            }
            Some(Dialog::SaveSearch) => self.save_saved(name),
            Some(Dialog::RenameTab { id }) => {
                let name = name.trim().to_owned();
                if name.is_empty() {
                    self.dialog = Some(Dialog::RenameTab { id });
                } else if let Some(tab) = self.tab_by_id_mut(id) {
                    tab.custom_title = Some(name);
                }
            }
            Some(Dialog::ConfirmDeleteSaved { id, name }) => {
                if let Some(catalog) = &self.catalog {
                    match catalog.delete_saved_search(&id) {
                        Ok(()) => {
                            let text = self.tr.saved_deleted(&name);
                            self.push_notice(BannerLevel::Info, text, false);
                            // Every tab associated with the deleted
                            // entry loses its association — its next
                            // Save then creates a fresh entry.
                            for tab in &mut self.tabs {
                                if tab.loaded_saved_id.as_deref() == Some(id.as_str()) {
                                    tab.loaded_saved_id = None;
                                }
                            }
                            if self.selected_saved.as_deref() == Some(id.as_str()) {
                                self.selected_saved = None;
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

    // -- Banners ---------------------------------------------------------------

    /// The banner list, computed in one place from the current state.
    /// Order: ongoing work first, then screen context, then the
    /// newest notices (capped so events never bury the content).
    pub fn banners(&self) -> Vec<Banner> {
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
                action: Some((tr.cancel_build.to_owned(), BannerAction::CancelBuild)),
                dismiss: None,
                working: true,
            });
        }

        for tab in &self.tabs {
            if let Some(job) = &tab.job {
                out.push(Banner {
                    level: BannerLevel::Info,
                    text: tr.banner_searching(&job.query),
                    action: Some((tr.cancel.to_owned(), BannerAction::CancelSearch(tab.id))),
                    dismiss: None,
                    working: true,
                });
            }
        }

        if self.screen == Screen::Search && self.catalog.is_some() {
            match self.search_project() {
                None => out.push(Banner {
                    level: BannerLevel::Info,
                    text: tr.banner_no_project.to_owned(),
                    action: Some(if self.projects.is_empty() {
                        (tr.new_project.to_owned(), BannerAction::NewProject)
                    } else {
                        (tr.open_projects.to_owned(), BannerAction::OpenProjects)
                    }),
                    dismiss: None,
                    working: false,
                }),
                Some(p) => {
                    let building = self.build.as_ref().is_some_and(|b| b.project_id == p.id);
                    if !building && !p.index_db_path.exists() {
                        out.push(Banner {
                            level: BannerLevel::Info,
                            text: tr.banner_never_built.to_owned(),
                            action: Some((
                                tr.build_index.to_owned(),
                                BannerAction::StartBuild(p.id.clone()),
                            )),
                            dismiss: None,
                            working: false,
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
                                BannerAction::StartBuild(p.id.clone()),
                            )),
                            dismiss: None,
                            working: false,
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
                working: false,
            });
        }
        out
    }

    /// Runs the action bound to a banner button. Returns the editor
    /// form values when the action opened the project editor, so the
    /// caller can fill the dialog's fields.
    pub fn run_banner_action(&mut self, index: usize) -> Option<EditorValues> {
        let action = self.banners().get(index).and_then(|b| b.action.clone());
        match action.map(|a| a.1) {
            Some(BannerAction::CancelBuild) => self.cancel_build(),
            Some(BannerAction::CancelSearch(id)) => self.cancel_tab_search(id),
            Some(BannerAction::NewProject) => return Some(self.new_project()),
            Some(BannerAction::OpenProjects) => self.screen = Screen::Projects,
            Some(BannerAction::StartBuild(id)) => {
                self.ask_start_build_for(Some(id));
            }
            None => {}
        }
        None
    }

    pub fn dismiss_notice(&mut self, index: usize) {
        if index < self.notices.len() {
            self.notices.remove(index);
        }
    }

    // -- Preferences -----------------------------------------------------------

    /// Saves `self.prefs` through the catalog; failures surface as a
    /// sticky error notice.
    pub fn save_prefs(&mut self) {
        if let Some(catalog) = &self.catalog {
            if let Err(e) = catalog.save_preferences(&self.prefs) {
                let msg = e.to_string();
                self.push_notice(BannerLevel::Error, self.tr.prefs_save_failed(&msg), true);
            }
        }
    }

    pub fn set_language(&mut self, index: i32) {
        let Some(lang) = rsearch_catalog::Language::ALL.get(index as usize).copied() else {
            return;
        };
        if lang != self.prefs.language {
            self.prefs.language = lang;
            self.tr = tr::for_language(lang);
            self.save_prefs();
        }
    }

    /// Updates the theme preference; the controller applies the
    /// matching `Palette.color-scheme` to Slint.
    pub fn set_theme(&mut self, index: i32) {
        let pref = match index {
            1 => ThemePreference::Light,
            2 => ThemePreference::Dark,
            _ => ThemePreference::System,
        };
        if pref != self.prefs.theme {
            self.prefs.theme = pref;
            self.save_prefs();
        }
    }

    pub fn pref_dirs_edited(&mut self, text: &str) {
        self.prefs.default_excluded_dirs = util::parse_list(text);
        self.save_prefs();
    }

    pub fn pref_include_masks_edited(&mut self, text: &str) {
        self.prefs.default_include_masks = rsearch_engine::parse_masks(text);
        self.save_prefs();
    }

    pub fn pref_exclude_masks_edited(&mut self, text: &str) {
        self.prefs.default_exclude_masks = rsearch_engine::parse_masks(text);
        self.save_prefs();
    }

    /// Parses the MiB buffer; the preference only changes when the
    /// text is a positive whole number of MiB — non-numeric or zero
    /// text keeps the previous value. The inline error is derived from
    /// the field text at sync time (`sync_all`), same rule as the
    /// project editor's max-size field.
    pub fn pref_max_size_edited(&mut self, text: &str) {
        if let Some(mib) = util::parse_max_size_mib(text) {
            self.prefs.default_max_indexed_file_size = mib.saturating_mul(1024 * 1024);
            self.save_prefs();
        }
    }

    pub fn pref_check_toggled(&mut self, checked: bool) {
        if checked != self.prefs.check_for_updates {
            self.prefs.check_for_updates = checked;
            self.save_prefs();
        }
    }

    pub fn check_updates(&mut self) {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc};
    use std::time::Duration;

    use rsearch_catalog::{ProjectSettings, RootSpec, SearchParams};
    use rsearch_engine::report::PipelineTimings;
    use rsearch_engine::{BuildPhase, BuildSummary, PhaseDurations, ProgressSnapshot};
    use rsearch_engine::{Occurrence, SearchReport, SkippedRoot};

    use crate::results::{ResultContext, ResultList};
    use crate::viewer::{MatchPos, Seg, ViewerLine};

    // -- Helpers ----------------------------------------------------------

    /// An `App` without a catalog — tab logic never needs one.
    fn app() -> App {
        App {
            tr: &tr::EN,
            catalog: None,
            catalog_error: None,
            projects: Vec::new(),
            selected: None,
            screen: Screen::Search,
            prefs: AppPreferences::default(),
            dialog: None,
            build: None,
            last_report: None,
            notices: Vec::new(),
            tabs: vec![SearchTab::new(0)],
            active_tab: 0,
            next_tab_id: 1,
            saved: Vec::new(),
            selected_saved: None,
        }
    }

    /// A temp directory that removes itself on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> TempDir {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "rsearch-gui-{}-{}-{}",
                label,
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create temp dir");
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// An `App` backed by a real (temporary) catalog with one project
    /// selected — its index file exists so `run_search` can start a
    /// job (which then fails fast on the empty file).
    fn app_with_project() -> (App, TempDir) {
        let tmp = TempDir::new("app");
        let catalog = Catalog::open(tmp.0.join("projects.db")).expect("open catalog");
        let settings = ProjectSettings {
            roots: vec![RootSpec::new(tmp.0.clone())],
            ..ProjectSettings::default()
        };
        let project = catalog.create_project("proj", settings).expect("project");
        std::fs::write(&project.index_db_path, b"").expect("empty index file");
        let mut a = app();
        a.selected = Some(project.id.clone());
        a.projects = vec![project];
        a.catalog = Some(catalog);
        // The tab's own project selection — a snapshot of the
        // Projects screen's, never a live link.
        a.tabs[0].form.project_id = a.selected.clone();
        a.refresh_saved();
        (a, tmp)
    }

    fn occ(line: usize) -> Occurrence {
        Occurrence {
            line,
            column: 1,
            line_text: format!("line {line}"),
            context_before: Vec::new(),
            context_after: Vec::new(),
        }
    }

    fn file(path: &str, lines: &[usize]) -> FileResult {
        FileResult {
            file_path: PathBuf::from(path),
            entry_path: None,
            occurrences: lines.iter().map(|&l| occ(l)).collect(),
        }
    }

    fn report(files: Vec<FileResult>, too_large: usize) -> SearchReport {
        SearchReport {
            results: files,
            candidates_from_index: 0,
            candidates_too_large: too_large,
            skipped_stale: 0,
            skipped_index_errors: 0,
            skipped_security_limits: 0,
            verification_errors: 0,
            truncated_files: 0,
            archives_opened: 0,
            elapsed: Duration::from_millis(1),
        }
    }

    fn list(files: Vec<FileResult>, query: &str) -> ResultList {
        ResultList::new(
            report(files, 0),
            ResultContext {
                project_id: "p".into(),
                project_name: "proj".into(),
                query: query.into(),
                case_sensitive: false,
                whole_word: false,
            },
            false,
            0,
            0,
            false,
        )
    }

    /// Attaches a fake in-flight job to tab `i`; the returned flag and
    /// sender let the test drive the job's channel.
    fn fake_job(app: &mut App, i: usize) -> (Arc<AtomicBool>, mpsc::Sender<SearchMsg>) {
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let id = app.tabs[i].id;
        app.tabs[i].job = Some(SearchJob::for_test(id, cancel.clone(), rx));
        (cancel, tx)
    }

    fn result_files(app: &App, i: usize) -> Vec<String> {
        app.tabs[i].results.with(|l| {
            l.report
                .results
                .iter()
                .map(|r| r.file_path.display().to_string())
                .collect()
        })
    }

    // -- Tabs ------------------------------------------------------------

    #[test]
    fn new_tab_is_created_active_with_unique_ids() {
        let mut a = app();
        a.new_tab();
        a.new_tab();
        assert_eq!(a.tabs.len(), 3);
        assert_eq!(a.active_tab, 2);
        let mut ids: Vec<TabId> = a.tabs.iter().map(|t| t.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 3);
    }

    #[test]
    fn tabs_keep_independent_state() {
        let mut a = app();
        a.tab_mut().form.query = "first".into();
        a.new_tab();
        a.tab_mut().form.query = "second".into();
        a.tab_mut().form.case_sensitive = true;
        a.activate_tab(0);
        assert_eq!(a.tab().form.query, "first");
        assert!(!a.tab().form.case_sensitive);
        a.activate_tab(1);
        assert_eq!(a.tab().form.query, "second");
        assert!(a.tab().form.case_sensitive);
        // Out-of-range indices are ignored.
        a.activate_tab(99);
        a.activate_tab(-1);
        assert_eq!(a.active_tab, 1);
    }

    #[test]
    fn close_tab_removes_only_that_tab() {
        let mut a = app();
        a.tab_mut().form.query = "first".into();
        a.new_tab();
        a.tab_mut().form.query = "keep".into();
        a.new_tab();
        a.tab_mut().form.query = "gone".into();
        a.activate_tab(0);
        // Closing a non-active tab keeps the active one.
        a.close_tab(2);
        assert_eq!(a.tabs.len(), 2);
        assert_eq!(a.active_tab, 0);
        assert_eq!(a.tab().form.query, "first");
        assert_eq!(a.tabs[1].form.query, "keep");
        // Closing the active tab activates the next one.
        a.close_tab(0);
        assert_eq!(a.tab().form.query, "keep");
        // Closing the last tab leaves a fresh empty one.
        a.close_tab(0);
        assert_eq!(a.tabs.len(), 1);
        assert_eq!(a.tab().form.query, "");
        assert_eq!(a.tab().title(a.tr.nav_search), "Search");
    }

    // -- Tab titles --------------------------------------------------------

    #[test]
    fn automatic_title_follows_the_query() {
        let mut a = app();
        assert_eq!(a.tab().title(a.tr.nav_search), "Search");
        a.tab_mut().form.query = "SaveLocally".into();
        assert_eq!(a.tab().title(""), "SaveLocally");
        a.tab_mut().form.query = "SaveLocallyException".into();
        assert_eq!(a.tab().title(""), "SaveLocallyException");
        a.tab_mut().form.query.clear();
        assert_eq!(a.tab().title(a.tr.nav_search), "Search");
        a.tr = &tr::FR;
        assert_eq!(a.tab().title(a.tr.nav_search), "Recherche");
    }

    #[test]
    fn custom_title_is_fixed_until_reset() {
        let mut a = app();
        a.tab_mut().form.query = "foo".into();
        a.ask_rename_tab(0);
        assert!(matches!(a.dialog, Some(Dialog::RenameTab { .. })));
        assert!(!a.dialog_can_reset());
        a.dialog_confirm("Bug production");
        assert_eq!(a.tab().title(""), "Bug production");
        // Editing the query no longer moves the title.
        a.tab_mut().form.query = "SaveLocallyException".into();
        assert_eq!(a.tab().title(""), "Bug production");
        // The reset action drops the custom title.
        a.ask_rename_tab(0);
        assert!(a.dialog_can_reset());
        a.dialog_reset_name();
        assert!(a.dialog.is_none());
        assert_eq!(a.tab().title(""), "SaveLocallyException");
    }

    #[test]
    fn empty_rename_reopens_the_dialog() {
        let mut a = app();
        a.ask_rename_tab(0);
        a.dialog_confirm("   ");
        assert!(matches!(a.dialog, Some(Dialog::RenameTab { .. })));
        assert!(a.tab().custom_title.is_none());
        a.dialog_cancel();
    }

    #[test]
    fn long_titles_are_ellipsized_for_display_only() {
        let mut a = app();
        a.tab_mut().form.query = "x".repeat(80);
        let shown = a.tab().display_title("");
        assert_eq!(shown.chars().count(), TAB_TITLE_CHARS);
        assert!(shown.ends_with('…'));
        assert_eq!(a.tab().title(""), "x".repeat(80));
    }

    // -- Saved searches -----------------------------------------------------

    /// Registers a saved search in the test catalog and refreshes the
    /// app's cache — the combo then lists it at row `position + 1`.
    fn make_saved(a: &mut App, name: &str, query: &str, params: SearchParams) -> SavedSearch {
        let saved = a
            .catalog
            .as_ref()
            .unwrap()
            .create_saved_search(
                &a.tab().form.project_id.clone().unwrap(),
                name,
                query,
                params,
            )
            .expect("saved search");
        a.refresh_saved();
        saved
    }

    /// 1-based combo row of `id` in the saved list.
    fn saved_row(a: &App, id: &str) -> i32 {
        a.saved.iter().position(|s| s.id == id).unwrap() as i32 + 1
    }

    #[test]
    fn new_tab_has_no_saved_search_association() {
        let mut a = app();
        assert!(a.tab().loaded_saved_id.is_none());
        a.new_tab();
        assert!(a.tab().loaded_saved_id.is_none());
    }

    #[test]
    fn select_saved_only_marks_the_combo_selection() {
        let (mut a, _tmp) = app_with_project();
        make_saved(&mut a, "Licences Eclipse", "W3C", SearchParams::default());
        a.tab_mut().form.query = "local".into();
        a.select_saved(1);
        assert_eq!(a.saved_index(), 1);
        // Nothing in the tab changed.
        assert_eq!(a.tab().form.query, "local");
        assert_eq!(a.tab().title(""), "local");
        assert!(a.tab().loaded_saved_id.is_none());
        // Row 0 (the placeholder) clears the selection.
        a.select_saved(0);
        assert_eq!(a.saved_index(), 0);
        assert!(a.selected_saved.is_none());
    }

    #[test]
    fn load_saved_fills_a_new_tab_without_searching() {
        let (mut a, _tmp) = app_with_project();
        let saved = make_saved(
            &mut a,
            "Licences Eclipse",
            "W3C",
            SearchParams {
                case_sensitive: true,
                include_masks: vec!["*.java".into()],
                exclude_masks: vec!["Test*".into()],
                ..SearchParams::default()
            },
        );
        a.tab_mut().form.query = "local".into();
        a.select_saved(1);
        a.load_saved();
        // A new tab was opened and activated; the old one is intact.
        assert_eq!(a.tabs.len(), 2);
        assert_eq!(a.active_tab, 1);
        assert_eq!(a.tabs[0].form.query, "local");
        let tab = a.tab();
        assert_eq!(tab.loaded_saved_id.as_deref(), Some(saved.id.as_str()));
        assert_eq!(tab.form.query, "W3C");
        assert!(tab.form.case_sensitive);
        assert_eq!(tab.form.include_text, "*.java");
        assert_eq!(tab.form.exclude_text, "Test*");
        assert!(tab.job.is_none());
        // The automatic title starts on the saved name — and the save
        // dialog proposes it so Enregistrer updates by default.
        assert_eq!(tab.title(""), "Licences Eclipse");
        assert_eq!(a.suggested_saved_name(), "Licences Eclipse");
    }

    #[test]
    fn load_saved_activates_the_tab_already_holding_it() {
        let (mut a, _tmp) = app_with_project();
        let saved = make_saved(&mut a, "Licences Eclipse", "W3C", SearchParams::default());
        a.select_saved(1);
        a.load_saved();
        assert_eq!(a.active_tab, 1);
        // Move elsewhere and mutate the loaded tab's form.
        a.tab_mut().form.case_sensitive = true;
        a.new_tab();
        a.tab_mut().form.query = "local".into();
        // Loading the same entry again just brings its tab back —
        // no third tab, no refill.
        a.select_saved(1);
        a.load_saved();
        assert_eq!(a.tabs.len(), 3);
        assert_eq!(a.active_tab, 1);
        assert_eq!(a.tab().loaded_saved_id.as_deref(), Some(saved.id.as_str()));
        assert!(a.tab().form.case_sensitive);
    }

    #[test]
    fn save_with_unchanged_label_updates_the_same_entry() {
        let (mut a, _tmp) = app_with_project();
        let saved = make_saved(&mut a, "Licences Eclipse", "W3C", SearchParams::default());
        a.select_saved(1);
        a.load_saved();
        // Options changed, name kept in the save dialog.
        a.tab_mut().form.case_sensitive = true;
        a.tab_mut().form.context_lines = 4;
        a.ask_save_search();
        a.dialog_confirm("Licences Eclipse");
        assert_eq!(a.tab().loaded_saved_id.as_deref(), Some(saved.id.as_str()));
        assert_eq!(a.saved.len(), 1);
        let e = a
            .catalog
            .as_ref()
            .unwrap()
            .get_saved_search(&saved.id)
            .unwrap();
        assert_eq!(e.name, "Licences Eclipse");
        assert_eq!(e.query, "W3C");
        assert!(e.params.case_sensitive);
        assert_eq!(e.params.context_lines, 4);
    }

    #[test]
    fn save_with_changed_query_still_updates_the_same_entry() {
        let (mut a, _tmp) = app_with_project();
        let saved = make_saved(&mut a, "Licences Eclipse", "W3C", SearchParams::default());
        a.select_saved(1);
        a.load_saved();
        a.tab_mut().form.query = "GPL".into();
        a.ask_save_search();
        a.dialog_confirm("Licences Eclipse");
        assert_eq!(a.tab().loaded_saved_id.as_deref(), Some(saved.id.as_str()));
        assert_eq!(a.saved.len(), 1);
        let e = a
            .catalog
            .as_ref()
            .unwrap()
            .get_saved_search(&saved.id)
            .unwrap();
        assert_eq!(e.name, "Licences Eclipse");
        assert_eq!(e.query, "GPL");
    }

    #[test]
    fn save_with_changed_name_updates_the_same_entry() {
        let (mut a, _tmp) = app_with_project();
        let first = make_saved(
            &mut a,
            "Licences Eclipse",
            "W3C",
            SearchParams {
                case_sensitive: true,
                ..SearchParams::default()
            },
        );
        let other = make_saved(&mut a, "GPL", "gpl license", SearchParams::default());
        a.select_saved(saved_row(&a, &first.id));
        a.load_saved();
        // Renaming in the save dialog is a modification of the SAME
        // entry — even when the new name collides with another entry.
        a.ask_save_search();
        a.dialog_confirm("GPL");
        assert_eq!(a.tab().loaded_saved_id.as_deref(), Some(first.id.as_str()));
        assert_eq!(a.saved.len(), 2);
        let catalog = a.catalog.as_ref().unwrap();
        let e = catalog.get_saved_search(&first.id).unwrap();
        assert_eq!(e.name, "GPL");
        assert_eq!(e.query, "W3C");
        assert!(e.params.case_sensitive);
        // The other entry already named "GPL" is untouched — same
        // names may coexist.
        let e = catalog.get_saved_search(&other.id).unwrap();
        assert_eq!(e.name, "GPL");
        assert_eq!(e.query, "gpl license");
    }

    #[test]
    fn duplicate_always_creates_a_new_entry() {
        let (mut a, _tmp) = app_with_project();
        let saved = make_saved(
            &mut a,
            "Licences Eclipse",
            "W3C",
            SearchParams {
                case_sensitive: true,
                ..SearchParams::default()
            },
        );
        a.select_saved(1);
        a.load_saved();
        a.tab_mut().form.context_lines = 8;
        a.ask_save_search();
        assert!(a.dialog_can_duplicate());
        // Duplicating under a different name copies the CURRENT form
        // and leaves the original untouched.
        a.dialog_duplicate("Licences W3C");
        let copy_id = a.tab().loaded_saved_id.clone().unwrap();
        assert_ne!(copy_id, saved.id);
        assert_eq!(a.saved.len(), 2);
        let catalog = a.catalog.as_ref().unwrap();
        let e = catalog.get_saved_search(&saved.id).unwrap();
        assert_eq!(e.name, "Licences Eclipse");
        assert_eq!(e.query, "W3C");
        assert_ne!(e.params.context_lines, 8);
        let e = catalog.get_saved_search(&copy_id).unwrap();
        assert_eq!(e.name, "Licences W3C");
        assert_eq!(e.query, "W3C");
        assert_eq!(e.params.context_lines, 8);
        // Duplicating under the SAME name works too.
        a.ask_save_search();
        a.dialog_duplicate("Licences W3C");
        assert_eq!(a.saved.len(), 3);
        let same_name = a.saved.iter().filter(|s| s.name == "Licences W3C").count();
        assert_eq!(same_name, 2);
    }

    #[test]
    fn duplicate_is_only_offered_on_an_associated_tab() {
        let (mut a, _tmp) = app_with_project();
        a.tab_mut().form.query = "local search".into();
        a.ask_save_search();
        assert!(matches!(a.dialog, Some(Dialog::SaveSearch)));
        assert!(!a.dialog_can_duplicate());
        a.dialog_cancel();
    }

    #[test]
    fn deleting_a_loaded_saved_search_clears_the_association() {
        let (mut a, _tmp) = app_with_project();
        let saved = make_saved(&mut a, "Licences Eclipse", "W3C", SearchParams::default());
        a.select_saved(1);
        a.load_saved();
        a.ask_delete_saved();
        assert!(matches!(a.dialog, Some(Dialog::ConfirmDeleteSaved { .. })));
        a.dialog_confirm("");
        // The entry is gone and the tab lost its association — its
        // form is untouched.
        assert_eq!(a.saved.len(), 0);
        assert_eq!(a.saved_index(), 0);
        assert!(a.tab().loaded_saved_id.is_none());
        assert_eq!(a.tab().form.query, "W3C");
        // Saving now creates a fresh entry under a new id.
        a.ask_save_search();
        a.dialog_confirm("Licences Eclipse");
        assert_eq!(a.saved.len(), 1);
        assert_ne!(a.saved[0].id, saved.id);
        assert_eq!(
            a.tab().loaded_saved_id.as_deref(),
            Some(a.saved[0].id.as_str())
        );
    }

    #[test]
    fn delete_clears_every_tab_associated_with_the_entry() {
        let (mut a, _tmp) = app_with_project();
        let saved = make_saved(
            &mut a,
            "Licences Eclipse",
            "W3C",
            SearchParams {
                whole_word: true,
                ..SearchParams::default()
            },
        );
        // Load it (opens a tab), then associate a second tab with the
        // same id — Charger dedups, so the association is set up
        // directly, the way several tabs could still share it.
        a.select_saved(1);
        a.load_saved();
        a.new_tab();
        a.tab_mut().loaded_saved_id = Some(saved.id.clone());
        a.tab_mut().form.query = "W3C".into();
        a.tab_mut().form.whole_word = true;
        assert_eq!(a.tabs.len(), 3);
        assert_eq!(
            a.tabs[1].loaded_saved_id.as_deref(),
            Some(saved.id.as_str())
        );
        assert_eq!(
            a.tabs[2].loaded_saved_id.as_deref(),
            Some(saved.id.as_str())
        );
        a.ask_delete_saved();
        a.dialog_confirm("");
        // Both associated tabs lose the association; their forms
        // stay as loaded and the first tab is untouched.
        assert_eq!(a.tabs[0].form.query, "");
        for tab in &a.tabs[1..] {
            assert!(tab.loaded_saved_id.is_none());
            assert_eq!(tab.form.query, "W3C");
            assert!(tab.form.whole_word);
        }
    }

    #[test]
    fn deleting_an_unrelated_saved_search_keeps_tab_associations() {
        let (mut a, _tmp) = app_with_project();
        let loaded = make_saved(&mut a, "Licences Eclipse", "W3C", SearchParams::default());
        let other = make_saved(&mut a, "GPL", "gpl", SearchParams::default());
        // The tab is associated with the first entry; the combo
        // selects (and deletes) the second.
        a.select_saved(saved_row(&a, &loaded.id));
        a.load_saved();
        a.select_saved(saved_row(&a, &other.id));
        a.ask_delete_saved();
        a.dialog_confirm("");
        assert_eq!(a.saved.len(), 1);
        assert_eq!(a.saved[0].id, loaded.id);
        assert_eq!(a.tab().loaded_saved_id.as_deref(), Some(loaded.id.as_str()));
    }

    // -- Build outcomes ---------------------------------------------------

    /// A minimal successful `BuildReport` with `files` documents
    /// indexed this run.
    fn build_report(kind: BuildKind, files: usize, duration: Duration) -> BuildReport {
        BuildReport {
            counters: ProgressSnapshot::default(),
            total_errors: 0,
            errors: Vec::new(),
            omitted_errors: 0,
            durations: PhaseDurations::default(),
            timings: PipelineTimings::default(),
            skipped_roots: Vec::new(),
            excluded_directories: BTreeMap::new(),
            index_size: None,
            sqlite_version: String::new(),
            cancelled: false,
            summary: BuildSummary {
                indexed_files: files,
                top_extensions: Vec::new(),
                ignored_by_name: 0,
                ignored_by_sniff: 0,
                too_large: 0,
                errors: 0,
                security_limits: 0,
                archives_processed: 0,
                archive_entries_indexed: 0,
                duration,
                archives_included: false,
                kind,
                update_delta: None,
            },
        }
    }

    #[test]
    fn zero_file_full_build_warns_stickily() {
        let mut a = app();
        a.finish_build(
            "p",
            &ProjectSettings::default(),
            Ok(build_report(BuildKind::Full, 0, Duration::from_secs(2))),
        );
        let n = a.notices.last().expect("a notice was pushed");
        assert!(matches!(n.level, BannerLevel::Warning));
        assert!(n.sticky, "the warning must stay until dismissed");
    }

    #[test]
    fn productive_build_reports_transient_success() {
        let mut a = app();
        a.finish_build(
            "p",
            &ProjectSettings::default(),
            Ok(build_report(BuildKind::Full, 5, Duration::from_secs(2))),
        );
        let n = a.notices.last().expect("a notice was pushed");
        assert!(matches!(n.level, BannerLevel::Success));
        assert!(!n.sticky);
    }

    #[test]
    fn no_op_update_does_not_warn_about_zero_files() {
        // An incremental update that re-indexed nothing is a normal
        // outcome — only a full build with zero files is suspicious.
        let mut a = app();
        a.finish_build(
            "p",
            &ProjectSettings::default(),
            Ok(build_report(BuildKind::Update, 0, Duration::from_secs(1))),
        );
        let n = a.notices.last().expect("a notice was pushed");
        assert!(matches!(n.level, BannerLevel::Success));
        assert!(!n.sticky);
    }

    // -- Editor validation --------------------------------------------------

    /// Opens a create-mode editor dialog with one root on `tmp` and
    /// returns the form values with `name` and `max_size` set.
    fn editor_values(a: &mut App, tmp: &TempDir, name: &str, max_size: &str) -> EditorValues {
        let mut values = a.new_project();
        assert!(a.editor_add_root());
        assert!(a.editor_root_path(0, tmp.0.display().to_string()));
        values.name = name.into();
        values.max_size_text = max_size.into();
        values
    }

    #[test]
    fn apply_editor_rejects_non_numeric_max_size_inline() {
        let (mut a, tmp) = app_with_project();
        let values = editor_values(&mut a, &tmp, "second", "abc");
        let err = a.apply_editor(values).unwrap_err();
        assert_eq!(err, tr::EN.err_max_size_invalid);
        // The dialog stays open (inline error) and nothing was created.
        assert!(matches!(a.dialog, Some(Dialog::Editor { .. })));
        assert_eq!(a.projects.len(), 1);
    }

    #[test]
    fn apply_editor_rejects_zero_max_size_inline() {
        let (mut a, tmp) = app_with_project();
        let values = editor_values(&mut a, &tmp, "second", "0");
        let err = a.apply_editor(values).unwrap_err();
        assert_eq!(err, tr::EN.err_max_size_invalid);
        assert_eq!(a.projects.len(), 1);
    }

    #[test]
    fn apply_editor_accepts_a_valid_max_size() {
        let (mut a, tmp) = app_with_project();
        let values = editor_values(&mut a, &tmp, "second", "12");
        assert!(a.apply_editor(values).is_ok());
        assert_eq!(a.projects.len(), 2);
    }

    #[test]
    fn prefs_max_size_rejects_invalid_and_zero_keeps_previous() {
        let mut a = app();
        a.prefs.default_max_indexed_file_size = 16 * 1024 * 1024;
        a.pref_max_size_edited("abc");
        assert_eq!(a.prefs.default_max_indexed_file_size, 16 * 1024 * 1024);
        a.pref_max_size_edited("0");
        assert_eq!(a.prefs.default_max_indexed_file_size, 16 * 1024 * 1024);
        a.pref_max_size_edited(" 12 ");
        assert_eq!(a.prefs.default_max_indexed_file_size, 12 * 1024 * 1024);
    }

    // -- Build start guard --------------------------------------------------

    #[test]
    fn second_build_is_refused_while_one_runs() {
        let (mut a, tmp) = app_with_project();
        // A project whose root is a genuinely empty directory: its
        // build completes with zero indexed files.
        let empty_root = tmp.0.join("empty-root");
        std::fs::create_dir(&empty_root).expect("empty root");
        let catalog = a.catalog.as_ref().unwrap();
        let settings = ProjectSettings {
            roots: vec![RootSpec::new(empty_root)],
            ..ProjectSettings::default()
        };
        let project = catalog.create_project("empty", settings).expect("project");
        a.refresh();
        a.selected = Some(project.id.clone());

        a.start_build();
        let first = a
            .build
            .as_ref()
            .expect("first build started")
            .project_id
            .clone();

        // A second start while the first runs is refused with a sticky
        // notice; the running build is untouched.
        let before = a.notices.len();
        a.start_build();
        assert_eq!(a.notices.len(), before + 1);
        assert!(a.notices.last().unwrap().sticky);
        assert_eq!(a.build.as_ref().unwrap().project_id, first);

        // The build completes (empty root → zero files) and the sticky
        // zero-files warning fires — never only the collapsed summary.
        let mut tries = 0;
        while a.poll_build() {
            tries += 1;
            assert!(tries < 2000, "build did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(a.build.is_none());
        assert!(
            a.notices
                .iter()
                .any(|n| n.sticky && n.text.contains("0 files")),
            "zero-file build must warn"
        );

        // Once finished, starting a new build works normally.
        a.start_build();
        assert!(a.build.is_some());
    }

    #[test]
    fn banner_start_build_is_refused_while_a_build_runs() {
        let (mut a, tmp) = app_with_project();
        // A second project without an index on disk: the Search screen
        // shows its "never built" banner with a StartBuild action.
        let catalog = a.catalog.as_ref().unwrap();
        let settings = ProjectSettings {
            roots: vec![RootSpec::new(tmp.0.clone())],
            ..ProjectSettings::default()
        };
        let other = catalog
            .create_project("no-index", settings)
            .expect("project");
        a.refresh();

        // Start a build on the selected project, then try the other
        // project's banner — the orphaning path the guard must close.
        a.start_build();
        let first = a
            .build
            .as_ref()
            .expect("first build started")
            .project_id
            .clone();
        // The Search screen's banners describe the active tab's
        // project, not the Projects screen's selection.
        a.tab_mut().form.project_id = Some(other.id.clone());
        let idx = a
            .banners()
            .iter()
            .position(|b| matches!(&b.action, Some((_, BannerAction::StartBuild(_)))))
            .expect("never-built banner offers StartBuild");
        let before = a.notices.len();
        a.run_banner_action(idx);
        assert_eq!(a.notices.len(), before + 1, "refusal notice shown");
        assert_eq!(a.build.as_ref().unwrap().project_id, first);
    }

    // -- Recoverable infrastructure failures ---------------------------------

    #[test]
    fn save_without_selection_is_ignored() {
        let (mut a, _tmp) = app_with_project();
        a.tab_mut().form.project_id = None;
        a.dialog = Some(Dialog::SaveSearch);
        a.tab_mut().form.query = "abc".into();
        a.save_saved("name");
        assert!(a.saved.is_empty(), "nothing saved without a project");
        // The dialog is not force-closed by the ignored save.
        assert!(matches!(a.dialog, Some(Dialog::SaveSearch)));
    }

    #[test]
    fn start_build_on_a_vanished_project_is_reported() {
        let (mut a, _tmp) = app_with_project();
        a.selected = Some("gone".into());
        let before = a.notices.len();
        a.start_build();
        assert_eq!(a.notices.len(), before + 1, "the refusal is reported");
        assert!(a.build.is_none());
    }

    #[test]
    fn run_search_spawns_a_job_or_reports_failure() {
        let (mut a, _tmp) = app_with_project();
        a.tab_mut().form.query = "abc".into();
        a.run_search();
        // Either the thread spawned (job in flight) or the failure was
        // reported — never a panic, never a silent no-op.
        assert!(a.tab().job.is_some() || a.notices.iter().any(|n| n.level == BannerLevel::Error));
    }

    // -- Project selection decoupling ------------------------------------------

    #[test]
    fn projects_screen_selection_does_not_move_the_search_tab() {
        let (mut a, tmp) = app_with_project();
        let catalog = a.catalog.as_ref().unwrap();
        let settings = ProjectSettings {
            roots: vec![RootSpec::new(tmp.0.clone())],
            ..ProjectSettings::default()
        };
        let other = catalog.create_project("other", settings).expect("project");
        a.refresh();
        let tab_project = a.tab().form.project_id.clone().unwrap();
        // Selecting in the Projects screen list never touches the tab.
        let idx = a.projects.iter().position(|p| p.id == other.id).unwrap() as i32;
        a.select_project(idx);
        assert_eq!(a.selected.as_deref(), Some(other.id.as_str()));
        assert_eq!(
            a.tab().form.project_id.as_deref(),
            Some(tab_project.as_str())
        );
        // And the other way: the tab's picker leaves the Projects
        // screen's selection alone.
        a.select_search_project(0);
        assert_eq!(a.selected.as_deref(), Some(other.id.as_str()));
    }

    #[test]
    fn search_project_selection_is_per_tab() {
        let (mut a, tmp) = app_with_project();
        let catalog = a.catalog.as_ref().unwrap();
        let settings = ProjectSettings {
            roots: vec![RootSpec::new(tmp.0.clone())],
            ..ProjectSettings::default()
        };
        let other = catalog.create_project("other", settings).expect("project");
        a.refresh();
        let first = a.tab().form.project_id.clone().unwrap();
        a.new_tab();
        // The new tab starts on the same project — a snapshot, not a
        // live link.
        assert_eq!(a.tab().form.project_id.as_deref(), Some(first.as_str()));
        let idx = a.projects.iter().position(|p| p.id == other.id).unwrap() as i32;
        a.select_search_project(idx);
        assert_eq!(a.tab().form.project_id.as_deref(), Some(other.id.as_str()));
        // The first tab kept its own project.
        assert_eq!(a.tabs[0].form.project_id.as_deref(), Some(first.as_str()));
        // Switching tabs restores each tab's own selection.
        a.activate_tab(0);
        assert_eq!(a.search_project().unwrap().id, first);
        a.activate_tab(1);
        assert_eq!(a.search_project().unwrap().id, other.id);
    }

    #[test]
    fn renaming_a_project_updates_its_name_in_search_tabs() {
        let (mut a, _tmp) = app_with_project();
        let id = a.tab().form.project_id.clone().unwrap();
        a.catalog
            .as_ref()
            .unwrap()
            .rename_project(&id, "Renamed")
            .expect("rename");
        a.refresh();
        // Same stable project id, new name — the picker model is
        // rebuilt from the projects cache, so every tab shows the new
        // name without its selection moving.
        assert_eq!(a.tab().form.project_id.as_deref(), Some(id.as_str()));
        assert_eq!(a.search_project().unwrap().name, "Renamed");
    }

    #[test]
    fn deleting_a_project_used_by_a_search_tab_is_refused() {
        let (mut a, _tmp) = app_with_project();
        let id = a.tab().form.project_id.clone().unwrap();
        a.ask_delete_project();
        assert!(
            a.dialog.is_none(),
            "no confirmation for a project a tab still uses"
        );
        assert!(a
            .notices
            .iter()
            .any(|n| n.level == BannerLevel::Warning && n.sticky));
        // Once no tab references it, deletion proceeds normally.
        a.tab_mut().form.project_id = None;
        a.ask_delete_project();
        match &a.dialog {
            Some(Dialog::ConfirmDelete { id: dialog_id, .. }) => {
                assert_eq!(dialog_id, &id)
            }
            _ => panic!("expected the delete confirmation dialog"),
        }
    }

    // -- Build confirmation -------------------------------------------------

    #[test]
    fn ask_start_build_opens_confirmation_never_built() {
        let (mut a, _tmp) = app_with_project();
        a.ask_start_build();
        match &a.dialog {
            Some(Dialog::ConfirmBuild {
                update,
                estimate,
                archives,
                ..
            }) => {
                assert!(!*update, "never built → rebuild");
                assert!(estimate.is_none(), "no duration reference yet");
                // The flag mirrors the project's settings, whatever the
                // default (engine default is enabled; the GUI defaults
                // for new projects come from the preferences, D15).
                assert_eq!(
                    *archives,
                    a.selected_project().unwrap().settings.archives_enabled
                );
            }
            _ => panic!("expected the build confirmation dialog"),
        }
        assert!(a.build.is_none(), "asking never starts the build");
    }

    #[test]
    fn ask_start_build_refuses_while_a_build_runs() {
        let (mut a, _tmp) = app_with_project();
        a.start_build();
        let before = a.notices.len();
        a.ask_start_build();
        assert_eq!(a.notices.len(), before + 1, "refusal notice");
        assert!(
            !matches!(a.dialog, Some(Dialog::ConfirmBuild { .. })),
            "no dialog for a build that would be refused"
        );
    }

    #[test]
    fn confirm_dialog_cancel_does_not_start() {
        let (mut a, _tmp) = app_with_project();
        a.ask_start_build();
        a.dialog_cancel();
        assert!(a.dialog.is_none());
        assert!(a.build.is_none());
    }

    #[test]
    fn confirm_dialog_confirm_starts_the_build() {
        let (mut a, tmp) = app_with_project();
        let empty_root = tmp.0.join("confirm-root");
        std::fs::create_dir(&empty_root).expect("root");
        let catalog = a.catalog.as_ref().unwrap();
        let settings = ProjectSettings {
            roots: vec![RootSpec::new(empty_root)],
            ..ProjectSettings::default()
        };
        let project = catalog
            .create_project("confirm", settings)
            .expect("project");
        a.refresh();
        a.selected = Some(project.id.clone());
        a.ask_start_build();
        a.dialog_confirm("");
        assert!(a.dialog.is_none(), "confirm consumes the dialog");
        assert!(a.build.is_some());
    }

    #[test]
    fn confirmation_after_a_build_carries_its_duration() {
        let (mut a, tmp) = app_with_project();
        let empty_root = tmp.0.join("estimate-root");
        std::fs::create_dir(&empty_root).expect("root");
        let catalog = a.catalog.as_ref().unwrap();
        let settings = ProjectSettings {
            roots: vec![RootSpec::new(empty_root)],
            ..ProjectSettings::default()
        };
        let project = catalog
            .create_project("estimate", settings)
            .expect("project");
        a.refresh();
        a.selected = Some(project.id.clone());
        a.start_build();
        let mut tries = 0;
        while a.poll_build() {
            tries += 1;
            assert!(tries < 2000, "build did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
        a.ask_start_build();
        match &a.dialog {
            Some(Dialog::ConfirmBuild {
                update, estimate, ..
            }) => {
                assert!(*update, "a built project updates");
                assert!(
                    estimate.is_some(),
                    "the last build duration is the reference"
                );
            }
            _ => panic!("expected the build confirmation dialog"),
        }
    }

    #[test]
    fn confirmation_carries_the_archives_flag() {
        let (mut a, tmp) = app_with_project();
        let catalog = a.catalog.as_ref().unwrap();
        let settings = ProjectSettings {
            roots: vec![RootSpec::new(tmp.0.clone())],
            archives_enabled: true,
            ..ProjectSettings::default()
        };
        let project = catalog
            .create_project("archives", settings)
            .expect("project");
        a.refresh();
        a.selected = Some(project.id.clone());
        a.ask_start_build();
        match &a.dialog {
            Some(Dialog::ConfirmBuild { archives, .. }) => assert!(*archives),
            _ => panic!("expected the build confirmation dialog"),
        }
    }

    // -- Post-build report ---------------------------------------------------

    #[test]
    fn build_report_details_are_kept_and_announced() {
        let mut a = app();
        let mut report = build_report(BuildKind::Full, 3, Duration::from_secs(1));
        report.total_errors = 2;
        report.omitted_errors = 1;
        report.skipped_roots = vec![SkippedRoot {
            path: PathBuf::from("C:\\dup"),
            reason: "duplicate of source root C:\\a".into(),
        }];
        a.finish_build("p", &ProjectSettings::default(), Ok(report));
        // Sticky issue notice — visible without opening any section.
        assert!(a
            .notices
            .iter()
            .any(|n| n.sticky && matches!(n.level, BannerLevel::Warning)));
        // The report is kept for the detail display.
        let (id, stored) = a.last_report.as_ref().expect("report stored");
        assert_eq!(id, "p");
        assert_eq!(stored.total_errors, 2);
        assert_eq!(stored.skipped_roots.len(), 1);
    }

    #[test]
    fn clean_build_pushes_no_issue_notice() {
        let mut a = app();
        a.finish_build(
            "p",
            &ProjectSettings::default(),
            Ok(build_report(BuildKind::Full, 3, Duration::from_secs(1))),
        );
        assert!(
            a.notices
                .iter()
                .all(|n| !matches!(n.level, BannerLevel::Warning)),
            "a clean build raises no issue"
        );
        assert!(a.last_report.is_some());
    }

    #[test]
    fn cancelled_build_keeps_its_partial_report() {
        let mut a = app();
        let report = build_report(BuildKind::Full, 1, Duration::from_secs(1));
        a.finish_build(
            "p",
            &ProjectSettings::default(),
            Err(BuildError::Cancelled {
                report: Box::new(report),
            }),
        );
        assert!(a.last_report.is_some(), "partial report kept");
        let n = a.notices.last().unwrap();
        assert!(matches!(n.level, BannerLevel::Info));
    }

    // -- Phase names -----------------------------------------------------------

    #[test]
    fn phase_names_are_localized_per_language() {
        assert_eq!(tr::EN.phase_name(BuildPhase::Scanning), "Scanning");
        assert_eq!(tr::FR.phase_name(BuildPhase::Scanning), "Analyse");
        assert_eq!(tr::ES.phase_name(BuildPhase::Swapping), "Activando");
        assert_eq!(tr::EN.phase_name(BuildPhase::Failed), "Failed");
    }

    // -- Asynchronous job routing ----------------------------------------------

    #[test]
    fn job_events_only_reach_their_own_tab() {
        let mut a = app();
        a.new_tab();
        let (_c0, tx0) = fake_job(&mut a, 0);
        let (_c1, tx1) = fake_job(&mut a, 1);

        tx0.send(SearchMsg::Initial(report(vec![file("a.txt", &[1])], 0)))
            .unwrap();
        tx1.send(SearchMsg::Initial(report(vec![file("b.txt", &[2])], 2)))
            .unwrap();
        // Phase-B event of tab 1's job — lands in tab 1 only.
        tx1.send(SearchMsg::Progress {
            done: 1,
            total: 2,
            found: Some(file("b-big.txt", &[9])),
        })
        .unwrap();
        tx0.send(SearchMsg::Done(Ok(report(vec![file("a.txt", &[1])], 0))))
            .unwrap();

        assert!(a.poll_search());

        assert_eq!(result_files(&a, 0), vec!["a.txt"]);
        assert!(a.tabs[0].job.is_none(), "Done consumed the job");
        let l1: Vec<String> = a.tabs[1].results.with(|l| {
            assert!(l.in_flight);
            assert_eq!(l.oversized_done, 1);
            assert_eq!(l.oversized_total, 2);
            l.report
                .results
                .iter()
                .map(|r| r.file_path.display().to_string())
                .collect()
        });
        assert_eq!(l1, vec!["b-big.txt", "b.txt"]);
        assert!(a.tabs[1].job.is_some(), "tab 1's job still running");
    }

    #[test]
    fn closing_a_tab_cancels_its_job_and_late_messages_die() {
        let mut a = app();
        a.new_tab();
        let (cancel0, tx0) = fake_job(&mut a, 0);
        let (cancel1, tx1) = fake_job(&mut a, 1);

        a.close_tab(0);
        assert!(cancel0.load(Ordering::Acquire), "closing cancels");
        assert!(!cancel1.load(Ordering::Acquire));
        assert_eq!(a.tabs.len(), 1);
        assert_eq!(a.active_tab, 0);

        // The closed tab's channel is gone: a late Initial cannot
        // reach the surviving tab — or anywhere.
        let _ = tx0.send(SearchMsg::Initial(report(vec![file("a.txt", &[1])], 0)));
        tx1.send(SearchMsg::Done(Ok(report(vec![file("b.txt", &[2])], 0))))
            .unwrap();
        a.poll_search();
        assert_eq!(result_files(&a, 0), vec!["b.txt"]);
        assert!(a.tabs[0].job.is_none());
    }

    #[test]
    fn cancelled_search_marks_the_tab_results() {
        let mut a = app();
        let (_c, tx) = fake_job(&mut a, 0);
        tx.send(SearchMsg::Initial(report(vec![file("a.txt", &[1])], 0)))
            .unwrap();
        tx.send(SearchMsg::Done(Err(SearchError::Cancelled)))
            .unwrap();
        a.poll_search();
        a.tabs[0].results.with(|l| {
            assert!(l.cancelled);
            assert!(!l.in_flight);
        });
        assert!(a.tabs[0].job.is_none());
    }

    // -- Viewer ------------------------------------------------------------

    #[test]
    fn viewer_and_selection_are_per_tab() {
        let mut a = app();
        a.new_tab();
        a.tabs[0]
            .results
            .replace(list(vec![file("a.txt", &[1, 2])], "a"));
        a.tabs[0].results.select(0, 1);
        a.tabs[1]
            .results
            .replace(list(vec![file("b.txt", &[3])], "b"));

        // Selections are independent.
        assert_eq!(a.tabs[0].results.with(|l| l.selected), Some((0, 1)));
        assert_eq!(a.tabs[1].results.with(|l| l.selected), None);

        // Opening the viewer touches only the active tab.
        a.activate_tab(1);
        a.open_viewer(0, 0);
        assert!(a.tabs[1].viewer.is_some());
        assert!(a.tabs[1].viewer.as_ref().unwrap().loading);
        assert!(a.tabs[0].viewer.is_none());
        a.close_viewer();
        assert!(a.tabs[1].viewer.is_none());
    }

    #[test]
    fn viewer_navigation_wraps_and_stays_in_the_tab() {
        let mut a = app();
        let lines = vec![
            ViewerLine {
                num: 1,
                segs: vec![Seg {
                    text: "x".into(),
                    hit: true,
                }],
            },
            ViewerLine {
                num: 2,
                segs: vec![Seg {
                    text: "x".into(),
                    hit: true,
                }],
            },
        ];
        let matches = vec![
            MatchPos {
                line: 1,
                hit: 0,
                column: 1,
            },
            MatchPos {
                line: 2,
                hit: 0,
                column: 1,
            },
        ];
        let tab = a.tab_mut();
        tab.viewer_lines.set_lines(lines, Some(matches[0]));
        tab.viewer = Some(Viewer {
            title: "f".into(),
            focus_line: 1,
            loading: false,
            error: None,
            truncated: false,
            matches,
            match_idx: 0,
            nav_flip: false,
        });
        a.viewer_navigate(1);
        assert_eq!(a.tab().viewer.as_ref().unwrap().match_idx, 1);
        assert_eq!(a.tab().viewer.as_ref().unwrap().focus_line, 2);
        a.viewer_navigate(1);
        assert_eq!(a.tab().viewer.as_ref().unwrap().match_idx, 0);
        a.viewer_navigate(-1);
        assert_eq!(a.tab().viewer.as_ref().unwrap().match_idx, 1);
    }
}
