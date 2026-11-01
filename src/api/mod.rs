//! Public inference endpoints and the shared request pipeline.

pub mod anthropic;
pub mod chat;
pub mod decisions;
pub mod embeddings;
mod guard;
pub mod mcp_tools;
pub mod models;
pub mod rerank;
pub mod responses;
pub mod routing;
pub mod sign_ins;

use std::{collections::HashSet, convert::Infallible, sync::Arc};

use axum::{
    Json,
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use opentelemetry::context::FutureExt as _;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    accounting::{Rollup, Tracker},
    affinity::Affinity,
    auth::ClientKey,
    capture::{Body as Logged, ChatStream, MessagesStream, Reassembler, ResponsesStream},
    conversations::{self, History, Previous, Turn},
    error::{Dialect, ErrorKind, GatewayError},
    ir::{Aggregator, ChatRequest, ChatResponse, Message, Part, Role, StreamEvent, Usage},
    labels::{ClientLabels, Labels},
    model::{ApiStyle, AutoRouter, Deployment, Mode, PromptLogging, ProviderKind, VirtualKey},
    providers::{self, ByteStream, EventStream, Op},
    registry::Snapshot,
    sse::SseParser,
    state::AppState,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inbound {
    Messages,
    Responses,
    Chat,
}

impl Inbound {
    pub fn dialect(self) -> Dialect {
        match self {
            Self::Messages => Dialect::Anthropic,
            Self::Responses | Self::Chat => Dialect::OpenAi,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Messages => "messages",
            Self::Responses => "responses",
            Self::Chat => "chat",
        }
    }

    /// Upstream speaks the same protocol: forward the request untouched (full feature fidelity).
    fn is_native(self, d: &Deployment) -> bool {
        matches!(
            (self, d.style),
            (Self::Messages, ApiStyle::Anthropic) | (Self::Responses, ApiStyle::Responses) | (Self::Chat, ApiStyle::Chat)
        )
    }
}

/// Converts normalized events into a client wire format.
pub trait StreamEncoder: Send + 'static {
    fn start(&mut self) -> Vec<Bytes>;
    fn event(&mut self, ev: &StreamEvent) -> Vec<Bytes>;
    fn error(&mut self, e: &GatewayError) -> Vec<Bytes>;
}

pub fn sse_event(event: &str, data: &Value) -> Bytes {
    let mut s = String::with_capacity(64);
    s.push_str("event: ");
    s.push_str(event);
    s.push_str("\ndata: ");
    s.push_str(&serde_json::to_string(data).expect("json"));
    s.push_str("\n\n");
    Bytes::from(s)
}

// Every public endpoint takes `ClientKey` before its body, so unauthenticated requests are
// refused from their headers and never get a body buffered (see `auth::ClientKey`).

pub async fn messages(
    State(state): State<AppState>,
    ClientKey(key): ClientKey,
    ClientLabels(labels): ClientLabels,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    serve(&state, key, labels, &headers, body, Inbound::Messages).await
}

pub async fn responses(
    State(state): State<AppState>,
    ClientKey(key): ClientKey,
    ClientLabels(labels): ClientLabels,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    serve(&state, key, labels, &headers, body, Inbound::Responses).await
}

pub async fn chat_completions(
    State(state): State<AppState>,
    ClientKey(key): ClientKey,
    ClientLabels(labels): ClientLabels,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    serve(&state, key, labels, &headers, body, Inbound::Chat).await
}

/// Run one inference request as an already authenticated virtual key. Also used by the admin
/// playground, which picks the key by id instead of by secret.
pub async fn serve(state: &AppState, key: Arc<VirtualKey>, labels: Labels, headers: &HeaderMap, body: Bytes, inbound: Inbound) -> Response {
    let (state, headers) = (state.clone(), headers.clone());
    let result = to_completion(async move { run(&state, key, &labels, &headers, body, inbound).await }).await;
    match result {
        Ok(r) => r,
        Err(e) => e.into_response_for(inbound.dialect()),
    }
}

/// Runs a request in its own task, so it finishes, and is accounted, even when the client stops
/// waiting: hyper drops the handler of a closed connection, which would cancel the upstream call
/// mid-flight and log no usage for what the provider bills. A streamed response returns as soon
/// as it starts; its body is accounted as it is sent (see `Tracker`).
pub(crate) async fn to_completion<T: Send + 'static>(
    fut: impl Future<Output = Result<T, GatewayError>> + Send + 'static,
) -> Result<T, GatewayError> {
    // The task stays in the request's trace (see `telemetry::http_span`).
    tokio::spawn(fut.with_current_context()).await.unwrap_or_else(|e| {
        tracing::error!(error = %e, "request task failed");
        Err(GatewayError::internal("internal error"))
    })
}

enum Outcome {
    Stream { first: StreamEvent, rest: EventStream },
    Full(ChatResponse),
    PassStream(ByteStream),
    PassFull(Bytes, Usage),
}

async fn run(
    state: &AppState,
    key: Arc<VirtualKey>,
    labels: &Labels,
    headers: &HeaderMap,
    body: Bytes,
    inbound: Inbound,
) -> Result<Response, GatewayError> {
    let raw: Value = serde_json::from_slice(&body).map_err(|e| GatewayError::invalid(format!("invalid JSON body: {e}")))?;
    if !raw.is_object() {
        return Err(GatewayError::invalid("request body must be a JSON object"));
    }
    let model = raw.get("model").and_then(Value::as_str).ok_or_else(|| GatewayError::invalid("model is required"))?;
    // Before routing, so a refused request costs no decision call either.
    check_billing_options(&raw, inbound)?;
    let snap = state.registry.snapshot();
    let requested = headers.get(crate::guardrails::HEADER).and_then(|v| v.to_str().ok());
    if asks_web_search(&raw) && !web_search_allowed(&snap, &key) {
        return Err(GatewayError::forbidden("web search is not allowed for this key"));
    }
    let project = key.project_id.and_then(|p| snap.projects.get(&p));
    let enforced: Vec<Uuid> = key.guardrail_ids.iter().chain(project.into_iter().flat_map(|p| &p.guardrail_ids)).copied().collect();
    let guards = crate::guardrails::for_request(&snap.guardrails, &enforced, requested)?;
    // A router reached through an alias goes by the alias's name, in the log as in its errors.
    let router = match snap.routers.get(snap.resolve(model)) {
        Some(r) if r.model_name != model => Some(Arc::new(AutoRouter { model_name: model.to_string(), ..(**r).clone() })),
        r => r.cloned(),
    };
    match router {
        Some(router) => {
            if !key.allows_model(model) || !key.allows_router(&router) {
                return Err(GatewayError::forbidden(format!("this key is not allowed to use model '{model}'")));
            }
            routing::route(state, key, labels, headers, raw, &body, inbound, router, &guards).await
        }
        None => run_model(state, key, labels, headers, raw, &body, inbound, None, &guards).await,
    }
}

/// What the guardrails did to the prompt: `x-gateway-guardrail` (passed, flagged, masked), and when
/// they masked it, `x-gateway-guardrail-prompt`, the prompt's last text as the model got it, base64
/// (UTF-8 does not fit in a header). Left out when long, as headers are limited in size.
pub(crate) fn guardrail_header(resp: &mut Response, screened: Option<&guard::Screened>) {
    use base64::Engine;
    let Some(s) = screened else { return };
    resp.headers_mut().insert("x-gateway-guardrail", HeaderValue::from_static(s.action.as_str()));
    if let Some(text) = &s.masked_prompt {
        let encoded = base64::engine::general_purpose::STANDARD.encode(text);
        if encoded.len() <= 8192
            && let Ok(v) = HeaderValue::from_str(&encoded)
        {
            resp.headers_mut().insert("x-gateway-guardrail-prompt", v);
        }
    }
}

/// Serve a request on a model's deployments. `parent` is set for the routed call of an auto
/// router, whose request already passed the key's limits and `guards`, and whose labels it
/// carries. `body` is the request as the client sent it: its size estimates the input tokens
/// should the upstream report none, and a logged request keeps it.
pub(crate) async fn run_model(
    state: &AppState,
    key: Arc<VirtualKey>,
    labels: &Labels,
    headers: &HeaderMap,
    mut raw: Value,
    body: &Bytes,
    inbound: Inbound,
    parent: Option<Arc<Rollup>>,
    guards: &[Arc<crate::guardrails::Active>],
) -> Result<Response, GatewayError> {
    let model = raw.get("model").and_then(Value::as_str).unwrap_or_default().to_string();
    let stream = raw.get("stream").and_then(Value::as_bool).unwrap_or(false);
    if !key.allows_model(&model) {
        return Err(GatewayError::forbidden(format!("this key is not allowed to use model '{model}'")));
    }
    let mut candidates = usable_deployments(state, &key, &model, Mode::Chat)?;

    let mut tracker = match &parent {
        Some(p) => Tracker::child(state, p, true, &model, inbound.name(), stream),
        None => Tracker::new(state, Some(&key), labels, &model, inbound.name(), stream),
    };
    tracker.estimate_input(body.len());
    // A routed call logs the request as routed (its model and effort set); the router's entry
    // keeps it as the client sent it.
    if let Some(c) = tracker.log() {
        c.request(if parent.is_some() { Logged::Json(raw.clone()) } else { Logged::Bytes(body.clone()) });
    }
    // Before the conversation lookup, so a key over its limits cannot make the gateway read
    // stored conversations either.
    if parent.is_none()
        && let Err(e) = crate::auth::check_key_limits(state, &key).await
    {
        tracker.fail(&e);
        return Err(e);
    }
    // After the limits, so a key over them pays for no guardrail call either.
    let mut guarded = None;
    if !guards.is_empty() {
        match guard::screen_prompt(state, inbound, &mut raw, guards, tracker.logging()).await {
            Ok((charge, screened)) => {
                tracker.guardrails(charge);
                if screened.action == crate::guardrails::Action::Masked
                    && let Some(c) = tracker.log()
                {
                    c.screened(raw.clone());
                }
                guarded = Some(screened);
            }
            Err((charge, e)) => {
                tracker.guardrails(charge);
                tracker.fail(&e);
                return Err(e);
            }
        }
    }

    // Gateway MCP tools: taken out of the request, tool routers replaced by the servers they
    // pick, listed for the key, run by the gateway.
    let mut routed = Vec::new();
    let toolset = match mcp_tools::take(&mut raw, inbound) {
        Ok(w) if w.is_empty() => None,
        Ok(w) => {
            let (w, r) = mcp_tools::route(state, &key, labels, &raw, inbound, w).await;
            routed = r;
            match mcp_tools::toolset(state, &key, labels, w).await {
                Ok(set) => Some(Arc::new(set)),
                Err(e) => {
                    tracker.fail(&e);
                    return Err(e);
                }
            }
        }
        Err(e) => {
            tracker.fail(&e);
            return Err(e);
        }
    };

    if parent.is_none() && tracker.logging() {
        let conversation = Affinity::fingerprint(state, &raw, inbound);
        tracker.log().expect("logging").conversation(conversation);
    }

    // Responses API conversation state. Like OpenAI, `store` defaults to true.
    let store = inbound == Inbound::Responses && raw.get("store").and_then(Value::as_bool).unwrap_or(true);
    let previous_id = match inbound {
        Inbound::Responses => raw.get("previous_response_id").and_then(Value::as_str).map(str::to_string),
        Inbound::Messages | Inbound::Chat => None,
    };
    let previous = match &previous_id {
        Some(p) => match conversations::resolve(state, p, key.id).await {
            Ok(prev) => Some(prev),
            Err(e) => {
                tracker.fail(&e);
                return Err(e);
            }
        },
        None => None,
    };
    let (preferred, max_attempts) = match &previous {
        // We hold the transcript: prefer the deployment that served the last turn (prompt cache,
        // reasoning signatures) but any deployment of the model can continue.
        Some(Previous::Gateway(h)) => (h.last_deployment(), state.settings.get().max_attempts as usize),
        // The upstream holds the conversation: only that exact deployment (Azure resource,
        // OpenAI account) knows the id, so there is nowhere else to fail over to.
        Some(Previous::Upstream(pin)) => {
            candidates.retain(|d| d.id == pin.deployment_id && inbound.is_native(d));
            if candidates.is_empty() {
                let e = GatewayError::invalid(format!(
                    "the previous response is stored by a deployment that no longer serves '{model}' over the \
                     Responses API (removed, disabled or reconfigured); start a new conversation"
                ));
                tracker.fail(&e);
                return Err(e);
            }
            (Some(pin.deployment_id), 1)
        }
        None => (None, state.settings.get().max_attempts as usize),
    };
    // Sticky routing: a conversation goes back to the deployment that served its earlier turns,
    // where the provider's prompt cache for it is. Stored conversations already know theirs, and
    // a model with one deployment has nowhere else to go.
    let affinity = (previous.is_none() && candidates.len() > 1).then(|| Affinity::of(state, &raw, inbound)).flatten();
    let sticky = match &affinity {
        Some(a) => a.deployment(state, &model).await,
        None => None,
    };
    let preferred = preferred.or(sticky);
    let gateway_history = match &previous {
        Some(Previous::Gateway(h)) => Some(h),
        _ => None,
    };
    // Our response id for translated Responses turns; it embeds the conversation root.
    let (conv_root, response_id) = conversations::new_response_id(gateway_history.map(|_| previous_id.as_deref().unwrap_or_default()));

    let mut ir: Option<ChatRequest> = None;
    let mut tried = HashSet::new();
    let mut last_err: Option<GatewayError> = None;
    let mut outcome = None;
    let mut served_by = None;
    for _ in 0..max_attempts {
        let d = match crate::router::pick(state, &candidates, &tried, preferred).await {
            Ok(d) => d,
            Err(e) => {
                last_err.get_or_insert(e);
                break;
            }
        };
        tried.insert(d.id);
        tracker.attempt(d.clone());
        // A gateway-held transcript must be replayed, and gateway MCP tools run in rounds, so both
        // are translated even for native upstreams.
        let result = if inbound.is_native(&d) && gateway_history.is_none() && toolset.is_none() {
            passthrough(state, &d, &raw, stream, inbound, headers).await
        } else {
            if ir.is_none() {
                let parsed = match inbound {
                    Inbound::Messages => anthropic::parse_request(&raw),
                    Inbound::Responses => responses::parse_request(&raw),
                    Inbound::Chat => chat::parse_request(&raw),
                };
                let parsed = parsed.and_then(|mut p| {
                    if let Some(set) = &toolset {
                        mcp_tools::offer(&mut p, set)?;
                    }
                    Ok(p)
                });
                match parsed {
                    Ok(p) => ir = Some(p),
                    Err(e) => {
                        tracker.fail(&e);
                        return Err(e);
                    }
                }
            }
            let turn = ir.as_ref().unwrap();
            let turn = match gateway_history {
                Some(h) => std::borrow::Cow::Owned(with_history(turn, &raw, h, d.provider)),
                None => std::borrow::Cow::Borrowed(turn),
            };
            match &toolset {
                Some(set) => {
                    let caller = mcp_tools::Caller { key: key.clone(), labels: labels.clone() };
                    match mcp_tools::start(state, &d, turn.into_owned(), set.clone(), caller).await {
                        Ok((first, rest)) if stream => Ok(Outcome::Stream { first, rest }),
                        Ok((first, rest)) => collect(first, rest).await,
                        Err(e) => Err(e),
                    }
                }
                None => translate(state, &d, &turn, stream).await,
            }
        };
        match result {
            Ok(o) => {
                outcome = Some(o);
                served_by = Some(d);
                break;
            }
            Err(e) if e.is_retryable() => {
                tracing::warn!(deployment = %d.id, model = %model, error = %e, "attempt failed; trying another deployment");
                crate::router::failed(state, &d).await;
                last_err = Some(e);
            }
            Err(e) => {
                tracker.fail(&e);
                return Err(e);
            }
        }
    }

    let Some(outcome) = outcome else {
        let e = last_err.unwrap_or_else(|| GatewayError::unavailable("no deployment available"));
        tracker.fail(&e);
        return Err(e);
    };

    let served_by = served_by.expect("set together with outcome");
    if let Some(a) = affinity
        && sticky != Some(served_by.id)
    {
        let (state, model, id) = (state.clone(), model.clone(), served_by.id);
        tokio::spawn(async move { a.set_deployment(&state, &model, id).await });
    }

    // What to persist so this response can be continued with `previous_response_id`.
    let turn_sink = (store && matches!(outcome, Outcome::Full(_) | Outcome::Stream { .. })).then(|| {
        let turn = ir.as_ref().expect("translated requests are parsed");
        TurnSink {
            state: state.clone(),
            root: conv_root.clone(),
            response_id: response_id.clone(),
            turn: Turn {
                parent: gateway_history.and(previous_id.clone()),
                key_id: key.id,
                deployment_id: served_by.id,
                provider: served_by.provider,
                system: responses::input_system(turn, &raw),
                messages: turn.messages.clone(),
            },
        }
    });
    let pin_sink = store.then(|| PinSink { state: state.clone(), key_id: key.id, deployment_id: served_by.id });

    let request_id = HeaderValue::from_str(&tracker.id.to_string()).expect("uuid header");
    let mut resp = match outcome {
        Outcome::Full(r) => {
            tracker.set_usage(r.usage);
            let v = match inbound {
                Inbound::Messages => anthropic::render_response(&r, &crate::util::prefixed_id("msg"), &model),
                Inbound::Responses => responses::render_response(&r, &response_id, &model),
                Inbound::Chat => chat::render_response(&r, &format!("chatcmpl-{}", crate::util::random_hex(12)), &model),
            };
            if let Some(c) = tracker.log() {
                c.response(Logged::Json(logged_response(v.clone(), &r, inbound)));
            }
            if let Some(sink) = turn_sink {
                sink.finish(r.history());
            }
            Json(v).into_response()
        }
        Outcome::PassFull(bytes, usage) => {
            tracker.set_usage(usage);
            if let Some(c) = tracker.log() {
                c.response(Logged::Bytes(bytes.clone()));
            }
            if let Some(pin) = pin_sink
                && let Some(id) = serde_json::from_slice::<Value>(&bytes).ok().and_then(|v| v["id"].as_str().map(str::to_string))
            {
                pin.save(id);
            }
            ([(header::CONTENT_TYPE, "application/json")], bytes).into_response()
        }
        Outcome::Stream { first, rest } => {
            let id = match inbound {
                Inbound::Messages => crate::util::prefixed_id("msg"),
                Inbound::Responses => response_id.clone(),
                Inbound::Chat => format!("chatcmpl-{}", crate::util::random_hex(12)),
            };
            let enc: Box<dyn StreamEncoder> = match inbound {
                Inbound::Messages => Box::new(anthropic::AnthropicEncoder::new(id.clone(), model.clone())),
                Inbound::Responses => Box::new(responses::ResponsesEncoder::new(id.clone(), model.clone())),
                Inbound::Chat => {
                    let include_usage = raw.pointer("/stream_options/include_usage").and_then(Value::as_bool).unwrap_or(false);
                    Box::new(chat::ChatEncoder::new(id.clone(), model.clone(), include_usage))
                }
            };
            // A logged stream is stored as the same response would be without streaming.
            let render: Option<Render> = tracker.logging().then(|| -> Render {
                let model = model.clone();
                Box::new(move |r: &ChatResponse| {
                    let v = match inbound {
                        Inbound::Messages => anthropic::render_response(r, &id, &model),
                        Inbound::Responses => responses::render_response(r, &id, &model),
                        Inbound::Chat => chat::render_response(r, &id, &model),
                    };
                    logged_response(v, r, inbound)
                })
            });
            sse_response(Body::from_stream(encode_stream(first, rest, enc, tracker, turn_sink, render)))
        }
        Outcome::PassStream(bytes) => sse_response(Body::from_stream(sniff_stream(bytes, inbound, tracker, pin_sink))),
    };
    resp.headers_mut().insert("x-request-id", request_id);
    guardrail_header(&mut resp, guarded.as_ref());
    if !routed.is_empty() {
        for (name, value) in mcp_tools::Routed::headers(&routed) {
            if let Ok(v) = HeaderValue::from_str(&value) {
                resp.headers_mut().insert(name, v);
            }
        }
    }
    Ok(resp)
}

/// The deployments of a model that serve `mode`. A model asked for on the wrong family of
/// endpoints gets told which endpoints serve it. An alias's are its target's.
pub(crate) fn deployments_for(state: &AppState, model: &str, mode: Mode) -> Result<Vec<Arc<Deployment>>, GatewayError> {
    let snap = state.registry.snapshot();
    let target = snap.resolve(model);
    match snap.deployments.get(target) {
        Some(candidates) => for_mode(candidates.clone(), model, mode),
        None if snap.routers.contains_key(target) => {
            Err(GatewayError::invalid(format!("'{model}' is an auto router for chat models; call {}", Mode::Chat.endpoints())))
        }
        None => Err(GatewayError::not_found(format!("model '{model}' not found"))),
    }
}

/// The deployments of `model` serving `mode` that `key` may use (see
/// `VirtualKey::allows_deployment`). A model none of whose deployments the key may use is
/// forbidden, like one outside its `allowed_models`.
pub(crate) fn usable_deployments(
    state: &AppState,
    key: &VirtualKey,
    model: &str,
    mode: Mode,
) -> Result<Vec<Arc<Deployment>>, GatewayError> {
    let mut candidates = deployments_for(state, model, mode)?;
    let any = !candidates.is_empty();
    candidates.retain(|d| key.allows_deployment(d));
    if any && candidates.is_empty() {
        return Err(GatewayError::forbidden(format!("this key is not allowed to use model '{model}'")));
    }
    Ok(candidates)
}

/// Whether `key` may call `model`: a model through at least one of its deployments, a router
/// when its tags allow the key and through at least one of its routes. An alias by its own name
/// in `allowed_models`, then as its target.
pub(crate) fn key_can_use(snap: &Snapshot, key: &VirtualKey, model: &str) -> bool {
    let reaches = |m: &str| snap.deployments.get(m).is_some_and(|ds| ds.iter().any(|d| key.allows_deployment(d)));
    let target = snap.resolve(model);
    key.allows_model(model)
        && match snap.routers.get(target) {
            Some(r) => key.allows_router(r) && r.models().any(|m| key.allows_model(m) && reaches(m)),
            None => reaches(target),
        }
}

/// Whether `key` may have the model search the web: neither the key nor its project forbids it.
pub(crate) fn web_search_allowed(snap: &Snapshot, key: &VirtualKey) -> bool {
    key.web_search && key.project_id.and_then(|p| snap.projects.get(&p)).is_none_or(|p| p.web_search)
}

/// Whether `key`'s prompts and responses are logged, for debugging. Its project's policy decides
/// (the gateway's, `settings.prompt_logging`, for keys outside projects): `force` logs every key,
/// `deny` none, `allow` those that opt in. A key whose project the registry does not know is not
/// logged.
pub(crate) fn prompt_logged(snap: &Snapshot, settings: &crate::settings::Settings, key: &VirtualKey) -> bool {
    let policy = match key.project_id {
        Some(p) => snap.projects.get(&p).map_or(PromptLogging::Deny, |p| p.prompt_logging),
        None => settings.prompt_logging,
    };
    match policy {
        PromptLogging::Force => true,
        PromptLogging::Deny => false,
        PromptLogging::Allow => key.prompt_logging,
    }
}

/// Whether a request body asks for the provider's web search, in any API's field: Chat
/// Completions' `web_search_options` (`{}` too), or a `web_search…` tool (Responses, Messages).
/// Read from the body as sent, since passthrough forwards it unparsed.
fn asks_web_search(raw: &Value) -> bool {
    raw.get("web_search_options").is_some_and(|w| !w.is_null())
        || raw
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|ts| ts.iter().any(|t| t.get("type").and_then(Value::as_str).is_some_and(|ty| ty.starts_with("web_search"))))
}

fn for_mode(mut candidates: Vec<Arc<Deployment>>, model: &str, mode: Mode) -> Result<Vec<Arc<Deployment>>, GatewayError> {
    let other = candidates.first().map(|d| d.mode);
    candidates.retain(|d| d.mode == mode);
    match other {
        Some(m) if candidates.is_empty() => {
            Err(GatewayError::invalid(format!("'{model}' is a {} model; call {}", m.as_str(), m.endpoints())))
        }
        _ => Ok(candidates),
    }
}

/// Tries deployments of a model until one succeeds. A retryable failure cools that deployment
/// down and moves on; any other failure, or running out of deployments, fails the request.
pub(crate) async fn with_failover<T, F, Fut>(
    state: &AppState,
    candidates: &[Arc<Deployment>],
    model: &str,
    tracker: &mut Tracker,
    mut attempt: F,
) -> Result<T, GatewayError>
where
    F: FnMut(Arc<Deployment>) -> Fut,
    Fut: Future<Output = Result<T, GatewayError>>,
{
    let mut tried = HashSet::new();
    let mut last_err: Option<GatewayError> = None;
    for _ in 0..state.settings.get().max_attempts as usize {
        let d = match crate::router::pick(state, candidates, &tried, None).await {
            Ok(d) => d,
            Err(e) => {
                last_err.get_or_insert(e);
                break;
            }
        };
        tried.insert(d.id);
        let span = tracker.attempt(d.clone());
        let result = attempt(d.clone()).with_context(span.clone()).await;
        crate::telemetry::end_attempt(&span, result.as_ref().err());
        match result {
            Ok(r) => return Ok(r),
            Err(e) if e.is_retryable() => {
                tracing::warn!(deployment = %d.id, model = %model, error = %e, "attempt failed; trying another deployment");
                crate::router::failed(state, &d).await;
                last_err = Some(e);
            }
            Err(e) => {
                tracker.fail(&e);
                return Err(e);
            }
        }
    }
    let e = last_err.unwrap_or_else(|| GatewayError::unavailable("no deployment available"));
    tracker.fail(&e);
    Err(e)
}

/// Saves a translated Responses turn once its output is complete.
struct TurnSink {
    state: AppState,
    root: String,
    response_id: String,
    turn: Turn,
}

impl TurnSink {
    fn finish(mut self, output: Vec<Part>) {
        if !output.is_empty() {
            self.turn.messages.push(Message { role: Role::Assistant, content: output });
        }
        conversations::save_turn(&self.state, self.root, self.response_id, self.turn);
    }
}

/// Records which deployment holds an upstream-stored response.
struct PinSink {
    state: AppState,
    key_id: Uuid,
    deployment_id: Uuid,
}

impl PinSink {
    fn save(&self, response_id: String) {
        let pin = conversations::Pin { key_id: self.key_id, deployment_id: self.deployment_id };
        conversations::save_pin(&self.state, response_id, pin);
    }
}

/// The request to send for a continued gateway-stored conversation: this turn's input appended
/// to the replayed history. Like OpenAI, only this request's `instructions` apply.
fn with_history(turn: &ChatRequest, raw: &Value, h: &History, provider: ProviderKind) -> ChatRequest {
    let input_system = responses::input_system(turn, raw);
    let instructions = turn.system.len() - input_system.len();
    let mut req = turn.clone();
    req.system = turn.system[..instructions].to_vec();
    req.system.extend(h.system());
    req.system.extend(input_system);
    req.messages = h.messages_for(provider);
    req.messages.extend(turn.messages.iter().cloned());
    req.normalize();
    req
}

fn sse_response(body: Body) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(body)
        .expect("valid response")
}

async fn translate(state: &AppState, d: &Arc<Deployment>, ir: &ChatRequest, stream: bool) -> Result<Outcome, GatewayError> {
    let mut events = providers::stream_chat(state, d, ir).await?;
    if stream {
        // Wait for the first event so early upstream errors can still be retried elsewhere.
        match events.next().await {
            Some(Ok(first)) => Ok(Outcome::Stream { first, rest: events }),
            Some(Err(e)) => Err(e),
            None => Err(GatewayError::upstream_stream(ErrorKind::Upstream, "empty stream")),
        }
    } else {
        let mut agg = Aggregator::default();
        while let Some(ev) = events.next().await {
            agg.push(&ev?);
        }
        Ok(Outcome::Full(agg.finish()))
    }
}

/// A response that is not streamed, put together from its events.
async fn collect(first: StreamEvent, mut rest: EventStream) -> Result<Outcome, GatewayError> {
    let mut agg = Aggregator::default();
    agg.push(&first);
    while let Some(ev) = rest.next().await {
        agg.push(&ev?);
    }
    Ok(Outcome::Full(agg.finish()))
}

/// Renders a finished translated response as its API returns it without streaming.
type Render = Box<dyn Fn(&ChatResponse) -> Value + Send>;

/// A translated response as logged: what the client got, and under `x_gateway_not_sent` what
/// the provider ran itself and cited (web searches, their results, citations), which the
/// translation has no place for in the client's API, and in Chat Completions the gateway's MCP
/// calls (as Responses has them).
fn logged_response(mut v: Value, r: &ChatResponse, inbound: Inbound) -> Value {
    let mcp: Vec<Value> = if inbound == Inbound::Chat { r.mcp.iter().map(|(_, item)| responses::mcp_item(item)).collect() } else { vec![] };
    if (!r.server_tools.is_empty() || !r.annotations.is_empty() || !mcp.is_empty())
        && let Some(o) = v.as_object_mut()
    {
        let mut not_sent = json!({ "server_tools": r.server_tools, "annotations": r.annotations });
        if !mcp.is_empty() {
            not_sent["mcp"] = mcp.into();
        }
        o.insert("x_gateway_not_sent".into(), not_sent);
    }
    v
}

fn encode_stream(
    first: StreamEvent,
    rest: EventStream,
    enc: Box<dyn StreamEncoder>,
    tracker: Tracker,
    sink: Option<TurnSink>,
    render: Option<Render>,
) -> impl futures::Stream<Item = Result<Bytes, Infallible>> + Send {
    struct St {
        first: Option<StreamEvent>,
        rest: EventStream,
        enc: Box<dyn StreamEncoder>,
        tracker: Tracker,
        /// Collects the output for `sink` and the log; only present when either needs it.
        agg: Option<Aggregator>,
        sink: Option<TurnSink>,
        /// The request is logged: how to render its response.
        render: Option<Render>,
        started: bool,
        done: bool,
    }
    impl Drop for St {
        /// A stream cut short (an upstream error, a client that went away) logs what it had.
        fn drop(&mut self) {
            if let (Some(agg), Some(render)) = (self.agg.take(), self.render.as_ref())
                && let Some(c) = self.tracker.log()
            {
                c.reassembled(render(&agg.finish()));
                c.incomplete();
            }
        }
    }
    let agg = (sink.is_some() || render.is_some()).then(Aggregator::default);
    let st = St { first: Some(first), rest, enc, tracker, agg, sink, render, started: false, done: false };
    futures::stream::unfold(st, |mut st| async move {
        if !st.started {
            st.started = true;
            return Some((Ok(concat(st.enc.start())), st));
        }
        if st.done {
            return None;
        }
        let next = match st.first.take() {
            Some(ev) => Some(Ok(ev)),
            None => st.rest.next().await,
        };
        let chunk = match next {
            Some(Ok(ev)) => {
                if ev.is_content() {
                    st.tracker.first_token();
                }
                if let StreamEvent::TextDelta { text, .. }
                | StreamEvent::ReasoningDelta { text, .. }
                | StreamEvent::ToolArgsDelta { json: text, .. } = &ev
                {
                    st.tracker.streamed(text.len());
                    if st.render.is_some()
                        && let Some(c) = st.tracker.log()
                        && !c.hold(text.len())
                    {
                        st.render = None;
                    }
                }
                if let Some(agg) = st.agg.as_mut() {
                    agg.push(&ev);
                }
                if let StreamEvent::Finish { usage, .. } = &ev {
                    st.tracker.set_usage(*usage);
                    st.done = true;
                    // Only completed streams are stored; a failed one cannot be continued.
                    if let Some(agg) = st.agg.take() {
                        let r = agg.finish();
                        if let (Some(render), Some(c)) = (st.render.take(), st.tracker.log()) {
                            c.reassembled(render(&r));
                        }
                        if let Some(sink) = st.sink.take() {
                            sink.finish(r.history());
                        }
                    }
                }
                st.enc.event(&ev)
            }
            Some(Err(e)) => {
                st.tracker.fail(&e);
                st.done = true;
                st.enc.error(&e)
            }
            None => {
                st.tracker.stream_ended();
                return None;
            }
        };
        if st.done {
            st.tracker.stream_ended();
        }
        Some((Ok(concat(chunk)), st))
    })
}

fn concat(chunks: Vec<Bytes>) -> Bytes {
    if chunks.len() == 1 {
        return chunks.into_iter().next().unwrap();
    }
    let mut b = BytesMut::new();
    for c in chunks {
        b.extend_from_slice(&c);
    }
    b.freeze()
}

/// Headers clients may set that must reach a same-protocol upstream.
const FORWARDED_HEADERS: &[&str] = &["anthropic-beta", "anthropic-version", "openai-beta"];

async fn passthrough(
    state: &AppState,
    d: &Deployment,
    raw: &Value,
    stream: bool,
    inbound: Inbound,
    client_headers: &HeaderMap,
) -> Result<Outcome, GatewayError> {
    let mut body = raw.clone();
    body["model"] = d.upstream_model.clone().into();
    if inbound == Inbound::Messages && body.get("max_tokens").is_none() {
        let mt = d.config.default_max_tokens.unwrap_or(providers::anthropic::DEFAULT_MAX_TOKENS);
        body["max_tokens"] = mt.into();
    }
    // Without this, OpenAI-compatible servers report no token usage on streams, and spend and
    // TPM limits could not be accounted. Clients see one extra chunk with empty `choices`.
    if inbound == Inbound::Chat && stream {
        request_stream_usage(&mut body)?;
    }
    let op = match inbound {
        Inbound::Messages if stream => Op::Messages,
        Inbound::Messages => Op::MessagesOnce,
        Inbound::Responses => Op::Responses,
        Inbound::Chat => Op::ChatCompletions,
    };
    // Claude through Bedrock's InvokeModel: the SDK sends the body and the stream comes back as
    // Anthropic's SSE.
    if d.bedrock_invoke() {
        return Ok(match providers::bedrock::passthrough(state, d, &body, stream).await? {
            providers::bedrock::Passthrough::Stream(s) => Outcome::PassStream(s),
            providers::bedrock::Passthrough::Full(bytes) => {
                let usage =
                    serde_json::from_slice::<Value>(&bytes).ok().map(|v| providers::anthropic::usage(&v["usage"])).unwrap_or_default();
                Outcome::PassFull(bytes, usage)
            }
        });
    }
    providers::prepare_body(d, op, &mut body);
    let url = providers::url(state, d, op).await?;
    let mut headers = providers::headers(d)?;
    for name in FORWARDED_HEADERS {
        if let Some(v) = client_headers.get(*name) {
            headers.insert(*name, v.clone());
        }
    }
    let resp = providers::post(state, d, &url, headers, providers::json_bytes(&body)).await?;
    if stream {
        return Ok(Outcome::PassStream(providers::body_stream(resp)));
    }
    let bytes = resp.bytes().await.map_err(|e| GatewayError::from_reqwest(&e))?;
    let usage = serde_json::from_slice::<Value>(&bytes).ok().map(|v| passthrough_usage(inbound, &v)).unwrap_or_default();
    Ok(Outcome::PassFull(bytes, usage))
}

/// What a same-protocol upstream's finished response reports: its tokens and web searches.
fn passthrough_usage(inbound: Inbound, v: &Value) -> Usage {
    let Some(u) = v.get("usage") else { return Usage::default() };
    match inbound {
        Inbound::Messages => providers::anthropic::usage(u),
        Inbound::Responses => Usage { search_units: providers::openai_responses::web_searches(v), ..providers::openai_chat::usage(u) },
        Inbound::Chat => Usage {
            search_units: v.pointer("/choices/0/message").map_or(0, providers::openai_chat::web_searches),
            ..providers::openai_chat::usage(u)
        },
    }
}

/// Refuses request options that would have the provider bill outside what the gateway accounts.
/// Passthrough forwards the client's body as is, so these would otherwise reach the provider.
///
/// - `background` (Responses): the provider answers at once with no usage and keeps generating;
///   the gateway would record nothing, and cannot retrieve the result for the client either.
/// - `service_tier` `priority` or `scale` (OpenAI): billed above the deployment's prices, which
///   budgets and cost reporting use. `auto`, `default` and `flex` are billed at or below them.
/// - Server tools the provider bills per call, container or image, apart from tokens: OpenAI's
///   `code_interpreter`, `file_search` and `image_generation`, Anthropic's `code_execution_…`.
///   Usage reports only tokens, so budgets would not see that spend. (Web search is allowed per key.)
fn check_billing_options(raw: &Value, inbound: Inbound) -> Result<(), GatewayError> {
    let billed_tool = |ty: &str| matches!(ty, "code_interpreter" | "file_search" | "image_generation") || ty.starts_with("code_execution");
    if let Some(ty) = raw
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|t| t.get("type").and_then(Value::as_str))
        .find(|ty| billed_tool(ty))
    {
        return Err(GatewayError::invalid(format!(
            "the {ty} tool is not supported: the provider bills it apart from tokens, which the gateway cannot account for"
        )));
    }
    if inbound == Inbound::Responses && raw.get("background").is_some_and(|b| b != &Value::Bool(false) && !b.is_null()) {
        return Err(GatewayError::invalid(
            "background responses are not supported: the gateway cannot account for them or retrieve their result",
        ));
    }
    if matches!(inbound, Inbound::Chat | Inbound::Responses)
        && let Some(tier) = raw.get("service_tier").filter(|t| !t.is_null())
        && !matches!(tier.as_str(), Some("auto" | "default" | "flex"))
    {
        return Err(GatewayError::invalid(format!(
            "service_tier {tier} is not supported: deployment prices only cover auto, default and flex"
        )));
    }
    Ok(())
}

/// Sets `stream_options.include_usage` on a Chat Completions body (a JSON object). The client's
/// `stream_options` is matched by shape first: indexing into a non-object JSON value panics.
fn request_stream_usage(body: &mut Value) -> Result<(), GatewayError> {
    match body.get_mut("stream_options") {
        Some(Value::Object(o)) => {
            o.insert("include_usage".into(), true.into());
        }
        None | Some(Value::Null) => body["stream_options"] = json!({ "include_usage": true }),
        Some(_) => return Err(GatewayError::invalid("stream_options must be an object")),
    }
    Ok(())
}

/// Forward upstream bytes unchanged while extracting usage and timing for accounting.
fn sniff_stream(
    body: ByteStream,
    inbound: Inbound,
    tracker: Tracker,
    pin: Option<PinSink>,
) -> impl futures::Stream<Item = Result<Bytes, Infallible>> + Send {
    struct St {
        body: ByteStream,
        parser: SseParser,
        inbound: Inbound,
        tracker: Tracker,
        pin: Option<PinSink>,
        /// The request is logged: the response put back together as the stream passes.
        whole: Option<Reassembler>,
        done: bool,
    }
    impl Drop for St {
        /// A stream cut short (an upstream error, a client that went away) logs what it had.
        fn drop(&mut self) {
            if let (Some(whole), Some(c)) = (self.whole.take(), self.tracker.log()) {
                c.reassembled(whole.finish());
                c.incomplete();
            }
        }
    }
    let whole = tracker.logging().then(|| match inbound {
        Inbound::Chat => Reassembler::Chat(ChatStream::default()),
        Inbound::Messages => Reassembler::Messages(MessagesStream::default()),
        Inbound::Responses => Reassembler::Responses(ResponsesStream::default()),
    });
    let st = St { body, parser: SseParser::default(), inbound, tracker, pin, whole, done: false };
    futures::stream::unfold(st, |mut st| async move {
        if st.done {
            return None;
        }
        match st.body.next().await {
            Some(Ok(chunk)) => {
                if st.whole.is_some()
                    && let Some(c) = st.tracker.log()
                    && !c.hold(chunk.len())
                {
                    st.whole = None;
                }
                for ev in st.parser.feed(&chunk) {
                    let Ok(v) = serde_json::from_str::<Value>(&ev.data) else { continue };
                    inspect(&mut st.tracker, st.inbound, &v, st.pin.as_ref());
                    if let Some(w) = st.whole.as_mut() {
                        w.push(&v);
                    }
                }
                Some((Ok(chunk), st))
            }
            Some(Err(e)) => {
                st.done = true;
                let err = GatewayError::from_reqwest(&e);
                st.tracker.fail(&err);
                let bytes = match st.inbound {
                    Inbound::Messages => sse_event("error", &err.body(Dialect::Anthropic)),
                    Inbound::Chat => Bytes::from(format!(
                        "data: {}

data: [DONE]

",
                        err.body(Dialect::OpenAi)
                    )),
                    Inbound::Responses => {
                        sse_event("error", &json!({ "type": "error", "code": err.openai_type(), "message": err.message, "param": null }))
                    }
                };
                Some((Ok(bytes), st))
            }
            None => {
                st.tracker.stream_ended();
                if let (Some(whole), Some(c)) = (st.whole.take(), st.tracker.log()) {
                    c.reassembled(whole.finish());
                }
                None
            }
        }
    })
}

/// Bytes of generated content (text, tool arguments, reasoning) in one passthrough stream
/// event, for the output estimate when a stream ends without a usage report.
fn content_bytes(inbound: Inbound, ty: &str, v: &Value) -> usize {
    let len = |v: Option<&Value>| v.and_then(Value::as_str).map_or(0, str::len);
    match inbound {
        Inbound::Chat => {
            let Some(d) = v.pointer("/choices/0/delta") else { return 0 };
            let tools: usize = d
                .get("tool_calls")
                .and_then(Value::as_array)
                .map_or(0, |calls| calls.iter().map(|c| len(c.pointer("/function/arguments"))).sum());
            len(d.get("content")) + len(d.get("reasoning_content")) + len(d.get("reasoning")) + tools
        }
        Inbound::Messages if ty == "content_block_delta" => {
            let d = v.get("delta");
            ["text", "partial_json", "thinking"].iter().map(|k| len(d.and_then(|d| d.get(*k)))).sum()
        }
        Inbound::Responses if ty.ends_with(".delta") => len(v.get("delta")),
        Inbound::Messages | Inbound::Responses => 0,
    }
}

fn inspect(tracker: &mut Tracker, inbound: Inbound, v: &Value, pin: Option<&PinSink>) {
    let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
    tracker.streamed(content_bytes(inbound, ty, v));
    if inbound == Inbound::Chat {
        // Chat Completions chunks carry no event type.
        if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            tracker.set_usage(providers::openai_chat::usage(u));
        }
        let delta = v.pointer("/choices/0/delta");
        if let Some(d) = delta {
            tracker.searched(providers::openai_chat::web_searches(d));
        }
        if delta.is_some_and(|d| d.get("content").and_then(Value::as_str).is_some_and(|s| !s.is_empty()) || d.get("tool_calls").is_some()) {
            tracker.first_token();
        }
        if let Some(msg) = v.pointer("/error/message").and_then(Value::as_str) {
            tracker.fail_status(502, msg);
        }
        return;
    }
    match (inbound, ty) {
        (Inbound::Messages, "message_start") => {
            if let Some(u) = v.pointer("/message/usage") {
                tracker.merge_usage(&providers::anthropic::usage(u));
            }
        }
        (Inbound::Messages, "content_block_delta") => tracker.first_token(),
        (Inbound::Messages, "message_delta") => {
            if let Some(u) = v.get("usage") {
                tracker.merge_usage(&providers::anthropic::usage(u));
                tracker.usage_complete();
            }
        }
        (Inbound::Messages, "error") => {
            let t = v.pointer("/error/type").and_then(Value::as_str).unwrap_or("api_error");
            let status = match providers::anthropic::error_kind(t) {
                ErrorKind::RateLimit => 429,
                ErrorKind::Overloaded => 529,
                ErrorKind::InvalidRequest => 400,
                _ => 500,
            };
            tracker.fail_status(status, v.pointer("/error/message").and_then(Value::as_str).unwrap_or(t));
        }
        (Inbound::Responses, t) if t.ends_with(".delta") => tracker.first_token(),
        (Inbound::Responses, "response.completed" | "response.incomplete") => {
            if let Some(u) = v.pointer("/response/usage") {
                let searches = providers::openai_responses::web_searches(&v["response"]);
                tracker.set_usage(Usage { search_units: searches, ..providers::openai_chat::usage(u) });
            }
            if let (Some(pin), Some(id)) = (pin, v.pointer("/response/id").and_then(Value::as_str)) {
                pin.save(id.to_string());
            }
        }
        (Inbound::Responses, "response.failed" | "error") => {
            let msg = v
                .pointer("/response/error/message")
                .or_else(|| v.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("upstream stream error");
            tracker.fail_status(502, msg);
        }
        _ => {}
    }
}

/// `POST /v1/messages/count_tokens` — forwarded to an Anthropic-style deployment when one
/// serves the model, otherwise a rough estimate (≈4 bytes per token).
pub async fn count_tokens(
    State(state): State<AppState>,
    ClientKey(key): ClientKey,
    ClientLabels(labels): ClientLabels,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let result: Result<Response, GatewayError> = async {
        let raw: Value = serde_json::from_slice(&body).map_err(|e| GatewayError::invalid(format!("invalid JSON: {e}")))?;
        let model = raw.get("model").and_then(Value::as_str).ok_or_else(|| GatewayError::invalid("model is required"))?.to_string();
        // Limited and logged like an inference request: it is an upstream call on the key's behalf.
        let mut tracker = Tracker::new(&state, Some(&key), &labels, &model, "count_tokens", false);
        let result = count(&state, &key, &headers, raw, &model, body.len(), &mut tracker).await;
        if let Err(e) = &result {
            tracker.fail(e);
        }
        result
    }
    .await;
    result.unwrap_or_else(|e| e.into_response_for(Dialect::Anthropic))
}

async fn count(
    state: &AppState,
    key: &VirtualKey,
    headers: &HeaderMap,
    mut raw: Value,
    model: &str,
    body_len: usize,
    tracker: &mut Tracker,
) -> Result<Response, GatewayError> {
    if !key.allows_model(model) {
        return Err(GatewayError::forbidden(format!("this key is not allowed to use model '{model}'")));
    }
    let snap = state.registry.snapshot();
    let deps = snap.deployments.get(snap.resolve(model)).ok_or_else(|| GatewayError::not_found(format!("model '{model}' not found")))?;
    if !deps.iter().any(|d| key.allows_deployment(d)) {
        return Err(GatewayError::forbidden(format!("this key is not allowed to use model '{model}'")));
    }
    crate::auth::check_key_limits(state, key).await?;
    if let Some(d) = deps.iter().find(|d| d.style == ApiStyle::Anthropic && key.allows_deployment(d)) {
        tracker.attempt(d.clone());
        if d.bedrock_invoke() {
            return Ok(Json(providers::bedrock::count_tokens(state, d, &raw).await?).into_response());
        }
        raw["model"] = d.upstream_model.clone().into();
        providers::prepare_body(d, Op::CountTokens, &mut raw);
        let url = providers::url(state, d, Op::CountTokens).await?;
        let mut h = providers::headers(d)?;
        for name in FORWARDED_HEADERS {
            if let Some(v) = headers.get(*name) {
                h.insert(*name, v.clone());
            }
        }
        let resp = providers::post(state, d, &url, h, providers::json_bytes(&raw)).await?;
        let bytes = resp.bytes().await.map_err(|e| GatewayError::from_reqwest(&e))?;
        return Ok(([(header::CONTENT_TYPE, "application/json")], bytes).into_response());
    }
    let estimate = (body_len / 4).max(1);
    Ok(Json(json!({ "input_tokens": estimate })).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_serve_their_target_to_keys_that_name_the_alias() {
        use crate::model::{CredentialSettings, DeploymentConfig, ModelAlias, Pricing, Secrets};
        let luna = Deployment {
            id: Uuid::nil(),
            model_name: "luna".into(),
            mode: Mode::Chat,
            provider: ProviderKind::Openai,
            style: ApiStyle::Chat,
            upstream_model: "luna".into(),
            config: DeploymentConfig::default(),
            connection: CredentialSettings::default(),
            secrets: Secrets::default(),
            weight: 1,
            priority: 0,
            rpm_limit: None,
            tpm_limit: None,
            timeout: std::time::Duration::from_secs(1),
            pricing: Pricing::default(),
            tags: vec!["eu".into()],
            project_ids: vec![],
            catalog: None,
        };
        let mut snap = Snapshot::default();
        snap.deployments.insert("luna".into(), vec![Arc::new(luna)]);
        snap.aliases.insert("smart".into(), Arc::new(ModelAlias { model_name: "smart".into(), target: "luna".into() }));
        assert_eq!((snap.resolve("smart"), snap.resolve("luna"), snap.resolve("none")), ("luna", "luna", "none"));
        let key = |models: &[&str], tags: &[&str]| VirtualKey {
            id: Uuid::nil(),
            name: "k".into(),
            key_hash: vec![],
            key_prefix: "gw-".into(),
            scope: crate::model::KeyScope::Inference,
            project_id: None,
            project_active: true,
            allowed_models: models.iter().map(|m| m.to_string()).collect(),
            allowed_tags: tags.iter().map(|t| t.to_string()).collect(),
            rpm_limit: None,
            tpm_limit: None,
            max_budget_usd: None,
            budget_period: None,
            spend_usd: 0.0,
            spend_reset_at: None,
            enabled: true,
            expires_at: None,
            metadata: json!({}),
            guardrail_ids: vec![],
            web_search: true,
            prompt_logging: false,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        assert!(key_can_use(&snap, &key(&[], &[]), "smart"));
        assert!(key_can_use(&snap, &key(&["smart"], &["eu"]), "smart"));
        assert!(!key_can_use(&snap, &key(&["luna"], &[]), "smart"), "by the alias's own name");
        assert!(!key_can_use(&snap, &key(&[], &["us"]), "smart"), "with the target's tags");
    }

    #[test]
    fn web_search_is_refused_when_the_key_or_its_project_forbids_it() {
        for asks in [
            json!({ "web_search_options": {} }),
            json!({ "tools": [{ "type": "web_search" }] }),
            json!({ "tools": [{ "type": "function", "name": "f" }, { "type": "web_search_preview_2025_03_11" }] }),
            json!({ "tools": [{ "type": "web_search_20250305", "name": "web_search" }] }),
        ] {
            assert!(asks_web_search(&asks), "{asks}");
        }
        for not in [json!({}), json!({ "web_search_options": null }), json!({ "tools": [{ "type": "function", "name": "web_search" }] })] {
            assert!(!asks_web_search(&not), "{not}");
        }

        let project = |web_search| crate::model::ProjectRow {
            id: Uuid::from_u128(1),
            name: "p".into(),
            description: None,
            enabled: true,
            max_budget_usd: None,
            budget_period: None,
            spend_reset_at: None,
            guardrail_ids: vec![],
            web_search,
            prompt_logging: PromptLogging::Deny,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let key = |project_id, web_search| VirtualKey {
            id: Uuid::nil(),
            name: "k".into(),
            key_hash: vec![],
            key_prefix: "gw-".into(),
            scope: crate::model::KeyScope::Inference,
            project_id,
            project_active: true,
            allowed_models: vec![],
            allowed_tags: vec![],
            rpm_limit: None,
            tpm_limit: None,
            max_budget_usd: None,
            budget_period: None,
            spend_usd: 0.0,
            spend_reset_at: None,
            enabled: true,
            expires_at: None,
            metadata: json!({}),
            guardrail_ids: vec![],
            web_search,
            prompt_logging: false,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let mut snap = Snapshot::default();
        let p = Some(Uuid::from_u128(1));
        assert!(web_search_allowed(&snap, &key(None, true)));
        assert!(!web_search_allowed(&snap, &key(None, false)));
        snap.projects.insert(Uuid::from_u128(1), Arc::new(project(true)));
        assert!(web_search_allowed(&snap, &key(p, true)));
        assert!(!web_search_allowed(&snap, &key(p, false)));
        // A project that forbids it overrides the key.
        snap.projects.insert(Uuid::from_u128(1), Arc::new(project(false)));
        assert!(!web_search_allowed(&snap, &key(p, true)));
    }

    /// What the provider ran and cited on a translated path is never sent to the client, and is
    /// logged beside the response.
    #[test]
    fn provider_side_results_are_logged_not_sent() {
        let search = StreamEvent::ServerTool { block: json!({ "type": "web_search_call", "action": { "query": "q" } }) };
        let cite = StreamEvent::Annotation { annotation: json!({ "type": "url_citation", "url": "https://example.com" }) };
        let encoders: Vec<Box<dyn StreamEncoder>> = vec![
            Box::new(anthropic::AnthropicEncoder::new("msg_1".into(), "m".into())),
            Box::new(responses::ResponsesEncoder::new("resp_1".into(), "m".into())),
            Box::new(chat::ChatEncoder::new("chatcmpl-1".into(), "m".into(), true)),
        ];
        for mut enc in encoders {
            enc.start();
            assert!(enc.event(&search).is_empty() && enc.event(&cite).is_empty());
        }
        let r = ChatResponse {
            content: vec![Part::Text { text: "Hi".into(), cache: false }],
            server_tools: vec![json!({ "type": "web_search_call" })],
            annotations: vec![json!({ "type": "url_citation" })],
            ..Default::default()
        };
        let client = chat::render_response(&r, "chatcmpl-1", "m");
        assert!(client.get("x_gateway_not_sent").is_none());
        let logged = logged_response(client, &r, Inbound::Chat);
        assert_eq!(logged["x_gateway_not_sent"]["server_tools"], json!([{ "type": "web_search_call" }]));
        assert_eq!(logged["choices"][0]["message"]["content"], "Hi");
        // Without any, the logged response is what the client got.
        let plain = ChatResponse { content: r.content.clone(), ..Default::default() };
        assert!(logged_response(chat::render_response(&plain, "c", "m"), &plain, Inbound::Chat).get("x_gateway_not_sent").is_none());
    }

    /// The gateway's MCP items: Responses and Messages return them in order among the answer's
    /// parts, streamed or not; Chat Completions only logs them.
    #[test]
    fn gateway_mcp_calls_are_returned_where_the_api_has_them() {
        use crate::ir::{BlockKind, McpItem, StopReason};
        let tools = McpItem::Tools { server: "docs".into(), tools: vec![json!({ "name": "read", "inputSchema": { "type": "object" } })] };
        let call = McpItem::Call {
            server: "docs".into(),
            tool: "read".into(),
            arguments: json!({ "path": "a" }),
            result: Ok(json!({ "content": [{ "type": "text", "text": "A" }] })),
        };
        let failed =
            McpItem::Call { server: "docs".into(), tool: "read".into(), arguments: json!({}), result: Err("budget reached".into()) };
        // Tools, a call and a failed call, then the answer's text in block 3.
        let events = [
            StreamEvent::Mcp { index: 0, item: tools },
            StreamEvent::Mcp { index: 1, item: call },
            StreamEvent::Mcp { index: 2, item: failed },
            StreamEvent::BlockStart { index: 3, kind: BlockKind::Text },
            StreamEvent::TextDelta { index: 3, text: "Done".into() },
            StreamEvent::BlockStop { index: 3 },
            StreamEvent::Finish { stop_reason: StopReason::EndTurn, usage: Usage::default() },
        ];
        let mut agg = Aggregator::default();
        events.iter().for_each(|e| agg.push(e));
        let r = agg.finish();
        assert_eq!(r.mcp.iter().map(|(at, _)| *at).collect::<Vec<_>>(), [0, 0, 0]);

        let v = responses::render_response(&r, "resp_1", "m");
        let types: Vec<&str> = v["output"].as_array().unwrap().iter().map(|o| o["type"].as_str().unwrap()).collect();
        assert_eq!(types, ["mcp_list_tools", "mcp_call", "mcp_call", "message"]);
        assert_eq!(v["output"][0]["tools"][0]["input_schema"], json!({ "type": "object" }));
        assert_eq!((v["output"][1]["output"].as_str(), v["output"][1]["arguments"].as_str()), (Some("A"), Some(r#"{"path":"a"}"#)));
        assert_eq!((v["output"][2]["status"].as_str(), v["output"][2]["error"].as_str()), (Some("failed"), Some("budget reached")));

        let v = anthropic::render_response(&r, "msg_1", "m");
        let types: Vec<&str> = v["content"].as_array().unwrap().iter().map(|o| o["type"].as_str().unwrap()).collect();
        assert_eq!(types, ["mcp_tool_use", "mcp_tool_result", "mcp_tool_use", "mcp_tool_result", "text"]);
        assert_eq!(v["content"][1]["tool_use_id"], v["content"][0]["id"]);
        assert_eq!(v["content"][3]["is_error"], true);

        // Streamed: the same items, and Messages' text block after the two calls' four blocks.
        let sse = |mut enc: Box<dyn StreamEncoder>| {
            let mut out = enc.start();
            events.iter().for_each(|e| out.extend(enc.event(e)));
            out.iter().map(|b| String::from_utf8_lossy(b).into_owned()).collect::<String>()
        };
        let s = sse(Box::new(responses::ResponsesEncoder::new("resp_1".into(), "m".into())));
        assert!(
            s.contains("response.mcp_list_tools.completed")
                && s.contains("response.mcp_call.completed")
                && s.contains("response.mcp_call.failed")
        );
        let s = sse(Box::new(anthropic::AnthropicEncoder::new("msg_1".into(), "m".into())));
        assert!(s.contains(r#""index":4,"type":"text_delta""#) || s.contains(r#""index":4"#));
        assert!(s.contains(r#""type":"mcp_tool_use""#));
        let s = sse(Box::new(chat::ChatEncoder::new("chatcmpl-1".into(), "m".into(), true)));
        assert!(!s.contains("mcp"));

        let logged = logged_response(chat::render_response(&r, "c", "m"), &r, Inbound::Chat);
        assert_eq!(logged["x_gateway_not_sent"]["mcp"].as_array().map(Vec::len), Some(3));
        assert!(logged_response(responses::render_response(&r, "r", "m"), &r, Inbound::Responses).get("x_gateway_not_sent").is_none());
        // A later turn's history has the calls as text.
        let history = crate::ir::parts_text(&r.history());
        assert!(history.contains("read of docs") && history.contains("budget reached") && history.ends_with("Done"));
    }

    #[test]
    fn prompt_logging_follows_the_project_then_the_key() {
        use crate::settings::Settings;
        let project = |prompt_logging| crate::model::ProjectRow {
            id: Uuid::from_u128(1),
            name: "p".into(),
            description: None,
            enabled: true,
            max_budget_usd: None,
            budget_period: None,
            spend_reset_at: None,
            guardrail_ids: vec![],
            web_search: true,
            prompt_logging,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let key = |project_id, prompt_logging| VirtualKey {
            id: Uuid::nil(),
            name: "k".into(),
            key_hash: vec![],
            key_prefix: "gw-".into(),
            scope: crate::model::KeyScope::Inference,
            project_id,
            project_active: true,
            allowed_models: vec![],
            allowed_tags: vec![],
            rpm_limit: None,
            tpm_limit: None,
            max_budget_usd: None,
            budget_period: None,
            spend_usd: 0.0,
            spend_reset_at: None,
            enabled: true,
            expires_at: None,
            metadata: json!({}),
            guardrail_ids: vec![],
            web_search: true,
            prompt_logging,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let deny = Settings::default();
        let allow = Settings { prompt_logging: PromptLogging::Allow, ..Settings::default() };
        let mut snap = Snapshot::default();
        let p = Some(Uuid::from_u128(1));
        // Outside projects, the gateway's policy: denied by default.
        assert!(!prompt_logged(&snap, &deny, &key(None, true)));
        assert!(prompt_logged(&snap, &allow, &key(None, true)));
        assert!(!prompt_logged(&snap, &allow, &key(None, false)));
        // A project the registry does not know logs nothing, whatever the gateway allows.
        assert!(!prompt_logged(&snap, &allow, &key(p, true)));
        for (policy, opted_in, opted_out) in
            [(PromptLogging::Deny, false, false), (PromptLogging::Allow, true, false), (PromptLogging::Force, true, true)]
        {
            snap.projects.insert(Uuid::from_u128(1), Arc::new(project(policy)));
            // The project decides, not the gateway's policy for keys outside projects.
            for settings in [&deny, &allow] {
                assert_eq!(prompt_logged(&snap, settings, &key(p, true)), opted_in, "{policy:?}, key opted in");
                assert_eq!(prompt_logged(&snap, settings, &key(p, false)), opted_out, "{policy:?}, key not opted in");
            }
        }
    }

    #[test]
    fn stream_usage_handles_any_client_shape() {
        for (sent, want) in [
            (json!({}), json!({ "include_usage": true })),
            (json!({ "stream_options": null }), json!({ "include_usage": true })),
            (json!({ "stream_options": { "x": 1 } }), json!({ "x": 1, "include_usage": true })),
        ] {
            let mut body = sent;
            request_stream_usage(&mut body).unwrap();
            assert_eq!(body["stream_options"], want);
        }
        for bad in [json!("x"), json!(1), json!(true), json!([1])] {
            let mut body = json!({ "stream_options": bad });
            assert_eq!(request_stream_usage(&mut body).unwrap_err().status, StatusCode::BAD_REQUEST);
        }
    }
}

#[cfg(test)]
mod accounting_tests {
    use std::{
        sync::atomic::{AtomicBool, Ordering},
        time::Duration,
    };

    use super::*;

    #[test]
    fn passthrough_content_is_counted_for_estimates() {
        let chat =
            json!({ "choices": [{ "delta": { "content": "hello", "tool_calls": [{ "function": { "arguments": "{\"a\":1}" } }] } }] });
        assert_eq!(content_bytes(Inbound::Chat, "", &chat), 5 + 7);
        assert_eq!(content_bytes(Inbound::Chat, "", &json!({ "choices": [] })), 0);
        let text = json!({ "type": "content_block_delta", "delta": { "type": "text_delta", "text": "abc" } });
        assert_eq!(content_bytes(Inbound::Messages, "content_block_delta", &text), 3);
        let thinking = json!({ "type": "content_block_delta", "delta": { "type": "thinking_delta", "thinking": "hmm" } });
        assert_eq!(content_bytes(Inbound::Messages, "content_block_delta", &thinking), 3);
        assert_eq!(content_bytes(Inbound::Messages, "message_delta", &json!({ "delta": { "text": "x" } })), 0);
        let delta = json!({ "type": "response.output_text.delta", "delta": "four" });
        assert_eq!(content_bytes(Inbound::Responses, "response.output_text.delta", &delta), 4);
    }

    /// A request keeps running when whoever awaited it goes away, as hyper does with the handler
    /// of a closed connection.
    #[tokio::test]
    async fn requests_run_to_completion_without_their_client() {
        static FINISHED: AtomicBool = AtomicBool::new(false);
        let request = to_completion(async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            FINISHED.store(true, Ordering::SeqCst);
            Ok(())
        });
        // The client gives up long before the request is done.
        assert!(tokio::time::timeout(Duration::from_millis(5), request).await.is_err());
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(FINISHED.load(Ordering::SeqCst));
    }
}

#[cfg(test)]
mod billing_option_tests {
    use super::*;

    #[test]
    fn options_billed_outside_the_gateway_are_refused() {
        let ok = |v: Value, i| check_billing_options(&v, i).is_ok();
        assert!(ok(json!({ "model": "m" }), Inbound::Responses));
        assert!(ok(json!({ "background": false }), Inbound::Responses));
        assert!(ok(json!({ "background": null }), Inbound::Responses));
        assert!(!ok(json!({ "background": true }), Inbound::Responses));
        assert!(!ok(json!({ "background": "true" }), Inbound::Responses));
        for tier in ["auto", "default", "flex"] {
            assert!(ok(json!({ "service_tier": tier }), Inbound::Chat), "{tier}");
            assert!(ok(json!({ "service_tier": tier }), Inbound::Responses), "{tier}");
        }
        for tier in [json!("priority"), json!("scale"), json!(1)] {
            assert!(!ok(json!({ "service_tier": tier }), Inbound::Chat), "{tier}");
            assert!(!ok(json!({ "service_tier": tier }), Inbound::Responses), "{tier}");
        }
        // Anthropic's own tiers (auto, standard_only) bill within the account's pricing.
        assert!(ok(json!({ "service_tier": "standard_only" }), Inbound::Messages));
        assert!(ok(json!({ "background": true }), Inbound::Chat));
    }

    #[test]
    fn server_tools_billed_outside_the_gateway_are_refused() {
        let ok = |v: Value, i| check_billing_options(&v, i).is_ok();
        let tools = |ty: &str| json!({ "tools": [{ "type": "function", "name": "f" }, { "type": ty }] });
        for ty in ["function", "custom", "web_search", "web_search_20250305", "mcp"] {
            assert!(ok(tools(ty), Inbound::Responses), "{ty}");
        }
        for ty in ["code_interpreter", "file_search", "image_generation"] {
            assert!(!ok(tools(ty), Inbound::Responses), "{ty}");
            assert!(!ok(tools(ty), Inbound::Chat), "{ty}");
        }
        assert!(!ok(tools("code_execution_20250825"), Inbound::Messages));
        assert!(ok(json!({ "tools": "not a list" }), Inbound::Messages));
    }
}
