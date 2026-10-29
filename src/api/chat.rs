//! Inbound OpenAI Chat Completions API: request parsing and response/stream rendering.

use std::collections::HashMap;

use bytes::Bytes;
use serde_json::{Map, Value, json};

use super::StreamEncoder;
use crate::{
    error::{Dialect, GatewayError},
    ir::{
        BlockKind, ChatRequest, ChatResponse, Effort, ImageSource, Message, Part, Reasoning, Role, StopReason,
        StreamEvent, TextBlock, Tool, ToolChoice, Usage,
    },
};

fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join(""),
        _ => String::new(),
    }
}

fn parse_content(content: &Value) -> Result<Vec<Part>, GatewayError> {
    let parts = match content {
        Value::String(s) => return Ok(vec![Part::Text { text: s.clone(), cache: false }]),
        Value::Array(a) => a,
        Value::Null => return Ok(vec![]),
        _ => return Err(GatewayError::invalid("message content must be a string or an array")),
    };
    let mut out = Vec::new();
    for p in parts {
        match p.get("type").and_then(Value::as_str).unwrap_or("text") {
            "text" => out.push(Part::Text { text: p["text"].as_str().unwrap_or_default().to_string(), cache: false }),
            "image_url" => {
                let url = p
                    .get("image_url")
                    .and_then(|u| u.get("url").and_then(Value::as_str).or_else(|| u.as_str()))
                    .ok_or_else(|| GatewayError::invalid("image_url part requires a url"))?;
                out.push(Part::Image { source: ImageSource::from_url(url), cache: false });
            }
            "file" => {
                let f = &p["file"];
                let data = f
                    .get("file_data")
                    .and_then(Value::as_str)
                    .ok_or_else(|| GatewayError::invalid("file part requires file_data (file_id is not supported)"))?;
                let part = Part::file_from_data_url(data, f.get("filename").and_then(Value::as_str))
                    .ok_or_else(|| GatewayError::invalid("file_data must be a base64 data URL"))?;
                out.push(part);
            }
            other => {
                return Err(GatewayError::invalid(format!("content part type '{other}' is not supported for this model's provider")));
            }
        }
    }
    Ok(out)
}

pub fn parse_request(v: &Value) -> Result<ChatRequest, GatewayError> {
    let mut req = ChatRequest::default();
    let msgs = v.get("messages").and_then(Value::as_array).ok_or_else(|| GatewayError::invalid("messages is required"))?;
    for m in msgs {
        let content = m.get("content").unwrap_or(&Value::Null);
        match m.get("role").and_then(Value::as_str) {
            Some("system" | "developer") => req.system.push(TextBlock { text: text_of(content), cache: false }),
            Some("user") => req.messages.push(Message { role: Role::User, content: parse_content(content)? }),
            Some("assistant") => {
                let mut parts = parse_content(content)?;
                if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
                    for c in calls {
                        parts.push(Part::ToolCall {
                            id: c["id"].as_str().unwrap_or_default().to_string(),
                            name: c.pointer("/function/name").and_then(Value::as_str).unwrap_or_default().to_string(),
                            arguments: c.pointer("/function/arguments").and_then(Value::as_str).unwrap_or("{}").to_string(),
                        });
                    }
                }
                req.messages.push(Message { role: Role::Assistant, content: parts });
            }
            Some("tool") => req.messages.push(Message {
                role: Role::User,
                content: vec![Part::ToolResult {
                    call_id: m["tool_call_id"].as_str().unwrap_or_default().to_string(),
                    content: vec![Part::Text { text: text_of(content), cache: false }],
                    is_error: false,
                    cache: false,
                }],
            }),
            _ => return Err(GatewayError::invalid("message role must be system, developer, user, assistant or tool")),
        }
    }
    if let Some(tools) = v.get("tools").and_then(Value::as_array) {
        for t in tools {
            if t.get("type").and_then(Value::as_str).unwrap_or("function") != "function" {
                return Err(GatewayError::invalid("only function tools are supported"));
            }
            let f = &t["function"];
            req.tools.push(Tool {
                name: f["name"].as_str().unwrap_or_default().to_string(),
                description: f.get("description").and_then(Value::as_str).map(str::to_string),
                parameters: f.get("parameters").cloned().filter(|p| !p.is_null()).unwrap_or_else(|| json!({ "type": "object" })),
                cache: false,
            });
        }
    }
    req.tool_choice = match v.get("tool_choice") {
        Some(Value::String(s)) => match s.as_str() {
            "none" => Some(ToolChoice::None),
            "required" => Some(ToolChoice::Required),
            _ => Some(ToolChoice::Auto),
        },
        Some(o @ Value::Object(_)) => o.pointer("/function/name").and_then(Value::as_str).map(|n| ToolChoice::Tool(n.to_string())),
        _ => None,
    };
    req.max_tokens = v
        .get("max_completion_tokens")
        .or_else(|| v.get("max_tokens"))
        .and_then(Value::as_u64)
        .map(|n| n as u32);
    req.temperature = v.get("temperature").and_then(Value::as_f64);
    req.top_p = v.get("top_p").and_then(Value::as_f64);
    req.stop = match v.get("stop") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect(),
        _ => vec![],
    };
    req.reasoning = v.get("reasoning_effort").and_then(Value::as_str).and_then(Effort::parse).map(Reasoning::Effort);
    if let Some(rf) = v.get("response_format")
        && rf.get("type").and_then(Value::as_str) == Some("json_schema")
    {
        req.json_schema = rf.get("json_schema").cloned();
    }
    req.user = v.get("user").and_then(Value::as_str).map(str::to_string);
    req.normalize();
    Ok(req)
}

pub fn finish_reason(s: StopReason) -> &'static str {
    match s {
        StopReason::EndTurn | StopReason::StopSequence => "stop",
        StopReason::MaxTokens => "length",
        StopReason::ToolUse => "tool_calls",
        StopReason::ContentFilter => "content_filter",
    }
}

pub fn usage_json(u: &Usage) -> Value {
    json!({
        "prompt_tokens": u.total_input(),
        "completion_tokens": u.output_tokens,
        "total_tokens": u.total(),
        "prompt_tokens_details": { "cached_tokens": u.cache_read_tokens },
        "completion_tokens_details": { "reasoning_tokens": u.reasoning_tokens },
    })
}

pub fn render_response(r: &ChatResponse, id: &str, model: &str) -> Value {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut calls = Vec::new();
    for p in &r.content {
        match p {
            Part::Text { text: t, .. } => text.push_str(t),
            Part::Reasoning { text: t, .. } => reasoning.push_str(t),
            Part::ToolCall { id, name, arguments } => calls.push(json!({
                "id": id, "type": "function", "function": { "name": name, "arguments": arguments }
            })),
            _ => {}
        }
    }
    let mut message = Map::new();
    message.insert("role".into(), "assistant".into());
    message.insert("content".into(), if text.is_empty() && !calls.is_empty() { Value::Null } else { text.into() });
    if !reasoning.is_empty() {
        message.insert("reasoning_content".into(), reasoning.into());
    }
    if !calls.is_empty() {
        message.insert("tool_calls".into(), Value::Array(calls));
    }
    json!({
        "id": id,
        "object": "chat.completion",
        "created": chrono::Utc::now().timestamp(),
        "model": model,
        "choices": [{ "index": 0, "message": message, "finish_reason": finish_reason(r.stop_reason) }],
        "usage": usage_json(&r.usage),
    })
}

fn data(v: &Value) -> Bytes {
    Bytes::from(format!("data: {}\n\n", serde_json::to_string(v).expect("json")))
}

pub struct ChatEncoder {
    id: String,
    model: String,
    created: i64,
    include_usage: bool,
    /// Our block index -> position in the `tool_calls` array.
    tool_index: HashMap<usize, usize>,
}

impl ChatEncoder {
    pub fn new(id: String, model: String, include_usage: bool) -> Self {
        Self { id, model, created: chrono::Utc::now().timestamp(), include_usage, tool_index: HashMap::new() }
    }

    fn chunk(&self, delta: Value, finish: Option<&str>) -> Bytes {
        data(&json!({
            "id": self.id, "object": "chat.completion.chunk", "created": self.created, "model": self.model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
        }))
    }
}

impl StreamEncoder for ChatEncoder {
    fn start(&mut self) -> Vec<Bytes> {
        vec![self.chunk(json!({ "role": "assistant", "content": "" }), None)]
    }

    fn event(&mut self, ev: &StreamEvent) -> Vec<Bytes> {
        match ev {
            StreamEvent::BlockStart { index, kind: BlockKind::ToolCall { id, name } } => {
                let n = self.tool_index.len();
                self.tool_index.insert(*index, n);
                vec![self.chunk(
                    json!({ "tool_calls": [{ "index": n, "id": id, "type": "function", "function": { "name": name, "arguments": "" } }] }),
                    None,
                )]
            }
            StreamEvent::TextDelta { text, .. } => vec![self.chunk(json!({ "content": text }), None)],
            StreamEvent::ReasoningDelta { text, .. } => vec![self.chunk(json!({ "reasoning_content": text }), None)],
            StreamEvent::ToolArgsDelta { index, json: args } => match self.tool_index.get(index) {
                Some(n) => vec![self.chunk(json!({ "tool_calls": [{ "index": n, "function": { "arguments": args } }] }), None)],
                None => vec![],
            },
            StreamEvent::Finish { stop_reason, usage } => {
                let mut out = vec![self.chunk(json!({}), Some(finish_reason(*stop_reason)))];
                if self.include_usage {
                    out.push(data(&json!({
                        "id": self.id, "object": "chat.completion.chunk", "created": self.created, "model": self.model,
                        "choices": [], "usage": usage_json(usage),
                    })));
                }
                out.push(Bytes::from_static(b"data: [DONE]\n\n"));
                out
            }
            _ => vec![],
        }
    }

    fn error(&mut self, e: &GatewayError) -> Vec<Bytes> {
        vec![data(&e.body(Dialect::OpenAi)), Bytes::from_static(b"data: [DONE]\n\n")]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tool_round_trip() {
        let v = json!({
            "model": "m", "max_tokens": 50, "stop": "END",
            "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": [{"type": "text", "text": "weather?"}]},
                {"role": "assistant", "content": null, "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "w", "arguments": "{}"}}]},
                {"role": "tool", "tool_call_id": "c1", "content": "sunny"}
            ],
            "tools": [{"type": "function", "function": {"name": "w", "parameters": {"type": "object"}}}]
        });
        let r = parse_request(&v).unwrap();
        assert_eq!(r.system[0].text, "be brief");
        assert_eq!(r.messages.len(), 3);
        assert_eq!(r.stop, vec!["END"]);
        assert!(matches!(&r.messages[2].content[0], Part::ToolResult { call_id, .. } if call_id == "c1"));
    }

    #[test]
    fn stream_ends_with_usage_and_done() {
        let mut enc = ChatEncoder::new("chatcmpl-1".into(), "m".into(), true);
        let mut out = enc.start();
        out.extend(enc.event(&StreamEvent::TextDelta { index: 0, text: "hi".into() }));
        out.extend(enc.event(&StreamEvent::Finish { stop_reason: StopReason::EndTurn, usage: Usage { input_tokens: 2, output_tokens: 1, ..Default::default() } }));
        let s: String = out.iter().map(|b| String::from_utf8_lossy(b).into_owned()).collect();
        assert!(s.contains("\"content\":\"hi\""));
        assert!(s.contains("\"finish_reason\":\"stop\""));
        assert!(s.contains("\"prompt_tokens\":2"));
        assert!(s.ends_with("data: [DONE]\n\n"));
    }
}
