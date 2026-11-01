//! Amazon Bedrock through the official SDK (`aws-sdk-bedrockruntime`): Converse for any model,
//! and Claude's Messages API through InvokeModel. Clients and error mapping live in [`crate::aws`].
//!
//! Requests are built as Bedrock's JSON (Converse's or Anthropic's), as the providers' own tests
//! check them, and handed to the SDK as its types or as an InvokeModel body. Stream events come
//! back as the SDK's types and drive the same state machines as before.

use std::{
    collections::{HashMap, VecDeque},
    time::Duration,
};

use aws_sdk_bedrockruntime::{
    primitives::{Blob, event_stream::EventReceiver},
    types as bt,
};
use aws_smithy_types::{Document, Number};
use base64::Engine;
use bytes::Bytes;
use futures::StreamExt;
use serde_json::{Map, Value, json};

use super::{ByteStream, EventResult, EventStream, anthropic::AnthropicHandler};
use crate::{
    aws::{sdk_error, stream_sdk_error},
    error::{ErrorKind, GatewayError},
    ir::{BlockKind, BlockTracker, ChatRequest, Effort, ImageSource, Part, Role, StopReason, StreamEvent, ToolChoice, Usage},
    model::Deployment,
    state::AppState,
};

/// Claude's Messages API version for InvokeModel, which takes it in the body.
const BEDROCK_ANTHROPIC_VERSION: &str = "bedrock-2023-05-31";

/// Converse, streamed.
pub async fn send(state: &AppState, d: &Deployment, req: &ChatRequest) -> Result<EventStream, GatewayError> {
    let body = build_body(req, d)?;
    let client = state.aws.runtime(state, d).await?;
    let call = converse_request(client.converse_stream().model_id(&d.upstream_model), &body)?;
    let out = within(d.timeout, call.send()).await?.map_err(sdk_error)?;
    let mut dec = ConverseDecoder::default();
    Ok(sdk_stream(out.stream, d.timeout, move |ev, out| dec.on_sdk(ev, out), |_| {}))
}

/// Claude's Messages API through InvokeModel, streamed, for a translated request.
pub async fn send_invoke(state: &AppState, d: &Deployment, req: &ChatRequest) -> Result<EventStream, GatewayError> {
    let mut body = super::anthropic::build_body(req, d);
    invoke_body(&mut body);
    let client = state.aws.runtime(state, d).await?;
    let call = client
        .invoke_model_with_response_stream()
        .model_id(&d.upstream_model)
        .content_type("application/json")
        .accept("application/json")
        .body(Blob::new(super::json_bytes(&body)));
    let out = within(d.timeout, call.send()).await?.map_err(sdk_error)?;
    let mut handler = AnthropicHandler::default();
    Ok(sdk_stream(
        out.body,
        d.timeout,
        move |ev, out| {
            if let Some(v) = chunk_json(&ev) {
                handler.on_json(&v, out);
            }
        },
        |_| {},
    ))
}

/// A client's own Messages request to Claude through InvokeModel: streamed as Anthropic's SSE, or
/// the whole response.
pub enum Passthrough {
    Stream(ByteStream),
    Full(Bytes),
}

pub async fn passthrough(state: &AppState, d: &Deployment, raw: &Value, stream: bool) -> Result<Passthrough, GatewayError> {
    let mut body = raw.clone();
    invoke_body(&mut body);
    let client = state.aws.runtime(state, d).await?;
    if !stream {
        let call = client.invoke_model().model_id(&d.upstream_model).content_type("application/json").accept("application/json");
        let out = within(d.timeout, call.body(Blob::new(super::json_bytes(&body))).send()).await?.map_err(sdk_error)?;
        return Ok(Passthrough::Full(Bytes::from(out.body.into_inner())));
    }
    let call = client
        .invoke_model_with_response_stream()
        .model_id(&d.upstream_model)
        .content_type("application/json")
        .accept("application/json")
        .body(Blob::new(super::json_bytes(&body)));
    let out = within(d.timeout, call.send()).await?.map_err(sdk_error)?;
    Ok(Passthrough::Stream(sse_stream(out.body, d.timeout)))
}

/// Claude's token count for a Messages request (`/v1/messages/count_tokens`), through CountTokens.
pub async fn count_tokens(state: &AppState, d: &Deployment, raw: &Value) -> Result<Value, GatewayError> {
    let mut body = raw.clone();
    invoke_body(&mut body);
    // An InvokeModel body needs `max_tokens`; it does not change the count.
    body.as_object_mut().map(|o| o.entry("max_tokens").or_insert(json!(1)));
    let request = bt::InvokeModelTokensRequest::builder().body(Blob::new(super::json_bytes(&body))).build().map_err(build_error)?;
    let client = state.aws.runtime(state, d).await?;
    // CountTokens takes the foundation model, not an inference profile.
    let call = client.count_tokens().model_id(foundation_model(&d.upstream_model)).input(bt::CountTokensInput::InvokeModel(request));
    let out = within(d.timeout, call.send()).await?.map_err(sdk_error)?;
    Ok(json!({ "input_tokens": out.input_tokens() }))
}

/// Titan Text Embeddings V2 and other InvokeModel calls answered with one JSON body.
pub async fn invoke_json(state: &AppState, d: &Deployment, body: &Value) -> Result<Value, GatewayError> {
    let client = state.aws.runtime(state, d).await?;
    let call = client
        .invoke_model()
        .model_id(&d.upstream_model)
        .content_type("application/json")
        .accept("application/json")
        .body(Blob::new(super::json_bytes(body)));
    let out = within(d.timeout, call.send()).await?.map_err(sdk_error)?;
    serde_json::from_slice(out.body.as_ref())
        .map_err(|_| GatewayError::upstream_stream(ErrorKind::Upstream, "Bedrock returned a body that is not JSON"))
}

/// A Messages body for InvokeModel: the model is in the request, the version in the body.
fn invoke_body(body: &mut Value) {
    if let Some(o) = body.as_object_mut() {
        o.remove("model");
        o.remove("stream");
        o.insert("anthropic_version".into(), BEDROCK_ANTHROPIC_VERSION.into());
    }
}

/// An inference profile's foundation model: `eu.anthropic.claude-…` is `anthropic.claude-…`.
fn foundation_model(id: &str) -> &str {
    match id.split_once('.') {
        Some((geo, rest)) if ["us", "eu", "apac", "global", "jp", "au", "ca", "us-gov"].contains(&geo) => rest,
        _ => id,
    }
}

/// Run an SDK call within the deployment's timeout.
async fn within<T>(timeout: Duration, call: impl Future<Output = T>) -> Result<T, GatewayError> {
    tokio::time::timeout(timeout, call).await.map_err(|_| GatewayError { upstream: true, ..GatewayError::timeout("upstream timed out") })
}

fn build_error(e: aws_smithy_types::error::operation::BuildError) -> GatewayError {
    GatewayError::invalid(format!("not a valid Bedrock request: {e}"))
}

/// The Anthropic event a response stream chunk carries.
fn chunk_json(ev: &bt::ResponseStream) -> Option<Value> {
    match ev {
        bt::ResponseStream::Chunk(part) => serde_json::from_slice(part.bytes()?.as_ref()).ok(),
        _ => None,
    }
}

/// An SDK event stream as the gateway's events. `feed` turns each event into events; the stream
/// ends after the first `Finish` or error, and is an error if Bedrock closes it without finishing.
/// Each event must arrive within `timeout`.
fn sdk_stream<T, E, F, G>(rx: EventReceiver<T, E>, timeout: Duration, feed: F, end: G) -> EventStream
where
    T: Send + 'static,
    E: aws_smithy_types::error::metadata::ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
    F: FnMut(T, &mut Vec<EventResult>) + Send + 'static,
    G: FnMut(&mut Vec<EventResult>) + Send + 'static,
{
    struct St<T, E, F, G> {
        rx: EventReceiver<T, E>,
        feed: F,
        end: G,
        pending: VecDeque<EventResult>,
        eof: bool,
        done: bool,
    }
    let st = St { rx, feed, end, pending: VecDeque::new(), eof: false, done: false };
    futures::stream::unfold(st, move |mut st| async move {
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
            match tokio::time::timeout(timeout, st.rx.recv()).await {
                Ok(Ok(Some(ev))) => (st.feed)(ev, &mut out),
                Ok(Err(e)) => out.push(Err(stream_sdk_error(e))),
                Err(_) => {
                    out.push(Err(GatewayError::upstream_stream(ErrorKind::Timeout, "Bedrock sent nothing within the deployment's timeout")))
                }
                Ok(Ok(None)) => {
                    st.eof = true;
                    (st.end)(&mut out);
                    if !out.iter().any(|e| matches!(e, Ok(StreamEvent::Finish { .. }) | Err(_))) {
                        out.push(Err(GatewayError::upstream_stream(ErrorKind::Upstream, "stream ended before completion")));
                    }
                }
            }
            st.pending.extend(out);
        }
    })
    .boxed()
}

/// InvokeModel's response stream as Anthropic's SSE, for clients of the Messages API. An error
/// inside the stream ends it with an `error` event, as Anthropic's API does.
fn sse_stream(rx: EventReceiver<bt::ResponseStream, bt::error::ResponseStreamError>, timeout: Duration) -> ByteStream {
    futures::stream::unfold(Some(rx), move |rx| async move {
        let mut rx = rx?;
        loop {
            let ev = match tokio::time::timeout(timeout, rx.recv()).await {
                Ok(Ok(Some(ev))) => ev,
                Ok(Ok(None)) => return None,
                Ok(Err(e)) => return Some((Ok(sse_error(&stream_sdk_error(e))), None)),
                Err(_) => {
                    let e = GatewayError::upstream_stream(ErrorKind::Timeout, "Bedrock sent nothing within the deployment's timeout");
                    return Some((Ok(sse_error(&e)), None));
                }
            };
            if let Some(v) = chunk_json(&ev) {
                let ty = v["type"].as_str().unwrap_or("message").to_string();
                return Some((Ok(Bytes::from(format!("event: {ty}\ndata: {v}\n\n"))), Some(rx)));
            }
        }
    })
    .boxed()
}

/// An Anthropic `error` event for an error inside a stream.
fn sse_error(e: &GatewayError) -> Bytes {
    let v = json!({ "type": "error", "error": { "type": e.anthropic_type(), "message": e.message } });
    Bytes::from(format!("event: error\ndata: {v}\n\n"))
}

// ---------------------------------------------------------------- Converse JSON as SDK types

/// A Converse request, built as Bedrock's JSON (`build_body`), on the SDK's builder.
fn converse_request(
    call: aws_sdk_bedrockruntime::operation::converse_stream::builders::ConverseStreamFluentBuilder,
    body: &Value,
) -> Result<aws_sdk_bedrockruntime::operation::converse_stream::builders::ConverseStreamFluentBuilder, GatewayError> {
    let messages = body["messages"].as_array().into_iter().flatten().map(message).collect::<Result<Vec<_>, _>>()?;
    let mut call = call.set_messages(Some(messages));
    if let Some(sys) = body["system"].as_array() {
        call = call.set_system(Some(sys.iter().map(system_block).collect::<Result<Vec<_>, _>>()?));
    }
    if let Some(ic) = body.get("inferenceConfig") {
        let f = |k: &str| ic.get(k).and_then(Value::as_f64).map(|x| x as f32);
        call = call.inference_config(
            bt::InferenceConfiguration::builder()
                .set_max_tokens(ic.get("maxTokens").and_then(Value::as_i64).map(|n| n as i32))
                .set_temperature(f("temperature"))
                .set_top_p(f("topP"))
                .set_stop_sequences(
                    ic.get("stopSequences")
                        .and_then(Value::as_array)
                        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()),
                )
                .build(),
        );
    }
    if let Some(tc) = body.get("toolConfig") {
        call = call.tool_config(tool_config(tc)?);
    }
    if let Some(extra) = body.get("additionalModelRequestFields") {
        call = call.additional_model_request_fields(sdk_document(extra));
    }
    if let Some(def) = body.pointer("/outputConfig/textFormat/structure/jsonSchema") {
        let s = |k: &str| def.get(k).and_then(Value::as_str).map(str::to_string);
        let def = bt::JsonSchemaDefinition::builder()
            .set_schema(s("schema"))
            .set_name(s("name"))
            .set_description(s("description"))
            .build()
            .map_err(build_error)?;
        let format = bt::OutputFormat::builder()
            .r#type(bt::OutputFormatType::JsonSchema)
            .structure(bt::OutputFormatStructure::JsonSchema(def))
            .build()
            .map_err(build_error)?;
        call = call.output_config(bt::OutputConfig::builder().text_format(format).build());
    }
    Ok(call)
}

fn message(m: &Value) -> Result<bt::Message, GatewayError> {
    let role = if m["role"] == "assistant" { bt::ConversationRole::Assistant } else { bt::ConversationRole::User };
    let content = m["content"].as_array().into_iter().flatten().map(content_block).collect::<Result<Vec<_>, _>>()?;
    bt::Message::builder().role(role).set_content(Some(content)).build().map_err(build_error)
}

fn cache_point_block() -> Result<bt::CachePointBlock, GatewayError> {
    bt::CachePointBlock::builder().r#type(bt::CachePointType::Default).build().map_err(build_error)
}

fn bytes(v: &Value) -> Result<Blob, GatewayError> {
    let b64 = v.as_str().unwrap_or_default();
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map(Blob::new)
        .map_err(|_| GatewayError::invalid("an image or document is not valid base64"))
}

fn image_block(v: &Value) -> Result<bt::ImageBlock, GatewayError> {
    bt::ImageBlock::builder()
        .format(bt::ImageFormat::from(v["format"].as_str().unwrap_or_default()))
        .source(bt::ImageSource::Bytes(bytes(&v["source"]["bytes"])?))
        .build()
        .map_err(build_error)
}

fn document_block(v: &Value) -> Result<bt::DocumentBlock, GatewayError> {
    bt::DocumentBlock::builder()
        .format(bt::DocumentFormat::from(v["format"].as_str().unwrap_or_default()))
        .name(v["name"].as_str().unwrap_or("document"))
        .source(bt::DocumentSource::Bytes(bytes(&v["source"]["bytes"])?))
        .build()
        .map_err(build_error)
}

fn content_block(b: &Value) -> Result<bt::ContentBlock, GatewayError> {
    Ok(if let Some(t) = b.get("text").and_then(Value::as_str) {
        bt::ContentBlock::Text(t.to_string())
    } else if b.get("cachePoint").is_some() {
        bt::ContentBlock::CachePoint(cache_point_block()?)
    } else if let Some(i) = b.get("image") {
        bt::ContentBlock::Image(image_block(i)?)
    } else if let Some(d) = b.get("document") {
        bt::ContentBlock::Document(document_block(d)?)
    } else if let Some(tu) = b.get("toolUse") {
        bt::ContentBlock::ToolUse(
            bt::ToolUseBlock::builder()
                .tool_use_id(tu["toolUseId"].as_str().unwrap_or_default())
                .name(tu["name"].as_str().unwrap_or_default())
                .input(sdk_document(&tu["input"]))
                .build()
                .map_err(build_error)?,
        )
    } else if let Some(tr) = b.get("toolResult") {
        let mut content = Vec::new();
        for c in tr["content"].as_array().into_iter().flatten() {
            content.push(if let Some(t) = c.get("text").and_then(Value::as_str) {
                bt::ToolResultContentBlock::Text(t.to_string())
            } else if let Some(i) = c.get("image") {
                bt::ToolResultContentBlock::Image(image_block(i)?)
            } else if let Some(d) = c.get("document") {
                bt::ToolResultContentBlock::Document(document_block(d)?)
            } else {
                continue;
            });
        }
        bt::ContentBlock::ToolResult(
            bt::ToolResultBlock::builder()
                .tool_use_id(tr["toolUseId"].as_str().unwrap_or_default())
                .set_content(Some(content))
                .set_status((tr["status"] == "error").then_some(bt::ToolResultStatus::Error))
                .build()
                .map_err(build_error)?,
        )
    } else if let Some(r) = b.pointer("/reasoningContent/reasoningText") {
        bt::ContentBlock::ReasoningContent(bt::ReasoningContentBlock::ReasoningText(
            bt::ReasoningTextBlock::builder()
                .text(r["text"].as_str().unwrap_or_default())
                .set_signature(r["signature"].as_str().map(str::to_string))
                .build()
                .map_err(build_error)?,
        ))
    } else {
        return Err(GatewayError::internal(format!("unexpected Converse content block {b}")));
    })
}

fn system_block(b: &Value) -> Result<bt::SystemContentBlock, GatewayError> {
    match b.get("text").and_then(Value::as_str) {
        Some(t) => Ok(bt::SystemContentBlock::Text(t.to_string())),
        None => Ok(bt::SystemContentBlock::CachePoint(cache_point_block()?)),
    }
}

fn tool_config(tc: &Value) -> Result<bt::ToolConfiguration, GatewayError> {
    let mut tools = Vec::new();
    for t in tc["tools"].as_array().into_iter().flatten() {
        tools.push(match t.get("toolSpec") {
            Some(spec) => bt::Tool::ToolSpec(
                bt::ToolSpecification::builder()
                    .name(spec["name"].as_str().unwrap_or_default())
                    .set_description(spec["description"].as_str().map(str::to_string))
                    .input_schema(bt::ToolInputSchema::Json(sdk_document(&spec["inputSchema"]["json"])))
                    .build()
                    .map_err(build_error)?,
            ),
            None => bt::Tool::CachePoint(cache_point_block()?),
        });
    }
    let choice = match tc.get("toolChoice") {
        Some(c) if c.get("any").is_some() => Some(bt::ToolChoice::Any(bt::AnyToolChoice::builder().build())),
        Some(c) if c.get("auto").is_some() => Some(bt::ToolChoice::Auto(bt::AutoToolChoice::builder().build())),
        Some(c) => match c.pointer("/tool/name").and_then(Value::as_str) {
            Some(n) => Some(bt::ToolChoice::Tool(bt::SpecificToolChoice::builder().name(n).build().map_err(build_error)?)),
            None => None,
        },
        None => None,
    };
    bt::ToolConfiguration::builder().set_tools(Some(tools)).set_tool_choice(choice).build().map_err(build_error)
}

/// JSON as the SDK's untyped document (tool input and schemas, additional model fields).
pub(crate) fn sdk_document(v: &Value) -> Document {
    match v {
        Value::Null => Document::Null,
        Value::Bool(b) => Document::Bool(*b),
        Value::Number(n) => Document::Number(match (n.as_u64(), n.as_i64()) {
            (Some(u), _) => Number::PosInt(u),
            (None, Some(i)) => Number::NegInt(i),
            _ => Number::Float(n.as_f64().unwrap_or_default()),
        }),
        Value::String(s) => Document::String(s.clone()),
        Value::Array(a) => Document::Array(a.iter().map(sdk_document).collect()),
        Value::Object(o) => Document::Object(o.iter().map(|(k, v)| (k.clone(), sdk_document(v))).collect()),
    }
}

fn is_claude(d: &Deployment) -> bool {
    let m = d.upstream_model.to_ascii_lowercase();
    m.contains("anthropic") || m.contains("claude")
}

fn cache_point() -> Value {
    json!({ "cachePoint": { "type": "default" } })
}

fn image(src: &ImageSource) -> Result<Value, GatewayError> {
    match src {
        ImageSource::Base64 { media_type, data } => {
            let format = media_type.strip_prefix("image/").unwrap_or(media_type);
            let format = if format == "jpg" { "jpeg" } else { format };
            Ok(json!({ "image": { "format": format, "source": { "bytes": data } } }))
        }
        ImageSource::Url(_) => Err(GatewayError::invalid("bedrock does not accept image URLs; send base64 images")),
    }
}

fn document(media_type: &str, data: &str, filename: &Option<String>) -> Result<Value, GatewayError> {
    let format = match media_type {
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        "text/csv" => "csv",
        "text/html" => "html",
        "text/markdown" => "md",
        other => return Err(GatewayError::invalid(format!("bedrock does not accept {other} documents"))),
    };
    Ok(json!({ "document": { "format": format, "name": document_name(filename), "source": { "bytes": data } } }))
}

/// Bedrock allows letters, digits, single spaces, hyphens, parentheses and square brackets in a
/// document's name; the extension goes, since `format` gives the type.
fn document_name(filename: &Option<String>) -> String {
    let stem = filename.as_deref().map(|f| f.rsplit_once('.').map_or(f, |(s, _)| s)).unwrap_or_default();
    let cleaned: String = stem.chars().map(|c| if c.is_ascii_alphanumeric() || "-()[]".contains(c) { c } else { ' ' }).collect();
    let name = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if name.is_empty() { "document".into() } else { name }
}

fn blocks(p: &Part, out: &mut Vec<Value>) -> Result<(), GatewayError> {
    match p {
        Part::Text { text, cache } => {
            if !text.is_empty() {
                out.push(json!({ "text": text }));
            }
            if *cache {
                out.push(cache_point());
            }
        }
        Part::Image { source, cache } => {
            out.push(image(source)?);
            if *cache {
                out.push(cache_point());
            }
        }
        Part::File { media_type, data, filename, cache } => {
            out.push(document(media_type, data, filename)?);
            if *cache {
                out.push(cache_point());
            }
        }
        Part::ToolCall { id, name, arguments } => {
            out.push(json!({ "toolUse": { "toolUseId": id, "name": name, "input": crate::ir::parse_args(arguments) } }));
        }
        Part::ToolResult { call_id, content, is_error, cache } => {
            let mut inner = Vec::new();
            for c in content {
                match c {
                    Part::Text { text, .. } => inner.push(json!({ "text": text })),
                    Part::Image { source, .. } => inner.push(image(source)?),
                    Part::File { media_type, data, filename, .. } => inner.push(document(media_type, data, filename)?),
                    _ => {}
                }
            }
            if inner.is_empty() {
                inner.push(json!({ "text": "" }));
            }
            let mut r = json!({ "toolUseId": call_id, "content": inner });
            if *is_error {
                r["status"] = "error".into();
            }
            out.push(json!({ "toolResult": r }));
            if *cache {
                out.push(cache_point());
            }
        }
        Part::Reasoning { text, signature: Some(sig) } => {
            out.push(json!({ "reasoningContent": { "reasoningText": { "text": text, "signature": sig } } }));
        }
        Part::Reasoning { signature: None, .. } => {}
    }
    Ok(())
}

pub fn build_body(req: &ChatRequest, d: &Deployment) -> Result<Value, GatewayError> {
    if req.web_search.is_some() {
        return Err(GatewayError::invalid("web search is not supported for this model's provider"));
    }
    let mut messages = Vec::new();
    for m in &req.messages {
        let mut parts: Vec<&Part> = m.content.iter().collect();
        if m.role == Role::User {
            parts.sort_by_key(|p| !matches!(p, Part::ToolResult { .. }));
        }
        let mut content = Vec::new();
        for p in parts {
            blocks(p, &mut content)?;
        }
        if content.is_empty() {
            continue;
        }
        let role = if m.role == Role::User { "user" } else { "assistant" };
        messages.push(json!({ "role": role, "content": content }));
    }

    let mut body = Map::new();
    body.insert("messages".into(), Value::Array(messages));
    if !req.system.is_empty() {
        let mut sys = Vec::new();
        for b in &req.system {
            sys.push(json!({ "text": b.text }));
            if b.cache {
                sys.push(cache_point());
            }
        }
        body.insert("system".into(), Value::Array(sys));
    }

    let claude = is_claude(d);
    let mut max_tokens = req.max_tokens.or(d.config.default_max_tokens);
    let mut extra = Map::new();
    // The nearest effort the model takes, if any: `none` asks for no thinking.
    let thinking = req.reasoning.filter(|_| claude).and_then(|r| Some((r, d.fit_effort(r.effort())?))).filter(|(_, e)| *e != Effort::None);
    if let Some((r, effort)) = thinking {
        if d.config.thinking_mode.as_deref() == Some("budget") {
            let budget = r.budget_tokens().max(1024);
            let mt = max_tokens.unwrap_or(super::anthropic::DEFAULT_MAX_TOKENS);
            max_tokens = Some(if mt <= budget { mt + budget } else { mt });
            extra.insert("thinking".into(), json!({ "type": "enabled", "budget_tokens": budget }));
        } else {
            extra.insert("thinking".into(), json!({ "type": "adaptive" }));
            extra.insert("output_config".into(), json!({ "effort": effort.as_str() }));
        }
    }
    if claude && max_tokens.is_none() {
        max_tokens = Some(super::anthropic::DEFAULT_MAX_TOKENS);
    }

    let mut inference = Map::new();
    if let Some(mt) = max_tokens {
        inference.insert("maxTokens".into(), mt.into());
    }
    let sampling = !claude || (d.config.forward_sampling == Some(true) && thinking.is_none());
    if sampling {
        if let Some(t) = req.temperature {
            inference.insert("temperature".into(), t.into());
        }
        if let Some(p) = req.top_p {
            inference.insert("topP".into(), p.into());
        }
    }
    if !req.stop.is_empty() {
        inference.insert("stopSequences".into(), json!(req.stop));
    }
    if !inference.is_empty() {
        body.insert("inferenceConfig".into(), Value::Object(inference));
    }

    if !req.tools.is_empty() && req.tool_choice != Some(ToolChoice::None) {
        let mut tools = Vec::new();
        for t in &req.tools {
            let mut spec = json!({ "name": t.name, "inputSchema": { "json": t.parameters } });
            if let Some(desc) = &t.description {
                spec["description"] = desc.clone().into();
            }
            tools.push(json!({ "toolSpec": spec }));
            if t.cache {
                tools.push(cache_point());
            }
        }
        let mut tool_config = json!({ "tools": tools });
        match &req.tool_choice {
            Some(ToolChoice::Required) => tool_config["toolChoice"] = json!({ "any": {} }),
            Some(ToolChoice::Tool(n)) => tool_config["toolChoice"] = json!({ "tool": { "name": n } }),
            Some(ToolChoice::Auto) => tool_config["toolChoice"] = json!({ "auto": {} }),
            _ => {}
        }
        body.insert("toolConfig".into(), tool_config);
    } else if req.messages.iter().any(|m| m.content.iter().any(|p| matches!(p, Part::ToolCall { .. } | Part::ToolResult { .. }))) {
        // Converse rejects tool blocks in history without a toolConfig.
        let names: Vec<&str> = req
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|p| if let Part::ToolCall { name, .. } = p { Some(name.as_str()) } else { None })
            .collect();
        let tools: Vec<Value> = names
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .map(|n| json!({ "toolSpec": { "name": n, "inputSchema": { "json": { "type": "object" } } } }))
            .collect();
        if !tools.is_empty() {
            body.insert("toolConfig".into(), json!({ "tools": tools }));
        }
    }
    if let Some(schema) = &req.json_schema {
        body.insert("outputConfig".into(), json!({ "textFormat": text_format(schema) }));
    }
    if !extra.is_empty() {
        body.insert("additionalModelRequestFields".into(), Value::Object(extra));
    }
    Ok(Value::Object(body))
}

/// Structured output (`{name, schema, strict}`, or a bare schema) as Converse's `textFormat`, which
/// takes the schema as a JSON string. Converse is always strict.
fn text_format(schema: &Value) -> Value {
    let mut def = json!({ "schema": schema.get("schema").unwrap_or(schema).to_string() });
    for k in ["name", "description"] {
        if let Some(v) = schema.get(k).and_then(Value::as_str) {
            def[k] = v.into();
        }
    }
    json!({ "type": "json_schema", "structure": { "jsonSchema": def } })
}

/// Converse's stream events into the gateway's events.
#[derive(Default)]
pub struct ConverseDecoder {
    blocks: BlockTracker,
    map: HashMap<u64, usize>,
    stop: Option<StopReason>,
    saw_tool: bool,
    usage: Usage,
    done: bool,
}

impl ConverseDecoder {
    fn block(&mut self, idx: u64, kind: BlockKind, evs: &mut Vec<StreamEvent>) -> usize {
        if let Some(&i) = self.map.get(&idx) {
            return i;
        }
        let i = self.blocks.start(evs, kind);
        self.map.insert(idx, i);
        i
    }

    fn finish(&mut self, evs: &mut Vec<StreamEvent>) {
        if self.done {
            return;
        }
        self.done = true;
        self.blocks.stop_all(evs);
        let mut stop = self.stop.unwrap_or_default();
        if stop == StopReason::EndTurn && self.saw_tool {
            stop = StopReason::ToolUse;
        }
        evs.push(StreamEvent::Finish { stop_reason: stop, usage: self.usage });
    }

    fn on_event(&mut self, ty: &str, v: &Value, evs: &mut Vec<StreamEvent>) {
        let idx = v.get("contentBlockIndex").and_then(Value::as_u64).unwrap_or(0);
        match ty {
            "contentBlockStart" => {
                if let Some(tu) = v.pointer("/start/toolUse") {
                    self.saw_tool = true;
                    let id = tu.get("toolUseId").and_then(Value::as_str).unwrap_or_default().to_string();
                    let name = tu.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
                    self.block(idx, BlockKind::ToolCall { id, name }, evs);
                }
            }
            "contentBlockDelta" => {
                let d = &v["delta"];
                if let Some(t) = d.get("text").and_then(Value::as_str) {
                    let i = self.block(idx, BlockKind::Text, evs);
                    evs.push(StreamEvent::TextDelta { index: i, text: t.to_string() });
                } else if let Some(tu) = d.get("toolUse") {
                    let i = self.block(idx, BlockKind::ToolCall { id: crate::util::prefixed_id("tooluse"), name: String::new() }, evs);
                    let input = tu.get("input").and_then(Value::as_str).unwrap_or_default().to_string();
                    evs.push(StreamEvent::ToolArgsDelta { index: i, json: input });
                } else if let Some(rc) = d.get("reasoningContent") {
                    let i = self.block(idx, BlockKind::Reasoning, evs);
                    if let Some(t) = rc.get("text").and_then(Value::as_str) {
                        evs.push(StreamEvent::ReasoningDelta { index: i, text: t.to_string() });
                    }
                    if let Some(s) = rc.get("signature").and_then(Value::as_str) {
                        evs.push(StreamEvent::SignatureDelta { index: i, signature: s.to_string() });
                    }
                }
            }
            "contentBlockStop" => {
                if let Some(&i) = self.map.get(&idx) {
                    self.blocks.stop(evs, i);
                }
            }
            "messageStop" => {
                self.stop = Some(match v.get("stopReason").and_then(Value::as_str).unwrap_or("end_turn") {
                    "max_tokens" | "model_context_window_exceeded" => StopReason::MaxTokens,
                    "tool_use" => StopReason::ToolUse,
                    "stop_sequence" => StopReason::StopSequence,
                    "guardrail_intervened" | "content_filtered" => StopReason::ContentFilter,
                    _ => StopReason::EndTurn,
                });
            }
            "metadata" => {
                if let Some(u) = v.get("usage") {
                    let g = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
                    self.usage = Usage {
                        input_tokens: g("inputTokens"),
                        output_tokens: g("outputTokens"),
                        cache_read_tokens: g("cacheReadInputTokens"),
                        cache_write_tokens: g("cacheWriteInputTokens"),
                        reasoning_tokens: 0,
                        search_units: 0,
                    };
                }
                self.finish(evs);
            }
            _ => {}
        }
    }
}

impl ConverseDecoder {
    /// An SDK event, as the JSON Converse sends it on the wire.
    fn on_sdk(&mut self, ev: bt::ConverseStreamOutput, out: &mut Vec<EventResult>) {
        let mut evs = Vec::new();
        if let Some((ty, v)) = event_json(&ev) {
            self.on_event(ty, &v, &mut evs);
        }
        out.extend(evs.into_iter().map(Ok));
    }
}

/// A Converse stream event as Bedrock's JSON: its type and payload. Events the gateway does not
/// use (message start, images in a block start, redacted reasoning) give `None`.
fn event_json(ev: &bt::ConverseStreamOutput) -> Option<(&'static str, Value)> {
    use bt::ConverseStreamOutput as O;
    Some(match ev {
        O::ContentBlockStart(e) => {
            let bt::ContentBlockStart::ToolUse(t) = e.start()? else { return None };
            (
                "contentBlockStart",
                json!({ "contentBlockIndex": e.content_block_index(), "start": { "toolUse": { "toolUseId": t.tool_use_id(), "name": t.name() } } }),
            )
        }
        O::ContentBlockDelta(e) => {
            let delta = match e.delta()? {
                bt::ContentBlockDelta::Text(t) => json!({ "text": t }),
                bt::ContentBlockDelta::ToolUse(t) => json!({ "toolUse": { "input": t.input() } }),
                bt::ContentBlockDelta::ReasoningContent(bt::ReasoningContentBlockDelta::Text(t)) => {
                    json!({ "reasoningContent": { "text": t } })
                }
                bt::ContentBlockDelta::ReasoningContent(bt::ReasoningContentBlockDelta::Signature(s)) => {
                    json!({ "reasoningContent": { "signature": s } })
                }
                _ => return None,
            };
            ("contentBlockDelta", json!({ "contentBlockIndex": e.content_block_index(), "delta": delta }))
        }
        O::ContentBlockStop(e) => ("contentBlockStop", json!({ "contentBlockIndex": e.content_block_index() })),
        O::MessageStop(e) => ("messageStop", json!({ "stopReason": e.stop_reason().as_str() })),
        O::Metadata(e) => {
            let u = e.usage();
            let usage = u.map(|u| {
                json!({
                    "inputTokens": u.input_tokens(),
                    "outputTokens": u.output_tokens(),
                    "cacheReadInputTokens": u.cache_read_input_tokens().unwrap_or(0),
                    "cacheWriteInputTokens": u.cache_write_input_tokens().unwrap_or(0),
                })
            });
            ("metadata", json!({ "usage": usage }))
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(events: &[(&str, Value)]) -> crate::ir::ChatResponse {
        let mut d = ConverseDecoder::default();
        let mut evs = Vec::new();
        for (ty, v) in events {
            d.on_event(ty, v, &mut evs);
        }
        let mut agg = crate::ir::Aggregator::default();
        evs.iter().for_each(|e| agg.push(e));
        agg.finish()
    }

    #[test]
    fn structured_output_becomes_the_text_format() {
        let d =
            crate::providers::tests::deployment(crate::model::ProviderKind::Bedrock, crate::model::ApiStyle::Converse, "anthropic.claude");
        let schema = json!({ "type": "object", "properties": { "name": { "type": "string" } } });
        let req = ChatRequest { json_schema: Some(json!({ "name": "contact", "schema": schema, "strict": true })), ..Default::default() };
        let body = build_body(&req, &d).unwrap();
        assert_eq!(
            body["outputConfig"],
            json!({ "textFormat": { "type": "json_schema", "structure": { "jsonSchema": { "schema": schema.to_string(), "name": "contact" } } } })
        );
        // And on the SDK's request, which is what is sent.
        let config = aws_sdk_bedrockruntime::Config::builder().behavior_version_latest().build();
        let client = aws_sdk_bedrockruntime::Client::from_conf(config);
        let call = converse_request(client.converse_stream().model_id("m"), &body).unwrap();
        let sent = call.get_output_config().as_ref().and_then(|c| c.text_format()).and_then(|f| f.structure()).unwrap();
        assert_eq!(sent.as_json_schema().unwrap().schema(), schema.to_string());
    }

    #[test]
    fn decodes_converse_stream() {
        let r = run(&[
            ("contentBlockDelta", json!({ "contentBlockIndex": 0, "delta": { "text": "Hi" } })),
            ("contentBlockStop", json!({ "contentBlockIndex": 0 })),
            ("contentBlockStart", json!({ "contentBlockIndex": 1, "start": { "toolUse": { "toolUseId": "t1", "name": "f" } } })),
            ("contentBlockDelta", json!({ "contentBlockIndex": 1, "delta": { "toolUse": { "input": "{\"a\":1}" } } })),
            ("contentBlockStop", json!({ "contentBlockIndex": 1 })),
            ("messageStop", json!({ "stopReason": "tool_use" })),
            ("metadata", json!({ "usage": { "inputTokens": 5, "outputTokens": 3 } })),
        ]);
        assert_eq!(r.stop_reason, StopReason::ToolUse);
        assert_eq!(r.usage.output_tokens, 3);
        assert!(matches!(&r.content[1], Part::ToolCall { arguments, .. } if arguments == "{\"a\":1}"));
    }

    #[test]
    fn sdk_events_read_as_converse_json() {
        let delta = bt::ConverseStreamOutput::ContentBlockDelta(
            bt::ContentBlockDeltaEvent::builder().content_block_index(2).delta(bt::ContentBlockDelta::Text("Hi".into())).build().unwrap(),
        );
        assert_eq!(event_json(&delta), Some(("contentBlockDelta", json!({ "contentBlockIndex": 2, "delta": { "text": "Hi" } }))));
        let start = bt::ConverseStreamOutput::ContentBlockStart(
            bt::ContentBlockStartEvent::builder()
                .content_block_index(1)
                .start(bt::ContentBlockStart::ToolUse(bt::ToolUseBlockStart::builder().tool_use_id("t1").name("f").build().unwrap()))
                .build()
                .unwrap(),
        );
        assert_eq!(event_json(&start).unwrap().1["start"]["toolUse"]["name"], "f");
        let stop =
            bt::ConverseStreamOutput::MessageStop(bt::MessageStopEvent::builder().stop_reason(bt::StopReason::MaxTokens).build().unwrap());
        assert_eq!(event_json(&stop), Some(("messageStop", json!({ "stopReason": "max_tokens" }))));
    }

    #[test]
    fn converse_json_becomes_sdk_types() {
        let body = json!({
            "messages": [
                { "role": "user", "content": [{ "text": "hi" }, { "cachePoint": { "type": "default" } }, { "image": { "format": "png", "source": { "bytes": "aGk=" } } }] },
                { "role": "assistant", "content": [{ "toolUse": { "toolUseId": "t1", "name": "f", "input": { "a": 1 } } }] },
                { "role": "user", "content": [{ "toolResult": { "toolUseId": "t1", "content": [{ "text": "ok" }], "status": "error" } }] },
            ],
            "system": [{ "text": "be brief" }, { "cachePoint": { "type": "default" } }],
            "inferenceConfig": { "maxTokens": 100, "temperature": 0.5, "stopSequences": ["x"] },
            "toolConfig": { "tools": [{ "toolSpec": { "name": "f", "inputSchema": { "json": { "type": "object" } } } }], "toolChoice": { "tool": { "name": "f" } } },
            "additionalModelRequestFields": { "thinking": { "type": "adaptive" } },
        });
        let messages: Vec<bt::Message> = body["messages"].as_array().unwrap().iter().map(|m| message(m).unwrap()).collect();
        assert_eq!(messages[0].content().len(), 3);
        assert!(matches!(&messages[0].content()[2], bt::ContentBlock::Image(i) if i.format() == &bt::ImageFormat::Png));
        assert!(matches!(&messages[1].content()[0], bt::ContentBlock::ToolUse(t) if t.input() == &sdk_document(&json!({ "a": 1 }))));
        assert!(matches!(&messages[2].content()[0], bt::ContentBlock::ToolResult(t) if t.status() == Some(&bt::ToolResultStatus::Error)));
        let tc = tool_config(&body["toolConfig"]).unwrap();
        assert!(matches!(tc.tool_choice(), Some(bt::ToolChoice::Tool(t)) if t.name() == "f"));
        assert!(matches!(system_block(&body["system"][1]).unwrap(), bt::SystemContentBlock::CachePoint(_)));
        assert!(content_block(&json!({ "image": { "format": "png", "source": { "bytes": "not base64!" } } })).is_err());
    }

    #[test]
    fn invoke_bodies() {
        let mut b = json!({ "model": "claude", "stream": true, "max_tokens": 10, "messages": [] });
        invoke_body(&mut b);
        assert_eq!(b, json!({ "max_tokens": 10, "messages": [], "anthropic_version": BEDROCK_ANTHROPIC_VERSION }));
        assert_eq!(foundation_model("eu.anthropic.claude-sonnet-4-5-20250929-v1:0"), "anthropic.claude-sonnet-4-5-20250929-v1:0");
        assert_eq!(foundation_model("anthropic.claude-v2:1"), "anthropic.claude-v2:1");
    }
}
