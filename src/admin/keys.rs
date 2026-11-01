//! Virtual keys, and the tags they and the deployments carry.

use std::collections::{BTreeMap, HashMap};

use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{LIST_BUDGET_LOOKUPS, R, project_filter, search_text, tag_filter, valid_model_name, yes};
use crate::{
    admin::auth::Access,
    audit::{self, Actor},
    budget::Spender,
    db,
    error::AdminError,
    model::{KeyScope, PromptLogging, VirtualKey, normalize_tags},
    state::AppState,
    sync::broadcast_reload,
};

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct KeyInput {
    name: String,
    /// `inference` (the default), `admin` or `all`; in a project, `inference` or `project`.
    #[serde(default)]
    scope: KeyScope,
    /// The project to group the key under; none when omitted.
    #[serde(default)]
    project_id: Option<Uuid>,
    #[serde(default)]
    allowed_models: Vec<String>,
    /// Tags of existing deployments. Empty = no tag restriction.
    #[serde(default)]
    allowed_tags: Vec<String>,
    rpm_limit: Option<i32>,
    tpm_limit: Option<i32>,
    /// Budget in USD per `budget_period`; both None = unlimited.
    max_budget_usd: Option<f64>,
    /// "day", "week" or "month" (UTC calendar periods).
    budget_period: Option<String>,
    expires_at: Option<DateTime<Utc>>,
    #[serde(default = "yes")]
    enabled: bool,
    #[serde(default)]
    metadata: Option<Value>,
    /// Guardrails every request of the key goes through, on top of its project's. Admins only;
    /// omitted, they stay as they are.
    #[serde(default)]
    guardrail_ids: Option<Vec<Uuid>>,
    /// Whether requests may have the model search the web. A project that forbids it overrides this.
    #[serde(default = "yes")]
    web_search: bool,
    /// Whether its prompts and responses are logged, for debugging, where its project allows it.
    /// A project that denies or forces logging overrides this.
    #[serde(default)]
    prompt_logging: bool,
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct KeyView {
    #[serde(flatten)]
    key: VirtualKey,
    /// Current budget period and its spend; None for unlimited keys.
    budget: Option<crate::budget::BudgetStatus>,
    /// Whether its prompts and responses are logged now: its own `prompt_logging` where its
    /// project allows it, always where the project forces it.
    prompt_logged: bool,
}

fn validate_key(i: &KeyInput) -> R<()> {
    if i.name.trim().is_empty() || i.name.len() > 200 {
        return Err(AdminError::bad_request("name must be 1-200 characters"));
    }
    if i.rpm_limit.is_some_and(|v| v < 1) || i.tpm_limit.is_some_and(|v| v < 1) {
        return Err(AdminError::bad_request("limits must be positive"));
    }
    check_budget(i.max_budget_usd, i.budget_period.as_deref())?;
    match (i.project_id, i.scope) {
        (Some(_), s) if !s.fits_project() => {
            return Err(AdminError::bad_request(
                "a key with every right an admin has stays outside projects; in a project, use the inference or project scope",
            ));
        }
        (None, KeyScope::Project) => return Err(AdminError::bad_request("a key with the project scope needs a project_id")),
        _ => {}
    }
    if i.allowed_models.iter().any(|m| !valid_model_name(m)) {
        return Err(AdminError::bad_request("invalid model name in allowed_models"));
    }
    Ok(())
}

/// The guardrails a key or project gets: `wanted`, each once and in order, all existing; `held`
/// when not given. Only admins choose them, so that no one takes a guardrail off their own keys.
pub(super) async fn pick_guardrails(state: &AppState, access: Option<&Access>, wanted: Option<Vec<Uuid>>, held: &[Uuid]) -> R<Vec<Uuid>> {
    let Some(mut ids) = wanted else { return Ok(held.to_vec()) };
    ids.sort();
    ids.dedup();
    let mut current = held.to_vec();
    current.sort();
    if ids == current {
        return Ok(held.to_vec());
    }
    if access.is_some_and(|a| a.projects().is_some()) {
        return Err(AdminError::forbidden("only admins choose a key's guardrails"));
    }
    let known = db::list_guardrails(&state.db).await?;
    if let Some(id) = ids.iter().find(|id| !known.iter().any(|g| g.id == **id)) {
        return Err(AdminError::bad_request(format!("no such guardrail {id}")));
    }
    Ok(ids)
}

/// Whether a key in `project` may be used: false while the project is inactive.
async fn check_project(state: &AppState, project: Option<Uuid>) -> R<bool> {
    let Some(id) = project else { return Ok(true) };
    let p = db::get_project(&state.db, id).await?.ok_or_else(|| AdminError::bad_request("project_id names no project"))?;
    Ok(p.enabled)
}

/// A key's or project's budget: an amount with a period, or neither for unlimited.
pub(super) fn check_budget(limit: Option<f64>, period: Option<&str>) -> R<()> {
    if limit.is_some_and(|v| v < 0.0 || !v.is_finite()) {
        return Err(AdminError::bad_request("max_budget_usd must be a non-negative number"));
    }
    match (limit, period) {
        (None, None) => Ok(()),
        (Some(_), Some(p)) if crate::budget::BudgetPeriod::parse(p).is_some() => Ok(()),
        (Some(_), Some(_)) => Err(AdminError::bad_request("budget_period must be day, week or month")),
        _ => Err(AdminError::bad_request("set max_budget_usd together with budget_period, or neither for unlimited")),
    }
}

/// Every tag on a deployment, router, MCP server or A2A agent, with how many deployments, routers
/// and MCP servers or agents carry it.
async fn known_tags(state: &AppState) -> R<BTreeMap<String, (usize, usize, usize)>> {
    let (deployments, routers, servers, agents) = tokio::try_join!(
        db::list_deployments(&state.db),
        db::list_routers(&state.db),
        db::list_mcp_servers(&state.db),
        db::list_a2a_agents(&state.db)
    )?;
    let mut tags: BTreeMap<String, (usize, usize, usize)> = BTreeMap::new();
    for t in agents.into_iter().flat_map(|a| a.tags) {
        tags.entry(t).or_default().2 += 1;
    }
    for t in deployments.into_iter().flat_map(|d| d.tags) {
        tags.entry(t).or_default().0 += 1;
    }
    for t in routers.into_iter().flat_map(|r| r.tags) {
        tags.entry(t).or_default().1 += 1;
    }
    for t in servers.into_iter().flat_map(|s| s.tags) {
        tags.entry(t).or_default().2 += 1;
    }
    Ok(tags)
}

/// List tags
///
/// Every tag on deployments, routers and MCP servers, and those keys still hold, with how many
/// of each carry it.
#[utoipa::path(get, path = "/tags", tag = "Configuration",
    responses((status = 200, description = "The tags.", body = Vec<crate::openapi::TagUsage>)),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn list_tags(State(state): State<AppState>, Extension(access): Extension<Access>) -> R<Json<Value>> {
    let (tags, held) = tokio::try_join!(known_tags(&state), async { Ok(db::key_tag_counts(&state.db, access.projects()).await?) })?;
    // Tags only keys still hold, once nothing carries them any more, come last.
    let mut out: Vec<Value> =
        tags.iter()
            .map(|(tag, (d, r, m))| json!({ "tag": tag, "deployments": d, "routers": r, "mcp_servers": m, "keys": held.get(tag).copied().unwrap_or(0) }))
            .collect();
    let mut orphans: Vec<(&String, &i64)> = held.iter().filter(|(t, _)| !tags.contains_key(*t)).collect();
    orphans.sort();
    out.extend(orphans.into_iter().map(|(tag, n)| json!({ "tag": tag, "deployments": 0, "routers": 0, "mcp_servers": 0, "keys": n })));
    Ok(Json(json!(out)))
}

/// A key's tags, normalized. Keys only pick tags that deployments, routers or MCP servers define;
/// tags a key already holds stay valid after the last one carrying them is gone.
async fn key_tags(state: &AppState, wanted: Vec<String>, held: &[String]) -> R<Vec<String>> {
    let tags = normalize_tags(wanted).map_err(AdminError::bad_request)?;
    let known = known_tags(state).await?;
    if let Some(t) = tags.iter().find(|t| !known.contains_key(*t) && !held.contains(t)) {
        return Err(AdminError::bad_request(format!(
            "no deployment, router, MCP server or A2A agent has the tag '{t}'; add tags on the Models or Tools page"
        )));
    }
    Ok(tags)
}

/// Whether the caller sees this key: a user only their projects' keys, which call the inference
/// API or reach what the user does (the project scope).
pub(super) fn sees_key(access: &Access, k: &VirtualKey) -> bool {
    access.sees(k.project_id) && (access.projects().is_none() || k.scope.fits_project())
}

/// What a user may not touch looks like a key that does not exist.
pub(super) fn check_sees_key(access: &Access, k: &VirtualKey) -> R<()> {
    if sees_key(access, k) { Ok(()) } else { Err(AdminError::not_found()) }
}

/// A user's keys stay in their projects, where a key has no more rights than the user
/// (`validate_key` keeps admin and all keys out of projects).
fn check_user_key(access: &Access, i: &KeyInput) -> R<()> {
    if access.projects().is_none() {
        return Ok(());
    }
    if !access.sees(i.project_id) {
        return Err(AdminError::forbidden("put the key in one of your projects"));
    }
    Ok(())
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct KeyQuery {
    /// Just the keys whose name or shown prefix contains this text, ignoring case, or the key
    /// this whole secret belongs to by its prefix.
    q: Option<String>,
    /// Just the keys holding any of these tags, as `azure,cheap`; `-` stands for none.
    tags: Option<String>,
    /// Just the keys in any of these projects (ids, comma-separated); `-` stands for none.
    projects: Option<String>,
    /// Just the keys whose prompts and responses are logged now.
    logging: Option<bool>,
}

/// List virtual keys
///
/// With their current budget period.
#[utoipa::path(get, path = "/keys", tag = "Virtual keys", params(KeyQuery),
    responses(
        (status = 200, description = "The keys.", body = Vec<KeyView>),
        (status = 400, description = "A project filter that is not ids.", body = crate::openapi::AdminError),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn list_keys(
    State(state): State<AppState>,
    Extension(access): Extension<Access>,
    Query(q): Query<KeyQuery>,
) -> R<Json<Vec<KeyView>>> {
    let search = search_text(q.q.as_deref());
    let (tags, projects) = (tag_filter(q.tags.as_deref())?, project_filter(q.projects.as_deref())?);
    let settings = state.settings.get();
    let (logging, gateway_allows) = (q.logging.unwrap_or(false), settings.prompt_logging == PromptLogging::Allow);
    let keys = db::search_keys(&state.db, search.as_deref(), &tags, &projects, access.projects(), logging, gateway_allows).await?;
    let keys: Vec<VirtualKey> = keys.into_iter().filter(|k| sees_key(&access, k)).collect();
    // A few at a time: missing counters are rebuilt from Postgres, which the request path shares.
    let mut budgets = Vec::with_capacity(keys.len());
    for chunk in keys.chunks(LIST_BUDGET_LOOKUPS) {
        budgets.extend(futures::future::join_all(chunk.iter().map(|k| crate::budget::status(&state, k))).await);
    }
    let snap = state.registry.snapshot();
    let view = |(key, budget)| KeyView { prompt_logged: crate::api::prompt_logged(&snap, &settings, &key), key, budget };
    Ok(Json(keys.into_iter().zip(budgets).map(view).collect()))
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct KeyOptionsQuery {
    /// Just the keys that may call models now: enabled, unexpired, in an active project, with an
    /// inference scope.
    usable: Option<bool>,
    /// Just the keys that may call this model (its tags, allowed models and projects all apply).
    model: Option<String>,
}

/// Key names
///
/// The keys a picker offers, by name: id, name and project, without settings or budgets.
#[utoipa::path(get, path = "/keys/options", tag = "Virtual keys", params(KeyOptionsQuery),
    responses((status = 200, description = "The keys.", body = Vec<crate::db::NamedItem>)),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn key_options(
    State(state): State<AppState>,
    Extension(access): Extension<Access>,
    Query(q): Query<KeyOptionsQuery>,
) -> R<Json<Vec<db::NamedItem>>> {
    let usable = q.usable.unwrap_or(false);
    let mut keys = db::key_names(&state.db, access.projects(), usable).await?;
    // Whether a project is active and what a key may call, the registry knows.
    if usable || q.model.is_some() {
        let snap = state.registry.snapshot();
        let by_id: HashMap<Uuid, &std::sync::Arc<VirtualKey>> = snap.keys.values().map(|k| (k.id, k)).collect();
        keys.retain(|n| {
            by_id.get(&n.id).is_some_and(|k| {
                (!usable || crate::auth::can_call_models(k)) && q.model.as_deref().is_none_or(|m| crate::api::key_can_use(&snap, k, m))
            })
        });
    }
    Ok(Json(keys))
}

/// Project names
///
/// The projects a picker offers, by name: id and name only.
#[utoipa::path(get, path = "/projects/options", tag = "Virtual keys",
    responses((status = 200, description = "The projects.", body = Vec<crate::db::NamedItem>)),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn project_options(State(state): State<AppState>, Extension(access): Extension<Access>) -> R<Json<Vec<db::NamedItem>>> {
    Ok(Json(db::project_names(&state.db, access.projects()).await?))
}

/// Create virtual key
///
/// The answer holds its secret, this once; the gateway keeps only a hash.
#[utoipa::path(post, path = "/keys", tag = "Virtual keys", request_body = KeyInput,
    responses(
        (status = 201, description = "The new key and its secret.", body = crate::openapi::KeyCreated),
        (status = 400, description = "Invalid key.", body = crate::openapi::AdminError),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn create_key(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(access): Extension<Access>,
    Json(i): Json<KeyInput>,
) -> R<(StatusCode, Json<Value>)> {
    validate_key(&i)?;
    check_user_key(&access, &i)?;
    let project_active = check_project(&state, i.project_id).await?;
    let allowed_tags = key_tags(&state, i.allowed_tags, &[]).await?;
    let guardrail_ids = pick_guardrails(&state, Some(&access), i.guardrail_ids, &[]).await?;
    let (secret, hash, prefix) = crate::auth::generate_key();
    let now = Utc::now();
    let mut models = i.allowed_models;
    models.sort();
    models.dedup();
    let k = VirtualKey {
        id: Uuid::now_v7(),
        name: i.name.trim().to_string(),
        key_hash: hash,
        key_prefix: prefix,
        scope: i.scope,
        project_id: i.project_id,
        project_active,
        allowed_models: models,
        allowed_tags,
        rpm_limit: i.rpm_limit,
        tpm_limit: i.tpm_limit,
        max_budget_usd: i.max_budget_usd,
        budget_period: i.budget_period,
        spend_usd: 0.0,
        spend_reset_at: None,
        enabled: i.enabled,
        expires_at: i.expires_at,
        metadata: i.metadata.unwrap_or_else(|| json!({})),
        guardrail_ids,
        web_search: i.web_search,
        prompt_logging: i.prompt_logging,
        created_at: now,
        updated_at: now,
    };
    db::insert_key(&state.db, &k).await?;
    broadcast_reload(&state, "key created").await;
    tracing::info!(key = %k.id, name = %k.name, scope = k.scope.as_str(), "virtual key created");
    audit::Event::new("key", "create").on(k.id, &k.name).details(audit::snapshot(&k)).record(&state, &actor).await;
    // The plaintext key is returned exactly once and never stored.
    Ok((StatusCode::CREATED, Json(json!({ "key": secret, "record": k }))))
}

/// Update virtual key
#[utoipa::path(put, path = "/keys/{id}", tag = "Virtual keys", params(("id" = Uuid, Path)),
    request_body = KeyInput,
    responses(
        (status = 200, description = "The key.", body = crate::model::VirtualKey),
        (status = 400, description = "Invalid key.", body = crate::openapi::AdminError),
        (status = 404, description = "No such key.", body = crate::openapi::AdminError),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn update_key(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(access): Extension<Access>,
    Path(id): Path<Uuid>,
    Json(i): Json<KeyInput>,
) -> R<Json<VirtualKey>> {
    validate_key(&i)?;
    let mut k = db::get_key(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    check_sees_key(&access, &k)?;
    check_user_key(&access, &i)?;
    let project_active = check_project(&state, i.project_id).await?;
    let before = k.clone();
    k.allowed_tags = key_tags(&state, i.allowed_tags, &k.allowed_tags).await?;
    k.guardrail_ids = pick_guardrails(&state, Some(&access), i.guardrail_ids, &k.guardrail_ids).await?;
    let mut models = i.allowed_models;
    models.sort();
    models.dedup();
    k.name = i.name.trim().to_string();
    if k.scope != i.scope {
        tracing::info!(key = %k.id, name = %k.name, from = k.scope.as_str(), to = i.scope.as_str(), "virtual key scope changed");
    }
    k.scope = i.scope;
    k.project_id = i.project_id;
    k.project_active = project_active;
    k.allowed_models = models;
    k.rpm_limit = i.rpm_limit;
    k.tpm_limit = i.tpm_limit;
    // A changed period starts counting from its own start; the request log has the history.
    k.max_budget_usd = i.max_budget_usd;
    k.budget_period = i.budget_period;
    k.enabled = i.enabled;
    k.expires_at = i.expires_at;
    k.web_search = i.web_search;
    k.prompt_logging = i.prompt_logging;
    if let Some(m) = i.metadata {
        k.metadata = m;
    }
    db::update_key(&state.db, &k).await?;
    broadcast_reload(&state, "key updated").await;
    let changes = audit::changes(&before, &k);
    if !changes.is_empty() {
        audit::Event::new("key", "update").on(id, &k.name).details(changes.into()).record(&state, &actor).await;
    }
    Ok(Json(k))
}

/// Delete virtual key
#[utoipa::path(delete, path = "/keys/{id}", tag = "Virtual keys", params(("id" = Uuid, Path)),
    responses((status = 204, description = "Deleted."), (status = 404, description = "No such key.", body = crate::openapi::AdminError)),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn delete_key(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(access): Extension<Access>,
    Path(id): Path<Uuid>,
) -> R<StatusCode> {
    let k = db::get_key(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    check_sees_key(&access, &k)?;
    if !db::delete_key(&state.db, id).await? {
        return Err(AdminError::not_found());
    }
    broadcast_reload(&state, "key deleted").await;
    tracing::info!(key = %id, "virtual key deleted");
    audit::Event::new("key", "delete").on(id, k.name).details(json!({ "key_prefix": k.key_prefix })).record(&state, &actor).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Regenerate virtual key
///
/// Gives the key a new secret. It keeps its settings, spend and history.
#[utoipa::path(post, path = "/keys/{id}/regenerate", tag = "Virtual keys", params(("id" = Uuid, Path)),
    responses(
        (status = 200, description = "The new secret.", body = crate::openapi::KeyRegenerated),
        (status = 404, description = "No such key.", body = crate::openapi::AdminError),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
// The old secret stops working as soon as each replica reloads.
pub(super) async fn regenerate_key(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(access): Extension<Access>,
    Path(id): Path<Uuid>,
) -> R<Json<Value>> {
    let k = db::get_key(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    check_sees_key(&access, &k)?;
    let (secret, hash, prefix) = crate::auth::generate_key();
    if !db::rotate_key(&state.db, id, &hash, &prefix).await? {
        return Err(AdminError::not_found());
    }
    broadcast_reload(&state, "key regenerated").await;
    tracing::info!(key = %id, "virtual key regenerated");
    let details = json!({ "key_prefix": { "from": k.key_prefix, "to": prefix } });
    audit::Event::new("key", "regenerate").on(id, k.name).details(details).record(&state, &actor).await;
    // As on create, the plaintext key is returned exactly once and never stored.
    Ok(Json(json!({ "key": secret, "key_prefix": prefix })))
}

/// Reset spend
///
/// Starts the key's current budget period over.
#[utoipa::path(post, path = "/keys/{id}/reset-spend", tag = "Virtual keys", params(("id" = Uuid, Path)),
    responses((status = 204, description = "Reset.")),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn reset_spend(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(access): Extension<Access>,
    Path(id): Path<Uuid>,
) -> R<StatusCode> {
    let k = db::get_key(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    check_sees_key(&access, &k)?;
    db::reset_key_spend(&state.db, id).await?;
    let details = json!({ "spend_usd": k.spend_usd });
    audit::Event::new("key", "reset_spend").on(id, k.name).details(details).record(&state, &actor).await;
    restart_spend(&state, Spender::Key(id), "key spend reset").await;
    Ok(StatusCode::NO_CONTENT)
}

/// After a spend reset in Postgres: replicas must know the new reset time before the counters
/// go, or a request in between could rebuild a counter from the old history. The counters are
/// dropped again shortly after, for peers that apply the reload a moment later.
pub(super) async fn restart_spend(state: &AppState, who: Spender, reason: &str) {
    broadcast_reload(state, reason).await;
    state.cache.reset_period_spend(who).await;
    let cache = state.cache.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        cache.reset_period_spend(who).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A user sees their projects' inference keys only; an admin every key.
    #[test]
    fn users_see_only_their_projects_inference_keys() {
        let (mine, other) = (Uuid::from_u128(1), Uuid::from_u128(2));
        let key = |project: Option<Uuid>, scope: KeyScope| VirtualKey {
            id: Uuid::now_v7(),
            name: "k".into(),
            key_hash: vec![],
            key_prefix: String::new(),
            scope,
            project_id: project,
            project_active: true,
            allowed_models: vec![],
            allowed_tags: vec![],
            rpm_limit: None,
            tpm_limit: None,
            max_budget_usd: None,
            budget_period: None,
            spend_usd: 0.0,
            spend_reset_at: None,
            enabled: true,
            expires_at: None,
            metadata: json!({}),
            guardrail_ids: Vec::new(),
            web_search: true,
            prompt_logging: false,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let user = Access::User { projects: vec![mine] };
        assert!(sees_key(&user, &key(Some(mine), KeyScope::Inference)));
        assert!(!sees_key(&user, &key(Some(other), KeyScope::Inference)));
        assert!(!sees_key(&user, &key(None, KeyScope::Inference)));
        // Its secret would give them an admin's rights.
        assert!(!sees_key(&user, &key(Some(mine), KeyScope::Admin)));
        assert!(!sees_key(&user, &key(Some(mine), KeyScope::All)));
        for k in [key(None, KeyScope::Admin), key(Some(other), KeyScope::Inference)] {
            assert!(sees_key(&Access::Admin, &k));
        }
    }
}
