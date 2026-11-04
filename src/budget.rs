//! Periodic budgets of keys and projects: per day, week or month, as UTC calendar periods. A
//! project's budget comes on top of its keys' own, with its own period.
//!
//! Spend in the current period is a Valkey counter shared by all replicas. When the counter is
//! missing (a new period, or Valkey lost its data) it is rebuilt from the request log in the log
//! store (Postgres or ClickHouse, see `logstore`), so budgets survive restarts. When Valkey comes
//! back empty, one replica rebuilds every budget at once (`restore_if_lost`). When neither can be
//! read, `GATEWAY_BUDGET_FAIL_MODE` decides: refuse requests on a budget (`closed`) or let them
//! through (`open`).

use std::{
    collections::HashMap,
    sync::{Arc, LazyLock},
    time::Instant,
};

use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc};
use serde::Serialize;

use uuid::Uuid;

use crate::{
    model::{ProjectRow, VirtualKey},
    state::AppState,
};

/// Whose spend a budget counts: one key's, or that of every request made in a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Spender {
    Key(Uuid),
    Project(Uuid),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum BudgetPeriod {
    Day,
    Week,
    Month,
}

impl BudgetPeriod {
    pub const ALL: [BudgetPeriod; 3] = [Self::Day, Self::Week, Self::Month];

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "day" => Some(Self::Day),
            "week" => Some(Self::Week),
            "month" => Some(Self::Month),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
        }
    }

    /// The calendar period containing `at`. Weeks start on Monday (ISO 8601).
    pub fn window(self, at: DateTime<Utc>) -> Window {
        let day = at.date_naive();
        let midnight = |d: NaiveDate| Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).expect("valid midnight"));
        let (id, start, end) = match self {
            Self::Day => {
                let start = midnight(day);
                (format!("d:{}", day.format("%Y-%m-%d")), start, start + Duration::days(1))
            }
            Self::Week => {
                let monday = day - Duration::days(day.weekday().num_days_from_monday() as i64);
                let iso = day.iso_week();
                let start = midnight(monday);
                (format!("w:{}-W{:02}", iso.year(), iso.week()), start, start + Duration::days(7))
            }
            Self::Month => {
                let first = NaiveDate::from_ymd_opt(day.year(), day.month(), 1).expect("valid month start");
                let next = if day.month() == 12 {
                    NaiveDate::from_ymd_opt(day.year() + 1, 1, 1)
                } else {
                    NaiveDate::from_ymd_opt(day.year(), day.month() + 1, 1)
                }
                .expect("valid next month");
                (format!("m:{}", day.format("%Y-%m")), midnight(first), midnight(next))
            }
        };
        Window { period: self, id, start, end }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Window {
    pub period: BudgetPeriod,
    /// Stable id of this period, e.g. `d:2026-10-25`, `w:2026-W43`, `m:2026-10`.
    #[serde(skip)]
    pub id: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

/// A key's or project's budget for the current period and what has been spent in it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct BudgetStatus {
    pub period: BudgetPeriod,
    pub limit_usd: f64,
    pub spent_usd: f64,
    pub resets_at: DateTime<Utc>,
    /// Spend could not be counted (Valkey and the log store unreachable); `spent_usd` is 0.
    #[serde(skip)]
    pub unknown: bool,
}

impl BudgetStatus {
    pub fn exhausted(&self) -> bool {
        self.spent_usd >= self.limit_usd
    }
}

/// The key's current budget period and spend, or `None` for an unlimited key.
pub async fn status(state: &AppState, key: &VirtualKey) -> Option<BudgetStatus> {
    let period = key.budget_period.as_deref().and_then(BudgetPeriod::parse)?;
    period_status(state, Spender::Key(key.id), key.max_budget_usd?, period, key.spend_reset_at).await
}

/// The project's current budget period and spend, or `None` for an unlimited project.
pub async fn project_status(state: &AppState, project: &ProjectRow) -> Option<BudgetStatus> {
    let period = project.budget_period.as_deref().and_then(BudgetPeriod::parse)?;
    period_status(state, Spender::Project(project.id), project.max_budget_usd?, period, project.spend_reset_at).await
}

async fn period_status(
    state: &AppState,
    who: Spender,
    limit: f64,
    period: BudgetPeriod,
    spend_reset_at: Option<DateTime<Utc>>,
) -> Option<BudgetStatus> {
    let now = Utc::now();
    let w = period.window(now);
    let spent = match state.cache.period_spend(who, &w.id).await {
        Ok(Some(v)) => Some(v),
        Ok(None) => rebuild(state, who, &w, spend_reset_at, None).await,
        // Valkey is unreachable: a recent rebuild stands in, so an outage costs one log store query
        // per spender and period every `RECENT`, not one per request.
        Err(e) => {
            tracing::warn!(error = %e, "could not read spend from valkey; using the request log");
            rebuild(state, who, &w, spend_reset_at, Some(RECENT)).await
        }
    };
    Some(BudgetStatus { period, limit_usd: limit, spent_usd: spent.unwrap_or(0.0), resets_at: w.end, unknown: spent.is_none() })
}

/// How long spend rebuilt from the log store stands in for Valkey while Valkey is unreachable.
const RECENT: std::time::Duration = std::time::Duration::from_secs(30);

/// A spender's budget period: its counter in Valkey.
type Counter = (Spender, String);
/// Spend rebuilt from the log store on this replica, and when.
static REBUILT: LazyLock<std::sync::Mutex<HashMap<Counter, (f64, Instant)>>> = LazyLock::new(Default::default);
/// One rebuild at a time per spender and period: requests arriving meanwhile wait for its result.
type Gate = Arc<tokio::sync::Mutex<()>>;
static REBUILDING: LazyLock<std::sync::Mutex<HashMap<Counter, Gate>>> = LazyLock::new(Default::default);

/// The period's spend from the request log, seeded into Valkey, or `None` when the log store
/// cannot be read. A rebuild finished after this call started is reused, as is one younger than
/// `reuse` (while Valkey is unreachable).
async fn rebuild(
    state: &AppState,
    who: Spender,
    w: &Window,
    spend_reset_at: Option<DateTime<Utc>>,
    reuse: Option<std::time::Duration>,
) -> Option<f64> {
    let started = Instant::now();
    let k = (who, w.id.clone());
    let fresh = |k: &Counter| {
        let rebuilt = REBUILT.lock().expect("rebuilt spend");
        rebuilt.get(k).filter(|(_, at)| *at >= started || reuse.is_some_and(|r| at.elapsed() < r)).map(|(v, _)| *v)
    };
    if let Some(v) = fresh(&k) {
        return Some(v);
    }
    let gate = REBUILDING.lock().expect("spend rebuilds").entry(k.clone()).or_default().clone();
    let _turn = gate.lock().await;
    if let Some(v) = fresh(&k) {
        return Some(v);
    }
    // "Reset spend" moves the start of the current period.
    let since = spend_reset_at.map_or(w.start, |r| r.max(w.start));
    let spent = match crate::logstore::spend_since(state, who, since).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, store = state.logs.name(), "could not read spend from the request log");
            REBUILDING.lock().expect("spend rebuilds").remove(&k);
            return None;
        }
    };
    let ttl = (w.end - Utc::now()).num_seconds().max(0) as u64 + 86_400;
    state.cache.seed_period_spend(who, &w.id, spent, ttl).await;
    {
        let mut rebuilt = REBUILT.lock().expect("rebuilt spend");
        rebuilt.retain(|_, (_, at)| at.elapsed() < RECENT);
        rebuilt.insert(k.clone(), (spent, Instant::now()));
    }
    REBUILDING.lock().expect("spend rebuilds").remove(&k);
    Some(spent)
}

/// Budgets rebuilt at once after Valkey lost its data, at most this many at a time.
const RESTORE_CONCURRENCY: usize = 8;

/// When Valkey has lost its data (restarted without persistence, flushed, failed over to an empty
/// replica), rebuilds the current period of every key and project with a budget, rather than one
/// by one on their next requests. Its marker key is missing then; one replica takes a lock and
/// restores, the others carry on (a request meanwhile rebuilds its own counter). Run with the
/// heartbeat.
pub async fn restore_if_lost(state: &AppState) {
    match state.cache.spend_restored().await {
        Ok(true) | Err(_) => return,
        Ok(false) => {}
    }
    if !state.cache.lock_spend_restore().await {
        return;
    }
    let snap = state.registry.snapshot();
    let mut todo: Vec<(Spender, BudgetPeriod, Option<DateTime<Utc>>)> = Vec::new();
    for k in snap.keys.values() {
        if let (Some(p), Some(_)) = (k.budget_period.as_deref().and_then(BudgetPeriod::parse), k.max_budget_usd) {
            todo.push((Spender::Key(k.id), p, k.spend_reset_at));
        }
    }
    for p in snap.projects.values() {
        if let (Some(period), Some(_)) = (p.budget_period.as_deref().and_then(BudgetPeriod::parse), p.max_budget_usd) {
            todo.push((Spender::Project(p.id), period, p.spend_reset_at));
        }
    }
    let n = todo.len();
    let now = Utc::now();
    use futures::StreamExt;
    let failed = futures::stream::iter(todo)
        .map(|(who, period, reset)| async move { rebuild(state, who, &period.window(now), reset, None).await.is_none() })
        .buffer_unordered(RESTORE_CONCURRENCY)
        .filter(|failed| std::future::ready(*failed))
        .count()
        .await;
    if failed > 0 {
        // The marker stays unset, so the next heartbeat tries again.
        tracing::warn!(budgets = n, failed, "restoring budget counters in valkey was incomplete");
        state.cache.unlock_spend_restore().await;
        return;
    }
    state.cache.mark_spend_restored().await;
    tracing::info!(budgets = n, store = state.logs.name(), "restored budget counters in valkey from the request log");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn windows() {
        // Sunday 2026-10-25 23:59 UTC.
        let t = at("2026-10-25T23:59:00Z");
        let d = BudgetPeriod::Day.window(t);
        assert_eq!((d.id.as_str(), d.start, d.end), ("d:2026-10-25", at("2026-10-25T00:00:00Z"), at("2026-10-26T00:00:00Z")));
        let w = BudgetPeriod::Week.window(t);
        assert_eq!((w.id.as_str(), w.start, w.end), ("w:2026-W43", at("2026-10-19T00:00:00Z"), at("2026-10-26T00:00:00Z")));
        let m = BudgetPeriod::Month.window(t);
        assert_eq!((m.id.as_str(), m.start, m.end), ("m:2026-10", at("2026-10-01T00:00:00Z"), at("2026-11-01T00:00:00Z")));
    }

    #[test]
    fn year_boundaries() {
        let m = BudgetPeriod::Month.window(at("2026-12-31T12:00:00Z"));
        assert_eq!((m.id.as_str(), m.end), ("m:2026-12", at("2027-01-01T00:00:00Z")));
        // Friday 2027-01-01 still belongs to ISO week 53 of 2026, which started Monday 2026-12-28.
        let w = BudgetPeriod::Week.window(at("2027-01-01T08:00:00Z"));
        assert_eq!((w.id.as_str(), w.start), ("w:2026-W53", at("2026-12-28T00:00:00Z")));
    }

    #[sqlx::test]
    async fn a_spent_budget_refuses_requests_while_valkey_is_down(pool: sqlx::PgPool) {
        // Valkey cannot be reached in tests, so spend comes from the request log.
        let state = crate::state::test_state_with_db(pool.clone());
        let mut key = crate::auth::tests::key();
        key.budget_period = Some("day".into());
        key.max_budget_usd = Some(1.0);
        sqlx::query(
            "INSERT INTO request_logs (id, ts, key_id, model_name, api, stream, status_code, latency_ms, cost_usd)
             VALUES ($1, now(), $2, 'm', 'chat', false, 200, 10, 1.5)",
        )
        .bind(Uuid::now_v7())
        .bind(key.id)
        .execute(&pool)
        .await
        .unwrap();
        let err = crate::auth::check_key_limits(&state, &key).await.unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::TOO_MANY_REQUESTS);
        assert!(err.message.contains("budget"), "{}", err.message);

        let mut roomy = crate::auth::tests::key();
        roomy.budget_period = Some("day".into());
        roomy.max_budget_usd = Some(10.0);
        assert!(crate::auth::check_key_limits(&state, &roomy).await.is_ok());
    }

    #[tokio::test]
    async fn budgets_fail_closed_when_spend_cannot_be_counted() {
        // Valkey and Postgres both point at closed ports.
        let mut state = crate::state::test_state();
        let mut key = crate::auth::tests::key();
        key.budget_period = Some("day".into());
        key.max_budget_usd = Some(100.0);
        let err = crate::auth::check_key_limits(&state, &key).await.unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
        // Keys without a budget are not affected.
        assert!(crate::auth::check_key_limits(&state, &crate::auth::tests::key()).await.is_ok());

        state.inner_mut().cfg.budget_fail_open = true;
        assert!(crate::auth::check_key_limits(&state, &key).await.is_ok());
    }
}
