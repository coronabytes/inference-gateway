//! Anthropic Messages upstream (api.anthropic.com, Azure Foundry, Bedrock Mantle).

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use super::{EventResult, EventStream, Op, Sse, SseHandler};
use crate::{
    error::{ErrorKind, GatewayError},
    ir::{BlockKind, BlockTracker, ChatRequest, Effort, ImageSource, Part, Role, StopReason, StreamEvent, ToolChoice, Usage, WebSearch},
    model::Deployment,
    state::AppState,
};

pub const DEFAULT_MAX_TOKENS: u32 = 32_000;

pub async fn send(state: &AppState, d: &Deployment, req: &ChatRequest) -> Result<EventStream, GatewayError> {
    let mut body = build_body(req, d);
    super::prepare_body(d, Op::Messages, &mut body);
    let url = super::url(state, d, Op::Messages).await?;
    let resp = super::post(state, d, &url, super::headers(d)?, super::json_bytes(&body)).await?;
    Ok(super::decode_stream(super::body_stream(resp), Sse::new(AnthropicHandler::default())))
}

fn cache_control(on: bool, obj: &mut Map<String, Value>) {
    if on {
        obj.insert("cache_control".into(), json!({ "type": "ephemeral" }));
    }
}

fn image_block(src: &ImageSource) -> Value {
    match src {
        ImageSource::Base64 { media_type, data } => {
            json!({ "type": "image", "source": { "type": "base64", "media_type": media_type, "data": data } })
        }
        ImageSource::Url(u) => json!({ "type": "image", "source": { "type": "url", "url": u } }),
    }
}

fn block(p: &Part) -> Option<Value> {
    let mut v = match p {
        Part::Text { text, cache } => {
            if text.is_empty() {
                return None;
            }
            let mut o = Map::new();
            o.insert("type".into(), "text".into());
            o.insert("text".into(), text.clone().into());
            cache_control(*cache, &mut o);
            Value::Object(o)
        }
        Part::Image { source, cache } => {
            let mut v = image_block(source);
            cache_control(*cache, v.as_object_mut().unwrap());
            v
        }
        Part::File { media_type, data, filename, cache } => {
            let mut v = json!({ "type": "document", "source": { "type": "base64", "media_type": media_type, "data": data } });
            if let Some(f) = filename {
                v["title"] = f.clone().into();
            }
            cache_control(*cache, v.as_object_mut().unwrap());
            v
        }
        Part::ToolCall { id, name, arguments } => {
            json!({ "type": "tool_use", "id": id, "name": name, "input": crate::ir::parse_args(arguments) })
        }
        Part::ToolResult { call_id, content, is_error, cache } => {
            let inner: Vec<Value> = content.iter().filter_map(block).collect();
            let mut o = Map::new();
            o.insert("type".into(), "tool_result".into());
            o.insert("tool_use_id".into(), call_id.clone().into());
            o.insert("content".into(), Value::Array(inner));
            if *is_error {
                o.insert("is_error".into(), true.into());
            }
            cache_control(*cache, &mut o);
            Value::Object(o)
        }
        // Anthropic only accepts thinking blocks it signed itself.
        Part::Reasoning { text, signature: Some(sig) } => {
            json!({ "type": "thinking", "thinking": text, "signature": sig })
        }
        Part::Reasoning { signature: None, .. } => return None,
    };
    if let Some(o) = v.as_object_mut()
        && o.get("type").and_then(Value::as_str) == Some("tool_result")
        && o.get("content").and_then(Value::as_array).is_some_and(Vec::is_empty)
    {
        o.remove("content");
    }
    Some(v)
}

pub fn build_body(req: &ChatRequest, d: &Deployment) -> Value {
    let mut messages = Vec::new();
    for m in &req.messages {
        let mut parts: Vec<&Part> = m.content.iter().collect();
        if m.role == Role::User {
            // tool_result blocks must lead the user turn.
            parts.sort_by_key(|p| !matches!(p, Part::ToolResult { .. }));
        }
        let content: Vec<Value> = parts.into_iter().filter_map(block).collect();
        if content.is_empty() {
            continue;
        }
        let role = if m.role == Role::User { "user" } else { "assistant" };
        messages.push(json!({ "role": role, "content": content }));
    }

    let mut max_tokens = req.max_tokens.or(d.config.default_max_tokens).unwrap_or(DEFAULT_MAX_TOKENS);
    let mut body = Map::new();
    body.insert("model".into(), d.upstream_model.clone().into());
    body.insert("messages".into(), Value::Array(messages));
    body.insert("stream".into(), true.into());

    if !req.system.is_empty() {
        let sys: Vec<Value> = req
            .system
            .iter()
            .map(|b| {
                let mut o = Map::new();
                o.insert("type".into(), "text".into());
                o.insert("text".into(), b.text.clone().into());
                cache_control(b.cache, &mut o);
                Value::Object(o)
            })
            .collect();
        body.insert("system".into(), Value::Array(sys));
    }
    let mut tools: Vec<Value> = req
        .tools
        .iter()
        .map(|t| {
            let mut o = Map::new();
            o.insert("name".into(), t.name.clone().into());
            if let Some(desc) = &t.description {
                o.insert("description".into(), desc.clone().into());
            }
            o.insert("input_schema".into(), t.parameters.clone());
            cache_control(t.cache, &mut o);
            Value::Object(o)
        })
        .collect();
    tools.extend(req.web_search.as_ref().map(web_search_tool));
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
        if let Some(tc) = &req.tool_choice {
            body.insert(
                "tool_choice".into(),
                match tc {
                    ToolChoice::Auto => json!({ "type": "auto" }),
                    ToolChoice::None => json!({ "type": "none" }),
                    ToolChoice::Required => json!({ "type": "any" }),
                    ToolChoice::Tool(n) => json!({ "type": "tool", "name": n }),
                },
            );
        }
    }
    if !req.stop.is_empty() {
        body.insert("stop_sequences".into(), json!(req.stop));
    }

    let mut output_config = Map::new();
    // The nearest effort the model takes, if any: `none` asks for no thinking.
    let thinking = req.reasoning.and_then(|r| Some((r, d.fit_effort(r.effort())?))).filter(|(_, e)| *e != Effort::None);
    if let Some((r, effort)) = thinking {
        if d.config.thinking_mode.as_deref() == Some("budget") {
            let budget = r.budget_tokens().max(1024);
            if max_tokens <= budget {
                max_tokens += budget;
            }
            body.insert("thinking".into(), json!({ "type": "enabled", "budget_tokens": budget }));
        } else {
            let mut thinking = json!({ "type": "adaptive" });
            if req.reasoning_summary {
                thinking["display"] = "summarized".into();
            }
            body.insert("thinking".into(), thinking);
            output_config.insert("effort".into(), effort.as_str().into());
        }
    }
    if let Some(schema) = &req.json_schema {
        let s = schema.get("schema").cloned().unwrap_or_else(|| schema.clone());
        output_config.insert("format".into(), json!({ "type": "json_schema", "schema": s }));
    }
    if !output_config.is_empty() {
        body.insert("output_config".into(), Value::Object(output_config));
    }
    if d.config.forward_sampling == Some(true) && thinking.is_none() {
        if let Some(t) = req.temperature {
            body.insert("temperature".into(), t.into());
        }
        if let Some(p) = req.top_p {
            body.insert("top_p".into(), p.into());
        }
    }
    if let Some(u) = &req.user {
        body.insert("metadata".into(), json!({ "user_id": u }));
    }
    body.insert("max_tokens".into(), max_tokens.into());
    Value::Object(body)
}

/// Anthropic's web search tool, in its first version: every Claude model with web search takes it.
fn web_search_tool(ws: &WebSearch) -> Value {
    let mut t = json!({ "type": "web_search_20250305", "name": "web_search" });
    if let Some(n) = ws.max_uses {
        t["max_uses"] = n.into();
    }
    // One list or the other: Anthropic refuses both.
    if !ws.allowed_domains.is_empty() {
        t["allowed_domains"] = json!(ws.allowed_domains);
    } else if !ws.blocked_domains.is_empty() {
        t["blocked_domains"] = json!(ws.blocked_domains);
    }
    if let Some(l) = &ws.location {
        let mut o = l.fields();
        o.insert("type".into(), "approximate".into());
        t["user_location"] = Value::Object(o);
    }
    t
}

pub fn stop_reason(s: &str) -> StopReason {
    match s {
        "max_tokens" | "model_context_window_exceeded" => StopReason::MaxTokens,
        "tool_use" => StopReason::ToolUse,
        "stop_sequence" => StopReason::StopSequence,
        "refusal" => StopReason::ContentFilter,
        _ => StopReason::EndTurn,
    }
}

pub fn usage(v: &Value) -> Usage {
    let g = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
    Usage {
        input_tokens: g("input_tokens"),
        output_tokens: g("output_tokens"),
        cache_read_tokens: g("cache_read_input_tokens"),
        cache_write_tokens: g("cache_creation_input_tokens"),
        reasoning_tokens: 0,
        search_units: v.pointer("/server_tool_use/web_search_requests").and_then(Value::as_u64).unwrap_or(0),
    }
}

pub fn error_kind(t: &str) -> ErrorKind {
    match t {
        "invalid_request_error" | "request_too_large" => ErrorKind::InvalidRequest,
        "rate_limit_error" => ErrorKind::RateLimit,
        "overloaded_error" => ErrorKind::Overloaded,
        "timeout_error" => ErrorKind::Timeout,
        _ => ErrorKind::Upstream,
    }
}

/// Decodes Anthropic Messages SSE into normalized events.
#[derive(Default)]
pub struct AnthropicHandler {
    blocks: BlockTracker,
    /// upstream content index -> our index (None for block types we drop)
    map: HashMap<u64, Option<usize>>,
    usage: Usage,
    stop: StopReason,
}

impl AnthropicHandler {
    pub fn on_json(&mut self, v: &Value, out: &mut Vec<EventResult>) {
        let mut evs = Vec::new();
        let idx = v.get("index").and_then(Value::as_u64).unwrap_or(0);
        match v.get("type").and_then(Value::as_str).unwrap_or("") {
            "message_start" => {
                if let Some(u) = v.pointer("/message/usage") {
                    self.usage.merge_max(&usage(u));
                }
            }
            "content_block_start" => {
                let cb = &v["content_block"];
                let kind = match cb.get("type").and_then(Value::as_str) {
                    Some("text") => Some(BlockKind::Text),
                    Some("tool_use") => Some(BlockKind::ToolCall {
                        id: cb["id"].as_str().unwrap_or_default().to_string(),
                        name: cb["name"].as_str().unwrap_or_default().to_string(),
                    }),
                    Some("thinking") => Some(BlockKind::Reasoning),
                    _ => None,
                };
                let mapped = kind.map(|k| self.blocks.start(&mut evs, k));
                self.map.insert(idx, mapped);
                if let Some(i) = mapped {
                    if let Some(t) = cb.get("text").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                        evs.push(StreamEvent::TextDelta { index: i, text: t.to_string() });
                    }
                    if let Some(t) = cb.get("thinking").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                        evs.push(StreamEvent::ReasoningDelta { index: i, text: t.to_string() });
                    }
                }
            }
            "content_block_delta" => {
                if let Some(Some(i)) = self.map.get(&idx).copied() {
                    let d = &v["delta"];
                    let s = |k: &str| d.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
                    match d.get("type").and_then(Value::as_str) {
                        Some("text_delta") => evs.push(StreamEvent::TextDelta { index: i, text: s("text") }),
                        Some("input_json_delta") => evs.push(StreamEvent::ToolArgsDelta { index: i, json: s("partial_json") }),
                        Some("thinking_delta") => evs.push(StreamEvent::ReasoningDelta { index: i, text: s("thinking") }),
                        Some("signature_delta") => {
                            evs.push(StreamEvent::SignatureDelta { index: i, signature: s("signature") })
                        }
                        _ => {}
                    }
                }
            }
            "content_block_stop" => {
                if let Some(Some(i)) = self.map.get(&idx).copied() {
                    self.blocks.stop(&mut evs, i);
                }
            }
            "message_delta" => {
                if let Some(r) = v.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop = stop_reason(r);
                }
                if let Some(u) = v.get("usage") {
                    self.usage.merge_max(&usage(u));
                }
            }
            "message_stop" => {
                self.blocks.stop_all(&mut evs);
                evs.push(StreamEvent::Finish { stop_reason: self.stop, usage: self.usage });
            }
            "error" => {
                let t = v.pointer("/error/type").and_then(Value::as_str).unwrap_or("api_error");
                let m = v.pointer("/error/message").and_then(Value::as_str).unwrap_or("stream error");
                out.extend(evs.into_iter().map(Ok));
                out.push(Err(GatewayError::upstream_stream(error_kind(t), m)));
                return;
            }
            _ => {}
        }
        out.extend(evs.into_iter().map(Ok));
    }
}

impl SseHandler for AnthropicHandler {
    fn on_event(&mut self, _event: Option<&str>, data: &str, out: &mut Vec<EventResult>) {
        match serde_json::from_str::<Value>(data) {
            Ok(v) => self.on_json(&v, out),
            Err(_) => tracing::debug!("ignoring non-JSON SSE data from anthropic upstream"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::Decoder;

    #[test]
    fn decodes_tool_stream() {
        let sse = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu_1\",\"name\":\"get\",\"input\":{}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"a\\\":1}\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":7}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        let mut dec = Sse::new(AnthropicHandler::default());
        let mut out = Vec::new();
        dec.feed(sse.as_bytes(), &mut out);
        let mut agg = crate::ir::Aggregator::default();
        for e in &out {
            agg.push(e.as_ref().unwrap());
        }
        let r = agg.finish();
        assert_eq!(r.stop_reason, StopReason::ToolUse);
        assert_eq!(r.usage.input_tokens, 10);
        assert_eq!(r.usage.output_tokens, 7);
        assert!(matches!(&r.content[1], Part::ToolCall { arguments, .. } if arguments == "{\"a\":1}"));
    }

    #[test]
    fn web_search_becomes_the_server_tool_and_its_searches_are_counted() {
        let d = crate::providers::tests::deployment(crate::model::ProviderKind::Anthropic, crate::model::ApiStyle::Anthropic, "claude");
        let req = ChatRequest {
            web_search: Some(WebSearch { max_uses: Some(2), blocked_domains: vec!["example.com".into()], ..Default::default() }),
            ..Default::default()
        };
        let body = build_body(&req, &d);
        assert_eq!(
            body["tools"],
            json!([{ "type": "web_search_20250305", "name": "web_search", "max_uses": 2, "blocked_domains": ["example.com"] }])
        );
        let u = usage(&json!({ "input_tokens": 5, "output_tokens": 2, "server_tool_use": { "web_search_requests": 3 } }));
        assert_eq!(u.search_units, 3);
    }
}
