//! ClickHouse as the log store (`GATEWAY_CLICKHOUSE_URL`): the request log, logged bodies, the
//! audit log and the spend that budgets are rebuilt from. Spoken to over its HTTP interface with
//! `JSONEachRow` rows; values always travel as query parameters (`{name:Type}`), never in the SQL.
//!
//! The schema is in `migrations/clickhouse/`, applied at startup. With ClickHouse configured,
//! nothing per request is written to Postgres (see `logstore`).

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Serialize, de::DeserializeOwned};
use uuid::Uuid;

mod audit;
mod import;
mod logs;
mod metrics;
mod retention;
mod spend;

pub use import::import_from_postgres;
pub use metrics::Selection;
pub use retention::prune;

/// Where ClickHouse is, from the environment.
#[derive(Clone)]
pub struct ClickHouseConfig {
    pub url: String,
    pub user: String,
    pub password: String,
    pub database: String,
}

impl std::fmt::Debug for ClickHouseConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClickHouseConfig").field("url", &self.url).field("user", &self.user).field("database", &self.database).finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("clickhouse request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("clickhouse: {0}")]
    Server(String),
    #[error("clickhouse returned a row that does not decode: {0}")]
    Decode(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// A ClickHouse connection: an HTTP client and where to send queries.
pub struct ClickHouse {
    http: reqwest::Client,
    cfg: ClickHouseConfig,
}

/// The query parameters of one statement, by name (`{name:Type}` in its SQL).
#[derive(Default)]
pub struct Params(Vec<(String, String)>);

impl Params {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn str(mut self, name: &str, v: &str) -> Self {
        self.0.push((format!("param_{name}"), v.to_string()));
        self
    }

    /// `\N` is ClickHouse's NULL in parameters.
    pub fn opt_str(self, name: &str, v: Option<&str>) -> Self {
        match v {
            Some(v) => self.str(name, v),
            None => self.str(name, "\\N"),
        }
    }

    pub fn num(self, name: &str, v: impl std::fmt::Display) -> Self {
        let v = v.to_string();
        self.str(name, &v)
    }

    pub fn uuid(self, name: &str, v: Uuid) -> Self {
        self.str(name, &v.to_string())
    }

    pub fn time(self, name: &str, v: DateTime<Utc>) -> Self {
        self.str(name, &time(v))
    }

    pub fn uuids(self, name: &str, v: &[Uuid]) -> Self {
        self.str(name, &array(v.iter().map(Uuid::to_string)))
    }

    pub fn strs<S: AsRef<str>>(self, name: &str, v: &[S]) -> Self {
        self.str(name, &array(v.iter().map(|s| s.as_ref().to_string())))
    }
}

/// A `DateTime64(3, 'UTC')` as ClickHouse reads it.
pub fn time(v: DateTime<Utc>) -> String {
    v.format("%Y-%m-%d %H:%M:%S%.3f").to_string()
}

/// An array parameter: `['a', 'b']`, quoted for ClickHouse.
fn array(items: impl Iterator<Item = String>) -> String {
    let mut out = String::from("[");
    for (i, s) in items.enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('\'');
        for c in s.chars() {
            match c {
                '\\' => out.push_str("\\\\"),
                '\'' => out.push_str("\\'"),
                c => out.push(c),
            }
        }
        out.push('\'');
    }
    out.push(']');
    out
}

/// Settings sent with every statement: 64-bit integers as JSON numbers and times as RFC 3339, so
/// rows decode into the same types as the Postgres queries'.
const SETTINGS: [(&str, &str); 3] = [
    ("output_format_json_quote_64bit_integers", "0"),
    ("date_time_output_format", "iso"),
    ("output_format_json_named_tuples_as_objects", "1"),
];

impl ClickHouse {
    pub fn new(cfg: ClickHouseConfig) -> Result<Self> {
        // The URL is the operator's, not an admin's: no network guard, as for Postgres and Valkey.
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(60))
            .pool_idle_timeout(Duration::from_secs(50))
            .no_proxy()
            .build()?;
        Ok(Self { http, cfg })
    }

    fn post(&self, sql: &str, params: Params, extra: &[(&str, &str)]) -> reqwest::RequestBuilder {
        let mut query: Vec<(String, String)> = vec![("database".into(), self.cfg.database.clone())];
        query.extend(SETTINGS.iter().chain(extra).map(|(k, v)| (k.to_string(), v.to_string())));
        query.extend(params.0);
        self.http
            .post(&self.cfg.url)
            .basic_auth(&self.cfg.user, Some(&self.cfg.password))
            .query(&query)
            .header("content-type", "text/plain; charset=utf-8")
            .body(sql.to_string())
    }

    async fn send(req: reqwest::RequestBuilder) -> Result<String> {
        let res = req.send().await?;
        let status = res.status();
        let body = res.text().await?;
        if !status.is_success() {
            return Err(Error::Server(body.trim().chars().take(2000).collect()));
        }
        Ok(body)
    }

    /// Runs a statement that returns nothing.
    pub async fn execute(&self, sql: &str, params: Params) -> Result<()> {
        Self::send(self.post(sql, params, &[])).await.map(drop)
    }

    /// Runs a query and decodes its rows. The SQL must not have a `FORMAT` clause.
    pub async fn rows<T: DeserializeOwned>(&self, sql: &str, params: Params) -> Result<Vec<T>> {
        let body = Self::send(self.post(&format!("{sql}\nFORMAT JSONEachRow"), params, &[])).await?;
        body.lines().filter(|l| !l.is_empty()).map(|l| serde_json::from_str(l).map_err(Error::from)).collect()
    }

    pub async fn row<T: DeserializeOwned>(&self, sql: &str, params: Params) -> Result<Option<T>> {
        Ok(self.rows(sql, params).await?.into_iter().next())
    }

    /// Inserts rows, all in one part. `wait_async`: a small insert that ClickHouse may merge with
    /// others (the audit log); it still returns only once the rows are written.
    pub async fn insert<T: Serialize>(&self, table: &str, rows: &[T], wait_async: bool) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut body = String::new();
        for r in rows {
            body.push_str(&serde_json::to_string(r)?);
            body.push('\n');
        }
        let extra: &[(&str, &str)] = if wait_async { &[("async_insert", "1"), ("wait_for_async_insert", "1")] } else { &[] };
        let mut query: Vec<(String, String)> = vec![
            ("database".into(), self.cfg.database.clone()),
            ("query".into(), format!("INSERT INTO {table} FORMAT JSONEachRow")),
            ("input_format_skip_unknown_fields".into(), "0".into()),
        ];
        query.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        let req = self.http.post(&self.cfg.url).basic_auth(&self.cfg.user, Some(&self.cfg.password)).query(&query).body(body);
        Self::send(req).await.map(drop)
    }

    pub async fn ping(&self) -> bool {
        self.execute("SELECT 1", Params::new()).await.is_ok()
    }

    /// Applies the migrations not yet applied, in order. Statements in a file are separated by
    /// a line holding only `;`. Replicas starting together are serialised by the caller.
    pub async fn migrate(&self) -> Result<()> {
        self.execute(
            "CREATE TABLE IF NOT EXISTS _migrations (version UInt32, name String, applied_at DateTime DEFAULT now())
             ENGINE = MergeTree ORDER BY version",
            Params::new(),
        )
        .await?;
        #[derive(serde::Deserialize)]
        struct Applied {
            version: u32,
        }
        let applied: Vec<u32> =
            self.rows::<Applied>("SELECT version FROM _migrations", Params::new()).await?.into_iter().map(|a| a.version).collect();
        for (version, name, sql) in MIGRATIONS {
            if applied.contains(version) {
                continue;
            }
            for stmt in sql.split("\n;\n").map(str::trim).filter(|s| !s.is_empty() && !is_comment(s)) {
                self.execute(stmt, Params::new()).await.map_err(|e| Error::Server(format!("migration {version} ({name}): {e}")))?;
            }
            self.execute(
                "INSERT INTO _migrations (version, name) VALUES ({v:UInt32}, {n:String})",
                Params::new().num("v", version).str("n", name),
            )
            .await?;
            tracing::info!(version, name, "applied clickhouse migration");
        }
        Ok(())
    }
}

#[cfg(test)]
mod live_tests;

fn is_comment(stmt: &str) -> bool {
    stmt.lines().all(|l| l.trim().is_empty() || l.trim_start().starts_with("--"))
}

/// The ClickHouse migrations: version, name, SQL.
const MIGRATIONS: &[(u32, &str, &str)] = &[(1, "init", include_str!("../migrations/clickhouse/0001_init.sql"))];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn array_parameters_are_quoted() {
        assert_eq!(array(["a'b".to_string(), "c\\".to_string()].into_iter()), r"['a\'b','c\\']");
        assert_eq!(array(std::iter::empty()), "[]");
    }

    #[test]
    fn migrations_split_into_statements() {
        for (_, _, sql) in MIGRATIONS {
            assert!(sql.split("\n;\n").filter(|s| !s.trim().is_empty() && !is_comment(s)).count() > 1);
        }
    }
}
