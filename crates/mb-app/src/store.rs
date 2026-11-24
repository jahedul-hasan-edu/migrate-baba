//! Persistence: connection profiles, settings and migration jobs. Secrets live in the OS keychain only.
//!
//! Layout (per user):
//! - `<config>/profiles.json`, `<config>/settings.json`
//! - `<data>/jobs/<id>/job.json`  job definition, written by the app
//! - `<data>/jobs/<id>/run.json`  run state + checkpoint, written by the background worker
//! - `<data>/jobs/<id>/control`   "pause" / "cancel" requests from the app to the worker
//! - `<data>/jobs/<id>/lock`      held by the live worker (released by the OS if it dies)
//! - `<data>/jobs/<id>/log.txt`   worker log

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use mb_core::conn::{ConnParams, Engine, Provider};
use mb_core::data::copy::{CopyMode, Part};
use mb_core::doctor::Finding;
use mb_core::model::QName;
use serde::{Deserialize, Serialize};

const KEYRING_SERVICE: &str = "migrate-baba";

pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

pub fn new_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("p{:x}", n)
}

// ---- connections ----------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Profile {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub engine: Engine,
    pub provider: Provider,
    pub params: ConnParams,
    /// Production targets get extra confirmation.
    pub production: bool,
    /// Schemas excluded from compare/migrate.
    pub excluded_schemas: Vec<String>,
    /// Password is in memory only (loaded from / saved to the keychain).
    #[serde(skip)]
    pub password_loaded: bool,
}

impl Profile {
    pub fn new(engine: Engine) -> Self {
        let provider = Provider::Generic;
        let params = ConnParams {
            port: engine.default_port(),
            ssl_mode: provider.default_ssl(),
            user: engine.default_user().into(),
            database: if engine == Engine::Postgres { "postgres".into() } else { String::new() },
            host: String::new(),
            ..Default::default()
        };
        Self {
            id: new_id(),
            name: String::new(),
            engine,
            provider,
            params,
            production: false,
            excluded_schemas: Vec::new(),
            password_loaded: true,
        }
    }

    pub fn label(&self) -> String {
        if self.name.trim().is_empty() {
            if self.params.host.is_empty() { "New connection".into() } else { self.params.display() }
        } else {
            self.name.clone()
        }
    }

    /// Identity of everything that affects Doctor results (not the password).
    pub fn signature(&self, h: &mut impl Hasher) {
        self.engine.hash(h);
        self.params.host.hash(h);
        self.params.port.hash(h);
        self.params.database.hash(h);
        self.params.user.hash(h);
        self.params.ssl_mode.as_str().hash(h);
        self.excluded_schemas.hash(h);
    }
}

// ---- settings ---------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeChoice {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: ThemeChoice,
    pub chunk_size: u64,
    pub lock_timeout_secs: u64,
    pub ignore_comments: bool,
    pub cascade_drops: bool,
    pub concurrent_indexes: bool,
    pub active_job: Option<String>,
    /// Pair picked in versions before jobs existed; seeds the first job, then dropped.
    #[serde(skip_serializing)]
    pub last_source: Option<String>,
    #[serde(skip_serializing)]
    pub last_target: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::System,
            chunk_size: 10_000,
            lock_timeout_secs: 10,
            ignore_comments: false,
            cascade_drops: false,
            concurrent_indexes: false,
            active_job: None,
            last_source: None,
            last_target: None,
        }
    }
}

// ---- jobs -------------------------------------------------------------------------------------

pub fn default_workers() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 8)
}

/// What a data migration copies and how.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DataSpec {
    pub tables: Vec<QName>,
    pub mode: CopyMode,
    pub workers: usize,
    pub create_missing: bool,
    pub consistent_snapshot: bool,
    pub fast_load: bool,
    pub reset_sequences: bool,
    pub analyze: bool,
}

impl Default for DataSpec {
    fn default() -> Self {
        Self {
            tables: Vec::new(),
            mode: CopyMode::Replace,
            workers: default_workers(),
            create_missing: true,
            consistent_snapshot: true,
            fast_load: true,
            reset_sequences: true,
            analyze: true,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DoctorRecord {
    pub at: i64,
    pub sig: u64,
    pub findings: Vec<Finding>,
}

impl DoctorRecord {
    pub fn blocked(&self) -> bool {
        mb_core::doctor::blocked(&self.findings)
    }
}

/// A migration job: a source → target pair with its settings. Persisted as `job.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Job {
    pub id: String,
    pub created: i64,
    pub source_id: Option<String>,
    pub target_id: Option<String>,
    /// Comma-separated schemas to include (empty = all).
    pub schemas: String,
    pub doctor: Option<DoctorRecord>,
    pub data: DataSpec,
}

impl Default for Job {
    fn default() -> Self {
        Self {
            id: format!("j{}", &new_id()[1..]),
            created: now_ms(),
            source_id: None,
            target_id: None,
            schemas: String::new(),
            doctor: None,
            data: DataSpec::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunStatus {
    #[default]
    Running,
    Paused,
    Done,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PauseReason {
    /// The user pressed Pause.
    User,
    /// The worker stopped without saying why (app/computer shut down, crash).
    Interrupted,
    /// An obstacle needs fixing; see `issue`.
    Issue,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Issue {
    pub title: String,
    pub detail: String,
    pub hint: String,
    pub table: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TableRun {
    pub table: QName,
    pub est_rows: u64,
    pub est_bytes: u64,
    pub rows: u64,
    pub bytes: u64,
    pub state: u8,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CheckpointData {
    pub parts: BTreeMap<String, Vec<Part>>,
    /// Committed parts per table: (index, rows, bytes).
    pub done: BTreeMap<String, Vec<(u32, u64, u64)>>,
}

impl CheckpointData {
    pub fn has_progress(&self) -> bool {
        self.done.values().any(|v| !v.is_empty())
    }
}

/// Live state of a data migration, written by the background worker. Persisted as `run.json`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RunState {
    pub status: RunStatus,
    pub reason: Option<PauseReason>,
    pub issue: Option<Issue>,
    pub phase: String,
    pub pid: u32,
    pub started_at: i64,
    pub updated_at: i64,
    pub finished_at: i64,
    /// Time spent running, summed over all resumes.
    pub active_ms: u64,
    pub rate_bps: f64,
    pub tables: Vec<TableRun>,
    pub warnings: Vec<String>,
    pub spec: DataSpec,
    pub ckpt: CheckpointData,
}

impl RunState {
    pub fn totals(&self) -> (u64, u64, u64) {
        let (mut bytes, mut est, mut rows) = (0, 0, 0);
        for t in &self.tables {
            let done = t.state == mb_core::data::state::DONE;
            bytes += if done { t.bytes.max(t.est_bytes) } else { t.bytes.min(t.est_bytes.max(1)) };
            est += t.est_bytes.max(1);
            rows += t.rows;
        }
        (bytes, est, rows)
    }

    pub fn fraction(&self) -> f32 {
        if self.status == RunStatus::Done {
            return 1.0;
        }
        let (b, e, _) = self.totals();
        if e == 0 { 0.0 } else { (b as f32 / e as f32).clamp(0.0, 0.999) }
    }
}

// ---- store ------------------------------------------------------------------------------------

pub struct Store {
    config_dir: PathBuf,
    jobs_dir: PathBuf,
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(tmp, path)
}

pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let data = std::fs::read(path).ok()?;
    serde_json::from_slice(&data).ok()
}

pub fn write_json<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    write_atomic(path, &serde_json::to_vec(value).unwrap_or_default())
}

impl Store {
    pub fn open() -> Self {
        // FERRY_HOME keeps everything in one folder (portable installs, tests).
        let dirs = directories::ProjectDirs::from("com", "MigrateBaba", "MigrateBaba");
        let (config_dir, data_dir) = match (std::env::var_os("FERRY_HOME"), &dirs) {
            (Some(h), _) => (PathBuf::from(&h), PathBuf::from(h)),
            (None, Some(d)) => (d.config_dir().to_path_buf(), d.data_dir().to_path_buf()),
            (None, None) => (PathBuf::from(".migrate-baba"), PathBuf::from(".migrate-baba")),
        };
        let jobs_dir = data_dir.join("jobs");
        let _ = std::fs::create_dir_all(&config_dir);
        let _ = std::fs::create_dir_all(&jobs_dir);
        Self { config_dir, jobs_dir }
    }

    pub fn load_profiles(&self) -> Vec<Profile> {
        let mut v: Vec<Profile> = read_json(&self.config_dir.join("profiles.json")).unwrap_or_default();
        for p in &mut v {
            p.password_loaded = false;
        }
        v
    }

    pub fn save_profiles(&self, profiles: &[Profile]) -> std::io::Result<()> {
        write_json(&self.config_dir.join("profiles.json"), &profiles)
    }

    pub fn load_settings(&self) -> Settings {
        read_json(&self.config_dir.join("settings.json")).unwrap_or_default()
    }

    pub fn save_settings(&self, s: &Settings) -> std::io::Result<()> {
        write_json(&self.config_dir.join("settings.json"), s)
    }

    pub fn set_password(&self, id: &str, password: &str) -> Result<(), String> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, id).map_err(|e| e.to_string())?;
        if password.is_empty() {
            match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(e) => Err(e.to_string()),
            }
        } else {
            entry.set_password(password).map_err(|e| e.to_string())
        }
    }

    pub fn get_password(&self, id: &str) -> Option<String> {
        keyring::Entry::new(KEYRING_SERVICE, id).ok()?.get_password().ok()
    }

    pub fn delete_password(&self, id: &str) {
        if let Ok(e) = keyring::Entry::new(KEYRING_SERVICE, id) {
            let _ = e.delete_credential();
        }
    }

    /// Profile with its password filled in from the keychain.
    pub fn resolve(&self, profiles: &[Profile], id: &str) -> Option<Profile> {
        let mut p = profiles.iter().find(|p| p.id == id)?.clone();
        if !p.password_loaded {
            p.params.password = self.get_password(id).unwrap_or_default();
            p.password_loaded = true;
        }
        Some(p)
    }

    // -- jobs --

    pub fn job_dir(&self, id: &str) -> PathBuf {
        self.jobs_dir.join(id)
    }

    pub fn load_jobs(&self) -> Vec<Job> {
        let mut v: Vec<Job> = std::fs::read_dir(&self.jobs_dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| read_json::<Job>(&e.path().join("job.json")))
            .collect();
        v.sort_by_key(|j| j.created);
        v
    }

    pub fn load_job(&self, id: &str) -> Option<Job> {
        read_json(&self.job_dir(id).join("job.json"))
    }

    pub fn save_job(&self, job: &Job) -> std::io::Result<()> {
        let dir = self.job_dir(&job.id);
        std::fs::create_dir_all(&dir)?;
        write_json(&dir.join("job.json"), job)
    }

    pub fn delete_job(&self, id: &str) {
        let _ = std::fs::remove_dir_all(self.job_dir(id));
    }

    pub fn run_path(&self, id: &str) -> PathBuf {
        self.job_dir(id).join("run.json")
    }

    pub fn load_run(&self, id: &str) -> Option<RunState> {
        read_json(&self.run_path(id))
    }

    pub fn save_run(&self, id: &str, run: &RunState) -> std::io::Result<()> {
        write_json(&self.run_path(id), run)
    }

    pub fn clear_run(&self, id: &str) {
        let _ = std::fs::remove_file(self.run_path(id));
    }
}
