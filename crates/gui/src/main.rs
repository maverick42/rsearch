#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! rsearch GUI entry point (Slint).

mod app;
mod editor;
mod results;
mod results_view;
mod shell_open;
mod tr;
mod ui;
mod util;
mod viewer;

fn main() -> Result<(), slint::PlatformError> {
    // A GUI crash is otherwise invisible: the panic text lands in a
    // log file and a message box instead of a closed console.
    std::panic::set_hook(Box::new(|info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        let report = format!("{info}\n\n{backtrace}");
        let log = std::env::temp_dir().join("rsearch-panic.log");
        let _ = std::fs::write(&log, &report);
        eprintln!("{report}");
        let _ = rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Error)
            .set_title("rsearch crashed")
            .set_description(format!("{info}\n\nDetails written to {}", log.display()))
            .set_buttons(rfd::MessageButtons::Ok)
            .show();
    }));
    ui::run()
}
