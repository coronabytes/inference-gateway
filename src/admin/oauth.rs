//! Admin API for upstream OAuth: finding an MCP server's or A2A agent's authorization server, and
//! signing keys in to upstreams whose keys sign in on their own (`oauth_user`).

use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, Uri},
    response::{Html, IntoResponse, Redirect, Response},
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
    labels::Labels,
    oauth::{self, Discovered, Kind, Slot, UpstreamAuth, Who},
    state::AppState,
};

/// Where authorization servers send the person signing a key in back to, after the gateway's address.
pub const CALLBACK_PATH: &str = "/admin/auth/oauth/callback";
/// The gateway's client ID metadata document, after its address: its URL is the gateway's client
/// ID at authorization servers that take such documents.
pub const CLIENT_METADATA_PATH: &str = "/admin/auth/oauth/client.json";
/// The sign-in in progress, in the console session.
const PENDING_KEY: &str = "oauth.pending";
/// One started from a sign-in link, kept apart so neither replaces the other in the same browser.
const LINK_PENDING_KEY: &str = "oauth.pending_link";
/// A sign-in must come back within this time.
const PENDING_TTL_SECS: i64 = 900;

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DiscoverInput {
    /// `mcp` or `a2a`.
    kind: String,
    /// The MCP server's URL, or the A2A agent's card URL.
    url: String,
    /// Use the gateway's client ID metadata document as its client ID, where the authorization server
    /// takes one and the gateway's address is public HTTPS. Nothing is registered.
    #[serde(default)]
    metadata_document: bool,
    /// Otherwise register the gateway as a public client (RFC 7591), where the authorization server
    /// offers it.
    #[serde(default)]
    register: bool,
}

/// Finds the authorization server of an MCP server or A2A agent, and gets the gateway a client
/// there when asked. Its redirect URI is the gateway's address as the console reaches it.
pub(crate) async fn discover(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Json(i): Json<DiscoverInput>,
) -> R<Json<Discovered>> {
    let kind = Kind::parse(&i.kind).ok_or_else(|| AdminError::bad_request("kind is mcp or a2a"))?;
    let url = super::mcp::check_url(&state, &i.url)?;
    let origin = crate::a2a::origin(&headers, &uri);
    let redirect = format!("{origin}{CALLBACK_PATH}");
    let document = (i.metadata_document && fetchable(&origin)).then(|| format!("{origin}{CLIENT_METADATA_PATH}"));
    oauth::discover(&state, kind, &url, document.as_deref(), i.register, &redirect).await.map(Json).map_err(AdminError::bad_request)
}

/// Whether an authorization server elsewhere can fetch documents from `origin`: client ID metadata
/// documents must be served over HTTPS, and a loopback address is only the server's own.
fn fetchable(origin: &str) -> bool {
    let Ok(u) = reqwest::Url::parse(origin) else { return false };
    let host = u.host_str().unwrap_or("localhost").trim_matches(['[', ']']);
    let loopback = host == "localhost" || host.ends_with(".localhost") || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback());
    u.scheme() == "https" && !loopback
}

/// `GET /admin/auth/oauth/client.json`: the gateway's client ID metadata document. An upstream's
/// authorization server fetches it when a key signs in with the gateway's client ID, its URL.
pub(crate) async fn client_metadata(headers: HeaderMap, uri: Uri) -> Json<serde_json::Value> {
    let origin = crate::a2a::origin(&headers, &uri);
    Json(serde_json::json!({
        "client_id": format!("{origin}{CLIENT_METADATA_PATH}"),
        "client_name": "Inference Gateway",
        "client_uri": origin,
        "redirect_uris": [format!("{origin}{CALLBACK_PATH}")],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    }))
}

/// An upstream a key signs in to on its own, and whether it has.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct Connection {
    /// `mcp` or `a2a`.
    kind: &'static str,
    upstream_id: Uuid,
    name: String,
    /// The key's own sign-in, where the key signs in once.
    connected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    signed_in_at: Option<DateTime<Utc>>,
    /// When the access token expires; a refreshable one is renewed then.
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<DateTime<Utc>>,
    refreshable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<String>,
    /// When the key's sign-in was last used, at most an hour off.
    #[serde(skip_serializing_if = "Option::is_none")]
    last_used_at: Option<DateTime<Utc>>,
    /// Who signed the key in.
    #[serde(skip_serializing_if = "Option::is_none")]
    connected_by: Option<String>,
    /// The request labels that tell the key's sign-ins apart (`tenant`, `user`); empty where the
    /// key signs in once.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    sign_in_by: Vec<String>,
    /// How many sign-ins the key has by those labels.
    #[serde(skip_serializing_if = "Option::is_none")]
    sign_ins: Option<i64>,
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

/// The sign-in that `labels` (`tenant=…,user=…`) names for the upstream: the key's own without them.
fn slot_for(client: &oauth::Client, key: Uuid, labels: Option<&str>) -> R<Slot> {
    let labels = Labels::parse(labels.unwrap_or_default()).map_err(AdminError::bad_request)?;
    client.slot(Who { key, labels: &labels }).map_err(|e| AdminError::bad_request(e.message()))
}

/// The MCP servers and A2A agents this key signs in to on its own, and its sign-ins.
pub(crate) async fn list_connections(
    State(state): State<AppState>,
    Extension(access): Extension<Access>,
    Path(id): Path<Uuid>,
) -> R<Json<Vec<Connection>>> {
    let key = visible_key(&state, &access, id).await?;
    let (own, counts) = db::key_connections(&state.db, id).await?;
    let snap = state.registry.snapshot();
    let mcp = snap
        .mcp_servers
        .values()
        .filter(|s| s.auth.per_key() && s.allows(&key))
        .map(|s| (Kind::Mcp, s.id, s.name.clone(), s.auth.sign_in_by().to_vec()));
    let a2a = snap
        .a2a_agents
        .values()
        .filter(|a| a.auth.per_key() && a.allows(&key))
        .map(|a| (Kind::A2a, a.id, a.name.clone(), a.auth.sign_in_by().to_vec()));
    let mut out: Vec<Connection> = mcp
        .chain(a2a)
        .map(|(kind, upstream_id, name, sign_in_by)| {
            let row = own.iter().find(|r| r.upstream_kind == kind.as_str() && r.upstream_id == upstream_id);
            let sign_ins =
                (!sign_in_by.is_empty()).then(|| counts.iter().find(|c| c.0 == kind.as_str() && c.1 == upstream_id).map_or(0, |c| c.2));
            Connection {
                kind: kind.as_str(),
                upstream_id,
                name,
                connected: row.is_some(),
                signed_in_at: row.map(|r| r.created_at),
                expires_at: row.and_then(|r| r.expires_at),
                refreshable: row.is_some_and(|r| r.refreshable),
                scope: row.and_then(|r| r.scope.clone()),
                connected_by: row.and_then(|r| r.connected_by.clone()),
                last_used_at: row.map(|r| r.last_used_at),
                sign_in_by,
                sign_ins,
            }
        })
        .collect();
    out.sort_by(|a, b| (a.kind, &a.name).cmp(&(b.kind, &b.name)));
    Ok(Json(out))
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct SignInsQuery {
    /// Only sign-ins with these labels, as `tenant=…,user=…`.
    labels: Option<String>,
    /// At most this many, 50 by default (at most 500).
    limit: Option<i64>,
    offset: Option<i64>,
}

/// A key's sign-in to an upstream by labels.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct SignIn {
    /// `tenant=…,user=…`.
    labels: String,
    signed_in_at: DateTime<Utc>,
    /// When it was last used, at most an hour off.
    last_used_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<DateTime<Utc>>,
    refreshable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    connected_by: Option<String>,
}

impl From<db::KeySignIn> for SignIn {
    fn from(r: db::KeySignIn) -> Self {
        SignIn {
            labels: r.labels,
            signed_in_at: r.created_at,
            last_used_at: r.last_used_at,
            expires_at: r.expires_at,
            refreshable: r.refreshable,
            scope: r.scope,
            connected_by: r.connected_by,
        }
    }
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct SignIns {
    sign_ins: Vec<SignIn>,
    /// How many match, in all.
    total: i64,
}

/// A key's sign-ins to an upstream that tells them apart by labels, newest first.
pub(crate) async fn list_sign_ins(
    State(state): State<AppState>,
    Extension(access): Extension<Access>,
    Path((id, kind, upstream)): Path<(Uuid, String, Uuid)>,
    Query(q): Query<SignInsQuery>,
) -> R<Json<SignIns>> {
    visible_key(&state, &access, id).await?;
    let kind = Kind::parse(&kind).ok_or_else(AdminError::not_found)?;
    let having: Vec<String> = Labels::parse(q.labels.as_deref().unwrap_or_default())
        .map_err(AdminError::bad_request)?
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    let (rows, total) =
        db::sign_ins(&state.db, kind.as_str(), upstream, id, &having, q.limit.unwrap_or(50).clamp(1, 500), q.offset.unwrap_or(0).max(0))
            .await?;
    Ok(Json(SignIns { sign_ins: rows.into_iter().map(SignIn::from).collect(), total }))
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
    /// The sign-in's labels, where the upstream tells a key's sign-ins apart by them.
    #[serde(default)]
    labels: String,
    started: i64,
    /// Who started it, for the audit log.
    actor_kind: String,
    actor_id: Option<Uuid>,
    actor_name: Option<String>,
    /// The console page to come back to.
    #[serde(default)]
    back: Back,
    /// Started from a sign-in link rather than the console: it ends on a page of its own.
    #[serde(default)]
    link: bool,
}

/// The console page a sign-in comes back to: the Access page, or the playground with the key chosen.
#[derive(Serialize, Deserialize, Clone, Copy, Default, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Back {
    #[default]
    Access,
    Playground,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct ConnectQuery {
    /// The console page to come back to: `access` (the default) or `playground`.
    #[param(value_type = Option<String>)]
    back: Option<Back>,
    /// For an upstream that tells a key's sign-ins apart by labels: the sign-in's, as
    /// `tenant=…,user=…`.
    labels: Option<String>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct ConnectStart {
    /// Where to send the browser: the upstream's sign-in, which comes back to the gateway.
    authorization_url: String,
}

/// Starts signing the key in to an upstream: the console sends the browser to the URL this returns.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn connect(
    State(state): State<AppState>,
    Extension(access): Extension<Access>,
    Extension(actor): Extension<Actor>,
    session: Session,
    headers: HeaderMap,
    uri: Uri,
    Path((id, kind, upstream)): Path<(Uuid, String, Uuid)>,
    Query(q): Query<ConnectQuery>,
) -> R<Json<ConnectStart>> {
    let key = visible_key(&state, &access, id).await?;
    let kind = Kind::parse(&kind).ok_or_else(AdminError::not_found)?;
    let client = client(&state, &key, kind, upstream).ok_or_else(AdminError::not_found)?;
    let slot = slot_for(&client, id, q.labels.as_deref())?;
    let (authorization_url, pending) = start(&client, &slot, &headers, &uri, &actor, q.back.unwrap_or_default(), false)?;
    session.insert(PENDING_KEY, pending).await.map_err(|e| AdminError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(ConnectStart { authorization_url }))
}

/// The upstream's sign-in URL for `slot`, and the sign-in to keep until it comes back.
fn start(
    client: &oauth::Client,
    slot: &Slot,
    headers: &HeaderMap,
    uri: &Uri,
    actor: &Actor,
    back: Back,
    link: bool,
) -> R<(String, Pending)> {
    let redirect_uri = client.config.redirect_uri.clone().unwrap_or_else(|| format!("{}{CALLBACK_PATH}", crate::a2a::origin(headers, uri)));
    let (st, verifier) = (oauth::random_token(), oauth::random_token());
    let authorization_url = client.authorization_url(&redirect_uri, &st, &verifier).map_err(AdminError::bad_request)?;
    let pending = Pending {
        state: st,
        verifier,
        redirect_uri,
        kind: client.kind,
        upstream: client.upstream,
        key: slot.key,
        labels: slot.labels.clone(),
        started: Utc::now().timestamp(),
        actor_kind: actor.kind.to_string(),
        actor_id: actor.id,
        actor_name: actor.name.clone(),
        back,
        link,
    };
    Ok((authorization_url, pending))
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct DisconnectQuery {
    /// The sign-in's labels, as listed; without them, the key's own sign-in.
    labels: Option<String>,
}

/// Signs the key, or one of its sign-ins by labels, out of an upstream: its tokens are deleted.
pub(crate) async fn disconnect(
    State(state): State<AppState>,
    Extension(access): Extension<Access>,
    Extension(actor): Extension<Actor>,
    Path((id, kind, upstream)): Path<(Uuid, String, Uuid)>,
    Query(q): Query<DisconnectQuery>,
) -> R<StatusCode> {
    let key = visible_key(&state, &access, id).await?;
    let kind = Kind::parse(&kind).ok_or_else(AdminError::not_found)?;
    let slot = Slot { key: id, labels: q.labels.unwrap_or_default() };
    if !db::delete_oauth_token(&state.db, db::SignInRef { kind: kind.as_str(), upstream, key: id, labels: &slot.labels }).await? {
        return Err(AdminError::not_found());
    }
    oauth::forget(&state, kind, upstream, &slot);
    let name = client(&state, &key, kind, upstream).map_or_else(|| upstream.to_string(), |c| c.name.clone());
    let mut details = serde_json::json!({ "upstream": name, "kind": kind.as_str() });
    if !slot.labels.is_empty() {
        details["labels"] = slot.labels.into();
    }
    audit::Event::new("key", "disconnect").on(id, &key.name).details(details).record(&state, &actor).await;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /admin/auth/oauth/sign-in/{ticket}`: a sign-in link an application handed one of its
/// tenants or users (see `oauth::Ticket`). Sends the browser to the upstream's sign-in, which comes
/// back to `callback` in this browser.
pub(crate) async fn sign_in_link(
    State(state): State<AppState>,
    session: Session,
    headers: HeaderMap,
    uri: Uri,
    Path(ticket): Path<String>,
) -> Response {
    let Some(t) = oauth::Ticket::open(&ticket, &state.cfg.secret_box) else {
        return page(StatusCode::GONE, "This sign-in link has expired", "Ask for a new link where you got this one.");
    };
    let gone = || page(StatusCode::NOT_FOUND, "This sign-in link no longer works", "Ask for a new link where you got this one.");
    let Ok(Some(key)) = db::get_key(&state.db, t.slot.key).await else { return gone() };
    let Some(client) = client(&state, &key, t.kind, t.upstream) else { return gone() };
    // The upstream may tell sign-ins apart differently since the link was made.
    let same = Labels::parse(&t.slot.labels)
        .ok()
        .and_then(|l| oauth::labels_for(&client.config.sign_in_by, &l).ok())
        .is_some_and(|l| l == t.slot.labels);
    if t.slot.labels.is_empty() || !same {
        return gone();
    }
    let actor = Actor::virtual_key(key.id, &key.name);
    let (url, pending) = match start(&client, &t.slot, &headers, &uri, &actor, Back::Access, true) {
        Ok(s) => s,
        Err(e) => return page(StatusCode::BAD_GATEWAY, "The sign-in could not start", &e.1),
    };
    if session.insert(LINK_PENDING_KEY, pending).await.is_err() {
        return page(StatusCode::INTERNAL_SERVER_ERROR, "The sign-in could not start", "Try the link again.");
    }
    Redirect::to(&url).into_response()
}

/// A page for someone who followed a sign-in link: they never see the console.
fn page(status: StatusCode, title: &str, message: &str) -> Response {
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let (title, message) = (esc(title), esc(message));
    (
        status,
        Html(format!(
            "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width\"><title>{title}</title><body style=\"font-family:system-ui;max-width:40em;margin:4em auto;padding:0 16px\"><h1>{title}</h1><p>{message}</p>"
        )),
    )
        .into_response()
}

#[derive(Deserialize)]
pub(crate) struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// Back to the console page the sign-in started on, with how it went.
fn back_to(page: Back, key: Option<Uuid>, param: &str, message: &str) -> Response {
    let q = percent_encoding::utf8_percent_encode(message, percent_encoding::NON_ALPHANUMERIC);
    Redirect::to(&match (page, key) {
        (Back::Playground, Some(key)) => format!("/#/models/playground?key={key}&{param}={q}"),
        _ => format!("/#/access?{param}={q}"),
    })
    .into_response()
}

/// The sign-in in progress that `returned` (the `state` the upstream sent back) belongs to, taken
/// out of the browser's session: one from the console or one from a link.
async fn take_pending(session: &Session, returned: Option<&str>) -> Option<Pending> {
    let returned = returned?;
    for slot in [PENDING_KEY, LINK_PENDING_KEY] {
        let Ok(Some(p)) = session.get::<Pending>(slot).await else { continue };
        if bool::from(subtle::ConstantTimeEq::ct_eq(returned.as_bytes(), p.state.as_bytes())) {
            let _ = session.remove::<Pending>(slot).await;
            return Some(p);
        }
    }
    None
}

/// `GET /admin/auth/oauth/callback`: where the upstream's sign-in comes back to. It finishes only in
/// the browser session that started it, so a link cannot sign someone's key in to another account.
pub(crate) async fn callback(State(state): State<AppState>, session: Session, Query(q): Query<CallbackQuery>) -> Response {
    let pending = take_pending(&session, q.state.as_deref()).await;
    let (back, key, link) = pending.as_ref().map_or((Back::Access, None, false), |p| (p.back, Some(p.key), p.link));
    let done = |ok: bool, message: &str| match (link, ok) {
        (true, true) => {
            page(StatusCode::OK, &format!("You are signed in to {message}"), "You can close this tab and go back to where you came from.")
        }
        (true, false) => page(StatusCode::BAD_REQUEST, "The sign-in did not work", message),
        (false, ok) => back_to(back, key, if ok { "connected" } else { "connect_error" }, message),
    };
    if let Some(err) = q.error {
        return done(false, &format!("The sign-in was refused: {}", q.error_description.unwrap_or(err)));
    }
    let (Some(code), Some(p)) = (q.code, pending) else {
        return done(false, "The sign-in expired or was started in another browser; try again.");
    };
    if Utc::now().timestamp() - p.started > PENDING_TTL_SECS {
        return done(false, "The sign-in expired or was started in another browser; try again.");
    }
    let Ok(Some(key)) = db::get_key(&state.db, p.key).await else { return done(false, "The key no longer exists.") };
    let Some(client) = client(&state, &key, p.kind, p.upstream) else {
        return done(false, "The key may no longer use this server, or it no longer signs keys in.");
    };
    let slot = Slot { key: p.key, labels: p.labels.clone() };
    let actor = Actor::restore(&p.actor_kind, p.actor_id, p.actor_name);
    // The sign-in's labels are its own, so who signed in is the person in the console, or a link.
    let by = match p.link {
        true => "sign-in link".to_string(),
        false => actor.name.clone().unwrap_or_else(|| actor.kind.to_string()),
    };
    match client.complete(&state, &slot, &code, &p.verifier, &p.redirect_uri, &by).await {
        Ok(()) => {
            let mut details = serde_json::json!({ "upstream": client.name, "kind": p.kind.as_str() });
            if !slot.labels.is_empty() {
                details["labels"] = slot.labels.clone().into();
                details["link"] = p.link.into();
            }
            audit::Event::new("key", "connect").on(p.key, &key.name).details(details).record(&state, &actor).await;
            done(true, &client.name)
        }
        Err(e) => {
            tracing::warn!(upstream = %client.name, error = %e, "upstream sign-in failed");
            done(false, &format!("The sign-in to {} failed: {e}", client.name))
        }
    }
}

/// After an upstream's update: where keys sign in on their own before and after, but their sign-ins
/// are told apart by other labels now, the sign-ins kept so far no longer fit and are removed.
/// Returns how many were.
pub(crate) async fn drop_stale_sign_ins(
    state: &AppState,
    kind: Kind,
    upstream: Uuid,
    before: (&str, Option<&oauth::OAuthConfig>),
    after: (&str, Option<&oauth::OAuthConfig>),
) -> R<u64> {
    let by = |(auth, oauth): (&str, Option<&oauth::OAuthConfig>)| {
        (auth == "oauth_user").then(|| oauth.map(|o| o.sign_in_by.clone()).unwrap_or_default())
    };
    let (Some(was), Some(is)) = (by(before), by(after)) else { return Ok(0) };
    if was == is {
        return Ok(0);
    }
    let n = db::delete_upstream_oauth_tokens(&state.db, kind.as_str(), upstream).await?;
    oauth::forget_upstream(state, kind, upstream);
    Ok(n)
}
