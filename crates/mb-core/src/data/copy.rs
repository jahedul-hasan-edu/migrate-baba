//! Parallel table copy: COPY TO STDOUT → COPY FROM STDIN streaming between servers.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bytes::Bytes;
use futures_util::{SinkExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use tokio_postgres::Client;

use super::{binary_safe, col_list, copy_columns, state, TableProgress};
use crate::conn::{connect, simple_rows, ConnParams, SessionOptions, Side};
use crate::error::{Error, PgResultExt, Result};
use crate::ident::quote_literal;
use crate::model::{Catalog, QName};
use crate::progress::{cancellable, CancellationToken, Level, LogFn};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CopyMode {
    /// TRUNCATE target tables, then copy everything.
    #[default]
    Replace,
    /// Copy rows without touching existing ones.
    Append,
    /// Upsert changed rows and delete extra rows (needs a primary key; falls back to Replace).
    Sync,
}

impl CopyMode {
    pub fn label(self) -> &'static str {
        match self {
            CopyMode::Replace => "Replace (truncate + copy)",
            CopyMode::Append => "Append (insert only)",
            CopyMode::Sync => "Sync (upsert + delete)",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CopyOptions {
    pub mode: CopyMode,
    pub workers: usize,
    pub consistent_snapshot: bool,
    /// Use `session_replication_role = replica` when allowed (disables FK checks and triggers).
    pub disable_triggers: bool,
    pub reset_sequences: bool,
    pub analyze: bool,
    /// Tables bigger than this many 8 KiB pages are split into ctid ranges (PG14+ sources).
    pub split_threshold_pages: i64,
    pub stop_on_error: bool,
}

impl Default for CopyOptions {
    fn default() -> Self {
        Self {
            mode: CopyMode::Replace,
            workers: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 8),
            consistent_snapshot: true,
            disable_triggers: true,
            reset_sequences: true,
            analyze: true,
            split_threshold_pages: 8192,
            stop_on_error: false,
        }
    }
}

/// One table to copy.
#[derive(Debug, Clone)]
pub struct TableJob {
    pub table: QName,
    pub columns: Vec<String>,
    pub pk: Vec<String>,
    pub binary: bool,
    pub pages: i64,
    /// Selected tables this table references through foreign keys (target side).
    pub fk_deps: Vec<QName>,
    pub progress: Arc<TableProgress>,
}

#[derive(Debug, Clone, Default)]
pub struct CopySummary {
    pub tables_ok: usize,
    pub tables_failed: usize,
    pub rows: u64,
    pub bytes: u64,
    pub millis: u128,
    pub snapshot_used: bool,
    pub replica_role_used: bool,
    pub warnings: Vec<String>,
}

/// Build jobs for the selected tables; tables missing on either side are reported as warnings.
pub fn plan_jobs(source: &Catalog, target: &Catalog, tables: &[QName]) -> (Vec<TableJob>, Vec<String>) {
    let selected: BTreeSet<&QName> = tables.iter().collect();
    let mut jobs = Vec::new();
    let mut warnings = Vec::new();
    for t in tables {
        let (Some(s), Some(d)) = (source.tables.get(t), target.tables.get(t)) else {
            warnings.push(format!("{t}: table must exist on both sides (create it with Schema Compare first)"));
            continue;
        };
        if s.partition_key.is_some() {
            // Partitioned parents hold no rows; the partitions are copied individually.
            continue;
        }
        let (columns, w) = copy_columns(s, d);
        warnings.extend(w);
        if columns.is_empty() {
            warnings.push(format!("{t}: no common columns"));
            continue;
        }
        let binary = binary_safe(s, d, &columns);
        let fk_deps = d
            .foreign_key_targets()
            .filter(|r| *r != t && selected.contains(r))
            .cloned()
            .collect();
        jobs.push(TableJob {
            table: t.clone(),
            pk: s.primary_key_columns(),
            columns,
            binary,
            pages: s.pages,
            fk_deps,
            progress: Arc::new(TableProgress::new(t.clone(), s.est_rows, s.total_bytes)),
        });
    }
    (jobs, warnings)
}

/// FK dependency levels: level 0 has no deps among selected tables.
fn waves(jobs: &[TableJob]) -> Vec<Vec<usize>> {
    let idx: BTreeMap<&QName, usize> = jobs.iter().enumerate().map(|(i, j)| (&j.table, i)).collect();
    let mut level = vec![usize::MAX; jobs.len()];
    fn lvl(i: usize, jobs: &[TableJob], idx: &BTreeMap<&QName, usize>, level: &mut Vec<usize>, stack: &mut Vec<usize>) -> usize {
        if level[i] != usize::MAX {
            return level[i];
        }
        if stack.contains(&i) {
            return 0; // cycle: break it
        }
        stack.push(i);
        let mut l = 0;
        for d in &jobs[i].fk_deps {
            if let Some(&j) = idx.get(d) {
                l = l.max(lvl(j, jobs, idx, level, stack) + 1);
            }
        }
        stack.pop();
        level[i] = l;
        l
    }
    for i in 0..jobs.len() {
        lvl(i, jobs, &idx, &mut level, &mut Vec::new());
    }
    let max = level.iter().copied().max().unwrap_or(0);
    let mut out = vec![Vec::new(); max + 1];
    for (i, l) in level.into_iter().enumerate() {
        out[l].push(i);
    }
    out.retain(|w| !w.is_empty());
    out
}

#[derive(Debug, Clone)]
struct WorkItem {
    job: usize,
    /// ctid page range [lo, hi)
    range: Option<(i64, Option<i64>)>,
    weight: i64,
}

fn split_items(job_idx: usize, job: &TableJob, opts: &CopyOptions, src_version: i32) -> Vec<WorkItem> {
    let can_split = src_version >= 140000
        && opts.mode != CopyMode::Sync
        && opts.split_threshold_pages > 0
        && job.pages > opts.split_threshold_pages;
    if !can_split {
        return vec![WorkItem { job: job_idx, range: None, weight: job.pages }];
    }
    let parts = ((job.pages + opts.split_threshold_pages - 1) / opts.split_threshold_pages).min(opts.workers.max(1) as i64 * 2).max(2);
    let step = (job.pages + parts - 1) / parts;
    (0..parts)
        .map(|p| {
            let lo = p * step;
            let hi = if p == parts - 1 { None } else { Some((p + 1) * step) };
            WorkItem { job: job_idx, range: Some((lo, hi)), weight: step }
        })
        .collect()
}

/// Stream one COPY from `src` into `tgt`. Returns rows written.
pub(crate) async fn pipe_copy(
    src: &Client,
    tgt: &Client,
    out_sql: &str,
    in_sql: &str,
    progress: &TableProgress,
    cancel: &CancellationToken,
) -> Result<u64> {
    let fut = async {
        let sink = tgt.copy_in::<_, Bytes>(in_sql).await.sql(in_sql)?;
        let stream = src.copy_out(out_sql).await.sql(out_sql)?;
        futures_util::pin_mut!(sink);
        futures_util::pin_mut!(stream);
        let mut counted = stream.as_mut().inspect_ok(|b| {
            progress.bytes.fetch_add(b.len() as u64, Ordering::Relaxed);
        });
        sink.as_mut().send_all(&mut counted).await.map_err(|e| Error::from_pg(e, Some(out_sql)))?;
        sink.as_mut().finish().await.sql(in_sql)
    };
    cancellable(cancel, fut).await
}

pub(crate) fn copy_sql(table: &QName, cols: &[String], binary: bool, filter: Option<&str>) -> (String, String) {
    let fmt = if binary { " (FORMAT binary)" } else { "" };
    let cl = col_list(cols);
    let out = format!(
        "COPY (SELECT {cl} FROM ONLY {}{}) TO STDOUT{fmt}",
        table.sql(),
        filter.map(|f| format!(" WHERE {f}")).unwrap_or_default()
    );
    let inn = format!("COPY {} ({cl}) FROM STDIN{fmt}", table.sql());
    (out, inn)
}

struct Shared<'a> {
    jobs: &'a [TableJob],
    opts: &'a CopyOptions,
    snapshot: Option<String>,
    cancel: CancellationToken,
    log: &'a LogFn,
    queue: Mutex<VecDeque<WorkItem>>,
}

async fn run_item(src: &Client, tgt: &Client, item: &WorkItem, sh: &Shared<'_>) -> Result<u64> {
    let job = &sh.jobs[item.job];
    let mut begin = "BEGIN ISOLATION LEVEL REPEATABLE READ, READ ONLY;".to_string();
    if let Some(s) = &sh.snapshot {
        begin.push_str(&format!(" SET TRANSACTION SNAPSHOT {};", quote_literal(s)));
    }
    src.batch_execute(&begin).await.sql(&begin)?;
    let result = async {
        if sh.opts.mode == CopyMode::Sync && !job.pk.is_empty() {
            let c = super::sync::sync_table(src, tgt, job, None, &sh.cancel).await?;
            Ok(c.inserted + c.updated)
        } else {
            tgt.batch_execute("BEGIN").await.sql("BEGIN")?;
            let filter = item.range.map(|(lo, hi)| match hi {
                Some(hi) => format!("ctid >= '({lo},0)'::tid AND ctid < '({hi},0)'::tid"),
                None => format!("ctid >= '({lo},0)'::tid"),
            });
            let (out_sql, in_sql) = copy_sql(&job.table, &job.columns, job.binary, filter.as_deref());
            match pipe_copy(src, tgt, &out_sql, &in_sql, &job.progress, &sh.cancel).await {
                Ok(rows) => {
                    tgt.batch_execute("COMMIT").await.sql("COMMIT")?;
                    Ok(rows)
                }
                Err(e) => {
                    let _ = tgt.batch_execute("ROLLBACK").await;
                    Err(e)
                }
            }
        }
    }
    .await;
    let _ = src.batch_execute("COMMIT").await;
    result
}

/// Run the copy job. Progress is reported through each job's `TableProgress`.
pub async fn run(
    source: &ConnParams,
    target: &ConnParams,
    jobs: &[TableJob],
    src_version: i32,
    opts: &CopyOptions,
    cancel: &CancellationToken,
    log: &LogFn,
) -> Result<CopySummary> {
    let t0 = Instant::now();
    let mut summary = CopySummary::default();
    if jobs.is_empty() {
        return Ok(summary);
    }
    let run_cancel = cancel.child_token();

    // Snapshot coordinator: keeps the exporting transaction open for the whole run.
    let mut coordinator = None;
    let mut snapshot = None;
    if opts.consistent_snapshot {
        let c = connect(source, &SessionOptions::data(Side::Source)).await?;
        let res = async {
            c.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ, READ ONLY").await.sql("BEGIN")?;
            let rows = simple_rows(&c, "SELECT pg_export_snapshot() AS s").await?;
            Ok::<_, Error>(rows.first().and_then(|r| r.get("s")).map(str::to_string))
        }
        .await;
        match res {
            Ok(Some(s)) => {
                log(Level::Info, format!("using exported snapshot {s}"));
                snapshot = Some(s);
                coordinator = Some(c);
                summary.snapshot_used = true;
            }
            Ok(None) | Err(_) => {
                let w = "could not export a snapshot (standby or pooler?) — tables are copied from independent snapshots".to_string();
                log(Level::Warn, w.clone());
                summary.warnings.push(w);
            }
        }
    }

    // Target admin connection: permissions check, truncate, post-processing.
    let admin = connect(target, &SessionOptions::data(Side::Target)).await?;
    let replica = opts.disable_triggers
        && admin.batch_execute("SET session_replication_role = replica").await.is_ok();
    summary.replica_role_used = replica;
    if opts.disable_triggers && !replica {
        let w = "not allowed to set session_replication_role (needs superuser); loading tables in foreign-key order with triggers enabled".to_string();
        log(Level::Warn, w.clone());
        summary.warnings.push(w);
    }

    let truncate: Vec<String> = jobs
        .iter()
        .filter(|j| opts.mode == CopyMode::Replace || (opts.mode == CopyMode::Sync && j.pk.is_empty()))
        .map(|j| j.table.sql())
        .collect();
    if !truncate.is_empty() {
        let sql = format!("TRUNCATE {}", truncate.join(", "));
        log(Level::Info, format!("truncating {} table(s)", truncate.len()));
        admin.batch_execute(&sql).await.sql(&sql)?;
    }

    // Work items, grouped into FK waves unless FK checks are disabled.
    let wave_list = if replica { vec![(0..jobs.len()).collect::<Vec<_>>()] } else { waves(jobs) };
    let workers_n = opts.workers.max(1);

    let shared = Shared {
        jobs,
        opts,
        snapshot,
        cancel: run_cancel.clone(),
        log,
        queue: Mutex::new(VecDeque::new()),
    };

    // Open worker connection pairs once.
    let total_items: usize = jobs.iter().enumerate().map(|(i, j)| split_items(i, j, opts, src_version).len()).sum();
    let n = workers_n.min(total_items).max(1);
    log(Level::Info, format!("opening {n} worker connection pair(s)"));
    let mut opening = Vec::with_capacity(n);
    for _ in 0..n {
        opening.push(open_worker_pair(source, target, replica));
    }
    let pairs: Vec<(Client, Client)> = futures_util::future::try_join_all(opening).await?;

    for (wi, wave) in wave_list.iter().enumerate() {
        if run_cancel.is_cancelled() {
            break;
        }
        let mut items: Vec<WorkItem> = Vec::new();
        for &ji in wave {
            let its = split_items(ji, &jobs[ji], opts, src_version);
            jobs[ji].progress.parts_total.store(its.len() as u32, Ordering::Relaxed);
            items.extend(its);
        }
        items.sort_by_key(|i| std::cmp::Reverse(i.weight)); // LPT scheduling
        if wave_list.len() > 1 {
            log(Level::Info, format!("wave {}/{}: {} item(s)", wi + 1, wave_list.len(), items.len()));
        }
        *shared.queue.lock().unwrap() = items.into();
        // Workers borrow the long-lived connections for this wave.
        let mut workers = Vec::with_capacity(pairs.len());
        for (s, t) in &pairs {
            workers.push(worker_ref(s, t, &shared));
        }
        futures_util::future::join_all(workers).await;
    }
    drop(pairs);

    // Anything never started is cancelled.
    for j in jobs {
        if matches!(j.progress.state(), state::QUEUED | state::RUNNING) {
            j.progress.finish(state::CANCELLED, Some("not run".into()));
        }
    }

    let copied: Vec<&TableJob> = jobs.iter().filter(|j| j.progress.state() == state::DONE).collect();
    if !run_cancel.is_cancelled() && !copied.is_empty() {
        if opts.reset_sequences {
            if let Err(e) = reset_sequences(&admin, &copied).await {
                let w = format!("sequence reset failed: {e}");
                log(Level::Warn, w.clone());
                summary.warnings.push(w);
            }
        }
        if opts.analyze {
            let sql = format!("ANALYZE {}", copied.iter().map(|j| j.table.sql()).collect::<Vec<_>>().join(", "));
            if let Err(e) = admin.batch_execute(&sql).await {
                log(Level::Warn, format!("ANALYZE failed: {e}"));
            } else {
                log(Level::Info, format!("analyzed {} table(s)", copied.len()));
            }
        }
    }
    if let Some(c) = coordinator {
        let _ = c.batch_execute("COMMIT").await;
    }

    for j in jobs {
        summary.rows += j.progress.rows.load(Ordering::Relaxed);
        summary.bytes += j.progress.bytes.load(Ordering::Relaxed);
        match j.progress.state() {
            state::DONE => summary.tables_ok += 1,
            state::FAILED => summary.tables_failed += 1,
            _ => {}
        }
    }
    summary.millis = t0.elapsed().as_millis();
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    Ok(summary)
}

async fn open_worker_pair(source: &ConnParams, target: &ConnParams, replica: bool) -> Result<(Client, Client)> {
    let (so, to) = (SessionOptions::data(Side::Source), SessionOptions::data(Side::Target));
    let (s, t) = tokio::try_join!(connect(source, &so), connect(target, &to))?;
    if replica {
        t.batch_execute("SET session_replication_role = replica").await.sql("SET session_replication_role")?;
    }
    Ok((s, t))
}

async fn worker_ref(src: &Client, tgt: &Client, sh: &Shared<'_>) {
    // Same as `worker` but borrowing connections so they can be reused across waves.
    loop {
        if sh.cancel.is_cancelled() {
            return;
        }
        let Some(item) = sh.queue.lock().unwrap().pop_front() else { return };
        let job = &sh.jobs[item.job];
        let p = &job.progress;
        if p.state() == state::FAILED {
            continue;
        }
        p.mark_running();
        match run_item(src, tgt, &item, sh).await {
            Ok(rows) => {
                p.rows.fetch_add(rows, Ordering::Relaxed);
                let done = p.parts_done.fetch_add(1, Ordering::Relaxed) + 1;
                if done >= p.parts_total.load(Ordering::Relaxed) && p.state() == state::RUNNING {
                    p.finish(state::DONE, None);
                    (sh.log)(
                        Level::Info,
                        format!(
                            "{}: {} rows in {:.1}s",
                            job.table,
                            p.rows.load(Ordering::Relaxed),
                            p.elapsed().as_secs_f64()
                        ),
                    );
                }
            }
            Err(Error::Cancelled) => {
                p.finish(state::CANCELLED, Some("cancelled".into()));
                return;
            }
            Err(e) => {
                let msg = e.to_string();
                (sh.log)(Level::Error, format!("{}: {msg}", job.table));
                p.finish(state::FAILED, Some(msg));
                if sh.opts.stop_on_error {
                    sh.cancel.cancel();
                }
            }
        }
    }
}

/// `setval` every sequence owned by (or identity of) a column of the copied tables.
async fn reset_sequences(admin: &Client, copied: &[&TableJob]) -> Result<()> {
    let wanted: BTreeSet<&QName> = copied.iter().map(|j| &j.table).collect();
    let rows = simple_rows(
        admin,
        "SELECT n.nspname AS ts, c.relname AS tn, a.attname AS col, sn.nspname AS ss, s.relname AS sname \
         FROM pg_depend d JOIN pg_class s ON s.oid = d.objid AND s.relkind = 'S' \
         JOIN pg_namespace sn ON sn.oid = s.relnamespace \
         JOIN pg_class c ON c.oid = d.refobjid JOIN pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum = d.refobjsubid \
         WHERE d.classid = 'pg_class'::regclass AND d.refclassid = 'pg_class'::regclass AND d.deptype IN ('a', 'i')",
    )
    .await?;
    let mut sql = String::new();
    for r in rows {
        let t = QName::new(r.get("ts").unwrap_or_default(), r.get("tn").unwrap_or_default());
        if !wanted.contains(&t) {
            continue;
        }
        let seq = QName::new(r.get("ss").unwrap_or_default(), r.get("sname").unwrap_or_default());
        let col = crate::ident::quote_ident(r.get("col").unwrap_or_default());
        sql.push_str(&format!(
            "SELECT setval({}, COALESCE((SELECT max({col}) FROM {}), 1), (SELECT max({col}) FROM {}) IS NOT NULL);\n",
            quote_literal(&seq.sql()),
            t.sql(),
            t.sql()
        ));
    }
    if !sql.is_empty() {
        admin.batch_execute(&sql).await.sql(&sql)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(name: &str, deps: &[&str]) -> TableJob {
        TableJob {
            table: QName::new("public", name),
            columns: vec!["id".into()],
            pk: vec!["id".into()],
            binary: true,
            pages: 1,
            fk_deps: deps.iter().map(|d| QName::new("public", *d)).collect(),
            progress: Arc::new(TableProgress::new(QName::new("public", name), 0, 0)),
        }
    }

    #[test]
    fn fk_waves() {
        let jobs = vec![job("orders", &["users"]), job("users", &[]), job("items", &["orders", "users"])];
        let w = waves(&jobs);
        assert_eq!(w, vec![vec![1], vec![0], vec![2]]);
    }

    #[test]
    fn fk_cycle_terminates() {
        let jobs = vec![job("a", &["b"]), job("b", &["a"])];
        let w = waves(&jobs);
        assert_eq!(w.iter().map(|x| x.len()).sum::<usize>(), 2);
    }

    #[test]
    fn splits_large_tables() {
        let mut j = job("big", &[]);
        j.pages = 100_000;
        let opts = CopyOptions { workers: 4, split_threshold_pages: 10_000, ..Default::default() };
        let items = split_items(0, &j, &opts, 150000);
        assert_eq!(items.len(), 8);
        assert_eq!(items.last().unwrap().range.unwrap().1, None);
        assert_eq!(split_items(0, &j, &opts, 130000).len(), 1);
    }
}
