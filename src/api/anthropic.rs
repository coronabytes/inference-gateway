//! Inbound Anthropic Messages API: request parsing and response/stream rendering.

use std::collections::HashMap;

use bytes::Bytes;
use serde_json::{Value, json};

use super::{StreamEncoder, sse_event};
use crate::{
    error::{Dialect, GatewayError},
    ir::{
        BlockKind, ChatRequest, ChatResponse, Effort, ImageSource, Location, McpItem, Message, Part, Reasoning, Role, StopReason,
        StreamEvent, TextBlock, Tool, ToolChoice, Usage, WebSearch, strings,
    },
    util::prefixed_id,
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
            // A web search the provider ran in an earlier turn: its answer is in the text that follows.
            "redacted_thinking" | "server_tool_use" | "web_search_tool_result" => {}
            // A gateway MCP tool's call in an earlier turn, and its result: the model gets them as
            // text, since the tool may not be offered now.
            "mcp_tool_use" => out.push(Part::Text {
                text: format!(
                    "[The gateway called the MCP tool {} of {} with {}]",
                    b["name"].as_str().unwrap_or_default(),
                    b["server_name"].as_str().unwrap_or_default(),
                    b.get("input").unwrap_or(&Value::Null)
                ),
                cache,
            }),
            "mcp_tool_result" => {
                let text = crate::ir::parts_text(&parse_blocks(b.get("content").unwrap_or(&Value::Null))?);
                out.push(Part::Text { text: format!("[The MCP tool's result: {text}]"), cache });
            }
            other => {
                return Err(GatewayError::invalid(format!("content block type '{other}' is not supported for this model's provider")));
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
            match t.get("type").and_then(Value::as_str) {
                None | Some("custom") => {}
                Some(ty) if ty.starts_with("web_search_") => {
                    req.web_search = Some(WebSearch {
                        max_uses: t.get("max_uses").and_then(Value::as_u64).map(|n| n as u32),
                        allowed_domains: strings(t.get("allowed_domains")),
                        blocked_domains: strings(t.get("blocked_domains")),
                        context_size: None,
                        location: t.get("user_location").and_then(Location::parse),
                    });
                    continue;
                }
                Some(ty) => return Err(GatewayError::invalid(format!("server tool '{ty}' is not supported for this model's provider"))),
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
    let mut v = json!({
        "input_tokens": u.input_tokens,
        "output_tokens": u.output_tokens,
        "cache_creation_input_tokens": u.cache_write_tokens,
        "cache_read_input_tokens": u.cache_read_tokens,
    });
    if u.search_units > 0 {
        v["server_tool_use"] = json!({ "web_search_requests": u.search_units });
    }
    v
}

/// A gateway MCP call as Anthropic's MCP connector returns its own: an `mcp_tool_use` block and
/// an `mcp_tool_result` block. A server's tools have no block.
fn mcp_blocks(item: &McpItem) -> Vec<Value> {
    let McpItem::Call { server, tool, arguments, result } = item else { return vec![] };
    let id = prefixed_id("mcptoolu");
    let content = match result {
        Ok(r) => r
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|c| match c.get("type").and_then(Value::as_str) {
                Some("text") => json!({ "type": "text", "text": c.get("text") }),
                _ => json!({ "type": "text", "text": c.to_string() }),
            })
            .collect(),
        Err(e) => vec![json!({ "type": "text", "text": e })],
    };
    vec![
        json!({ "type": "mcp_tool_use", "id": id, "name": tool, "server_name": server, "input": arguments }),
        json!({ "type": "mcp_tool_result", "tool_use_id": id, "is_error": item.is_error(), "content": content }),
    ]
}

pub fn render_response(r: &ChatResponse, id: &str, model: &str) -> Value {
    let mut mcp = r.mcp.iter().peekable();
    let mut content = Vec::new();
    for (i, p) in r.content.iter().enumerate() {
        while let Some((_, item)) = mcp.next_if(|(at, _)| *at <= i) {
            content.extend(mcp_blocks(item));
        }
        content.extend(match p {
            Part::Text { text, .. } => Some(json!({ "type": "text", "text": text })),
            Part::ToolCall { id, name, arguments } => {
                Some(json!({ "type": "tool_use", "id": id, "name": name, "input": crate::ir::parse_args(arguments) }))
            }
            Part::Reasoning { text, signature } => {
                Some(json!({ "type": "thinking", "thinking": text, "signature": signature.clone().unwrap_or_default() }))
            }
            _ => None,
        });
    }
    content.extend(mcp.flat_map(|(_, item)| mcp_blocks(item)));
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
    /// Block indexes as the client gets them: a gateway MCP call takes two blocks, its tools none.
    blocks: HashMap<usize, usize>,
    next: usize,
}

impl AnthropicEncoder {
    pub fn new(id: String, model: String) -> Self {
        Self { id, model, blocks: HashMap::new(), next: 0 }
    }

    fn at(&self, index: usize) -> usize {
        self.blocks.get(&index).copied().unwrap_or(index)
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
                let at = self.next;
                self.next += 1;
                self.blocks.insert(*index, at);
                let index = &at;
                let block = match kind {
                    BlockKind::Text => json!({ "type": "text", "text": "" }),
                    BlockKind::ToolCall { id, name } => json!({ "type": "tool_use", "id": id, "name": name, "input": {} }),
                    BlockKind::Reasoning => json!({ "type": "thinking", "thinking": "", "signature": "" }),
                };
                vec![sse_event("content_block_start", &json!({ "type": "content_block_start", "index": index, "content_block": block }))]
            }
            StreamEvent::TextDelta { index, text } => vec![delta(self.at(*index), json!({ "type": "text_delta", "text": text }))],
            StreamEvent::ToolArgsDelta { index, json } => {
                vec![delta(self.at(*index), json!({ "type": "input_json_delta", "partial_json": json }))]
            }
            StreamEvent::ReasoningDelta { index, text } => {
                vec![delta(self.at(*index), json!({ "type": "thinking_delta", "thinking": text }))]
            }
            StreamEvent::SignatureDelta { index, signature } => {
                vec![delta(self.at(*index), json!({ "type": "signature_delta", "signature": signature }))]
            }
            StreamEvent::BlockStop { index } => {
                vec![sse_event("content_block_stop", &json!({ "type": "content_block_stop", "index": self.at(*index) }))]
            }
            // The provider's own tools and citations: logged, never sent (see `StreamEvent::ServerTool`).
            StreamEvent::ServerTool { .. } | StreamEvent::Annotation { .. } => vec![],
            // Whole when it comes: the call has run.
            StreamEvent::Mcp { item, .. } => {
                let mut out = Vec::new();
                for block in mcp_blocks(item) {
                    let index = self.next;
                    self.next += 1;
                    out.push(sse_event(
                        "content_block_start",
                        &json!({ "type": "content_block_start", "index": index, "content_block": block }),
                    ));
                    out.push(sse_event("content_block_stop", &json!({ "type": "content_block_stop", "index": index })));
                }
                out
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
    fn rejects_server_tools_but_web_search() {
        let v = json!({"messages": [], "tools": [{"type": "code_execution_20250825", "name": "code_execution"}]});
        assert!(parse_request(&v).is_err());
        let v = json!({"messages": [], "tools": [{
            "type": "web_search_20260209", "name": "web_search", "max_uses": 3, "allowed_domains": ["metoffice.gov.uk"],
            "user_location": {"type": "approximate", "city": "London", "country": "GB"}
        }]});
        let ws = parse_request(&v).unwrap().web_search.unwrap();
        assert_eq!((ws.max_uses, ws.allowed_domains.as_slice()), (Some(3), ["metoffice.gov.uk".to_string()].as_slice()));
        assert_eq!(ws.location.unwrap().city.as_deref(), Some("London"));
    }
}
