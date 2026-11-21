//! `mbaba` — command-line interface to the Migrate Baba engine (CI-friendly).
//!
//! Exit codes: 0 = success / no differences, 1 = error, 2 = differences found.

use std::collections::BTreeSet;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand, ValueEnum};
use mb_core::conn::{connect, ConnParams, SessionOptions, Side};
use mb_core::data::compare::{compare_tables, CompareOptions, CompareStatus};
use mb_core::data::copy::{self, plan_jobs, CopyMode, CopyOptions};
use mb_core::diff::{compare, DiffStatus};
use mb_core::introspect::{introspect, SchemaFilter};
use mb_core::model::{Catalog, QName};
use mb_core::plan::{plan_selection, PlanOptions};
use mb_core::progress::{CancellationToken, Level, LogFn};

#[derive(Parser)]
#[command(name = "mbaba", version, about = "Compare, diff and migrate PostgreSQL databases")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Args, Clone)]
struct Pair {
    /// Source URL (desired state). Env: MB_SOURCE_URL
    #[arg(long, env = "MB_SOURCE_URL")]
    source: String,
    /// Target URL (database to change). Env: MB_TARGET_URL
    #[arg(long, env = "MB_TARGET_URL")]
    target: String,
    /// Only these schemas (comma separated)
    #[arg(long, value_delimiter = ',')]
    schema: Vec<String>,
    /// Exclude schemas (comma separated)
    #[arg(long, value_delimiter = ',')]
    exclude: Vec<String>,
    /// Exclude Supabase-managed schemas (auth, storage, realtime, …)
    #[arg(long)]
    supabase: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum Mode {
    Replace,
    Append,
    Sync,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print the SQL that makes TARGET look like SOURCE.
    Diff {
        #[command(flatten)]
        pair: Pair,
        /// Write the script to a file instead of stdout
        #[arg(long)]
        out: Option<std::path::PathBuf>,
        #[arg(long)]
        ignore_comments: bool,
        #[arg(long)]
        cascade: bool,
        /// Only print a summary of differing objects
        #[arg(long)]
        summary: bool,
    },
    /// Execute a SQL file against TARGET in one transaction.
    Apply {
        #[arg(long, env = "MB_TARGET_URL")]
        target: String,
        #[arg(long)]
        file: std::path::PathBuf,
        /// Run and roll back
        #[arg(long)]
        dry_run: bool,
    },
    /// Copy table data from SOURCE to TARGET.
    Copy {
        #[command(flatten)]
        pair: Pair,
        /// schema.table list (default: all tables present on both sides)
        #[arg(long, value_delimiter = ',')]
        tables: Vec<String>,
        #[arg(long, value_enum, default_value = "replace")]
        mode: Mode,
        #[arg(long, default_value_t = 4)]
        workers: usize,
        #[arg(long)]
        no_snapshot: bool,
    },
    /// Compare table data (row counts + chunk hashes).
    DataDiff {
        #[command(flatten)]
        pair: Pair,
        #[arg(long, value_delimiter = ',')]
        tables: Vec<String>,
        #[arg(long, default_value_t = 10_000)]
        chunk: u64,
    },
    /// Dump the introspected catalog as JSON.
    Inspect {
        #[arg(long)]
        url: String,
    },
}

fn filter(p: &Pair) -> SchemaFilter {
    let mut exclude = p.exclude.clone();
    if p.supabase {
        exclude.extend(mb_core::conn::SUPABASE_MANAGED_SCHEMAS.iter().map(|s| s.to_string()));
    }
    SchemaFilter { include: p.schema.clone(), exclude }
}

async fn read(url: &str, side: Side, f: &SchemaFilter) -> mb_core::Result<(ConnParams, Catalog)> {
    let params = ConnParams::parse_url(url)?;
    let c = connect(&params, &SessionOptions::introspection(side)).await?;
    let cat = introspect(&c, f).await?;
    Ok((params, cat))
}

async fn read_pair(p: &Pair) -> mb_core::Result<((ConnParams, Catalog), (ConnParams, Catalog))> {
    let f = filter(p);
    tokio::try_join!(read(&p.source, Side::Source, &f), read(&p.target, Side::Target, &f))
}

fn parse_tables(v: &[String]) -> Vec<QName> {
    v.iter()
        .map(|t| match t.split_once('.') {
            Some((s, n)) => QName::new(s, n),
            None => QName::new("public", t.as_str()),
        })
        .collect()
}

fn stderr_log() -> LogFn {
    Arc::new(|lvl, msg| {
        let tag = match lvl {
            Level::Error => "error",
            Level::Warn => "warn ",
            _ => "info ",
        };
        eprintln!("[{tag}] {msg}");
    })
}

async fn run(cli: Cli) -> mb_core::Result<u8> {
    match cli.cmd {
        Cmd::Diff { pair, out, ignore_comments, cascade, summary } => {
            let ((_, mut s), (_, mut t)) = read_pair(&pair).await?;
            if ignore_comments {
                s.strip_comments();
                t.strip_comments();
            }
            let diffs = compare(&s, &t);
            let changed: Vec<_> = diffs.iter().filter(|d| d.status != DiffStatus::Identical).collect();
            if summary {
                for d in &changed {
                    println!("{:<10} {:<18} {}", d.status.label(), d.key.kind.label(), d.key.display());
                    for det in &d.details {
                        println!("{:<30}  - {det}", "");
                    }
                }
            } else {
                let sel: BTreeSet<_> = changed.iter().map(|d| d.key.clone()).collect();
                let plan = plan_selection(&s, &t, &sel, &PlanOptions { cascade_drops: cascade, ..Default::default() });
                let sql = plan.to_sql("Migration generated by mbaba");
                match out {
                    Some(p) => std::fs::write(p, sql)?,
                    None => print!("{sql}"),
                }
            }
            eprintln!("{} object(s) differ", changed.len());
            Ok(if changed.is_empty() { 0 } else { 2 })
        }
        Cmd::Apply { target, file, dry_run } => {
            let sql = std::fs::read_to_string(&file)?;
            let p = ConnParams::parse_url(&target)?;
            let c = connect(&p, &SessionOptions::deploy(10)).await?;
            c.batch_execute("BEGIN").await.map_err(|e| mb_core::Error::from_pg(e, None))?;
            if let Err(e) = c.batch_execute(&sql).await {
                let _ = c.batch_execute("ROLLBACK").await;
                return Err(mb_core::Error::from_pg(e, None));
            }
            let end = if dry_run { "ROLLBACK" } else { "COMMIT" };
            c.batch_execute(end).await.map_err(|e| mb_core::Error::from_pg(e, None))?;
            eprintln!("{}", if dry_run { "dry run OK (rolled back)" } else { "applied" });
            Ok(0)
        }
        Cmd::Copy { pair, tables, mode, workers, no_snapshot } => {
            let ((sp, s), (tp, t)) = read_pair(&pair).await?;
            let tables = if tables.is_empty() {
                s.tables.keys().filter(|k| t.tables.contains_key(*k)).cloned().collect()
            } else {
                parse_tables(&tables)
            };
            let (jobs, warnings) = plan_jobs(&s, &t, &tables);
            let log = stderr_log();
            for w in warnings {
                log(Level::Warn, w);
            }
            let opts = CopyOptions {
                mode: match mode {
                    Mode::Replace => CopyMode::Replace,
                    Mode::Append => CopyMode::Append,
                    Mode::Sync => CopyMode::Sync,
                },
                workers,
                consistent_snapshot: !no_snapshot,
                ..Default::default()
            };
            let cancel = CancellationToken::new();
            let c2 = cancel.clone();
            tokio::spawn(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    c2.cancel();
                }
            });
            let sum = copy::run(&sp, &tp, &jobs, s.server_version, &opts, &cancel, &log).await?;
            eprintln!(
                "{} ok, {} failed, {} rows, {} bytes in {} ms",
                sum.tables_ok, sum.tables_failed, sum.rows, sum.bytes, sum.millis
            );
            Ok(if sum.tables_failed == 0 { 0 } else { 1 })
        }
        Cmd::DataDiff { pair, tables, chunk } => {
            let ((sp, s), (tp, t)) = read_pair(&pair).await?;
            let tables = if tables.is_empty() {
                s.tables.keys().chain(t.tables.keys()).cloned().collect::<BTreeSet<_>>().into_iter().collect()
            } else {
                parse_tables(&tables)
            };
            let differ = std::sync::atomic::AtomicUsize::new(0);
            let on_result = |r: mb_core::data::compare::TableCompare| {
                if r.status != CompareStatus::Identical && r.status != CompareStatus::Skipped {
                    differ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                println!(
                    "{:<40} {:<14} src={:<10} tgt={:<10} +{} -{} ~{} {}",
                    r.table.to_string(),
                    r.status.label(),
                    r.source_rows,
                    r.target_rows,
                    r.only_source,
                    r.only_target,
                    r.different,
                    r.message.unwrap_or_default()
                );
            };
            let opts = CompareOptions { chunk_size: chunk, ..Default::default() };
            compare_tables(&sp, &tp, &s, &t, &tables, &opts, &CancellationToken::new(), &|_| {}, &on_result).await?;
            Ok(if differ.load(std::sync::atomic::Ordering::Relaxed) == 0 { 0 } else { 2 })
        }
        Cmd::Inspect { url } => {
            let (_, cat) = read(&url, Side::Source, &SchemaFilter::default()).await?;
            println!("{}", serde_json::to_string_pretty(&cat).unwrap_or_default());
            Ok(0)
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("runtime");
    match rt.block_on(run(cli)) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}
