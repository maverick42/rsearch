//! Slint wiring: the generated bindings plus the controller that
//! connects the `AppState` global to [`App`].
//!
//! Every widget callback lands here. The pattern is uniform: mutate
//! `App`, then `sync_all` pushes the new state back into the
//! properties — the same one-way data flow the previous UI had,
//! without widgets owning logic. Engine work never runs in these
//! handlers: builds and searches live on their own threads and a
//! short timer ([`TICK`]) polls their progress.

slint::include_modules!();

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use rsearch_catalog::{Language, ThemePreference};
use rsearch_engine::{BuildKind, BuildReport};
use slint::{ModelRc, SharedString, Timer, TimerMode, VecModel};

use crate::app::{dialog_kind, App, Dialog, Screen};
use crate::editor::{EditorValues, RootEdit};
use crate::tr::Strings;
use crate::util;

/// How often engine progress is polled, in milliseconds. The UI
/// thread itself never blocks; the timer only drains channels.
const TICK: Duration = Duration::from_millis(100);

/// Application entry point: builds the window, wires the callbacks
/// and runs the event loop until the window closes.
pub fn run() -> Result<(), slint::PlatformError> {
    let app = Rc::new(RefCell::new(App::new()));
    let ui = AppWindow::new()?;
    let st = ui.global::<AppState>();

    st.set_app_version(env!("CARGO_PKG_VERSION").into());
    st.set_language_names(string_model(Language::ALL.iter().map(|l| l.native_name())));
    push_texts(&ui, &app.borrow());
    sync_prefs(&ui, &app.borrow());
    ui.invoke_apply_theme(theme_mode(app.borrow().prefs.theme));

    wire(&ui, &app);

    {
        let a = app.borrow();
        st.set_results(a.tab().results.clone().into());
        st.set_viewer_lines(a.tab().viewer_lines.model());
        push_search_form(&ui, &a);
        sync_all(&ui, &a);
    }
    ui.invoke_focus_search();

    let tick = Timer::default();
    {
        let weak = ui.as_weak();
        let app = app.clone();
        tick.start(TimerMode::Repeated, TICK, move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            if app.borrow_mut().tick() {
                sync_all(&ui, &app.borrow());
            }
        });
    }

    let result = ui.run();
    drop(tick);
    result
}

/// The `apply-theme` argument for a theme preference.
fn theme_mode(theme: ThemePreference) -> i32 {
    match theme {
        ThemePreference::Light => 1,
        ThemePreference::Dark => 2,
        ThemePreference::System => 0,
    }
}

fn string_model(items: impl Iterator<Item = &'static str>) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(
        items.map(SharedString::from).collect::<Vec<_>>(),
    ))
}

fn kv_model(rows: Vec<(String, String)>) -> ModelRc<KvRow> {
    ModelRc::new(VecModel::from(
        rows.into_iter()
            .map(|(label, value)| KvRow {
                label: label.into(),
                value: value.into(),
            })
            .collect::<Vec<_>>(),
    ))
}

/// Installs the active text table and the localized picker models.
fn push_texts(ui: &AppWindow, app: &App) {
    let st = ui.global::<AppState>();
    st.set_tr(tr_strings(app.tr));
    st.set_theme_names(string_model(
        [app.tr.theme_system, app.tr.theme_light, app.tr.theme_dark].into_iter(),
    ));
}

/// The whole `TrStrings` table for the active language.
fn tr_strings(tr: &Strings) -> TrStrings {
    TrStrings {
        app_title: tr.app_title.into(),
        nav_search: tr.nav_search.into(),
        nav_projects: tr.nav_projects.into(),
        nav_preferences: tr.nav_preferences.into(),
        save: tr.save.into(),
        cancel: tr.cancel.into(),
        delete: tr.delete.into(),
        rename: tr.rename.into(),
        retry: tr.retry.into(),
        yes: tr.yes.into(),
        no: tr.no.into(),
        browse: tr.browse.into(),
        edit: tr.edit.into(),
        open_projects: tr.open_projects.into(),
        dismiss: tr.dismiss.into(),
        projects: tr.projects.into(),
        new_project: tr.new_project.into(),
        no_projects_hint: tr.no_projects_hint.into(),
        select_project_hint: tr.select_project_hint.into(),
        status_never_built: tr.status_never_built.into(),
        settings_section: tr.settings_section.into(),
        source_roots: tr.source_roots.into(),
        root_recursive: tr.root_recursive.into(),
        root_top_level_only: tr.root_top_level_only.into(),
        excluded_dirs: tr.excluded_dirs.into(),
        include_masks: tr.include_masks.into(),
        exclude_masks: tr.exclude_masks.into(),
        respect_gitignore: tr.respect_gitignore.into(),
        max_indexed_file_size: tr.max_indexed_file_size.into(),
        index_archives: tr.index_archives.into(),
        archive_max_depth: tr.archive_max_depth.into(),
        cancel_build: tr.cancel_build.into(),
        last_build: tr.last_build.into(),
        name: tr.name.into(),
        project_name_hint: tr.project_name_hint.into(),
        add_root: tr.add_root.into(),
        remove_root: tr.remove_root.into(),
        root_path_hint: tr.root_path_hint.into(),
        excluded_dirs_hint: tr.excluded_dirs_hint.into(),
        masks_hint: tr.masks_hint.into(),
        delete_project_title: tr.delete_project_title.into(),
        delete_warning: tr.delete_warning.into(),
        catalog_unavailable: tr.catalog_unavailable.into(),
        search_project_label: tr.search_project_label.into(),
        search_field_hint: tr.search_field_hint.into(),
        search_button: tr.search_button.into(),
        options_section: tr.options_section.into(),
        opt_case_sensitive: tr.opt_case_sensitive.into(),
        opt_whole_word: tr.opt_whole_word.into(),
        opt_analyze_oversized: tr.opt_analyze_oversized.into(),
        opt_context_lines: tr.opt_context_lines.into(),
        results_section: tr.results_section.into(),
        copy_path: tr.copy_path.into(),
        expand_all: tr.expand_all.into(),
        collapse_all: tr.collapse_all.into(),
        export_results: tr.export_results.into(),
        saved_searches: tr.saved_searches.into(),
        saved_name_hint: tr.saved_name_hint.into(),
        load: tr.load.into(),
        save_search_title: tr.save_search_title.into(),
        duplicate: tr.duplicate.into(),
        new_tab: tr.new_tab.into(),
        close_tab: tr.close_tab.into(),
        rename_tab_title: tr.rename_tab_title.into(),
        tab_name_hint: tr.tab_name_hint.into(),
        reset_tab_name: tr.reset_tab_name.into(),
        prefs_language: tr.prefs_language.into(),
        prefs_theme: tr.prefs_theme.into(),
        prefs_defaults_section: tr.prefs_defaults_section.into(),
        prefs_default_excluded_dirs: tr.prefs_default_excluded_dirs.into(),
        prefs_default_include_masks: tr.prefs_default_include_masks.into(),
        prefs_default_exclude_masks: tr.prefs_default_exclude_masks.into(),
        prefs_default_max_size: tr.prefs_default_max_size.into(),
        prefs_defaults_note: tr.prefs_defaults_note.into(),
        prefs_updates_section: tr.prefs_updates_section.into(),
        prefs_check_updates: tr.prefs_check_updates.into(),
        prefs_check_now: tr.prefs_check_now.into(),
        prefs_check_updates_soon: tr.prefs_check_updates_soon.into(),
        prefs_autosave_note: tr.prefs_autosave_note.into(),
        viewer_hint: tr.viewer_hint.into(),
        viewer_loading: tr.viewer_loading.into(),
        viewer_truncated: tr.viewer_truncated.into(),
        build_report_section: tr.build_report_section.into(),
        archives_excluded: tr.archives_excluded.into(),
        project_in_use: tr.project_in_use.into(),
    }
}

// -- Synchronization ----------------------------------------------------

/// Pushes every derived property after a mutation — the rendering
/// side of the one-way flow.
fn sync_all(ui: &AppWindow, app: &App) {
    let st = ui.global::<AppState>();
    st.set_catalog_ok(app.catalog.is_some());
    st.set_catalog_error(app.catalog_error.clone().unwrap_or_default().into());
    st.set_screen(match app.screen {
        Screen::Search => 0,
        Screen::Projects => 1,
        Screen::Preferences => 2,
    });
    sync_projects(ui, app);
    sync_selection(ui, app);
    sync_tabs(ui, app);
    sync_search(ui, app);
    sync_saved(ui, app);
    sync_results(ui, app);
    sync_banners(ui, app);
    sync_viewer(ui, app);
    st.set_dialog_kind(dialog_kind(&app.dialog));
    // The preferences max-size error is derived from the field text at
    // sync time — non-numeric or zero is reported inline, same rule as
    // the project editor's field. Reading the property never clobbers
    // the edit (only writes would).
    st.set_pref_error(
        if util::parse_max_size_mib(&st.get_pref_max_size()).is_none() {
            app.tr.err_max_size_invalid.into()
        } else {
            "".into()
        },
    );
}

/// Flags of the viewer overlay. The line rows are *not* pushed here:
/// they live in the shared `ViewerLines` model, filled once when a
/// load completes — resyncing them every tick would defeat
/// virtualization.
fn sync_viewer(ui: &AppWindow, app: &App) {
    let st = ui.global::<AppState>();
    match &app.tab().viewer {
        Some(v) => {
            st.set_viewer_open(true);
            st.set_viewer_title(v.title.clone().into());
            st.set_viewer_loading(v.loading);
            st.set_viewer_error(v.error.clone().unwrap_or_default().into());
            st.set_viewer_truncated(v.truncated);
            st.set_viewer_focus_line(v.focus_line as i32);
            // Horizontal scroll: the widest row gives the range, the
            // focused match's center gives the target.
            st.set_viewer_content_px(app.tab().viewer_lines.content_px());
            st.set_viewer_focus_px(
                v.matches
                    .get(v.match_idx)
                    .map(|m| app.tab().viewer_lines.focus_px(*m))
                    .unwrap_or(0.0),
            );
            st.set_viewer_nav_flip(v.nav_flip);
            st.set_viewer_match_label(if v.loading || v.error.is_some() || v.matches.is_empty() {
                "".into()
            } else {
                format!("{}/{}", v.match_idx + 1, v.matches.len()).into()
            });
            st.set_viewer_nav_enabled(!v.loading && v.error.is_none() && v.matches.len() > 1);
        }
        None => {
            st.set_viewer_open(false);
            st.set_viewer_error("".into());
            st.set_viewer_content_px(0.0);
            st.set_viewer_focus_px(0.0);
            st.set_viewer_nav_flip(false);
        }
    }
}

/// Secondary line of a project row: the index file's modification
/// date in the user's timezone and its size, e.g.
/// `2026-10-04 14:32 · 16.0 MiB`. A dash while the project has no
/// index file yet.
fn index_file_detail(path: &std::path::Path) -> String {
    let Ok(md) = std::fs::metadata(path) else {
        return "—".to_owned();
    };
    let date = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| util::format_unix_local(d.as_secs() as i64))
        .unwrap_or_else(|| "—".to_owned());
    format!("{date} · {}", util::format_bytes(md.len()))
}

fn sync_projects(ui: &AppWindow, app: &App) {
    let st = ui.global::<AppState>();
    let rows: Vec<ProjectRow> = app
        .projects
        .iter()
        .map(|p| {
            let status = app.status(p);
            ProjectRow {
                id: p.id.clone().into(),
                name: p.name.clone().into(),
                status_kind: status.kind(),
                status_text: status.text(app.tr).into(),
                detail: index_file_detail(&p.index_db_path).into(),
                selected: app.selected.as_deref() == Some(p.id.as_str()),
            }
        })
        .collect();
    st.set_projects(ModelRc::new(VecModel::from(rows)));
    st.set_project_names(ModelRc::new(VecModel::from(
        app.projects
            .iter()
            .map(|p| SharedString::from(p.name.as_str()))
            .collect::<Vec<_>>(),
    )));
}

/// The detail column of the Projects screen (status, actions, live
/// build progress, settings, last-build summary).
fn sync_selection(ui: &AppWindow, app: &App) {
    let st = ui.global::<AppState>();
    let tr = app.tr;
    let project = app.selected_project();
    st.set_has_selection(project.is_some());
    let Some(p) = project else {
        st.set_sel_name("".into());
        st.set_sel_status(0);
        st.set_sel_status_text("".into());
        st.set_sel_archives_excluded(false);
        st.set_build_action_label(tr.build_index.into());
        st.set_settings_rows(kv_model(Vec::new()));
        st.set_summary_rows(kv_model(Vec::new()));
        st.set_has_summary(false);
        st.set_has_build_report(false);
        st.set_build_report_rows(kv_model(Vec::new()));
        st.set_sel_building(false);
        st.set_build_phase("".into());
        st.set_build_counters(kv_model(Vec::new()));
        st.set_build_busy(app.build.is_some());
        return;
    };

    let status = app.status(p);
    st.set_sel_name(p.name.clone().into());
    st.set_sel_status(status.kind());
    st.set_sel_status_text(status.text(tr).into());
    // D15: archive indexing must never become a silent result loss —
    // the scope is visible where the user searches, not only in the
    // settings detail.
    st.set_sel_archives_excluded(!p.settings.archives_enabled);
    st.set_build_action_label(
        if p.last_build_settings.is_some() && p.index_db_path.exists() {
            tr.update_index
        } else {
            tr.build_index
        }
        .into(),
    );
    st.set_settings_rows(kv_model(settings_rows(tr, &p.settings)));
    st.set_has_summary(p.last_build_summary.is_some());
    st.set_summary_rows(kv_model(match &p.last_build_summary {
        Some(s) => summary_rows(tr, s),
        None => Vec::new(),
    }));

    let report = app
        .last_report
        .as_ref()
        .filter(|(id, _)| app.selected.as_deref() == Some(id.as_str()));
    st.set_has_build_report(report.is_some());
    st.set_build_report_rows(kv_model(match report {
        Some((_, r)) => build_report_rows(tr, r),
        None => Vec::new(),
    }));

    st.set_build_busy(app.build.is_some());
    let active = app.build.as_ref().filter(|b| b.project_id == p.id);
    st.set_sel_building(active.is_some());
    match active {
        Some(b) => {
            let snap = b.handle.progress().snapshot();
            st.set_build_phase(
                snap.phase
                    .map(|phase| tr.phase_name(phase))
                    .unwrap_or(tr.starting)
                    .into(),
            );
            st.set_build_counters(kv_model(vec![
                (tr.files_seen.into(), snap.files_seen.to_string()),
                (tr.files_indexed.into(), snap.files_indexed.to_string()),
                (tr.files_ignored.into(), snap.files_ignored.to_string()),
                (tr.errors.into(), snap.errors.to_string()),
                (tr.archives.into(), snap.archives.to_string()),
                (tr.archive_entries.into(), snap.archive_entries.to_string()),
                (tr.bytes_read.into(), util::format_bytes(snap.bytes_read)),
            ]));
        }
        None => {
            st.set_build_phase("".into());
            st.set_build_counters(kv_model(Vec::new()));
        }
    }
}

/// The tab strip: one row per open search tab.
fn sync_tabs(ui: &AppWindow, app: &App) {
    let st = ui.global::<AppState>();
    let default = app.tr.nav_search;
    let rows: Vec<TabRow> = app
        .tabs
        .iter()
        .enumerate()
        .map(|(i, t)| TabRow {
            title: t.display_title(default).into(),
            active: i == app.active_tab,
            working: t.job.is_some(),
            saved: t.loaded_saved_id.is_some(),
        })
        .collect();
    st.set_tabs(ModelRc::new(VecModel::from(rows)));
}

/// Rebinds the shared models of the active tab — only on tab
/// switch/create/close: `sync_all` must never rebind, or every tick
/// would reset the virtualized lists.
fn bind_tab_models(ui: &AppWindow, app: &App) {
    let st = ui.global::<AppState>();
    let tab = app.tab();
    st.set_results(tab.results.clone().into());
    st.set_viewer_lines(tab.viewer_lines.model());
}

/// Search-form flags the UI greys buttons on — for the active tab.
/// Also pushes the tab's own project selection: the picker and its
/// status hints describe the tab's project, never the Projects
/// screen's selection.
fn sync_search(ui: &AppWindow, app: &App) {
    let st = ui.global::<AppState>();
    st.set_query_valid(app.tab().form.query_is_valid());
    st.set_query_too_short(!app.tab().form.query.is_empty() && !app.tab().form.query_is_valid());
    st.set_query_hint(
        app.tr
            .search_too_short(rsearch_engine::search::MIN_QUERY_CHARS)
            .into(),
    );
    st.set_searching(app.tab().job.is_some());
    st.set_can_search(app.can_search());
    st.set_search_project_index(app.search_project_index());
    match app.search_project() {
        Some(p) => {
            let status = app.status(p);
            st.set_search_has_project(true);
            st.set_search_status(status.kind());
            st.set_search_status_text(status.text(app.tr).into());
            // D15: archive indexing must never become a silent result
            // loss — the scope is visible where the user searches.
            st.set_search_archives_excluded(!p.settings.archives_enabled);
        }
        None => {
            st.set_search_has_project(false);
            st.set_search_status(0);
            st.set_search_status_text("".into());
            st.set_search_archives_excluded(false);
        }
    }
}

fn sync_saved(ui: &AppWindow, app: &App) {
    let st = ui.global::<AppState>();
    // Row 0 is the localized "select a query" placeholder — a real
    // saved search starts at index 1 (see `App::saved_index`).
    let mut names: Vec<SharedString> = vec![app.tr.saved_combo_hint.into()];
    names.extend(app.saved.iter().map(|s| s.name.clone().into()));
    st.set_saved_names(ModelRc::new(VecModel::from(names)));
    st.set_saved_index(app.saved_index());
}

/// Results header + notes; the rows themselves live in the shared
/// [`crate::results::ResultsModel`].
fn sync_results(ui: &AppWindow, app: &App) {
    let st = ui.global::<AppState>();
    app.tab().results.with(|l| {
        st.set_has_results(l.present);
        st.set_results_empty(l.present && l.is_empty());

        if !l.present {
            st.set_results_title("".into());
            st.set_results_notes("".into());
            st.set_results_empty_text(if app.tab().job.is_some() {
                "".into()
            } else {
                app.tr.empty_results_hint.into()
            });
            return;
        }

        let files = l.report.results.len();
        let matches = l.match_count();
        let mut title = format!(
            "· \"{}\" · {}",
            l.query,
            app.tr.results_count(matches, files)
        );
        if app.tab().form.project_id.as_deref() != Some(l.project_id.as_str()) {
            title.push_str(&format!(
                " · {}",
                app.tr.results_for_project(&l.project_name)
            ));
        }
        st.set_results_title(title.into());

        let mut notes: Vec<String> = Vec::new();
        if l.report.skipped_stale > 0 {
            notes.push(app.tr.skipped_changed(l.report.skipped_stale));
        }
        if l.report.skipped_index_errors > 0 {
            notes.push(app.tr.skipped_index_errors(l.report.skipped_index_errors));
        }
        if l.report.skipped_security_limits > 0 {
            notes.push(
                app.tr
                    .skipped_security_limits(l.report.skipped_security_limits),
            );
        }
        if l.report.verification_errors > 0 {
            notes.push(app.tr.skipped_unreadable(l.report.verification_errors));
        }
        if l.report.truncated_files > 0 {
            notes.push(app.tr.truncated_matches(l.report.truncated_files));
        }
        if !l.analyze_oversized && l.report.candidates_too_large > 0 {
            notes.push(app.tr.oversized_not_analyzed(l.report.candidates_too_large));
        }
        if l.cancelled {
            notes.push(app.tr.results_cancelled.to_owned());
        }
        if l.in_flight && l.analyze_oversized && l.oversized_total > 0 {
            notes.push(
                app.tr
                    .oversized_progress(l.oversized_done, l.oversized_total),
            );
        }
        st.set_results_notes(notes.join("\n").into());

        st.set_results_empty_text(if l.is_empty() {
            app.tr.no_results_hint.into()
        } else {
            "".into()
        });
    });
}

fn sync_banners(ui: &AppWindow, app: &App) {
    let st = ui.global::<AppState>();
    let rows: Vec<BannerRow> = app
        .banners()
        .into_iter()
        .map(|b| BannerRow {
            level: b.level as i32,
            text: b.text.into(),
            action_label: b
                .action
                .as_ref()
                .map(|(label, _)| label.clone().into())
                .unwrap_or_default(),
            has_action: b.action.is_some(),
            dismissible: b.dismiss.is_some(),
            notice_idx: b.dismiss.map(|i| i as i32).unwrap_or(-1),
            working: b.working,
        })
        .collect();
    st.set_banners(ModelRc::new(VecModel::from(rows)));
}

/// Project settings as label/value rows (Projects screen).
fn settings_rows(tr: &Strings, s: &rsearch_catalog::ProjectSettings) -> Vec<(String, String)> {
    let roots = s
        .roots
        .iter()
        .map(|r| {
            format!(
                "{}  ({})",
                r.path.display(),
                if r.recursive {
                    tr.root_recursive
                } else {
                    tr.root_top_level_only
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut rows = vec![
        (
            tr.source_roots.into(),
            if roots.is_empty() {
                "—".into()
            } else {
                roots
            },
        ),
        (
            tr.excluded_dirs.into(),
            if s.excluded_dirs.is_empty() {
                "—".into()
            } else {
                util::join_list(&s.excluded_dirs)
            },
        ),
        (
            tr.include_masks.into(),
            if s.include_masks.is_empty() {
                "—".into()
            } else {
                s.include_masks.join("; ")
            },
        ),
        (
            tr.exclude_masks.into(),
            if s.exclude_masks.is_empty() {
                "—".into()
            } else {
                s.exclude_masks.join("; ")
            },
        ),
        (
            tr.respect_gitignore.into(),
            if s.respect_gitignore {
                tr.yes.into()
            } else {
                tr.no.into()
            },
        ),
        (
            tr.max_indexed_file_size.into(),
            util::format_bytes(s.max_indexed_file_size),
        ),
        (
            tr.index_archives.into(),
            if s.archives_enabled {
                tr.yes.into()
            } else {
                tr.no.into()
            },
        ),
    ];
    if s.archives_enabled {
        rows.push((tr.archive_max_depth.into(), s.archive_max_depth.to_string()));
    }
    rows
}

/// Last-build summary as label/value rows (Projects screen).
fn summary_rows(tr: &Strings, s: &rsearch_engine::BuildSummary) -> Vec<(String, String)> {
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
    let exts = s
        .top_extensions
        .iter()
        .map(|(e, n)| format!("{e} ({n})"))
        .collect::<Vec<_>>()
        .join(", ");
    vec![
        (tr.kind.into(), kind),
        (tr.duration.into(), util::format_duration(s.duration)),
        (tr.files_indexed.into(), s.indexed_files.to_string()),
        (
            tr.top_extensions.into(),
            if exts.is_empty() { "—".into() } else { exts },
        ),
        (tr.ignored_by_name.into(), s.ignored_by_name.to_string()),
        (tr.ignored_by_sniff.into(), s.ignored_by_sniff.to_string()),
        (tr.too_large.into(), s.too_large.to_string()),
        (tr.errors.into(), s.errors.to_string()),
        (tr.security_limits.into(), s.security_limits.to_string()),
        (
            tr.archives_processed.into(),
            s.archives_processed.to_string(),
        ),
        (
            tr.archive_entries_indexed.into(),
            s.archive_entries_indexed.to_string(),
        ),
        (
            tr.index_archives.into(),
            if s.archives_included {
                tr.yes.into()
            } else {
                tr.no.into()
            },
        ),
    ]
}

/// Detailed error records shown in the build-report section before
/// the "+ more" row; the exact total is always shown above them.
const MAX_REPORT_ERROR_ROWS: usize = 20;

/// Last-build report as label/value rows (Projects screen): the exact
/// file-error count (with the omitted-detail count), up to
/// [`MAX_REPORT_ERROR_ROWS`] detailed records, then the source roots
/// the engine skipped before the scan (D11) with their reasons.
fn build_report_rows(tr: &Strings, r: &BuildReport) -> Vec<(String, String)> {
    let mut rows = Vec::new();
    if r.total_errors > 0 || r.omitted_errors > 0 {
        let mut value = r.total_errors.to_string();
        if r.omitted_errors > 0 {
            value.push_str(" — ");
            value.push_str(&tr.build_errors_omitted(r.omitted_errors as usize));
        }
        rows.push((tr.report_file_errors.into(), value));
        for record in r.errors.iter().take(MAX_REPORT_ERROR_ROWS) {
            let place = match &record.entry_path {
                Some(entry) => format!("{} ({})", record.file_path, entry),
                None => record.file_path.clone(),
            };
            rows.push((
                record.code.to_string(),
                format!("{}: {}", place, record.message),
            ));
        }
        let listed = r.errors.len().min(MAX_REPORT_ERROR_ROWS);
        if r.total_errors as usize > listed {
            rows.push((
                tr.report_more_errors(r.total_errors as usize - listed),
                "".into(),
            ));
        }
    }
    if !r.skipped_roots.is_empty() {
        let value = r
            .skipped_roots
            .iter()
            .map(|root| format!("{} — {}", root.path.display(), root.reason))
            .collect::<Vec<_>>()
            .join("\n");
        rows.push((tr.report_skipped_roots.into(), value));
    }
    rows
}

// -- Preferences ---------------------------------------------------------------

/// Pushes the preference fields. Kept out of [`sync_all`]: rewriting
/// an `in-out` text property while the user types would clobber the
/// edit — these only change on screen entry and language switch.
fn sync_prefs(ui: &AppWindow, app: &App) {
    let st = ui.global::<AppState>();
    st.set_pref_language(
        Language::ALL
            .iter()
            .position(|l| *l == app.prefs.language)
            .unwrap_or(0) as i32,
    );
    st.set_pref_theme(theme_mode(app.prefs.theme));
    st.set_pref_dirs(app.prefs.default_excluded_dirs.join("\n").into());
    st.set_pref_include_masks(app.prefs.default_include_masks.join(";").into());
    st.set_pref_exclude_masks(app.prefs.default_exclude_masks.join(";").into());
    st.set_pref_max_size(
        app.prefs
            .default_max_indexed_file_size
            .div_ceil(1024 * 1024)
            .to_string()
            .into(),
    );
    st.set_pref_check_updates(app.prefs.check_for_updates);
}

// -- Search form --------------------------------------------------------------

/// UI properties → active tab's form (before anything reads it).
fn pull_search_form(ui: &AppWindow, app: &mut App) {
    let st = ui.global::<AppState>();
    let form = &mut app.tab_mut().form;
    form.query = st.get_query().to_string();
    form.case_sensitive = st.get_opt_case();
    form.whole_word = st.get_opt_word();
    form.analyze_oversized = st.get_opt_oversized();
    form.context_lines = st.get_opt_context().round().max(0.0) as usize;
    form.include_text = st.get_opt_include_masks().to_string();
    form.exclude_text = st.get_opt_exclude_masks().to_string();
}

/// Active tab's form → UI properties (a tab switch, a tab creation,
/// a saved search loaded into the active tab).
fn push_search_form(ui: &AppWindow, app: &App) {
    let st = ui.global::<AppState>();
    st.set_query(app.tab().form.query.clone().into());
    st.set_opt_case(app.tab().form.case_sensitive);
    st.set_opt_word(app.tab().form.whole_word);
    st.set_opt_oversized(app.tab().form.analyze_oversized);
    st.set_opt_context(app.tab().form.context_lines as f32);
    st.set_opt_include_masks(app.tab().form.include_text.clone().into());
    st.set_opt_exclude_masks(app.tab().form.exclude_text.clone().into());
    st.set_search_project_index(app.search_project_index());
}

// -- Dialogs ---------------------------------------------------------------------

/// Pushes the editor form's values into the dialog properties.
fn push_editor(ui: &AppWindow, app: &App, values: &EditorValues) {
    let st = ui.global::<AppState>();
    st.set_dialog_title(
        if values.original.is_some() {
            app.tr.edit_project_title
        } else {
            app.tr.new_project_title
        }
        .into(),
    );
    st.set_ed_name(values.name.clone().into());
    st.set_ed_excluded_dirs(values.excluded_dirs_text.clone().into());
    st.set_ed_include_masks(values.include_masks_text.clone().into());
    st.set_ed_exclude_masks(values.exclude_masks_text.clone().into());
    st.set_ed_gitignore(values.respect_gitignore);
    st.set_ed_max_size(values.max_size_text.clone().into());
    st.set_ed_archives(values.archives_enabled);
    st.set_ed_archive_depth(values.archive_max_depth as f32);
    st.set_ed_error("".into());
    sync_ed_roots(ui, &values.roots);
}

fn sync_ed_roots(ui: &AppWindow, roots: &[RootEdit]) {
    let st = ui.global::<AppState>();
    st.set_ed_roots(ModelRc::new(VecModel::from(
        roots
            .iter()
            .map(|r| RootRow {
                path: r.path.clone().into(),
                recursive: r.recursive,
            })
            .collect::<Vec<_>>(),
    )));
}

/// Title/message/label of the confirm and name dialogs — set once
/// when the dialog opens (the kind itself is synced everywhere).
fn push_dialog_header(ui: &AppWindow, app: &App) {
    let st = ui.global::<AppState>();
    let tr = app.tr;
    st.set_dialog_can_reset(app.dialog_can_reset());
    st.set_dialog_can_duplicate(app.dialog_can_duplicate());
    match &app.dialog {
        Some(Dialog::SaveSearch) => {
            st.set_dialog_title(tr.save_search_title.into());
            st.set_dialog_confirm_label(tr.save.into());
            // The associated entry's name is proposed — confirming it
            // unchanged UPDATEs that entry, renaming CREATEs a new one.
            st.set_dialog_name(app.suggested_saved_name().into());
            st.set_dialog_name_hint(tr.saved_name_hint.into());
        }
        Some(Dialog::RenameTab { id }) => {
            st.set_dialog_title(tr.rename_tab_title.into());
            st.set_dialog_confirm_label(tr.rename.into());
            st.set_dialog_name(app.tab_title(*id).unwrap_or_default().into());
            st.set_dialog_name_hint(tr.tab_name_hint.into());
        }
        Some(Dialog::ConfirmDelete { name, .. }) => {
            st.set_dialog_title(tr.delete_project_title.into());
            st.set_dialog_message(tr.delete_confirm(name).into());
            st.set_dialog_warning(tr.delete_warning.into());
            st.set_dialog_confirm_label(tr.delete.into());
        }
        Some(Dialog::ConfirmDeleteSaved { name, .. }) => {
            st.set_dialog_title(tr.delete_saved_title.into());
            st.set_dialog_message(tr.delete_saved_confirm(name).into());
            st.set_dialog_warning("".into());
            st.set_dialog_confirm_label(tr.delete.into());
        }
        Some(Dialog::ConfirmBuild {
            update,
            estimate,
            archives,
            ..
        }) => {
            let title = if *update {
                tr.update_index
            } else {
                tr.build_index
            };
            st.set_dialog_title(title.into());
            st.set_dialog_warning("".into());
            let mut body = match estimate {
                Some(d) => tr.confirm_build_last_duration(&util::format_duration(*d)),
                None => tr.confirm_build_unknown_duration.to_owned(),
            };
            if *archives {
                body.push(' ');
                body.push_str(tr.confirm_build_archives);
            }
            st.set_dialog_message(body.into());
            st.set_dialog_confirm_label(title.into());
        }
        _ => {}
    }
}

// -- Callback wiring -------------------------------------------------------------

/// Connects every `AppState` callback to the application. Each
/// handler mutates `App` then resyncs the derived properties.
fn wire(ui: &AppWindow, app: &Rc<RefCell<App>>) {
    let st = ui.global::<AppState>();

    macro_rules! on {
        ($cb:ident, |$a:ident, $u:ident $(, $x:ident : $t:ty)*| $body:block) => {
            st.$cb({
                let weak = ui.as_weak();
                let app = app.clone();
                move |$($x: $t),*| {
                    let Some($u) = weak.upgrade() else {
                        return;
                    };
                    let $a = &mut *app.borrow_mut();
                    $body
                    sync_all(&$u, $a);
                }
            })
        };
    }

    on!(on_retry_catalog, |a, _u| {
        a.retry_catalog();
    });

    on!(on_navigate, |a, _u, index: i32| {
        a.navigate(index);
        if a.screen == Screen::Preferences {
            sync_prefs(&_u, a);
        }
    });

    on!(on_select_project, |a, _u, index: i32| {
        a.select_project(index);
    });
    on!(on_select_search_project, |a, _u, index: i32| {
        a.select_search_project(index);
    });
    on!(on_new_project, |a, u| {
        let values = a.new_project();
        push_editor(&u, a, &values);
    });
    on!(on_edit_project, |a, u| {
        if let Some(values) = a.edit_project() {
            push_editor(&u, a, &values);
        }
    });
    on!(on_ask_delete_project, |a, u| {
        a.ask_delete_project();
        push_dialog_header(&u, a);
    });
    on!(on_start_build, |a, u| {
        a.ask_start_build();
        push_dialog_header(&u, a);
    });
    on!(on_cancel_build, |a, _u| {
        a.cancel_build();
    });

    // -- Search tabs ------------------------------------------------------
    // Each tab switch saves the UI form into the outgoing tab, then
    // installs the incoming tab's form and shared models — `sync_all`
    // alone would not rebind `results`/`viewer-lines`.
    on!(on_new_tab, |a, u| {
        pull_search_form(&u, a);
        a.new_tab();
        bind_tab_models(&u, a);
        push_search_form(&u, a);
        u.invoke_focus_search();
    });
    on!(on_activate_tab, |a, u, index: i32| {
        pull_search_form(&u, a);
        a.activate_tab(index);
        bind_tab_models(&u, a);
        push_search_form(&u, a);
    });
    on!(on_close_tab, |a, u, index: i32| {
        pull_search_form(&u, a);
        a.close_tab(index);
        bind_tab_models(&u, a);
        push_search_form(&u, a);
    });
    on!(on_ask_rename_tab, |a, u, index: i32| {
        a.ask_rename_tab(index);
        push_dialog_header(&u, a);
    });

    on!(on_query_changed, |a, u| {
        a.tab_mut().form.query = u.global::<AppState>().get_query().to_string();
    });
    on!(on_run_search, |a, u| {
        pull_search_form(&u, a);
        a.run_search();
    });
    on!(on_cancel_search, |a, _u| {
        a.cancel_search();
    });

    on!(on_select_saved, |a, _u, index: i32| {
        // Selecting only marks which saved search Load/Delete applies
        // to — the tab's form and title stay untouched.
        a.select_saved(index);
    });
    on!(on_load_saved, |a, u| {
        // Charger activates the tab already holding the entry or
        // fills a new one — it never starts a search. Either way the
        // visible tab may change, so the models are rebound.
        pull_search_form(&u, a);
        a.load_saved();
        bind_tab_models(&u, a);
        push_search_form(&u, a);
    });
    on!(on_ask_save_search, |a, u| {
        pull_search_form(&u, a);
        a.ask_save_search();
        push_dialog_header(&u, a);
    });
    on!(on_ask_delete_saved, |a, u| {
        a.ask_delete_saved();
        push_dialog_header(&u, a);
    });

    on!(on_toggle_result_file, |a, _u, file: i32| {
        a.tab().results.toggle_file(file.max(0) as usize);
    });
    on!(on_select_occurrence, |a, _u, file: i32, occ: i32| {
        a.tab()
            .results
            .select(file.max(0) as usize, occ.max(0) as usize);
    });
    on!(on_open_viewer, |a, _u, file: i32, occ: i32| {
        a.open_viewer(file.max(0) as usize, occ.max(0) as usize);
    });
    on!(on_expand_all_results, |a, _u| {
        a.tab().results.expand_all();
    });
    on!(on_collapse_all_results, |a, _u| {
        a.tab().results.collapse_all();
    });
    on!(on_export_results, |a, u| {
        // The export text crosses back through `results-export` —
        // the widget then pushes it to the clipboard itself (the
        // clipboard is only reachable from the .slint side).
        let text = a.tab().results.export_text();
        u.global::<AppState>().set_results_export(text.into());
    });
    on!(on_close_viewer, |a, _u| {
        a.close_viewer();
    });
    on!(on_viewer_navigate, |a, _u, dir: i32| {
        a.viewer_navigate(dir);
    });
    on!(on_viewer_word_search, |a, u, line: i32, seg: i32, x: f32, w: f32| {
        let Some(word) = a.viewer_word_at(line.max(0) as usize, seg.max(0) as usize, x, w) else {
            return;
        };
        // Same parameters as the tab's current search: the form holds
        // them (pull first, in case an option was edited while the
        // viewer was open); only the query is replaced.
        pull_search_form(&u, a);
        let mut form = a.tab().form.clone();
        form.query = word;
        // The viewer belongs to the originating tab — close it before
        // the new tab becomes active, then search in that new tab.
        a.close_viewer();
        a.new_tab();
        a.tab_mut().form = form;
        push_search_form(&u, a);
        a.run_search();
    });

    on!(on_banner_action, |a, u, index: i32| {
        if let Some(values) = a.run_banner_action(index as usize) {
            push_editor(&u, a, &values);
        } else {
            // A banner action may open a dialog without returning
            // editor values (e.g. the build confirmation).
            push_dialog_header(&u, a);
        }
    });
    on!(on_banner_dismiss, |a, _u, index: i32| {
        if index >= 0 {
            a.dismiss_notice(index as usize);
        }
    });

    on!(on_dialog_cancel, |a, _u| {
        a.dialog_cancel();
    });
    on!(on_dialog_reset_name, |a, _u| {
        a.dialog_reset_name();
    });
    on!(on_dialog_confirm, |a, u| {
        let name = u.global::<AppState>().get_dialog_name().to_string();
        a.dialog_confirm(&name);
    });
    on!(on_dialog_duplicate, |a, u| {
        let name = u.global::<AppState>().get_dialog_name().to_string();
        a.dialog_duplicate(&name);
    });

    on!(
        on_ed_root_path_edited,
        |a, _u, index: i32, path: SharedString| {
            a.editor_root_path(index as usize, path.to_string());
        }
    );
    on!(
        on_ed_root_recursive_toggled,
        |a, _u, index: i32, rec: bool| {
            a.editor_root_recursive(index as usize, rec);
        }
    );
    on!(on_ed_browse_root, |a, u, index: i32| {
        match a.editor_browse_root(index as usize) {
            Some(Ok(_)) => {
                u.global::<AppState>().set_ed_error("".into());
                if let Some(Dialog::Editor { values_roots, .. }) = &a.dialog {
                    sync_ed_roots(&u, values_roots);
                }
            }
            Some(Err(msg)) => u.global::<AppState>().set_ed_error(msg.into()),
            None => {}
        }
    });
    on!(on_ed_remove_root, |a, u, index: i32| {
        if a.editor_remove_root(index as usize) {
            if let Some(Dialog::Editor { values_roots, .. }) = &a.dialog {
                sync_ed_roots(&u, values_roots);
            }
        }
    });
    on!(on_ed_add_root, |a, u| {
        if a.editor_add_root() {
            if let Some(Dialog::Editor { values_roots, .. }) = &a.dialog {
                sync_ed_roots(&u, values_roots);
            }
        }
    });
    on!(on_ed_submit, |a, u| {
        let st = u.global::<AppState>();
        let values = EditorValues {
            original: None,
            name: st.get_ed_name().to_string(),
            roots: Vec::new(),
            excluded_dirs_text: st.get_ed_excluded_dirs().to_string(),
            include_masks_text: st.get_ed_include_masks().to_string(),
            exclude_masks_text: st.get_ed_exclude_masks().to_string(),
            respect_gitignore: st.get_ed_gitignore(),
            max_size_text: st.get_ed_max_size().to_string(),
            archives_enabled: st.get_ed_archives(),
            archive_max_depth: st.get_ed_archive_depth().round().max(0.0) as u32,
        };
        match a.apply_editor(values) {
            Ok(()) => {}
            Err(msg) => st.set_ed_error(msg.into()),
        }
    });

    on!(on_language_selected, |a, u, index: i32| {
        a.set_language(index);
        push_texts(&u, a);
        sync_prefs(&u, a);
    });
    on!(on_theme_selected, |a, u, index: i32| {
        a.set_theme(index);
        u.invoke_apply_theme(theme_mode(a.prefs.theme));
    });
    on!(on_pref_dirs_edited, |a, u| {
        let text = u.global::<AppState>().get_pref_dirs().to_string();
        a.pref_dirs_edited(&text);
    });
    on!(on_pref_include_masks_edited, |a, u| {
        let text = u.global::<AppState>().get_pref_include_masks().to_string();
        a.pref_include_masks_edited(&text);
    });
    on!(on_pref_exclude_masks_edited, |a, u| {
        let text = u.global::<AppState>().get_pref_exclude_masks().to_string();
        a.pref_exclude_masks_edited(&text);
    });
    on!(on_pref_max_size_edited, |a, u| {
        let text = u.global::<AppState>().get_pref_max_size().to_string();
        a.pref_max_size_edited(&text);
    });
    on!(on_pref_check_toggled, |a, u| {
        let checked = u.global::<AppState>().get_pref_check_updates();
        a.pref_check_toggled(checked);
    });
    on!(on_check_updates, |a, _u| {
        a.check_updates();
    });
}
