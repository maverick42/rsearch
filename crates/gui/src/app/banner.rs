//! Contextual banners shown at the top of the content area.
//!
//! The *model* is produced in one place ([`super::RsearchApp::banners`]);
//! this module only knows how to draw a [`Banner`]. Banners are
//! contextual: they appear while their condition holds and disappear
//! when it no longer does — they never block the UI.

use iced::widget::{button, container, row, space, text, tooltip};
use iced::{border, Alignment, Background, Element, Fill};

use super::{theme, Message};
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
    pub fn accent(self) -> iced::Color {
        match self {
            BannerLevel::Info => theme::ACCENT,
            BannerLevel::Success => theme::OK,
            BannerLevel::Warning => theme::WARN,
            BannerLevel::Error => theme::ERR,
        }
    }
}

/// One banner line: text, an optional action button, an optional
/// dismiss control and an optional in-progress marker for ongoing work.
#[derive(Clone)]
pub struct Banner {
    pub level: BannerLevel,
    pub text: String,
    /// (label, message) for the trailing button, when relevant.
    pub action: Option<(String, Message)>,
    /// Index into the app's notice list — `Some` makes the banner
    /// dismissible with a close button.
    pub dismiss: Option<usize>,
    /// Whether the banner reports ongoing work.
    pub spinner: bool,
}

/// Renders one banner row. The banner is produced per frame and
/// dropped afterwards, so all rendered content is owned/`'static`.
pub fn view(banner: &Banner, tr: &Strings) -> Element<'static, Message> {
    let accent = banner.level.accent();

    let mut content =
        row![
            container(space().width(3.0).height(18.0)).style(move |_| container::Style {
                background: Some(Background::Color(accent)),
                border: border::rounded(2),
                ..container::Style::default()
            }),
        ]
        .spacing(10)
        .align_y(Alignment::Center);

    if banner.spinner {
        content = content.push(text("●").color(accent).size(11.0));
    }
    content = content.push(text(banner.text.clone()).width(Fill));
    if let Some((label, message)) = &banner.action {
        content = content.push(
            button(text(label.clone()))
                .padding([4.0, 10.0])
                .on_press(message.clone()),
        );
    }
    if let Some(index) = banner.dismiss {
        let close = button(text("✕"))
            .padding([4.0, 8.0])
            .style(button::text)
            .on_press(Message::DismissNotice(index));
        content = content.push(tooltip(close, tr.dismiss, tooltip::Position::Left));
    }

    container(content)
        .width(Fill)
        .padding([8.0, 10.0])
        .style(theme::banner(accent))
        .into()
}
