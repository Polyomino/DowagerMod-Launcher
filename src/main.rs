// GUI app: no console window in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod civ;
mod config;
mod git;
mod log;
mod report;

use app::LauncherApp;

fn main() -> eframe::Result<()> {
    log::start_session();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            // Tall enough for header + status card + full branch list +
            // Civ section + log with no outer scrolling.
            .with_inner_size([760.0, 880.0])
            .with_min_inner_size([620.0, 460.0])
            .with_title("DowagerMod Launcher"),
        ..Default::default()
    };
    eframe::run_native(
        "DowagerMod Launcher",
        options,
        Box::new(|cc| Ok(Box::new(LauncherApp::new(cc)))),
    )
}
