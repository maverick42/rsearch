#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! rsearch GUI entry point (Slint).

mod app;
mod editor;
mod results;
mod shell_open;
mod tr;
mod ui;
mod util;
mod viewer;

fn main() -> Result<(), slint::PlatformError> {
    ui::run()
}
