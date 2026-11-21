use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Instant;

use eframe::egui::{self, Color32, RichText};
use mb_core::conn::{connect, SessionOptions, Side};
use mb_core::deploy::{deploy, DeployOptions, DeployProgress, DeployReport};
use mb_core::diff::{DiffStatus, ObjectDiff, ObjectKey, ObjectKind};
use mb_core::introspect::introspect;
use mb_core::model::Catalog;
use mb_core::plan::{plan_selection, Hazard, Plan, PlanOptions};
use mb_core::progress::{CancellationToken, Level};

use crate::app::{App, Event, Loaded, Purpose};
use crate::sql_highlight::highlight;
use crate::store::HistoryEntry;
use crate::theme;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Diff,
    Changes,
    Script,
}

#[derive(Clone, Debug)]
pub enum StepState {
    Pending,
    Running,
    Done(u128),
    Skipped(String),
    Failed,
}

pub struct DeployDialog {
    pub test: bool,
    pub confirm: String,
    pub running: bool,
    pub single_tx: bool,
    pub plan: Plan,
    pub steps: Vec<StepState>,
    pub report: Option<Result<DeployReport, String>>,
    pub cancel: CancellationToken,
    pub started: Option<Instant>,
}

pub struct DiffRow {
    left: Option<(usize, String)>,
    right: Option<(usize, String)>,
    kind: u8, // 0 equal, 1 only left (source), 2 only right (target), 3 changed
}

#[derive(Default)]
pub struct SchemaView {
    pub loading: bool,
    pub diffs: Vec<ObjectDiff>,
    pub checked: BTreeSet<ObjectKey>,
    pub focus: Option<usize>,
    pub search: String,
    pub show_identical: bool,
    pub kind_filter: Option<ObjectKind>,
    pub tab: Tab,
    pub plan: Plan,
    pub script: String,
    pub plan_dirty: bool,
    pub deploy: Option<DeployDialog>,
    diff_rows: Option<(usize, Vec<DiffRow>)>,
    pub catalogs: Option<(Arc<Catalog>, Arc<Catalog>)>,
    pub target_fingerprint: u64,
    pub elapsed: f64,
    pub compared: bool,
}

enum Item {
    Header(DiffStatus, usize),
    Row(usize),
}

impl SchemaView {
    pub fn on_loaded(&mut self, l: &Loaded) {
        self.loading = false;
        self.compared = true;
        self.diffs = l.diffs.clone();
        // Stable order: status, kind, name.
        self.diffs.sort_by(|a, b| (a.status, a.key.kind, a.key.display()).cmp(&(b.status, b.key.kind, b.key.display())));
        self.checked = self.diffs.iter().filter(|d| d.status != DiffStatus::Identical).map(|d| d.key.clone()).collect();
        self.focus = self.diffs.iter().position(|d| d.status != DiffStatus::Identical);
        self.catalogs = Some((l.source.clone(), l.target.clone()));
        self.target_fingerprint = l.target.fingerprint();
        self.elapsed = l.millis;
        self.plan_dirty = true;
        self.diff_rows = None;
    }

    fn visible(&self) -> Vec<Item> {
        let q = self.search.to_lowercase();
        let mut out = Vec::new();
        for status in [DiffStatus::Added, DiffStatus::Changed, DiffStatus::Removed, DiffStatus::Identical] {
            if status == DiffStatus::Identical && !self.show_identical {
                continue;
            }
            let rows: Vec<usize> = self
                .diffs
                .iter()
                .enumerate()
                .filter(|(_, d)| d.status == status)
                .filter(|(_, d)| self.kind_filter.map(|k| kind_matches(k, d.key.kind)).unwrap_or(true))
                .filter(|(_, d)| q.is_empty() || d.key.display().to_lowercase().contains(&q))
                .map(|(i, _)| i)
                .collect();
            if rows.is_empty() {
                continue;
            }
            out.push(Item::Header(status, rows.len()));
            out.extend(rows.into_iter().map(Item::Row));
        }
        out
    }

    fn build_diff_rows(d: &ObjectDiff) -> Vec<DiffRow> {
        let old: Vec<&str> = d.target_ddl.lines().collect();
        let new: Vec<&str> = d.source_ddl.lines().collect();
        let ops = similar::capture_diff_slices(similar::Algorithm::Myers, &old, &new);
        let mut rows = Vec::new();
        for op in ops {
            let (tag, o, n) = op.as_tag_tuple();
            match tag {
                similar::DiffTag::Equal => {
                    for (i, j) in o.zip(n) {
                        rows.push(DiffRow { left: Some((j + 1, new[j].to_string())), right: Some((i + 1, old[i].to_string())), kind: 0 });
                    }
                }
                similar::DiffTag::Delete => {
                    for i in o {
                        rows.push(DiffRow { left: None, right: Some((i + 1, old[i].to_string())), kind: 2 });
                    }
                }
                similar::DiffTag::Insert => {
                    for j in n {
                        rows.push(DiffRow { left: Some((j + 1, new[j].to_string())), right: None, kind: 1 });
                    }
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

fn kind_matches(filter: ObjectKind, k: ObjectKind) -> bool {
    match filter {
        ObjectKind::View => matches!(k, ObjectKind::View | ObjectKind::MaterializedView),
        ObjectKind::Function => matches!(k, ObjectKind::Function | ObjectKind::Procedure),
        f => f == k,
    }
}

impl App {
    pub fn start_schema_compare(&mut self) {
        if self.schema.loading {
            return;
        }
        self.schema.loading = self.load_catalogs(Purpose::Schema);
    }

    fn plan_options(&self) -> PlanOptions {
        PlanOptions { cascade_drops: self.settings.cascade_drops, concurrent_indexes: self.settings.concurrent_indexes }
    }

    fn refresh_plan(&mut self) {
        if !self.schema.plan_dirty {
            return;
        }
        self.schema.plan_dirty = false;
        if let Some((s, t)) = &self.schema.catalogs {
            let plan = plan_selection(s, t, &self.schema.checked, &self.plan_options());
            let (sl, tl) = self.pair_labels();
            self.schema.script = plan.to_sql(&format!("Migration: {sl} → {tl}"));
            self.schema.plan = plan;
        }
    }

    pub fn ui_schema(&mut self, ui: &mut egui::Ui) {
        self.refresh_plan();

        // Toolbar
        ui.horizontal(|ui| {
            let label = if self.schema.loading { "Comparing…" } else { "⟳ Compare  (F5)" };
            if theme::primary_button(ui, !self.schema.loading, label, false).clicked() {
                self.start_schema_compare();
            }
            if self.schema.loading {
                ui.spinner();
            }
            ui.separator();
            ui.add(egui::TextEdit::singleline(&mut self.schema.search).hint_text("🔍 filter objects").desired_width(180.0));
            egui::ComboBox::from_id_salt("kind_filter")
                .selected_text(self.schema.kind_filter.map(|k| k.label()).unwrap_or("All kinds"))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.schema.kind_filter, None, "All kinds");
                    for k in [
                        ObjectKind::Table,
                        ObjectKind::View,
                        ObjectKind::Function,
                        ObjectKind::Type,
                        ObjectKind::Sequence,
                        ObjectKind::Extension,
                        ObjectKind::Schema,
                    ] {
                        ui.selectable_value(&mut self.schema.kind_filter, Some(k), k.label());
                    }
                });
            ui.checkbox(&mut self.schema.show_identical, "Show identical");
            ui.menu_button("Options ▾", |ui| {
                let mut changed = false;
                changed |= ui.checkbox(&mut self.settings.ignore_comments, "Ignore comments (re-compare)").changed();
                changed |= ui.checkbox(&mut self.settings.cascade_drops, "Use CASCADE on drops").changed();
                changed |= ui
                    .checkbox(&mut self.settings.concurrent_indexes, "Create indexes CONCURRENTLY (outside transaction)")
                    .changed();
                if changed {
                    self.schema.plan_dirty = true;
                    self.settings_dirty = true;
                }
            });
            if self.schema.compared {
                ui.separator();
                let s = mb_core::diff::summarize(&self.schema.diffs);
                theme::badge(ui, &format!("+{}", s.added), theme::GREEN);
                theme::badge(ui, &format!("~{}", s.changed), theme::AMBER);
                theme::badge(ui, &format!("-{}", s.removed), theme::RED);
                ui.label(RichText::new(format!("{} identical · {}", s.identical, theme::fmt_ms(self.schema.elapsed))).weak());
            }
        });
        ui.separator();

        if !self.schema.compared {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.heading("Compare two databases");
                ui.label("Pick a Source and a Target in the bar above, then press Compare (F5).");
                ui.label(RichText::new("Source = desired state · Target = database that will be changed").weak());
            });
            return;
        }

        // Footer first (bottom panel inside the central area).
        egui::Panel::bottom("schema_footer").show(ui, |ui| self.schema_footer(ui));

        egui::Panel::left("schema_list").resizable(true).default_size(380.0).min_size(260.0).show(ui, |ui| {
            self.schema_list(ui);
        });
        egui::CentralPanel::default().show(ui, |ui| self.schema_detail(ui));
    }

    fn schema_list(&mut self, ui: &mut egui::Ui) {
        let items = self.schema.visible();
        ui.horizontal(|ui| {
            if ui.small_button("Select all").clicked() {
                self.schema.checked =
                    self.schema.diffs.iter().filter(|d| d.status != DiffStatus::Identical).map(|d| d.key.clone()).collect();
                self.schema.plan_dirty = true;
            }
            if ui.small_button("None").clicked() {
                self.schema.checked.clear();
                self.schema.plan_dirty = true;
            }
            ui.label(RichText::new(format!("{} selected", self.schema.checked.len())).weak());
        });
        let row_h = 22.0;
        let mut toggle_group: Option<(DiffStatus, bool)> = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, row_h, items.len(), |ui, range| {
            for item in &items[range] {
                match item {
                    Item::Header(status, n) => {
                        let all = self
                            .schema
                            .diffs
                            .iter()
                            .filter(|d| d.status == *status)
                            .all(|d| self.schema.checked.contains(&d.key));
                        ui.horizontal(|ui| {
                            ui.set_height(row_h);
                            if *status != DiffStatus::Identical {
                                let mut v = all;
                                if ui.checkbox(&mut v, "").changed() {
                                    toggle_group = Some((*status, v));
                                }
                            }
                            ui.label(RichText::new(format!("{}  ({n})", status.label())).strong().color(theme::status_color(*status)));
                        });
                    }
                    Item::Row(i) => {
                        let d = &self.schema.diffs[*i];
                        let focused = self.schema.focus == Some(*i);
                        ui.horizontal(|ui| {
                            ui.set_height(row_h);
                            ui.add_space(14.0);
                            if d.status != DiffStatus::Identical {
                                let mut v = self.schema.checked.contains(&d.key);
                                if ui.checkbox(&mut v, "").changed() {
                                    if v {
                                        self.schema.checked.insert(d.key.clone());
                                    } else {
                                        self.schema.checked.remove(&d.key);
                                    }
                                    self.schema.plan_dirty = true;
                                }
                            }
                            ui.label(RichText::new(d.key.kind.short()).monospace().small().color(theme::status_color(d.status)));
                            let r = ui.selectable_label(focused, d.key.display());
                            if r.clicked() {
                                self.schema.focus = Some(*i);
                                self.schema.diff_rows = None;
                                if self.schema.tab == Tab::Script {
                                    self.schema.tab = Tab::Diff;
                                }
                            }
                            if let Some(first) = d.details.first() {
                                r.on_hover_text(d.details.join("\n"));
                                let _ = first;
                            }
                        });
                    }
                }
            }
        });
        if let Some((status, v)) = toggle_group {
            for d in self.schema.diffs.iter().filter(|d| d.status == status) {
                if v {
                    self.schema.checked.insert(d.key.clone());
                } else {
                    self.schema.checked.remove(&d.key);
                }
            }
            self.schema.plan_dirty = true;
        }
    }

    fn schema_detail(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.schema.tab, Tab::Diff, "Side-by-side");
            ui.selectable_value(&mut self.schema.tab, Tab::Changes, "Changes");
            ui.selectable_value(&mut self.schema.tab, Tab::Script, format!("Script ({} steps)", self.schema.plan.steps.len()));
        });
        ui.separator();
        match self.schema.tab {
            Tab::Script => self.script_view(ui),
            Tab::Changes => {
                let Some(d) = self.schema.focus.and_then(|i| self.schema.diffs.get(i)) else {
                    ui.label("Select an object on the left.");
                    return;
                };
                ui.heading(format!("{} {}", d.key.kind.label(), d.key.display()));
                theme::badge(ui, d.status.label(), theme::status_color(d.status));
                ui.add_space(6.0);
                for det in &d.details {
                    ui.label(format!("• {det}"));
                }
            }
            Tab::Diff => self.side_by_side(ui),
        }
    }

    fn side_by_side(&mut self, ui: &mut egui::Ui) {
        let Some(i) = self.schema.focus else {
            ui.label("Select an object on the left to see its definition in both databases.");
            return;
        };
        if self.schema.diff_rows.as_ref().map(|(k, _)| *k != i).unwrap_or(true) {
            let rows = SchemaView::build_diff_rows(&self.schema.diffs[i]);
            self.schema.diff_rows = Some((i, rows));
        }
        let d = &self.schema.diffs[i];
        let (sl, tl) = self.pair_labels();
        ui.horizontal(|ui| {
            ui.strong(d.key.display());
            theme::badge(ui, d.status.label(), theme::status_color(d.status));
        });
        let half = (ui.available_width() - 16.0) / 2.0;
        ui.horizontal(|ui| {
            ui.add_sized([half, 18.0], egui::Label::new(RichText::new(format!("Source · {sl}")).weak()));
            ui.add_sized([half, 18.0], egui::Label::new(RichText::new(format!("Target · {tl}")).weak()));
        });
        let rows = &self.schema.diff_rows.as_ref().unwrap().1;
        let dark = ui.visuals().dark_mode;
        let font = egui::FontId::monospace(12.5);
        let row_h = ui.fonts_mut(|f| f.row_height(&font)) + 2.0;
        let text_color = ui.visuals().text_color();
        let weak = ui.visuals().weak_text_color();
        egui::ScrollArea::both().auto_shrink([false, false]).id_salt("sbs").show_rows(ui, row_h, rows.len(), |ui, range| {
            for r in &rows[range] {
                let (lc, rc) = match r.kind {
                    1 => (Some(theme::GREEN), None),
                    2 => (None, Some(theme::RED)),
                    3 => (Some(theme::AMBER), Some(theme::AMBER)),
                    _ => (None, None),
                };
                ui.horizontal(|ui| {
                    for (cell, color) in [(&r.left, lc), (&r.right, rc)] {
                        let (rect, _) = ui.allocate_exact_size(egui::vec2(half, row_h), egui::Sense::hover());
                        if let Some(c) = color {
                            ui.painter().rect_filled(rect, 0.0, theme::tint(c, dark));
                        }
                        if let Some((n, text)) = cell {
                            ui.painter().text(
                                rect.left_center() + egui::vec2(4.0, 0.0),
                                egui::Align2::LEFT_CENTER,
                                format!("{n:>4}"),
                                font.clone(),
                                weak,
                            );
                            let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
                            painter.text(
                                rect.left_center() + egui::vec2(44.0, 0.0),
                                egui::Align2::LEFT_CENTER,
                                text,
                                font.clone(),
                                text_color,
                            );
                        }
                    }
                });
            }
        });
    }

    fn script_view(&mut self, ui: &mut egui::Ui) {
        let counts = self.schema.plan.hazard_counts();
        if !counts.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.label("Hazards:");
                for (h, n) in &counts {
                    theme::badge(ui, &format!("{} ×{n}", h.label()), theme::hazard_color(*h)).on_hover_text(h.explain());
                }
            });
        }
        let dark = ui.visuals().dark_mode;
        let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, wrap: f32| {
            let mut job = highlight(buf.as_str(), dark, egui::FontId::monospace(12.5));
            job.wrap.max_width = wrap;
            ui.fonts_mut(|f| f.layout_job(job))
        };
        egui::ScrollArea::both().auto_shrink([false, false]).id_salt("script").show(ui, |ui| {
            let mut text = self.schema.script.as_str();
            ui.add(
                egui::TextEdit::multiline(&mut text)
                    .code_editor()
                    .desired_width(f32::INFINITY)
                    .layouter(&mut layouter),
            );
        });
    }

    fn schema_footer(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let plan = &self.schema.plan;
            let n = plan.steps.len();
            ui.label(format!("{} object(s) selected · {n} statement(s)", self.schema.checked.len()));
            let dl = plan.steps.iter().filter(|s| s.hazards.contains(&Hazard::DataLoss)).count();
            if dl > 0 {
                theme::badge(ui, &format!("{dl} data-loss"), theme::RED);
            }
            let manual = plan.steps.iter().filter(|s| s.is_manual()).count();
            if manual > 0 {
                theme::badge(ui, &format!("{manual} manual"), theme::RED);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let has = n > 0;
                if theme::primary_button(ui, has, "Deploy…", dl > 0).clicked() {
                    self.open_deploy(false);
                }
                if ui.add_enabled(has, egui::Button::new("Test deploy")).on_hover_text("Run inside a transaction and roll back").clicked() {
                    self.open_deploy(true);
                }
                if ui.add_enabled(has, egui::Button::new("Save as migration…")).clicked() {
                    let name = format!("{}_migrate_baba.sql", chrono::Local::now().format("%Y%m%d%H%M%S"));
                    self.save_script(&name);
                }
                if ui.add_enabled(has, egui::Button::new("Save .sql…")).clicked() {
                    self.save_script("migration.sql");
                }
                if ui.add_enabled(has, egui::Button::new("Copy SQL")).clicked() {
                    ui.ctx().copy_text(self.schema.script.clone());
                    self.notify("Script copied to clipboard");
                }
            });
        });
        ui.add_space(2.0);
    }

    fn save_script(&mut self, name: &str) {
        if let Some(path) = rfd::FileDialog::new().set_file_name(name).add_filter("SQL", &["sql"]).save_file() {
            match std::fs::write(&path, &self.schema.script) {
                Ok(()) => self.notify(format!("Saved {}", path.display())),
                Err(e) => self.log(Level::Error, format!("Could not save: {e}")),
            }
        }
    }

    fn open_deploy(&mut self, test: bool) {
        let plan = self.schema.plan.clone();
        self.schema.deploy = Some(DeployDialog {
            test,
            confirm: String::new(),
            running: false,
            single_tx: true,
            steps: vec![StepState::Pending; plan.steps.len()],
            plan,
            report: None,
            cancel: CancellationToken::new(),
            started: None,
        });
    }

    fn run_deploy(&mut self) {
        let Some(target) = self.params(&self.target_id.clone()) else { return };
        let filter = self.schema_filter();
        let ignore_comments = self.settings.ignore_comments;
        let expected = self.schema.target_fingerprint;
        let lock = self.settings.lock_timeout_secs;
        let Some(dlg) = self.schema.deploy.as_mut() else { return };
        dlg.running = true;
        dlg.started = Some(Instant::now());
        dlg.report = None;
        dlg.steps = vec![StepState::Pending; dlg.plan.steps.len()];
        let plan = dlg.plan.clone();
        let opts = DeployOptions { test_only: dlg.test, single_transaction: dlg.single_tx, lock_timeout_secs: lock };
        let cancel = dlg.cancel.clone();
        let test = dlg.test;
        let bus = self.bus.clone();
        self.spawn(async move {
            // Stale-compare guard: the target must still look like it did at compare time.
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
                    bus.send(Event::DeployDone {
                        test,
                        result: Err("The target schema changed since the last compare. Re-run Compare before deploying.".into()),
                    });
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

    pub fn on_deploy_progress(&mut self, p: DeployProgress) {
        let Some(d) = self.schema.deploy.as_mut() else { return };
        match p {
            DeployProgress::Started { index, .. } => d.steps[index] = StepState::Running,
            DeployProgress::Finished { index, millis } => d.steps[index] = StepState::Done(millis),
            DeployProgress::Skipped { index, reason } => d.steps[index] = StepState::Skipped(reason),
        }
    }

    pub fn on_deploy_done(&mut self, test: bool, result: Result<DeployReport, String>) {
        let (sl, tl) = self.pair_labels();
        let script = self.schema.script.clone();
        let mut ok = false;
        let summary;
        if let Some(d) = self.schema.deploy.as_mut() {
            d.running = false;
            if let Ok(r) = &result {
                if let Some(i) = r.failed_step {
                    d.steps[i] = StepState::Failed;
                }
            }
            d.report = Some(result.clone());
        }
        match &result {
            Ok(r) if r.ok() => {
                ok = true;
                summary = format!(
                    "{} {} statement(s) in {}{}",
                    if test { "Validated" } else { "Executed" },
                    r.executed,
                    theme::fmt_ms(r.millis as f64),
                    if test { " — rolled back" } else { "" }
                );
                self.notify(summary.clone());
            }
            Ok(r) => {
                summary = format!("Failed at step {}: {}", r.failed_step.map(|i| i + 1).unwrap_or(0), r.error.clone().unwrap_or_default());
                self.log(Level::Error, summary.clone());
            }
            Err(e) => {
                summary = e.clone();
                self.log(Level::Error, format!("Deploy failed: {e}"));
            }
        }
        if !test {
            self.add_history(HistoryEntry {
                id: String::new(),
                kind: "Schema deploy".into(),
                when: String::new(),
                source: sl,
                target: tl,
                ok,
                millis: result.as_ref().map(|r| r.millis).unwrap_or(0),
                summary,
                script,
                log: Vec::new(),
            });
            if ok {
                // Re-compare so the user sees what remains (should be nothing).
                self.start_schema_compare();
            }
        }
    }

    pub fn schema_dialogs(&mut self, ctx: &egui::Context) {
        let Some(_) = self.schema.deploy.as_ref() else { return };
        let target = self.profile(&self.target_id).cloned();
        let target_label = target.as_ref().map(|p| p.label()).unwrap_or_default();
        let dbname = target.as_ref().map(|p| p.params.database.clone()).unwrap_or_default();
        let production = target.as_ref().map(|p| p.production).unwrap_or(false);
        let mut close = false;
        let mut run = false;
        let modal = egui::Modal::new(egui::Id::unique("deploy_modal")).show(ctx, |ui| {
            let d = self.schema.deploy.as_mut().unwrap();
            ui.set_width(720.0);
            ui.heading(if d.test { "Test deploy (rolled back)" } else { "Deploy to target" });
            ui.horizontal(|ui| {
                ui.label("Target:");
                ui.strong(&target_label);
                if production {
                    theme::badge(ui, "PRODUCTION", theme::RED);
                }
            });
            ui.add_space(4.0);
            let counts = d.plan.hazard_counts();
            ui.label(format!("{} statement(s)", d.plan.steps.len()));
            for (h, n) in &counts {
                ui.horizontal(|ui| {
                    theme::badge(ui, h.label(), theme::hazard_color(*h));
                    ui.label(format!("×{n} — {}", h.explain()));
                });
            }
            ui.add_space(6.0);
            egui::ScrollArea::vertical().max_height(300.0).auto_shrink([false, true]).show(ui, |ui| {
                for (i, s) in d.plan.steps.iter().enumerate() {
                    let (icon, color) = match &d.steps[i] {
                        StepState::Pending => ("○", theme::GREY),
                        StepState::Running => ("◌", theme::ACCENT),
                        StepState::Done(_) => ("✔", theme::GREEN),
                        StepState::Skipped(_) => ("↷", theme::AMBER),
                        StepState::Failed => ("✖", theme::RED),
                    };
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(icon).color(color));
                        let first = s.sql.lines().next().unwrap_or_default();
                        let mut t = RichText::new(format!("{:>3}. {}", i + 1, first)).monospace().small();
                        if s.hazards.contains(&Hazard::DataLoss) {
                            t = t.color(theme::RED);
                        }
                        ui.label(t).on_hover_text(&s.sql);
                        if let StepState::Done(ms) = d.steps[i] {
                            ui.label(RichText::new(format!("{ms} ms")).weak().small());
                        }
                        if let StepState::Skipped(r) = &d.steps[i] {
                            ui.label(RichText::new(r).weak().small());
                        }
                    });
                }
            });
            ui.separator();
            let needs_confirm = !d.test && (d.plan.has_data_loss() || production);
            if !d.test {
                ui.checkbox(&mut d.single_tx, "Run in a single transaction (all-or-nothing)");
            }
            if needs_confirm && d.report.is_none() {
                ui.label(
                    RichText::new(format!("This deploy can destroy data. Type the database name `{dbname}` to confirm:"))
                        .color(theme::RED),
                );
                ui.add(egui::TextEdit::singleline(&mut d.confirm).desired_width(240.0));
            }
            match &d.report {
                Some(Ok(r)) if r.ok() => {
                    ui.label(
                        RichText::new(if d.test {
                            format!("✔ All {} statement(s) succeeded and were rolled back.", r.executed)
                        } else {
                            format!("✔ Deployed {} statement(s) in {}.", r.executed, theme::fmt_ms(r.millis as f64))
                        })
                        .color(theme::GREEN)
                        .strong(),
                    );
                }
                Some(Ok(r)) => {
                    ui.label(
                        RichText::new(format!(
                            "✖ Failed at statement {}{}",
                            r.failed_step.map(|i| i + 1).unwrap_or(0),
                            if r.rolled_back { " — everything was rolled back." } else { "." }
                        ))
                        .color(theme::RED)
                        .strong(),
                    );
                    egui::ScrollArea::vertical().max_height(140.0).id_salt("err").show(ui, |ui| {
                        ui.label(RichText::new(r.error.clone().unwrap_or_default()).monospace().small());
                    });
                }
                Some(Err(e)) => {
                    ui.label(RichText::new(format!("✖ {e}")).color(theme::RED));
                }
                None => {}
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if d.running {
                    ui.spinner();
                    ui.label("Running…");
                    if ui.button("Cancel").clicked() {
                        d.cancel.cancel();
                    }
                } else {
                    let confirmed = !needs_confirm || d.confirm.trim() == dbname;
                    let label = if d.test { "Run test" } else { "Deploy now" };
                    let done = matches!(&d.report, Some(Ok(r)) if r.ok());
                    if !done && theme::primary_button(ui, confirmed, label, needs_confirm).clicked() {
                        d.cancel = CancellationToken::new();
                        run = true;
                    }
                    if ui.button("Close").clicked() {
                        close = true;
                    }
                }
            });
        });
        if modal.should_close() && !self.schema.deploy.as_ref().map(|d| d.running).unwrap_or(false) {
            close = true;
        }
        if run {
            self.run_deploy();
        }
        if close {
            self.schema.deploy = None;
        }
        let _ = Color32::WHITE;
    }
}
