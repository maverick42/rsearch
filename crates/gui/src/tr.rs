//! User-facing texts, kept separate from the UI code so adding a
//! language only means adding another [`Strings`] table.
//!
//! The application holds a `&'static Strings`; every label, button,
//! hint, banner and message is read through it. English ([`EN`]) is
//! the default and reference language. Texts that embed values use a
//! `{placeholder}` template field plus a small interpolation method —
//! translators override the template, never the code.
//!
//! Because `Strings` is a plain struct literal per language, a missing
//! translation is a compile error — tables can never drift apart.

use std::time::Duration;

use rsearch_catalog::Language;
use rsearch_engine::BuildPhase;

/// All user-visible text of the GUI.
#[derive(Debug)]
pub struct Strings {
    /// Application and window title.
    pub app_title: &'static str,

    // -- Navigation ----------------------------------------------------
    pub nav_search: &'static str,
    pub nav_projects: &'static str,
    pub nav_preferences: &'static str,

    // -- Common ----------------------------------------------------------
    pub save: &'static str,
    pub cancel: &'static str,
    pub delete: &'static str,
    pub rename: &'static str,
    pub retry: &'static str,
    pub yes: &'static str,
    pub no: &'static str,
    pub browse: &'static str,
    pub edit: &'static str,
    pub open_projects: &'static str,
    /// Tooltip of a banner's close button.
    pub dismiss: &'static str,

    // -- Projects list ----------------------------------------------------
    pub projects: &'static str,
    pub new_project: &'static str,
    pub no_projects_hint: &'static str,
    pub select_project_hint: &'static str,

    // -- Project status ---------------------------------------------------
    pub status_never_built: &'static str,
    pub status_rebuild_needed: &'static str,
    pub status_up_to_date: &'static str,

    // -- Project details ----------------------------------------------------
    pub build_index: &'static str,
    pub update_index: &'static str,
    pub settings_section: &'static str,
    pub source_roots: &'static str,
    pub root_recursive: &'static str,
    pub root_top_level_only: &'static str,
    pub excluded_dirs: &'static str,
    /// File-name masks a file must match to be indexed.
    pub include_masks: &'static str,
    /// File-name masks that keep a file out of the index.
    pub exclude_masks: &'static str,
    pub respect_gitignore: &'static str,
    pub max_indexed_file_size: &'static str,
    /// Fallback encoding for files that are neither valid UTF-8 nor
    /// UTF-16.
    pub fallback_encoding: &'static str,
    /// The "no fallback" choice of the fallback-encoding combo.
    pub fallback_none: &'static str,
    pub index_archives: &'static str,
    pub archive_max_depth: &'static str,

    // -- Build progress -------------------------------------------------------
    pub cancel_build: &'static str,
    pub starting: &'static str,
    /// Localized names of the engine build phases (live progress).
    pub phase_scanning: &'static str,
    pub phase_processing: &'static str,
    pub phase_writing: &'static str,
    pub phase_finalizing: &'static str,
    pub phase_swapping: &'static str,
    pub phase_completed: &'static str,
    pub phase_cancelled: &'static str,
    pub phase_failed: &'static str,
    pub files_seen: &'static str,
    pub files_indexed: &'static str,
    pub files_ignored: &'static str,
    pub errors: &'static str,
    pub archives: &'static str,
    pub archive_entries: &'static str,
    pub bytes_read: &'static str,

    // -- Build summary ----------------------------------------------------------
    pub last_build: &'static str,
    pub kind: &'static str,
    pub kind_full: &'static str,
    pub kind_update: &'static str,
    pub duration: &'static str,
    pub top_extensions: &'static str,
    /// Files rejected by a name rule (masks or known-binary extension).
    pub ignored_by_name: &'static str,
    pub ignored_by_sniff: &'static str,
    pub too_large: &'static str,
    pub security_limits: &'static str,
    pub archives_processed: &'static str,
    pub archive_entries_indexed: &'static str,
    pub delta_added: &'static str,
    pub delta_removed: &'static str,
    pub delta_updated: &'static str,

    // -- Project editor ------------------------------------------------------------
    pub new_project_title: &'static str,
    pub edit_project_title: &'static str,
    pub name: &'static str,
    pub project_name_hint: &'static str,
    pub add_root: &'static str,
    pub remove_root: &'static str,
    pub root_path_hint: &'static str,
    pub excluded_dirs_hint: &'static str,
    /// Hint of every mask field (project editor, search options,
    /// preferences): the syntax and the `*`/`?` wildcards.
    pub masks_hint: &'static str,
    pub err_name_required: &'static str,
    pub err_non_unicode_path: &'static str,
    /// The max-size field is not a positive whole number of MiB.
    pub err_max_size_invalid: &'static str,

    // -- Delete project confirmation -------------------------------------------------
    pub delete_project_title: &'static str,
    pub delete_warning: &'static str,
    /// Deletion is refused while a search tab still uses the project.
    pub project_in_use: &'static str,

    // -- Catalog ------------------------------------------------------------------------
    pub catalog_unavailable: &'static str,

    // -- Search screen -------------------------------------------------------------------
    pub search_project_label: &'static str,
    pub search_field_hint: &'static str,
    pub search_button: &'static str,
    /// "Enter at least {min} characters." ({min} = MIN_QUERY_CHARS)
    pub search_too_short_template: &'static str,
    pub options_section: &'static str,
    pub opt_case_sensitive: &'static str,
    pub opt_whole_word: &'static str,
    pub opt_analyze_oversized: &'static str,
    pub opt_context_lines: &'static str,
    pub results_section: &'static str,
    /// Accessible name of a file row's copy button.
    pub copy_path: &'static str,
    /// Accessible name of a file row's remove button — drops the
    /// file group from the displayed list only.
    pub remove_result: &'static str,
    /// Accessible names of the results-header buttons.
    pub expand_all: &'static str,
    pub collapse_all: &'static str,
    /// "Export results to clipboard" — accessible name.
    pub export_results: &'static str,
    /// 'Results for project "{name}"' — shown when the displayed
    /// results belong to a different project than the selected one.
    pub results_for_project_template: &'static str,
    /// "{matches} matches in {files} files"
    pub results_count_template: &'static str,
    pub no_results_hint: &'static str,
    pub empty_results_hint: &'static str,

    // -- Results view (filter, sort, detail line) -----------------------------
    /// Filter field placeholder: "Filter the results: *.asp".
    pub results_filter_placeholder: &'static str,
    /// Accessible name of the ✕ button in the filter field.
    pub results_filter_clear: &'static str,
    /// Accessible name of the filter field.
    pub results_filter_label: &'static str,
    /// Visible label before the sort combo.
    pub results_sort_label: &'static str,
    /// The descending checkbox's label.
    pub results_desc_label: &'static str,

    /// "{visible} of {total} files · {occ} occurrences" — the counter
    /// shown while a filter is active.
    pub results_filtered_counter_template: &'static str,
    /// "No file matches the filter ({hidden} hidden)".
    pub results_all_filtered_template: &'static str,
    /// The six sort combo entries, in [`SortKey::ALL`] order.
    pub sort_path: &'static str,
    pub sort_name: &'static str,
    pub sort_modified: &'static str,
    pub sort_occurrences: &'static str,
    pub sort_size: &'static str,
    pub sort_extension: &'static str,
    /// "{n} changed since indexing"
    pub skipped_changed_template: &'static str,
    /// "{n} failed at index time" — status-3 documents (undecodable
    /// or unreadable at build).
    pub skipped_index_errors_template: &'static str,
    /// "{n} blocked by security limits at index time" — status-4
    /// documents.
    pub skipped_security_limits_template: &'static str,
    /// "{n} unreadable"
    pub skipped_unreadable_template: &'static str,
    /// "{n} only partially searched"
    pub truncated_matches_template: &'static str,
    /// "{n} files over the size limit not analyzed"
    pub oversized_not_analyzed_template: &'static str,
    /// "Analyzing oversized files — {done}/{total}"
    pub oversized_progress_template: &'static str,
    /// "Search cancelled — results may be incomplete."
    pub results_cancelled: &'static str,

    // -- Saved searches --------------------------------------------------------------------
    pub saved_searches: &'static str,
    pub saved_combo_hint: &'static str,
    /// The Charger button — loads the selected saved search into the
    /// active tab.
    pub load: &'static str,
    /// Title of the save-search dialog.
    pub save_search_title: &'static str,
    /// The Dupliquer button of the save dialog — saves a copy.
    pub duplicate: &'static str,
    pub delete_saved_title: &'static str,
    /// Placeholder of the save dialog's name field.
    pub saved_name_hint: &'static str,

    // -- Search tabs -------------------------------------------------------------------------
    /// Accessible name of the "+" button opening a fresh tab. The
    /// default tab title itself is `nav_search`.
    pub new_tab: &'static str,
    /// Accessible name of a tab's close button.
    pub close_tab: &'static str,
    /// Title of the tab-rename dialog.
    pub rename_tab_title: &'static str,
    /// Placeholder of the tab-rename name field.
    pub tab_name_hint: &'static str,
    /// "Use automatic name" — drops a tab's custom title.
    pub reset_tab_name: &'static str,

    // -- Context banners --------------------------------------------------------------------
    pub banner_no_project: &'static str,
    pub banner_never_built: &'static str,
    pub banner_needs_rebuild: &'static str,
    /// 'Indexing "{name}"…'
    pub banner_building_template: &'static str,
    /// 'Searching for "{query}"…'
    pub banner_searching_template: &'static str,

    // -- Preferences ------------------------------------------------------------------------
    pub prefs_language: &'static str,
    pub prefs_theme: &'static str,
    pub theme_system: &'static str,
    pub theme_light: &'static str,
    pub theme_dark: &'static str,
    pub prefs_defaults_section: &'static str,
    pub prefs_default_excluded_dirs: &'static str,
    pub prefs_default_include_masks: &'static str,
    pub prefs_default_exclude_masks: &'static str,
    pub prefs_default_max_size: &'static str,
    pub prefs_defaults_note: &'static str,
    pub prefs_updates_section: &'static str,
    pub prefs_check_updates: &'static str,
    pub prefs_check_now: &'static str,
    pub prefs_autosave_note: &'static str,
    /// Shown when the update check finds no configured update source.
    pub update_not_configured: &'static str,

    // -- Internal file viewer ---------------------------------------------------------------
    /// Shown while the viewer reads a file.
    pub viewer_loading: &'static str,
    /// "Could not load the file: {message}"
    pub viewer_error_template: &'static str,
    /// Result rows inside archives have no preview.
    pub viewer_archive_unavailable: &'static str,
    /// Shown when only the head of a large file is displayed.
    pub viewer_truncated: &'static str,
    /// Viewer toolbar: open the displayed file with its Windows
    /// file association.
    pub viewer_open_external: &'static str,
    /// Viewer toolbar: copy the lines currently visible on screen.
    pub viewer_copy_visible: &'static str,
    /// Windows reports no application associated with the file type.
    pub viewer_no_associated_app: &'static str,
    /// The displayed file no longer exists on disk.
    pub viewer_file_missing: &'static str,
    /// "Windows refused to open the file (code {code})."
    pub viewer_open_failed_template: &'static str,

    // -- Messages (templates; use the interpolation methods) -----------------------------------
    /// 'Delete project "{name}"?'
    pub delete_confirm_template: &'static str,
    /// 'Project "{name}" created.'
    pub project_created_template: &'static str,
    pub project_updated: &'static str,
    /// 'Project "{name}" deleted.'
    pub project_deleted_template: &'static str,
    /// "Build completed in {secs} s — {files} files indexed."
    pub build_completed_template: &'static str,
    pub build_cancelled: &'static str,
    /// "Build failed: {message}"
    pub build_failed_template: &'static str,
    /// "Build finished in {secs} s but indexed 0 files — check the
    /// source roots and the max indexed file size." Sticky warning
    /// replacing the success notice when a full build indexes nothing.
    pub build_zero_files_template: &'static str,
    /// Shown when a build is started while another one is running.
    pub build_already_running: &'static str,
    /// 'Search "{name}" saved.'
    pub saved_created_template: &'static str,
    /// 'Saved search "{name}" updated.'
    pub saved_updated_template: &'static str,
    /// 'Saved search "{name}" deleted.'
    pub saved_deleted_template: &'static str,
    /// 'Delete saved search "{name}"?'
    pub delete_saved_confirm_template: &'static str,
    /// "Search completed — {matches} matches in {files} files ({secs} s)."
    pub search_done_template: &'static str,
    /// "Search failed: {message}"
    pub search_failed_template: &'static str,
    /// "Search cancelled."
    pub search_cancelled: &'static str,
    /// "Could not load preferences: {message}"
    pub prefs_load_failed_template: &'static str,
    /// "Could not save preferences: {message}"
    pub prefs_save_failed_template: &'static str,
    /// "rsearch {version} is available."
    pub update_available_template: &'static str,
    /// "rsearch is up to date."
    pub update_up_to_date: &'static str,

    // -- Build confirmation (before any build starts) ----------------------
    /// "The last build took {duration}." — duration reference of the
    /// confirmation dialog.
    pub confirm_build_last_duration_template: &'static str,
    /// Shown for a never-built project: no duration reference exists.
    pub confirm_build_unknown_duration: &'static str,
    /// Appended when the settings about to be used enable archives
    /// (D14: archives are the main cost driver, ~×7).
    pub confirm_build_archives: &'static str,

    // -- Post-build report -------------------------------------------------
    /// "{n} file error(s) during the build." — sticky notice part.
    pub build_file_errors_template: &'static str,
    /// "details of {n} more error(s) omitted." — appended when the
    /// report's detail list was capped.
    pub build_errors_omitted_template: &'static str,
    /// "{n} source root(s) skipped before the scan — duplicate of, or
    /// contained in, another root." (D11)
    pub build_skipped_roots_template: &'static str,
    /// Title of the Projects-screen section holding the last build
    /// report's details.
    pub build_report_section: &'static str,
    /// Label of the file-error count row.
    pub report_file_errors: &'static str,
    /// "+ {n} more error(s) not listed." — detail-list cap row.
    pub report_more_errors_template: &'static str,
    /// Label of the skipped-roots row.
    pub report_skipped_roots: &'static str,

    // -- Search screen scope (D15) ------------------------------------------
    /// Shown next to the project picker when archive indexing is
    /// disabled: archive contents are outside the candidate set.
    pub archives_excluded: &'static str,

    // -- Multi-project picker -------------------------------------------------
    /// The picker button's label when nothing is selected.
    pub pick_no_projects: &'static str,
    /// "{n} projects" — the picker button for a multi-project
    /// selection (a single selection shows the project's name).
    pub picker_projects_template: &'static str,
    /// Title of the multi-project picker dialog.
    pub pick_projects_title: &'static str,
    /// The picker's select-everything / clear actions.
    pub select_all: &'static str,
    pub deselect_all: &'static str,
    /// The picker dialog's confirm button.
    pub apply: &'static str,
    /// Shown inside the picker when the catalog holds no project.
    pub pick_empty_hint: &'static str,
    /// "project {pos}/{total}: {name}" — appended to the searching
    /// banner of a multi-project job.
    pub searching_project_template: &'static str,
    /// "Search completed — {matches} matches in {files} files across
    /// {projects} projects ({secs} s)." — the done banner of a
    /// multi-project job (mono keeps `search_done_template`).
    pub search_done_multi_template: &'static str,
    /// "Search completed for {ok} of {total} projects — {matches}
    /// matches in {files} files." — first line of a partial outcome:
    /// the file counts come from the deduplicated merged list.
    pub search_partial_template: &'static str,
    /// "{n} selected project(s) no longer exist — removed from the
    /// selection."
    pub missing_projects_template: &'static str,
    /// A loaded saved search whose whole selection vanished.
    pub saved_needs_projects: &'static str,
    /// "Results across {n} projects" — the results-header label when
    /// the displayed multi-project results no longer match the
    /// current selection.
    pub results_for_projects_template: &'static str,

    // -- Recoverable infrastructure failures ---------------------------------
    /// The search thread could not be spawned (banner, never a crash).
    pub search_thread_failed: &'static str,
    /// The viewer loader thread could not be spawned (overlay error).
    pub viewer_thread_failed: &'static str,
    /// A build was requested for a project that is no longer in the
    /// (fresh) projects cache.
    pub project_not_found: &'static str,
    /// Hint under the disabled update-check checkbox: no feed exists
    /// yet, the real mechanism is a later step.
    pub prefs_check_updates_soon: &'static str,
}

impl Strings {
    /// 'Delete project "{name}"?'
    pub fn delete_confirm(&self, name: &str) -> String {
        self.delete_confirm_template.replace("{name}", name)
    }

    /// 'Project "{name}" created.'
    pub fn project_created(&self, name: &str) -> String {
        self.project_created_template.replace("{name}", name)
    }

    /// 'Project "{name}" deleted.'
    pub fn project_deleted(&self, name: &str) -> String {
        self.project_deleted_template.replace("{name}", name)
    }

    /// "Build completed in {secs} s — {files} files indexed."
    pub fn build_completed(&self, files: usize, duration: Duration) -> String {
        self.build_completed_template
            .replace("{secs}", &format!("{:.1}", duration.as_secs_f64()))
            .replace("{files}", &files.to_string())
    }

    /// "Build failed: {message}"
    pub fn build_failed(&self, message: &str) -> String {
        self.build_failed_template.replace("{message}", message)
    }

    /// "Build finished in {secs} s but indexed 0 files — check the
    /// source roots and the max indexed file size."
    pub fn build_zero_files(&self, duration: Duration) -> String {
        self.build_zero_files_template
            .replace("{secs}", &format!("{:.1}", duration.as_secs_f64()))
    }

    /// "The last build took {duration}."
    pub fn confirm_build_last_duration(&self, duration: &str) -> String {
        self.confirm_build_last_duration_template
            .replace("{duration}", duration)
    }

    /// "{n} file error(s) during the build."
    pub fn build_file_errors(&self, n: usize) -> String {
        self.build_file_errors_template
            .replace("{n}", &n.to_string())
    }

    /// "details of {n} more error(s) omitted."
    pub fn build_errors_omitted(&self, n: usize) -> String {
        self.build_errors_omitted_template
            .replace("{n}", &n.to_string())
    }

    /// "{n} source root(s) skipped before the scan — duplicate of, or
    /// contained in, another root."
    pub fn build_skipped_roots(&self, n: usize) -> String {
        self.build_skipped_roots_template
            .replace("{n}", &n.to_string())
    }

    /// "+ {n} more error(s) not listed."
    pub fn report_more_errors(&self, n: usize) -> String {
        self.report_more_errors_template
            .replace("{n}", &n.to_string())
    }

    /// The localized name of a build phase (live progress display).
    pub fn phase_name(&self, phase: BuildPhase) -> &'static str {
        match phase {
            BuildPhase::Scanning => self.phase_scanning,
            BuildPhase::Processing => self.phase_processing,
            BuildPhase::Writing => self.phase_writing,
            BuildPhase::Finalizing => self.phase_finalizing,
            BuildPhase::Swapping => self.phase_swapping,
            BuildPhase::Completed => self.phase_completed,
            BuildPhase::Cancelled => self.phase_cancelled,
            BuildPhase::Failed => self.phase_failed,
        }
    }

    /// "Enter at least {min} characters."
    pub fn search_too_short(&self, min: usize) -> String {
        self.search_too_short_template
            .replace("{min}", &min.to_string())
    }

    /// 'Results for project "{name}"'
    pub fn results_for_project(&self, name: &str) -> String {
        self.results_for_project_template.replace("{name}", name)
    }

    /// "{matches} matches in {files} files"
    pub fn results_count(&self, matches: usize, files: usize) -> String {
        self.results_count_template
            .replace("{matches}", &matches.to_string())
            .replace("{files}", &files.to_string())
    }

    /// "{visible} of {total} files · {occ} occurrences"
    pub fn results_filtered_counter(&self, visible: usize, total: usize, occ: usize) -> String {
        self.results_filtered_counter_template
            .replace("{visible}", &visible.to_string())
            .replace("{total}", &total.to_string())
            .replace("{occ}", &occ.to_string())
    }

    /// "No file matches the filter ({hidden} hidden)"
    pub fn results_all_filtered(&self, hidden: usize) -> String {
        self.results_all_filtered_template
            .replace("{hidden}", &hidden.to_string())
    }

    /// "{n} changed since indexing"
    pub fn skipped_changed(&self, n: usize) -> String {
        self.skipped_changed_template.replace("{n}", &n.to_string())
    }

    /// "{n} failed at index time"
    pub fn skipped_index_errors(&self, n: usize) -> String {
        self.skipped_index_errors_template
            .replace("{n}", &n.to_string())
    }

    /// "{n} blocked by security limits at index time"
    pub fn skipped_security_limits(&self, n: usize) -> String {
        self.skipped_security_limits_template
            .replace("{n}", &n.to_string())
    }

    /// "{n} unreadable"
    pub fn skipped_unreadable(&self, n: usize) -> String {
        self.skipped_unreadable_template
            .replace("{n}", &n.to_string())
    }

    /// "{n} only partially searched"
    pub fn truncated_matches(&self, n: usize) -> String {
        self.truncated_matches_template
            .replace("{n}", &n.to_string())
    }

    /// "{n} files over the size limit not analyzed"
    pub fn oversized_not_analyzed(&self, n: usize) -> String {
        self.oversized_not_analyzed_template
            .replace("{n}", &n.to_string())
    }

    /// "Analyzing oversized files — {done}/{total}"
    pub fn oversized_progress(&self, done: usize, total: usize) -> String {
        self.oversized_progress_template
            .replace("{done}", &done.to_string())
            .replace("{total}", &total.to_string())
    }

    /// 'Indexing "{name}"…'
    pub fn banner_building(&self, name: &str) -> String {
        self.banner_building_template.replace("{name}", name)
    }

    /// 'Searching for "{query}"…'
    pub fn banner_searching(&self, query: &str) -> String {
        self.banner_searching_template.replace("{query}", query)
    }

    /// 'Search "{name}" saved.'
    pub fn saved_created(&self, name: &str) -> String {
        self.saved_created_template.replace("{name}", name)
    }

    /// 'Saved search "{name}" updated.'
    pub fn saved_updated(&self, name: &str) -> String {
        self.saved_updated_template.replace("{name}", name)
    }

    /// 'Saved search "{name}" deleted.'
    pub fn saved_deleted(&self, name: &str) -> String {
        self.saved_deleted_template.replace("{name}", name)
    }

    /// 'Delete saved search "{name}"?'
    pub fn delete_saved_confirm(&self, name: &str) -> String {
        self.delete_saved_confirm_template.replace("{name}", name)
    }

    /// "{n} projects" — the picker button's multi-selection label.
    pub fn picker_projects(&self, n: usize) -> String {
        self.picker_projects_template.replace("{n}", &n.to_string())
    }

    /// "project {pos}/{total}: {name}" — the running target inside
    /// the searching banner of a multi-project job.
    pub fn searching_project(&self, pos: usize, total: usize, name: &str) -> String {
        self.searching_project_template
            .replace("{pos}", &pos.to_string())
            .replace("{total}", &total.to_string())
            .replace("{name}", name)
    }

    /// "Search completed — {matches} matches in {files} files ({secs} s)."
    pub fn search_done(&self, matches: usize, files: usize, duration: Duration) -> String {
        self.search_done_template
            .replace("{matches}", &matches.to_string())
            .replace("{files}", &files.to_string())
            .replace("{secs}", &format!("{:.1}", duration.as_secs_f64()))
    }

    /// The done banner of a fully successful multi-project search —
    /// same counters as [`Strings::search_done`], plus the projects
    /// count.
    pub fn search_done_multi(
        &self,
        matches: usize,
        files: usize,
        projects: usize,
        duration: Duration,
    ) -> String {
        self.search_done_multi_template
            .replace("{matches}", &matches.to_string())
            .replace("{files}", &files.to_string())
            .replace("{projects}", &projects.to_string())
            .replace("{secs}", &format!("{:.1}", duration.as_secs_f64()))
    }

    /// First line of a partial outcome: "{ok} of {total} projects —
    /// {matches} matches in {files} files." The file counts are the
    /// unique merged list's, never a sum of per-project reports.
    pub fn search_partial(&self, ok: usize, total: usize, matches: usize, files: usize) -> String {
        self.search_partial_template
            .replace("{ok}", &ok.to_string())
            .replace("{total}", &total.to_string())
            .replace("{matches}", &matches.to_string())
            .replace("{files}", &files.to_string())
    }

    /// "{n} selected project(s) no longer exist — removed from the
    /// selection."
    pub fn missing_projects(&self, n: usize) -> String {
        self.missing_projects_template
            .replace("{n}", &n.to_string())
    }

    /// "Results across {n} projects"
    pub fn results_for_projects(&self, n: usize) -> String {
        self.results_for_projects_template
            .replace("{n}", &n.to_string())
    }

    /// "Search failed: {message}"
    pub fn search_failed(&self, message: &str) -> String {
        self.search_failed_template.replace("{message}", message)
    }

    /// "Could not load preferences: {message}"
    pub fn prefs_load_failed(&self, message: &str) -> String {
        self.prefs_load_failed_template
            .replace("{message}", message)
    }

    /// "Could not save preferences: {message}"
    pub fn prefs_save_failed(&self, message: &str) -> String {
        self.prefs_save_failed_template
            .replace("{message}", message)
    }

    /// "rsearch {version} is available."
    pub fn update_available(&self, version: &str) -> String {
        self.update_available_template.replace("{version}", version)
    }

    /// "Could not load the file: {message}"
    pub fn viewer_error(&self, message: &str) -> String {
        self.viewer_error_template.replace("{message}", message)
    }

    /// "Windows refused to open the file (code {code})."
    pub fn viewer_open_failed(&self, code: &str) -> String {
        self.viewer_open_failed_template.replace("{code}", code)
    }
}

/// The text table for a [`Language`]; English is the default.
pub fn for_language(language: Language) -> &'static Strings {
    match language {
        Language::English => &EN,
        Language::French => &FR,
        Language::Spanish => &ES,
    }
}

/// English text table — the default and reference language.
pub static EN: Strings = Strings {
    app_title: "rsearch",

    nav_search: "Search",
    nav_projects: "Projects",
    nav_preferences: "Preferences",

    save: "Save",
    cancel: "Cancel",
    delete: "Delete",
    rename: "Rename",
    retry: "Retry",
    yes: "yes",
    no: "no",
    browse: "Browse…",
    edit: "Edit…",
    open_projects: "Open Projects",
    dismiss: "Dismiss",

    projects: "Projects",
    new_project: "+ New project",
    no_projects_hint: "No projects yet.\nCreate one to start indexing.",
    select_project_hint: "Select a project.",

    status_never_built: "Never built",
    status_rebuild_needed: "Rebuild needed",
    status_up_to_date: "Up to date",

    build_index: "Build index",
    update_index: "Update index",
    settings_section: "Settings",
    source_roots: "Source roots",
    root_recursive: "recursive",
    root_top_level_only: "top level only",
    excluded_dirs: "Excluded directories",
    include_masks: "Include masks",
    exclude_masks: "Exclude masks",
    respect_gitignore: "Respect .gitignore files",
    max_indexed_file_size: "Max indexed file size",
    fallback_encoding: "Fallback encoding",
    fallback_none: "None",
    index_archives: "Index archive contents (.zip, .jar, …)",
    archive_max_depth: "Archive nesting depth",

    cancel_build: "Cancel build",
    starting: "Starting…",
    phase_scanning: "Scanning",
    phase_processing: "Processing",
    phase_writing: "Writing",
    phase_finalizing: "Finalizing",
    phase_swapping: "Swapping",
    phase_completed: "Completed",
    phase_cancelled: "Cancelled",
    phase_failed: "Failed",
    files_seen: "Files seen",
    files_indexed: "Files indexed",
    files_ignored: "Files ignored",
    errors: "Errors",
    archives: "Archives",
    archive_entries: "Archive entries",
    bytes_read: "Bytes read",

    last_build: "Last build",
    kind: "Kind",
    kind_full: "Full rebuild",
    kind_update: "Incremental update",
    duration: "Duration",
    top_extensions: "Top extensions",
    ignored_by_name: "Ignored by name",
    ignored_by_sniff: "Ignored by sniffing",
    too_large: "Too large",
    security_limits: "Security limits",
    archives_processed: "Archives processed",
    archive_entries_indexed: "Archive entries indexed",
    delta_added: "added",
    delta_removed: "removed",
    delta_updated: "updated",

    new_project_title: "New project",
    edit_project_title: "Edit project",
    name: "Name",
    project_name_hint: "Project name",
    add_root: "+ Add root",
    remove_root: "Remove this root",
    root_path_hint: "Directory path, e.g. C:\\src\\my-project",
    excluded_dirs_hint: "One per line, or separated by commas",
    masks_hint: "e.g. *.java;Test* — * and ? wildcards, on the file name",
    err_name_required: "A name is required.",
    err_non_unicode_path: "The selected path is not valid Unicode and cannot be used.",
    err_max_size_invalid: "Max indexed file size must be a whole number of MiB greater than zero.",

    delete_project_title: "Delete project",
    delete_warning: "Its index files will be removed from disk. This cannot be undone.",
    project_in_use: "This project is used by an open search tab — close that tab first.",

    catalog_unavailable: "The project catalog could not be opened",

    search_project_label: "Project",
    search_field_hint: "Text to search for…",
    search_button: "Search",
    search_too_short_template: "Enter at least {min} characters.",
    options_section: "Options",
    opt_case_sensitive: "Case sensitive",
    opt_whole_word: "Whole word",
    opt_analyze_oversized: "Analyze files over the size limit",
    opt_context_lines: "Context lines",
    results_section: "Results",
    copy_path: "Copy path",
    remove_result: "Remove this file from the list",
    expand_all: "Expand all",
    collapse_all: "Collapse all",
    export_results: "Export results to the clipboard",
    results_for_project_template: "Results for project \"{name}\"",
    results_count_template: "{matches} matches in {files} files",
    no_results_hint: "No matches found.",
    empty_results_hint: "Results will appear here.",

    results_filter_placeholder: "Filter the results: *.asp",
    results_filter_clear: "Clear the filter",
    results_filter_label: "Filter the results by file-name mask",
    results_sort_label: "Sort",
    results_desc_label: "Desc.",
    results_filtered_counter_template: "{visible} of {total} files · {occ} occurrences",
    results_all_filtered_template: "No file matches the filter ({hidden} hidden)",
    sort_path: "Path",
    sort_name: "Name",
    sort_modified: "Modified",
    sort_occurrences: "Occurrences",
    sort_size: "Size",
    sort_extension: "Extension",
    skipped_changed_template: "{n} changed since indexing",
    skipped_index_errors_template: "{n} failed at index time",
    skipped_security_limits_template: "{n} blocked by security limits at index time",
    skipped_unreadable_template: "{n} unreadable",
    truncated_matches_template: "{n} only partially searched",
    oversized_not_analyzed_template: "{n} files over the size limit not analyzed",
    oversized_progress_template: "Analyzing oversized files — {done}/{total}",
    results_cancelled: "Search cancelled — results may be incomplete.",

    saved_searches: "Saved searches",
    saved_combo_hint: "Select a saved search…",
    load: "Load",
    save_search_title: "Save search",
    duplicate: "Duplicate",
    delete_saved_title: "Delete saved search",
    saved_name_hint: "Search name",

    new_tab: "New search tab",
    close_tab: "Close tab",
    rename_tab_title: "Rename tab",
    tab_name_hint: "New name",
    reset_tab_name: "Use automatic name",

    banner_no_project: "Select a project to start searching.",
    banner_never_built: "This project has no index yet — build it to enable search.",
    banner_needs_rebuild: "Project settings changed since the last build.",
    banner_building_template: "Indexing \"{name}\"…",
    banner_searching_template: "Searching for \"{query}\"…",

    prefs_language: "Language",
    prefs_theme: "Theme",
    theme_system: "System",
    theme_light: "Light",
    theme_dark: "Dark",
    prefs_defaults_section: "Defaults for new projects",
    prefs_default_excluded_dirs: "Default excluded directories",
    prefs_default_include_masks: "Default include masks",
    prefs_default_exclude_masks: "Default exclude masks",
    prefs_default_max_size: "Default maximum file size",
    prefs_defaults_note:
        "These defaults apply when a project is created; existing projects keep their own settings.",
    prefs_updates_section: "Updates",
    prefs_check_updates: "Automatically check for updates",
    prefs_check_now: "Check now",
    prefs_autosave_note: "Changes are saved automatically.",
    update_not_configured:
        "Update checking is not configured for this build — no update source is defined yet.",

    viewer_loading: "Loading file…",
    viewer_error_template: "Could not load the file: {message}",
    viewer_archive_unavailable: "Preview is not available for archive entries.",
    viewer_truncated: "File is large — showing the beginning only.",
    viewer_open_external: "Open with associated application",
    viewer_copy_visible: "Copy the visible lines",
    viewer_no_associated_app: "No application is associated with this file type.",
    viewer_file_missing: "The file no longer exists.",
    viewer_open_failed_template: "Windows refused to open the file (code {code}).",

    delete_confirm_template: "Delete project \"{name}\"?",
    project_created_template: "Project \"{name}\" created.",
    project_updated: "Project updated.",
    project_deleted_template: "Project \"{name}\" deleted.",
    build_completed_template: "Build completed in {secs} s — {files} files indexed.",
    build_cancelled: "Build cancelled. The previous index is unchanged.",
    build_failed_template: "Build failed: {message}",
    build_zero_files_template:
        "Build finished in {secs} s but indexed 0 files — check the source roots and the max indexed file size.",
    build_already_running: "A build is already running — wait for it to finish or cancel it first.",
    saved_created_template: "Search \"{name}\" saved.",
    saved_updated_template: "Saved search \"{name}\" updated.",
    saved_deleted_template: "Saved search \"{name}\" deleted.",
    delete_saved_confirm_template: "Delete saved search \"{name}\"?",
    search_done_template: "Search completed — {matches} matches in {files} files ({secs} s).",
    search_failed_template: "Search failed: {message}",
    search_cancelled: "Search cancelled.",
    prefs_load_failed_template: "Could not load preferences: {message}",
    prefs_save_failed_template: "Could not save preferences: {message}",
    update_available_template: "rsearch {version} is available.",
    update_up_to_date: "rsearch is up to date.",
    confirm_build_last_duration_template: "The last build took {duration}.",
    confirm_build_unknown_duration:
        "Duration unknown — it depends on the volume of files to index.",
    confirm_build_archives:
        "Archive indexing is enabled: expect a build roughly 7× longer (measured on a real corpus).",
    build_file_errors_template: "{n} file error(s) during the build.",
    build_errors_omitted_template: "details of {n} more error(s) omitted.",
    build_skipped_roots_template:
        "{n} source root(s) skipped before the scan — duplicate of, or contained in, another root.",
    build_report_section: "Build report",
    report_file_errors: "File errors",
    report_more_errors_template: "+ {n} more error(s) not listed.",
    report_skipped_roots: "Skipped source roots",
    archives_excluded: "archives excluded",
    pick_no_projects: "Select projects…",
    picker_projects_template: "{n} projects",
    pick_projects_title: "Projects to search",
    select_all: "Select all",
    deselect_all: "Select none",
    apply: "Apply",
    pick_empty_hint: "No projects exist yet — create one on the Projects screen.",
    searching_project_template: "project {pos}/{total}: {name}",
    search_done_multi_template:
        "Search completed — {matches} matches in {files} files across {projects} projects ({secs} s).",
    search_partial_template:
        "Search completed for {ok} of {total} projects — {matches} matches in {files} files.",
    missing_projects_template:
        "{n} selected project(s) no longer exist — removed from the selection.",
    saved_needs_projects:
        "The projects of this saved search no longer exist — select new ones to run it.",
    results_for_projects_template: "Results across {n} projects",
    search_thread_failed: "The search could not start — the system refused a new thread.",
    viewer_thread_failed: "The file could not be loaded — the system refused a new thread.",
    project_not_found: "This project no longer exists.",
    prefs_check_updates_soon: "Coming soon — no update feed is configured for this build yet.",
};

/// French text table.
pub static FR: Strings = Strings {
    app_title: "rsearch",

    nav_search: "Recherche",
    nav_projects: "Projets",
    nav_preferences: "Préférences",

    save: "Enregistrer",
    cancel: "Annuler",
    delete: "Supprimer",
    rename: "Renommer",
    retry: "Réessayer",
    yes: "oui",
    no: "non",
    browse: "Parcourir…",
    edit: "Modifier…",
    open_projects: "Ouvrir les projets",
    dismiss: "Ignorer",

    projects: "Projets",
    new_project: "+ Nouveau projet",
    no_projects_hint: "Aucun projet pour le moment.\nCréez-en un pour commencer l'indexation.",
    select_project_hint: "Sélectionnez un projet.",

    status_never_built: "Jamais construit",
    status_rebuild_needed: "Reconstruction nécessaire",
    status_up_to_date: "À jour",

    build_index: "Construire l'index",
    update_index: "Mettre à jour l'index",
    settings_section: "Paramètres",
    source_roots: "Racines sources",
    root_recursive: "récursif",
    root_top_level_only: "niveau supérieur uniquement",
    excluded_dirs: "Répertoires exclus",
    include_masks: "Masques à inclure",
    exclude_masks: "Masques à exclure",
    respect_gitignore: "Respecter les fichiers .gitignore",
    max_indexed_file_size: "Taille maximale d'un fichier indexé",
    fallback_encoding: "Encodage de repli",
    fallback_none: "Aucun",
    index_archives: "Indexer le contenu des archives (.zip, .jar, …)",
    archive_max_depth: "Profondeur d'imbrication des archives",

    cancel_build: "Annuler la construction",
    starting: "Démarrage…",
    phase_scanning: "Analyse",
    phase_processing: "Traitement",
    phase_writing: "Écriture",
    phase_finalizing: "Finalisation",
    phase_swapping: "Activation",
    phase_completed: "Terminé",
    phase_cancelled: "Annulé",
    phase_failed: "Échec",
    files_seen: "Fichiers vus",
    files_indexed: "Fichiers indexés",
    files_ignored: "Fichiers ignorés",
    errors: "Erreurs",
    archives: "Archives compressées",
    archive_entries: "Entrées d'archives",
    bytes_read: "Octets lus",

    last_build: "Dernière construction",
    kind: "Type",
    kind_full: "Reconstruction complète",
    kind_update: "Mise à jour incrémentale",
    duration: "Durée",
    top_extensions: "Principales extensions",
    ignored_by_name: "Ignorés (nom)",
    ignored_by_sniff: "Ignorés (analyse)",
    too_large: "Trop volumineux",
    security_limits: "Limites de sécurité",
    archives_processed: "Archives traitées",
    archive_entries_indexed: "Entrées d'archives indexées",
    delta_added: "ajoutés",
    delta_removed: "supprimés",
    delta_updated: "modifiés",

    new_project_title: "Nouveau projet",
    edit_project_title: "Modifier le projet",
    name: "Nom",
    project_name_hint: "Nom du projet",
    add_root: "+ Ajouter une racine",
    remove_root: "Supprimer cette racine",
    root_path_hint: "Chemin du répertoire, ex. C:\\src\\mon-projet",
    excluded_dirs_hint: "Un par ligne, ou séparés par des virgules",
    masks_hint: "ex. *.java;Test* — jokers * et ?, sur le nom du fichier",
    err_name_required: "Un nom est requis.",
    err_non_unicode_path: "Le chemin sélectionné n'est pas un Unicode valide et ne peut pas être utilisé.",
    err_max_size_invalid:
        "La taille max indexée doit être un nombre entier de MiB supérieur à zéro.",

    delete_project_title: "Supprimer le projet",
    delete_warning: "Ses fichiers d'index seront supprimés du disque. Cette action est irréversible.",
    project_in_use:
        "Ce projet est utilisé par un onglet de recherche ouvert — fermez d'abord cet onglet.",

    catalog_unavailable: "Le catalogue de projets n'a pas pu être ouvert",

    search_project_label: "Projet",
    search_field_hint: "Texte à rechercher…",
    search_button: "Rechercher",
    search_too_short_template: "Saisissez au moins {min} caractères.",
    options_section: "Options",
    opt_case_sensitive: "Respecter la casse",
    opt_whole_word: "Mot entier",
    opt_analyze_oversized: "Analyser les fichiers dépassant la limite",
    opt_context_lines: "Lignes de contexte",
    results_section: "Résultats",
    copy_path: "Copier le chemin",
    remove_result: "Retirer ce fichier de la liste",
    expand_all: "Tout déplier",
    collapse_all: "Tout replier",
    export_results: "Exporter les résultats dans le presse-papiers",
    results_for_project_template: "Résultats pour le projet \"{name}\"",
    results_count_template: "{matches} occurrences dans {files} fichiers",
    no_results_hint: "Aucune occurrence trouvée.",
    empty_results_hint: "Les résultats s'afficheront ici.",

    results_filter_placeholder: "Filtrer les résultats : *.asp",
    results_filter_clear: "Effacer le filtre",
    results_filter_label: "Filtrer les résultats par masque de nom de fichier",
    results_sort_label: "Tri",
    results_desc_label: "Desc.",
    results_filtered_counter_template: "{visible} sur {total} fichiers · {occ} occurrences",
    results_all_filtered_template: "Aucun fichier ne correspond au filtre ({hidden} masqués)",
    sort_path: "Chemin",
    sort_name: "Nom",
    sort_modified: "Date de modification",
    sort_occurrences: "Occurrences",
    sort_size: "Taille",
    sort_extension: "Extension",
    skipped_changed_template: "{n} modifiés depuis l'indexation",
    skipped_index_errors_template: "{n} en échec à l'indexation",
    skipped_security_limits_template: "{n} bloqués par les limites de sécurité à l'indexation",
    skipped_unreadable_template: "{n} illisibles",
    truncated_matches_template: "{n} partiellement analysés",
    oversized_not_analyzed_template: "{n} fichiers dépassant la limite non analysés",
    oversized_progress_template: "Analyse des fichiers volumineux — {done}/{total}",
    results_cancelled: "Recherche annulée — résultats incomplets.",

    saved_searches: "Recherches sauvegardées",
    saved_combo_hint: "Sélectionner une recherche…",
    load: "Charger",
    save_search_title: "Enregistrer la recherche",
    duplicate: "Dupliquer",
    delete_saved_title: "Supprimer la recherche sauvegardée",
    saved_name_hint: "Nom de la recherche",

    new_tab: "Nouvel onglet de recherche",
    close_tab: "Fermer l'onglet",
    rename_tab_title: "Renommer l'onglet",
    tab_name_hint: "Nouveau nom",
    reset_tab_name: "Nom automatique",

    banner_no_project: "Sélectionnez un projet pour commencer à rechercher.",
    banner_never_built: "Ce projet n'a pas encore d'index — construisez-le pour activer la recherche.",
    banner_needs_rebuild: "Les paramètres du projet ont changé depuis la dernière construction.",
    banner_building_template: "Indexation de \"{name}\"…",
    banner_searching_template: "Recherche de \"{query}\"…",

    prefs_language: "Langue",
    prefs_theme: "Thème",
    theme_system: "Système",
    theme_light: "Clair",
    theme_dark: "Sombre",
    prefs_defaults_section: "Valeurs par défaut des nouveaux projets",
    prefs_default_excluded_dirs: "Répertoires exclus par défaut",
    prefs_default_include_masks: "Masques à inclure par défaut",
    prefs_default_exclude_masks: "Masques à exclure par défaut",
    prefs_default_max_size: "Taille maximale de fichier par défaut",
    prefs_defaults_note: "Ces valeurs s'appliquent à la création d'un projet ; les projets existants conservent leurs propres paramètres.",
    prefs_updates_section: "Mises à jour",
    prefs_check_updates: "Rechercher automatiquement les mises à jour",
    prefs_check_now: "Vérifier maintenant",
    prefs_autosave_note: "Les modifications sont enregistrées automatiquement.",
    update_not_configured: "La recherche de mises à jour n'est pas encore configurée — aucune source de mise à jour n'est définie pour cette version.",

    viewer_loading: "Chargement du fichier…",
    viewer_error_template: "Impossible de charger le fichier : {message}",
    viewer_archive_unavailable: "L'aperçu n'est pas disponible pour les entrées d'archive.",
    viewer_truncated: "Fichier volumineux — affichage du début uniquement.",
    viewer_open_external: "Ouvrir dans l'application associée",
    viewer_copy_visible: "Copier les lignes affichées",
    viewer_no_associated_app: "Aucune application n'est associée à ce type de fichier.",
    viewer_file_missing: "Le fichier n'existe plus.",
    viewer_open_failed_template: "Windows a refusé d'ouvrir le fichier (code {code}).",

    delete_confirm_template: "Supprimer le projet \"{name}\" ?",
    project_created_template: "Projet \"{name}\" créé.",
    project_updated: "Projet mis à jour.",
    project_deleted_template: "Projet \"{name}\" supprimé.",
    build_completed_template: "Construction terminée en {secs} s — {files} fichiers indexés.",
    build_cancelled: "Construction annulée. L'index précédent est inchangé.",
    build_failed_template: "Échec de la construction : {message}",
    build_zero_files_template:
        "Construction terminée en {secs} s mais 0 fichier indexé — vérifie les racines sources et la taille max indexée.",
    build_already_running:
        "Une construction est déjà en cours — attends qu'elle se termine ou annule-la d'abord.",
    saved_created_template: "Recherche \"{name}\" sauvegardée.",
    saved_updated_template: "Recherche sauvegardée \"{name}\" mise à jour.",
    saved_deleted_template: "Recherche sauvegardée \"{name}\" supprimée.",
    delete_saved_confirm_template: "Supprimer la recherche sauvegardée \"{name}\" ?",
    search_done_template: "Recherche terminée — {matches} occurrences dans {files} fichiers ({secs} s).",
    search_failed_template: "Échec de la recherche : {message}",
    search_cancelled: "Recherche annulée.",
    prefs_load_failed_template: "Impossible de charger les préférences : {message}",
    prefs_save_failed_template: "Impossible d'enregistrer les préférences : {message}",
    update_available_template: "rsearch {version} est disponible.",
    update_up_to_date: "rsearch est à jour.",
    confirm_build_last_duration_template: "Le dernier build a duré {duration}.",
    confirm_build_unknown_duration:
        "Durée inconnue — elle dépend du volume de fichiers à indexer.",
    confirm_build_archives:
        "L'indexation des archives est activée : attends-toi à une construction environ 7× plus longue (mesuré sur un corpus réel).",
    build_file_errors_template: "{n} erreur(s) fichier pendant la construction.",
    build_errors_omitted_template: "détails de {n} autre(s) erreur(s) omis.",
    build_skipped_roots_template:
        "{n} racine(s) source écartée(s) avant le scan — doublon de, ou contenue dans, une autre racine.",
    build_report_section: "Rapport de build",
    report_file_errors: "Erreurs fichier",
    report_more_errors_template: "+ {n} autre(s) erreur(s) non listée(s).",
    report_skipped_roots: "Racines sources écartées",
    archives_excluded: "archives exclues",
    pick_no_projects: "Sélectionner des projets…",
    picker_projects_template: "{n} projets",
    pick_projects_title: "Projets à interroger",
    select_all: "Tout sélectionner",
    deselect_all: "Tout désélectionner",
    apply: "Appliquer",
    pick_empty_hint: "Aucun projet n'existe encore — créez-en un dans l'écran Projets.",
    searching_project_template: "projet {pos}/{total} : {name}",
    search_done_multi_template:
        "Recherche terminée — {matches} occurrences dans {files} fichiers sur {projects} projets ({secs} s).",
    search_partial_template:
        "Recherche terminée pour {ok} projet(s) sur {total} — {matches} occurrences dans {files} fichiers.",
    missing_projects_template:
        "{n} projet(s) sélectionné(s) n'existe(nt) plus — retiré(s) de la sélection.",
    saved_needs_projects:
        "Les projets de cette recherche sauvegardée n'existent plus — sélectionnez-en pour l'exécuter.",
    results_for_projects_template: "Résultats sur {n} projets",
    search_thread_failed:
        "La recherche n'a pas pu démarrer — le système a refusé un nouveau thread.",
    viewer_thread_failed:
        "Le fichier n'a pas pu être chargé — le système a refusé un nouveau thread.",
    project_not_found: "Ce projet n'existe plus.",
    prefs_check_updates_soon:
        "Bientôt disponible — aucun flux de mises à jour n'est configuré pour cette version.",
};

/// Spanish text table.
pub static ES: Strings = Strings {
    app_title: "rsearch",

    nav_search: "Búsqueda",
    nav_projects: "Proyectos",
    nav_preferences: "Preferencias",

    save: "Guardar",
    cancel: "Cancelar",
    delete: "Eliminar",
    rename: "Renombrar",
    retry: "Reintentar",
    yes: "sí",
    no: "no",
    browse: "Examinar…",
    edit: "Editar…",
    open_projects: "Abrir proyectos",
    dismiss: "Descartar",

    projects: "Proyectos",
    new_project: "+ Nuevo proyecto",
    no_projects_hint: "Aún no hay proyectos.\nCree uno para empezar a indexar.",
    select_project_hint: "Seleccione un proyecto.",

    status_never_built: "Nunca construido",
    status_rebuild_needed: "Reconstrucción necesaria",
    status_up_to_date: "Actualizado",

    build_index: "Construir índice",
    update_index: "Actualizar índice",
    settings_section: "Configuración",
    source_roots: "Raíces de origen",
    root_recursive: "recursivo",
    root_top_level_only: "solo nivel superior",
    excluded_dirs: "Directorios excluidos",
    include_masks: "Máscaras a incluir",
    exclude_masks: "Máscaras a excluir",
    respect_gitignore: "Respetar archivos .gitignore",
    max_indexed_file_size: "Tamaño máximo de archivo indexado",
    fallback_encoding: "Codificación de respaldo",
    fallback_none: "Ninguno",
    index_archives: "Indexar el contenido de archivos comprimidos (.zip, .jar, …)",
    archive_max_depth: "Profundidad de anidación de archivos comprimidos",

    cancel_build: "Cancelar construcción",
    starting: "Iniciando…",
    phase_scanning: "Explorando",
    phase_processing: "Procesando",
    phase_writing: "Escribiendo",
    phase_finalizing: "Finalizando",
    phase_swapping: "Activando",
    phase_completed: "Completado",
    phase_cancelled: "Cancelado",
    phase_failed: "Fallido",
    files_seen: "Archivos vistos",
    files_indexed: "Archivos indexados",
    files_ignored: "Archivos ignorados",
    errors: "Errores",
    archives: "Archivos comprimidos",
    archive_entries: "Entradas de archivos comprimidos",
    bytes_read: "Bytes leídos",

    last_build: "Última construcción",
    kind: "Tipo",
    kind_full: "Reconstrucción completa",
    kind_update: "Actualización incremental",
    duration: "Duración",
    top_extensions: "Extensiones principales",
    ignored_by_name: "Ignorados por nombre",
    ignored_by_sniff: "Ignorados por análisis",
    too_large: "Demasiado grandes",
    security_limits: "Límites de seguridad",
    archives_processed: "Archivos comprimidos procesados",
    archive_entries_indexed: "Entradas indexadas en archivos comprimidos",
    delta_added: "añadidos",
    delta_removed: "eliminados",
    delta_updated: "modificados",

    new_project_title: "Nuevo proyecto",
    edit_project_title: "Editar proyecto",
    name: "Nombre",
    project_name_hint: "Nombre del proyecto",
    add_root: "+ Añadir raíz",
    remove_root: "Quitar esta raíz",
    root_path_hint: "Ruta del directorio, p. ej. C:\\src\\mi-proyecto",
    excluded_dirs_hint: "Uno por línea, o separados por comas",
    masks_hint: "p. ej. *.java;Test* — comodines * y ?, sobre el nombre de archivo",
    err_name_required: "Se requiere un nombre.",
    err_non_unicode_path: "La ruta seleccionada no es Unicode válido y no se puede usar.",
    err_max_size_invalid:
        "El tamaño máximo indexado debe ser un número entero de MiB mayor que cero.",

    delete_project_title: "Eliminar proyecto",
    delete_warning: "Sus archivos de índice se eliminarán del disco. Esta acción no se puede deshacer.",
    project_in_use:
        "Este proyecto lo está usando una pestaña de búsqueda abierta — ciérrela primero.",

    catalog_unavailable: "No se pudo abrir el catálogo de proyectos",

    search_project_label: "Proyecto",
    search_field_hint: "Texto a buscar…",
    search_button: "Buscar",
    search_too_short_template: "Introduzca al menos {min} caracteres.",
    options_section: "Opciones",
    opt_case_sensitive: "Distinguir mayúsculas",
    opt_whole_word: "Palabra completa",
    opt_analyze_oversized: "Analizar archivos que superan el límite",
    opt_context_lines: "Líneas de contexto",
    results_section: "Resultados",
    copy_path: "Copiar la ruta",
    remove_result: "Quitar este archivo de la lista",
    expand_all: "Expandir todo",
    collapse_all: "Contraer todo",
    export_results: "Exportar los resultados al portapapeles",
    results_for_project_template: "Resultados del proyecto \"{name}\"",
    results_count_template: "{matches} coincidencias en {files} archivos",
    no_results_hint: "No se encontraron coincidencias.",
    empty_results_hint: "Los resultados aparecerán aquí.",

    results_filter_placeholder: "Filtrar los resultados: *.asp",
    results_filter_clear: "Borrar el filtro",
    results_filter_label: "Filtrar los resultados por máscara de nombre de archivo",
    results_sort_label: "Orden",
    results_desc_label: "Desc.",
    results_filtered_counter_template: "{visible} de {total} archivos · {occ} apariciones",
    results_all_filtered_template: "Ningún archivo corresponde al filtro ({hidden} ocultos)",
    sort_path: "Ruta",
    sort_name: "Nombre",
    sort_modified: "Fecha de modificación",
    sort_occurrences: "Apariciones",
    sort_size: "Tamaño",
    sort_extension: "Extensión",
    skipped_changed_template: "{n} modificados desde la indexación",
    skipped_index_errors_template: "{n} con error al indexar",
    skipped_security_limits_template: "{n} bloqueados por límites de seguridad al indexar",
    skipped_unreadable_template: "{n} ilegibles",
    truncated_matches_template: "{n} analizados parcialmente",
    oversized_not_analyzed_template: "{n} archivos que superan el límite sin analizar",
    oversized_progress_template: "Analizando archivos grandes — {done}/{total}",
    results_cancelled: "Búsqueda cancelada — resultados incompletos.",

    saved_searches: "Búsquedas guardadas",
    saved_combo_hint: "Seleccionar una búsqueda…",
    load: "Cargar",
    save_search_title: "Guardar búsqueda",
    duplicate: "Duplicar",
    delete_saved_title: "Eliminar búsqueda guardada",
    saved_name_hint: "Nombre de la búsqueda",

    new_tab: "Nueva pestaña de búsqueda",
    close_tab: "Cerrar pestaña",
    rename_tab_title: "Renombrar la pestaña",
    tab_name_hint: "Nuevo nombre",
    reset_tab_name: "Nombre automático",

    banner_no_project: "Seleccione un proyecto para empezar a buscar.",
    banner_never_built: "Este proyecto aún no tiene índice — constrúyalo para habilitar la búsqueda.",
    banner_needs_rebuild: "La configuración del proyecto cambió desde la última construcción.",
    banner_building_template: "Indexando \"{name}\"…",
    banner_searching_template: "Buscando \"{query}\"…",

    prefs_language: "Idioma",
    prefs_theme: "Tema",
    theme_system: "Sistema",
    theme_light: "Claro",
    theme_dark: "Oscuro",
    prefs_defaults_section: "Valores predeterminados para proyectos nuevos",
    prefs_default_excluded_dirs: "Directorios excluidos por defecto",
    prefs_default_include_masks: "Máscaras a incluir por defecto",
    prefs_default_exclude_masks: "Máscaras a excluir por defecto",
    prefs_default_max_size: "Tamaño máximo de archivo por defecto",
    prefs_defaults_note: "Estos valores se aplican al crear un proyecto; los proyectos existentes conservan su propia configuración.",
    prefs_updates_section: "Actualizaciones",
    prefs_check_updates: "Buscar actualizaciones automáticamente",
    prefs_check_now: "Buscar ahora",
    prefs_autosave_note: "Los cambios se guardan automáticamente.",
    update_not_configured: "La comprobación de actualizaciones aún no está configurada — no hay ninguna fuente de actualización definida para esta compilación.",

    viewer_loading: "Cargando archivo…",
    viewer_error_template: "No se pudo cargar el archivo: {message}",
    viewer_archive_unavailable: "La vista previa no está disponible para entradas de archivos comprimidos.",
    viewer_truncated: "Archivo grande — se muestra solo el comienzo.",
    viewer_open_external: "Abrir con la aplicación asociada",
    viewer_copy_visible: "Copiar las líneas visibles",
    viewer_no_associated_app: "No hay ninguna aplicación asociada a este tipo de archivo.",
    viewer_file_missing: "El archivo ya no existe.",
    viewer_open_failed_template: "Windows rechazó abrir el archivo (código {code}).",

    delete_confirm_template: "¿Eliminar el proyecto \"{name}\"?",
    project_created_template: "Proyecto \"{name}\" creado.",
    project_updated: "Proyecto actualizado.",
    project_deleted_template: "Proyecto \"{name}\" eliminado.",
    build_completed_template: "Construcción terminada en {secs} s — {files} archivos indexados.",
    build_cancelled: "Construcción cancelada. El índice anterior no se modificó.",
    build_failed_template: "Error en la construcción: {message}",
    build_zero_files_template:
        "Construcción terminada en {secs} s pero 0 archivos indexados — comprueba las raíces de origen y el tamaño máximo indexado.",
    build_already_running:
        "Ya hay una construcción en curso — espera a que termine o cancélala primero.",
    saved_created_template: "Búsqueda \"{name}\" guardada.",
    saved_updated_template: "Búsqueda guardada \"{name}\" actualizada.",
    saved_deleted_template: "Búsqueda guardada \"{name}\" eliminada.",
    delete_saved_confirm_template: "¿Eliminar la búsqueda guardada \"{name}\"?",
    search_done_template: "Búsqueda terminada — {matches} coincidencias en {files} archivos ({secs} s).",
    search_failed_template: "Error en la búsqueda: {message}",
    search_cancelled: "Búsqueda cancelada.",
    prefs_load_failed_template: "No se pudieron cargar las preferencias: {message}",
    prefs_save_failed_template: "No se pudieron guardar las preferencias: {message}",
    update_available_template: "rsearch {version} está disponible.",
    update_up_to_date: "rsearch está actualizado.",
    confirm_build_last_duration_template: "La última construcción duró {duration}.",
    confirm_build_unknown_duration:
        "Duración desconocida — depende del volumen de archivos que haya que indexar.",
    confirm_build_archives:
        "La indexación de archivos está activada: espera una construcción unas 7 veces más larga (medido en un corpus real).",
    build_file_errors_template: "{n} error(es) de archivo durante la construcción.",
    build_errors_omitted_template: "detalles de {n} error(es) más omitidos.",
    build_skipped_roots_template:
        "{n} raíz(es) de origen descartada(s) antes del escaneo — duplicado de, o contenida en, otra raíz.",
    build_report_section: "Informe de construcción",
    report_file_errors: "Errores de archivo",
    report_more_errors_template: "+ {n} error(es) más no listado(s).",
    report_skipped_roots: "Raíces de origen descartadas",
    archives_excluded: "archivos excluidos",
    pick_no_projects: "Seleccionar proyectos…",
    picker_projects_template: "{n} proyectos",
    pick_projects_title: "Proyectos en los que buscar",
    select_all: "Seleccionar todo",
    deselect_all: "Deseleccionar todo",
    apply: "Aplicar",
    pick_empty_hint:
        "Todavía no existe ningún proyecto — cree uno en la pantalla Proyectos.",
    searching_project_template: "proyecto {pos}/{total}: {name}",
    search_done_multi_template:
        "Búsqueda terminada — {matches} coincidencias en {files} archivos en {projects} proyectos ({secs} s).",
    search_partial_template:
        "Búsqueda terminada en {ok} de {total} proyectos — {matches} coincidencias en {files} archivos.",
    missing_projects_template:
        "{n} proyecto(s) seleccionado(s) ya no existe(n) — retirado(s) de la selección.",
    saved_needs_projects:
        "Los proyectos de esta búsqueda guardada ya no existen — seleccione otros para ejecutarla.",
    results_for_projects_template: "Resultados en {n} proyectos",
    search_thread_failed: "La búsqueda no pudo iniciarse — el sistema rechazó un nuevo hilo.",
    viewer_thread_failed: "El archivo no pudo cargarse — el sistema rechazó un nuevo hilo.",
    project_not_found: "Este proyecto ya no existe.",
    prefs_check_updates_soon:
        "Próximamente — todavía no hay un canal de actualizaciones configurado para esta versión.",
};
