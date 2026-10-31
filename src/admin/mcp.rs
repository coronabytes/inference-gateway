//! Admin API for MCP servers: which upstream the gateway proxies at `/mcp/{name}`, how it
//! authenticates there, and which keys may use it; and trying one out.

use std::collections::BTreeMap;

use axum::{
    Extension, Json,
    extract::{Path, State},
    http::{HeaderName, HeaderValue, StatusCode},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::R;
use crate::{
    audit::{self, Actor},
    db,
    error::AdminError,
    mcp::{McpAuth, McpServer, McpServerRow, ProbedTool, hub::SEPARATOR},
    oauth::OAuthConfig,
    model::normalize_tags,
    state::AppState,
    sync::broadcast_reload,
};

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct McpServerView {
    id: Uuid,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    url: String,
    auth: McpAuth,
    #[serde(skip_serializing_if = "Option::is_none")]
    auth_header: Option<String>,
    /// Whether a secret is stored. The secret itself is never returned.
    has_secret: bool,
    /// The OAuth client, for `oauth` and `oauth_user` auth.
    #[serde(skip_serializing_if = "Option::is_none")]
    oauth: Option<OAuthConfig>,
    tags: Vec<String>,
    project_ids: Vec<Uuid>,
    timeout_secs: i32,
    enabled: bool,
    /// The tools keys may call; empty for every tool the server offers.
    allowed_tools: Vec<String>,
    /// Tools keys may not call, whatever `allowed_tools` says.
    denied_tools: Vec<String>,
    /// USD per tool call, unless `tool_prices` names the tool.
    price_per_call: f64,
    /// USD per call of these tools.
    tool_prices: BTreeMap<String, f64>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

fn view(s: McpServerRow) -> McpServerView {
    McpServerView {
        id: s.id,
        name: s.name,
        description: s.description,
        url: s.url,
        auth: McpAuth::parse(&s.auth).unwrap_or(McpAuth::None),
        auth_header: s.auth_header,
        has_secret: s.secret.is_some(),
        oauth: s.oauth.map(|o| o.0),
        tags: s.tags,
        project_ids: s.project_ids,
        timeout_secs: s.timeout_secs,
        enabled: s.enabled,
        allowed_tools: s.allowed_tools,
        denied_tools: s.denied_tools,
        price_per_call: s.price_per_call,
        tool_prices: s.tool_prices.0,
        created_at: s.created_at,
        updated_at: s.updated_at,
    }
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct McpServerInput {
    /// What clients call it by, at `/mcp/{name}`: lowercase letters, digits, `-` and `_`.
    name: String,
    #[serde(default)]
    description: Option<String>,
    /// The server's Streamable HTTP endpoint, such as `https://mcp.example.com/mcp`.
    url: String,
    auth: McpAuth,
    /// The header that carries the secret, for `header` auth.
    #[serde(default)]
    auth_header: Option<String>,
    /// The token or API key the gateway sends, for `bearer` and `header` auth, or the OAuth client's
    /// secret. Write-only; omitted on an update, the server keeps its secret.
    #[serde(default)]
    secret: Option<String>,
    /// The OAuth client, for `oauth` and `oauth_user` auth.
    #[serde(default)]
    oauth: Option<OAuthConfig>,
    /// A key with tags may only use untagged servers and those sharing a tag with it.
    #[serde(default)]
    tags: Vec<String>,
    /// Projects the server is limited to; empty means any key may use it (as its tags allow).
    #[serde(default)]
    project_ids: Vec<Uuid>,
    /// Seconds a POST may take, its streamed answer included. Default 300.
    #[serde(default)]
    timeout_secs: Option<i32>,
    #[serde(default = "enabled")]
    enabled: bool,
    /// The tools keys may call; empty (the default) for every tool the server offers, new ones included.
    #[serde(default)]
    allowed_tools: Vec<String>,
    /// Tools keys may not call, whatever `allowed_tools` says.
    #[serde(default)]
    denied_tools: Vec<String>,
    /// USD per tool call, charged once the tool answered. Default 0.
    #[serde(default)]
    price_per_call: f64,
    /// USD per call of these tools, instead of `price_per_call`.
    #[serde(default)]
    tool_prices: BTreeMap<String, f64>,
}

fn enabled() -> bool {
    true
}

/// Headers the gateway sets itself, or forwards from the client, which the secret cannot go in.
const RESERVED_HEADERS: &[&str] = &["host", "content-type", "content-length", "accept", "connection", "transfer-encoding", "user-agent"];

/// A name clients call an upstream by, in a path: 1-64 lowercase letters, digits, `-` and `_`.
pub(super) fn check_name(name: &str, what: &str) -> R<String> {
    let name = name.trim();
    let ok = (1..=64).contains(&name.len())
        && name.bytes().next().is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    if !ok {
        return Err(AdminError::bad_request(format!("the {what}'s name is 1-64 lowercase letters, digits, - and _, starting with a letter or digit")));
    }
    Ok(name.to_string())
}

/// An upstream's URL: http(s), without credentials, to an address the gateway may reach.
pub(super) fn check_url(state: &AppState, url: &str) -> R<String> {
    let url = url.trim().to_string();
    let parsed = reqwest::Url::parse(&url).map_err(|_| AdminError::bad_request("the URL is not valid"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(AdminError::bad_request("the URL must start with https:// or http://"));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(AdminError::bad_request("put credentials in the authentication, not in the URL"));
    }
    state.cfg.upstream_policy.check_url(&url).map_err(|why| AdminError::bad_request(format!("the URL is not allowed: {why}")))?;
    Ok(url)
}

/// How the gateway signs in to an upstream, as checked: the header name, for `header` auth, the
/// sealed secret, and the OAuth client.
pub(super) struct CheckedAuth {
    pub header: Option<String>,
    pub secret: Option<Vec<u8>>,
    pub oauth: Option<sqlx::types::Json<OAuthConfig>>,
}

/// How the gateway signs in to an upstream. The secret is `existing`'s when no new one is given;
/// `reserved` are headers the gateway sets.
pub(super) fn check_auth(
    state: &AppState,
    auth: McpAuth,
    header: Option<&str>,
    secret: Option<&str>,
    oauth: Option<OAuthConfig>,
    existing: Option<&[u8]>,
    reserved: &[&str],
) -> R<CheckedAuth> {
    let oauth = match auth {
        McpAuth::OAuth | McpAuth::OAuthUser => Some(sqlx::types::Json(check_oauth(state, auth, oauth)?)),
        _ if oauth.is_some() => return Err(AdminError::bad_request("an OAuth client only applies to OAuth authentication")),
        _ => None,
    };
    // A public client (PKCE, no secret) keeps none.
    let public = oauth.as_ref().is_some_and(|o| o.token_auth.as_deref() == Some("none"));
    let secret = match (auth, secret.map(str::trim)) {
        (McpAuth::None, _) => None,
        _ if public => None,
        (_, Some(s)) if !s.is_empty() => {
            if HeaderValue::from_str(s).is_err() {
                return Err(AdminError::bad_request("the secret must be printable characters without line breaks"));
            }
            Some(state.cfg.secret_box.seal(s.as_bytes()))
        }
        // Signing in per key works without a client secret, where the server registered a public client.
        (McpAuth::OAuthUser, _) => existing.map(<[u8]>::to_vec),
        _ => Some(existing.map(<[u8]>::to_vec).ok_or_else(|| AdminError::bad_request("the secret is required for this authentication"))?),
    };
    let header = check_header(auth, header, reserved)?;
    Ok(CheckedAuth { header, secret, oauth })
}

/// An OAuth client: URLs the gateway may reach, a client id, and what signing keys in needs.
fn check_oauth(state: &AppState, auth: McpAuth, oauth: Option<OAuthConfig>) -> R<OAuthConfig> {
    let o = oauth.ok_or_else(|| AdminError::bad_request("OAuth needs its client: token URL and client id"))?;
    let token_url = check_url(state, &o.token_url).map_err(|e| AdminError::bad_request(format!("token URL: {}", e.1)))?;
    let optional_url = |u: Option<String>, what: &str| -> R<Option<String>> {
        match u.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
            Some(u) => check_url(state, u).map(Some).map_err(|e| AdminError::bad_request(format!("{what}: {}", e.1))),
            None => Ok(None),
        }
    };
    let authorization_url = optional_url(o.authorization_url, "authorization URL")?;
    if auth == McpAuth::OAuthUser && authorization_url.is_none() {
        return Err(AdminError::bad_request("signing keys in needs the authorization URL"));
    }
    let client_id = o.client_id.trim().to_string();
    if client_id.is_empty() || client_id.len() > 500 {
        return Err(AdminError::bad_request("the client id is required"));
    }
    let mut scopes: Vec<String> = o.scopes.iter().flat_map(|s| s.split_whitespace()).map(str::to_string).collect();
    scopes.dedup();
    if let Some(m) = o.token_auth.as_deref().filter(|m| !matches!(*m, "client_secret_basic" | "client_secret_post" | "none")) {
        return Err(AdminError::bad_request(format!("'{m}' is not a token authentication: use client_secret_basic, client_secret_post or none")));
    }
    // The resource and the redirect URI are compared as given, so only checked to be URLs.
    let url = |u: Option<String>, what: &str| -> R<Option<String>> {
        match u.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
            Some(u) if reqwest::Url::parse(u).is_ok() => Ok(Some(u.to_string())),
            Some(_) => Err(AdminError::bad_request(format!("the {what} is not a valid URL"))),
            None => Ok(None),
        }
    };
    if !crate::oauth::valid_sign_in_by(&o.sign_in_by) {
        return Err(AdminError::bad_request("sign-ins are per key, per tenant ([\"tenant\"]) or per tenant and user ([\"tenant\", \"user\"])"));
    }
    if auth != McpAuth::OAuthUser && !o.sign_in_by.is_empty() {
        return Err(AdminError::bad_request("only servers that keys sign in to on their own keep sign-ins per tenant or user"));
    }
    let range = crate::oauth::SIGN_IN_RETENTION_RANGE;
    match o.sign_in_retention_days {
        Some(d) if !range.contains(&d) => {
            return Err(AdminError::bad_request(format!("unused sign-ins are kept {} to {} days", range.start(), range.end())));
        }
        Some(_) if auth != McpAuth::OAuthUser => return Err(AdminError::bad_request("only servers that keys sign in to on their own keep sign-ins")),
        _ => {}
    }
    Ok(OAuthConfig {
        token_url,
        authorization_url,
        client_id,
        scopes,
        resource: url(o.resource, "resource")?,
        redirect_uri: url(o.redirect_uri, "redirect URI")?,
        token_auth: o.token_auth,
        sign_in_by: o.sign_in_by,
        sign_in_retention_days: o.sign_in_retention_days,
    })
}

/// The header name, for `header` auth.
fn check_header(auth: McpAuth, header: Option<&str>, reserved: &[&str]) -> R<Option<String>> {
    let auth_header = match auth {
        McpAuth::Header => {
            let h = header.map(str::trim).unwrap_or_default().to_ascii_lowercase();
            if HeaderName::try_from(h.as_str()).is_err() || h.is_empty() {
                return Err(AdminError::bad_request("the header name is required, such as x-api-key"));
            }
            if RESERVED_HEADERS.contains(&h.as_str()) || reserved.contains(&h.as_str()) {
                return Err(AdminError::bad_request(format!("the gateway sets {h} itself; the secret goes in another header")));
            }
            Some(h)
        }
        _ if header.is_some_and(|h| !h.trim().is_empty()) => {
            return Err(AdminError::bad_request("a header name only applies to header authentication"));
        }
        _ => None,
    };
    Ok(auth_header)
}

/// Projects, sorted and deduplicated, each of which must exist.
pub(super) async fn check_projects(state: &AppState, mut project_ids: Vec<Uuid>) -> R<Vec<Uuid>> {
    project_ids.sort();
    project_ids.dedup();
    for p in &project_ids {
        if db::get_project(&state.db, *p).await?.is_none() {
            return Err(AdminError::bad_request(format!("no project {p}")));
        }
    }
    Ok(project_ids)
}

/// A server's tool filters, sorted and deduplicated, and its prices, checked.
pub(super) fn check_tools(allowed: Vec<String>, denied: Vec<String>, price_per_call: f64, tool_prices: &BTreeMap<String, f64>) -> R<(Vec<String>, Vec<String>)> {
    // Tool names as servers give them: case matters; at most 128 characters, without control characters.
    let tool_name = |t: &str| (1..=128).contains(&t.len()) && !t.chars().any(char::is_control);
    let tool_list = |list: Vec<String>, what: &str| -> R<Vec<String>> {
        let mut list: Vec<String> = list.into_iter().map(|t| t.trim().to_string()).collect();
        if let Some(bad) = list.iter().find(|t| !tool_name(t)) {
            return Err(AdminError::bad_request(format!("'{bad}' in {what} is not a tool name")));
        }
        list.sort();
        list.dedup();
        Ok(list)
    };
    let allowed = tool_list(allowed, "allowed_tools")?;
    let denied = tool_list(denied, "denied_tools")?;
    let price = |p: f64| p.is_finite() && p >= 0.0;
    if !price(price_per_call) || !tool_prices.values().all(|p| price(*p)) {
        return Err(AdminError::bad_request("prices are zero or more USD"));
    }
    if let Some(bad) = tool_prices.keys().find(|t| !tool_name(t)) {
        return Err(AdminError::bad_request(format!("'{bad}' in tool_prices is not a tool name")));
    }
    Ok((allowed, denied))
}

/// Headers the gateway sets itself on MCP requests: no secret goes in them.
pub(super) const MCP_RESERVED: [&str; 5] = ["mcp-session-id", "mcp-protocol-version", "last-event-id", "mcp-method", "mcp-name"];

/// Checks an input and builds the row it describes; `existing` keeps its secret when none is given.
async fn row(state: &AppState, i: McpServerInput, existing: Option<&McpServerRow>) -> R<McpServerRow> {
    let name = check_name(&i.name, "server")?;
    // `__` separates server and tool in the names /mcp lists.
    if name.contains(SEPARATOR) {
        return Err(AdminError::bad_request(format!("the server's name cannot contain {SEPARATOR}")));
    }
    if db::list_tool_routers(&state.db).await?.iter().any(|r| r.name == name) {
        return Err(AdminError::conflict("a tool router has this name: requests could not tell them apart"));
    }
    let description = i.description.as_deref().map(str::trim).filter(|d| !d.is_empty()).map(str::to_string);
    let url = check_url(state, &i.url)?;
    let auth = check_auth(state, i.auth, i.auth_header.as_deref(), i.secret.as_deref(), i.oauth, existing.and_then(|e| e.secret.as_deref()), &MCP_RESERVED)?;
    let tags = normalize_tags(i.tags).map_err(AdminError::bad_request)?;
    let project_ids = check_projects(state, i.project_ids).await?;
    let timeout_secs = i.timeout_secs.unwrap_or(existing.map_or(300, |e| e.timeout_secs));
    if !(1..=3600).contains(&timeout_secs) {
        return Err(AdminError::bad_request("the timeout is 1 to 3600 seconds"));
    }
    let (allowed_tools, denied_tools) = check_tools(i.allowed_tools, i.denied_tools, i.price_per_call, &i.tool_prices)?;
    let now = Utc::now();
    Ok(McpServerRow {
        id: existing.map_or_else(Uuid::now_v7, |e| e.id),
        name,
        description,
        url,
        auth: i.auth.as_str().to_string(),
        auth_header: auth.header,
        secret: auth.secret,
        oauth: auth.oauth,
        tags,
        project_ids,
        timeout_secs,
        enabled: i.enabled,
        allowed_tools,
        denied_tools,
        price_per_call: i.price_per_call,
        tool_prices: sqlx::types::Json(i.tool_prices),
        created_at: existing.map_or(now, |e| e.created_at),
        updated_at: now,
    })
}

fn conflict_on_duplicate_name(e: sqlx::Error) -> AdminError {
    if db::is_unique_violation(&e) { AdminError::conflict("an MCP server with this name already exists") } else { e.into() }
}

pub(crate) async fn list_mcp_servers(State(state): State<AppState>) -> R<Json<Vec<McpServerView>>> {
    Ok(Json(db::list_mcp_servers(&state.db).await?.into_iter().map(view).collect()))
}

pub(crate) async fn create_mcp_server(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(i): Json<McpServerInput>,
) -> R<(StatusCode, Json<McpServerView>)> {
    let row = row(&state, i, None).await?;
    db::insert_mcp_server(&state.db, &row).await.map_err(conflict_on_duplicate_name)?;
    broadcast_reload(&state, "MCP server created").await;
    tracing::info!(mcp_server = %row.id, name = %row.name, "MCP server created");
    let v = view(row);
    audit::Event::new("mcp_server", "create").on(v.id, &v.name).details(audit::snapshot(&v)).record(&state, &actor).await;
    Ok((StatusCode::CREATED, Json(v)))
}

pub(crate) async fn update_mcp_server(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<Uuid>,
    Json(i): Json<McpServerInput>,
) -> R<Json<McpServerView>> {
    let existing = db::get_mcp_server(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    let secret_changed = i.secret.as_deref().is_some_and(|s| !s.trim().is_empty());
    let before = view(existing.clone());
    let row = row(&state, i, Some(&existing)).await?;
    db::update_mcp_server(&state.db, &row).await.map_err(conflict_on_duplicate_name)?;
    let dropped = super::oauth::drop_stale_sign_ins(&state, crate::oauth::Kind::Mcp, id, (&existing.auth, existing.oauth.as_deref()), (&row.auth, row.oauth.as_deref())).await?;
    broadcast_reload(&state, "MCP server updated").await;
    tracing::info!(mcp_server = %id, "MCP server updated");
    let v = view(row);
    let mut changes = audit::changes(&before, &v);
    if secret_changed {
        changes.insert("secret".into(), "replaced".into());
    }
    if dropped > 0 {
        changes.insert("sign_ins".into(), format!("{dropped} removed").into());
    }
    if !changes.is_empty() {
        audit::Event::new("mcp_server", "update").on(id, &v.name).details(changes.into()).record(&state, &actor).await;
    }
    Ok(Json(v))
}

pub(crate) async fn delete_mcp_server(State(state): State<AppState>, Extension(actor): Extension<Actor>, Path(id): Path<Uuid>) -> R<StatusCode> {
    let name = db::get_mcp_server(&state.db, id).await?.ok_or_else(AdminError::not_found)?.name;
    let routers = super::tool_routers::routers_using(&state, id).await?;
    if !routers.is_empty() {
        return Err(AdminError::conflict(format!("tool routers use this server: {}", routers.join(", "))));
    }
    if !db::delete_mcp_server(&state.db, id).await? {
        return Err(AdminError::not_found());
    }
    broadcast_reload(&state, "MCP server deleted").await;
    tracing::info!(mcp_server = %id, "MCP server deleted");
    audit::Event::new("mcp_server", "delete").on(id, name).record(&state, &actor).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct McpServerTestResult {
    ok: bool,
    latency_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    protocol_version: Option<String>,
    /// The server's `serverInfo`: its name and version.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    server_info: Option<serde_json::Value>,
    tools: Vec<ProbedTool>,
    /// Why the server could not be used, when `ok` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Connects to the server as a client would (also while it is disabled), lists its tools and
/// disconnects. Nothing is logged or charged to a key.
pub(crate) async fn test_mcp_server(State(state): State<AppState>, Path(id): Path<Uuid>) -> R<Json<McpServerTestResult>> {
    let row = db::get_mcp_server(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    let server = McpServer::build(row, &state.cfg.secret_box).map_err(|e| AdminError(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let started = std::time::Instant::now();
    let result = crate::mcp::probe(&state, &server).await;
    let latency_ms = started.elapsed().as_millis() as u64;
    Ok(Json(match result {
        Ok(p) => McpServerTestResult { ok: true, latency_ms, protocol_version: p.protocol_version, server_info: p.server_info, tools: p.tools, error: None },
        Err(e) => McpServerTestResult { ok: false, latency_ms, protocol_version: None, server_info: None, tools: Vec::new(), error: Some(e) },
    }))
}
