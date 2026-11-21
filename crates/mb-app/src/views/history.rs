use eframe::egui::{self, RichText};

use crate::app::App;
use crate::sql_highlight::highlight;
use crate::store::ThemeChoice;
use crate::theme;

#[derive(Default)]
pub struct HistoryView {
    pub selected: Option<String>,
}

impl App {
    pub fn ui_history(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("hist_list").resizable(true).default_size(360.0).show(ui, |ui| {
            ui.heading("History");
            ui.label(RichText::new(format!("Stored in {}", self.store.history_dir().display())).small().weak());
            ui.separator();
            let mut pick = None;
            egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, 44.0, self.history.len(), |ui, range| {
                for h in &self.history[range] {
                    let sel = self.history_view.selected.as_deref() == Some(h.id.as_str());
                    let r = ui.selectable_label(
                        sel,
                        RichText::new(format!(
                            "{} {}  ·  {}\n{} → {}",
                            if h.ok { "✔" } else { "✖" },
                            h.kind,
                            h.when,
                            h.source,
                            h.target
                        ))
                        .color(if h.ok { ui.visuals().text_color() } else { theme::RED }),
                    );
                    if r.clicked() {
                        pick = Some(h.id.clone());
                    }
                }
            });
            if let Some(p) = pick {
                self.history_view.selected = Some(p);
            }
            if self.history.is_empty() {
                ui.label(RichText::new("Deploys, data migrations and syncs will appear here.").weak());
            }
        });
        egui::CentralPanel::default().show(ui, |ui| {
            let Some(h) = self.history_view.selected.as_ref().and_then(|id| self.history.iter().find(|h| &h.id == id)).cloned() else {
                ui.label("Select a run on the left.");
                return;
            };
            ui.heading(&h.kind);
            ui.label(format!("{} · {} → {} · {}", h.when, h.source, h.target, theme::fmt_ms(h.millis as f64)));
            ui.label(RichText::new(&h.summary).color(if h.ok { theme::GREEN } else { theme::RED }));
            ui.horizontal(|ui| {
                if !h.script.is_empty() && ui.button("Copy script").clicked() {
                    ui.ctx().copy_text(h.script.clone());
                }
                if ui.button(RichText::new("Delete entry").color(theme::RED)).clicked() {
                    self.store.delete_history(&h.id);
                    self.history.retain(|x| x.id != h.id);
                    self.history_view.selected = None;
                }
            });
            ui.separator();
            egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                for l in &h.log {
                    ui.label(RichText::new(l).monospace().small());
                }
                if !h.script.is_empty() {
                    let job = highlight(&h.script, ui.visuals().dark_mode, egui::FontId::monospace(12.5));
                    ui.label(job);
                }
            });
        });
    }

    pub fn ui_settings(&mut self, ui: &mut egui::Ui) {
        ui.heading("Settings");
        ui.add_space(8.0);
        let s = &mut self.settings;
        let mut changed = false;
        egui::Grid::new("settings").num_columns(2).spacing([16.0, 10.0]).show(ui, |ui| {
            ui.label("Theme");
            ui.horizontal(|ui| {
                for (t, l) in [(ThemeChoice::System, "System"), (ThemeChoice::Dark, "Dark"), (ThemeChoice::Light, "Light")] {
                    changed |= ui.selectable_value(&mut s.theme, t, l).changed();
                }
            });
            ui.end_row();
            ui.label("UI scale");
            changed |= ui.add(egui::Slider::new(&mut s.ui_scale, 0.8..=1.6).step_by(0.05)).drag_stopped();
            ui.end_row();
            ui.label("Parallel workers");
            changed |= ui.add(egui::Slider::new(&mut s.workers, 1..=32)).changed();
            ui.end_row();
            ui.label("Data compare chunk size");
            changed |= ui.add(egui::DragValue::new(&mut s.chunk_size).range(500..=1_000_000).speed(500)).changed();
            ui.end_row();
            ui.label("Deploy lock_timeout (s)");
            changed |= ui.add(egui::DragValue::new(&mut s.lock_timeout_secs).range(0..=600)).changed();
            ui.end_row();
            ui.label("Schema compare");
            ui.vertical(|ui| {
                changed |= ui.checkbox(&mut s.ignore_comments, "Ignore comments").changed();
                changed |= ui.checkbox(&mut s.cascade_drops, "Use CASCADE on drops").changed();
                changed |= ui.checkbox(&mut s.concurrent_indexes, "Create indexes CONCURRENTLY").changed();
            });
            ui.end_row();
        });
        if changed {
            self.migrate.opts.workers = self.settings.workers;
            self.schema.plan_dirty = true;
            self.settings_dirty = true;
        }
        ui.add_space(16.0);
        ui.separator();
        ui.label(RichText::new("Keyboard: F5 compare/load · Ctrl+1…6 switch screens · Esc closes dialogs").weak());
        if !self.store.keychain_ok {
            ui.label(RichText::new("⚠ The OS keychain is unavailable; passwords are kept for this session only.").color(theme::AMBER));
        }
    }
}
