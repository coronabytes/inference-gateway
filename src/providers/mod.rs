//! Upstream transports and wire-format codecs.

pub mod anthropic;
pub mod bedrock;
pub mod openai_chat;
pub mod openai_responses;

use std::{collections::VecDeque, pin::Pin};

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::{Value, json};

use crate::{
    entra,
    error::GatewayError,
    google,
    ir::{ChatRequest, StreamEvent},
    model::{ApiStyle, Deployment, ProviderKind},
    sse::SseParser,
    state::AppState,
};

pub const ANTHROPIC_VERSION: &str = "2023-06-01";

pub type EventResult = Result<StreamEvent, GatewayError>;
pub type EventStream = Pin<Box<dyn Stream<Item = EventResult> + Send>>;
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

/// Turns upstream bytes into normalized events.
pub trait Decoder: Send + 'static {
    fn feed(&mut self, chunk: &[u8], out: &mut Vec<EventResult>);
    fn finish(&mut self, _out: &mut Vec<EventResult>) {}
}

/// Handler for JSON-over-SSE upstreams.
pub trait SseHandler: Send + 'static {
    fn on_event(&mut self, event: Option<&str>, data: &str, out: &mut Vec<EventResult>);
    fn on_end(&mut self, _out: &mut Vec<EventResult>) {}
}

pub struct Sse<H> {
    parser: SseParser,
    handler: H,
}

impl<H: SseHandler> Sse<H> {
    pub fn new(handler: H) -> Self {
        Self { parser: SseParser::default(), handler }
    }
}

impl<H: SseHandler> Decoder for Sse<H> {
    fn feed(&mut self, chunk: &[u8], out: &mut Vec<EventResult>) {
        for ev in self.parser.feed(chunk) {
            self.handler.on_event(ev.event.as_deref(), &ev.data, out);
        }
    }

    fn finish(&mut self, out: &mut Vec<EventResult>) {
        for ev in self.parser.finish() {
            self.handler.on_event(ev.event.as_deref(), &ev.data, out);
        }
        self.handler.on_end(out);
    }
}

/// Drive a decoder over an upstream body. The stream ends after the first `Finish` or error;
/// an upstream that closes without finishing yields an error.
pub fn decode_stream<D: Decoder>(body: ByteStream, decoder: D) -> EventStream {
    struct St<D> {
        body: ByteStream,
        dec: D,
        pending: VecDeque<EventResult>,
        eof: bool,
        done: bool,
    }
    let st = St { body, dec: decoder, pending: VecDeque::new(), eof: false, done: false };
    futures::stream::unfold(st, |mut st| async move {
        loop {
            if st.done {
                return None;
            }
            if let Some(item) = st.pending.pop_front() {
                if matches!(item, Ok(StreamEvent::Finish { .. }) | Err(_)) {
                    st.done = true;
                }
                return Some((item, st));
            }
            if st.eof {
                return None;
            }
            let mut out = Vec::new();
            match st.body.next().await {
                Some(Ok(chunk)) => st.dec.feed(&chunk, &mut out),
                Some(Err(e)) => out.push(Err(GatewayError::from_reqwest(&e))),
                None => {
                    st.eof = true;
                    st.dec.finish(&mut out);
                    if !out.iter().any(|e| matches!(e, Ok(StreamEvent::Finish { .. }) | Err(_))) {
                        out.push(Err(GatewayError::upstream_stream(crate::error::ErrorKind::Upstream, "stream ended before completion")));
                    }
                }
            }
            st.pending.extend(out);
        }
    })
    .boxed()
}

/// Which upstream operation to call.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Op {
    ChatCompletions,
    Responses,
    /// Anthropic Messages, streamed.
    Messages,
    /// Anthropic Messages without streaming. Only Vertex has a separate route for it.
    MessagesOnce,
    CountTokens,
    /// System One decisions (decision deployments).
    SystemOne,
    /// OpenAI embeddings (embedding deployments).
    Embeddings,
    /// Rerank (rerank deployments).
    Rerank,
}

/// RFC 3986 unreserved characters stay literal; everything else is percent-encoded.
const PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.').remove(b'~');
/// Vertex model IDs carry a version after `@` (`claude-sonnet-4-5@20250929`), which is valid in a path.
const VERTEX_MODEL: &AsciiSet = &PATH_SEGMENT.remove(b'@');

/// Body of `anthropic_version` for Claude on Vertex AI.
const VERTEX_ANTHROPIC_VERSION: &str = "vertex-2023-10-16";
/// Gemini 3 rejects function calls in the history that carry no thought signature. Translated
/// requests have lost it, so they send Google's documented placeholder instead.
const SKIP_THOUGHT_SIGNATURE: &str = "skip_thought_signature_validator";

/// Mistral's API base: global, or pinned to its EU or US endpoint.
pub(crate) fn mistral_base(region: Option<&str>) -> String {
    match region {
        Some(r @ ("eu" | "us")) => format!("https://api.{r}.mistral.ai/v1"),
        _ => "https://api.mistral.ai/v1".into(),
    }
}

/// API base URL including the version segment (e.g. `.../v1`).
async fn api_base(state: &AppState, d: &Deployment) -> Result<String, GatewayError> {
    let custom = d.connection.base_url.as_deref().map(|s| s.trim_end_matches('/').to_string());
    Ok(match d.provider {
        ProviderKind::Openai => custom.unwrap_or_else(|| "https://api.openai.com/v1".into()),
        ProviderKind::Anthropic => custom.unwrap_or_else(|| "https://api.anthropic.com/v1".into()),
        ProviderKind::Azure => {
            let base = custom.ok_or_else(|| GatewayError::internal("azure credential has no endpoint"))?;
            let has_path = reqwest::Url::parse(&base).map(|u| u.path() != "/" && !u.path().is_empty()).unwrap_or(false);
            if has_path {
                base
            } else if d.style == ApiStyle::Anthropic {
                format!("{base}/anthropic/v1")
            } else {
                format!("{base}/openai/v1")
            }
        }
        ProviderKind::Bedrock => match (custom, d.style) {
            (Some(c), _) => c,
            (None, ApiStyle::Anthropic) => {
                format!("https://bedrock-mantle.{}.api.aws/anthropic/v1", state.aws.region(d).await?)
            }
            (None, _) => format!("https://bedrock-runtime.{}.amazonaws.com", state.aws.region(d).await?),
        },
        ProviderKind::Typesafe => custom.unwrap_or_else(|| "https://api.typesafe.ai".into()),
        ProviderKind::Mistral => custom.unwrap_or_else(|| mistral_base(d.connection.region.as_deref())),
        ProviderKind::Gemini => custom.unwrap_or_else(|| "https://generativelanguage.googleapis.com/v1beta/openai".into()),
        ProviderKind::Vertex => {
            let project = google::project(&d.connection, &d.secrets)?;
            let location = d.connection.region.as_deref().unwrap_or("global");
            let host = custom.unwrap_or_else(|| google::vertex_host(location));
            let seg = |s: &str| utf8_percent_encode(s, PATH_SEGMENT).to_string();
            format!("{host}/v1/projects/{}/locations/{}", seg(&project), seg(location))
        }
    })
}

/// System One endpoint. TypeSafe serves it under its API base; Microsoft Foundry under the
/// resource host, so a project-scoped or OpenAI-style endpoint is trimmed back to the host.
fn systemone_url(d: &Deployment) -> Result<String, GatewayError> {
    let custom = d.connection.base_url.as_deref().map(|s| s.trim_end_matches('/'));
    match d.provider {
        ProviderKind::Typesafe => {
            let base = custom.unwrap_or("https://api.typesafe.ai");
            Ok(format!("{}/v1/systemone", base.strip_suffix("/v1").unwrap_or(base)))
        }
        ProviderKind::Azure => {
            let base = custom.ok_or_else(|| GatewayError::internal("azure credential has no endpoint"))?;
            let mut url = reqwest::Url::parse(base).map_err(|_| GatewayError::internal("azure endpoint is not a URL"))?;
            let path = foundry_resource_path(url.path()).to_string();
            url.set_path(&format!("{path}/providers/microsoft/v1/systemone"));
            url.set_query(None);
            url.set_fragment(None);
            Ok(url.into())
        }
        p => Err(GatewayError::internal(format!("provider {} has no System One API", p.as_str()))),
    }
}

/// Drop a trailing `/api/projects/<name>` and API route (`/openai/v1`, `/openai`, `/models`, `/v1`).
pub(crate) fn foundry_resource_path(path: &str) -> &str {
    let mut p = path.trim_end_matches('/');
    for suffix in ["/openai/v1", "/openai", "/models", "/v1"] {
        if let Some(rest) = p.strip_suffix(suffix) {
            p = rest;
            break;
        }
    }
    if let Some((head, project)) = p.rsplit_once('/')
        && !project.is_empty()
        && let Some(rest) = head.strip_suffix("/api/projects")
    {
        p = rest;
    }
    p
}

/// Rerank endpoint. OpenAI-compatible servers (vLLM, Cohere, Jina, …) serve Cohere's API at
/// `{base}/rerank`. Azure AI Foundry serves Cohere's v2 API: a serverless endpoint
/// (`*.models.ai.azure.com`) at `/v2/rerank`, a Foundry resource at `/providers/cohere/v2/rerank`;
/// an endpoint that already ends in `/rerank` is used as is. Bedrock reranks through its Agent
/// Runtime's Rerank API, so a runtime endpoint override is moved to the Agent Runtime host.
async fn rerank_url(state: &AppState, d: &Deployment) -> Result<String, GatewayError> {
    let custom = d.connection.base_url.as_deref().map(|s| s.trim_end_matches('/'));
    match d.provider {
        ProviderKind::Openai => Ok(format!("{}/rerank", custom.unwrap_or("https://api.openai.com/v1"))),
        ProviderKind::Azure => {
            let base = custom.ok_or_else(|| GatewayError::internal("azure credential has no endpoint"))?;
            let mut url = reqwest::Url::parse(base).map_err(|_| GatewayError::internal("azure endpoint is not a URL"))?;
            url.set_query(None);
            url.set_fragment(None);
            if url.path().trim_end_matches('/').ends_with("/rerank") {
                return Ok(url.into());
            }
            let serverless = url.host_str().is_some_and(|h| h.ends_with(".models.ai.azure.com"));
            let path = if serverless {
                "/v2/rerank".to_string()
            } else {
                format!("{}/providers/cohere/v2/rerank", foundry_resource_path(url.path()))
            };
            url.set_path(&path);
            Ok(url.into())
        }
        ProviderKind::Bedrock => Ok(match custom {
            Some(c) => format!("{}/rerank", c.replacen("bedrock-runtime", "bedrock-agent-runtime", 1)),
            None => format!("https://bedrock-agent-runtime.{}.amazonaws.com/rerank", state.aws.region(d).await?),
        }),
        p => Err(GatewayError::internal(format!("provider {} has no rerank API", p.as_str()))),
    }
}

pub async fn url(state: &AppState, d: &Deployment, op: Op) -> Result<String, GatewayError> {
    if op == Op::SystemOne {
        return systemone_url(d);
    }
    if op == Op::Rerank {
        return rerank_url(state, d).await;
    }
    let base = api_base(state, d).await?;
    let vertex = d.provider == ProviderKind::Vertex;
    let vertex_model = || utf8_percent_encode(&d.upstream_model, VERTEX_MODEL).to_string();
    let mut url = match op {
        // Gemini and Model Garden models through Vertex's OpenAI-compatible endpoint.
        Op::ChatCompletions if vertex => format!("{base}/endpoints/openapi/chat/completions"),
        Op::Messages if vertex => format!("{base}/publishers/anthropic/models/{}:streamRawPredict", vertex_model()),
        Op::MessagesOnce if vertex => format!("{base}/publishers/anthropic/models/{}:rawPredict", vertex_model()),
        Op::CountTokens if vertex => format!("{base}/publishers/anthropic/models/count-tokens:rawPredict"),
        Op::Embeddings if vertex => format!("{base}/publishers/google/models/{}:predict", vertex_model()),
        Op::ChatCompletions => format!("{base}/chat/completions"),
        Op::Responses => format!("{base}/responses"),
        Op::Messages | Op::MessagesOnce => format!("{base}/messages"),
        Op::CountTokens => format!("{base}/messages/count_tokens"),
        Op::Embeddings => format!("{base}/embeddings"),
        Op::SystemOne | Op::Rerank => unreachable!("handled above"),
    };
    if d.provider == ProviderKind::Azure
        && let Some(v) = &d.connection.api_version
    {
        url.push_str("?api-version=");
        url.push_str(&utf8_percent_encode(v, PATH_SEGMENT).to_string());
    }
    Ok(url)
}

/// Static headers (auth except SigV4, versions, configured extras).
pub fn headers(d: &Deployment) -> Result<HeaderMap, GatewayError> {
    let mut h = HeaderMap::new();
    h.insert("content-type", HeaderValue::from_static("application/json"));
    let key = d.secrets.api_key.as_deref();
    let bad_key = || GatewayError::internal("provider API key contains invalid header characters");
    match d.provider {
        ProviderKind::Openai => {
            if let Some(k) = key {
                // Servers that take the key in another header get it there, as is.
                let (name, value) = match &d.connection.auth_header {
                    Some(name) => (
                        HeaderName::try_from(name.as_str()).map_err(|_| GatewayError::internal("invalid auth_header"))?,
                        HeaderValue::from_str(k),
                    ),
                    None => (axum::http::header::AUTHORIZATION, HeaderValue::from_str(&format!("Bearer {k}"))),
                };
                let mut value = value.map_err(|_| bad_key())?;
                value.set_sensitive(true);
                h.insert(name, value);
            }
        }
        // Entra ID: an access token is added when the request is sent (see `post`).
        ProviderKind::Azure if key.is_none() && entra::source(&d.connection, &d.secrets).is_some() => {}
        // Foundry's System One route takes the resource key as a bearer token.
        ProviderKind::Azure if d.style == ApiStyle::Systemone => {
            let k = key.ok_or_else(|| GatewayError::internal("azure deployment has no api_key"))?;
            h.insert("authorization", HeaderValue::from_str(&format!("Bearer {k}")).map_err(|_| bad_key())?);
        }
        // Serverless endpoints take the key as a bearer token, Foundry resources as `api-key`.
        ProviderKind::Azure if d.style == ApiStyle::Rerank => {
            let k = key.ok_or_else(|| GatewayError::internal("azure deployment has no api_key"))?;
            let mut bearer = HeaderValue::from_str(&format!("Bearer {k}")).map_err(|_| bad_key())?;
            bearer.set_sensitive(true);
            h.insert("authorization", bearer);
            h.insert("api-key", HeaderValue::from_str(k).map_err(|_| bad_key())?);
        }
        ProviderKind::Azure => {
            let k = key.ok_or_else(|| GatewayError::internal("azure deployment has no api_key"))?;
            let v = HeaderValue::from_str(k).map_err(|_| bad_key())?;
            h.insert("api-key", v.clone());
            if d.style == ApiStyle::Anthropic {
                h.insert("x-api-key", v);
            }
        }
        ProviderKind::Typesafe | ProviderKind::Mistral | ProviderKind::Gemini => {
            let k = key.ok_or_else(|| GatewayError::internal(format!("{} deployment has no api_key", d.provider.as_str())))?;
            h.insert("authorization", HeaderValue::from_str(&format!("Bearer {k}")).map_err(|_| bad_key())?);
        }
        // An access token is added when the request is sent (see `post`).
        ProviderKind::Vertex => {}
        ProviderKind::Anthropic => {
            let k = key.ok_or_else(|| GatewayError::internal("anthropic deployment has no api_key"))?;
            h.insert("x-api-key", HeaderValue::from_str(k).map_err(|_| bad_key())?);
        }
        ProviderKind::Bedrock => {
            if let Some(k) = key {
                h.insert("authorization", HeaderValue::from_str(&format!("Bearer {k}")).map_err(|_| bad_key())?);
            }
        }
    }
    // Vertex takes the version in the body instead.
    if d.style == ApiStyle::Anthropic && d.provider != ProviderKind::Vertex {
        h.insert("anthropic-version", HeaderValue::from_static(ANTHROPIC_VERSION));
    }
    for (k, v) in &d.config.headers {
        let name = HeaderName::try_from(k.as_str()).map_err(|_| GatewayError::internal("invalid configured header"))?;
        let value = HeaderValue::from_str(v).map_err(|_| GatewayError::internal("invalid configured header"))?;
        h.insert(name, value);
    }
    Ok(h)
}

/// POST a JSON body upstream; non-2xx responses become classified errors.
pub async fn post(
    state: &AppState,
    d: &Deployment,
    url: &str,
    mut headers: HeaderMap,
    body: Vec<u8>,
) -> Result<reqwest::Response, GatewayError> {
    // Hostnames are checked by the HTTP client's resolver; IP literals never reach it.
    if let Err(why) = state.cfg.upstream_policy.check_url(url) {
        tracing::warn!(deployment = %d.id, %why, "blocked upstream request");
        return Err(GatewayError::internal(format!("upstream address not allowed: {why}")));
    }
    if d.provider == ProviderKind::Bedrock && d.secrets.api_key.is_none() {
        state.aws.sign(d, url, &mut headers, &body).await?;
    }
    if d.provider == ProviderKind::Vertex {
        let token = state.google.access_token(&d.connection, &d.secrets).await?;
        let mut v = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| GatewayError::internal("invalid Google access token"))?;
        v.set_sensitive(true);
        headers.insert("authorization", v);
    }
    if d.provider == ProviderKind::Azure
        && d.secrets.api_key.is_none()
        && let Some(source) = entra::source(&d.connection, &d.secrets)
    {
        let scope = entra::scope_for(d.connection.base_url.as_deref());
        let token = state.entra.access_token(&source, scope).await?;
        let mut v =
            HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| GatewayError::internal("invalid Entra ID access token"))?;
        v.set_sensitive(true);
        headers.insert("authorization", v);
    }
    let resp =
        state.http.post(url).headers(headers).timeout(d.timeout).body(body).send().await.map_err(|e| GatewayError::from_reqwest(&e))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        tracing::warn!(deployment = %d.id, status = status.as_u16(), body = %crate::util::truncate(&text, 500), "upstream error");
        return Err(GatewayError::from_upstream(status, &text));
    }
    Ok(resp)
}

pub fn body_stream(resp: reqwest::Response) -> ByteStream {
    resp.bytes_stream().boxed()
}

/// Send a translated request and return normalized events. Upstreams are always called in
/// streaming mode; non-streaming clients get the aggregated result.
pub async fn stream_chat(state: &AppState, d: &Deployment, req: &ChatRequest) -> Result<EventStream, GatewayError> {
    match d.style {
        ApiStyle::Chat => openai_chat::send(state, d, req).await,
        ApiStyle::Responses => openai_responses::send(state, d, req).await,
        ApiStyle::Anthropic if d.bedrock_invoke() => bedrock::send_invoke(state, d, req).await,
        ApiStyle::Anthropic => anthropic::send(state, d, req).await,
        ApiStyle::Converse => bedrock::send(state, d, req).await,
        ApiStyle::Systemone | ApiStyle::Embeddings | ApiStyle::Rerank => {
            Err(GatewayError::invalid(format!("'{}' is a {} model; call {}", d.model_name, d.mode.as_str(), d.mode.endpoints())))
        }
    }
}

/// Provider quirks applied to a request body just before it is sent, whether it was translated
/// or passed through.
pub fn prepare_body(d: &Deployment, op: Op, body: &mut Value) {
    let Some(obj) = body.as_object_mut() else { return };
    match (d.provider, op) {
        // Claude on Vertex: the model is in the URL, the version in the body.
        (ProviderKind::Vertex, Op::Messages | Op::MessagesOnce) => {
            obj.remove("model");
            obj.insert("anthropic_version".into(), VERTEX_ANTHROPIC_VERSION.into());
        }
        (ProviderKind::Vertex, Op::CountTokens) => {
            obj.insert("anthropic_version".into(), VERTEX_ANTHROPIC_VERSION.into());
        }
        (ProviderKind::Mistral | ProviderKind::Vertex | ProviderKind::Gemini, Op::ChatCompletions) => {
            // Neither Mistral nor Google take `max_completion_tokens`; Mistral rejects unknown fields.
            if let Some(v) = obj.remove("max_completion_tokens") {
                obj.entry("max_tokens").or_insert(v);
            }
            if d.provider == ProviderKind::Mistral {
                // Mistral reports usage on the last chunk without being asked.
                obj.remove("stream_options");
                obj.remove("user");
                return;
            }
            // Vertex names Gemini models by publisher.
            if d.provider == ProviderKind::Vertex
                && let Some(Value::String(m)) = obj.get_mut("model")
                && !m.contains('/')
            {
                *m = format!("google/{m}");
            }
            if let Some(Value::Array(messages)) = obj.get_mut("messages") {
                fill_thought_signatures(messages);
            }
        }
        _ => {}
    }
}

/// Give the first tool call of each assistant turn a thought signature when it has none.
fn fill_thought_signatures(messages: &mut [Value]) {
    for m in messages.iter_mut().filter(|m| m["role"] == "assistant") {
        let Some(Value::Array(calls)) = m.get_mut("tool_calls") else { continue };
        let signed = calls.iter().any(|c| c.pointer("/extra_content/google/thought_signature").is_some());
        if let (false, Some(Value::Object(first))) = (signed, calls.first_mut()) {
            first.insert("extra_content".into(), json!({ "google": { "thought_signature": SKIP_THOUGHT_SIGNATURE } }));
        }
    }
}

pub fn json_bytes(v: &serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(v).expect("json serialize")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::model::{CredentialSettings, DeploymentConfig, Mode, Pricing, Secrets};

    /// A deployment for tests, here and in the providers' own.
    pub(crate) fn deployment(provider: ProviderKind, style: ApiStyle, model: &str) -> Deployment {
        Deployment {
            id: uuid::Uuid::nil(),
            model_name: "m".into(),
            mode: Mode::Chat,
            provider,
            style,
            upstream_model: model.into(),
            config: DeploymentConfig::default(),
            connection: CredentialSettings::default(),
            secrets: Secrets::default(),
            weight: 1,
            priority: 0,
            rpm_limit: None,
            tpm_limit: None,
            timeout: std::time::Duration::from_secs(60),
            pricing: Pricing::default(),
            tags: Vec::new(),
            project_ids: Vec::new(),
            catalog: None,
        }
    }

    #[test]
    fn efforts_come_from_the_catalog() {
        use crate::ir::Effort::*;
        let with = |style, catalog: Option<serde_json::Value>| {
            Deployment { catalog: catalog.map(std::sync::Arc::new), ..deployment(ProviderKind::Azure, style, "m") }.efforts()
        };
        let luna = serde_json::json!({ "supports_reasoning": true, "supports_minimal_reasoning_effort": false, "supports_none_reasoning_effort": true,
            "supports_xhigh_reasoning_effort": true, "supports_max_reasoning_effort": true });
        assert_eq!(with(ApiStyle::Responses, Some(luna)), [None, Low, Medium, High, Xhigh, Max]);
        let gpt5 = serde_json::json!({ "supports_minimal_reasoning_effort": true, "supports_none_reasoning_effort": false });
        assert_eq!(with(ApiStyle::Chat, Some(gpt5)), [Minimal, Low, Medium, High]);
        let pro = serde_json::json!({ "supports_low_reasoning_effort": false, "supports_xhigh_reasoning_effort": true });
        assert_eq!(with(ApiStyle::Responses, Some(pro)), [Medium, High, Xhigh]);
        let levels = serde_json::json!({ "reasoning_effort_levels": ["none", "high"], "supports_minimal_reasoning_effort": true });
        assert_eq!(with(ApiStyle::Chat, Some(levels)), [None, High], "the list wins over flags");
        assert_eq!(with(ApiStyle::Chat, Option::None), [Minimal, Low, Medium, High]);
        // Anthropic models always take none (no thinking).
        let sonnet = serde_json::json!({ "supports_xhigh_reasoning_effort": true, "supports_max_reasoning_effort": true });
        assert_eq!(with(ApiStyle::Anthropic, Some(sonnet)), [None, Low, Medium, High, Xhigh, Max]);
        assert_eq!(with(ApiStyle::Converse, Option::None), [None, Low, Medium, High]);
        let bedrock = serde_json::json!({ "supports_xhigh_reasoning_effort": true, "supports_max_reasoning_effort": true, "bedrock_output_config_effort_ceiling": "xhigh" });
        assert_eq!(with(ApiStyle::Anthropic, Some(bedrock)), [None, Low, Medium, High, Xhigh], "capped by Bedrock");
    }

    #[test]
    fn translated_efforts_fit_the_model() {
        use crate::ir::{ChatRequest, Effort, Reasoning};
        let at = |style, catalog: Option<serde_json::Value>| Deployment {
            catalog: catalog.map(std::sync::Arc::new),
            ..deployment(ProviderKind::Azure, style, "m")
        };
        let asking = |e| ChatRequest { reasoning: Some(Reasoning::Effort(e)), ..ChatRequest::default() };
        let luna = serde_json::json!({ "supports_reasoning": true, "supports_minimal_reasoning_effort": false, "supports_none_reasoning_effort": true });
        let responses = |e| openai_responses::build_body(&asking(e), &at(ApiStyle::Responses, Some(luna.clone())));
        assert_eq!(responses(Effort::Minimal)["reasoning"]["effort"], "low");
        assert_eq!(responses(Effort::None)["reasoning"]["effort"], "none");
        assert_eq!(responses(Effort::Max)["reasoning"]["effort"], "high", "luna's entry here has no max");
        // No catalog entry: minimal to high, as before; `none` sends no effort (the model's default).
        let unknown = |e| openai_chat::build_body(&asking(e), &at(ApiStyle::Chat, Option::None));
        assert_eq!(unknown(Effort::Xhigh)["reasoning_effort"], "high");
        assert!(unknown(Effort::None).get("reasoning_effort").is_none());
        // A model that does not reason, or cannot turn reasoning off, gets no effort.
        let gpt4o = serde_json::json!({ "supports_vision": true });
        assert!(openai_chat::build_body(&asking(Effort::High), &at(ApiStyle::Chat, Some(gpt4o))).get("reasoning_effort").is_none());
        let o3 = serde_json::json!({ "supports_reasoning": true });
        assert!(openai_chat::build_body(&asking(Effort::None), &at(ApiStyle::Chat, Some(o3))).get("reasoning_effort").is_none());
        // Anthropic: `none` is no thinking; levels it lacks fit the nearest.
        let sonnet = serde_json::json!({ "supports_reasoning": true, "supports_xhigh_reasoning_effort": true });
        let claude = |e| anthropic::build_body(&asking(e), &at(ApiStyle::Anthropic, Some(sonnet.clone())));
        assert!(claude(Effort::None).get("thinking").is_none());
        assert_eq!(claude(Effort::Minimal)["output_config"]["effort"], "low");
        assert_eq!(claude(Effort::Max)["output_config"]["effort"], "xhigh");
    }

    #[test]
    fn openai_key_goes_in_the_configured_header() {
        let mut d = deployment(ProviderKind::Openai, ApiStyle::Chat, "m");
        d.secrets.api_key = Some("k1".into());
        let h = headers(&d).unwrap();
        assert_eq!(h["authorization"], "Bearer k1");
        assert!(h["authorization"].is_sensitive());
        d.connection.auth_header = Some("X-Api-Token".into());
        let h = headers(&d).unwrap();
        assert_eq!(h["x-api-token"], "k1");
        assert!(h["x-api-token"].is_sensitive());
        assert!(!h.contains_key("authorization"));
    }

    #[test]
    fn azure_with_entra_sends_no_key_headers() {
        for style in [ApiStyle::Chat, ApiStyle::Anthropic, ApiStyle::Rerank, ApiStyle::Systemone] {
            let mut d = deployment(ProviderKind::Azure, style, "m");
            d.connection.tenant_id = Some("contoso.onmicrosoft.com".into());
            d.connection.client_id = Some("00000000-0000-0000-0000-000000000001".into());
            d.secrets.client_secret = Some("s".into());
            // The token is added in `post`.
            let h = headers(&d).unwrap();
            assert!(!h.contains_key("api-key") && !h.contains_key("x-api-key") && !h.contains_key("authorization"));
        }
        let d = deployment(ProviderKind::Azure, ApiStyle::Chat, "m");
        assert!(headers(&d).is_err(), "neither a key nor Entra ID");
    }

    #[test]
    fn mistral_regions() {
        assert_eq!(mistral_base(None), "https://api.mistral.ai/v1");
        assert_eq!(mistral_base(Some("global")), "https://api.mistral.ai/v1");
        assert_eq!(mistral_base(Some("eu")), "https://api.eu.mistral.ai/v1");
        assert_eq!(mistral_base(Some("us")), "https://api.us.mistral.ai/v1");
    }

    #[test]
    fn vertex_claude_body() {
        let d = deployment(ProviderKind::Vertex, ApiStyle::Anthropic, "claude-sonnet-4-5@20250929");
        let mut body = json!({ "model": "claude-sonnet-4-5@20250929", "max_tokens": 10, "messages": [] });
        prepare_body(&d, Op::Messages, &mut body);
        assert_eq!(body, json!({ "max_tokens": 10, "messages": [], "anthropic_version": "vertex-2023-10-16" }));
        assert_eq!(utf8_percent_encode(&d.upstream_model, VERTEX_MODEL).to_string(), "claude-sonnet-4-5@20250929");
    }

    #[test]
    fn mistral_chat_body() {
        let d = deployment(ProviderKind::Mistral, ApiStyle::Chat, "mistral-large-latest");
        let mut body = json!({
            "model": "mistral-large-latest", "max_completion_tokens": 5, "stream_options": { "include_usage": true }, "user": "u",
            "messages": [{ "role": "assistant", "tool_calls": [{ "id": "1" }] }],
        });
        prepare_body(&d, Op::ChatCompletions, &mut body);
        assert_eq!(body["max_tokens"], 5);
        assert!(body.get("max_completion_tokens").is_none() && body.get("stream_options").is_none() && body.get("user").is_none());
        // Thought signatures are Google's.
        assert!(body.pointer("/messages/0/tool_calls/0/extra_content").is_none());
    }

    #[test]
    fn google_chat_body() {
        let d = deployment(ProviderKind::Vertex, ApiStyle::Chat, "gemini-2.5-flash");
        let mut body = json!({
            "model": "gemini-2.5-flash",
            "messages": [
                { "role": "assistant", "tool_calls": [{ "id": "1" }, { "id": "2" }] },
                { "role": "assistant", "tool_calls": [{ "id": "3", "extra_content": { "google": { "thought_signature": "real" } } }] },
            ],
        });
        prepare_body(&d, Op::ChatCompletions, &mut body);
        assert_eq!(body["model"], "google/gemini-2.5-flash");
        assert_eq!(body.pointer("/messages/0/tool_calls/0/extra_content/google/thought_signature").unwrap(), SKIP_THOUGHT_SIGNATURE);
        assert!(body.pointer("/messages/0/tool_calls/1/extra_content").is_none());
        assert_eq!(body.pointer("/messages/1/tool_calls/0/extra_content/google/thought_signature").unwrap(), "real");
        let mut publisher = json!({ "model": "meta/llama-4-maverick", "messages": [] });
        prepare_body(&d, Op::ChatCompletions, &mut publisher);
        assert_eq!(publisher["model"], "meta/llama-4-maverick");
    }

    #[test]
    fn pdfs_are_translated_between_every_api_and_provider() {
        let url = "data:application/pdf;base64,JVBERi0=";
        let parsed = [
            crate::api::chat::parse_request(&json!({ "messages": [{ "role": "user", "content": [
                { "type": "file", "file": { "filename": "Q3 report.pdf", "file_data": url } }] }] })),
            crate::api::responses::parse_request(&json!({ "input": [{ "role": "user", "content": [
                { "type": "input_file", "filename": "Q3 report.pdf", "file_data": url }] }] })),
            crate::api::anthropic::parse_request(&json!({ "max_tokens": 10, "messages": [{ "role": "user", "content": [
                { "type": "document", "title": "Q3 report.pdf", "source": { "type": "base64", "media_type": "application/pdf", "data": "JVBERi0=" } }] }] })),
        ];
        for req in parsed {
            let req = req.unwrap();
            assert!(matches!(&req.messages[0].content[0],
                crate::ir::Part::File { media_type, data, filename: Some(f), .. } if media_type == "application/pdf" && data == "JVBERi0=" && f == "Q3 report.pdf"));

            let chat = openai_chat::build_body(&req, &deployment(ProviderKind::Openai, ApiStyle::Chat, "m"));
            assert_eq!(
                chat.pointer("/messages/0/content/0"),
                Some(&json!({ "type": "file", "file": { "filename": "Q3 report.pdf", "file_data": url } }))
            );
            let responses = openai_responses::build_body(&req, &deployment(ProviderKind::Openai, ApiStyle::Responses, "m"));
            assert_eq!(
                responses.pointer("/input/0/content/0"),
                Some(&json!({ "type": "input_file", "filename": "Q3 report.pdf", "file_data": url }))
            );
            let claude = anthropic::build_body(&req, &deployment(ProviderKind::Anthropic, ApiStyle::Anthropic, "m"));
            assert_eq!(claude.pointer("/messages/0/content/0/source/data"), Some(&json!("JVBERi0=")));
            assert_eq!(claude.pointer("/messages/0/content/0/type"), Some(&json!("document")));
            let converse = bedrock::build_body(&req, &deployment(ProviderKind::Bedrock, ApiStyle::Converse, "m")).unwrap();
            assert_eq!(
                converse.pointer("/messages/0/content/0/document"),
                Some(&json!({ "format": "pdf", "name": "Q3 report", "source": { "bytes": "JVBERi0=" } }))
            );
        }
        assert!(
            crate::api::chat::parse_request(
                &json!({ "messages": [{ "role": "user", "content": [{ "type": "file", "file": { "file_id": "file-1" } }] }] })
            )
            .is_err()
        );
    }
}
