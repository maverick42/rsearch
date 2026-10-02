//! The Preferences screen: application-wide settings, saved on every
//! change.
//!
//! Three distinct kinds of data are deliberately kept apart:
//! [`AppPreferences`] here (global), `ProjectSettings` on the Projects
//! screen (per project) and `SearchParams` behind saved searches (per
//! search). The `default_*` fields only seed *new* projects — editing
//! them never rewrites an existing project's settings.

use std::fmt;

use iced::widget::{
    button, checkbox, column, container, pick_list, row, rule, scrollable, text, text_editor,
    text_input,
};
use iced::{Alignment, Element, Fill};
use rsearch_catalog::{AppPreferences, Language, ThemePreference};

use super::{banner::BannerLevel, theme, Message, RsearchApp};
use crate::util;

/// Editable buffers for the list-valued preferences; synced from
/// [`AppPreferences`] when the screen is (re)entered.
pub struct PrefsScreen {
    /// Multiline editor for `default_excluded_dirs`.
    dirs_text: text_editor::Content,
    exts_text: String,
    /// Display buffer for the MiB value; `prefs` only updates on a
    /// valid parse.
    max_size_text: String,
    /// Whether the "defaults" section is expanded.
    defaults_open: bool,
    /// `false` means the buffers must be reloaded from `prefs`.
    synced: bool,
}

impl Default for PrefsScreen {
    fn default() -> Self {
        Self {
            dirs_text: text_editor::Content::new(),
            exts_text: String::new(),
            max_size_text: String::new(),
            defaults_open: true,
            synced: false,
        }
    }
}

impl PrefsScreen {
    /// Marks the buffers stale (called when entering the screen or when
    /// preferences were reloaded externally).
    pub fn invalidate(&mut self) {
        self.synced = false;
    }

    /// Toggles the defaults section.
    pub fn toggle_defaults(&mut self) {
        self.defaults_open = !self.defaults_open;
    }

    /// Applies an edit to the dirs buffer and returns the parsed list.
    pub fn edit_dirs(&mut self, action: text_editor::Action) -> Vec<String> {
        self.dirs_text.perform(action);
        util::parse_list(&self.dirs_text.text())
    }

    /// Applies an edit to the extensions buffer and returns the parsed
    /// list.
    pub fn edit_exts(&mut self, text: String) -> Vec<String> {
        self.exts_text = text;
        util::parse_extensions(&self.exts_text)
    }

    /// Updates the max-size buffer; returns the new byte value when the
    /// text parses, `None` when the buffer holds an invalid value (the
    /// preference keeps its previous value).
    pub fn edit_max_size(&mut self, text: String) -> Option<u64> {
        self.max_size_text = text;
        self.max_size_text
            .trim()
            .parse::<u64>()
            .ok()
            .map(|mb| mb.max(1).saturating_mul(1024 * 1024))
    }

    /// Reloads the buffers from `prefs` when stale — called from
    /// `update`, never from `view` (which borrows immutably).
    pub(super) fn sync(&mut self, prefs: &AppPreferences) {
        if self.synced {
            return;
        }
        self.dirs_text = text_editor::Content::with_text(&prefs.default_excluded_dirs.join("\n"));
        self.exts_text = util::join_list(&prefs.default_excluded_extensions);
        self.max_size_text = prefs
            .default_max_indexed_file_size
            .div_ceil(1024 * 1024)
            .max(1)
            .to_string();
        self.synced = true;
    }
}

/// A language entry in the picker, displayed by its native name.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LangChoice(Language);

impl fmt::Display for LangChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.native_name())
    }
}

/// A theme entry in the picker, displayed by its localized label.
#[derive(Debug, Clone, PartialEq)]
struct ThemeChoice(ThemePreference, &'static str);

impl fmt::Display for ThemeChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.1)
    }
}

impl RsearchApp {
    /// Saves `self.prefs` through the catalog; failures surface as a
    /// sticky error notice.
    pub(super) fn save_prefs(&mut self) {
        if let Some(catalog) = &self.catalog {
            if let Err(e) = catalog.save_preferences(&self.prefs) {
                let msg = e.to_string();
                self.push_notice(BannerLevel::Error, self.tr.prefs_save_failed(&msg), true);
            }
        }
    }

    /// Applies a changed preference value and persists it.
    pub(super) fn prefs_changed(&mut self) {
        self.tr = crate::tr::for_language(self.prefs.language);
        self.save_prefs();
    }

    pub(super) fn prefs_view(&self) -> Element<'_, Message> {
        let tr = self.tr;

        let langs: Vec<LangChoice> = Language::ALL.iter().copied().map(LangChoice).collect();
        let themes: Vec<ThemeChoice> = [
            (ThemePreference::System, tr.theme_system),
            (ThemePreference::Light, tr.theme_light),
            (ThemePreference::Dark, tr.theme_dark),
        ]
        .into_iter()
        .map(|(p, l)| ThemeChoice(p, l))
        .collect();

        let mut col = column![
            text(tr.prefs_autosave_note).style(theme::weak).size(13.0),
            Self::section(
                tr.prefs_language,
                pick_list(langs, Some(LangChoice(self.prefs.language)), |c| {
                    Message::SetLanguage(c.0)
                })
                .width(200.0)
                .into(),
            ),
            Self::section(
                tr.prefs_theme,
                pick_list(
                    themes.clone(),
                    themes.iter().find(|t| t.0 == self.prefs.theme).cloned(),
                    |c| Message::SetTheme(c.0),
                )
                .width(200.0)
                .into(),
            ),
        ]
        .spacing(4);

        // -- Defaults for new projects ---------------------------------
        let defaults_body: Element<'_, Message> = column![
            text(tr.prefs_defaults_note).style(theme::weak).size(13.0),
            text(tr.prefs_default_excluded_dirs),
            text_editor(&self.prefs_screen.dirs_text)
                .placeholder(tr.excluded_dirs_hint)
                .height(96.0)
                .on_action(Message::PrefDirs),
            text(tr.prefs_default_excluded_extensions),
            text_input(tr.excluded_extensions_hint, &self.prefs_screen.exts_text)
                .width(Fill)
                .on_input(Message::PrefExtensions),
            row![
                text(tr.prefs_default_max_size),
                text_input("0", &self.prefs_screen.max_size_text)
                    .width(120.0)
                    .on_input(Message::PrefMaxSize),
                text("MiB").style(theme::weak),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        ]
        .spacing(6)
        .into();

        col = col.push(Self::collapsible_section(
            tr.prefs_defaults_section,
            self.prefs_screen.defaults_open,
            Message::TogglePrefDefaults,
            defaults_body,
        ));

        col = col.push(Self::section(
            tr.prefs_updates_section,
            column![
                checkbox(self.prefs.check_for_updates)
                    .label(tr.prefs_check_updates)
                    .on_toggle(Message::PrefCheckUpdates),
                button(text(tr.prefs_check_now)).on_press(Message::CheckUpdates),
            ]
            .spacing(8)
            .into(),
        ));

        scrollable(col.spacing(8))
            .direction(iced::widget::scrollable::Direction::Vertical(
                iced::widget::scrollable::Scrollbar::new().width(8),
            ))
            .height(Fill)
            .into()
    }

    /// A titled section — a heading, its content, then a separator.
    fn section<'a>(title: &'a str, body: Element<'a, Message>) -> Element<'a, Message> {
        column![
            text(title).font(bold()),
            container(body).padding(iced::Padding::ZERO.left(4.0).top(2.0)),
            rule::horizontal(1),
        ]
        .spacing(6)
        .into()
    }

    /// A collapsible section: clickable heading plus optional body.
    fn collapsible_section<'a>(
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
        col.push(rule::horizontal(1)).into()
    }
}

/// The bold face used for section titles.
fn bold() -> iced::Font {
    iced::Font {
        weight: iced::font::Weight::Bold,
        ..iced::Font::DEFAULT
    }
}
