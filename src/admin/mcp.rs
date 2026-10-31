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
    /// The token or API key the gateway sends, for `bearer` and `header` auth. Write-only; omitted
    /// on an update, the server keeps its secret.
    #[serde(default)]
    secret: Option<String>,
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
const RESERVED_HEADERS: &[&str] = &[
    "host", "content-type", "content-length", "accept", "connection", "transfer-encoding", "user-agent", "mcp-session-id", "mcp-protocol-version",
    "last-event-id",
];

/// Checks an input and builds the row it describes; `existing` keeps its secret when none is given.
async fn row(state: &AppState, i: McpServerInput, existing: Option<&McpServerRow>) -> R<McpServerRow> {
    let name = i.name.trim().to_string();
    let slug = |s: &str| {
        (1..=64).contains(&s.len())
            && s.bytes().next().is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
            // `__` separates server and tool in the names /mcp lists.
            && !s.contains(SEPARATOR)
    };
    if !slug(&name) {
        return Err(AdminError::bad_request("the name is 1-64 lowercase letters, digits, - and _ (not __), starting with a letter or digit"));
    }
    let description = i.description.as_deref().map(str::trim).filter(|d| !d.is_empty()).map(str::to_string);
    let url = i.url.trim().to_string();
    let parsed = reqwest::Url::parse(&url).map_err(|_| AdminError::bad_request("the URL is not valid"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(AdminError::bad_request("the URL must start with https:// or http://"));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(AdminError::bad_request("put credentials in the authentication, not in the URL"));
    }
    state.cfg.upstream_policy.check_url(&url).map_err(|why| AdminError::bad_request(format!("the URL is not allowed: {why}")))?;
    let auth_header = match i.auth {
        McpAuth::Header => {
            let h = i.auth_header.as_deref().map(str::trim).unwrap_or_default().to_ascii_lowercase();
            if HeaderName::try_from(h.as_str()).is_err() || h.is_empty() {
                return Err(AdminError::bad_request("the header name is required, such as x-api-key"));
            }
            if RESERVED_HEADERS.contains(&h.as_str()) {
                return Err(AdminError::bad_request(format!("the gateway sets {h} itself; the secret goes in another header")));
            }
            Some(h)
        }
        _ if i.auth_header.as_deref().is_some_and(|h| !h.trim().is_empty()) => {
            return Err(AdminError::bad_request("a header name only applies to header authentication"));
        }
        _ => None,
    };
    let secret = match (i.auth, i.secret.as_deref().map(str::trim)) {
        (McpAuth::None, _) => None,
        (_, Some(s)) if !s.is_empty() => {
            if HeaderValue::from_str(s).is_err() {
                return Err(AdminError::bad_request("the secret must be printable characters without line breaks"));
            }
            Some(state.cfg.secret_box.seal(s.as_bytes()))
        }
        _ => Some(
            existing
                .and_then(|e| e.secret.clone())
                .ok_or_else(|| AdminError::bad_request("the secret is required for this authentication"))?,
        ),
    };
    let tags = normalize_tags(i.tags).map_err(AdminError::bad_request)?;
    let mut project_ids = i.project_ids;
    project_ids.sort();
    project_ids.dedup();
    for p in &project_ids {
        if db::get_project(&state.db, *p).await?.is_none() {
            return Err(AdminError::bad_request(format!("no project {p}")));
        }
    }
    let timeout_secs = i.timeout_secs.unwrap_or(existing.map_or(300, |e| e.timeout_secs));
    if !(1..=3600).contains(&timeout_secs) {
        return Err(AdminError::bad_request("the timeout is 1 to 3600 seconds"));
    }
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
    let allowed_tools = tool_list(i.allowed_tools, "allowed_tools")?;
    let denied_tools = tool_list(i.denied_tools, "denied_tools")?;
    let price = |p: f64| p.is_finite() && p >= 0.0;
    if !price(i.price_per_call) || !i.tool_prices.values().all(|p| price(*p)) {
        return Err(AdminError::bad_request("prices are zero or more USD"));
    }
    if let Some(bad) = i.tool_prices.keys().find(|t| !tool_name(t)) {
        return Err(AdminError::bad_request(format!("'{bad}' in tool_prices is not a tool name")));
    }
    let now = Utc::now();
    Ok(McpServerRow {
        id: existing.map_or_else(Uuid::now_v7, |e| e.id),
        name,
        description,
        url,
        auth: i.auth.as_str().to_string(),
        auth_header,
        secret,
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
    broadcast_reload(&state, "MCP server updated").await;
    tracing::info!(mcp_server = %id, "MCP server updated");
    let v = view(row);
    let mut changes = audit::changes(&before, &v);
    if secret_changed {
        changes.insert("secret".into(), "replaced".into());
    }
    if !changes.is_empty() {
        audit::Event::new("mcp_server", "update").on(id, &v.name).details(changes.into()).record(&state, &actor).await;
    }
    Ok(Json(v))
}

pub(crate) async fn delete_mcp_server(State(state): State<AppState>, Extension(actor): Extension<Actor>, Path(id): Path<Uuid>) -> R<StatusCode> {
    let name = db::get_mcp_server(&state.db, id).await?.ok_or_else(AdminError::not_found)?.name;
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
