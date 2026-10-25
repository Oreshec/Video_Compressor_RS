//! Video Compressor — точка входа Rust-версии (eframe/egui).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod settings;

use eframe::egui;

/// `%APPDATA%\VideoCompressorRS\panic.log` — рядом с настройками.
fn super_or_settings_panic_log() -> std::path::PathBuf {
    let settings_path = settings::settings_path();
    match settings_path.parent() {
        Some(parent) => parent.join("panic.log"),
        None => std::env::temp_dir().join("VideoCompressorRS_panic.log"),
    }
}

fn main() -> eframe::Result<()> {
    // Паники не теряются: релизная сборка без консоли, поэтому пишем в файл.
    std::panic::set_hook(Box::new(|info| {
        let msg = format!("panic: {info}\n");
        eprint!("{msg}");
        let log = super_or_settings_panic_log();
        if let Some(parent) = log.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .and_then(|mut f| std::io::Write::write_all(&mut f, msg.as_bytes()));
    }));

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
