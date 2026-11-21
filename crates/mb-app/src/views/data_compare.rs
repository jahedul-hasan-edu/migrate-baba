use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use eframe::egui::{self, RichText};
use egui_extras::{Column, TableBuilder};
use mb_core::data::compare::{compare_tables, CompareOptions, CompareStatus, TableCompare};
use mb_core::data::sync::{sync_tables, SyncRequest};
use mb_core::data::{state, TableProgress};
use mb_core::model::{Catalog, QName};
use mb_core::progress::{CancellationToken, Level};

use crate::app::{App, Event, Loaded, Purpose};
use crate::store::HistoryEntry;
use crate::theme;

pub struct DataRow {
    pub table: QName,
    pub checked: bool,
    pub in_source: bool,
    pub in_target: bool,
    pub est_rows: i64,
    pub result: Option<TableCompare>,
    pub running: bool,
}

#[derive(Default)]
pub struct DataView {
    pub loading: bool,
    pub running: bool,
    pub rows: Vec<DataRow>,
    pub focus: Option<usize>,
    pub filter: String,
    pub cancel: Option<CancellationToken>,
    pub catalogs: Option<(Arc<Catalog>, Arc<Catalog>)>,
    pub syncing: bool,
    pub sync_progress: Vec<Arc<TableProgress>>,
    pub sync_confirm: Option<String>,
    pub started: Option<Instant>,
    pub elapsed: f64,
}

impl DataView {
    pub fn on_loaded(&mut self, l: &Loaded) {
        self.loading = false;
        let mut names: Vec<QName> = l
            .source
            .tables
            .iter()
            .chain(l.target.tables.iter())
            .filter(|(_, t)| t.partition_key.is_none())
            .map(|(k, _)| k.clone())
            .collect();
        names.sort();
        names.dedup();
        self.rows = names
            .into_iter()
            .map(|t| {
                let s = l.source.tables.get(&t);
                let d = l.target.tables.get(&t);
                DataRow {
                    checked: s.is_some() && d.is_some(),
                    in_source: s.is_some(),
                    in_target: d.is_some(),
                    est_rows: s.or(d).map(|x| x.est_rows).unwrap_or(0),
                    table: t,
                    result: None,
                    running: false,
                }
            })
            .collect();
        self.catalogs = Some((l.source.clone(), l.target.clone()));
        self.focus = None;
    }
}

fn status_color(s: CompareStatus) -> egui::Color32 {
    match s {
        CompareStatus::Identical => theme::GREEN,
        CompareStatus::Different => theme::AMBER,
        CompareStatus::OnlyInSource | CompareStatus::OnlyInTarget | CompareStatus::Error => theme::RED,
        CompareStatus::Skipped => theme::GREY,
    }
}

impl App {
    fn start_data_compare(&mut self) {
        let Some((s, t)) = self.data.catalogs.clone() else { return };
        let (Some(src), Some(tgt)) = (self.params(&self.source_id.clone()), self.params(&self.target_id.clone())) else {
            return;
        };
        let tables: Vec<QName> = self.data.rows.iter().filter(|r| r.checked).map(|r| r.table.clone()).collect();
        if tables.is_empty() {
            self.log(Level::Warn, "Select at least one table".into());
            return;
        }
        for r in &mut self.data.rows {
            if r.checked {
                r.result = None;
            }
        }
        let cancel = CancellationToken::new();
        self.data.cancel = Some(cancel.clone());
        self.data.running = true;
        self.data.started = Some(Instant::now());
        let opts = CompareOptions { chunk_size: self.settings.chunk_size, parallel: self.settings.workers.min(8), ..Default::default() };
        let bus = self.bus.clone();
        self.log(Level::Info, format!("Comparing data of {} table(s)…", tables.len()));
        self.spawn(async move {
            let b1 = bus.clone();
            let b2 = bus.clone();
            let on_start = move |t: QName| b1.send(Event::DataStarted(t));
            let on_result = move |r: TableCompare| b2.send(Event::DataResult(r));
            let r = compare_tables(&src, &tgt, &s, &t, &tables, &opts, &cancel, &on_start, &on_result)
                .await
                .map_err(|e| e.to_string());
            bus.send(Event::DataDone(r));
        });
    }

    pub fn on_data_started(&mut self, t: QName) {
        if let Some(r) = self.data.rows.iter_mut().find(|r| r.table == t) {
            r.running = true;
        }
    }

    pub fn on_data_result(&mut self, res: TableCompare) {
        if let Some(r) = self.data.rows.iter_mut().find(|r| r.table == res.table) {
            r.running = false;
            r.result = Some(res);
        }
    }

    pub fn on_data_done(&mut self, r: Result<(), String>) {
        self.data.running = false;
        self.data.elapsed = self.data.started.map(|s| s.elapsed().as_secs_f64() * 1000.0).unwrap_or(0.0);
        for row in &mut self.data.rows {
            row.running = false;
        }
        match r {
            Ok(()) => {
                let diff = self
                    .data
                    .rows
                    .iter()
                    .filter(|r| r.result.as_ref().map(|x| x.status != CompareStatus::Identical).unwrap_or(false))
                    .count();
                self.notify(format!("Data compare finished in {} — {diff} table(s) differ", theme::fmt_ms(self.data.elapsed)));
            }
            Err(e) => self.log(Level::Error, format!("Data compare: {e}")),
        }
    }

    fn start_sync(&mut self) {
        let Some((s, t)) = self.data.catalogs.clone() else { return };
        let (Some(src), Some(tgt)) = (self.params(&self.source_id.clone()), self.params(&self.target_id.clone())) else {
            return;
        };
        let reqs: Vec<SyncRequest> = self
            .data
            .rows
            .iter()
            .filter(|r| r.checked)
            .filter_map(|r| r.result.as_ref())
            .filter(|x| x.status == CompareStatus::Different)
            .map(|x| SyncRequest { table: x.table.clone(), ranges: x.diff_ranges.clone() })
            .collect();
        if reqs.is_empty() {
            self.log(Level::Warn, "Nothing to sync: compare first, then select tables that differ".into());
            return;
        }
        let progress: Vec<Arc<TableProgress>> = reqs
            .iter()
            .map(|r| Arc::new(TableProgress::new(r.table.clone(), 0, 0)))
            .collect();
        self.data.sync_progress = progress.clone();
        self.data.syncing = true;
        let cancel = CancellationToken::new();
        self.data.cancel = Some(cancel.clone());
        let bus = self.bus.clone();
        self.spawn(async move {
            let log = bus.log_fn();
            let r = sync_tables(&src, &tgt, &s, &t, &reqs, &progress, &cancel, &log).await.map_err(|e| e.to_string());
            bus.send(Event::SyncDone(r));
        });
    }

    pub fn on_sync_done(&mut self, r: Result<(), String>) {
        self.data.syncing = false;
        let (sl, tl) = self.pair_labels();
        let mut lines = Vec::new();
        let mut ok = r.is_ok();
        for p in &self.data.sync_progress {
            let st = p.state();
            if st == state::FAILED {
                ok = false;
            }
            lines.push(format!(
                "{}: {} (+{} ~{} -{}) {}",
                p.table,
                state::label(st),
                p.inserted.load(Ordering::Relaxed),
                p.updated.load(Ordering::Relaxed),
                p.deleted.load(Ordering::Relaxed),
                p.message().unwrap_or_default()
            ));
        }
        if let Err(e) = &r {
            self.log(Level::Error, format!("Sync failed: {e}"));
        } else {
            self.notify("Sync finished — re-run the compare to verify");
        }
        self.add_history(HistoryEntry {
            id: String::new(),
            kind: "Data sync".into(),
            when: String::new(),
            source: sl,
            target: tl,
            ok,
            millis: 0,
            summary: format!("{} table(s) synced", self.data.sync_progress.len()),
            script: String::new(),
            log: lines,
        });
    }

    pub fn ui_data(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.add_enabled(!self.data.loading && !self.data.running, egui::Button::new("⟳ Load tables (F5)")).clicked() {
                self.data.loading = self.load_catalogs(Purpose::Data);
            }
            let can = !self.data.rows.is_empty() && !self.data.running && !self.data.syncing;
            if theme::primary_button(ui, can, "Compare data", false).clicked() {
                self.start_data_compare();
            }
            if self.data.running || self.data.loading || self.data.syncing {
                ui.spinner();
                if (self.data.running || self.data.syncing) && ui.button("Cancel").clicked() {
                    if let Some(c) = &self.data.cancel {
                        c.cancel();
                    }
                }
            }
            ui.separator();
            ui.label("Chunk");
            ui.add(egui::DragValue::new(&mut self.settings.chunk_size).range(500..=1_000_000).speed(500));
            ui.label("rows");
            ui.separator();
            ui.add(egui::TextEdit::singleline(&mut self.data.filter).hint_text("🔍 filter tables").desired_width(160.0));
        });
        ui.separator();

        if self.data.rows.is_empty() {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| {
                ui.heading("Compare table data");
                ui.label("Load the table list, pick tables and press Compare data.");
                ui.label(
                    RichText::new("Rows are hashed in key-range chunks on each server; only hashes travel over the network.")
                        .weak(),
                );
            });
            return;
        }

        egui::Panel::bottom("data_footer").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let diff = self
                    .data
                    .rows
                    .iter()
                    .filter(|r| r.checked && r.result.as_ref().map(|x| x.status == CompareStatus::Different).unwrap_or(false))
                    .count();
                ui.label(format!("{diff} selected table(s) differ"));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::primary_button(ui, diff > 0 && !self.data.syncing && !self.data.running, "Sync to target…", true).clicked() {
                        self.data.sync_confirm = Some(String::new());
                    }
                });
            });
            if !self.data.sync_progress.is_empty() {
                for p in &self.data.sync_progress {
                    let st = p.state();
                    ui.label(
                        RichText::new(format!(
                            "{}  {}  +{} ~{} -{}  {}",
                            p.table,
                            state::label(st),
                            theme::fmt_count(p.inserted.load(Ordering::Relaxed)),
                            theme::fmt_count(p.updated.load(Ordering::Relaxed)),
                            theme::fmt_count(p.deleted.load(Ordering::Relaxed)),
                            p.message().unwrap_or_default()
                        ))
                        .small()
                        .color(if st == state::FAILED { theme::RED } else { ui.visuals().text_color() }),
                    );
                }
            }
            ui.add_space(2.0);
        });

        if let Some(i) = self.data.focus {
            egui::Panel::bottom("data_detail").resizable(true).default_size(150.0).show(ui, |ui| {
                if let Some(r) = self.data.rows.get(i).and_then(|r| r.result.as_ref()) {
                    ui.horizontal(|ui| {
                        ui.strong(r.table.to_string());
                        theme::badge(ui, r.status.label(), status_color(r.status));
                        if let Some(m) = &r.message {
                            ui.label(RichText::new(m).weak());
                        }
                    });
                    ui.columns(3, |cols| {
                        for (ui, (title, keys)) in cols.iter_mut().zip([
                            ("Only in source (will insert)", &r.sample_only_source),
                            ("Only in target (will delete)", &r.sample_only_target),
                            ("Different (will update)", &r.sample_different),
                        ]) {
                            ui.label(RichText::new(title).strong());
                            egui::ScrollArea::vertical().id_salt(title).max_height(110.0).show(ui, |ui| {
                                for k in keys.iter() {
                                    ui.label(RichText::new(k).monospace().small());
                                }
                                if keys.is_empty() {
                                    ui.label(RichText::new("—").weak());
                                }
                            });
                        }
                    });
                } else {
                    ui.label("Compare the table to see sample keys.");
                }
            });
        }

        egui::CentralPanel::default().show(ui, |ui| self.data_table(ui));
    }

    fn data_table(&mut self, ui: &mut egui::Ui) {
        let q = self.data.filter.to_lowercase();
        let idx: Vec<usize> = (0..self.data.rows.len())
            .filter(|&i| q.is_empty() || self.data.rows[i].table.to_string().to_lowercase().contains(&q))
            .collect();
        let all = idx.iter().all(|&i| self.data.rows[i].checked || !(self.data.rows[i].in_source && self.data.rows[i].in_target));
        let mut toggle_all = None;
        let mut focus = None;
        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .sense(egui::Sense::click())
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(28.0))
            .column(Column::initial(240.0).at_least(120.0))
            .column(Column::initial(100.0))
            .column(Column::initial(100.0))
            .column(Column::initial(110.0))
            .column(Column::initial(90.0))
            .column(Column::initial(90.0))
            .column(Column::initial(90.0))
            .column(Column::remainder())
            .header(24.0, |mut h| {
                h.col(|ui| {
                    let mut v = all;
                    if ui.checkbox(&mut v, "").changed() {
                        toggle_all = Some(v);
                    }
                });
                for t in ["Table", "Source rows", "Target rows", "Status", "Only source", "Only target", "Different", "Time"] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(24.0, idx.len(), |mut row| {
                    let i = idx[row.index()];
                    let r = &mut self.data.rows[i];
                    row.set_selected(self.data.focus == Some(i));
                    row.col(|ui| {
                        ui.add_enabled(r.in_source && r.in_target, egui::Checkbox::without_text(&mut r.checked));
                    });
                    row.col(|ui| {
                        ui.label(r.table.to_string());
                    });
                    let res = r.result.as_ref();
                    row.col(|ui| {
                        ui.label(res.map(|x| theme::fmt_count(x.source_rows)).unwrap_or_else(|| {
                            if r.in_source { format!("~{}", theme::fmt_count(r.est_rows.max(0) as u64)) } else { "—".into() }
                        }));
                    });
                    row.col(|ui| {
                        ui.label(res.map(|x| theme::fmt_count(x.target_rows)).unwrap_or_else(|| if r.in_target { "…".into() } else { "—".into() }));
                    });
                    row.col(|ui| {
                        if r.running {
                            ui.spinner();
                        } else if let Some(x) = res {
                            theme::badge(ui, x.status.label(), status_color(x.status));
                        } else if !r.in_target {
                            theme::badge(ui, "missing on target", theme::RED);
                        } else if !r.in_source {
                            theme::badge(ui, "missing on source", theme::RED);
                        }
                    });
                    for v in [res.map(|x| x.only_source), res.map(|x| x.only_target), res.map(|x| x.different)] {
                        row.col(|ui| {
                            match v {
                                Some(0) | None => ui.label(RichText::new("·").weak()),
                                Some(n) => ui.label(RichText::new(theme::fmt_count(n)).color(theme::AMBER)),
                            };
                        });
                    }
                    row.col(|ui| {
                        if let Some(x) = res {
                            ui.label(RichText::new(theme::fmt_ms(x.millis as f64)).weak());
                        }
                    });
                    if row.response().clicked() {
                        focus = Some(i);
                    }
                });
            });
        if let Some(v) = toggle_all {
            for &i in &idx {
                let r = &mut self.data.rows[i];
                if r.in_source && r.in_target {
                    r.checked = v;
                }
            }
        }
        if focus.is_some() {
            self.data.focus = focus;
        }
    }

    pub fn data_dialogs(&mut self, ctx: &egui::Context) {
        let Some(_) = self.data.sync_confirm.as_ref() else { return };
        let target = self.profile(&self.target_id).cloned();
        let dbname = target.as_ref().map(|p| p.params.database.clone()).unwrap_or_default();
        let label = target.as_ref().map(|p| p.label()).unwrap_or_default();
        let mut go = false;
        let mut close = false;
        let m = egui::Modal::new(egui::Id::unique("sync_modal")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.heading("Sync data to target");
            ui.label(format!("Rows in {label} will be inserted, updated and deleted so the selected tables match the source."));
            ui.label(RichText::new(format!("Type `{dbname}` to confirm:")).color(theme::RED));
            let text = self.data.sync_confirm.as_mut().unwrap();
            ui.add(egui::TextEdit::singleline(text).desired_width(220.0));
            let ok = text.trim() == dbname;
            ui.horizontal(|ui| {
                if theme::primary_button(ui, ok, "Sync now", true).clicked() {
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
            self.data.sync_confirm = None;
            self.start_sync();
        } else if close {
            self.data.sync_confirm = None;
        }
    }
}
