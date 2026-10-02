//! The Preferences screen: application-wide settings, saved on every
//! change.
//!
//! Three distinct kinds of data are deliberately kept apart:
//! [`AppPreferences`] here (global), `ProjectSettings` on the Projects
//! screen (per project) and `SearchParams` behind saved searches (per
//! search). The `default_*` fields only seed *new* projects — editing
//! them never rewrites an existing project's settings.

use eframe::egui;
use rsearch_catalog::{AppPreferences, Language, ThemePreference};

use super::{Action, RsearchApp};
use crate::util;

/// Editable buffers for the list-valued preferences; synced from
/// [`AppPreferences`] when the screen is (re)entered.
#[derive(Default)]
pub struct PrefsScreen {
    dirs_text: String,
    exts_text: String,
    max_size_mb: u64,
    /// `false` means the buffers must be reloaded from `prefs`.
    synced: bool,
}

impl PrefsScreen {
    /// Marks the buffers stale (called when entering the screen or when
    /// preferences were reloaded externally).
    pub fn invalidate(&mut self) {
        self.synced = false;
    }

    fn sync(&mut self, prefs: &AppPreferences) {
        if self.synced {
            return;
        }
        self.dirs_text = prefs.default_excluded_dirs.join("\n");
        self.exts_text = util::join_list(&prefs.default_excluded_extensions);
        self.max_size_mb = prefs
            .default_max_indexed_file_size
            .div_ceil(1024 * 1024)
            .max(1);
        self.synced = true;
    }
}

impl RsearchApp {
    /// Saves `self.prefs` through the catalog; failures surface as a
    /// sticky error notice.
    fn save_prefs(&mut self) {
        if let Some(catalog) = &self.catalog {
            if let Err(e) = catalog.save_preferences(&self.prefs) {
                let msg = e.to_string();
                self.push_notice(
                    super::banner::BannerLevel::Error,
                    self.tr.prefs_save_failed(&msg),
                    true,
                );
            }
        }
    }

    /// Applies a changed preference value and persists it.
    fn prefs_changed(&mut self, changed: bool) {
        if changed {
            self.applied_theme = None;
            self.tr = crate::tr::for_language(self.prefs.language);
            self.save_prefs();
        }
    }

    pub(super) fn prefs_ui(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let tr = self.tr;
        self.prefs_screen.sync(&self.prefs);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(4.0);
                ui.weak(tr.prefs_autosave_note);
                ui.add_space(8.0);

                // -- Language & theme -----------------------------------
                Self::section(ui, tr.prefs_language, |ui| {
                    let mut language = self.prefs.language;
                    egui::ComboBox::from_id_salt("prefs_language")
                        .selected_text(language.native_name())
                        .width(160.0)
                        .show_ui(ui, |ui| {
                            for lang in Language::ALL {
                                ui.selectable_value(&mut language, lang, lang.native_name());
                            }
                        });
                    self.prefs_changed(language != self.prefs.language);
                    self.prefs.language = language;
                });
                Self::section(ui, tr.prefs_theme, |ui| {
                    let mut theme = self.prefs.theme;
                    egui::ComboBox::from_id_salt("prefs_theme")
                        .selected_text(match theme {
                            ThemePreference::System => tr.theme_system,
                            ThemePreference::Light => tr.theme_light,
                            ThemePreference::Dark => tr.theme_dark,
                        })
                        .width(160.0)
                        .show_ui(ui, |ui| {
                            for (value, label) in [
                                (ThemePreference::System, tr.theme_system),
                                (ThemePreference::Light, tr.theme_light),
                                (ThemePreference::Dark, tr.theme_dark),
                            ] {
                                ui.selectable_value(&mut theme, value, label);
                            }
                        });
                    self.prefs_changed(theme != self.prefs.theme);
                    self.prefs.theme = theme;
                });

                // -- Defaults for new projects ---------------------------
                egui::CollapsingHeader::new(
                    egui::RichText::new(tr.prefs_defaults_section).strong(),
                )
                .id_salt("prefs_defaults")
                .default_open(true)
                .show(ui, |ui| {
                    ui.weak(tr.prefs_defaults_note);
                    ui.add_space(6.0);
                    ui.label(tr.prefs_default_excluded_dirs);
                    let dirs_changed = ui
                        .add(
                            egui::TextEdit::multiline(&mut self.prefs_screen.dirs_text)
                                .desired_rows(4)
                                .desired_width(f32::INFINITY)
                                .hint_text(tr.excluded_dirs_hint),
                        )
                        .changed();
                    ui.add_space(4.0);
                    ui.label(tr.prefs_default_excluded_extensions);
                    let exts_changed = ui
                        .add(
                            egui::TextEdit::singleline(&mut self.prefs_screen.exts_text)
                                .desired_width(f32::INFINITY)
                                .hint_text(tr.excluded_extensions_hint),
                        )
                        .changed();
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label(tr.prefs_default_max_size);
                        let size_changed = ui
                            .add(
                                egui::DragValue::new(&mut self.prefs_screen.max_size_mb)
                                    .range(1..=1_048_576)
                                    .suffix(" MiB"),
                            )
                            .changed();
                        if size_changed {
                            self.prefs.default_max_indexed_file_size =
                                self.prefs_screen.max_size_mb.saturating_mul(1024 * 1024);
                        }
                        if size_changed {
                            self.save_prefs();
                        }
                    });
                    if dirs_changed {
                        self.prefs.default_excluded_dirs =
                            util::parse_list(&self.prefs_screen.dirs_text);
                        self.save_prefs();
                    }
                    if exts_changed {
                        self.prefs.default_excluded_extensions =
                            util::parse_extensions(&self.prefs_screen.exts_text);
                        self.save_prefs();
                    }
                });

                // -- Updates ---------------------------------------------
                Self::section(ui, tr.prefs_updates_section, |ui| {
                    let check_updates = ui
                        .checkbox(&mut self.prefs.check_for_updates, tr.prefs_check_updates)
                        .changed();
                    if check_updates {
                        self.save_prefs();
                    }
                    if ui.button(tr.prefs_check_now).clicked() {
                        actions.push(Action::CheckUpdates);
                    }
                });
            });
    }

    /// A titled section — a heading followed by its content.
    fn section(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
        ui.add_space(6.0);
        ui.label(egui::RichText::new(title).strong());
        ui.add_space(2.0);
        body(ui);
        ui.add_space(6.0);
        ui.separator();
    }
}
