//! Metrics and usage per key, aggregated from the request log.

use std::collections::{BTreeSet, HashMap};

use axum::{
    Extension, Json,
    extract::{Path, Query, State},
};
use chrono::{DateTime, Datelike, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{R, label_filter};
use crate::{admin::auth::Access, budget::BudgetPeriod, db, error::AdminError, state::AppState};

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
    /// Metrics only: what the table's rows total: `model` (the default), `project` or `key`.
    group: Option<String>,
    /// Metrics only: the column the table's rows are sorted by: `name` (the model's, project's or
    /// key's), `requests` (the default), `errors`, `input`, `output`, `latency`, `p95`, `ttft` or `cost`.
    sort: Option<String>,
    /// Metrics only: `asc` or `desc`; the default is ascending for `name`, descending otherwise.
    order: Option<String>,
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

/// The traffic a Metrics view counts, from its time range, key, project, label and model filters.
struct Filter {
    window: Window,
    key_id: Option<Uuid>,
    deleted_keys: bool,
    /// The picked models; None for every model except router entries (`exclude`).
    models: Option<Vec<String>>,
    exclude: Vec<String>,
    /// The picked project's traffic, within what the caller may see; None for everything.
    projects: Option<Vec<Uuid>>,
    labels: Option<Value>,
}

impl Filter {
    fn new(state: &AppState, access: &Access, q: &RangeQuery) -> R<Self> {
        let window = window(q, Utc::now())?;
        if q.project_id.is_some_and(|p| !access.sees(Some(p))) {
            return Err(AdminError::forbidden("not your project"));
        }
        let projects = match q.project_id {
            Some(p) => Some(vec![p]),
            None => access.projects().map(<[Uuid]>::to_vec),
        };
        let labels = label_filter(q.labels.as_deref())?;
        let models: Option<Vec<String>> = q
            .models
            .as_deref()
            .map(|m| m.split(',').map(str::trim).filter(|m| !m.is_empty()).map(String::from).collect::<Vec<_>>())
            .filter(|m| !m.is_empty());
        // Without a pick, the routers are left out: their calls are counted under their own models.
        // Router entries are logged under the name requested, so under an alias of a router too.
        let exclude: Vec<String> = match models {
            Some(_) => Vec::new(),
            None => {
                let snap = state.registry.snapshot();
                snap.routers.keys().chain(snap.aliases.keys()).filter(|m| snap.routers.contains_key(snap.resolve(m))).cloned().collect()
            }
        };
        Ok(Self { window, key_id: q.key_id, deleted_keys: q.deleted_keys.unwrap_or(false), models, exclude, projects, labels })
    }

    fn selection(&self) -> crate::logstore::Selection<'_> {
        crate::logstore::Selection {
            since: self.window.since,
            until: self.window.until,
            key_id: self.key_id,
            deleted_keys: self.deleted_keys,
            models: self.models.as_deref(),
            exclude: &self.exclude,
            projects: self.projects.as_deref(),
            labels: self.labels.as_ref(),
        }
    }
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

/// The table's columns, by the `sort` parameter's names.
#[derive(Debug, Clone, Copy, PartialEq)]
enum SortBy {
    Name,
    Requests,
    Errors,
    Input,
    Output,
    Latency,
    P95,
    Ttft,
    Cost,
}

impl SortBy {
    fn parse(s: Option<&str>) -> Result<Self, AdminError> {
        Ok(match s.unwrap_or("requests") {
            "name" => Self::Name,
            "requests" => Self::Requests,
            "errors" => Self::Errors,
            "input" => Self::Input,
            "output" => Self::Output,
            "latency" => Self::Latency,
            "p95" => Self::P95,
            "ttft" => Self::Ttft,
            "cost" => Self::Cost,
            _ => return Err(AdminError::bad_request("sort must be name, requests, errors, input, output, latency, p95, ttft or cost")),
        })
    }

    /// The column's value in a row, as a number; None (no latency yet) sorts last either way.
    fn value(self, t: &Totals) -> Option<f64> {
        match self {
            Self::Name => None,
            Self::Requests => Some(t.requests as f64),
            Self::Errors => Some(t.errors as f64),
            // As the table shows it: cache reads and writes are input too.
            Self::Input => Some((t.input_tokens + t.cache_read_tokens + t.cache_write_tokens) as f64),
            Self::Output => Some(t.output_tokens as f64),
            Self::Latency => t.avg_latency_ms,
            Self::P95 => t.p95_latency_ms.map(f64::from),
            Self::Ttft => t.avg_ttft_ms,
            Self::Cost => Some(t.cost_usd),
        }
    }
}

/// One row of the table: a model (`id` is its name), or a project or key (`id` is None for the
/// requests without one, `name` is None for one deleted since).
#[derive(Serialize)]
struct Row {
    id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    totals: Totals,
    /// The name the table shows and sorts by (a model's is its id); rows without one sort last.
    #[serde(skip)]
    label: Option<String>,
}

impl Row {
    fn label(&self) -> Option<&str> {
        self.label.as_deref()
    }
}

/// Sorts the rows by `by`, ties by name; rows without a value (or a name) go last.
fn sort_rows(rows: &mut [Row], by: SortBy, desc: bool) {
    use std::cmp::Ordering;
    rows.sort_by(|a, b| {
        let primary = match by {
            SortBy::Name => match (a.label(), b.label()) {
                (Some(x), Some(y)) => x.cmp(y),
                (Some(_), None) => return Ordering::Less,
                (None, Some(_)) => return Ordering::Greater,
                (None, None) => Ordering::Equal,
            },
            _ => match (by.value(&a.totals), by.value(&b.totals)) {
                (Some(x), Some(y)) => x.total_cmp(&y),
                (Some(_), None) => return Ordering::Less,
                (None, Some(_)) => return Ordering::Greater,
                (None, None) => Ordering::Equal,
            },
        };
        (if desc { primary.reverse() } else { primary }).then_with(|| a.label().cmp(&b.label())).then_with(|| a.id.cmp(&b.id))
    });
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

/// Metrics requests aggregating the request log at once on this replica. Each runs up to four scans,
/// so open Metrics pages cannot take the connection pool the request path writes through.
pub(crate) static LOG_SCANS: std::sync::LazyLock<tokio::sync::Semaphore> = std::sync::LazyLock::new(|| tokio::sync::Semaphore::new(3));

/// Metrics
///
/// Requests, errors, tokens, latency and cost over time, and totals by model, project or key.
#[utoipa::path(get, path = "/metrics", tag = "Usage", params(RangeQuery),
    responses(
        (status = 200, description = "Totals and series for the range.", body = Object),
        (status = 400, description = "Invalid range.", body = crate::error::AdminErrorBody),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
// Metrics come from the request log in Postgres, so they cover its whole retention and every replica's traffic.
pub(super) async fn metrics(
    State(state): State<AppState>,
    Extension(access): Extension<Access>,
    Query(q): Query<RangeQuery>,
) -> R<Json<Value>> {
    let sort = SortBy::parse(q.sort.as_deref())?;
    let group = match q.group.as_deref() {
        None | Some("model") => db::Group::Model,
        Some("project") => db::Group::Project,
        Some("key") => db::Group::Key,
        Some(_) => return Err(AdminError::bad_request("group must be model, project or key")),
    };
    let desc = match q.order.as_deref() {
        None => sort != SortBy::Name,
        Some("asc") => false,
        Some("desc") => true,
        Some(_) => return Err(AdminError::bad_request("order must be asc or desc")),
    };
    let _scan = LOG_SCANS.acquire().await.map_err(|_| AdminError::unavailable("shutting down"))?;
    let filter = Filter::new(&state, &access, &q)?;
    let Window { label, step, ref stamps, since, until } = filter.window;
    let projects = access.projects();
    let picked_models = filter.models.as_deref();
    let selection = filter.selection();

    let snap = state.registry.snapshot();
    // Router entries are logged under the name requested, so under an alias of a router too.
    let is_router = |m: &str| snap.routers.contains_key(snap.resolve(m));
    // Every model's totals, whatever the pick: the model filter's choices, and the by-model table,
    // from which models are picked. By project or key, the rows count the selection's traffic, as
    // the tiles and charts do.
    let every_model = crate::logstore::Selection { models: None, exclude: &[], ..selection };
    let (series, model_totals, entity_totals, selected) = tokio::try_join!(
        crate::logstore::selection_series(&state, &selection, step),
        crate::logstore::group_totals(&state, &every_model, db::Group::Model),
        async {
            match group {
                db::Group::Model => Ok(Vec::new()),
                _ => crate::logstore::group_totals(&state, &selection, group).await,
            }
        },
        crate::logstore::selection_totals(&state, &selection),
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
    // The model filter's choices, by name.
    let model_out: Vec<Value> =
        models.iter().map(|m| json!({ "model": m, "active": is_router(m) || snap.deployments.contains_key(snap.resolve(m)) })).collect();
    let mut rows: Vec<Row> = match group {
        // The models with requests in the range, and those picked.
        db::Group::Model => {
            let picked = picked_models.unwrap_or_default();
            models
                .iter()
                .filter(|m| model_totals.contains_key(*m) || picked.iter().any(|p| p == *m))
                .map(|m| Row {
                    id: Some(m.to_string()),
                    name: None,
                    totals: totals(model_totals.get(m).copied()),
                    label: Some(m.to_string()),
                })
                .collect()
        }
        // The projects or keys with requests in the range, named from Postgres.
        _ => {
            let ids: Vec<Uuid> = entity_totals.iter().filter_map(|t| Uuid::parse_str(&t.subject).ok()).collect();
            let names: HashMap<Uuid, String> = match group {
                db::Group::Project => db::project_names(&state.db, Some(&ids)).await?.into_iter().map(|p| (p.id, p.name)).collect(),
                _ => db::key_names_of(&state.db, &ids).await?,
            };
            entity_totals
                .iter()
                .map(|t| {
                    let id = Uuid::parse_str(&t.subject).ok();
                    let name = id.and_then(|id| names.get(&id).cloned());
                    Row { id: id.map(|id| id.to_string()), label: name.clone(), name, totals: totals(Some(t)) }
                })
                .collect()
        }
    };
    sort_rows(&mut rows, sort, desc);
    Ok(Json(json!({
        "range": label, "step_secs": step, "from": since, "to": until, "models": model_out, "group": group.as_str(), "rows": rows,
        // The picked models', or everything except router entries.
        "totals": totals(Some(&selected)), "series": series_json(stamps, &buckets),
    })))
}

/// Token usage and cost per project, key and model over a Metrics selection.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct Billing {
    /// The covered time, end exclusive.
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    /// Sorted by project, key and model.
    rows: Vec<BillingRow>,
}

#[derive(Serialize, utoipa::ToSchema, Debug, PartialEq)]
pub(crate) struct BillingRow {
    /// The project's name, or `Deleted project` / `No project`.
    project: String,
    /// The key's name, or `Deleted key` / `No key`.
    key: String,
    model: String,
    requests: i64,
    input_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    output_tokens: i64,
    cost_usd: f64,
}

/// A project's or key's name, or what stands for one deleted since or missing.
fn entity_name(id: Option<Uuid>, names: &HashMap<Uuid, String>, deleted: &str, none: &str) -> String {
    match id {
        Some(id) => names.get(&id).cloned().unwrap_or_else(|| deleted.to_string()),
        None => none.to_string(),
    }
}

fn billing_rows(totals: Vec<db::BillingTotals>, projects: &HashMap<Uuid, String>, keys: &HashMap<Uuid, String>) -> Vec<BillingRow> {
    let mut rows: Vec<BillingRow> = totals
        .into_iter()
        .map(|t| BillingRow {
            project: entity_name(t.project_id, projects, "Deleted project", "No project"),
            key: entity_name(t.key_id, keys, "Deleted key", "No key"),
            model: t.model,
            requests: t.requests,
            input_tokens: t.input_tokens,
            cache_read_tokens: t.cache_read_tokens,
            cache_write_tokens: t.cache_write_tokens,
            output_tokens: t.output_tokens,
            cost_usd: t.cost_usd,
        })
        .collect();
    rows.sort_by(|a, b| {
        (a.project.to_lowercase(), a.key.to_lowercase(), &a.model).cmp(&(b.project.to_lowercase(), b.key.to_lowercase(), &b.model))
    });
    rows
}

/// Billing
///
/// Token usage and cost per project, key and model, with the Metrics page's filters.
#[utoipa::path(get, path = "/metrics/billing", tag = "Usage", params(RangeQuery),
    responses(
        (status = 200, description = "Totals per project, key and model.", body = Billing),
        (status = 400, description = "Invalid range or filter.", body = crate::error::AdminErrorBody),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn billing(
    State(state): State<AppState>,
    Extension(access): Extension<Access>,
    Query(q): Query<RangeQuery>,
) -> R<Json<Billing>> {
    let _scan = LOG_SCANS.acquire().await.map_err(|_| AdminError::unavailable("shutting down"))?;
    let filter = Filter::new(&state, &access, &q)?;
    let totals = crate::logstore::billing_totals(&state, &filter.selection()).await?;
    let project_ids: Vec<Uuid> = totals.iter().filter_map(|t| t.project_id).collect::<BTreeSet<_>>().into_iter().collect();
    let key_ids: Vec<Uuid> = totals.iter().filter_map(|t| t.key_id).collect::<BTreeSet<_>>().into_iter().collect();
    let projects: HashMap<Uuid, String> =
        db::project_names(&state.db, Some(&project_ids)).await?.into_iter().map(|p| (p.id, p.name)).collect();
    let keys = db::key_names_of(&state.db, &key_ids).await?;
    Ok(Json(Billing { from: filter.window.since, to: filter.window.until, rows: billing_rows(totals, &projects, &keys) }))
}

/// Usage by key
///
/// Spend by key, top 50.
#[utoipa::path(get, path = "/usage/keys", tag = "Usage",
    params(("range" = Option<String>, Query, description = "`1h`, `24h` (default), `7d` or `30d`.")),
    responses((status = 200, description = "Usage per key.", body = Vec<crate::db::KeyUsageRow>)),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
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
    Ok(Json(crate::logstore::key_usage_since(&state, since, access.projects()).await?))
}

/// A key's busiest minute in each hour of the last 7 days, and the 95th percentiles over the
/// minutes it had traffic: what its requests and tokens per minute limits are measured against.
#[derive(Serialize, utoipa::ToSchema, Debug, PartialEq)]
pub(crate) struct KeyRates {
    /// One point per hour, oldest first.
    series: Vec<RatePoint>,
    /// Requests per minute; None without traffic.
    #[schema(required)]
    rpm_p95: Option<i64>,
    /// Tokens per minute; None without traffic.
    #[schema(required)]
    tpm_p95: Option<i64>,
}

#[derive(Serialize, utoipa::ToSchema, Debug, PartialEq)]
pub(crate) struct RatePoint {
    /// Start of the hour (Unix seconds).
    ts: i64,
    /// The hour's busiest minute: its requests and its tokens (not necessarily the same minute).
    rpm: i64,
    tpm: i64,
}

/// The 95th percentile by nearest rank.
fn p95(mut values: Vec<i64>) -> Option<i64> {
    values.sort_unstable();
    let rank = (values.len() as f64 * 0.95).ceil() as usize;
    values.get(rank.checked_sub(1)?).copied()
}

fn hourly_rates(minutes: &[db::KeyMinute], stamps: &[i64]) -> KeyRates {
    let mut peaks: HashMap<i64, (i64, i64)> = HashMap::new();
    for m in minutes {
        let p = peaks.entry(m.minute - m.minute.rem_euclid(3600)).or_default();
        *p = (p.0.max(m.requests), p.1.max(m.tokens));
    }
    KeyRates {
        series: stamps
            .iter()
            .map(|&ts| {
                let (rpm, tpm) = peaks.get(&ts).copied().unwrap_or_default();
                RatePoint { ts, rpm, tpm }
            })
            .collect(),
        rpm_p95: p95(minutes.iter().map(|m| m.requests).collect()),
        tpm_p95: p95(minutes.iter().map(|m| m.tokens).collect()),
    }
}

/// Key rates
///
/// The key's requests and tokens per minute over the last 7 days, for setting its limits.
#[utoipa::path(get, path = "/keys/{id}/rates", tag = "Usage", params(("id" = Uuid, Path)),
    responses(
        (status = 200, description = "Hourly peaks and percentiles.", body = KeyRates),
        (status = 404, description = "No such key.", body = crate::error::AdminErrorBody),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn key_rates(
    State(state): State<AppState>,
    Extension(access): Extension<Access>,
    Path(id): Path<Uuid>,
) -> R<Json<KeyRates>> {
    let k = db::get_key(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    super::keys::check_sees_key(&access, &k)?;
    let _scan = LOG_SCANS.acquire().await.map_err(|_| AdminError::unavailable("shutting down"))?;
    let now = Utc::now().timestamp();
    let first = now - now.rem_euclid(3600) - 167 * 3600;
    let stamps: Vec<i64> = (0..168).map(|i| first + i * 3600).collect();
    let since = DateTime::from_timestamp(first, 0).unwrap_or_default();
    let minutes = crate::logstore::key_minutes(&state, id, since).await?;
    Ok(Json(hourly_rates(&minutes, &stamps)))
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct SpendQuery {
    /// The budget period to sum by: `day` (the default, the last 30), `week` (the last 12) or `month` (the last 12).
    period: Option<String>,
}

/// A key's spend per budget period, since it was created.
#[derive(Serialize, utoipa::ToSchema, Debug, PartialEq)]
pub(crate) struct KeySpend {
    period: BudgetPeriod,
    /// One point per period, oldest first; the last is the current one.
    series: Vec<SpendPoint>,
    total_usd: f64,
    /// Average over the finished periods; None before the first one ends.
    #[schema(required)]
    avg_usd: Option<f64>,
}

#[derive(Serialize, utoipa::ToSchema, Debug, PartialEq)]
pub(crate) struct SpendPoint {
    /// Start of the period (Unix seconds).
    ts: i64,
    cost_usd: f64,
}

/// The periods up to the current one, at most `count`, that end after `created`.
fn spend_periods(period: BudgetPeriod, now: DateTime<Utc>, created: DateTime<Utc>, count: usize) -> Vec<i64> {
    let mut w = period.window(now);
    let mut starts = Vec::new();
    while starts.len() < count && w.end > created {
        starts.push(w.start.timestamp());
        w = period.window(w.start - chrono::Duration::seconds(1));
    }
    starts.reverse();
    starts
}

fn spend_by_period(period: BudgetPeriod, starts: &[i64], days: &[(i64, f64)]) -> KeySpend {
    let mut sums: HashMap<i64, f64> = HashMap::new();
    for &(day, cost) in days {
        let start = period.window(DateTime::from_timestamp(day, 0).unwrap_or_default()).start.timestamp();
        *sums.entry(start).or_default() += cost;
    }
    let series: Vec<SpendPoint> = starts.iter().map(|&ts| SpendPoint { ts, cost_usd: sums.get(&ts).copied().unwrap_or(0.0) }).collect();
    let finished = &series[..series.len().saturating_sub(1)];
    KeySpend {
        period,
        total_usd: series.iter().map(|p| p.cost_usd).sum(),
        avg_usd: (!finished.is_empty()).then(|| finished.iter().map(|p| p.cost_usd).sum::<f64>() / finished.len() as f64),
        series,
    }
}

/// Key spend
///
/// The key's spend per day, week or month, as its budget counts it, with the total and the
/// average per period.
#[utoipa::path(get, path = "/keys/{id}/spend", tag = "Usage", params(("id" = Uuid, Path), SpendQuery),
    responses(
        (status = 200, description = "Spend per period.", body = KeySpend),
        (status = 400, description = "Invalid period.", body = crate::error::AdminErrorBody),
        (status = 404, description = "No such key.", body = crate::error::AdminErrorBody),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn key_spend(
    State(state): State<AppState>,
    Extension(access): Extension<Access>,
    Path(id): Path<Uuid>,
    Query(q): Query<SpendQuery>,
) -> R<Json<KeySpend>> {
    let period = match q.period.as_deref() {
        None => BudgetPeriod::Day,
        Some(p) => BudgetPeriod::parse(p).ok_or_else(|| AdminError::bad_request("period must be day, week or month"))?,
    };
    let k = db::get_key(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    super::keys::check_sees_key(&access, &k)?;
    let _scan = LOG_SCANS.acquire().await.map_err(|_| AdminError::unavailable("shutting down"))?;
    let count = if period == BudgetPeriod::Day { 30 } else { 12 };
    let starts = spend_periods(period, Utc::now(), k.created_at, count);
    let since = starts.first().and_then(|&s| DateTime::from_timestamp(s, 0)).unwrap_or_else(Utc::now);
    let days = crate::logstore::key_daily_spend(&state, id, since).await?;
    Ok(Json(spend_by_period(period, &starts, &days)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn billing_rows_are_named_and_sorted() {
        let (p1, k1, k2, gone) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3), Uuid::from_u128(4));
        let projects = HashMap::from([(p1, "acme".to_string())]);
        let keys = HashMap::from([(k1, "Prod".to_string()), (k2, "batch".to_string())]);
        let t = |project_id, key_id, model: &str, cost_usd| db::BillingTotals {
            project_id,
            key_id,
            model: model.to_string(),
            cost_usd,
            ..Default::default()
        };
        let rows = billing_rows(
            vec![
                t(Some(p1), Some(k1), "gpt", 1.0),
                t(None, None, "gpt", 2.0),
                t(Some(p1), Some(k2), "claude", 3.0),
                t(Some(gone), Some(gone), "gpt", 4.0),
                t(Some(p1), Some(k1), "claude", 5.0),
            ],
            &projects,
            &keys,
        );
        let got: Vec<(&str, &str, &str, f64)> =
            rows.iter().map(|r| (r.project.as_str(), r.key.as_str(), r.model.as_str(), r.cost_usd)).collect();
        assert_eq!(
            got,
            [
                ("acme", "batch", "claude", 3.0),
                ("acme", "Prod", "claude", 5.0),
                ("acme", "Prod", "gpt", 1.0),
                ("Deleted project", "Deleted key", "gpt", 4.0),
                ("No project", "No key", "gpt", 2.0),
            ]
        );
    }

    #[test]
    fn rows_sort_by_column_with_blanks_last() {
        let row = |m: Option<&str>, requests: i64, latency: Option<f64>| Row {
            id: Some(m.unwrap_or("deleted").to_string()),
            name: None,
            totals: Totals { requests, avg_latency_ms: latency, ..Default::default() },
            label: m.map(String::from),
        };
        let order = |by, desc| {
            let mut rows = vec![row(Some("b"), 5, Some(20.0)), row(Some("a"), 5, None), row(Some("c"), 9, Some(10.0))];
            sort_rows(&mut rows, by, desc);
            rows.iter().map(|r| r.label().unwrap().to_string()).collect::<Vec<_>>()
        };
        // Ties go by name.
        assert_eq!(order(SortBy::Requests, true), ["c", "a", "b"]);
        assert_eq!(order(SortBy::Requests, false), ["a", "b", "c"]);
        // A row without a latency is last whichever way.
        assert_eq!(order(SortBy::Latency, true), ["b", "c", "a"]);
        assert_eq!(order(SortBy::Latency, false), ["c", "b", "a"]);
        assert_eq!(order(SortBy::Name, true), ["c", "b", "a"]);
        // A deleted key, without a name, is last by name either way.
        for desc in [false, true] {
            let mut rows = vec![row(None, 9, None), row(Some("b"), 1, None), row(Some("a"), 1, None)];
            sort_rows(&mut rows, SortBy::Name, desc);
            assert_eq!(rows.last().unwrap().label(), None);
        }
        assert!(SortBy::parse(Some("speed")).is_err());
        assert_eq!(SortBy::parse(None).unwrap(), SortBy::Requests);
    }

    #[test]
    fn key_rates_peak_per_hour() {
        let m = |minute: i64, requests: i64, tokens: i64| db::KeyMinute { minute, requests, tokens };
        // Two busy minutes in the first hour, one in the third.
        let minutes = [m(60, 3, 100), m(120, 5, 40), m(7260, 2, 900)];
        let r = hourly_rates(&minutes, &[0, 3600, 7200]);
        let points: Vec<_> = r.series.iter().map(|p| (p.ts, p.rpm, p.tpm)).collect();
        assert_eq!(points, [(0, 5, 100), (3600, 0, 0), (7200, 2, 900)]);
        assert_eq!((r.rpm_p95, r.tpm_p95), (Some(5), Some(900)));
        assert_eq!(hourly_rates(&[], &[0]).rpm_p95, None);
        // Nearest rank: of 1..=20, the 19th.
        assert_eq!(p95((1..=20).collect()), Some(19));
        assert_eq!(p95(vec![7]), Some(7));
    }

    #[test]
    fn key_spend_per_period() {
        let at = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);
        let ts = |s: &str| at(s).timestamp();
        // Wednesday 2026-10-28; the key is from Saturday 2026-10-10.
        let now = at("2026-10-28T12:00:00Z");
        let created = at("2026-10-10T09:00:00Z");
        let weeks = spend_periods(BudgetPeriod::Week, now, created, 12);
        assert_eq!(weeks, [ts("2026-10-05T00:00:00Z"), ts("2026-10-12T00:00:00Z"), ts("2026-10-19T00:00:00Z"), ts("2026-10-26T00:00:00Z")]);
        assert_eq!(spend_periods(BudgetPeriod::Day, now, created, 3).len(), 3);
        let days = [(ts("2026-10-10T00:00:00Z"), 1.0), (ts("2026-10-11T00:00:00Z"), 2.0), (ts("2026-10-27T00:00:00Z"), 4.0)];
        let s = spend_by_period(BudgetPeriod::Week, &weeks, &days);
        let costs: Vec<f64> = s.series.iter().map(|p| p.cost_usd).collect();
        assert_eq!(costs, [3.0, 0.0, 0.0, 4.0]);
        assert_eq!(s.total_usd, 7.0);
        // The current week is left out of the average.
        assert_eq!(s.avg_usd, Some(1.0));
        assert_eq!(spend_by_period(BudgetPeriod::Week, &weeks[3..], &days).avg_usd, None);
    }

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
            group: None,
            sort: None,
            order: None,
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
