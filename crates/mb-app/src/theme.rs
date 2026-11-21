//! Visual language: restrained neutral theme + semantic colours.

use eframe::egui::{self, Color32, CornerRadius, Margin, Stroke, Theme, ThemePreference};
use mb_core::diff::DiffStatus;
use mb_core::plan::Hazard;

use crate::store::{Settings, ThemeChoice};

pub const ACCENT: Color32 = Color32::from_rgb(64, 132, 255);
pub const GREEN: Color32 = Color32::from_rgb(46, 170, 94);
pub const AMBER: Color32 = Color32::from_rgb(222, 150, 30);
pub const RED: Color32 = Color32::from_rgb(226, 72, 64);
pub const GREY: Color32 = Color32::from_rgb(128, 134, 145);

pub fn status_color(s: DiffStatus) -> Color32 {
    match s {
        DiffStatus::Added => GREEN,
        DiffStatus::Changed => AMBER,
        DiffStatus::Removed => RED,
        DiffStatus::Identical => GREY,
    }
}

pub fn hazard_color(h: Hazard) -> Color32 {
    match h {
        Hazard::DataLoss => RED,
        Hazard::Manual => RED,
        Hazard::TableRewrite | Hazard::LocksTable => AMBER,
        Hazard::MayFail => AMBER,
    }
}

/// Faint row tint for diff backgrounds.
pub fn tint(c: Color32, dark: bool) -> Color32 {
    let a = if dark { 38 } else { 34 };
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
}

pub fn apply(ctx: &egui::Context, settings: &Settings) {
    ctx.set_theme(match settings.theme {
        ThemeChoice::System => ThemePreference::System,
        ThemeChoice::Dark => ThemePreference::Dark,
        ThemeChoice::Light => ThemePreference::Light,
    });
    ctx.set_zoom_factor(settings.ui_scale.clamp(0.7, 2.0));
    for theme in [Theme::Dark, Theme::Light] {
        ctx.style_mut_of(theme, |s| {
            s.spacing.item_spacing = egui::vec2(8.0, 6.0);
            s.spacing.button_padding = egui::vec2(10.0, 4.0);
            s.spacing.interact_size.y = 24.0;
            s.spacing.window_margin = Margin::same(12);
            s.visuals.window_corner_radius = CornerRadius::same(8);
            s.visuals.menu_corner_radius = CornerRadius::same(6);
            for w in [
                &mut s.visuals.widgets.inactive,
                &mut s.visuals.widgets.hovered,
                &mut s.visuals.widgets.active,
                &mut s.visuals.widgets.noninteractive,
                &mut s.visuals.widgets.open,
            ] {
                w.corner_radius = CornerRadius::same(5);
            }
            s.visuals.selection.bg_fill = ACCENT.linear_multiply(0.55);
            s.visuals.selection.stroke = Stroke::new(1.0, ACCENT);
            s.visuals.hyperlink_color = ACCENT;
            if theme == Theme::Dark {
                s.visuals.panel_fill = Color32::from_rgb(28, 30, 34);
                s.visuals.window_fill = Color32::from_rgb(34, 36, 41);
                s.visuals.extreme_bg_color = Color32::from_rgb(20, 21, 24);
                s.visuals.faint_bg_color = Color32::from_rgb(36, 38, 43);
            } else {
                s.visuals.panel_fill = Color32::from_rgb(246, 247, 249);
                s.visuals.window_fill = Color32::WHITE;
                s.visuals.extreme_bg_color = Color32::WHITE;
                s.visuals.faint_bg_color = Color32::from_rgb(238, 240, 244);
            }
        });
    }
}

/// A small coloured pill (status/hazard badge).
pub fn badge(ui: &mut egui::Ui, text: &str, color: Color32) -> egui::Response {
    let dark = ui.visuals().dark_mode;
    egui::Frame::NONE
        .fill(tint(color, dark))
        .stroke(Stroke::new(1.0, color.linear_multiply(0.7)))
        .corner_radius(CornerRadius::same(9))
        .inner_margin(Margin::symmetric(6, 1))
        .show(ui, |ui| ui.label(egui::RichText::new(text).small().color(color).strong()))
        .response
}

/// Primary (accent) or danger button.
pub fn primary_button(ui: &mut egui::Ui, enabled: bool, text: &str, danger: bool) -> egui::Response {
    let fill = if danger { RED } else { ACCENT };
    ui.add_enabled(
        enabled,
        egui::Button::new(egui::RichText::new(text).color(Color32::WHITE).strong()).fill(fill),
    )
}

pub fn fmt_bytes(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

pub fn fmt_count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

pub fn fmt_ms(ms: f64) -> String {
    if ms < 1000.0 {
        format!("{ms:.0} ms")
    } else if ms < 60_000.0 {
        format!("{:.1} s", ms / 1000.0)
    } else {
        format!("{}m {:02}s", (ms / 60_000.0) as u64, ((ms / 1000.0) as u64) % 60)
    }
}
