//! Series and totals aggregated from the request log, for metrics, the model hub and key usage.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

/// Request-log aggregates for the Metrics page: per subject (a model, a deployment, or one row
/// for everything), or per subject and time bucket.
#[derive(Debug, Default, Clone)]
pub struct LogTotals {
    pub subject: String,
    /// Start of the time bucket (Unix seconds); 0 for totals.
    pub bucket: i64,
    pub requests: i64,
    pub errors: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub cost_usd: f64,
    pub latency_ms_sum: i64,
    pub ttft_ms_sum: i64,
    pub ttft_count: i64,
    pub p95_latency_ms: Option<i32>,
}

/// Per time bucket of `step` seconds, from `since` up to `until`, over the entries of `models`
/// (all when `None`) except those of `exclude`, optionally for one key (or with `deleted_keys` the
/// keys that no longer exist, whose id the log keeps) and for the entries with all of `labels` (a
/// JSON object), as in every query below.
pub async fn selection_series(
    pool: &PgPool,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    step: i64,
    key_id: Option<Uuid>,
    deleted_keys: bool,
    models: Option<&[String]>,
    exclude: &[String],
    projects: Option<&[Uuid]>,
    labels: Option<&serde_json::Value>,
) -> sqlx::Result<Vec<LogTotals>> {
    sqlx::query_as!(
        LogTotals,
        r#"SELECT '' AS "subject!",
                  (floor(extract(epoch FROM ts) / $2) * $2)::bigint AS "bucket!",
                  count(*) AS "requests!", count(*) FILTER (WHERE status_code >= 400) AS "errors!",
                  sum(input_tokens)::bigint AS "input_tokens!", sum(output_tokens)::bigint AS "output_tokens!",
                  sum(cache_read_tokens)::bigint AS "cache_read_tokens!", sum(cache_write_tokens)::bigint AS "cache_write_tokens!",
                  sum(cost_usd) AS "cost_usd!", sum(latency_ms)::bigint AS "latency_ms_sum!",
                  coalesce(sum(ttft_ms), 0)::bigint AS "ttft_ms_sum!", count(ttft_ms) AS "ttft_count!",
                  NULL::int AS "p95_latency_ms?"
           FROM request_logs
           WHERE ts >= $1 AND ts < $4 AND ($3::uuid IS NULL OR key_id = $3) AND ($5::uuid[] IS NULL OR project_id = ANY($5))
             AND ($6::jsonb IS NULL OR labels @> $6)
             AND (NOT $7 OR (key_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM virtual_keys k WHERE k.id = request_logs.key_id)))
             AND ($8::text[] IS NULL OR model_name = ANY($8)) AND NOT (model_name = ANY($9))
           GROUP BY 2"#,
        since,
        step as f64,
        key_id,
        until,
        projects,
        labels,
        deleted_keys,
        models,
        exclude,
    )
    .fetch_all(pool)
    .await
}

/// What the Metrics table's rows total: a model, a project or a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Model,
    Project,
    Key,
}

impl Group {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Project => "project",
            Self::Key => "key",
        }
    }
}

/// Totals per `group`, with exact latency percentiles. The subject is the model's name, or the
/// project's or key's id, empty for the entries without one. Filtered as `selection_series`.
pub async fn group_totals(
    pool: &PgPool,
    group: Group,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    key_id: Option<Uuid>,
    deleted_keys: bool,
    models: Option<&[String]>,
    exclude: &[String],
    projects: Option<&[Uuid]>,
    labels: Option<&serde_json::Value>,
) -> sqlx::Result<Vec<LogTotals>> {
    sqlx::query_as!(
        LogTotals,
        r#"SELECT (CASE $7 WHEN 'project' THEN coalesce(project_id::text, '') WHEN 'key' THEN coalesce(key_id::text, '')
                   ELSE model_name END) AS "subject!", 0::bigint AS "bucket!",
                  count(*) AS "requests!", count(*) FILTER (WHERE status_code >= 400) AS "errors!",
                  sum(input_tokens)::bigint AS "input_tokens!", sum(output_tokens)::bigint AS "output_tokens!",
                  sum(cache_read_tokens)::bigint AS "cache_read_tokens!", sum(cache_write_tokens)::bigint AS "cache_write_tokens!",
                  sum(cost_usd) AS "cost_usd!", sum(latency_ms)::bigint AS "latency_ms_sum!",
                  coalesce(sum(ttft_ms), 0)::bigint AS "ttft_ms_sum!", count(ttft_ms) AS "ttft_count!",
                  percentile_disc(0.95) WITHIN GROUP (ORDER BY latency_ms) AS "p95_latency_ms?"
           FROM request_logs
           WHERE ts >= $1 AND ts < $3 AND ($2::uuid IS NULL OR key_id = $2) AND ($4::uuid[] IS NULL OR project_id = ANY($4))
             AND ($5::jsonb IS NULL OR labels @> $5)
             AND (NOT $6 OR (key_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM virtual_keys k WHERE k.id = request_logs.key_id)))
             AND ($8::text[] IS NULL OR model_name = ANY($8)) AND NOT (model_name = ANY($9))
           GROUP BY 1"#,
        since,
        key_id,
        until,
        projects,
        labels,
        deleted_keys,
        group.as_str(),
        models,
        exclude,
    )
    .fetch_all(pool)
    .await
}

/// Totals over the entries of `models` (all when `None`) except those of `exclude`, with exact
/// latency percentiles.
pub async fn selection_totals(
    pool: &PgPool,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    key_id: Option<Uuid>,
    deleted_keys: bool,
    models: Option<&[String]>,
    exclude: &[String],
    projects: Option<&[Uuid]>,
    labels: Option<&serde_json::Value>,
) -> sqlx::Result<LogTotals> {
    sqlx::query_as!(
        LogTotals,
        r#"SELECT '' AS "subject!", 0::bigint AS "bucket!",
                  count(*) AS "requests!", count(*) FILTER (WHERE status_code >= 400) AS "errors!",
                  coalesce(sum(input_tokens), 0)::bigint AS "input_tokens!", coalesce(sum(output_tokens), 0)::bigint AS "output_tokens!",
                  coalesce(sum(cache_read_tokens), 0)::bigint AS "cache_read_tokens!",
                  coalesce(sum(cache_write_tokens), 0)::bigint AS "cache_write_tokens!",
                  coalesce(sum(cost_usd), 0) AS "cost_usd!", coalesce(sum(latency_ms), 0)::bigint AS "latency_ms_sum!",
                  coalesce(sum(ttft_ms), 0)::bigint AS "ttft_ms_sum!", count(ttft_ms) AS "ttft_count!",
                  percentile_disc(0.95) WITHIN GROUP (ORDER BY latency_ms) AS "p95_latency_ms?"
           FROM request_logs
           WHERE ts >= $1 AND ts < $4 AND ($2::uuid IS NULL OR key_id = $2) AND NOT (model_name = ANY($3))
             AND ($5::uuid[] IS NULL OR project_id = ANY($5)) AND ($6::jsonb IS NULL OR labels @> $6)
             AND (NOT $7 OR (key_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM virtual_keys k WHERE k.id = request_logs.key_id)))
             AND ($8::text[] IS NULL OR model_name = ANY($8))"#,
        since,
        key_id,
        exclude,
        until,
        projects,
        labels,
        deleted_keys,
        models,
    )
    .fetch_one(pool)
    .await
}

/// One model's requests in one hour, for the model hub's status bar.
#[derive(Debug)]
pub struct ModelHour {
    pub model_name: String,
    /// Start of the hour (Unix seconds).
    pub bucket: i64,
    pub requests: i64,
    pub errors: i64,
}

/// Each of `models`' requests per hour since `since`, of one key or within `projects` when given.
pub async fn model_hours(
    pool: &PgPool,
    since: DateTime<Utc>,
    models: &[String],
    key_id: Option<Uuid>,
    projects: Option<&[Uuid]>,
) -> sqlx::Result<Vec<ModelHour>> {
    sqlx::query_as!(
        ModelHour,
        r#"SELECT model_name, (floor(extract(epoch FROM ts) / 3600) * 3600)::bigint AS "bucket!",
                  count(*) AS "requests!", count(*) FILTER (WHERE status_code >= 400) AS "errors!"
           FROM request_logs
           WHERE ts >= $1 AND model_name = ANY($2) AND ($3::uuid IS NULL OR key_id = $3) AND ($4::uuid[] IS NULL OR project_id = ANY($4))
           GROUP BY 1, 2"#,
        since,
        models,
        key_id,
        projects,
    )
    .fetch_all(pool)
    .await
}

/// How fast one model answered its successful requests, for the model hub.
#[derive(Debug)]
pub struct ModelSpeed {
    pub model_name: String,
    pub p50_latency_ms: Option<i32>,
    /// Output tokens and the milliseconds spent generating them (after the first token) in
    /// streamed answers; non-streamed ones count their whole latency, in `whole_*`.
    pub stream_tokens: i64,
    pub stream_ms: i64,
    pub whole_tokens: i64,
    pub whole_ms: i64,
}

/// The successful requests' median latency and generation speed per model, filtered as `model_hours`.
pub async fn model_speeds(
    pool: &PgPool,
    since: DateTime<Utc>,
    models: &[String],
    key_id: Option<Uuid>,
    projects: Option<&[Uuid]>,
) -> sqlx::Result<Vec<ModelSpeed>> {
    sqlx::query_as!(
        ModelSpeed,
        r#"SELECT model_name,
                  percentile_disc(0.5) WITHIN GROUP (ORDER BY latency_ms) AS "p50_latency_ms?",
                  coalesce(sum(output_tokens) FILTER (WHERE ttft_ms IS NOT NULL AND latency_ms > ttft_ms), 0)::bigint AS "stream_tokens!",
                  coalesce(sum(latency_ms - ttft_ms) FILTER (WHERE ttft_ms IS NOT NULL AND latency_ms > ttft_ms), 0)::bigint AS "stream_ms!",
                  coalesce(sum(output_tokens) FILTER (WHERE ttft_ms IS NULL), 0)::bigint AS "whole_tokens!",
                  coalesce(sum(latency_ms) FILTER (WHERE ttft_ms IS NULL), 0)::bigint AS "whole_ms!"
           FROM request_logs
           WHERE ts >= $1 AND model_name = ANY($2) AND ($3::uuid IS NULL OR key_id = $3) AND ($4::uuid[] IS NULL OR project_id = ANY($4))
             AND status_code < 400
           GROUP BY 1"#,
        since,
        models,
        key_id,
        projects,
    )
    .fetch_all(pool)
    .await
}

/// One minute of a key's traffic, counted as its rate limits count it.
#[derive(Debug)]
pub struct KeyMinute {
    /// Start of the minute (Unix seconds).
    pub minute: i64,
    pub requests: i64,
    /// Input (cache reads and writes included) and output tokens.
    pub tokens: i64,
}

/// A key's requests and tokens per minute since `since`, for the minutes it had traffic. The
/// calls of a routed request count through the router's entry, as for its limits.
pub async fn key_minutes(pool: &PgPool, key_id: Uuid, since: DateTime<Utc>) -> sqlx::Result<Vec<KeyMinute>> {
    sqlx::query_as!(
        KeyMinute,
        r#"SELECT (floor(extract(epoch FROM ts) / 60) * 60)::bigint AS "minute!", count(*) AS "requests!",
                  coalesce(sum(input_tokens + cache_read_tokens + cache_write_tokens + output_tokens), 0)::bigint AS "tokens!"
           FROM request_logs
           WHERE key_id = $1 AND ts >= $2 AND parent_id IS NULL
           GROUP BY 1"#,
        key_id,
        since,
    )
    .fetch_all(pool)
    .await
}

/// A key's spend per UTC day since `since`, for the days it spent anything, counted as its budget counts it.
pub async fn key_daily_spend(pool: &PgPool, key_id: Uuid, since: DateTime<Utc>) -> sqlx::Result<Vec<(i64, f64)>> {
    let rows = sqlx::query!(
        r#"SELECT (floor(extract(epoch FROM ts) / 86400) * 86400)::bigint AS "day!", coalesce(sum(cost_usd), 0) AS "cost_usd!"
           FROM request_logs
           WHERE key_id = $1 AND ts >= $2 AND parent_id IS NULL
           GROUP BY 1"#,
        key_id,
        since,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| (r.day, r.cost_usd)).collect())
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct KeyUsageRow {
    #[schema(required)]
    pub key_id: Option<Uuid>,
    #[schema(required)]
    pub key_name: Option<String>,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_usd: f64,
}

pub async fn key_usage_since(pool: &PgPool, since: DateTime<Utc>, projects: Option<&[Uuid]>) -> sqlx::Result<Vec<KeyUsageRow>> {
    sqlx::query_as!(
        KeyUsageRow,
        r#"SELECT l.key_id, k.name AS "key_name?", count(*) AS "requests!",
                  COALESCE(sum(l.input_tokens + l.cache_read_tokens + l.cache_write_tokens), 0)::bigint AS "input_tokens!",
                  COALESCE(sum(l.output_tokens), 0)::bigint AS "output_tokens!",
                  COALESCE(sum(l.cost_usd), 0) AS "cost_usd!"
           FROM request_logs l LEFT JOIN virtual_keys k ON k.id = l.key_id
           WHERE l.ts >= $1 AND l.parent_id IS NULL AND ($2::uuid[] IS NULL OR l.project_id = ANY($2))
           GROUP BY l.key_id, k.name ORDER BY 6 DESC, 3 DESC LIMIT 50"#,
        since,
        projects,
    )
    .fetch_all(pool)
    .await
}
