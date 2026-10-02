#![windows_subsystem = "windows"]

//! rsearch GUI entry point.

mod app;
mod editor;
mod tr;
mod util;

use eframe::egui;

use crate::app::RsearchApp;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(tr::EN.app_title)
            .with_inner_size([1024.0, 720.0])
            .with_min_inner_size([720.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        tr::EN.app_title,
        options,
        Box::new(|cc| Ok(Box::new(RsearchApp::new(cc)))),
    )
}
