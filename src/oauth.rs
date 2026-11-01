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
//!
//! The other way round, the gateway as the authorization server of its own MCP endpoints, is
//! `server`.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use base64::Engine;
use chrono::Utc;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    crypto::SecretBox,
    db,
    error::{ErrorKind, GatewayError},
    labels::Labels,
    mcp::McpAuth,
    state::AppState,
    util::TtlCache,
};

pub mod server;

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
    /// For `oauth_user`: the request labels that tell a key's sign-ins apart, `tenant` or `tenant`
    /// and `user`. Empty: the key signs in once.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sign_in_by: Vec<String>,
    /// For `oauth_user`: how many days a sign-in is kept unused before it is deleted, 1 to 90;
    /// `SIGN_IN_RETENTION_DAYS` when not set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sign_in_retention_days: Option<u32>,
}

/// How many days a sign-in is kept unused, when its upstream does not say.
pub const SIGN_IN_RETENTION_DAYS: u32 = 7;
/// The range an upstream may keep unused sign-ins for, in days.
pub const SIGN_IN_RETENTION_RANGE: std::ops::RangeInclusive<u32> = 1..=90;

/// The label keys `sign_in_by` may hold, in this order.
pub const SIGN_IN_LABELS: [&str; 2] = ["tenant", "user"];

/// Whether `by` is a `sign_in_by` the gateway takes: nothing, `tenant`, or `tenant` and `user`.
pub fn valid_sign_in_by(by: &[String]) -> bool {
    by.len() <= SIGN_IN_LABELS.len() && by.iter().zip(SIGN_IN_LABELS).all(|(a, b)| a == b)
}

/// Who calls an upstream: the key, and the request's labels.
#[derive(Debug, Clone, Copy)]
pub struct Who<'a> {
    pub key: Uuid,
    pub labels: &'a Labels,
}

/// Where a sign-in is kept: the key, and the labels the upstream tells the key's sign-ins apart by.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Slot {
    pub key: Uuid,
    /// `key=value` pairs sorted by key and joined with commas, such as `tenant=t1,user=u1`; empty
    /// when the key signs in once.
    pub labels: String,
}

impl Slot {
    /// The whole key's sign-in.
    pub fn key(key: Uuid) -> Self {
        Slot { key, labels: String::new() }
    }
}

/// The labels named in `by`, as a `Slot` keeps them; `Err` with the first one the request lacks.
pub fn labels_for(by: &[String], labels: &Labels) -> Result<String, String> {
    let mut by: Vec<&str> = by.iter().map(String::as_str).collect();
    by.sort_unstable();
    let pairs = by.iter().map(|k| labels.get(k).map(|v| format!("{k}={v}")).ok_or_else(|| k.to_string())).collect::<Result<Vec<_>, _>>()?;
    Ok(pairs.join(","))
}

/// Which kind of upstream a token is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
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
    /// The caller has not signed in to the upstream (or its sign-in expired): the key, or the key
    /// with `labels` where the upstream tells sign-ins apart by labels. `link` signs them in.
    NotConnected {
        name: String,
        labels: String,
        link: Option<String>,
    },
    /// The upstream tells sign-ins apart by a label the request does not have.
    MissingLabel {
        name: String,
        label: String,
    },
    Failed(String),
}

impl AuthError {
    /// Whether the caller has no sign-in for the upstream, rather than the sign-in failing.
    pub fn not_signed_in(&self) -> bool {
        matches!(self, AuthError::NotConnected { .. } | AuthError::MissingLabel { .. })
    }

    pub fn message(&self) -> String {
        match self {
            AuthError::NotConnected { name, labels, .. } if labels.is_empty() => {
                format!("this key has not signed in to '{name}'; sign it in on the console's Access page (Connections)")
            }
            AuthError::NotConnected { name, labels, link: Some(link) } => {
                format!("'{name}' needs a sign-in for {labels}: open {link} to sign in (the link works for 15 minutes)")
            }
            AuthError::NotConnected { name, labels, link: None } => {
                format!("'{name}' needs a sign-in for {labels}; get a sign-in link from POST /v1/oauth/sign-ins")
            }
            AuthError::MissingLabel { name, label } => {
                format!("'{name}' keeps a sign-in per {label}; send the request with the label {label} in {}", crate::labels::HEADER)
            }
            AuthError::Failed(e) => e.clone(),
        }
    }

    pub fn into_gateway(self) -> GatewayError {
        match self {
            AuthError::NotConnected { .. } => GatewayError { code: Some("sign_in_required"), ..GatewayError::forbidden(self.message()) },
            AuthError::MissingLabel { .. } => GatewayError::invalid(self.message()),
            AuthError::Failed(e) => GatewayError {
                upstream: true,
                ..GatewayError::new(ErrorKind::Upstream, StatusCode::BAD_GATEWAY, format!("the upstream's sign-in failed: {e}"))
            },
        }
    }
}

fn sensitive(v: &str) -> Option<HeaderValue> {
    let mut v = HeaderValue::from_str(v).ok()?;
    v.set_sensitive(true);
    Some(v)
}

fn bearer(token: &str) -> Result<(HeaderName, HeaderValue), AuthError> {
    sensitive(&format!("Bearer {token}"))
        .map(|v| (header::AUTHORIZATION, v))
        .ok_or_else(|| AuthError::Failed("the token is not a valid header value".into()))
}

impl UpstreamAuth {
    /// From an upstream's stored `auth`, header name, sealed secret and OAuth client.
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
                    McpAuth::Bearer => {
                        UpstreamAuth::Static(header::AUTHORIZATION, sensitive(&format!("Bearer {secret}")).ok_or_else(invalid)?)
                    }
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

    /// The labels that tell a key's sign-ins apart; empty where the key signs in once, or not at all.
    pub fn sign_in_by(&self) -> &[String] {
        match self {
            UpstreamAuth::PerKey(c) => &c.config.sign_in_by,
            _ => &[],
        }
    }

    /// Where `who`'s sign-in is kept, for an upstream whose keys sign in on their own.
    pub fn slot(&self, who: Who) -> Result<Option<Slot>, AuthError> {
        match self {
            UpstreamAuth::PerKey(c) => c.slot(who).map(Some),
            _ => Ok(None),
        }
    }

    /// The header to call the upstream with, for `who` (needed for `oauth_user`).
    pub async fn header(&self, state: &AppState, who: Option<Who<'_>>) -> Result<Option<(HeaderName, HeaderValue)>, AuthError> {
        match self {
            UpstreamAuth::None => Ok(None),
            UpstreamAuth::Static(h, v) => Ok(Some((h.clone(), v.clone()))),
            UpstreamAuth::Client(c) => bearer(&state.oauth.client_token(state, c).await.map_err(AuthError::Failed)?).map(Some),
            UpstreamAuth::PerKey(c) => {
                let who = who.ok_or_else(|| AuthError::NotConnected { name: c.name.clone(), labels: String::new(), link: None })?;
                let slot = c.slot(who)?;
                let token = state.oauth.user_token(state, c, &slot).await?;
                state.oauth.used(state, c, slot);
                bearer(&token).map(Some)
            }
        }
    }

    /// The upstream refused the token: get another one next time.
    pub async fn renew(&self, state: &AppState, who: Option<Who<'_>>) {
        match self {
            UpstreamAuth::Client(c) => {
                state.oauth.client.remove(&(c.kind, c.upstream));
            }
            UpstreamAuth::PerKey(c) => {
                if let Some(slot) = who.and_then(|w| c.slot(w).ok()) {
                    if let Err(e) = db::expire_oauth_token(&state.db, c.sign_in(&slot)).await {
                        tracing::warn!(error = %e, "could not mark an OAuth token expired");
                    }
                    state.oauth.user.remove(&(c.kind, c.upstream, slot));
                }
            }
            _ => {}
        }
    }
}

impl Client {
    /// Where `who`'s sign-in is kept; refused when the request lacks a label the upstream tells
    /// sign-ins apart by.
    pub fn slot(&self, who: Who) -> Result<Slot, AuthError> {
        let labels =
            labels_for(&self.config.sign_in_by, who.labels).map_err(|label| AuthError::MissingLabel { name: self.name.clone(), label })?;
        Ok(Slot { key: who.key, labels })
    }

    /// The sign-in's row.
    pub fn sign_in<'a>(&self, slot: &'a Slot) -> db::SignInRef<'a> {
        db::SignInRef { kind: self.kind.as_str(), upstream: self.upstream, key: slot.key, labels: &slot.labels }
    }

    /// No sign-in for `slot`: with a link to sign in where the upstream tells sign-ins apart by labels.
    fn not_connected(&self, state: &AppState, slot: &Slot) -> AuthError {
        let link =
            (!slot.labels.is_empty()).then(|| self.link_origin().map(|o| Ticket::new(self, slot).url(&o, &state.cfg.secret_box))).flatten();
        AuthError::NotConnected { name: self.name.clone(), labels: slot.labels.clone(), link }
    }

    /// The gateway's address as the upstream's sign-in sends people back to it, for links in
    /// errors; requests to the API do not tell how browsers reach the gateway.
    fn link_origin(&self) -> Option<String> {
        let url = reqwest::Url::parse(self.config.redirect_uri.as_deref()?).ok()?;
        Some(url.origin().ascii_serialization()).filter(|o| o != "null")
    }
}

/// A link that signs one tenant or user in to an upstream, for the application to show them: the
/// sign-in sealed with the gateway's encryption key, so it needs no table and cannot be altered.
/// It completes one sign-in: its nonce is claimed in Valkey then (see `Ticket::used_key`), so a
/// link seen by someone else cannot sign their account in to the slot afterwards.
#[derive(Debug, Serialize, Deserialize)]
pub struct Ticket {
    pub kind: Kind,
    pub upstream: Uuid,
    #[serde(flatten)]
    pub slot: Slot,
    /// Unix seconds.
    pub expires: i64,
    pub nonce: String,
}

/// Where sign-in links go, after the gateway's address, followed by the ticket.
pub const SIGN_IN_PATH: &str = "/admin/auth/oauth/sign-in";
/// How long a sign-in link works.
pub const TICKET_TTL_SECS: i64 = 900;

impl Ticket {
    pub fn new(c: &Client, slot: &Slot) -> Self {
        Ticket {
            kind: c.kind,
            upstream: c.upstream,
            slot: slot.clone(),
            expires: Utc::now().timestamp() + TICKET_TTL_SECS,
            nonce: random_token(),
        }
    }

    /// The Valkey key that marks a ticket's nonce used.
    pub fn used_key(nonce: &str) -> String {
        format!("gw:signin-link:{nonce}")
    }

    pub fn url(&self, origin: &str, sb: &SecretBox) -> String {
        let sealed = sb.seal(&serde_json::to_vec(self).expect("a ticket serializes"));
        format!("{origin}{SIGN_IN_PATH}/{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sealed))
    }

    /// The ticket in a link, if the gateway sealed it and it has not expired.
    pub fn open(s: &str, sb: &SecretBox) -> Option<Self> {
        let sealed = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s).ok()?;
        let t: Ticket = serde_json::from_slice(&sb.open(&sealed).ok()?).ok()?;
        (t.expires > Utc::now().timestamp()).then_some(t)
    }
}

/// Tokens in this replica's memory: client credentials ones, and sign-ins' ones read from Postgres.
/// Each token is kept until `EARLY` before it expires, so one never expires on the way.
pub struct Tokens {
    client: TtlCache<(Kind, Uuid), String>,
    user: TtlCache<(Kind, Uuid, Slot), String>,
    /// The sign-ins whose use this replica recorded in Postgres within `USED_EVERY`.
    used: TtlCache<(Kind, Uuid, Slot), ()>,
}

impl Default for Tokens {
    fn default() -> Self {
        Self {
            client: TtlCache::new(USER_HOLD, MEMORY_PRUNE_AT),
            user: TtlCache::new(USER_HOLD, MEMORY_PRUNE_AT),
            used: TtlCache::new(USED_EVERY, MEMORY_PRUNE_AT),
        }
    }
}

/// A sign-in's use is recorded at most this often, so calls do not each write to Postgres.
const USED_EVERY: Duration = Duration::from_secs(3600);
/// Tokens and sign-ins kept in memory at most, of each kind.
const MEMORY_PRUNE_AT: usize = 1000;

/// A token is renewed this long before it expires, so one never expires on the way.
const EARLY: Duration = Duration::from_secs(60);
/// How long a key's token is used from memory before Postgres is asked again.
const USER_HOLD: Duration = Duration::from_secs(300);

fn fresh(expires: Option<Instant>) -> bool {
    expires.is_none_or(|e| e > Instant::now() + EARLY)
}

/// Until when a token that expires at `expires` may be used from memory.
fn usable_until(expires: Instant) -> Instant {
    expires.checked_sub(EARLY).unwrap_or(expires)
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

/// A token is kept this long at most, whatever its server says: an `expires_in` of years would
/// keep a dead token (and an absurd one would overflow the clock).
const MAX_TOKEN_LIFE: u64 = 30 * 24 * 3600;
/// A document or token response from an upstream is read up to this size.
const MAX_RESPONSE: usize = 1 << 20;

/// How long a token lives, as its server says and within `MAX_TOKEN_LIFE`.
fn lifetime(expires_in: Option<u64>) -> Option<Duration> {
    expires_in.map(|s| Duration::from_secs(s.min(MAX_TOKEN_LIFE)))
}

/// When a token expires, as its server says and within `MAX_TOKEN_LIFE`.
fn expiry(expires_in: Option<u64>) -> Option<chrono::DateTime<Utc>> {
    lifetime(expires_in).map(|d| Utc::now() + chrono::Duration::from_std(d).expect("bounded"))
}

/// The response's body, read up to `max` bytes; a larger one is an error rather than memory.
pub(crate) async fn read_body(resp: reqwest::Response, max: usize) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| GatewayError::from_reqwest(&e).message)?;
        if body.len() + chunk.len() > max {
            return Err(format!("the answer is larger than {} KB", max / 1024));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// A JSON body, read within `MAX_RESPONSE`; `Null` when it is not JSON or too large.
async fn json_body(resp: reqwest::Response) -> Value {
    read_body(resp, MAX_RESPONSE).await.ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null)
}

impl Tokens {
    async fn client_token(&self, state: &AppState, c: &Client) -> Result<String, String> {
        if let Some(token) = self.client.get(&(c.kind, c.upstream)) {
            return Ok(token);
        }
        let mut form = vec![("grant_type", "client_credentials".to_string())];
        if !c.config.scopes.is_empty() {
            form.push(("scope", c.config.scopes.join(" ")));
        }
        let t = token_request(state, c, form).await?;
        // Without an expiry, an hour: the upstream refusing it renews it sooner.
        let expires = Instant::now() + lifetime(t.expires_in).unwrap_or(Duration::from_secs(3600));
        self.client.insert_until((c.kind, c.upstream), t.access_token.clone(), usable_until(expires));
        Ok(t.access_token)
    }

    /// Keeps a sign-in's token in memory until `until`. With many sign-ins (one per user), those
    /// held past their time are let go first, so memory does not grow without end.
    fn hold(&self, slot: (Kind, Uuid, Slot), token: String, until: Instant) {
        self.user.insert_until(slot, token, usable_until(until));
    }

    /// Records that the sign-in was used, at most once in `USED_EVERY`, without waiting for it.
    fn used(&self, state: &AppState, c: &Client, s: Slot) {
        let slot = (c.kind, c.upstream, s);
        if self.used.get(&slot).is_some() {
            return;
        }
        self.used.insert(slot.clone(), ());
        let state = state.clone();
        tokio::spawn(async move {
            let (kind, upstream, s) = slot;
            let r = db::SignInRef { kind: kind.as_str(), upstream, key: s.key, labels: &s.labels };
            if let Err(e) = db::touch_oauth_token(&state.db, r).await {
                tracing::warn!(error = %e, "could not record a sign-in's use");
            }
        });
    }

    async fn user_token(&self, state: &AppState, c: &Client, s: &Slot) -> Result<String, AuthError> {
        let slot = (c.kind, c.upstream, s.clone());
        if let Some(token) = self.user.get(&slot) {
            return Ok(token);
        }
        let failed = |e: String| AuthError::Failed(e);
        let not_connected = || c.not_connected(state, s);
        let sb = &state.cfg.secret_box;
        let open = |sealed: &[u8]| sb.open(sealed).and_then(|b| String::from_utf8(b).map_err(|_| "corrupt token".into())).map_err(failed);
        let row = db::get_oauth_token(&state.db, c.sign_in(s)).await.map_err(|e| failed(e.to_string()))?.ok_or_else(not_connected)?;
        let until = |at: Option<chrono::DateTime<Utc>>| at.map(|at| Instant::now() + (at - Utc::now()).to_std().unwrap_or_default());
        // Kept in memory for at most a few minutes, so a key signed out on another replica stops
        // working here too.
        let held = |e: Option<Instant>| {
            let cap = Instant::now() + USER_HOLD;
            e.map_or(cap, |e| e.min(cap))
        };
        if fresh(until(row.expires_at)) {
            let token = open(&row.access_token)?;
            self.hold(slot, token.clone(), held(until(row.expires_at)));
            return Ok(token);
        }
        // Refreshed under the row's lock: another replica may be doing the same, and the
        // authorization server may give a new refresh token and retire the old one.
        let mut tx = state.db.begin().await.map_err(|e| failed(e.to_string()))?;
        let row = db::lock_oauth_token(&mut *tx, c.sign_in(s)).await.map_err(|e| failed(e.to_string()))?.ok_or_else(not_connected)?;
        if fresh(until(row.expires_at)) {
            let token = open(&row.access_token)?;
            self.hold(slot, token.clone(), held(until(row.expires_at)));
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
                    let _ = db::delete_oauth_token(&mut *tx, c.sign_in(s)).await;
                    let _ = tx.commit().await;
                    return Err(not_connected());
                }
                return Err(failed(e));
            }
        };
        let expires_at = expiry(t.expires_in);
        let refresh = t.refresh_token.as_deref().unwrap_or(&refresh);
        db::update_oauth_token(&mut *tx, c.sign_in(s), &sb.seal(t.access_token.as_bytes()), Some(&sb.seal(refresh.as_bytes())), expires_at)
            .await
            .map_err(|e| failed(e.to_string()))?;
        tx.commit().await.map_err(|e| failed(e.to_string()))?;
        self.hold(slot, t.access_token.clone(), held(until(expires_at)));
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
    let body = json_body(resp).await;
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

    /// Exchanges an authorization code, and stores the sign-in's tokens.
    pub async fn complete(
        &self,
        state: &AppState,
        slot: &Slot,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
        by: &str,
    ) -> Result<(), String> {
        let form = vec![
            ("grant_type", "authorization_code".to_string()),
            ("code", code.to_string()),
            ("redirect_uri", redirect_uri.to_string()),
            ("code_verifier", verifier.to_string()),
        ];
        let t = token_request(state, self, form).await?;
        let sb = &state.cfg.secret_box;
        let expires_at = expiry(t.expires_in);
        let refresh = t.refresh_token.as_deref().map(|r| sb.seal(r.as_bytes()));
        db::save_oauth_token(
            &state.db,
            self.sign_in(slot),
            &sb.seal(t.access_token.as_bytes()),
            refresh.as_deref(),
            expires_at,
            t.scope.as_deref(),
            by,
        )
        .await
        .map_err(|e| e.to_string())?;
        state.oauth.user.remove(&(self.kind, self.upstream, slot.clone()));
        Ok(())
    }
}

/// Forgets a sign-in's tokens for an upstream, in this replica's memory.
pub fn forget(state: &AppState, kind: Kind, upstream: Uuid, slot: &Slot) {
    state.oauth.user.remove(&(kind, upstream, slot.clone()));
}

/// Forgets every sign-in's tokens for an upstream, in this replica's memory.
pub fn forget_upstream(state: &AppState, kind: Kind, upstream: Uuid) {
    state.oauth.user.retain(|(k, u, _), _| (*k, *u) != (kind, upstream));
}

// ---------------------------------------------------------------- discovery

/// What discovery found out about an upstream's authorization server.
#[derive(Debug, Default, Serialize, utoipa::ToSchema)]
pub struct Discovered {
    #[schema(required)]
    pub token_url: Option<String>,
    #[schema(required)]
    pub authorization_url: Option<String>,
    #[schema(required)]
    pub registration_url: Option<String>,
    /// Scopes the upstream or its authorization server lists.
    pub scopes: Vec<String>,
    /// The resource tokens are for, as the upstream names itself.
    #[schema(required)]
    pub resource: Option<String>,
    /// Ways the token URL takes clients to authenticate.
    pub token_auth_methods: Vec<String>,
    /// Whether the authorization server takes client ID metadata documents as client IDs.
    pub client_metadata_documents: bool,
    /// The gateway's client there, when discovery got it one (see `client`).
    #[schema(required)]
    pub client_id: Option<String>,
    #[schema(required)]
    pub client_secret: Option<String>,
    #[schema(required)]
    pub redirect_uri: Option<String>,
    /// How the gateway got `client_id`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
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
    let bytes = read_body(resp, MAX_RESPONSE).await.map_err(|e| format!("{url}: {e}"))?;
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
                let list = |k: &str| {
                    m.get(k)
                        .and_then(Value::as_array)
                        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                        .unwrap_or_default()
                };
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
            let m =
                metadata.ok_or("the server publishes no protected resource metadata (RFC 9728); enter its token and authorization URLs")?;
            found.resource = m.get("resource").and_then(Value::as_str).map(str::to_string).or_else(|| Some(url.to_string()));
            found.scopes = m
                .get("scopes_supported")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default();
            m.pointer("/authorization_servers/0")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or("the metadata names no authorization server")?
        }
        Kind::A2a => {
            let card = get_json(state, url).await?;
            let schemes = card.get("securitySchemes").and_then(Value::as_object).ok_or("the agent card names no security schemes")?;
            // 1.0: `{"oauth2SecurityScheme": {...}}`; 0.3: `{"type": "oauth2", ...}`.
            let scheme = schemes
                .values()
                .find_map(|s| {
                    s.get("oauth2SecurityScheme")
                        .cloned()
                        .or_else(|| (s.get("type").and_then(Value::as_str) == Some("oauth2")).then(|| s.clone()))
                })
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
        let resp = state
            .http
            .post(&reg)
            .json(&body)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| format!("registration: {}", GatewayError::from_reqwest(&e).message))?;
        let status = resp.status();
        let v = json_body(resp).await;
        if !status.is_success() {
            return Err(format!(
                "registration was refused (HTTP {}): {}",
                status.as_u16(),
                v.get("error_description").or(v.get("error")).and_then(Value::as_str).unwrap_or("no details")
            ));
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
    use std::collections::HashMap;

    use super::*;

    fn by(labels: &[&str]) -> Vec<String> {
        labels.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn sign_ins_split_by_nothing_tenant_or_tenant_and_user() {
        assert!(valid_sign_in_by(&[]));
        assert!(valid_sign_in_by(&by(&["tenant"])));
        assert!(valid_sign_in_by(&by(&["tenant", "user"])));
        for bad in [&["user"][..], &["user", "tenant"], &["tenant", "user", "x"], &["team"]] {
            assert!(!valid_sign_in_by(&by(bad)), "{bad:?}");
        }
    }

    #[test]
    fn slots_keep_the_named_labels_sorted_and_refuse_missing_ones() {
        let labels = Labels::parse("user=u1,feature=chat,tenant=t1").unwrap();
        assert_eq!(labels_for(&[], &labels).unwrap(), "");
        assert_eq!(labels_for(&by(&["tenant"]), &labels).unwrap(), "tenant=t1");
        assert_eq!(labels_for(&by(&["user", "tenant"]), &labels).unwrap(), "tenant=t1,user=u1");
        assert_eq!(labels_for(&by(&["tenant", "user"]), &Labels::parse("tenant=t1").unwrap()).unwrap_err(), "user");
        let c = Client {
            kind: Kind::Mcp,
            upstream: Uuid::nil(),
            name: "x".into(),
            config: OAuthConfig { sign_in_by: by(&["tenant"]), ..Default::default() },
            secret: None,
        };
        let err = c.slot(Who { key: Uuid::nil(), labels: &Labels::default() }).unwrap_err();
        assert!(matches!(&err, AuthError::MissingLabel { label, .. } if label == "tenant"));
        assert!(err.not_signed_in());
        assert_eq!(err.into_gateway().status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn sign_in_links_are_sealed_and_expire() {
        let sb = SecretBox::from_base64(&base64::engine::general_purpose::STANDARD.encode([7u8; 32])).unwrap();
        let config = OAuthConfig {
            sign_in_by: by(&["tenant"]),
            redirect_uri: Some("https://gw.example.com/admin/auth/oauth/callback".into()),
            ..Default::default()
        };
        let c = Client { kind: Kind::A2a, upstream: Uuid::now_v7(), name: "x".into(), config, secret: None };
        let slot = Slot { key: Uuid::now_v7(), labels: "tenant=t1".into() };
        let url = Ticket::new(&c, &slot).url(&c.link_origin().unwrap(), &sb);
        let ticket = url.strip_prefix("https://gw.example.com/admin/auth/oauth/sign-in/").expect("on the gateway's address");
        let t = Ticket::open(ticket, &sb).unwrap();
        assert_eq!((t.kind, t.upstream, &t.slot), (Kind::A2a, c.upstream, &slot));
        assert_ne!(t.nonce, Ticket::new(&c, &slot).nonce, "every link has its own nonce");
        // Altered, or sealed with another key: refused.
        let mut altered = ticket.to_string();
        altered.replace_range(10..11, if &ticket[10..11] == "A" { "B" } else { "A" });
        assert!(Ticket::open(&altered, &sb).is_none());
        let other = SecretBox::from_base64(&base64::engine::general_purpose::STANDARD.encode([8u8; 32])).unwrap();
        assert!(Ticket::open(ticket, &other).is_none());
        // Expired: refused.
        let old = Ticket { expires: Utc::now().timestamp() - 1, ..Ticket::new(&c, &slot) };
        let old = old.url("https://gw.example.com", &sb);
        assert!(Ticket::open(old.rsplit('/').next().unwrap(), &sb).is_none());
    }

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
                let f: HashMap<String, String> =
                    reqwest::Url::parse(&format!("http://x/?{body}")).unwrap().query_pairs().into_owned().collect();
                // "gw:s3cret" in Basic auth, form-encoded first.
                assert_eq!(headers.get(header::AUTHORIZATION).unwrap(), "Basic Z3c6czNjcmV0");
                assert_eq!(
                    (f["grant_type"].as_str(), f["resource"].as_str(), f["scope"].as_str()),
                    ("client_credentials", "https://mcp.example/mcp", "a b")
                );
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
        let auth = UpstreamAuth::Client(Arc::new(Client {
            kind: Kind::Mcp,
            upstream: Uuid::now_v7(),
            name: "x".into(),
            config,
            secret: Some("s3cret".into()),
        }));
        let header = |h: Option<(HeaderName, HeaderValue)>| h.unwrap().1.to_str().unwrap().to_string();
        assert_eq!(header(auth.header(&state, None).await.unwrap()), "Bearer token-0");
        assert_eq!(header(auth.header(&state, None).await.unwrap()), "Bearer token-0");
        auth.renew(&state, None).await;
        assert_eq!(header(auth.header(&state, None).await.unwrap()), "Bearer token-1");
        // Keys that sign in on their own need a key.
        let per_key = UpstreamAuth::PerKey(Arc::new(Client {
            kind: Kind::Mcp,
            upstream: Uuid::nil(),
            name: "y".into(),
            config: OAuthConfig::default(),
            secret: None,
        }));
        assert!(matches!(per_key.header(&state, None).await, Err(AuthError::NotConnected { .. })));
    }

    #[test]
    fn token_lifetimes_are_bounded() {
        assert_eq!(lifetime(Some(3600)), Some(Duration::from_secs(3600)));
        assert_eq!(lifetime(Some(u64::MAX)), Some(Duration::from_secs(MAX_TOKEN_LIFE)));
        assert_eq!(lifetime(None), None);
        let at = expiry(Some(u64::MAX)).unwrap();
        assert!(at <= Utc::now() + chrono::Duration::seconds(MAX_TOKEN_LIFE as i64));
    }

    #[test]
    fn well_known_addresses_put_the_path_after_the_name() {
        assert_eq!(
            well_known("https://auth.example.com/tenant/x", "oauth-authorization-server"),
            [
                "https://auth.example.com/.well-known/oauth-authorization-server/tenant/x",
                "https://auth.example.com/.well-known/oauth-authorization-server"
            ]
        );
        assert_eq!(
            well_known("https://auth.example.com/", "openid-configuration"),
            ["https://auth.example.com/.well-known/openid-configuration"]
        );
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
