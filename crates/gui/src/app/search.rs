//! The search screen: project picker, query field, options, saved
//! searches and the result list.
//!
//! This module only lays out widgets and collects [`Action`]s; starting
//! the job, talking to the catalog and deciding banners stays in
//! [`super::RsearchApp`].

use eframe::egui;
use rsearch_catalog::SavedSearch;
use rsearch_engine::search::MIN_QUERY_CHARS;
use rsearch_engine::{FileResult, SearchOptions, SearchReport};

use super::{theme, Action, RsearchApp};
use crate::util;

/// UI state of the search screen.
#[derive(Default)]
pub struct SearchScreen {
    /// The query text being edited.
    pub query: String,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub context_lines: usize,
    /// Raw text of the extension filter ("rs, toml"); parsed on use.
    pub extensions_text: String,
    /// Saved searches of the selected project — a display cache of the
    /// catalog, refreshed whenever the project changes or a search is
    /// saved/renamed/deleted.
    pub saved: Vec<SavedSearch>,
    /// Id of the saved search currently loaded into the form.
    pub loaded_saved: Option<String>,
    /// The last completed search, tagged with the project and query it
    /// ran on.
    pub last: Option<FinishedSearch>,
    /// Selected (file index, occurrence index) in the result list —
    /// the future double-click / open-in-editor hook.
    pub selected: Option<(usize, usize)>,
    /// Focus the query field on the next frame.
    pub want_focus: bool,
}

/// A finished search, kept with enough context to label its results
/// correctly even if the project selection changed since.
pub struct FinishedSearch {
    pub project_id: String,
    pub project_name: String,
    pub query: String,
    pub report: SearchReport,
}

impl SearchScreen {
    /// Engine options built from the current form state.
    pub fn options(&self) -> SearchOptions {
        let extensions = util::parse_extensions(&self.extensions_text);
        SearchOptions {
            case_sensitive: self.case_sensitive,
            whole_word: self.whole_word,
            context_lines: self.context_lines,
            extensions: if extensions.is_empty() {
                None
            } else {
                Some(extensions)
            },
        }
    }

    /// Whether the query passes the engine's minimum length.
    pub fn query_is_valid(&self) -> bool {
        self.query.chars().count() >= MIN_QUERY_CHARS
    }
}

/// `path` display string, `archive.zip!/entry` for archive members.
fn display_path(r: &FileResult) -> String {
    match &r.entry_path {
        Some(entry) => format!("{}!{}", r.file_path.display(), entry),
        None => r.file_path.display().to_string(),
    }
}

impl RsearchApp {
    /// Whether the current form state can launch a search.
    fn can_search(&self) -> bool {
        if self.search_job.is_some() || !self.search_screen.query_is_valid() {
            return false;
        }
        self.selected_project()
            .is_some_and(|p| p.index_db_path.exists())
    }

    pub(super) fn search_ui(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        // Controls stay on top; the result list owns the remaining
        // space with its own scroll area.
        self.project_picker_ui(ui, actions);
        ui.add_space(10.0);
        self.query_ui(ui, actions);
        self.options_ui(ui);
        ui.add_space(6.0);
        self.saved_row_ui(ui, actions);
        ui.add_space(14.0);
        self.results_ui(ui);
    }

    /// "Project: [combo]  (status)" — selecting a project here and in
    /// the Projects screen share the same `selected` state.
    fn project_picker_ui(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let tr = self.tr;
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(tr.search_project_label).strong());
            let selected_text = self
                .selected_project()
                .map(|p| p.name.clone())
                .unwrap_or_else(|| tr.select_project_hint.to_owned());
            egui::ComboBox::from_id_salt("search_project")
                .selected_text(selected_text)
                .width(260.0)
                .show_ui(ui, |ui| {
                    for p in &self.projects {
                        if ui
                            .selectable_label(
                                self.selected.as_deref() == Some(p.id.as_str()),
                                &p.name,
                            )
                            .clicked()
                        {
                            actions.push(Action::Select(p.id.clone()));
                        }
                    }
                });
            if let Some(p) = self.selected_project() {
                let status = self.status(p);
                ui.weak(format!("· {}", status.text(tr)));
            }
        });
    }

    /// The dominant element of the screen: the query field plus the
    /// primary Search button.
    fn query_ui(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let tr = self.tr;
        let running = self.search_job.is_some();
        ui.horizontal(|ui| {
            let field = egui::TextEdit::singleline(&mut self.search_screen.query)
                .hint_text(tr.search_field_hint)
                .font(egui::TextStyle::Heading)
                .desired_width(f32::INFINITY)
                .min_size(egui::vec2(120.0, 34.0))
                .margin(egui::Margin::symmetric(10, 8));
            let resp = ui.add_enabled(!running, field);
            if self.search_screen.want_focus {
                resp.request_focus();
                self.search_screen.want_focus = false;
            }
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let button = if running {
                egui::Button::new(tr.search_running).min_size(egui::vec2(96.0, 34.0))
            } else {
                egui::Button::new(egui::RichText::new(tr.search_button).strong())
                    .min_size(egui::vec2(96.0, 34.0))
                    .fill(theme::ACCENT)
            };
            let clicked = ui.add_enabled(self.can_search(), button).clicked();
            if (enter || clicked) && self.can_search() {
                actions.push(Action::RunSearch);
            }
        });
        if !self.search_screen.query.is_empty() && !self.search_screen.query_is_valid() {
            ui.weak(tr.search_too_short(MIN_QUERY_CHARS));
        }
    }

    /// Collapsible options — everything the engine's [`SearchOptions`]
    /// currently supports, laid out so new options can join the grid.
    fn options_ui(&mut self, ui: &mut egui::Ui) {
        let tr = self.tr;
        egui::CollapsingHeader::new(tr.options_section)
            .id_salt("search_options")
            .default_open(false)
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.checkbox(
                        &mut self.search_screen.case_sensitive,
                        tr.opt_case_sensitive,
                    );
                    ui.checkbox(&mut self.search_screen.whole_word, tr.opt_whole_word);
                    ui.horizontal(|ui| {
                        ui.label(tr.opt_context_lines);
                        ui.add(
                            egui::DragValue::new(&mut self.search_screen.context_lines)
                                .range(0..=16),
                        );
                    });
                });
                ui.horizontal(|ui| {
                    ui.label(tr.opt_extensions);
                    ui.add(
                        egui::TextEdit::singleline(&mut self.search_screen.extensions_text)
                            .desired_width(260.0)
                            .hint_text(tr.opt_extensions_hint),
                    );
                });
            });
    }

    /// Saved searches of the selected project: load / run / save /
    /// rename / delete.
    fn saved_row_ui(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let tr = self.tr;
        if self.selected.is_none() {
            return;
        }
        let loaded = self.search_screen.loaded_saved.clone();
        let loaded_name = loaded
            .as_deref()
            .and_then(|id| {
                self.search_screen
                    .saved
                    .iter()
                    .find(|s| s.id == id)
                    .map(|s| s.name.clone())
            })
            .unwrap_or_else(|| tr.saved_combo_hint.to_owned());
        let has_saved = loaded.is_some();

        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new(tr.saved_searches).strong());
            egui::ComboBox::from_id_salt("saved_searches")
                .selected_text(loaded_name)
                .width(220.0)
                .show_ui(ui, |ui| {
                    if self.search_screen.saved.is_empty() {
                        ui.weak("—");
                    }
                    for s in &self.search_screen.saved {
                        if ui
                            .selectable_label(loaded.as_deref() == Some(s.id.as_str()), &s.name)
                            .clicked()
                        {
                            actions.push(Action::LoadSaved(s.id.clone()));
                        }
                    }
                });
            if ui
                .add_enabled(has_saved, egui::Button::new(tr.run))
                .clicked()
            {
                if let Some(id) = &loaded {
                    actions.push(Action::RunSaved(id.clone()));
                }
            }
            let can_save = self.search_screen.query_is_valid();
            if ui
                .add_enabled(can_save, egui::Button::new(format!("{}…", tr.save)))
                .clicked()
            {
                actions.push(Action::AskSaveSearch);
            }
            if ui
                .add_enabled(has_saved, egui::Button::new(format!("{}…", tr.rename)))
                .clicked()
            {
                if let Some(id) = &loaded {
                    actions.push(Action::RenameSaved(id.clone()));
                }
            }
            if ui
                .add_enabled(has_saved, egui::Button::new(format!("{}…", tr.delete)))
                .clicked()
            {
                if let Some(id) = &loaded {
                    actions.push(Action::AskDeleteSaved(id.clone()));
                }
            }
        });
    }

    /// Results header (counts, skipped counters, provenance) then the
    /// collapsible per-file list.
    fn results_ui(&mut self, ui: &mut egui::Ui) {
        let tr = self.tr;
        let Some(fin) = &self.search_screen.last else {
            if self.search_job.is_none() {
                ui.vertical_centered(|ui| {
                    ui.add_space(80.0);
                    ui.weak(tr.empty_results_hint);
                });
            }
            return;
        };

        let files = fin.report.results.len();
        let matches: usize = fin.report.results.iter().map(|r| r.occurrences.len()).sum();

        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new(tr.results_section).strong());
            ui.weak(format!("· \"{}\"", fin.query));
            ui.weak(format!("· {}", tr.results_count(matches, files)));
            if self.selected.as_deref() != Some(fin.project_id.as_str()) {
                ui.weak(format!("· {}", tr.results_for_project(&fin.project_name)));
            }
        });

        // Honest accounting: what the index promised but could not
        // deliver is reported, never folded into "no results".
        let mut skipped = Vec::new();
        if fin.report.skipped_stale > 0 {
            skipped.push(tr.skipped_changed(fin.report.skipped_stale));
        }
        if fin.report.skipped_unverifiable > 0 {
            skipped.push(tr.skipped_unverifiable(fin.report.skipped_unverifiable));
        }
        if fin.report.verification_errors > 0 {
            skipped.push(tr.skipped_unreadable(fin.report.verification_errors));
        }
        if fin.report.truncated_files > 0 {
            skipped.push(tr.truncated_matches(fin.report.truncated_files));
        }
        if !skipped.is_empty() {
            ui.weak(skipped.join(" · "));
        }
        ui.add_space(4.0);

        if files == 0 {
            ui.vertical_centered(|ui| {
                ui.add_space(60.0);
                ui.weak(tr.no_results_hint);
            });
            return;
        }

        let default_open = files <= 20;
        egui::ScrollArea::vertical()
            .id_salt("results")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (fi, fr) in fin.report.results.iter().enumerate() {
                    let header = format!("{}  ({})", display_path(fr), fr.occurrences.len());
                    egui::CollapsingHeader::new(egui::RichText::new(header).monospace())
                        .id_salt(fi)
                        .default_open(default_open)
                        .show_background(files > 1)
                        .show(ui, |ui| {
                            for (oi, occ) in fr.occurrences.iter().enumerate() {
                                let selected = self.search_screen.selected == Some((fi, oi));
                                let text = format!(
                                    "{}:{}  {}",
                                    occ.line,
                                    occ.column,
                                    occ.line_text.trim_end()
                                );
                                if ui
                                    .selectable_label(
                                        selected,
                                        egui::RichText::new(text).monospace(),
                                    )
                                    .clicked()
                                {
                                    self.search_screen.selected =
                                        if selected { None } else { Some((fi, oi)) };
                                }
                                if selected {
                                    for line in
                                        occ.context_before.iter().chain(occ.context_after.iter())
                                    {
                                        ui.weak(egui::RichText::new(line.trim_end()).monospace());
                                    }
                                }
                            }
                        });
                }
            });
    }
}
