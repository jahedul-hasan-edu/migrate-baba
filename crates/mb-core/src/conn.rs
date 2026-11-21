//! Connection profiles, URL parsing, TLS and session setup.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio_postgres::{Client, SimpleQueryMessage, SimpleQueryRow};

use crate::error::{Error, PgResultExt, Result};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum SslMode {
    Disable,
    #[default]
    Prefer,
    Require,
    VerifyFull,
}

impl SslMode {
    pub const ALL: [SslMode; 4] = [SslMode::Disable, SslMode::Prefer, SslMode::Require, SslMode::VerifyFull];

    pub fn as_str(self) -> &'static str {
        match self {
            SslMode::Disable => "disable",
            SslMode::Prefer => "prefer",
            SslMode::Require => "require",
            SslMode::VerifyFull => "verify-full",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "disable" | "allow" => SslMode::Disable,
            "prefer" => SslMode::Prefer,
            "require" | "verify-ca" => SslMode::Require,
            "verify-full" => SslMode::VerifyFull,
            _ => return None,
        })
    }
}

/// Hosting provider presets. They only influence defaults (port, SSL, excluded schemas, hints).
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum Provider {
    #[default]
    Generic,
    Supabase,
    SupabasePooler,
    Neon,
    AwsRds,
    GoogleCloudSql,
    Azure,
    DigitalOcean,
    Render,
    Railway,
    CrunchyBridge,
    Timescale,
    Local,
}

/// Schemas managed by Supabase that should not be migrated by default.
pub const SUPABASE_MANAGED_SCHEMAS: &[&str] = &[
    "auth",
    "storage",
    "realtime",
    "_realtime",
    "supabase_functions",
    "supabase_migrations",
    "extensions",
    "graphql",
    "graphql_public",
    "pgsodium",
    "pgsodium_masks",
    "vault",
    "net",
    "_analytics",
    "pgbouncer",
    "cron",
    "pgtle",
    "_supavisor",
];

impl Provider {
    pub const ALL: [Provider; 13] = [
        Provider::Generic,
        Provider::Supabase,
        Provider::SupabasePooler,
        Provider::Neon,
        Provider::AwsRds,
        Provider::GoogleCloudSql,
        Provider::Azure,
        Provider::DigitalOcean,
        Provider::Render,
        Provider::Railway,
        Provider::CrunchyBridge,
        Provider::Timescale,
        Provider::Local,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Provider::Generic => "PostgreSQL",
            Provider::Supabase => "Supabase (direct)",
            Provider::SupabasePooler => "Supabase (session pooler)",
            Provider::Neon => "Neon",
            Provider::AwsRds => "AWS RDS / Aurora",
            Provider::GoogleCloudSql => "Google Cloud SQL",
            Provider::Azure => "Azure Flexible Server",
            Provider::DigitalOcean => "DigitalOcean",
            Provider::Render => "Render",
            Provider::Railway => "Railway",
            Provider::CrunchyBridge => "Crunchy Bridge",
            Provider::Timescale => "Timescale",
            Provider::Local => "Local",
        }
    }

    pub fn default_port(self) -> u16 {
        5432
    }

    pub fn default_ssl(self) -> SslMode {
        match self {
            Provider::Generic => SslMode::Prefer,
            Provider::Local => SslMode::Disable,
            _ => SslMode::Require,
        }
    }

    pub fn default_user(self) -> &'static str {
        "postgres"
    }

    pub fn default_database(self) -> &'static str {
        match self {
            Provider::Neon => "neondb",
            Provider::DigitalOcean => "defaultdb",
            Provider::Railway => "railway",
            Provider::Timescale => "tsdb",
            _ => "postgres",
        }
    }

    pub fn excluded_schemas(self) -> Vec<String> {
        match self {
            Provider::Supabase | Provider::SupabasePooler => {
                SUPABASE_MANAGED_SCHEMAS.iter().map(|s| s.to_string()).collect()
            }
            Provider::Timescale => vec![
                "_timescaledb_catalog".into(),
                "_timescaledb_internal".into(),
                "_timescaledb_cache".into(),
                "_timescaledb_config".into(),
                "_timescaledb_functions".into(),
                "_timescaledb_debug".into(),
                "timescaledb_information".into(),
                "timescaledb_experimental".into(),
                "toolkit_experimental".into(),
            ],
            Provider::AwsRds => vec!["aws_commons".into(), "aws_s3".into()],
            _ => Vec::new(),
        }
    }

    pub fn host_hint(self) -> &'static str {
        match self {
            Provider::Supabase => "db.<project-ref>.supabase.co",
            Provider::SupabasePooler => "aws-0-<region>.pooler.supabase.com (user: postgres.<project-ref>)",
            Provider::Neon => "ep-xxxx.<region>.aws.neon.tech",
            Provider::AwsRds => "<instance>.<id>.<region>.rds.amazonaws.com",
            Provider::GoogleCloudSql => "public IP or Cloud SQL Auth Proxy (127.0.0.1)",
            Provider::Azure => "<server>.postgres.database.azure.com",
            Provider::DigitalOcean => "<cluster>.db.ondigitalocean.com (port 25060)",
            Provider::Local => "localhost",
            _ => "hostname",
        }
    }

    /// Detect the provider from a host name.
    pub fn detect(host: &str, port: u16) -> Provider {
        let h = host.to_ascii_lowercase();
        if h.ends_with("pooler.supabase.com") {
            Provider::SupabasePooler
        } else if h.ends_with("supabase.co") || h.ends_with("supabase.com") {
            Provider::Supabase
        } else if h.contains("neon.tech") {
            Provider::Neon
        } else if h.contains("rds.amazonaws.com") {
            Provider::AwsRds
        } else if h.contains("postgres.database.azure.com") {
            Provider::Azure
        } else if h.contains("ondigitalocean.com") {
            Provider::DigitalOcean
        } else if h.contains("render.com") {
            Provider::Render
        } else if h.contains("rlwy.net") || h.contains("railway") {
            Provider::Railway
        } else if h.contains("crunchybridge") {
            Provider::CrunchyBridge
        } else if h.contains("tsdb.cloud.timescale.com") || h.contains("timescale") {
            Provider::Timescale
        } else if h == "localhost" || h == "127.0.0.1" || h == "::1" {
            let _ = port;
            Provider::Local
        } else {
            Provider::Generic
        }
    }
}

/// Everything needed to open a connection.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConnParams {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    #[serde(skip)]
    pub password: String,
    pub ssl_mode: SslMode,
    #[serde(default = "default_timeout")]
    pub connect_timeout_secs: u64,
}

fn default_timeout() -> u64 {
    15
}

impl std::fmt::Debug for ConnParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnParams")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("database", &self.database)
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .field("ssl_mode", &self.ssl_mode)
            .finish()
    }
}

impl Default for ConnParams {
    fn default() -> Self {
        Self {
            host: "localhost".into(),
            port: 5432,
            database: "postgres".into(),
            user: "postgres".into(),
            password: String::new(),
            ssl_mode: SslMode::Prefer,
            connect_timeout_secs: 15,
        }
    }
}

impl ConnParams {
    /// Parse `postgres://user:pass@host:port/db?sslmode=require` (also `postgresql://`).
    pub fn parse_url(input: &str) -> Result<Self> {
        let input = input.trim();
        let url = url::Url::parse(input).map_err(|e| Error::Invalid(format!("bad connection URL: {e}")))?;
        if url.scheme() != "postgres" && url.scheme() != "postgresql" {
            return Err(Error::Invalid("URL must start with postgres:// or postgresql://".into()));
        }
        let decode = |s: &str| percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned();
        let mut p = ConnParams {
            host: url.host_str().unwrap_or("localhost").trim_matches(['[', ']']).to_string(),
            port: url.port().unwrap_or(5432),
            database: decode(url.path().trim_start_matches('/')),
            user: decode(url.username()),
            password: url.password().map(decode).unwrap_or_default(),
            ssl_mode: SslMode::Prefer,
            connect_timeout_secs: 15,
        };
        if p.database.is_empty() {
            p.database = "postgres".into();
        }
        if p.user.is_empty() {
            p.user = "postgres".into();
        }
        for (k, v) in url.query_pairs() {
            match k.as_ref() {
                "sslmode" => {
                    p.ssl_mode = SslMode::parse(&v).ok_or_else(|| Error::Invalid(format!("unknown sslmode '{v}'")))?
                }
                "connect_timeout" => p.connect_timeout_secs = v.parse().unwrap_or(15),
                "user" => p.user = v.into_owned(),
                "password" => p.password = v.into_owned(),
                "dbname" => p.database = v.into_owned(),
                "host" => p.host = v.into_owned(),
                "port" => p.port = v.parse().unwrap_or(5432),
                _ => {}
            }
        }
        Ok(p)
    }

    /// Render as URL. Password is included only when asked (never for display/logging).
    pub fn to_url(&self, with_password: bool) -> String {
        let enc = |s: &str| {
            percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC)
                .to_string()
                .replace("%2E", ".")
                .replace("%5F", "_")
                .replace("%2D", "-")
        };
        let auth = if with_password && !self.password.is_empty() {
            format!("{}:{}@", enc(&self.user), enc(&self.password))
        } else {
            format!("{}@", enc(&self.user))
        };
        let host = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        format!(
            "postgresql://{auth}{host}:{}/{}?sslmode={}",
            self.port,
            enc(&self.database),
            self.ssl_mode.as_str()
        )
    }

    /// Short human label, e.g. `postgres@db.x.supabase.co:5432/postgres`.
    pub fn display(&self) -> String {
        format!("{}@{}:{}/{}", self.user, self.host, self.port, self.database)
    }

    /// Warnings about the connection target (e.g., transaction pooler).
    pub fn warnings(&self) -> Vec<String> {
        let mut w = Vec::new();
        let h = self.host.to_ascii_lowercase();
        if self.port == 6543 || h.contains("-pooler") && h.contains("neon") {
            w.push(
                "This looks like a transaction-mode pooler. Snapshots and session settings are not \
                 reliable through it; use the direct connection or the session pooler (port 5432)."
                    .into(),
            );
        }
        if self.ssl_mode == SslMode::Disable && !matches!(h.as_str(), "localhost" | "127.0.0.1" | "::1") {
            w.push("SSL is disabled for a remote host; credentials and data travel in clear text.".into());
        }
        w
    }
}

/// Which side a session serves. Source sessions are forced read-only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Source,
    Target,
}

/// Session-level settings applied right after connecting.
#[derive(Clone, Debug)]
pub struct SessionOptions {
    pub side: Side,
    /// Normalise text output (dates, floats, bytea) — required for data hashing/copy.
    pub normalize_output: bool,
    /// `statement_timeout` in seconds (0 = none).
    pub statement_timeout_secs: u64,
    /// `lock_timeout` in seconds (0 = none).
    pub lock_timeout_secs: u64,
    /// Use an empty `search_path` so catalog functions emit fully-qualified names.
    pub empty_search_path: bool,
}

impl SessionOptions {
    pub fn introspection(side: Side) -> Self {
        Self { side, normalize_output: true, statement_timeout_secs: 120, lock_timeout_secs: 0, empty_search_path: true }
    }

    pub fn data(side: Side) -> Self {
        Self { side, normalize_output: true, statement_timeout_secs: 0, lock_timeout_secs: 0, empty_search_path: false }
    }

    pub fn deploy(lock_timeout_secs: u64) -> Self {
        Self {
            side: Side::Target,
            normalize_output: false,
            statement_timeout_secs: 0,
            lock_timeout_secs,
            empty_search_path: false,
        }
    }
}

fn tls_connector(mode: SslMode) -> Result<postgres_native_tls::MakeTlsConnector> {
    let mut b = native_tls::TlsConnector::builder();
    if mode != SslMode::VerifyFull {
        // libpq semantics: `require`/`prefer` encrypt but don't verify the certificate.
        b.danger_accept_invalid_certs(true);
        b.danger_accept_invalid_hostnames(true);
    }
    let c = b.build().map_err(|e| Error::Tls(e.to_string()))?;
    Ok(postgres_native_tls::MakeTlsConnector::new(c))
}

/// Open a connection, spawn its driver task, and apply session settings.
pub async fn connect(params: &ConnParams, opts: &SessionOptions) -> Result<Client> {
    use tokio_postgres::config::SslMode as PgSsl;
    let mut cfg = tokio_postgres::Config::new();
    cfg.host(&params.host)
        .port(params.port)
        .dbname(&params.database)
        .user(&params.user)
        .application_name("migrate-baba")
        .connect_timeout(Duration::from_secs(params.connect_timeout_secs.max(1)))
        .keepalives(true)
        .keepalives_idle(Duration::from_secs(30))
        .ssl_mode(match params.ssl_mode {
            SslMode::Disable => PgSsl::Disable,
            SslMode::Prefer => PgSsl::Prefer,
            SslMode::Require | SslMode::VerifyFull => PgSsl::Require,
        });
    if !params.password.is_empty() {
        cfg.password(&params.password);
    }
    let tls = tls_connector(params.ssl_mode)?;
    let (client, connection) = cfg.connect(tls).await.map_err(|e| {
        let mut msg = e.to_string();
        let mut src = std::error::Error::source(&e);
        while let Some(s) = src {
            msg.push_str(": ");
            msg.push_str(&s.to_string());
            src = s.source();
        }
        if let Some(db) = e.as_db_error() {
            msg = format!("{} [SQLSTATE {}]", db.message(), db.code().code());
        }
        Error::Connect(msg)
    })?;
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            tracing::debug!("connection closed with error: {e}");
        }
    });

    let mut setup = String::new();
    if opts.empty_search_path {
        setup.push_str("SET search_path = '';");
    }
    if opts.normalize_output {
        setup.push_str(
            "SET DateStyle = 'ISO, MDY'; SET IntervalStyle = 'postgres'; SET TimeZone = 'UTC'; \
             SET extra_float_digits = 3; SET bytea_output = 'hex'; SET client_encoding = 'UTF8';",
        );
    }
    setup.push_str(&format!("SET statement_timeout = {};", opts.statement_timeout_secs * 1000));
    if opts.lock_timeout_secs > 0 {
        setup.push_str(&format!("SET lock_timeout = {};", opts.lock_timeout_secs * 1000));
    }
    if opts.side == Side::Source {
        setup.push_str("SET default_transaction_read_only = on;");
    }
    client.batch_execute(&setup).await.sql(&setup)?;
    Ok(client)
}

/// Ask the server to cancel the statement currently running on `client`.
pub async fn cancel_running(client: &Client, params: &ConnParams) -> Result<()> {
    let tls = tls_connector(params.ssl_mode)?;
    client.cancel_token().cancel_query(tls).await.map_err(|e| Error::from_pg(e, None))
}

/// Run a simple-protocol query and return its rows.
pub async fn simple_rows(client: &Client, sql: &str) -> Result<Vec<SimpleQueryRow>> {
    let msgs = client.simple_query(sql).await.sql(sql)?;
    Ok(msgs
        .into_iter()
        .filter_map(|m| match m {
            SimpleQueryMessage::Row(r) => Some(r),
            _ => None,
        })
        .collect())
}

/// Result of "Test connection".
#[derive(Clone, Debug, Default)]
pub struct ServerInfo {
    pub version: String,
    pub version_num: i32,
    pub current_user: String,
    pub database: String,
    pub is_superuser: bool,
    pub can_replica_role: bool,
    pub latency_ms: f64,
    pub connect_ms: f64,
    pub warnings: Vec<String>,
}

pub async fn test_connection(params: &ConnParams) -> Result<ServerInfo> {
    let t0 = Instant::now();
    let client = connect(params, &SessionOptions::introspection(Side::Target)).await?;
    let connect_ms = t0.elapsed().as_secs_f64() * 1000.0;

    // Measure a pure round-trip.
    let t1 = Instant::now();
    client.simple_query("SELECT 1").await.sql("SELECT 1")?;
    let latency_ms = t1.elapsed().as_secs_f64() * 1000.0;

    let rows = simple_rows(
        &client,
        "SELECT current_setting('server_version') v, current_setting('server_version_num') n, \
         current_user u, current_database() d, \
         (SELECT rolsuper FROM pg_roles WHERE rolname = current_user) s",
    )
    .await?;
    let r = rows.first().ok_or_else(|| Error::Connect("empty response".into()))?;
    let can_replica = client
        .batch_execute("SET session_replication_role = replica; RESET session_replication_role;")
        .await
        .is_ok();
    Ok(ServerInfo {
        version: r.get("v").unwrap_or_default().to_string(),
        version_num: r.get("n").and_then(|v| v.parse().ok()).unwrap_or(0),
        current_user: r.get("u").unwrap_or_default().to_string(),
        database: r.get("d").unwrap_or_default().to_string(),
        is_superuser: r.get("s") == Some("t"),
        can_replica_role: can_replica,
        latency_ms,
        connect_ms,
        warnings: params.warnings(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_supabase_url() {
        let p = ConnParams::parse_url(
            "postgresql://postgres.abcd:p%40ss%3Aword@aws-0-eu-central-1.pooler.supabase.com:5432/postgres?sslmode=require",
        )
        .unwrap();
        assert_eq!(p.user, "postgres.abcd");
        assert_eq!(p.password, "p@ss:word");
        assert_eq!(p.host, "aws-0-eu-central-1.pooler.supabase.com");
        assert_eq!(p.port, 5432);
        assert_eq!(p.database, "postgres");
        assert_eq!(p.ssl_mode, SslMode::Require);
        assert_eq!(Provider::detect(&p.host, p.port), Provider::SupabasePooler);
    }

    #[test]
    fn url_roundtrip_hides_password() {
        let mut p = ConnParams::default();
        p.password = "secret".into();
        assert!(!p.to_url(false).contains("secret"));
        let back = ConnParams::parse_url(&p.to_url(true)).unwrap();
        assert_eq!(back.password, "secret");
        assert!(!format!("{p:?}").contains("secret"));
    }

    #[test]
    fn defaults_when_missing() {
        let p = ConnParams::parse_url("postgres://localhost").unwrap();
        assert_eq!(p.user, "postgres");
        assert_eq!(p.database, "postgres");
        assert_eq!(p.port, 5432);
    }

    #[test]
    fn pooler_warning() {
        let p = ConnParams::parse_url("postgres://u@x.pooler.supabase.com:6543/postgres").unwrap();
        assert!(!p.warnings().is_empty());
    }
}
