//! The search screen: project picker, query field, options, saved
//! searches and the result list.
//!
//! This module only lays out widgets and emits [`Message`]s; starting
//! the job, talking to the catalog and deciding banners stays in
//! [`super::RsearchApp`].

use std::fmt;

use iced::widget::{
    button, column, container, pick_list, row, scrollable, slider, space, text, text_input,
};
use iced::{Alignment, Element, Fill, Font};
use rsearch_catalog::SavedSearch;
use rsearch_engine::search::MIN_QUERY_CHARS;
use rsearch_engine::{FileResult, SearchOptions, SearchReport};

use super::{theme, Message, RsearchApp};
use crate::util;

/// `text_input` id of the query field — the target of focus requests
/// when the screen is entered.
pub const QUERY_ID: &str = "search-query";

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
    /// Whether the options section is expanded.
    pub options_open: bool,
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
}

/// A finished search, kept with enough context to label its results
/// correctly even if the project selection changed since.
pub struct FinishedSearch {
    pub project_id: String,
    pub project_name: String,
    pub query: String,
    pub report: SearchReport,
    /// Expanded state of each file group, aligned with
    /// `report.results`.
    pub open: Vec<bool>,
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

/// A project entry in the picker: displays by name, identifies by id.
#[derive(Debug, Clone)]
struct ProjectPick {
    id: String,
    name: String,
}

impl PartialEq for ProjectPick {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for ProjectPick {}

impl fmt::Display for ProjectPick {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)
    }
}

/// A saved-search entry in the picker: displays by name, identifies by
/// id.
#[derive(Debug, Clone)]
struct SavedPick {
    id: String,
    name: String,
}

impl PartialEq for SavedPick {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for SavedPick {}

impl fmt::Display for SavedPick {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)
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

    pub(super) fn search_view(&self) -> Element<'_, Message> {
        column![
            self.project_picker_view(),
            self.query_view(),
            self.options_view(),
            self.saved_row_view(),
            self.results_view(),
        ]
        .spacing(10)
        .into()
    }

    /// "Project: [picker]  (status)" — selecting a project here and in
    /// the Projects screen share the same `selected` state.
    fn project_picker_view(&self) -> Element<'_, Message> {
        let tr = self.tr;
        let items: Vec<ProjectPick> = self
            .projects
            .iter()
            .map(|p| ProjectPick {
                id: p.id.clone(),
                name: p.name.clone(),
            })
            .collect();
        let selected = self.selected_project().map(|p| ProjectPick {
            id: p.id.clone(),
            name: p.name.clone(),
        });
        let mut row = row![
            text(tr.search_project_label).font(bold()),
            pick_list(items, selected, |p| Message::SelectProject(p.id))
                .placeholder(tr.select_project_hint)
                .width(280.0),
        ]
        .spacing(10)
        .align_y(Alignment::Center);
        if let Some(p) = self.selected_project() {
            let status = self.status(p);
            row = row.push(
                text(format!("· {}", status.text(tr)))
                    .style(theme::weak)
                    .size(13.0),
            );
        }
        row.into()
    }

    /// The dominant element of the screen: the query field plus the
    /// primary Search button.
    fn query_view(&self) -> Element<'_, Message> {
        let tr = self.tr;
        let running = self.search_job.is_some();
        let can = self.can_search();
        let mut field = text_input(tr.search_field_hint, &self.search_screen.query)
            .id(QUERY_ID)
            .size(20.0)
            .padding([8.0, 12.0])
            .width(Fill);
        if !running {
            field = field.on_input(Message::QueryChanged);
            if can {
                field = field.on_submit(Message::RunSearch);
            }
        }
        let go = if running {
            // While a search runs, the primary action is cancelling
            // it: `CancelSearch` raises the engine's flag and the job
            // reports `SearchError::Cancelled` on the next tick.
            button(text(tr.cancel).center().width(Fill))
                .padding([8.0, 18.0])
                .width(120.0)
                .style(button::secondary)
                .on_press(Message::CancelSearch)
        } else {
            let mut go = button(text(tr.search_button).center().width(Fill))
                .padding([8.0, 18.0])
                .width(120.0)
                .style(button::primary);
            if can {
                go = go.on_press(Message::RunSearch);
            }
            go
        };
        let mut col = column![row![field, go].spacing(10).align_y(Alignment::Center)];
        if !self.search_screen.query.is_empty() && !self.search_screen.query_is_valid() {
            col = col.push(
                text(tr.search_too_short(MIN_QUERY_CHARS))
                    .style(theme::weak)
                    .size(13.0),
            );
        }
        col.into()
    }

    /// Collapsible options — everything the engine's [`SearchOptions`]
    /// currently supports, laid out so new options can join the grid.
    fn options_view(&self) -> Element<'_, Message> {
        let tr = self.tr;
        let open = self.search_screen.options_open;
        let header = button(text(format!(
            "{}  {}",
            if open { "▾" } else { "▸" },
            tr.options_section
        )))
        .padding([2.0, 4.0])
        .style(button::text)
        .on_press(Message::ToggleOptions);
        if !open {
            return header.into();
        }
        let body = column![
            row![
                iced::widget::checkbox(self.search_screen.case_sensitive)
                    .label(tr.opt_case_sensitive)
                    .on_toggle(Message::CaseSensitive),
                iced::widget::checkbox(self.search_screen.whole_word)
                    .label(tr.opt_whole_word)
                    .on_toggle(Message::WholeWord),
                row![
                    text(tr.opt_context_lines),
                    slider(0..=16_u32, self.search_screen.context_lines as u32, |v| {
                        Message::ContextLines(v as usize)
                    },)
                    .width(140.0),
                    text(self.search_screen.context_lines.to_string()).width(28.0),
                ]
                .spacing(10)
                .align_y(Alignment::Center),
            ]
            .spacing(24)
            .align_y(Alignment::Center),
            row![
                text(tr.opt_extensions),
                text_input(tr.opt_extensions_hint, &self.search_screen.extensions_text)
                    .width(280.0)
                    .on_input(Message::ExtensionsChanged),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        ]
        .spacing(8);
        column![header, body].spacing(4).into()
    }

    /// Saved searches of the selected project: load / run / save /
    /// rename / delete.
    fn saved_row_view(&self) -> Element<'_, Message> {
        let tr = self.tr;
        if self.selected.is_none() {
            return space().into();
        }
        let loaded = self.search_screen.loaded_saved.clone();
        let items: Vec<SavedPick> = self
            .search_screen
            .saved
            .iter()
            .map(|s| SavedPick {
                id: s.id.clone(),
                name: s.name.clone(),
            })
            .collect();
        let selected = loaded
            .as_deref()
            .and_then(|id| items.iter().find(|s| s.id == id).cloned());
        let has_saved = loaded.is_some();
        let can_save = self.search_screen.query_is_valid();

        let mut buttons = row![pick_list(items, selected, |s| Message::LoadSaved(s.id))
            .placeholder(tr.saved_combo_hint)
            .width(240.0),]
        .spacing(8);

        buttons = buttons.push(button(text(tr.run)).on_press_maybe(
            has_saved.then(|| Message::RunSaved(loaded.clone().unwrap_or_default())),
        ));
        buttons = buttons.push(
            button(text(format!("{}…", tr.save)))
                .on_press_maybe(can_save.then_some(Message::AskSaveSearch)),
        );
        buttons = buttons.push(button(text(format!("{}…", tr.rename))).on_press_maybe(
            has_saved.then(|| Message::AskRenameSaved(loaded.clone().unwrap_or_default())),
        ));
        buttons = buttons.push(button(text(format!("{}…", tr.delete))).on_press_maybe(
            has_saved.then(|| Message::AskDeleteSaved(loaded.clone().unwrap_or_default())),
        ));

        row![
            text(tr.saved_searches).font(bold()),
            buttons.align_y(Alignment::Center),
        ]
        .spacing(14)
        .align_y(Alignment::Center)
        .into()
    }

    /// Results header (counts, skipped counters, provenance) then the
    /// collapsible per-file list.
    fn results_view(&self) -> Element<'_, Message> {
        let tr = self.tr;
        let Some(fin) = &self.search_screen.last else {
            if self.search_job.is_none() {
                return container(
                    text(tr.empty_results_hint)
                        .style(theme::weak)
                        .size(15.0)
                        .center(),
                )
                .width(Fill)
                .height(Fill)
                .center(Fill)
                .into();
            }
            return space().into();
        };

        let files = fin.report.results.len();
        let matches: usize = fin.report.results.iter().map(|r| r.occurrences.len()).sum();

        let mut header = row![
            text(tr.results_section).font(bold()),
            text(format!("· \"{}\"", fin.query))
                .style(theme::weak)
                .size(13.0),
            text(format!("· {}", tr.results_count(matches, files)))
                .style(theme::weak)
                .size(13.0),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        if self.selected.as_deref() != Some(fin.project_id.as_str()) {
            header = header.push(
                text(format!("· {}", tr.results_for_project(&fin.project_name)))
                    .style(theme::weak)
                    .size(13.0),
            );
        }

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

        let mut col = column![header].spacing(4);
        if !skipped.is_empty() {
            col = col.push(text(skipped.join(" · ")).style(theme::weak).size(13.0));
        }

        if files == 0 {
            col = col.push(
                container(
                    text(tr.no_results_hint)
                        .style(theme::weak)
                        .size(15.0)
                        .center(),
                )
                .width(Fill)
                .padding(iced::Padding::ZERO.top(40.0)),
            );
            return col.into();
        }

        let mut list = column![].spacing(2);
        for (fi, fr) in fin.report.results.iter().enumerate() {
            let open = fin.open.get(fi).copied().unwrap_or(false);
            let header = format!("{}  ({})", display_path(fr), fr.occurrences.len());
            list = list.push(
                button(
                    text(format!("{} {}", if open { "▾" } else { "▸" }, header))
                        .font(Font::MONOSPACE)
                        .size(13.0),
                )
                .width(Fill)
                .padding([3.0, 8.0])
                .style(theme::group_header())
                .on_press(Message::ToggleResultFile(fi)),
            );
            if !open {
                continue;
            }
            for (oi, occ) in fr.occurrences.iter().enumerate() {
                let selected = self.search_screen.selected == Some((fi, oi));
                let line = format!("{}:{}  {}", occ.line, occ.column, occ.line_text.trim_end());
                list = list.push(
                    button(text(line).font(Font::MONOSPACE).size(13.0))
                        .width(Fill)
                        .padding([3.0, 8.0])
                        .style(theme::list_row(selected))
                        .on_press(Message::SelectOccurrence(fi, oi)),
                );
                if selected {
                    for ctx in occ.context_before.iter().chain(occ.context_after.iter()) {
                        list = list.push(
                            container(
                                text(ctx.trim_end())
                                    .font(Font::MONOSPACE)
                                    .size(12.0)
                                    .style(theme::weak),
                            )
                            .padding(iced::Padding::ZERO.left(28.0)),
                        );
                    }
                }
            }
        }
        col.push(
            scrollable(list)
                .direction(iced::widget::scrollable::Direction::Vertical(
                    iced::widget::scrollable::Scrollbar::new().width(8),
                ))
                .height(Fill),
        )
        .into()
    }
}

/// The bold face used for section labels.
fn bold() -> Font {
    Font {
        weight: iced::font::Weight::Bold,
        ..Font::DEFAULT
    }
}
