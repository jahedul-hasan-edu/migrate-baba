//! The source → target picker shown at the top of Doctor, Compare and Migration.
//! Picking the connection that's already on the other side swaps them; "New connection…" opens the
//! add modal and drops the result straight into that side. The pair is frozen while work depends on it.

use eframe::egui::{self, Align, Layout, RichText};

use crate::app::App;
use crate::store::RunStatus;
use crate::theme;

const COMBO_W: f32 = 230.0;

impl App {
    /// Why the job's source/target can't change right now.
    pub fn pair_lock(&self) -> Option<&'static str> {
        let e = self.entry();
        if e.running() {
            return Some("A migration is running for this pair");
        }
        if e.run.as_ref().map(|r| r.status == RunStatus::Paused).unwrap_or(false) {
            return Some("Resume or discard the paused migration to change connections");
        }
        if self.migration.deploying() {
            return Some("A schema deploy is running");
        }
        if self.compare.data_running {
            return Some("A data compare is running");
        }
        None
    }

    /// Put a connection on one side. Picking the other side's connection swaps the two.
    pub fn set_side(&mut self, source: bool, id: Option<String>) {
        if let Some(why) = self.pair_lock() {
            self.notify(mb_core::progress::Level::Warn, why);
            return;
        }
        let job = self.job();
        let (mine, other) = if source { (&job.source_id, &job.target_id) } else { (&job.target_id, &job.source_id) };
        if *mine == id {
            return;
        }
        let swap = id.is_some() && *other == id;
        let old = mine.clone();
        self.retire_finished_run();
        let job = self.job_mut();
        let (mine, other) = if source { (&mut job.source_id, &mut job.target_id) } else { (&mut job.target_id, &mut job.source_id) };
        *mine = id;
        if swap {
            *other = old;
        }
        self.save_job();
        self.after_pair_change();
    }

    pub fn swap_sides(&mut self) {
        if let Some(why) = self.pair_lock() {
            self.notify(mb_core::progress::Level::Warn, why);
            return;
        }
        self.retire_finished_run();
        let job = self.job_mut();
        std::mem::swap(&mut job.source_id, &mut job.target_id);
        self.save_job();
        self.after_pair_change();
    }

    /// A finished or cancelled run belongs to the old pair; forget it before the pair changes.
    fn retire_finished_run(&mut self) {
        let done = self.entry().run.as_ref().map(|r| matches!(r.status, RunStatus::Done | RunStatus::Cancelled)).unwrap_or(false);
        if done {
            self.discard_run();
        }
    }

    pub fn pair_bar(&mut self, ui: &mut egui::Ui) {
        let p = theme::pal(ui);
        let lock = self.pair_lock();
        let (src, tgt) = (self.job().source_id.clone(), self.job().target_id.clone());
        let mut pick: Option<(bool, Option<String>)> = None;
        let mut add: Option<bool> = None;
        let mut swap = false;
        let mut schemas_done = false;

        ui.horizontal_wrapped(|ui| {
            ui.add_enabled_ui(lock.is_none(), |ui| {
                for source in [true, false] {
                    let (mine, other) = if source { (&src, &tgt) } else { (&tgt, &src) };
                    ui.label(theme::strong(if source { "From" } else { "To" }).color(p.muted));
                    let current = self.profile(mine);
                    let text = match current {
                        Some(x) if x.production && !source => RichText::new(format!("{}   PROD", x.label())).color(p.red),
                        Some(x) => RichText::new(x.label()),
                        None => RichText::new(if source { "Choose source…" } else { "Choose target…" }).color(p.faint),
                    };
                    ui.scope(|ui| {
                        ui.spacing_mut().button_padding = egui::vec2(12.0, 8.0);
                        egui::ComboBox::from_id_salt(("pair", source)).width(COMBO_W).selected_text(text).show_ui(ui, |ui| {
                            for x in &self.profiles {
                                let sel = mine.as_deref() == Some(x.id.as_str());
                                let is_other = other.as_deref() == Some(x.id.as_str());
                                let label = if is_other {
                                    RichText::new(format!("{}   ⇄ swap", x.label()))
                                } else {
                                    RichText::new(x.label())
                                };
                                let r = ui.selectable_label(sel, label).on_hover_text(format!("{} · {}", x.engine.label(), x.params.display()));
                                if r.clicked() && !sel {
                                    pick = Some((source, Some(x.id.clone())));
                                }
                            }
                            if !self.profiles.is_empty() {
                                ui.separator();
                            }
                            if ui.selectable_label(false, RichText::new("+  New connection…").color(p.accent)).clicked() {
                                add = Some(source);
                            }
                        });
                    });
                    if source {
                        let can = src.is_some() || tgt.is_some();
                        ui.add_space(2.0);
                        if ui.add_enabled(can, egui::Button::new(RichText::new("⇄").size(16.0)).frame(false)).on_hover_text("Swap source and target").clicked() {
                            swap = true;
                        }
                        ui.add_space(2.0);
                    }
                }
                ui.add_space(10.0);
                ui.label(theme::strong("Schemas").color(p.muted));
                let mut s = self.job().schemas.clone();
                let r = theme::input(ui, &mut s, "All", 150.0).on_hover_text("Limit to these schemas, comma separated (e.g. public, sales)");
                if s != self.job().schemas {
                    self.job_mut().schemas = s;
                }
                schemas_done = r.lost_focus();
            });
            if let Some(why) = lock {
                ui.add_space(6.0);
                ui.label(theme::small(format!("🔒 {why}")).color(p.muted));
            }
        });

        if let Some((source, id)) = pick {
            self.set_side(source, id);
        }
        if swap {
            self.swap_sides();
        }
        if let Some(source) = add {
            self.add_connection(Some(source));
        }
        if schemas_done {
            self.save_job();
            self.after_pair_change();
        }
    }

    /// Standard header for the pair screens: picker on top, then a row with `left` and right-aligned `right`.
    pub fn screen_header(&mut self, ui: &mut egui::Ui, left: impl FnOnce(&mut Self, &mut egui::Ui), right: impl FnOnce(&mut Self, &mut egui::Ui)) {
        self.pair_bar(ui);
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            ui.set_min_height(36.0);
            left(self, ui);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| right(self, ui));
        });
        ui.add_space(14.0);
    }
}
