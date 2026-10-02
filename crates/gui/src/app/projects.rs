//! The Projects screen: list on the left, details on the right —
//! status, build/update controls, live progress, settings and the
//! last build summary.
//!
//! Widgets only emit [`Message`]s; every catalog or engine call is
//! applied afterwards in [`super::RsearchApp::update`].

use iced::widget::{button, column, container, row, rule, scrollable, text};
use iced::{Alignment, Element, Fill, Font};
use rsearch_catalog::ProjectSettings;
use rsearch_engine::{BuildKind, BuildSummary, ProgressSnapshot};

use super::{theme, Message, RsearchApp};
use crate::tr::Strings;
use crate::util;

/// Width of the project list column.
const LIST_WIDTH: f32 = 260.0;

impl RsearchApp {
    pub(super) fn projects_view(&self) -> Element<'_, Message> {
        row![
            self.project_list_view(),
            rule::vertical(1),
            self.project_detail_view(),
        ]
        .spacing(16)
        .into()
    }

    /// The project list: one selectable row per project with a status
    /// line, plus the New project button on top.
    fn project_list_view(&self) -> Element<'_, Message> {
        let tr = self.tr;
        let header = row![
            text(tr.projects).size(18.0).width(Fill),
            button(text(tr.new_project)).on_press_maybe(if self.catalog.is_some() {
                Some(Message::NewProject)
            } else {
                None
            }),
        ]
        .align_y(Alignment::Center);

        let mut col = column![header].spacing(6).width(LIST_WIDTH);

        if self.projects.is_empty() {
            col = col.push(
                container(text(tr.no_projects_hint).style(theme::weak).center())
                    .width(Fill)
                    .padding(iced::Padding::ZERO.top(40.0)),
            );
            return col.into();
        }

        let mut list = column![].spacing(2);
        for p in &self.projects {
            let status = self.status(p);
            let is_selected = self.selected.as_deref() == Some(p.id.as_str());
            let date = p
                .last_build_at
                .map(util::format_unix)
                .unwrap_or_else(|| "—".to_owned());
            list = list.push(
                column![
                    button(text(p.name.clone()).font(bold()))
                        .width(Fill)
                        .padding([4.0, 8.0])
                        .style(theme::list_row(is_selected))
                        .on_press(Message::SelectProject(p.id.clone())),
                    container(
                        row![
                            text(status.text(tr)).size(12.0).color(status_color(status)),
                            text(format!("· {date}")).size(12.0).style(theme::weak),
                        ]
                        .spacing(6),
                    )
                    .padding(iced::Padding::ZERO.left(14.0)),
                ]
                .spacing(2),
            );
        }
        col.push(scrollable(list).height(Fill)).into()
    }

    /// Header, actions, live build progress, settings and last build
    /// summary of the selected project.
    fn project_detail_view(&self) -> Element<'_, Message> {
        let tr = self.tr;
        let Some(project) = self.selected_project() else {
            return container(text(tr.select_project_hint).style(theme::weak).center())
                .width(Fill)
                .height(Fill)
                .center(Fill)
                .into();
        };

        let status = self.status(project);
        let busy = self.build.is_some();

        // A first build is `rebuild_index`; afterwards `update_index`
        // (which falls back to a full rebuild by itself when needed).
        let label = if project.last_build_settings.is_some() && project.index_db_path.exists() {
            tr.update_index
        } else {
            tr.build_index
        };
        let actions = row![
            button(text(label))
                .padding([6.0, 18.0])
                .style(button::primary)
                .on_press_maybe((!busy).then(|| Message::StartBuild(project.id.clone()))),
            button(text(tr.edit))
                .on_press_maybe((!busy).then(|| Message::EditProject(project.id.clone()))),
            button(text(tr.delete))
                .style(button::danger)
                .on_press_maybe((!busy).then(|| Message::AskDeleteProject(project.id.clone()))),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        let mut col = column![
            row![
                text(project.name.clone()).size(20.0).font(bold()),
                text(format!("({})", status.text(tr))).color(status_color(status)),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
            actions,
        ]
        .spacing(8);

        if let Some(active) = self.build.as_ref().filter(|b| b.project_id == project.id) {
            let snap = active.handle.progress().snapshot();
            col = col.push(
                container(Self::build_progress_view(tr, &snap))
                    .width(Fill)
                    .padding(12.0)
                    .style(theme::subtle),
            );
        }

        col = col.push(Self::collapsible(
            tr.settings_section,
            self.settings_open,
            Message::ToggleProjectSettings,
            Self::settings_view(tr, &project.settings),
        ));

        match &project.last_build_summary {
            Some(summary) => {
                col = col.push(Self::collapsible(
                    tr.last_build,
                    self.summary_open,
                    Message::ToggleBuildSummary,
                    Self::summary_view(tr, summary),
                ));
            }
            None => {
                col = col.push(text(tr.status_never_built).style(theme::weak));
            }
        }

        scrollable(col.spacing(6)).height(Fill).into()
    }

    /// A collapsible section: clickable heading plus optional body.
    fn collapsible<'a>(
        title: &'a str,
        open: bool,
        toggle: Message,
        body: Element<'a, Message>,
    ) -> Element<'a, Message> {
        let mut col = column![
            button(text(format!("{}  {}", if open { "▾" } else { "▸" }, title)))
                .padding([2.0, 4.0])
                .style(button::text)
                .on_press(toggle)
        ]
        .spacing(4);
        if open {
            col = col.push(body);
        }
        col.into()
    }

    fn build_progress_view<'a>(tr: &Strings, snap: &ProgressSnapshot) -> Element<'a, Message> {
        let running = !snap.phase.is_some_and(|p| p.is_terminal());
        let mut head = row![text(
            snap.phase
                .map(|p| p.to_string())
                .unwrap_or_else(|| tr.starting.to_owned()),
        )
        .width(Fill),]
        .spacing(8)
        .align_y(Alignment::Center);
        if running {
            head = head.push(text("●").color(theme::ACCENT).size(11.0));
        }
        head = head.push(
            button(text(tr.cancel_build))
                .style(button::danger)
                .on_press(Message::CancelBuild),
        );

        let pairs: [(&str, String); 7] = [
            (tr.files_seen, snap.files_seen.to_string()),
            (tr.files_indexed, snap.files_indexed.to_string()),
            (tr.files_ignored, snap.files_ignored.to_string()),
            (tr.errors, snap.errors.to_string()),
            (tr.archives, snap.archives.to_string()),
            (tr.archive_entries, snap.archive_entries.to_string()),
            (tr.bytes_read, util::format_bytes(snap.bytes_read)),
        ];
        let mut counters = row![].spacing(24);
        for (label, value) in pairs.iter() {
            counters = counters.push(
                column![
                    text(*label).size(12.0).style(theme::weak),
                    text(value.clone()),
                ]
                .spacing(2),
            );
        }
        column![head, counters].spacing(8).into()
    }

    /// One label/value row of a details grid.
    fn kv_row<'a>(label: &'a str, value: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
        row![
            container(text(label).style(theme::weak)).width(200.0),
            value.into(),
        ]
        .spacing(8)
        .into()
    }

    fn settings_view<'a>(tr: &Strings, s: &'a ProjectSettings) -> Element<'a, Message> {
        let mut col = column![].spacing(4);

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
        col = col.push(Self::kv_row(
            tr.source_roots,
            text(if roots.is_empty() {
                "—".to_owned()
            } else {
                roots
            }),
        ));
        col = col.push(Self::kv_row(
            tr.excluded_dirs,
            text(if s.excluded_dirs.is_empty() {
                "—".to_owned()
            } else {
                util::join_list(&s.excluded_dirs)
            }),
        ));
        col = col.push(Self::kv_row(
            tr.excluded_extensions,
            text(if s.excluded_extensions.is_empty() {
                "—".to_owned()
            } else {
                util::join_list(&s.excluded_extensions)
            }),
        ));
        col = col.push(Self::kv_row(
            tr.respect_gitignore,
            text(if s.respect_gitignore { tr.yes } else { tr.no }),
        ));
        col = col.push(Self::kv_row(
            tr.max_indexed_file_size,
            text(util::format_bytes(s.max_indexed_file_size)),
        ));
        col = col.push(Self::kv_row(
            tr.index_archives,
            text(if s.archives_enabled { tr.yes } else { tr.no }),
        ));
        if s.archives_enabled {
            col = col.push(Self::kv_row(
                tr.archive_max_depth,
                text(s.archive_max_depth.to_string()),
            ));
        }
        col.into()
    }

    fn summary_view<'a>(tr: &Strings, s: &'a BuildSummary) -> Element<'a, Message> {
        let mut col = column![].spacing(4);
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
        for (label, value) in [
            (tr.kind, kind),
            (tr.duration, util::format_duration(s.duration)),
            (tr.files_indexed, s.indexed_files.to_string()),
            (
                tr.top_extensions,
                if exts.is_empty() {
                    "—".to_owned()
                } else {
                    exts
                },
            ),
            (tr.ignored_by_extension, s.ignored_by_extension.to_string()),
            (tr.ignored_by_sniff, s.ignored_by_sniff.to_string()),
            (tr.too_large, s.too_large.to_string()),
            (tr.errors, s.errors.to_string()),
            (tr.security_limits, s.security_limits.to_string()),
            (tr.archives_processed, s.archives_processed.to_string()),
            (
                tr.archive_entries_indexed,
                s.archive_entries_indexed.to_string(),
            ),
            (
                tr.index_archives,
                if s.archives_included {
                    tr.yes.to_owned()
                } else {
                    tr.no.to_owned()
                },
            ),
        ] {
            col = col.push(Self::kv_row(label, text(value)));
        }
        col.into()
    }
}

/// Color of a project status marker.
fn status_color(status: super::Status) -> iced::Color {
    match status {
        super::Status::NeverBuilt => theme::NEUTRAL,
        super::Status::RebuildNeeded => theme::WARN,
        super::Status::UpToDate => theme::OK,
    }
}

/// The bold face used for headings.
fn bold() -> Font {
    Font {
        weight: iced::font::Weight::Bold,
        ..Font::DEFAULT
    }
}
