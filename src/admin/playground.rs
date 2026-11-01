//! The console's playground: what a key may call, and requests sent as that key.

use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{R, sees_key};
use crate::{
    admin_auth::Access,
    db,
    error::AdminError,
    model::{Mode, VirtualKey},
    state::AppState,
};

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct PlaygroundQuery {
    key_id: Uuid,
}

/// What the playground offers for a key: the models it may call, the guardrails a request may
/// ask for (those the key or its project enforce marked), and the MCP servers and A2A agents it
/// may use.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct PlaygroundOptions {
    models: Vec<PlaygroundModel>,
    guardrails: Vec<PlaygroundGuardrail>,
    mcp_servers: Vec<PlaygroundUpstream>,
    a2a_agents: Vec<PlaygroundUpstream>,
}

/// An MCP server or A2A agent the key may use, or a tool router.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct PlaygroundUpstream {
    name: String,
    /// The key signs in to it itself, and has not yet.
    needs_sign_in: bool,
    /// A tool router, listed with the MCP servers: requests name it like one.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    router: bool,
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct PlaygroundModel {
    name: String,
    mode: Mode,
    /// An auto router (chat mode).
    router: bool,
    /// Requests may have it search the web: it has a web search, and the key and its project allow it.
    web_search: bool,
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct PlaygroundGuardrail {
    name: String,
    /// Set on the key or its project: every request goes through it.
    enforced: bool,
}

/// `GET /admin/api/playground/options?key_id=…`.
pub(super) async fn playground_options(
    State(state): State<AppState>,
    Extension(access): Extension<Access>,
    Query(q): Query<PlaygroundQuery>,
) -> R<Json<PlaygroundOptions>> {
    let snap = state.registry.snapshot();
    // As in `playground`: a user sees only the inference keys of their projects.
    let key = snap.keys.values().find(|k| k.id == q.key_id && sees_key(&access, k)).ok_or_else(AdminError::not_found)?;
    let models = crate::api::models::names(&snap, key)
        .into_iter()
        .map(|name| {
            let router = snap.routers.contains_key(snap.resolve(name));
            let mode = if router {
                Mode::Chat
            } else {
                crate::api::models::deployments(&snap, Some(key), name).first().map_or(Mode::Chat, |d| d.mode)
            };
            let web_search = mode == Mode::Chat && crate::api::models::web_search(&snap, key, name);
            PlaygroundModel { name: name.clone(), mode, router, web_search }
        })
        .collect();
    let project = key.project_id.and_then(|p| snap.projects.get(&p));
    let enforced = |id: Uuid| key.guardrail_ids.contains(&id) || project.is_some_and(|p| p.guardrail_ids.contains(&id));
    let guardrails =
        snap.guardrails.values().map(|g| PlaygroundGuardrail { name: g.row.name.clone(), enforced: enforced(g.row.id) }).collect();
    let (signed_in, _) = db::key_connections(&state.db, key.id).await?;
    // Only the key's own sign-in is asked for here; one per tenant or user is asked for by the call
    // that needs it, with a link.
    let upstream = |kind: crate::oauth::Kind, id: Uuid, name: &str, auth: &crate::oauth::UpstreamAuth| PlaygroundUpstream {
        name: name.to_string(),
        needs_sign_in: auth.per_key()
            && auth.sign_in_by().is_empty()
            && !signed_in.iter().any(|r| r.upstream_kind == kind.as_str() && r.upstream_id == id),
        router: false,
    };
    let mut mcp_servers: Vec<PlaygroundUpstream> =
        snap.mcp_servers.values().filter(|s| s.allows(key)).map(|s| upstream(crate::oauth::Kind::Mcp, s.id, &s.name, &s.auth)).collect();
    let mut a2a_agents: Vec<PlaygroundUpstream> =
        snap.a2a_agents.values().filter(|a| a.allows(key)).map(|a| upstream(crate::oauth::Kind::A2a, a.id, &a.name, &a.auth)).collect();
    // Tool routers with a route the key may use.
    mcp_servers.extend(snap.tool_routers.values().filter(|r| !r.usable(&snap, key).is_empty()).map(|r| PlaygroundUpstream {
        name: r.name.clone(),
        needs_sign_in: false,
        router: true,
    }));
    mcp_servers.sort_by(|a, b| a.name.cmp(&b.name));
    a2a_agents.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(PlaygroundOptions { models, guardrails, mcp_servers, a2a_agents }))
}

/// The key the playground plays as: for a user, only an inference key of their projects; others
/// look like keys that do not exist.
#[allow(clippy::result_large_err)]
fn playground_key(state: &AppState, access: &Access, id: Uuid) -> Result<std::sync::Arc<VirtualKey>, Response> {
    let not_found = |e: crate::error::GatewayError| e.into_response_for(crate::error::Dialect::OpenAi);
    if !state.registry.snapshot().keys.values().any(|k| k.id == id && sees_key(access, k)) {
        return Err(not_found(crate::error::GatewayError::not_found("virtual key not found")));
    }
    crate::auth::key_by_id(state, id).map_err(not_found)
}

/// `POST /admin/api/playground/a2a/{agent}?key_id=…`: a JSON-RPC call of the agent as the key,
/// as at `/a2a/{agent}`, logged and charged as there.
pub(super) async fn playground_a2a(
    State(state): State<AppState>,
    Extension(access): Extension<Access>,
    Path(agent): Path<String>,
    Query(q): Query<PlaygroundQuery>,
    headers: axum::http::HeaderMap,
    body: bytes::Bytes,
) -> Response {
    match playground_key(&state, &access, q.key_id) {
        Ok(key) => crate::a2a::jsonrpc_for(&state, &key, &agent, &headers, body).await,
        Err(r) => r,
    }
}

/// `GET /admin/api/playground/a2a/{agent}/card?key_id=…`: the agent's card as the key gets it.
pub(super) async fn playground_a2a_card(
    State(state): State<AppState>,
    Extension(access): Extension<Access>,
    Path(agent): Path<String>,
    Query(q): Query<PlaygroundQuery>,
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
) -> Response {
    match playground_key(&state, &access, q.key_id) {
        Ok(key) => crate::a2a::card_for(&state, &key, &agent, &uri, &headers).await,
        Err(r) => r,
    }
}

/// `POST /admin/api/playground/{chat|responses|messages|embeddings|rerank|decisions|systemone|models|models-extended}?key_id=…`
/// — the admin UI's playground. Runs the normal inference pipeline as the chosen virtual key,
/// so its model access, rate limits, budget and request logging all apply. `models` and
/// `models-extended` answer as `GET /v1/models` and `GET /v1/models-extended` do for the key,
/// and ignore the body.
pub(super) async fn playground(
    State(state): State<AppState>,
    Extension(access): Extension<Access>,
    Path(api): Path<String>,
    Query(q): Query<PlaygroundQuery>,
    headers: axum::http::HeaderMap,
    body: bytes::Bytes,
) -> Response {
    use crate::api::{Inbound, decisions::Route};
    // A user plays only as an inference key of their projects; others look like keys that do not exist.
    if !state.registry.snapshot().keys.values().any(|k| k.id == q.key_id && sees_key(&access, k)) {
        return crate::error::GatewayError::not_found("virtual key not found").into_response_for(crate::error::Dialect::OpenAi);
    }
    let labels = match crate::labels::Labels::from_headers(&headers) {
        Ok(labels) => labels,
        Err(e) => return e.into_response_for(crate::error::Dialect::OpenAi),
    };
    let inbound = match api.as_str() {
        "chat" => Inbound::Chat,
        "responses" => Inbound::Responses,
        "messages" => Inbound::Messages,
        "models" | "models-extended" => {
            return match crate::auth::key_by_id(&state, q.key_id) {
                Ok(key) if api == "models" => crate::api::models::serve(&state, &key, &headers),
                Ok(key) => crate::api::models::serve_extended(&state, &key),
                Err(e) => e.into_response_for(crate::error::Dialect::OpenAi),
            };
        }
        "embeddings" | "rerank" | "decisions" | "systemone" => {
            let key = match crate::auth::key_by_id(&state, q.key_id) {
                Ok(key) => key,
                Err(e) => return e.into_response_for(crate::error::Dialect::OpenAi),
            };
            return match api.as_str() {
                "embeddings" => crate::api::embeddings::serve(&state, key, labels, body)
                    .await
                    .unwrap_or_else(|e| e.into_response_for(crate::error::Dialect::OpenAi)),
                "rerank" => crate::api::rerank::serve(&state, key, labels, body)
                    .await
                    .unwrap_or_else(|e| e.into_response_for(crate::error::Dialect::OpenAi)),
                "decisions" => crate::api::decisions::serve(&state, key, labels, &headers, body, Route::Decisions).await,
                _ => crate::api::decisions::serve(&state, key, labels, &headers, body, Route::SystemOne).await,
            };
        }
        _ => return AdminError::not_found().into_response(),
    };
    match crate::auth::key_by_id(&state, q.key_id) {
        Ok(key) => crate::api::serve(&state, key, labels, &headers, body, inbound).await,
        Err(e) => e.into_response_for(inbound.dialect()),
    }
}
