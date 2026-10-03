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
    AppPreferences, Catalog, Project, ProjectSettings, SavedSearch, SearchParams, ThemePreference,
};
use rsearch_engine::{BuildError, BuildHandle, FileResult, SearchError, SearchReport};

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
    CancelSearch,
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
    /// Name a new saved search.
    SaveSearch,
    RenameSaved {
        id: String,
    },
    ConfirmDeleteSaved {
        id: String,
        name: String,
    },
}

/// The kind of dialog for `AppState.dialog-kind`: 0 none, 1 editor,
/// 2 name field, 3 confirm.
pub fn dialog_kind(dialog: &Option<Dialog>) -> i32 {
    match dialog {
        None => 0,
        Some(Dialog::Editor { .. }) => 1,
        Some(Dialog::SaveSearch) | Some(Dialog::RenameSaved { .. }) => 2,
        Some(Dialog::ConfirmDelete { .. }) | Some(Dialog::ConfirmDeleteSaved { .. }) => 3,
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
}

/// The editable search form state. Bound to the UI properties; the
/// catalog's saved searches are loaded into it.
#[derive(Default)]
pub struct SearchForm {
    pub query: String,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub context_lines: usize,
    pub extensions_text: String,
    pub analyze_oversized: bool,
}

impl SearchForm {
    /// Engine options built from the current form state.
    pub fn options(&self) -> rsearch_engine::SearchOptions {
        let extensions = util::parse_extensions(&self.extensions_text);
        rsearch_engine::SearchOptions {
            case_sensitive: self.case_sensitive,
            whole_word: self.whole_word,
            context_lines: self.context_lines,
            extensions: if extensions.is_empty() {
                None
            } else {
                Some(extensions)
            },
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
    /// Selected project id — shared by the Search picker and the
    /// Projects list.
    pub selected: Option<String>,
    pub screen: Screen,
    /// Global application preferences (`preferences.json`).
    pub prefs: AppPreferences,
    pub dialog: Option<Dialog>,
    pub build: Option<ActiveBuild>,
    /// Banner notices, oldest first.
    pub notices: Vec<Notice>,
    pub search_form: SearchForm,
    /// Saved searches of the selected project — a display cache of the
    /// catalog.
    pub saved: Vec<SavedSearch>,
    /// Id of the saved search currently loaded into the form.
    pub loaded_saved: Option<String>,
    /// The flattened result list the Slint `ListView` displays through
    /// the shared [`ResultsModel`].
    pub results: Rc<ResultsModel>,
    /// The search currently running on its background thread.
    pub search_job: Option<SearchJob>,
    /// The internal file viewer, when open.
    pub viewer: Option<Viewer>,
    /// Line rows of the viewer overlay — installed once into the
    /// `viewer-lines` Slint property, filled when a load completes.
    pub viewer_lines: Rc<ViewerLines>,
    /// Loader thread of the pending view, dropped to abandon it.
    viewer_rx: Option<std::sync::mpsc::Receiver<viewer::ViewerOutcome>>,
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
            notices: Vec::new(),
            search_form: SearchForm::default(),
            saved: Vec::new(),
            loaded_saved: None,
            results: Rc::new(ResultsModel::default()),
            search_job: None,
            viewer: None,
            viewer_lines: ViewerLines::shared(),
            viewer_rx: None,
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
                        .loaded_saved
                        .as_deref()
                        .is_some_and(|l| saved.iter().all(|s| s.id != l))
                    {
                        self.loaded_saved = None;
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
                self.loaded_saved = None;
            }
        }
    }

    pub fn selected_project(&self) -> Option<&Project> {
        self.selected
            .as_deref()
            .and_then(|id| self.projects.iter().find(|p| p.id == id))
    }

    /// Index of the selected project in `projects` (for the UI), -1.
    pub fn selected_index(&self) -> i32 {
        self.selected
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

    pub fn select_project(&mut self, index: i32) {
        if let Some(p) = self.projects.get(index as usize) {
            let id = p.id.clone();
            if self.selected.as_deref() != Some(id.as_str()) {
                self.selected = Some(id);
                self.loaded_saved = None;
                self.refresh_saved();
            }
        }
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

    /// Starts a build or update for the selected project.
    pub fn start_build(&mut self) {
        let Some(catalog) = &self.catalog else {
            return;
        };
        let Some(project_id) = self.selected.clone() else {
            return;
        };
        let project = match catalog.get_project(&project_id) {
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
        true
    }

    // -- Search --------------------------------------------------------

    /// Whether the current form state can launch a search.
    pub fn can_search(&self) -> bool {
        if self.search_job.is_some() || !self.search_form.query_is_valid() {
            return false;
        }
        self.selected_project()
            .is_some_and(|p| p.index_db_path.exists())
    }

    /// Launches the form's query on a background thread against the
    /// selected project's index.
    pub fn run_search(&mut self) {
        if !self.can_search() {
            return;
        }
        let Some(project) = self.selected_project().cloned() else {
            return;
        };
        let options = self.search_form.options();
        let query = self.search_form.query.clone();
        // Results of the previous search are replaced by this job's —
        // partial results then belong unambiguously to it.
        self.results.clear();
        self.search_job = Some(SearchJob::start(&project, query, options));
    }

    pub fn cancel_search(&mut self) {
        if let Some(job) = &self.search_job {
            job.cancel();
        }
    }

    /// Collects search progress and results, tagging them with the
    /// project they ran on — the results area shows that provenance
    /// instead of silently attaching them to whatever is selected now.
    /// Returns `true` while a search job is in flight or when a
    /// message was processed this tick.
    fn poll_search(&mut self) -> bool {
        let Some(job) = &self.search_job else {
            return false;
        };
        let mut changed = false;
        for msg in job.poll() {
            changed = true;
            match msg {
                SearchMsg::Initial(report) => self.search_initial(report),
                SearchMsg::Progress { done, total, found } => {
                    self.search_progress(done, total, found)
                }
                SearchMsg::Done(result) => {
                    self.search_done(result);
                    break;
                }
            }
        }
        changed || self.search_job.is_some()
    }

    /// Phase-A report: all indexed candidates are verified — show them
    /// now. With the deep scan enabled the job keeps running and
    /// oversized files still pending stay counted in
    /// `candidates_too_large`.
    fn search_initial(&mut self, report: SearchReport) {
        let Some(job) = &self.search_job else {
            return;
        };
        let project_name = self
            .projects
            .iter()
            .find(|p| p.id == job.project_id)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| job.project_id.clone());
        let oversized_total = report.candidates_too_large;
        self.results.replace(ResultList::new(
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
        ));
    }

    /// One oversized file was verified during the deep scan; the model
    /// merges its result into the canonical order.
    fn search_progress(&mut self, done: usize, total: usize, found: Option<FileResult>) {
        self.results.insert_oversized(done, total, found);
    }

    /// Terminal message: complete — or stopped. Results that already
    /// arrived stay on screen but a cancelled search is labeled
    /// incomplete, never finished.
    fn search_done(&mut self, result: Result<SearchReport, SearchError>) {
        let Some(job) = self.search_job.take() else {
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
                self.results.replace(ResultList::new(
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
                self.results.finish(true);
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
                self.results.finish(false);
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

    /// Opens the viewer on the file/occurrence of a result row; the
    /// file itself is read and decoded on a worker thread — the
    /// overlay shows "loading" immediately and never blocks the UI.
    pub fn open_viewer(&mut self, file: usize, occ: usize) {
        let Some((path, entry, line, col)) = self.results.with(|l| {
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
            .results
            .with(|l| (l.query.clone(), l.case_sensitive, l.whole_word));
        let project_id = self.results.with(|l| l.project_id.clone());
        let fallback = self
            .projects
            .iter()
            .find(|p| p.id == project_id)
            .and_then(|p| p.settings.to_build_options().fallback_encoding);

        self.viewer = Some(Viewer {
            title: format!("{}:{}", path.display(), line),
            focus_line: line,
            loading: true,
            error: None,
            truncated: false,
            matches: Vec::new(),
            match_idx: 0,
        });
        self.viewer_lines.clear();
        self.viewer_rx = Some(viewer::start_load(
            path,
            query,
            case_sensitive,
            whole_word,
            fallback,
            line,
            col,
        ));
    }

    /// Closes the overlay; a pending load is abandoned (its sender
    /// dies with the receiver).
    pub fn close_viewer(&mut self) {
        self.viewer = None;
        self.viewer_rx = None;
        self.viewer_lines.clear();
    }

    /// Moves the focused match among the file's occurrences —
    /// `dir` is -1/+1 and wraps around both ends. Two hits sharing a
    /// line are two stops: only the green marker moves then.
    pub fn viewer_navigate(&mut self, dir: i32) {
        let Some(v) = &mut self.viewer else {
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
        self.viewer_lines.set_focus(old, new);
    }

    /// Picks up the loader thread's outcome once per load. Returns
    /// `true` only when the viewer state actually changed — the timer
    /// must not resync while a load is merely pending.
    fn poll_viewer(&mut self) -> bool {
        let Some(rx) = &self.viewer_rx else {
            return false;
        };
        let outcome = match rx.try_recv() {
            Ok(o) => o,
            Err(std::sync::mpsc::TryRecvError::Empty) => return false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.viewer_rx = None;
                let text = self.tr.viewer_error("loader stopped unexpectedly");
                if let Some(v) = &mut self.viewer {
                    v.loading = false;
                    v.error = Some(text);
                }
                return true;
            }
        };
        self.viewer_rx = None;
        match outcome {
            viewer::ViewerOutcome::Loaded(content) => {
                let focus = content.matches.get(content.match_idx).copied();
                self.viewer_lines.set_lines(content.lines, focus);
                self.viewer = Some(Viewer {
                    title: content.title,
                    focus_line: content.focus_line,
                    loading: false,
                    error: None,
                    truncated: content.truncated,
                    matches: content.matches,
                    match_idx: content.match_idx,
                });
            }
            viewer::ViewerOutcome::Failed(msg) => {
                let text = self.tr.viewer_error(&msg);
                if let Some(v) = &mut self.viewer {
                    v.loading = false;
                    v.error = Some(text);
                }
            }
        }
        true
    }

    // -- Saved searches ---------------------------------------------------

    /// Copies a saved search's query and options into the form.
    pub fn load_saved(&mut self, index: i32) {
        let Some(catalog) = &self.catalog else {
            return;
        };
        let Some(saved) = self.saved.get(index as usize) else {
            return;
        };
        let id = saved.id.clone();
        match catalog.get_saved_search(&id) {
            Ok(s) => {
                self.search_form.query = s.query;
                self.search_form.case_sensitive = s.params.case_sensitive;
                self.search_form.whole_word = s.params.whole_word;
                self.search_form.context_lines = s.params.context_lines;
                self.search_form.extensions_text = s
                    .params
                    .extensions
                    .map(|e| util::join_list(&e))
                    .unwrap_or_default();
                self.search_form.analyze_oversized = s.params.analyze_oversized;
                self.loaded_saved = Some(s.id);
                self.results.clear_selection();
            }
            Err(e) => self.push_notice(BannerLevel::Error, e.to_string(), true),
        }
    }

    /// Index of the loaded saved search in `saved`, -1 for none.
    pub fn saved_index(&self) -> i32 {
        self.loaded_saved
            .as_deref()
            .and_then(|id| self.saved.iter().position(|s| s.id == id))
            .map(|i| i as i32)
            .unwrap_or(-1)
    }

    /// Persists the form's query + options as a new saved search on
    /// the selected project.
    fn create_saved_search(&mut self, name: &str) {
        let Some(catalog) = &self.catalog else {
            return;
        };
        let Some(project_id) = self.selected.clone() else {
            return;
        };
        let params = SearchParams::from_engine(&self.search_form.options());
        match catalog.create_saved_search(&project_id, name, &self.search_form.query, params) {
            Ok(saved) => {
                let text = self.tr.saved_created(&saved.name);
                self.push_notice(BannerLevel::Success, text, false);
                self.loaded_saved = Some(saved.id);
                self.refresh_saved();
            }
            Err(e) => self.push_notice(BannerLevel::Error, e.to_string(), true),
        }
    }

    pub fn ask_save_search(&mut self) {
        if self.selected.is_some() && self.search_form.query_is_valid() {
            self.dialog = Some(Dialog::SaveSearch);
        }
    }

    pub fn ask_rename_saved(&mut self) {
        if let Some(id) = self.loaded_saved.clone() {
            if self.saved.iter().any(|s| s.id == id) {
                self.dialog = Some(Dialog::RenameSaved { id });
            }
        }
    }

    pub fn ask_delete_saved(&mut self) {
        if let Some(id) = self.loaded_saved.clone() {
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
        if let Some(p) = self.selected_project() {
            self.dialog = Some(Dialog::ConfirmDelete {
                id: p.id.clone(),
                name: p.name.clone(),
            });
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

    /// Applies the confirm button of whichever dialog is open. `name`
    /// is the current content of the name field (name dialogs only).
    pub fn dialog_confirm(&mut self, name: &str) {
        match self.dialog.take() {
            Some(Dialog::ConfirmDelete { id, name }) => self.delete_project(&id, &name),
            Some(Dialog::SaveSearch) => {
                let name = name.trim().to_owned();
                if name.is_empty() {
                    self.dialog = Some(Dialog::SaveSearch);
                } else {
                    self.create_saved_search(&name);
                }
            }
            Some(Dialog::RenameSaved { id }) => {
                let new_name = name.trim().to_owned();
                if new_name.is_empty() {
                    self.dialog = Some(Dialog::RenameSaved { id });
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
                            if self.loaded_saved.as_deref() == Some(id.as_str()) {
                                self.loaded_saved = None;
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

        if let Some(job) = &self.search_job {
            out.push(Banner {
                level: BannerLevel::Info,
                text: tr.banner_searching(&job.query),
                action: Some((tr.cancel.to_owned(), BannerAction::CancelSearch)),
                dismiss: None,
                working: true,
            });
        }

        if self.screen == Screen::Search && self.catalog.is_some() {
            match self.selected_project() {
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
            Some(BannerAction::CancelSearch) => self.cancel_search(),
            Some(BannerAction::NewProject) => return Some(self.new_project()),
            Some(BannerAction::OpenProjects) => self.screen = Screen::Projects,
            Some(BannerAction::StartBuild(id)) => {
                self.selected = Some(id);
                self.start_build();
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

    pub fn pref_exts_edited(&mut self, text: &str) {
        self.prefs.default_excluded_extensions = util::parse_extensions(text);
        self.save_prefs();
    }

    /// Parses the MiB buffer; the preference only changes when the
    /// text parses — invalid input keeps the previous value.
    pub fn pref_max_size_edited(&mut self, text: &str) {
        if let Ok(mb) = text.trim().parse::<u64>() {
            self.prefs.default_max_indexed_file_size = mb.max(1).saturating_mul(1024 * 1024);
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
