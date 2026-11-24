//! Doctor: pre-flight checks for the job's pair. Blockers lock Compare and Migration until fixed.

use eframe::egui::{self, CornerRadius, Margin, RichText, Stroke};
use mb_core::doctor::{diagnose, DoctorInput, Finding, Severity};
use mb_core::progress::Level;

use crate::app::{App, Event, Screen};
use crate::store::{now_ms, DoctorRecord};
use crate::theme;

#[derive(Default)]
pub struct DoctorView {
    pub running: bool,
    pub show_passed: bool,
}

fn block(title: impl Into<String>, detail: impl Into<String>, fix: &str) -> Finding {
    Finding { severity: Severity::Block, title: title.into(), detail: detail.into(), fix: Some(fix.into()) }
}

impl App {
    pub fn run_doctor(&mut self) {
        if self.doctor.running || self.pair_problem().is_some() {
            return;
        }
        let job = self.job().clone();
        let (Some(sp), Some(tp)) = (self.profile(&job.source_id).cloned(), self.profile(&job.target_id).cloned()) else { return };
        let sig = self.signature();
        let job_id = job.id.clone();

        let mut extra = Vec::new();
        for (p, side) in [(&sp, "Source"), (&tp, "Target")] {
            if !p.engine.migratable() {
                extra.push(block(
                    format!("{side} engine {} isn't supported yet", p.engine.label()),
                    p.label(),
                    "Use PostgreSQL connections for now. MySQL and SQL Server migrations are on the roadmap.",
                ));
            }
        }
        if !extra.is_empty() {
            self.on_doctor(job_id, sig, extra);
            return;
        }
        if tp.production {
            extra.push(Finding {
                severity: Severity::Warn,
                title: "Target is production".into(),
                detail: "Writes ask you to type the database name.".into(),
                fix: None,
            });
        }
        let (Some(src), Some(tgt)) = (self.params(&job.source_id), self.params(&job.target_id)) else { return };
        let input = DoctorInput { source: src, target: tgt, filter: self.schema_filter(), workers: job.data.workers };
        self.doctor.running = true;
        let bus = self.bus.clone();
        self.spawn(async move {
            let mut findings = diagnose(&input).await;
            findings.extend(extra);
            findings.sort_by(|a, b| b.severity.cmp(&a.severity));
            bus.send(Event::Doctor { job_id, sig, findings });
        });
    }

    pub fn on_doctor(&mut self, job_id: String, sig: u64, findings: Vec<Finding>) {
        let Some(i) = self.jobs.iter().position(|e| e.job.id == job_id) else { return };
        let blocked = mb_core::doctor::blocked(&findings);
        self.jobs[i].job.doctor = Some(DoctorRecord { at: now_ms(), sig, findings });
        let _ = self.store.save_job(&self.jobs[i].job);
        if i == self.active {
            self.doctor.running = false;
            if blocked {
                self.notify(Level::Warn, "Fix the blockers, then re-check");
            } else {
                self.notify(Level::Info, "Ready to migrate");
            }
        }
    }

    pub fn doctor_actions(&mut self, ui: &mut egui::Ui) {
        let passed = self.doctor_passed();
        if passed {
            if theme::primary(ui, true, "Continue  →").clicked() {
                self.go(Screen::Compare);
            }
            if theme::secondary(ui, !self.doctor.running, "Re-check").clicked() {
                self.run_doctor();
            }
        } else {
            let label = if self.job().doctor.is_some() { "Re-check" } else { "Run Doctor" };
            if theme::primary(ui, !self.doctor.running && self.pair_problem().is_none(), label).clicked() {
                self.run_doctor();
            }
        }
    }

    pub fn ui_doctor(&mut self, ui: &mut egui::Ui) {
        let p = theme::pal(ui);
        let sig = self.signature();
        let record = self.job().doctor.clone();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.set_max_width(860.0);
            // Status line.
            ui.horizontal(|ui| {
                if self.doctor.running {
                    ui.spinner();
                    ui.label(theme::heading("Checking…"));
                    return;
                }
                match &record {
                    None => {
                        ui.label(theme::heading("Not checked yet"));
                    }
                    Some(r) if r.sig != sig => {
                        theme::dot(ui, p.amber);
                        ui.label(theme::heading("Connections changed"));
                    }
                    Some(r) => {
                        let blocks = r.findings.iter().filter(|f| f.severity == Severity::Block).count();
                        let warns = r.findings.iter().filter(|f| f.severity == Severity::Warn).count();
                        if blocks > 0 {
                            theme::dot(ui, p.red);
                            ui.label(theme::heading(format!("{blocks} blocker{} to fix", if blocks == 1 { "" } else { "s" })));
                        } else {
                            theme::dot(ui, p.green);
                            ui.label(theme::heading("Ready to migrate"));
                        }
                        let mut sub = theme::fmt_ago(r.at);
                        if warns > 0 {
                            sub = format!("{warns} warning{} · {sub}", if warns == 1 { "" } else { "s" });
                        }
                        theme::muted(ui, sub);
                    }
                }
            });
            ui.add_space(14.0);

            let Some(r) = record else {
                theme::muted(ui, "Doctor checks access, versions, extensions and permissions on both databases.");
                return;
            };
            let (passed, open): (Vec<&Finding>, Vec<&Finding>) = r.findings.iter().partition(|f| f.severity == Severity::Pass);
            for f in open {
                self.finding(ui, f);
                ui.add_space(10.0);
            }
            if !passed.is_empty() {
                let label = format!("{}  {} checks passed", if self.doctor.show_passed { "▾" } else { "▸" }, passed.len());
                if ui.add(egui::Button::new(RichText::new(label).color(p.green)).frame(false)).clicked() {
                    self.doctor.show_passed = !self.doctor.show_passed;
                }
                if self.doctor.show_passed {
                    for f in passed {
                        ui.horizontal(|ui| {
                            ui.add_space(6.0);
                            theme::dot(ui, p.green);
                            ui.label(theme::strong(&f.title));
                            theme::muted(ui, &f.detail);
                        });
                    }
                }
            }
        });
    }

    fn finding(&mut self, ui: &mut egui::Ui, f: &Finding) {
        let p = theme::pal(ui);
        let color = if f.severity == Severity::Block { p.red } else { p.amber };
        egui::Frame::NONE
            .fill(p.card)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(CornerRadius::same(12))
            .inner_margin(Margin::symmetric(18, 14))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    theme::dot(ui, color);
                    ui.label(theme::strong(&f.title));
                    if f.severity == Severity::Block {
                        theme::chip(ui, "Blocker", p.red);
                    }
                });
                if !f.detail.is_empty() {
                    theme::muted(ui, &f.detail);
                }
                if let Some(fix) = &f.fix {
                    let looks_sql = fix.contains(';');
                    if looks_sql {
                        egui::Frame::NONE
                            .fill(p.input)
                            .corner_radius(CornerRadius::same(8))
                            .inner_margin(Margin::same(10))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.horizontal_top(|ui| {
                                    ui.add(egui::Label::new(theme::mono(fix)).wrap());
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                                        if theme::ghost(ui, "Copy").clicked() {
                                            ui.ctx().copy_text(fix.clone());
                                            self.notify(Level::Info, "Copied");
                                        }
                                    });
                                });
                            });
                    } else {
                        ui.label(RichText::new(format!("→ {fix}")).color(p.text));
                    }
                }
            });
    }
}
