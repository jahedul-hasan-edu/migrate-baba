use eframe::egui::{self, Color32, RichText};
use mb_core::conn::{ConnParams, Provider, ServerInfo, SslMode};
use mb_core::progress::Level;

use crate::app::App;
use crate::store::{new_id, Profile};
use crate::theme;

pub struct ConnectionsView {
    pub selected: Option<String>,
    pub draft: Profile,
    pub password: String,
    pub url: String,
    pub show_pw: bool,
    pub testing: bool,
    pub test: Option<Result<ServerInfo, String>>,
    pub excluded_text: String,
    pub port_text: String,
}

impl ConnectionsView {
    pub fn new(profiles: &[Profile]) -> Self {
        let draft = profiles.first().cloned().unwrap_or_else(|| Profile::new(Provider::Generic));
        Self {
            selected: profiles.first().map(|p| p.id.clone()),
            excluded_text: draft.excluded_schemas.join(", "),
            port_text: draft.params.port.to_string(),
            draft,
            password: String::new(),
            url: String::new(),
            show_pw: false,
            testing: false,
            test: None,
        }
    }

    fn load(&mut self, p: &Profile, password: String) {
        self.selected = Some(p.id.clone());
        self.draft = p.clone();
        self.password = password;
        self.excluded_text = p.excluded_schemas.join(", ");
        self.port_text = p.params.port.to_string();
        self.test = None;
        self.url.clear();
    }

    fn params(&self) -> ConnParams {
        let mut p = self.draft.params.clone();
        p.password = self.password.clone();
        p.port = self.port_text.trim().parse().unwrap_or(5432);
        p
    }
}

impl App {
    pub fn on_test_result(&mut self, profile_id: String, result: Result<ServerInfo, String>) {
        self.conn_view.testing = false;
        match &result {
            Ok(i) => self.log(
                Level::Info,
                format!("Connected: PostgreSQL {} · round trip {:.0} ms", i.version, i.latency_ms),
            ),
            Err(e) => self.log(Level::Error, format!("Connection test failed: {e}")),
        }
        if self.conn_view.draft.id == profile_id {
            self.conn_view.test = Some(result);
        }
    }

    fn select_profile(&mut self, id: &str) {
        if let Some(p) = self.profiles.iter().find(|p| p.id == id).cloned() {
            let pw = self.params(&Some(p.id.clone())).map(|c| c.password).unwrap_or_default();
            self.conn_view.load(&p, pw);
        }
    }

    fn save_draft(&mut self) {
        let v = &mut self.conn_view;
        v.draft.params.port = v.port_text.trim().parse().unwrap_or(5432);
        v.draft.excluded_schemas =
            v.excluded_text.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        if v.draft.name.trim().is_empty() {
            v.draft.name = v.draft.params.display();
        }
        let mut p = v.draft.clone();
        p.params.password = v.password.clone();
        p.password_loaded = true;
        let pw = v.password.clone();
        let id = p.id.clone();
        match self.profiles.iter_mut().find(|x| x.id == p.id) {
            Some(slot) => *slot = p,
            None => self.profiles.push(p),
        }
        self.conn_view.selected = Some(id.clone());
        if let Err(e) = self.store.set_password(&id, &pw) {
            self.log(Level::Warn, format!("Could not store the password in the OS keychain ({e}); it is kept for this session only."));
        }
        self.save_profiles();
        self.notify(format!("Saved connection '{}'", self.conn_view.draft.name));
    }

    pub fn ui_connections(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("conn_list").resizable(true).default_size(240.0).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Connections");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("+ New").clicked() {
                        let p = Profile::new(Provider::Generic);
                        self.conn_view.load(&p, String::new());
                        self.conn_view.selected = None;
                    }
                });
            });
            ui.separator();
            let mut clicked = None;
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                for p in &self.profiles {
                    let sel = self.conn_view.selected.as_deref() == Some(p.id.as_str());
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("●").color(Color32::from_rgb(p.color[0], p.color[1], p.color[2])));
                        let mut text = RichText::new(p.label());
                        if p.production {
                            text = text.color(theme::RED);
                        }
                        if ui.selectable_label(sel, text).clicked() {
                            clicked = Some(p.id.clone());
                        }
                    });
                    ui.label(RichText::new(format!("   {} · {}", p.provider.label(), p.params.host)).small().weak());
                    ui.add_space(4.0);
                }
                if self.profiles.is_empty() {
                    ui.label(RichText::new("No saved connections yet.\nFill in the form and press Save.").weak());
                }
            });
            if let Some(id) = clicked {
                self.select_profile(&id);
            }
        });

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.set_max_width(640.0);
                self.connection_form(ui);
            });
        });
    }

    fn connection_form(&mut self, ui: &mut egui::Ui) {
        let is_saved = self.profiles.iter().any(|p| p.id == self.conn_view.draft.id);
        ui.heading(if is_saved { "Edit connection" } else { "New connection" });
        ui.add_space(6.0);

        // Paste URL
        ui.horizontal(|ui| {
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.conn_view.url)
                    .hint_text("Paste a connection URL: postgresql://user:pass@host:5432/db?sslmode=require")
                    .desired_width(470.0),
            );
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui.button("Fill from URL").clicked() || enter {
                match ConnParams::parse_url(&self.conn_view.url) {
                    Ok(p) => {
                        let v = &mut self.conn_view;
                        let provider = Provider::detect(&p.host, p.port);
                        v.password = p.password.clone();
                        v.port_text = p.port.to_string();
                        v.draft.provider = provider;
                        v.draft.params = ConnParams { password: String::new(), ..p };
                        if v.excluded_text.trim().is_empty() {
                            v.excluded_text = provider.excluded_schemas().join(", ");
                        }
                        v.url.clear(); // never keep the secret in the URL field
                    }
                    Err(e) => self.log(Level::Error, e.to_string()),
                }
            }
        });
        ui.add_space(8.0);

        let v = &mut self.conn_view;
        egui::Grid::new("conn_form").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label("Provider");
            let before = v.draft.provider;
            egui::ComboBox::from_id_salt("provider").selected_text(v.draft.provider.label()).width(260.0).show_ui(ui, |ui| {
                for p in Provider::ALL {
                    ui.selectable_value(&mut v.draft.provider, p, p.label());
                }
            });
            if before != v.draft.provider {
                let p = v.draft.provider;
                v.draft.params.ssl_mode = p.default_ssl();
                v.excluded_text = p.excluded_schemas().join(", ");
                if v.draft.params.database.is_empty() || v.draft.params.database == before.default_database() {
                    v.draft.params.database = p.default_database().into();
                }
            }
            ui.end_row();

            ui.label("Name");
            ui.add(egui::TextEdit::singleline(&mut v.draft.name).hint_text("e.g. staging").desired_width(360.0));
            ui.end_row();

            ui.label("Host");
            ui.add(egui::TextEdit::singleline(&mut v.draft.params.host).hint_text(v.draft.provider.host_hint()).desired_width(360.0));
            ui.end_row();

            ui.label("Port");
            ui.add(egui::TextEdit::singleline(&mut v.port_text).desired_width(80.0));
            ui.end_row();

            ui.label("Database");
            ui.add(egui::TextEdit::singleline(&mut v.draft.params.database).desired_width(220.0));
            ui.end_row();

            ui.label("User");
            ui.add(egui::TextEdit::singleline(&mut v.draft.params.user).desired_width(220.0));
            ui.end_row();

            ui.label("Password");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut v.password).password(!v.show_pw).desired_width(220.0));
                if ui.small_button(if v.show_pw { "hide" } else { "show" }).clicked() {
                    v.show_pw = !v.show_pw;
                }
                ui.label(RichText::new("stored in OS keychain").small().weak());
            });
            ui.end_row();

            ui.label("SSL mode");
            egui::ComboBox::from_id_salt("ssl").selected_text(v.draft.params.ssl_mode.as_str()).show_ui(ui, |ui| {
                for m in SslMode::ALL {
                    ui.selectable_value(&mut v.draft.params.ssl_mode, m, m.as_str());
                }
            });
            ui.end_row();

            ui.label("Excluded schemas");
            ui.add(
                egui::TextEdit::singleline(&mut v.excluded_text)
                    .hint_text("comma separated, e.g. auth, storage")
                    .desired_width(360.0),
            );
            ui.end_row();

            ui.label("Colour tag");
            ui.horizontal(|ui| {
                ui.color_edit_button_srgb(&mut v.draft.color);
                ui.checkbox(&mut v.draft.production, "Production (extra confirmation for writes)");
            });
            ui.end_row();
        });

        for w in self.conn_view.params().warnings() {
            ui.label(RichText::new(format!("⚠ {w}")).color(theme::AMBER));
        }

        ui.add_space(10.0);
        ui.horizontal(|ui| {
            let can = !self.conn_view.draft.params.host.trim().is_empty();
            if ui.add_enabled(can && !self.conn_view.testing, egui::Button::new("Test connection")).clicked() {
                self.conn_view.testing = true;
                self.conn_view.test = None;
                let p = self.conn_view.params();
                let id = self.conn_view.draft.id.clone();
                self.test_profile(p, id);
            }
            if self.conn_view.testing {
                ui.spinner();
            }
            if theme::primary_button(ui, can, "Save", false).clicked() {
                self.save_draft();
            }
            let id = Some(self.conn_view.draft.id.clone());
            if ui.add_enabled(is_saved, egui::Button::new("Use as source")).clicked() {
                self.source_id = id.clone();
                self.catalogs = None;
                self.save_settings();
                self.notify("Source set");
            }
            if ui.add_enabled(is_saved, egui::Button::new("Use as target")).clicked() {
                self.target_id = id.clone();
                self.catalogs = None;
                self.save_settings();
                self.notify("Target set");
            }
            if ui.add_enabled(is_saved, egui::Button::new("Duplicate")).clicked() {
                let mut p = self.conn_view.draft.clone();
                p.id = new_id();
                p.name = format!("{} (copy)", p.name);
                let pw = self.conn_view.password.clone();
                self.conn_view.load(&p, pw);
                self.conn_view.selected = None;
            }
            if ui.add_enabled(is_saved, egui::Button::new(RichText::new("Delete").color(theme::RED))).clicked() {
                let id = self.conn_view.draft.id.clone();
                self.profiles.retain(|p| p.id != id);
                self.store.delete_password(&id);
                if self.source_id.as_deref() == Some(id.as_str()) {
                    self.source_id = None;
                }
                if self.target_id.as_deref() == Some(id.as_str()) {
                    self.target_id = None;
                }
                self.save_profiles();
                self.save_settings();
                let next = self.profiles.first().cloned().unwrap_or_else(|| Profile::new(Provider::Generic));
                self.conn_view.load(&next, String::new());
                if !self.profiles.is_empty() {
                    let nid = next.id.clone();
                    self.select_profile(&nid);
                }
            }
        });

        ui.add_space(10.0);
        match &self.conn_view.test {
            Some(Ok(i)) => {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.label(RichText::new("✔ Connection OK").color(theme::GREEN).strong());
                    egui::Grid::new("test_info").num_columns(2).show(ui, |ui| {
                        ui.label("Server");
                        ui.label(format!("PostgreSQL {}", i.version));
                        ui.end_row();
                        ui.label("Round trip");
                        ui.label(format!("{:.1} ms (connect {:.0} ms)", i.latency_ms, i.connect_ms));
                        ui.end_row();
                        ui.label("User / database");
                        ui.label(format!("{} / {}", i.current_user, i.database));
                        ui.end_row();
                        ui.label("Superuser");
                        ui.label(if i.is_superuser { "yes" } else { "no" });
                        ui.end_row();
                        ui.label("Can disable triggers");
                        ui.label(if i.can_replica_role {
                            "yes (fast loads ignore FK order)"
                        } else {
                            "no (loads follow foreign-key order)"
                        });
                        ui.end_row();
                    });
                    if i.version_num > 0 && i.version_num < 120000 {
                        ui.label(RichText::new("PostgreSQL 12 or newer is required for schema compare.").color(theme::RED));
                    }
                });
            }
            Some(Err(e)) => {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.label(RichText::new("✖ Connection failed").color(theme::RED).strong());
                    ui.label(e);
                });
            }
            None => {}
        }
    }
}
