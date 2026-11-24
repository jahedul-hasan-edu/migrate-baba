//! Compare: what differs between source and target — schema objects side by side, and table data.

use std::sync::Arc;
use std::time::Instant;

use eframe::egui::{self, Color32, RichText};
use egui_extras::{Column, TableBuilder};
use mb_core::data::compare::{compare_tables, CompareOptions, CompareStatus, TableCompare};
use mb_core::diff::{DiffStatus, ObjectDiff};
use mb_core::model::{Catalog, QName};
use mb_core::progress::{CancellationToken, Level};

use crate::app::{App, Event, Loaded};
use crate::theme::{self, Pal};

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Schema,
    Data,
}

struct DiffRow {
    left: Option<(usize, String)>,
    right: Option<(usize, String)>,
    kind: u8, // 0 equal, 1 only source, 2 only target, 3 changed
}

pub struct DataRow {
    pub table: QName,
    pub checked: bool,
    pub est_rows: i64,
    pub result: Option<TableCompare>,
    pub running: bool,
}

#[derive(Default)]
pub struct CompareView {
    pub tab: Tab,
    pub diffs: Vec<ObjectDiff>,
    pub focus: Option<usize>,
    pub search: String,
    pub show_identical: bool,
    diff_rows: Option<(usize, Vec<DiffRow>)>,
    pub elapsed: f64,
    pub rows: Vec<DataRow>,
    pub data_running: bool,
    cancel: Option<CancellationToken>,
    started: Option<Instant>,
    catalogs: Option<(Arc<Catalog>, Arc<Catalog>)>,
}

pub fn status_color(p: &Pal, s: DiffStatus) -> Color32 {
    match s {
        DiffStatus::Added => p.green,
        DiffStatus::Changed => p.amber,
        DiffStatus::Removed => p.red,
        DiffStatus::Identical => p.faint,
    }
}

fn data_color(p: &Pal, s: CompareStatus) -> Color32 {
    match s {
        CompareStatus::Identical => p.green,
        CompareStatus::Different => p.amber,
        CompareStatus::OnlyInSource | CompareStatus::OnlyInTarget | CompareStatus::Error => p.red,
        CompareStatus::Skipped => p.faint,
    }
}

impl CompareView {
    pub fn on_loaded(&mut self, l: &Loaded) {
        self.diffs = l.diffs.clone();
        self.diffs.sort_by(|a, b| (a.status, a.key.kind, a.key.display()).cmp(&(b.status, b.key.kind, b.key.display())));
        self.focus = self.diffs.iter().position(|d| d.status != DiffStatus::Identical);
        self.diff_rows = None;
        self.elapsed = l.millis;
        self.catalogs = Some((l.source.clone(), l.target.clone()));
        self.rows = l
            .source
            .tables
            .values()
            .filter(|t| t.partition_key.is_none() && l.target.tables.contains_key(&t.name))
            .map(|t| DataRow { table: t.name.clone(), checked: true, est_rows: t.est_rows, result: None, running: false })
            .collect();
        self.rows.sort_by(|a, b| a.table.cmp(&b.table));
    }

    pub fn on_data_started(&mut self, t: QName) {
        if let Some(r) = self.rows.iter_mut().find(|r| r.table == t) {
            r.running = true;
        }
    }

    pub fn on_data_result(&mut self, res: TableCompare) {
        if let Some(r) = self.rows.iter_mut().find(|r| r.table == res.table) {
            r.running = false;
            r.result = Some(res);
        }
    }

    pub fn on_data_done(&mut self, r: Result<(), String>) -> Result<(), String> {
        self.data_running = false;
        self.cancel = None;
        for row in &mut self.rows {
            row.running = false;
        }
        r.map_err(|e| format!("Data compare: {e}"))
    }

    fn visible(&self) -> Vec<(Option<DiffStatus>, usize)> {
        let q = self.search.to_lowercase();
        let mut out = Vec::new();
        for status in [DiffStatus::Added, DiffStatus::Changed, DiffStatus::Removed, DiffStatus::Identical] {
            if status == DiffStatus::Identical && !self.show_identical {
                continue;
            }
            let rows: Vec<usize> = (0..self.diffs.len())
                .filter(|&i| self.diffs[i].status == status)
                .filter(|&i| q.is_empty() || self.diffs[i].key.display().to_lowercase().contains(&q))
                .collect();
            if rows.is_empty() {
                continue;
            }
            out.push((Some(status), rows.len()));
            out.extend(rows.into_iter().map(|i| (None, i)));
        }
        out
    }

    fn build_rows(d: &ObjectDiff) -> Vec<DiffRow> {
        let old: Vec<&str> = d.target_ddl.lines().collect();
        let new: Vec<&str> = d.source_ddl.lines().collect();
        let mut rows = Vec::new();
        for op in similar::capture_diff_slices(similar::Algorithm::Myers, &old, &new) {
            let (tag, o, n) = op.as_tag_tuple();
            match tag {
                similar::DiffTag::Equal => {
                    for (i, j) in o.zip(n) {
                        rows.push(DiffRow { left: Some((j + 1, new[j].to_string())), right: Some((i + 1, old[i].to_string())), kind: 0 });
                    }
                }
                similar::DiffTag::Delete => {
                    rows.extend(o.map(|i| DiffRow { left: None, right: Some((i + 1, old[i].to_string())), kind: 2 }));
                }
                similar::DiffTag::Insert => {
                    rows.extend(n.map(|j| DiffRow { left: Some((j + 1, new[j].to_string())), right: None, kind: 1 }));
                }
                similar::DiffTag::Replace => {
                    let (o, n): (Vec<usize>, Vec<usize>) = (o.collect(), n.collect());
                    for k in 0..o.len().max(n.len()) {
                        rows.push(DiffRow {
                            left: n.get(k).map(|&j| (j + 1, new[j].to_string())),
                            right: o.get(k).map(|&i| (i + 1, old[i].to_string())),
                            kind: 3,
                        });
                    }
                }
            }
        }
        rows
    }
}

impl App {
    pub fn compare_actions(&mut self, ui: &mut egui::Ui) {
        match self.compare.tab {
            Tab::Schema => {
                if theme::primary(ui, !self.cat_loading, if self.cat_loading { "Reading…" } else { "Refresh" }).clicked() {
                    self.load_catalogs();
                }
            }
            Tab::Data => {
                if self.compare.data_running {
                    if theme::secondary(ui, true, "Stop").clicked() {
                        if let Some(c) = &self.compare.cancel {
                            c.cancel();
                        }
                    }
                } else {
                    let n = self.compare.rows.iter().filter(|r| r.checked).count();
                    if theme::primary(ui, n > 0 && !self.cat_loading, &format!("Compare data ({n})")).clicked() {
                        self.start_data_compare();
                    }
                }
            }
        }
    }

    fn start_data_compare(&mut self) {
        let Some((s, t)) = self.compare.catalogs.clone() else { return };
        let job = self.job().clone();
        let (Some(src), Some(tgt)) = (self.params(&job.source_id), self.params(&job.target_id)) else { return };
        let tables: Vec<QName> = self.compare.rows.iter().filter(|r| r.checked).map(|r| r.table.clone()).collect();
        for r in self.compare.rows.iter_mut().filter(|r| r.checked) {
            r.result = None;
        }
        let cancel = CancellationToken::new();
        self.compare.cancel = Some(cancel.clone());
        self.compare.data_running = true;
        self.compare.started = Some(Instant::now());
        let opts = CompareOptions { chunk_size: self.settings.chunk_size, parallel: job.data.workers.clamp(1, 8), ..Default::default() };
        let bus = self.bus.clone();
        self.spawn(async move {
            let (b1, b2) = (bus.clone(), bus.clone());
            let on_start = move |t: QName| b1.send(Event::DataStarted(t));
            let on_result = move |r: TableCompare| b2.send(Event::DataResult(r));
            let r = compare_tables(&src, &tgt, &s, &t, &tables, &opts, &cancel, &on_start, &on_result).await.map_err(|e| e.to_string());
            if r.is_ok() {
                bus.send(Event::Notice(Level::Info, "Data compare finished".into()));
            }
            bus.send(Event::DataDone(r));
        });
    }

    pub fn ui_compare(&mut self, ui: &mut egui::Ui) {
        let p = theme::pal(ui);
        ui.horizontal(|ui| {
            let mut tab = self.compare.tab;
            if theme::segmented(ui, &mut tab, &[(Tab::Schema, "Schema"), (Tab::Data, "Data")]) {
                self.compare.tab = tab;
            }
            ui.add_space(8.0);
            if self.compare.tab == Tab::Schema && !self.compare.diffs.is_empty() {
                let s = mb_core::diff::summarize(&self.compare.diffs);
                theme::chip(ui, &format!("+{} new", s.added), p.green);
                theme::chip(ui, &format!("~{} changed", s.changed), p.amber);
                theme::chip(ui, &format!("−{} only in target", s.removed), p.red);
                theme::muted(ui, format!("{} identical", s.identical));
            }
        });
        ui.add_space(14.0);
        if self.cat_loading && self.cat.is_none() {
            ui.horizontal(|ui| {
                ui.spinner();
                theme::muted(ui, "Reading both databases…");
            });
            return;
        }
        if self.cat.is_none() {
            theme::empty_state(ui, "Nothing loaded", "Press Refresh to read both databases.");
            return;
        }
        match self.compare.tab {
            Tab::Schema => self.schema_compare(ui),
            Tab::Data => self.data_compare(ui),
        }
    }

    fn schema_compare(&mut self, ui: &mut egui::Ui) {
        let p = theme::pal(ui);
        if self.compare.diffs.iter().all(|d| d.status == DiffStatus::Identical) && !self.compare.show_identical {
            theme::empty_state(ui, "Schemas match", "Source and target have the same objects.");
            return;
        }
        egui::Panel::left("cmp_list")
            .resizable(true)
            .default_size(340.0)
            .min_size(240.0)
            .frame(egui::Frame::NONE.inner_margin(egui::Margin { right: 16, ..Default::default() }))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    theme::input(ui, &mut self.compare.search, "Filter objects", ui.available_width() - 110.0);
                    ui.checkbox(&mut self.compare.show_identical, "Identical");
                });
                ui.add_space(6.0);
                let items = self.compare.visible();
                egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, 30.0, items.len(), |ui, range| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for &(head, i) in &items[range] {
                        if let Some(status) = head {
                            ui.allocate_ui(egui::vec2(ui.available_width(), 30.0), |ui| {
                                ui.horizontal_centered(|ui| {
                                    ui.add_space(8.0);
                                    theme::section(ui, &format!("{} · {i}", status.label()), false);
                                });
                            });
                            continue;
                        }
                        let d = &self.compare.diffs[i];
                        let sel = self.compare.focus == Some(i);
                        let (resp, rect) = theme::row(ui, 30.0, sel);
                        let painter = ui.painter().with_clip_rect(rect);
                        painter.text(
                            rect.left_center() + egui::vec2(10.0, 0.0),
                            egui::Align2::LEFT_CENTER,
                            d.key.kind.short(),
                            egui::FontId::monospace(11.0),
                            status_color(p, d.status),
                        );
                        painter.text(
                            rect.left_center() + egui::vec2(46.0, 0.0),
                            egui::Align2::LEFT_CENTER,
                            d.key.display(),
                            egui::FontId::proportional(theme::BODY),
                            if sel { p.text } else { p.muted },
                        );
                        let resp = if d.details.is_empty() { resp } else { resp.on_hover_text(d.details.join("\n")) };
                        if resp.clicked() {
                            self.compare.focus = Some(i);
                        }
                    }
                });
            });
        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| self.side_by_side(ui));
    }

    fn side_by_side(&mut self, ui: &mut egui::Ui) {
        let p = theme::pal(ui);
        let Some(i) = self.compare.focus.filter(|&i| i < self.compare.diffs.len()) else {
            theme::empty_state(ui, "Pick an object", "Its definition shows here for both databases.");
            return;
        };
        if self.compare.diff_rows.as_ref().map(|(k, _)| *k != i).unwrap_or(true) {
            self.compare.diff_rows = Some((i, CompareView::build_rows(&self.compare.diffs[i])));
        }
        let d = &self.compare.diffs[i];
        ui.horizontal(|ui| {
            ui.label(theme::title(d.key.display()));
            theme::chip(ui, d.status.label(), status_color(p, d.status));
        });
        for det in d.details.iter().take(4) {
            theme::muted(ui, format!("• {det}"));
        }
        ui.add_space(6.0);
        let half = (ui.available_width() - 12.0) / 2.0;
        ui.horizontal(|ui| {
            ui.add_sized([half, 18.0], egui::Label::new(theme::small("SOURCE").color(p.faint)));
            ui.add_sized([half, 18.0], egui::Label::new(theme::small("TARGET").color(p.faint)));
        });
        let rows = &self.compare.diff_rows.as_ref().unwrap().1;
        let font = egui::FontId::monospace(theme::MONO);
        let row_h = ui.fonts_mut(|f| f.row_height(&font)) + 3.0;
        egui::Frame::NONE.fill(p.input).corner_radius(egui::CornerRadius::same(10)).inner_margin(egui::Margin::same(6)).show(ui, |ui| {
            egui::ScrollArea::both().auto_shrink([false, false]).id_salt("sbs").show_rows(ui, row_h, rows.len(), |ui, range| {
                for r in &rows[range] {
                    let (lc, rc) = match r.kind {
                        1 => (Some(p.green), None),
                        2 => (None, Some(p.red)),
                        3 => (Some(p.amber), Some(p.amber)),
                        _ => (None, None),
                    };
                    ui.horizontal(|ui| {
                        for (cell, color) in [(&r.left, lc), (&r.right, rc)] {
                            let (rect, _) = ui.allocate_exact_size(egui::vec2(half - 6.0, row_h), egui::Sense::hover());
                            if let Some(c) = color {
                                ui.painter().rect_filled(rect, 0.0, c.gamma_multiply(0.14));
                            }
                            if let Some((n, text)) = cell {
                                ui.painter().text(rect.left_center() + egui::vec2(4.0, 0.0), egui::Align2::LEFT_CENTER, format!("{n:>4}"), font.clone(), p.faint);
                                ui.painter().with_clip_rect(rect.intersect(ui.clip_rect())).text(
                                    rect.left_center() + egui::vec2(44.0, 0.0),
                                    egui::Align2::LEFT_CENTER,
                                    text,
                                    font.clone(),
                                    p.text,
                                );
                            }
                        }
                    });
                }
            });
        });
    }

    fn data_compare(&mut self, ui: &mut egui::Ui) {
        let p = theme::pal(ui);
        if self.compare.rows.is_empty() {
            theme::empty_state(ui, "No common tables", "Migrate the schema first, then compare data.");
            return;
        }
        let running = self.compare.data_running;
        let all = self.compare.rows.iter().all(|r| r.checked);
        let mut toggle = None;
        TableBuilder::new(ui)
            .striped(false)
            .resizable(true)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(32.0))
            .column(Column::initial(280.0).at_least(140.0))
            .column(Column::initial(110.0))
            .column(Column::initial(110.0))
            .column(Column::initial(130.0))
            .column(Column::remainder())
            .header(30.0, |mut h| {
                h.col(|ui| {
                    let mut v = all;
                    if ui.add_enabled(!running, egui::Checkbox::without_text(&mut v)).changed() {
                        toggle = Some(v);
                    }
                });
                for t in ["Table", "Source rows", "Target rows", "Status", "Differences"] {
                    h.col(|ui| {
                        ui.label(theme::small(t).color(p.faint));
                    });
                }
            })
            .body(|body| {
                body.rows(34.0, self.compare.rows.len(), |mut row| {
                    let r = &mut self.compare.rows[row.index()];
                    row.col(|ui| {
                        ui.add_enabled(!running, egui::Checkbox::without_text(&mut r.checked));
                    });
                    row.col(|ui| {
                        ui.label(r.table.to_string());
                    });
                    let res = r.result.as_ref();
                    row.col(|ui| {
                        ui.label(res.map(|x| theme::fmt_count(x.source_rows)).unwrap_or_else(|| format!("~{}", theme::fmt_count(r.est_rows.max(0) as u64))));
                    });
                    row.col(|ui| {
                        if let Some(x) = res {
                            ui.label(theme::fmt_count(x.target_rows));
                        }
                    });
                    row.col(|ui| {
                        if r.running {
                            ui.spinner();
                        } else if let Some(x) = res {
                            theme::chip(ui, x.status.label(), data_color(p, x.status)).on_hover_text(x.message.clone().unwrap_or_default());
                        }
                    });
                    row.col(|ui| {
                        if let Some(x) = res {
                            let parts: Vec<String> = [(x.only_source, "missing"), (x.only_target, "extra"), (x.different, "changed")]
                                .iter()
                                .filter(|(n, _)| *n > 0)
                                .map(|(n, l)| format!("{} {l}", theme::fmt_count(*n)))
                                .collect();
                            if parts.is_empty() {
                                ui.label(RichText::new("—").color(p.faint));
                            } else {
                                ui.label(RichText::new(parts.join(" · ")).color(p.amber));
                            }
                        }
                    });
                });
            });
        if let Some(v) = toggle {
            for r in &mut self.compare.rows {
                r.checked = v;
            }
        }
    }
}
