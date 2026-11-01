//! MCP gateway: the MCP servers set up in the console, proxied at `/mcp/{name}` over Streamable
//! HTTP. Clients authenticate with their virtual key; the gateway calls the server with its own
//! secret, which never leaves it. Messages pass through as they are. The gateway reads only
//! `tools/call` requests and their replies: each call gets an entry in the request log (api `mcp`,
//! the server as its model), counts against the key's limits and budgets, and has its call and reply
//! kept where the key's prompts are logged.
//!
//! The upstream's session ids are handed to clients bound to their key (see [`SessionTag`]), so a
//! session one key opened cannot be used with another, and any replica can forward any session.

use std::{
    collections::{BTreeMap, HashMap},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::{Duration, Instant},
};

use aws_lc_rs::hmac;
use axum::{
    body::{Body, Bytes},
    extract::{Path, State},
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use futures::{Stream, StreamExt, stream::BoxStream};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use uuid::Uuid;

pub mod hub;
pub mod router;

use crate::{
    accounting::Tracker,
    auth, capture,
    crypto::SecretBox,
    error::{Dialect, ErrorKind, GatewayError},
    labels::Labels,
    mcp_auth::McpKey,
    model::VirtualKey,
    oauth::{Kind, OAuthConfig, Slot, UpstreamAuth, Who},
    sse::SseParser,
    state::AppState,
};

/// An MCP server as stored.
#[derive(Debug, Clone)]
pub struct McpServerRow {
    pub id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub url: String,
    pub auth: String,
    pub auth_header: Option<String>,
    /// Sealed with the gateway's encryption key.
    pub secret: Option<Vec<u8>>,
    /// The OAuth client, for `oauth` and `oauth_user`.
    pub oauth: Option<sqlx::types::Json<OAuthConfig>>,
    pub tags: Vec<String>,
    pub project_ids: Vec<Uuid>,
    pub timeout_secs: i32,
    pub enabled: bool,
    pub allowed_tools: Vec<String>,
    pub denied_tools: Vec<String>,
    pub price_per_call: f64,
    pub tool_prices: sqlx::types::Json<BTreeMap<String, f64>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// How the gateway authenticates to an upstream: an MCP server or an A2A agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpAuth {
    /// No credentials: a server on a private network, or one that needs none.
    None,
    /// `Authorization: Bearer <secret>`.
    Bearer,
    /// The secret as the value of a header of the server's choice, such as `X-API-Key`.
    Header,
    /// OAuth tokens the gateway gets with its own client credentials.
    #[serde(rename = "oauth")]
    OAuth,
    /// OAuth tokens each key gets by signing in, in the console.
    #[serde(rename = "oauth_user")]
    OAuthUser,
}

impl McpAuth {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "none" => Some(Self::None),
            "bearer" => Some(Self::Bearer),
            "header" => Some(Self::Header),
            "oauth" => Some(Self::OAuth),
            "oauth_user" => Some(Self::OAuthUser),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Bearer => "bearer",
            Self::Header => "header",
            Self::OAuth => "oauth",
            Self::OAuthUser => "oauth_user",
        }
    }
}

/// An enabled MCP server, ready to call: its secret decrypted once.
pub struct McpServer {
    pub id: Uuid,
    pub name: String,
    pub url: String,
    /// How the gateway authenticates to the server.
    pub auth: UpstreamAuth,
    pub tags: Vec<String>,
    pub project_ids: Vec<Uuid>,
    /// For a POST's answer, streamed or not. A client's GET stream stays open as long as both
    /// sides keep it.
    pub timeout: Duration,
    /// The tools keys may call: empty for all of them, but never the denied ones.
    allowed_tools: Vec<String>,
    denied_tools: Vec<String>,
    /// USD per call, by tool, else `price_per_call`.
    price_per_call: f64,
    tool_prices: BTreeMap<String, f64>,
}

impl McpServer {
    pub fn build(row: McpServerRow, sb: &SecretBox) -> Result<Self, String> {
        let ctx = |e: String| format!("MCP server {} ({}): {e}", row.id, row.name);
        let oauth = row.oauth.as_ref().map(|o| &o.0);
        let auth =
            UpstreamAuth::build(Kind::Mcp, row.id, &row.name, &row.auth, row.auth_header.as_deref(), row.secret.as_deref(), oauth, sb)
                .map_err(ctx)?;
        Ok(Self {
            id: row.id,
            name: row.name,
            url: row.url,
            auth,
            tags: row.tags,
            project_ids: row.project_ids,
            timeout: Duration::from_secs(row.timeout_secs.max(1) as u64),
            allowed_tools: row.allowed_tools,
            denied_tools: row.denied_tools,
            price_per_call: row.price_per_call,
            tool_prices: row.tool_prices.0,
        })
    }

    /// Whether keys may see and call this tool of the server.
    pub fn allows_tool(&self, tool: &str) -> bool {
        (self.allowed_tools.is_empty() || self.allowed_tools.iter().any(|t| t == tool)) && !self.denied_tools.iter().any(|t| t == tool)
    }

    /// Whether some of the server's tools are hidden from keys.
    fn filters_tools(&self) -> bool {
        !self.allowed_tools.is_empty() || !self.denied_tools.is_empty()
    }

    /// What a call of `tool` costs, in USD.
    pub fn price(&self, tool: &str) -> f64 {
        self.tool_prices.get(tool).copied().unwrap_or(self.price_per_call)
    }

    pub fn allows(&self, key: &VirtualKey) -> bool {
        open_to(key, &self.project_ids, &self.tags)
    }
}

/// As for deployments: an upstream limited to projects is only for keys in one of them, and a key
/// with tags may use what carries no tag it lacks.
pub fn open_to(key: &VirtualKey, project_ids: &[Uuid], tags: &[String]) -> bool {
    let in_project = project_ids.is_empty() || key.project_id.is_some_and(|p| project_ids.contains(&p));
    in_project && key.holds_all(tags)
}

/// Headers of the client's request the server gets, with the `Mcp-Param-*` ones (SEP-2243);
/// everything else (the client's key above all) stays at the gateway.
const FORWARDED: [&str; 6] = ["accept", "content-type", "mcp-protocol-version", "last-event-id", "mcp-method", "mcp-name"];
const FORWARDED_PREFIX: &str = "mcp-param-";
/// Headers of the server's answer the client gets. Not `WWW-Authenticate`: a client must not start
/// signing in to the server, whose credentials are the gateway's.
const RETURNED: [&str; 2] = ["content-type", "mcp-protocol-version"];
const SESSION: &str = "mcp-session-id";
/// The answer to a POST is read up to this size for its replies; it is forwarded whole either way.
const MAX_READ: usize = 8 * 1024 * 1024;

/// `POST`, `GET` and `DELETE /mcp/{server}`.
pub async fn proxy(
    State(state): State<AppState>,
    McpKey(key): McpKey,
    Path(name): Path<String>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    forward(&state, &key, &name, method, &headers, body).await.unwrap_or_else(|e| e.into_response_for(Dialect::OpenAi))
}

async fn forward(
    state: &AppState,
    key: &Arc<VirtualKey>,
    name: &str,
    method: Method,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<Response, GatewayError> {
    let server = state
        .registry
        .snapshot()
        .mcp_servers
        .get(name)
        .filter(|s| s.allows(key))
        .cloned()
        .ok_or_else(|| GatewayError::not_found(format!("no MCP server '{name}' that this key may use")))?;
    let labels = Labels::from_headers(headers)?;
    let messages = if method == Method::POST { messages(&body) } else { Vec::new() };
    // A `tools/call` is a request: one without an id (a string or a number) is refused here rather
    // than passed to a server that might run it unanswered, unlimited, unlogged and uncharged.
    if let Some(m) =
        messages.iter().find(|m| m.get("method").and_then(Value::as_str) == Some("tools/call") && m.get("id").and_then(id_key).is_none())
    {
        let mut t = Tracker::new(state, Some(key), &labels, &server.name, "mcp", false);
        t.tool(m.pointer("/params/name").and_then(Value::as_str).unwrap_or("?"));
        t.fail_status(400, "tools/call without an id");
        if messages.len() > 1 {
            return Err(GatewayError::invalid("every tools/call needs an id that is a string or a number"));
        }
        let error = json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32600, "message": "tools/call needs an id that is a string or a number" } });
        return Ok(axum::Json(error).into_response());
    }
    // A call of a tool the server's filters hide is answered here, as one of an unknown tool.
    if let Some((id, tool)) = messages.iter().filter_map(call_of).find(|(_, tool)| !server.allows_tool(tool)) {
        let mut t = Tracker::new(state, Some(key), &labels, &server.name, "mcp", false);
        t.tool(tool);
        t.fail_status(403, "the tool is not available through the gateway");
        if messages.len() > 1 {
            return Err(GatewayError::forbidden(format!("the tool '{tool}' is not available through the gateway")));
        }
        let error = json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32602, "message": format!("Unknown tool: {tool}") } });
        return Ok(axum::Json(error).into_response());
    }
    // `tools/list` answers are read whole, to take out the hidden tools.
    let lists: Vec<String> = match server.filters_tools() {
        true => messages
            .iter()
            .filter(|m| m.get("method").and_then(Value::as_str) == Some("tools/list"))
            .filter_map(|m| m.get("id").and_then(id_key))
            .collect(),
        false => Vec::new(),
    };
    let mut calls = Calls::new(state, key, &labels, &server, &messages);
    if !calls.is_empty()
        && let Err(e) = auth::check_key_limits(state, key).await
    {
        calls.fail(&e);
        return Err(e);
    }
    let tag = SessionTag::new(state, key, &server);
    let session = match headers.get(SESSION) {
        // A session the key did not open is as unknown as one the server ended: the client starts over.
        Some(v) => Some(tag.open(v.as_bytes()).ok_or_else(|| {
            let e = GatewayError::not_found("unknown MCP session");
            calls.fail(&e);
            e
        })?),
        None => None,
    };
    if let Err(why) = state.cfg.upstream_policy.check_url(&server.url) {
        let e = GatewayError::internal(format!("the MCP server's address is not allowed: {why}"));
        calls.fail(&e);
        return Err(e);
    }

    let send = async |auth: Option<(HeaderName, HeaderValue)>| {
        let mut req = state.http.request(method.clone(), &server.url);
        for (h, v) in headers {
            if FORWARDED.contains(&h.as_str()) || h.as_str().starts_with(FORWARDED_PREFIX) {
                req = req.header(h, v);
            }
        }
        if let Some(s) = session {
            req = req.header(SESSION, s);
        }
        if let Some((h, v)) = auth {
            req = req.header(h, v);
        }
        if method == Method::POST {
            req = req.timeout(server.timeout).body(body.clone());
        }
        req.send().await.map_err(|e| GatewayError::from_reqwest(&e))
    };
    let who = Who { key: key.id, labels: &labels };
    let auth = server.auth.header(state, Some(who)).await.map_err(|e| {
        let e = e.into_gateway();
        calls.fail(&e);
        e
    })?;
    let mut resp = send(auth).await;
    // A token the server refuses is renewed, once.
    if server.auth.renews() && resp.as_ref().is_ok_and(|r| r.status() == StatusCode::UNAUTHORIZED) {
        server.auth.renew(state, Some(who)).await;
        let auth = server.auth.header(state, Some(who)).await.map_err(|e| {
            let e = e.into_gateway();
            calls.fail(&e);
            e
        })?;
        resp = send(auth).await;
    }
    let resp = resp.inspect_err(|e| calls.fail(e))?;
    let status = resp.status();
    if matches!(status.as_u16(), 401 | 403) {
        tracing::warn!(server = %server.name, status = status.as_u16(), "MCP server refused the gateway's credentials");
        let e = GatewayError {
            upstream: true,
            ..GatewayError::new(
                ErrorKind::Upstream,
                StatusCode::BAD_GATEWAY,
                format!("the MCP server refused the gateway's credentials (HTTP {})", status.as_u16()),
            )
        };
        calls.fail(&e);
        return Err(e);
    }

    let mut out = Response::builder().status(status);
    for h in RETURNED {
        if let Some(v) = resp.headers().get(h) {
            out = out.header(h, v);
        }
    }
    if let Some(s) = resp.headers().get(SESSION).and_then(|v| tag.seal(v.as_bytes())) {
        out = out.header(SESSION, s);
    }
    let stream = resp.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|t| t.starts_with("text/event-stream"));
    if !lists.is_empty() && status.is_success() {
        let body = filtered(resp, stream, &server, &lists, &mut calls).await.inspect_err(|e| calls.fail(e))?;
        return Ok(out.body(Body::from(body)).unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response()));
    }
    let upstream = resp.bytes_stream().boxed();
    let body = if calls.is_empty() {
        Body::from_stream(upstream)
    } else if !status.is_success() {
        calls.fail_status(status.as_u16(), format!("the MCP server answered HTTP {}", status.as_u16()));
        Body::from_stream(upstream)
    } else {
        let reader = if stream { Reader::Sse(SseParser::default()) } else { Reader::Json(Vec::new()) };
        Body::from_stream(Watch { inner: upstream, calls, reader })
    };
    Ok(out.body(body).unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response()))
}

/// The JSON-RPC messages in a POST: one, or a batch (protocol versions before 2025-06-18). A body
/// that is not JSON goes to the server as it is, which answers it.
fn messages(body: &[u8]) -> Vec<Value> {
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Array(batch)) => batch,
        Ok(v @ Value::Object(_)) => vec![v],
        _ => Vec::new(),
    }
}

/// A `tools/call` request's id and tool.
fn call_of(m: &Value) -> Option<(&Value, &str)> {
    (m.get("method").and_then(Value::as_str) == Some("tools/call")).then_some(())?;
    Some((m.get("id")?, m.pointer("/params/name").and_then(Value::as_str).unwrap_or("?")))
}

/// The answer to a POST with `tools/list` requests, read whole, without the tools the server's
/// filters hide; the replies to tool calls in it settle those calls.
async fn filtered(
    resp: reqwest::Response,
    sse: bool,
    server: &McpServer,
    lists: &[String],
    calls: &mut Calls,
) -> Result<Bytes, GatewayError> {
    let mut body = resp.bytes_stream();
    let mut buf = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| GatewayError::from_reqwest(&e))?;
        if buf.len() + chunk.len() > MAX_READ {
            return Err(GatewayError::new(ErrorKind::Upstream, StatusCode::BAD_GATEWAY, "the MCP server's answer is too large"));
        }
        buf.extend_from_slice(&chunk);
    }
    calls.ended = true;
    let mut edit = |m: &mut Value| {
        calls.settle(m);
        if m.get("id").and_then(id_key).is_some_and(|id| lists.contains(&id))
            && let Some(tools) = m.pointer_mut("/result/tools").and_then(Value::as_array_mut)
        {
            tools.retain(|t| t.get("name").and_then(Value::as_str).is_some_and(|n| server.allows_tool(n)));
        }
    };
    if !sse {
        let mut v: Value = serde_json::from_slice(&buf)
            .map_err(|_| GatewayError::new(ErrorKind::Upstream, StatusCode::BAD_GATEWAY, "the MCP server's answer is not JSON"))?;
        match &mut v {
            Value::Array(batch) => batch.iter_mut().for_each(&mut edit),
            m => edit(m),
        }
        return Ok(Bytes::from(v.to_string()));
    }
    // Events are written again with their name and data; ids and retry hints, which only matter for
    // resuming a stream, are left out of this one, which is complete.
    let mut parser = SseParser::default();
    let mut out = String::new();
    let mut events = parser.feed(&buf);
    events.extend(parser.finish());
    for event in events {
        if let Some(name) = &event.event {
            out.push_str(&format!("event: {name}\n"));
        }
        let data = match serde_json::from_str::<Value>(&event.data) {
            Ok(mut m) => {
                edit(&mut m);
                m.to_string()
            }
            Err(_) => event.data,
        };
        for line in data.split('\n') {
            out.push_str(&format!("data: {line}\n"));
        }
        out.push('\n');
    }
    Ok(Bytes::from(out))
}

/// A JSON-RPC id as a map key: `1` and `"1"` are different ids.
fn id_key(id: &Value) -> Option<String> {
    matches!(id, Value::String(_) | Value::Number(_)).then(|| id.to_string())
}

/// The `tools/call` requests of a POST, by id, each with its entry in the request log. An entry is
/// written when its call is dropped: once its reply came, it failed, or the exchange ended without a
/// reply (see `Drop`).
struct Calls {
    /// Each call's entry, and what the call costs once the tool answers.
    open: HashMap<String, (Tracker, f64)>,
    /// The server's answer ran to its end (rather than the client going away first).
    ended: bool,
}

impl Calls {
    fn new(state: &AppState, key: &VirtualKey, labels: &Labels, server: &McpServer, messages: &[Value]) -> Self {
        let mut calls = HashMap::new();
        for m in messages {
            let Some((Some(id), tool)) = call_of(m).map(|(id, tool)| (id_key(id), tool)) else { continue };
            let mut t = Tracker::new(state, Some(key), labels, &server.name, "mcp", false);
            t.tool(tool);
            if let Some(c) = t.log() {
                c.request(capture::Body::Json(m.clone()));
            }
            calls.insert(id, (t, server.price(tool)));
        }
        Self { open: calls, ended: false }
    }

    fn is_empty(&self) -> bool {
        self.open.is_empty()
    }

    /// Ends every open call with the error.
    fn fail(&mut self, e: &GatewayError) {
        self.open.drain().for_each(|(_, (mut t, _))| t.fail(e));
    }

    fn fail_status(&mut self, status: u16, msg: impl Into<String>) {
        let msg = msg.into();
        self.open.drain().for_each(|(_, (mut t, _))| t.fail_status(status, msg.clone()));
    }

    /// A message from the server: the reply to one of the calls ends its entry. Requests and
    /// notifications the server sends meanwhile (progress, sampling) are not the gateway's.
    fn settle(&mut self, reply: &Value) {
        let Some((mut t, price)) = reply.get("id").and_then(id_key).and_then(|id| self.open.remove(&id)) else { return };
        // A tool that answered is charged, also when it reports an error: it ran.
        if reply.get("result").is_some() {
            t.charge(price);
        }
        if let Some(e) = reply.get("error") {
            let code = e.get("code").and_then(Value::as_i64).unwrap_or(0);
            let status = match code {
                -32600 | -32602 => 400,
                -32601 => 404,
                _ => 502,
            };
            let message = e.get("message").and_then(Value::as_str).unwrap_or("no message");
            t.fail_status(status, format!("JSON-RPC error {code}: {message}"));
        } else if reply.pointer("/result/isError").and_then(Value::as_bool) == Some(true) {
            // The tool ran and failed, as its result says: 422, so the log and the metrics count it.
            let text = reply
                .pointer("/result/content")
                .and_then(Value::as_array)
                .and_then(|c| c.iter().find_map(|p| p.get("text").and_then(Value::as_str)))
                .unwrap_or("the tool reported an error");
            t.fail_status(422, text);
        }
        if let Some(c) = t.log() {
            c.response(capture::Body::Json(reply.clone()));
        }
    }
}

impl Drop for Calls {
    fn drop(&mut self) {
        let (status, msg) = match self.ended {
            true => (502, "the MCP server's answer had no reply to this call"),
            // 499, as nginx logs it: the client closed the request before it finished.
            false => (499, "the client went away before the tool answered"),
        };
        for (_, (mut t, _)) in self.open.drain() {
            if let Some(c) = t.log().filter(|_| !self.ended) {
                c.incomplete();
            }
            t.fail_status(status, msg);
        }
    }
}

/// How the replies are read from the answer to a POST.
enum Reader {
    /// `application/json`: one message (or a batch), read once the body is complete.
    Json(Vec<u8>),
    /// `text/event-stream`: messages as events, read as they arrive.
    Sse(SseParser),
}

/// The answer to a POST with tool calls, passed on to the client as it arrives, read on the way.
struct Watch {
    inner: BoxStream<'static, reqwest::Result<Bytes>>,
    calls: Calls,
    reader: Reader,
}

impl Watch {
    fn read(&mut self, chunk: &[u8]) {
        match &mut self.reader {
            Reader::Json(buf) => {
                if buf.len() + chunk.len() <= MAX_READ {
                    buf.extend_from_slice(chunk);
                }
            }
            Reader::Sse(parser) => {
                for event in parser.feed(chunk) {
                    settle_all(&mut self.calls, &event.data);
                }
            }
        }
    }

    fn end(&mut self) {
        self.calls.ended = true;
        match &mut self.reader {
            Reader::Json(buf) => {
                let buf = std::mem::take(buf);
                settle_all(&mut self.calls, &String::from_utf8_lossy(&buf));
            }
            Reader::Sse(parser) => {
                for event in parser.finish() {
                    settle_all(&mut self.calls, &event.data);
                }
            }
        }
    }
}

fn settle_all(calls: &mut Calls, data: &str) {
    for m in messages(data.as_bytes()) {
        calls.settle(&m);
    }
}

impl Stream for Watch {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.inner.poll_next_unpin(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                self.read(&chunk);
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(e))) => {
                // The calls still open end here, with the error.
                let e = GatewayError::from_reqwest(&e);
                self.calls.fail(&e);
                Poll::Ready(Some(Err(std::io::Error::other(e.message))))
            }
            Poll::Ready(None) => {
                self.end();
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Binds the server's session ids to the key and server they were opened with: clients get
/// `<tag><session id>`, the tag a keyed hash of all three, and the gateway forwards a session only
/// with the key that opened it. Keyed with a secret derived from the encryption key, so every
/// replica checks it the same.
struct SessionTag {
    key: hmac::Key,
    prefix: Vec<u8>,
}

/// Hex characters of the tag before the server's session id.
const TAG_LEN: usize = 32;

impl SessionTag {
    fn new(state: &AppState, key: &VirtualKey, server: &McpServer) -> Self {
        let mut prefix = Vec::with_capacity(32);
        prefix.extend_from_slice(key.id.as_bytes());
        prefix.extend_from_slice(server.id.as_bytes());
        Self { key: hmac::Key::new(hmac::HMAC_SHA256, &state.cfg.affinity_salt), prefix }
    }

    fn tag(&self, session: &[u8]) -> String {
        let mut ctx = hmac::Context::with_key(&self.key);
        ctx.update(b"mcp-session\n");
        ctx.update(&self.prefix);
        ctx.update(session);
        hex::encode(&ctx.sign().as_ref()[..TAG_LEN / 2])
    }

    /// The id the client gets for the server's session id.
    fn seal(&self, session: &[u8]) -> Option<HeaderValue> {
        HeaderValue::from_str(&format!("{}{}", self.tag(session), std::str::from_utf8(session).ok()?)).ok()
    }

    /// The server's session id in the one a client sent, when this key and server opened it.
    fn open<'a>(&self, presented: &'a [u8]) -> Option<&'a [u8]> {
        if presented.len() <= TAG_LEN {
            return None;
        }
        let (tag, session) = presented.split_at(TAG_LEN);
        bool::from(tag.ct_eq(self.tag(session).as_bytes())).then_some(session)
    }
}

// ---------------------------------------------------------------- the gateway as a client

/// What a server says about itself: from `initialize` and `tools/list`, for the console's test.
#[derive(Serialize, utoipa::ToSchema)]
pub struct Probe {
    /// The protocol version the server agreed to.
    pub protocol_version: Option<String>,
    /// The server's `serverInfo`: its name and version.
    #[schema(value_type = Option<Object>)]
    pub server_info: Option<Value>,
    pub tools: Vec<ProbedTool>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct ProbedTool {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// The protocol version the gateway asks for when it talks to a server itself: the newest with an
/// `initialize` handshake, which servers of every version still answer.
const PROTOCOL_VERSION: &str = "2025-11-25";

/// A session the gateway opened with a server.
#[derive(Clone)]
struct Session {
    /// None when the server keeps no sessions.
    id: Option<String>,
    version: Option<String>,
    /// The header the gateway authenticates with.
    auth: Option<(HeaderName, HeaderValue)>,
}

/// Why `/mcp` cannot list a server's tools for a key.
pub enum ToolsError {
    Auth(crate::oauth::AuthError),
    Failed(String),
}

/// Opens a session with the server, as a client would, with `auth` as the gateway's credentials.
async fn open(state: &AppState, server: &McpServer, auth: Option<(HeaderName, HeaderValue)>) -> Result<(Session, Value), RpcError> {
    state.cfg.upstream_policy.check_url(&server.url).map_err(|why| RpcError::other(format!("the address is not allowed: {why}")))?;
    let init = json!({
        "jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "inference-gateway", "version": crate::VERSION },
        },
    });
    let (id, reply) = rpc(state, server, &Session { id: None, version: None, auth: auth.clone() }, &init).await?;
    let result = reply.get("result").cloned().ok_or_else(|| RpcError::other("initialize did not succeed"))?;
    let version = result.get("protocolVersion").and_then(Value::as_str).map(str::to_string);
    let session = Session { id, version, auth };
    rpc(state, server, &session, &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).await?;
    Ok((session, result))
}

/// Every tool the server offers, page by page, as the server describes them.
async fn tools(state: &AppState, server: &McpServer, session: &Session) -> Result<Vec<Value>, RpcError> {
    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;
    // Pages until the server has no more, within reason.
    for page in 1..=10 {
        let mut list = json!({ "jsonrpc": "2.0", "id": page, "method": "tools/list" });
        if let Some(c) = &cursor {
            list["params"] = json!({ "cursor": c });
        }
        let (_, reply) = rpc(state, server, session, &list).await?;
        let result = reply.get("result").ok_or_else(|| RpcError::other("tools/list did not succeed"))?;
        tools.extend(result.get("tools").and_then(Value::as_array).into_iter().flatten().cloned());
        cursor = result.get("nextCursor").and_then(Value::as_str).map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }
    Ok(tools)
}

/// Ends a session the gateway opened, if the server keeps sessions.
async fn close(state: &AppState, server: &McpServer, session: &Session) {
    let Some(id) = &session.id else { return };
    let mut req = state.http.delete(&server.url).header(SESSION, id).timeout(Duration::from_secs(10));
    if let Some((h, v)) = &session.auth {
        req = req.header(h, v);
    }
    let _ = req.send().await;
}

/// Opens a session with the server, as a client would, lists its tools and closes the session.
pub async fn probe(state: &AppState, server: &McpServer) -> Result<Probe, String> {
    let auth = match server.auth.header(state, None).await {
        Ok(a) => a,
        Err(e) if e.not_signed_in() => {
            return Err(
                "each key signs in to this server on its own; sign a key in on the Access page, then call the server with it".into()
            );
        }
        Err(e) => return Err(e.message()),
    };
    let (session, result) = open(state, server, auth).await.map_err(|e| e.message)?;
    let listed = tools(state, server, &session).await;
    close(state, server, &session).await;
    let tools = listed
        .map_err(|e| e.message)?
        .iter()
        .filter_map(|t| {
            let name = t.get("name").and_then(Value::as_str)?.to_string();
            let description = t.get("description").and_then(Value::as_str).map(|d| crate::util::truncate(d, 300));
            Some(ProbedTool { name, description })
        })
        .collect();
    Ok(Probe {
        protocol_version: result.get("protocolVersion").and_then(Value::as_str).map(str::to_string),
        server_info: result.get("serverInfo").cloned(),
        tools,
    })
}

/// Why a message to a server failed: the server's HTTP status, when it answered with one.
#[derive(Debug)]
struct RpcError {
    status: Option<u16>,
    message: String,
}

impl RpcError {
    fn other(message: impl Into<String>) -> Self {
        Self { status: None, message: message.into() }
    }
}

/// Sends one message and returns the session id the server gave, if any, and the reply to the
/// message (`null` for a notification, which has none).
async fn rpc(state: &AppState, server: &McpServer, session: &Session, msg: &Value) -> Result<(Option<String>, Value), RpcError> {
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("?");
    let mut req = state
        .http
        .post(&server.url)
        .header(header::ACCEPT, "application/json, text/event-stream")
        .header("mcp-method", method)
        .json(msg)
        .timeout(server.timeout);
    if let Some(s) = &session.id {
        req = req.header(SESSION, s);
    }
    if let Some(v) = &session.version {
        req = req.header("mcp-protocol-version", v);
    }
    if let Some((h, v)) = &session.auth {
        req = req.header(h, v);
    }
    let failed = |e: reqwest::Error| RpcError::other(format!("{method}: {}", GatewayError::from_reqwest(&e).message));
    let resp = req.send().await.map_err(failed)?;
    let status = resp.status();
    let session = resp.headers().get(SESSION).and_then(|v| v.to_str().ok()).map(str::to_string);
    let sse = resp.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|t| t.starts_with("text/event-stream"));
    if !status.is_success() {
        let hint = match status.as_u16() {
            401 | 403 => " (check the server's authentication)",
            405 => " (check the URL: it is the server's MCP endpoint, often ending in /mcp)",
            _ => "",
        };
        return Err(RpcError {
            status: Some(status.as_u16()),
            message: format!("{method}: the server answered HTTP {}{hint}", status.as_u16()),
        });
    }
    let Some(id) = msg.get("id") else { return Ok((session, Value::Null)) };
    let mut body = resp.bytes_stream();
    let mut parser = SseParser::default();
    let mut buf = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(failed)?;
        if sse {
            // An event stream may carry requests and notifications before the reply.
            for event in parser.feed(&chunk) {
                if let Some(reply) = messages(event.data.as_bytes()).into_iter().find(|m| m.get("id") == Some(id)) {
                    return Ok((session, reply));
                }
            }
        } else if buf.len() + chunk.len() > MAX_READ {
            return Err(RpcError::other(format!("{method}: the answer is too large")));
        } else {
            buf.extend_from_slice(&chunk);
        }
    }
    messages(&buf)
        .into_iter()
        .find(|m| m.get("id") == Some(id))
        .map(|r| (session, r))
        .ok_or_else(|| RpcError::other(format!("{method}: the server's answer has no reply")))
}

/// The sessions the gateway keeps open with servers for `/mcp`, one per server and sign-in, and the
/// servers' tool lists, both in this replica's memory. A session unused for `SESSION_IDLE` is opened
/// again; a tool list is fetched again after `TOOLS_FRESH`, or when its server's settings change.
#[derive(Default)]
pub struct Upstreams {
    sessions: std::sync::Mutex<HashMap<(Uuid, Slot), (Session, Instant)>>,
    /// By server, and by sign-in where keys sign in on their own.
    tools: std::sync::Mutex<HashMap<(Uuid, Option<Slot>), (Arc<McpServer>, Instant, Arc<Vec<Value>>)>>,
}

const SESSION_IDLE: Duration = Duration::from_secs(600);
const TOOLS_FRESH: Duration = Duration::from_secs(60);

impl Upstreams {
    /// The server's tools as it lists them for `who`, unfiltered.
    pub async fn tools(&self, state: &AppState, server: &Arc<McpServer>, who: Who<'_>) -> Result<Arc<Vec<Value>>, ToolsError> {
        let slot = (server.id, server.auth.slot(who).map_err(ToolsError::Auth)?);
        if let Some((s, at, tools)) = self.tools.lock().expect("tools lock").get(&slot)
            && Arc::ptr_eq(s, server)
            && at.elapsed() < TOOLS_FRESH
        {
            return Ok(tools.clone());
        }
        let mut renewed = false;
        let listed = loop {
            let auth = server.auth.header(state, Some(who)).await.map_err(ToolsError::Auth)?;
            let result = match open(state, server, auth).await {
                Ok((session, _)) => {
                    let listed = tools(state, server, &session).await;
                    close(state, server, &session).await;
                    listed
                }
                Err(e) => Err(e),
            };
            match result {
                Err(RpcError { status: Some(401), .. }) if server.auth.renews() && !renewed => {
                    server.auth.renew(state, Some(who)).await;
                    renewed = true;
                }
                other => break other.map_err(|e| ToolsError::Failed(e.message))?,
            }
        };
        let listed = Arc::new(listed);
        self.tools.lock().expect("tools lock").insert(slot, (server.clone(), Instant::now(), listed.clone()));
        Ok(listed)
    }

    /// Sends a request for `who` and returns the server's reply, in its session with the
    /// server, which is opened when there is none or the server forgot it.
    pub async fn call(&self, state: &AppState, who: Who<'_>, server: &McpServer, msg: &Value) -> Result<Value, String> {
        let slot = (server.id, server.auth.slot(who).map_err(|e| e.message())?.unwrap_or_else(|| Slot::key(who.key)));
        let cached = {
            let mut sessions = self.sessions.lock().expect("sessions lock");
            sessions.retain(|_, (_, used)| used.elapsed() < SESSION_IDLE);
            sessions.get(&slot).map(|(s, _)| s.clone())
        };
        let reused = cached.is_some();
        // The token may have been renewed since the session was opened.
        let auth = server.auth.header(state, Some(who)).await.map_err(|e| e.message())?;
        let mut session = match cached {
            Some(s) => Session { auth: auth.clone(), ..s },
            None => open(state, server, auth).await.map_err(|e| e.message)?.0,
        };
        let mut result = rpc(state, server, &session, msg).await;
        // A server that restarted, or ended the session, answers 404: start a new one, once. One
        // that refuses the token gets a renewed one, once.
        let refused = matches!(result, Err(RpcError { status: Some(401), .. })) && server.auth.renews();
        if (reused && matches!(result, Err(RpcError { status: Some(404), .. }))) || refused {
            if refused {
                server.auth.renew(state, Some(who)).await;
            }
            let auth = server.auth.header(state, Some(who)).await.map_err(|e| e.message())?;
            session = open(state, server, auth).await.map_err(|e| e.message)?.0;
            result = rpc(state, server, &session, msg).await;
        }
        self.sessions.lock().expect("sessions lock").insert(slot, (session, Instant::now()));
        result.map(|(_, reply)| reply).map_err(|e| e.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_read_from_single_and_batched_bodies() {
        assert_eq!(messages(br#"{"jsonrpc":"2.0","id":1,"method":"tools/call"}"#).len(), 1);
        assert_eq!(messages(br#"[{"id":1},{"id":2}]"#).len(), 2);
        assert!(messages(b"not json").is_empty());
        assert!(messages(b"42").is_empty());
    }

    #[test]
    fn ids_keep_their_type() {
        assert_eq!(id_key(&json!(1)), Some("1".into()));
        assert_eq!(id_key(&json!("1")), Some("\"1\"".into()));
        assert_eq!(id_key(&Value::Null), None);
    }

    pub(super) fn server(id: Uuid) -> McpServer {
        McpServer {
            id,
            name: "s".into(),
            url: "http://x".into(),
            auth: UpstreamAuth::None,
            tags: vec![],
            project_ids: vec![],
            timeout: Duration::from_secs(1),
            allowed_tools: vec![],
            denied_tools: vec![],
            price_per_call: 0.0,
            tool_prices: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn sessions_only_open_for_the_key_and_server_that_opened_them() {
        let state = crate::state::test_state();
        let key = |id| VirtualKey { id, ..crate::auth::tests::key() };
        let (a, b) = (key(Uuid::now_v7()), key(Uuid::now_v7()));
        let s = server(Uuid::now_v7());
        let sealed = SessionTag::new(&state, &a, &s).seal(b"upstream-session-1").unwrap();
        assert_eq!(SessionTag::new(&state, &a, &s).open(sealed.as_bytes()), Some(&b"upstream-session-1"[..]));
        assert_eq!(SessionTag::new(&state, &b, &s).open(sealed.as_bytes()), None);
        assert_eq!(SessionTag::new(&state, &a, &server(Uuid::now_v7())).open(sealed.as_bytes()), None);
        // The bare session id, or one with a tag for another session, is refused.
        assert_eq!(SessionTag::new(&state, &a, &s).open(b"upstream-session-1"), None);
        let other = SessionTag::new(&state, &a, &s).seal(b"upstream-session-2").unwrap();
        let forged = format!("{}upstream-session-1", &other.to_str().unwrap()[..TAG_LEN]);
        assert_eq!(SessionTag::new(&state, &a, &s).open(forged.as_bytes()), None);
    }

    /// An MCP server on loopback that takes only the gateway's secret: `initialize` opens session
    /// `up-1`, `echo` answers as JSON, `fail` reports a tool error, and with `?sse` calls are answered
    /// as events, after a notification.
    async fn upstream() -> String {
        use axum::{Json, routing::post};
        async fn answer(uri: axum::http::Uri, headers: HeaderMap, Json(m): Json<Value>) -> Response {
            if headers.get(header::AUTHORIZATION).map(|v| v.as_bytes()) != Some(b"Bearer upstream-secret") {
                return StatusCode::UNAUTHORIZED.into_response();
            }
            let id = m["id"].clone();
            let result = match (m["method"].as_str(), m.pointer("/params/name").and_then(Value::as_str)) {
                (Some("initialize"), _) => {
                    return ([(SESSION, "up-1")], Json(json!({ "jsonrpc": "2.0", "id": id, "result": {} }))).into_response();
                }
                (Some("tools/list"), _) => {
                    json!({ "tools": (["echo", "fail", "secret"].map(|n| json!({ "name": n, "inputSchema": { "type": "object" } })).to_vec()) })
                }
                (Some("tools/call"), Some("fail")) => json!({ "content": [{ "type": "text", "text": "it broke" }], "isError": true }),
                (Some("tools/call"), _) if headers.get(SESSION).map(|v| v.as_bytes()) != Some(b"up-1") => {
                    return StatusCode::NOT_FOUND.into_response();
                }
                (Some("tools/call"), _) => json!({ "content": [{ "type": "text", "text": m.pointer("/params/arguments/text") }] }),
                _ => return StatusCode::ACCEPTED.into_response(),
            };
            let reply = json!({ "jsonrpc": "2.0", "id": id, "result": result });
            if uri.query() == Some("sse") {
                let events = format!("data: {}\n\ndata: {reply}\n\n", json!({ "jsonrpc": "2.0", "method": "notifications/progress" }));
                return ([(header::CONTENT_TYPE, "text/event-stream")], events).into_response();
            }
            Json(reply).into_response()
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, axum::Router::new().route("/mcp", post(answer))).await.unwrap() });
        format!("http://{addr}/mcp")
    }

    async fn call(app: &axum::Router, path: &str, session: Option<&str>, body: Value) -> (StatusCode, HeaderMap, Value) {
        use tower::ServiceExt;
        let mut req =
            axum::http::Request::post(path).header("authorization", "Bearer sk-gw-client").header("content-type", "application/json");
        if let Some(s) = session {
            req = req.header(SESSION, s);
        }
        let resp = app.clone().oneshot(req.body(Body::from(body.to_string())).unwrap()).await.unwrap();
        let (status, headers) = (resp.status(), resp.headers().clone());
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8_lossy(&bytes);
        // The last message of an event stream, or the JSON body.
        let last = text.rsplit("data: ").next().unwrap_or_default();
        (status, headers, serde_json::from_str(last.trim()).unwrap_or(Value::Null))
    }

    /// A state with key `sk-gw-client` and server `tools` at `url`, which hides its tool `secret`
    /// and charges $0.01 a call, $0.5 for `fail`.
    fn filtered_state(url: String) -> (AppState, tokio::sync::mpsc::Receiver<crate::accounting::RequestRecord>) {
        let (state, records) = crate::state::test_state_local();
        let mut snap = crate::registry::Snapshot::default();
        let key = VirtualKey { key_hash: crate::util::sha256(b"sk-gw-client").to_vec(), ..crate::auth::tests::key() };
        snap.keys.insert(crate::util::sha256(b"sk-gw-client"), Arc::new(key));
        let auth = UpstreamAuth::Static(header::AUTHORIZATION, HeaderValue::from_static("Bearer upstream-secret"));
        let server = McpServer {
            name: "tools".into(),
            url,
            auth,
            denied_tools: vec!["secret".into()],
            price_per_call: 0.01,
            tool_prices: BTreeMap::from([("fail".into(), 0.5)]),
            ..server(Uuid::now_v7())
        };
        snap.mcp_servers.insert("tools".into(), Arc::new(server));
        state.registry.set_snapshot(snap);
        (state, records)
    }

    #[tokio::test]
    async fn hidden_tools_are_left_out_and_refused_and_calls_are_charged() {
        let (state, mut records) = filtered_state(upstream().await);
        let app = axum::Router::new().route("/mcp/{server}", axum::routing::post(proxy)).with_state(state);
        let (_, headers, _) = call(&app, "/mcp/tools", None, json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" })).await;
        let session = headers.get(SESSION).unwrap().to_str().unwrap().to_string();

        let (_, _, reply) = call(&app, "/mcp/tools", Some(&session), json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" })).await;
        let names: Vec<&str> = reply["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["echo", "fail"]);

        let (status, _, reply) = call(
            &app,
            "/mcp/tools",
            Some(&session),
            json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "secret" } }),
        )
        .await;
        assert_eq!((status, &reply["error"]["code"]), (StatusCode::OK, &json!(-32602)));
        let r = records.try_recv().unwrap();
        assert_eq!((r.tool.as_deref(), r.status_code, r.cost_usd), (Some("secret"), 403, 0.0));

        // A call without an id, or with one that is not a string or number, never reaches the server.
        for id in [None, Some(json!(true)), Some(json!({}))] {
            let mut m = json!({ "jsonrpc": "2.0", "method": "tools/call", "params": { "name": "echo", "arguments": { "text": "hi" } } });
            if let Some(id) = id {
                m["id"] = id;
            }
            let (status, _, reply) = call(&app, "/mcp/tools", Some(&session), m).await;
            assert_eq!((status, &reply["error"]["code"]), (StatusCode::OK, &json!(-32600)));
            let r = records.try_recv().unwrap();
            assert_eq!((r.tool.as_deref(), r.status_code, r.cost_usd), (Some("echo"), 400, 0.0));
        }

        let echo =
            json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": { "name": "echo", "arguments": { "text": "hi" } } });
        call(&app, "/mcp/tools", Some(&session), echo).await;
        assert_eq!(records.try_recv().unwrap().cost_usd, 0.01);
        // A tool that reports an error has run, and is charged.
        call(
            &app,
            "/mcp/tools",
            Some(&session),
            json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": { "name": "fail" } }),
        )
        .await;
        let r = records.try_recv().unwrap();
        assert_eq!((r.status_code, r.cost_usd), (422, 0.5));
    }

    #[tokio::test]
    async fn the_gateways_own_server_lists_and_calls_every_servers_tools() {
        let (state, mut records) = filtered_state(upstream().await);
        let app = axum::Router::new().route("/mcp", axum::routing::post(hub::hub)).with_state(state);
        let list = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });
        let send = |body: Value| {
            use tower::ServiceExt;
            let req = axum::http::Request::post("/mcp")
                .header("host", "gateway.example.com")
                .header("authorization", "Bearer sk-gw-client")
                .header("content-type", "application/json")
                .header("accept", "application/json, text/event-stream")
                .header("mcp-protocol-version", "2025-11-25")
                .body(Body::from(body.to_string()))
                .unwrap();
            let app = app.clone();
            async move {
                let resp = app.oneshot(req).await.unwrap();
                let status = resp.status();
                let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
                serde_json::from_slice::<Value>(&bytes).unwrap_or_else(|_| panic!("{status}: {}", String::from_utf8_lossy(&bytes)))
            }
        };
        let reply = send(list).await;
        let names: Vec<&str> = reply["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["tools__echo", "tools__fail"]);

        let reply = send(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "tools__echo", "arguments": { "text": "hi" } } })).await;
        assert_eq!(reply["result"]["content"][0]["text"], "hi");
        let r = records.try_recv().unwrap();
        assert_eq!((r.model.as_str(), r.tool.as_deref(), r.status_code, r.cost_usd), ("tools", Some("echo"), 200, 0.01));

        let reply = send(json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "tools__secret" } })).await;
        assert_eq!(reply["error"]["code"], -32602);
        assert_eq!(records.try_recv().unwrap().status_code, 403);
    }

    #[tokio::test]
    async fn tool_calls_are_forwarded_with_the_gateways_secret_and_logged() {
        let (state, mut records) = crate::state::test_state_local();
        let url = upstream().await;
        let mut snap = crate::registry::Snapshot::default();
        let key = VirtualKey { key_hash: crate::util::sha256(b"sk-gw-client").to_vec(), ..crate::auth::tests::key() };
        snap.keys.insert(crate::util::sha256(b"sk-gw-client"), Arc::new(key));
        for (name, url) in [("tools", url.clone()), ("events", format!("{url}?sse"))] {
            let auth = UpstreamAuth::Static(header::AUTHORIZATION, HeaderValue::from_static("Bearer upstream-secret"));
            snap.mcp_servers.insert(name.into(), Arc::new(McpServer { name: name.into(), url, auth, ..server(Uuid::now_v7()) }));
        }
        state.registry.set_snapshot(snap);
        let app = axum::Router::new().route("/mcp/{server}", axum::routing::post(proxy)).with_state(state);

        let (status, headers, _) = call(&app, "/mcp/tools", None, json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" })).await;
        assert_eq!(status, StatusCode::OK);
        let session = headers.get(SESSION).unwrap().to_str().unwrap().to_string();
        assert!(session.len() == TAG_LEN + 4 && session.ends_with("up-1"), "{session}");
        assert!(records.try_recv().is_err(), "only tool calls are logged");

        let echo =
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "echo", "arguments": { "text": "hi" } } });
        let (status, _, reply) = call(&app, "/mcp/tools", Some(&session), echo.clone()).await;
        assert_eq!((status, &reply["result"]["content"][0]["text"]), (StatusCode::OK, &json!("hi")));
        let r = records.try_recv().unwrap();
        assert_eq!((r.api, r.model.as_str(), r.tool.as_deref(), r.status_code, r.error), ("mcp", "tools", Some("echo"), 200, None));

        let fail = json!({ "jsonrpc": "2.0", "id": "x", "method": "tools/call", "params": { "name": "fail" } });
        call(&app, "/mcp/tools", Some(&session), fail).await;
        let r = records.try_recv().unwrap();
        assert_eq!((r.status_code, r.error.as_deref()), (422, Some("it broke")));

        // The upstream's own session id is not one the gateway handed out.
        let (status, ..) = call(&app, "/mcp/tools", Some("up-1"), echo.clone()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(records.try_recv().unwrap().status_code, 404);

        // Events: the reply comes after a notification, and is read from the stream.
        let (status, _, reply) =
            call(&app, "/mcp/events", None, json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "fail" } }))
                .await;
        assert_eq!((status, &reply["result"]["isError"]), (StatusCode::OK, &json!(true)));
        let r = records.try_recv().unwrap();
        assert_eq!((r.model.as_str(), r.status_code), ("events", 422));
    }

    #[test]
    fn access_follows_projects_and_tags() {
        let project = Uuid::now_v7();
        let mut s = server(Uuid::now_v7());
        let mut k = crate::auth::tests::key();
        assert!(s.allows(&k));
        s.project_ids = vec![project];
        assert!(!s.allows(&k));
        k.project_id = Some(project);
        assert!(s.allows(&k));
        s.tags = vec!["internal".into()];
        k.allowed_tags = vec!["public".into()];
        assert!(!s.allows(&k));
        k.allowed_tags = vec![];
        assert!(s.allows(&k));
    }
}
