//! Connections: a plain list of saved databases. Add and edit happen in a modal; delete asks first.
//! Which connection is the source or target is chosen where it matters — on Doctor, Compare and
//! Migration — so this screen stays a simple address book.

use std::collections::HashMap;

use eframe::egui::{self, Align, Layout, RichText};
use egui_extras::{Column, TableBuilder};
use mb_core::conn::{probe_tcp, test_connection, ConnParams, Engine, Provider, SslMode};
use mb_core::progress::Level;

use crate::app::{App, Confirm, Event};
use crate::store::Profile;
use crate::theme;

/// More than enough for any real setup; stops the list (and the keychain) being flooded.
pub const MAX_PROFILES: usize = 100;
const TEST_COOLDOWN_MS: u64 = 1500;

pub enum Tested {
    Running,
    Done(Result<String, String>),
}

/// The add/edit form shown in the modal.
pub struct Editor {
    pub draft: Profile,
    pub password: String,
    pub port: String,
    pub excluded: String,
    pub url: String,
    pub more: bool,
    pub is_new: bool,
    /// Opened from a source/target picker: assign the saved connection to that side.
    pub assign: Option<bool>,
    pub test: Option<Tested>,
}

impl Editor {
    pub fn new(engine: Engine, assign: Option<bool>) -> Self {
        let draft = Profile::new(engine);
        Self {
            port: draft.params.port.to_string(),
            draft,
            password: String::new(),
            excluded: String::new(),
            url: String::new(),
            more: false,
            is_new: true,
            assign,
            test: None,
        }
    }

    fn edit(p: &Profile, password: String) -> Self {
        Self {
            port: p.params.port.to_string(),
            excluded: p.excluded_schemas.join(", "),
            draft: p.clone(),
            password,
            is_new: false,
            ..Self::new(p.engine, None)
        }
    }

    fn params(&self) -> ConnParams {
        let mut p = self.draft.params.clone();
        p.password = self.password.clone();
        p.port = self.port.trim().parse().unwrap_or(self.draft.engine.default_port());
        p
    }

    /// Name used when the field is left blank.
    fn effective_name(&self) -> String {
        let n = self.draft.name.trim();
        if !n.is_empty() {
            n.to_string()
        } else if !self.draft.params.database.trim().is_empty() {
            self.draft.params.database.trim().to_string()
        } else {
            self.draft.params.host.trim().to_string()
        }
    }

    /// First reason the form can't be saved, if any.
    fn problem(&self, others: &[Profile]) -> Option<String> {
        let host = self.draft.params.host.trim();
        if host.is_empty() {
            return Some("Enter a host".into());
        }
        if host.contains(char::is_whitespace) {
            return Some("Host can't contain spaces".into());
        }
        match self.port.trim().parse::<u16>() {
            Ok(p) if p > 0 => {}
            _ => return Some("Port must be a number from 1 to 65535".into()),
        }
        if self.draft.engine == Engine::Postgres && self.draft.params.database.trim().is_empty() {
            return Some("Enter a database".into());
        }
        if self.draft.params.user.trim().is_empty() {
            return Some("Enter a user".into());
        }
        let name = self.effective_name();
        let port: u16 = self.port.trim().parse().unwrap_or(0);
        for o in others.iter().filter(|o| o.id != self.draft.id) {
            if o.label().eq_ignore_ascii_case(&name) {
                return Some(format!("A connection named “{name}” already exists"));
            }
            let same = o.engine == self.draft.engine
                && o.params.host.eq_ignore_ascii_case(host)
                && o.params.port == port
                && o.params.database == self.draft.params.database.trim()
                && o.params.user == self.draft.params.user.trim();
            if same {
                return Some(format!("“{}” already points at this database and user", o.label()));
            }
        }
        None
    }
}

#[derive(Default)]
pub struct ConnectionsView {
    pub editor: Option<Editor>,
    pub search: String,
    /// Last test per connection in this session (shown in the list).
    pub tested: HashMap<String, Tested>,
}

impl ConnectionsView {
    pub fn on_tested(&mut self, profile_id: String, result: Result<String, String>) {
        if let Some(e) = self.editor.as_mut().filter(|e| e.draft.id == profile_id) {
            e.test = Some(Tested::Done(result.clone()));
        }
        // Only connections that are saved show a status in the list.
        if let Some(t) = self.tested.get_mut(&profile_id) {
            *t = Tested::Done(result);
        }
    }

    fn testing(&self, id: &str) -> bool {
        matches!(self.tested.get(id), Some(Tested::Running))
            || self.editor.as_ref().map(|e| e.draft.id == id && matches!(e.test, Some(Tested::Running))).unwrap_or(false)
    }
}

impl App {
    /// Open the add form. `assign` = Some(true/false) puts the new connection on source/target.
    pub fn add_connection(&mut self, assign: Option<bool>) {
        if self.profiles.len() >= MAX_PROFILES {
            self.notify(Level::Warn, format!("You can keep up to {MAX_PROFILES} connections — delete one first"));
            return;
        }
        self.conn.editor = Some(Editor::new(Engine::Postgres, assign));
    }

    fn edit_connection(&mut self, id: &str) {
        if let Some(why) = self.profile_lock(id) {
            self.notify(Level::Warn, why);
            return;
        }
        let pw = self.params(&Some(id.to_string())).map(|c| c.password).unwrap_or_default();
        if let Some(p) = self.profiles.iter().find(|p| p.id == id) {
            self.conn.editor = Some(Editor::edit(p, pw));
        }
    }

    /// Why a connection can't be edited or deleted right now (a migration depends on it).
    pub fn profile_lock(&self, id: &str) -> Option<&'static str> {
        let uses = |e: &crate::app::JobEntry| e.job.source_id.as_deref() == Some(id) || e.job.target_id.as_deref() == Some(id);
        if self.jobs.iter().any(|e| uses(e) && e.running()) {
            return Some("A running migration uses this connection");
        }
        if self.jobs.iter().any(|e| uses(e) && e.run.as_ref().map(|r| r.status == crate::store::RunStatus::Paused).unwrap_or(false)) {
            return Some("A paused migration uses this connection — resume or discard it first");
        }
        None
    }

    /// Start a connection test unless one is running or just finished. Returns whether it started.
    fn test_params(&mut self, id: String, engine: Engine, params: ConnParams) -> bool {
        if self.conn.testing(&id) || self.cooling(cool_key(&id), TEST_COOLDOWN_MS) {
            return false;
        }
        self.mark(cool_key(&id));
        if self.profiles.iter().any(|p| p.id == id) {
            self.conn.tested.insert(id.clone(), Tested::Running);
        }
        let bus = self.bus.clone();
        self.spawn(async move {
            let result = if engine == Engine::Postgres {
                test_connection(&params)
                    .await
                    .map(|i| format!("PostgreSQL {} · {:.0} ms", i.version.split_whitespace().next().unwrap_or(""), i.latency_ms))
                    .map_err(|e| e.to_string())
            } else {
                probe_tcp(&params.host, params.port, params.connect_timeout_secs)
                    .await
                    .map(|ms| format!("Port reachable · {ms:.0} ms"))
                    .map_err(|e| e.to_string())
            };
            bus.send(Event::Tested { profile_id: id, result });
        });
        true
    }

    fn test_saved(&mut self, id: &str) {
        let Some(params) = self.params(&Some(id.to_string())) else { return };
        let engine = self.profiles.iter().find(|p| p.id == id).map(|p| p.engine).unwrap_or_default();
        self.test_params(id.to_string(), engine, params);
    }

    fn save_editor(&mut self) {
        let Some(mut e) = self.conn.editor.take() else { return };
        e.draft.name = e.effective_name();
        e.draft.params.host = e.draft.params.host.trim().to_string();
        e.draft.params.database = e.draft.params.database.trim().to_string();
        e.draft.params.user = e.draft.params.user.trim().to_string();
        e.draft.params.port = e.port.trim().parse().unwrap_or(e.draft.engine.default_port());
        e.draft.excluded_schemas = e.excluded.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        let mut p = e.draft.clone();
        p.params.password = e.password.clone();
        p.password_loaded = true;
        let id = p.id.clone();
        let name = p.label();
        match self.profiles.iter_mut().find(|x| x.id == id) {
            Some(slot) => *slot = p,
            None => self.profiles.push(p),
        }
        if let Err(err) = self.store.set_password(&id, &e.password) {
            self.notify(Level::Warn, format!("Password kept for this session only (keychain: {err})"));
        }
        self.save_profiles();
        match e.test {
            Some(Tested::Done(r)) => {
                self.conn.tested.insert(id.clone(), Tested::Done(r));
            }
            _ => {
                self.conn.tested.remove(&id);
            }
        }
        // Anything read through the old settings is stale now.
        let job = self.job();
        if job.source_id.as_deref() == Some(id.as_str()) || job.target_id.as_deref() == Some(id.as_str()) {
            self.after_pair_change();
        }
        if let Some(source) = e.assign {
            self.set_side(source, Some(id));
        }
        self.notify(Level::Info, format!("Saved “{name}”"));
    }

    // ---- list -------------------------------------------------------------------------------------

    pub fn ui_connections(&mut self, ui: &mut egui::Ui) {
        let p = theme::pal(ui);
        ui.horizontal(|ui| {
            ui.label(theme::heading("Connections"));
            if !self.profiles.is_empty() {
                theme::muted(ui, self.profiles.len().to_string());
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let full = self.profiles.len() >= MAX_PROFILES;
                let r = theme::primary(ui, !full, "+  Add connection");
                if r.clicked() {
                    self.add_connection(None);
                }
                if full {
                    r.on_disabled_hover_text(format!("Up to {MAX_PROFILES} connections"));
                }
                if self.profiles.len() > 6 {
                    theme::input(ui, &mut self.conn.search, "Search", 220.0);
                }
            });
        });
        ui.add_space(4.0);
        if self.profiles.len() < 2 {
            theme::muted(ui, "Add at least two databases — one to copy from and one to copy to.");
        }
        ui.add_space(14.0);

        if self.profiles.is_empty() {
            theme::empty_state(ui, "No connections yet", "Your databases show up here once you add them.");
            ui.add_space(10.0);
            ui.vertical_centered(|ui| {
                if theme::primary(ui, true, "+  Add connection").clicked() {
                    self.add_connection(None);
                }
            });
            return;
        }

        let q = self.conn.search.trim().to_lowercase();
        let rows: Vec<usize> = (0..self.profiles.len())
            .filter(|&i| {
                let x = &self.profiles[i];
                q.is_empty() || x.label().to_lowercase().contains(&q) || x.params.host.to_lowercase().contains(&q) || x.params.database.to_lowercase().contains(&q)
            })
            .collect();
        let mut action: Option<(String, u8)> = None;
        theme::card(ui, |ui| {
            TableBuilder::new(ui)
                .striped(false)
                .cell_layout(Layout::left_to_right(Align::Center))
                .column(Column::initial(210.0).at_least(140.0).clip(true))
                .column(Column::remainder().at_least(120.0).clip(true))
                .column(Column::initial(120.0).at_least(70.0).clip(true))
                .column(Column::initial(100.0).at_least(60.0).clip(true))
                .column(Column::initial(170.0).at_least(90.0).clip(true))
                .column(Column::exact(140.0))
                .header(28.0, |mut h| {
                    for t in ["Name", "Server", "Database", "User", "Status", ""] {
                        h.col(|ui| {
                            ui.label(theme::small(t).color(p.faint));
                        });
                    }
                })
                .body(|body| {
                    body.rows(42.0, rows.len(), |mut row| {
                        let x = &self.profiles[rows[row.index()]];
                        let lock = self.profile_lock(&x.id);
                        row.col(|ui| {
                            let w = theme::engine_badge(ui, ui.cursor().left_center(), x.engine);
                            ui.add_space(w + 8.0);
                            ui.label(theme::strong(x.label())).on_hover_text(x.engine.label());
                            if x.production {
                                theme::chip(ui, "PROD", p.red).on_hover_text("Writes ask you to type the database name");
                            }
                        });
                        row.col(|ui| {
                            theme::muted(ui, format!("{}:{}", x.params.host, x.params.port));
                        });
                        row.col(|ui| {
                            theme::muted(ui, &x.params.database);
                        });
                        row.col(|ui| {
                            theme::muted(ui, &x.params.user);
                        });
                        row.col(|ui| match self.conn.tested.get(&x.id) {
                            Some(Tested::Running) => {
                                ui.spinner();
                            }
                            Some(Tested::Done(Ok(m))) => {
                                theme::dot(ui, p.green);
                                ui.label(theme::small(m).color(p.green));
                            }
                            Some(Tested::Done(Err(e))) => {
                                theme::dot(ui, p.red);
                                ui.label(theme::small(e.lines().next().unwrap_or_default()).color(p.red)).on_hover_text(e);
                            }
                            None => {
                                ui.label(theme::small("Not tested").color(p.faint));
                            }
                        });
                        row.col(|ui| {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let del = ui.add_enabled(lock.is_none(), egui::Button::new(RichText::new("Delete").color(p.red)).frame(false));
                                if del.clicked() {
                                    action = Some((x.id.clone(), 2));
                                }
                                let edit = ui.add_enabled(lock.is_none(), egui::Button::new(RichText::new("Edit").color(p.text)).frame(false));
                                if edit.clicked() {
                                    action = Some((x.id.clone(), 1));
                                }
                                if let Some(why) = lock {
                                    del.on_disabled_hover_text(why);
                                    edit.on_disabled_hover_text(why);
                                }
                                let busy = self.conn.testing(&x.id);
                                if ui.add_enabled(!busy, egui::Button::new(RichText::new("Test").color(p.muted)).frame(false)).clicked() {
                                    action = Some((x.id.clone(), 0));
                                }
                            });
                        });
                        if row.response().double_clicked() && lock.is_none() {
                            action = Some((x.id.clone(), 1));
                        }
                    });
                });
            if rows.is_empty() {
                theme::muted(ui, "No connection matches your search.");
            }
        });
        match action {
            Some((id, 0)) => self.test_saved(&id),
            Some((id, 1)) => self.edit_connection(&id),
            Some((id, _)) => self.confirm = Some(Confirm::DeleteProfile(id)),
            None => {}
        }
    }

    // ---- add / edit modal -------------------------------------------------------------------------

    pub fn connection_dialog(&mut self, ctx: &egui::Context) {
        if self.conn.editor.is_none() {
            return;
        }
        let others = self.profiles.clone();
        let cooling = {
            let id = self.conn.editor.as_ref().map(|e| e.draft.id.clone()).unwrap_or_default();
            self.cooling(cool_key(&id), TEST_COOLDOWN_MS)
        };
        let (mut save, mut close, mut test) = (false, false, false);
        let modal = egui::Modal::new(egui::Id::unique("conn_modal")).show(ctx, |ui| {
            let p = theme::pal(ui);
            let e = self.conn.editor.as_mut().unwrap();
            ui.set_width(560.0);
            ui.label(theme::title(if e.is_new { "Add connection" } else { "Edit connection" }));
            ui.add_space(8.0);
            let mut changed = false;

            theme::field_label(ui, "Engine");
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 22.0;
                for en in Engine::ALL {
                    if theme::radio(ui, e.draft.engine == en, en.label()).clicked() && e.draft.engine != en {
                        let old = e.draft.engine;
                        if e.port.trim() == old.default_port().to_string() || e.port.trim().is_empty() {
                            e.port = en.default_port().to_string();
                        }
                        if e.draft.params.user == old.default_user() || e.draft.params.user.is_empty() {
                            e.draft.params.user = en.default_user().into();
                        }
                        if en != Engine::Postgres && e.draft.params.database == "postgres" {
                            e.draft.params.database.clear();
                        }
                        e.draft.engine = en;
                        changed = true;
                    }
                }
            });
            ui.add_space(2.0);

            let full = ui.available_width();
            let gap = 14.0;
            theme::field_label(ui, "Name");
            let hint = format!("Optional — defaults to “{}”", e.effective_name_hint());
            changed |= theme::input(ui, &mut e.draft.name, &hint, full).changed();

            let left = (full - gap) * 0.72;
            let right = full - gap - left;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                ui.vertical(|ui| {
                    ui.set_width(left);
                    theme::field_label(ui, "Host");
                    changed |= theme::input(ui, &mut e.draft.params.host, e.draft.provider.host_hint(), left).changed();
                });
                ui.vertical(|ui| {
                    ui.set_width(right);
                    theme::field_label(ui, "Port");
                    changed |= theme::input(ui, &mut e.port, "", right).changed();
                });
            });
            theme::field_label(ui, "Database");
            changed |= theme::input(ui, &mut e.draft.params.database, "", full).changed();
            let half = (full - gap) / 2.0;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                ui.vertical(|ui| {
                    ui.set_width(half);
                    theme::field_label(ui, "User");
                    changed |= theme::input(ui, &mut e.draft.params.user, "", half).changed();
                });
                ui.vertical(|ui| {
                    ui.set_width(half);
                    theme::field_label(ui, "Password");
                    changed |= theme::password(ui, &mut e.password, half).changed();
                });
            });

            if theme::ghost(ui, if e.more { "▾  Fewer options" } else { "▸  More options" }).clicked() {
                e.more = !e.more;
            }
            if e.more {
                egui::Grid::new("conn_more").num_columns(2).spacing([16.0, 10.0]).show(ui, |ui| {
                    if e.draft.engine == Engine::Postgres {
                        theme::field_label(ui, "From URL");
                        let r = theme::input(ui, &mut e.url, "postgresql://user:pass@host:5432/db", 380.0);
                        if r.lost_focus() && !e.url.trim().is_empty() {
                            match ConnParams::parse_url(&e.url) {
                                Ok(pp) => {
                                    let pv = Provider::detect(&pp.host, pp.port);
                                    e.password = pp.password.clone();
                                    e.port = pp.port.to_string();
                                    e.draft.provider = pv;
                                    e.draft.params = ConnParams { password: String::new(), ..pp };
                                    if e.excluded.trim().is_empty() {
                                        e.excluded = pv.excluded_schemas().join(", ");
                                    }
                                    changed = true;
                                }
                                Err(err) => e.test = Some(Tested::Done(Err(err.to_string()))),
                            }
                            e.url.clear(); // never keep the secret around
                        }
                        ui.end_row();

                        theme::field_label(ui, "Provider");
                        let before = e.draft.provider;
                        egui::ComboBox::from_id_salt("conn_provider").width(380.0).selected_text(e.draft.provider.label()).show_ui(ui, |ui| {
                            for pv in Provider::ALL {
                                ui.selectable_value(&mut e.draft.provider, pv, pv.label());
                            }
                        });
                        if before != e.draft.provider {
                            let pv = e.draft.provider;
                            e.draft.params.ssl_mode = pv.default_ssl();
                            e.excluded = pv.excluded_schemas().join(", ");
                            if e.draft.params.database.is_empty() || e.draft.params.database == before.default_database() {
                                e.draft.params.database = pv.default_database().into();
                            }
                            changed = true;
                        }
                        ui.end_row();

                        theme::field_label(ui, "SSL");
                        let before = e.draft.params.ssl_mode;
                        egui::ComboBox::from_id_salt("conn_ssl").width(380.0).selected_text(e.draft.params.ssl_mode.as_str()).show_ui(ui, |ui| {
                            for m in SslMode::ALL {
                                ui.selectable_value(&mut e.draft.params.ssl_mode, m, m.as_str());
                            }
                        });
                        changed |= before != e.draft.params.ssl_mode;
                        ui.end_row();

                        theme::field_label(ui, "Skip schemas");
                        changed |= theme::input(ui, &mut e.excluded, "e.g. auth, storage", 380.0).changed();
                        ui.end_row();
                    }
                    theme::field_label(ui, "Production");
                    changed |= ui.checkbox(&mut e.draft.production, "Ask for confirmation before writes").changed();
                    ui.end_row();
                });
            }
            if changed && !matches!(e.test, Some(Tested::Running)) {
                e.test = None;
            }

            ui.add_space(4.0);
            match &e.test {
                Some(Tested::Running) => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        theme::muted(ui, "Testing…");
                    });
                }
                Some(Tested::Done(Ok(m))) => {
                    ui.horizontal(|ui| {
                        theme::dot(ui, p.green);
                        ui.label(RichText::new(m).color(p.green));
                    });
                }
                Some(Tested::Done(Err(err))) => {
                    ui.horizontal(|ui| {
                        theme::dot(ui, p.red);
                        ui.add(egui::Label::new(RichText::new(err.lines().next().unwrap_or_default()).color(p.red)).truncate()).on_hover_text(err);
                    });
                }
                None => {}
            }
            if e.draft.engine != Engine::Postgres {
                ui.label(theme::small(format!("{} can be saved and tested; migration support is coming.", e.draft.engine.label())).color(p.muted));
            }

            let problem = e.problem(&others);
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if theme::primary(ui, problem.is_none(), "Save").clicked() {
                    save = true;
                }
                let can_test = problem.is_none() && !matches!(e.test, Some(Tested::Running)) && !cooling;
                if theme::secondary(ui, can_test, "Test").clicked() {
                    test = true;
                }
                if theme::ghost(ui, "Cancel").clicked() {
                    close = true;
                }
                if let Some(why) = &problem {
                    ui.add_space(6.0);
                    ui.label(theme::small(why).color(p.amber));
                }
            });
            if problem.is_none() && ui.input(|i| i.key_pressed(egui::Key::Enter) && i.modifiers.command) {
                save = true;
            }
        });
        if modal.should_close() {
            close = true;
        }
        if test {
            let e = self.conn.editor.as_ref().unwrap();
            let (id, engine, params) = (e.draft.id.clone(), e.draft.engine, e.params());
            if self.test_params(id, engine, params) {
                if let Some(e) = self.conn.editor.as_mut() {
                    e.test = Some(Tested::Running);
                }
            }
        }
        if save {
            self.save_editor();
        } else if close {
            self.conn.editor = None;
        }
    }
}

impl Editor {
    fn effective_name_hint(&self) -> String {
        let n = self.effective_name();
        if n.is_empty() { "database name".into() } else { n }
    }
}

fn cool_key(id: &str) -> String {
    format!("test:{id}")
}
