//! The gateway as an OAuth authorization server for MCP clients, such as Claude or ChatGPT adding
//! the gateway as a remote MCP server (MCP authorization, 2026-07-28).
//!
//! Clients identify themselves only with a Client ID Metadata Document (CIMD, draft -02): their
//! `client_id` is the HTTPS URL of a JSON document that lists their name and redirect URIs. There
//! is no dynamic client registration. Clients are public: they prove themselves with PKCE (S256).
//!
//! A person signed in to the console approves a client and picks the virtual key it acts as. The
//! client then gets tokens for the MCP endpoint it asked for (`resource`), and its requests there
//! count as the key's: its access, limits, budgets and logging apply. Access tokens last an hour,
//! refresh tokens 30 days and rotate on use; a grant ends 90 days after its approval however often
//! it is refreshed, and at once when a replaced refresh token is presented again. Tokens are stored
//! as SHA-256 hashes only (see `client_grants`); grants are listed and revoked per key in the console.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use axum::{
    Json,
    body::Bytes,
    extract::{FromRequestParts, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, Uri, header, request::Parts},
    response::{Html, IntoResponse, Redirect, Response},
};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    auth, db,
    error::{Dialect, GatewayError},
    model::VirtualKey,
    state::AppState,
    util::TtlCache,
};

/// The one scope: the MCP endpoints, as the key may use them.
pub const SCOPE: &str = "mcp";
const ACCESS_PREFIX: &str = "gwat_";
const REFRESH_PREFIX: &str = "gwrt_";
const CODE_PREFIX: &str = "gwac_";
const ACCESS_TTL: Duration = Duration::from_secs(3600);
const REFRESH_TTL: Duration = Duration::from_secs(30 * 24 * 3600);
/// How long a person has to approve a client in the console.
pub const REQUEST_TTL: Duration = Duration::from_secs(600);
const CODE_TTL: Duration = Duration::from_secs(300);
/// CIMD: "The recommended maximum size to read is 5 kilobytes."
const MAX_DOCUMENT: usize = 5 * 1024;
/// How long a replica trusts what it read of a token; revoking takes effect on the others within it.
const TOKEN_CACHE: Duration = Duration::from_secs(60);
/// Metadata documents a replica keeps, at most: the soonest to expire makes room.
const MAX_DOCUMENTS: usize = 256;
/// Authorization requests a replica takes a minute. People connect an app now and then; anything
/// faster is a scan, since the request fetches the document any caller names.
const AUTHORIZE_PER_MINUTE: u32 = 60;

/// A grant as stored: one client, one key, one MCP endpoint.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct ClientGrant {
    pub id: Uuid,
    pub key_id: Uuid,
    /// The client's metadata document URL.
    pub client_id: String,
    pub client_name: Option<String>,
    pub redirect_uri: String,
    /// The MCP endpoint's path: `/mcp`, `/mcp/{server}`, or `/` for every one.
    pub resource: String,
    pub scope: String,
    pub access_expires_at: DateTime<Utc>,
    pub refresh_expires_at: DateTime<Utc>,
    pub created_by: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

/// A client's metadata document, checked.
#[derive(Debug, Clone, PartialEq)]
pub struct ClientDoc {
    pub client_id: String,
    pub client_name: Option<String>,
    pub client_uri: Option<String>,
    pub redirect_uris: Vec<String>,
}

/// An authorization request waiting for a person in the console, in Valkey.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pending {
    pub client_id: String,
    pub client_name: Option<String>,
    pub client_uri: Option<String>,
    pub redirect_uri: String,
    pub state: Option<String>,
    pub code_challenge: String,
    /// The MCP endpoint's path, and its URL as the client named it.
    pub resource: String,
    pub resource_url: String,
    pub scope: String,
    pub issuer: String,
}

/// An approved request, waiting for the client to exchange its code, in Valkey.
#[derive(Serialize, Deserialize)]
struct Approved {
    request: Pending,
    key_id: Uuid,
    created_by: Option<String>,
}

/// Per replica: fetched metadata documents and what tokens stand for.
pub struct Store {
    docs: TtlCache<String, Arc<ClientDoc>>,
    tokens: TtlCache<[u8; 32], Cached>,
    http: OnceLock<reqwest::Client>,
    /// When the current minute of authorization requests began, and how many it has had.
    authorizations: Mutex<Option<(Instant, u32)>>,
}

impl Default for Store {
    fn default() -> Self {
        Self {
            docs: TtlCache::new(TOKEN_CACHE, MAX_DOCUMENTS),
            tokens: TtlCache::new(TOKEN_CACHE, 10_000),
            http: OnceLock::new(),
            authorizations: Mutex::new(None),
        }
    }
}

#[derive(Clone)]
struct Cached {
    key_id: Uuid,
    resource: String,
    expires_at: DateTime<Utc>,
}

pub fn request_key(id: &str) -> String {
    format!("gw:oauth:req:{id}")
}

fn code_key(code: &str) -> String {
    format!("gw:oauth:code:{}", hex::encode(crate::util::sha256(code.as_bytes())))
}

/// The gateway's address as the client reached it: the issuer, and where its endpoints are.
fn origin(headers: &HeaderMap, uri: &Uri) -> String {
    crate::a2a::origin(headers, uri)
}

fn is_loopback(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1")
}

// ---------------------------------------------------------------- client metadata documents

/// A Client Identifier URL (CIMD 3): https, with a path, without credentials, fragment or dot
/// segments. Plain http only on loopback, for development (the network policy still decides).
pub fn check_client_id(raw: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(raw)
        .map_err(|_| "client_id is not a URL: this gateway only takes clients that identify with a metadata document URL".to_string())?;
    let loopback = url.host_str().is_some_and(is_loopback);
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        return Err("client_id must be an https URL".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("client_id must not hold credentials".into());
    }
    if url.fragment().is_some() || raw.contains('#') {
        return Err("client_id must not have a fragment".into());
    }
    // The parser removes dot segments, so they are looked for in the URL as given.
    let path = raw.split_once("://").map_or("", |(_, rest)| rest).split(['?', '#']).next().unwrap_or("");
    if path.split('/').skip(1).any(|s| s == "." || s == "..") {
        return Err("client_id must not have . or .. segments".into());
    }
    if url.path().len() <= 1 {
        return Err("client_id must have a path, such as https://example.com/client.json".into());
    }
    Ok(url)
}

/// Where a client may be sent back to: https, or http on loopback for programs on the person's computer.
fn valid_redirect(raw: &str) -> bool {
    reqwest::Url::parse(raw)
        .is_ok_and(|u| u.fragment().is_none() && (u.scheme() == "https" || (u.scheme() == "http" && u.host_str().is_some_and(is_loopback))))
}

/// Checks a fetched document (CIMD 4, MCP): it names itself, lists its redirect URIs, and
/// authenticates with nothing but PKCE.
pub fn parse_document(client_id: &str, v: &Value) -> Result<ClientDoc, String> {
    let o = v.as_object().ok_or("the client's metadata document is not a JSON object")?;
    if o.get("client_id").and_then(Value::as_str) != Some(client_id) {
        return Err("the client's metadata document names another client_id".into());
    }
    if o.contains_key("client_secret") || o.contains_key("client_secret_expires_at") {
        return Err("the client's metadata document holds a client secret, which metadata documents must not".into());
    }
    match o.get("token_endpoint_auth_method").and_then(Value::as_str) {
        None | Some("none") => {}
        Some("private_key_jwt") => {
            return Err("this gateway takes public clients only (token_endpoint_auth_method none), not private_key_jwt yet".into());
        }
        Some(m) => return Err(format!("token_endpoint_auth_method {m} is not allowed for clients with a metadata document")),
    }
    let has = |field: &str, value: &str| o.get(field).and_then(Value::as_array).is_none_or(|a| a.iter().any(|x| x.as_str() == Some(value)));
    if !has("grant_types", "authorization_code") || !has("response_types", "code") {
        return Err("the client does not use the authorization code flow".into());
    }
    let redirect_uris: Vec<String> = o
        .get("redirect_uris")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    if redirect_uris.is_empty() {
        return Err("the client's metadata document lists no redirect_uris".into());
    }
    if let Some(bad) = redirect_uris.iter().find(|u| !valid_redirect(u)) {
        return Err(format!("redirect URI {bad} is neither https nor on localhost"));
    }
    let text = |field: &str| {
        o.get(field).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(|s| s.chars().take(200).collect::<String>())
    };
    Ok(ClientDoc { client_id: client_id.to_string(), client_name: text("client_name"), client_uri: text("client_uri"), redirect_uris })
}

/// How long a document may be kept, by its Cache-Control: 1 minute to a day, 10 minutes without
/// one, not at all with `no-store`.
fn cache_for(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let cc = headers.get(reqwest::header::CACHE_CONTROL).and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
    if cc.contains("no-store") || cc.contains("no-cache") {
        return None;
    }
    let max_age = cc.split(',').find_map(|d| d.trim().strip_prefix("max-age=")?.parse::<u64>().ok()).unwrap_or(600);
    Some(Duration::from_secs(max_age.clamp(60, 86_400)))
}

impl Store {
    fn http(&self, state: &AppState) -> &reqwest::Client {
        self.http.get_or_init(|| {
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .dns_resolver(state.cfg.upstream_policy.resolver())
                // Not through an environment proxy, which would resolve hostnames past the policy.
                .no_proxy()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("HTTP client")
        })
    }

    /// Whether this replica takes another authorization request now (`AUTHORIZE_PER_MINUTE`).
    pub fn allow_authorization(&self) -> bool {
        let mut slot = self.authorizations.lock().expect("authorizations");
        let now = Instant::now();
        match slot.as_mut() {
            Some((since, n)) if now.duration_since(*since) < Duration::from_secs(60) => {
                if *n >= AUTHORIZE_PER_MINUTE {
                    return false;
                }
                *n += 1;
                true
            }
            _ => {
                *slot = Some((now, 1));
                true
            }
        }
    }

    /// The client's metadata document, fetched (without following redirects, at most 5 KB, from an
    /// address the gateway may reach) or from this replica's cache. Why a fetch failed goes to the
    /// log only: the caller is not signed in, and the answer must not tell what is at the address.
    pub async fn document(&self, state: &AppState, client_id: &str) -> Result<Arc<ClientDoc>, String> {
        if let Some(doc) = self.docs.get(client_id) {
            return Ok(doc);
        }
        check_client_id(client_id)?;
        state
            .cfg
            .upstream_policy
            .check_url(client_id)
            .map_err(|why| format!("the client's metadata document is at an address the gateway may not reach: {why}"))?;
        let unreadable = |why: String| {
            tracing::info!(client = %client_id, why, "an MCP client's metadata document could not be read");
            format!("the client's metadata document at {client_id} could not be read (see the gateway log)")
        };
        let resp = self
            .http(state)
            .get(client_id)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|e| unreadable(format!("fetch failed: {e}")))?;
        if resp.status() != reqwest::StatusCode::OK {
            return Err(unreadable(format!("answered HTTP {}", resp.status().as_u16())));
        }
        let keep = cache_for(resp.headers());
        let mut body = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            body.extend_from_slice(&chunk.map_err(|e| unreadable(format!("read failed: {e}")))?);
            if body.len() > MAX_DOCUMENT {
                return Err(unreadable("larger than 5 KB".into()));
            }
        }
        let v: Value = serde_json::from_slice(&body).map_err(|_| unreadable("not JSON".into()))?;
        let doc = Arc::new(parse_document(client_id, &v)?);
        if let Some(keep) = keep {
            // Within `MAX_DOCUMENTS`: expired ones go first, then the one that expires soonest.
            self.docs.insert_until(client_id.to_string(), doc.clone(), Instant::now() + keep);
        }
        Ok(doc)
    }

    /// The key an access token acts as on `path`: valid, for this endpoint, and the key still usable.
    async fn key_for_token(&self, state: &AppState, token: &str, path: &str) -> Result<Arc<VirtualKey>, GatewayError> {
        let hash = crate::util::sha256(token.as_bytes());
        let cached = self.tokens.get(&hash);
        let c = match cached {
            Some(c) => c,
            None => {
                let g = db::client_grant_by_access(&state.db, &hash)
                    .await
                    .map_err(|e| GatewayError::internal(format!("token lookup failed: {e}")))?
                    .ok_or_else(|| GatewayError::unauthorized("the access token is invalid or has expired"))?;
                let (db, id) = (state.db.clone(), g.id);
                tokio::spawn(async move {
                    if let Err(e) = db::touch_client_grant(&db, id).await {
                        tracing::warn!(error = %e, "failed to note a client grant's use");
                    }
                });
                let c = Cached { key_id: g.key_id, resource: g.resource, expires_at: g.access_expires_at };
                self.tokens.insert(hash, c.clone());
                c
            }
        };
        if c.expires_at <= Utc::now() {
            return Err(GatewayError::unauthorized("the access token has expired"));
        }
        if c.resource != "/" && c.resource != path.trim_end_matches('/') {
            return Err(GatewayError::unauthorized(format!("the access token is for {}, not this MCP endpoint", c.resource)));
        }
        auth::key_by_id(state, c.key_id).map_err(|e| {
            if e.status == StatusCode::NOT_FOUND { GatewayError::unauthorized("the access token's key no longer exists") } else { e }
        })
    }

    /// Forgets what this replica read of a grant's tokens, after a revocation here.
    pub fn forget_tokens(&self) {
        self.tokens.clear();
    }
}

// ---------------------------------------------------------------- discovery

fn cors(mut resp: Response) -> Response {
    resp.headers_mut().insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    resp
}

fn protected_resource(headers: &HeaderMap, uri: &Uri, path: &str) -> Response {
    let origin = origin(headers, uri);
    cors(
        Json(json!({
            "resource": format!("{origin}{path}"),
            "authorization_servers": [origin],
            "scopes_supported": [SCOPE],
            "bearer_methods_supported": ["header"],
            "resource_name": "Inference Gateway MCP",
        }))
        .into_response(),
    )
}

/// `GET /.well-known/oauth-protected-resource`: every MCP endpoint of the gateway (RFC 9728).
pub async fn resource_root(headers: HeaderMap, uri: Uri) -> Response {
    protected_resource(&headers, &uri, "")
}

/// `GET /.well-known/oauth-protected-resource/mcp`.
pub async fn resource_hub(headers: HeaderMap, uri: Uri) -> Response {
    protected_resource(&headers, &uri, "/mcp")
}

/// `GET /.well-known/oauth-protected-resource/mcp/{server}`.
pub async fn resource_server(headers: HeaderMap, uri: Uri, axum::extract::Path(server): axum::extract::Path<String>) -> Response {
    protected_resource(&headers, &uri, &format!("/mcp/{server}"))
}

/// `GET /.well-known/oauth-authorization-server` (RFC 8414).
pub async fn metadata(headers: HeaderMap, uri: Uri) -> Response {
    let issuer = origin(&headers, &uri);
    cors(
        Json(json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{issuer}/oauth/authorize"),
            "token_endpoint": format!("{issuer}/oauth/token"),
            "revocation_endpoint": format!("{issuer}/oauth/revoke"),
            "response_types_supported": ["code"],
            "grant_types_supported": ["authorization_code", "refresh_token"],
            "code_challenge_methods_supported": ["S256"],
            "token_endpoint_auth_methods_supported": ["none"],
            "revocation_endpoint_auth_methods_supported": ["none"],
            "scopes_supported": [SCOPE],
            "client_id_metadata_document_supported": true,
            "authorization_response_iss_parameter_supported": true,
        }))
        .into_response(),
    )
}

/// The `WWW-Authenticate` challenge of a refused MCP request: where its protected resource
/// metadata is (RFC 9728 5.1), and with `invalid_token` when a token was presented.
pub fn challenge(parts: &Parts, token_presented: bool) -> HeaderValue {
    let origin = origin(&parts.headers, &parts.uri);
    let path = parts.uri.path().trim_end_matches('/');
    let error = if token_presented { "error=\"invalid_token\", " } else { "" };
    HeaderValue::from_str(&format!(
        "Bearer {error}resource_metadata=\"{origin}/.well-known/oauth-protected-resource{path}\", scope=\"{SCOPE}\""
    ))
    .unwrap_or_else(|_| HeaderValue::from_static("Bearer"))
}

/// The key an MCP request acts as: an OAuth access token's, or a virtual key presented directly.
/// A refused request gets the challenge MCP clients start their sign-in from.
pub struct McpKey(pub Arc<VirtualKey>);

impl FromRequestParts<AppState> for McpKey {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Response> {
        let presented = auth::presented_key(&parts.headers).map(str::to_string);
        let result = match presented.as_deref() {
            Some(t) if t.starts_with(ACCESS_PREFIX) => state.mcp_auth.key_for_token(state, t, parts.uri.path()).await,
            _ => auth::authenticate(state, &parts.headers),
        };
        result.map(McpKey).map_err(|e| {
            let unauthorized = e.status == StatusCode::UNAUTHORIZED;
            let mut resp = e.into_response_for(Dialect::OpenAi);
            if unauthorized {
                resp.headers_mut().insert(header::WWW_AUTHENTICATE, challenge(parts, presented.is_some()));
            }
            resp
        })
    }
}

// ---------------------------------------------------------------- authorization

/// A refusal shown to the person, for a request whose client or redirect URI cannot be trusted:
/// nothing is sent back to the client then.
fn refuse_page(msg: &str) -> Response {
    refuse_with(StatusCode::BAD_REQUEST, msg)
}

fn refuse_with(status: StatusCode, msg: &str) -> Response {
    let msg = msg.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    (
        status,
        Html(format!(
            "<!doctype html><meta charset=utf-8><title>Sign-in refused</title><body style=\"font-family:system-ui;max-width:40em;margin:4em auto\"><h1>The app's sign-in was refused</h1><p>{msg}</p><p>Nothing was sent back to the app.</p>"
        )),
    )
        .into_response()
}

/// Back to the client with an OAuth error, its `state` and the issuer (RFC 9207).
fn error_redirect(redirect_uri: &str, error: &str, description: &str, state: Option<&str>, issuer: &str) -> Response {
    Redirect::to(&with_params(
        redirect_uri,
        &[("error", Some(error)), ("error_description", Some(description)), ("state", state), ("iss", Some(issuer))],
    ))
    .into_response()
}

pub fn with_params(base: &str, params: &[(&str, Option<&str>)]) -> String {
    let Ok(mut url) = reqwest::Url::parse(base) else { return base.to_string() };
    {
        let mut q = url.query_pairs_mut();
        for (k, v) in params {
            if let Some(v) = v {
                q.append_pair(k, v);
            }
        }
    }
    url.to_string()
}

/// The MCP endpoint a `resource` names, as a path: `/mcp`, `/mcp/{server}`, or `/` for the
/// gateway as a whole (what the root protected resource metadata names). Only this gateway's.
pub fn resource_path(resource: Option<&str>, issuer: &str) -> Result<(String, String), String> {
    let Some(r) = resource.map(str::trim).filter(|r| !r.is_empty()) else { return Ok(("/mcp".into(), format!("{issuer}/mcp"))) };
    let url = reqwest::Url::parse(r).map_err(|_| "resource is not a URL".to_string())?;
    if url.fragment().is_some() || url.query().is_some() {
        return Err("resource must not have a query or fragment".into());
    }
    let path = url.path().trim_end_matches('/');
    let ok = path.is_empty() || path == "/mcp" || path.strip_prefix("/mcp/").is_some_and(|s| !s.is_empty() && !s.contains('/'));
    if !ok {
        return Err(format!("{r} is not an MCP endpoint of this gateway"));
    }
    Ok((if path.is_empty() { "/".into() } else { path.to_string() }, r.to_string()))
}

/// `GET /oauth/authorize`: checks the client and its request, then sends the person to the console
/// to approve it.
pub async fn authorize(State(state): State<AppState>, headers: HeaderMap, uri: Uri, Query(q): Query<HashMap<String, String>>) -> Response {
    let issuer = origin(&headers, &uri);
    let param = |k: &str| q.get(k).map(String::as_str).filter(|v| !v.is_empty());
    let Some(client_id) = param("client_id") else { return refuse_page("The app did not say who it is (client_id).") };
    if !state.mcp_auth.allow_authorization() {
        tracing::warn!(client = %client_id, "MCP client sign-ins are being started faster than this replica takes them");
        return refuse_with(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many apps are signing in to this gateway right now; try again in a minute.",
        );
    }
    let doc = match state.mcp_auth.document(&state, client_id).await {
        Ok(d) => d,
        Err(e) => return refuse_page(&e),
    };
    let redirect_uri = match param("redirect_uri") {
        Some(r) if doc.redirect_uris.iter().any(|u| u == r) => r.to_string(),
        Some(r) => return refuse_page(&format!("The app asked to return to {r}, which its metadata document does not list.")),
        None if doc.redirect_uris.len() == 1 => doc.redirect_uris[0].clone(),
        None => return refuse_page("The app did not say where to return to (redirect_uri)."),
    };
    let st = param("state");
    let fail = |error: &str, description: &str| error_redirect(&redirect_uri, error, description, st, &issuer);
    if param("response_type") != Some("code") {
        return fail("unsupported_response_type", "only the authorization code flow (response_type=code)");
    }
    let Some(challenge) = param("code_challenge") else { return fail("invalid_request", "PKCE is required: send code_challenge") };
    if param("code_challenge_method") != Some("S256") {
        return fail("invalid_request", "code_challenge_method must be S256");
    }
    let scope = param("scope").unwrap_or(SCOPE);
    if scope.split_whitespace().any(|s| s != SCOPE) {
        return fail("invalid_scope", "the only scope is mcp");
    }
    let (resource, resource_url) = match resource_path(param("resource"), &issuer) {
        Ok(r) => r,
        Err(e) => return fail("invalid_target", &e),
    };
    let pending = Pending {
        client_id: client_id.to_string(),
        client_name: doc.client_name.clone(),
        client_uri: doc.client_uri.clone(),
        redirect_uri: redirect_uri.clone(),
        state: st.map(str::to_string),
        code_challenge: challenge.to_string(),
        resource,
        resource_url,
        scope: SCOPE.into(),
        issuer: issuer.clone(),
    };
    let id = crate::util::random_hex(16);
    let saved = serde_json::to_string(&pending).expect("serializable");
    if let Err(e) = state.cache.put_for(&request_key(&id), &saved, REQUEST_TTL).await {
        tracing::warn!(error = %e, "failed to keep an MCP client's sign-in");
        return fail("temporarily_unavailable", "the gateway could not keep the sign-in; try again");
    }
    Redirect::to(&format!("/#/connect/{id}")).into_response()
}

/// Approves a request for `key`: the code the client exchanges, and where to send the person.
pub async fn approve(state: &AppState, request: Pending, key_id: Uuid, created_by: Option<String>) -> Result<String, String> {
    let code = format!("{CODE_PREFIX}{}", crate::util::random_hex(32));
    let (redirect, st, issuer) = (request.redirect_uri.clone(), request.state.clone(), request.issuer.clone());
    let approved = serde_json::to_string(&Approved { request, key_id, created_by }).expect("serializable");
    state.cache.put_for(&code_key(&code), &approved, CODE_TTL).await.map_err(|e| format!("the gateway could not keep the code: {e}"))?;
    Ok(with_params(&redirect, &[("code", Some(&code)), ("state", st.as_deref()), ("iss", Some(&issuer))]))
}

/// Where to send the person who denied a request.
pub fn denied(request: &Pending) -> String {
    with_params(
        &request.redirect_uri,
        &[
            ("error", Some("access_denied")),
            ("error_description", Some("the request was denied")),
            ("state", request.state.as_deref()),
            ("iss", Some(&request.issuer)),
        ],
    )
}

// ---------------------------------------------------------------- tokens

fn token_error(status: StatusCode, error: &str, description: &str) -> Response {
    no_store(cors((status, Json(json!({ "error": error, "error_description": description }))).into_response()))
}

fn no_store(mut resp: Response) -> Response {
    resp.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp.headers_mut().insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    resp
}

fn form(body: &Bytes) -> HashMap<String, String> {
    let body = String::from_utf8_lossy(body);
    reqwest::Url::parse(&format!("http://x/?{body}")).map(|u| u.query_pairs().into_owned().collect()).unwrap_or_default()
}

fn new_tokens() -> (String, String, Duration) {
    (format!("{ACCESS_PREFIX}{}", crate::util::random_hex(32)), format!("{REFRESH_PREFIX}{}", crate::util::random_hex(32)), ACCESS_TTL)
}

fn token_response(access: &str, refresh: &str, scope: &str) -> Response {
    no_store(cors(
        Json(json!({
            "access_token": access, "token_type": "Bearer", "expires_in": ACCESS_TTL.as_secs(),
            "refresh_token": refresh, "scope": scope,
        }))
        .into_response(),
    ))
}

fn later(d: Duration) -> DateTime<Utc> {
    Utc::now() + chrono::Duration::from_std(d).expect("short duration")
}

/// `POST /oauth/token`: a code for tokens, or a refresh token for new ones.
pub async fn token(State(state): State<AppState>, headers: HeaderMap, uri: Uri, body: Bytes) -> Response {
    let f = form(&body);
    let param = |k: &str| f.get(k).map(String::as_str).filter(|v| !v.is_empty());
    let Some(client_id) = param("client_id") else {
        return token_error(StatusCode::UNAUTHORIZED, "invalid_client", "client_id is required");
    };
    match param("grant_type") {
        Some("authorization_code") => {
            let Some(code) = param("code") else { return token_error(StatusCode::BAD_REQUEST, "invalid_request", "code is required") };
            let taken = match state.cache.take(&code_key(code)).await {
                Ok(t) => t,
                Err(e) => {
                    return token_error(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "temporarily_unavailable",
                        &format!("the code could not be read: {e}"),
                    );
                }
            };
            let Some(a) = taken.and_then(|t| serde_json::from_str::<Approved>(&t).ok()) else {
                return token_error(StatusCode::BAD_REQUEST, "invalid_grant", "the code is invalid, expired or used");
            };
            let r = &a.request;
            if r.client_id != client_id {
                return token_error(StatusCode::BAD_REQUEST, "invalid_grant", "the code was issued to another client");
            }
            if param("redirect_uri").is_some_and(|u| u != r.redirect_uri) {
                return token_error(StatusCode::BAD_REQUEST, "invalid_grant", "redirect_uri differs from the authorization request's");
            }
            let verified = param("code_verifier").is_some_and(|v| {
                bool::from(subtle::ConstantTimeEq::ct_eq(crate::oauth::challenge(v).as_bytes(), r.code_challenge.as_bytes()))
            });
            if !verified {
                return token_error(StatusCode::BAD_REQUEST, "invalid_grant", "the code_verifier does not match the code_challenge");
            }
            if let Some(res) = param("resource") {
                match resource_path(Some(res), &origin(&headers, &uri)) {
                    Ok((path, _)) if path == r.resource => {}
                    _ => {
                        return token_error(StatusCode::BAD_REQUEST, "invalid_target", "resource differs from the authorization request's");
                    }
                }
            }
            if auth::key_by_id(&state, a.key_id).is_err() {
                return token_error(StatusCode::BAD_REQUEST, "invalid_grant", "the key it was approved for can no longer be used");
            }
            let (access, refresh, _) = new_tokens();
            let grant = ClientGrant {
                id: Uuid::now_v7(),
                key_id: a.key_id,
                client_id: r.client_id.clone(),
                client_name: r.client_name.clone(),
                redirect_uri: r.redirect_uri.clone(),
                resource: r.resource.clone(),
                scope: r.scope.clone(),
                access_expires_at: later(ACCESS_TTL),
                refresh_expires_at: later(REFRESH_TTL),
                created_by: a.created_by.clone(),
                created_at: Utc::now(),
                last_used_at: None,
            };
            let hash = |t: &str| crate::util::sha256(t.as_bytes());
            if let Err(e) = db::insert_client_grant(&state.db, &grant, &hash(&access), &hash(&refresh)).await {
                return token_error(StatusCode::INTERNAL_SERVER_ERROR, "server_error", &format!("the grant could not be saved: {e}"));
            }
            tracing::info!(client = %grant.client_id, key = %grant.key_id, resource = %grant.resource, "MCP client signed in");
            token_response(&access, &refresh, &grant.scope)
        }
        Some("refresh_token") => {
            let Some(old) = param("refresh_token") else {
                return token_error(StatusCode::BAD_REQUEST, "invalid_request", "refresh_token is required");
            };
            let (access, refresh, _) = new_tokens();
            let hash = |t: &str| crate::util::sha256(t.as_bytes());
            let rotated = db::rotate_client_grant(
                &state.db,
                client_id,
                &hash(old),
                &hash(&access),
                later(ACCESS_TTL),
                &hash(&refresh),
                later(REFRESH_TTL),
            )
            .await;
            match rotated {
                Ok(Some(g)) if auth::key_by_id(&state, g.key_id).is_ok() => {
                    // The replaced access token stops working here at once, on other replicas within a minute.
                    state.mcp_auth.forget_tokens();
                    token_response(&access, &refresh, &g.scope)
                }
                Ok(Some(_)) => token_error(StatusCode::BAD_REQUEST, "invalid_grant", "the key it was approved for can no longer be used"),
                // A refresh token the gateway already replaced is in someone else's hands: the grant ends.
                Ok(None) => match db::revoke_replayed_client_grant(&state.db, client_id, &hash(old)).await {
                    Ok(Some(id)) => {
                        state.mcp_auth.forget_tokens();
                        tracing::warn!(grant = %id, client = %client_id, "a replaced refresh token was presented again; the grant is revoked");
                        token_error(
                            StatusCode::BAD_REQUEST,
                            "invalid_grant",
                            "the refresh token was used before; the grant has been revoked, sign in again",
                        )
                    }
                    Ok(None) => token_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_grant",
                        "the refresh token is invalid, expired, used or not this client's",
                    ),
                    Err(e) => token_error(StatusCode::INTERNAL_SERVER_ERROR, "server_error", &format!("the grant could not be read: {e}")),
                },
                Err(e) => token_error(StatusCode::INTERNAL_SERVER_ERROR, "server_error", &format!("the grant could not be read: {e}")),
            }
        }
        _ => token_error(StatusCode::BAD_REQUEST, "unsupported_grant_type", "grant_type must be authorization_code or refresh_token"),
    }
}

/// `POST /oauth/revoke` (RFC 7009): revokes the grant of an access or refresh token. Answers 200
/// whether or not there was one.
pub async fn revoke(State(state): State<AppState>, body: Bytes) -> Response {
    let f = form(&body);
    let (Some(token), Some(client_id)) = (f.get("token"), f.get("client_id")) else {
        return token_error(StatusCode::BAD_REQUEST, "invalid_request", "token and client_id are required");
    };
    match db::revoke_client_grant(&state.db, &crate::util::sha256(token.as_bytes()), client_id).await {
        Ok(Some(id)) => {
            state.mcp_auth.forget_tokens();
            tracing::info!(grant = %id, client = %client_id, "MCP client revoked its grant");
            no_store(cors(StatusCode::OK.into_response()))
        }
        Ok(None) => no_store(cors(StatusCode::OK.into_response())),
        Err(e) => token_error(StatusCode::INTERNAL_SERVER_ERROR, "server_error", &e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_ids_are_https_urls_with_a_path() {
        assert!(check_client_id("https://app.example.com/oauth/client.json").is_ok());
        assert!(check_client_id("http://127.0.0.1:9996/client.json").is_ok());
        for bad in [
            "my-client",
            "http://app.example.com/client.json",
            "https://app.example.com",
            "https://app.example.com/",
            "https://user:pw@app.example.com/c.json",
            "https://app.example.com/c.json#x",
            "https://app.example.com/a/../c.json",
            "https://app.example.com/./c.json",
        ] {
            assert!(check_client_id(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn documents_name_themselves_and_use_pkce_only() {
        let id = "https://app.example.com/client.json";
        let ok = json!({ "client_id": id, "client_name": "App", "redirect_uris": ["https://app.example.com/cb", "http://localhost:33418/cb"], "token_endpoint_auth_method": "none" });
        let doc = parse_document(id, &ok).unwrap();
        assert_eq!((doc.client_name.as_deref(), doc.redirect_uris.len()), (Some("App"), 2));
        let with = |k: &str, v: Value| {
            let mut d = ok.clone();
            d[k] = v;
            parse_document(id, &d)
        };
        assert!(with("client_id", json!("https://other.example.com/client.json")).is_err());
        assert!(with("client_secret", json!("s")).is_err());
        assert!(with("token_endpoint_auth_method", json!("client_secret_basic")).is_err());
        assert!(with("token_endpoint_auth_method", json!("private_key_jwt")).is_err());
        assert!(with("redirect_uris", json!([])).is_err());
        assert!(with("redirect_uris", json!(["http://app.example.com/cb"])).is_err());
        assert!(with("redirect_uris", json!(["myapp://cb"])).is_err());
        assert!(with("grant_types", json!(["client_credentials"])).is_err());
        assert!(with("grant_types", json!(["authorization_code", "refresh_token"])).is_ok());
    }

    #[test]
    fn resources_are_this_gateways_mcp_endpoints() {
        let gw = "https://gw.example.com";
        assert_eq!(resource_path(None, gw).unwrap().0, "/mcp");
        assert_eq!(resource_path(Some("https://gw.example.com/mcp"), gw).unwrap().0, "/mcp");
        assert_eq!(resource_path(Some("https://gw.example.com/mcp/github/"), gw).unwrap().0, "/mcp/github");
        assert_eq!(resource_path(Some("https://gw.example.com"), gw).unwrap().0, "/");
        assert!(resource_path(Some("https://gw.example.com/v1/chat/completions"), gw).is_err());
        assert!(resource_path(Some("https://gw.example.com/mcp/a/b"), gw).is_err());
    }

    #[test]
    fn the_document_cache_is_bounded_and_sign_ins_are_limited() {
        let store = Store::default();
        let doc = |n: usize| {
            Arc::new(ClientDoc {
                client_id: format!("https://c.example/{n}.json"),
                client_name: None,
                client_uri: None,
                redirect_uris: vec![],
            })
        };
        let now = Instant::now();
        for n in 0..MAX_DOCUMENTS {
            store.docs.insert_until(doc(n).client_id.clone(), doc(n), now + Duration::from_secs(100 + n as u64));
        }
        store.docs.insert_until("https://c.example/new.json".into(), doc(9999), now + Duration::from_secs(3600));
        // The one that expired soonest made room.
        assert!(
            store.docs.get("https://c.example/0.json").is_none()
                && store.docs.get("https://c.example/1.json").is_some()
                && store.docs.get("https://c.example/new.json").is_some()
        );

        for _ in 0..AUTHORIZE_PER_MINUTE {
            assert!(store.allow_authorization());
        }
        assert!(!store.allow_authorization());
        // A minute later, the window starts over.
        *store.authorizations.lock().unwrap() = Some((Instant::now() - Duration::from_secs(61), AUTHORIZE_PER_MINUTE));
        assert!(store.allow_authorization());
    }

    #[test]
    fn documents_are_cached_by_their_headers() {
        let h = |v: &str| {
            let mut m = reqwest::header::HeaderMap::new();
            m.insert(reqwest::header::CACHE_CONTROL, v.parse().unwrap());
            m
        };
        assert_eq!(cache_for(&reqwest::header::HeaderMap::new()), Some(Duration::from_secs(600)));
        assert_eq!(cache_for(&h("public, max-age=5")), Some(Duration::from_secs(60)));
        assert_eq!(cache_for(&h("max-age=3600")), Some(Duration::from_secs(3600)));
        assert_eq!(cache_for(&h("no-store")), None);
    }
}
