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
    collections::HashMap,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
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

use crate::{
    accounting::Tracker,
    auth::{self, ClientKey},
    capture,
    crypto::SecretBox,
    error::{Dialect, ErrorKind, GatewayError},
    labels::Labels,
    model::VirtualKey,
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
    pub tags: Vec<String>,
    pub project_ids: Vec<Uuid>,
    pub timeout_secs: i32,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// How the gateway authenticates to an MCP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpAuth {
    /// No credentials: a server on a private network, or one that needs none.
    None,
    /// `Authorization: Bearer <secret>`.
    Bearer,
    /// The secret as the value of a header of the server's choice, such as `X-API-Key`.
    Header,
}

impl McpAuth {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "none" => Some(Self::None),
            "bearer" => Some(Self::Bearer),
            "header" => Some(Self::Header),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Bearer => "bearer",
            Self::Header => "header",
        }
    }
}

/// An enabled MCP server, ready to call: its secret decrypted once.
pub struct McpServer {
    pub id: Uuid,
    pub name: String,
    pub url: String,
    /// The header carrying the gateway's secret, and its value.
    auth: Option<(HeaderName, HeaderValue)>,
    pub tags: Vec<String>,
    pub project_ids: Vec<Uuid>,
    /// For a POST's answer, streamed or not. A client's GET stream stays open as long as both
    /// sides keep it.
    pub timeout: Duration,
}

impl McpServer {
    pub fn build(row: McpServerRow, sb: &SecretBox) -> Result<Self, String> {
        let ctx = |e: &str| format!("MCP server {} ({}): {e}", row.id, row.name);
        let auth = McpAuth::parse(&row.auth).ok_or_else(|| ctx("unknown auth"))?;
        let secret = match &row.secret {
            Some(sealed) => Some(String::from_utf8(sb.open(sealed).map_err(|e| ctx(&e))?).map_err(|_| ctx("corrupt secret"))?),
            None => None,
        };
        let auth = match (auth, secret) {
            (McpAuth::None, _) => None,
            (_, None) => return Err(ctx("its secret is missing")),
            (McpAuth::Bearer, Some(s)) => Some((header::AUTHORIZATION, sensitive(&format!("Bearer {s}")).ok_or_else(|| ctx("the secret is not a valid header value"))?)),
            (McpAuth::Header, Some(s)) => {
                let name = row.auth_header.as_deref().and_then(|h| HeaderName::try_from(h).ok()).ok_or_else(|| ctx("invalid auth header"))?;
                Some((name, sensitive(&s).ok_or_else(|| ctx("the secret is not a valid header value"))?))
            }
        };
        Ok(Self {
            id: row.id,
            name: row.name,
            url: row.url,
            auth,
            tags: row.tags,
            project_ids: row.project_ids,
            timeout: Duration::from_secs(row.timeout_secs.max(1) as u64),
        })
    }

    /// As for deployments: a server limited to projects is only for keys in one of them, and a key
    /// with tags may use what carries no tag it lacks.
    pub fn allows(&self, key: &VirtualKey) -> bool {
        let in_project = self.project_ids.is_empty() || key.project_id.is_some_and(|p| self.project_ids.contains(&p));
        in_project && key.holds_all(&self.tags)
    }
}

fn sensitive(v: &str) -> Option<HeaderValue> {
    let mut v = HeaderValue::from_str(v).ok()?;
    v.set_sensitive(true);
    Some(v)
}

/// Headers of the client's request the server gets; everything else (the client's key above all)
/// stays at the gateway.
const FORWARDED: [&str; 4] = ["accept", "content-type", "mcp-protocol-version", "last-event-id"];
/// Headers of the server's answer the client gets. Not `WWW-Authenticate`: a client must not start
/// signing in to the server, whose credentials are the gateway's.
const RETURNED: [&str; 2] = ["content-type", "mcp-protocol-version"];
const SESSION: &str = "mcp-session-id";
/// The answer to a POST is read up to this size for its replies; it is forwarded whole either way.
const MAX_READ: usize = 8 * 1024 * 1024;

/// `POST`, `GET` and `DELETE /mcp/{server}`.
pub async fn proxy(
    State(state): State<AppState>,
    ClientKey(key): ClientKey,
    Path(name): Path<String>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    forward(&state, &key, &name, method, &headers, body).await.unwrap_or_else(|e| e.into_response_for(Dialect::OpenAi))
}

async fn forward(state: &AppState, key: &Arc<VirtualKey>, name: &str, method: Method, headers: &HeaderMap, body: Bytes) -> Result<Response, GatewayError> {
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

    let mut req = state.http.request(method.clone(), &server.url);
    for h in FORWARDED {
        if let Some(v) = headers.get(h) {
            req = req.header(h, v);
        }
    }
    if let Some(s) = session {
        req = req.header(SESSION, s);
    }
    if let Some((h, v)) = &server.auth {
        req = req.header(h, v);
    }
    if method == Method::POST {
        req = req.timeout(server.timeout).body(body);
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            let e = GatewayError::from_reqwest(&e);
            calls.fail(&e);
            return Err(e);
        }
    };
    let status = resp.status();
    if matches!(status.as_u16(), 401 | 403) {
        tracing::warn!(server = %server.name, status = status.as_u16(), "MCP server refused the gateway's credentials");
        let e = GatewayError {
            upstream: true,
            ..GatewayError::new(ErrorKind::Upstream, StatusCode::BAD_GATEWAY, format!("the MCP server refused the gateway's credentials (HTTP {})", status.as_u16()))
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

/// A JSON-RPC id as a map key: `1` and `"1"` are different ids.
fn id_key(id: &Value) -> Option<String> {
    matches!(id, Value::String(_) | Value::Number(_)).then(|| id.to_string())
}

/// The `tools/call` requests of a POST, by id, each with its entry in the request log. An entry is
/// written when its call is dropped: once its reply came, it failed, or the exchange ended without a
/// reply (see `Drop`).
struct Calls {
    open: HashMap<String, Tracker>,
    /// The server's answer ran to its end (rather than the client going away first).
    ended: bool,
}

impl Calls {
    fn new(state: &AppState, key: &VirtualKey, labels: &Labels, server: &McpServer, messages: &[Value]) -> Self {
        let mut calls = HashMap::new();
        for m in messages {
            let (Some("tools/call"), Some(id)) = (m.get("method").and_then(Value::as_str), m.get("id").and_then(id_key)) else { continue };
            let mut t = Tracker::new(state, Some(key), labels, &server.name, "mcp", false);
            t.tool(m.pointer("/params/name").and_then(Value::as_str).unwrap_or("?"));
            if let Some(c) = t.log() {
                c.request(capture::Body::Json(m.clone()));
            }
            calls.insert(id, t);
        }
        Self { open: calls, ended: false }
    }

    fn is_empty(&self) -> bool {
        self.open.is_empty()
    }

    /// Ends every open call with the error.
    fn fail(&mut self, e: &GatewayError) {
        self.open.drain().for_each(|(_, mut t)| t.fail(e));
    }

    fn fail_status(&mut self, status: u16, msg: impl Into<String>) {
        let msg = msg.into();
        self.open.drain().for_each(|(_, mut t)| t.fail_status(status, msg.clone()));
    }

    /// A message from the server: the reply to one of the calls ends its entry. Requests and
    /// notifications the server sends meanwhile (progress, sampling) are not the gateway's.
    fn settle(&mut self, reply: &Value) {
        let Some(mut t) = reply.get("id").and_then(id_key).and_then(|id| self.open.remove(&id)) else { return };
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
        for (_, mut t) in self.open.drain() {
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

// ---------------------------------------------------------------- probing

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

/// The protocol version the gateway asks for when it talks to a server itself.
const PROTOCOL_VERSION: &str = "2025-11-25";

/// Opens a session with the server, as a client would, lists its tools and closes the session.
pub async fn probe(state: &AppState, server: &McpServer) -> Result<Probe, String> {
    state.cfg.upstream_policy.check_url(&server.url).map_err(|why| format!("the address is not allowed: {why}"))?;
    let init = json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "inference-gateway", "version": crate::VERSION },
        },
    });
    let (session, reply) = rpc(state, server, None, None, &init).await?;
    let result = reply.get("result").ok_or_else(|| "initialize did not succeed".to_string())?;
    let version = result.get("protocolVersion").and_then(Value::as_str).map(str::to_string);
    let server_info = result.get("serverInfo").cloned();
    let session = session.as_deref();
    let notified = rpc(state, server, session, version.as_deref(), &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).await;
    let mut tools = Vec::new();
    let listed = async {
        notified?;
        let mut cursor: Option<String> = None;
        // Pages until the server has no more, within reason.
        for page in 2..12 {
            let mut list = json!({ "jsonrpc": "2.0", "id": page, "method": "tools/list" });
            if let Some(c) = &cursor {
                list["params"] = json!({ "cursor": c });
            }
            let (_, reply) = rpc(state, server, session, version.as_deref(), &list).await?;
            let result = reply.get("result").ok_or_else(|| "tools/list did not succeed".to_string())?;
            for t in result.get("tools").and_then(Value::as_array).into_iter().flatten() {
                let Some(name) = t.get("name").and_then(Value::as_str) else { continue };
                let description = t.get("description").and_then(Value::as_str).map(|d| crate::util::truncate(d, 300));
                tools.push(ProbedTool { name: name.to_string(), description });
            }
            cursor = result.get("nextCursor").and_then(Value::as_str).map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        Ok::<_, String>(())
    }
    .await;
    if let Some(s) = session {
        let mut req = state.http.delete(&server.url).header(SESSION, s).timeout(Duration::from_secs(10));
        if let Some((h, v)) = &server.auth {
            req = req.header(h, v);
        }
        let _ = req.send().await;
    }
    listed?;
    Ok(Probe { protocol_version: version, server_info, tools })
}

/// Sends one message and returns the session id the server gave, if any, and the reply to the
/// message (`null` for a notification, which has none).
async fn rpc(state: &AppState, server: &McpServer, session: Option<&str>, version: Option<&str>, msg: &Value) -> Result<(Option<String>, Value), String> {
    let mut req = state
        .http
        .post(&server.url)
        .header(header::ACCEPT, "application/json, text/event-stream")
        .json(msg)
        .timeout(server.timeout.min(Duration::from_secs(30)));
    if let Some(s) = session {
        req = req.header(SESSION, s);
    }
    if let Some(v) = version {
        req = req.header("mcp-protocol-version", v);
    }
    if let Some((h, v)) = &server.auth {
        req = req.header(h, v);
    }
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("?");
    let resp = req.send().await.map_err(|e| format!("{method}: {}", GatewayError::from_reqwest(&e).message))?;
    let status = resp.status();
    let session = resp.headers().get(SESSION).and_then(|v| v.to_str().ok()).map(str::to_string);
    let sse = resp.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|t| t.starts_with("text/event-stream"));
    if !status.is_success() {
        let hint = match status.as_u16() {
            401 | 403 => " (check the server's authentication)",
            404 | 405 => " (check the URL: it is the server's MCP endpoint, often ending in /mcp)",
            _ => "",
        };
        return Err(format!("{method}: the server answered HTTP {}{hint}", status.as_u16()));
    }
    let Some(id) = msg.get("id") else { return Ok((session, Value::Null)) };
    let mut body = resp.bytes_stream();
    let mut parser = SseParser::default();
    let mut buf = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| format!("{method}: {}", GatewayError::from_reqwest(&e).message))?;
        if sse {
            // An event stream may carry requests and notifications before the reply.
            for event in parser.feed(&chunk) {
                if let Some(reply) = messages(event.data.as_bytes()).into_iter().find(|m| m.get("id") == Some(id)) {
                    return Ok((session, reply));
                }
            }
        } else if buf.len() + chunk.len() > MAX_READ {
            return Err(format!("{method}: the answer is too large"));
        } else {
            buf.extend_from_slice(&chunk);
        }
    }
    messages(&buf)
        .into_iter()
        .find(|m| m.get("id") == Some(id))
        .map(|r| (session, r))
        .ok_or_else(|| format!("{method}: the server's answer has no reply"))
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

    fn server(id: Uuid) -> McpServer {
        McpServer { id, name: "s".into(), url: "http://x".into(), auth: None, tags: vec![], project_ids: vec![], timeout: Duration::from_secs(1) }
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
                (Some("initialize"), _) => return ([(SESSION, "up-1")], Json(json!({ "jsonrpc": "2.0", "id": id, "result": {} }))).into_response(),
                (Some("tools/call"), Some("fail")) => json!({ "content": [{ "type": "text", "text": "it broke" }], "isError": true }),
                (Some("tools/call"), _) if headers.get(SESSION).map(|v| v.as_bytes()) != Some(b"up-1") => return StatusCode::NOT_FOUND.into_response(),
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
        let mut req = axum::http::Request::post(path).header("authorization", "Bearer sk-gw-client").header("content-type", "application/json");
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

    #[tokio::test]
    async fn tool_calls_are_forwarded_with_the_gateways_secret_and_logged() {
        let (state, mut records) = crate::state::test_state_local();
        let url = upstream().await;
        let mut snap = crate::registry::Snapshot::default();
        let key = VirtualKey { key_hash: crate::util::sha256(b"sk-gw-client").to_vec(), ..crate::auth::tests::key() };
        snap.keys.insert(crate::util::sha256(b"sk-gw-client"), Arc::new(key));
        for (name, url) in [("tools", url.clone()), ("events", format!("{url}?sse"))] {
            let auth = Some((header::AUTHORIZATION, HeaderValue::from_static("Bearer upstream-secret")));
            snap.mcp_servers.insert(name.into(), Arc::new(McpServer { name: name.into(), url, auth, ..server(Uuid::now_v7()) }));
        }
        state.registry.set_snapshot(snap);
        let app = axum::Router::new().route("/mcp/{server}", axum::routing::post(proxy)).with_state(state);

        let (status, headers, _) = call(&app, "/mcp/tools", None, json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" })).await;
        assert_eq!(status, StatusCode::OK);
        let session = headers.get(SESSION).unwrap().to_str().unwrap().to_string();
        assert!(session.len() == TAG_LEN + 4 && session.ends_with("up-1"), "{session}");
        assert!(records.try_recv().is_err(), "only tool calls are logged");

        let echo = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "echo", "arguments": { "text": "hi" } } });
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
        let (status, _, reply) = call(&app, "/mcp/events", None, json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "fail" } })).await;
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
