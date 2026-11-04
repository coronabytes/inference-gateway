//! Series and totals from the request log in ClickHouse, for metrics, the model hub and key usage.
//! Mirrors `db::metrics`.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use sqlx::PgPool;
use uuid::Uuid;

use super::{ClickHouse, Params, logs::Where};
use crate::db::{KeyMinute, KeyUsageRow, LogTotals, ModelHour, ModelSpeed};

/// The aggregates of `LogTotals`, after its subject and bucket.
const TOTALS: &str = "count() AS requests, countIf(status_code >= 400) AS errors,
    sum(input_tokens) AS input_tokens, sum(output_tokens) AS output_tokens,
    sum(cache_read_tokens) AS cache_read_tokens, sum(cache_write_tokens) AS cache_write_tokens,
    sum(cost_usd) AS cost_usd, sum(latency_ms) AS latency_ms_sum,
    sum(ifNull(ttft_ms, 0)) AS ttft_ms_sum, count(ttft_ms) AS ttft_count";

#[derive(Deserialize)]
struct Totals {
    subject: String,
    bucket: i64,
    requests: i64,
    errors: i64,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    cost_usd: f64,
    latency_ms_sum: i64,
    ttft_ms_sum: i64,
    ttft_count: i64,
    p95_latency_ms: Option<i32>,
}

impl From<Totals> for LogTotals {
    fn from(t: Totals) -> Self {
        Self {
            subject: t.subject,
            bucket: t.bucket,
            requests: t.requests,
            errors: t.errors,
            input_tokens: t.input_tokens,
            output_tokens: t.output_tokens,
            cache_read_tokens: t.cache_read_tokens,
            cache_write_tokens: t.cache_write_tokens,
            cost_usd: t.cost_usd,
            latency_ms_sum: t.latency_ms_sum,
            ttft_ms_sum: t.ttft_ms_sum,
            ttft_count: t.ttft_count,
            // No entries: no percentile, as in Postgres.
            p95_latency_ms: t.p95_latency_ms.filter(|_| t.requests > 0),
        }
    }
}

/// The filter every Metrics query shares (see `db::selection_series`).
pub struct Selection<'a> {
    pub since: DateTime<Utc>,
    pub until: DateTime<Utc>,
    pub key_id: Option<Uuid>,
    pub deleted_keys: bool,
    pub models: Option<&'a [String]>,
    pub exclude: &'a [String],
    pub projects: Option<&'a [Uuid]>,
    pub labels: Option<&'a serde_json::Value>,
}

async fn selection(pg: &PgPool, s: &Selection<'_>, models: bool) -> crate::logstore::Result<Where> {
    let mut w = Where::new();
    w.range("ts", Some(s.since), Some(s.until));
    if let Some(k) = s.key_id {
        w.and("key_id = {key:UUID}");
        w.with(|p| p.uuid("key", k));
    }
    w.projects("project_id", s.projects);
    w.labels("labels", s.labels);
    if s.deleted_keys {
        w.deleted_keys("key_id", &super::logs::existing_keys(pg).await?);
    }
    if models {
        if let Some(m) = s.models {
            w.and("has({models:Array(String)}, model_name)");
            w.with(|p| p.strs("models", m));
        }
        if !s.exclude.is_empty() {
            w.and("NOT has({exclude:Array(String)}, model_name)");
            w.with(|p| p.strs("exclude", s.exclude));
        }
    }
    Ok(w)
}

impl ClickHouse {
    pub async fn selection_series(&self, pg: &PgPool, s: &Selection<'_>, step: i64) -> crate::logstore::Result<Vec<LogTotals>> {
        let w = selection(pg, s, true).await?;
        let sql = format!(
            "SELECT '' AS subject, intDiv(toInt64(toUnixTimestamp(ts)), {{step:Int64}}) * {{step:Int64}} AS bucket, {TOTALS},
                    CAST(NULL, 'Nullable(Int32)') AS p95_latency_ms
             FROM request_logs WHERE {} GROUP BY bucket",
            w.sql()
        );
        let rows: Vec<Totals> = self.rows(&sql, w.into_params().num("step", step.max(1))).await?;
        Ok(rows.into_iter().map(LogTotals::from).collect())
    }

    pub async fn model_totals(&self, pg: &PgPool, s: &Selection<'_>) -> crate::logstore::Result<Vec<LogTotals>> {
        let w = selection(pg, s, false).await?;
        let sql = format!(
            "SELECT model_name AS subject, 0 AS bucket, {TOTALS}, toInt32(quantileExactLow(0.95)(latency_ms)) AS p95_latency_ms
             FROM request_logs WHERE {} GROUP BY model_name",
            w.sql()
        );
        let rows: Vec<Totals> = self.rows(&sql, w.into_params()).await?;
        Ok(rows.into_iter().map(LogTotals::from).collect())
    }

    pub async fn selection_totals(&self, pg: &PgPool, s: &Selection<'_>) -> crate::logstore::Result<LogTotals> {
        let w = selection(pg, s, true).await?;
        let sql = format!(
            "SELECT '' AS subject, 0 AS bucket, {TOTALS}, toInt32(quantileExactLow(0.95)(latency_ms)) AS p95_latency_ms
             FROM request_logs WHERE {}",
            w.sql()
        );
        let row: Option<Totals> = self.row(&sql, w.into_params()).await?;
        Ok(row.map(LogTotals::from).unwrap_or_default())
    }

    fn hub_filter(since: DateTime<Utc>, models: &[String], key_id: Option<Uuid>, projects: Option<&[Uuid]>) -> Where {
        let mut w = Where::new();
        w.range("ts", Some(since), None);
        w.and("has({models:Array(String)}, model_name)");
        w.with(|p| p.strs("models", models));
        if let Some(k) = key_id {
            w.and("key_id = {key:UUID}");
            w.with(|p| p.uuid("key", k));
        }
        w.projects("project_id", projects);
        w
    }

    pub async fn model_hours(
        &self,
        since: DateTime<Utc>,
        models: &[String],
        key_id: Option<Uuid>,
        projects: Option<&[Uuid]>,
    ) -> super::Result<Vec<ModelHour>> {
        #[derive(Deserialize)]
        struct Row {
            model_name: String,
            bucket: i64,
            requests: i64,
            errors: i64,
        }
        let w = Self::hub_filter(since, models, key_id, projects);
        let sql = format!(
            "SELECT model_name, toInt64(toUnixTimestamp(toStartOfHour(ts))) AS bucket, count() AS requests, countIf(status_code >= 400) AS errors
             FROM request_logs WHERE {} GROUP BY model_name, bucket",
            w.sql()
        );
        let rows: Vec<Row> = self.rows(&sql, w.into_params()).await?;
        Ok(rows
            .into_iter()
            .map(|r| ModelHour { model_name: r.model_name, bucket: r.bucket, requests: r.requests, errors: r.errors })
            .collect())
    }

    pub async fn model_speeds(
        &self,
        since: DateTime<Utc>,
        models: &[String],
        key_id: Option<Uuid>,
        projects: Option<&[Uuid]>,
    ) -> super::Result<Vec<ModelSpeed>> {
        #[derive(Deserialize)]
        struct Row {
            model_name: String,
            p50_latency_ms: Option<i32>,
            stream_tokens: i64,
            stream_ms: i64,
            whole_tokens: i64,
            whole_ms: i64,
        }
        let mut w = Self::hub_filter(since, models, key_id, projects);
        w.and("status_code < 400");
        let sql = format!(
            "SELECT model_name, toInt32(quantileExactLow(0.5)(latency_ms)) AS p50_latency_ms,
                    sumIf(output_tokens, ttft_ms IS NOT NULL AND latency_ms > ttft_ms) AS stream_tokens,
                    sumIf(toInt64(latency_ms) - ifNull(ttft_ms, 0), ttft_ms IS NOT NULL AND latency_ms > ttft_ms) AS stream_ms,
                    sumIf(output_tokens, ttft_ms IS NULL) AS whole_tokens,
                    sumIf(latency_ms, ttft_ms IS NULL) AS whole_ms
             FROM request_logs WHERE {} GROUP BY model_name",
            w.sql()
        );
        let rows: Vec<Row> = self.rows(&sql, w.into_params()).await?;
        Ok(rows
            .into_iter()
            .map(|r| ModelSpeed {
                model_name: r.model_name,
                p50_latency_ms: r.p50_latency_ms,
                stream_tokens: r.stream_tokens,
                stream_ms: r.stream_ms,
                whole_tokens: r.whole_tokens,
                whole_ms: r.whole_ms,
            })
            .collect())
    }

    pub async fn key_minutes(&self, key_id: Uuid, since: DateTime<Utc>) -> super::Result<Vec<KeyMinute>> {
        #[derive(Deserialize)]
        struct Row {
            minute: i64,
            requests: i64,
            tokens: i64,
        }
        let rows: Vec<Row> = self
            .rows(
                "SELECT toInt64(toUnixTimestamp(toStartOfMinute(ts))) AS minute, count() AS requests,
                        sum(input_tokens + cache_read_tokens + cache_write_tokens + output_tokens) AS tokens
                 FROM request_logs
                 WHERE key_id = {key:UUID} AND ts >= {since:DateTime64(3, 'UTC')} AND parent_id IS NULL
                 GROUP BY minute",
                Params::new().uuid("key", key_id).time("since", since),
            )
            .await?;
        Ok(rows.into_iter().map(|r| KeyMinute { minute: r.minute, requests: r.requests, tokens: r.tokens }).collect())
    }

    pub async fn key_daily_spend(&self, key_id: Uuid, since: DateTime<Utc>) -> super::Result<Vec<(i64, f64)>> {
        #[derive(Deserialize)]
        struct Row {
            day: i64,
            cost_usd: f64,
        }
        let rows: Vec<Row> = self
            .rows(
                "SELECT toInt64(toUnixTimestamp(toStartOfDay(ts))) AS day, sum(cost_usd) AS cost_usd
                 FROM request_logs
                 WHERE key_id = {key:UUID} AND ts >= {since:DateTime64(3, 'UTC')} AND parent_id IS NULL
                 GROUP BY day",
                Params::new().uuid("key", key_id).time("since", since),
            )
            .await?;
        Ok(rows.into_iter().map(|r| (r.day, r.cost_usd)).collect())
    }

    pub async fn key_usage_since(
        &self,
        pg: &PgPool,
        since: DateTime<Utc>,
        projects: Option<&[Uuid]>,
    ) -> crate::logstore::Result<Vec<KeyUsageRow>> {
        #[derive(Deserialize)]
        struct Row {
            key_id: Option<Uuid>,
            requests: i64,
            input_tokens: i64,
            output_tokens: i64,
            cost_usd: f64,
        }
        let mut w = Where::new();
        w.range("ts", Some(since), None);
        w.and("parent_id IS NULL");
        w.projects("project_id", projects);
        let sql = format!(
            "SELECT key_id, count() AS requests, sum(input_tokens + cache_read_tokens + cache_write_tokens) AS input_tokens,
                    sum(output_tokens) AS output_tokens, sum(cost_usd) AS cost_usd
             FROM request_logs WHERE {} GROUP BY key_id ORDER BY cost_usd DESC, requests DESC LIMIT 50",
            w.sql()
        );
        let rows: Vec<Row> = self.rows(&sql, w.into_params()).await?;
        let ids: Vec<Uuid> = rows.iter().filter_map(|r| r.key_id).collect();
        let names: HashMap<Uuid, String> = sqlx::query_as::<_, (Uuid, String)>("SELECT id, name FROM virtual_keys WHERE id = ANY($1)")
            .bind(&ids)
            .fetch_all(pg)
            .await?
            .into_iter()
            .collect();
        Ok(rows
            .into_iter()
            .map(|r| KeyUsageRow {
                key_name: r.key_id.and_then(|k| names.get(&k).cloned()),
                key_id: r.key_id,
                requests: r.requests,
                input_tokens: r.input_tokens,
                output_tokens: r.output_tokens,
                cost_usd: r.cost_usd,
            })
            .collect())
    }
}
