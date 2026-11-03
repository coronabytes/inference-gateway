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
    admin::auth::Access,
    audit::Actor,
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
    /// Speech: the voices its deployments list, the default first; `*` when they let others through.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    voices: Vec<crate::model::Voice>,
    /// Transcription: a realtime model, streamed to over `/playground/realtime`.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    realtime: bool,
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct PlaygroundGuardrail {
    name: String,
    /// Set on the key or its project: every request goes through it.
    enforced: bool,
}

/// Playground options
///
/// The models a key may call, with their modes, the guardrails a request may ask for (those
/// the key or its project enforce marked), and the MCP servers and A2A agents it may use.
#[utoipa::path(get, path = "/playground/options", tag = "Playground", params(PlaygroundQuery),
    responses(
        (status = 200, description = "The key's models, guardrails, MCP servers and A2A agents.", body = PlaygroundOptions),
        (status = 404, description = "No such key, or not one of the caller's.", body = crate::error::AdminErrorBody),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
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
            let mut voices: Vec<crate::model::Voice> = Vec::new();
            if mode == Mode::Speech {
                for v in crate::api::models::deployments(&snap, Some(key), name).iter().flat_map(|d| &d.config.voices) {
                    if !voices.iter().any(|w| w.id == v.id) {
                        voices.push(v.clone());
                    }
                }
            }
            let realtime =
                mode == Mode::Transcription && crate::api::models::deployments(&snap, Some(key), name).iter().any(|d| d.config.realtime);
            PlaygroundModel { name: name.clone(), mode, router, web_search, voices, realtime }
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

/// The playground's audio settings, kept for the person signed in: the models the chat dictates
/// and reads replies aloud with, and the voice. Empty leaves each to the key's models.
#[derive(Serialize, Deserialize, utoipa::ToSchema)]
pub(crate) struct AudioSettings {
    /// The transcription model for dictation.
    stt: String,
    /// The speech model that reads replies aloud.
    tts: String,
    /// The speech model's voice; empty for its default.
    voice: String,
}

/// Longest model name or voice id the settings keep.
const MAX_AUDIO_SETTING: usize = 200;

/// Playground: audio settings
///
/// The audio settings of the person signed in; all empty until they save some.
#[utoipa::path(get, path = "/playground/audio-settings", tag = "Playground",
    responses((status = 200, description = "The settings.", body = AudioSettings)),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn get_audio_settings(State(state): State<AppState>, Extension(actor): Extension<Actor>) -> R<Json<AudioSettings>> {
    let s = db::get_audio_settings(&state.db, &actor.subject()).await?.unwrap_or_default();
    Ok(Json(AudioSettings { stt: s.stt, tts: s.tts, voice: s.voice }))
}

/// Playground: save audio settings
///
/// Saves the audio settings of the person signed in.
#[utoipa::path(put, path = "/playground/audio-settings", tag = "Playground", request_body = AudioSettings,
    responses(
        (status = 200, description = "Saved.", body = AudioSettings),
        (status = 400, description = "A value is too long.", body = crate::error::AdminErrorBody),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn put_audio_settings(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(s): Json<AudioSettings>,
) -> R<Json<AudioSettings>> {
    let s = AudioSettings { stt: s.stt.trim().to_string(), tts: s.tts.trim().to_string(), voice: s.voice.trim().to_string() };
    if [&s.stt, &s.tts, &s.voice].iter().any(|v| v.chars().count() > MAX_AUDIO_SETTING) {
        return Err(AdminError::bad_request(format!("a model name or voice can be up to {MAX_AUDIO_SETTING} characters")));
    }
    let row = db::AudioSettingsRow { stt: s.stt, tts: s.tts, voice: s.voice };
    db::save_audio_settings(&state.db, &actor.subject(), &row).await?;
    Ok(Json(AudioSettings { stt: row.stt, tts: row.tts, voice: row.voice }))
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct RealtimeQuery {
    key_id: Uuid,
    /// The realtime transcription model, instead of naming it in `session.update`.
    model: Option<String>,
}

/// Playground: realtime transcription
///
/// A transcription session as a chosen virtual key, as at `/v1/realtime?intent=transcription`:
/// OpenAI Realtime events over a WebSocket, logged and billed to the key.
#[utoipa::path(get, path = "/playground/realtime", tag = "Playground", params(RealtimeQuery),
    responses(
        (status = 101, description = "The session: OpenAI Realtime events over a WebSocket."),
        (status = 403, description = "Opened from another site.", body = crate::error::AdminErrorBody),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn playground_realtime(
    State(state): State<AppState>,
    Extension(access): Extension<Access>,
    Query(q): Query<RealtimeQuery>,
    headers: axum::http::HeaderMap,
    ws: axum::extract::ws::WebSocketUpgrade,
) -> Response {
    // Browsers send the console's cookie with a WebSocket from any site, and no CSRF header: only
    // the console's own pages may open one.
    if !same_origin(&headers) {
        return AdminError::forbidden("the session must be opened from the console").into_response();
    }
    let key = match playground_key(&state, &access, q.key_id) {
        Ok(key) => key,
        Err(r) => return r,
    };
    let labels = match crate::labels::Labels::from_headers(&headers) {
        Ok(labels) if !labels.is_empty() => labels,
        Ok(_) => crate::labels::Labels::parse("app=playground").expect("valid labels"),
        Err(e) => return e.into_response_for(crate::error::Dialect::OpenAi),
    };
    crate::api::realtime::serve(state, key, labels, Some("transcription"), q.model, ws)
}

/// Whether a browser's request comes from the page's own origin: its `Origin`, if any, names the
/// host it was sent to.
fn same_origin(headers: &axum::http::HeaderMap) -> bool {
    let Some(origin) = headers.get(axum::http::header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let host = headers.get(axum::http::header::HOST).and_then(|v| v.to_str().ok());
    let origin_host = reqwest::Url::parse(origin).ok().and_then(|u| {
        u.host_str().map(|h| match u.port() {
            Some(p) => format!("{h}:{p}"),
            None => h.to_string(),
        })
    });
    origin_host.is_some() && origin_host.as_deref() == host
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

/// Playground: an A2A agent
///
/// A JSON-RPC call of an A2A agent as a chosen virtual key, as at `/a2a/{agent}`: logged,
/// counted against the key's limits and charged the agent's price.
#[utoipa::path(post, path = "/playground/a2a/{agent}", tag = "Playground",
    params(("agent" = String, Path), PlaygroundQuery),
    request_body = Object,
    responses((status = 200, description = "The agent's answer, or its event stream.", body = Object)),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
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

/// Playground: an A2A agent's card
///
/// The agent's card as a chosen virtual key gets it at `/a2a/{agent}/.well-known/agent-card.json`.
#[utoipa::path(get, path = "/playground/a2a/{agent}/card", tag = "Playground",
    params(("agent" = String, Path), PlaygroundQuery),
    responses((status = 200, description = "The agent card, with the gateway's addresses.", body = Object)),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
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

/// Playground
///
/// Runs an inference request as a chosen virtual key, with its limits, budget and logging.
/// `api` is `chat`, `responses`, `messages`, `embeddings`, `rerank`, `decisions`, `systemone`,
/// `transcriptions` (a `multipart/form-data` body) or `speech`; the body is that endpoint's. `models` and `models-extended` list the key's models as
/// `GET /v1/models` and `GET /v1/models-extended` do; their body is ignored.
#[utoipa::path(post, path = "/playground/{api}", tag = "Playground",
    params(("api" = String, Path), PlaygroundQuery),
    request_body = Object,
    responses((status = 200, description = "The endpoint's answer.", body = crate::openapi::ProviderAnswer)),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
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
        "embeddings" | "rerank" | "decisions" | "systemone" | "transcriptions" | "speech" => {
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
                "transcriptions" => crate::api::audio::serve_transcription(&state, key, labels, &headers, body)
                    .await
                    .unwrap_or_else(|e| e.into_response_for(crate::error::Dialect::OpenAi)),
                "speech" => crate::api::audio::serve_speech(&state, key, labels, body)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn realtime_sessions_open_from_the_console_only() {
        let headers = |origin: Option<&'static str>| {
            let mut h = axum::http::HeaderMap::new();
            h.insert(axum::http::header::HOST, "localhost:5173".parse().unwrap());
            if let Some(o) = origin {
                h.insert(axum::http::header::ORIGIN, o.parse().unwrap());
            }
            h
        };
        assert!(same_origin(&headers(Some("http://localhost:5173"))));
        assert!(same_origin(&headers(None)), "not a browser");
        assert!(!same_origin(&headers(Some("https://evil.example"))));
        assert!(!same_origin(&headers(Some("http://localhost:8080"))));
        assert!(!same_origin(&headers(Some("null"))));
    }
}
