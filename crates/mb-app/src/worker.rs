//! Headless data-migration runner (`mb-app --run-job <id>`).
//!
//! Reads `job.json`, resumes from `run.json` when the previous run was paused, and keeps `run.json`
//! current twice a second. Every committed chunk is checkpointed, so a pause, crash or shutdown
//! loses at most the chunks in flight. Any obstacle pauses the run with an [`Issue`] that tells the
//! user what to fix before resuming.

use std::collections::{BTreeSet, VecDeque};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mb_core::conn::{connect, ConnParams, SessionOptions, Side};
use mb_core::data::copy::{self, plan_jobs, Checkpoint, CopyOptions, CopySummary, Part, Resume};
use mb_core::data::{state, TableProgress};
use mb_core::deploy::{deploy, DeployOptions};
use mb_core::diff::{ObjectKey, ObjectKind};
use mb_core::introspect::{introspect, SchemaFilter};
use mb_core::model::{Catalog, QName};
use mb_core::plan::{plan_selection, PlanOptions};
use mb_core::progress::{CancellationToken, Level, LogFn};

use crate::jobs;
use crate::store::{now_ms, CheckpointData, Issue, Job, PauseReason, RunState, RunStatus, Store, TableRun};

struct Ctx {
    store: Store,
    job_id: String,
    dir: PathBuf,
    state: Mutex<RunState>,
    log: Mutex<Option<std::fs::File>>,
}

impl Ctx {
    fn flush(&self) {
        let s = self.state.lock().unwrap();
        let _ = self.store.save_run(&self.job_id, &s);
    }

    fn update(&self, f: impl FnOnce(&mut RunState)) {
        f(&mut self.state.lock().unwrap());
        self.flush();
    }

    fn log(&self, level: Level, msg: &str) {
        if let Some(f) = self.log.lock().unwrap().as_mut() {
            let _ = writeln!(f, "{} {:?} {}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"), level, msg.replace('\n', " | "));
        }
    }
}

fn key(t: &QName) -> String {
    t.to_string()
}

impl Checkpoint for Ctx {
    fn planned(&self, table: &QName) -> Option<Vec<Part>> {
        self.state.lock().unwrap().ckpt.parts.get(&key(table)).cloned()
    }

    fn plan(&self, table: &QName, parts: &[Part]) {
        self.update(|s| {
            s.ckpt.parts.insert(key(table), parts.to_vec());
        });
    }

    fn done(&self, table: &QName) -> Vec<(u32, u64, u64)> {
        self.state.lock().unwrap().ckpt.done.get(&key(table)).cloned().unwrap_or_default()
    }

    fn mark_done(&self, table: &QName, part: u32, rows: u64, bytes: u64) {
        self.update(|s| {
            let v = s.ckpt.done.entry(key(table)).or_default();
            if !v.iter().any(|d| d.0 == part) {
                v.push((part, rows, bytes));
            }
        });
    }
}

fn sqlstate(msg: &str) -> Option<&str> {
    let i = msg.find("[SQLSTATE ")? + 10;
    msg.get(i..i + 5)
}

/// Turn an error into something the user can act on.
pub fn classify(msg: &str, table: Option<&QName>) -> Issue {
    let lower = msg.to_lowercase();
    let (title, hint) = match sqlstate(msg) {
        Some("42501") => ("Permission denied", "Run Doctor for the GRANT statements, apply them, then resume."),
        Some("23505") => ("Duplicate rows on target", "Use Replace or Sync mode, or empty the target table, then resume."),
        Some("23503") => ("Foreign key violation", "Include the referenced tables, or use a superuser on the target for fast load."),
        Some("23502") => ("Missing required value", "A target column is NOT NULL but the source has NULLs. Relax the column, then resume."),
        Some("23514") => ("Check constraint failed", "Source rows violate a target CHECK constraint. Align the constraint, then resume."),
        Some(c) if c.starts_with("22") => ("Value doesn't fit the target", "Migrate the schema first so column types match, then resume."),
        Some("42P01") => ("Table missing on target", "Turn on Create missing tables or migrate the schema, then resume."),
        Some("42703") => ("Columns don't match", "Migrate the schema first so the columns match, then resume."),
        Some("53100") => ("Target disk is full", "Free or add storage on the target server, then resume."),
        Some("53300") => ("Too many connections", "Lower the worker count or free connections on the server, then resume."),
        Some("40P01") | Some("55P03") => ("Table is locked", "Wait for other sessions on the target to finish, then resume."),
        Some(c) if c.starts_with("57P") => ("Server restarted", "Wait until the server is back, then resume."),
        Some(c) if c.starts_with("08") => ("Connection lost", "Check the network and the server, then resume."),
        _ if lower.starts_with("connection failed") => ("Can't reach the database", "Check that the server is up and reachable, then resume."),
        _ if lower.contains("connection")
            || lower.contains("timed out")
            || lower.contains("broken pipe")
            || lower.contains("reset by peer")
            || lower.contains("os error") =>
        {
            ("Connection lost", "Check the network and the server, then resume.")
        }
        _ if lower.contains("password") || lower.contains("authentication") => {
            ("Sign-in failed", "Update the password in Connections, then resume.")
        }
        _ => ("Migration stopped", "Fix the cause shown below, then resume."),
    };
    Issue {
        title: title.into(),
        detail: msg.lines().take(6).collect::<Vec<_>>().join("\n"),
        hint: hint.into(),
        table: table.map(|t| t.to_string()),
    }
}

fn issue(title: &str, detail: impl Into<String>, hint: &str) -> Issue {
    Issue { title: title.into(), detail: detail.into(), hint: hint.into(), table: None }
}

async fn read_pair(src: &ConnParams, tgt: &ConnParams, f: &SchemaFilter) -> mb_core::Result<(Catalog, Catalog)> {
    let (so, to) = (SessionOptions::introspection(Side::Source), SessionOptions::introspection(Side::Target));
    let a = async {
        let c = connect(src, &so).await?;
        introspect(&c, f).await
    };
    let b = async {
        let c = connect(tgt, &to).await?;
        introspect(&c, f).await
    };
    tokio::try_join!(a, b)
}

enum Outcome {
    Done(CopySummary),
    Stopped,
    Failed(Issue),
}

/// Entry point of the worker process. Returns the exit code.
pub fn main(job_id: &str) -> i32 {
    let store = Store::open();
    let dir = store.job_dir(job_id);
    let Some(_lock) = jobs::acquire(&dir) else { return 0 }; // another worker owns it
    let Some(job) = store.load_job(job_id) else { return 1 };
    jobs::keep_awake();
    let _ = jobs::take_request(&dir); // stale request from an earlier run

    let log_path = dir.join("log.txt");
    if std::fs::metadata(&log_path).map(|m| m.len() > 4 << 20).unwrap_or(false) {
        let _ = std::fs::rename(&log_path, dir.join("log.old.txt"));
    }
    let log_file = std::fs::OpenOptions::new().create(true).append(true).open(&log_path).ok();

    let prev = store.load_run(job_id);
    let resuming = prev.as_ref().map(|p| p.status == RunStatus::Paused && p.ckpt.has_progress()).unwrap_or(false);
    let mut st = if resuming { prev.unwrap() } else { RunState { spec: job.data.clone(), started_at: now_ms(), ..Default::default() } };
    st.status = RunStatus::Running;
    st.reason = None;
    st.issue = None;
    st.pid = std::process::id();
    st.phase = "Connecting".into();
    st.updated_at = now_ms();
    st.warnings.clear();
    let base_active = st.active_ms;

    let ctx = Arc::new(Ctx { store, job_id: job_id.to_string(), dir, state: Mutex::new(st), log: Mutex::new(log_file) });
    ctx.flush();
    ctx.log(Level::Info, if resuming { "resuming" } else { "starting" });

    let rt = match tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            ctx.update(|s| {
                s.status = RunStatus::Paused;
                s.reason = Some(PauseReason::Issue);
                s.issue = Some(issue("Couldn't start", e.to_string(), "Restart the app and resume."));
            });
            return 1;
        }
    };

    let cancel = CancellationToken::new();
    let stop_reason: Arc<Mutex<Option<String>>> = Arc::default();
    let progress: Arc<Mutex<Vec<Arc<TableProgress>>>> = Arc::default();
    let session_start = Instant::now();

    let outcome = rt.block_on(async {
        // Ticker: publish progress, watch for pause/cancel.
        let ticker = {
            let (ctx, cancel, stop, progress) = (ctx.clone(), cancel.clone(), stop_reason.clone(), progress.clone());
            tokio::spawn(async move {
                let mut samples: VecDeque<(Instant, u64)> = VecDeque::new();
                let mut iv = tokio::time::interval(Duration::from_millis(500));
                loop {
                    iv.tick().await;
                    if let Some(req) = jobs::take_request(&ctx.dir) {
                        *stop.lock().unwrap() = Some(req);
                        cancel.cancel();
                    }
                    let tables: Vec<TableRun> = progress
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|p| TableRun {
                            table: p.table.clone(),
                            est_rows: p.est_rows,
                            est_bytes: p.est_bytes,
                            rows: p.rows_estimate(),
                            bytes: p.bytes.load(Ordering::Relaxed),
                            state: p.state(),
                        })
                        .collect();
                    let bytes: u64 = tables.iter().map(|t| t.bytes).sum();
                    let now = Instant::now();
                    samples.push_back((now, bytes));
                    while samples.len() > 10 {
                        samples.pop_front();
                    }
                    let rate = match (samples.front(), samples.back()) {
                        (Some((t0, b0)), Some((t1, b1))) if t1 > t0 => b1.saturating_sub(*b0) as f64 / (*t1 - *t0).as_secs_f64(),
                        _ => 0.0,
                    };
                    ctx.update(|s| {
                        if !tables.is_empty() {
                            s.tables = tables;
                        }
                        s.rate_bps = rate;
                        s.updated_at = now_ms();
                        s.active_ms = base_active + session_start.elapsed().as_millis() as u64;
                    });
                }
            })
        };
        let out = execute(&ctx, &job, resuming, &cancel, &progress).await;
        ticker.abort();
        out
    });

    // Final progress snapshot.
    let tables: Vec<TableRun> = progress
        .lock()
        .unwrap()
        .iter()
        .map(|p| TableRun {
            table: p.table.clone(),
            est_rows: p.est_rows,
            est_bytes: p.est_bytes,
            rows: p.rows.load(Ordering::Relaxed),
            bytes: p.bytes.load(Ordering::Relaxed),
            state: if p.state() == state::RUNNING { state::QUEUED } else { p.state() },
        })
        .collect();
    let stop = stop_reason.lock().unwrap().clone();
    let outcome = match (outcome, stop.as_deref()) {
        (_, Some("cancel")) => Outcome::Stopped,
        (Outcome::Failed(_), Some(_)) | (Outcome::Stopped, Some(_)) => Outcome::Stopped,
        (o, _) => o,
    };
    let cancelled = stop.as_deref() == Some("cancel");
    let target = load_params(&ctx.store, &job).map(|p| p.1);
    if matches!(outcome, Outcome::Done(_)) || cancelled {
        if let Ok(t) = &target {
            let _ = rt.block_on(copy::clear_checkpoint(t, job_id));
        }
    }
    ctx.update(|s| {
        if !tables.is_empty() {
            s.tables = tables;
        }
        s.rate_bps = 0.0;
        s.updated_at = now_ms();
        s.active_ms = base_active + session_start.elapsed().as_millis() as u64;
        match &outcome {
            Outcome::Done(sum) => {
                s.status = RunStatus::Done;
                s.phase = "Done".into();
                s.finished_at = now_ms();
                s.warnings = sum.warnings.clone();
                s.ckpt = CheckpointData::default();
            }
            Outcome::Stopped if cancelled => {
                s.status = RunStatus::Cancelled;
                s.phase = "Cancelled".into();
                s.finished_at = now_ms();
                s.ckpt = CheckpointData::default();
            }
            Outcome::Stopped => {
                s.status = RunStatus::Paused;
                s.reason = Some(PauseReason::User);
                s.phase = "Paused".into();
            }
            Outcome::Failed(i) => {
                s.status = RunStatus::Paused;
                s.reason = Some(PauseReason::Issue);
                s.issue = Some(i.clone());
                s.phase = "Needs attention".into();
            }
        }
    });
    match &outcome {
        Outcome::Done(_) => ctx.log(Level::Info, "done"),
        Outcome::Stopped => ctx.log(Level::Info, if cancelled { "cancelled" } else { "paused" }),
        Outcome::Failed(i) => ctx.log(Level::Error, &format!("{}: {}", i.title, i.detail)),
    }
    rt.shutdown_timeout(Duration::from_secs(2));
    0
}

fn load_params(store: &Store, job: &Job) -> Result<(ConnParams, ConnParams), Issue> {
    let profiles = store.load_profiles();
    let get = |id: &Option<String>, side: &str| {
        id.as_ref()
            .and_then(|id| store.resolve(&profiles, id))
            .ok_or_else(|| issue(&format!("{side} connection missing"), "", "Pick it again in Connections, then resume."))
    };
    let (s, t) = (get(&job.source_id, "Source")?, get(&job.target_id, "Target")?);
    for p in [&s, &t] {
        if !p.engine.migratable() {
            return Err(issue(
                &format!("{} isn't supported yet", p.engine.label()),
                p.label(),
                "Migrations currently run between PostgreSQL databases.",
            ));
        }
    }
    Ok((s.params, t.params))
}

pub fn schema_filter(job: &Job, excluded: &[&[String]]) -> SchemaFilter {
    let include = job.schemas.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    let mut exclude: Vec<String> = Vec::new();
    for list in excluded {
        for s in *list {
            if !exclude.contains(s) {
                exclude.push(s.clone());
            }
        }
    }
    SchemaFilter { include, exclude }
}

async fn execute(
    ctx: &Arc<Ctx>,
    job: &Job,
    resuming: bool,
    cancel: &CancellationToken,
    progress: &Arc<Mutex<Vec<Arc<TableProgress>>>>,
) -> Outcome {
    let (src, tgt) = match load_params(&ctx.store, job) {
        Ok(p) => p,
        Err(i) => return Outcome::Failed(i),
    };
    let profiles = ctx.store.load_profiles();
    let excl = |id: &Option<String>| {
        profiles.iter().find(|p| Some(&p.id) == id.as_ref()).map(|p| p.excluded_schemas.clone()).unwrap_or_default()
    };
    let (ea, eb) = (excl(&job.source_id), excl(&job.target_id));
    let filter = schema_filter(job, &[&ea, &eb]);
    let spec = ctx.state.lock().unwrap().spec.clone();
    let log: LogFn = {
        let ctx = ctx.clone();
        Arc::new(move |lvl, msg| ctx.log(lvl, &msg))
    };

    if !resuming {
        // Stale markers from an abandoned run of this job must not be mistaken for progress.
        let _ = copy::clear_checkpoint(&tgt, &ctx.job_id).await;
    }

    ctx.update(|s| s.phase = "Reading schemas".into());
    let (mut s, mut t) = match read_pair(&src, &tgt, &filter).await {
        Ok(p) => p,
        Err(e) => return Outcome::Failed(classify(&e.to_string(), None)),
    };
    let tables: Vec<QName> = spec.tables.iter().filter(|q| s.tables.contains_key(*q)).cloned().collect();
    if tables.is_empty() {
        return Outcome::Failed(issue("No tables to copy", "None of the selected tables exist in the source.", "Pick tables again, then run."));
    }

    if spec.create_missing {
        let missing: Vec<&QName> = tables.iter().filter(|q| !t.tables.contains_key(*q)).collect();
        if !missing.is_empty() {
            ctx.update(|st| st.phase = format!("Creating {} table(s)", missing.len()));
            let mut sel: BTreeSet<ObjectKey> =
                missing.iter().map(|q| ObjectKey::new(ObjectKind::Table, q.schema.clone(), q.name.clone())).collect();
            for k in s.schemas.keys().filter(|k| !t.schemas.contains_key(*k)) {
                sel.insert(ObjectKey::new(ObjectKind::Schema, "", k.clone()));
            }
            for k in s.types.keys().filter(|k| !t.types.contains_key(*k)) {
                sel.insert(ObjectKey::new(ObjectKind::Type, k.schema.clone(), k.name.clone()));
            }
            for k in s.sequences.keys().filter(|k| !t.sequences.contains_key(*k)) {
                sel.insert(ObjectKey::new(ObjectKind::Sequence, k.schema.clone(), k.name.clone()));
            }
            let plan = plan_selection(&s, &t, &sel, &PlanOptions::default());
            match deploy(&tgt, &plan, &DeployOptions::default(), cancel, &|_| {}).await {
                Ok(rep) if rep.ok() => {}
                Ok(rep) => return Outcome::Failed(classify(&rep.error.unwrap_or_default(), None)),
                Err(mb_core::Error::Cancelled) => return Outcome::Stopped,
                Err(e) => return Outcome::Failed(classify(&e.to_string(), None)),
            }
            match read_pair(&src, &tgt, &filter).await {
                Ok(p) => (s, t) = p,
                Err(e) => return Outcome::Failed(classify(&e.to_string(), None)),
            }
        }
    }

    let (jobs, warnings) = plan_jobs(&s, &t, &tables);
    for w in &warnings {
        log(Level::Warn, w.clone());
    }
    if jobs.is_empty() {
        return Outcome::Failed(issue(
            "Tables missing on target",
            warnings.join("\n"),
            "Turn on Create missing tables or migrate the schema, then resume.",
        ));
    }
    *progress.lock().unwrap() = jobs.iter().map(|j| j.progress.clone()).collect();
    ctx.update(|st| {
        st.phase = "Copying".into();
        st.warnings = warnings.clone();
    });

    let opts = CopyOptions {
        mode: spec.mode,
        workers: spec.workers.max(1),
        consistent_snapshot: spec.consistent_snapshot,
        disable_triggers: spec.fast_load,
        reset_sequences: spec.reset_sequences,
        analyze: spec.analyze,
        stop_on_error: true,
        ..Default::default()
    };
    let resume = Resume { job_id: ctx.job_id.clone(), store: ctx.as_ref() };
    match copy::run_with(&src, &tgt, &jobs, s.server_version, &opts, cancel, &log, Some(&resume)).await {
        Ok(sum) if sum.tables_failed == 0 && jobs.iter().all(|j| j.progress.state() == state::DONE) => Outcome::Done(sum),
        Ok(_) => {
            let failed = jobs.iter().find(|j| j.progress.state() == state::FAILED);
            match failed {
                Some(j) => Outcome::Failed(classify(&j.progress.message().unwrap_or_default(), Some(&j.table))),
                None if cancel.is_cancelled() => Outcome::Stopped,
                None => Outcome::Failed(issue("Migration stopped", "Some tables didn't finish.", "Resume to continue.")),
            }
        }
        Err(mb_core::Error::Cancelled) => Outcome::Stopped,
        Err(e) => Outcome::Failed(classify(&e.to_string(), None)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_sqlstate() {
        let i = classify("duplicate key value [SQLSTATE 23505]\nDetail: x", None);
        assert_eq!(i.title, "Duplicate rows on target");
        let i = classify("connection failed: timed out", None);
        assert_eq!(i.title, "Can't reach the database");
        let i = classify("db error: connection reset by peer", None);
        assert_eq!(i.title, "Connection lost");
        assert_eq!(sqlstate("x [SQLSTATE 42P01]"), Some("42P01"));
    }
}
