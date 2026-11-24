//! App shell: state, the event bus between async tasks and the UI thread, background-job polling,
//! step gating (Connections → Doctor → Compare / Migration), sidebar and top bar.

use std::future::Future;
use std::hash::{DefaultHasher, Hasher};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use eframe::egui::{self, Align, Color32, CornerRadius, Key, Layout, Margin, RichText, Stroke};
use mb_core::conn::{connect, ConnParams, SessionOptions, Side};
use mb_core::data::compare::TableCompare;
use mb_core::deploy::{DeployProgress, DeployReport};
use mb_core::diff::ObjectDiff;
use mb_core::doctor::Finding;
use mb_core::introspect::{introspect, SchemaFilter};
use mb_core::model::{Catalog, QName};
use mb_core::progress::Level;

use crate::jobs;
use crate::store::{now_ms, Job, PauseReason, Profile, RunState, RunStatus, Settings, Store, ThemeChoice};
use crate::theme::{self, StepState};
use crate::views::{compare::CompareView, connections::ConnectionsView, doctor::DoctorView, migration::MigrationView};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    Connections,
    Doctor,
    Compare,
    Migration,
}

impl Screen {
    pub const ALL: [Screen; 4] = [Screen::Connections, Screen::Doctor, Screen::Compare, Screen::Migration];

    pub fn label(self) -> &'static str {
        match self {
            Screen::Connections => "Connections",
            Screen::Doctor => "Doctor",
            Screen::Compare => "Compare",
            Screen::Migration => "Migration",
        }
    }
}

/// Messages from async tasks to the UI thread.
pub enum Event {
    Notice(Level, String),
    Tested { profile_id: String, result: Result<String, String> },
    Doctor { job_id: String, sig: u64, findings: Vec<Finding> },
    Catalogs { job_id: String, sig: u64, result: Result<Loaded, String> },
    Deploy(DeployProgress),
    DeployDone { test: bool, result: Result<DeployReport, String> },
    DataStarted(QName),
    DataResult(TableCompare),
    DataDone(Result<(), String>),
}

pub struct Loaded {
    pub source: Arc<Catalog>,
    pub target: Arc<Catalog>,
    pub diffs: Vec<ObjectDiff>,
    pub millis: f64,
    pub job_id: String,
    pub sig: u64,
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
}

/// A job plus what the app knows about its background run.
pub struct JobEntry {
    pub job: Job,
    pub run: Option<RunState>,
    /// Set between spawning a worker and its first heartbeat.
    pub starting: Option<(Instant, i64)>,
    /// Pause/cancel requested, waiting for the worker to stop.
    pub stopping: bool,
    run_mtime: Option<SystemTime>,
    last_poll: Option<Instant>,
}

impl JobEntry {
    fn new(job: Job, store: &Store) -> Self {
        let path = store.run_path(&job.id);
        Self {
            run: store.load_run(&job.id),
            run_mtime: std::fs::metadata(path).and_then(|m| m.modified()).ok(),
            job,
            starting: None,
            stopping: false,
            last_poll: None,
        }
    }

    pub fn running(&self) -> bool {
        self.starting.is_some() || self.run.as_ref().map(|r| r.status == RunStatus::Running).unwrap_or(false)
    }

    pub fn status_color(&self, p: &theme::Pal) -> Color32 {
        match &self.run {
            _ if self.starting.is_some() => p.accent,
            Some(r) => match r.status {
                RunStatus::Running => p.accent,
                RunStatus::Paused if r.reason == Some(PauseReason::Issue) => p.red,
                RunStatus::Paused => p.amber,
                RunStatus::Done => p.green,
                RunStatus::Cancelled => p.faint,
            },
            None => p.faint,
        }
    }
}

pub struct App {
    rt: tokio::runtime::Runtime,
    rx: Receiver<Event>,
    pub bus: Bus,
    pub store: Store,
    pub profiles: Vec<Profile>,
    pub settings: Settings,
    pub jobs: Vec<JobEntry>,
    pub active: usize,
    pub screen: Screen,
    pub notice: Option<(Level, String, Instant)>,
    pub cat: Option<Loaded>,
    pub cat_loading: bool,
    pub conn: ConnectionsView,
    pub doctor: DoctorView,
    pub compare: CompareView,
    pub migration: MigrationView,
    #[cfg(debug_assertions)]
    shot: Option<crate::shot::Shot>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("ferry-io")
            .enable_all()
            .build()
            .expect("tokio runtime");
        let (tx, rx) = channel();
        let store = Store::open();
        let profiles = store.load_profiles();
        let settings = store.load_settings();
        theme::install_fonts(&cc.egui_ctx);
        theme::apply(&cc.egui_ctx, settings.theme);

        let mut jobs: Vec<JobEntry> = store.load_jobs().into_iter().map(|j| JobEntry::new(j, &store)).collect();
        if jobs.is_empty() {
            let known = |id: &Option<String>| id.clone().filter(|i| profiles.iter().any(|p| &p.id == i));
            let j = Job { source_id: known(&settings.last_source), target_id: known(&settings.last_target), ..Default::default() };
            let _ = store.save_job(&j);
            jobs.push(JobEntry::new(j, &store));
        }
        let active = settings
            .active_job
            .as_ref()
            .and_then(|id| jobs.iter().position(|e| &e.job.id == id))
            .unwrap_or(jobs.len() - 1);
        let mut app = Self {
            rt,
            rx,
            bus: Bus { tx, ctx: cc.egui_ctx.clone() },
            store,
            profiles,
            settings,
            jobs,
            active,
            screen: Screen::Connections,
            notice: None,
            cat: None,
            cat_loading: false,
            conn: ConnectionsView::default(),
            doctor: DoctorView::default(),
            compare: CompareView::default(),
            migration: MigrationView::default(),
            #[cfg(debug_assertions)]
            shot: crate::shot::Shot::from_env(),
        };
        for i in 0..app.jobs.len() {
            app.check_alive(i);
        }
        app.screen = app.landing_screen();
        #[cfg(debug_assertions)]
        if let Ok(s) = std::env::var("FERRY_SCREEN") {
            if let Some(sc) = Screen::ALL.into_iter().find(|x| x.label().eq_ignore_ascii_case(&s)) {
                app.screen = sc;
                if sc == Screen::Doctor {
                    app.run_doctor();
                }
                if matches!(sc, Screen::Compare | Screen::Migration) {
                    app.ensure_catalogs();
                }
            }
        }
        app.conn.reload(&app.profiles, &app.jobs[app.active].job, &app.store);
        app
    }

    // ---- infrastructure ---------------------------------------------------------------------

    pub fn spawn(&self, fut: impl Future<Output = ()> + Send + 'static) {
        self.rt.spawn(fut);
    }

    pub fn notify(&mut self, level: Level, text: impl Into<String>) {
        let t: String = text.into();
        self.notice = Some((level, t.lines().next().unwrap_or_default().to_string(), Instant::now()));
    }

    pub fn job(&self) -> &Job {
        &self.jobs[self.active].job
    }

    pub fn entry(&self) -> &JobEntry {
        &self.jobs[self.active]
    }

    pub fn job_mut(&mut self) -> &mut Job {
        &mut self.jobs[self.active].job
    }

    pub fn save_job(&mut self) {
        if let Err(e) = self.store.save_job(&self.jobs[self.active].job) {
            self.notify(Level::Error, format!("Couldn't save the job: {e}"));
        }
    }

    pub fn save_profiles(&mut self) {
        if let Err(e) = self.store.save_profiles(&self.profiles) {
            self.notify(Level::Error, format!("Couldn't save connections: {e}"));
        }
    }

    pub fn save_settings(&mut self) {
        self.settings.active_job = Some(self.job().id.clone());
        let _ = self.store.save_settings(&self.settings);
    }

    pub fn profile(&self, id: &Option<String>) -> Option<&Profile> {
        id.as_ref().and_then(|i| self.profiles.iter().find(|p| &p.id == i))
    }

    /// Connection parameters with the password loaded from the keychain (cached afterwards).
    pub fn params(&mut self, id: &Option<String>) -> Option<ConnParams> {
        let id = id.clone()?;
        let idx = self.profiles.iter().position(|p| p.id == id)?;
        if !self.profiles[idx].password_loaded {
            self.profiles[idx].params.password = self.store.get_password(&id).unwrap_or_default();
            self.profiles[idx].password_loaded = true;
        }
        Some(self.profiles[idx].params.clone())
    }

    pub fn job_name(&self, job: &Job) -> String {
        match (self.profile(&job.source_id), self.profile(&job.target_id)) {
            (Some(s), Some(t)) => format!("{} → {}", s.label(), t.label()),
            (Some(s), None) => format!("{} → …", s.label()),
            _ => "New migration".into(),
        }
    }

    pub fn schema_filter(&self) -> SchemaFilter {
        let job = self.job();
        let ex = |id: &Option<String>| self.profile(id).map(|p| p.excluded_schemas.clone()).unwrap_or_default();
        let (a, b) = (ex(&job.source_id), ex(&job.target_id));
        crate::worker::schema_filter(job, &[&a, &b])
    }

    /// Fingerprint of the job's pair; Doctor results and loaded catalogs are valid only for it.
    pub fn signature(&self) -> u64 {
        let mut h = DefaultHasher::new();
        let job = self.job();
        for id in [&job.source_id, &job.target_id] {
            match self.profile(id) {
                Some(p) => p.signature(&mut h),
                None => h.write_u8(0),
            }
        }
        h.write(job.schemas.as_bytes());
        h.finish()
    }

    // ---- gating -----------------------------------------------------------------------------

    /// Why the Doctor can't run yet (None = ready).
    pub fn pair_problem(&self) -> Option<&'static str> {
        let job = self.job();
        if self.profiles.len() < 2 {
            return Some("Add two connections first");
        }
        match (self.profile(&job.source_id), self.profile(&job.target_id)) {
            (None, _) => Some("Pick a source connection"),
            (_, None) => Some("Pick a target connection"),
            (Some(s), Some(t)) if s.id == t.id => Some("Source and target must differ"),
            _ => None,
        }
    }

    pub fn doctor_passed(&self) -> bool {
        self.pair_problem().is_none()
            && self.job().doctor.as_ref().map(|d| d.sig == self.signature() && !d.blocked()).unwrap_or(false)
    }

    pub fn step_state(&self, s: Screen) -> StepState {
        match s {
            Screen::Connections => {
                if self.pair_problem().is_none() { StepState::Done } else { StepState::Open }
            }
            Screen::Doctor => match self.pair_problem() {
                Some(_) => StepState::Locked,
                None if self.doctor_passed() => StepState::Done,
                None => StepState::Open,
            },
            Screen::Compare => {
                if self.doctor_passed() { StepState::Open } else { StepState::Locked }
            }
            // An existing run stays reachable so it can be watched, resumed or discarded.
            Screen::Migration => {
                if self.doctor_passed() || self.entry().running() || self.entry().run.is_some() { StepState::Open } else { StepState::Locked }
            }
        }
    }

    fn lock_reason(&self, s: Screen) -> &'static str {
        match s {
            Screen::Doctor => self.pair_problem().unwrap_or(""),
            _ => self.pair_problem().unwrap_or("Run Doctor and fix blockers first"),
        }
    }

    fn landing_screen(&self) -> Screen {
        if self.entry().run.is_some() && self.step_state(Screen::Migration) != StepState::Locked {
            Screen::Migration
        } else if self.doctor_passed() {
            Screen::Compare
        } else if self.pair_problem().is_none() {
            Screen::Doctor
        } else {
            Screen::Connections
        }
    }

    pub fn go(&mut self, s: Screen) {
        if self.step_state(s) == StepState::Locked {
            self.notify(Level::Warn, self.lock_reason(s));
            return;
        }
        self.screen = s;
        match s {
            Screen::Doctor if self.job().doctor.is_none() && !self.doctor.running => self.run_doctor(),
            Screen::Compare | Screen::Migration => self.ensure_catalogs(),
            _ => {}
        }
    }

    pub fn select_job(&mut self, i: usize) {
        if i == self.active || i >= self.jobs.len() {
            return;
        }
        self.active = i;
        self.cat = None;
        self.cat_loading = false;
        self.compare = CompareView::default();
        self.migration = MigrationView::default();
        self.doctor = DoctorView::default();
        self.conn.reload(&self.profiles, &self.jobs[i].job, &self.store);
        self.save_settings();
        self.screen = self.landing_screen();
        if matches!(self.screen, Screen::Compare | Screen::Migration) {
            self.ensure_catalogs();
        }
    }

    pub fn new_job(&mut self) {
        let j = Job::default();
        let _ = self.store.save_job(&j);
        self.jobs.push(JobEntry::new(j, &self.store));
        self.select_job(self.jobs.len() - 1);
        self.screen = Screen::Connections;
    }

    fn delete_job(&mut self, i: usize) {
        if self.jobs[i].running() {
            self.notify(Level::Warn, "Pause or cancel the migration first");
            return;
        }
        let job = self.jobs.remove(i).job;
        if self.jobs.is_empty() {
            let j = Job::default();
            let _ = self.store.save_job(&j);
            self.jobs.push(JobEntry::new(j, &self.store));
        }
        // Leftover resume markers on the target belong to this job only.
        if let Some(t) = self.params(&job.target_id) {
            let id = job.id.clone();
            self.spawn(async move {
                let _ = mb_core::data::copy::clear_checkpoint(&t, &id).await;
            });
        }
        self.store.delete_job(&job.id);
        self.active = usize::MAX;
        self.select_job(i.min(self.jobs.len() - 1));
    }

    // ---- background work ----------------------------------------------------------------------

    pub fn ensure_catalogs(&mut self) {
        let fresh = self.cat.as_ref().map(|c| c.job_id == self.job().id && c.sig == self.signature()).unwrap_or(false);
        if !fresh && !self.cat_loading {
            self.load_catalogs();
        }
    }

    /// Introspect source and target concurrently.
    pub fn load_catalogs(&mut self) {
        let (s_id, t_id) = (self.job().source_id.clone(), self.job().target_id.clone());
        let (Some(src), Some(tgt)) = (self.params(&s_id), self.params(&t_id)) else { return };
        let filter = self.schema_filter();
        let ignore_comments = self.settings.ignore_comments;
        let (job_id, sig) = (self.job().id.clone(), self.signature());
        let bus = self.bus.clone();
        self.cat_loading = true;
        self.spawn(async move {
            let t0 = Instant::now();
            let (so, to) = (SessionOptions::introspection(Side::Source), SessionOptions::introspection(Side::Target));
            let side = |p: ConnParams, o: SessionOptions, f: SchemaFilter| async move {
                let c = connect(&p, &o).await?;
                introspect(&c, &f).await
            };
            let r = tokio::try_join!(side(src, so, filter.clone()), side(tgt, to, filter));
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
                    job_id: job_id.clone(),
                    sig,
                }
            });
            bus.send(Event::Catalogs { job_id, sig, result });
        });
    }

    fn on_catalogs(&mut self, job_id: String, sig: u64, result: Result<Loaded, String>) {
        if job_id != self.job().id || sig != self.signature() {
            return; // stale: the pair changed while reading
        }
        self.cat_loading = false;
        match result {
            Ok(l) => {
                self.compare.on_loaded(&l);
                self.migration.on_loaded(&l, &self.jobs[self.active].job.data);
                self.cat = Some(l);
            }
            Err(e) => self.notify(Level::Error, format!("Reading schemas failed: {e}")),
        }
    }

    fn drain_events(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                Event::Notice(l, m) => self.notify(l, m),
                Event::Tested { profile_id, result } => self.conn.on_tested(profile_id, result),
                Event::Doctor { job_id, sig, findings } => self.on_doctor(job_id, sig, findings),
                Event::Catalogs { job_id, sig, result } => self.on_catalogs(job_id, sig, result),
                Event::Deploy(p) => self.migration.on_deploy_progress(p),
                Event::DeployDone { test, result } => self.on_deploy_done(test, result),
                Event::DataStarted(t) => self.compare.on_data_started(t),
                Event::DataResult(r) => self.compare.on_data_result(r),
                Event::DataDone(r) => {
                    if let Err(e) = self.compare.on_data_done(r) {
                        self.notify(Level::Error, e);
                    }
                }
            }
        }
    }

    /// If the run file says "running" but no worker holds the lock, the run was interrupted.
    fn check_alive(&mut self, i: usize) {
        let e = &mut self.jobs[i];
        if e.starting.is_some() || !e.run.as_ref().map(|r| r.status == RunStatus::Running).unwrap_or(false) {
            return;
        }
        let dir = self.store.job_dir(&e.job.id);
        if jobs::alive(&dir) {
            return;
        }
        // Re-read from disk: the worker may have just written its final state.
        let Some(mut run) = self.store.load_run(&e.job.id) else { return };
        if run.status == RunStatus::Running {
            run.status = RunStatus::Paused;
            run.reason = Some(PauseReason::Interrupted);
            run.phase = "Interrupted".into();
            run.rate_bps = 0.0;
            let _ = self.store.save_run(&e.job.id, &run);
        }
        e.run = Some(run);
        e.stopping = false;
    }

    fn poll_jobs(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        let mut any = false;
        for i in 0..self.jobs.len() {
            let e = &mut self.jobs[i];
            if !e.running() && !e.stopping {
                continue;
            }
            any = true;
            let every = if i == self.active { Duration::from_millis(500) } else { Duration::from_secs(2) };
            if e.last_poll.map(|t| now - t < every).unwrap_or(false) {
                continue;
            }
            e.last_poll = Some(now);
            let path = self.store.run_path(&e.job.id);
            let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
            if mtime != e.run_mtime {
                e.run_mtime = mtime;
                e.run = self.store.load_run(&e.job.id);
            }
            if let Some((t, spawned_ms)) = e.starting {
                let started = e.run.as_ref().map(|r| r.status == RunStatus::Running && r.updated_at >= spawned_ms).unwrap_or(false);
                let gone = e.run.as_ref().map(|r| r.status != RunStatus::Running && r.updated_at >= spawned_ms).unwrap_or(false);
                if started || gone {
                    e.starting = None;
                } else if t.elapsed() > Duration::from_secs(20) {
                    e.starting = None;
                    self.notice = Some((Level::Error, "The migration worker didn't start".into(), Instant::now()));
                }
            }
            if e.run.as_ref().map(|r| r.status != RunStatus::Running).unwrap_or(true) && e.starting.is_none() {
                e.stopping = false;
            }
            self.check_alive(i);
        }
        if any {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }

    pub fn start_worker(&mut self) {
        let id = self.job().id.clone();
        match jobs::spawn(&id) {
            Ok(()) => {
                let e = &mut self.jobs[self.active];
                e.starting = Some((Instant::now(), now_ms()));
                e.stopping = false;
            }
            Err(e) => self.notify(Level::Error, format!("Couldn't start the migration: {e}")),
        }
    }

    pub fn request_stop(&mut self, what: &str) {
        let dir = self.store.job_dir(&self.job().id);
        if let Err(e) = jobs::request(&dir, what) {
            self.notify(Level::Error, format!("Couldn't reach the migration: {e}"));
            return;
        }
        self.jobs[self.active].stopping = true;
    }

    /// Forget a paused/finished run (and its resume markers on the target).
    pub fn discard_run(&mut self) {
        let id = self.job().id.clone();
        self.store.clear_run(&id);
        self.jobs[self.active].run = None;
        self.jobs[self.active].run_mtime = None;
        if let Some(t) = self.params(&self.job().target_id.clone()) {
            self.spawn(async move {
                let _ = mb_core::data::copy::clear_checkpoint(&t, &id).await;
            });
        }
    }

    // ---- chrome -------------------------------------------------------------------------------

    fn brand(&mut self, ui: &mut egui::Ui) {
        let p = theme::pal(ui);
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(40.0, 40.0), egui::Sense::hover());
            ui.painter().rect_filled(rect, CornerRadius::same(10), p.accent);
            // Two opposed arrows: the "ferry" between databases.
            let c = rect.center();
            let s = Stroke::new(1.8, p.on_accent);
            let painter = ui.painter();
            for (y, dir) in [(-4.0, 1.0), (4.0, -1.0)] {
                let a = c + egui::vec2(-8.0 * dir, y);
                let b = c + egui::vec2(8.0 * dir, y);
                painter.line_segment([a, b], s);
                painter.line_segment([b, b + egui::vec2(-3.5 * dir, -3.0)], s);
                painter.line_segment([b, b + egui::vec2(-3.5 * dir, 3.0)], s);
            }
            ui.add_space(4.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.add_space(2.0);
                ui.label(theme::title("Ferry"));
                ui.label(theme::small("Database migration").color(p.muted));
            });
        });
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        let p = theme::pal(ui);
        ui.add_space(18.0);
        self.brand(ui);
        ui.add_space(18.0);

        ui.spacing_mut().item_spacing.y = 2.0;
        for (i, s) in Screen::ALL.into_iter().enumerate() {
            let st = self.step_state(s);
            let sel = self.screen == s;
            let (resp, rect) = theme::row(ui, 40.0, sel);
            theme::step_marker(ui, egui::pos2(rect.left() + 22.0, rect.center().y), i + 1, st, sel);
            let color = match st {
                StepState::Locked => p.faint,
                _ if sel => p.text,
                _ => p.muted,
            };
            let text = if sel { theme::strong(s.label()) } else { RichText::new(s.label()) };
            ui.painter().text(
                egui::pos2(rect.left() + 42.0, rect.center().y),
                egui::Align2::LEFT_CENTER,
                text.text(),
                egui::FontId::new(theme::BODY, if sel { egui::FontFamily::Name("semibold".into()) } else { egui::FontFamily::Proportional }),
                color,
            );
            let resp = if st == StepState::Locked { resp.on_hover_text(self.lock_reason(s)) } else { resp };
            if resp.clicked() {
                self.go(s);
            }
        }

        ui.add_space(22.0);
        let list_h = (ui.available_height() - 64.0).max(80.0);
        egui::ScrollArea::vertical().max_height(list_h).auto_shrink([false, true]).show(ui, |ui| self.sidebar_lists(ui, p));
        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            ui.add_space(16.0);
            let mut t = self.settings.theme;
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                if theme::segmented(ui, &mut t, &[(ThemeChoice::System, "System"), (ThemeChoice::Light, "Light"), (ThemeChoice::Dark, "Dark")]) {
                    self.settings.theme = t;
                    theme::apply(ui.ctx(), t);
                    self.save_settings();
                }
            });
        });
    }

    fn sidebar_lists(&mut self, ui: &mut egui::Ui, p: &'static theme::Pal) {
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.horizontal(|ui| {
            ui.add_space(12.0);
            theme::section(ui, "Jobs", false);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(6.0);
                if theme::ghost(ui, "+").on_hover_text("New migration").clicked() {
                    self.new_job();
                }
            });
        });
        let mut pick = None;
        let mut delete = None;
        for i in 0..self.jobs.len() {
            let sel = i == self.active;
            let name = self.job_name(&self.jobs[i].job);
            let color = self.jobs[i].status_color(p);
            let (resp, rect) = theme::row(ui, 36.0, sel);
            ui.painter().circle_filled(egui::pos2(rect.left() + 20.0, rect.center().y), 4.0, color);
            let font = egui::FontId::new(theme::BODY, if sel { egui::FontFamily::Name("semibold".into()) } else { egui::FontFamily::Proportional });
            let painter = ui.painter().with_clip_rect(rect.shrink(4.0));
            painter.text(egui::pos2(rect.left() + 36.0, rect.center().y), egui::Align2::LEFT_CENTER, name, font, if sel { p.text } else { p.muted });
            if resp.clicked() {
                pick = Some(i);
            }
            resp.context_menu(|ui| {
                if ui.button("Delete").clicked() {
                    delete = Some(i);
                    ui.close();
                }
            });
        }
        if let Some(i) = pick {
            self.select_job(i);
        }
        if let Some(i) = delete {
            self.delete_job(i);
        }

        if !self.profiles.is_empty() {
            ui.add_space(22.0);
            ui.horizontal(|ui| {
                ui.add_space(12.0);
                theme::section(ui, "Connections", false);
            });
            let mut action: Option<(String, u8)> = None;
            for prof in &self.profiles {
                let (resp, rect) = theme::row(ui, 34.0, false);
                let w = theme::engine_badge(ui, rect.left_center() + egui::vec2(12.0, 0.0), prof.engine);
                ui.painter().with_clip_rect(rect.shrink(4.0)).text(
                    rect.left_center() + egui::vec2(22.0 + w, 0.0),
                    egui::Align2::LEFT_CENTER,
                    prof.label(),
                    egui::FontId::proportional(theme::BODY),
                    p.muted,
                );
                let resp = resp.on_hover_text(format!("{}  ·  right-click for options", prof.params.display()));
                let id = prof.id.clone();
                resp.context_menu(|ui| {
                    if ui.button("Use as source").clicked() {
                        action = Some((id.clone(), 0));
                        ui.close();
                    }
                    if ui.button("Use as target").clicked() {
                        action = Some((id.clone(), 1));
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Delete").clicked() {
                        action = Some((id.clone(), 2));
                        ui.close();
                    }
                });
                if resp.clicked() {
                    action = Some((id, 3));
                }
            }
            if let Some((id, what)) = action {
                match what {
                    0 => self.set_side(true, Some(id)),
                    1 => self.set_side(false, Some(id)),
                    2 => self.delete_profile(&id),
                    _ => {
                        self.screen = Screen::Connections;
                    }
                }
            }
        }

    }

    pub fn set_side(&mut self, source: bool, id: Option<String>) {
        let job = self.job_mut();
        if source {
            job.source_id = id;
        } else {
            job.target_id = id;
        }
        self.save_job();
        self.cat = None;
        self.conn.reload(&self.profiles, &self.jobs[self.active].job, &self.store);
    }

    pub fn delete_profile(&mut self, id: &str) {
        if self.jobs.iter().any(|e| e.running() && (e.job.source_id.as_deref() == Some(id) || e.job.target_id.as_deref() == Some(id))) {
            self.notify(Level::Warn, "A running migration uses this connection");
            return;
        }
        self.profiles.retain(|p| p.id != id);
        self.store.delete_password(id);
        self.save_profiles();
        for i in 0..self.jobs.len() {
            let j = &mut self.jobs[i].job;
            let mut changed = false;
            if j.source_id.as_deref() == Some(id) {
                j.source_id = None;
                changed = true;
            }
            if j.target_id.as_deref() == Some(id) {
                j.target_id = None;
                changed = true;
            }
            if changed {
                let _ = self.store.save_job(&self.jobs[i].job);
            }
        }
        self.cat = None;
        self.conn.reload(&self.profiles, &self.jobs[self.active].job, &self.store);
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.set_height(44.0);
            let name = self.job_name(self.job());
            ui.label(theme::title(format!("{} · {}", self.screen.label(), name)).size(17.0));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| match self.screen {
                Screen::Connections => self.connections_actions(ui),
                Screen::Doctor => self.doctor_actions(ui),
                Screen::Compare => self.compare_actions(ui),
                Screen::Migration => self.migration_actions(ui),
            });
        });
    }

    fn toast(&mut self, ctx: &egui::Context) {
        let Some((lvl, msg, at)) = &self.notice else { return };
        let age = at.elapsed();
        if age > Duration::from_secs(5) {
            self.notice = None;
            return;
        }
        ctx.request_repaint_after(Duration::from_secs(5) - age);
        let (lvl, msg) = (*lvl, msg.clone());
        egui::Area::new(egui::Id::unique("toast")).anchor(egui::Align2::RIGHT_BOTTOM, [-20.0, -20.0]).show(ctx, |ui| {
            let p = theme::pal(ui);
            let c = match lvl {
                Level::Error => p.red,
                Level::Warn => p.amber,
                _ => p.green,
            };
            egui::Frame::NONE
                .fill(p.card)
                .stroke(Stroke::new(1.0, p.border))
                .corner_radius(CornerRadius::same(10))
                .inner_margin(Margin::symmetric(14, 10))
                .shadow(ui.visuals().popup_shadow)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        theme::dot(ui, c);
                        ui.label(msg);
                    });
                });
        });
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let n = ctx.input(|i| {
            [Key::Num1, Key::Num2, Key::Num3, Key::Num4].iter().position(|k| i.modifiers.command && i.key_pressed(*k))
        });
        if let Some(n) = n {
            self.go(Screen::ALL[n]);
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();
        self.poll_jobs(ctx);
        #[cfg(debug_assertions)]
        if let Some(s) = self.shot.as_mut() {
            s.tick(ctx);
        }
        self.shortcuts(ctx);
        if self.doctor.running || self.cat_loading || self.compare.data_running || self.migration.deploying() {
            // Spinners animate; keep it modest.
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let p = theme::pal(ui);
        egui::Panel::top("top_bar")
            .frame(egui::Frame::NONE.fill(p.sidebar).inner_margin(Margin::symmetric(24, 10)).stroke(Stroke::new(1.0, p.border)))
            .show(ui, |ui| self.top_bar(ui));
        egui::Panel::left("sidebar")
            .resizable(false)
            .exact_size(292.0)
            .frame(egui::Frame::NONE.fill(p.sidebar).inner_margin(Margin::symmetric(14, 0)).stroke(Stroke::new(1.0, p.border)))
            .show(ui, |ui| self.sidebar(ui));
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(p.bg).inner_margin(Margin::symmetric(40, 28)))
            .show(ui, |ui| match self.screen {
                Screen::Connections => self.ui_connections(ui),
                Screen::Doctor => self.ui_doctor(ui),
                Screen::Compare => self.ui_compare(ui),
                Screen::Migration => self.ui_migration(ui),
            });
        let ctx = ui.ctx().clone();
        self.migration_dialogs(&ctx);
        self.toast(&ctx);
    }
}
