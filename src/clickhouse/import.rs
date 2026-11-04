//! The one-time copy of the current budget periods' request log from Postgres, the first time a
//! gateway starts with ClickHouse, so budgets rebuilt from ClickHouse include what was spent
//! before the switch. Older history stays in Postgres.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use sqlx::{PgPool, Row, postgres::PgRow};
use uuid::Uuid;

use super::{ClickHouse, Params, logs::LogInsert};
use crate::budget::BudgetPeriod;

/// Rows per read from Postgres and insert into ClickHouse.
const BATCH: i64 = 10_000;
/// Serialises replicas that start together: one copies, the others wait for it.
const LOCK_ID: i64 = 0x6777_6368_696d; // "gwchim"

/// Applies the ClickHouse migrations and, once, copies the request log since the start of the
/// earliest current budget period (this month, or this ISO week when it began last month).
pub async fn import_from_postgres(ch: &ClickHouse, pg: &PgPool) -> Result<(), String> {
    let mut conn = pg.acquire().await.map_err(|e| format!("postgres: {e}"))?.detach();
    sqlx::query("SELECT pg_advisory_lock($1)").bind(LOCK_ID).execute(&mut conn).await.map_err(|e| format!("postgres lock: {e}"))?;
    let result = async {
        ch.migrate().await.map_err(|e| e.to_string())?;
        #[derive(Deserialize)]
        struct Done {
            n: u64,
        }
        let done = ch.row::<Done>("SELECT count() AS n FROM _pg_import", Params::new()).await.map_err(|e| e.to_string())?;
        if done.is_some_and(|d| d.n > 0) {
            return Ok(());
        }
        copy(ch, pg).await
    }
    .await;
    let _ = sqlx::Connection::close(conn).await;
    result
}

async fn copy(ch: &ClickHouse, pg: &PgPool) -> Result<(), String> {
    let now = Utc::now();
    let since = BudgetPeriod::ALL.iter().map(|p| p.window(now).start).min().expect("periods");
    // A copy cut short left only copied rows behind: no request was served with ClickHouse yet.
    for t in ["request_logs", "spend_hours"] {
        ch.execute(&format!("TRUNCATE TABLE {t}"), Params::new()).await.map_err(|e| e.to_string())?;
    }
    tracing::info!(%since, "copying the current budget periods' request log from postgres to clickhouse");
    let (mut after_ts, mut after_id, mut total) = (since, Uuid::nil(), 0u64);
    loop {
        let rows: Vec<PgRow> = sqlx::query(
            "SELECT id, ts, key_id, project_id, model_name, deployment_id, provider, api, stream, status_code, latency_ms, ttft_ms,
                    input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, cost_usd, attempts, error, parent_id,
                    labels, guardrails, guardrail_cost_usd, searches, search_cost_usd, audio_seconds, characters, bodies, tool,
                    task_state, context_id
             FROM request_logs WHERE (ts, id) > ($1, $2) ORDER BY ts, id LIMIT $3",
        )
        .bind(after_ts)
        .bind(after_id)
        .bind(BATCH)
        .fetch_all(pg)
        .await
        .map_err(|e| format!("reading the postgres request log: {e}"))?;
        let Some(last) = rows.last() else { break };
        after_ts = last.try_get("ts").map_err(|e| e.to_string())?;
        after_id = last.try_get("id").map_err(|e| e.to_string())?;
        let batch: Vec<LogInsert> = rows.iter().map(to_insert).collect::<Result<_, sqlx::Error>>().map_err(|e| e.to_string())?;
        ch.insert("request_logs", &batch, false).await.map_err(|e| e.to_string())?;
        total += batch.len() as u64;
        if (rows.len() as i64) < BATCH {
            break;
        }
    }
    ch.execute(
        "INSERT INTO _pg_import (since, rows) VALUES ({since:DateTime64(3, 'UTC')}, {rows:UInt64})",
        Params::new().time("since", since).num("rows", total),
    )
    .await
    .map_err(|e| e.to_string())?;
    tracing::info!(rows = total, "copied the request log to clickhouse");
    Ok(())
}

fn to_insert(r: &PgRow) -> Result<LogInsert, sqlx::Error> {
    let ts: DateTime<Utc> = r.try_get("ts")?;
    let n = |c: &str| r.try_get::<i64, _>(c).map(|v| v.max(0) as u64);
    let i = |c: &str| r.try_get::<i32, _>(c).map(|v| v.max(0) as u32);
    Ok(LogInsert {
        id: r.try_get("id")?,
        ts: super::time(ts),
        key_id: r.try_get("key_id")?,
        project_id: r.try_get("project_id")?,
        model_name: r.try_get("model_name")?,
        deployment_id: r.try_get("deployment_id")?,
        provider: r.try_get("provider")?,
        api: r.try_get("api")?,
        stream: r.try_get("stream")?,
        status_code: i("status_code")?.min(u16::MAX as u32) as u16,
        latency_ms: i("latency_ms")?,
        ttft_ms: r.try_get::<Option<i32>, _>("ttft_ms")?.map(|v| v.max(0) as u32),
        input_tokens: n("input_tokens")?,
        output_tokens: n("output_tokens")?,
        cache_read_tokens: n("cache_read_tokens")?,
        cache_write_tokens: n("cache_write_tokens")?,
        cost_usd: r.try_get("cost_usd")?,
        attempts: i("attempts")?,
        error: r.try_get("error")?,
        parent_id: r.try_get("parent_id")?,
        labels: r.try_get::<Option<serde_json::Value>, _>("labels")?.unwrap_or_else(|| serde_json::json!({})),
        guardrails: r.try_get::<Option<serde_json::Value>, _>("guardrails")?.map(|g| g.to_string()),
        guardrail_cost_usd: r.try_get("guardrail_cost_usd")?,
        searches: n("searches")?,
        search_cost_usd: r.try_get("search_cost_usd")?,
        audio_seconds: n("audio_seconds")?,
        characters: n("characters")?,
        // Bodies stay in Postgres: the copied entries read as dropped (no body row).
        bodies: r.try_get::<Option<String>, _>("bodies")?.map(|b| if b == "deleted" { b } else { "dropped".into() }),
        tool: r.try_get("tool")?,
        task_state: r.try_get("task_state")?,
        context_id: r.try_get("context_id")?,
    })
}
