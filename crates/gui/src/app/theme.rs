//! Visual theme: two tuned palettes (dark/light) applied over egui's
//! defaults — accent color, widget shapes and spacing — so the app
//! reads as a real desktop application rather than a widget demo.

use eframe::egui;
use rsearch_catalog::ThemePreference;

/// Accent color shared by both themes (selections, focused actions).
pub const ACCENT: egui::Color32 = egui::Color32::from_rgb(0x3B, 0x82, 0xF6);
/// Accent color for filled/strong emphasis on light backgrounds.
pub const ACCENT_DEEP: egui::Color32 = egui::Color32::from_rgb(0x1D, 0x4E, 0xD8);

/// Resolves the preference against the OS theme.
fn dark_requested(ctx: &egui::Context, pref: ThemePreference) -> bool {
    match pref {
        ThemePreference::Dark => true,
        ThemePreference::Light => false,
        ThemePreference::System => ctx.system_theme() != Some(egui::Theme::Light),
    }
}

/// Applies the palette for `pref`. `applied` remembers which palette is
/// live so restyling only happens when the resolved theme changes.
pub fn apply(ctx: &egui::Context, pref: ThemePreference, applied: &mut Option<bool>) {
    let dark = dark_requested(ctx, pref);
    if *applied == Some(dark) {
        return;
    }
    *applied = Some(dark);
    ctx.set_theme(match pref {
        ThemePreference::System => egui::ThemePreference::System,
        ThemePreference::Light => egui::ThemePreference::Light,
        ThemePreference::Dark => egui::ThemePreference::Dark,
    });
    ctx.set_visuals(visuals(dark));
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(12.0, 6.0);
        style.spacing.indent = 20.0;
        style.spacing.scroll.bar_width = 10.0;
        style.spacing.scroll.floating = true;
    });
}

fn visuals(dark: bool) -> egui::Visuals {
    let mut v = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };

    if dark {
        v.panel_fill = egui::Color32::from_rgb(0x24, 0x24, 0x24);
        v.window_fill = egui::Color32::from_rgb(0x2B, 0x2B, 0x2B);
        v.extreme_bg_color = egui::Color32::from_rgb(0x1B, 0x1B, 0x1B);
        v.faint_bg_color = egui::Color32::from_rgb(0x2E, 0x2E, 0x2E);
        v.widgets.inactive.weak_bg_fill = egui::Color32::from_rgb(0x32, 0x32, 0x32);
        v.widgets.inactive.bg_fill = egui::Color32::from_rgb(0x3A, 0x3A, 0x3A);
        v.widgets.hovered.weak_bg_fill = egui::Color32::from_rgb(0x3C, 0x3C, 0x3C);
        v.widgets.hovered.bg_fill = egui::Color32::from_rgb(0x46, 0x46, 0x46);
        v.widgets.active.weak_bg_fill = egui::Color32::from_rgb(0x44, 0x44, 0x44);
        v.widgets.active.bg_fill = egui::Color32::from_rgb(0x50, 0x50, 0x50);
    } else {
        v.panel_fill = egui::Color32::from_rgb(0xF7, 0xF7, 0xF8);
        v.window_fill = egui::Color32::from_rgb(0xFF, 0xFF, 0xFF);
        v.extreme_bg_color = egui::Color32::from_rgb(0xEC, 0xEC, 0xEE);
        v.faint_bg_color = egui::Color32::from_rgb(0xF0, 0xF0, 0xF2);
        v.widgets.inactive.weak_bg_fill = egui::Color32::from_rgb(0xED, 0xED, 0xEF);
        v.widgets.inactive.bg_fill = egui::Color32::from_rgb(0xFF, 0xFF, 0xFF);
        v.widgets.hovered.weak_bg_fill = egui::Color32::from_rgb(0xE4, 0xE4, 0xE8);
        v.widgets.hovered.bg_fill = egui::Color32::from_rgb(0xF4, 0xF4, 0xF6);
        v.widgets.active.weak_bg_fill = egui::Color32::from_rgb(0xDC, 0xDC, 0xE2);
        v.widgets.active.bg_fill = egui::Color32::from_rgb(0xE8, 0xE8, 0xEE);
    }

    let rounding = egui::CornerRadius::same(6);
    for state in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        state.corner_radius = rounding;
    }
    v.window_corner_radius = egui::CornerRadius::same(10);
    v.menu_corner_radius = egui::CornerRadius::same(8);

    v.selection.bg_fill = ACCENT.linear_multiply(if dark { 0.42 } else { 0.22 });
    v.selection.stroke = egui::Stroke::new(1.0, ACCENT);
    v.hyperlink_color = if dark { ACCENT } else { ACCENT_DEEP };
    v.widgets.hovered.fg_stroke.color = if dark {
        egui::Color32::WHITE
    } else {
        egui::Color32::from_rgb(0x11, 0x11, 0x11)
    };

    v.window_stroke = egui::Stroke::new(
        1.0,
        if dark {
            egui::Color32::from_rgb(0x45, 0x45, 0x45)
        } else {
            egui::Color32::from_rgb(0xD8, 0xD8, 0xDC)
        },
    );
    v
}
