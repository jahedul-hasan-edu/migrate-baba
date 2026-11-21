#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod sql_highlight;
mod store;
mod theme;
mod views;

use eframe::egui;

fn main() -> eframe::Result {
    // Prefer the platform-native GPU API. Some Vulkan drivers (notably older Intel ones on Windows)
    // crash during device creation; DX12 / Metal are the most robust choices. Respect a user override.
    if std::env::var_os("WGPU_BACKEND").is_none() {
        let backend = if cfg!(windows) {
            Some("dx12")
        } else if cfg!(target_os = "macos") {
            Some("metal")
        } else {
            None
        };
        if let Some(b) = backend {
            // SAFETY: called at the very start of `main`, before any other thread exists.
            unsafe { std::env::set_var("WGPU_BACKEND", b) };
        }
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Migrate Baba — PostgreSQL compare & migrate")
            .with_inner_size([1360.0, 860.0])
            .with_min_inner_size([960.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native("Migrate Baba", options, Box::new(|cc| Ok(Box::new(app::App::new(cc)))))
}
