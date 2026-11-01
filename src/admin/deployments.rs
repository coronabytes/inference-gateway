//! Deployments: a model name served by an upstream model through a credential, and what follows
//! when one is renamed or removed (routers, keys, aliases and model metadata that name it).

use std::collections::{BTreeSet, HashMap, HashSet};

use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{R, aliases::keep_aliases, csv_ids, project_filter, search_text, tag_filter, valid_model_name, yes};
use crate::{
    audit::{self, Actor},
    db,
    error::AdminError,
    ir::{Aggregator, ChatRequest, Message, Part, Role},
    model::{AliasRow, CredentialRow, DeploymentConfig, DeploymentRow, Mode, ProviderKind, RouterRow, VirtualKey, normalize_tags},
    state::AppState,
    sync::broadcast_reload,
};

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct DeploymentView {
    id: Uuid,
    model_name: String,
    mode: String,
    provider: String,
    upstream_model: String,
    config: DeploymentConfig,
    credential_id: Uuid,
    credential_name: Option<String>,
    weight: i32,
    priority: i32,
    rpm_limit: Option<i32>,
    tpm_limit: Option<i32>,
    timeout_secs: i32,
    input_cost_per_mtok: f64,
    output_cost_per_mtok: f64,
    cache_read_cost_per_mtok: Option<f64>,
    cache_write_cost_per_mtok: Option<f64>,
    long_context_threshold: Option<i32>,
    long_input_cost_per_mtok: Option<f64>,
    long_output_cost_per_mtok: Option<f64>,
    long_cache_read_cost_per_mtok: Option<f64>,
    long_cache_write_cost_per_mtok: Option<f64>,
    cost_per_k_searches: Option<f64>,
    base_model: Option<String>,
    tags: Vec<String>,
    project_ids: Vec<Uuid>,
    enabled: bool,
    cooldown_secs: Option<i64>,
    /// Its recent calls, as adaptive routing sees them; absent without any.
    health: Option<HealthView>,
    /// The reasoning efforts it accepts, from its price catalog entry; absent while it is not
    /// loaded (disabled).
    reasoning_efforts: Option<Vec<crate::model::ReasoningEffort>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

/// A deployment's recent calls, each counting half as much a minute later.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct HealthView {
    calls: f64,
    /// Share of the calls that failed with a retryable error (rate limit, server error, timeout).
    error_rate: f64,
    /// Average time to first token of streamed responses.
    ttft_ms: Option<f64>,
    /// The share of its weight the deployment gets now: 1 when healthy, down to 0.05.
    weight_factor: f64,
}

fn view(r: DeploymentRow, credential_name: Option<String>, cooldown: Option<i64>) -> DeploymentView {
    DeploymentView {
        id: r.id,
        model_name: r.model_name,
        mode: r.mode,
        provider: r.provider,
        upstream_model: r.upstream_model,
        // Credential headers saved before they were refused are not shown.
        config: r.config.0.redacted(),
        credential_id: r.credential_id,
        credential_name,
        weight: r.weight,
        priority: r.priority,
        rpm_limit: r.rpm_limit,
        tpm_limit: r.tpm_limit,
        timeout_secs: r.timeout_secs,
        input_cost_per_mtok: r.input_cost_per_mtok,
        output_cost_per_mtok: r.output_cost_per_mtok,
        cache_read_cost_per_mtok: r.cache_read_cost_per_mtok,
        cache_write_cost_per_mtok: r.cache_write_cost_per_mtok,
        long_context_threshold: r.long_context_threshold,
        long_input_cost_per_mtok: r.long_input_cost_per_mtok,
        long_output_cost_per_mtok: r.long_output_cost_per_mtok,
        long_cache_read_cost_per_mtok: r.long_cache_read_cost_per_mtok,
        long_cache_write_cost_per_mtok: r.long_cache_write_cost_per_mtok,
        cost_per_k_searches: r.cost_per_k_searches,
        base_model: r.base_model,
        tags: r.tags,
        project_ids: r.project_ids,
        enabled: r.enabled,
        cooldown_secs: cooldown,
        health: None,
        reasoning_efforts: None,
        created_at: r.created_at,
        updated_at: r.updated_at,
    }
}

/// The health of every deployment with recent calls, each weighed against the others of its
/// model, as `router::pick` does.
async fn deployment_health(state: &AppState, rows: &[DeploymentRow]) -> HashMap<Uuid, HealthView> {
    if !state.settings.get().adaptive_routing {
        return HashMap::new();
    }
    let ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
    let health = state.cache.health(&ids).await;
    let mut models: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, r) in rows.iter().enumerate() {
        models.entry(r.model_name.as_str()).or_default().push(i);
    }
    let mut out = HashMap::new();
    for members in models.values() {
        let priorities: Vec<i32> = members.iter().map(|&i| rows[i].priority).collect();
        let hs: Vec<crate::cache::Health> = members.iter().map(|&i| health[i]).collect();
        for (&i, factor) in members.iter().zip(crate::router::factors(&priorities, &hs)) {
            let h = health[i];
            let calls = h.ok + h.err;
            if calls >= 0.05 {
                out.insert(rows[i].id, HealthView { calls, error_rate: h.err / calls, ttft_ms: h.ttft_ms, weight_factor: factor });
            }
        }
    }
    out
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeploymentInput {
    pub(super) model_name: String,
    /// chat (default), embedding, decision or rerank.
    #[serde(default)]
    pub(super) mode: Mode,
    provider: String,
    pub(super) upstream_model: String,
    /// An existing credential for the same provider. Create credentials first.
    credential_id: Uuid,
    #[serde(default)]
    config: DeploymentConfig,
    #[serde(default = "one")]
    weight: i32,
    #[serde(default)]
    priority: i32,
    rpm_limit: Option<i32>,
    tpm_limit: Option<i32>,
    #[serde(default = "default_timeout")]
    timeout_secs: i32,
    #[serde(default)]
    input_cost_per_mtok: f64,
    #[serde(default)]
    output_cost_per_mtok: f64,
    cache_read_cost_per_mtok: Option<f64>,
    cache_write_cost_per_mtok: Option<f64>,
    /// Input tokens above which the long_* prices apply to the whole request. None = one price tier.
    long_context_threshold: Option<i32>,
    long_input_cost_per_mtok: Option<f64>,
    long_output_cost_per_mtok: Option<f64>,
    long_cache_read_cost_per_mtok: Option<f64>,
    long_cache_write_cost_per_mtok: Option<f64>,
    /// USD per 1,000 searches: for rerank models a query over up to 100 documents, for chat
    /// models a web search the model ran.
    cost_per_k_searches: Option<f64>,
    /// Price catalog key, e.g. `azure/gpt-4o` for an Azure deployment with its own name.
    #[serde(default)]
    base_model: Option<String>,
    /// Permission tags; a key with tags needs all of them to use this deployment.
    #[serde(default)]
    pub(super) tags: Vec<String>,
    /// Projects whose keys alone may use this deployment, whatever tags other keys hold; empty
    /// for any key. Omitted, an update keeps the deployment's projects.
    #[serde(default)]
    pub(super) project_ids: Option<Vec<Uuid>>,
    #[serde(default = "yes")]
    enabled: bool,
}

fn one() -> i32 {
    1
}
fn default_timeout() -> i32 {
    600
}
/// Validates the input and returns the referenced credential's name.
/// `id` is the deployment being updated, if any.
async fn validate(state: &AppState, id: Option<Uuid>, i: &DeploymentInput) -> R<String> {
    let provider = check_deployment(i)?;
    let cred = db::get_credential(&state.db, i.credential_id)
        .await?
        .ok_or_else(|| AdminError::bad_request("credential_id does not refer to an existing credential"))?;
    let (deployments, routers, aliases) =
        tokio::try_join!(db::list_deployments(&state.db), db::list_routers(&state.db), db::list_aliases(&state.db))?;
    check_deployment_fits(i, provider, id, &cred, &deployments, &routers, &aliases)?;
    Ok(cred.name)
}

/// Sorts the projects a deployment is limited to, drops repeats, and checks that each exists.
async fn check_projects(state: &AppState, ids: Option<&mut Vec<Uuid>>) -> R<()> {
    let Some(ids) = ids else { return Ok(()) };
    ids.sort();
    ids.dedup();
    if ids.is_empty() {
        return Ok(());
    }
    let projects = db::list_project_rows(&state.db).await?;
    match ids.iter().find(|id| !projects.iter().any(|p| p.id == **id)) {
        Some(id) => Err(AdminError::bad_request(format!("project_ids: {id} names no project"))),
        None => Ok(()),
    }
}

/// The checks of a deployment that need nothing but the input itself.
pub(super) fn check_deployment(i: &DeploymentInput) -> R<ProviderKind> {
    let provider = ProviderKind::parse(&i.provider).ok_or_else(|| AdminError::bad_request("unknown provider"))?;
    if !valid_model_name(&i.model_name) {
        return Err(AdminError::bad_request("model_name must be 1-128 chars of [A-Za-z0-9-_.:/@]"));
    }
    if i.upstream_model.trim().is_empty() {
        return Err(AdminError::bad_request("upstream_model is required"));
    }
    if i.base_model.as_ref().is_some_and(|b| b.len() > 300) {
        return Err(AdminError::bad_request("base_model can be at most 300 characters"));
    }
    let style = i.mode.style(provider, i.config.api_style).map_err(AdminError::bad_request)?;
    if i.config.bedrock_api.is_some() && !(provider == ProviderKind::Bedrock && style == crate::model::ApiStyle::Anthropic) {
        return Err(AdminError::bad_request("bedrock_api only applies to Bedrock deployments with the Anthropic Messages protocol"));
    }
    i.config.check().map_err(AdminError::bad_request)?;
    if i.weight < 1 || i.timeout_secs < 1 {
        return Err(AdminError::bad_request("weight and timeout_secs must be positive"));
    }
    if i.rpm_limit.is_some_and(|v| v < 1) || i.tpm_limit.is_some_and(|v| v < 1) {
        return Err(AdminError::bad_request("limits must be positive"));
    }
    let prices = [
        Some(i.input_cost_per_mtok),
        Some(i.output_cost_per_mtok),
        i.cache_read_cost_per_mtok,
        i.cache_write_cost_per_mtok,
        i.long_input_cost_per_mtok,
        i.long_output_cost_per_mtok,
        i.long_cache_read_cost_per_mtok,
        i.long_cache_write_cost_per_mtok,
        i.cost_per_k_searches,
    ];
    if prices.into_iter().flatten().any(|p| !p.is_finite() || p < 0.0) {
        return Err(AdminError::bad_request("prices must be non-negative numbers"));
    }
    match i.long_context_threshold {
        Some(t) if t < 1 => return Err(AdminError::bad_request("long_context_threshold must be a positive token count")),
        Some(_) if i.long_input_cost_per_mtok.is_none() || i.long_output_cost_per_mtok.is_none() => {
            return Err(AdminError::bad_request("long-context pricing needs long_input_cost_per_mtok and long_output_cost_per_mtok"));
        }
        _ => {}
    }
    Ok(provider)
}

/// The checks of a deployment against its credential and the rest of the configuration
/// (`deployments`, `routers` and `aliases`; `deployments` may include the deployment itself, as `id`).
pub(super) fn check_deployment_fits(
    i: &DeploymentInput,
    provider: ProviderKind,
    id: Option<Uuid>,
    cred: &CredentialRow,
    deployments: &[DeploymentRow],
    routers: &[RouterRow],
    aliases: &[AliasRow],
) -> R<()> {
    if cred.provider != provider.as_str() {
        return Err(AdminError::bad_request(format!("credential '{}' is for {}, not {}", cred.name, cred.provider, i.provider)));
    }
    if routers.iter().any(|r| r.model_name == i.model_name) {
        return Err(AdminError::bad_request(format!("'{}' is an auto router's model name", i.model_name)));
    }
    if aliases.iter().any(|a| a.model_name == i.model_name) {
        return Err(AdminError::bad_request(format!("'{}' is a model alias", i.model_name)));
    }
    // One model name is one kind of model: clients call one family of endpoints for it.
    let other = deployments.iter().find(|d| d.model_name == i.model_name && Some(d.id) != id && d.mode != i.mode.as_str());
    if let Some(d) = other {
        return Err(AdminError::bad_request(format!(
            "'{}' already has {} deployments; all deployments of a model name need the same mode",
            i.model_name, d.mode
        )));
    }
    Ok(())
}

pub(super) fn to_row(id: Uuid, i: DeploymentInput, created: DateTime<Utc>) -> DeploymentRow {
    DeploymentRow {
        id,
        model_name: i.model_name,
        mode: i.mode.as_str().into(),
        provider: i.provider,
        upstream_model: i.upstream_model.trim().to_string(),
        // Only chat deployments choose a protocol.
        config: sqlx::types::Json(DeploymentConfig { api_style: i.config.api_style.filter(|_| i.mode == Mode::Chat), ..i.config }),
        credential_id: i.credential_id,
        weight: i.weight,
        priority: i.priority,
        rpm_limit: i.rpm_limit,
        tpm_limit: i.tpm_limit,
        timeout_secs: i.timeout_secs,
        input_cost_per_mtok: i.input_cost_per_mtok,
        output_cost_per_mtok: i.output_cost_per_mtok,
        cache_read_cost_per_mtok: i.cache_read_cost_per_mtok,
        cache_write_cost_per_mtok: i.cache_write_cost_per_mtok,
        // Without a threshold there is no long-context tier, so its prices are not kept.
        long_context_threshold: i.long_context_threshold,
        long_input_cost_per_mtok: i.long_context_threshold.and(i.long_input_cost_per_mtok),
        long_output_cost_per_mtok: i.long_context_threshold.and(i.long_output_cost_per_mtok),
        long_cache_read_cost_per_mtok: i.long_context_threshold.and(i.long_cache_read_cost_per_mtok),
        long_cache_write_cost_per_mtok: i.long_context_threshold.and(i.long_cache_write_cost_per_mtok),
        // Rerank models bill their searches, chat models the web searches they run.
        cost_per_k_searches: i.cost_per_k_searches.filter(|_| matches!(i.mode, Mode::Rerank | Mode::Chat)),
        base_model: i.base_model.map(|b| b.trim().to_string()).filter(|b| !b.is_empty()),
        tags: i.tags,
        project_ids: i.project_ids.unwrap_or_default(),
        enabled: i.enabled,
        created_at: created,
        updated_at: Utc::now(),
    }
}

async fn credential_names(state: &AppState) -> R<HashMap<Uuid, String>> {
    Ok(db::list_credentials(&state.db).await?.into_iter().map(|c| (c.id, c.name)).collect())
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct DeploymentQuery {
    /// Just the deployments whose model name contains this text, ignoring case.
    q: Option<String>,
    /// Just the deployments of this mode: `chat`, `embedding`, `decision` or `rerank`.
    mode: Option<String>,
    /// Just the deployments carrying any of these tags, as `azure,cheap`; `-` stands for none.
    tags: Option<String>,
    /// Just the deployments using any of these credentials (ids, comma-separated).
    credentials: Option<String>,
    /// Just the deployments limited to any of these projects (ids, comma-separated); `-` stands
    /// for those open to every project.
    projects: Option<String>,
    /// Just these deployments (ids, comma-separated).
    ids: Option<String>,
}

/// List deployments
///
/// With their cooldowns.
#[utoipa::path(get, path = "/deployments", tag = "Deployments", params(DeploymentQuery),
    responses(
        (status = 200, description = "The deployments.", body = Vec<DeploymentView>),
        (status = 400, description = "An unknown mode, or a filter that is not ids.", body = crate::openapi::AdminError),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn list_deployments(State(state): State<AppState>, Query(q): Query<DeploymentQuery>) -> R<Json<Vec<DeploymentView>>> {
    let search = search_text(q.q.as_deref());
    let mode = q.mode.as_deref().map(str::trim).filter(|m| !m.is_empty());
    if let Some(m) = mode.filter(|m| Mode::parse(m).is_none()) {
        return Err(AdminError::bad_request(format!("unknown mode '{m}'")));
    }
    let (tags, projects) = (tag_filter(q.tags.as_deref())?, project_filter(q.projects.as_deref())?);
    let (credentials, ids) = (csv_ids(q.credentials.as_deref(), "credentials")?, csv_ids(q.ids.as_deref(), "ids")?);
    let rows = db::search_deployments(&state.db, search.as_deref(), mode, &tags, credentials.as_deref(), &projects, ids.as_deref());
    let (rows, names) = tokio::try_join!(async { Ok(rows.await?) }, credential_names(&state))?;
    let ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
    let (cds, mut health) = tokio::join!(state.cache.cooldown_ttls(&ids), deployment_health(&state, &rows));
    let snap = state.registry.snapshot();
    let efforts: HashMap<Uuid, Vec<crate::model::ReasoningEffort>> =
        snap.deployments.values().flatten().map(|d| (d.id, d.efforts())).collect();
    Ok(Json(
        rows.into_iter()
            .map(|r| {
                let (name, cd, h) = (names.get(&r.credential_id).cloned(), cds.get(&r.id).copied(), health.remove(&r.id));
                let reasoning_efforts = efforts.get(&r.id).cloned();
                DeploymentView { health: h, reasoning_efforts, ..view(r, name, cd) }
            })
            .collect(),
    ))
}

/// Get deployment
#[utoipa::path(get, path = "/deployments/{id}", tag = "Deployments", params(("id" = Uuid, Path)),
    responses(
        (status = 200, description = "The deployment.", body = DeploymentView),
        (status = 404, description = "No such deployment.", body = crate::openapi::AdminError),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn get_deployment(State(state): State<AppState>, Path(id): Path<Uuid>) -> R<Json<DeploymentView>> {
    let row = db::get_deployment(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    let name = credential_names(&state).await?.remove(&row.credential_id);
    Ok(Json(view(row, name, None)))
}

/// Create deployment
#[utoipa::path(post, path = "/deployments", tag = "Deployments", request_body = DeploymentInput,
    responses(
        (status = 201, description = "The new deployment.", body = DeploymentView),
        (status = 400, description = "Invalid deployment.", body = crate::openapi::AdminError),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn create_deployment(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(mut i): Json<DeploymentInput>,
) -> R<(StatusCode, Json<DeploymentView>)> {
    i.tags = normalize_tags(std::mem::take(&mut i.tags)).map_err(AdminError::bad_request)?;
    check_projects(&state, i.project_ids.as_mut()).await?;
    let cred_name = validate(&state, None, &i).await?;
    let row = to_row(Uuid::now_v7(), i, Utc::now());
    db::upsert_deployment(&state.db, &row).await?;
    broadcast_reload(&state, "deployment created").await;
    tracing::info!(deployment = %row.id, model = %row.model_name, "deployment created");
    let view = view(row, Some(cred_name), None);
    audit::Event::new("deployment", "create").on(view.id, &view.model_name).details(audit::snapshot(&view)).record(&state, &actor).await;
    Ok((StatusCode::CREATED, Json(view)))
}

/// Update deployment
///
/// Its mode cannot change; a renamed model is followed by routers and keys.
#[utoipa::path(put, path = "/deployments/{id}", tag = "Deployments", params(("id" = Uuid, Path)),
    request_body = DeploymentInput,
    responses(
        (status = 200, description = "The deployment.", body = DeploymentView),
        (status = 400, description = "Invalid deployment.", body = crate::openapi::AdminError),
        (status = 404, description = "No such deployment.", body = crate::openapi::AdminError),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn update_deployment(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<Uuid>,
    Json(mut i): Json<DeploymentInput>,
) -> R<Json<DeploymentView>> {
    let existing = db::get_deployment(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    // Clients of a model call the endpoints of its mode; changing it would break them.
    if existing.mode != i.mode.as_str() {
        return Err(AdminError::bad_request("a deployment's mode cannot be changed; create a new deployment"));
    }
    i.tags = normalize_tags(std::mem::take(&mut i.tags)).map_err(AdminError::bad_request)?;
    // Clients that predate projects on deployments leave them as they are.
    check_projects(&state, i.project_ids.as_mut()).await?;
    i.project_ids.get_or_insert_with(|| existing.project_ids.clone());
    let cred_name = validate(&state, Some(id), &i).await?;
    let row = to_row(id, i, existing.created_at);
    db::upsert_deployment(&state.db, &row).await?;
    if existing.model_name != row.model_name {
        keep_aliases(&state, id, &existing.model_name).await?;
        follow_rename(&state, &existing.model_name, &row.model_name).await?;
    }
    broadcast_reload(&state, "deployment updated").await;
    tracing::info!(deployment = %id, "deployment updated");
    let before_cred = if existing.credential_id == row.credential_id {
        Some(cred_name.clone())
    } else {
        credential_names(&state).await?.remove(&existing.credential_id)
    };
    let before = view(existing, before_cred, None);
    let view = view(row, Some(cred_name), None);
    let changes = audit::changes(&before, &view);
    if !changes.is_empty() {
        audit::Event::new("deployment", "update").on(id, &view.model_name).details(changes.into()).record(&state, &actor).await;
    }
    Ok(Json(view))
}

/// Once no deployment or router keeps a model's old name, routers and keys that named it follow
/// the rename, so routing and key access keep working, and so does the model's metadata.
pub(super) async fn follow_rename(state: &AppState, old: &str, new: &str) -> R<()> {
    let (deployments, mut routers, mut keys) =
        tokio::try_join!(db::list_deployments(&state.db), db::list_routers(&state.db), db::list_keys(&state.db))?;
    let (changed_routers, changed_keys) = rename_model(old, new, &deployments, &mut routers, &mut keys);
    for r in routers.iter().filter(|r| changed_routers.contains(&r.id)) {
        db::upsert_router(&state.db, r).await?;
        tracing::info!(router = %r.id, old, new, "router follows model rename");
    }
    for k in keys.iter().filter(|k| changed_keys.contains(&k.id)) {
        db::update_key(&state.db, k).await?;
        tracing::info!(key = %k.id, old, new, "key follows model rename");
    }
    // The model hub's name and description too, once nothing has the old name.
    let still_named = deployments.iter().any(|d| d.model_name == old) || routers.iter().any(|r| r.model_name == old);
    if old != new && !still_named && db::rename_model_metadata(&state.db, old, new).await? {
        tracing::info!(old, new, "model metadata follows model rename");
    }
    Ok(())
}

/// Points the routers and keys that name model `old` at `new`, unless a deployment or router is
/// still called `old`. Returns the ids of the routers and keys it changed. (Model metadata follows
/// separately, see `follow_rename` and the import.)
pub(crate) fn rename_model(
    old: &str,
    new: &str,
    deployments: &[DeploymentRow],
    routers: &mut [RouterRow],
    keys: &mut [VirtualKey],
) -> (HashSet<Uuid>, HashSet<Uuid>) {
    let (mut changed_routers, mut changed_keys) = (HashSet::new(), HashSet::new());
    if old == new || deployments.iter().any(|d| d.model_name == old) || routers.iter().any(|r| r.model_name == old) {
        return (changed_routers, changed_keys);
    }
    for r in routers.iter_mut() {
        if rename_in_router(r, old, new) {
            changed_routers.insert(r.id);
        }
    }
    for k in keys.iter_mut().filter(|k| k.allowed_models.iter().any(|m| m == old)) {
        for m in k.allowed_models.iter_mut().filter(|m| *m == old) {
            *m = new.to_string();
        }
        k.allowed_models.sort();
        k.allowed_models.dedup();
        changed_keys.insert(k.id);
    }
    (changed_routers, changed_keys)
}

/// Point a router's references to `old` at `new`; returns whether anything changed. If a route
/// already had `new` among its models, the first of the two is kept.
fn rename_in_router(r: &mut RouterRow, old: &str, new: &str) -> bool {
    let mut changed = false;
    if r.decision_model == old {
        r.decision_model = new.to_string();
        changed = true;
    }
    for route in r.routes.0.iter_mut() {
        for m in route.models.iter_mut().filter(|m| m.model == old) {
            m.model = new.to_string();
            changed = true;
        }
        let mut seen = BTreeSet::new();
        route.models.retain(|m| seen.insert(m.model.clone()));
    }
    changed
}

/// Delete deployment
#[utoipa::path(delete, path = "/deployments/{id}", tag = "Deployments", params(("id" = Uuid, Path)),
    responses((status = 204, description = "Deleted."), (status = 404, description = "No such deployment.", body = crate::openapi::AdminError)),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn delete_deployment(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<Uuid>,
) -> R<StatusCode> {
    let existing = db::get_deployment(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    if !db::delete_deployment(&state.db, id).await? {
        return Err(AdminError::not_found());
    }
    keep_aliases(&state, id, &existing.model_name).await?;
    broadcast_reload(&state, "deployment deleted").await;
    tracing::info!(deployment = %id, "deployment deleted");
    let details = json!({ "upstream_model": existing.upstream_model, "provider": existing.provider });
    audit::Event::new("deployment", "delete").on(id, existing.model_name).details(details).record(&state, &actor).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Clear cooldown
///
/// Puts a cooling-down deployment back into rotation.
#[utoipa::path(post, path = "/deployments/{id}/clear-cooldown", tag = "Deployments", params(("id" = Uuid, Path)),
    responses((status = 204, description = "Cleared.")),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn clear_cooldown(State(state): State<AppState>, Extension(actor): Extension<Actor>, Path(id): Path<Uuid>) -> StatusCode {
    state.cache.clear_cooldown(id).await;
    let name = state.registry.snapshot().deployments.values().flatten().find(|d| d.id == id).map(|d| d.model_name.clone());
    audit::Event::new("deployment", "clear_cooldown").on(id, name.unwrap_or_default()).record(&state, &actor).await;
    StatusCode::NO_CONTENT
}

/// Test deployment
///
/// Sends a tiny request through the deployment, bypassing routing.
#[utoipa::path(post, path = "/deployments/{id}/test", tag = "Deployments", params(("id" = Uuid, Path)),
    responses(
        (status = 200, description = "The result.", body = crate::openapi::DeploymentTest),
        (status = 400, description = "The deployment is disabled or misconfigured.", body = crate::openapi::AdminError),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn test_deployment(State(state): State<AppState>, Path(id): Path<Uuid>) -> R<Json<Value>> {
    let snap = state.registry.snapshot();
    let d = snap
        .deployments
        .values()
        .flatten()
        .find(|d| d.id == id)
        .cloned()
        .ok_or_else(|| AdminError::bad_request("deployment is not active (disabled or misconfigured)"))?;
    if d.mode == Mode::Embedding {
        let start = std::time::Instant::now();
        let result = crate::api::embeddings::probe(&state, &d).await;
        let latency_ms = start.elapsed().as_millis() as u64;
        return Ok(Json(match result {
            Ok((dims, usage)) => json!({
                "ok": true,
                "latency_ms": latency_ms,
                "text": format!("{dims} dimensions"),
                "usage": { "input_tokens": usage.input_tokens, "output_tokens": 0 },
            }),
            Err(e) => json!({ "ok": false, "latency_ms": latency_ms, "status": e.status.as_u16(), "error": e.message }),
        }));
    }
    if d.mode == Mode::Rerank {
        let start = std::time::Instant::now();
        let result = crate::api::rerank::probe(&state, &d).await;
        let latency_ms = start.elapsed().as_millis() as u64;
        return Ok(Json(match result {
            Ok((best, score, usage)) => json!({
                "ok": true,
                "latency_ms": latency_ms,
                // The second document answers the question.
                "text": format!("{} first (score {score:.3})", if best == 1 { "relevant document" } else { "unrelated document" }),
                "usage": { "input_tokens": usage.input_tokens, "output_tokens": 0 },
            }),
            Err(e) => json!({ "ok": false, "latency_ms": latency_ms, "status": e.status.as_u16(), "error": e.message }),
        }));
    }
    if d.mode == Mode::Decision {
        let start = std::time::Instant::now();
        let result = crate::api::decisions::probe(&state, &d).await;
        let latency_ms = start.elapsed().as_millis() as u64;
        return Ok(Json(match result {
            Ok(r) => json!({
                "ok": true,
                "latency_ms": latency_ms,
                "text": format!("reachable: {}", r.pointer("/answers/reachable/noul").and_then(Value::as_f64).unwrap_or_default()),
                "usage": { "input_tokens": r.pointer("/usage/input_tokens"), "output_tokens": r.pointer("/usage/output_tokens") },
            }),
            Err(e) => json!({ "ok": false, "latency_ms": latency_ms, "status": e.status.as_u16(), "error": e.message }),
        }));
    }
    let req = ChatRequest {
        messages: vec![Message {
            role: Role::User,
            content: vec![Part::Text { text: "Reply with the single word: OK".into(), cache: false }],
        }],
        max_tokens: Some(256),
        ..Default::default()
    };
    let start = std::time::Instant::now();
    let result = async {
        let mut events = crate::providers::stream_chat(&state, &d, &req).await?;
        let mut agg = Aggregator::default();
        while let Some(ev) = events.next().await {
            agg.push(&ev?);
        }
        Ok::<_, crate::error::GatewayError>(agg.finish())
    }
    .await;
    let latency_ms = start.elapsed().as_millis() as u64;
    Ok(Json(match result {
        Ok(r) => json!({
            "ok": true,
            "latency_ms": latency_ms,
            "text": crate::util::truncate(&crate::ir::parts_text(&r.content), 200),
            "usage": { "input_tokens": r.usage.total_input(), "output_tokens": r.usage.output_tokens },
        }),
        Err(e) => json!({ "ok": false, "latency_ms": latency_ms, "status": e.status.as_u16(), "error": e.message }),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Route;

    #[test]
    fn routers_follow_renames() {
        let route = |name: &str, models: &[&str]| Route {
            name: name.into(),
            description: "d".into(),
            models: models.iter().map(|m| crate::model::RouteModel { model: m.to_string(), reasoning_effort: None }).collect(),
        };
        let mut r = RouterRow {
            id: Uuid::nil(),
            model_name: "auto".into(),
            decision_model: "eu/decision".into(),
            instructions: None,
            routes: sqlx::types::Json(vec![route("Simple", &["eu/luna", "haiku"]), route("Complex", &["eu/luna", "luna"])]),
            fallback_route: Some("Simple".into()),
            tags: vec![],
            enabled: true,
            sticky: true,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        assert!(rename_in_router(&mut r, "eu/luna", "luna"));
        let models = |i: usize| r.routes.0[i].model_names().map(str::to_string).collect::<Vec<_>>();
        assert_eq!(models(0), ["luna", "haiku"]);
        assert_eq!(models(1), ["luna"]);
        assert_eq!(r.fallback_route.as_deref(), Some("Simple"));
        assert!(rename_in_router(&mut r, "eu/decision", "decision"));
        assert_eq!(r.decision_model, "decision");
        assert!(!rename_in_router(&mut r, "unrelated", "x"));
    }

    #[test]
    fn keys_and_routers_follow_a_model_rename() {
        let route = |models: &[&str]| Route {
            name: "Only".into(),
            description: "d".into(),
            models: models.iter().map(|m| crate::model::RouteModel { model: m.to_string(), reasoning_effort: None }).collect(),
        };
        let router = |name: &str, models: &[&str]| RouterRow {
            id: Uuid::now_v7(),
            model_name: name.into(),
            decision_model: "decision".into(),
            instructions: None,
            routes: sqlx::types::Json(vec![route(models)]),
            fallback_route: None,
            tags: vec![],
            enabled: true,
            sticky: true,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let key = |models: &[&str]| VirtualKey {
            id: Uuid::now_v7(),
            name: "k".into(),
            key_hash: vec![],
            key_prefix: String::new(),
            scope: crate::model::KeyScope::Inference,
            project_id: None,
            project_active: true,
            allowed_models: models.iter().map(|m| m.to_string()).collect(),
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
        let mut routers = vec![router("auto", &["luna", "haiku"]), router("other", &["haiku"])];
        let mut keys = vec![key(&["luna", "auto"]), key(&["haiku"]), key(&["luna", "sol"])];
        let (r, k) = rename_model("luna", "sol", &[], &mut routers, &mut keys);
        assert_eq!(r, HashSet::from([routers[0].id]));
        assert_eq!(k, HashSet::from([keys[0].id, keys[2].id]));
        assert_eq!(routers[0].routes.0[0].model_names().collect::<Vec<_>>(), ["sol", "haiku"]);
        assert_eq!(keys[0].allowed_models, ["auto", "sol"]);
        assert_eq!(keys[2].allowed_models, ["sol"]);

        // A router's rename: keys that name it follow.
        routers[0].model_name = "smart".into();
        let (r, k) = rename_model("auto", "smart", &[], &mut routers, &mut keys);
        assert!(r.is_empty());
        assert_eq!(k, HashSet::from([keys[0].id]));
        assert_eq!(keys[0].allowed_models, ["smart", "sol"]);

        // While something still has the old name, nothing follows.
        let (r, k) = rename_model("other", "x", &[], &mut routers, &mut keys);
        assert!(r.is_empty() && k.is_empty());
    }
}
