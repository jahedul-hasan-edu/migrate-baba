use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use eframe::egui::{self, RichText};
use egui_extras::{Column, TableBuilder};
use mb_core::conn::{connect, ConnParams, SessionOptions, Side};
use mb_core::data::copy::{self, plan_jobs, CopyMode, CopyOptions, CopySummary, TableJob};
use mb_core::data::{state, TableProgress};
use mb_core::deploy::{deploy, DeployOptions};
use mb_core::diff::{ObjectKey, ObjectKind};
use mb_core::introspect::{introspect, SchemaFilter};
use mb_core::model::{Catalog, QName};
use mb_core::plan::{plan_selection, PlanOptions};
use mb_core::progress::{CancellationToken, Level};

use crate::app::{App, Event, Loaded, Purpose};
use crate::store::{HistoryEntry, Settings};
use crate::theme;

pub struct Prepared {
    pub jobs: Vec<TableJob>,
    pub warnings: Vec<String>,
}

pub struct MigRow {
    pub table: QName,
    pub checked: bool,
    pub in_target: bool,
    pub est_rows: i64,
    pub bytes: i64,
}

pub struct MigrateView {
    pub loading: bool,
    pub running: bool,
    pub rows: Vec<MigRow>,
    pub opts: CopyOptions,
    pub create_missing: bool,
    pub filter: String,
    pub progress: HashMap<QName, Arc<TableProgress>>,
    pub cancel: Option<CancellationToken>,
    pub summary: Option<Result<CopySummary, String>>,
    pub started: Option<Instant>,
    pub finished: Option<f64>,
    pub confirm: Option<String>,
    samples: VecDeque<(Instant, u64)>,
    pub warnings: Vec<String>,
}

impl MigrateView {
    pub fn new(settings: &Settings) -> Self {
        Self {
            loading: false,
            running: false,
            rows: Vec::new(),
            opts: CopyOptions { workers: settings.workers, ..Default::default() },
            create_missing: true,
            filter: String::new(),
            progress: HashMap::new(),
            cancel: None,
            summary: None,
            started: None,
            finished: None,
            confirm: None,
            samples: VecDeque::new(),
            warnings: Vec::new(),
        }
    }

    pub fn on_loaded(&mut self, l: &Loaded) {
        self.loading = false;
        let mut v: Vec<MigRow> = l
            .source
            .tables
            .values()
            .filter(|t| t.partition_key.is_none())
            .map(|t| MigRow {
                table: t.name.clone(),
                checked: true,
                in_target: l.target.tables.contains_key(&t.name),
                est_rows: t.est_rows,
                bytes: t.total_bytes,
            })
            .collect();
        v.sort_by(|a, b| a.table.cmp(&b.table));
        self.rows = v;
        self.progress.clear();
        self.summary = None;
    }

    fn totals(&self) -> (u64, u64) {
        let mut done = 0;
        let mut est = 0;
        for p in self.progress.values() {
            done += p.bytes.load(Ordering::Relaxed);
            est += p.est_bytes.max(1);
        }
        (done, est)
    }
}

async fn read_pair(src: &ConnParams, tgt: &ConnParams, f: &SchemaFilter) -> mb_core::Result<(Catalog, Catalog)> {
    let a = async {
        let c = connect(src, &SessionOptions::introspection(Side::Source)).await?;
        introspect(&c, f).await
    };
    let b = async {
        let c = connect(tgt, &SessionOptions::introspection(Side::Target)).await?;
        introspect(&c, f).await
    };
    tokio::try_join!(a, b)
}

impl App {
    fn start_copy(&mut self) {
        let (Some(src), Some(tgt)) = (self.params(&self.source_id.clone()), self.params(&self.target_id.clone())) else {
            self.log(Level::Warn, "Pick a source and a target first".into());
            return;
        };
        let tables: Vec<QName> = self.migrate.rows.iter().filter(|r| r.checked).map(|r| r.table.clone()).collect();
        if tables.is_empty() {
            self.log(Level::Warn, "Select at least one table".into());
            return;
        }
        let filter = self.schema_filter();
        let opts = self.migrate.opts.clone();
        let create_missing = self.migrate.create_missing;
        let lock = self.settings.lock_timeout_secs;
        let cancel = CancellationToken::new();
        self.migrate.cancel = Some(cancel.clone());
        self.migrate.running = true;
        self.migrate.summary = None;
        self.migrate.progress.clear();
        self.migrate.samples.clear();
        self.migrate.warnings.clear();
        self.migrate.started = Some(Instant::now());
        self.migrate.finished = None;
        let bus = self.bus.clone();
        self.log(Level::Info, format!("Starting {} of {} table(s)…", opts.mode.label(), tables.len()));
        self.spawn(async move {
            let log = bus.log_fn();
            let r = async {
                let (mut s, mut t) = read_pair(&src, &tgt, &filter).await?;
                if create_missing {
                    let missing: Vec<&QName> = tables.iter().filter(|q| !t.tables.contains_key(*q)).collect();
                    if !missing.is_empty() {
                        let mut sel: BTreeSet<ObjectKey> = missing
                            .iter()
                            .map(|q| ObjectKey::new(ObjectKind::Table, q.schema.clone(), q.name.clone()))
                            .collect();
                        // Bring along schemas, types and sequences that only exist in the source.
                        for k in s.schemas.keys().filter(|k| !t.schemas.contains_key(*k)) {
                            sel.insert(ObjectKey::new(ObjectKind::Schema, "", k.clone()));
                        }
                        for k in s.types.keys().filter(|k| !t.types.contains_key(*k)) {
                            sel.insert(ObjectKey::new(ObjectKind::Type, k.schema.clone(), k.name.clone()));
                        }
                        for k in s.sequences.keys().filter(|k| !t.sequences.contains_key(*k)) {
                            sel.insert(ObjectKey::new(ObjectKind::Sequence, k.schema.clone(), k.name.clone()));
                        }
                        let plan = plan_selection(&s, &t, &sel, &PlanOptions::default());
                        log(Level::Info, format!("Creating {} missing table(s) ({} statements)", missing.len(), plan.steps.len()));
                        let rep = deploy(
                            &tgt,
                            &plan,
                            &DeployOptions { lock_timeout_secs: lock, ..Default::default() },
                            &cancel,
                            &|_| {},
                        )
                        .await?;
                        if !rep.ok() {
                            return Err(mb_core::Error::Invalid(format!(
                                "creating missing tables failed: {}",
                                rep.error.unwrap_or_default()
                            )));
                        }
                        (s, t) = read_pair(&src, &tgt, &filter).await?;
                    }
                }
                let (jobs, warnings) = plan_jobs(&s, &t, &tables);
                for w in &warnings {
                    log(Level::Warn, w.clone());
                }
                bus.send(Event::CopyPrepared(Ok(Prepared { jobs: jobs.clone(), warnings })));
                copy::run(&src, &tgt, &jobs, s.server_version, &opts, &cancel, &log).await
            }
            .await
            .map_err(|e| e.to_string());
            bus.send(Event::CopyDone(r));
        });
    }

    pub fn on_copy_prepared(&mut self, r: Result<Prepared, String>) {
        if let Ok(p) = r {
            self.migrate.progress = p.jobs.iter().map(|j| (j.table.clone(), j.progress.clone())).collect();
            self.migrate.warnings = p.warnings;
            for row in &mut self.migrate.rows {
                if self.migrate.progress.contains_key(&row.table) {
                    row.in_target = true;
                }
            }
        }
    }

    pub fn on_copy_done(&mut self, r: Result<CopySummary, String>) {
        self.migrate.running = false;
        self.migrate.finished = self.migrate.started.map(|s| s.elapsed().as_secs_f64() * 1000.0);
        let (sl, tl) = self.pair_labels();
        let mut lines = Vec::new();
        let mut v: Vec<_> = self.migrate.progress.values().cloned().collect();
        v.sort_by(|a, b| a.table.cmp(&b.table));
        for p in v {
            lines.push(format!(
                "{}: {} · {} rows · {} {}",
                p.table,
                state::label(p.state()),
                p.rows.load(Ordering::Relaxed),
                theme::fmt_bytes(p.bytes.load(Ordering::Relaxed)),
                p.message().unwrap_or_default()
            ));
        }
        let (ok, summary, millis) = match &r {
            Ok(s) => {
                let msg = format!(
                    "{} table(s) copied, {} failed · {} rows · {} in {}",
                    s.tables_ok,
                    s.tables_failed,
                    theme::fmt_count(s.rows),
                    theme::fmt_bytes(s.bytes),
                    theme::fmt_ms(s.millis as f64)
                );
                if s.tables_failed == 0 {
                    self.notify(msg.clone());
                } else {
                    self.log(Level::Warn, msg.clone());
                }
                (s.tables_failed == 0, msg, s.millis)
            }
            Err(e) => {
                self.log(Level::Error, format!("Data migration: {e}"));
                (false, e.clone(), 0)
            }
        };
        self.migrate.summary = Some(r);
        self.add_history(HistoryEntry {
            id: String::new(),
            kind: format!("Data migration ({:?})", self.migrate.opts.mode),
            when: String::new(),
            source: sl,
            target: tl,
            ok,
            millis,
            summary,
            script: String::new(),
            log: lines,
        });
    }

    pub fn ui_migrate(&mut self, ui: &mut egui::Ui) {
        // Throughput samples (once per ~250 ms).
        if self.migrate.running {
            let (done, _) = self.migrate.totals();
            let now = Instant::now();
            if self.migrate.samples.back().map(|(t, _)| now.duration_since(*t).as_millis() > 250).unwrap_or(true) {
                self.migrate.samples.push_back((now, done));
                while self.migrate.samples.len() > 40 {
                    self.migrate.samples.pop_front();
                }
            }
        }

        ui.horizontal(|ui| {
            if ui.add_enabled(!self.migrate.loading && !self.migrate.running, egui::Button::new("⟳ Load tables (F5)")).clicked() {
                self.migrate.loading = self.load_catalogs(Purpose::Migrate);
            }
            if self.migrate.loading {
                ui.spinner();
            }
            let n = self.migrate.rows.iter().filter(|r| r.checked).count();
            let danger = self.migrate.opts.mode != CopyMode::Append;
            if theme::primary_button(ui, n > 0 && !self.migrate.running, &format!("Start migration ({n})"), danger).clicked() {
                self.migrate.confirm = Some(String::new());
            }
            if self.migrate.running && ui.button("Cancel").clicked() {
                if let Some(c) = &self.migrate.cancel {
                    c.cancel();
                }
            }
            ui.separator();
            ui.add(egui::TextEdit::singleline(&mut self.migrate.filter).hint_text("🔍 filter tables").desired_width(160.0));
        });
        ui.add_space(4.0);

        // Options
        ui.add_enabled_ui(!self.migrate.running, |ui| {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label("Mode");
                    egui::ComboBox::from_id_salt("copy_mode").selected_text(self.migrate.opts.mode.label()).show_ui(ui, |ui| {
                        for m in [CopyMode::Replace, CopyMode::Append, CopyMode::Sync] {
                            ui.selectable_value(&mut self.migrate.opts.mode, m, m.label());
                        }
                    });
                    ui.separator();
                    ui.label("Workers");
                    ui.add(egui::Slider::new(&mut self.migrate.opts.workers, 1..=32));
                    ui.separator();
                    ui.checkbox(&mut self.migrate.create_missing, "Create missing tables");
                    ui.checkbox(&mut self.migrate.opts.consistent_snapshot, "Consistent snapshot");
                    ui.checkbox(&mut self.migrate.opts.disable_triggers, "Disable triggers/FK checks");
                    ui.checkbox(&mut self.migrate.opts.reset_sequences, "Reset sequences");
                    ui.checkbox(&mut self.migrate.opts.analyze, "ANALYZE after");
                    ui.checkbox(&mut self.migrate.opts.stop_on_error, "Stop on first error");
                });
            });
        });

        // Overall progress
        if !self.migrate.progress.is_empty() {
            let (done, est) = self.migrate.totals();
            let frac = if self.migrate.summary.is_some() { 1.0 } else { (done as f32 / est as f32).min(0.99) };
            let rate = match (self.migrate.samples.front(), self.migrate.samples.back()) {
                (Some((t0, b0)), Some((t1, b1))) if t1 > t0 => (b1 - b0) as f64 / t1.duration_since(*t0).as_secs_f64(),
                _ => 0.0,
            };
            let elapsed = self.migrate.finished.unwrap_or_else(|| self.migrate.started.map(|s| s.elapsed().as_secs_f64() * 1000.0).unwrap_or(0.0));
            let eta = if rate > 0.0 && est > done { format!(" · ETA {}", theme::fmt_ms((est - done) as f64 / rate * 1000.0)) } else { String::new() };
            ui.add_space(4.0);
            ui.add(
                egui::ProgressBar::new(frac)
                    .text(format!(
                        "{} transferred · {}/s · {}{}",
                        theme::fmt_bytes(done),
                        theme::fmt_bytes(rate as u64),
                        theme::fmt_ms(elapsed),
                        if self.migrate.running { eta } else { String::new() }
                    ))
                    .animate(self.migrate.running),
            );
            for w in &self.migrate.warnings {
                ui.label(RichText::new(format!("⚠ {w}")).small().color(theme::AMBER));
            }
            if let Some(Ok(s)) = &self.migrate.summary {
                for w in &s.warnings {
                    ui.label(RichText::new(format!("⚠ {w}")).small().color(theme::AMBER));
                }
            }
        }
        ui.separator();

        if self.migrate.rows.is_empty() {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| {
                ui.heading("Copy data between databases");
                ui.label("Load the table list, choose tables and a mode, then start.");
                ui.label(RichText::new("Parallel binary COPY streams · consistent snapshot · FK-aware ordering").weak());
            });
            return;
        }
        self.migrate_table(ui);
    }

    fn migrate_table(&mut self, ui: &mut egui::Ui) {
        let q = self.migrate.filter.to_lowercase();
        let idx: Vec<usize> = (0..self.migrate.rows.len())
            .filter(|&i| q.is_empty() || self.migrate.rows[i].table.to_string().to_lowercase().contains(&q))
            .collect();
        let all = idx.iter().all(|&i| self.migrate.rows[i].checked);
        let mut toggle = None;
        let running = self.migrate.running;
        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(28.0))
            .column(Column::initial(240.0).at_least(120.0))
            .column(Column::initial(90.0))
            .column(Column::initial(80.0))
            .column(Column::initial(90.0))
            .column(Column::initial(160.0))
            .column(Column::initial(100.0))
            .column(Column::initial(80.0))
            .column(Column::remainder())
            .header(24.0, |mut h| {
                h.col(|ui| {
                    let mut v = all;
                    if ui.add_enabled(!running, egui::Checkbox::without_text(&mut v)).changed() {
                        toggle = Some(v);
                    }
                });
                for t in ["Table", "Est. rows", "Size", "State", "Progress", "Rows", "Time", "Message"] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(24.0, idx.len(), |mut row| {
                    let r = &mut self.migrate.rows[idx[row.index()]];
                    let p = self.migrate.progress.get(&r.table);
                    row.col(|ui| {
                        ui.add_enabled(!running, egui::Checkbox::without_text(&mut r.checked));
                    });
                    row.col(|ui| {
                        ui.label(r.table.to_string());
                        if !r.in_target {
                            theme::badge(ui, "new", theme::GREEN).on_hover_text("Missing on target");
                        }
                    });
                    row.col(|ui| {
                        ui.label(theme::fmt_count(r.est_rows.max(0) as u64));
                    });
                    row.col(|ui| {
                        ui.label(theme::fmt_bytes(r.bytes.max(0) as u64));
                    });
                    let st = p.map(|p| p.state());
                    row.col(|ui| {
                        if let Some(st) = st {
                            let c = match st {
                                state::DONE => theme::GREEN,
                                state::FAILED => theme::RED,
                                state::RUNNING => theme::ACCENT,
                                _ => theme::GREY,
                            };
                            theme::badge(ui, state::label(st), c);
                        }
                    });
                    row.col(|ui| {
                        if let Some(p) = p {
                            let frac = if p.state() == state::DONE {
                                1.0
                            } else if p.est_bytes > 0 {
                                (p.bytes.load(Ordering::Relaxed) as f32 / p.est_bytes as f32).min(0.99)
                            } else {
                                0.0
                            };
                            ui.add(egui::ProgressBar::new(frac).desired_height(12.0).desired_width(150.0));
                        }
                    });
                    row.col(|ui| {
                        if let Some(p) = p {
                            ui.label(theme::fmt_count(p.rows_estimate()));
                        }
                    });
                    row.col(|ui| {
                        if let Some(p) = p {
                            let e = p.elapsed();
                            if !e.is_zero() {
                                ui.label(RichText::new(theme::fmt_ms(e.as_secs_f64() * 1000.0)).weak());
                            }
                        }
                    });
                    row.col(|ui| {
                        if let Some(m) = p.and_then(|p| p.message()) {
                            ui.label(RichText::new(m.lines().next().unwrap_or_default()).small().color(theme::RED)).on_hover_text(m);
                        }
                    });
                });
            });
        if let Some(v) = toggle {
            for &i in &idx {
                self.migrate.rows[i].checked = v;
            }
        }
    }

    pub fn migrate_dialogs(&mut self, ctx: &egui::Context) {
        let Some(_) = self.migrate.confirm.as_ref() else { return };
        let target = self.profile(&self.target_id).cloned();
        let dbname = target.as_ref().map(|p| p.params.database.clone()).unwrap_or_default();
        let label = target.as_ref().map(|p| p.label()).unwrap_or_default();
        let production = target.as_ref().map(|p| p.production).unwrap_or(false);
        let mode = self.migrate.opts.mode;
        let needs = mode != CopyMode::Append || production;
        let n = self.migrate.rows.iter().filter(|r| r.checked).count();
        let mut go = false;
        let mut close = false;
        let m = egui::Modal::new(egui::Id::unique("copy_modal")).show(ctx, |ui| {
            ui.set_width(480.0);
            ui.heading("Start data migration");
            ui.label(format!("{} · {n} table(s) → {label}", mode.label()));
            match mode {
                CopyMode::Replace => {
                    ui.label(RichText::new("All rows in the selected target tables will be deleted (TRUNCATE) first.").color(theme::RED));
                }
                CopyMode::Sync => {
                    ui.label(RichText::new("Target rows not present in the source will be deleted.").color(theme::RED));
                }
                CopyMode::Append => {
                    ui.label("Rows are inserted; duplicates will fail on unique constraints.");
                }
            }
            let text = self.migrate.confirm.as_mut().unwrap();
            if needs {
                ui.label(RichText::new(format!("Type `{dbname}` to confirm:")).color(theme::RED));
                ui.add(egui::TextEdit::singleline(text).desired_width(220.0));
            }
            let ok = !needs || text.trim() == dbname;
            ui.horizontal(|ui| {
                if theme::primary_button(ui, ok, "Start", needs).clicked() {
                    go = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        if m.should_close() {
            close = true;
        }
        if go {
            self.migrate.confirm = None;
            self.start_copy();
        } else if close {
            self.migrate.confirm = None;
        }
    }
}
