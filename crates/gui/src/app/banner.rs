//! Contextual banners shown at the top of the content area.
//!
//! The *model* is produced in one place ([`super::RsearchApp::banners`]);
//! this module only knows how to draw a [`Banner`] and report clicks.
//! Banners are contextual: they appear while their condition holds and
//! disappear when it no longer does — they never block the UI.

use eframe::egui;

use super::Action;
use crate::tr::Strings;

/// Severity of a banner — drives its accent color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BannerLevel {
    Info,
    Success,
    Warning,
    Error,
}

impl BannerLevel {
    fn accent(self) -> egui::Color32 {
        match self {
            BannerLevel::Info => egui::Color32::from_rgb(0x3B, 0x82, 0xF6),
            BannerLevel::Success => egui::Color32::from_rgb(0x22, 0xA3, 0x55),
            BannerLevel::Warning => egui::Color32::from_rgb(0xD9, 0xA0, 0x00),
            BannerLevel::Error => egui::Color32::from_rgb(0xE0, 0x45, 0x45),
        }
    }
}

/// One banner line: text, an optional action button, an optional
/// dismiss control and an optional spinner for ongoing work.
#[derive(Clone)]
pub struct Banner {
    pub level: BannerLevel,
    pub text: String,
    /// (label, action) for the trailing button, when relevant.
    pub action: Option<(String, Action)>,
    /// Index into the app's notice list — `Some` makes the banner
    /// dismissible with a close button.
    pub dismiss: Option<usize>,
    /// Whether to draw a spinner before the text.
    pub spinner: bool,
}

/// Draws one banner; pushes onto `actions` when the user clicked its
/// button or close control.
pub fn show(ui: &mut egui::Ui, tr: &Strings, banner: &Banner, actions: &mut Vec<Action>) {
    let accent = banner.level.accent();
    let dark = ui.visuals().dark_mode;
    let fill = accent.linear_multiply(if dark { 0.14 } else { 0.08 });
    let stroke = accent.linear_multiply(if dark { 0.45 } else { 0.30 });

    egui::Frame::new()
        .fill(fill)
        .stroke(egui::Stroke::new(1.0, stroke))
        .corner_radius(egui::CornerRadius::same(8))
        .inner_margin(egui::Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let height = ui.spacing().interact_size.y;
                let (bar, _) =
                    ui.allocate_exact_size(egui::vec2(3.0, height), egui::Sense::hover());
                ui.painter()
                    .rect_filled(bar, egui::CornerRadius::same(2), accent);
                if banner.spinner {
                    ui.spinner();
                }
                ui.add(egui::Label::new(&banner.text).wrap());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(index) = banner.dismiss {
                        if ui
                            .add(egui::Button::new("✕").frame(false))
                            .on_hover_text(tr.dismiss)
                            .clicked()
                        {
                            actions.push(Action::DismissNotice(index));
                        }
                    }
                    if let Some((label, action)) = &banner.action {
                        if ui.add(egui::Button::new(label).small()).clicked() {
                            actions.push(action.clone());
                        }
                    }
                });
            });
        });
}
