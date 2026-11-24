#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod jobs;
#[cfg(debug_assertions)]
mod shot;
mod sql_highlight;
mod store;
mod theme;
mod views;
mod worker;

use eframe::egui;

fn main() -> eframe::Result {
    // Headless background run of a data migration (started by the app, outlives the window).
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some(jobs::ARG) {
        let code = args.next().map(|id| worker::main(&id)).unwrap_or(2);
        std::process::exit(code);
    }

    // Renderer: on Windows the GL backend uses a fraction of DX12's memory (~25 MB vs ~290 MB
    // private) for this UI; if GL can't start we relaunch on DX12. Vulkan is avoided because some
    // drivers crash during device creation. A user-set WGPU_BACKEND always wins.
    let ours = std::env::var_os("WGPU_BACKEND").is_none();
    if ours {
        let backend = if cfg!(windows) {
            Some("gl")
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
            .with_title("Ferry")
            .with_inner_size([1360.0, 860.0])
            .with_min_inner_size([1040.0, 640.0]),
        ..Default::default()
    };
    let r = eframe::run_native("Ferry", options, Box::new(|cc| Ok(Box::new(app::App::new(cc)))));
    if r.is_err() && ours && cfg!(windows) {
        if let Ok(exe) = std::env::current_exe() {
            if std::process::Command::new(exe).env("WGPU_BACKEND", "dx12").spawn().is_ok() {
                return Ok(());
            }
        }
    }
    r
}
