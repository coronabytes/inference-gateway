//! Inbound Anthropic Messages API: request parsing and response/stream rendering.

use bytes::Bytes;
use serde_json::{Value, json};

use super::{StreamEncoder, sse_event};
use crate::{
    error::{Dialect, GatewayError},
    ir::{
        BlockKind, ChatRequest, ChatResponse, Effort, ImageSource, Message, Part, Reasoning, Role, StopReason,
        StreamEvent, TextBlock, Tool, ToolChoice, Usage,
    },
};

fn has_cache(v: &Value) -> bool {
    v.get("cache_control").is_some_and(|c| !c.is_null())
}

fn parse_image(b: &Value) -> Result<ImageSource, GatewayError> {
    let src = &b["source"];
    match src.get("type").and_then(Value::as_str) {
        Some("base64") => Ok(ImageSource::Base64 {
            media_type: src["media_type"].as_str().unwrap_or("image/png").to_string(),
            data: src["data"].as_str().unwrap_or_default().to_string(),
        }),
        Some("url") => Ok(ImageSource::Url(src["url"].as_str().unwrap_or_default().to_string())),
        _ => Err(GatewayError::invalid("unsupported image source type for this model")),
    }
}

fn parse_document(b: &Value, cache: bool) -> Result<Part, GatewayError> {
    let src = &b["source"];
    match src.get("type").and_then(Value::as_str) {
        Some("base64") => Ok(Part::File {
            media_type: src["media_type"].as_str().unwrap_or("application/pdf").to_string(),
            data: src["data"].as_str().unwrap_or_default().to_string(),
            filename: b.get("title").and_then(Value::as_str).map(str::to_string),
            cache,
        }),
        _ => Err(GatewayError::invalid("only base64 document sources are supported for this model's provider")),
    }
}

fn parse_blocks(content: &Value) -> Result<Vec<Part>, GatewayError> {
    let blocks = match content {
        Value::String(s) => return Ok(vec![Part::Text { text: s.clone(), cache: false }]),
        Value::Array(a) => a,
        Value::Null => return Ok(vec![]),
        _ => return Err(GatewayError::invalid("message content must be a string or an array")),
    };
    let mut out = Vec::with_capacity(blocks.len());
    for b in blocks {
        let cache = has_cache(b);
        match b.get("type").and_then(Value::as_str).unwrap_or("") {
            "text" => out.push(Part::Text { text: b["text"].as_str().unwrap_or_default().to_string(), cache }),
            "image" => out.push(Part::Image { source: parse_image(b)?, cache }),
            "document" => out.push(parse_document(b, cache)?),
            "tool_use" => out.push(Part::ToolCall {
                id: b["id"].as_str().unwrap_or_default().to_string(),
                name: b["name"].as_str().unwrap_or_default().to_string(),
                arguments: b.get("input").map(Value::to_string).unwrap_or_else(|| "{}".into()),
            }),
            "tool_result" => out.push(Part::ToolResult {
                call_id: b["tool_use_id"].as_str().unwrap_or_default().to_string(),
                content: parse_blocks(b.get("content").unwrap_or(&Value::Null))?,
                is_error: b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                cache,
            }),
            "thinking" => out.push(Part::Reasoning {
                text: b["thinking"].as_str().unwrap_or_default().to_string(),
                signature: b.get("signature").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string),
            }),
            "redacted_thinking" => {}
            other => {
                return Err(GatewayError::invalid(format!(
                    "content block type '{other}' is not supported for this model's provider"
                )));
            }
        }
    }
    Ok(out)
}

pub fn parse_request(v: &Value) -> Result<ChatRequest, GatewayError> {
    let mut req = ChatRequest::default();
    match v.get("system") {
        Some(Value::String(s)) => req.system.push(TextBlock { text: s.clone(), cache: false }),
        Some(Value::Array(a)) => {
            for b in a {
                req.system.push(TextBlock { text: b["text"].as_str().unwrap_or_default().to_string(), cache: has_cache(b) });
            }
        }
        _ => {}
    }
    let msgs = v.get("messages").and_then(Value::as_array).ok_or_else(|| GatewayError::invalid("messages is required"))?;
    for m in msgs {
        let content = parse_blocks(m.get("content").unwrap_or(&Value::Null))?;
        match m.get("role").and_then(Value::as_str) {
            Some("user") => req.messages.push(Message { role: Role::User, content }),
            Some("assistant") => req.messages.push(Message { role: Role::Assistant, content }),
            Some("system") => req.system.push(TextBlock { text: crate::ir::parts_text(&content), cache: false }),
            _ => return Err(GatewayError::invalid("message role must be user or assistant")),
        }
    }
    if let Some(tools) = v.get("tools").and_then(Value::as_array) {
        for t in tools {
            if let Some(ty) = t.get("type").and_then(Value::as_str)
                && ty != "custom"
            {
                return Err(GatewayError::invalid(format!("server tool '{ty}' is not supported for this model's provider")));
            }
            req.tools.push(Tool {
                name: t["name"].as_str().unwrap_or_default().to_string(),
                description: t.get("description").and_then(Value::as_str).map(str::to_string),
                parameters: t.get("input_schema").cloned().unwrap_or_else(|| json!({ "type": "object" })),
                cache: has_cache(t),
            });
        }
    }
    req.tool_choice = v.get("tool_choice").and_then(|tc| match tc.get("type").and_then(Value::as_str) {
        Some("auto") => Some(ToolChoice::Auto),
        Some("any") => Some(ToolChoice::Required),
        Some("none") => Some(ToolChoice::None),
        Some("tool") => tc.get("name").and_then(Value::as_str).map(|n| ToolChoice::Tool(n.to_string())),
        _ => None,
    });
    req.max_tokens = v.get("max_tokens").and_then(Value::as_u64).map(|n| n as u32);
    req.temperature = v.get("temperature").and_then(Value::as_f64);
    req.top_p = v.get("top_p").and_then(Value::as_f64);
    if let Some(stop) = v.get("stop_sequences").and_then(Value::as_array) {
        req.stop = stop.iter().filter_map(|s| s.as_str().map(str::to_string)).collect();
    }

    let effort = v.pointer("/output_config/effort").and_then(Value::as_str).and_then(Effort::parse);
    let thinking = v.get("thinking");
    req.reasoning = match thinking.and_then(|t| t.get("type")).and_then(Value::as_str) {
        Some("enabled") => {
            let b = thinking.and_then(|t| t.get("budget_tokens")).and_then(Value::as_u64).unwrap_or(4096);
            Some(Reasoning::Budget(b as u32))
        }
        Some("adaptive") => Some(Reasoning::Effort(effort.unwrap_or(Effort::Medium))),
        // Explicitly off: `none` on other providers, rather than their default effort.
        Some("disabled") => Some(Reasoning::Effort(Effort::None)),
        _ => effort.map(Reasoning::Effort),
    };
    req.reasoning_summary = thinking.and_then(|t| t.get("display")).and_then(Value::as_str) == Some("summarized");
    if let Some(fmt) = v.pointer("/output_config/format")
        && fmt.get("type").and_then(Value::as_str) == Some("json_schema")
    {
        req.json_schema = Some(json!({ "name": "output", "schema": fmt["schema"], "strict": true }));
    }
    req.user = v.pointer("/metadata/user_id").and_then(Value::as_str).map(str::to_string);
    req.normalize();
    Ok(req)
}

pub fn stop_reason_str(s: StopReason) -> &'static str {
    match s {
        StopReason::EndTurn => "end_turn",
        StopReason::MaxTokens => "max_tokens",
        StopReason::ToolUse => "tool_use",
        StopReason::StopSequence => "stop_sequence",
        StopReason::ContentFilter => "refusal",
    }
}

fn usage_json(u: &Usage) -> Value {
    json!({
        "input_tokens": u.input_tokens,
        "output_tokens": u.output_tokens,
        "cache_creation_input_tokens": u.cache_write_tokens,
        "cache_read_input_tokens": u.cache_read_tokens,
    })
}

pub fn render_response(r: &ChatResponse, id: &str, model: &str) -> Value {
    let content: Vec<Value> = r
        .content
        .iter()
        .filter_map(|p| match p {
            Part::Text { text, .. } => Some(json!({ "type": "text", "text": text })),
            Part::ToolCall { id, name, arguments } => {
                Some(json!({ "type": "tool_use", "id": id, "name": name, "input": crate::ir::parse_args(arguments) }))
            }
            Part::Reasoning { text, signature } => {
                Some(json!({ "type": "thinking", "thinking": text, "signature": signature.clone().unwrap_or_default() }))
            }
            _ => None,
        })
        .collect();
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason_str(r.stop_reason),
        "stop_sequence": null,
        "usage": usage_json(&r.usage),
    })
}

pub struct AnthropicEncoder {
    id: String,
    model: String,
}

impl AnthropicEncoder {
    pub fn new(id: String, model: String) -> Self {
        Self { id, model }
    }
}

impl StreamEncoder for AnthropicEncoder {
    fn start(&mut self) -> Vec<Bytes> {
        vec![sse_event(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": self.id, "type": "message", "role": "assistant", "model": self.model,
                    "content": [], "stop_reason": null, "stop_sequence": null,
                    "usage": { "input_tokens": 0, "output_tokens": 0 }
                }
            }),
        )]
    }

    fn event(&mut self, ev: &StreamEvent) -> Vec<Bytes> {
        let delta = |index: usize, d: Value| {
            sse_event("content_block_delta", &json!({ "type": "content_block_delta", "index": index, "delta": d }))
        };
        match ev {
            StreamEvent::BlockStart { index, kind } => {
                let block = match kind {
                    BlockKind::Text => json!({ "type": "text", "text": "" }),
                    BlockKind::ToolCall { id, name } => json!({ "type": "tool_use", "id": id, "name": name, "input": {} }),
                    BlockKind::Reasoning => json!({ "type": "thinking", "thinking": "", "signature": "" }),
                };
                vec![sse_event(
                    "content_block_start",
                    &json!({ "type": "content_block_start", "index": index, "content_block": block }),
                )]
            }
            StreamEvent::TextDelta { index, text } => vec![delta(*index, json!({ "type": "text_delta", "text": text }))],
            StreamEvent::ToolArgsDelta { index, json } => {
                vec![delta(*index, json!({ "type": "input_json_delta", "partial_json": json }))]
            }
            StreamEvent::ReasoningDelta { index, text } => {
                vec![delta(*index, json!({ "type": "thinking_delta", "thinking": text }))]
            }
            StreamEvent::SignatureDelta { index, signature } => {
                vec![delta(*index, json!({ "type": "signature_delta", "signature": signature }))]
            }
            StreamEvent::BlockStop { index } => {
                vec![sse_event("content_block_stop", &json!({ "type": "content_block_stop", "index": index }))]
            }
            StreamEvent::Finish { stop_reason, usage } => vec![
                sse_event(
                    "message_delta",
                    &json!({
                        "type": "message_delta",
                        "delta": { "stop_reason": stop_reason_str(*stop_reason), "stop_sequence": null },
                        "usage": usage_json(usage),
                    }),
                ),
                sse_event("message_stop", &json!({ "type": "message_stop" })),
            ],
        }
    }

    fn error(&mut self, e: &GatewayError) -> Vec<Bytes> {
        vec![sse_event("error", &e.body(Dialect::Anthropic))]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tools_and_results() {
        let v = json!({
            "model": "m", "max_tokens": 100,
            "system": [{"type": "text", "text": "sys", "cache_control": {"type": "ephemeral"}}],
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "f", "input": {"a": 1}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]}
            ],
            "tools": [{"name": "f", "input_schema": {"type": "object"}}],
            "thinking": {"type": "adaptive"}, "output_config": {"effort": "high"}
        });
        let r = parse_request(&v).unwrap();
        assert!(r.system[0].cache);
        assert_eq!(r.messages.len(), 3);
        assert!(matches!(r.reasoning, Some(Reasoning::Effort(Effort::High))));
        assert!(matches!(&r.messages[2].content[0], Part::ToolResult { call_id, .. } if call_id == "t1"));
    }

    #[test]
    fn rejects_server_tools() {
        let v = json!({"messages": [], "tools": [{"type": "web_search_20260209", "name": "web_search"}]});
        assert!(parse_request(&v).is_err());
    }
}
