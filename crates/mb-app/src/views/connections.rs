//! Connections: the job's Source and Target, each picked from saved connections or entered inline.
//! Edits save themselves once the field loses focus; passwords go to the OS keychain.

use eframe::egui::{self, RichText};
use mb_core::conn::{probe_tcp, test_connection, ConnParams, Engine, Provider, SslMode};
use mb_core::progress::Level;

use crate::app::{App, Event, Screen};
use crate::store::{Job, Profile, Store};
use crate::theme;

pub struct Card {
    pub draft: Profile,
    pub password: String,
    pub port: String,
    pub excluded: String,
    pub url: String,
    pub dirty: bool,
    pub testing: bool,
    pub test: Option<Result<String, String>>,
    pub more: bool,
}

impl Default for Card {
    fn default() -> Self {
        Self::fresh(Engine::Postgres)
    }
}

impl Card {
    fn fresh(engine: Engine) -> Self {
        let draft = Profile::new(engine);
        Self {
            port: draft.params.port.to_string(),
            draft,
            password: String::new(),
            excluded: String::new(),
            url: String::new(),
            dirty: false,
            testing: false,
            test: None,
            more: false,
        }
    }

    fn load(p: &Profile, password: String) -> Self {
        Self {
            port: p.params.port.to_string(),
            excluded: p.excluded_schemas.join(", "),
            draft: p.clone(),
            password,
            ..Self::fresh(p.engine)
        }
    }

    fn params(&self) -> ConnParams {
        let mut p = self.draft.params.clone();
        p.password = self.password.clone();
        p.port = self.port.trim().parse().unwrap_or(self.draft.engine.default_port());
        p
    }
}

#[derive(Default)]
pub struct ConnectionsView {
    pub cards: [Card; 2],
}

impl ConnectionsView {
    /// Load both cards from the job's current pair.
    pub fn reload(&mut self, profiles: &[Profile], job: &Job, store: &Store) {
        for (i, id) in [&job.source_id, &job.target_id].into_iter().enumerate() {
            let keep_engine = self.cards[i].draft.engine;
            self.cards[i] = match id.as_ref().and_then(|id| profiles.iter().find(|p| &p.id == id)) {
                Some(p) => {
                    let pw = if p.password_loaded { p.params.password.clone() } else { store.get_password(&p.id).unwrap_or_default() };
                    Card::load(p, pw)
                }
                None => Card::fresh(keep_engine),
            };
        }
    }

    pub fn on_tested(&mut self, profile_id: String, result: Result<String, String>) {
        for c in &mut self.cards {
            if c.draft.id == profile_id {
                c.testing = false;
                c.test = Some(result.clone());
            }
        }
    }
}

impl App {
    pub fn connections_actions(&mut self, ui: &mut egui::Ui) {
        let problem = self.pair_problem();
        if theme::primary(ui, problem.is_none(), "Run Doctor  →").clicked() {
            self.screen = Screen::Doctor;
            self.run_doctor();
        }
        if let Some(p) = problem {
            theme::muted(ui, p);
        }
    }

    fn save_card(&mut self, i: usize) {
        let c = &mut self.conn.cards[i];
        c.draft.params.port = c.port.trim().parse().unwrap_or(c.draft.engine.default_port());
        c.draft.excluded_schemas = c.excluded.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        if c.draft.name.trim().is_empty() {
            c.draft.name = if c.draft.params.database.trim().is_empty() { c.draft.params.host.clone() } else { c.draft.params.database.clone() };
        }
        c.dirty = false;
        let mut p = c.draft.clone();
        p.params.password = c.password.clone();
        p.password_loaded = true;
        let pw = c.password.clone();
        let id = p.id.clone();
        match self.profiles.iter_mut().find(|x| x.id == id) {
            Some(slot) => *slot = p,
            None => self.profiles.push(p),
        }
        if let Err(e) = self.store.set_password(&id, &pw) {
            self.notify(Level::Warn, format!("Password kept for this session only (keychain: {e})"));
        }
        self.save_profiles();
        let job = self.job_mut();
        let slot = if i == 0 { &mut job.source_id } else { &mut job.target_id };
        if slot.as_deref() != Some(id.as_str()) {
            *slot = Some(id);
        }
        self.save_job();
        self.cat = None;
    }

    fn test_card(&mut self, i: usize) {
        let c = &mut self.conn.cards[i];
        c.testing = true;
        c.test = None;
        let params = c.params();
        let engine = c.draft.engine;
        let id = c.draft.id.clone();
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
    }

    pub fn ui_connections(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.set_max_width(860.0);
            for i in 0..2 {
                self.connection_card(ui, i);
                ui.add_space(20.0);
            }
            // Optional schema scope for the whole job.
            theme::card(ui, |ui| {
                theme::section(ui, "Scope", true);
                ui.add_space(6.0);
                theme::field_label(ui, "Schemas");
                let mut s = self.job().schemas.clone();
                let r = theme::input(ui, &mut s, "All schemas — or e.g. public, sales", ui.available_width());
                if s != self.job().schemas {
                    self.job_mut().schemas = s;
                }
                if r.lost_focus() {
                    self.save_job();
                    self.cat = None;
                }
            });
            ui.add_space(12.0);
        });
    }

    fn connection_card(&mut self, ui: &mut egui::Ui, i: usize) {
        let p = theme::pal(ui);
        let other = self.conn.cards[1 - i].draft.id.clone();
        let saved_ids: Vec<(String, String)> = self
            .profiles
            .iter()
            .filter(|x| x.engine == self.conn.cards[i].draft.engine)
            .map(|x| (x.id.clone(), x.label()))
            .collect();
        let mut focused = false;
        let mut pick: Option<Option<String>> = None;
        let mut test = false;
        let mut save_now = false;

        theme::card(ui, |ui| {
            let c = &mut self.conn.cards[i];
            let is_saved = saved_ids.iter().any(|(id, _)| *id == c.draft.id);
            theme::section(ui, if i == 0 { "Source" } else { "Target" }, true);
            ui.add_space(6.0);

            theme::field_label(ui, "Engine");
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 22.0;
                for e in Engine::ALL {
                    if theme::radio(ui, c.draft.engine == e, e.label()).clicked() && c.draft.engine != e {
                        // Switch to the first saved connection of that engine, or a blank one.
                        pick = Some(self.profiles.iter().find(|x| x.engine == e && x.id != other).map(|x| x.id.clone()));
                        if pick == Some(None) {
                            *c = Card::fresh(e);
                        }
                    }
                }
            });
            ui.add_space(4.0);

            theme::field_label(ui, "Saved connection");
            let w = ui.available_width();
            let selected = if is_saved { c.draft.label() } else { "New connection".to_string() };
            ui.scope(|ui| {
                ui.spacing_mut().button_padding = egui::vec2(12.0, 10.0);
                egui::ComboBox::from_id_salt(("saved", i)).width(w).selected_text(selected).show_ui(ui, |ui| {
                    for (id, label) in &saved_ids {
                        let mut text = RichText::new(label);
                        if *id == other {
                            text = text.color(p.faint);
                        }
                        if ui.selectable_label(*id == c.draft.id, text).clicked() {
                            pick = Some(Some(id.clone()));
                        }
                    }
                    if !saved_ids.is_empty() {
                        ui.separator();
                    }
                    if ui.selectable_label(!is_saved, "＋ New connection").clicked() {
                        pick = Some(None);
                    }
                });
            });
            ui.add_space(4.0);

            let gap = 14.0;
            let full = ui.available_width();
            let left = (full - gap) * 0.72;
            let right = full - gap - left;
            let mut changed = false;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                ui.vertical(|ui| {
                    ui.set_width(left);
                    theme::field_label(ui, "Host");
                    let r = theme::input(ui, &mut c.draft.params.host, "db.example.com", left);
                    focused |= r.has_focus();
                    changed |= r.changed();
                });
                ui.vertical(|ui| {
                    ui.set_width(right);
                    theme::field_label(ui, "Port");
                    let r = theme::input(ui, &mut c.port, "", right);
                    focused |= r.has_focus();
                    changed |= r.changed();
                });
            });
            theme::field_label(ui, "Database");
            let r = theme::input(ui, &mut c.draft.params.database, "", full);
            focused |= r.has_focus();
            changed |= r.changed();
            let half = (full - gap) / 2.0;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                ui.vertical(|ui| {
                    ui.set_width(half);
                    theme::field_label(ui, "User");
                    let r = theme::input(ui, &mut c.draft.params.user, "", half);
                    focused |= r.has_focus();
                    changed |= r.changed();
                });
                ui.vertical(|ui| {
                    ui.set_width(half);
                    theme::field_label(ui, "Password");
                    let r = theme::password(ui, &mut c.password, half);
                    focused |= r.has_focus();
                    changed |= r.changed();
                });
            });

            if theme::ghost(ui, if c.more { "▾  Fewer options" } else { "▸  More options" }).clicked() {
                c.more = !c.more;
            }
            if c.more {
                egui::Grid::new(("more", i)).num_columns(2).spacing([16.0, 10.0]).show(ui, |ui| {
                    theme::field_label(ui, "Name");
                    let r = theme::input(ui, &mut c.draft.name, "e.g. legacy-mysql", 320.0);
                    focused |= r.has_focus();
                    changed |= r.changed();
                    ui.end_row();

                    if c.draft.engine == Engine::Postgres {
                        theme::field_label(ui, "Provider");
                        let before = c.draft.provider;
                        egui::ComboBox::from_id_salt(("provider", i)).width(320.0).selected_text(c.draft.provider.label()).show_ui(ui, |ui| {
                            for pv in Provider::ALL {
                                ui.selectable_value(&mut c.draft.provider, pv, pv.label());
                            }
                        });
                        if before != c.draft.provider {
                            let pv = c.draft.provider;
                            c.draft.params.ssl_mode = pv.default_ssl();
                            c.excluded = pv.excluded_schemas().join(", ");
                            if c.draft.params.database.is_empty() || c.draft.params.database == before.default_database() {
                                c.draft.params.database = pv.default_database().into();
                            }
                            changed = true;
                        }
                        ui.end_row();

                        theme::field_label(ui, "SSL");
                        let before = c.draft.params.ssl_mode;
                        egui::ComboBox::from_id_salt(("ssl", i)).width(320.0).selected_text(c.draft.params.ssl_mode.as_str()).show_ui(ui, |ui| {
                            for m in SslMode::ALL {
                                ui.selectable_value(&mut c.draft.params.ssl_mode, m, m.as_str());
                            }
                        });
                        changed |= before != c.draft.params.ssl_mode;
                        ui.end_row();

                        theme::field_label(ui, "Skip schemas");
                        let r = theme::input(ui, &mut c.excluded, "e.g. auth, storage", 320.0);
                        focused |= r.has_focus();
                        changed |= r.changed();
                        ui.end_row();

                        theme::field_label(ui, "From URL");
                        let r = theme::input(ui, &mut c.url, "postgresql://user:pass@host:5432/db", 320.0);
                        if r.lost_focus() && !c.url.trim().is_empty() {
                            match ConnParams::parse_url(&c.url) {
                                Ok(pp) => {
                                    let pv = Provider::detect(&pp.host, pp.port);
                                    c.password = pp.password.clone();
                                    c.port = pp.port.to_string();
                                    c.draft.provider = pv;
                                    c.draft.params = ConnParams { password: String::new(), ..pp };
                                    if c.excluded.trim().is_empty() {
                                        c.excluded = pv.excluded_schemas().join(", ");
                                    }
                                    changed = true;
                                }
                                Err(e) => c.test = Some(Err(e.to_string())),
                            }
                            c.url.clear(); // never keep the secret around
                        }
                        focused |= r.has_focus();
                        ui.end_row();
                    }

                    theme::field_label(ui, "Production");
                    changed |= ui.checkbox(&mut c.draft.production, "Ask for confirmation before writes").changed();
                    ui.end_row();
                });
            }
            if changed {
                c.dirty = true;
                c.test = None;
            }

            ui.add_space(6.0);
            let can_test = !c.draft.params.host.trim().is_empty() && !c.testing;
            if theme::wide(ui, can_test, if c.testing { "Testing…" } else { "Test Connection" }).clicked() {
                test = true;
                save_now = c.dirty;
            }
            match &c.test {
                Some(Ok(m)) => {
                    ui.horizontal(|ui| {
                        theme::dot(ui, p.green);
                        ui.label(RichText::new(m).color(p.green));
                    });
                }
                Some(Err(e)) => {
                    ui.horizontal(|ui| {
                        theme::dot(ui, p.red);
                        ui.label(RichText::new(e.lines().next().unwrap_or_default()).color(p.red)).on_hover_text(e);
                    });
                }
                None => {}
            }
            if c.draft.id == other && is_saved {
                ui.horizontal(|ui| {
                    theme::dot(ui, p.amber);
                    ui.label(RichText::new("Same as the other side — pick a different connection").color(p.amber));
                });
            }
            if c.draft.engine != Engine::Postgres {
                ui.label(theme::small(format!("{} can be saved and tested; migration support is coming.", c.draft.engine.label())).color(p.muted));
            }
        });

        if let Some(choice) = pick {
            match choice {
                Some(id) => self.set_side(i == 0, Some(id)),
                None => {
                    let engine = self.conn.cards[i].draft.engine;
                    self.set_side(i == 0, None);
                    self.conn.cards[i] = Card::fresh(engine);
                }
            }
            return;
        }
        let c = &self.conn.cards[i];
        if (save_now || (c.dirty && !focused)) && !c.draft.params.host.trim().is_empty() {
            self.save_card(i);
        }
        if test {
            self.test_card(i);
        }
    }
}
