//! Persistence: connection profiles, settings, run history. Secrets go to the OS keychain.

use std::path::PathBuf;

use mb_core::conn::{ConnParams, Provider};
use serde::{Deserialize, Serialize};

const KEYRING_SERVICE: &str = "migrate-baba";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub provider: Provider,
    pub params: ConnParams,
    /// Environment colour tag (RGB).
    pub color: [u8; 3],
    /// Production targets get extra confirmation.
    pub production: bool,
    /// Schemas excluded from compare/migrate.
    pub excluded_schemas: Vec<String>,
    /// Password is in memory only (loaded from / saved to the keychain).
    #[serde(skip)]
    pub password_loaded: bool,
}

impl Profile {
    pub fn new(provider: Provider) -> Self {
        let params = ConnParams {
            port: provider.default_port(),
            ssl_mode: provider.default_ssl(),
            user: provider.default_user().into(),
            database: provider.default_database().into(),
            host: if provider == Provider::Local { "localhost".into() } else { String::new() },
            ..Default::default()
        };
        Self {
            id: new_id(),
            name: String::new(),
            provider,
            params,
            color: [90, 140, 255],
            production: false,
            excluded_schemas: provider.excluded_schemas(),
            password_loaded: true,
        }
    }

    pub fn label(&self) -> String {
        if self.name.is_empty() {
            self.params.display()
        } else {
            self.name.clone()
        }
    }
}

pub fn new_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("p{:x}", n)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeChoice {
    #[default]
    System,
    Dark,
    Light,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: ThemeChoice,
    pub ui_scale: f32,
    pub workers: usize,
    pub chunk_size: u64,
    pub lock_timeout_secs: u64,
    pub ignore_comments: bool,
    pub cascade_drops: bool,
    pub concurrent_indexes: bool,
    pub last_source: Option<String>,
    pub last_target: Option<String>,
    pub schema_include: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::System,
            ui_scale: 1.0,
            workers: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 8),
            chunk_size: 10_000,
            lock_timeout_secs: 10,
            ignore_comments: false,
            cascade_drops: false,
            concurrent_indexes: false,
            last_source: None,
            last_target: None,
            schema_include: String::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: String,
    pub kind: String,
    pub when: String,
    pub source: String,
    pub target: String,
    pub ok: bool,
    pub millis: u128,
    pub summary: String,
    pub script: String,
    pub log: Vec<String>,
}

pub struct Store {
    config_dir: PathBuf,
    history_dir: PathBuf,
    pub keychain_ok: bool,
}

impl Store {
    pub fn open() -> Self {
        let dirs = directories::ProjectDirs::from("com", "MigrateBaba", "MigrateBaba");
        let (config_dir, data_dir) = match &dirs {
            Some(d) => (d.config_dir().to_path_buf(), d.data_dir().to_path_buf()),
            None => (PathBuf::from(".migrate-baba"), PathBuf::from(".migrate-baba")),
        };
        let history_dir = data_dir.join("history");
        let _ = std::fs::create_dir_all(&config_dir);
        let _ = std::fs::create_dir_all(&history_dir);
        Self { config_dir, history_dir, keychain_ok: true }
    }

    pub fn history_dir(&self) -> &std::path::Path {
        &self.history_dir
    }

    fn read_json<T: serde::de::DeserializeOwned>(&self, name: &str) -> Option<T> {
        let data = std::fs::read_to_string(self.config_dir.join(name)).ok()?;
        serde_json::from_str(&data).ok()
    }

    fn write_json<T: Serialize>(&self, name: &str, value: &T) -> std::io::Result<()> {
        let path = self.config_dir.join(name);
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(value).unwrap_or_default())?;
        std::fs::rename(tmp, path)
    }

    pub fn load_profiles(&self) -> Vec<Profile> {
        let mut v: Vec<Profile> = self.read_json("profiles.json").unwrap_or_default();
        for p in &mut v {
            p.password_loaded = false;
        }
        v
    }

    pub fn save_profiles(&self, profiles: &[Profile]) -> std::io::Result<()> {
        self.write_json("profiles.json", &profiles)
    }

    pub fn load_settings(&self) -> Settings {
        self.read_json("settings.json").unwrap_or_default()
    }

    pub fn save_settings(&self, s: &Settings) -> std::io::Result<()> {
        self.write_json("settings.json", s)
    }

    pub fn set_password(&mut self, id: &str, password: &str) -> Result<(), String> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, id).map_err(|e| e.to_string())?;
        let r = if password.is_empty() {
            match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(e) => Err(e.to_string()),
            }
        } else {
            entry.set_password(password).map_err(|e| e.to_string())
        };
        self.keychain_ok = r.is_ok();
        r
    }

    pub fn get_password(&self, id: &str) -> Option<String> {
        keyring::Entry::new(KEYRING_SERVICE, id).ok()?.get_password().ok()
    }

    pub fn delete_password(&self, id: &str) {
        if let Ok(e) = keyring::Entry::new(KEYRING_SERVICE, id) {
            let _ = e.delete_credential();
        }
    }

    pub fn load_history(&self) -> Vec<HistoryEntry> {
        let mut out: Vec<HistoryEntry> = std::fs::read_dir(&self.history_dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().extension().map(|x| x == "json").unwrap_or(false))
            .filter_map(|e| std::fs::read_to_string(e.path()).ok())
            .filter_map(|s| serde_json::from_str(&s).ok())
            .collect();
        out.sort_by(|a, b| b.id.cmp(&a.id));
        out
    }

    pub fn add_history(&self, entry: &HistoryEntry) {
        let path = self.history_dir.join(format!("{}.json", entry.id));
        let _ = std::fs::write(path, serde_json::to_vec_pretty(entry).unwrap_or_default());
    }

    pub fn delete_history(&self, id: &str) {
        let _ = std::fs::remove_file(self.history_dir.join(format!("{id}.json")));
    }
}
