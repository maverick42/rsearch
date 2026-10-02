#![windows_subsystem = "windows"]

//! rsearch GUI entry point.

mod app;
mod editor;
mod tr;
mod util;

use crate::app::RsearchApp;
use iced::{window, Size};

fn main() -> iced::Result {
    iced::application(RsearchApp::boot, RsearchApp::update, RsearchApp::view)
        .title(RsearchApp::title)
        .theme(RsearchApp::theme)
        .subscription(RsearchApp::subscription)
        .window(window::Settings {
            size: Size::new(1024.0, 720.0),
            min_size: Some(Size::new(720.0, 480.0)),
            ..Default::default()
        })
        .run()
}
