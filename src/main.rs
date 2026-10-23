//! Video Compressor — точка входа Rust-версии (eframe/egui).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod core;
mod settings;

use eframe::egui;

fn main() -> eframe::Result<()> {
    let settings = settings::load();
    let (width, height) = match (settings.width, settings.height) {
        (w, h) if w >= 400 && h >= 300 => (w as f32, h as f32),
        _ => (960.0, 680.0),
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(format!(
                "Video Compressor RS {}",
                env!("CARGO_PKG_VERSION")
            ))
            .with_inner_size([width.max(760.0), height.max(560.0)])
            .with_min_inner_size([760.0, 560.0])
            .with_resizable(true),
        ..Default::default()
    };

    eframe::run_native(
        "Video Compressor",
        options,
        Box::new(|cc| Ok(Box::new(app::VcApp::new(cc)))),
    )
}
