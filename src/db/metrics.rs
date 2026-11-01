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

/// Totals per model, with exact latency percentiles.
pub async fn model_totals(
    pool: &PgPool,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    key_id: Option<Uuid>,
    deleted_keys: bool,
    projects: Option<&[Uuid]>,
    labels: Option<&serde_json::Value>,
) -> sqlx::Result<Vec<LogTotals>> {
    sqlx::query_as!(
        LogTotals,
        r#"SELECT model_name AS "subject!", 0::bigint AS "bucket!",
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
           GROUP BY model_name"#,
        since,
        key_id,
        until,
        projects,
        labels,
        deleted_keys,
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

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct KeyUsageRow {
    pub key_id: Option<Uuid>,
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
