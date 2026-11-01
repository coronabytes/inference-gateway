//! Metrics and usage per key, aggregated from the request log.

use std::collections::{BTreeSet, HashMap};

use axum::{
    Extension, Json,
    extract::{Query, State},
    http::StatusCode,
};
use chrono::{DateTime, Datelike, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{R, label_filter};
use crate::{admin_auth::Access, db, error::AdminError, state::AppState};

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct RangeQuery {
    range: Option<String>,
    /// Metrics only: just this virtual key's traffic.
    key_id: Option<Uuid>,
    /// Metrics only: just the traffic of this project's keys.
    project_id: Option<Uuid>,
    /// Metrics only: just the traffic of keys that were deleted since.
    deleted_keys: Option<bool>,
    /// Metrics only: just the requests with all of these labels, as `tenant=…,feature=core`.
    labels: Option<String>,
    /// Metrics only: the series and totals of just these models, comma-separated; omitted means
    /// every model except auto routers, whose entries already count their calls.
    models: Option<String>,
    /// Metrics with `range=custom`: first and last day, inclusive (UTC).
    from: Option<chrono::NaiveDate>,
    to: Option<chrono::NaiveDate>,
}

/// The time a metrics view covers, split into buckets of `step` seconds starting at `stamps`.
#[derive(Debug, PartialEq)]
struct Window {
    label: &'static str,
    step: i64,
    stamps: Vec<i64>,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
}

const DAY: i64 = 86_400;
/// Custom ranges are shown with daily buckets; this many days at most.
const MAX_CUSTOM_DAYS: i64 = 400;

fn window(q: &RangeQuery, now: DateTime<Utc>) -> R<Window> {
    let ts = |secs: i64| DateTime::from_timestamp(secs, 0).unwrap_or(now);
    let midnight = |d: chrono::NaiveDate| d.and_hms_opt(0, 0, 0).map(|t| t.and_utc().timestamp()).unwrap_or(0);
    // Hourly buckets up to two days, daily ones beyond.
    let step_for = |span: i64| if span <= 2 * DAY { 3600 } else { DAY };
    let (label, step, since, until) = match q.range.as_deref() {
        None | Some("1h") => ("1h", 60, now.timestamp() - 59 * 60, now.timestamp()),
        Some("24h") => ("24h", 3600, now.timestamp() - 23 * 3600, now.timestamp()),
        Some("7d") => ("7d", 3600, now.timestamp() - 167 * 3600, now.timestamp()),
        Some("month") => {
            let first = midnight(now.date_naive().with_day(1).unwrap_or(now.date_naive()));
            ("month", step_for(now.timestamp() - first), first, now.timestamp())
        }
        Some("custom") => {
            let (Some(from), Some(to)) = (q.from, q.to) else {
                return Err(AdminError::bad_request("a custom range needs from and to dates (YYYY-MM-DD)"));
            };
            let days = (to - from).num_days() + 1;
            if days < 1 {
                return Err(AdminError::bad_request("the range ends before it starts"));
            }
            if days > MAX_CUSTOM_DAYS {
                return Err(AdminError::bad_request(format!("a custom range spans at most {MAX_CUSTOM_DAYS} days")));
            }
            let (start, end) = (midnight(from), midnight(to) + DAY);
            ("custom", step_for(end - start), start, end - 1)
        }
        Some(_) => return Err(AdminError::bad_request("range must be 1h, 24h, 7d, month or custom")),
    };
    // Buckets are aligned to their size (UTC midnight for days); the last one holds `until`.
    let first = since - since.rem_euclid(step);
    let stamps: Vec<i64> = (0..).map(|i| first + i * step).take_while(|t| *t <= until).collect();
    Ok(Window { label, step, stamps, since: ts(first), until: ts(until + 1) })
}

#[derive(Serialize, Default)]
struct Totals {
    requests: i64,
    errors: i64,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    cost_usd: f64,
    avg_latency_ms: Option<f64>,
    p95_latency_ms: Option<i32>,
    avg_ttft_ms: Option<f64>,
}

fn totals(t: Option<&db::LogTotals>) -> Totals {
    let Some(t) = t else { return Totals::default() };
    Totals {
        requests: t.requests,
        errors: t.errors,
        input_tokens: t.input_tokens,
        output_tokens: t.output_tokens,
        cache_read_tokens: t.cache_read_tokens,
        cache_write_tokens: t.cache_write_tokens,
        cost_usd: t.cost_usd,
        avg_latency_ms: (t.requests > 0).then(|| t.latency_ms_sum as f64 / t.requests as f64),
        p95_latency_ms: t.p95_latency_ms,
        avg_ttft_ms: (t.ttft_count > 0).then(|| t.ttft_ms_sum as f64 / t.ttft_count as f64),
    }
}

/// One point per bucket start in `stamps`; buckets without requests are zero.
fn series_json(stamps: &[i64], buckets: &HashMap<i64, &db::LogTotals>) -> Vec<Value> {
    stamps
        .iter()
        .map(|ts| match buckets.get(ts) {
            Some(b) => json!({
                "ts": ts,
                "requests": b.requests,
                "errors": b.errors,
                "input_tokens": b.input_tokens + b.cache_read_tokens + b.cache_write_tokens,
                "output_tokens": b.output_tokens,
                "cost_usd": b.cost_usd,
                "avg_latency_ms": (b.requests > 0).then(|| b.latency_ms_sum as f64 / b.requests as f64),
            }),
            None => json!({
                "ts": ts, "requests": 0, "errors": 0, "input_tokens": 0, "output_tokens": 0, "cost_usd": 0.0,
                "avg_latency_ms": null,
            }),
        })
        .collect()
}

/// Metrics requests aggregating the request log at once on this replica. Each runs three scans,
/// so open Metrics pages cannot take the connection pool the request path writes through.
pub(crate) static LOG_SCANS: std::sync::LazyLock<tokio::sync::Semaphore> = std::sync::LazyLock::new(|| tokio::sync::Semaphore::new(3));

/// Metrics come from the request log in Postgres, so they cover its whole retention and every
/// replica's traffic.
pub(super) async fn metrics(
    State(state): State<AppState>,
    Extension(access): Extension<Access>,
    Query(q): Query<RangeQuery>,
) -> R<Json<Value>> {
    let _scan = LOG_SCANS.acquire().await.map_err(|_| AdminError(StatusCode::SERVICE_UNAVAILABLE, "shutting down".into()))?;
    let Window { label, step, stamps, since, until } = window(&q, Utc::now())?;
    let projects = access.projects();
    if q.project_id.is_some_and(|p| !access.sees(Some(p))) {
        return Err(AdminError(StatusCode::FORBIDDEN, "not your project".into()));
    }
    // The traffic counted: the picked project's, within what the caller may see.
    let picked = q.project_id.map(|p| [p]);
    let counted = picked.as_ref().map(|p| &p[..]).or(projects);
    let labels = label_filter(q.labels.as_deref())?;
    let labels = labels.as_ref();
    let deleted = q.deleted_keys.unwrap_or(false);
    let picked_models: Option<Vec<String>> = q
        .models
        .as_deref()
        .map(|m| m.split(',').map(str::trim).filter(|m| !m.is_empty()).map(String::from).collect::<Vec<_>>())
        .filter(|m| !m.is_empty());

    let snap = state.registry.snapshot();
    // Router entries are logged under the name requested, so under an alias of a router too.
    let is_router = |m: &str| snap.routers.contains_key(snap.resolve(m));
    // Without a pick, the routers are left out: their calls are counted under their own models.
    let exclude: Vec<String> = match picked_models {
        Some(_) => Vec::new(),
        None => snap.routers.keys().chain(snap.aliases.keys()).filter(|m| is_router(m)).cloned().collect(),
    };
    let (series, model_totals, selected) = tokio::try_join!(
        db::selection_series(&state.db, since, until, step, q.key_id, deleted, picked_models.as_deref(), &exclude, counted, labels),
        db::model_totals(&state.db, since, until, q.key_id, deleted, counted, labels),
        db::selection_totals(&state.db, since, until, q.key_id, deleted, picked_models.as_deref(), &exclude, counted, labels),
    )?;

    let buckets: HashMap<i64, &db::LogTotals> = series.iter().map(|b| (b.bucket, b)).collect();
    let model_totals: HashMap<&str, &db::LogTotals> = model_totals.iter().map(|t| (t.subject.as_str(), t)).collect();

    // Configured models and routers, plus models that have since been removed but still have traffic.
    // A user sees only the models that served their projects.
    let mut models: BTreeSet<&str> = match projects {
        None => snap.deployments.keys().chain(snap.routers.keys()).chain(snap.aliases.keys()).map(String::as_str).collect(),
        Some(_) => BTreeSet::new(),
    };
    models.extend(model_totals.keys().copied());
    let model_out: Vec<Value> = models
        .into_iter()
        .map(|m| {
            let router = is_router(m);
            json!({
                "model": m, "active": router || snap.deployments.contains_key(snap.resolve(m)), "router": router,
                "totals": totals(model_totals.get(m).copied()),
            })
        })
        .collect();
    Ok(Json(json!({
        "range": label, "step_secs": step, "from": since, "to": until, "models": model_out,
        // The picked models', or everything except router entries.
        "totals": totals(Some(&selected)), "series": series_json(&stamps, &buckets),
    })))
}

pub(super) async fn key_usage(
    State(state): State<AppState>,
    Extension(access): Extension<Access>,
    Query(q): Query<RangeQuery>,
) -> R<Json<Vec<db::KeyUsageRow>>> {
    let since = Utc::now()
        - match q.range.as_deref() {
            Some("1h") => chrono::Duration::hours(1),
            Some("7d") => chrono::Duration::days(7),
            Some("30d") => chrono::Duration::days(30),
            _ => chrono::Duration::hours(24),
        };
    Ok(Json(db::key_usage_since(&state.db, since, access.projects()).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_series_fill_empty_buckets() {
        let b = db::LogTotals { subject: "m".into(), bucket: 120, requests: 4, errors: 1, latency_ms_sum: 400, ..Default::default() };
        let buckets = HashMap::from([(120, &b)]);
        let s = series_json(&[60, 120, 180], &buckets);
        assert_eq!(s.len(), 3);
        assert_eq!(s[0]["requests"], 0);
        assert_eq!(s[0]["avg_latency_ms"], Value::Null);
        assert_eq!(s[1]["requests"], 4);
        assert_eq!(s[1]["avg_latency_ms"], 100.0);
        assert_eq!(series_json(&[60], &HashMap::new())[0]["errors"], 0);
        let t = totals(Some(&db::LogTotals { requests: 2, latency_ms_sum: 300, ttft_count: 1, ttft_ms_sum: 50, ..Default::default() }));
        assert_eq!((t.avg_latency_ms, t.avg_ttft_ms), (Some(150.0), Some(50.0)));
        assert_eq!(totals(None).requests, 0);
    }

    #[test]
    fn metric_windows() {
        let q = |range: &str, from: Option<&str>, to: Option<&str>| RangeQuery {
            range: Some(range.into()),
            key_id: None,
            project_id: None,
            deleted_keys: None,
            labels: None,
            models: None,
            from: from.map(|d| d.parse().unwrap()),
            to: to.map(|d| d.parse().unwrap()),
        };
        // 2026-10-25 17:42:30 UTC.
        let now = DateTime::parse_from_rfc3339("2026-10-25T17:42:30Z").unwrap().with_timezone(&Utc);
        let w = window(&q("1h", None, None), now).unwrap();
        assert_eq!((w.step, w.stamps.len()), (60, 60));
        assert_eq!(*w.stamps.last().unwrap(), now.timestamp() - 30);
        assert_eq!(window(&q("7d", None, None), now).unwrap().stamps.len(), 168);
        // This month: daily buckets from the 1st (UTC) through today.
        let m = window(&q("month", None, None), now).unwrap();
        assert_eq!((m.step, m.stamps.len()), (DAY, 25));
        assert_eq!(m.since.to_rfc3339(), "2026-10-01T00:00:00+00:00");
        // Custom: whole days, inclusive; hourly up to two days.
        let c = window(&q("custom", Some("2026-09-01"), Some("2026-09-30")), now).unwrap();
        assert_eq!((c.step, c.stamps.len()), (DAY, 30));
        assert_eq!(c.until.to_rfc3339(), "2026-10-01T00:00:00+00:00");
        let short = window(&q("custom", Some("2026-09-01"), Some("2026-09-02")), now).unwrap();
        assert_eq!((short.step, short.stamps.len()), (3600, 48));
        assert!(window(&q("custom", Some("2026-09-02"), Some("2026-09-01")), now).is_err());
        assert!(window(&q("custom", None, None), now).is_err());
        assert!(window(&q("custom", Some("2024-01-01"), Some("2026-01-01")), now).is_err());
        assert!(window(&q("year", None, None), now).is_err());
    }
}
