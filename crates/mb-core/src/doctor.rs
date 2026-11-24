//! Pre-flight checks ("Doctor"): everything that would stop or degrade a migration between two
//! PostgreSQL databases, each with a concrete fix. `Block` findings must be fixed before migrating.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio_postgres::{Client, SimpleQueryRow};

use crate::conn::{connect, simple_rows, ConnParams, SessionOptions, Side};
use crate::ident::quote_ident;
use crate::introspect::SchemaFilter;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Severity {
    Pass,
    Warn,
    Block,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Finding {
    pub severity: Severity,
    pub title: String,
    pub detail: String,
    /// SQL or an instruction that resolves the finding.
    pub fix: Option<String>,
}

impl Finding {
    fn new(severity: Severity, title: impl Into<String>) -> Self {
        Self { severity, title: title.into(), detail: String::new(), fix: None }
    }
    fn detail(mut self, d: impl Into<String>) -> Self {
        self.detail = d.into();
        self
    }
    fn fix(mut self, f: impl Into<String>) -> Self {
        self.fix = Some(f.into());
        self
    }
}

pub struct DoctorInput {
    pub source: ConnParams,
    pub target: ConnParams,
    pub filter: SchemaFilter,
    /// Parallel connection pairs the migration will open.
    pub workers: usize,
}

struct Info {
    version_num: i32,
    version: String,
    user: String,
    database: String,
    encoding: String,
    collate: String,
    size: i64,
    standby: bool,
    read_only: bool,
    started: String,
    db_oid: String,
    db_create: bool,
    free_conns: i64,
    latency_ms: f64,
}

async fn info(c: &Client) -> crate::Result<Info> {
    let t0 = Instant::now();
    let rows = simple_rows(
        c,
        "SELECT current_setting('server_version_num') AS n, current_setting('server_version') AS v, \
         current_user AS u, current_database() AS d, pg_encoding_to_char(db.encoding) AS enc, db.datcollate AS coll, \
         pg_database_size(current_database()) AS sz, pg_is_in_recovery() AS standby, \
         current_setting('default_transaction_read_only') AS ro, pg_postmaster_start_time()::text AS started, \
         db.oid::text AS dboid, has_database_privilege(current_database(), 'CREATE') AS dbc, \
         current_setting('max_connections')::int - (SELECT count(*) FROM pg_stat_activity) AS free \
         FROM pg_database db WHERE db.datname = current_database()",
    )
    .await?;
    let latency_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let r = rows.first().ok_or_else(|| crate::Error::Connect("empty response".into()))?;
    let g = |k: &str| r.get(k).unwrap_or_default().to_string();
    Ok(Info {
        version_num: g("n").parse().unwrap_or(0),
        version: g("v"),
        user: g("u"),
        database: g("d"),
        encoding: g("enc"),
        collate: g("coll"),
        size: g("sz").parse().unwrap_or(0),
        standby: g("standby") == "t",
        read_only: g("ro") == "on",
        started: g("started"),
        db_oid: g("dboid"),
        db_create: g("dbc") == "t",
        free_conns: g("free").parse().unwrap_or(i64::MAX),
        latency_ms,
    })
}

struct TableAccess {
    select: bool,
    insert: bool,
    truncate: bool,
    has_pk: bool,
}

/// User tables (not owned by extensions) with the current user's privileges.
async fn tables(c: &Client, filter: &SchemaFilter) -> crate::Result<BTreeMap<(String, String), TableAccess>> {
    let rows = simple_rows(
        c,
        "SELECT n.nspname AS s, c.relname AS t, \
         has_schema_privilege(n.oid, 'USAGE') AND has_table_privilege(c.oid, 'SELECT') AS sel, \
         has_table_privilege(c.oid, 'INSERT') AS ins, has_table_privilege(c.oid, 'TRUNCATE') AS tr, \
         EXISTS (SELECT 1 FROM pg_constraint k WHERE k.conrelid = c.oid AND k.contype = 'p') AS pk \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relkind IN ('r', 'p') AND NOT c.relispartition \
         AND n.nspname NOT IN ('pg_catalog', 'information_schema') AND n.nspname NOT LIKE 'pg\\_%' \
         AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.classid = 'pg_class'::regclass AND d.objid = c.oid AND d.deptype = 'e')",
    )
    .await?;
    let t = |r: &SimpleQueryRow, k: &str| r.get(k) == Some("t");
    Ok(rows
        .iter()
        .filter(|r| filter.allows(r.get("s").unwrap_or_default()))
        .map(|r| {
            (
                (r.get("s").unwrap_or_default().to_string(), r.get("t").unwrap_or_default().to_string()),
                TableAccess { select: t(r, "sel"), insert: t(r, "ins"), truncate: t(r, "tr"), has_pk: t(r, "pk") },
            )
        })
        .collect())
}

async fn names(c: &Client, sql: &str) -> crate::Result<BTreeSet<String>> {
    Ok(simple_rows(c, sql).await?.iter().filter_map(|r| r.get(0).map(str::to_string)).collect())
}

fn list(items: &[String], max: usize) -> String {
    let mut s = items.iter().take(max).cloned().collect::<Vec<_>>().join(", ");
    if items.len() > max {
        s.push_str(&format!(" +{} more", items.len() - max));
    }
    s
}

fn fmt_size(b: i64) -> String {
    let b = b.max(0) as f64;
    match b {
        x if x >= 1e12 => format!("{:.1} TB", x / 1e12),
        x if x >= 1e9 => format!("{:.1} GB", x / 1e9),
        x if x >= 1e6 => format!("{:.1} MB", x / 1e6),
        x => format!("{:.0} KB", x / 1e3),
    }
}

/// Run every check. Never fails: problems become findings.
pub async fn diagnose(input: &DoctorInput) -> Vec<Finding> {
    let mut out = Vec::new();
    for (p, side) in [(&input.source, "Source"), (&input.target, "Target")] {
        for w in p.warnings() {
            out.push(Finding::new(Severity::Warn, format!("{side} connection")).detail(w));
        }
    }

    let (so, to) = (SessionOptions::introspection(Side::Source), SessionOptions::introspection(Side::Target));
    let (sc, tc) = tokio::join!(connect(&input.source, &so), connect(&input.target, &to));
    let mut reach = |r: &crate::Result<Client>, side: &str, p: &ConnParams| {
        if let Err(e) = r {
            out.push(
                Finding::new(Severity::Block, format!("{side} unreachable"))
                    .detail(format!("{} — {e}", p.display()))
                    .fix("Check host, port, credentials and that your IP is allowed by the server's firewall."),
            );
        }
    };
    reach(&sc, "Source", &input.source);
    reach(&tc, "Target", &input.target);
    let (Ok(sc), Ok(tc)) = (sc, tc) else { return out };

    let (si, ti) = tokio::join!(info(&sc), info(&tc));
    let (si, ti) = match (si, ti) {
        (Ok(a), Ok(b)) => (a, b),
        (a, b) => {
            for (r, side) in [(a.err(), "Source"), (b.err(), "Target")] {
                if let Some(e) = r {
                    out.push(Finding::new(Severity::Block, format!("{side} not readable")).detail(e.to_string()));
                }
            }
            return out;
        }
    };

    for (i, side) in [(&si, "Source"), (&ti, "Target")] {
        out.push(
            Finding::new(Severity::Pass, format!("{side} connected"))
                .detail(format!("PostgreSQL {} · {} · {:.0} ms", i.version, i.user, i.latency_ms)),
        );
        if i.version_num < 120000 {
            out.push(
                Finding::new(Severity::Block, format!("{side} PostgreSQL {} is too old", i.version))
                    .fix("Upgrade the server to PostgreSQL 12 or newer."),
            );
        }
    }

    if si.started == ti.started && si.db_oid == ti.db_oid {
        out.push(
            Finding::new(Severity::Block, "Source and target are the same database")
                .detail(format!("{} on the same server", si.database))
                .fix("Pick a different target connection."),
        );
    }
    if ti.standby || ti.read_only {
        out.push(
            Finding::new(Severity::Block, "Target is read-only")
                .detail(if ti.standby { "The target is a standby replica." } else { "default_transaction_read_only is on." })
                .fix("Connect the target to the primary (writable) server."),
        );
    }
    if si.standby {
        out.push(
            Finding::new(Severity::Warn, "Source is a standby")
                .detail("A consistent snapshot can't be exported; tables are copied from independent snapshots."),
        );
    }
    if ti.version_num / 10000 < si.version_num / 10000 {
        out.push(
            Finding::new(Severity::Warn, "Target runs an older major version")
                .detail(format!("{} → {}: newer syntax may fail on the target.", si.version, ti.version)),
        );
    }
    if si.encoding != ti.encoding {
        out.push(
            Finding::new(Severity::Warn, "Encodings differ")
                .detail(format!("{} → {}: characters the target can't store will fail.", si.encoding, ti.encoding)),
        );
    }
    if si.collate != ti.collate {
        out.push(
            Finding::new(Severity::Warn, "Collations differ")
                .detail(format!("{} → {}: text ordering and unique indexes may behave differently.", si.collate, ti.collate)),
        );
    }

    // Source tables: readable?
    let src_tables = match tables(&sc, &input.filter).await {
        Ok(t) => t,
        Err(e) => {
            out.push(Finding::new(Severity::Block, "Can't list source tables").detail(e.to_string()));
            return out;
        }
    };
    let unreadable: Vec<&(String, String)> = src_tables.iter().filter(|(_, a)| !a.select).map(|(k, _)| k).collect();
    if unreadable.is_empty() {
        out.push(
            Finding::new(Severity::Pass, "Source readable")
                .detail(format!("{} tables · {}", src_tables.len(), fmt_size(si.size))),
        );
    } else {
        let schemas: BTreeSet<&String> = unreadable.iter().map(|k| &k.0).collect();
        let user = quote_ident(&si.user);
        let fix = schemas
            .iter()
            .map(|s| format!("GRANT USAGE ON SCHEMA {q} TO {user};\nGRANT SELECT ON ALL TABLES IN SCHEMA {q} TO {user};", q = quote_ident(s)))
            .collect::<Vec<_>>()
            .join("\n");
        let names: Vec<String> = unreadable.iter().map(|(s, t)| format!("{s}.{t}")).collect();
        out.push(
            Finding::new(Severity::Block, format!("{} source table(s) not readable", unreadable.len()))
                .detail(list(&names, 6))
                .fix(format!("-- run on the source as the table owner\n{fix}")),
        );
    }
    let no_pk: Vec<String> = src_tables.iter().filter(|(_, a)| !a.has_pk).map(|((s, t), _)| format!("{s}.{t}")).collect();
    if !no_pk.is_empty() {
        out.push(
            Finding::new(Severity::Warn, format!("{} table(s) without a primary key", no_pk.len()))
                .detail(format!("Sync mode and row-level compare skip them. {}", list(&no_pk, 5))),
        );
    }

    // Extensions.
    let used = names(&sc, "SELECT extname FROM pg_extension WHERE extname <> 'plpgsql'").await.unwrap_or_default();
    let avail = names(&tc, "SELECT name FROM pg_available_extensions").await.unwrap_or_default();
    let missing: Vec<String> = used.iter().filter(|e| !avail.contains(*e)).cloned().collect();
    if !missing.is_empty() {
        out.push(
            Finding::new(Severity::Block, format!("{} extension(s) unavailable on target", missing.len()))
                .detail(list(&missing, 8))
                .fix("Install them on the target server, or enable them in your provider's dashboard."),
        );
    } else if !used.is_empty() {
        out.push(Finding::new(Severity::Pass, "Extensions available").detail(list(&used.into_iter().collect::<Vec<_>>(), 8)));
    }

    // Target: can create objects?
    let tgt_user = quote_ident(&ti.user);
    let src_schemas: BTreeSet<&String> = src_tables.keys().map(|k| &k.0).collect();
    let tgt_schemas: BTreeMap<String, bool> = simple_rows(&tc, "SELECT nspname AS s, has_schema_privilege(oid, 'CREATE') AS c FROM pg_namespace")
        .await
        .map(|rows| rows.iter().map(|r| (r.get("s").unwrap_or_default().to_string(), r.get("c") == Some("t"))).collect())
        .unwrap_or_default();
    let mut create_fix = Vec::new();
    for s in &src_schemas {
        match tgt_schemas.get(*s) {
            Some(true) => {}
            Some(false) => create_fix.push(format!("GRANT USAGE, CREATE ON SCHEMA {} TO {tgt_user};", quote_ident(s))),
            None if !ti.db_create => {
                create_fix.push(format!("GRANT CREATE ON DATABASE {} TO {tgt_user};", quote_ident(&ti.database)))
            }
            None => {}
        }
    }
    create_fix.dedup();
    if create_fix.is_empty() {
        out.push(Finding::new(Severity::Pass, "Target writable").detail(format!("{} can create objects", ti.user)));
    } else {
        out.push(
            Finding::new(Severity::Block, "Target user can't create objects")
                .detail(format!("{} lacks CREATE on {} schema(s)", ti.user, create_fix.len()))
                .fix(format!("-- run on the target as an admin\n{}", create_fix.join("\n"))),
        );
    }

    // Target tables that already exist: insert/truncate rights.
    if let Ok(tgt_tables) = tables(&tc, &input.filter).await {
        let mut no_insert = Vec::new();
        let mut no_trunc = Vec::new();
        for k in src_tables.keys() {
            if let Some(a) = tgt_tables.get(k) {
                if !a.insert {
                    no_insert.push(k);
                } else if !a.truncate {
                    no_trunc.push(k);
                }
            }
        }
        let grant = |ks: &[&(String, String)], what: &str| {
            let schemas: BTreeSet<&String> = ks.iter().map(|k| &k.0).collect();
            schemas
                .iter()
                .map(|s| format!("GRANT {what} ON ALL TABLES IN SCHEMA {} TO {tgt_user};", quote_ident(s)))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let show = |ks: &[&(String, String)]| list(&ks.iter().map(|(s, t)| format!("{s}.{t}")).collect::<Vec<_>>(), 6);
        if !no_insert.is_empty() {
            out.push(
                Finding::new(Severity::Block, format!("{} target table(s) not writable", no_insert.len()))
                    .detail(show(&no_insert))
                    .fix(grant(&no_insert, "SELECT, INSERT, UPDATE, DELETE, TRUNCATE")),
            );
        }
        if !no_trunc.is_empty() {
            out.push(
                Finding::new(Severity::Warn, format!("{} target table(s) can't be truncated", no_trunc.len()))
                    .detail(format!("Replace mode needs TRUNCATE. {}", show(&no_trunc)))
                    .fix(grant(&no_trunc, "TRUNCATE")),
            );
        }
    }

    // Fast loads.
    let replica = tc.batch_execute("SET session_replication_role = replica; RESET session_replication_role;").await.is_ok();
    out.push(if replica {
        Finding::new(Severity::Pass, "Fast load").detail("Triggers and FK checks can be paused during the copy.")
    } else {
        Finding::new(Severity::Warn, "Foreign-key ordered load")
            .detail("Not allowed to pause triggers (needs superuser). Tables load in FK order; circular references may fail.")
    });

    // Exact resume needs a marker table on the target.
    if ti.db_create || tgt_schemas.contains_key(crate::data::copy::CHECKPOINT_SCHEMA) {
        out.push(Finding::new(Severity::Pass, "Resumable").detail("Interrupted runs continue exactly where they stopped."));
    } else {
        out.push(
            Finding::new(Severity::Warn, "Resume is approximate")
                .detail("Can't create the checkpoint schema on the target; a crash mid-chunk may re-copy that chunk.")
                .fix(format!("GRANT CREATE ON DATABASE {} TO {tgt_user};", quote_ident(&ti.database))),
        );
    }

    // Connection headroom.
    let need = (input.workers.max(1) + 2) as i64;
    if ti.free_conns < need || si.free_conns < need {
        let free = ti.free_conns.min(si.free_conns).max(0);
        out.push(
            Finding::new(Severity::Warn, "Few free connections")
                .detail(format!("{need} needed, {free} free. Lower the worker count or raise max_connections.")),
        );
    }

    out.sort_by(|a, b| b.severity.cmp(&a.severity));
    out
}

/// Whether any finding blocks migration.
pub fn blocked(findings: &[Finding]) -> bool {
    findings.iter().any(|f| f.severity == Severity::Block)
}
