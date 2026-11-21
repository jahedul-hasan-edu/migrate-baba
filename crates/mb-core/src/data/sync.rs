//! Staging-table sync: copy source rows of a key range into a temp table on the target, then
//! delete missing rows and upsert changed ones — all in one target transaction.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use tokio_postgres::{Client, SimpleQueryMessage};

use super::compare::KeyRange;
use super::copy::{copy_sql, pipe_copy, TableJob};
use super::{binary_safe, col_list, copy_columns, state, TableProgress};
use crate::conn::{connect, simple_rows, ConnParams, SessionOptions, Side};
use crate::error::{Error, PgResultExt, Result};
use crate::ident::quote_ident;
use crate::model::{Catalog, QName};
use crate::progress::{CancellationToken, Level, LogFn};

#[derive(Clone, Copy, Debug, Default)]
pub struct SyncCounts {
    pub inserted: u64,
    pub updated: u64,
    pub deleted: u64,
}

async fn exec_count(c: &Client, sql: &str) -> Result<u64> {
    let msgs = c.simple_query(sql).await.sql(sql)?;
    Ok(msgs
        .iter()
        .filter_map(|m| match m {
            SimpleQueryMessage::CommandComplete(n) => Some(*n),
            _ => None,
        })
        .sum())
}

/// Sync one table (target must already be outside a transaction; the source connection should
/// already be inside the read transaction/snapshot it wants to read from).
pub async fn sync_table(
    src: &Client,
    tgt: &Client,
    job: &TableJob,
    ranges: Option<&[KeyRange]>,
    cancel: &CancellationToken,
) -> Result<SyncCounts> {
    if job.pk.is_empty() {
        return Err(Error::Unsupported(format!("{}: sync requires a primary key", job.table)));
    }
    let t = job.table.sql();
    let cols = col_list(&job.columns);
    let pk = col_list(&job.pk);
    let all = [KeyRange::default()];
    let ranges = ranges.unwrap_or(&all);
    let mut counts = SyncCounts::default();

    tgt.batch_execute("BEGIN").await.sql("BEGIN")?;
    let res = async {
        let stage = format!("CREATE TEMP TABLE _mb_stage ON COMMIT DROP AS SELECT {cols} FROM {t} WITH NO DATA");
        tgt.batch_execute(&stage).await.sql(&stage)?;

        let non_pk: Vec<&String> = job.columns.iter().filter(|c| !job.pk.contains(c)).collect();
        let pk_s = job.pk.iter().map(|c| format!("_s.{}", quote_ident(c))).collect::<Vec<_>>().join(", ");
        let pk_t = job.pk.iter().map(|c| format!("_t.{}", quote_ident(c))).collect::<Vec<_>>().join(", ");
        let on_conflict = if non_pk.is_empty() {
            "DO NOTHING".to_string()
        } else {
            let set = non_pk.iter().map(|c| format!("{0} = EXCLUDED.{0}", quote_ident(c))).collect::<Vec<_>>().join(", ");
            let tv = non_pk.iter().map(|c| format!("_t.{}", quote_ident(c))).collect::<Vec<_>>().join(", ");
            let ev = non_pk.iter().map(|c| format!("EXCLUDED.{}", quote_ident(c))).collect::<Vec<_>>().join(", ");
            format!("DO UPDATE SET {set} WHERE ROW({tv})::text IS DISTINCT FROM ROW({ev})::text")
        };

        for r in ranges {
            let src_pred = r.predicate(&job.pk, None);
            let filter = if src_pred == "true" { None } else { Some(src_pred.as_str()) };
            let (out_sql, _) = copy_sql(&job.table, &job.columns, job.binary, filter);
            let fmt = if job.binary { " (FORMAT binary)" } else { "" };
            let in_sql = format!("COPY _mb_stage ({cols}) FROM STDIN{fmt}");
            pipe_copy(src, tgt, &out_sql, &in_sql, &job.progress, cancel).await?;

            let tpred = r.predicate(&job.pk, Some("_t"));
            let del = format!(
                "DELETE FROM {t} AS _t WHERE {tpred} AND NOT EXISTS (SELECT 1 FROM _mb_stage _s WHERE ({pk_s}) = ({pk_t}))"
            );
            counts.deleted += exec_count(tgt, &del).await?;

            let up = format!(
                "WITH up AS (INSERT INTO {t} AS _t ({cols}) OVERRIDING SYSTEM VALUE SELECT {cols} FROM _mb_stage \
                 ON CONFLICT ({pk}) {on_conflict} RETURNING (xmax = 0) AS ins) \
                 SELECT count(*) FILTER (WHERE ins) AS i, count(*) FILTER (WHERE NOT ins) AS u FROM up"
            );
            let rows = simple_rows(tgt, &up).await?;
            if let Some(row) = rows.first() {
                counts.inserted += row.get("i").and_then(|v| v.parse().ok()).unwrap_or(0);
                counts.updated += row.get("u").and_then(|v| v.parse().ok()).unwrap_or(0);
            }
            tgt.batch_execute("TRUNCATE _mb_stage").await.sql("TRUNCATE _mb_stage")?;
        }
        Ok::<_, Error>(())
    }
    .await;
    match res {
        Ok(()) => {
            tgt.batch_execute("COMMIT").await.sql("COMMIT")?;
            job.progress.inserted.fetch_add(counts.inserted, Ordering::Relaxed);
            job.progress.updated.fetch_add(counts.updated, Ordering::Relaxed);
            job.progress.deleted.fetch_add(counts.deleted, Ordering::Relaxed);
            Ok(counts)
        }
        Err(e) => {
            let _ = tgt.batch_execute("ROLLBACK").await;
            Err(e)
        }
    }
}

/// A table to sync after a data compare.
#[derive(Clone, Debug)]
pub struct SyncRequest {
    pub table: QName,
    /// Ranges to sync; empty = whole table.
    pub ranges: Vec<KeyRange>,
}

/// Sync tables found different by a data compare. Tables without a PK are replaced.
pub async fn sync_tables(
    source: &ConnParams,
    target: &ConnParams,
    src_cat: &Catalog,
    tgt_cat: &Catalog,
    requests: &[SyncRequest],
    progress: &[Arc<TableProgress>],
    cancel: &CancellationToken,
    log: &LogFn,
) -> Result<()> {
    let src = connect(source, &SessionOptions::data(Side::Source)).await?;
    let tgt = connect(target, &SessionOptions::data(Side::Target)).await?;
    if tgt.batch_execute("SET session_replication_role = replica").await.is_err() {
        log(Level::Warn, "session_replication_role not permitted: FK checks and triggers stay active during sync".into());
    }
    for (req, p) in requests.iter().zip(progress) {
        if cancel.is_cancelled() {
            p.finish(state::CANCELLED, Some("cancelled".into()));
            continue;
        }
        let (Some(s), Some(d)) = (src_cat.tables.get(&req.table), tgt_cat.tables.get(&req.table)) else {
            p.finish(state::SKIPPED, Some("table missing on one side".into()));
            continue;
        };
        let (columns, _) = copy_columns(s, d);
        let job = TableJob {
            table: req.table.clone(),
            binary: binary_safe(s, d, &columns),
            columns,
            pk: s.primary_key_columns(),
            pages: s.pages,
            fk_deps: Vec::new(),
            progress: p.clone(),
        };
        p.mark_running();
        src.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ, READ ONLY").await.sql("BEGIN")?;
        let r = if job.pk.is_empty() {
            // Replace the whole table.
            async {
                tgt.batch_execute("BEGIN").await.sql("BEGIN")?;
                let trunc = format!("TRUNCATE {}", job.table.sql());
                tgt.batch_execute(&trunc).await.sql(&trunc)?;
                let (o, i) = copy_sql(&job.table, &job.columns, job.binary, None);
                match pipe_copy(&src, &tgt, &o, &i, p, cancel).await {
                    Ok(n) => {
                        tgt.batch_execute("COMMIT").await.sql("COMMIT")?;
                        p.inserted.fetch_add(n, Ordering::Relaxed);
                        Ok(SyncCounts { inserted: n, ..Default::default() })
                    }
                    Err(e) => {
                        let _ = tgt.batch_execute("ROLLBACK").await;
                        Err(e)
                    }
                }
            }
            .await
        } else {
            let ranges = if req.ranges.is_empty() { None } else { Some(req.ranges.as_slice()) };
            sync_table(&src, &tgt, &job, ranges, cancel).await
        };
        let _ = src.batch_execute("COMMIT").await;
        match r {
            Ok(c) => {
                p.rows.store(c.inserted + c.updated + c.deleted, Ordering::Relaxed);
                p.finish(state::DONE, None);
                log(
                    Level::Info,
                    format!("{}: +{} inserted, ~{} updated, -{} deleted", req.table, c.inserted, c.updated, c.deleted),
                );
            }
            Err(Error::Cancelled) => p.finish(state::CANCELLED, Some("cancelled".into())),
            Err(e) => {
                log(Level::Error, format!("{}: {e}", req.table));
                p.finish(state::FAILED, Some(e.to_string()));
            }
        }
    }
    Ok(())
}
