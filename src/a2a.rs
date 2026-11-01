//! A2A gateway: the agents set up in the console, proxied at `/a2a/{name}`. Clients get the
//! agent's card with the gateway's addresses and authenticate with their virtual key; the gateway
//! calls the agent with its own secret. Messages pass through as they are, in A2A 1.0 and 0.3,
//! over JSON-RPC (`/a2a/{name}`) and HTTP+JSON (`/a2a/{name}/rest/…`).
//!
//! The gateway reads what it needs on the way: each message sent is an entry in the request log
//! (api `a2a`, the agent as its model) with the state its task was left in, counts against the
//! key's limits and budgets, and is charged the agent's price. Every key reaches the agent as the
//! gateway, so tasks are bound to the key whose message created them: the task and context ids in a
//! key's answers are recorded, and a call about another key's task is answered as if it did not exist.

use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use axum::{
    body::{Body, Bytes},
    extract::{Path, State},
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use futures::{Stream, StreamExt, stream::BoxStream};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    accounting::Tracker,
    auth::{self, ClientKey},
    capture,
    crypto::SecretBox,
    error::{Dialect, ErrorKind, GatewayError},
    labels::Labels,
    model::VirtualKey,
    oauth::{Kind, OAuthConfig, UpstreamAuth, Who},
    sse::SseParser,
    state::AppState,
    util::TtlCache,
};

/// An A2A agent as stored.
#[derive(Debug, Clone)]
pub struct A2aAgentRow {
    pub id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub card_url: String,
    pub auth: String,
    pub auth_header: Option<String>,
    /// Sealed with the gateway's encryption key.
    pub secret: Option<Vec<u8>>,
    /// The OAuth client, for `oauth` and `oauth_user`.
    pub oauth: Option<sqlx::types::Json<OAuthConfig>>,
    pub tags: Vec<String>,
    pub project_ids: Vec<Uuid>,
    pub timeout_secs: i32,
    pub price_per_message: f64,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// An enabled agent, ready to call: its secret decrypted once.
pub struct A2aAgent {
    pub id: Uuid,
    pub name: String,
    pub card_url: String,
    pub auth: UpstreamAuth,
    pub tags: Vec<String>,
    pub project_ids: Vec<Uuid>,
    /// For an answer that is not streamed. Streams last as long as the task they follow.
    pub timeout: Duration,
    pub price_per_message: f64,
}

impl A2aAgent {
    pub fn build(row: A2aAgentRow, sb: &SecretBox) -> Result<Self, String> {
        let ctx = |e: String| format!("A2A agent {} ({}): {e}", row.id, row.name);
        let oauth = row.oauth.as_ref().map(|o| &o.0);
        let auth =
            UpstreamAuth::build(Kind::A2a, row.id, &row.name, &row.auth, row.auth_header.as_deref(), row.secret.as_deref(), oauth, sb)
                .map_err(ctx)?;
        Ok(Self {
            id: row.id,
            name: row.name,
            card_url: row.card_url,
            auth,
            tags: row.tags,
            project_ids: row.project_ids,
            timeout: Duration::from_secs(row.timeout_secs.max(1) as u64),
            price_per_message: row.price_per_message,
        })
    }

    pub fn allows(&self, key: &VirtualKey) -> bool {
        crate::mcp::open_to(key, &self.project_ids, &self.tags)
    }
}

// ---------------------------------------------------------------- agent cards

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Binding {
    JsonRpc,
    Rest,
}

impl Binding {
    fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_uppercase().as_str() {
            "JSONRPC" => Some(Self::JsonRpc),
            "HTTP+JSON" | "REST" => Some(Self::Rest),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::JsonRpc => "JSONRPC",
            Self::Rest => "HTTP+JSON",
        }
    }
}

/// One of the agent's interfaces the gateway proxies.
#[derive(Debug, Clone)]
struct Interface {
    url: String,
    version: String,
    tenant: Option<String>,
}

/// An agent's card, as the agent serves it, with the newest interface of each binding the gateway
/// proxies. gRPC interfaces are not proxied.
#[derive(Debug)]
pub struct Card {
    raw: Value,
    /// An A2A 0.3 card (`url`, `additionalInterfaces`) rather than a 1.0 one (`supportedInterfaces`).
    legacy: bool,
    jsonrpc: Option<Interface>,
    rest: Option<Interface>,
}

/// Orders protocol versions such as `0.3.0` and `1.0` by their numbers.
fn version_key(v: &str) -> Vec<u64> {
    v.split('.').map(|p| p.trim().parse().unwrap_or(0)).collect()
}

impl Card {
    fn parse(raw: Value) -> Result<Self, String> {
        if !raw.is_object() {
            return Err("the agent card is not a JSON object".into());
        }
        let mut found: Vec<(Binding, Interface)> = Vec::new();
        let legacy = raw.get("supportedInterfaces").is_none();
        let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        if !legacy {
            for i in raw["supportedInterfaces"].as_array().into_iter().flatten() {
                let (Some(url), Some(binding)) = (s(i, "url"), i.get("protocolBinding").and_then(Value::as_str).and_then(Binding::parse))
                else {
                    continue;
                };
                found.push((
                    binding,
                    Interface { url, version: s(i, "protocolVersion").unwrap_or_else(|| "1.0".into()), tenant: s(i, "tenant") },
                ));
            }
        } else {
            let version = s(&raw, "protocolVersion").unwrap_or_else(|| "0.3.0".into());
            if let Some(url) = s(&raw, "url") {
                let binding = raw.get("preferredTransport").and_then(Value::as_str).map_or(Some(Binding::JsonRpc), Binding::parse);
                if let Some(b) = binding {
                    found.push((b, Interface { url, version: version.clone(), tenant: None }));
                }
            }
            for i in raw["additionalInterfaces"].as_array().into_iter().flatten() {
                let (Some(url), Some(binding)) = (s(i, "url"), i.get("transport").and_then(Value::as_str).and_then(Binding::parse)) else {
                    continue;
                };
                found.push((binding, Interface { url, version: version.clone(), tenant: None }));
            }
        }
        let newest = |b: Binding| found.iter().filter(|(fb, _)| *fb == b).map(|(_, i)| i).max_by_key(|i| version_key(&i.version)).cloned();
        let card = Self { jsonrpc: newest(Binding::JsonRpc), rest: newest(Binding::Rest), legacy, raw };
        if card.jsonrpc.is_none() && card.rest.is_none() {
            return Err("the agent card names no JSON-RPC or HTTP+JSON interface".into());
        }
        Ok(card)
    }

    fn interface(&self, b: Binding) -> Option<&Interface> {
        match b {
            Binding::JsonRpc => self.jsonrpc.as_ref(),
            Binding::Rest => self.rest.as_ref(),
        }
    }

    /// The card as a client of the gateway gets it: the gateway's interfaces at `base`
    /// (`https://gateway/a2a/{name}`), the gateway's key as its security, and no signatures (they
    /// no longer match). The extended card is not offered: it would list the agent's own addresses.
    fn rewritten(&self, base: &str) -> Value {
        let mut v = self.raw.clone();
        let gateway = |b: Binding| match b {
            Binding::JsonRpc => base.to_string(),
            Binding::Rest => format!("{base}/rest"),
        };
        let ours: Vec<(Binding, &Interface)> =
            [Binding::JsonRpc, Binding::Rest].into_iter().filter_map(|b| Some((b, self.interface(b)?))).collect();
        let o = v.as_object_mut().expect("checked when parsed");
        o.remove("signatures");
        let description = "A virtual key of the inference gateway.";
        if self.legacy {
            let (b, i) = ours[0];
            o.insert("url".into(), json!(gateway(b)));
            o.insert("preferredTransport".into(), json!(b.name()));
            o.insert("protocolVersion".into(), json!(i.version));
            o.insert(
                "additionalInterfaces".into(),
                ours.iter().map(|(b, _)| json!({ "url": gateway(*b), "transport": b.name() })).collect(),
            );
            o.insert("securitySchemes".into(), json!({ "gateway": { "type": "http", "scheme": "bearer", "description": description } }));
            o.insert("security".into(), json!([{ "gateway": [] }]));
            o.insert("supportsAuthenticatedExtendedCard".into(), json!(false));
        } else {
            let interfaces: Vec<Value> = ours
                .iter()
                .map(|(b, i)| {
                    let mut x = json!({ "url": gateway(*b), "protocolBinding": b.name(), "protocolVersion": i.version });
                    if let Some(t) = &i.tenant {
                        x["tenant"] = json!(t);
                    }
                    x
                })
                .collect();
            o.insert("supportedInterfaces".into(), interfaces.into());
            o.insert(
                "securitySchemes".into(),
                json!({ "gateway": { "httpAuthSecurityScheme": { "scheme": "Bearer", "description": description } } }),
            );
            o.insert("securityRequirements".into(), json!([{ "schemes": { "gateway": { "list": [] } } }]));
            if let Some(c) = o.get_mut("capabilities").and_then(Value::as_object_mut) {
                c.insert("extendedAgentCard".into(), json!(false));
            }
        }
        v
    }
}

/// Agents' cards in this replica's memory, fetched again after `CARD_FRESH` or when the agent's
/// settings change. Where each key signs in on its own, the card is fetched with the key's token
/// and may be that account's own, so it is kept per key.
pub struct Cards(TtlCache<(Uuid, Option<Uuid>), (Arc<A2aAgent>, Arc<Card>)>);

const CARD_FRESH: Duration = Duration::from_secs(300);

impl Default for Cards {
    fn default() -> Self {
        Self(TtlCache::new(CARD_FRESH, 10_000))
    }
}

impl Cards {
    /// `who`: who asks, for an agent whose keys sign in on their own and whose card needs it.
    pub async fn get(&self, state: &AppState, agent: &Arc<A2aAgent>, who: Option<Who<'_>>) -> Result<Arc<Card>, String> {
        let slot = (agent.id, if agent.auth.per_key() { who.as_ref().map(|w| w.key) } else { None });
        if let Some((a, card)) = self.0.get(&slot)
            && Arc::ptr_eq(&a, agent)
        {
            return Ok(card);
        }
        let card = Arc::new(fetch_card(state, agent, who).await?);
        self.0.insert(slot, (agent.clone(), card.clone()));
        Ok(card)
    }
}

/// A card is read up to this size.
const MAX_CARD: usize = 1 << 20;

/// The agent's card, as the agent serves it to the gateway. Where each key signs in on its own,
/// the card is fetched without credentials (cards are mostly public) unless `who` has signed in.
pub async fn fetch_card(state: &AppState, agent: &A2aAgent, who: Option<Who<'_>>) -> Result<Card, String> {
    state.cfg.upstream_policy.check_url(&agent.card_url).map_err(|why| format!("the card's address is not allowed: {why}"))?;
    let mut req = state.http.get(&agent.card_url).header(header::ACCEPT, "application/json").timeout(Duration::from_secs(30));
    match agent.auth.header(state, who).await {
        Ok(Some((h, v))) => req = req.header(h, v),
        Ok(None) => {}
        Err(e) if e.not_signed_in() => {}
        Err(e) => return Err(e.message()),
    }
    let resp = req.send().await.map_err(|e| format!("the card could not be fetched: {}", GatewayError::from_reqwest(&e).message))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("the card's address answered HTTP {}", status.as_u16()));
    }
    let bytes = crate::oauth::read_body(resp, MAX_CARD).await.map_err(|e| format!("the card could not be read: {e}"))?;
    let raw: Value = serde_json::from_slice(&bytes).map_err(|_| "the card is not JSON".to_string())?;
    Card::parse(raw)
}

/// What the admin API's test shows of a card.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct CardSummary {
    pub name: Option<String>,
    pub description: Option<String>,
    pub version: Option<String>,
    /// The interfaces the gateway proxies: binding, protocol version and the agent's own URL.
    pub interfaces: Vec<CardInterface>,
    pub skills: Vec<CardSkill>,
}

#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct CardInterface {
    pub binding: &'static str,
    pub version: String,
    pub url: String,
}

#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct CardSkill {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl Card {
    pub fn summary(&self) -> CardSummary {
        let s = |k: &str| self.raw.get(k).and_then(Value::as_str).map(str::to_string);
        CardSummary {
            name: s("name"),
            description: s("description").map(|d| crate::util::truncate(&d, 500)),
            version: s("version"),
            interfaces: [Binding::JsonRpc, Binding::Rest]
                .into_iter()
                .filter_map(|b| {
                    self.interface(b).map(|i| CardInterface { binding: b.name(), version: i.version.clone(), url: i.url.clone() })
                })
                .collect(),
            skills: self.raw["skills"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|k| {
                    let name = k.get("name").or_else(|| k.get("id")).and_then(Value::as_str)?.to_string();
                    Some(CardSkill {
                        name,
                        description: k.get("description").and_then(Value::as_str).map(|d| crate::util::truncate(d, 300)),
                    })
                })
                .collect(),
        }
    }
}

// ---------------------------------------------------------------- requests

/// What a request asks of the agent, as far as the gateway is concerned.
#[derive(Debug, PartialEq)]
enum Ask {
    /// A message: logged, limited and charged. `stream`: the answer is a stream of events.
    Send { stream: bool, task: Option<String>, context: Option<String> },
    /// About a task: only for the key whose message created it.
    Task(String),
    /// Not offered through the gateway.
    Refused(&'static str),
    /// Anything else passes through.
    Other,
}

fn send(stream: bool, message: Option<&Value>) -> Ask {
    let s = |k: &str| message.and_then(|m| m.get(k)).and_then(Value::as_str).map(str::to_string);
    Ask::Send { stream, task: s("taskId"), context: s("contextId") }
}

const NO_LISTING: &str = "listing tasks is not available through the gateway";
const NO_EXTENDED_CARD: &str = "the extended agent card is not available through the gateway";

/// A JSON-RPC request, in A2A 1.0's method names or 0.3's.
fn ask_jsonrpc(msg: &Value) -> Ask {
    let params = msg.get("params");
    let p = |k: &str| params.and_then(|p| p.get(k)).and_then(Value::as_str).map(|s| s.strip_prefix("tasks/").unwrap_or(s).to_string());
    match msg.get("method").and_then(Value::as_str).unwrap_or_default() {
        "SendMessage" | "message/send" => send(false, params.and_then(|p| p.get("message"))),
        "SendStreamingMessage" | "message/stream" => send(true, params.and_then(|p| p.get("message"))),
        "ListTasks" | "tasks/list" => Ask::Refused(NO_LISTING),
        "GetExtendedAgentCard" | "agent/getAuthenticatedExtendedCard" => Ask::Refused(NO_EXTENDED_CARD),
        // The push notification config methods name their task `taskId` (and in 0.3 sometimes `id`).
        m if m.contains("PushNotificationConfig") || m.starts_with("tasks/pushNotificationConfig/") => {
            p("taskId").or_else(|| p("id")).map_or(Ask::Other, Ask::Task)
        }
        "GetTask" | "tasks/get" | "CancelTask" | "tasks/cancel" | "SubscribeToTask" | "tasks/resubscribe" => {
            p("id").or_else(|| p("name")).map_or(Ask::Other, Ask::Task)
        }
        _ => Ask::Other,
    }
}

/// An HTTP+JSON request: `path` is what follows `/rest/`, perhaps after a tenant or a `v1/`.
fn ask_rest(path: &str, body: &[u8]) -> Ask {
    let path = path.trim_end_matches('/');
    let message = || serde_json::from_slice::<Value>(body).ok().and_then(|b| b.get("message").cloned());
    if path.ends_with("message:send") || path.ends_with("message/send") {
        return send(false, message().as_ref());
    }
    if path.ends_with("message:stream") || path.ends_with("message/stream") {
        return send(true, message().as_ref());
    }
    let segments: Vec<&str> = path.split('/').collect();
    if let Some(i) = segments.iter().position(|s| *s == "tasks" || s.starts_with("tasks:")) {
        return match segments.get(i + 1) {
            Some(id) => Ask::Task(id.split(':').next().unwrap_or(id).to_string()),
            None => Ask::Refused(NO_LISTING),
        };
    }
    if segments.iter().any(|s| matches!(*s, "extendedAgentCard" | "card")) || path.ends_with("agent-card/extended") {
        return Ask::Refused(NO_EXTENDED_CARD);
    }
    Ask::Other
}

/// The client's path after `/rest/`, as the agent gets it. Axum percent-decoded it, so it is
/// parsed as a URL path again and comes back encoded as reqwest sends it. Refused when it would
/// leave the interface's path (`.` or `..` segments, with `/` or `\`), or holds `?` or `#`, which
/// would cut it short on the way.
fn rest_path(raw: &str) -> Option<String> {
    if raw.split(['/', '\\']).any(|s| s == "." || s == "..") {
        return None;
    }
    let u = reqwest::Url::parse(&format!("http://x/{}", raw.trim_start_matches('/'))).ok()?;
    if u.query().is_some() || u.fragment().is_some() {
        return None;
    }
    Some(u.path().trim_start_matches('/').to_string())
}

/// Whether `url` is under the interface's URL: same origin, and its path below the interface's.
fn stays_under(url: &str, interface: &str) -> bool {
    let (Ok(u), Ok(i)) = (reqwest::Url::parse(url), reqwest::Url::parse(interface)) else { return false };
    u.origin() == i.origin() && u.path().starts_with(&format!("{}/", i.path().trim_end_matches('/')))
}

/// How long the gateway remembers which key a task or context belongs to, since it was last used.
const OWNERSHIP: Duration = Duration::from_secs(30 * 24 * 3600);

/// The Valkey key recording who a task (`t`) or context (`c`) of `agent` belongs to. The id is
/// hashed: agents choose it, and it may be long or contain anything.
fn owner_key(agent: Uuid, kind: char, id: &str) -> String {
    format!("gw:a2a:{agent}:{kind}:{}", hex::encode(&crate::util::sha256(id.as_bytes())[..16]))
}

/// Records that the tasks and contexts in an answer belong to `key`.
fn claim(state: &AppState, agent: Uuid, key: Uuid, seen: &Seen) {
    let ids: Vec<String> =
        seen.tasks.iter().map(|t| owner_key(agent, 't', t)).chain(seen.context.iter().map(|c| owner_key(agent, 'c', c))).collect();
    if ids.is_empty() {
        return;
    }
    let cache = state.cache.clone();
    tokio::spawn(async move {
        for id in ids {
            cache.set_a2a_owner(&id, &key.to_string(), OWNERSHIP).await;
        }
    });
}

/// Whether `key` may go on with a task or context: tasks only when it is theirs; contexts unless
/// another key's (a client may start a context of its own choosing).
async fn may_use(state: &AppState, agent: Uuid, key: Uuid, kind: char, id: &str) -> bool {
    match state.cache.a2a_owner(&owner_key(agent, kind, id), OWNERSHIP).await {
        Some(owner) => owner == key.to_string(),
        None => kind == 'c',
    }
}

// ---------------------------------------------------------------- what the agent answered

/// What the gateway reads in an agent's answer.
#[derive(Default, Debug)]
struct Seen {
    tasks: Vec<String>,
    context: Option<String>,
    /// The task's latest state, such as `completed` or `input-required`.
    state: Option<String>,
    /// A JSON-RPC error: its code and message.
    error: Option<(i64, String)>,
}

/// `TASK_STATE_INPUT_REQUIRED` (1.0) and `input-required` (0.3) both as `input-required`.
fn task_state(s: &str) -> String {
    s.strip_prefix("TASK_STATE_").unwrap_or(s).to_ascii_lowercase().replace('_', "-")
}

impl Seen {
    /// One message of the answer: a JSON-RPC response, or an HTTP+JSON body or event.
    fn message(&mut self, v: &Value) {
        if let Some(e) = v.get("error").filter(|e| e.is_object()) {
            let code = e.get("code").and_then(Value::as_i64).unwrap_or(0);
            self.error = Some((code, e.get("message").and_then(Value::as_str).unwrap_or("no message").to_string()));
            return;
        }
        self.walk(v.get("result").unwrap_or(v), 0);
    }

    /// Tasks (an object with an `id` and a `status`), task and context ids, and states, in the
    /// answer's objects; arrays (a task's history and artifacts) only repeat them.
    fn walk(&mut self, v: &Value, depth: usize) {
        let Some(o) = v.as_object() else { return };
        if let Some(state) = o.get("status").and_then(|s| s.get("state")).and_then(Value::as_str) {
            self.state = Some(task_state(state));
            if let Some(id) = o.get("id").and_then(Value::as_str) {
                self.task(id);
            }
        }
        if let Some(id) = o.get("taskId").and_then(Value::as_str) {
            self.task(id);
        }
        if let Some(c) = o.get("contextId").and_then(Value::as_str) {
            self.context.get_or_insert_with(|| c.to_string());
        }
        if depth < 3 {
            o.values().for_each(|v| self.walk(v, depth + 1));
        }
    }

    fn task(&mut self, id: &str) {
        if !self.tasks.iter().any(|t| t == id) && self.tasks.len() < 16 {
            self.tasks.push(id.to_string());
        }
    }
}

/// A message sent to the agent and its entry in the request log, written when it is dropped: once
/// the answer is complete, or when the client went away first.
struct Exchange {
    state: AppState,
    agent: Uuid,
    key: Uuid,
    tracker: Tracker,
    price: f64,
    seen: Seen,
    /// The agent's answer ran to its end, or failed (rather than the client going away first).
    ended: bool,
    /// The events of a stream, for prompt logging.
    events: Vec<Value>,
}

impl Exchange {
    fn read(&mut self, v: &Value) {
        self.tracker.first_token();
        self.seen.message(v);
        if self.tracker.logging() && self.events.len() < 1000 {
            self.events.push(v.clone());
        }
    }
}

impl Drop for Exchange {
    fn drop(&mut self) {
        let t = &mut self.tracker;
        t.task(self.seen.state.as_deref(), self.seen.context.as_deref());
        if !self.ended {
            if let Some(c) = t.log() {
                c.incomplete();
            }
            t.fail_status(499, "the client went away before the agent answered");
        } else if let Some((code, message)) = &self.seen.error {
            let status = match code {
                -32001 => 404,
                -32002 => 409,
                -32600 | -32602 | -32003 | -32004 | -32005 => 400,
                -32601 => 404,
                _ => 502,
            };
            t.fail_status(status, format!("JSON-RPC error {code}: {message}"));
        } else {
            // The agent answered: it is charged, also for a task that failed.
            t.charge(self.price);
            match self.seen.state.as_deref() {
                Some("failed") => t.fail_status(422, "the task failed"),
                Some("rejected") => t.fail_status(422, "the agent rejected the task"),
                _ => {}
            }
        }
        if let Some(c) = t.log() {
            let events = std::mem::take(&mut self.events);
            c.response(capture::Body::Json(if events.len() == 1 { events.into_iter().next().expect("one") } else { Value::Array(events) }));
        }
        claim(&self.state, self.agent, self.key, &self.seen);
    }
}

// ---------------------------------------------------------------- handlers

/// Headers of the client's request the agent gets; everything else (the client's key above all)
/// stays at the gateway.
const FORWARDED: [&str; 5] = ["accept", "content-type", "a2a-version", "a2a-extensions", "x-a2a-extensions"];
/// Headers of the agent's answer the client gets. Not `WWW-Authenticate`: the agent's credentials
/// are the gateway's.
const RETURNED: [&str; 4] = ["content-type", "a2a-version", "a2a-extensions", "x-a2a-extensions"];
/// An answer that is not streamed is read up to this size.
const MAX_READ: usize = 16 * 1024 * 1024;

/// The agent `name`, if this key may use it.
fn agent(state: &AppState, key: &VirtualKey, name: &str) -> Result<Arc<A2aAgent>, GatewayError> {
    state
        .registry
        .snapshot()
        .a2a_agents
        .get(name)
        .filter(|a| a.allows(key))
        .cloned()
        .ok_or_else(|| GatewayError::not_found(format!("no A2A agent '{name}' that this key may use")))
}

/// The card's agent, unreachable or broken: a gateway error the client can act on.
fn card_error(e: String) -> GatewayError {
    GatewayError { upstream: true, ..GatewayError::new(ErrorKind::Upstream, StatusCode::BAD_GATEWAY, format!("the agent's card: {e}")) }
}

/// Where clients reach the gateway, as they called it: the forwarded scheme and host when a proxy
/// is in front of it.
pub(crate) fn origin(headers: &HeaderMap, uri: &Uri) -> String {
    let h = |n: &str| headers.get(n).and_then(|v| v.to_str().ok()).map(|v| v.split(',').next().unwrap_or(v).trim().to_string());
    let host = h("x-forwarded-host")
        .or_else(|| h("host"))
        .or_else(|| uri.authority().map(|a| a.to_string()))
        .unwrap_or_else(|| "localhost".into());
    let local = host.starts_with("localhost") || host.starts_with("127.") || host.starts_with("[::1]");
    let scheme = h("x-forwarded-proto").unwrap_or_else(|| if local { "http".into() } else { "https".into() });
    format!("{scheme}://{host}")
}

/// A2A: agent card
///
/// The agent's card as the agent serves it, with the gateway's addresses as its interfaces (the newest
/// JSON-RPC and HTTP+JSON ones) and the virtual key as its security.
#[utoipa::path(get, path = "/a2a/{agent}/.well-known/agent-card.json", operation_id = "a2a_card", tag = "A2A", params(("agent" = String, Path)),
    responses(
        (status = 200, description = "The agent card.", body = Object),
        (status = 401, description = "Missing, invalid, disabled or expired key.", body = crate::error::OpenAiError),
        (status = 404, description = "No such agent for this key.", body = crate::error::OpenAiError),
        (status = 502, description = "The agent's card could not be fetched or read.", body = crate::error::OpenAiError),
    ),
    security(("virtual_key" = []), ("virtual_key_header" = [])))]
pub async fn card(
    State(state): State<AppState>,
    ClientKey(key): ClientKey,
    Path(name): Path<String>,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    card_for(&state, &key, &name, &uri, &headers).await
}

/// The agent's card for `key`, with the gateway's addresses. Also the console playground's.
pub async fn card_for(state: &AppState, key: &Arc<VirtualKey>, name: &str, uri: &Uri, headers: &HeaderMap) -> Response {
    let result = async {
        let agent = agent(state, key, name)?;
        let labels = Labels::from_headers(headers).unwrap_or_default();
        let card = state.a2a.get(state, &agent, Some(Who { key: key.id, labels: &labels })).await.map_err(card_error)?;
        Ok::<_, GatewayError>(axum::Json(card.rewritten(&format!("{}/a2a/{}", origin(headers, uri), agent.name))).into_response())
    }
    .await;
    result.unwrap_or_else(|e| e.into_response_for(Dialect::OpenAi))
}

/// A2A: JSON-RPC
///
/// The agent's JSON-RPC interface, in A2A 1.0 (`SendMessage`, …) or 0.3 (`message/send`, …), as the card
/// says. Listing tasks and the extended card are not offered.
#[utoipa::path(post, path = "/a2a/{agent}", operation_id = "a2a_jsonrpc", tag = "A2A", params(("agent" = String, Path),
        ("x-gateway-labels" = Option<String>, Header, description = crate::openapi::LABELS_HEADER)),
    request_body(content = Object, content_type = "application/json"),
    responses(
        (status = 200, description = crate::openapi::A2A_STATES, content((Object = "application/json"), (String = "text/event-stream"))),
        (status = 401, description = "Missing, invalid, disabled or expired key.", body = crate::error::OpenAiError),
        (status = 404, description = "No such agent for this key.", body = crate::error::OpenAiError),
        (status = 429, description = "A rate limit or the key's budget, for a message; see `Retry-After`.", body = crate::error::OpenAiError),
        (status = 502, description = "The agent failed, or refused the gateway's credentials.", body = crate::error::OpenAiError),
    ),
    security(("virtual_key" = []), ("virtual_key_header" = [])))]
pub async fn jsonrpc(
    State(state): State<AppState>,
    ClientKey(key): ClientKey,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    jsonrpc_for(&state, &key, &name, &headers, body).await
}

/// A JSON-RPC call of the agent as `key`. Also the console playground's.
pub async fn jsonrpc_for(state: &AppState, key: &Arc<VirtualKey>, name: &str, headers: &HeaderMap, body: Bytes) -> Response {
    let msg: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let ask = ask_jsonrpc(&msg);
    forward(state, key, name, Binding::JsonRpc, Method::POST, "", None, headers, body, ask, msg.get("id").cloned())
        .await
        .unwrap_or_else(|e| e.into_response_for(Dialect::OpenAi))
}

/// A2A: HTTP+JSON
///
/// The agent's HTTP+JSON interface: `path` as the A2A specification has it, such as `message:send` or
/// `tasks/{id}`.
#[utoipa::path(post, path = "/a2a/{agent}/rest/{*path}", operation_id = "a2a_rest_post", tag = "A2A", params(("agent" = String, Path), ("*path" = String, Path),
        ("x-gateway-labels" = Option<String>, Header, description = crate::openapi::LABELS_HEADER)),
    request_body(content = Object, content_type = "application/json"),
    responses(
        (status = 200, description = crate::openapi::A2A_STATES, content((Object = "application/json"), (String = "text/event-stream"))),
        (status = 403, description = "Listing tasks or the extended card, which are not offered.", body = crate::error::OpenAiError),
        (status = 404, description = "No such agent for this key, or another key's task.", body = crate::error::OpenAiError),
        (status = 429, description = "A rate limit or the key's budget, for a message; see `Retry-After`.", body = crate::error::OpenAiError),
        (status = 502, description = "The agent failed, or refused the gateway's credentials.", body = crate::error::OpenAiError),
    ),
    security(("virtual_key" = []), ("virtual_key_header" = [])))]
pub async fn rest(
    State(state): State<AppState>,
    ClientKey(key): ClientKey,
    Path((name, path)): Path<(String, String)>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(path) = rest_path(&path) else {
        return GatewayError::invalid("the path must stay under the agent's interface: no . or .. segments, and no ? or #")
            .into_response_for(Dialect::OpenAi);
    };
    let ask = ask_rest(&path, &body);
    forward(&state, &key, &name, Binding::Rest, method, &path, uri.query(), &headers, body, ask, None)
        .await
        .unwrap_or_else(|e| e.into_response_for(Dialect::OpenAi))
}

// `rest` for the other methods, each with its own documentation.

/// A2A: HTTP+JSON, read
#[utoipa::path(get, path = "/a2a/{agent}/rest/{*path}", operation_id = "a2a_rest_get", tag = "A2A", params(("agent" = String, Path), ("*path" = String, Path)),
    responses(
        (status = 200, description = "The agent's answer.", content((Object = "application/json"), (String = "text/event-stream"))),
        (status = 404, description = "No such agent for this key, or another key's task.", body = crate::error::OpenAiError),
    ),
    security(("virtual_key" = []), ("virtual_key_header" = [])))]
pub async fn rest_get(
    state: State<AppState>,
    key: ClientKey,
    path: Path<(String, String)>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    rest(state, key, path, method, uri, headers, body).await
}

/// A2A: HTTP+JSON, delete
#[utoipa::path(delete, path = "/a2a/{agent}/rest/{*path}", operation_id = "a2a_rest_delete", tag = "A2A", params(("agent" = String, Path), ("*path" = String, Path)),
    responses(
        (status = 200, description = "The agent's answer."),
        (status = 404, description = "No such agent for this key, or another key's task.", body = crate::error::OpenAiError),
    ),
    security(("virtual_key" = []), ("virtual_key_header" = [])))]
pub async fn rest_delete(
    state: State<AppState>,
    key: ClientKey,
    path: Path<(String, String)>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    rest(state, key, path, method, uri, headers, body).await
}

/// A refusal in the binding's own form: a JSON-RPC error for JSON-RPC (`id` is the request's), an
/// HTTP status for HTTP+JSON.
fn refuse(rpc_id: Option<Value>, binding: Binding, code: i64, status: StatusCode, message: &str) -> Response {
    match binding {
        Binding::JsonRpc => {
            axum::Json(json!({ "jsonrpc": "2.0", "id": rpc_id, "error": { "code": code, "message": message } })).into_response()
        }
        Binding::Rest => {
            GatewayError::new(if status == StatusCode::NOT_FOUND { ErrorKind::NotFound } else { ErrorKind::Permission }, status, message)
                .into_response_for(Dialect::OpenAi)
        }
    }
}

async fn forward(
    state: &AppState,
    key: &Arc<VirtualKey>,
    name: &str,
    binding: Binding,
    method: Method,
    path: &str,
    query: Option<&str>,
    headers: &HeaderMap,
    body: Bytes,
    ask: Ask,
    rpc_id: Option<Value>,
) -> Result<Response, GatewayError> {
    let agent = agent(state, key, name)?;
    let labels = Labels::from_headers(headers)?;
    // A2A's own error codes: -32001 task not found, -32004 unsupported operation.
    let not_found = |what: &str| refuse(rpc_id.clone(), binding, -32001, StatusCode::NOT_FOUND, &format!("{what} not found"));
    let stream = match &ask {
        Ask::Refused(why) => return Ok(refuse(rpc_id, binding, -32004, StatusCode::FORBIDDEN, why)),
        Ask::Task(id) if !may_use(state, agent.id, key.id, 't', id).await => return Ok(not_found("task")),
        Ask::Send { task: Some(id), .. } if !may_use(state, agent.id, key.id, 't', id).await => return Ok(not_found("task")),
        Ask::Send { context: Some(id), .. } if !may_use(state, agent.id, key.id, 'c', id).await => return Ok(not_found("context")),
        Ask::Send { stream, .. } => Some(*stream),
        _ => None,
    };
    let card = state.a2a.get(state, &agent, Some(Who { key: key.id, labels: &labels })).await.map_err(card_error)?;
    let interface =
        card.interface(binding).ok_or_else(|| GatewayError::not_found(format!("the agent offers no {} interface", binding.name())))?;
    let mut exchange = match stream {
        Some(stream) => {
            let mut tracker = Tracker::new(state, Some(key), &labels, &agent.name, "a2a", stream);
            if let Some(c) = tracker.log() {
                c.request(capture::Body::Bytes(body.clone()));
            }
            if let Err(e) = auth::check_key_limits(state, key).await {
                tracker.fail(&e);
                return Err(e);
            }
            Some(Exchange {
                state: state.clone(),
                agent: agent.id,
                key: key.id,
                tracker,
                price: agent.price_per_message,
                seen: Seen::default(),
                ended: false,
                events: Vec::new(),
            })
        }
        None => None,
    };
    let fail = |exchange: &mut Option<Exchange>, e: &GatewayError| {
        if let Some(x) = exchange {
            x.ended = true;
            x.tracker.fail(e);
            x.seen.error = None;
            x.price = 0.0;
        }
    };

    let url = match binding {
        Binding::JsonRpc => interface.url.clone(),
        Binding::Rest => {
            let mut u = format!("{}/{}", interface.url.trim_end_matches('/'), path.trim_start_matches('/'));
            if let Some(q) = query {
                u.push('?');
                u.push_str(q);
            }
            u
        }
    };
    if let Err(why) = state.cfg.upstream_policy.check_url(&url) {
        let e = GatewayError::internal(format!("the agent's address is not allowed: {why}"));
        fail(&mut exchange, &e);
        return Err(e);
    }
    // The client's path, checked above, cannot leave the interface; this holds whatever the URL parser made of it.
    if binding == Binding::Rest && !stays_under(&url, &interface.url) {
        let e = GatewayError::invalid("the path must stay under the agent's interface");
        fail(&mut exchange, &e);
        return Err(e);
    }
    let send = async |auth: Option<(HeaderName, HeaderValue)>| {
        let mut req = state.http.request(method.clone(), &url);
        for h in FORWARDED {
            if let Some(v) = headers.get(h) {
                req = req.header(h, v);
            }
        }
        if let Some((h, v)) = auth {
            req = req.header(h, v);
        }
        // Streams follow their task for as long as it runs.
        if stream != Some(true) {
            req = req.timeout(agent.timeout);
        }
        if !body.is_empty() {
            req = req.body(body.clone());
        }
        req.send().await.map_err(|e| GatewayError::from_reqwest(&e))
    };
    let header =
        async || agent.auth.header(state, Some(Who { key: key.id, labels: &labels })).await.map_err(crate::oauth::AuthError::into_gateway);
    let mut resp = match header().await {
        Ok(auth) => send(auth).await,
        Err(e) => Err(e),
    };
    // A token the agent refuses is renewed, once.
    if agent.auth.renews() && resp.as_ref().is_ok_and(|r| r.status() == StatusCode::UNAUTHORIZED) {
        agent.auth.renew(state, Some(Who { key: key.id, labels: &labels })).await;
        resp = match header().await {
            Ok(auth) => send(auth).await,
            Err(e) => Err(e),
        };
    }
    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            fail(&mut exchange, &e);
            return Err(e);
        }
    };
    let status = resp.status();
    if matches!(status.as_u16(), 401 | 403) {
        tracing::warn!(agent = %agent.name, status = status.as_u16(), "A2A agent refused the gateway's credentials");
        let e = GatewayError {
            upstream: true,
            ..GatewayError::new(
                ErrorKind::Upstream,
                StatusCode::BAD_GATEWAY,
                format!("the agent refused the gateway's credentials (HTTP {})", status.as_u16()),
            )
        };
        fail(&mut exchange, &e);
        return Err(e);
    }
    let mut out = Response::builder().status(status);
    for h in RETURNED {
        if let Some(v) = resp.headers().get(h) {
            out = out.header(h, v);
        }
    }
    let sse = resp.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|t| t.starts_with("text/event-stream"));
    let body = match exchange {
        None => Body::from_stream(resp.bytes_stream()),
        Some(mut x) if !status.is_success() => {
            x.ended = true;
            x.price = 0.0;
            x.tracker.fail_status(status.as_u16(), format!("the agent answered HTTP {}", status.as_u16()));
            Body::from_stream(resp.bytes_stream())
        }
        Some(x) if sse => Body::from_stream(Watch { inner: resp.bytes_stream().boxed(), parser: SseParser::default(), exchange: x }),
        // An answer that is not streamed is read whole, so the tasks in it are the key's before the
        // client can ask about them.
        Some(mut x) => {
            let mut bytes = Vec::new();
            let mut upstream = resp.bytes_stream();
            while let Some(chunk) = upstream.next().await {
                match chunk {
                    Ok(c) if bytes.len() + c.len() <= MAX_READ => bytes.extend_from_slice(&c),
                    Ok(_) => {
                        let e = GatewayError::new(ErrorKind::Upstream, StatusCode::BAD_GATEWAY, "the agent's answer is too large");
                        fail(&mut Some(x), &e);
                        return Err(e);
                    }
                    Err(e) => {
                        let e = GatewayError::from_reqwest(&e);
                        fail(&mut Some(x), &e);
                        return Err(e);
                    }
                }
            }
            x.ended = true;
            if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
                x.read(&v);
            }
            let owner = key.id.to_string();
            let ids: Vec<String> = x
                .seen
                .tasks
                .iter()
                .map(|t| owner_key(agent.id, 't', t))
                .chain(x.seen.context.iter().map(|c| owner_key(agent.id, 'c', c)))
                .collect();
            futures::future::join_all(ids.iter().map(|id| state.cache.set_a2a_owner(id, &owner, OWNERSHIP))).await;
            Body::from(bytes)
        }
    };
    Ok(out.body(body).unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response()))
}

/// A streamed answer, passed on to the client as it arrives, read on the way.
struct Watch {
    inner: BoxStream<'static, reqwest::Result<Bytes>>,
    parser: SseParser,
    exchange: Exchange,
}

impl Watch {
    fn events(&mut self, events: Vec<crate::sse::SseEvent>) {
        for e in events {
            if let Ok(v) = serde_json::from_str::<Value>(&e.data) {
                self.exchange.read(&v);
            }
        }
        // Ownership is recorded as soon as the task shows, while the stream goes on.
        if !self.exchange.seen.tasks.is_empty() {
            claim(&self.exchange.state, self.exchange.agent, self.exchange.key, &self.exchange.seen);
        }
    }
}

impl Stream for Watch {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.inner.poll_next_unpin(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                let events = self.parser.feed(&chunk);
                if !events.is_empty() {
                    self.events(events);
                }
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(e))) => {
                let e = GatewayError::from_reqwest(&e);
                self.exchange.ended = true;
                self.exchange.tracker.fail(&e);
                Poll::Ready(Some(Err(std::io::Error::other(e.message))))
            }
            Poll::Ready(None) => {
                let events = self.parser.finish();
                self.events(events);
                self.exchange.ended = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_read_in_both_protocol_versions() {
        let m = |method: &str, params: Value| ask_jsonrpc(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }));
        let msg = json!({ "message": { "taskId": "t1", "contextId": "c1" } });
        let sent = Ask::Send { stream: false, task: Some("t1".into()), context: Some("c1".into()) };
        assert_eq!(m("SendMessage", msg.clone()), sent);
        assert_eq!(m("message/send", msg.clone()), sent);
        assert!(matches!(m("SendStreamingMessage", json!({ "message": {} })), Ask::Send { stream: true, task: None, .. }));
        assert_eq!(m("GetTask", json!({ "id": "t1" })), Ask::Task("t1".into()));
        assert_eq!(m("tasks/cancel", json!({ "id": "t1" })), Ask::Task("t1".into()));
        assert_eq!(m("GetTaskPushNotificationConfig", json!({ "taskId": "t1", "id": "cfg" })), Ask::Task("t1".into()));
        assert_eq!(m("tasks/pushNotificationConfig/get", json!({ "id": "t1" })), Ask::Task("t1".into()));
        assert_eq!(m("ListTasks", json!({})), Ask::Refused(NO_LISTING));
        assert_eq!(m("GetExtendedAgentCard", json!({})), Ask::Refused(NO_EXTENDED_CARD));
        assert_eq!(m("Something", json!({})), Ask::Other);

        assert!(
            matches!(ask_rest("message:send", br#"{"message":{"taskId":"t9"}}"#), Ask::Send { stream: false, task: Some(t), .. } if t == "t9")
        );
        assert!(matches!(ask_rest("v1/message:stream", b"{}"), Ask::Send { stream: true, .. }));
        assert_eq!(ask_rest("tasks/t1", b""), Ask::Task("t1".into()));
        assert_eq!(ask_rest("tenant-a/tasks/t1:cancel", b""), Ask::Task("t1".into()));
        assert_eq!(ask_rest("tasks/t1/pushNotificationConfigs/c", b""), Ask::Task("t1".into()));
        assert_eq!(ask_rest("tasks", b""), Ask::Refused(NO_LISTING));
        assert_eq!(ask_rest("extendedAgentCard", b""), Ask::Refused(NO_EXTENDED_CARD));
    }

    #[test]
    fn rest_paths_stay_under_the_interface() {
        assert_eq!(rest_path("v1/message:send").as_deref(), Some("v1/message:send"));
        assert_eq!(rest_path("/tasks/t 1").as_deref(), Some("tasks/t%201"));
        for bad in ["../admin", "a/../../b", "a\\..\\b", "./tasks", "tasks#x", "tasks?all=1"] {
            assert_eq!(rest_path(bad), None, "{bad}");
        }
        let iface = "https://agent.example/a2a/rest";
        assert!(stays_under("https://agent.example/a2a/rest/v1/tasks/t1", iface));
        assert!(!stays_under("https://agent.example/a2a/admin", iface));
        assert!(!stays_under("https://agent.example/a2a/restricted", iface));
        assert!(!stays_under("https://other.example/a2a/rest/x", iface));
        assert!(stays_under("https://agent.example/x", "https://agent.example/"));
    }

    #[test]
    fn answers_give_their_tasks_contexts_and_states() {
        // 1.0: a task, then a stream's status update.
        let mut s = Seen::default();
        s.message(&json!({ "jsonrpc": "2.0", "id": 1, "result": { "task": { "id": "t1", "contextId": "c1", "status": { "state": "TASK_STATE_WORKING" } } } }));
        s.message(&json!({ "jsonrpc": "2.0", "id": 1, "result": { "statusUpdate": { "taskId": "t1", "contextId": "c1", "status": { "state": "TASK_STATE_INPUT_REQUIRED" } } } }));
        assert_eq!((s.tasks, s.context.as_deref(), s.state.as_deref()), (vec!["t1".to_string()], Some("c1"), Some("input-required")));
        // 0.3: the task is the result.
        let mut s = Seen::default();
        s.message(&json!({ "jsonrpc": "2.0", "id": 1, "result": { "kind": "task", "id": "t2", "contextId": "c2", "status": { "state": "completed" } } }));
        assert_eq!((s.tasks, s.state.as_deref()), (vec!["t2".to_string()], Some("completed")));
        let mut s = Seen::default();
        s.message(&json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": -32001, "message": "Task not found" } }));
        assert_eq!(s.error, Some((-32001, "Task not found".into())));
    }

    fn card_v1() -> Value {
        json!({
            "name": "Weather", "version": "1", "capabilities": { "streaming": true, "extendedAgentCard": true },
            "supportedInterfaces": [
                { "url": "https://agent.example/a2a/v03", "protocolBinding": "JSONRPC", "protocolVersion": "0.3" },
                { "url": "https://agent.example/a2a", "protocolBinding": "JSONRPC", "protocolVersion": "1.0" },
                { "url": "https://agent.example/rest", "protocolBinding": "HTTP+JSON", "protocolVersion": "1.0" },
                { "url": "agent.example:443", "protocolBinding": "GRPC", "protocolVersion": "1.0" },
            ],
            "securitySchemes": { "oauth": { "oauth2SecurityScheme": {} } },
            "signatures": [{ "protected": "x", "signature": "y" }],
        })
    }

    #[test]
    fn cards_point_clients_at_the_gateway() {
        let card = Card::parse(card_v1()).unwrap();
        assert_eq!(card.jsonrpc.as_ref().unwrap().url, "https://agent.example/a2a");
        let v = card.rewritten("https://gw.example/a2a/weather");
        let urls: Vec<&str> = v["supportedInterfaces"].as_array().unwrap().iter().map(|i| i["url"].as_str().unwrap()).collect();
        assert_eq!(urls, ["https://gw.example/a2a/weather", "https://gw.example/a2a/weather/rest"]);
        assert!(!v.to_string().contains("agent.example"), "{v}");
        assert!(v.get("signatures").is_none());
        assert_eq!(v["capabilities"]["extendedAgentCard"], false);
        assert_eq!(v["securitySchemes"]["gateway"]["httpAuthSecurityScheme"]["scheme"], "Bearer");

        let legacy = Card::parse(json!({
            "name": "Old", "protocolVersion": "0.3.0", "url": "https://old.example/rpc", "preferredTransport": "JSONRPC",
            "additionalInterfaces": [{ "url": "https://old.example/rpc", "transport": "JSONRPC" }, { "url": "https://old.example/grpc", "transport": "GRPC" }],
            "securitySchemes": { "key": { "type": "apiKey", "in": "header", "name": "X-Key" } }, "security": [{ "key": [] }],
            "supportsAuthenticatedExtendedCard": true,
        }))
        .unwrap();
        let v = legacy.rewritten("https://gw.example/a2a/old");
        assert_eq!(v["url"], "https://gw.example/a2a/old");
        assert_eq!(v["additionalInterfaces"], json!([{ "url": "https://gw.example/a2a/old", "transport": "JSONRPC" }]));
        assert_eq!(v["security"], json!([{ "gateway": [] }]));
        assert!(!v.to_string().contains("old.example"), "{v}");

        assert!(Card::parse(json!({ "supportedInterfaces": [{ "url": "x:1", "protocolBinding": "GRPC" }] })).is_err());
    }

    /// An A2A 1.0 agent on loopback that takes only the gateway's secret: its card, and `SendMessage`
    /// answering with a task whose state is the message's text.
    async fn upstream() -> String {
        use axum::{Json, routing::get, routing::post};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let card = json!({
            "name": "Echo", "version": "1", "capabilities": {},
            "supportedInterfaces": [{ "url": format!("{base}/rpc"), "protocolBinding": "JSONRPC", "protocolVersion": "1.0" }],
        });
        let authorized = |h: &HeaderMap| h.get(header::AUTHORIZATION).map(|v| v.as_bytes()) == Some(b"Bearer agent-secret");
        let app = axum::Router::new()
            .route(
                "/.well-known/agent-card.json",
                get(move |h: HeaderMap| async move {
                    if authorized(&h) { Json(card).into_response() } else { StatusCode::UNAUTHORIZED.into_response() }
                }),
            )
            .route(
                "/rpc",
                post(move |h: HeaderMap, Json(m): Json<Value>| async move {
                    if !authorized(&h) {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    let state = m.pointer("/params/message/parts/0/text").and_then(Value::as_str).unwrap_or("TASK_STATE_COMPLETED");
                    let task = json!({ "id": "task-1", "contextId": "ctx-1", "status": { "state": state } });
                    Json(json!({ "jsonrpc": "2.0", "id": m["id"], "result": { "task": task } })).into_response()
                }),
            );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("{base}/.well-known/agent-card.json")
    }

    #[tokio::test]
    async fn messages_are_forwarded_logged_and_charged_and_other_keys_tasks_stay_hidden() {
        use tower::ServiceExt;
        let (state, mut records) = crate::state::test_state_local();
        let mut snap = crate::registry::Snapshot::default();
        let key = VirtualKey { key_hash: crate::util::sha256(b"sk-gw-a").to_vec(), ..crate::auth::tests::key() };
        snap.keys.insert(crate::util::sha256(b"sk-gw-a"), Arc::new(key));
        let auth = UpstreamAuth::Static(header::AUTHORIZATION, HeaderValue::from_static("Bearer agent-secret"));
        let agent = A2aAgent {
            id: Uuid::now_v7(),
            name: "echo".into(),
            card_url: upstream().await,
            auth,
            tags: vec![],
            project_ids: vec![],
            timeout: Duration::from_secs(5),
            price_per_message: 0.02,
        };
        snap.a2a_agents.insert("echo".into(), Arc::new(agent));
        state.registry.set_snapshot(snap);
        let app = axum::Router::new()
            .route("/a2a/{agent}", axum::routing::post(jsonrpc))
            .route("/a2a/{agent}/.well-known/agent-card.json", axum::routing::get(card))
            .with_state(state);
        let send = |req: axum::http::request::Builder, body: Value| {
            let app = app.clone();
            async move {
                let req = req
                    .header("authorization", "Bearer sk-gw-a")
                    .header("host", "gw.example.com")
                    .header("content-type", "application/json");
                let resp = app.oneshot(req.body(Body::from(body.to_string())).unwrap()).await.unwrap();
                let status = resp.status();
                let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
                (status, serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null))
            }
        };

        let (status, card) = send(axum::http::Request::get("/a2a/echo/.well-known/agent-card.json"), Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(card["supportedInterfaces"][0]["url"], "https://gw.example.com/a2a/echo");

        let message = |text: &str| json!({ "jsonrpc": "2.0", "id": 1, "method": "SendMessage", "params": { "message": { "parts": [{ "text": text }] } } });
        let (status, reply) = send(axum::http::Request::post("/a2a/echo"), message("TASK_STATE_INPUT_REQUIRED")).await;
        assert_eq!((status, &reply["result"]["task"]["id"]), (StatusCode::OK, &json!("task-1")));
        let r = records.try_recv().unwrap();
        assert_eq!(
            (r.api, r.model.as_str(), r.status_code, r.task_state.as_deref(), r.context_id.as_deref(), r.cost_usd),
            ("a2a", "echo", 200, Some("input-required"), Some("ctx-1"), 0.02)
        );
        send(axum::http::Request::post("/a2a/echo"), message("TASK_STATE_FAILED")).await;
        let r = records.try_recv().unwrap();
        assert_eq!((r.status_code, r.task_state.as_deref()), (422, Some("failed")));

        // Without Valkey no task is known to belong to anyone: asking about one is answered as for
        // an unknown task, and nothing is logged.
        let (_, reply) = send(
            axum::http::Request::post("/a2a/echo"),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "GetTask", "params": { "id": "task-1" } }),
        )
        .await;
        assert_eq!(reply["error"]["code"], -32001);
        let (_, reply) =
            send(axum::http::Request::post("/a2a/echo"), json!({ "jsonrpc": "2.0", "id": 3, "method": "ListTasks", "params": {} })).await;
        assert_eq!(reply["error"]["code"], -32004);
        assert!(records.try_recv().is_err());
    }

    #[test]
    fn the_card_names_the_gateway_as_clients_reach_it() {
        let h = |pairs: &[(&'static str, &'static str)]| {
            pairs.iter().map(|(k, v)| (HeaderName::from_static(k), HeaderValue::from_static(v))).collect::<HeaderMap>()
        };
        let uri = Uri::from_static("/a2a/x");
        assert_eq!(origin(&h(&[("host", "gw.example.com")]), &uri), "https://gw.example.com");
        assert_eq!(origin(&h(&[("host", "localhost:8080")]), &uri), "http://localhost:8080");
        assert_eq!(
            origin(&h(&[("host", "10.0.0.5"), ("x-forwarded-host", "ai.example.com"), ("x-forwarded-proto", "https")]), &uri),
            "https://ai.example.com"
        );
    }
}
