//! OAuth to upstreams, MCP servers and A2A agents alike. Two ways:
//!
//! - **`oauth`** (client credentials): the gateway gets tokens with its own client, keeps each until
//!   a minute before it expires, and gets a new one when the upstream refuses it.
//! - **`oauth_user`** (per key): each virtual key signs in to the upstream once, in the console, with
//!   the authorization code flow and PKCE. Its tokens are stored sealed per (upstream, key) and
//!   refreshed under a row lock, since an authorization server may rotate refresh tokens.
//!
//! Both send the RFC 8707 `resource` when one is set, as MCP asks of its clients. Discovery finds an
//! upstream's authorization server (RFC 9728 for MCP servers, the agent card for A2A agents, then
//! RFC 8414 or OpenID metadata) and gets the gateway a public client there: its client ID metadata
//! document (CIMD) where the server takes one, else dynamic client registration (RFC 7591).

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use base64::Engine;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    crypto::SecretBox,
    db,
    error::{ErrorKind, GatewayError},
    mcp::McpAuth,
    state::AppState,
};

/// An upstream's OAuth client, as configured.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct OAuthConfig {
    /// Where the gateway gets tokens.
    pub token_url: String,
    /// Where keys sign in, for `oauth_user`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_url: Option<String>,
    pub client_id: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    /// The RFC 8707 resource the tokens are for, such as the MCP server's URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    /// The redirect URI the client was registered with, for `oauth_user`. Without one, the
    /// gateway's address as the console reaches it, followed by `/admin/auth/oauth/callback`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_uri: Option<String>,
    /// How the client authenticates at the token URL: `client_secret_basic` (the default with a
    /// secret), `client_secret_post`, or `none` (a public client).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_auth: Option<String>,
}

/// Which kind of upstream a token is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Mcp,
    A2a,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Mcp => "mcp",
            Kind::A2a => "a2a",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "mcp" => Some(Kind::Mcp),
            "a2a" => Some(Kind::A2a),
            _ => None,
        }
    }
}

/// An upstream's OAuth client, ready to use: its secret decrypted once.
#[derive(Debug)]
pub struct Client {
    pub kind: Kind,
    pub upstream: Uuid,
    /// The upstream's name, for messages.
    pub name: String,
    pub config: OAuthConfig,
    secret: Option<String>,
}

/// How the gateway authenticates to an upstream.
#[derive(Debug)]
pub enum UpstreamAuth {
    None,
    /// A header with a stored secret: `Authorization: Bearer …`, or one of the upstream's choice.
    Static(HeaderName, HeaderValue),
    /// Tokens from the gateway's own client credentials.
    Client(Arc<Client>),
    /// Each key's own tokens.
    PerKey(Arc<Client>),
}

/// Why there is no token to call an upstream with.
#[derive(Debug)]
pub enum AuthError {
    /// The key has not signed in to the upstream (or its sign-in expired).
    NotConnected(String),
    Failed(String),
}

impl AuthError {
    pub fn message(&self) -> String {
        match self {
            AuthError::NotConnected(name) => {
                format!("this key has not signed in to '{name}'; sign it in on the console's Access page (Connections)")
            }
            AuthError::Failed(e) => e.clone(),
        }
    }

    pub fn into_gateway(self) -> GatewayError {
        match self {
            AuthError::NotConnected(_) => GatewayError::forbidden(self.message()),
            AuthError::Failed(e) => {
                GatewayError { upstream: true, ..GatewayError::new(ErrorKind::Upstream, StatusCode::BAD_GATEWAY, format!("the upstream's sign-in failed: {e}")) }
            }
        }
    }
}

fn sensitive(v: &str) -> Option<HeaderValue> {
    let mut v = HeaderValue::from_str(v).ok()?;
    v.set_sensitive(true);
    Some(v)
}

fn bearer(token: &str) -> Result<(HeaderName, HeaderValue), AuthError> {
    sensitive(&format!("Bearer {token}")).map(|v| (header::AUTHORIZATION, v)).ok_or_else(|| AuthError::Failed("the token is not a valid header value".into()))
}

impl UpstreamAuth {
    /// From an upstream's stored `auth`, header name, sealed secret and OAuth client.
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        kind: Kind,
        upstream: Uuid,
        name: &str,
        auth: &str,
        header: Option<&str>,
        sealed: Option<&[u8]>,
        oauth: Option<&OAuthConfig>,
        sb: &SecretBox,
    ) -> Result<Self, String> {
        let auth = McpAuth::parse(auth).ok_or("unknown auth")?;
        let secret = match sealed {
            Some(sealed) => Some(String::from_utf8(sb.open(sealed)?).map_err(|_| "corrupt secret")?),
            None => None,
        };
        let client = |oauth: Option<&OAuthConfig>, secret: Option<String>| -> Result<Arc<Client>, String> {
            let config = oauth.cloned().ok_or("its OAuth client is missing")?;
            Ok(Arc::new(Client { kind, upstream, name: name.to_string(), config, secret }))
        };
        Ok(match auth {
            McpAuth::None => UpstreamAuth::None,
            McpAuth::Bearer | McpAuth::Header => {
                let secret = secret.ok_or("its secret is missing")?;
                let invalid = || "the secret is not a valid header value".to_string();
                match auth {
                    McpAuth::Bearer => UpstreamAuth::Static(header::AUTHORIZATION, sensitive(&format!("Bearer {secret}")).ok_or_else(invalid)?),
                    _ => {
                        let name = header.and_then(|h| HeaderName::try_from(h).ok()).ok_or("invalid auth header")?;
                        UpstreamAuth::Static(name, sensitive(&secret).ok_or_else(invalid)?)
                    }
                }
            }
            McpAuth::OAuth => UpstreamAuth::Client(client(oauth, secret)?),
            McpAuth::OAuthUser => UpstreamAuth::PerKey(client(oauth, secret)?),
        })
    }

    /// Whether the upstream's tokens may be renewed when it refuses one.
    pub fn renews(&self) -> bool {
        matches!(self, UpstreamAuth::Client(_) | UpstreamAuth::PerKey(_))
    }

    /// Whether each key signs in on its own.
    pub fn per_key(&self) -> bool {
        matches!(self, UpstreamAuth::PerKey(_))
    }

    /// The header to call the upstream with, for `key` (needed for `oauth_user`).
    pub async fn header(&self, state: &AppState, key: Option<Uuid>) -> Result<Option<(HeaderName, HeaderValue)>, AuthError> {
        match self {
            UpstreamAuth::None => Ok(None),
            UpstreamAuth::Static(h, v) => Ok(Some((h.clone(), v.clone()))),
            UpstreamAuth::Client(c) => bearer(&state.oauth.client_token(state, c).await.map_err(AuthError::Failed)?).map(Some),
            UpstreamAuth::PerKey(c) => {
                let key = key.ok_or_else(|| AuthError::NotConnected(c.name.clone()))?;
                bearer(&state.oauth.user_token(state, c, key).await?).map(Some)
            }
        }
    }

    /// The upstream refused the token: get another one next time.
    pub async fn renew(&self, state: &AppState, key: Option<Uuid>) {
        match self {
            UpstreamAuth::Client(c) => {
                state.oauth.client.lock().expect("tokens lock").remove(&(c.kind, c.upstream));
            }
            UpstreamAuth::PerKey(c) => {
                if let Some(key) = key {
                    state.oauth.user.lock().expect("tokens lock").remove(&(c.kind, c.upstream, key));
                    if let Err(e) = db::expire_oauth_token(&state.db, c.kind.as_str(), c.upstream, key).await {
                        tracing::warn!(error = %e, "could not mark an OAuth token expired");
                    }
                }
            }
            _ => {}
        }
    }
}

/// Tokens in this replica's memory: client credentials ones, and keys' ones read from Postgres.
#[derive(Default)]
pub struct Tokens {
    client: std::sync::Mutex<HashMap<(Kind, Uuid), (String, Instant)>>,
    user: std::sync::Mutex<HashMap<(Kind, Uuid, Uuid), (String, Option<Instant>)>>,
}

/// A token is renewed this long before it expires, so one never expires on the way.
const EARLY: Duration = Duration::from_secs(60);
/// How long a key's token is used from memory before Postgres is asked again.
const USER_HOLD: Duration = Duration::from_secs(300);

fn fresh(expires: Option<Instant>) -> bool {
    expires.is_none_or(|e| e > Instant::now() + EARLY)
}

/// An authorization server's token response.
#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
    #[serde(default)]
    pub scope: Option<String>,
}

impl Tokens {
    async fn client_token(&self, state: &AppState, c: &Client) -> Result<String, String> {
        if let Some((token, expires)) = self.client.lock().expect("tokens lock").get(&(c.kind, c.upstream))
            && fresh(Some(*expires))
        {
            return Ok(token.clone());
        }
        let mut form = vec![("grant_type", "client_credentials".to_string())];
        if !c.config.scopes.is_empty() {
            form.push(("scope", c.config.scopes.join(" ")));
        }
        let t = token_request(state, c, form).await?;
        // Without an expiry, an hour: the upstream refusing it renews it sooner.
        let expires = Instant::now() + Duration::from_secs(t.expires_in.unwrap_or(3600));
        self.client.lock().expect("tokens lock").insert((c.kind, c.upstream), (t.access_token.clone(), expires));
        Ok(t.access_token)
    }

    async fn user_token(&self, state: &AppState, c: &Client, key: Uuid) -> Result<String, AuthError> {
        let slot = (c.kind, c.upstream, key);
        if let Some((token, expires)) = self.user.lock().expect("tokens lock").get(&slot)
            && fresh(*expires)
        {
            return Ok(token.clone());
        }
        let failed = |e: String| AuthError::Failed(e);
        let not_connected = || AuthError::NotConnected(c.name.clone());
        let sb = &state.cfg.secret_box;
        let open = |sealed: &[u8]| sb.open(sealed).and_then(|b| String::from_utf8(b).map_err(|_| "corrupt token".into())).map_err(failed);
        let row = db::get_oauth_token(&state.db, c.kind.as_str(), c.upstream, key).await.map_err(|e| failed(e.to_string()))?.ok_or_else(not_connected)?;
        let until = |at: Option<chrono::DateTime<Utc>>| at.map(|at| Instant::now() + (at - Utc::now()).to_std().unwrap_or_default());
        // Kept in memory for at most a few minutes, so a key signed out on another replica stops
        // working here too.
        let held = |e: Option<Instant>| {
            let cap = Instant::now() + USER_HOLD;
            Some(e.map_or(cap, |e| e.min(cap)))
        };
        if fresh(until(row.expires_at)) {
            let token = open(&row.access_token)?;
            self.user.lock().expect("tokens lock").insert(slot, (token.clone(), held(until(row.expires_at))));
            return Ok(token);
        }
        // Refreshed under the row's lock: another replica may be doing the same, and the
        // authorization server may give a new refresh token and retire the old one.
        let mut tx = state.db.begin().await.map_err(|e| failed(e.to_string()))?;
        let row = db::lock_oauth_token(&mut *tx, c.kind.as_str(), c.upstream, key).await.map_err(|e| failed(e.to_string()))?.ok_or_else(not_connected)?;
        if fresh(until(row.expires_at)) {
            let token = open(&row.access_token)?;
            self.user.lock().expect("tokens lock").insert(slot, (token.clone(), held(until(row.expires_at))));
            return Ok(token);
        }
        let refresh = row.refresh_token.as_deref().map(open).transpose()?.ok_or_else(not_connected)?;
        let mut form = vec![("grant_type", "refresh_token".to_string()), ("refresh_token", refresh.clone())];
        if let Some(r) = &c.config.resource {
            form.push(("resource", r.clone()));
        }
        let t = match token_request(state, c, form).await {
            Ok(t) => t,
            Err(e) => {
                // A refresh token the server no longer takes needs a new sign-in.
                if e.starts_with("invalid_grant") {
                    let _ = db::delete_oauth_token(&mut *tx, c.kind.as_str(), c.upstream, key).await;
                    let _ = tx.commit().await;
                    return Err(not_connected());
                }
                return Err(failed(e));
            }
        };
        let expires_at = t.expires_in.map(|s| Utc::now() + chrono::Duration::seconds(s as i64));
        let refresh = t.refresh_token.as_deref().unwrap_or(&refresh);
        db::update_oauth_token(&mut *tx, c.kind.as_str(), c.upstream, key, &sb.seal(t.access_token.as_bytes()), Some(&sb.seal(refresh.as_bytes())), expires_at)
            .await
            .map_err(|e| failed(e.to_string()))?;
        tx.commit().await.map_err(|e| failed(e.to_string()))?;
        self.user.lock().expect("tokens lock").insert(slot, (t.access_token.clone(), held(until(expires_at))));
        Ok(t.access_token)
    }
}

/// A request to the client's token URL, authenticated as its config says.
async fn token_request(state: &AppState, c: &Client, mut form: Vec<(&'static str, String)>) -> Result<TokenResponse, String> {
    let url = &c.config.token_url;
    state.cfg.upstream_policy.check_url(url).map_err(|why| format!("the token URL is not allowed: {why}"))?;
    let method = c.config.token_auth.as_deref().unwrap_or(if c.secret.is_some() { "client_secret_basic" } else { "none" });
    let mut req = state.http.post(url).header(header::ACCEPT, "application/json").timeout(Duration::from_secs(30));
    match (method, &c.secret) {
        ("client_secret_basic", Some(secret)) => {
            // RFC 6749 2.3.1: both parts form-encoded before they are joined.
            let enc = |s: &str| percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string();
            let basic = base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", enc(&c.config.client_id), enc(secret)));
            req = req.header(header::AUTHORIZATION, format!("Basic {basic}"));
        }
        ("client_secret_post", Some(secret)) => {
            form.push(("client_id", c.config.client_id.clone()));
            form.push(("client_secret", secret.clone()));
        }
        _ => form.push(("client_id", c.config.client_id.clone())),
    }
    if let Some(r) = &c.config.resource
        && !form.iter().any(|(k, _)| *k == "resource")
    {
        form.push(("resource", r.clone()));
    }
    let resp = req.form(&form).send().await.map_err(|e| GatewayError::from_reqwest(&e).message)?;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let error = body.get("error").and_then(Value::as_str).unwrap_or("error");
        let description = body.get("error_description").and_then(Value::as_str).map(|d| format!(": {d}")).unwrap_or_default();
        return Err(format!("{error}{description} (HTTP {})", status.as_u16()));
    }
    serde_json::from_value(body).map_err(|_| "the token response has no access_token".to_string())
}

// ---------------------------------------------------------------- signing keys in

/// A random string for PKCE and `state`: 32 bytes, base64url.
pub fn random_token() -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(crate::util::random_bytes::<32>())
}

/// The PKCE S256 challenge for a verifier.
pub fn challenge(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(crate::util::sha256(verifier.as_bytes()))
}

impl Client {
    /// Where to send the person signing a key in.
    pub fn authorization_url(&self, redirect_uri: &str, state: &str, verifier: &str) -> Result<String, String> {
        let base = self.config.authorization_url.as_deref().ok_or("no authorization URL is set")?;
        let mut url = reqwest::Url::parse(base).map_err(|_| "the authorization URL is not valid")?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("response_type", "code")
                .append_pair("client_id", &self.config.client_id)
                .append_pair("redirect_uri", redirect_uri)
                .append_pair("state", state)
                .append_pair("code_challenge", &challenge(verifier))
                .append_pair("code_challenge_method", "S256");
            if !self.config.scopes.is_empty() {
                q.append_pair("scope", &self.config.scopes.join(" "));
            }
            if let Some(r) = &self.config.resource {
                q.append_pair("resource", r);
            }
        }
        Ok(url.into())
    }

    /// Exchanges an authorization code, and stores the key's tokens.
    pub async fn complete(&self, state: &AppState, key: Uuid, code: &str, verifier: &str, redirect_uri: &str, by: &str) -> Result<(), String> {
        let form = vec![
            ("grant_type", "authorization_code".to_string()),
            ("code", code.to_string()),
            ("redirect_uri", redirect_uri.to_string()),
            ("code_verifier", verifier.to_string()),
        ];
        let t = token_request(state, self, form).await?;
        let sb = &state.cfg.secret_box;
        let expires_at = t.expires_in.map(|s| Utc::now() + chrono::Duration::seconds(s as i64));
        let refresh = t.refresh_token.as_deref().map(|r| sb.seal(r.as_bytes()));
        db::save_oauth_token(&state.db, self.kind.as_str(), self.upstream, key, &sb.seal(t.access_token.as_bytes()), refresh.as_deref(), expires_at, t.scope.as_deref(), by)
            .await
            .map_err(|e| e.to_string())?;
        state.oauth.user.lock().expect("tokens lock").remove(&(self.kind, self.upstream, key));
        Ok(())
    }
}

/// Forgets a key's tokens for an upstream, in this replica's memory.
pub fn forget(state: &AppState, kind: Kind, upstream: Uuid, key: Uuid) {
    state.oauth.user.lock().expect("tokens lock").remove(&(kind, upstream, key));
}

// ---------------------------------------------------------------- discovery

/// What discovery found out about an upstream's authorization server.
#[derive(Debug, Default, Serialize, utoipa::ToSchema)]
pub struct Discovered {
    pub token_url: Option<String>,
    pub authorization_url: Option<String>,
    pub registration_url: Option<String>,
    /// Scopes the upstream or its authorization server lists.
    pub scopes: Vec<String>,
    /// The resource tokens are for, as the upstream names itself.
    pub resource: Option<String>,
    /// Ways the token URL takes clients to authenticate.
    pub token_auth_methods: Vec<String>,
    /// Whether the authorization server takes client ID metadata documents as client IDs.
    pub client_metadata_documents: bool,
    /// The gateway's client there, when discovery got it one (see `client`).
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub redirect_uri: Option<String>,
    /// How the gateway got `client_id`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client: Option<ClientSource>,
}

/// How discovery got the gateway a client at an authorization server.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClientSource {
    /// Its client ID is the URL of the gateway's client ID metadata document, which the server
    /// fetches when a key signs in. Nothing is registered.
    MetadataDocument,
    /// Dynamic client registration (RFC 7591).
    Registered,
}

/// A small JSON document from an address an upstream gave.
async fn get_json(state: &AppState, url: &str) -> Result<Value, String> {
    state.cfg.upstream_policy.check_url(url).map_err(|why| format!("{url} is not allowed: {why}"))?;
    let resp = state
        .http
        .get(url)
        .header(header::ACCEPT, "application/json")
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("{url}: {}", GatewayError::from_reqwest(&e).message))?;
    if !resp.status().is_success() {
        return Err(format!("{url} answered HTTP {}", resp.status().as_u16()));
    }
    let bytes = resp.bytes().await.map_err(|e| format!("{url}: {}", GatewayError::from_reqwest(&e).message))?;
    if bytes.len() > 1 << 20 {
        return Err(format!("{url}: the document is too large"));
    }
    serde_json::from_slice(&bytes).map_err(|_| format!("{url} is not JSON"))
}

/// `/.well-known/{name}` for `url`, with `url`'s path after it (RFC 8414 and 9728), then without.
fn well_known(url: &str, name: &str) -> Vec<String> {
    let Ok(u) = reqwest::Url::parse(url) else { return Vec::new() };
    let origin = u.origin().ascii_serialization();
    let path = u.path().trim_end_matches('/');
    let mut out = Vec::new();
    if !path.is_empty() {
        out.push(format!("{origin}/.well-known/{name}{path}"));
    }
    out.push(format!("{origin}/.well-known/{name}"));
    out
}

/// The `resource_metadata` an MCP server names in its `WWW-Authenticate` answer to a client
/// without a token (RFC 9728 5.1).
async fn resource_metadata_hint(state: &AppState, url: &str) -> Option<String> {
    state.cfg.upstream_policy.check_url(url).ok()?;
    let resp = state
        .http
        .post(url)
        .header(header::ACCEPT, "application/json, text/event-stream")
        .json(&json!({ "jsonrpc": "2.0", "id": 0, "method": "ping" }))
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .ok()?;
    let challenge = resp.headers().get(header::WWW_AUTHENTICATE)?.to_str().ok()?;
    let start = challenge.find("resource_metadata=\"")? + "resource_metadata=\"".len();
    Some(challenge[start..].split('"').next()?.to_string())
}

/// Fills in what an authorization server's metadata says.
async fn server_metadata(state: &AppState, issuer: &str, found: &mut Discovered) -> Result<(), String> {
    let mut candidates = well_known(issuer, "oauth-authorization-server");
    candidates.extend(well_known(issuer, "openid-configuration"));
    candidates.push(format!("{}/.well-known/openid-configuration", issuer.trim_end_matches('/')));
    let mut last = format!("no metadata found for {issuer}");
    for url in candidates {
        match get_json(state, &url).await {
            Ok(m) => {
                let s = |k: &str| m.get(k).and_then(Value::as_str).map(str::to_string);
                let list = |k: &str| m.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default();
                found.token_url = found.token_url.take().or(s("token_endpoint"));
                found.authorization_url = found.authorization_url.take().or(s("authorization_endpoint"));
                found.registration_url = s("registration_endpoint");
                found.token_auth_methods = list("token_endpoint_auth_methods_supported");
                found.client_metadata_documents = m.get("client_id_metadata_document_supported").and_then(Value::as_bool).unwrap_or(false);
                if found.scopes.is_empty() {
                    found.scopes = list("scopes_supported");
                }
                return Ok(());
            }
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// Finds an upstream's authorization server: for an MCP server from its protected resource
/// metadata, for an A2A agent from the OAuth 2 scheme on its card. Then gets the gateway a public
/// client with `redirect_uri` there: `metadata_document` (the URL of the gateway's client ID
/// metadata document) as its client ID where the server takes one, else, with `register`, a client
/// registered where the server offers registration.
pub async fn discover(
    state: &AppState,
    kind: Kind,
    url: &str,
    metadata_document: Option<&str>,
    register: bool,
    redirect_uri: &str,
) -> Result<Discovered, String> {
    let mut found = Discovered::default();
    let issuer = match kind {
        Kind::Mcp => {
            let mut candidates: Vec<String> = resource_metadata_hint(state, url).await.into_iter().collect();
            candidates.extend(well_known(url, "oauth-protected-resource"));
            let mut metadata = None;
            for c in candidates {
                if let Ok(m) = get_json(state, &c).await {
                    metadata = Some(m);
                    break;
                }
            }
            let m = metadata.ok_or("the server publishes no protected resource metadata (RFC 9728); enter its token and authorization URLs")?;
            found.resource = m.get("resource").and_then(Value::as_str).map(str::to_string).or_else(|| Some(url.to_string()));
            found.scopes = m.get("scopes_supported").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default();
            m.pointer("/authorization_servers/0").and_then(Value::as_str).map(str::to_string).ok_or("the metadata names no authorization server")?
        }
        Kind::A2a => {
            let card = get_json(state, url).await?;
            let schemes = card.get("securitySchemes").and_then(Value::as_object).ok_or("the agent card names no security schemes")?;
            // 1.0: `{"oauth2SecurityScheme": {...}}`; 0.3: `{"type": "oauth2", ...}`.
            let scheme = schemes
                .values()
                .find_map(|s| s.get("oauth2SecurityScheme").cloned().or_else(|| (s.get("type").and_then(Value::as_str) == Some("oauth2")).then(|| s.clone())))
                .ok_or("the agent card names no OAuth 2 scheme")?;
            let flows = scheme.get("flows").cloned().unwrap_or(Value::Null);
            let flow = |k: &str| flows.get(k).cloned().unwrap_or(Value::Null);
            let code = flow("authorizationCode");
            let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
            found.authorization_url = s(&code, "authorizationUrl");
            found.token_url = s(&code, "tokenUrl").or_else(|| s(&flow("clientCredentials"), "tokenUrl"));
            let scopes = code.get("scopes").or_else(|| flows.get("clientCredentials").and_then(|f| f.get("scopes")));
            found.scopes = scopes.and_then(Value::as_object).map(|o| o.keys().cloned().collect()).unwrap_or_default();
            // The scheme may point at its authorization server's metadata, for registration.
            if let Some(m) = s(&scheme, "oauth2MetadataUrl") {
                let m = get_json(state, &m).await?;
                found.registration_url = s(&m, "registration_endpoint");
                found.token_auth_methods = m
                    .get("token_endpoint_auth_methods_supported")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                    .unwrap_or_default();
                found.client_metadata_documents = m.get("client_id_metadata_document_supported").and_then(Value::as_bool).unwrap_or(false);
                found.token_url = found.token_url.take().or(s(&m, "token_endpoint"));
                found.authorization_url = found.authorization_url.take().or(s(&m, "authorization_endpoint"));
            }
            String::new()
        }
    };
    if !issuer.is_empty() {
        server_metadata(state, &issuer, &mut found).await?;
    }
    if found.token_url.is_none() {
        return Err("no token URL found".into());
    }
    if let Some(doc) = metadata_document
        && found.client_metadata_documents
    {
        found.client_id = Some(doc.to_string());
        found.redirect_uri = Some(redirect_uri.to_string());
        found.client = Some(ClientSource::MetadataDocument);
    } else if register && let Some(reg) = found.registration_url.clone() {
        state.cfg.upstream_policy.check_url(&reg).map_err(|why| format!("the registration URL is not allowed: {why}"))?;
        let body = json!({
            "client_name": "Inference Gateway",
            "redirect_uris": [redirect_uri],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
        });
        let resp = state.http.post(&reg).json(&body).timeout(Duration::from_secs(10)).send().await.map_err(|e| format!("registration: {}", GatewayError::from_reqwest(&e).message))?;
        let status = resp.status();
        let v: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            return Err(format!("registration was refused (HTTP {}): {}", status.as_u16(), v.get("error_description").or(v.get("error")).and_then(Value::as_str).unwrap_or("no details")));
        }
        found.client_id = v.get("client_id").and_then(Value::as_str).map(str::to_string);
        found.client_secret = v.get("client_secret").and_then(Value::as_str).map(str::to_string);
        found.redirect_uri = Some(redirect_uri.to_string());
        found.client = Some(ClientSource::Registered);
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenges_are_the_verifiers_sha256_in_base64url() {
        // As Node's crypto computes it: createHash("sha256").update(v).digest("base64url").
        assert_eq!(challenge("dBjftJeZ4CVP-mJ92ZPw7S6aZoB8HMPVaZv9NFoqfGo"), "U196Xi5E-ltsIa0AaeuhHk_Hw-BwcfYgebIxEx8MIfk");
        assert_eq!(random_token().len(), 43);
    }

    /// Client credentials: fetched once with Basic auth and the resource, used until renewed.
    #[tokio::test]
    async fn client_credentials_are_fetched_once_and_renewed_when_refused() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use axum::{Json, routing::post};
        static ISSUED: AtomicUsize = AtomicUsize::new(0);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let token_url = format!("http://{}/token", listener.local_addr().unwrap());
        let app = axum::Router::new().route(
            "/token",
            post(|headers: axum::http::HeaderMap, body: String| async move {
                let f: HashMap<String, String> = reqwest::Url::parse(&format!("http://x/?{body}")).unwrap().query_pairs().into_owned().collect();
                // "gw:s3cret" in Basic auth, form-encoded first.
                assert_eq!(headers.get(header::AUTHORIZATION).unwrap(), "Basic Z3c6czNjcmV0");
                assert_eq!((f["grant_type"].as_str(), f["resource"].as_str(), f["scope"].as_str()), ("client_credentials", "https://mcp.example/mcp", "a b"));
                let n = ISSUED.fetch_add(1, Ordering::SeqCst);
                Json(json!({ "access_token": format!("token-{n}"), "expires_in": 3600 }))
            }),
        );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let (state, _) = crate::state::test_state_local();
        let config = OAuthConfig {
            token_url,
            client_id: "gw".into(),
            scopes: vec!["a".into(), "b".into()],
            resource: Some("https://mcp.example/mcp".into()),
            ..Default::default()
        };
        let auth = UpstreamAuth::Client(Arc::new(Client { kind: Kind::Mcp, upstream: Uuid::now_v7(), name: "x".into(), config, secret: Some("s3cret".into()) }));
        let header = |h: Option<(HeaderName, HeaderValue)>| h.unwrap().1.to_str().unwrap().to_string();
        assert_eq!(header(auth.header(&state, None).await.unwrap()), "Bearer token-0");
        assert_eq!(header(auth.header(&state, None).await.unwrap()), "Bearer token-0");
        auth.renew(&state, None).await;
        assert_eq!(header(auth.header(&state, None).await.unwrap()), "Bearer token-1");
        // Keys that sign in on their own need a key.
        let per_key = UpstreamAuth::PerKey(Arc::new(Client { kind: Kind::Mcp, upstream: Uuid::nil(), name: "y".into(), config: OAuthConfig::default(), secret: None }));
        assert!(matches!(per_key.header(&state, None).await, Err(AuthError::NotConnected(_))));
    }

    #[test]
    fn well_known_addresses_put_the_path_after_the_name() {
        assert_eq!(
            well_known("https://auth.example.com/tenant/x", "oauth-authorization-server"),
            ["https://auth.example.com/.well-known/oauth-authorization-server/tenant/x", "https://auth.example.com/.well-known/oauth-authorization-server"]
        );
        assert_eq!(well_known("https://auth.example.com/", "openid-configuration"), ["https://auth.example.com/.well-known/openid-configuration"]);
    }

    #[test]
    fn authorization_urls_carry_pkce_and_the_resource() {
        let c = Client {
            kind: Kind::Mcp,
            upstream: Uuid::nil(),
            name: "x".into(),
            config: OAuthConfig {
                authorization_url: Some("https://auth.example.com/authorize?tenant=a".into()),
                client_id: "gw".into(),
                scopes: vec!["read".into(), "write".into()],
                resource: Some("https://mcp.example.com/mcp".into()),
                ..Default::default()
            },
            secret: None,
        };
        let url = reqwest::Url::parse(&c.authorization_url("https://gw/cb", "st", "verifier").unwrap()).unwrap();
        let q: HashMap<String, String> = url.query_pairs().into_owned().collect();
        assert_eq!(q["tenant"], "a");
        assert_eq!(q["code_challenge"], challenge("verifier"));
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["scope"], "read write");
        assert_eq!(q["resource"], "https://mcp.example.com/mcp");
        assert_eq!(q["redirect_uri"], "https://gw/cb");
    }
}
