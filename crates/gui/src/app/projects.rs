//! The Projects screen: list on the left, details on the right —
//! status, build/update controls, live progress, settings and the
//! last build summary.
//!
//! Widgets only collect [`Action`]s; every catalog or engine call is
//! applied afterwards in [`super::RsearchApp::apply`].

use eframe::egui;
use rsearch_catalog::{Project, ProjectSettings};
use rsearch_engine::{BuildKind, BuildSummary, ProgressSnapshot};

use super::{theme, Action, RsearchApp};
use crate::tr::Strings;
use crate::util;

impl RsearchApp {
    pub(super) fn projects_ui(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        egui::Panel::left("project_list")
            .resizable(true)
            .default_size(260.0)
            .size_range(200.0..=400.0)
            .show(ui, |ui| {
                self.project_list_ui(ui, actions);
            });

        egui::ScrollArea::vertical()
            .id_salt("project_detail")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(4.0);
                match self.selected_project().cloned() {
                    Some(project) => self.project_details_ui(ui, &project, actions),
                    None => {
                        ui.vertical_centered(|ui| {
                            ui.add_space(120.0);
                            ui.weak(self.tr.select_project_hint);
                        });
                    }
                }
            });
    }

    /// The project list: one selectable row per project with a status
    /// line, plus the New project button on top.
    fn project_list_ui(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let tr = self.tr;
        ui.horizontal(|ui| {
            ui.heading(tr.projects);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(self.catalog.is_some(), egui::Button::new(tr.new_project))
                    .clicked()
                {
                    actions.push(Action::NewProject);
                }
            });
        });
        ui.add_space(4.0);
        ui.separator();

        if self.projects.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(60.0);
                ui.weak(tr.no_projects_hint);
            });
            return;
        }
        egui::ScrollArea::vertical()
            .id_salt("project_rows")
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for p in &self.projects {
                    let status = self.status(p);
                    let is_selected = self.selected.as_deref() == Some(p.id.as_str());
                    let row = egui::Button::selectable(
                        is_selected,
                        egui::RichText::new(&p.name).strong(),
                    )
                    .min_size(egui::vec2(ui.available_width(), 22.0));
                    if ui.add(row).clicked() {
                        actions.push(Action::Select(p.id.clone()));
                    }
                    let date = p
                        .last_build_at
                        .map(util::format_unix)
                        .unwrap_or_else(|| "—".to_owned());
                    ui.horizontal(|ui| {
                        ui.add_space(14.0);
                        ui.colored_label(status.color(), status.text(tr));
                        ui.weak(format!("· {date}"));
                    });
                    ui.add_space(6.0);
                }
            });
    }

    /// Header, actions, live build progress, settings and last build
    /// summary of the selected project.
    fn project_details_ui(
        &mut self,
        ui: &mut egui::Ui,
        project: &Project,
        actions: &mut Vec<Action>,
    ) {
        let tr = self.tr;
        let status = self.status(project);
        let busy = self.build.is_some();

        ui.horizontal(|ui| {
            ui.heading(&project.name);
            ui.colored_label(status.color(), format!("({})", status.text(tr)));
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            // A first build is `rebuild_index`; afterwards
            // `update_index` (which falls back to a full rebuild by
            // itself when needed).
            let label = if project.last_build_settings.is_some() && project.index_db_path.exists() {
                tr.update_index
            } else {
                tr.build_index
            };
            let primary = egui::Button::new(egui::RichText::new(label).strong())
                .fill(theme::ACCENT)
                .min_size(egui::vec2(120.0, 28.0));
            if ui.add_enabled(!busy, primary).clicked() {
                actions.push(Action::StartBuild(project.id.clone()));
            }
            if ui.add_enabled(!busy, egui::Button::new(tr.edit)).clicked() {
                actions.push(Action::Edit(project.id.clone()));
            }
            if ui
                .add_enabled(!busy, egui::Button::new(tr.delete))
                .clicked()
            {
                actions.push(Action::AskDelete(project.id.clone()));
            }
        });
        ui.add_space(8.0);

        if let Some(active) = self.build.as_ref().filter(|b| b.project_id == project.id) {
            let snap = active.handle.progress().snapshot();
            egui::Frame::new()
                .fill(ui.visuals().faint_bg_color)
                .corner_radius(egui::CornerRadius::same(8))
                .inner_margin(egui::Margin::same(12))
                .show(ui, |ui| {
                    Self::build_progress_ui(ui, tr, &snap, actions);
                });
            ui.add_space(8.0);
        }

        egui::CollapsingHeader::new(tr.settings_section)
            .id_salt("project_settings")
            .default_open(true)
            .show(ui, |ui| {
                Self::settings_ui(ui, tr, &project.settings);
            });

        match &project.last_build_summary {
            Some(summary) => {
                egui::CollapsingHeader::new(tr.last_build)
                    .id_salt("last_build")
                    .default_open(true)
                    .show(ui, |ui| {
                        Self::summary_ui(ui, tr, summary);
                    });
            }
            None => {
                ui.add_space(4.0);
                ui.weak(tr.status_never_built);
            }
        }
    }

    fn build_progress_ui(
        ui: &mut egui::Ui,
        tr: &Strings,
        snap: &ProgressSnapshot,
        actions: &mut Vec<Action>,
    ) {
        ui.horizontal(|ui| {
            if !snap.phase.is_some_and(|p| p.is_terminal()) {
                ui.spinner();
            }
            ui.label(
                snap.phase
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| tr.starting.to_owned()),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(tr.cancel_build).clicked() {
                    actions.push(Action::CancelBuild);
                }
            });
        });
        egui::Grid::new(ui.id().with("progress"))
            .num_columns(4)
            .spacing([24.0, 4.0])
            .show(ui, |ui| {
                let pairs = [
                    (tr.files_seen, snap.files_seen.to_string()),
                    (tr.files_indexed, snap.files_indexed.to_string()),
                    (tr.files_ignored, snap.files_ignored.to_string()),
                    (tr.errors, snap.errors.to_string()),
                    (tr.archives, snap.archives.to_string()),
                    (tr.archive_entries, snap.archive_entries.to_string()),
                    (tr.bytes_read, util::format_bytes(snap.bytes_read)),
                ];
                for (i, (label, value)) in pairs.iter().enumerate() {
                    if i > 0 && i % 2 == 0 {
                        ui.end_row();
                    }
                    ui.weak(*label);
                    ui.label(value);
                }
                ui.end_row();
            });
    }

    fn settings_ui(ui: &mut egui::Ui, tr: &Strings, s: &ProjectSettings) {
        egui::Grid::new(ui.id().with("settings"))
            .num_columns(2)
            .spacing([32.0, 4.0])
            .show(ui, |ui| {
                ui.weak(tr.source_roots);
                ui.vertical(|ui| {
                    for root in &s.roots {
                        ui.label(format!(
                            "{}  ({})",
                            root.path.display(),
                            if root.recursive {
                                tr.root_recursive
                            } else {
                                tr.root_top_level_only
                            }
                        ));
                    }
                });
                ui.end_row();
                ui.weak(tr.excluded_dirs);
                ui.label(if s.excluded_dirs.is_empty() {
                    "—".to_owned()
                } else {
                    util::join_list(&s.excluded_dirs)
                });
                ui.end_row();
                ui.weak(tr.excluded_extensions);
                ui.label(if s.excluded_extensions.is_empty() {
                    "—".to_owned()
                } else {
                    util::join_list(&s.excluded_extensions)
                });
                ui.end_row();
                ui.weak(tr.respect_gitignore);
                ui.label(if s.respect_gitignore { tr.yes } else { tr.no });
                ui.end_row();
                ui.weak(tr.max_indexed_file_size);
                ui.label(util::format_bytes(s.max_indexed_file_size));
                ui.end_row();
                ui.weak(tr.index_archives);
                ui.label(if s.archives_enabled { tr.yes } else { tr.no });
                ui.end_row();
                if s.archives_enabled {
                    ui.weak(tr.archive_max_depth);
                    ui.label(s.archive_max_depth.to_string());
                    ui.end_row();
                }
            });
    }

    fn summary_ui(ui: &mut egui::Ui, tr: &Strings, s: &BuildSummary) {
        egui::Grid::new(ui.id().with("summary"))
            .num_columns(2)
            .spacing([32.0, 4.0])
            .show(ui, |ui| {
                let row = |ui: &mut egui::Ui, label: &str, value: String| {
                    ui.weak(label);
                    ui.label(value);
                    ui.end_row();
                };
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
                row(ui, tr.kind, kind);
                row(ui, tr.duration, util::format_duration(s.duration));
                row(ui, tr.files_indexed, s.indexed_files.to_string());
                let exts = s
                    .top_extensions
                    .iter()
                    .map(|(e, n)| format!("{e} ({n})"))
                    .collect::<Vec<_>>()
                    .join(", ");
                row(
                    ui,
                    tr.top_extensions,
                    if exts.is_empty() {
                        "—".to_owned()
                    } else {
                        exts
                    },
                );
                row(
                    ui,
                    tr.ignored_by_extension,
                    s.ignored_by_extension.to_string(),
                );
                row(ui, tr.ignored_by_sniff, s.ignored_by_sniff.to_string());
                row(ui, tr.too_large, s.too_large.to_string());
                row(ui, tr.errors, s.errors.to_string());
                row(ui, tr.security_limits, s.security_limits.to_string());
                row(ui, tr.archives_processed, s.archives_processed.to_string());
                row(
                    ui,
                    tr.archive_entries_indexed,
                    s.archive_entries_indexed.to_string(),
                );
                row(
                    ui,
                    tr.index_archives,
                    if s.archives_included {
                        tr.yes.to_owned()
                    } else {
                        tr.no.to_owned()
                    },
                );
            });
    }
}
