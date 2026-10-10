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
use crate::results_view::{FilterError, SortKey, ViewSpec};
use crate::shell_open::{self, ShellOpenError};
use crate::tr::{self, Strings};
use crate::util;
use crate::viewer::{self, ViewerLines};

use self::search_job::{ProjectOutcome, ProjectResult, SearchJob, SearchMsg};

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
    /// The multi-project picker of the active tab. `checked` is a
    /// draft aligned to `self.projects` order — the tab's selection
    /// only changes when the dialog is confirmed.
    PickProjects {
        checked: Vec<bool>,
        projects: Vec<(String, String)>,
    },
}

/// The kind of dialog for `AppState.dialog-kind`: 0 none, 1 editor,
/// 2 name field, 3 confirm, 4 build confirmation, 5 project picker.
pub fn dialog_kind(dialog: &Option<Dialog>) -> i32 {
    match dialog {
        None => 0,
        Some(Dialog::Editor { .. }) => 1,
        Some(Dialog::SaveSearch) | Some(Dialog::RenameTab { .. }) => 2,
        Some(Dialog::ConfirmDelete { .. }) | Some(Dialog::ConfirmDeleteSaved { .. }) => 3,
        Some(Dialog::ConfirmBuild { .. }) => 4,
        Some(Dialog::PickProjects { .. }) => 5,
    }
}

/// Internal file-viewer state — `None` while the overlay is closed.
/// The line rows themselves live in [`ViewerLines`], shared with the
/// Slint model, so a finished load paints without a full resync.
pub struct Viewer {
    /// Header text: the file path and the focused line.
    pub title: String,
    /// The physical file currently displayed — the target of the
    /// open-with-association action. Regular files only: archive
    /// entries never open the viewer (`open_viewer` refuses them),
    /// so this is always a real filesystem path, never a `zip!entry`
    /// logical path.
    pub path: std::path::PathBuf,
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
#[derive(Clone, Default)]
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
    /// The projects this tab searches, in selection order — no
    /// duplicates, resolved against the catalog at launch time. Each
    /// tab keeps its own selection, independent of the Projects
    /// screen's; empty means nothing is selected (search disabled).
    pub project_ids: Vec<String>,
    /// `true` once the selection was explicitly set — by the picker,
    /// a loaded saved search, or inheritance from another tab. The
    /// startup seeding in [`App::refresh`] only fills a tab that
    /// never picked: an intentionally empty selection is a valid
    /// state a refresh must not undo.
    pub projects_picked: bool,
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
    /// How this tab displays its results (filter + sort) — per-tab
    /// state, persisted across searches in the same tab, default in
    /// a fresh tab.
    pub view: ViewSpec,
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
            view: ViewSpec::default(),
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
    ///
    /// `project_ids` is the selection resolved by the caller: the
    /// saved `project_ids` (or the owning project's fallback) already
    /// filtered to projects that still exist.
    fn fill_saved(&mut self, saved: &SavedSearch, project_ids: Vec<String>) {
        self.form.project_ids = project_ids;
        self.form.projects_picked = true;
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
    /// building selection. Search tabs keep their own selection
    /// (`SearchForm::project_ids`); the two never sync.
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
    /// Latest results-list fit (viewport px, char px) reported by the
    /// UI, applied on the next tick — never during the layout pass
    /// that reported it.
    pending_line_fit: Option<(f32, f32)>,
    /// Toggled every time the results row heights re-wrap (viewport
    /// width change): the UI swaps between two ListView instances so
    /// the displayed one is always freshly created — Slint keeps a
    /// hidden scroll anchor across model resets, and a stale anchor
    /// measured on the old heights pushes the rows out of the
    /// viewport.
    pub results_redraw: bool,
    /// Id of the project whose detail panel was last pushed to the
    /// UI — the edge [`App::project_panel_changed`] detects. `None`
    /// before the first sync or while nothing is selected.
    panel_shown: Option<String>,
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
            pending_line_fit: None,
            results_redraw: false,
            panel_shown: None,
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
                // Tabs that never picked a project start on the last
                // project used for a search — the saved-searches combo
                // then picks up where the user left off — falling back
                // to the healed selection. A tab cannot reference a
                // deleted project: deletion is refused while any tab
                // selects it. `projects_picked` guards the seeding: an
                // intentionally empty selection stays empty.
                let remembered = self
                    .prefs
                    .last_search_project_id
                    .as_deref()
                    .filter(|id| self.projects.iter().any(|p| p.id == *id))
                    .map(str::to_owned);
                for tab in &mut self.tabs {
                    if tab.form.project_ids.is_empty() && !tab.form.projects_picked {
                        if let Some(id) = remembered.clone().or_else(|| self.selected.clone()) {
                            tab.form.project_ids = vec![id];
                        }
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

    /// Reloads the saved-searches cache — every project's searches,
    /// in catalog order. The list belongs to no selection: any tab
    /// can load any saved search, so the cache is global.
    fn refresh_saved(&mut self) {
        let Some(catalog) = &self.catalog else {
            return;
        };
        match catalog.list_all_saved_searches() {
            Ok(saved) => {
                // Forget associations whose entry vanished — the list
                // is global, so the check covers every tab at once.
                for tab in &mut self.tabs {
                    if tab
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
        }
    }

    pub fn selected_project(&self) -> Option<&Project> {
        self.selected
            .as_deref()
            .and_then(|id| self.projects.iter().find(|p| p.id == id))
    }

    /// The active tab's selected projects, resolved against the
    /// catalog in selection order — an id that vanished since it was
    /// picked is skipped (defensive: deletion is refused while any
    /// tab selects the project, and [`App::run_search`] prunes).
    pub fn search_projects(&self) -> Vec<&Project> {
        let mut seen = std::collections::HashSet::new();
        self.tab()
            .form
            .project_ids
            .iter()
            .filter(|id| seen.insert(id.as_str()))
            .filter_map(|id| self.projects.iter().find(|p| p.id == *id))
            .collect()
    }

    /// Label of the search-screen project picker button: the project
    /// name for a single selection, a translated count for several,
    /// a translated hint for none — never a list of names.
    pub fn search_picker_label(&self) -> String {
        match self.search_projects().as_slice() {
            [] => self.tr.pick_no_projects.to_owned(),
            [p] => p.name.clone(),
            ps => self.tr.picker_projects(ps.len()),
        }
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

    /// Whether the detail panel now shows a different project than
    /// the last one pushed — the "initial display" edge on which the
    /// Slint side reopens the panel's collapsible sections. Ordinary
    /// resyncs return `false`, so manual folds survive every refresh.
    pub fn project_panel_changed(&mut self) -> bool {
        if self.panel_shown == self.selected {
            return false;
        }
        self.panel_shown = self.selected.clone();
        true
    }

    /// Opens the multi-project picker of the active tab: one checkbox
    /// per existing project, pre-checked from the tab's selection.
    /// The dialog edits a draft — the tab's selection only changes
    /// on confirm ([`App::apply_picked_projects`]).
    pub fn ask_pick_projects(&mut self) {
        let selected = &self.tab().form.project_ids;
        let checked: Vec<bool> = self
            .projects
            .iter()
            .map(|p| selected.contains(&p.id))
            .collect();
        let projects = self
            .projects
            .iter()
            .map(|p| (p.id.clone(), p.name.clone()))
            .collect();
        self.dialog = Some(Dialog::PickProjects { checked, projects });
    }

    /// One checkbox of the picker dialog — draft state only, nothing
    /// is applied yet.
    pub fn pick_toggle(&mut self, index: usize, checked: bool) {
        if let Some(Dialog::PickProjects { checked: draft, .. }) = &mut self.dialog {
            if let Some(slot) = draft.get_mut(index) {
                *slot = checked;
            }
        }
    }

    /// The picker's Select all / Select none action — draft state
    /// only.
    pub fn pick_set_all(&mut self, checked: bool) {
        if let Some(Dialog::PickProjects { checked: draft, .. }) = &mut self.dialog {
            draft.iter_mut().for_each(|slot| *slot = checked);
        }
    }

    /// The picker rows to display — every project's name in catalog
    /// order with the draft's checkbox state.
    pub fn pick_rows(&self) -> Vec<(String, bool)> {
        let Some(Dialog::PickProjects { checked, projects }) = &self.dialog else {
            return Vec::new();
        };
        projects
            .iter()
            .zip(checked.iter().copied())
            .map(|((_, name), c)| (name.clone(), c))
            .collect()
    }

    /// Applies the picker's draft to the active tab. The catalog's
    /// project order is the selection's deterministic order;
    /// duplicates cannot exist (one checkbox per project) and ids are
    /// by construction all valid.
    ///
    /// An unchanged selection keeps the tab's saved-search
    /// association; a changed one dissociates the tab — editing the
    /// parameters must never silently modify the source entry.
    /// The first selected id becomes the remembered main project.
    fn apply_picked_projects(&mut self, checked: Vec<bool>, projects: Vec<(String, String)>) {
        let mut missing = 0;
        let ids: Vec<String> = projects
            .into_iter()
            .zip(checked)
            .filter(|(_, checked)| *checked)
            .filter_map(|((id, _), _)| {
                if self.projects.iter().any(|p| p.id == id) {
                    Some(id)
                } else {
                    missing += 1;
                    None
                }
            })
            .collect();
        if missing > 0 {
            self.push_notice(
                BannerLevel::Warning,
                self.tr.missing_projects(missing),
                false,
            );
        }
        let tab = self.tab_mut();
        if tab.form.project_ids != ids {
            tab.form.project_ids = ids;
            tab.loaded_saved_id = None;
            // The displayed results were produced by the previous
            // selection — they no longer describe what a search
            // would now run against.
            tab.results.clear();
        }
        tab.form.projects_picked = true;
        if let Some(first) = self.tab().form.project_ids.first().cloned() {
            self.remember_search_project(&first);
        }
    }

    /// Records the project just picked for a search as the startup
    /// default, so the next session's saved-searches combo opens on
    /// it. Best-effort: a failed write only means the next session
    /// starts elsewhere.
    fn remember_search_project(&mut self, project_id: &str) {
        if self.prefs.last_search_project_id.as_deref() == Some(project_id) {
            return;
        }
        self.prefs.last_search_project_id = Some(project_id.to_owned());
        if let Some(catalog) = &self.catalog {
            let _ = catalog.save_preferences(&self.prefs);
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
        // A fresh run supersedes the stored report of this project's
        // previous run — it must not stay on screen while the new
        // build progresses. Another project's report is untouched.
        if self
            .last_report
            .as_ref()
            .is_some_and(|(id, _)| *id == project_id)
        {
            self.last_report = None;
        }
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
    /// inherits the complete project selection of the tab it leaves
    /// — a snapshot, never a live link. A parent that never picked
    /// (or whose seeded selection is still empty) starts the child on
    /// the Projects screen's selection instead.
    pub fn new_tab(&mut self) {
        let id = self.alloc_tab_id();
        let parent_ids = self.tab().form.project_ids.clone();
        let picked = self.tab().form.projects_picked;
        let mut tab = SearchTab::new(id);
        tab.form.project_ids = if parent_ids.is_empty() && !picked {
            self.selected.iter().cloned().collect()
        } else {
            parent_ids
        };
        tab.form.projects_picked = picked;
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
            tab.form.project_ids = self.selected.iter().cloned().collect();
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

    /// Whether the active tab's form state can launch a search: a
    /// valid query and at least one selected project whose index file
    /// exists. Selected projects without an index still take part —
    /// their search reports a named failure without stopping the
    /// others.
    pub fn can_search(&self) -> bool {
        if self.tab().job.is_some() || !self.tab().form.query_is_valid() {
            return false;
        }
        self.search_projects()
            .iter()
            .any(|p| p.index_db_path.exists())
    }

    /// Launches the active tab's query on a background thread against
    /// every selected project's index, in selection order.
    ///
    /// Selection ids that vanished since they were picked are pruned
    /// first — never silently, never a panic — and the search still
    /// runs on the projects that remain valid.
    pub fn run_search(&mut self) {
        if self.tab().job.is_some() || !self.tab().form.query_is_valid() {
            return;
        }
        let Some(catalog) = &self.catalog else {
            return;
        };
        match catalog.list_projects() {
            Ok(projects) => self.projects = projects,
            Err(e) => {
                self.push_notice(BannerLevel::Error, e.to_string(), true);
                return;
            }
        }
        // Normalize the selection: deduped, existing projects only.
        let ids = self.tab().form.project_ids.clone();
        let mut valid: Vec<String> = Vec::with_capacity(ids.len());
        let mut pruned = 0usize;
        for id in ids {
            if self.projects.iter().any(|p| p.id == id) {
                if !valid.contains(&id) {
                    valid.push(id);
                }
            } else {
                pruned += 1;
            }
        }
        if valid != self.tab().form.project_ids {
            let tab = self.tab_mut();
            tab.form.project_ids = valid;
            tab.form.projects_picked = true;
            tab.loaded_saved_id = None;
            // The selection changed — results of a previous run no
            // longer describe what a search would run against.
            tab.results.clear();
            if let Some(first) = self.tab().form.project_ids.first().cloned() {
                self.remember_search_project(&first);
            }
        }
        if pruned > 0 {
            let text = self.tr.missing_projects(pruned);
            self.push_notice(BannerLevel::Warning, text, false);
        }
        if !self.can_search() {
            return;
        }
        let projects: Vec<Project> = self.search_projects().into_iter().cloned().collect();
        let tab = self.tab_mut();
        let options = tab.form.options();
        let query = tab.form.query.clone();
        // Results of the previous search are replaced by this job's —
        // partial results then belong unambiguously to it.
        tab.results.clear();
        tab.job = SearchJob::start(&projects, tab.id, query, options);
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
            let disconnected = job.disconnected();
            for msg in msgs {
                changed = true;
                match msg {
                    // A target's search is starting — track it so the
                    // searching banner can name the project and its
                    // position in the selection.
                    SearchMsg::Started { target } => {
                        if let Some(job) = self.tabs[i].job.as_mut() {
                            debug_assert!(
                                target < job.targets.len(),
                                "started target out of range"
                            );
                            if target < job.targets.len() {
                                job.current_target = target;
                                job.oversized_progress = None;
                            }
                        }
                    }
                    SearchMsg::Initial { target, report } => self.search_initial(i, target, report),
                    SearchMsg::Progress {
                        target,
                        done,
                        total,
                        found,
                    } => self.search_progress(i, target, done, total, found),
                    SearchMsg::Done(results) => {
                        self.search_done(i, results);
                        break;
                    }
                }
            }
            if disconnected && self.tabs[i].job.take().is_some() {
                self.tabs[i].results.finish(false);
                self.push_notice(
                    BannerLevel::Error,
                    self.tr.search_worker_failed.to_owned(),
                    true,
                );
                changed = true;
            }
            running |= self.tabs[i].job.is_some();
        }
        changed || running
    }

    /// Phase-A report of one target: all its indexed candidates are
    /// verified — show them now. With the deep scan enabled the job
    /// keeps running and oversized files still pending stay counted
    /// in `candidates_too_large`.
    ///
    /// The first report to arrive creates the list; every later
    /// target's report is folded into the in-flight list by
    /// [`ResultList::merge_report`] — physical duplicates are dropped
    /// as they arrive, the first project in selection order wins.
    fn search_initial(&mut self, tab_index: usize, target: usize, report: SearchReport) {
        let Some(job) = self.tabs[tab_index].job.as_mut() else {
            return;
        };
        let Some(initial) = job.initial_counters.get_mut(target) else {
            return;
        };
        *initial = Some(SearchReport {
            results: Vec::new(),
            candidates_from_index: report.candidates_from_index,
            candidates_too_large: report.candidates_too_large,
            skipped_stale: report.skipped_stale,
            skipped_index_errors: report.skipped_index_errors,
            skipped_security_limits: report.skipped_security_limits,
            verification_errors: report.verification_errors,
            truncated_files: report.truncated_files,
            archives_opened: report.archives_opened,
            elapsed: report.elapsed,
        });
        if target == job.current_target {
            job.oversized_progress = Some((0, report.candidates_too_large));
        }
        if self
            .tabs
            .get(tab_index)
            .is_some_and(|t| t.results.with(|l| l.present && l.in_flight))
        {
            self.tabs[tab_index].results.merge_report(target, report);
            return;
        }
        let Some(job) = &self.tabs[tab_index].job else {
            return;
        };
        let Some(target_ref) = job.targets.get(target) else {
            return;
        };
        let project_name = self
            .projects
            .iter()
            .find(|p| p.id == target_ref.project_id)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| target_ref.project_name.clone());
        let oversized_total = report.candidates_too_large;
        let project_ids: Vec<String> = job.targets.iter().map(|t| t.project_id.clone()).collect();
        let view = self.tabs[tab_index].view.clone();
        let merged = crate::results::merge_reports([(target, report)], project_ids);
        let list = ResultList::new(
            merged,
            crate::results::ResultContext {
                project_id: target_ref.project_id.clone(),
                project_name,
                query: job.query.clone(),
                case_sensitive: job.case_sensitive,
                whole_word: job.whole_word,
            },
            job.analyze_oversized,
            0,
            oversized_total,
            true,
            view,
        );
        self.tabs[tab_index].results.replace(list);
    }

    /// One oversized file was verified during the deep scan; the
    /// owning tab's model merges its result into the canonical order.
    /// `target` is the producing project — it routes the per-project
    /// `done` counter and the file's provenance; a physical file
    /// another project already reported is not inserted again.
    fn search_progress(
        &mut self,
        tab_index: usize,
        target: usize,
        done: usize,
        total: usize,
        found: Option<FileResult>,
    ) {
        let Some(job) = self.tabs[tab_index].job.as_mut() else {
            return;
        };
        if target >= job.targets.len() {
            return;
        }
        if target == job.current_target {
            job.oversized_progress = Some((done, total));
        }
        self.tabs[tab_index]
            .results
            .insert_oversized(target, done, total, found);
    }

    /// Terminal message: complete — or stopped.
    ///
    /// When every selected project completed, all reports go through
    /// [`crate::results::merge_reports`] — the single fusion point —
    /// and the display is replaced by the final reports, which carry
    /// complete counters. A partial outcome (a failure, a
    /// cancellation, an unattempted project) keeps the in-flight
    /// merged list instead: it already holds every result verified so
    /// far, and a failure never erases results. Failures surface as
    /// sticky banners naming their project; a cancellation labels the
    /// list incomplete, never finished.
    fn search_done(&mut self, tab_index: usize, results: Vec<ProjectResult>) {
        let Some(job) = self.tabs[tab_index].job.take() else {
            return;
        };
        if results.is_empty() {
            // An empty selection searches nothing — never a silent
            // empty report.
            self.tabs[tab_index].results.finish(false);
            let e = SearchError::Internal("search ran without any project".to_string());
            let text = self.tr.search_failed(&e.to_string());
            self.push_notice(BannerLevel::Error, text, true);
            return;
        }
        let succeeded = results
            .iter()
            .filter(|r| matches!(r.outcome, ProjectOutcome::Success(_)))
            .count();
        if succeeded < results.len() {
            for (target, result) in results.iter().enumerate() {
                if let ProjectOutcome::Success(report) = &result.outcome {
                    if let Some(initial) = job.initial_counters.get(target).and_then(Option::as_ref)
                    {
                        self.tabs[tab_index]
                            .results
                            .finalize_counters(initial, report);
                    }
                }
            }
            let interrupted = results.iter().any(|r| {
                matches!(
                    r.outcome,
                    ProjectOutcome::Cancelled | ProjectOutcome::NotAttempted
                )
            });
            self.tabs[tab_index].results.finish(interrupted);
            if results.len() == 1 {
                // Mono-project failure keeps its exact wording.
                if let ProjectOutcome::Failed(e) = &results[0].outcome {
                    let text = self.tr.search_failed(&e.to_string());
                    self.push_notice(BannerLevel::Error, text, true);
                }
                if interrupted {
                    self.push_notice(
                        BannerLevel::Info,
                        self.tr.search_cancelled.to_owned(),
                        false,
                    );
                }
                return;
            }
            // Multi-project partial outcome: ONE synthesized banner —
            // the summary (unique files kept, projects completed),
            // then one detail line per failing project. The in-flight
            // merged list already holds every result verified so far;
            // failures and cancelled targets never erase it.
            let failed = results
                .iter()
                .filter(|r| matches!(r.outcome, ProjectOutcome::Failed(_)))
                .count();
            let (files, matches) = self.tabs[tab_index].results.with(|l| {
                (
                    l.report.results.len(),
                    l.report
                        .results
                        .iter()
                        .map(|r| r.occurrences.len())
                        .sum::<usize>(),
                )
            });
            let mut text = self
                .tr
                .search_partial(succeeded, results.len(), matches, files);
            let cancelled = results
                .iter()
                .filter(|r| matches!(r.outcome, ProjectOutcome::Cancelled))
                .count();
            let pending = results
                .iter()
                .filter(|r| matches!(r.outcome, ProjectOutcome::NotAttempted))
                .count();
            text.push('\n');
            text.push_str(&self.tr.search_outcomes(failed, cancelled, pending));
            for r in results.iter() {
                if let ProjectOutcome::Failed(e) = &r.outcome {
                    text.push_str(&format!("\n{}: {e}", r.target.project_name));
                }
            }
            if interrupted {
                text.push('\n');
                text.push_str(self.tr.search_cancelled);
            }
            let (level, sticky) = if failed > 0 {
                (BannerLevel::Error, true)
            } else {
                (BannerLevel::Info, false)
            };
            self.push_notice(level, text, sticky);
            return;
        }
        // The display context is the first selected project's — its
        // id and name label the list, as a mono-project search
        // always did.
        let target_count = results.len();
        // The display context is the first selected project's — its
        // id and name label the list, as a mono-project search
        // always did.
        let context_target = results[0].target.clone();
        let project_name = self
            .projects
            .iter()
            .find(|p| p.id == context_target.project_id)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| context_target.project_name.clone());
        let project_ids: Vec<String> = job.targets.iter().map(|t| t.project_id.clone()).collect();
        let merged = crate::results::merge_reports(
            results
                .into_iter()
                .enumerate()
                .filter_map(|(i, r)| match r.outcome {
                    ProjectOutcome::Success(report) => Some((i, report)),
                    _ => None,
                }),
            project_ids,
        );
        let matches: usize = merged
            .report
            .results
            .iter()
            .map(|r| r.occurrences.len())
            .sum();
        let text = if target_count > 1 {
            self.tr.search_done_multi(
                matches,
                merged.report.results.len(),
                target_count,
                merged.report.elapsed,
            )
        } else if matches == 0 {
            self.tr.no_results_hint.to_owned()
        } else {
            self.tr
                .search_done(matches, merged.report.results.len(), merged.report.elapsed)
        };
        let level = if matches == 0 {
            BannerLevel::Info
        } else {
            BannerLevel::Success
        };
        let oversized_total = merged.report.candidates_too_large;
        let view = self.tabs[tab_index].view.clone();
        self.tabs[tab_index].results.complete(ResultList::new(
            merged,
            crate::results::ResultContext {
                project_id: context_target.project_id,
                project_name,
                query: job.query,
                case_sensitive: job.case_sensitive,
                whole_word: job.whole_word,
            },
            job.analyze_oversized,
            oversized_total,
            oversized_total,
            false,
            view,
        ));
        self.push_notice(level, text, false);
    }

    /// Periodic work driven by the UI timer: collect engine progress,
    /// expire notices. Returns `true` when something changed — an idle
    /// tick must not resync the UI, or every model push recreates the
    /// list delegates and eats mid-gesture clicks.
    pub fn tick(&mut self) -> bool {
        let fit = self.pending_line_fit.take();
        let refit = if let Some((avail, char_px)) = fit {
            let mut moved = false;
            for tab in &self.tabs {
                moved |= tab.results.set_line_fit(avail, char_px);
            }
            if moved {
                // Rows re-wrapped — their heights changed under the
                // ListView's cached anchor. Recreate the list.
                self.results_redraw = !self.results_redraw;
            }
            true
        } else {
            false
        };
        refit | self.poll_build() | self.poll_search() | self.poll_viewer() | self.expire_notices()
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
        // The retained result's producing project supplies the
        // fallback encoding — for a merged list each file remembers
        // which project's index reported it first.
        let project_id = self
            .tab()
            .results
            .with(|l| l.project_id_of(file).to_owned());
        let fallback = self
            .projects
            .iter()
            .find(|p| p.id == project_id)
            .and_then(|p| p.settings.to_build_options().fallback_encoding);

        let tr = self.tr;
        let tab = self.tab_mut();
        tab.viewer = Some(Viewer {
            title: format!("{}:{}", path.display(), line),
            path: path.clone(),
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

    /// Applies the results view built from the UI's filter/sort/desc
    /// controls to the active tab. On an invalid mask the previous
    /// view stays displayed and the error is returned; the tab's view
    /// is committed only on success.
    pub fn apply_results_view(
        &mut self,
        filter: &str,
        sort: SortKey,
        desc: bool,
    ) -> Result<(), FilterError> {
        let view = ViewSpec {
            filter: filter.to_string(),
            sort,
            desc,
        };
        self.tab().results.set_view(view.clone())?;
        self.tab_mut().view = view;
        Ok(())
    }

    /// Opens the file currently displayed in the viewer with its
    /// Windows file association — Windows resolves the application,
    /// rsearch never maps extensions itself. The viewer only ever
    /// holds regular files (archive entries never open it), so the
    /// stored path is the physical one; a file deleted since the
    /// search is reported, never recreated.
    pub fn open_viewer_file_with_app(&mut self) {
        let Some(path) = self.tab().viewer.as_ref().map(|v| v.path.clone()) else {
            return;
        };
        if !path.is_file() {
            self.push_notice(
                BannerLevel::Error,
                self.tr.viewer_file_missing.to_owned(),
                true,
            );
            return;
        }
        if let Err(e) = shell_open::open_with_association(&path) {
            let text = match e {
                ShellOpenError::NoAssociation => self.tr.viewer_no_associated_app.to_owned(),
                ShellOpenError::NotFound => self.tr.viewer_file_missing.to_owned(),
                ShellOpenError::Failed(code) => self.tr.viewer_open_failed(&code.to_string()),
            };
            self.push_notice(BannerLevel::Error, text, true);
        }
    }

    /// The word a double-click targeted in the active tab's viewer —
    /// `None` when the click landed on nothing word-like (separator,
    /// whitespace, a hit segment of the current query, empty area).
    pub fn viewer_word_at(
        &self,
        line: usize,
        seg: usize,
        x_px: f32,
        width_px: f32,
    ) -> Option<String> {
        self.tab().viewer_lines.word_at(line, seg, x_px, width_px)
    }

    /// The results list reports its viewport width and its measured
    /// monospace advance. Stored only — applying it rebuilds the
    /// rows, which must never happen inside the layout pass that
    /// reported the width; the next tick applies it. A non-positive
    /// report is a transient (a list instance mid-teardown reports a
    /// degenerate viewport), never a real wrap width — dropped.
    pub fn set_results_width(&mut self, avail_px: f32, char_px: f32) {
        if avail_px <= 0.0 || char_px <= 0.0 {
            return;
        }
        self.pending_line_fit = Some((avail_px, char_px));
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
                        path: content.path,
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
                // The stored selection: `project_ids` when present,
                // the owning project's id for documents written
                // before multi-project searches. Ids of deleted
                // projects are dropped — kept order, never replaced
                // by an arbitrary project.
                let ids = s.params.selected_project_ids(&s.project_id);
                let mut valid: Vec<String> = Vec::with_capacity(ids.len());
                let mut missing = 0usize;
                for pid in ids {
                    if self.projects.iter().any(|p| p.id == pid) {
                        if !valid.contains(&pid) {
                            valid.push(pid);
                        }
                    } else {
                        missing += 1;
                    }
                }
                if let Some(i) = self
                    .tabs
                    .iter()
                    .position(|t| t.loaded_saved_id.as_deref() == Some(id.as_str()))
                {
                    self.activate_tab(i as i32);
                } else {
                    self.new_tab();
                    self.tab_mut().fill_saved(&s, valid.clone());
                }
                if missing > 0 {
                    let text = self.tr.missing_projects(missing);
                    self.push_notice(BannerLevel::Warning, text, false);
                }
                if valid.is_empty() {
                    let text = self.tr.saved_needs_projects.to_owned();
                    self.push_notice(BannerLevel::Warning, text, true);
                }
                if let Some(first) = valid.first().cloned() {
                    self.remember_search_project(&first);
                }
                self.selected_saved = Some(id);
            }
            Err(e) => self.push_notice(BannerLevel::Error, e.to_string(), true),
        }
    }

    /// Opens the save dialog: the name field starts on the associated
    /// entry's name (so Enregistrer updates it by default) or on the
    /// tab title for an unassociated tab. A search needs at least one
    /// selected project to be persisted.
    pub fn ask_save_search(&mut self) {
        if self.search_projects().is_empty() || !self.tab().form.query_is_valid() {
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
        let project_ids = self.tab().form.project_ids.clone();
        let Some(owner) = project_ids.first().cloned() else {
            return;
        };
        if !self.tab().form.query_is_valid() {
            return;
        }
        let query = self.tab().form.query.clone();
        let mut params = SearchParams::from_engine(&self.tab().form.options());
        // The persisted selection is the whole ordered list; the SQL
        // `project_id` column stays the owner — the first selected.
        params.project_ids = project_ids;
        let outcome = match self.tab().loaded_saved_id.as_deref() {
            Some(id) => match catalog.update_saved_search(id, &name, &query, params.clone()) {
                Ok(()) => Ok((id.to_owned(), false)),
                // The entry was deleted while associated — insert a
                // fresh one rather than failing.
                Err(CatalogError::NotFound(_)) => catalog
                    .create_saved_search(&owner, &name, &query, params)
                    .map(|s| (s.id, true)),
                Err(e) => Err(e),
            },
            None => catalog
                .create_saved_search(&owner, &name, &query, params)
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
        let project_ids = self.tab().form.project_ids.clone();
        let Some(owner) = project_ids.first().cloned() else {
            return;
        };
        if !self.tab().form.query_is_valid() {
            return;
        }
        let query = self.tab().form.query.clone();
        let mut params = SearchParams::from_engine(&self.tab().form.options());
        params.project_ids = project_ids;
        let outcome = catalog
            .create_saved_search(&owner, &name, &query, params)
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
        // A search tab still selects this project — deleting it
        // would orphan part of the tab's search context. Refuse until
        // the user closes (or re-points) every tab selecting it.
        if self.tabs.iter().any(|t| t.form.project_ids.contains(&id)) {
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
            Some(Dialog::PickProjects { checked, projects }) => {
                self.apply_picked_projects(checked, projects)
            }
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
                let mut text = tr.banner_searching(&job.query);
                if job.targets.len() > 1 {
                    // The running target: name + position in the
                    // selection (updated on each Started message).
                    let i = job.current_target.min(job.targets.len() - 1);
                    let name = &job.targets[i].project_name;
                    text.push_str(" · ");
                    text.push_str(&tr.searching_project(i + 1, job.targets.len(), name));
                }
                out.push(Banner {
                    level: BannerLevel::Info,
                    text,
                    action: Some((tr.cancel.to_owned(), BannerAction::CancelSearch(tab.id))),
                    dismiss: None,
                    working: true,
                });
            }
        }

        if self.screen == Screen::Search && self.catalog.is_some() {
            let targets = self.search_projects();
            if targets.is_empty() {
                out.push(Banner {
                    level: BannerLevel::Info,
                    text: tr.banner_no_project.to_owned(),
                    action: Some(if self.projects.is_empty() {
                        (tr.new_project.to_owned(), BannerAction::NewProject)
                    } else {
                        (tr.open_projects.to_owned(), BannerAction::OpenProjects)
                    }),
                    dismiss: None,
                    working: false,
                });
            } else {
                let building =
                    |p: &Project| self.build.as_ref().is_some_and(|b| b.project_id == p.id);
                // The hints describe the first selected project that
                // needs attention — a missing index or stale settings.
                // Other selected projects still run; a missing index
                // also surfaces as a per-target failure in the final
                // tally.
                if let Some(p) = targets
                    .iter()
                    .find(|p| !building(p) && !p.index_db_path.exists())
                {
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
                } else if let Some(p) = targets.iter().find(|p| {
                    !building(p)
                        && p.last_build_settings.is_some()
                        && self.catalog.as_ref().is_some_and(|c| c.needs_rebuild(p))
                }) {
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
            pending_line_fit: None,
            results_redraw: false,
            panel_shown: None,
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
        a.tabs[0].form.project_ids = a.selected.iter().cloned().collect();
        a.tabs[0].form.projects_picked = true;
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
            size: 0,
            mtime: None,
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
            crate::results::MergedSearch::single(report(files, 0), "p"),
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
            ViewSpec::default(),
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

    /// A `Done` message for the fake job's single test target.
    fn done(outcome: ProjectOutcome) -> SearchMsg {
        SearchMsg::Done(vec![ProjectResult {
            target: search_job::test_target(),
            outcome,
        }])
    }

    /// A named test target for a multi-project fake job.
    fn target(id: &str, name: &str) -> search_job::SearchTarget {
        search_job::SearchTarget {
            project_id: id.into(),
            project_name: name.into(),
            index_db_path: PathBuf::from(format!("{id}.db")),
        }
    }

    /// A fake in-flight job over two targets, "pa" (A) then "pb" (B).
    fn fake_job_two(app: &mut App, i: usize) -> mpsc::Sender<SearchMsg> {
        let (tx, rx) = mpsc::channel();
        let id = app.tabs[i].id;
        app.tabs[i].job = Some(SearchJob::for_test_targets(
            id,
            vec![target("pa", "A"), target("pb", "B")],
            Arc::new(AtomicBool::new(false)),
            rx,
        ));
        tx
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
    fn new_tab_starts_with_the_default_view() {
        let mut a = app();
        assert_eq!(a.tab().view, ViewSpec::default());
        a.new_tab();
        assert_eq!(a.tab().view, ViewSpec::default());
    }

    #[test]
    fn apply_results_view_filters_and_commits_to_the_tab() {
        let mut a = app();
        a.tab_mut().results.replace(crate::results::ResultList::new(
            crate::results::MergedSearch::single(
                report(vec![file("f0.txt", &[1]), file("f1.asp", &[1])], 0),
                "p",
            ),
            ResultContext {
                project_id: "p".into(),
                project_name: "proj".into(),
                query: "q".into(),
                case_sensitive: false,
                whole_word: false,
            },
            false,
            0,
            0,
            false,
            ViewSpec::default(),
        ));
        a.apply_results_view("*.asp", SortKey::Name, false)
            .expect("valid mask");
        assert_eq!(a.tab().view.filter, "*.asp");
        assert_eq!(a.tab().view.sort, SortKey::Name);
        assert!(!a.tab().view.desc);
        // Only the matching file is displayed: header + 1 occurrence.
        use slint::Model as _;
        assert_eq!(a.tab().results.row_count(), 2);
    }

    #[test]
    fn apply_results_view_rejects_an_invalid_mask_and_keeps_the_previous() {
        let mut a = app();
        a.apply_results_view("*.asp", SortKey::Path, false)
            .expect("valid mask");
        assert!(a
            .apply_results_view("dir\\*.asp", SortKey::Path, false)
            .is_err());
        assert_eq!(a.tab().view.filter, "*.asp", "the previous view stands");
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
                &a.tab().form.project_ids.first().cloned().unwrap(),
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
        a.tab_mut().form.project_ids = vec![other.id.clone()];
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
        a.tab_mut().form.project_ids = Vec::new();
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
        let tab_project = a.tab().form.project_ids.clone();
        // Selecting in the Projects screen list never touches the tab.
        let idx = a.projects.iter().position(|p| p.id == other.id).unwrap() as i32;
        a.select_project(idx);
        assert_eq!(a.selected.as_deref(), Some(other.id.as_str()));
        assert_eq!(a.tab().form.project_ids, tab_project);
        // And the other way: the tab's picker leaves the Projects
        // screen's selection alone.
        a.ask_pick_projects();
        a.pick_set_all(false);
        a.pick_toggle(0, true);
        a.dialog_confirm("");
        assert_eq!(a.selected.as_deref(), Some(other.id.as_str()));
    }

    // -- Detail-panel section defaults --------------------------------------

    #[test]
    fn project_panel_changed_fires_once_per_displayed_project() {
        let (mut a, _tmp) = app_with_project();
        // The first push of a project's detail fires the edge once —
        // the UI reopens the three sections there.
        assert!(a.project_panel_changed());
        // Ordinary resyncs never re-fire while the same project stays
        // displayed — manual folds survive every refresh.
        assert!(!a.project_panel_changed());
        assert!(!a.project_panel_changed());
    }

    #[test]
    fn displaying_another_project_re_arms_the_section_defaults() {
        let (mut a, tmp) = app_with_project();
        assert!(a.project_panel_changed());
        let settings = ProjectSettings {
            roots: vec![RootSpec::new(tmp.0.clone())],
            ..ProjectSettings::default()
        };
        let other = a
            .catalog
            .as_ref()
            .unwrap()
            .create_project("other", settings)
            .expect("project");
        let other_id = other.id.clone();
        a.projects.push(other);
        a.select_project(1);
        assert_eq!(a.selected.as_deref(), Some(other_id.as_str()));
        assert!(a.project_panel_changed());
        assert!(!a.project_panel_changed());
    }

    #[test]
    fn reselecting_the_same_project_is_not_a_new_display() {
        let (mut a, _tmp) = app_with_project();
        assert!(a.project_panel_changed());
        a.select_project(0); // re-clicking the shown project's row
        assert!(
            !a.project_panel_changed(),
            "a reselection keeps the user's fold state"
        );
    }

    #[test]
    fn losing_the_selection_counts_as_a_display_change() {
        let (mut a, _tmp) = app_with_project();
        assert!(a.project_panel_changed());
        a.selected = None;
        assert!(a.project_panel_changed());
        // Back to a project: a fresh display again.
        a.select_project(0);
        assert!(a.project_panel_changed());
        assert!(!a.project_panel_changed());
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
        let first = a.tab().form.project_ids.clone();
        a.new_tab();
        // The new tab starts on the same selection — a snapshot, not
        // a live link.
        assert_eq!(a.tab().form.project_ids, first);
        // Pick `other` alone in the second tab through the picker.
        a.ask_pick_projects();
        a.pick_set_all(false);
        let idx = a.projects.iter().position(|p| p.id == other.id).unwrap();
        a.pick_toggle(idx, true);
        a.dialog_confirm("");
        assert_eq!(a.tab().form.project_ids, vec![other.id.clone()]);
        // The first tab kept its own selection.
        assert_eq!(a.tabs[0].form.project_ids, first);
        // Switching tabs restores each tab's own selection.
        a.activate_tab(0);
        assert_eq!(a.search_projects()[0].id, first[0]);
        a.activate_tab(1);
        assert_eq!(a.search_projects()[0].id, other.id);
    }

    #[test]
    fn renaming_a_project_updates_its_name_in_search_tabs() {
        let (mut a, _tmp) = app_with_project();
        let id = a.tab().form.project_ids.first().cloned().unwrap();
        a.catalog
            .as_ref()
            .unwrap()
            .rename_project(&id, "Renamed")
            .expect("rename");
        a.refresh();
        // Same stable project id, new name — the picker is rebuilt
        // from the projects cache, so every tab shows the new name
        // without its selection moving.
        assert_eq!(a.tab().form.project_ids, vec![id.clone()]);
        assert_eq!(a.search_projects()[0].name, "Renamed");
    }

    #[test]
    fn startup_seeds_the_tab_project_and_loads_its_saved_searches() {
        let tmp = TempDir::new("startup-saved");
        let catalog = Catalog::open(tmp.0.join("projects.db")).expect("open catalog");
        let settings = ProjectSettings {
            roots: vec![RootSpec::new(tmp.0.clone())],
            ..ProjectSettings::default()
        };
        let first = catalog
            .create_project("first", settings.clone())
            .expect("project");
        let second = catalog.create_project("second", settings).expect("project");
        catalog
            .create_saved_search(&second.id, "alpha", "needle", SearchParams::default())
            .expect("saved search");
        // Startup state: empty caches, no selection — what open_catalog
        // hands to refresh(). Without a remembered project the tab
        // starts on the first one.
        let mut a = app();
        a.catalog = Some(catalog);
        a.refresh();
        assert_eq!(
            a.tab().form.project_ids,
            vec![first.id.clone()],
            "the startup tab starts on the first project"
        );
        assert_eq!(
            a.saved.len(),
            1,
            "the saved list is global — every project's searches"
        );
        assert_eq!(a.saved[0].name, "alpha");
        // A fresh session remembers the last project used for a
        // search: the tab starts there.
        a.tabs[0].form.project_ids.clear();
        a.tabs[0].form.projects_picked = false;
        a.prefs.last_search_project_id = Some(second.id.clone());
        a.refresh();
        assert_eq!(
            a.tab().form.project_ids,
            vec![second.id.clone()],
            "the remembered project wins"
        );
        assert_eq!(a.saved.len(), 1, "the global list is unchanged");
        assert_eq!(a.saved_index(), 0, "the combo rests on the placeholder");
    }

    #[test]
    fn picking_a_search_project_remembers_it_for_the_next_session() {
        let (mut a, tmp) = app_with_project();
        let other = {
            let catalog = a.catalog.as_ref().unwrap();
            let settings = ProjectSettings {
                roots: vec![RootSpec::new(tmp.0.clone())],
                ..ProjectSettings::default()
            };
            catalog.create_project("other", settings).expect("project")
        };
        a.projects.push(other.clone());
        a.ask_pick_projects();
        a.pick_set_all(false);
        a.pick_toggle(1, true);
        a.dialog_confirm("");
        assert_eq!(a.tab().form.project_ids, vec![other.id.clone()]);
        assert_eq!(
            a.prefs.last_search_project_id.as_deref(),
            Some(other.id.as_str())
        );
        // The preference is on disk, so the next session sees it too.
        let reloaded = a
            .catalog
            .as_ref()
            .unwrap()
            .load_preferences()
            .expect("reload preferences");
        assert_eq!(
            reloaded.last_search_project_id.as_deref(),
            Some(other.id.as_str())
        );
    }

    #[test]
    fn deleting_a_project_used_by_a_search_tab_is_refused() {
        let (mut a, _tmp) = app_with_project();
        let id = a.tab().form.project_ids.first().cloned().unwrap();
        a.ask_delete_project();
        assert!(
            a.dialog.is_none(),
            "no confirmation for a project a tab still selects"
        );
        assert!(a
            .notices
            .iter()
            .any(|n| n.level == BannerLevel::Warning && n.sticky));
        // Once no tab selects it, deletion proceeds normally.
        a.tab_mut().form.project_ids.clear();
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

    #[test]
    fn relaunching_a_build_clears_the_previous_report() {
        let (mut a, _tmp) = app_with_project();
        a.start_build();
        a.cancel_build();
        let mut tries = 0;
        while a.poll_build() {
            tries += 1;
            assert!(tries < 2000, "build did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(a.last_report.is_some(), "cancelled run kept a report");

        a.start_build();
        assert!(a.build.is_some());
        assert!(
            a.last_report.is_none(),
            "a fresh run clears the stale report"
        );
        // Do not leave a build running behind the test.
        a.cancel_build();
        while a.poll_build() {
            tries += 1;
            assert!(tries < 4000, "build did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
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

        tx0.send(SearchMsg::Initial {
            target: 0,
            report: report(vec![file("a.txt", &[1])], 0),
        })
        .unwrap();
        tx1.send(SearchMsg::Initial {
            target: 0,
            report: report(vec![file("b.txt", &[2])], 2),
        })
        .unwrap();
        // Phase-B event of tab 1's job — lands in tab 1 only.
        tx1.send(SearchMsg::Progress {
            target: 0,
            done: 1,
            total: 2,
            found: Some(file("b-big.txt", &[9])),
        })
        .unwrap();
        tx0.send(done(ProjectOutcome::Success(report(
            vec![file("a.txt", &[1])],
            0,
        ))))
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
        let _ = tx0.send(SearchMsg::Initial {
            target: 0,
            report: report(vec![file("a.txt", &[1])], 0),
        });
        tx1.send(done(ProjectOutcome::Success(report(
            vec![file("b.txt", &[2])],
            0,
        ))))
        .unwrap();
        a.poll_search();
        assert_eq!(result_files(&a, 0), vec!["b.txt"]);
        assert!(a.tabs[0].job.is_none());
    }

    #[test]
    fn cancelled_search_marks_the_tab_results() {
        let mut a = app();
        let (_c, tx) = fake_job(&mut a, 0);
        tx.send(SearchMsg::Initial {
            target: 0,
            report: report(vec![file("a.txt", &[1])], 0),
        })
        .unwrap();
        tx.send(done(ProjectOutcome::Cancelled)).unwrap();
        a.poll_search();
        a.tabs[0].results.with(|l| {
            assert!(l.cancelled);
            assert!(!l.in_flight);
        });
        assert!(a.tabs[0].job.is_none());
    }

    // -- Multi-project results merge ----------------------------------------

    #[test]
    fn a_multi_target_done_merges_unique_physical_files() {
        let mut a = app();
        let tx = fake_job_two(&mut a, 0);
        tx.send(SearchMsg::Done(vec![
            ProjectResult {
                target: target("pa", "A"),
                outcome: ProjectOutcome::Success(report(
                    vec![file("c:/s/common.java", &[1]), file("c:/s/a.java", &[2])],
                    0,
                )),
            },
            ProjectResult {
                target: target("pb", "B"),
                outcome: ProjectOutcome::Success(report(
                    vec![file("c:/s/common.java", &[9]), file("c:/s/b.java", &[3])],
                    0,
                )),
            },
        ]))
        .unwrap();
        a.poll_search();
        a.tabs[0].results.with(|l| {
            let paths: Vec<PathBuf> = l
                .report
                .results
                .iter()
                .map(|r| r.file_path.clone())
                .collect();
            // common.java once, kept from project A — three unique
            // physical files in canonical order.
            assert_eq!(
                paths,
                ["c:/s/a.java", "c:/s/b.java", "c:/s/common.java"].map(PathBuf::from)
            );
            assert_eq!(l.report.results[2].occurrences[0].line, 1);
            // Each file keeps the project that produced it — the
            // viewer resolves that project's fallback encoding.
            assert_eq!(l.project_id_of(0), "pa");
            assert_eq!(l.project_id_of(1), "pb");
            assert_eq!(l.project_id_of(2), "pa");
            assert!(!l.in_flight);
            assert!(!l.cancelled);
        });
        // The success banner counts unique files and their matches:
        // 3 files, 3 matches — never per-project pairs.
        let last = a.notices.last().expect("done banner");
        assert_eq!(last.level, BannerLevel::Success);
        assert!(last.text.contains('3'), "{}", last.text);
        assert!(a.tabs[0].job.is_none());
    }

    #[test]
    fn phase_a_reports_merge_in_flight_across_targets() {
        let mut a = app();
        let tx = fake_job_two(&mut a, 0);
        tx.send(SearchMsg::Initial {
            target: 0,
            report: report(vec![file("f0.txt", &[1]), file("f2.txt", &[1])], 0),
        })
        .unwrap();
        tx.send(SearchMsg::Initial {
            target: 1,
            report: report(vec![file("f0.txt", &[9]), file("f1.txt", &[1])], 0),
        })
        .unwrap();
        a.poll_search();
        a.tabs[0].results.with(|l| {
            let paths: Vec<PathBuf> = l
                .report
                .results
                .iter()
                .map(|r| r.file_path.clone())
                .collect();
            assert_eq!(paths, ["f0.txt", "f1.txt", "f2.txt"].map(PathBuf::from));
            // The duplicate kept target 0's report — the first in
            // selection order.
            assert_eq!(l.report.results[0].occurrences[0].line, 1);
            assert_eq!(l.project_id_of(0), "pa");
            assert_eq!(l.project_id_of(1), "pb");
            assert!(l.in_flight, "the job still runs");
        });
    }

    #[test]
    fn a_partial_done_keeps_verified_results_and_reports_the_failure() {
        let mut a = app();
        let tx = fake_job_two(&mut a, 0);
        tx.send(SearchMsg::Initial {
            target: 0,
            report: report(vec![file("f0.txt", &[1])], 0),
        })
        .unwrap();
        tx.send(SearchMsg::Done(vec![
            ProjectResult {
                target: target("pa", "A"),
                outcome: ProjectOutcome::Success(report(vec![file("f0.txt", &[1])], 0)),
            },
            ProjectResult {
                target: target("pb", "B"),
                outcome: ProjectOutcome::Failed(SearchError::Index(
                    rsearch_engine::IndexError::NotFound,
                )),
            },
        ]))
        .unwrap();
        a.poll_search();
        a.tabs[0].results.with(|l| {
            // A failure never erases verified results.
            assert_eq!(l.report.results.len(), 1);
            assert!(!l.in_flight);
            assert!(!l.cancelled);
        });
        // The failing project is named in its own sticky banner.
        let err = a
            .notices
            .iter()
            .find(|n| n.level == BannerLevel::Error)
            .expect("failure banner");
        assert!(err.text.contains("B"), "{}", err.text);
        assert!(err.sticky);
    }

    #[test]
    fn a_cancelled_target_keeps_the_other_targets_results() {
        let mut a = app();
        let tx = fake_job_two(&mut a, 0);
        tx.send(SearchMsg::Initial {
            target: 0,
            report: report(vec![file("f0.txt", &[1])], 0),
        })
        .unwrap();
        tx.send(SearchMsg::Done(vec![
            ProjectResult {
                target: target("pa", "A"),
                outcome: ProjectOutcome::Success(report(vec![file("f0.txt", &[1])], 0)),
            },
            ProjectResult {
                target: target("pb", "B"),
                outcome: ProjectOutcome::Cancelled,
            },
        ]))
        .unwrap();
        a.poll_search();
        a.tabs[0].results.with(|l| {
            assert_eq!(l.report.results.len(), 1);
            assert!(l.cancelled, "the merged list is incomplete");
            assert!(!l.in_flight);
        });
        // One synthesized banner: the partial summary (1 of 2
        // projects) followed by the cancellation note.
        let expected = format!(
            "{}\n{}\n{}",
            a.tr.search_partial(1, 2, 1, 1),
            a.tr.search_outcomes(0, 1, 0),
            a.tr.search_cancelled
        );
        assert!(
            a.notices.iter().any(|n| n.text == expected),
            "{:?}",
            a.notices.iter().map(|n| &n.text).collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_viewer_opens_the_retained_result_of_a_merged_list() {
        // The shared file's first report came from project A — the
        // viewer opens that FileResult's path and occurrence.
        let mut a = app();
        let tx = fake_job_two(&mut a, 0);
        tx.send(SearchMsg::Done(vec![
            ProjectResult {
                target: target("pa", "A"),
                outcome: ProjectOutcome::Success(report(vec![file("c:/s/common.java", &[7])], 0)),
            },
            ProjectResult {
                target: target("pb", "B"),
                outcome: ProjectOutcome::Success(report(vec![file("c:/s/common.java", &[3])], 0)),
            },
        ]))
        .unwrap();
        a.poll_search();
        // One file — the retained FileResult of project A opens.
        a.tabs[0]
            .results
            .with(|l| assert_eq!(l.report.results.len(), 1));
        a.open_viewer(0, 0);
        let v = a.tabs[0].viewer.as_ref().expect("viewer opened");
        assert!(v.title.starts_with("c:/s/common.java:"), "{}", v.title);
        assert_eq!(v.path, PathBuf::from("c:/s/common.java"));
        assert_eq!(v.focus_line, 7, "the first report's occurrence");
        a.close_viewer();
    }

    // -- Multi-project selection ----------------------------------------------

    /// A second project in the same catalog (roots on `tmp`).
    fn make_project(a: &App, tmp: &TempDir, name: &str) -> Project {
        let settings = ProjectSettings {
            roots: vec![RootSpec::new(tmp.0.clone())],
            ..ProjectSettings::default()
        };
        a.catalog
            .as_ref()
            .unwrap()
            .create_project(name, settings)
            .expect("project")
    }

    /// An empty index file — `can_search` only checks its existence.
    fn write_index(p: &Project) {
        std::fs::write(&p.index_db_path, b"").expect("empty index file");
    }

    /// The active tab's picker draft: open, apply `edit` to the
    /// checkbox vector, confirm.
    fn pick_with(a: &mut App, edit: impl Fn(&mut App)) {
        a.ask_pick_projects();
        edit(a);
        a.dialog_confirm("");
    }

    #[test]
    fn an_empty_selection_cannot_search() {
        let (mut a, _tmp) = app_with_project();
        a.tab_mut().form.project_ids.clear();
        a.tab_mut().form.query = "abc".into();
        assert!(!a.can_search(), "no project selected — no search");
        a.run_search();
        assert!(a.tab().job.is_none(), "the refusal launches nothing");
    }

    #[test]
    fn a_multi_selection_searches_every_project_in_selection_order() {
        let (mut a, tmp) = app_with_project();
        let second = make_project(&a, &tmp, "second");
        write_index(&second);
        a.refresh();
        // A non-catalog order: selection order drives the target list.
        let first = a.projects[0].id.clone();
        a.tab_mut().form.project_ids = vec![second.id.clone(), first.clone()];
        a.tab_mut().form.query = "abc".into();
        assert!(a.can_search());
        a.run_search();
        let job = a.tab().job.as_ref().expect("a search job runs");
        let order: Vec<&str> = job.targets.iter().map(|t| t.project_id.as_str()).collect();
        assert_eq!(order, [second.id.as_str(), first.as_str()]);
        a.cancel_search();
    }

    #[test]
    fn run_search_deduplicates_and_prunes_the_selection() {
        let (mut a, _tmp) = app_with_project();
        let id = a.projects[0].id.clone();
        a.tab_mut().form.project_ids = vec![id.clone(), "gone".into(), id.clone()];
        a.tab_mut().form.query = "abc".into();
        a.run_search();
        let job = a.tab().job.as_ref().expect("a search job runs");
        assert_eq!(job.targets.len(), 1, "one target per project id");
        assert_eq!(
            a.tab().form.project_ids,
            vec![id],
            "the vanished id and the duplicate are pruned"
        );
        assert!(
            a.notices.iter().any(|n| n.text == a.tr.missing_projects(1)),
            "the pruning is reported"
        );
        a.cancel_search();
    }

    #[test]
    fn a_new_tab_inherits_the_full_selection() {
        let (mut a, tmp) = app_with_project();
        let second = make_project(&a, &tmp, "second");
        a.refresh();
        let first = a.projects[0].id.clone();
        pick_with(&mut a, |a| {
            a.pick_set_all(true);
        });
        let both = vec![first, second.id.clone()];
        assert_eq!(a.tab().form.project_ids, both, "the picker selected all");
        a.new_tab();
        assert_eq!(
            a.tab().form.project_ids,
            both,
            "the child inherits the complete selection"
        );
    }

    #[test]
    fn cancelling_the_picker_leaves_the_selection_untouched() {
        let (mut a, _tmp) = app_with_project();
        let before = a.tab().form.project_ids.clone();
        a.ask_pick_projects();
        a.pick_set_all(false);
        a.dialog_cancel();
        assert_eq!(a.tab().form.project_ids, before);
        assert!(a.dialog.is_none());
        // The picker draft itself was thrown away — reopening shows
        // the tab's selection again.
        a.ask_pick_projects();
        assert_eq!(a.pick_rows(), vec![(a.projects[0].name.clone(), true)]);
    }

    #[test]
    fn changing_the_selection_dissociates_the_loaded_saved_search() {
        let (mut a, _tmp) = app_with_project();
        let saved = make_saved(&mut a, "s", "W3C", SearchParams::default());
        a.select_saved(1);
        a.load_saved();
        assert_eq!(a.tab().loaded_saved_id.as_deref(), Some(saved.id.as_str()));
        // Confirming an unchanged selection keeps the association.
        a.ask_pick_projects();
        a.dialog_confirm("");
        assert_eq!(a.tab().loaded_saved_id.as_deref(), Some(saved.id.as_str()));
        // A changed selection dissociates the tab — the source entry
        // is never silently modified.
        a.ask_pick_projects();
        a.pick_set_all(false);
        a.dialog_confirm("");
        assert!(a.tab().loaded_saved_id.is_none());
        let stored = a
            .catalog
            .as_ref()
            .unwrap()
            .get_saved_search(&saved.id)
            .unwrap();
        assert_eq!(stored.query, "W3C", "the source entry is untouched");
    }

    #[test]
    fn changing_the_selection_drops_the_previous_results() {
        let (mut a, tmp) = app_with_project();
        make_project(&a, &tmp, "second");
        a.refresh();
        a.tab_mut()
            .results
            .replace(list(vec![file("f0.txt", &[1])], "q"));
        // Confirming an unchanged selection keeps the list.
        pick_with(&mut a, |_| {});
        assert!(a.tab().results.with(|l| l.present));
        // A changed selection makes the displayed list stale.
        pick_with(&mut a, |a| a.pick_set_all(true));
        assert!(!a.tab().results.with(|l| l.present));
    }

    #[test]
    fn the_searching_banner_names_the_running_target() {
        let mut a = app();
        let tx = fake_job_two(&mut a, 0);
        tx.send(SearchMsg::Started { target: 1 }).unwrap();
        a.poll_search();
        let banners = a.banners();
        let banner = banners
            .iter()
            .find(|b| b.working)
            .expect("the searching banner");
        assert!(banner.text.contains("2/2"), "{}", banner.text);
        assert!(banner.text.contains("B"), "{}", banner.text);
    }

    #[test]
    fn a_partial_multi_done_summarizes_unique_files_and_projects() {
        let mut a = app();
        let tx = fake_job_two(&mut a, 0);
        tx.send(SearchMsg::Initial {
            target: 0,
            report: report(vec![file("f0.txt", &[1])], 0),
        })
        .unwrap();
        tx.send(SearchMsg::Done(vec![
            ProjectResult {
                target: target("pa", "A"),
                outcome: ProjectOutcome::Success(report(vec![file("f0.txt", &[1])], 0)),
            },
            ProjectResult {
                target: target("pb", "B"),
                outcome: ProjectOutcome::Failed(SearchError::Internal("boom".into())),
            },
        ]))
        .unwrap();
        a.poll_search();
        // One synthesized banner: the unique merged files, the
        // projects completed, then a detail line per failure.
        let expected = format!(
            "{}\n{}\nB: internal search error: boom",
            a.tr.search_partial(1, 2, 1, 1),
            a.tr.search_outcomes(1, 0, 0)
        );
        let banner = a
            .notices
            .iter()
            .find(|n| n.text == expected)
            .expect("the aggregate summary banner");
        assert_eq!(banner.level, BannerLevel::Error);
        assert!(banner.sticky);
    }

    #[test]
    fn a_saved_search_restores_its_multi_project_selection() {
        let (mut a, tmp) = app_with_project();
        let second = make_project(&a, &tmp, "second");
        a.refresh();
        let first = a.projects[0].id.clone();
        let params = SearchParams {
            project_ids: vec![first.clone(), second.id.clone()],
            ..SearchParams::default()
        };
        make_saved(&mut a, "multi", "W3C", params);
        a.select_saved(1);
        a.load_saved();
        assert_eq!(
            a.tab().form.project_ids,
            vec![first, second.id.clone()],
            "the whole stored selection is restored"
        );
        assert!(a.tab().form.projects_picked);
    }

    #[test]
    fn a_historical_saved_search_falls_back_to_its_owner() {
        let (mut a, _tmp) = app_with_project();
        // No project_ids — the document predates multi-project
        // searches; the owning row's project_id is the selection.
        make_saved(&mut a, "old", "W3C", SearchParams::default());
        a.select_saved(1);
        a.load_saved();
        assert_eq!(a.tab().form.project_ids, vec![a.projects[0].id.clone()]);
    }

    #[test]
    fn loading_a_saved_search_drops_projects_that_vanished() {
        let (mut a, tmp) = app_with_project();
        let second = make_project(&a, &tmp, "second");
        a.refresh();
        let first = a.projects[0].id.clone();
        let params = SearchParams {
            project_ids: vec![first.clone(), second.id.clone()],
            ..SearchParams::default()
        };
        make_saved(&mut a, "multi", "W3C", params);
        // The second project disappears from the cache — as if
        // deleted by another path.
        a.projects.retain(|p| p.id != second.id);
        a.select_saved(1);
        a.load_saved();
        assert_eq!(a.tab().form.project_ids, vec![first]);
        assert!(a.notices.iter().any(|n| n.text == a.tr.missing_projects(1)));
    }

    #[test]
    fn a_saved_search_with_no_surviving_project_keeps_its_other_params() {
        let (mut a, _tmp) = app_with_project();
        // Only dead ids: the query and options still load, the
        // selection just has to be made again.
        let mut params = SearchParams {
            whole_word: true,
            ..SearchParams::default()
        };
        params.project_ids = vec!["gone".into()];
        make_saved(&mut a, "dead", "W3C", params);
        a.select_saved(1);
        a.load_saved();
        assert!(a.tab().form.project_ids.is_empty());
        assert!(a.tab().form.projects_picked, "no silent reseed");
        assert_eq!(a.tab().form.query, "W3C");
        assert!(a.tab().form.whole_word);
        assert!(a
            .notices
            .iter()
            .any(|n| n.text == a.tr.saved_needs_projects && n.sticky));
    }

    #[test]
    fn saving_writes_project_ids_with_the_first_as_owner() {
        let (mut a, tmp) = app_with_project();
        let second = make_project(&a, &tmp, "second");
        a.refresh();
        let first = a.projects[0].id.clone();
        a.tab_mut().form.project_ids = vec![first.clone(), second.id.clone()];
        a.tab_mut().form.query = "W3C".into();
        a.ask_save_search();
        assert!(matches!(a.dialog, Some(Dialog::SaveSearch)));
        a.dialog_confirm("multi");
        assert_eq!(a.saved.len(), 1);
        let stored = &a.saved[0];
        // The whole selection is persisted; the SQL owner stays the
        // first selected project.
        assert_eq!(stored.project_id, first);
        assert_eq!(stored.params.project_ids, vec![first, second.id.clone()]);
    }

    #[test]
    fn saving_a_single_project_also_writes_project_ids() {
        let (mut a, _tmp) = app_with_project();
        let id = a.projects[0].id.clone();
        a.tab_mut().form.query = "W3C".into();
        a.ask_save_search();
        a.dialog_confirm("mono");
        assert_eq!(a.saved[0].params.project_ids, vec![id]);
    }

    #[test]
    fn duplicating_a_saved_search_keeps_its_whole_selection() {
        let (mut a, tmp) = app_with_project();
        let second = make_project(&a, &tmp, "second");
        a.refresh();
        let first = a.projects[0].id.clone();
        let params = SearchParams {
            project_ids: vec![first.clone(), second.id.clone()],
            ..SearchParams::default()
        };
        make_saved(&mut a, "multi", "W3C", params);
        a.select_saved(1);
        a.load_saved();
        a.ask_save_search();
        assert!(a.dialog_can_duplicate());
        a.dialog_duplicate("copy");
        assert_eq!(a.saved.len(), 2);
        let copy = a.saved.iter().find(|s| s.name == "copy").unwrap();
        assert_eq!(copy.project_id, first);
        assert_eq!(
            copy.params.project_ids,
            vec![first.clone(), second.id.clone()]
        );
        // The source entry is unchanged.
        let source = a.saved.iter().find(|s| s.name == "multi").unwrap();
        assert_eq!(source.params.project_ids, vec![first, second.id]);
    }

    #[test]
    fn the_saved_list_spans_every_project() {
        let (mut a, tmp) = app_with_project();
        let second = make_project(&a, &tmp, "second");
        a.catalog
            .as_ref()
            .unwrap()
            .create_saved_search(&second.id, "on-b", "q", SearchParams::default())
            .expect("saved");
        make_saved(&mut a, "on-a", "q", SearchParams::default());
        // Both projects' entries are listed, whatever the tab selects.
        let names: Vec<&str> = a.saved.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"on-a") && names.contains(&"on-b"),
            "{names:?}"
        );
    }

    #[test]
    fn deleting_a_project_refreshes_the_saved_cache() {
        let (mut a, tmp) = app_with_project();
        let second = make_project(&a, &tmp, "second");
        a.catalog
            .as_ref()
            .unwrap()
            .create_saved_search(&second.id, "on-b", "q", SearchParams::default())
            .expect("saved");
        a.refresh();
        assert_eq!(a.saved.len(), 1);
        // No tab selects `second` — deletion proceeds; the catalog
        // cleans its saved searches and the cache follows.
        a.selected = Some(second.id.clone());
        a.ask_delete_project();
        assert!(matches!(a.dialog, Some(Dialog::ConfirmDelete { .. })));
        a.dialog_confirm("");
        assert!(a.projects.iter().all(|p| p.id != second.id));
        assert!(a.saved.iter().all(|s| s.name != "on-b"));
    }

    #[test]
    fn audit_launch_resolves_the_current_catalog_without_changing_other_tabs() {
        let (mut a, tmp) = app_with_project();
        let first = a.projects[0].id.clone();
        let second = make_project(&a, &tmp, "second");
        a.refresh();
        a.tab_mut().form.project_ids = vec![second.id.clone(), first.clone()];
        a.tab_mut().form.query = "needle".into();
        a.new_tab();
        a.activate_tab(0);
        a.catalog
            .as_ref()
            .unwrap()
            .delete_project(&second.id)
            .unwrap();
        a.run_search();
        assert_eq!(a.tab().form.project_ids, vec![first.clone()]);
        assert_eq!(a.tab().job.as_ref().unwrap().targets.len(), 1);
        assert_eq!(a.tabs[1].form.project_ids, vec![second.id, first]);
        assert!(a.notices.iter().any(|n| n.text == a.tr.missing_projects(1)));
        a.cancel_search();
    }

    #[test]
    fn audit_pruned_empty_selection_is_not_reseeded() {
        let (mut a, _tmp) = app_with_project();
        a.tab_mut().form.project_ids = vec!["gone".into()];
        a.tab_mut().form.projects_picked = false;
        a.tab_mut().form.query = "needle".into();
        a.tab_mut().loaded_saved_id = Some("source".into());
        a.run_search();
        assert!(a.tab().job.is_none());
        assert!(a.tab().loaded_saved_id.is_none());
        a.refresh();
        a.new_tab();
        assert!(a.tabs.iter().all(|t| t.form.project_ids.is_empty()));
    }

    #[test]
    fn audit_picker_draft_survives_project_list_refresh() {
        let (mut a, tmp) = app_with_project();
        let first = a.projects[0].id.clone();
        let second = make_project(&a, &tmp, "second");
        a.refresh();
        a.ask_pick_projects();
        a.pick_toggle(0, false);
        a.pick_toggle(1, true);
        a.catalog.as_ref().unwrap().delete_project(&first).unwrap();
        a.refresh();
        a.dialog_confirm("");
        assert_eq!(a.tab().form.project_ids, vec![second.id]);
    }

    #[test]
    fn audit_done_preserves_the_in_flight_display_state() {
        let mut a = app();
        let tx = fake_job_two(&mut a, 0);
        for (target, files) in [
            (0, vec![file("c.txt", &[1]), file("z.txt", &[2])]),
            (1, vec![file("a.txt", &[3]), file("c.txt", &[9])]),
        ] {
            tx.send(SearchMsg::Initial {
                target,
                report: report(files, 0),
            })
            .unwrap();
        }
        a.poll_search();
        a.tab().results.toggle_file(1);
        a.tab().results.hide_file(2);
        a.tab().results.select(0, 0);
        tx.send(SearchMsg::Done(vec![
            ProjectResult {
                target: target("pa", "A"),
                outcome: ProjectOutcome::Success(report(
                    vec![file("c.txt", &[1]), file("z.txt", &[2])],
                    0,
                )),
            },
            ProjectResult {
                target: target("pb", "B"),
                outcome: ProjectOutcome::Success(report(
                    vec![file("a.txt", &[3]), file("c.txt", &[9])],
                    0,
                )),
            },
        ]))
        .unwrap();
        a.poll_search();
        a.tab().results.with(|l| {
            assert_eq!(l.hidden, vec![false, false, true]);
            assert_eq!(l.open, vec![true, false, true]);
            assert_eq!(l.selected, Some((0, 0)));
            assert_eq!(l.project_id_of(0), "pb");
            assert_eq!(l.project_id_of(1), "pa");
            assert_eq!(l.report.results[1].occurrences[0].line, 1);
            assert!(!l.in_flight);
        });
    }

    #[test]
    fn audit_partial_done_keeps_authoritative_success_counters() {
        let mut a = app();
        let tx = fake_job_two(&mut a, 0);
        let mut initial = report(vec![file("a.txt", &[1])], 2);
        initial.verification_errors = 1;
        tx.send(SearchMsg::Initial {
            target: 0,
            report: initial,
        })
        .unwrap();
        tx.send(SearchMsg::Progress {
            target: 0,
            done: 2,
            total: 2,
            found: Some(file("big.txt", &[2])),
        })
        .unwrap();
        tx.send(SearchMsg::Initial {
            target: 1,
            report: report(vec![file("partial.txt", &[3])], 0),
        })
        .unwrap();
        let mut complete = report(vec![file("a.txt", &[1]), file("big.txt", &[2])], 2);
        complete.verification_errors = 3;
        complete.skipped_stale = 4;
        complete.truncated_files = 1;
        tx.send(SearchMsg::Done(vec![
            ProjectResult {
                target: target("pa", "A"),
                outcome: ProjectOutcome::Success(complete),
            },
            ProjectResult {
                target: target("pb", "B"),
                outcome: ProjectOutcome::Failed(SearchError::Internal("boom".into())),
            },
        ]))
        .unwrap();
        a.poll_search();
        a.tab().results.with(|l| {
            assert_eq!(l.report.results.len(), 3);
            assert_eq!(l.report.verification_errors, 3);
            assert_eq!(l.report.skipped_stale, 4);
            assert_eq!(l.report.truncated_files, 1);
            assert_eq!(l.report.candidates_too_large, 2);
            assert!(!l.in_flight);
        });
    }

    #[test]
    fn audit_disconnected_worker_releases_only_its_tab_and_keeps_results() {
        let mut a = app();
        a.new_tab();
        let (_c0, tx0) = fake_job(&mut a, 0);
        let (_c1, _tx1) = fake_job(&mut a, 1);
        tx0.send(SearchMsg::Initial {
            target: 0,
            report: report(vec![file("partial.txt", &[1])], 0),
        })
        .unwrap();
        a.poll_search();
        drop(tx0);
        a.poll_search();
        assert!(a.tabs[0].job.is_none());
        assert!(a.tabs[1].job.is_some());
        assert_eq!(result_files(&a, 0), vec!["partial.txt"]);
        assert!(!a.tabs[0].results.with(|l| l.in_flight));
        assert!(a
            .notices
            .iter()
            .any(|n| n.level == BannerLevel::Error && n.sticky));
    }

    #[test]
    fn audit_displayed_progress_is_local_to_the_running_project() {
        let mut a = app();
        let tx = fake_job_two(&mut a, 0);
        a.tab_mut().job.as_mut().unwrap().analyze_oversized = true;
        tx.send(SearchMsg::Started { target: 0 }).unwrap();
        tx.send(SearchMsg::Initial {
            target: 0,
            report: report(Vec::new(), 2),
        })
        .unwrap();
        tx.send(SearchMsg::Progress {
            target: 0,
            done: 2,
            total: 2,
            found: None,
        })
        .unwrap();
        tx.send(SearchMsg::Started { target: 1 }).unwrap();
        a.poll_search();
        assert_eq!(crate::ui::oversized_progress_note(&a), None);
        tx.send(SearchMsg::Initial {
            target: 1,
            report: report(Vec::new(), 3),
        })
        .unwrap();
        tx.send(SearchMsg::Progress {
            target: 1,
            done: 1,
            total: 3,
            found: None,
        })
        .unwrap();
        a.poll_search();
        assert_eq!(
            crate::ui::oversized_progress_note(&a),
            Some(a.tr.oversized_progress(1, 3))
        );
    }

    #[test]
    fn audit_partial_summary_distinguishes_failed_cancelled_and_unattempted_projects() {
        let mut a = app();
        let (tx, rx) = mpsc::channel();
        let targets = vec![
            target("pa", "A"),
            target("pb", "B"),
            target("pc", "C"),
            target("pd", "D"),
        ];
        a.tab_mut().job = Some(SearchJob::for_test_targets(
            0,
            targets,
            Arc::new(AtomicBool::new(false)),
            rx,
        ));
        tx.send(SearchMsg::Initial {
            target: 0,
            report: report(vec![file("common.txt", &[1])], 0),
        })
        .unwrap();
        tx.send(SearchMsg::Initial {
            target: 2,
            report: report(vec![file("COMMON.TXT", &[9])], 0),
        })
        .unwrap();
        tx.send(SearchMsg::Done(vec![
            ProjectResult {
                target: target("pa", "A"),
                outcome: ProjectOutcome::Success(report(vec![file("common.txt", &[1])], 0)),
            },
            ProjectResult {
                target: target("pb", "B"),
                outcome: ProjectOutcome::Failed(SearchError::Internal("boom".into())),
            },
            ProjectResult {
                target: target("pc", "C"),
                outcome: ProjectOutcome::Cancelled,
            },
            ProjectResult {
                target: target("pd", "D"),
                outcome: ProjectOutcome::NotAttempted,
            },
        ]))
        .unwrap();
        a.poll_search();
        let text = &a.notices.last().unwrap().text;
        assert!(text.contains(&a.tr.search_partial(1, 4, 1, 1)));
        assert!(text.contains("failed: 1"), "{text}");
        assert!(text.contains("cancelled: 1"), "{text}");
        assert!(text.contains("not attempted: 1"), "{text}");
        assert!(a.tab().results.with(|l| l.cancelled));
    }

    #[test]
    fn audit_project_failures_at_each_position_keep_other_results() {
        for failed in 0..=3 {
            let mut a = app();
            let (tx, rx) = mpsc::channel();
            let targets = vec![target("pa", "A"), target("pb", "B"), target("pc", "C")];
            a.tab_mut().job = Some(SearchJob::for_test_targets(
                0,
                targets,
                Arc::new(AtomicBool::new(false)),
                rx,
            ));
            let mut outcomes = Vec::new();
            for (i, (id, name)) in [("pa", "A"), ("pb", "B"), ("pc", "C")]
                .into_iter()
                .enumerate()
            {
                tx.send(SearchMsg::Started { target: i }).unwrap();
                let outcome = if i == failed || failed == 3 {
                    ProjectOutcome::Failed(SearchError::Internal("boom".into()))
                } else {
                    let files = vec![
                        file("common.txt", &[i + 1]),
                        file(&format!("{id}.txt"), &[1]),
                    ];
                    tx.send(SearchMsg::Initial {
                        target: i,
                        report: report(files, 0),
                    })
                    .unwrap();
                    ProjectOutcome::Success(report(
                        vec![
                            file("common.txt", &[i + 1]),
                            file(&format!("{id}.txt"), &[1]),
                        ],
                        0,
                    ))
                };
                outcomes.push(ProjectResult {
                    target: target(id, name),
                    outcome,
                });
            }
            tx.send(SearchMsg::Done(outcomes)).unwrap();
            a.poll_search();
            assert!(a.tab().job.is_none());
            let (successes, files) = if failed == 3 { (0, 0) } else { (2, 3) };
            a.tab().results.with(|l| {
                assert_eq!(l.report.results.len(), files);
                assert!(!l.in_flight && !l.cancelled);
                if failed < 3 {
                    let first_success = if failed == 0 { 1 } else { 0 };
                    assert_eq!(l.report.results[0].occurrences[0].line, first_success + 1);
                }
            });
            let notice = a.notices.last().unwrap();
            assert_eq!(notice.level, BannerLevel::Error);
            assert!(notice.sticky);
            assert!(notice
                .text
                .contains(&a.tr.search_partial(successes, 3, files, files)));
            assert!(notice
                .text
                .contains(&a.tr.search_outcomes(3 - successes, 0, 0)));
        }
    }

    #[test]
    fn audit_zero_match_success_still_reports_the_completed_projects() {
        let mut a = app();
        let tx = fake_job_two(&mut a, 0);
        tx.send(SearchMsg::Done(vec![
            ProjectResult {
                target: target("pa", "A"),
                outcome: ProjectOutcome::Success(report(Vec::new(), 0)),
            },
            ProjectResult {
                target: target("pb", "B"),
                outcome: ProjectOutcome::Success(report(Vec::new(), 0)),
            },
        ]))
        .unwrap();
        a.poll_search();
        assert_eq!(
            a.notices.last().unwrap().text,
            a.tr.search_done_multi(0, 0, 2, Duration::from_millis(2))
        );
        assert!(a.tab().job.is_none());
        assert!(a
            .tab()
            .results
            .with(|l| l.present && !l.in_flight && !l.cancelled));
        let mut mono = app();
        let (_cancel, tx) = fake_job(&mut mono, 0);
        tx.send(done(ProjectOutcome::Success(report(Vec::new(), 0))))
            .unwrap();
        mono.poll_search();
        assert_eq!(mono.notices.last().unwrap().text, mono.tr.no_results_hint);
        assert!(mono.tab().job.is_none());
    }

    #[test]
    fn audit_edited_selection_creates_a_new_saved_search_after_tab_switches() {
        let (mut a, tmp) = app_with_project();
        let second = make_project(&a, &tmp, "second");
        write_index(&second);
        a.refresh();
        let params = SearchParams {
            project_ids: vec![a.projects[0].id.clone(), second.id.clone()],
            whole_word: true,
            analyze_oversized: true,
            context_lines: 5,
            include_masks: vec!["*.txt".into()],
            exclude_masks: vec!["skip*".into()],
            ..SearchParams::default()
        };
        let source = make_saved(&mut a, "source", "needle", params);
        a.select_saved(1);
        a.load_saved();
        a.tab_mut().form.case_sensitive = true;
        a.ask_pick_projects();
        a.pick_toggle(0, false);
        a.dialog_confirm("");
        let edited = a.active_tab;
        a.activate_tab(0);
        a.activate_tab(edited as i32);
        assert_eq!(a.tab().form.project_ids, vec![second.id.clone()]);
        assert!(a.tab().loaded_saved_id.is_none());
        a.run_search();
        let job = a.tab().job.as_ref().unwrap();
        assert_eq!(job.query, source.query);
        assert!(job.case_sensitive && job.whole_word && job.analyze_oversized);
        assert_eq!(job.targets[0].project_id, second.id);
        a.cancel_search();
        a.ask_save_search();
        a.dialog_confirm("edited");
        let catalog = a.catalog.as_ref().unwrap();
        let original = catalog.get_saved_search(&source.id).unwrap();
        assert_eq!(original.params, source.params);
        assert_eq!(original.query, source.query);
        let saved = catalog
            .get_saved_search(a.tab().loaded_saved_id.as_ref().unwrap())
            .unwrap();
        assert_ne!(saved.id, source.id);
        assert_eq!(saved.project_id, second.id);
        assert!(
            saved.params.case_sensitive
                && saved.params.whole_word
                && saved.params.analyze_oversized
        );
        assert_eq!(saved.params.context_lines, 5);
        assert_eq!(saved.params.include_masks, source.params.include_masks);
        assert_eq!(saved.params.exclude_masks, source.params.exclude_masks);
    }

    #[test]
    fn audit_deleting_an_unselected_owner_refreshes_the_multi_saved_cache() {
        let (mut a, tmp) = app_with_project();
        let first = a.projects[0].id.clone();
        let second = make_project(&a, &tmp, "second");
        a.refresh();
        let params = SearchParams {
            project_ids: vec![first.clone(), second.id.clone()],
            case_sensitive: true,
            context_lines: 5,
            ..SearchParams::default()
        };
        let saved = make_saved(&mut a, "multi", "needle", params);
        pick_with(&mut a, |a| {
            a.pick_toggle(0, false);
            a.pick_toggle(1, true);
        });
        a.selected = Some(first);
        a.ask_delete_project();
        assert!(matches!(a.dialog, Some(Dialog::ConfirmDelete { .. })));
        a.dialog_confirm("");
        assert_eq!(a.saved.len(), 1);
        assert_eq!(a.saved[0].id, saved.id);
        assert_eq!(a.saved[0].project_id, second.id);
        assert_eq!(a.saved[0].params.project_ids, vec![second.id.clone()]);
        a.select_saved(1);
        a.load_saved();
        assert_eq!(a.tab().form.project_ids, vec![second.id]);
        assert!(a.tab().form.case_sensitive);
        assert_eq!(a.tab().form.context_lines, 5);
        assert_eq!(a.tab().form.query, "needle");
    }

    #[test]
    fn audit_late_messages_cannot_contaminate_the_next_job() {
        let mut a = app();
        let (_cancel, tx) = fake_job(&mut a, 0);
        tx.send(done(ProjectOutcome::Success(report(
            vec![file("old.txt", &[1])],
            0,
        ))))
        .unwrap();
        tx.send(SearchMsg::Started { target: 0 }).unwrap();
        tx.send(SearchMsg::Progress {
            target: 0,
            done: 1,
            total: 1,
            found: Some(file("late.txt", &[2])),
        })
        .unwrap();
        a.poll_search();
        assert!(a.tab().job.is_none());
        assert_eq!(result_files(&a, 0), vec!["old.txt"]);
        let notice = a.notices.last().unwrap().text.clone();
        let (_cancel, next) = fake_job(&mut a, 0);
        assert!(tx.send(SearchMsg::Started { target: 0 }).is_err());
        next.send(done(ProjectOutcome::Cancelled)).unwrap();
        a.poll_search();
        assert!(a.tab().job.is_none());
        assert_eq!(a.notices[0].text, notice);
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
            path: PathBuf::from("f"),
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

    #[test]
    fn open_external_without_viewer_is_a_noop() {
        let mut a = app();
        a.open_viewer_file_with_app();
        assert!(a.notices.is_empty());
    }

    #[test]
    fn open_external_on_missing_file_reports_and_launches_nothing() {
        let mut a = app();
        // A path that does not exist — the existence check must refuse
        // before any Windows call, so nothing is ever launched.
        let missing = std::env::temp_dir().join("rsearch-missing-dir/no such file.rs");
        let tab = a.tab_mut();
        tab.viewer = Some(Viewer {
            title: format!("{}:1", missing.display()),
            path: missing,
            focus_line: 1,
            loading: false,
            error: None,
            truncated: false,
            matches: Vec::new(),
            match_idx: 0,
            nav_flip: false,
        });
        a.open_viewer_file_with_app();
        assert_eq!(a.notices.len(), 1);
        assert_eq!(a.notices[0].text, a.tr.viewer_file_missing);
        assert_eq!(a.notices[0].level, BannerLevel::Error);
    }

    #[test]
    fn open_external_uses_the_displayed_documents_physical_path() {
        // The viewer's path field is the physical file open_viewer
        // received — never a title string parsed back. Spaces,
        // parentheses and accents are carried as data.
        let mut a = app();
        let real = std::env::temp_dir().join("rsearch open ext test (é).txt");
        std::fs::write(&real, b"x").expect("write fixture");
        let tab = a.tab_mut();
        tab.viewer = Some(Viewer {
            title: format!("{}:1", real.display()),
            path: real.clone(),
            focus_line: 1,
            loading: false,
            error: None,
            truncated: false,
            matches: Vec::new(),
            match_idx: 0,
            nav_flip: false,
        });
        assert_eq!(a.tab().viewer.as_ref().unwrap().path, real);
        std::fs::remove_file(&real).expect("remove fixture");
    }
}
