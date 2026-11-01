//! The request log, its stored prompts and responses, the labels and tools it holds, and the
//! audit log.

use axum::{
    Extension, Json,
    extract::{Path, Query, State},
};
use chrono::{DateTime, Datelike, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{R, check_sees_key, label_filter};
use crate::{
    admin::auth::Access,
    audit::{self, Actor},
    db,
    error::AdminError,
    state::AppState,
};

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct LogQuery {
    limit: Option<i64>,
    /// Matches model names containing this text, ignoring case.
    model: Option<String>,
    key_id: Option<Uuid>,
    /// Just the requests of this project's keys.
    project_id: Option<Uuid>,
    /// Just the requests of keys that were deleted since.
    deleted_keys: Option<bool>,
    errors: Option<bool>,
    /// Just the requests with all of these labels, as `tenant=…,feature=core`.
    labels: Option<String>,
    /// `1h`, `24h`, `7d`, `30d`, `month` or `custom`; omitted means no time limit.
    range: Option<String>,
    /// With `range=custom`: first and last day, inclusive (UTC).
    from: Option<chrono::NaiveDate>,
    to: Option<chrono::NaiveDate>,
    /// Just the requests whose prompts and responses are stored (prompt logging).
    bodies: Option<bool>,
    /// Just the requests whose stored prompt (after guardrails) or response contains these words:
    /// `"a phrase"`, `or`, `-word` and `prefix:*` work as in Postgres' `websearch_to_tsquery`.
    q: Option<String>,
    /// Just the requests whose stored response called this tool (`web_search` for web searches),
    /// and the MCP calls of a tool of this name.
    tool: Option<String>,
    /// Just the requests the guardrails did any of these to, as `blocked,masked`: `passed`,
    /// `flagged`, `masked` or `blocked`.
    guardrail: Option<String>,
}

/// Request log
///
/// Newest first. Routed requests come with the calls they made.
#[utoipa::path(get, path = "/logs", tag = "Usage", params(LogQuery),
    responses((status = 200, description = "Log entries.", body = Vec<crate::db::LogRow>)),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn logs(
    State(state): State<AppState>,
    Extension(access): Extension<Access>,
    Query(q): Query<LogQuery>,
) -> R<Json<Vec<db::LogRow>>> {
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let model = q.model.as_deref().map(str::trim).filter(|m| !m.is_empty());
    let (since, until) = log_window(q.range.as_deref(), q.from, q.to, Utc::now())?;
    let errors = q.errors.unwrap_or(false);
    let labels = label_filter(q.labels.as_deref())?;
    if q.project_id.is_some_and(|p| !access.sees(Some(p))) {
        return Err(AdminError::forbidden("not your project"));
    }
    let text = q.q.as_deref().map(str::trim).filter(|t| !t.is_empty());
    if text.is_some_and(|t| t.chars().count() > 200) {
        return Err(AdminError::bad_request("q can be at most 200 characters"));
    }
    let tool = q.tool.as_deref().map(str::trim).filter(|t| !t.is_empty());
    let guardrail: Option<Vec<String>> = match q.guardrail.as_deref().map(str::trim).filter(|g| !g.is_empty()) {
        Some(list) => {
            let actions: Vec<String> = list.split(',').map(|a| a.trim().to_string()).collect();
            if let Some(bad) = actions.iter().find(|a| !matches!(a.as_str(), "passed" | "flagged" | "masked" | "blocked")) {
                return Err(AdminError::bad_request(format!("guardrail must list passed, flagged, masked or blocked, not '{bad}'")));
            }
            Some(actions)
        }
        None => None,
    };
    // The picked project's requests, within what the caller may see.
    let picked = q.project_id.map(|p| [p]);
    let filter = db::LogFilter {
        limit,
        model,
        key_id: q.key_id,
        deleted_keys: q.deleted_keys.unwrap_or(false),
        errors_only: errors,
        since,
        until,
        projects: picked.as_ref().map(|p| &p[..]).or(access.projects()),
        labels: labels.as_ref(),
        bodies: q.bodies.unwrap_or(false),
        text,
        tool,
        guardrail: guardrail.as_deref(),
        body_days: state.settings.get().prompt_log_retention_days,
    };
    Ok(Json(db::recent_logs(&state.db, &filter).await?))
}

/// What the latest logged responses' tools are sampled from.
const TOOL_SAMPLE: i64 = 1000;

/// Tools in logged responses
///
/// The tools the latest 1000 logged responses called, at most 100, most used first. The
/// console offers them in its tool filter.
#[utoipa::path(get, path = "/logs/tools", tag = "Usage",
    responses((status = 200, description = "Tools, most used first.", body = Vec<crate::db::ToolUse>)),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn log_tools(State(state): State<AppState>, Extension(access): Extension<Access>) -> R<Json<Vec<db::ToolUse>>> {
    Ok(Json(db::recent_tools(&state.db, TOOL_SAMPLE, access.projects()).await?))
}

/// A logged request's bodies (see `capture`).
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct BodiesView {
    /// The request as the client sent it.
    #[schema(value_type = Option<Object>)]
    #[schema(required)]
    request: Option<Value>,
    /// The request as the model got it, when guardrails changed it.
    #[schema(value_type = Option<Object>)]
    #[schema(required)]
    screened_request: Option<Value>,
    /// What each guardrail was given, found (matched text included), did and answered.
    #[schema(value_type = Option<Vec<Object>>)]
    #[schema(required)]
    guardrails: Option<Value>,
    /// The response as the client got it; a stream put back together into one document.
    #[schema(value_type = Option<Object>)]
    #[schema(required)]
    response: Option<Value>,
    /// Sizes as sent and received, before placeholders and cuts.
    request_bytes: i32,
    response_bytes: i32,
    /// `stored` or `truncated`.
    #[schema(required)]
    status: Option<String>,
    /// The logged turn before this one in its conversation, and the one after, where the caller
    /// may see them.
    #[schema(required)]
    previous: Option<Uuid>,
    #[schema(required)]
    next: Option<Uuid>,
}

/// A logged request's prompt and response
///
/// For keys with prompt logging on: the request as sent and as the model got it, what each
/// guardrail did, the response, and the turns before and after it.
#[utoipa::path(get, path = "/logs/{id}/bodies", tag = "Usage",
    params(("id" = Uuid, Path, description = "Request log entry id")),
    responses(
        (status = 200, description = "The stored bodies.", body = BodiesView),
        (status = 404, description = "No stored bodies, or not the caller's project.", body = crate::error::AdminErrorBody),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
// An entry the caller may not see looks like one without bodies.
pub(super) async fn log_bodies(
    State(state): State<AppState>,
    Extension(access): Extension<Access>,
    Path(id): Path<Uuid>,
) -> R<Json<BodiesView>> {
    let row = db::log_bodies(&state.db, id).await?.filter(|r| access.sees(r.project_id)).ok_or_else(AdminError::not_found)?;
    let (previous, next) = db::turn_links(&state.db, &row, access.projects()).await?;
    Ok(Json(BodiesView {
        request: row.request,
        screened_request: row.screened_request,
        guardrails: row.guardrails,
        response: row.response,
        request_bytes: row.request_bytes,
        response_bytes: row.response_bytes,
        status: row.status,
        previous,
        next,
    }))
}

/// How many prompts and responses are stored for a key or project.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct StoredBodies {
    stored: i64,
}

/// How many were deleted.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct DeletedBodies {
    deleted: u64,
}

/// Count a key's stored prompts
#[utoipa::path(get, path = "/keys/{id}/bodies", tag = "Virtual keys",
    params(("id" = Uuid, Path, description = "Key id")),
    responses((status = 200, description = "How many are stored.", body = StoredBodies), (status = 404, description = "No such key.", body = crate::error::AdminErrorBody)),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn key_bodies(
    State(state): State<AppState>,
    Extension(access): Extension<Access>,
    Path(id): Path<Uuid>,
) -> R<Json<StoredBodies>> {
    let k = db::get_key(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    check_sees_key(&access, &k)?;
    Ok(Json(StoredBodies { stored: db::count_bodies(&state.db, db::BodiesOf::Key(id)).await? }))
}

/// Delete a key's stored prompts
///
/// Deletes the key's logged prompts and responses now, rather than at the end of their
/// retention. Their entries stay in the request log, marked `deleted`.
#[utoipa::path(delete, path = "/keys/{id}/bodies", tag = "Virtual keys",
    params(("id" = Uuid, Path, description = "Key id")),
    responses(
        (status = 200, description = "How many were deleted.", body = DeletedBodies),
        (status = 404, description = "No such key.", body = crate::error::AdminErrorBody),
        (status = 409, description = "Already being deleted.", body = crate::error::AdminErrorBody),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn delete_key_bodies(
    State(state): State<AppState>,
    Extension(access): Extension<Access>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<Uuid>,
) -> R<Json<DeletedBodies>> {
    let k = db::get_key(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    check_sees_key(&access, &k)?;
    let deleted = remove_bodies(&state, db::BodiesOf::Key(id)).await?;
    audit::Event::new("key", "delete_bodies").on(id, &k.name).details(json!({ "deleted": deleted })).record(&state, &actor).await;
    Ok(Json(DeletedBodies { deleted }))
}

/// Count a project's stored prompts
#[utoipa::path(get, path = "/projects/{id}/bodies", tag = "Virtual keys",
    params(("id" = Uuid, Path, description = "Project id")),
    responses((status = 200, description = "How many are stored.", body = StoredBodies), (status = 404, description = "No such project.", body = crate::error::AdminErrorBody)),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn project_bodies(State(state): State<AppState>, Path(id): Path<Uuid>) -> R<Json<StoredBodies>> {
    db::get_project(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    Ok(Json(StoredBodies { stored: db::count_bodies(&state.db, db::BodiesOf::Project(id)).await? }))
}

/// Delete a project's stored prompts
///
/// Deletes the logged prompts and responses of every request made in the project, also by
/// keys that have left it since.
#[utoipa::path(delete, path = "/projects/{id}/bodies", tag = "Virtual keys",
    params(("id" = Uuid, Path, description = "Project id")),
    responses(
        (status = 200, description = "How many were deleted.", body = DeletedBodies),
        (status = 404, description = "No such project.", body = crate::error::AdminErrorBody),
        (status = 409, description = "Already being deleted.", body = crate::error::AdminErrorBody),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn delete_project_bodies(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<Uuid>,
) -> R<Json<DeletedBodies>> {
    let p = db::get_project(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    let deleted = remove_bodies(&state, db::BodiesOf::Project(id)).await?;
    audit::Event::new("project", "delete_bodies").on(id, &p.name).details(json!({ "deleted": deleted })).record(&state, &actor).await;
    Ok(Json(DeletedBodies { deleted }))
}

/// Deletes stored bodies in their own task, so a client that stops waiting does not cut it
/// short, under a lock per key or project, so two clicks do not run it twice.
async fn remove_bodies(state: &AppState, of: db::BodiesOf) -> R<u64> {
    let id = match of {
        db::BodiesOf::Key(id) | db::BodiesOf::Project(id) => id,
    };
    let lock = 0x626f_6469_6573_0000 ^ (id.as_u128() as i64 & 0xffff_ffff); // "bodies"
    let state = state.clone();
    let task = tokio::spawn(async move { db::with_lock(&state.db, lock, db::delete_bodies(&state.db, of)).await });
    let done = task.await.map_err(|e| AdminError::internal(format!("deleting the bodies failed: {e}")))?;
    match done? {
        Some(deleted) => Ok(deleted?),
        None => Err(AdminError::conflict("these bodies are already being deleted")),
    }
}

/// Label suggestions are read from this many of the latest labelled requests...
const LABEL_SAMPLE: i64 = 1000;
/// ...and offer at most this many values of each key, the most used.
const LABEL_VALUES: usize = 50;

/// A label key the latest labelled requests carry, with its values.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct LabelKey {
    key: String,
    /// Most used first.
    values: Vec<LabelValue>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct LabelValue {
    value: String,
    /// How many of the sampled requests carry it.
    requests: i64,
}

/// Request labels in use
///
/// The label keys of the latest 1000 labelled requests, each with up to 50 of its values,
/// most used first. The console suggests them in its label filters.
#[utoipa::path(get, path = "/logs/labels", tag = "Usage",
    responses((status = 200, description = "Label keys, by name.", body = Vec<LabelKey>)),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn log_labels(State(state): State<AppState>, Extension(access): Extension<Access>) -> R<Json<Vec<LabelKey>>> {
    let mut keys: Vec<LabelKey> = Vec::new();
    for u in db::recent_labels(&state.db, LABEL_SAMPLE, access.projects()).await? {
        if keys.last().is_none_or(|k| k.key != u.key) {
            keys.push(LabelKey { key: u.key, values: Vec::new() });
        }
        let values = &mut keys.last_mut().expect("pushed above").values;
        if values.len() < LABEL_VALUES {
            values.push(LabelValue { value: u.value, requests: u.requests });
        }
    }
    Ok(Json(keys))
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct AuditQuery {
    limit: Option<i64>,
    /// `credential`, `deployment`, `router`, `key`, `project`, `configuration`, `price_catalog`,
    /// `oidc_settings`, `settings`, `admin` or `session`.
    target_type: Option<String>,
    /// Matches the target's or the actor's name containing this text, ignoring case.
    q: Option<String>,
    /// `1h`, `24h`, `7d`, `30d`, `month` or `custom`; omitted means no time limit.
    range: Option<String>,
    /// With `range=custom`: first and last day, inclusive (UTC).
    from: Option<chrono::NaiveDate>,
    to: Option<chrono::NaiveDate>,
}

/// Audit log
///
/// Newest first: who created, changed or deleted credentials, deployments, routers and keys,
/// imported configuration or prices, changed single sign-on, and signed in or out.
#[utoipa::path(get, path = "/audit", tag = "Usage", params(AuditQuery),
    responses(
        (status = 200, description = "Audit log entries.", body = Vec<crate::db::AuditEntry>),
        (status = 400, description = "Invalid range.", body = crate::error::AdminErrorBody),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn audit_log(State(state): State<AppState>, Query(q): Query<AuditQuery>) -> R<Json<Vec<db::AuditEntry>>> {
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let target_type = q.target_type.as_deref().map(str::trim).filter(|t| !t.is_empty());
    let search = q.q.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let (since, until) = log_window(q.range.as_deref(), q.from, q.to, Utc::now())?;
    Ok(Json(db::list_audit(&state.db, limit, target_type, search, since, until).await?))
}

/// Where a log range starts and where it ends (exclusive); None is unbounded.
type LogWindow = (Option<DateTime<Utc>>, Option<DateTime<Utc>>);

/// The start and end of a log range: `1h`, `24h`, `7d`, `30d`, `month` (since the first of the
/// month, UTC) or `custom` (whole UTC days `from` to `to`); none or `all` means no limit.
fn log_window(range: Option<&str>, from: Option<chrono::NaiveDate>, to: Option<chrono::NaiveDate>, now: DateTime<Utc>) -> R<LogWindow> {
    let midnight = |d: chrono::NaiveDate| d.and_hms_opt(0, 0, 0).map(|t| t.and_utc());
    Ok(match range {
        None | Some("" | "all") => (None, None),
        Some("1h") => (Some(now - chrono::Duration::hours(1)), None),
        Some("24h") => (Some(now - chrono::Duration::hours(24)), None),
        Some("7d") => (Some(now - chrono::Duration::days(7)), None),
        Some("30d") => (Some(now - chrono::Duration::days(30)), None),
        Some("month") => (now.date_naive().with_day(1).and_then(midnight), None),
        Some("custom") => {
            let (Some(from), Some(to)) = (from, to) else {
                return Err(AdminError::bad_request("a custom range needs from and to dates (YYYY-MM-DD)"));
            };
            if to < from {
                return Err(AdminError::bad_request("the range ends before it starts"));
            }
            (midnight(from), midnight(to).map(|t| t + chrono::Duration::days(1)))
        }
        Some(_) => return Err(AdminError::bad_request("range must be 1h, 24h, 7d, 30d, month or custom")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_windows() {
        let now = DateTime::parse_from_rfc3339("2026-10-25T17:42:30Z").unwrap().with_timezone(&Utc);
        let day = |d: &str| Some(d.parse::<chrono::NaiveDate>().unwrap());
        let text = |(since, until): LogWindow| (since.map(|t| t.to_rfc3339()), until.map(|t| t.to_rfc3339()));
        assert_eq!(log_window(None, None, None, now).unwrap(), (None, None));
        assert_eq!(text(log_window(Some("24h"), None, None, now).unwrap()), (Some("2026-10-24T17:42:30+00:00".into()), None));
        assert_eq!(text(log_window(Some("month"), None, None, now).unwrap()), (Some("2026-10-01T00:00:00+00:00".into()), None));
        // Custom: whole UTC days, both inclusive.
        assert_eq!(
            text(log_window(Some("custom"), day("2026-09-01"), day("2026-09-30"), now).unwrap()),
            (Some("2026-09-01T00:00:00+00:00".into()), Some("2026-10-01T00:00:00+00:00".into()))
        );
        assert!(log_window(Some("custom"), day("2026-09-02"), day("2026-09-01"), now).is_err());
        assert!(log_window(Some("custom"), None, None, now).is_err());
        assert!(log_window(Some("year"), None, None, now).is_err());
    }
}
