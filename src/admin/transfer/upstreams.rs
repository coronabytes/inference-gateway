//! MCP servers, A2A agents and tool routers in a configuration document. Like credentials, servers
//! and agents are exported without their secrets: a new one needs its secret entered after the
//! import. They name their projects, and tool routers their servers, by name.

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use super::{Changes, about, bad, claim};
use crate::{
    a2a::A2aAgentRow,
    admin::{
        R,
        a2a::A2A_RESERVED,
        mcp::{MCP_RESERVED, check_auth, check_name, check_tools, check_url},
        tool_routers::{self, ToolRouterInput},
    },
    mcp::{
        McpAuth, McpServerRow,
        hub::SEPARATOR,
        router::{ToolRoute, ToolRouterRow},
    },
    model::{DeploymentRow, ProjectRow, normalize_tags},
    oauth::OAuthConfig,
    state::AppState,
};

fn yes() -> bool {
    true
}

/// Project ids by name, for a document's `projects`; each must exist, in the file or on this gateway.
fn project_ids(names: &[String], projects: &[ProjectRow], what: &str) -> R<Vec<Uuid>> {
    let mut ids = Vec::new();
    for name in names.iter().map(|n| n.trim()) {
        let p = projects
            .iter()
            .find(|p| p.name == name)
            .ok_or_else(|| bad(format!("{what} is limited to project '{name}', which is neither in the file nor on this gateway")))?;
        ids.push(p.id);
    }
    ids.sort();
    ids.dedup();
    Ok(ids)
}

fn project_names(ids: &[Uuid], projects: &[ProjectRow]) -> Vec<String> {
    let mut names: Vec<String> = ids.iter().filter_map(|id| projects.iter().find(|p| p.id == *id).map(|p| p.name.clone())).collect();
    names.sort();
    names
}

/// Whether a new upstream works only once its secret is entered: one that authenticates with a
/// secret, unless it is a public OAuth client or signs keys in (where the secret is optional).
fn needs_secret(auth: McpAuth, oauth: Option<&OAuthConfig>, had_one: bool) -> bool {
    let public = oauth.is_some_and(|o| o.token_auth.as_deref() == Some("none"));
    match auth {
        McpAuth::None => false,
        _ if public => false,
        McpAuth::OAuthUser => had_one,
        _ => true,
    }
}

/// The auth checked as the admin API checks it, with the secret this gateway has (or will need).
fn checked_auth(
    state: &AppState,
    auth: McpAuth,
    header: Option<&str>,
    oauth: Option<OAuthConfig>,
    reserved: &[&str],
) -> R<(Option<String>, Option<sqlx::types::Json<OAuthConfig>>)> {
    // The secret is not in the file: it is checked as if one were stored.
    let c = check_auth(state, auth, header, None, oauth, Some(b"-"), reserved)?;
    Ok((c.header, c.oauth))
}

// ---------------------------------------------------------------- MCP servers

#[derive(Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct McpServerDoc {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<Uuid>,
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    url: String,
    auth: McpAuth,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    auth_header: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    oauth: Option<OAuthConfig>,
    /// Whether a secret was set on the exporting gateway. Never its value.
    #[serde(default)]
    secret: bool,
    #[serde(default)]
    tags: Vec<String>,
    /// The names of the projects it is limited to.
    #[serde(default)]
    projects: Vec<String>,
    timeout_secs: i32,
    #[serde(default)]
    allowed_tools: Vec<String>,
    #[serde(default)]
    denied_tools: Vec<String>,
    #[serde(default)]
    price_per_call: f64,
    #[serde(default)]
    tool_prices: BTreeMap<String, f64>,
    #[serde(default = "yes")]
    enabled: bool,
}

pub(super) fn mcp_server_doc(s: &McpServerRow, projects: &[ProjectRow]) -> McpServerDoc {
    McpServerDoc {
        id: Some(s.id),
        name: s.name.clone(),
        description: s.description.clone(),
        url: s.url.clone(),
        auth: McpAuth::parse(&s.auth).unwrap_or(McpAuth::None),
        auth_header: s.auth_header.clone(),
        oauth: s.oauth.as_ref().map(|o| o.0.clone()),
        secret: s.secret.is_some(),
        tags: s.tags.clone(),
        projects: project_names(&s.project_ids, projects),
        timeout_secs: s.timeout_secs,
        allowed_tools: s.allowed_tools.clone(),
        denied_tools: s.denied_tools.clone(),
        price_per_call: s.price_per_call,
        tool_prices: s.tool_prices.0.clone(),
        enabled: s.enabled,
    }
}

/// What an import of a kind of upstream changes: what it reports, and which entries to write.
#[derive(Default)]
pub(super) struct Imported {
    pub changes: Changes,
    pub created: HashSet<Uuid>,
    pub updated: HashSet<Uuid>,
    /// New ones that work once their secret is entered.
    pub needs_secrets: Vec<String>,
}

pub(super) fn import_mcp_servers(state: &AppState, docs: Vec<McpServerDoc>, servers: &mut Vec<McpServerRow>, projects: &[ProjectRow], now: DateTime<Utc>) -> R<Imported> {
    let mut out = Imported::default();
    let mut claimed = HashSet::new();
    let file_ids: HashSet<Uuid> = docs.iter().filter_map(|d| d.id).collect();
    for d in docs {
        let what = format!("MCP server '{}'", d.name.trim());
        let name = check_name(&d.name, "server").map_err(about(&what))?;
        if name.contains(SEPARATOR) {
            return Err(bad(format!("{what}: the name cannot contain {SEPARATOR}")));
        }
        let url = check_url(state, &d.url).map_err(about(&what))?;
        let (auth_header, oauth) = checked_auth(state, d.auth, d.auth_header.as_deref(), d.oauth.clone(), &MCP_RESERVED).map_err(about(&what))?;
        let tags = normalize_tags(d.tags.clone()).map_err(|e| bad(format!("{what}: {e}")))?;
        if !(1..=3600).contains(&d.timeout_secs) {
            return Err(bad(format!("{what}: the timeout is 1 to 3600 seconds")));
        }
        let (allowed_tools, denied_tools) = check_tools(d.allowed_tools.clone(), d.denied_tools.clone(), d.price_per_call, &d.tool_prices).map_err(about(&what))?;
        let found = d.id.and_then(|id| servers.iter().position(|x| x.id == id)).or_else(|| {
            servers.iter().position(|x| x.name == name && !claimed.contains(&x.id) && !file_ids.contains(&x.id))
        });
        let row = |id: Uuid, secret: Option<Vec<u8>>, created_at: DateTime<Utc>| -> R<McpServerRow> {
            Ok(McpServerRow {
                id,
                name: name.clone(),
                description: d.description.as_deref().map(str::trim).filter(|x| !x.is_empty()).map(str::to_string),
                url: url.clone(),
                auth: d.auth.as_str().to_string(),
                auth_header: auth_header.clone(),
                secret,
                oauth: oauth.clone(),
                tags: tags.clone(),
                project_ids: project_ids(&d.projects, projects, &what)?,
                timeout_secs: d.timeout_secs,
                enabled: d.enabled,
                allowed_tools: allowed_tools.clone(),
                denied_tools: denied_tools.clone(),
                price_per_call: d.price_per_call,
                tool_prices: sqlx::types::Json(d.tool_prices.clone()),
                created_at,
                updated_at: now,
            })
        };
        match found {
            Some(ix) => {
                claim(&mut claimed, servers[ix].id, &what)?;
                let x = &servers[ix];
                // It keeps its secret; one the auth no longer takes is dropped.
                let secret = if d.auth == McpAuth::None { None } else { x.secret.clone() };
                let new = row(x.id, secret, x.created_at)?;
                if mcp_server_doc(x, projects) == mcp_server_doc(&new, projects) {
                    out.changes.unchanged += 1;
                } else {
                    out.changes.updated.push(if x.name == name { name.clone() } else { format!("{name}, renamed from {}", x.name) });
                    out.updated.insert(new.id);
                    servers[ix] = new;
                }
            }
            None => {
                let new = row(d.id.unwrap_or_else(Uuid::now_v7), None, now)?;
                claimed.insert(new.id);
                out.created.insert(new.id);
                if needs_secret(d.auth, d.oauth.as_ref(), d.secret) {
                    out.needs_secrets.push(name.clone());
                }
                out.changes.created.push(name.clone());
                servers.push(new);
            }
        }
    }
    let mut names = HashSet::new();
    if let Some(s) = servers.iter().find(|s| !names.insert(s.name.as_str())) {
        return Err(bad(format!("MCP server '{}': another MCP server on this gateway has that name", s.name)));
    }
    Ok(out)
}

// ---------------------------------------------------------------- A2A agents

#[derive(Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct A2aAgentDoc {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<Uuid>,
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    card_url: String,
    auth: McpAuth,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    auth_header: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    oauth: Option<OAuthConfig>,
    /// Whether a secret was set on the exporting gateway. Never its value.
    #[serde(default)]
    secret: bool,
    #[serde(default)]
    tags: Vec<String>,
    /// The names of the projects it is limited to.
    #[serde(default)]
    projects: Vec<String>,
    timeout_secs: i32,
    #[serde(default)]
    price_per_message: f64,
    #[serde(default = "yes")]
    enabled: bool,
}

pub(super) fn a2a_agent_doc(a: &A2aAgentRow, projects: &[ProjectRow]) -> A2aAgentDoc {
    A2aAgentDoc {
        id: Some(a.id),
        name: a.name.clone(),
        description: a.description.clone(),
        card_url: a.card_url.clone(),
        auth: McpAuth::parse(&a.auth).unwrap_or(McpAuth::None),
        auth_header: a.auth_header.clone(),
        oauth: a.oauth.as_ref().map(|o| o.0.clone()),
        secret: a.secret.is_some(),
        tags: a.tags.clone(),
        projects: project_names(&a.project_ids, projects),
        timeout_secs: a.timeout_secs,
        price_per_message: a.price_per_message,
        enabled: a.enabled,
    }
}

pub(super) fn import_a2a_agents(state: &AppState, docs: Vec<A2aAgentDoc>, agents: &mut Vec<A2aAgentRow>, projects: &[ProjectRow], now: DateTime<Utc>) -> R<Imported> {
    let mut out = Imported::default();
    let mut claimed = HashSet::new();
    let file_ids: HashSet<Uuid> = docs.iter().filter_map(|d| d.id).collect();
    for d in docs {
        let what = format!("A2A agent '{}'", d.name.trim());
        let name = check_name(&d.name, "agent").map_err(about(&what))?;
        let card_url = crate::admin::a2a::check_card_url(state, &d.card_url).map_err(about(&what))?;
        let (auth_header, oauth) = checked_auth(state, d.auth, d.auth_header.as_deref(), d.oauth.clone(), &A2A_RESERVED).map_err(about(&what))?;
        let tags = normalize_tags(d.tags.clone()).map_err(|e| bad(format!("{what}: {e}")))?;
        if !(1..=3600).contains(&d.timeout_secs) {
            return Err(bad(format!("{what}: the timeout is 1 to 3600 seconds")));
        }
        if !(d.price_per_message.is_finite() && d.price_per_message >= 0.0) {
            return Err(bad(format!("{what}: the price is zero or more USD")));
        }
        let found = d.id.and_then(|id| agents.iter().position(|x| x.id == id)).or_else(|| {
            agents.iter().position(|x| x.name == name && !claimed.contains(&x.id) && !file_ids.contains(&x.id))
        });
        let row = |id: Uuid, secret: Option<Vec<u8>>, created_at: DateTime<Utc>| -> R<A2aAgentRow> {
            Ok(A2aAgentRow {
                id,
                name: name.clone(),
                description: d.description.as_deref().map(str::trim).filter(|x| !x.is_empty()).map(str::to_string),
                card_url: card_url.clone(),
                auth: d.auth.as_str().to_string(),
                auth_header: auth_header.clone(),
                secret,
                oauth: oauth.clone(),
                tags: tags.clone(),
                project_ids: project_ids(&d.projects, projects, &what)?,
                timeout_secs: d.timeout_secs,
                price_per_message: d.price_per_message,
                enabled: d.enabled,
                created_at,
                updated_at: now,
            })
        };
        match found {
            Some(ix) => {
                claim(&mut claimed, agents[ix].id, &what)?;
                let x = &agents[ix];
                let secret = if d.auth == McpAuth::None { None } else { x.secret.clone() };
                let new = row(x.id, secret, x.created_at)?;
                if a2a_agent_doc(x, projects) == a2a_agent_doc(&new, projects) {
                    out.changes.unchanged += 1;
                } else {
                    out.changes.updated.push(if x.name == name { name.clone() } else { format!("{name}, renamed from {}", x.name) });
                    out.updated.insert(new.id);
                    agents[ix] = new;
                }
            }
            None => {
                let new = row(d.id.unwrap_or_else(Uuid::now_v7), None, now)?;
                claimed.insert(new.id);
                out.created.insert(new.id);
                if needs_secret(d.auth, d.oauth.as_ref(), d.secret) {
                    out.needs_secrets.push(name.clone());
                }
                out.changes.created.push(name.clone());
                agents.push(new);
            }
        }
    }
    let mut names = HashSet::new();
    if let Some(a) = agents.iter().find(|a| !names.insert(a.name.as_str())) {
        return Err(bad(format!("A2A agent '{}': another A2A agent on this gateway has that name", a.name)));
    }
    Ok(out)
}

// ---------------------------------------------------------------- tool routers

#[derive(Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolRouterDoc {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<Uuid>,
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    decision_model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    routes: Vec<ToolRouteDoc>,
    threshold: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_routes: Option<i32>,
    #[serde(default = "yes")]
    enabled: bool,
}

/// A route, with its MCP server by name.
#[derive(Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolRouteDoc {
    name: String,
    description: String,
    /// The MCP server's name.
    server: String,
    #[serde(default)]
    tools: Vec<String>,
}

pub(super) fn tool_router_doc(r: &ToolRouterRow, servers: &[McpServerRow]) -> ToolRouterDoc {
    ToolRouterDoc {
        id: Some(r.id),
        name: r.name.clone(),
        description: r.description.clone(),
        decision_model: r.decision_model.clone(),
        instructions: r.instructions.clone(),
        routes: r
            .routes
            .0
            .iter()
            .map(|x: &ToolRoute| ToolRouteDoc {
                name: x.name.clone(),
                description: x.description.clone(),
                server: servers.iter().find(|s| s.id == x.server_id).map(|s| s.name.clone()).unwrap_or_default(),
                tools: x.tools.clone(),
            })
            .collect(),
        threshold: r.threshold,
        max_routes: r.max_routes,
        enabled: r.enabled,
    }
}

/// Tool routers, checked against the MCP servers and decision models as they will be.
pub(super) fn import_tool_routers(docs: Vec<ToolRouterDoc>, routers: &mut Vec<ToolRouterRow>, servers: &[McpServerRow], deployments: &[DeploymentRow]) -> R<Imported> {
    let mut out = Imported::default();
    let mut claimed = HashSet::new();
    let file_ids: HashSet<Uuid> = docs.iter().filter_map(|d| d.id).collect();
    let by_name: HashMap<&str, Uuid> = servers.iter().map(|s| (s.name.as_str(), s.id)).collect();
    let server_list: Vec<(Uuid, &str)> = servers.iter().map(|s| (s.id, s.name.as_str())).collect();
    let is_decision_model = |m: &str| deployments.iter().any(|d| d.model_name == m && d.mode == "decision");
    for d in docs {
        let what = format!("tool router '{}'", d.name.trim());
        let mut routes = Vec::new();
        for r in &d.routes {
            let server = by_name
                .get(r.server.trim())
                .ok_or_else(|| bad(format!("{what}: route {} uses MCP server '{}', which is neither in the file nor on this gateway", r.name, r.server)))?;
            routes.push(json!({ "name": r.name, "description": r.description, "server_id": server, "tools": r.tools }));
        }
        let input = json!({
            "name": d.name, "description": d.description, "decision_model": d.decision_model, "instructions": d.instructions,
            "routes": routes, "threshold": d.threshold, "max_routes": d.max_routes, "enabled": d.enabled,
        });
        let input: ToolRouterInput = serde_json::from_value(input).map_err(|e| bad(format!("{what}: {e}")))?;
        let name = d.name.trim().to_string();
        let found = d.id.and_then(|id| routers.iter().position(|x| x.id == id)).or_else(|| {
            routers.iter().position(|x| x.name == name && !claimed.contains(&x.id) && !file_ids.contains(&x.id))
        });
        match found {
            Some(ix) => {
                claim(&mut claimed, routers[ix].id, &what)?;
                let new = tool_routers::check(input, &server_list, is_decision_model, Some(&routers[ix])).map_err(about(&what))?;
                let x = &routers[ix];
                if tool_router_doc(x, servers) == tool_router_doc(&new, servers) {
                    out.changes.unchanged += 1;
                } else {
                    out.changes.updated.push(if x.name == new.name { new.name.clone() } else { format!("{}, renamed from {}", new.name, x.name) });
                    out.updated.insert(new.id);
                    routers[ix] = new;
                }
            }
            None => {
                let mut new = tool_routers::check(input, &server_list, is_decision_model, None).map_err(about(&what))?;
                if let Some(id) = d.id {
                    new.id = id;
                }
                claimed.insert(new.id);
                out.created.insert(new.id);
                out.changes.created.push(new.name.clone());
                routers.push(new);
            }
        }
    }
    let mut names = HashSet::new();
    if let Some(r) = routers.iter().find(|r| !names.insert(r.name.as_str())) {
        return Err(bad(format!("tool router '{}': another tool router on this gateway has that name", r.name)));
    }
    if let Some(r) = routers.iter().find(|r| by_name.contains_key(r.name.as_str())) {
        return Err(bad(format!("tool router '{}': an MCP server has that name", r.name)));
    }
    Ok(out)
}
