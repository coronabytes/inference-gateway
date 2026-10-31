//! `/mcp`: the gateway's own MCP server, with the tools of every MCP server a key may use, named
//! `{server}__{tool}`. Unlike `/mcp/{server}`, the gateway answers here itself, through the official
//! SDK's stateless Streamable HTTP server, which speaks every protocol version, so any replica
//! answers any request. Tool calls go to their server in a session the gateway keeps for the key
//! (see [`super::Upstreams`]), and are logged and charged as on `/mcp/{server}`.

use std::{
    borrow::Cow,
    sync::{Arc, LazyLock},
};

use axum::{
    extract::{Request, State},
    response::Response,
};
use rmcp::{
    ErrorData, ServerHandler,
    model::{CallToolRequestParams, CallToolResponse, CallToolResult, Implementation, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool},
    service::{RequestContext, RoleServer},
    transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager},
};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::{Calls, McpServer, ToolsError};
use crate::{
    auth::{self, ClientKey},
    error::{Dialect, GatewayError},
    labels::Labels,
    model::VirtualKey,
    oauth::AuthError,
    state::AppState,
};

/// Separates a server's name from its tool's in the names `/mcp` lists.
pub const SEPARATOR: &str = "__";

/// Who is calling, put into the request by [`hub`] for the handler.
#[derive(Clone)]
struct Caller {
    state: AppState,
    key: Arc<VirtualKey>,
    labels: Labels,
}

static SERVICE: LazyLock<StreamableHttpService<Hub, LocalSessionManager>> = LazyLock::new(|| {
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None)
        // The gateway is reached under names of its operator's choosing, and every request needs a key.
        .disable_allowed_hosts();
    StreamableHttpService::new(|| Ok(Hub), Arc::new(LocalSessionManager::default()), config)
});

/// `POST`, `GET` and `DELETE /mcp`: the key is authenticated here, before the SDK reads anything.
pub async fn hub(State(state): State<AppState>, ClientKey(key): ClientKey, req: Request) -> Response {
    let labels = match Labels::from_headers(req.headers()) {
        Ok(l) => l,
        Err(e) => return e.into_response_for(Dialect::OpenAi),
    };
    let (mut parts, body) = req.into_parts();
    parts.extensions.insert(Caller { state, key, labels });
    match SERVICE.clone().oneshot(Request::from_parts(parts, body)).await {
        Ok(resp) => resp.map(axum::body::Body::new),
        Err(never) => match never {},
    }
}

#[derive(Clone)]
struct Hub;

fn caller(ctx: &RequestContext<RoleServer>) -> Result<Caller, ErrorData> {
    ctx.extensions
        .get::<axum::http::request::Parts>()
        .and_then(|p| p.extensions.get::<Caller>())
        .cloned()
        .ok_or_else(|| ErrorData::internal_error("no caller", None))
}

/// The servers `key` may use, by name.
fn servers(state: &AppState, key: &VirtualKey) -> Vec<Arc<McpServer>> {
    let mut servers: Vec<Arc<McpServer>> = state.registry.snapshot().mcp_servers.values().filter(|s| s.allows(key)).cloned().collect();
    servers.sort_by(|a, b| a.name.cmp(&b.name));
    servers
}

/// The server and tool a name from `/mcp`'s list stands for. Server names may contain the
/// separator too: the longest server name that fits wins.
fn resolve<'a>(servers: &'a [Arc<McpServer>], name: &'a str) -> Option<(&'a Arc<McpServer>, &'a str)> {
    servers
        .iter()
        .filter_map(|s| Some((s, name.strip_prefix(s.name.as_str())?.strip_prefix(SEPARATOR)?)))
        .filter(|(_, tool)| !tool.is_empty())
        .max_by_key(|(s, _)| s.name.len())
}

impl ServerHandler for Hub {
    fn get_info(&self) -> ServerConfig {
        let mut config = ServerConfig::new(ServerCapabilities::builder().enable_tools().build());
        config.server_info = Implementation::new("inference-gateway", crate::VERSION);
        config.instructions = Some(format!(
            "The tools of the MCP servers behind the inference gateway, named after their server: server{SEPARATOR}tool."
        ));
        config
    }

    async fn list_tools(&self, _request: Option<PaginatedRequestParams>, ctx: RequestContext<RoleServer>) -> Result<ListToolsResult, ErrorData> {
        let Caller { state, key, .. } = caller(&ctx)?;
        let servers = servers(&state, &key);
        let lists = futures::future::join_all(servers.iter().map(|s| state.mcp.tools(&state, s, key.id))).await;
        let mut tools = Vec::new();
        for (server, listed) in servers.iter().zip(lists) {
            // A server that cannot be reached leaves its tools out, rather than failing the others',
            // and so does one the key has not signed in to yet.
            let listed = match listed {
                Ok(l) => l,
                Err(ToolsError::Auth(AuthError::NotConnected(_))) => continue,
                Err(ToolsError::Auth(e)) => {
                    tracing::warn!(server = %server.name, error = %e.message(), "MCP server's tools left out of /mcp");
                    continue;
                }
                Err(ToolsError::Failed(e)) => {
                    tracing::warn!(server = %server.name, error = %e, "MCP server's tools left out of /mcp");
                    continue;
                }
            };
            for t in listed.iter() {
                let Some(name) = t.get("name").and_then(Value::as_str).filter(|n| server.allows_tool(n)) else { continue };
                let mut t = t.clone();
                t["name"] = json!(format!("{}{SEPARATOR}{name}", server.name));
                match serde_json::from_value::<Tool>(t) {
                    Ok(t) => tools.push(t),
                    Err(e) => tracing::warn!(server = %server.name, tool = name, error = %e, "MCP tool left out of /mcp: its description is not valid"),
                }
            }
        }
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(&self, request: CallToolRequestParams, ctx: RequestContext<RoleServer>) -> Result<CallToolResponse, ErrorData> {
        let Caller { state, key, labels } = caller(&ctx)?;
        let servers = servers(&state, &key);
        let unknown = || ErrorData::invalid_params(format!("Unknown tool: {}", request.name), None);
        let (server, tool) = resolve(&servers, &request.name).ok_or_else(unknown)?;
        if !server.allows_tool(tool) {
            let mut t = crate::accounting::Tracker::new(&state, Some(&key), &labels, &server.name, "mcp", false);
            t.tool(tool);
            t.fail_status(403, "the tool is not available through the gateway");
            return Err(unknown());
        }
        // The call as the server gets it: its own tool name, the arguments and the progress token.
        let mut params = json!({ "name": tool, "arguments": request.arguments.clone().unwrap_or_default() });
        if let Some(token) = request.meta.as_ref().and_then(|m| m.get("progressToken")) {
            params["_meta"] = json!({ "progressToken": token });
        }
        let msg = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": params });
        let mut calls = Calls::new(&state, &key, &labels, server, std::slice::from_ref(&msg));
        if let Err(e) = auth::check_key_limits(&state, &key).await {
            calls.fail(&e);
            return Err(gateway_error(&e));
        }
        if let Err(why) = state.cfg.upstream_policy.check_url(&server.url) {
            let e = GatewayError::internal(format!("the MCP server's address is not allowed: {why}"));
            calls.fail(&e);
            return Err(gateway_error(&e));
        }
        let reply = match state.mcp.call(&state, key.id, server, &msg).await {
            Ok(r) => r,
            Err(e) => {
                calls.fail_status(502, e.clone());
                return Err(ErrorData::internal_error(format!("the MCP server failed: {e}"), None));
            }
        };
        calls.ended = true;
        calls.settle(&reply);
        if let Some(e) = reply.get("error") {
            return Err(serde_json::from_value(e.clone()).unwrap_or_else(|_| ErrorData::internal_error("the MCP server failed", None)));
        }
        let result = reply.get("result").cloned().unwrap_or(Value::Null);
        serde_json::from_value::<CallToolResult>(result)
            .map(Into::into)
            .map_err(|e| ErrorData::internal_error(format!("the MCP server's result is not valid: {e}"), None))
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [rmcp::model::ProtocolVersion]> {
        Cow::Borrowed(rmcp::model::ProtocolVersion::KNOWN_VERSIONS)
    }
}

/// A refusal of the gateway's own (a limit, a budget) as a JSON-RPC error.
fn gateway_error(e: &GatewayError) -> ErrorData {
    ErrorData::new(rmcp::model::ErrorCode(-32000), e.message.clone(), Some(json!({ "status": e.status.as_u16() })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(name: &str) -> Arc<McpServer> {
        Arc::new(McpServer { name: name.into(), ..super::super::tests::server(uuid::Uuid::now_v7()) })
    }

    #[test]
    fn names_resolve_to_the_longest_fitting_server() {
        let servers = [server("git"), server("git__hub"), server("docs")];
        let found = |n| resolve(&servers, n).map(|(s, t)| (s.name.clone(), t.to_string()));
        assert_eq!(found("git__status"), Some(("git".into(), "status".into())));
        assert_eq!(found("git__hub__issues"), Some(("git__hub".into(), "issues".into())));
        assert_eq!(found("docs__a__b"), Some(("docs".into(), "a__b".into())));
        assert_eq!(found("docs__"), None);
        assert_eq!(found("nope__x"), None);
        assert_eq!(found("docs"), None);
    }
}
