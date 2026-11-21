//! App shell: state, event bus between the tokio runtime and the UI thread, navigation chrome.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::Instant;

use eframe::egui::{self, Color32, Key, RichText};
use mb_core::conn::{connect, test_connection, ConnParams, ServerInfo, SessionOptions, Side};
use mb_core::data::compare::TableCompare;
use mb_core::data::copy::CopySummary;
use mb_core::deploy::{DeployProgress, DeployReport};
use mb_core::diff::ObjectDiff;
use mb_core::introspect::{introspect, SchemaFilter};
use mb_core::model::{Catalog, QName};
use mb_core::progress::{Level, LogFn};

use crate::store::{HistoryEntry, Profile, Settings, Store};
use crate::theme;
use crate::views::{
    connections::ConnectionsView, data_compare::DataView, history::HistoryView, migrate::MigrateView,
    schema::SchemaView,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    Connections,
    Schema,
    Data,
    Migrate,
    History,
    Settings,
}

impl Screen {
    pub const ALL: [Screen; 6] =
        [Screen::Connections, Screen::Schema, Screen::Data, Screen::Migrate, Screen::History, Screen::Settings];

    pub fn label(self) -> &'static str {
        match self {
            Screen::Connections => "Connections",
            Screen::Schema => "Schema Compare",
            Screen::Data => "Data Compare",
            Screen::Migrate => "Migrate Data",
            Screen::History => "History",
            Screen::Settings => "Settings",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Screen::Connections => "🔗",
            Screen::Schema => "⇆",
            Screen::Data => "≋",
            Screen::Migrate => "➡",
            Screen::History => "⟲",
            Screen::Settings => "⚙",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    Schema,
    Data,
    Migrate,
}

/// Messages from background tasks to the UI thread.
pub enum Event {
    Log(Level, String),
    TestResult { profile_id: String, result: Result<ServerInfo, String> },
    Catalogs { purpose: Purpose, result: Result<Loaded, String> },
    Deploy(DeployProgress),
    DeployDone { test: bool, result: Result<DeployReport, String> },
    DataStarted(QName),
    DataResult(TableCompare),
    DataDone(Result<(), String>),
    CopyPrepared(Result<crate::views::migrate::Prepared, String>),
    CopyDone(Result<CopySummary, String>),
    SyncDone(Result<(), String>),
}

pub struct Loaded {
    pub source: Arc<Catalog>,
    pub target: Arc<Catalog>,
    pub diffs: Vec<ObjectDiff>,
    pub millis: f64,
    pub src_id: String,
    pub tgt_id: String,
}

/// Cheap, cloneable handle used by tasks to talk to the UI.
#[derive(Clone)]
pub struct Bus {
    tx: Sender<Event>,
    ctx: egui::Context,
}

impl Bus {
    pub fn send(&self, e: Event) {
        let _ = self.tx.send(e);
        self.ctx.request_repaint();
    }

    pub fn log_fn(&self) -> LogFn {
        let b = self.clone();
        Arc::new(move |lvl, msg| b.send(Event::Log(lvl, msg)))
    }
}

pub struct LogLine {
    pub at: chrono::DateTime<chrono::Local>,
    pub level: Level,
    pub text: String,
}

pub struct App {
    rt: tokio::runtime::Runtime,
    rx: Receiver<Event>,
    pub bus: Bus,
    pub store: Store,
    pub profiles: Vec<Profile>,
    pub settings: Settings,
    pub settings_dirty: bool,
    pub history: Vec<HistoryEntry>,
    pub screen: Screen,
    pub logs: VecDeque<LogLine>,
    pub show_log: bool,
    pub notice: Option<(Level, String, Instant)>,
    pub source_id: Option<String>,
    pub target_id: Option<String>,
    pub catalogs: Option<Loaded>,
    pub conn_view: ConnectionsView,
    pub schema: SchemaView,
    pub data: DataView,
    pub migrate: MigrateView,
    pub history_view: HistoryView,
    started: Instant,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 8))
            .thread_name("mb-worker")
            .enable_all()
            .build()
            .expect("tokio runtime");
        let (tx, rx) = channel();
        let store = Store::open();
        let profiles = store.load_profiles();
        let settings = store.load_settings();
        theme::apply(&cc.egui_ctx, &settings);
        let history = store.load_history();
        let pick = |id: &Option<String>| id.clone().filter(|i| profiles.iter().any(|p| &p.id == i));
        let source_id = pick(&settings.last_source);
        let target_id = pick(&settings.last_target);
        let screen = if profiles.is_empty() { Screen::Connections } else { Screen::Schema };
        let mut app = Self {
            rt,
            rx,
            bus: Bus { tx, ctx: cc.egui_ctx.clone() },
            store,
            conn_view: ConnectionsView::new(&profiles),
            profiles,
            schema: SchemaView::default(),
            data: DataView::default(),
            migrate: MigrateView::new(&settings),
            history_view: HistoryView::default(),
            settings,
            settings_dirty: false,
            history,
            screen,
            logs: VecDeque::new(),
            show_log: false,
            notice: None,
            source_id,
            target_id,
            catalogs: None,
            started: Instant::now(),
        };
        app.log(Level::Info, "Migrate Baba ready".into());
        app
    }

    // ---- infrastructure -------------------------------------------------------------------

    pub fn spawn(&self, fut: impl Future<Output = ()> + Send + 'static) {
        self.rt.spawn(fut);
    }

    pub fn log(&mut self, level: Level, text: String) {
        if level >= Level::Warn {
            self.notice = Some((level, text.lines().next().unwrap_or_default().to_string(), Instant::now()));
        }
        self.logs.push_back(LogLine { at: chrono::Local::now(), level, text });
        while self.logs.len() > 5000 {
            self.logs.pop_front();
        }
    }

    pub fn notify(&mut self, text: impl Into<String>) {
        let t = text.into();
        self.log(Level::Info, t.clone());
        self.notice = Some((Level::Info, t, Instant::now()));
    }

    pub fn profile(&self, id: &Option<String>) -> Option<&Profile> {
        id.as_ref().and_then(|i| self.profiles.iter().find(|p| &p.id == i))
    }

    /// Connection parameters with the password loaded from the keychain.
    pub fn params(&mut self, id: &Option<String>) -> Option<ConnParams> {
        let id = id.clone()?;
        let idx = self.profiles.iter().position(|p| p.id == id)?;
        if !self.profiles[idx].password_loaded {
            if let Some(pw) = self.store.get_password(&id) {
                self.profiles[idx].params.password = pw;
            }
            self.profiles[idx].password_loaded = true;
        }
        Some(self.profiles[idx].params.clone())
    }

    pub fn save_profiles(&mut self) {
        if let Err(e) = self.store.save_profiles(&self.profiles) {
            self.log(Level::Error, format!("could not save profiles: {e}"));
        }
    }

    pub fn save_settings(&mut self) {
        self.settings.last_source = self.source_id.clone();
        self.settings.last_target = self.target_id.clone();
        if let Err(e) = self.store.save_settings(&self.settings) {
            self.log(Level::Error, format!("could not save settings: {e}"));
        }
        self.settings_dirty = false;
    }

    pub fn add_history(&mut self, mut e: HistoryEntry) {
        e.id = chrono::Local::now().format("%Y%m%d%H%M%S%3f").to_string();
        e.when = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        self.store.add_history(&e);
        self.history.insert(0, e);
    }

    pub fn pair_labels(&self) -> (String, String) {
        (
            self.profile(&self.source_id).map(|p| p.label()).unwrap_or_default(),
            self.profile(&self.target_id).map(|p| p.label()).unwrap_or_default(),
        )
    }

    pub fn schema_filter(&self) -> SchemaFilter {
        let include = self
            .settings
            .schema_include
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let mut exclude: Vec<String> = Vec::new();
        for p in [self.profile(&self.source_id), self.profile(&self.target_id)].into_iter().flatten() {
            for s in &p.excluded_schemas {
                if !exclude.contains(s) {
                    exclude.push(s.clone());
                }
            }
        }
        SchemaFilter { include, exclude }
    }

    /// Introspect source and target concurrently.
    pub fn load_catalogs(&mut self, purpose: Purpose) -> bool {
        let (Some(src), Some(tgt)) = (self.params(&self.source_id.clone()), self.params(&self.target_id.clone())) else {
            self.log(Level::Warn, "Pick a source and a target connection first".into());
            return false;
        };
        if self.source_id == self.target_id {
            self.log(Level::Warn, "Source and target are the same connection".into());
        }
        let filter = self.schema_filter();
        let ignore_comments = self.settings.ignore_comments;
        let bus = self.bus.clone();
        let (src_id, tgt_id) = (self.source_id.clone().unwrap_or_default(), self.target_id.clone().unwrap_or_default());
        self.log(Level::Info, format!("Reading schemas of {} and {}…", src.display(), tgt.display()));
        self.spawn(async move {
            let t0 = Instant::now();
            let side = |p: ConnParams, side: Side, f: SchemaFilter| async move {
                let c = connect(&p, &SessionOptions::introspection(side)).await?;
                introspect(&c, &f).await
            };
            let r = tokio::try_join!(side(src, Side::Source, filter.clone()), side(tgt, Side::Target, filter));
            let result = r.map_err(|e| e.to_string()).map(|(mut s, mut t)| {
                if ignore_comments {
                    s.strip_comments();
                    t.strip_comments();
                }
                let diffs = mb_core::diff::compare(&s, &t);
                Loaded {
                    source: Arc::new(s),
                    target: Arc::new(t),
                    diffs,
                    millis: t0.elapsed().as_secs_f64() * 1000.0,
                    src_id,
                    tgt_id,
                }
            });
            bus.send(Event::Catalogs { purpose, result });
        });
        true
    }

    pub fn test_profile(&mut self, params: ConnParams, profile_id: String) {
        let bus = self.bus.clone();
        self.spawn(async move {
            let r = test_connection(&params).await.map_err(|e| e.to_string());
            bus.send(Event::TestResult { profile_id, result: r });
        });
    }

    fn drain_events(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                Event::Log(l, m) => self.log(l, m),
                Event::TestResult { profile_id, result } => self.on_test_result(profile_id, result),
                Event::Catalogs { purpose, result } => self.on_catalogs(purpose, result),
                Event::Deploy(p) => self.on_deploy_progress(p),
                Event::DeployDone { test, result } => self.on_deploy_done(test, result),
                Event::DataStarted(t) => self.on_data_started(t),
                Event::DataResult(r) => self.on_data_result(r),
                Event::DataDone(r) => self.on_data_done(r),
                Event::CopyPrepared(r) => self.on_copy_prepared(r),
                Event::CopyDone(r) => self.on_copy_done(r),
                Event::SyncDone(r) => self.on_sync_done(r),
            }
        }
    }

    fn on_catalogs(&mut self, purpose: Purpose, result: Result<Loaded, String>) {
        match result {
            Ok(l) if Some(&l.src_id) != self.source_id.as_ref() || Some(&l.tgt_id) != self.target_id.as_ref() => {
                // Source/target were switched while reading: drop the stale result.
                self.schema.loading = false;
                self.data.loading = false;
                self.migrate.loading = false;
                self.log(Level::Warn, "Connections changed while reading schemas; result discarded".into());
            }
            Ok(l) => {
                self.log(
                    Level::Info,
                    format!(
                        "Read {} source objects and {} target objects in {}",
                        l.source.object_count(),
                        l.target.object_count(),
                        theme::fmt_ms(l.millis)
                    ),
                );
                match purpose {
                    Purpose::Schema => self.schema.on_loaded(&l),
                    Purpose::Data => self.data.on_loaded(&l),
                    Purpose::Migrate => self.migrate.on_loaded(&l),
                }
                // Keep other screens' table lists fresh too.
                if purpose != Purpose::Data && self.data.rows.is_empty() {
                    self.data.on_loaded(&l);
                }
                if purpose != Purpose::Migrate && self.migrate.rows.is_empty() {
                    self.migrate.on_loaded(&l);
                }
                self.catalogs = Some(l);
            }
            Err(e) => {
                self.log(Level::Error, format!("Reading schemas failed: {e}"));
                self.schema.loading = false;
                self.data.loading = false;
                self.migrate.loading = false;
            }
        }
    }

    // ---- chrome ---------------------------------------------------------------------------

    fn connection_picker(&mut self, ui: &mut egui::Ui, label: &str, source: bool) {
        let current = if source { self.source_id.clone() } else { self.target_id.clone() };
        let p = self.profile(&current).cloned();
        let text = p.as_ref().map(|p| p.label()).unwrap_or_else(|| "choose…".into());
        let color = p.as_ref().map(|p| Color32::from_rgb(p.color[0], p.color[1], p.color[2])).unwrap_or(theme::GREY);
        ui.label(RichText::new(label).weak());
        ui.label(RichText::new("●").color(color));
        let mut chosen = current.clone();
        egui::ComboBox::from_id_salt(label)
            .selected_text(if p.as_ref().map(|p| p.production).unwrap_or(false) && !source {
                RichText::new(format!("{text}  [PROD]")).color(theme::RED)
            } else {
                RichText::new(text)
            })
            .width(220.0)
            .show_ui(ui, |ui| {
                for p in &self.profiles {
                    let c = Color32::from_rgb(p.color[0], p.color[1], p.color[2]);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("●").color(c));
                        ui.selectable_value(&mut chosen, Some(p.id.clone()), p.label());
                    });
                }
                if self.profiles.is_empty() {
                    ui.label("No connections yet — add one in Connections.");
                }
            });
        if chosen != current {
            if source {
                self.source_id = chosen;
            } else {
                self.target_id = chosen;
            }
            self.catalogs = None;
            self.save_settings();
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("◆ Migrate Baba").strong().size(15.0).color(theme::ACCENT));
            ui.separator();
            self.connection_picker(ui, "Source", true);
            if ui.button("⇄").on_hover_text("Swap source and target").clicked() {
                std::mem::swap(&mut self.source_id, &mut self.target_id);
                self.catalogs = None;
                self.save_settings();
            }
            self.connection_picker(ui, "Target", false);
            ui.separator();
            ui.label(RichText::new("Schemas").weak());
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.settings.schema_include)
                    .hint_text("all (e.g. public, app)")
                    .desired_width(160.0),
            );
            if r.lost_focus() {
                self.save_settings();
            }
        });
    }

    fn nav(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        for s in Screen::ALL {
            let selected = self.screen == s;
            let text = RichText::new(format!("{}  {}", s.icon(), s.label())).size(13.5);
            let r = ui.add_sized([ui.available_width(), 30.0], egui::Button::selectable(selected, text));
            if r.clicked() {
                self.screen = s;
            }
        }
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            ui.add_space(6.0);
            ui.label(RichText::new(format!("v{}", env!("CARGO_PKG_VERSION"))).weak().small());
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let busy = self.schema.loading
                || self.schema.deploy.as_ref().map(|d| d.running).unwrap_or(false)
                || self.data.loading
                || self.data.running
                || self.migrate.running
                || self.migrate.loading
                || self.conn_view.testing;
            if busy {
                ui.spinner();
                ui.label("working…");
            } else {
                ui.label(RichText::new("● idle").color(theme::GREEN));
            }
            if let Some(l) = &self.catalogs {
                ui.separator();
                ui.label(RichText::new(format!("last read {}", theme::fmt_ms(l.millis))).weak());
            }
            if let Some((lvl, msg, at)) = &self.notice {
                if at.elapsed().as_secs() < 8 {
                    ui.separator();
                    let c = match lvl {
                        Level::Error => theme::RED,
                        Level::Warn => theme::AMBER,
                        _ => ui.visuals().text_color(),
                    };
                    ui.label(RichText::new(msg).color(c));
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let errs = self.logs.iter().filter(|l| l.level == Level::Error).count();
                let label = if errs > 0 { format!("Log ({errs} errors) ▴") } else { "Log ▴".to_string() };
                if ui.selectable_label(self.show_log, label).clicked() {
                    self.show_log = !self.show_log;
                }
            });
        });
    }

    fn log_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.strong("Log");
            if ui.small_button("Copy").clicked() {
                let text: String = self
                    .logs
                    .iter()
                    .map(|l| format!("{} {:?} {}\n", l.at.format("%H:%M:%S"), l.level, l.text))
                    .collect();
                ui.ctx().copy_text(text);
            }
            if ui.small_button("Clear").clicked() {
                self.logs.clear();
            }
        });
        let row_h = ui.text_style_height(&egui::TextStyle::Monospace) + 2.0;
        egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show_rows(
            ui,
            row_h,
            self.logs.len(),
            |ui, range| {
                for l in self.logs.range(range) {
                    let c = match l.level {
                        Level::Error => theme::RED,
                        Level::Warn => theme::AMBER,
                        Level::Debug => theme::GREY,
                        Level::Info => ui.visuals().text_color(),
                    };
                    ui.label(
                        RichText::new(format!("{}  {}", l.at.format("%H:%M:%S"), l.text.replace('\n', " ⏎ ")))
                            .monospace()
                            .color(c),
                    );
                }
            },
        );
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let (f5, nums) = ctx.input(|i| {
            let ctrl = i.modifiers.command;
            let nums: Vec<usize> = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6]
                .iter()
                .enumerate()
                .filter(|(_, k)| ctrl && i.key_pressed(**k))
                .map(|(n, _)| n)
                .collect();
            (i.key_pressed(Key::F5), nums)
        });
        if let Some(&n) = nums.first() {
            self.screen = Screen::ALL[n];
        }
        if f5 {
            match self.screen {
                Screen::Schema => self.start_schema_compare(),
                Screen::Data => {
                    self.data.loading = self.load_catalogs(Purpose::Data);
                }
                Screen::Migrate => {
                    self.migrate.loading = self.load_catalogs(Purpose::Migrate);
                }
                _ => {}
            }
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();
        self.shortcuts(ctx);
        // While data jobs run, repaint periodically to animate progress (atomics, no events).
        if self.migrate.running || self.data.syncing {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        if self.settings_dirty {
            theme::apply(ctx, &self.settings);
            self.save_settings();
        }
        let _ = self.started;
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("top_bar")
            .frame(egui::Frame::side_top_panel(ui.style()).inner_margin(egui::Margin::symmetric(10, 8)))
            .show(ui, |ui| self.top_bar(ui));
        egui::Panel::bottom("status_bar")
            .frame(egui::Frame::side_top_panel(ui.style()).inner_margin(egui::Margin::symmetric(10, 4)))
            .show(ui, |ui| self.status_bar(ui));
        if self.show_log {
            egui::Panel::bottom("log_panel")
                .resizable(true)
                .default_size(180.0)
                .min_size(80.0)
                .show(ui, |ui| self.log_panel(ui));
        }
        egui::Panel::left("nav")
            .resizable(false)
            .exact_size(170.0)
            .show(ui, |ui| self.nav(ui));
        egui::CentralPanel::default().show(ui, |ui| match self.screen {
            Screen::Connections => self.ui_connections(ui),
            Screen::Schema => self.ui_schema(ui),
            Screen::Data => self.ui_data(ui),
            Screen::Migrate => self.ui_migrate(ui),
            Screen::History => self.ui_history(ui),
            Screen::Settings => self.ui_settings(ui),
        });
        self.schema_dialogs(ui.ctx());
        self.migrate_dialogs(ui.ctx());
        self.data_dialogs(ui.ctx());
    }
}
