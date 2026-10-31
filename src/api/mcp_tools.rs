//! MCP tools the gateway runs itself inside a model request, with any model, streaming or not.
//!
//! A client names gateway MCP servers among its tools: `{"type": "mcp", "server_label": …}` in
//! Responses and Chat Completions, `mcp_servers` in Messages. The servers' tools (as the key may
//! use them) are offered to the model as function tools named `{server}__{tool}`. When the model
//! calls them, the gateway runs the calls, adds their results to the conversation and asks the
//! model again, until it answers or calls one of the client's own tools. The client sees one
//! response: the text of every round, and what the gateway ran as its API shows MCP calls (in
//! Responses and Messages; Chat Completions has no place for them, so they are only logged).

use std::{collections::HashMap, sync::Arc};

use futures::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::Inbound;
use crate::{
    error::{ErrorKind, GatewayError},
    ir::{self, Aggregator, BlockKind, ChatRequest, ImageSource, McpItem, Message, Part, Role, StopReason, StreamEvent, Usage},
    labels::Labels,
    mcp::{
        McpServer, ToolsError,
        hub::{SEPARATOR, run_tool},
        router::{self, ToolRoute},
    },
    model::{Deployment, VirtualKey},
    providers::{self, EventResult, EventStream},
    state::AppState,
};

/// Rounds of tool calls in one request, at most.
const MAX_ROUNDS: usize = 8;

/// A gateway MCP server a request names, and which of its tools it wants (all when `None`).
/// Servers a tool router picked are `optional`: one that cannot list its tools is left out
/// rather than failing the request.
#[derive(Debug, PartialEq)]
pub struct Wanted {
    server: String,
    allowed: Option<Vec<String>>,
    optional: bool,
}

/// The gateway MCP tools of a request, as offered to the model.
pub struct Toolset {
    /// By the name the model sees: the server, and the tool's own name there.
    by_name: HashMap<String, (Arc<McpServer>, String)>,
    pub defs: Vec<ir::Tool>,
    /// Each server's tools as it lists them, for the response.
    listed: Vec<(String, Vec<Value>)>,
}

impl Toolset {
    fn has(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }
}

fn invalid(msg: impl Into<String>) -> GatewayError {
    GatewayError::invalid(msg)
}

/// A server's `server_url` or `url` may only point at the gateway's own MCP endpoint for it.
fn check_url(url: Option<&Value>, server: &str) -> Result<(), GatewayError> {
    let Some(url) = url.and_then(Value::as_str).filter(|u| !u.is_empty()) else { return Ok(()) };
    let path = reqwest::Url::parse(url).map(|u| u.path().trim_end_matches('/').to_string()).unwrap_or_default();
    if path.ends_with(&format!("/mcp/{server}")) || path.ends_with("/mcp") {
        return Ok(());
    }
    Err(invalid(format!(
        "only the gateway's MCP servers can be used as tools: leave the URL of '{server}' out, or use the gateway's /mcp/{server}"
    )))
}

/// Tool names as a list (`["a", "b"]`) or as OpenAI's filter (`{"tool_names": [...]}`).
fn tool_names(v: Option<&Value>) -> Option<Vec<String>> {
    let v = v?;
    let list = v.as_array().or_else(|| v.get("tool_names").and_then(Value::as_array))?;
    Some(list.iter().filter_map(Value::as_str).map(str::to_string).collect())
}

/// Takes the gateway MCP servers out of the request, which then goes on without them.
pub fn take(raw: &mut Value, inbound: Inbound) -> Result<Vec<Wanted>, GatewayError> {
    let mut wanted = Vec::new();
    match inbound {
        Inbound::Responses | Inbound::Chat => {
            let Some(tools) = raw.get_mut("tools").and_then(Value::as_array_mut) else { return Ok(wanted) };
            let mut rest = Vec::with_capacity(tools.len());
            for t in tools.drain(..) {
                if t.get("type").and_then(Value::as_str) != Some("mcp") {
                    rest.push(t);
                    continue;
                }
                let server = t.get("server_label").and_then(Value::as_str).ok_or_else(|| invalid("an mcp tool needs its server_label: a gateway MCP server's name"))?;
                check_url(t.get("server_url"), server)?;
                if t.get("authorization").is_some() || t.get("headers").is_some() {
                    return Err(invalid("the gateway signs in to its MCP servers itself: leave authorization and headers out"));
                }
                match t.get("require_approval") {
                    None | Some(Value::Null) => {}
                    Some(Value::String(s)) if s == "never" => {}
                    Some(_) => return Err(invalid("the gateway runs MCP tools without asking: require_approval can only be never")),
                }
                wanted.push(Wanted { server: server.to_string(), allowed: tool_names(t.get("allowed_tools")), optional: false });
            }
            *tools = rest;
        }
        Inbound::Messages => {
            // Toolsets (`{"type": "mcp_toolset", "mcp_server_name": …}`) choose a server's tools.
            let mut chosen: HashMap<String, Vec<String>> = HashMap::new();
            if let Some(tools) = raw.get_mut("tools").and_then(Value::as_array_mut) {
                let mut rest = Vec::with_capacity(tools.len());
                for t in tools.drain(..) {
                    if !t.get("type").and_then(Value::as_str).is_some_and(|ty| ty.starts_with("mcp_toolset")) {
                        rest.push(t);
                        continue;
                    }
                    let Some(server) = t.get("mcp_server_name").and_then(Value::as_str) else { continue };
                    let all = t.pointer("/default_config/enabled").and_then(Value::as_bool).unwrap_or(true);
                    if !all {
                        let on = t.get("configs").and_then(Value::as_object).into_iter().flatten();
                        let on = on.filter(|(_, c)| c.get("enabled").and_then(Value::as_bool) == Some(true)).map(|(k, _)| k.clone());
                        chosen.entry(server.to_string()).or_default().extend(on);
                    }
                }
                *tools = rest;
            }
            let Some(servers) = raw.as_object_mut().and_then(|o| o.remove("mcp_servers")) else { return Ok(wanted) };
            for s in servers.as_array().into_iter().flatten() {
                let server = s.get("name").and_then(Value::as_str).ok_or_else(|| invalid("an MCP server needs its name: a gateway MCP server's name"))?;
                check_url(s.get("url"), server)?;
                if s.get("authorization_token").is_some() {
                    return Err(invalid("the gateway signs in to its MCP servers itself: leave authorization_token out"));
                }
                let configured = s.pointer("/tool_configuration/enabled").and_then(Value::as_bool);
                if configured == Some(false) {
                    continue;
                }
                let allowed = tool_names(s.pointer("/tool_configuration/allowed_tools")).or_else(|| chosen.remove(server));
                wanted.push(Wanted { server: server.to_string(), allowed, optional: false });
            }
        }
    }
    Ok(wanted)
}

/// A function name every provider takes: letters, digits, `_` and `-`, at most 64 characters.
fn function_name(server: &str, tool: &str) -> String {
    let name: String = format!("{server}{SEPARATOR}{tool}").chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
    if name.len() <= 64 {
        return name;
    }
    let hash = hex::encode(&crate::util::sha256(name.as_bytes())[..4]);
    format!("{}_{hash}", &name[..55])
}

/// What a tool router chose for a request, for the response's headers.
#[derive(Debug)]
pub struct Routed {
    pub router: String,
    pub routes: Vec<String>,
    /// `decision`, `fallback` (the decision failed: every route), or `none` (the key may use no route).
    pub how: &'static str,
}

impl Routed {
    /// The `x-gateway-tool-router`, `-routes` and `-route` headers: one entry per router, joined by `; `.
    pub fn headers(routed: &[Routed]) -> [(&'static str, String); 3] {
        let join = |f: &dyn Fn(&Routed) -> String| routed.iter().map(f).collect::<Vec<_>>().join("; ");
        [
            ("x-gateway-tool-router", join(&|r| r.router.clone())),
            ("x-gateway-tool-routes", join(&|r| r.routes.join(","))),
            ("x-gateway-tool-route", join(&|r| r.how.to_string())),
        ]
    }
}

/// Replaces the tool routers a request names by the servers and tools of the routes their
/// decision models pick for it. A failed decision offers every route the key may use.
pub async fn route(state: &AppState, key: &Arc<VirtualKey>, labels: &Labels, raw: &Value, inbound: Inbound, wanted: Vec<Wanted>) -> (Vec<Wanted>, Vec<Routed>) {
    let snap = state.registry.snapshot();
    if !wanted.iter().any(|w| snap.tool_routers.contains_key(&w.server)) {
        return (wanted, Vec::new());
    }
    let transcript = match inbound {
        Inbound::Messages => super::anthropic::parse_request(raw),
        Inbound::Responses => super::responses::parse_request(raw),
        Inbound::Chat => super::chat::parse_request(raw),
    }
    .map(|r| super::routing::transcript(&r));
    let (mut out, mut routed) = (Vec::new(), Vec::new());
    for w in wanted {
        let Some(router) = snap.tool_routers.get(&w.server) else {
            out.push(w);
            continue;
        };
        let usable = router.usable(&snap, key);
        let routes: Vec<&ToolRoute> = usable.iter().map(|(r, _)| *r).collect();
        let decided = match &transcript {
            _ if routes.is_empty() => Ok(Vec::new()),
            Ok(t) => crate::api::decisions::choose_tools(state, key, labels, router, &routes, t.clone()).await.map(|sure| router.pick(&routes, &sure)),
            Err(e) => Err(e.clone()),
        };
        let (picked, how) = match decided {
            Ok(_) if routes.is_empty() => (Vec::new(), "none"),
            Ok(picked) => (picked, "decision"),
            Err(e) => {
                tracing::warn!(router = %router.name, router_id = %router.id, error = %e.message, "tool route decision failed; offering every route");
                (routes.clone(), "fallback")
            }
        };
        let server_of = |r: &ToolRoute| usable.iter().find(|(u, _)| std::ptr::eq(*u, r)).map_or("", |(_, s)| s.name.as_str());
        for (server, allowed) in router::merge(picked.iter().map(|r| (*r, server_of(r)))) {
            out.push(Wanted { server, allowed, optional: true });
        }
        routed.push(Routed { router: router.name.clone(), routes: picked.iter().map(|r| r.name.clone()).collect(), how });
    }
    (out, routed)
}

/// The tools of the servers a request names, as the key may use them.
pub async fn toolset(state: &AppState, key: &VirtualKey, wanted: Vec<Wanted>) -> Result<Toolset, GatewayError> {
    let snap = state.registry.snapshot();
    let mut set = Toolset { by_name: HashMap::new(), defs: Vec::new(), listed: Vec::new() };
    for w in wanted {
        let found = snap
            .mcp_servers
            .get(&w.server)
            .filter(|s| s.allows(key))
            .cloned()
            .ok_or_else(|| GatewayError::not_found(format!("no MCP server '{}' that this key may use", w.server)));
        let found = match found {
            Ok(s) => Ok((s.clone(), state.mcp.tools(state, &s, key.id).await)),
            Err(e) => Err(e),
        };
        let (server, listed) = match found.and_then(|(server, listed)| {
            let listed = listed.map_err(|e| match e {
                ToolsError::Auth(e) => e.into_gateway(),
                ToolsError::Failed(e) => GatewayError {
                    upstream: true,
                    ..GatewayError::new(ErrorKind::Upstream, axum::http::StatusCode::BAD_GATEWAY, format!("MCP server '{}': {e}", server.name))
                },
            })?;
            Ok((server, listed))
        }) {
            Ok(found) => found,
            Err(e) if w.optional => {
                tracing::warn!(server = %w.server, error = %e.message, "routed MCP server left out of the request");
                continue;
            }
            Err(e) => return Err(e),
        };
        let mut offered = Vec::new();
        for t in listed.iter() {
            let Some(tool) = t.get("name").and_then(Value::as_str) else { continue };
            if !server.allows_tool(tool) || w.allowed.as_ref().is_some_and(|a| !a.iter().any(|x| x == tool)) {
                continue;
            }
            offered.push(t.clone());
            let name = function_name(&server.name, tool);
            // Named twice, directly and through a tool router: offered once.
            if set.by_name.contains_key(&name) {
                continue;
            }
            set.defs.push(ir::Tool {
                name: name.clone(),
                description: t.get("description").and_then(Value::as_str).map(str::to_string),
                parameters: t.get("inputSchema").cloned().filter(Value::is_object).unwrap_or_else(|| json!({ "type": "object" })),
                cache: false,
            });
            set.by_name.insert(name, (server.clone(), tool.to_string()));
        }
        set.listed.push((server.name.clone(), offered));
    }
    Ok(set)
}

/// Adds the toolset to a parsed request; a client tool of the same name is refused.
pub fn offer(ir: &mut ChatRequest, set: &Toolset) -> Result<(), GatewayError> {
    if let Some(t) = ir.tools.iter().find(|t| set.has(&t.name)) {
        return Err(invalid(format!("the tool name '{}' is taken by a gateway MCP tool", t.name)));
    }
    ir.tools.extend(set.defs.iter().cloned());
    Ok(())
}

/// A tool's result as the model gets it.
fn result_parts(result: &Value) -> (Vec<Part>, bool) {
    let mut parts = Vec::new();
    for c in result.get("content").and_then(Value::as_array).into_iter().flatten() {
        match c.get("type").and_then(Value::as_str) {
            Some("text") => parts.push(Part::Text { text: c.get("text").and_then(Value::as_str).unwrap_or_default().to_string(), cache: false }),
            Some("image") => {
                let (Some(data), Some(media_type)) = (c.get("data").and_then(Value::as_str), c.get("mimeType").and_then(Value::as_str)) else { continue };
                parts.push(Part::Image { source: ImageSource::Base64 { media_type: media_type.to_string(), data: data.to_string() }, cache: false });
            }
            // Resources, links and audio as their JSON: the model can read what they say.
            _ => parts.push(Part::Text { text: c.to_string(), cache: false }),
        }
    }
    if parts.is_empty()
        && let Some(s) = result.get("structuredContent")
    {
        parts.push(Part::Text { text: s.to_string(), cache: false });
    }
    (parts, result.get("isError").and_then(Value::as_bool) == Some(true))
}

/// Who calls, for the tool calls.
#[derive(Clone)]
pub struct Caller {
    pub key: Arc<VirtualKey>,
    pub labels: Labels,
}

/// Runs a request with gateway MCP tools on `d`: the first round is asked for here, so an
/// upstream that fails at once can still be failed over; the rounds after it run in their own
/// task, as the client reads the answer. The events are one response: the servers' tools first,
/// blocks renumbered, the gateway's tool calls as [`McpItem`]s once they ran, one `Finish` with
/// every round's usage.
pub async fn start(state: &AppState, d: &Arc<Deployment>, ir: ChatRequest, set: Arc<Toolset>, caller: Caller) -> Result<(StreamEvent, EventStream), GatewayError> {
    let mut events = providers::stream_chat(state, d, &ir).await?;
    let first = match events.next().await {
        Some(Ok(first)) => first,
        Some(Err(e)) => return Err(e),
        None => return Err(GatewayError::upstream_stream(ErrorKind::Upstream, "empty stream")),
    };
    let events: EventStream = Box::pin(futures::stream::once(async { Ok(first) }).chain(events));
    let (tx, mut rx) = mpsc::channel::<EventResult>(64);
    let (state, d) = (state.clone(), d.clone());
    tokio::spawn(async move { rounds(state, d, ir, set, caller, events, tx).await });
    let first = rx.recv().await.unwrap_or_else(|| Err(GatewayError::upstream_stream(ErrorKind::Upstream, "empty stream")))?;
    let rest = futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|ev| (ev, rx)) });
    Ok((first, Box::pin(rest)))
}

async fn rounds(state: AppState, d: Arc<Deployment>, mut ir: ChatRequest, set: Arc<Toolset>, caller: Caller, mut events: EventStream, tx: mpsc::Sender<EventResult>) {
    // `false` once the client stopped reading: the rounds stop too.
    let send = async |ev: EventResult| tx.send(ev).await.is_ok();
    let mut next_index = 0;
    let mut usage = Usage::default();
    for (server, tools) in &set.listed {
        let item = McpItem::Tools { server: server.clone(), tools: tools.clone() };
        if !send(Ok(StreamEvent::Mcp { index: next_index, item })).await {
            return;
        }
        next_index += 1;
    }
    for round in 0.. {
        let mut agg = Aggregator::default();
        // This round's block index → the client's, `None` for the gateway's tool calls.
        let mut index: HashMap<usize, Option<usize>> = HashMap::new();
        let mut stop = StopReason::EndTurn;
        while let Some(ev) = events.next().await {
            let ev = match ev {
                Ok(ev) => ev,
                Err(e) => {
                    send(Err(e)).await;
                    return;
                }
            };
            agg.push(&ev);
            let out = match ev {
                StreamEvent::BlockStart { index: i, kind } => {
                    let gateways = matches!(&kind, BlockKind::ToolCall { name, .. } if set.has(name));
                    let to = (!gateways).then(|| {
                        next_index += 1;
                        next_index - 1
                    });
                    index.insert(i, to);
                    to.map(|index| StreamEvent::BlockStart { index, kind })
                }
                StreamEvent::TextDelta { index: i, text } => index.get(&i).copied().flatten().map(|index| StreamEvent::TextDelta { index, text }),
                StreamEvent::ToolArgsDelta { index: i, json } => index.get(&i).copied().flatten().map(|index| StreamEvent::ToolArgsDelta { index, json }),
                StreamEvent::ReasoningDelta { index: i, text } => index.get(&i).copied().flatten().map(|index| StreamEvent::ReasoningDelta { index, text }),
                StreamEvent::SignatureDelta { index: i, signature } => {
                    index.get(&i).copied().flatten().map(|index| StreamEvent::SignatureDelta { index, signature })
                }
                StreamEvent::BlockStop { index: i } => index.get(&i).copied().flatten().map(|index| StreamEvent::BlockStop { index }),
                StreamEvent::Finish { stop_reason, usage: u } => {
                    usage.add(&u);
                    stop = stop_reason;
                    break;
                }
                ev @ (StreamEvent::ServerTool { .. } | StreamEvent::Annotation { .. } | StreamEvent::Mcp { .. }) => Some(ev),
            };
            if let Some(ev) = out
                && !send(Ok(ev)).await
            {
                return;
            }
        }
        let answer = agg.finish();
        let calls: Vec<(String, String, String)> = answer
            .content
            .iter()
            .filter_map(|p| match p {
                Part::ToolCall { id, name, arguments } if set.has(name) => Some((id.clone(), name.clone(), arguments.clone())),
                _ => None,
            })
            .collect();
        let client_calls = answer.content.iter().any(|p| matches!(p, Part::ToolCall { name, .. } if !set.has(name)));
        // Done: the model answered, asked for the client's tools (which the client runs), or ran
        // out of rounds. Calls of the gateway's tools in a last round are not run.
        if calls.is_empty() || client_calls || round + 1 >= MAX_ROUNDS {
            if !calls.is_empty() {
                tracing::warn!(rounds = round + 1, calls = calls.len(), "MCP tool calls left unrun: the model also called client tools, or ran out of rounds");
            }
            let stop_reason = if client_calls { StopReason::ToolUse } else if stop == StopReason::ToolUse { StopReason::EndTurn } else { stop };
            send(Ok(StreamEvent::Finish { stop_reason, usage })).await;
            return;
        }
        let results = futures::future::join_all(calls.iter().map(|(id, name, arguments)| {
            let (server, tool) = set.by_name[name].clone();
            let (state, caller) = (state.clone(), caller.clone());
            let arguments = ir::parse_args(arguments);
            async move {
                let result = run_tool(&state, &caller.key, &caller.labels, &server, &tool, arguments.clone(), None).await;
                (id.clone(), server, tool, arguments, result)
            }
        }))
        .await;
        let mut parts = Vec::with_capacity(results.len());
        for (id, server, tool, arguments, result) in results {
            let (content, is_error, result) = match result {
                Ok(r) => {
                    let (content, is_error) = result_parts(&r);
                    (content, is_error, Ok(r))
                }
                Err(e) => {
                    let message = e.message();
                    (vec![Part::Text { text: message.clone(), cache: false }], true, Err(message))
                }
            };
            let item = McpItem::Call { server: server.name.clone(), tool, arguments, result };
            if !send(Ok(StreamEvent::Mcp { index: next_index, item })).await {
                return;
            }
            next_index += 1;
            parts.push(Part::ToolResult { call_id: id, content, is_error, cache: false });
        }
        ir.messages.push(Message { role: Role::Assistant, content: answer.content });
        ir.messages.push(Message { role: Role::User, content: parts });
        events = match providers::stream_chat(&state, &d, &ir).await {
            Ok(e) => e,
            Err(e) => {
                send(Err(e)).await;
                return;
            }
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_tools_are_taken_out_of_each_api() {
        let mut raw = json!({ "tools": [
            { "type": "function", "name": "f" },
            { "type": "mcp", "server_label": "github", "require_approval": "never", "allowed_tools": ["search"] },
            { "type": "mcp", "server_label": "docs", "server_url": "https://gw.example.com/mcp/docs", "allowed_tools": { "tool_names": ["read"] } },
        ] });
        let wanted = take(&mut raw, Inbound::Responses).unwrap();
        assert_eq!(wanted, [
            Wanted { server: "github".into(), allowed: Some(vec!["search".into()]), optional: false },
            Wanted { server: "docs".into(), allowed: Some(vec!["read".into()]), optional: false },
        ]);
        assert_eq!(raw["tools"], json!([{ "type": "function", "name": "f" }]));

        let refused = |t: Value| take(&mut json!({ "tools": [t] }), Inbound::Chat).unwrap_err().message;
        assert!(refused(json!({ "type": "mcp", "server_label": "x", "server_url": "https://mcp.other.com/sse" })).contains("only the gateway's"));
        assert!(refused(json!({ "type": "mcp", "server_label": "x", "require_approval": "always" })).contains("never"));
        assert!(refused(json!({ "type": "mcp", "server_label": "x", "authorization": "t" })).contains("signs in"));

        let mut raw = json!({
            "mcp_servers": [{ "type": "url", "name": "github", "url": "https://gw.example.com/mcp/github" }, { "type": "url", "name": "off", "tool_configuration": { "enabled": false } }],
            "tools": [{ "type": "mcp_toolset", "mcp_server_name": "github", "default_config": { "enabled": false }, "configs": { "search": { "enabled": true } } }, { "name": "f" }],
        });
        let wanted = take(&mut raw, Inbound::Messages).unwrap();
        assert_eq!(wanted, [Wanted { server: "github".into(), allowed: Some(vec!["search".into()]), optional: false }]);
        assert!(raw.get("mcp_servers").is_none());
        assert_eq!(raw["tools"], json!([{ "name": "f" }]));
    }

    #[test]
    fn function_names_suit_every_provider() {
        assert_eq!(function_name("github", "search_issues"), "github__search_issues");
        assert_eq!(function_name("docs", "files.read"), "docs__files_read");
        let long = function_name("server", &"t".repeat(100));
        assert_eq!(long.len(), 64);
        assert_ne!(long, function_name("server", &"t".repeat(101)));
    }

    #[test]
    fn tool_results_become_the_models_content() {
        let (parts, is_error) = result_parts(&json!({ "content": [{ "type": "text", "text": "hi" }, { "type": "image", "data": "AAA", "mimeType": "image/png" }], "isError": true }));
        assert!(is_error);
        assert!(matches!(&parts[0], Part::Text { text, .. } if text == "hi"));
        assert!(matches!(&parts[1], Part::Image { source: ImageSource::Base64 { media_type, .. }, .. } if media_type == "image/png"));
        let (parts, _) = result_parts(&json!({ "content": [], "structuredContent": { "n": 1 } }));
        assert!(matches!(&parts[0], Part::Text { text, .. } if text == r#"{"n":1}"#));
    }
}
