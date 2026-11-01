//! Admin API for A2A agents: where the gateway finds an agent's card, how it authenticates there,
//! which keys may use the agent and what a message costs; and trying one out.

use axum::{
    Extension, Json,
    extract::{Path, State},
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    R,
    mcp::{check_auth, check_name, check_projects, check_url, secret_destination},
};
use crate::{
    a2a::{A2aAgent, A2aAgentRow, CardSummary},
    audit::{self, Actor},
    db,
    error::AdminError,
    mcp::McpAuth,
    model::normalize_tags,
    oauth::OAuthConfig,
    state::AppState,
    sync::broadcast_reload,
};

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct A2aAgentView {
    id: Uuid,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    description: Option<String>,
    card_url: String,
    auth: McpAuth,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    auth_header: Option<String>,
    /// Whether a secret is stored. The secret itself is never returned.
    has_secret: bool,
    /// The OAuth client, for `oauth` and `oauth_user` auth.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    oauth: Option<OAuthConfig>,
    tags: Vec<String>,
    project_ids: Vec<Uuid>,
    timeout_secs: i32,
    /// USD per message sent to the agent.
    price_per_message: f64,
    enabled: bool,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

fn view(a: A2aAgentRow) -> A2aAgentView {
    A2aAgentView {
        id: a.id,
        name: a.name,
        description: a.description,
        card_url: a.card_url,
        auth: McpAuth::parse(&a.auth).unwrap_or(McpAuth::None),
        auth_header: a.auth_header,
        has_secret: a.secret.is_some(),
        oauth: a.oauth.map(|o| o.0),
        tags: a.tags,
        project_ids: a.project_ids,
        timeout_secs: a.timeout_secs,
        price_per_message: a.price_per_message,
        enabled: a.enabled,
        created_at: a.created_at,
        updated_at: a.updated_at,
    }
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct A2aAgentInput {
    /// What clients call it by, at `/a2a/{name}`: lowercase letters, digits, `-` and `_`.
    name: String,
    #[serde(default)]
    description: Option<String>,
    /// The agent's card, such as `https://agent.example.com/.well-known/agent-card.json`. A URL
    /// without a path gets `/.well-known/agent-card.json`.
    card_url: String,
    auth: McpAuth,
    /// The header that carries the secret, for `header` auth.
    #[serde(default)]
    auth_header: Option<String>,
    /// The token or API key the gateway sends, for `bearer` and `header` auth, or the OAuth client's
    /// secret. Write-only; omitted on an update, the agent keeps its secret.
    #[serde(default)]
    secret: Option<String>,
    /// The OAuth client, for `oauth` and `oauth_user` auth.
    #[serde(default)]
    oauth: Option<OAuthConfig>,
    /// A key with tags may only use untagged agents and those sharing a tag with it.
    #[serde(default)]
    tags: Vec<String>,
    /// Projects the agent is limited to; empty means any key may use it (as its tags allow).
    #[serde(default)]
    project_ids: Vec<Uuid>,
    /// Seconds an answer that is not streamed may take. Default 600.
    #[serde(default)]
    timeout_secs: Option<i32>,
    /// USD per message sent to the agent, charged once it answered. Default 0.
    #[serde(default)]
    price_per_message: f64,
    #[serde(default = "enabled")]
    enabled: bool,
}

/// Headers the gateway passes on to A2A agents itself: no secret goes in them.
pub(super) const A2A_RESERVED: [&str; 3] = ["a2a-version", "a2a-extensions", "x-a2a-extensions"];

fn enabled() -> bool {
    true
}

/// An agent card's URL; one without a path gets `/.well-known/agent-card.json`.
pub(super) fn check_card_url(state: &AppState, url: &str) -> R<String> {
    let mut card_url = check_url(state, url)?;
    if reqwest::Url::parse(&card_url).is_ok_and(|u| u.path() == "/" && u.query().is_none()) {
        card_url = format!("{}/.well-known/agent-card.json", card_url.trim_end_matches('/'));
    }
    Ok(card_url)
}

/// Checks an input and builds the row it describes; `existing` keeps its secret when none is given.
async fn row(state: &AppState, i: A2aAgentInput, existing: Option<&A2aAgentRow>) -> R<A2aAgentRow> {
    let name = check_name(&i.name, "agent")?;
    let description = i.description.as_deref().map(str::trim).filter(|d| !d.is_empty()).map(str::to_string);
    let card_url = check_card_url(state, &i.card_url)?;
    let auth = check_auth(
        state,
        &card_url,
        i.auth,
        i.auth_header.as_deref(),
        i.secret.as_deref(),
        i.oauth,
        existing.and_then(|e| Some((e.secret.as_deref()?, secret_destination(&e.card_url, e.oauth.as_deref())))),
        &A2A_RESERVED,
    )?;
    let tags = normalize_tags(i.tags).map_err(AdminError::bad_request)?;
    let project_ids = check_projects(state, i.project_ids).await?;
    let timeout_secs = i.timeout_secs.unwrap_or(existing.map_or(600, |e| e.timeout_secs));
    if !(1..=3600).contains(&timeout_secs) {
        return Err(AdminError::bad_request("the timeout is 1 to 3600 seconds"));
    }
    if !(i.price_per_message.is_finite() && i.price_per_message >= 0.0) {
        return Err(AdminError::bad_request("the price is zero or more USD"));
    }
    let now = Utc::now();
    Ok(A2aAgentRow {
        id: existing.map_or_else(Uuid::now_v7, |e| e.id),
        name,
        description,
        card_url,
        auth: i.auth.as_str().to_string(),
        auth_header: auth.header,
        secret: auth.secret,
        oauth: auth.oauth,
        tags,
        project_ids,
        timeout_secs,
        price_per_message: i.price_per_message,
        enabled: i.enabled,
        created_at: existing.map_or(now, |e| e.created_at),
        updated_at: now,
    })
}

/// List A2A agents
#[utoipa::path(get, path = "/a2a-agents", tag = "A2A",
    responses((status = 200, description = "The A2A agents.", body = Vec<A2aAgentView>)),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(crate) async fn list_a2a_agents(State(state): State<AppState>) -> R<Json<Vec<A2aAgentView>>> {
    Ok(Json(db::list_a2a_agents(&state.db).await?.into_iter().map(view).collect()))
}

/// Create A2A agent
///
/// An agent, found by its card, proxied at `/a2a/{name}`.
#[utoipa::path(post, path = "/a2a-agents", tag = "A2A", request_body = A2aAgentInput,
    responses(
        (status = 201, description = "The new A2A agent.", body = A2aAgentView),
        (status = 400, description = "Invalid A2A agent.", body = crate::error::AdminErrorBody),
        (status = 409, description = "The name is taken.", body = crate::error::AdminErrorBody),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(crate) async fn create_a2a_agent(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(i): Json<A2aAgentInput>,
) -> R<(StatusCode, Json<A2aAgentView>)> {
    let row = row(&state, i, None).await?;
    db::insert_a2a_agent(&state.db, &row).await.map_err(AdminError::conflict_on_duplicate("an A2A agent"))?;
    broadcast_reload(&state, "A2A agent created").await;
    tracing::info!(a2a_agent = %row.id, name = %row.name, "A2A agent created");
    let v = view(row);
    audit::Event::new("a2a_agent", "create").on(v.id, &v.name).details(audit::snapshot(&v)).record(&state, &actor).await;
    Ok((StatusCode::CREATED, Json(v)))
}

/// Update A2A agent
///
/// Without a secret, the stored secret stays.
#[utoipa::path(put, path = "/a2a-agents/{id}", tag = "A2A", params(("id" = Uuid, Path)),
    request_body = A2aAgentInput,
    responses(
        (status = 200, description = "The A2A agent.", body = A2aAgentView),
        (status = 400, description = "Invalid A2A agent.", body = crate::error::AdminErrorBody),
        (status = 404, description = "No such A2A agent.", body = crate::error::AdminErrorBody),
        (status = 409, description = "The name is taken.", body = crate::error::AdminErrorBody),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(crate) async fn update_a2a_agent(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<Uuid>,
    Json(i): Json<A2aAgentInput>,
) -> R<Json<A2aAgentView>> {
    let existing = db::get_a2a_agent(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    let secret_changed = i.secret.as_deref().is_some_and(|s| !s.trim().is_empty());
    let before = view(existing.clone());
    let row = row(&state, i, Some(&existing)).await?;
    db::update_a2a_agent(&state.db, &row).await.map_err(AdminError::conflict_on_duplicate("an A2A agent"))?;
    let dropped = super::oauth::drop_stale_sign_ins(
        &state,
        crate::oauth::Kind::A2a,
        id,
        (&existing.auth, existing.oauth.as_deref()),
        (&row.auth, row.oauth.as_deref()),
    )
    .await?;
    broadcast_reload(&state, "A2A agent updated").await;
    tracing::info!(a2a_agent = %id, "A2A agent updated");
    let v = view(row);
    let mut changes = audit::changes(&before, &v);
    if secret_changed {
        changes.insert("secret".into(), "replaced".into());
    }
    if dropped > 0 {
        changes.insert("sign_ins".into(), format!("{dropped} removed").into());
    }
    if !changes.is_empty() {
        audit::Event::new("a2a_agent", "update").on(id, &v.name).details(changes.into()).record(&state, &actor).await;
    }
    Ok(Json(v))
}

/// Delete A2A agent
#[utoipa::path(delete, path = "/a2a-agents/{id}", tag = "A2A", params(("id" = Uuid, Path)),
    responses(
        (status = 204, description = "Deleted."),
        (status = 404, description = "No such A2A agent.", body = crate::error::AdminErrorBody),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(crate) async fn delete_a2a_agent(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<Uuid>,
) -> R<StatusCode> {
    let name = db::get_a2a_agent(&state.db, id).await?.ok_or_else(AdminError::not_found)?.name;
    if !db::delete_a2a_agent(&state.db, id).await? {
        return Err(AdminError::not_found());
    }
    broadcast_reload(&state, "A2A agent deleted").await;
    tracing::info!(a2a_agent = %id, "A2A agent deleted");
    audit::Event::new("a2a_agent", "delete").on(id, name).record(&state, &actor).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct A2aAgentTestResult {
    ok: bool,
    latency_ms: u64,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    card: Option<CardSummary>,
    /// Why the card could not be used, when `ok` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    error: Option<String>,
}

/// Test A2A agent
///
/// Fetches the agent's card, also while the agent is disabled, and says what the gateway proxies of it.
/// No message is sent.
#[utoipa::path(post, path = "/a2a-agents/{id}/test", tag = "A2A", params(("id" = Uuid, Path)),
    responses(
        (status = 200, description = "The card, or why it could not be used with `ok: false`.", body = A2aAgentTestResult),
        (status = 404, description = "No such A2A agent.", body = crate::error::AdminErrorBody),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
// Nothing is logged or charged.
pub(crate) async fn test_a2a_agent(State(state): State<AppState>, Path(id): Path<Uuid>) -> R<Json<A2aAgentTestResult>> {
    let row = db::get_a2a_agent(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    let agent = A2aAgent::build(row, &state.cfg.secret_box).map_err(AdminError::internal)?;
    let started = std::time::Instant::now();
    let result = crate::a2a::fetch_card(&state, &agent, None).await;
    let latency_ms = started.elapsed().as_millis() as u64;
    Ok(Json(match result {
        Ok(card) => A2aAgentTestResult { ok: true, latency_ms, card: Some(card.summary()), error: None },
        Err(e) => A2aAgentTestResult { ok: false, latency_ms, card: None, error: Some(e) },
    }))
}
