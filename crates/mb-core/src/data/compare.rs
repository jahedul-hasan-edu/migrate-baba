//! Data compare: key-range chunks hashed server-side, row-level drill-down for mismatches.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Instant;

use futures_util::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use tokio_postgres::Client;

use crate::conn::{connect, simple_rows, ConnParams, SessionOptions, Side};
use crate::error::{Error, Result};
use crate::ident::{quote_ident, quote_literal};
use crate::model::{Catalog, QName, TableDef};
use crate::progress::{cancellable, CancellationToken};

/// A primary-key range `[lo, hi)`; bounds are SQL row literals like `('42'::integer)`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyRange {
    pub lo: Option<String>,
    pub hi: Option<String>,
}

impl KeyRange {
    pub fn predicate(&self, pk: &[String], alias: Option<&str>) -> String {
        let cols = pk
            .iter()
            .map(|c| match alias {
                Some(a) => format!("{a}.{}", quote_ident(c)),
                None => quote_ident(c),
            })
            .collect::<Vec<_>>()
            .join(", ");
        let mut parts = Vec::new();
        if let Some(lo) = &self.lo {
            parts.push(format!("({cols}) >= {lo}"));
        }
        if let Some(hi) = &self.hi {
            parts.push(format!("({cols}) < {hi}"));
        }
        if parts.is_empty() {
            "true".into()
        } else {
            parts.join(" AND ")
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompareOptions {
    /// Rows per chunk.
    pub chunk_size: u64,
    /// Sample keys kept per category.
    pub sample_limit: usize,
    /// Tables compared concurrently (each uses one connection per side).
    pub parallel: usize,
    /// Chunk hash queries in flight per connection.
    pub pipeline_depth: usize,
}

impl Default for CompareOptions {
    fn default() -> Self {
        Self { chunk_size: 10_000, sample_limit: 100, parallel: 4, pipeline_depth: 8 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompareStatus {
    Identical,
    Different,
    OnlyInSource,
    OnlyInTarget,
    Skipped,
    Error,
}

impl CompareStatus {
    pub fn label(self) -> &'static str {
        match self {
            CompareStatus::Identical => "identical",
            CompareStatus::Different => "different",
            CompareStatus::OnlyInSource => "only in source",
            CompareStatus::OnlyInTarget => "only in target",
            CompareStatus::Skipped => "skipped",
            CompareStatus::Error => "error",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TableCompare {
    pub table: QName,
    pub status: CompareStatus,
    pub has_pk: bool,
    pub source_rows: u64,
    pub target_rows: u64,
    pub chunks: usize,
    pub chunks_different: usize,
    pub only_source: u64,
    pub only_target: u64,
    pub different: u64,
    pub sample_only_source: Vec<String>,
    pub sample_only_target: Vec<String>,
    pub sample_different: Vec<String>,
    /// Key ranges containing differences (used by sync).
    pub diff_ranges: Vec<KeyRange>,
    pub message: Option<String>,
    pub millis: u128,
}

impl TableCompare {
    fn new(table: QName, status: CompareStatus) -> Self {
        Self {
            table,
            status,
            has_pk: false,
            source_rows: 0,
            target_rows: 0,
            chunks: 0,
            chunks_different: 0,
            only_source: 0,
            only_target: 0,
            different: 0,
            sample_only_source: Vec::new(),
            sample_only_target: Vec::new(),
            sample_different: Vec::new(),
            diff_ranges: Vec::new(),
            message: None,
            millis: 0,
        }
    }
}

/// Columns present on both sides (source order).
pub fn common_columns(s: &TableDef, t: &TableDef) -> Vec<String> {
    s.columns.iter().filter(|c| t.column(&c.name).is_some()).map(|c| c.name.clone()).collect()
}

/// Order-independent 64-bit-per-row hash sum of the rows matching `pred`.
fn hash_sql(table: &QName, cols: &[String], pred: &str) -> String {
    let row = cols.iter().map(|c| quote_ident(c)).collect::<Vec<_>>().join(", ");
    format!(
        "SELECT count(*) AS n, coalesce(sum(('x' || left(md5(ROW({row})::text), 16))::bit(64)::bigint::numeric), 0)::text AS h \
         FROM ONLY {} WHERE {pred}",
        table.sql()
    )
}

async fn hash_range(c: &Client, sql: String) -> Result<(u64, String)> {
    let rows = simple_rows(c, &sql).await?;
    let r = rows.first().ok_or_else(|| Error::Invalid("empty hash result".into()))?;
    Ok((r.get("n").and_then(|v| v.parse().ok()).unwrap_or(0), r.get("h").unwrap_or_default().to_string()))
}

/// Run all chunk hash queries on one connection, `depth` in flight (pipelined).
async fn hash_all(c: &Client, sqls: &[String], depth: usize) -> Result<Vec<(u64, String)>> {
    let mut futs = Vec::with_capacity(sqls.len());
    for q in sqls {
        futs.push(hash_range(c, q.clone()));
    }
    futures_util::stream::iter(futs).buffered(depth).try_collect::<Vec<_>>().await
}

async fn boundaries(c: &Client, s: &TableDef, pk: &[String], chunk: u64) -> Result<Vec<String>> {
    let lit = pk
        .iter()
        .map(|col| {
            let ty = s.column(col).map(|c| c.data_type.clone()).unwrap_or_else(|| "text".into());
            format!("format('%L', {}) || {}", quote_ident(col), quote_literal(&format!("::{ty}")))
        })
        .collect::<Vec<_>>()
        .join(" || ', ' || ");
    let pkl = pk.iter().map(|c| quote_ident(c)).collect::<Vec<_>>().join(", ");
    let sql = format!(
        "SELECT '(' || {lit} || ')' AS b FROM (SELECT {pkl}, row_number() OVER (ORDER BY {pkl}) AS rn FROM ONLY {}) s \
         WHERE rn % {chunk} = 0 ORDER BY rn",
        s.name.sql()
    );
    Ok(simple_rows(c, &sql).await?.iter().filter_map(|r| r.get("b").map(str::to_string)).collect())
}

async fn row_hashes(c: &Client, table: &QName, pk: &[String], cols: &[String], pred: &str) -> Result<HashMap<String, String>> {
    let pkl = pk.iter().map(|c| quote_ident(c)).collect::<Vec<_>>().join(", ");
    let row = cols.iter().map(|c| quote_ident(c)).collect::<Vec<_>>().join(", ");
    let sql = format!("SELECT ROW({pkl})::text AS k, md5(ROW({row})::text) AS h FROM ONLY {} WHERE {pred}", table.sql());
    Ok(simple_rows(c, &sql)
        .await?
        .into_iter()
        .map(|r| (r.get("k").unwrap_or_default().to_string(), r.get("h").unwrap_or_default().to_string()))
        .collect())
}

async fn sample_keys(c: &Client, table: &QName, pk: &[String], pred: &str, limit: usize) -> Result<Vec<String>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let pkl = pk.iter().map(|c| quote_ident(c)).collect::<Vec<_>>().join(", ");
    let sql = format!("SELECT ROW({pkl})::text AS k FROM ONLY {} WHERE {pred} LIMIT {limit}", table.sql());
    Ok(simple_rows(c, &sql).await?.iter().filter_map(|r| r.get("k").map(str::to_string)).collect())
}

/// Compare one table present on both sides.
pub async fn compare_table(src: &Client, tgt: &Client, s: &TableDef, t: &TableDef, opts: &CompareOptions) -> Result<TableCompare> {
    let t0 = Instant::now();
    let mut res = TableCompare::new(s.name.clone(), CompareStatus::Identical);
    let cols = common_columns(s, t);
    let pk: Vec<String> = s.primary_key_columns().into_iter().filter(|c| t.column(c).is_some()).collect();
    res.has_pk = !pk.is_empty() && pk.len() == s.primary_key_columns().len();

    if !res.has_pk {
        let sql = hash_sql(&s.name, &cols, "true");
        let ((sn, sh), (tn, th)) = tokio::try_join!(hash_range(src, sql.clone()), hash_range(tgt, sql))?;
        res.source_rows = sn;
        res.target_rows = tn;
        res.chunks = 1;
        if sn != tn || sh != th {
            res.status = CompareStatus::Different;
            res.chunks_different = 1;
            res.diff_ranges.push(KeyRange::default());
            res.message = Some("no primary key: only whole-table comparison is possible".into());
        }
        res.millis = t0.elapsed().as_millis();
        return Ok(res);
    }

    let bounds = boundaries(src, s, &pk, opts.chunk_size.max(1)).await?;
    let mut ranges = Vec::with_capacity(bounds.len() + 1);
    let mut lo = None;
    for b in bounds {
        ranges.push(KeyRange { lo: lo.take(), hi: Some(b.clone()) });
        lo = Some(b);
    }
    ranges.push(KeyRange { lo, hi: None });
    res.chunks = ranges.len();

    let depth = opts.pipeline_depth.max(1);
    let sqls: Vec<String> = ranges.iter().map(|r| hash_sql(&s.name, &cols, &r.predicate(&pk, None))).collect();
    let (sh, th) = tokio::try_join!(hash_all(src, &sqls, depth), hash_all(tgt, &sqls, depth))?;

    for (i, r) in ranges.iter().enumerate() {
        let (sn, shh) = &sh[i];
        let (tn, thh) = &th[i];
        res.source_rows += sn;
        res.target_rows += tn;
        if sn == tn && shh == thh {
            continue;
        }
        let pred = r.predicate(&pk, None);
        let lim = opts.sample_limit;
        if *tn == 0 {
            res.only_source += sn;
            let need = lim.saturating_sub(res.sample_only_source.len());
            res.sample_only_source.extend(sample_keys(src, &s.name, &pk, &pred, need).await?);
        } else if *sn == 0 {
            res.only_target += tn;
            let need = lim.saturating_sub(res.sample_only_target.len());
            res.sample_only_target.extend(sample_keys(tgt, &s.name, &pk, &pred, need).await?);
        } else {
            let (a, b) = tokio::try_join!(
                row_hashes(src, &s.name, &pk, &cols, &pred),
                row_hashes(tgt, &s.name, &pk, &cols, &pred)
            )?;
            let mut any = false;
            for (k, h) in &a {
                match b.get(k) {
                    None => {
                        res.only_source += 1;
                        any = true;
                        if res.sample_only_source.len() < lim {
                            res.sample_only_source.push(k.clone());
                        }
                    }
                    Some(h2) if h2 != h => {
                        res.different += 1;
                        any = true;
                        if res.sample_different.len() < lim {
                            res.sample_different.push(k.clone());
                        }
                    }
                    _ => {}
                }
            }
            for k in b.keys() {
                if !a.contains_key(k) {
                    res.only_target += 1;
                    any = true;
                    if res.sample_only_target.len() < lim {
                        res.sample_only_target.push(k.clone());
                    }
                }
            }
            if !any {
                continue; // hash collision-free mismatch can't happen; differing collations can.
            }
        }
        res.chunks_different += 1;
        res.diff_ranges.push(r.clone());
    }
    if res.chunks_different > 0 {
        res.status = CompareStatus::Different;
    }
    res.millis = t0.elapsed().as_millis();
    Ok(res)
}

async fn open_pair(source: &ConnParams, target: &ConnParams) -> Result<(Client, Client)> {
    let (so, to) = (SessionOptions::data(Side::Source), SessionOptions::data(Side::Target));
    let (s, t) = tokio::try_join!(connect(source, &so), connect(target, &to))?;
    Ok((s, t))
}

type TableQueue<'a> = Mutex<VecDeque<(&'a TableDef, &'a TableDef)>>;

async fn compare_worker(
    sc: &Client,
    tc: &Client,
    queue: &TableQueue<'_>,
    opts: &CompareOptions,
    cancel: &CancellationToken,
    on_start: &(dyn Fn(QName) + Send + Sync),
    on_result: &(dyn Fn(TableCompare) + Send + Sync),
) {
    loop {
        let next = queue.lock().unwrap().pop_front();
        let Some((s, d)) = next else { break };
        on_start(s.name.clone());
        // Repeatable-read so both chunk passes and drill-downs see one snapshot per side.
        let _ = sc.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ, READ ONLY").await;
        let _ = tc.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ, READ ONLY").await;
        let r = cancellable(cancel, compare_table(sc, tc, s, d, opts)).await;
        let _ = sc.batch_execute("COMMIT").await;
        let _ = tc.batch_execute("COMMIT").await;
        match r {
            Ok(r) => on_result(r),
            Err(Error::Cancelled) => break,
            Err(e) => {
                let mut r = TableCompare::new(s.name.clone(), CompareStatus::Error);
                r.message = Some(e.to_string());
                on_result(r);
            }
        }
    }
}

/// Compare many tables with `opts.parallel` connection pairs; results are streamed to `on_result`.
pub async fn compare_tables(
    source: &ConnParams,
    target: &ConnParams,
    src_cat: &Catalog,
    tgt_cat: &Catalog,
    tables: &[QName],
    opts: &CompareOptions,
    cancel: &CancellationToken,
    on_start: &(dyn Fn(QName) + Send + Sync),
    on_result: &(dyn Fn(TableCompare) + Send + Sync),
) -> Result<()> {
    let mut queue = VecDeque::new();
    for t in tables {
        match (src_cat.tables.get(t), tgt_cat.tables.get(t)) {
            (Some(s), Some(d)) => {
                if s.partition_key.is_some() {
                    let mut r = TableCompare::new(t.clone(), CompareStatus::Skipped);
                    r.message = Some("partitioned parent: partitions are compared individually".into());
                    on_result(r);
                } else {
                    queue.push_back((s, d));
                }
            }
            (Some(s), None) => {
                let mut r = TableCompare::new(t.clone(), CompareStatus::OnlyInSource);
                r.source_rows = s.est_rows.max(0) as u64;
                r.message = Some("table missing on target (row count estimated)".into());
                on_result(r);
            }
            (None, Some(d)) => {
                let mut r = TableCompare::new(t.clone(), CompareStatus::OnlyInTarget);
                r.target_rows = d.est_rows.max(0) as u64;
                r.message = Some("table missing on source (row count estimated)".into());
                on_result(r);
            }
            (None, None) => {}
        }
    }
    if queue.is_empty() {
        return Ok(());
    }
    // Largest first.
    let mut v: Vec<_> = queue.into_iter().collect();
    v.sort_by_key(|(s, _)| std::cmp::Reverse(s.total_bytes));
    let queue = Mutex::new(VecDeque::from(v));
    let n = opts.parallel.max(1).min(queue.lock().unwrap().len());

    let mut conns = Vec::with_capacity(n);
    for _ in 0..n {
        conns.push(open_pair(source, target).await?);
    }
    let mut workers = Vec::with_capacity(n);
    for (sc, tc) in &conns {
        workers.push(compare_worker(sc, tc, &queue, opts, cancel, on_start, on_result));
    }
    futures_util::future::join_all(workers).await;
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_predicates() {
        let pk = vec!["id".to_string(), "Org".to_string()];
        let r = KeyRange { lo: Some("('1'::integer, 'a'::text)".into()), hi: None };
        assert_eq!(r.predicate(&pk, Some("_t")), "(_t.id, _t.\"Org\") >= ('1'::integer, 'a'::text)");
        assert_eq!(KeyRange::default().predicate(&pk, None), "true");
    }
}
