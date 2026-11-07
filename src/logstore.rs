//! Where the request log, logged bodies and the audit log live: Postgres (the default), or
//! ClickHouse when `GATEWAY_CLICKHOUSE_URL` is set. Everything that writes or reads these logs, or
//! rebuilds budget spend from them, goes through here. With ClickHouse, Postgres holds only
//! configuration: nothing per request is written to it.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::{
    accounting::RequestRecord,
    budget::Spender,
    capture::Stored,
    clickhouse::{self, ClickHouse},
    db::{
        self, AuditEntry, BodiesOf, BodiesRow, KeyMinute, KeyUsageRow, LabelUse, LogFilter, LogRow, LogTotals, ModelHour, ModelSpeed,
        ToolUse,
    },
    state::AppState,
};

pub enum LogStore {
    Postgres,
    ClickHouse(Arc<ClickHouse>),
}

impl LogStore {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::ClickHouse(_) => "clickhouse",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Postgres(#[from] sqlx::Error),
    #[error(transparent)]
    ClickHouse(#[from] clickhouse::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// A batch of entries. With Postgres, spend is also rolled into the keys.
pub async fn write_request_batch(state: &AppState, batch: &[RequestRecord]) -> Result<()> {
    match &state.logs {
        LogStore::Postgres => Ok(db::write_request_batch(&state.db, batch).await?),
        LogStore::ClickHouse(ch) => Ok(ch.write_request_batch(batch).await?),
    }
}

/// Logged bodies: entry id, time, key, project, and what is stored.
pub type BodyRow = (Uuid, DateTime<Utc>, Option<Uuid>, Option<Uuid>, Stored);

/// Stores the bodies of logged requests, once their entries are written. When that fails, their
/// entries read as dropped.
pub async fn write_body_batch(state: &AppState, rows: Vec<BodyRow>) -> Result<()> {
    match &state.logs {
        LogStore::Postgres => {
            let ids: Vec<Uuid> = rows.iter().map(|r| r.0).collect();
            let rows = rows.into_iter().map(|(id, ts, key, _, s)| (id, ts, key, s)).collect();
            if let Err(e) = db::write_body_batch(&state.db, rows).await {
                if let Err(e) = db::mark_bodies_dropped(&state.db, &ids).await {
                    tracing::warn!(error = %e, "could not mark the logged bodies as dropped");
                }
                return Err(e.into());
            }
            Ok(())
        }
        LogStore::ClickHouse(ch) => Ok(ch.write_body_batch(rows).await?),
    }
}

pub async fn recent_logs(state: &AppState, f: &LogFilter<'_>) -> Result<Vec<LogRow>> {
    match &state.logs {
        LogStore::Postgres => Ok(db::recent_logs(&state.db, f).await?),
        LogStore::ClickHouse(ch) => ch.recent_logs(&state.db, f).await,
    }
}

pub async fn log_bodies(state: &AppState, id: Uuid) -> Result<Option<BodiesRow>> {
    match &state.logs {
        LogStore::Postgres => Ok(db::log_bodies(&state.db, id).await?),
        LogStore::ClickHouse(ch) => Ok(ch.log_bodies(id).await?),
    }
}

pub async fn turn_links(state: &AppState, row: &BodiesRow, projects: Option<&[Uuid]>) -> Result<(Option<Uuid>, Option<Uuid>)> {
    match &state.logs {
        LogStore::Postgres => Ok(db::turn_links(&state.db, row, projects).await?),
        LogStore::ClickHouse(ch) => Ok(ch.turn_links(row, projects).await?),
    }
}

pub async fn recent_tools(state: &AppState, limit: i64, projects: Option<&[Uuid]>) -> Result<Vec<ToolUse>> {
    match &state.logs {
        LogStore::Postgres => Ok(db::recent_tools(&state.db, limit, projects).await?),
        LogStore::ClickHouse(ch) => Ok(ch.recent_tools(limit, projects).await?),
    }
}

pub async fn count_bodies(state: &AppState, of: BodiesOf) -> Result<i64> {
    match &state.logs {
        LogStore::Postgres => Ok(db::count_bodies(&state.db, of).await?),
        LogStore::ClickHouse(ch) => Ok(ch.count_bodies(of).await?),
    }
}

pub async fn delete_bodies(state: &AppState, of: BodiesOf) -> Result<u64> {
    match &state.logs {
        LogStore::Postgres => Ok(db::delete_bodies(&state.db, of).await?),
        LogStore::ClickHouse(ch) => Ok(ch.delete_bodies(of).await?),
    }
}

pub async fn recent_labels(state: &AppState, limit: i64, projects: Option<&[Uuid]>) -> Result<Vec<LabelUse>> {
    match &state.logs {
        LogStore::Postgres => Ok(db::recent_labels(&state.db, limit, projects).await?),
        LogStore::ClickHouse(ch) => Ok(ch.recent_labels(limit, projects).await?),
    }
}

pub async fn insert_audit(state: &AppState, e: &AuditEntry) -> Result<()> {
    match &state.logs {
        LogStore::Postgres => Ok(db::insert_audit(&state.db, e).await?),
        LogStore::ClickHouse(ch) => Ok(ch.insert_audit(e).await?),
    }
}

pub async fn list_audit(
    state: &AppState,
    limit: i64,
    target_type: Option<&str>,
    search: Option<&str>,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
) -> Result<Vec<AuditEntry>> {
    match &state.logs {
        LogStore::Postgres => Ok(db::list_audit(&state.db, limit, target_type, search, since, until).await?),
        LogStore::ClickHouse(ch) => Ok(ch.list_audit(limit, target_type, search, since, until).await?),
    }
}

/// What the Metrics page's queries share.
pub use clickhouse::Selection;

pub async fn selection_series(state: &AppState, s: &Selection<'_>, step: i64) -> Result<Vec<LogTotals>> {
    match &state.logs {
        LogStore::Postgres => Ok(db::selection_series(
            &state.db,
            s.since,
            s.until,
            step,
            s.key_id,
            s.deleted_keys,
            s.models,
            s.exclude,
            s.projects,
            s.labels,
            s.api,
        )
        .await?),
        LogStore::ClickHouse(ch) => ch.selection_series(&state.db, s, step).await,
    }
}

pub async fn group_totals(state: &AppState, s: &Selection<'_>, group: db::Group) -> Result<Vec<LogTotals>> {
    match &state.logs {
        LogStore::Postgres => Ok(db::group_totals(
            &state.db,
            group,
            s.since,
            s.until,
            s.key_id,
            s.deleted_keys,
            s.models,
            s.exclude,
            s.projects,
            s.labels,
            s.api,
        )
        .await?),
        LogStore::ClickHouse(ch) => ch.group_totals(&state.db, s, group).await,
    }
}

pub async fn billing_totals(state: &AppState, s: &Selection<'_>) -> Result<Vec<db::BillingTotals>> {
    match &state.logs {
        LogStore::Postgres => {
            Ok(db::billing_totals(&state.db, s.since, s.until, s.key_id, s.deleted_keys, s.models, s.exclude, s.projects, s.labels).await?)
        }
        LogStore::ClickHouse(ch) => ch.billing_totals(&state.db, s).await,
    }
}

pub async fn selection_totals(state: &AppState, s: &Selection<'_>) -> Result<LogTotals> {
    match &state.logs {
        LogStore::Postgres => Ok(db::selection_totals(
            &state.db,
            s.since,
            s.until,
            s.key_id,
            s.deleted_keys,
            s.models,
            s.exclude,
            s.projects,
            s.labels,
            s.api,
        )
        .await?),
        LogStore::ClickHouse(ch) => ch.selection_totals(&state.db, s).await,
    }
}

pub async fn model_hours(
    state: &AppState,
    since: DateTime<Utc>,
    models: &[String],
    key_id: Option<Uuid>,
    projects: Option<&[Uuid]>,
) -> Result<Vec<ModelHour>> {
    match &state.logs {
        LogStore::Postgres => Ok(db::model_hours(&state.db, since, models, key_id, projects).await?),
        LogStore::ClickHouse(ch) => Ok(ch.model_hours(since, models, key_id, projects).await?),
    }
}

pub async fn model_speeds(
    state: &AppState,
    since: DateTime<Utc>,
    models: &[String],
    key_id: Option<Uuid>,
    projects: Option<&[Uuid]>,
) -> Result<Vec<ModelSpeed>> {
    match &state.logs {
        LogStore::Postgres => Ok(db::model_speeds(&state.db, since, models, key_id, projects).await?),
        LogStore::ClickHouse(ch) => Ok(ch.model_speeds(since, models, key_id, projects).await?),
    }
}

pub async fn key_minutes(state: &AppState, key_id: Uuid, since: DateTime<Utc>) -> Result<Vec<KeyMinute>> {
    match &state.logs {
        LogStore::Postgres => Ok(db::key_minutes(&state.db, key_id, since).await?),
        LogStore::ClickHouse(ch) => Ok(ch.key_minutes(key_id, since).await?),
    }
}

pub async fn key_daily_spend(state: &AppState, key_id: Uuid, since: DateTime<Utc>) -> Result<Vec<(i64, f64)>> {
    match &state.logs {
        LogStore::Postgres => Ok(db::key_daily_spend(&state.db, key_id, since).await?),
        LogStore::ClickHouse(ch) => Ok(ch.key_daily_spend(key_id, since).await?),
    }
}

pub async fn key_usage_since(state: &AppState, since: DateTime<Utc>, projects: Option<&[Uuid]>) -> Result<Vec<KeyUsageRow>> {
    match &state.logs {
        LogStore::Postgres => Ok(db::key_usage_since(&state.db, since, projects).await?),
        LogStore::ClickHouse(ch) => ch.key_usage_since(&state.db, since, projects).await,
    }
}

/// What a key or project spent since `since`, from the durable log (rebuilds budget counters).
pub async fn spend_since(state: &AppState, who: Spender, since: DateTime<Utc>) -> Result<f64> {
    match (&state.logs, who) {
        (LogStore::Postgres, Spender::Key(id)) => Ok(db::key_spend_since(&state.db, id, since).await?),
        (LogStore::Postgres, Spender::Project(id)) => Ok(db::project_spend_since(&state.db, id, since).await?),
        (LogStore::ClickHouse(ch), who) => Ok(ch.spend_since(who, since).await?),
    }
}

/// Retention of the three logs (see `db::prune_logs`). One replica at a time.
pub async fn prune(state: &AppState, request_days: u32, audit_days: u32, body_days: u32) -> Result<(u64, u64, u64)> {
    match &state.logs {
        LogStore::Postgres => Ok(db::prune_logs(&state.db, request_days, audit_days, body_days).await?),
        LogStore::ClickHouse(ch) => {
            const LOCK_ID: i64 = 0x6777_6c6f_6773; // "gwlogs", as in Postgres
            match db::with_lock(&state.db, LOCK_ID, clickhouse::prune(ch, request_days, audit_days, body_days)).await? {
                Some(r) => Ok(r?),
                None => Ok((0, 0, 0)),
            }
        }
    }
}
