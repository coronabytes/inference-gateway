//! Admin API for upstream OAuth: finding an MCP server's or A2A agent's authorization server, and
//! signing keys in to upstreams whose keys sign in on their own (`oauth_user`).

use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Redirect, Response},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tower_sessions::Session;
use uuid::Uuid;

use super::{R, check_sees_key};
use crate::{
    admin_auth::Access,
    audit::{self, Actor},
    db,
    error::AdminError,
    oauth::{self, Discovered, Kind, UpstreamAuth},
    state::AppState,
};

/// Where authorization servers send the person signing a key in back to, after the gateway's address.
pub const CALLBACK_PATH: &str = "/admin/auth/oauth/callback";
/// The sign-in in progress, in the console session.
const PENDING_KEY: &str = "oauth.pending";
/// A sign-in must come back within this time.
const PENDING_TTL_SECS: i64 = 900;

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DiscoverInput {
    /// `mcp` or `a2a`.
    kind: String,
    /// The MCP server's URL, or the A2A agent's card URL.
    url: String,
    /// Register the gateway as a public client (RFC 7591), where the authorization server offers it.
    #[serde(default)]
    register: bool,
}

/// Finds the authorization server of an MCP server or A2A agent, and registers the gateway there
/// when asked. The redirect URI it registers is the gateway's address as the console reaches it.
pub(crate) async fn discover(State(state): State<AppState>, headers: HeaderMap, uri: Uri, Json(i): Json<DiscoverInput>) -> R<Json<Discovered>> {
    let kind = Kind::parse(&i.kind).ok_or_else(|| AdminError::bad_request("kind is mcp or a2a"))?;
    let url = super::mcp::check_url(&state, &i.url)?;
    let redirect = format!("{}{CALLBACK_PATH}", crate::a2a::origin(&headers, &uri));
    oauth::discover(&state, kind, &url, i.register, &redirect).await.map(Json).map_err(AdminError::bad_request)
}

/// An upstream a key signs in to on its own, and whether it has.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct Connection {
    /// `mcp` or `a2a`.
    kind: &'static str,
    upstream_id: Uuid,
    name: String,
    connected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    signed_in_at: Option<DateTime<Utc>>,
    /// When the access token expires; a refreshable one is renewed then.
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<DateTime<Utc>>,
    refreshable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<String>,
    /// Who signed the key in.
    #[serde(skip_serializing_if = "Option::is_none")]
    connected_by: Option<String>,
}

/// The upstream `kind`/`id` with its OAuth client, if the key may use it and signs in on its own.
fn client(state: &AppState, key: &crate::model::VirtualKey, kind: Kind, id: Uuid) -> Option<std::sync::Arc<oauth::Client>> {
    let snap = state.registry.snapshot();
    let auth = match kind {
        Kind::Mcp => snap.mcp_servers.values().find(|s| s.id == id && s.allows(key)).map(|s| &s.auth),
        Kind::A2a => snap.a2a_agents.values().find(|a| a.id == id && a.allows(key)).map(|a| &a.auth),
    };
    match auth? {
        UpstreamAuth::PerKey(c) => Some(c.clone()),
        _ => None,
    }
}

async fn visible_key(state: &AppState, access: &Access, id: Uuid) -> R<crate::model::VirtualKey> {
    let key = db::get_key(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    check_sees_key(access, &key)?;
    Ok(key)
}

/// The MCP servers and A2A agents this key signs in to on its own, and its sign-ins.
pub(crate) async fn list_connections(State(state): State<AppState>, Extension(access): Extension<Access>, Path(id): Path<Uuid>) -> R<Json<Vec<Connection>>> {
    let key = visible_key(&state, &access, id).await?;
    let signed_in = db::key_connections(&state.db, id).await?;
    let snap = state.registry.snapshot();
    let mcp = snap.mcp_servers.values().filter(|s| s.auth.per_key() && s.allows(&key)).map(|s| (Kind::Mcp, s.id, s.name.clone()));
    let a2a = snap.a2a_agents.values().filter(|a| a.auth.per_key() && a.allows(&key)).map(|a| (Kind::A2a, a.id, a.name.clone()));
    let mut out: Vec<Connection> = mcp
        .chain(a2a)
        .map(|(kind, upstream_id, name)| {
            let row = signed_in.iter().find(|r| r.0 == kind.as_str() && r.1 == upstream_id);
            Connection {
                kind: kind.as_str(),
                upstream_id,
                name,
                connected: row.is_some(),
                signed_in_at: row.map(|r| r.2),
                expires_at: row.and_then(|r| r.3),
                refreshable: row.is_some_and(|r| r.4),
                scope: row.and_then(|r| r.5.clone()),
                connected_by: row.and_then(|r| r.6.clone()),
            }
        })
        .collect();
    out.sort_by(|a, b| (a.kind, &a.name).cmp(&(b.kind, &b.name)));
    Ok(Json(out))
}

/// A sign-in in progress.
#[derive(Serialize, Deserialize)]
struct Pending {
    state: String,
    verifier: String,
    redirect_uri: String,
    kind: Kind,
    upstream: Uuid,
    key: Uuid,
    started: i64,
    /// Who started it, for the audit log.
    actor_kind: String,
    actor_id: Option<Uuid>,
    actor_name: Option<String>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct ConnectStart {
    /// Where to send the browser: the upstream's sign-in, which comes back to the gateway.
    authorization_url: String,
}

/// Starts signing the key in to an upstream: the console sends the browser to the URL this returns.
pub(crate) async fn connect(
    State(state): State<AppState>,
    Extension(access): Extension<Access>,
    Extension(actor): Extension<Actor>,
    session: Session,
    headers: HeaderMap,
    uri: Uri,
    Path((id, kind, upstream)): Path<(Uuid, String, Uuid)>,
) -> R<Json<ConnectStart>> {
    let key = visible_key(&state, &access, id).await?;
    let kind = Kind::parse(&kind).ok_or_else(AdminError::not_found)?;
    let client = client(&state, &key, kind, upstream).ok_or_else(AdminError::not_found)?;
    let redirect_uri = client.config.redirect_uri.clone().unwrap_or_else(|| format!("{}{CALLBACK_PATH}", crate::a2a::origin(&headers, &uri)));
    let (st, verifier) = (oauth::random_token(), oauth::random_token());
    let authorization_url = client.authorization_url(&redirect_uri, &st, &verifier).map_err(AdminError::bad_request)?;
    let pending = Pending {
        state: st,
        verifier,
        redirect_uri,
        kind,
        upstream,
        key: id,
        started: Utc::now().timestamp(),
        actor_kind: actor.kind.to_string(),
        actor_id: actor.id,
        actor_name: actor.name.clone(),
    };
    session.insert(PENDING_KEY, pending).await.map_err(|e| AdminError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(ConnectStart { authorization_url }))
}

/// Signs the key out of an upstream: its tokens are deleted.
pub(crate) async fn disconnect(
    State(state): State<AppState>,
    Extension(access): Extension<Access>,
    Extension(actor): Extension<Actor>,
    Path((id, kind, upstream)): Path<(Uuid, String, Uuid)>,
) -> R<StatusCode> {
    let key = visible_key(&state, &access, id).await?;
    let kind = Kind::parse(&kind).ok_or_else(AdminError::not_found)?;
    if !db::delete_oauth_token(&state.db, kind.as_str(), upstream, id).await? {
        return Err(AdminError::not_found());
    }
    oauth::forget(&state, kind, upstream, id);
    let name = client(&state, &key, kind, upstream).map_or_else(|| upstream.to_string(), |c| c.name.clone());
    audit::Event::new("key", "disconnect").on(id, &key.name).details(serde_json::json!({ "upstream": name, "kind": kind.as_str() })).record(&state, &actor).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub(crate) struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// Back to the console's Access page, with how the sign-in went.
fn back(param: &str, message: &str) -> Response {
    let q = percent_encoding::utf8_percent_encode(message, percent_encoding::NON_ALPHANUMERIC);
    Redirect::to(&format!("/#/access?{param}={q}")).into_response()
}

/// `GET /admin/auth/oauth/callback`: where the upstream's sign-in comes back to. It finishes only in
/// the browser session that started it, so a link cannot sign someone's key in to another account.
pub(crate) async fn callback(State(state): State<AppState>, session: Session, Query(q): Query<CallbackQuery>) -> Response {
    let pending: Option<Pending> = session.remove(PENDING_KEY).await.ok().flatten();
    if let Some(err) = q.error {
        return back("connect_error", &format!("The sign-in was refused: {}", q.error_description.unwrap_or(err)));
    }
    let (Some(code), Some(returned), Some(p)) = (q.code, q.state, pending) else {
        return back("connect_error", "The sign-in expired or was started in another browser; try again.");
    };
    if !bool::from(subtle::ConstantTimeEq::ct_eq(returned.as_bytes(), p.state.as_bytes())) || Utc::now().timestamp() - p.started > PENDING_TTL_SECS {
        return back("connect_error", "The sign-in expired or was started in another browser; try again.");
    }
    let Ok(Some(key)) = db::get_key(&state.db, p.key).await else { return back("connect_error", "The key no longer exists.") };
    let Some(client) = client(&state, &key, p.kind, p.upstream) else {
        return back("connect_error", "The key may no longer use this server, or it no longer signs keys in.");
    };
    let actor = Actor::restore(&p.actor_kind, p.actor_id, p.actor_name);
    let by = actor.name.clone().unwrap_or_else(|| actor.kind.to_string());
    match client.complete(&state, p.key, &code, &p.verifier, &p.redirect_uri, &by).await {
        Ok(()) => {
            audit::Event::new("key", "connect")
                .on(p.key, &key.name)
                .details(serde_json::json!({ "upstream": client.name, "kind": p.kind.as_str() }))
                .record(&state, &actor)
                .await;
            back("connected", &client.name)
        }
        Err(e) => {
            tracing::warn!(upstream = %client.name, error = %e, "upstream sign-in failed");
            back("connect_error", &format!("The sign-in to {} failed: {e}", client.name))
        }
    }
}
