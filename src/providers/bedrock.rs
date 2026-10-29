//! Amazon Bedrock Converse streaming codec. Signing lives in [`crate::aws`].

use std::collections::HashMap;

use aws_smithy_eventstream::frame::{DecodedFrame, MessageFrameDecoder};
use aws_smithy_types::event_stream::Message;
use bytes::BytesMut;
use serde_json::{Map, Value, json};

use super::{Decoder, EventResult, EventStream, Op};
use crate::{
    error::{ErrorKind, GatewayError},
    ir::{BlockKind, BlockTracker, ChatRequest, Effort, ImageSource, Part, Role, StopReason, StreamEvent, ToolChoice, Usage},
    model::Deployment,
    state::AppState,
};

pub async fn send(state: &AppState, d: &Deployment, req: &ChatRequest) -> Result<EventStream, GatewayError> {
    let body = build_body(req, d)?;
    let url = super::url(state, d, Op::ConverseStream).await?;
    let resp = super::post(state, d, &url, super::headers(d)?, super::json_bytes(&body)).await?;
    Ok(super::decode_stream(super::body_stream(resp), ConverseDecoder::default()))
}

fn header<'a>(m: &'a Message, name: &str) -> Option<&'a str> {
    m.headers()
        .iter()
        .find(|h| h.name().as_str() == name)
        .and_then(|h| h.value().as_string().ok())
        .map(|s| s.as_str())
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
    let cleaned: String =
        stem.chars().map(|c| if c.is_ascii_alphanumeric() || "-()[]".contains(c) { c } else { ' ' }).collect();
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
    if !extra.is_empty() {
        body.insert("additionalModelRequestFields".into(), Value::Object(extra));
    }
    Ok(Value::Object(body))
}

#[derive(Default)]
pub struct ConverseDecoder {
    frames: MessageFrameDecoder,
    buf: BytesMut,
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

impl Decoder for ConverseDecoder {
    fn feed(&mut self, chunk: &[u8], out: &mut Vec<EventResult>) {
        self.buf.extend_from_slice(chunk);
        loop {
            let m = match self.frames.decode_frame(&mut self.buf) {
                Ok(DecodedFrame::Complete(m)) => m,
                Ok(DecodedFrame::Incomplete) => return,
                Err(e) => {
                    out.push(Err(GatewayError::upstream_stream(ErrorKind::Upstream, format!("event stream: {e}"))));
                    return;
                }
            };
            let payload: Value = serde_json::from_slice(m.payload()).unwrap_or(Value::Null);
            let message_type = header(&m, ":message-type");
            if message_type == Some("exception") || message_type == Some("error") {
                let ty = header(&m, ":exception-type").or(header(&m, ":error-code")).unwrap_or("exception");
                let msg = payload.get("message").and_then(Value::as_str).unwrap_or(ty).to_string();
                let kind = match ty {
                    "throttlingException" => ErrorKind::RateLimit,
                    "serviceUnavailableException" => ErrorKind::Overloaded,
                    "validationException" => ErrorKind::InvalidRequest,
                    "modelTimeoutException" => ErrorKind::Timeout,
                    _ => ErrorKind::Upstream,
                };
                out.push(Err(GatewayError::upstream_stream(kind, format!("{ty}: {msg}"))));
                return;
            }
            let mut evs = Vec::new();
            if let Some(ty) = header(&m, ":event-type") {
                self.on_event(ty, &payload, &mut evs);
            }
            out.extend(evs.into_iter().map(Ok));
        }
    }

    fn finish(&mut self, out: &mut Vec<EventResult>) {
        if self.stop.is_some() {
            let mut evs = Vec::new();
            self.finish(&mut evs);
            out.extend(evs.into_iter().map(Ok));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use aws_smithy_types::event_stream::{Header, HeaderValue};

    #[test]
    fn rejects_corrupted_frame() {
        let mut bytes = frame("contentBlockDelta", r#"{"contentBlockIndex":0,"delta":{"text":"Hi"}}"#);
        let n = bytes.len();
        bytes[n - 10] ^= 0xff; // flip payload bits; the message CRC no longer matches
        let mut out = Vec::new();
        ConverseDecoder::default().feed(&bytes, &mut out);
        assert!(matches!(out.as_slice(), [Err(e)] if e.message.contains("event stream")));
    }

    /// Encode a frame with the SDK's own writer, exactly as Bedrock sends it.
    fn frame(event: &str, payload: &str) -> Vec<u8> {
        let msg = Message::new(payload.as_bytes().to_vec())
            .add_header(Header::new(":message-type", HeaderValue::String("event".into())))
            .add_header(Header::new(":event-type", HeaderValue::String(event.to_string().into())));
        let mut out = Vec::new();
        aws_smithy_eventstream::frame::write_message_to(&msg, &mut out).unwrap();
        out
    }

    #[test]
    fn decodes_converse_stream() {
        let mut bytes = Vec::new();
        bytes.extend(frame("messageStart", r#"{"role":"assistant"}"#));
        bytes.extend(frame("contentBlockDelta", r#"{"contentBlockIndex":0,"delta":{"text":"Hi"}}"#));
        bytes.extend(frame("contentBlockStop", r#"{"contentBlockIndex":0}"#));
        bytes.extend(frame("contentBlockStart", r#"{"contentBlockIndex":1,"start":{"toolUse":{"toolUseId":"t1","name":"f"}}}"#));
        bytes.extend(frame("contentBlockDelta", r#"{"contentBlockIndex":1,"delta":{"toolUse":{"input":"{\"a\":1}"}}}"#));
        bytes.extend(frame("contentBlockStop", r#"{"contentBlockIndex":1}"#));
        bytes.extend(frame("messageStop", r#"{"stopReason":"tool_use"}"#));
        bytes.extend(frame("metadata", r#"{"usage":{"inputTokens":5,"outputTokens":3}}"#));
        let mut d = ConverseDecoder::default();
        let mut out = Vec::new();
        // Feed in awkward chunk sizes to exercise frames split across network reads.
        for chunk in bytes.chunks(7) {
            d.feed(chunk, &mut out);
        }
        let mut agg = crate::ir::Aggregator::default();
        out.iter().for_each(|e| agg.push(e.as_ref().unwrap()));
        let r = agg.finish();
        assert_eq!(r.stop_reason, StopReason::ToolUse);
        assert_eq!(r.usage.output_tokens, 3);
        assert!(matches!(&r.content[1], Part::ToolCall { arguments, .. } if arguments == "{\"a\":1}"));
    }
}
