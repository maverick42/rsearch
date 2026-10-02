//! Visual theme: two custom `Palette`s (dark/light) mapped into a
//! single `Theme::custom` each, plus the shared style helpers (sidebar,
//! cards, banners, nav items) so the app reads as a real desktop
//! application rather than a widget demo.

use iced::border;
use iced::theme::{self, Palette};
use iced::widget::{button, container, text};
use iced::{Background, Color, Shadow, Theme, Vector};
use rsearch_catalog::ThemePreference;

/// Accent color shared by both themes (selections, primary actions).
pub const ACCENT: Color = Color::from_rgb8(0x3B, 0x82, 0xF6);
/// Neutral gray — the "never built" status marker.
pub const NEUTRAL: Color = Color::from_rgb8(0x80, 0x80, 0x80);
/// Amber — rebuild-needed / warning.
pub const WARN: Color = Color::from_rgb8(0xD9, 0xA0, 0x00);
/// Green — up-to-date / success.
pub const OK: Color = Color::from_rgb8(0x22, 0xA3, 0x55);
/// Red — errors.
pub const ERR: Color = Color::from_rgb8(0xE0, 0x45, 0x45);

/// Resolves the preference against the OS-reported mode and returns the
/// [`Theme`] iced applies this frame. `System` follows the platform;
/// an unreported mode falls back to the dark palette, matching the
/// previous behavior.
pub fn resolve(pref: ThemePreference, system: theme::Mode) -> Theme {
    let dark = match pref {
        ThemePreference::Dark => true,
        ThemePreference::Light => false,
        ThemePreference::System => system != theme::Mode::Light,
    };
    if dark {
        dark_theme()
    } else {
        light_theme()
    }
}

fn dark_theme() -> Theme {
    Theme::custom(
        "rsearch-dark",
        Palette {
            background: Color::from_rgb8(0x24, 0x24, 0x24),
            text: Color::from_rgb8(0xE8, 0xE8, 0xE8),
            primary: ACCENT,
            success: OK,
            warning: WARN,
            danger: ERR,
        },
    )
}

fn light_theme() -> Theme {
    Theme::custom(
        "rsearch-light",
        Palette {
            background: Color::from_rgb8(0xF7, 0xF7, 0xF8),
            text: Color::from_rgb8(0x11, 0x11, 0x11),
            primary: ACCENT,
            success: OK,
            warning: WARN,
            danger: ERR,
        },
    )
}

/// `color` with its alpha replaced by `alpha` — the tinted fills used
/// by banners and hover states.
pub fn tinted(color: Color, alpha: f32) -> Color {
    Color { a: alpha, ..color }
}

/// The left navigation strip.
pub fn sidebar(theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(
            theme.extended_palette().background.weakest.color,
        )),
        ..container::Style::default()
    }
}

/// A raised surface: dialogs and cards, rounded with a soft border and
/// a light shadow.
pub fn card(theme: &Theme) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(Background::Color(palette.background.weak.color)),
        border: border::rounded(10)
            .color(palette.background.strong.color)
            .width(1),
        shadow: Shadow {
            color: Color {
                a: 0.30,
                ..Color::BLACK
            },
            offset: Vector::new(0.0, 4.0),
            blur_radius: 16.0,
        },
        ..container::Style::default()
    }
}

/// A quiet inset surface (build progress panel, groups).
pub fn subtle(theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(
            theme.extended_palette().background.weaker.color,
        )),
        border: border::rounded(8),
        ..container::Style::default()
    }
}

/// A banner tinted by `accent`: translucent fill and matching border.
pub fn banner(accent: Color) -> impl Fn(&Theme) -> container::Style {
    move |theme| {
        let dark = theme.extended_palette().is_dark;
        container::Style {
            background: Some(Background::Color(tinted(
                accent,
                if dark { 0.14 } else { 0.08 },
            ))),
            border: border::rounded(8)
                .color(tinted(accent, if dark { 0.45 } else { 0.30 }))
                .width(1),
            ..container::Style::default()
        }
    }
}

/// A left navigation entry: accent fill when active, hover tint
/// otherwise, plain text color.
pub fn nav_button(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        if selected {
            let base = button::primary(theme, status);
            button::Style {
                border: base.border.rounded(8),
                ..base
            }
        } else {
            let palette = theme.extended_palette();
            let base = button::text(theme, status);
            match status {
                button::Status::Hovered => button::Style {
                    background: Some(Background::Color(palette.background.weak.color)),
                    text_color: theme.palette().text,
                    border: border::rounded(8),
                    ..base
                },
                button::Status::Pressed => button::Style {
                    background: Some(Background::Color(palette.background.strong.color)),
                    text_color: theme.palette().text,
                    border: border::rounded(8),
                    ..base
                },
                _ => base,
            }
        }
    }
}

/// A full-width list row (project entries, occurrences): transparent at
/// rest, tinted when hovered or selected.
pub fn list_row(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let palette = theme.extended_palette();
        let base = button::text(theme, status);
        let fill = if selected {
            Some(Background::Color(tinted(ACCENT, 0.20)))
        } else {
            match status {
                button::Status::Hovered => Some(Background::Color(palette.background.weak.color)),
                button::Status::Pressed => Some(Background::Color(palette.background.strong.color)),
                _ => None,
            }
        };
        button::Style {
            background: fill,
            text_color: theme.palette().text,
            border: border::rounded(6),
            ..base
        }
    }
}

/// A file-group header in the result list: subtle fill at rest so the
/// groups read as cards, accent tint on hover.
pub fn group_header() -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let palette = theme.extended_palette();
        let base = button::text(theme, status);
        let fill = match status {
            button::Status::Hovered => palette.background.weak.color,
            button::Status::Pressed => palette.background.strong.color,
            _ => palette.background.weaker.color,
        };
        button::Style {
            background: Some(Background::Color(fill)),
            text_color: theme.palette().text,
            border: border::rounded(6),
            ..base
        }
    }
}

/// Weaker (secondary) text.
pub fn weak(theme: &Theme) -> text::Style {
    text::secondary(theme)
}
