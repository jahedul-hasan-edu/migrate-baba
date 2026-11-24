//! Migration: copy data in a background worker (pausable, resumable, survives restarts) or apply
//! schema changes — tables, views, functions, procedures, triggers, types — to the target.

use std::collections::BTreeSet;

use eframe::egui::{self, Color32, CornerRadius, Margin, RichText, Stroke};
use egui_extras::{Column, TableBuilder};
use mb_core::conn::{connect, SessionOptions, Side};
use mb_core::data::copy::CopyMode;
use mb_core::data::state;
use mb_core::deploy::{deploy, DeployOptions, DeployProgress, DeployReport};
use mb_core::diff::{DiffStatus, ObjectKey, ObjectKind};
use mb_core::introspect::introspect;
use mb_core::model::QName;
use mb_core::plan::{plan_selection, Hazard, Plan, PlanOptions};
use mb_core::progress::{CancellationToken, Level};

use crate::app::{App, Event, Loaded};
use crate::sql_highlight::highlight;
use crate::store::{DataSpec, PauseReason, RunState, RunStatus};
use crate::theme::{self, Pal};
use crate::views::compare::status_color;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Data,
    Schema,
}

pub struct MigRow {
    pub table: QName,
    pub checked: bool,
    pub in_target: bool,
    pub est_rows: i64,
    pub bytes: i64,
}

#[derive(Clone)]
pub enum StepState {
    Pending,
    Running,
    Done,
    Skipped,
    Failed,
}

pub struct DeployState {
    pub test: bool,
    pub confirm: String,
    pub running: bool,
    pub plan: Plan,
    pub steps: Vec<StepState>,
    pub report: Option<Result<DeployReport, String>>,
    pub cancel: CancellationToken,
}

pub struct RunConfirm {
    pub text: String,
}

#[derive(Default)]
pub struct MigrationView {
    pub tab: Tab,
    pub rows: Vec<MigRow>,
    pub filter: String,
    pub spec: DataSpec,
    pub confirm: Option<RunConfirm>,
    // schema
    pub diffs: Vec<mb_core::diff::ObjectDiff>,
    pub checked: BTreeSet<ObjectKey>,
    pub plan: Plan,
    pub script: String,
    pub plan_dirty: bool,
    pub target_fp: u64,
    pub catalogs: Option<(std::sync::Arc<mb_core::model::Catalog>, std::sync::Arc<mb_core::model::Catalog>)>,
    pub deploy: Option<DeployState>,
}

const KINDS: [(ObjectKind, &str); 9] = [
    (ObjectKind::Table, "Tables"),
    (ObjectKind::View, "Views"),
    (ObjectKind::MaterializedView, "Materialized views"),
    (ObjectKind::Function, "Functions"),
    (ObjectKind::Procedure, "Procedures"),
    (ObjectKind::Type, "Types"),
    (ObjectKind::Sequence, "Sequences"),
    (ObjectKind::Extension, "Extensions"),
    (ObjectKind::Schema, "Schemas"),
];

impl MigrationView {
    pub fn on_loaded(&mut self, l: &Loaded, spec: &DataSpec) {
        let chosen: BTreeSet<&QName> = spec.tables.iter().collect();
        self.rows = l
            .source
            .tables
            .values()
            .filter(|t| t.partition_key.is_none())
            .map(|t| MigRow {
                table: t.name.clone(),
                checked: chosen.is_empty() || chosen.contains(&t.name),
                in_target: l.target.tables.contains_key(&t.name),
                est_rows: t.est_rows,
                bytes: t.total_bytes,
            })
            .collect();
        self.rows.sort_by(|a, b| a.table.cmp(&b.table));
        self.spec = spec.clone();
        self.diffs = l.diffs.iter().filter(|d| d.status != DiffStatus::Identical).cloned().collect();
        self.diffs.sort_by(|a, b| (a.key.kind, a.key.display()).cmp(&(b.key.kind, b.key.display())));
        self.checked = self.diffs.iter().filter(|d| d.status != DiffStatus::Removed).map(|d| d.key.clone()).collect();
        self.catalogs = Some((l.source.clone(), l.target.clone()));
        self.target_fp = l.target.fingerprint();
        self.plan_dirty = true;
    }

    pub fn deploying(&self) -> bool {
        self.deploy.as_ref().map(|d| d.running).unwrap_or(false)
    }

    pub fn on_deploy_progress(&mut self, p: DeployProgress) {
        let Some(d) = self.deploy.as_mut() else { return };
        match p {
            DeployProgress::Started { index, .. } => d.steps[index] = StepState::Running,
            DeployProgress::Finished { index, .. } => d.steps[index] = StepState::Done,
            DeployProgress::Skipped { index, .. } => d.steps[index] = StepState::Skipped,
        }
    }
}

fn run_label(r: &RunState) -> (&'static str, fn(&Pal) -> Color32) {
    match r.status {
        RunStatus::Running => ("Running", |p| p.accent),
        RunStatus::Paused => match r.reason {
            Some(PauseReason::Issue) => ("Needs attention", |p| p.red),
            Some(PauseReason::Interrupted) => ("Interrupted", |p| p.amber),
            _ => ("Paused", |p| p.amber),
        },
        RunStatus::Done => ("Done", |p| p.green),
        RunStatus::Cancelled => ("Cancelled", |p| p.faint),
    }
}

impl App {
    fn plan_options(&self) -> PlanOptions {
        PlanOptions { cascade_drops: self.settings.cascade_drops, concurrent_indexes: self.settings.concurrent_indexes }
    }

    fn refresh_plan(&mut self) {
        if !self.migration.plan_dirty {
            return;
        }
        self.migration.plan_dirty = false;
        if let Some((s, t)) = &self.migration.catalogs {
            let plan = plan_selection(s, t, &self.migration.checked, &self.plan_options());
            let name = self.job_name(self.job());
            self.migration.script = plan.to_sql(&format!("Ferry: {name}"));
            self.migration.plan = plan;
        }
    }

    pub fn migration_actions(&mut self, ui: &mut egui::Ui) {
        match self.migration.tab {
            Tab::Data => {
                let e = self.entry();
                let stopping = e.stopping;
                let starting = e.starting.is_some();
                let status = e.run.as_ref().map(|r| r.status);
                match status {
                    _ if starting => {
                        theme::secondary(ui, false, "Starting…");
                    }
                    Some(RunStatus::Running) => {
                        if theme::ghost(ui, "Cancel").clicked() && !stopping {
                            self.request_stop("cancel");
                        }
                        if theme::secondary(ui, !stopping, if stopping { "Pausing…" } else { "Pause" }).clicked() {
                            self.request_stop("pause");
                        }
                    }
                    Some(RunStatus::Paused) => {
                        if theme::ghost(ui, "Discard").on_hover_text("Forget progress and start over later").clicked() {
                            self.discard_run();
                        }
                        if theme::primary(ui, true, "Resume").clicked() {
                            self.start_worker();
                        }
                    }
                    _ => {
                        let n = self.migration.rows.iter().filter(|r| r.checked).count();
                        if theme::primary(ui, n > 0 && !self.cat_loading, "Run Migration").clicked() {
                            self.migration.confirm = Some(RunConfirm { text: String::new() });
                        }
                    }
                }
            }
            Tab::Schema => {
                self.refresh_plan();
                let has = !self.migration.plan.steps.is_empty();
                let loss = self.migration.plan.has_data_loss();
                let deploy = if loss { theme::danger(ui, has, "Deploy") } else { theme::primary(ui, has, "Deploy") };
                if deploy.clicked() {
                    self.open_deploy(false);
                }
                if theme::secondary(ui, has, "Test").on_hover_text("Run in a transaction and roll back").clicked() {
                    self.open_deploy(true);
                }
                ui.menu_button(RichText::new("•••").color(theme::pal(ui).muted), |ui| {
                    if ui.add_enabled(has, egui::Button::new("Copy SQL")).clicked() {
                        ui.ctx().copy_text(self.migration.script.clone());
                        ui.close();
                    }
                    if ui.add_enabled(has, egui::Button::new("Save .sql…")).clicked() {
                        ui.close();
                        let name = format!("{}_ferry.sql", chrono::Local::now().format("%Y%m%d%H%M%S"));
                        if let Some(path) = rfd::FileDialog::new().set_file_name(&name).add_filter("SQL", &["sql"]).save_file() {
                            match std::fs::write(&path, &self.migration.script) {
                                Ok(()) => self.notify(Level::Info, "Saved"),
                                Err(e) => self.notify(Level::Error, format!("Couldn't save: {e}")),
                            }
                        }
                    }
                    ui.separator();
                    let mut changed = false;
                    changed |= ui.checkbox(&mut self.settings.cascade_drops, "CASCADE on drops").changed();
                    changed |= ui.checkbox(&mut self.settings.concurrent_indexes, "Build indexes concurrently").changed();
                    if changed {
                        self.migration.plan_dirty = true;
                        self.save_settings();
                    }
                });
            }
        }
    }

    pub fn ui_migration(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let mut tab = self.migration.tab;
            if theme::segmented(ui, &mut tab, &[(Tab::Data, "Data"), (Tab::Schema, "Schema")]) {
                self.migration.tab = tab;
            }
        });
        ui.add_space(16.0);
        match self.migration.tab {
            Tab::Data => {
                let show_run = self.entry().run.is_some() || self.entry().starting.is_some();
                if show_run {
                    self.run_view(ui);
                } else {
                    self.data_setup(ui);
                }
            }
            Tab::Schema => self.schema_migration(ui),
        }
    }

    // ---- data: setup ----------------------------------------------------------------------------

    fn data_setup(&mut self, ui: &mut egui::Ui) {
        let p = theme::pal(ui);
        if self.migration.rows.is_empty() {
            if self.cat_loading {
                ui.horizontal(|ui| {
                    ui.spinner();
                    theme::muted(ui, "Reading tables…");
                });
            } else if self.cat.is_none() {
                theme::empty_state(ui, "Tables not loaded", "Reading the databases didn't finish.");
                ui.vertical_centered(|ui| {
                    if theme::secondary(ui, true, "Try again").clicked() {
                        self.load_catalogs();
                    }
                });
            } else {
                theme::empty_state(ui, "No tables found", "The source has no tables in the selected schemas.");
            }
            return;
        }
        let spec = &mut self.migration.spec;
        ui.horizontal(|ui| {
            theme::segmented(
                ui,
                &mut spec.mode,
                &[(CopyMode::Replace, "Replace"), (CopyMode::Append, "Append"), (CopyMode::Sync, "Sync")],
            );
            ui.add_space(6.0);
            theme::muted(ui, match spec.mode {
                CopyMode::Replace => "Empty target tables, then copy",
                CopyMode::Append => "Insert rows only",
                CopyMode::Sync => "Upsert and delete to match",
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.menu_button(RichText::new("Options").color(p.muted), |ui| {
                    ui.checkbox(&mut spec.create_missing, "Create missing tables");
                    ui.checkbox(&mut spec.consistent_snapshot, "Consistent snapshot");
                    ui.checkbox(&mut spec.fast_load, "Fast load (pause triggers)");
                    ui.checkbox(&mut spec.reset_sequences, "Reset sequences");
                    ui.checkbox(&mut spec.analyze, "Analyze after");
                });
                ui.add_space(6.0);
                if ui.small_button("+").clicked() {
                    spec.workers = (spec.workers + 1).min(32);
                }
                ui.label(theme::strong(spec.workers.to_string()));
                if ui.small_button("−").clicked() {
                    spec.workers = spec.workers.saturating_sub(1).max(1);
                }
                theme::muted(ui, "Workers");
            });
        });
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            theme::input(ui, &mut self.migration.filter, "Filter tables", 280.0);
            let n = self.migration.rows.iter().filter(|r| r.checked).count();
            let bytes: i64 = self.migration.rows.iter().filter(|r| r.checked).map(|r| r.bytes.max(0)).sum();
            theme::muted(ui, format!("{n} of {} tables · {}", self.migration.rows.len(), theme::fmt_bytes(bytes as u64)));
        });
        ui.add_space(6.0);

        let q = self.migration.filter.to_lowercase();
        let idx: Vec<usize> = (0..self.migration.rows.len())
            .filter(|&i| q.is_empty() || self.migration.rows[i].table.to_string().to_lowercase().contains(&q))
            .collect();
        let all = idx.iter().all(|&i| self.migration.rows[i].checked);
        let mut toggle = None;
        TableBuilder::new(ui)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(32.0))
            .column(Column::initial(340.0).at_least(160.0))
            .column(Column::initial(120.0))
            .column(Column::remainder())
            .header(30.0, |mut h| {
                h.col(|ui| {
                    let mut v = all;
                    if ui.add(egui::Checkbox::without_text(&mut v)).changed() {
                        toggle = Some(v);
                    }
                });
                for t in ["Table", "Rows", "Size"] {
                    h.col(|ui| {
                        ui.label(theme::small(t).color(p.faint));
                    });
                }
            })
            .body(|body| {
                body.rows(34.0, idx.len(), |mut row| {
                    let r = &mut self.migration.rows[idx[row.index()]];
                    row.col(|ui| {
                        ui.add(egui::Checkbox::without_text(&mut r.checked));
                    });
                    row.col(|ui| {
                        ui.label(r.table.to_string());
                        if !r.in_target {
                            theme::chip(ui, "new", p.green).on_hover_text("Missing on target");
                        }
                    });
                    row.col(|ui| {
                        ui.label(RichText::new(theme::fmt_count(r.est_rows.max(0) as u64)).color(p.muted));
                    });
                    row.col(|ui| {
                        ui.label(RichText::new(theme::fmt_bytes(r.bytes.max(0) as u64)).color(p.muted));
                    });
                });
            });
        if let Some(v) = toggle {
            for &i in &idx {
                self.migration.rows[i].checked = v;
            }
        }
    }

    fn start_run(&mut self) {
        let mut spec = self.migration.spec.clone();
        spec.tables = self.migration.rows.iter().filter(|r| r.checked).map(|r| r.table.clone()).collect();
        self.job_mut().data = spec;
        self.save_job();
        // A new run starts from scratch; the worker also drops stale markers on the target.
        self.store.clear_run(&self.job().id.clone());
        self.jobs[self.active].run = None;
        self.start_worker();
    }

    // ---- data: live run -------------------------------------------------------------------------

    fn run_view(&mut self, ui: &mut egui::Ui) {
        let p = theme::pal(ui);
        let starting = self.entry().starting.is_some();
        let Some(run) = self.entry().run.clone().filter(|_| !starting || self.entry().run.as_ref().map(|r| r.status == RunStatus::Running).unwrap_or(false)) else {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(theme::heading("Starting…"));
            });
            return;
        };
        let (label, color) = run_label(&run);
        let color = color(p);
        let frac = run.fraction();
        let (done_b, est_b, rows) = run.totals();

        // Headline.
        ui.horizontal(|ui| {
            ui.label(theme::heading(format!("{:.0}%", frac * 100.0)));
            ui.add_space(4.0);
            theme::chip(ui, label, color);
            if run.status == RunStatus::Running && !run.phase.is_empty() && run.phase != "Copying" {
                theme::muted(ui, &run.phase);
            }
        });
        ui.add_space(6.0);
        theme::progress(ui, frac, ui.available_width(), 8.0, color);
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let mut parts = vec![
                format!("{} of {}", theme::fmt_bytes(done_b), theme::fmt_bytes(est_b)),
                format!("{} rows", theme::fmt_count(rows)),
                theme::fmt_ms(run.active_ms as f64),
            ];
            if run.status == RunStatus::Running && run.rate_bps > 1.0 {
                parts.push(format!("{}/s", theme::fmt_bytes(run.rate_bps as u64)));
                if est_b > done_b {
                    parts.push(format!("~{} left", theme::fmt_ms((est_b - done_b) as f64 / run.rate_bps * 1000.0)));
                }
            }
            theme::muted(ui, parts.join("  ·  "));
        });
        ui.add_space(14.0);

        // Why it stopped, and what to do.
        match (run.status, run.reason) {
            (RunStatus::Paused, Some(PauseReason::Issue)) => {
                if let Some(i) = &run.issue {
                    self.banner(ui, p.red, &i.title, &i.hint, Some(&i.detail), i.table.as_deref());
                }
            }
            (RunStatus::Paused, Some(PauseReason::Interrupted)) => {
                self.banner(ui, p.amber, "Stopped unexpectedly", "Ferry or the computer stopped mid-run. Committed chunks are kept — resume to continue.", None, None);
            }
            (RunStatus::Paused, _) => {
                self.banner(ui, p.amber, "Paused by you", "Committed chunks are kept. Resume any time, even after a restart.", None, None);
            }
            (RunStatus::Done, _) => {
                let msg = format!("{} rows copied in {}.", theme::fmt_count(rows), theme::fmt_ms(run.active_ms as f64));
                self.banner(ui, p.green, "Migration complete", &msg, None, None);
                ui.horizontal(|ui| {
                    if theme::secondary(ui, true, "New run").clicked() {
                        self.discard_run();
                    }
                });
            }
            (RunStatus::Cancelled, _) => {
                ui.horizontal(|ui| {
                    if theme::secondary(ui, true, "New run").clicked() {
                        self.discard_run();
                    }
                });
            }
            (RunStatus::Running, _) => {
                ui.horizontal(|ui| {
                    theme::dot(ui, p.accent);
                    theme::muted(ui, "Runs in the background — you can close Ferry and come back.");
                });
            }
        }
        for w in run.warnings.iter().take(3) {
            ui.label(theme::small(w).color(p.amber));
        }
        ui.add_space(10.0);

        // Per-table progress.
        TableBuilder::new(ui)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::initial(320.0).at_least(160.0))
            .column(Column::initial(200.0))
            .column(Column::initial(120.0))
            .column(Column::remainder())
            .header(30.0, |mut h| {
                for t in ["Table", "Progress", "Rows", "State"] {
                    h.col(|ui| {
                        ui.label(theme::small(t).color(p.faint));
                    });
                }
            })
            .body(|body| {
                body.rows(34.0, run.tables.len(), |mut row| {
                    let t = &run.tables[row.index()];
                    let st = if t.state == state::RUNNING && run.status != RunStatus::Running { state::QUEUED } else { t.state };
                    let f = if st == state::DONE { 1.0 } else if t.est_bytes > 0 { (t.bytes as f32 / t.est_bytes as f32).min(0.99) } else { 0.0 };
                    let c = match st {
                        state::DONE => p.green,
                        state::FAILED => p.red,
                        state::RUNNING => p.accent,
                        _ => p.faint,
                    };
                    let failed_here = run.issue.as_ref().and_then(|i| i.table.as_deref()) == Some(t.table.to_string().as_str());
                    row.col(|ui| {
                        ui.label(t.table.to_string());
                    });
                    row.col(|ui| {
                        theme::progress(ui, f, 180.0, 6.0, c);
                    });
                    row.col(|ui| {
                        ui.label(RichText::new(theme::fmt_count(t.rows)).color(p.muted));
                    });
                    row.col(|ui| {
                        let label = match st {
                            _ if failed_here => "blocked",
                            state::QUEUED if run.status != RunStatus::Running && t.bytes > 0 => "paused",
                            s => state::label(s),
                        };
                        ui.label(RichText::new(label).color(if failed_here { p.red } else { c }));
                    });
                });
            });
    }

    fn banner(&mut self, ui: &mut egui::Ui, color: Color32, title: &str, hint: &str, detail: Option<&str>, table: Option<&str>) {
        let p = theme::pal(ui);
        egui::Frame::NONE
            .fill(color.gamma_multiply(0.10))
            .stroke(Stroke::new(1.0, color.gamma_multiply(0.45)))
            .corner_radius(CornerRadius::same(12))
            .inner_margin(Margin::symmetric(18, 14))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    theme::dot(ui, color);
                    ui.label(theme::strong(title));
                    if let Some(t) = table {
                        theme::chip(ui, t, color);
                    }
                });
                ui.label(RichText::new(hint).color(p.text));
                if let Some(d) = detail.filter(|d| !d.is_empty()) {
                    egui::CollapsingHeader::new(RichText::new("Details").color(p.muted)).id_salt("issue_detail").show(ui, |ui| {
                        ui.add(egui::Label::new(theme::mono(d)).wrap());
                        if theme::ghost(ui, "Copy").clicked() {
                            ui.ctx().copy_text(d.to_string());
                        }
                    });
                }
            });
        ui.add_space(10.0);
    }

    // ---- schema -----------------------------------------------------------------------------------

    fn schema_migration(&mut self, ui: &mut egui::Ui) {
        self.refresh_plan();
        let p = theme::pal(ui);
        if self.cat_loading && self.migration.catalogs.is_none() {
            ui.horizontal(|ui| {
                ui.spinner();
                theme::muted(ui, "Reading both databases…");
            });
            return;
        }
        if self.migration.diffs.is_empty() {
            theme::empty_state(ui, "Schemas match", "Nothing to apply to the target.");
            return;
        }
        egui::Panel::left("mig_objects")
            .resizable(true)
            .default_size(360.0)
            .min_size(260.0)
            .frame(egui::Frame::NONE.inner_margin(Margin { right: 16, ..Default::default() }))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    theme::muted(ui, format!("{} of {} selected", self.migration.checked.len(), self.migration.diffs.len()));
                });
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    let mut changed = false;
                    for (kind, label) in KINDS {
                        let idx: Vec<usize> = (0..self.migration.diffs.len()).filter(|&i| self.migration.diffs[i].key.kind == kind).collect();
                        if idx.is_empty() {
                            continue;
                        }
                        ui.add_space(8.0);
                        let all = idx.iter().all(|&i| self.migration.checked.contains(&self.migration.diffs[i].key));
                        let mut v = all;
                        ui.horizontal(|ui| {
                            if ui.checkbox(&mut v, "").changed() {
                                for &i in &idx {
                                    let k = self.migration.diffs[i].key.clone();
                                    if v {
                                        self.migration.checked.insert(k);
                                    } else {
                                        self.migration.checked.remove(&k);
                                    }
                                }
                                changed = true;
                            }
                            theme::section(ui, &format!("{label} · {}", idx.len()), false);
                        });
                        for i in idx {
                            let d = &self.migration.diffs[i];
                            let mut on = self.migration.checked.contains(&d.key);
                            ui.horizontal(|ui| {
                                ui.add_space(14.0);
                                if ui.checkbox(&mut on, "").changed() {
                                    if on {
                                        self.migration.checked.insert(d.key.clone());
                                    } else {
                                        self.migration.checked.remove(&d.key);
                                    }
                                    changed = true;
                                }
                                ui.painter().circle_filled(ui.cursor().min + egui::vec2(3.0, 10.0), 3.0, status_color(p, d.status));
                                ui.add_space(10.0);
                                ui.add(egui::Label::new(d.key.display()).truncate()).on_hover_text(format!(
                                    "{}{}",
                                    d.status.label(),
                                    if d.details.is_empty() { String::new() } else { format!("\n{}", d.details.join("\n")) }
                                ));
                            });
                        }
                    }
                    if changed {
                        self.migration.plan_dirty = true;
                    }
                });
            });
        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| {
            let plan = &self.migration.plan;
            ui.horizontal_wrapped(|ui| {
                ui.label(theme::strong(format!("{} statements", plan.steps.len())));
                for (h, n) in plan.hazard_counts() {
                    let c = match h {
                        Hazard::DataLoss | Hazard::Manual => p.red,
                        _ => p.amber,
                    };
                    theme::chip(ui, &format!("{} ×{n}", h.label()), c).on_hover_text(h.explain());
                }
            });
            ui.add_space(6.0);
            let dark = ui.visuals().dark_mode;
            let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, wrap: f32| {
                let mut job = highlight(buf.as_str(), dark, egui::FontId::monospace(theme::MONO));
                job.wrap.max_width = wrap;
                ui.fonts_mut(|f| f.layout_job(job))
            };
            egui::Frame::NONE.fill(p.input).corner_radius(CornerRadius::same(10)).inner_margin(Margin::same(10)).show(ui, |ui| {
                egui::ScrollArea::both().auto_shrink([false, false]).id_salt("script").show(ui, |ui| {
                    let mut text = self.migration.script.as_str();
                    ui.add(egui::TextEdit::multiline(&mut text).code_editor().frame(egui::Frame::NONE).desired_width(f32::INFINITY).layouter(&mut layouter));
                });
            });
        });
    }

    fn open_deploy(&mut self, test: bool) {
        let plan = self.migration.plan.clone();
        self.migration.deploy = Some(DeployState {
            test,
            confirm: String::new(),
            running: false,
            steps: vec![StepState::Pending; plan.steps.len()],
            plan,
            report: None,
            cancel: CancellationToken::new(),
        });
    }

    fn run_deploy(&mut self) {
        let Some(target) = self.params(&self.job().target_id.clone()) else { return };
        let filter = self.schema_filter();
        let ignore_comments = self.settings.ignore_comments;
        let expected = self.migration.target_fp;
        let lock = self.settings.lock_timeout_secs;
        let Some(d) = self.migration.deploy.as_mut() else { return };
        d.running = true;
        d.report = None;
        d.steps = vec![StepState::Pending; d.plan.steps.len()];
        let plan = d.plan.clone();
        let opts = DeployOptions { test_only: d.test, single_transaction: true, lock_timeout_secs: lock };
        let cancel = d.cancel.clone();
        let test = d.test;
        let bus = self.bus.clone();
        self.spawn(async move {
            // The target must still look the way it did when we compared.
            let check = async {
                let c = connect(&target, &SessionOptions::introspection(Side::Target)).await?;
                let mut cat = introspect(&c, &filter).await?;
                if ignore_comments {
                    cat.strip_comments();
                }
                Ok::<_, mb_core::Error>(cat.fingerprint())
            }
            .await;
            match check {
                Ok(fp) if fp != expected => {
                    bus.send(Event::DeployDone { test, result: Err("The target changed since it was read. Refresh, then deploy.".into()) });
                    return;
                }
                Err(e) => {
                    bus.send(Event::DeployDone { test, result: Err(e.to_string()) });
                    return;
                }
                _ => {}
            }
            let b2 = bus.clone();
            let progress = move |p: DeployProgress| b2.send(Event::Deploy(p));
            let r = deploy(&target, &plan, &opts, &cancel, &progress).await.map_err(|e| e.to_string());
            bus.send(Event::DeployDone { test, result: r });
        });
    }

    pub fn on_deploy_done(&mut self, test: bool, result: Result<DeployReport, String>) {
        if let Some(d) = self.migration.deploy.as_mut() {
            d.running = false;
            if let Ok(Some(i)) = result.as_ref().map(|r| r.failed_step) {
                d.steps[i] = StepState::Failed;
            }
            d.report = Some(result.clone());
        }
        match &result {
            Ok(r) if r.ok() => {
                self.notify(Level::Info, if test { "Test passed — rolled back" } else { "Schema deployed" });
                if !test {
                    self.load_catalogs();
                }
            }
            Ok(r) => self.notify(Level::Error, format!("Failed at statement {}", r.failed_step.map(|i| i + 1).unwrap_or(0))),
            Err(e) => self.notify(Level::Error, e.clone()),
        }
    }

    // ---- dialogs ------------------------------------------------------------------------------------

    pub fn migration_dialogs(&mut self, ctx: &egui::Context) {
        self.run_dialog(ctx);
        self.deploy_dialog(ctx);
    }

    fn run_dialog(&mut self, ctx: &egui::Context) {
        if self.migration.confirm.is_none() {
            return;
        }
        let target = self.profile(&self.job().target_id).cloned();
        let dbname = target.as_ref().map(|p| p.params.database.clone()).unwrap_or_default();
        let production = target.as_ref().map(|p| p.production).unwrap_or(false);
        let mode = self.migration.spec.mode;
        let needs = mode != CopyMode::Append || production;
        let n = self.migration.rows.iter().filter(|r| r.checked).count();
        let tname = target.map(|p| p.label()).unwrap_or_default();
        let (mut go, mut close) = (false, false);
        let m = egui::Modal::new(egui::Id::unique("run_modal")).show(ctx, |ui| {
            let p = theme::pal(ui);
            ui.set_width(440.0);
            ui.label(theme::title("Run migration"));
            theme::muted(ui, format!("{n} tables → {tname}"));
            ui.add_space(4.0);
            let warn = match mode {
                CopyMode::Replace => "Target tables are emptied first.",
                CopyMode::Sync => "Target rows missing in the source are deleted.",
                CopyMode::Append => "Rows are added; duplicates stop the run.",
            };
            ui.label(RichText::new(warn).color(if mode == CopyMode::Append { p.text } else { p.red }));
            let c = self.migration.confirm.as_mut().unwrap();
            if needs {
                ui.add_space(4.0);
                theme::field_label(ui, &format!("Type {dbname} to confirm"));
                theme::input(ui, &mut c.text, "", ui.available_width());
            }
            let ok = !needs || c.text.trim() == dbname;
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let b = if needs { theme::danger(ui, ok, "Start") } else { theme::primary(ui, ok, "Start") };
                if b.clicked() {
                    go = true;
                }
                if theme::ghost(ui, "Cancel").clicked() {
                    close = true;
                }
            });
        });
        if m.should_close() {
            close = true;
        }
        if go {
            self.migration.confirm = None;
            self.start_run();
        } else if close {
            self.migration.confirm = None;
        }
    }

    fn deploy_dialog(&mut self, ctx: &egui::Context) {
        if self.migration.deploy.is_none() {
            return;
        }
        let target = self.profile(&self.job().target_id).cloned();
        let dbname = target.as_ref().map(|p| p.params.database.clone()).unwrap_or_default();
        let production = target.as_ref().map(|p| p.production).unwrap_or(false);
        let (mut close, mut run) = (false, false);
        let modal = egui::Modal::new(egui::Id::unique("deploy_modal")).show(ctx, |ui| {
            let p = theme::pal(ui);
            let d = self.migration.deploy.as_mut().unwrap();
            ui.set_width(680.0);
            ui.label(theme::title(if d.test { "Test deploy" } else { "Deploy schema" }));
            theme::muted(ui, if d.test { "Runs in a transaction, then rolls back." } else { "Runs in one transaction — all or nothing." });
            ui.add_space(6.0);
            egui::ScrollArea::vertical().max_height(300.0).auto_shrink([false, true]).show(ui, |ui| {
                for (i, s) in d.plan.steps.iter().enumerate() {
                    let c = match &d.steps[i] {
                        StepState::Pending => p.faint,
                        StepState::Running => p.accent,
                        StepState::Done => p.green,
                        StepState::Skipped => p.amber,
                        StepState::Failed => p.red,
                    };
                    ui.horizontal(|ui| {
                        theme::dot(ui, c);
                        let first = s.sql.lines().next().unwrap_or_default();
                        let mut t = theme::mono(format!("{:>3}  {first}", i + 1));
                        if s.hazards.contains(&Hazard::DataLoss) {
                            t = t.color(p.red);
                        }
                        ui.add(egui::Label::new(t).truncate()).on_hover_text(&s.sql);
                    });
                }
            });
            ui.add_space(6.0);
            let needs = !d.test && (d.plan.has_data_loss() || production);
            if needs && d.report.is_none() {
                theme::field_label(ui, &format!("Type {dbname} to confirm"));
                theme::input(ui, &mut d.confirm, "", 260.0);
            }
            match &d.report {
                Some(Ok(r)) if r.ok() => {
                    ui.label(RichText::new(if d.test { format!("All {} statements passed.", r.executed) } else { format!("Deployed {} statements.", r.executed) }).color(p.green));
                }
                Some(Ok(r)) => {
                    ui.label(RichText::new(format!("Failed at statement {} — rolled back.", r.failed_step.map(|i| i + 1).unwrap_or(0))).color(p.red));
                    egui::ScrollArea::vertical().max_height(120.0).id_salt("err").show(ui, |ui| {
                        ui.add(egui::Label::new(theme::mono(r.error.clone().unwrap_or_default())).wrap());
                    });
                }
                Some(Err(e)) => {
                    ui.label(RichText::new(e).color(p.red));
                }
                None => {}
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if d.running {
                    ui.spinner();
                    if theme::ghost(ui, "Cancel").clicked() {
                        d.cancel.cancel();
                    }
                } else {
                    let done = matches!(&d.report, Some(Ok(r)) if r.ok());
                    let ok = !needs || d.confirm.trim() == dbname;
                    if !done {
                        let label = if d.test { "Run test" } else { "Deploy now" };
                        let b = if needs { theme::danger(ui, ok, label) } else { theme::primary(ui, ok, label) };
                        if b.clicked() {
                            d.cancel = CancellationToken::new();
                            run = true;
                        }
                    }
                    if theme::ghost(ui, "Close").clicked() {
                        close = true;
                    }
                }
            });
        });
        if modal.should_close() && !self.migration.deploying() {
            close = true;
        }
        if run {
            self.run_deploy();
        }
        if close {
            self.migration.deploy = None;
        }
    }
}
