//! Inbound OpenAI Responses API: request parsing and response/stream rendering.

use std::collections::HashMap;

use bytes::Bytes;
use serde_json::{Map, Value, json};

use super::{StreamEncoder, sse_event};
use crate::{
    error::GatewayError,
    ir::{
        BlockKind, ChatRequest, ChatResponse, Effort, ImageSource, Location, Message, Part, Reasoning, Role, StopReason,
        StreamEvent, TextBlock, Tool, ToolChoice, Usage, WebSearch, strings,
    },
    util::prefixed_id,
};

fn parse_content(content: &Value) -> Result<Vec<Part>, GatewayError> {
    let items = match content {
        Value::String(s) => return Ok(vec![Part::Text { text: s.clone(), cache: false }]),
        Value::Array(a) => a,
        Value::Null => return Ok(vec![]),
        _ => return Err(GatewayError::invalid("content must be a string or an array")),
    };
    let mut out = Vec::new();
    for c in items {
        match c.get("type").and_then(Value::as_str).unwrap_or("") {
            "input_text" | "output_text" | "text" | "summary_text" => {
                out.push(Part::Text { text: c["text"].as_str().unwrap_or_default().to_string(), cache: false })
            }
            "refusal" => out.push(Part::Text { text: c["refusal"].as_str().unwrap_or_default().to_string(), cache: false }),
            "input_image" => {
                let url = c
                    .get("image_url")
                    .and_then(|u| u.as_str().or_else(|| u.get("url").and_then(Value::as_str)))
                    .ok_or_else(|| GatewayError::invalid("input_image requires image_url (file_id is not supported)"))?;
                out.push(Part::Image { source: ImageSource::from_url(url), cache: false });
            }
            "input_file" => {
                let data = c.get("file_data").and_then(Value::as_str).ok_or_else(|| {
                    GatewayError::invalid("input_file requires file_data (file_id and file_url are not supported)")
                })?;
                let part = Part::file_from_data_url(data, c.get("filename").and_then(Value::as_str))
                    .ok_or_else(|| GatewayError::invalid("input_file file_data must be a base64 data URL"))?;
                out.push(part);
            }
            other => {
                return Err(GatewayError::invalid(format!("content type '{other}' is not supported for this model's provider")));
            }
        }
    }
    Ok(out)
}

/// Parses one request's own input. `previous_response_id` history is merged by the pipeline.
pub fn parse_request(v: &Value) -> Result<ChatRequest, GatewayError> {
    let mut req = ChatRequest::default();
    // Must stay the first system block: see `input_system`.
    if let Some(i) = v.get("instructions").and_then(Value::as_str) {
        req.system.push(TextBlock { text: i.to_string(), cache: false });
    }
    match v.get("input") {
        Some(Value::String(s)) => {
            req.messages.push(Message { role: Role::User, content: vec![Part::Text { text: s.clone(), cache: false }] })
        }
        Some(Value::Array(items)) => {
            for it in items {
                let ty = it.get("type").and_then(Value::as_str).unwrap_or("message");
                match ty {
                    "message" => {
                        let content = parse_content(it.get("content").unwrap_or(&Value::Null))?;
                        match it.get("role").and_then(Value::as_str) {
                            Some("user") => req.messages.push(Message { role: Role::User, content }),
                            Some("assistant") => req.messages.push(Message { role: Role::Assistant, content }),
                            Some("system" | "developer") => {
                                req.system.push(TextBlock { text: crate::ir::parts_text(&content), cache: false })
                            }
                            _ => return Err(GatewayError::invalid("invalid message role")),
                        }
                    }
                    "function_call" => req.messages.push(Message {
                        role: Role::Assistant,
                        content: vec![Part::ToolCall {
                            id: it["call_id"].as_str().unwrap_or_default().to_string(),
                            name: it["name"].as_str().unwrap_or_default().to_string(),
                            arguments: it["arguments"].as_str().unwrap_or("{}").to_string(),
                        }],
                    }),
                    "function_call_output" => {
                        let content = match &it["output"] {
                            Value::String(s) => vec![Part::Text { text: s.clone(), cache: false }],
                            other => parse_content(other)?,
                        };
                        req.messages.push(Message {
                            role: Role::User,
                            content: vec![Part::ToolResult {
                                call_id: it["call_id"].as_str().unwrap_or_default().to_string(),
                                content,
                                is_error: false,
                                cache: false,
                            }],
                        });
                    }
                    "reasoning" => {
                        let text = it
                            .get("summary")
                            .and_then(Value::as_array)
                            .map(|s| s.iter().filter_map(|x| x["text"].as_str()).collect::<Vec<_>>().join("\n"))
                            .unwrap_or_default();
                        let signature = it.get("encrypted_content").and_then(Value::as_str).map(str::to_string);
                        req.messages.push(Message { role: Role::Assistant, content: vec![Part::Reasoning { text, signature }] });
                    }
                    // A web search the provider ran in an earlier turn: its answer is in the message that follows.
                    "web_search_call" => {}
                    other => {
                        return Err(GatewayError::invalid(format!(
                            "input item type '{other}' is not supported for this model's provider"
                        )));
                    }
                }
            }
        }
        _ => return Err(GatewayError::invalid("input is required")),
    }
    if let Some(tools) = v.get("tools").and_then(Value::as_array) {
        for t in tools {
            let ty = t.get("type").and_then(Value::as_str).unwrap_or("function");
            // `web_search`, `web_search_preview` and their dated versions.
            if ty.starts_with("web_search") {
                req.web_search = Some(WebSearch {
                    allowed_domains: strings(t.pointer("/filters/allowed_domains")),
                    context_size: t.get("search_context_size").and_then(Value::as_str).map(str::to_string),
                    location: t.get("user_location").and_then(Location::parse),
                    ..Default::default()
                });
                continue;
            }
            if ty != "function" {
                return Err(GatewayError::invalid(format!("built-in tool '{ty}' is not supported for this model's provider")));
            }
            req.tools.push(Tool {
                name: t["name"].as_str().unwrap_or_default().to_string(),
                description: t.get("description").and_then(Value::as_str).map(str::to_string),
                parameters: t.get("parameters").cloned().filter(|p| !p.is_null()).unwrap_or_else(|| json!({ "type": "object" })),
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
        Some(o @ Value::Object(_)) => o.get("name").and_then(Value::as_str).map(|n| ToolChoice::Tool(n.to_string())),
        _ => None,
    };
    req.max_tokens = v.get("max_output_tokens").and_then(Value::as_u64).map(|n| n as u32);
    req.temperature = v.get("temperature").and_then(Value::as_f64);
    req.top_p = v.get("top_p").and_then(Value::as_f64);
    if let Some(r) = v.get("reasoning").filter(|r| r.is_object()) {
        req.reasoning = r.get("effort").and_then(Value::as_str).and_then(Effort::parse).map(Reasoning::Effort);
        req.reasoning_summary = r.get("summary").is_some_and(|s| !s.is_null());
    }
    if let Some(fmt) = v.pointer("/text/format")
        && fmt.get("type").and_then(Value::as_str) == Some("json_schema")
    {
        let mut s = fmt.clone();
        if let Some(o) = s.as_object_mut() {
            o.remove("type");
        }
        req.json_schema = Some(s);
    }
    req.user = v
        .get("safety_identifier")
        .or_else(|| v.get("user"))
        .and_then(Value::as_str)
        .map(str::to_string);
    req.normalize();
    Ok(req)
}

/// System/developer messages that came from `input` (as opposed to `instructions`). These are
/// part of the conversation and carry over to `previous_response_id` continuations.
pub fn input_system(req: &ChatRequest, raw: &Value) -> Vec<TextBlock> {
    let instructions = usize::from(raw.get("instructions").and_then(Value::as_str).is_some());
    req.system.iter().skip(instructions).cloned().collect()
}

pub fn usage_json(u: &Usage) -> Value {
    json!({
        "input_tokens": u.total_input(),
        "input_tokens_details": { "cached_tokens": u.cache_read_tokens },
        "output_tokens": u.output_tokens,
        "output_tokens_details": { "reasoning_tokens": u.reasoning_tokens },
        "total_tokens": u.total(),
    })
}

fn status_of(s: StopReason) -> (&'static str, Value) {
    match s {
        StopReason::MaxTokens => ("incomplete", json!({ "reason": "max_output_tokens" })),
        StopReason::ContentFilter => ("incomplete", json!({ "reason": "content_filter" })),
        _ => ("completed", Value::Null),
    }
}

fn envelope(id: &str, model: &str, created: i64, status: &str, output: Vec<Value>, usage: Value, incomplete: Value) -> Value {
    json!({
        "id": id,
        "object": "response",
        "created_at": created,
        "status": status,
        "error": null,
        "incomplete_details": incomplete,
        "instructions": null,
        "model": model,
        "output": output,
        "parallel_tool_calls": true,
        "tool_choice": "auto",
        "tools": [],
        "temperature": null,
        "top_p": null,
        "metadata": {},
        "usage": usage,
    })
}

fn reasoning_item(id: &str, text: &str, signature: Option<&str>) -> Value {
    let mut item = json!({
        "type": "reasoning", "id": id,
        "summary": if text.is_empty() { json!([]) } else { json!([{ "type": "summary_text", "text": text }]) },
    });
    if let Some(sig) = signature {
        item["encrypted_content"] = sig.into();
    }
    item
}

fn message_item(id: &str, text: &str, status: &str) -> Value {
    json!({
        "type": "message", "id": id, "status": status, "role": "assistant",
        "content": [{ "type": "output_text", "text": text, "annotations": [] }],
    })
}

fn call_item(id: &str, call_id: &str, name: &str, args: &str, status: &str) -> Value {
    json!({ "type": "function_call", "id": id, "call_id": call_id, "name": name, "arguments": args, "status": status })
}

pub fn render_response(r: &ChatResponse, id: &str, model: &str) -> Value {
    let output: Vec<Value> = r
        .content
        .iter()
        .filter_map(|p| match p {
            Part::Text { text, .. } => Some(message_item(&prefixed_id("msg"), text, "completed")),
            Part::ToolCall { id, name, arguments } => Some(call_item(&prefixed_id("fc"), id, name, arguments, "completed")),
            Part::Reasoning { text, signature } => Some(reasoning_item(&prefixed_id("rs"), text, signature.as_deref())),
            _ => None,
        })
        .collect();
    let (status, incomplete) = status_of(r.stop_reason);
    envelope(id, model, chrono::Utc::now().timestamp(), status, output, usage_json(&r.usage), incomplete)
}

struct Item {
    output_index: usize,
    id: String,
    kind: BlockKind,
    buf: String,
    signature: Option<String>,
    summary_open: bool,
}

pub struct ResponsesEncoder {
    id: String,
    model: String,
    created: i64,
    seq: u64,
    items: HashMap<usize, Item>,
    next_output: usize,
    done: Vec<(usize, Value)>,
}

impl ResponsesEncoder {
    pub fn new(id: String, model: String) -> Self {
        Self {
            id,
            model,
            created: chrono::Utc::now().timestamp(),
            seq: 0,
            items: HashMap::new(),
            next_output: 0,
            done: Vec::new(),
        }
    }

    fn emit(&mut self, ty: &str, mut data: Value) -> Bytes {
        if let Some(o) = data.as_object_mut() {
            o.insert("type".into(), ty.into());
            o.insert("sequence_number".into(), self.seq.into());
        }
        self.seq += 1;
        sse_event(ty, &data)
    }

    fn output(&self) -> Vec<Value> {
        let mut done = self.done.clone();
        done.sort_by_key(|(i, _)| *i);
        done.into_iter().map(|(_, v)| v).collect()
    }
}

impl StreamEncoder for ResponsesEncoder {
    fn start(&mut self) -> Vec<Bytes> {
        let resp = envelope(&self.id, &self.model, self.created, "in_progress", vec![], Value::Null, Value::Null);
        vec![
            self.emit("response.created", json!({ "response": resp.clone() })),
            self.emit("response.in_progress", json!({ "response": resp })),
        ]
    }

    fn event(&mut self, ev: &StreamEvent) -> Vec<Bytes> {
        let mut out = Vec::new();
        match ev {
            StreamEvent::BlockStart { index, kind } => {
                let output_index = self.next_output;
                self.next_output += 1;
                let (id, item) = match kind {
                    BlockKind::Text => {
                        let id = prefixed_id("msg");
                        let item = json!({ "type": "message", "id": id, "status": "in_progress", "role": "assistant", "content": [] });
                        (id, item)
                    }
                    BlockKind::ToolCall { id: call_id, name } => {
                        let id = prefixed_id("fc");
                        let item = call_item(&id, call_id, name, "", "in_progress");
                        (id, item)
                    }
                    BlockKind::Reasoning => {
                        let id = prefixed_id("rs");
                        let item = json!({ "type": "reasoning", "id": id, "summary": [] });
                        (id, item)
                    }
                };
                out.push(self.emit("response.output_item.added", json!({ "output_index": output_index, "item": item })));
                if matches!(kind, BlockKind::Text) {
                    out.push(self.emit(
                        "response.content_part.added",
                        json!({
                            "item_id": id, "output_index": output_index, "content_index": 0,
                            "part": { "type": "output_text", "text": "", "annotations": [] }
                        }),
                    ));
                }
                self.items.insert(
                    *index,
                    Item { output_index, id, kind: kind.clone(), buf: String::new(), signature: None, summary_open: false },
                );
            }
            StreamEvent::TextDelta { index, text } => {
                if let Some(it) = self.items.get_mut(index) {
                    it.buf.push_str(text);
                    let (id, oi) = (it.id.clone(), it.output_index);
                    out.push(self.emit(
                        "response.output_text.delta",
                        json!({ "item_id": id, "output_index": oi, "content_index": 0, "delta": text, "logprobs": [] }),
                    ));
                }
            }
            StreamEvent::ToolArgsDelta { index, json: delta } => {
                if let Some(it) = self.items.get_mut(index) {
                    it.buf.push_str(delta);
                    let (id, oi) = (it.id.clone(), it.output_index);
                    out.push(self.emit(
                        "response.function_call_arguments.delta",
                        json!({ "item_id": id, "output_index": oi, "delta": delta }),
                    ));
                }
            }
            StreamEvent::ReasoningDelta { index, text } => {
                if let Some(it) = self.items.get_mut(index) {
                    it.buf.push_str(text);
                    let open = std::mem::replace(&mut it.summary_open, true);
                    let (id, oi) = (it.id.clone(), it.output_index);
                    if !open {
                        out.push(self.emit(
                            "response.reasoning_summary_part.added",
                            json!({ "item_id": id, "output_index": oi, "summary_index": 0, "part": { "type": "summary_text", "text": "" } }),
                        ));
                    }
                    out.push(self.emit(
                        "response.reasoning_summary_text.delta",
                        json!({ "item_id": id, "output_index": oi, "summary_index": 0, "delta": text }),
                    ));
                }
            }
            StreamEvent::SignatureDelta { index, signature } => {
                if let Some(it) = self.items.get_mut(index) {
                    it.signature.get_or_insert_with(String::new).push_str(signature);
                }
            }
            // The provider's own tools and citations: logged, never sent (see `StreamEvent::ServerTool`).
            StreamEvent::ServerTool { .. } | StreamEvent::Annotation { .. } => {}
            StreamEvent::BlockStop { index } => {
                let Some(it) = self.items.remove(index) else { return out };
                let (id, oi) = (it.id.clone(), it.output_index);
                let item = match &it.kind {
                    BlockKind::Text => {
                        out.push(self.emit(
                            "response.output_text.done",
                            json!({ "item_id": id, "output_index": oi, "content_index": 0, "text": it.buf, "logprobs": [] }),
                        ));
                        out.push(self.emit(
                            "response.content_part.done",
                            json!({
                                "item_id": id, "output_index": oi, "content_index": 0,
                                "part": { "type": "output_text", "text": it.buf, "annotations": [] }
                            }),
                        ));
                        message_item(&id, &it.buf, "completed")
                    }
                    BlockKind::ToolCall { id: call_id, name } => {
                        let args = if it.buf.is_empty() { "{}".to_string() } else { it.buf.clone() };
                        out.push(self.emit(
                            "response.function_call_arguments.done",
                            json!({ "item_id": id, "output_index": oi, "arguments": args }),
                        ));
                        call_item(&id, call_id, name, &args, "completed")
                    }
                    BlockKind::Reasoning => {
                        if it.summary_open {
                            out.push(self.emit(
                                "response.reasoning_summary_text.done",
                                json!({ "item_id": id, "output_index": oi, "summary_index": 0, "text": it.buf }),
                            ));
                            out.push(self.emit(
                                "response.reasoning_summary_part.done",
                                json!({ "item_id": id, "output_index": oi, "summary_index": 0, "part": { "type": "summary_text", "text": it.buf } }),
                            ));
                        }
                        reasoning_item(&id, &it.buf, it.signature.as_deref())
                    }
                };
                out.push(self.emit("response.output_item.done", json!({ "output_index": oi, "item": item.clone() })));
                self.done.push((oi, item));
            }
            StreamEvent::Finish { stop_reason, usage } => {
                let (status, incomplete) = status_of(*stop_reason);
                let resp = envelope(&self.id, &self.model, self.created, status, self.output(), usage_json(usage), incomplete);
                let ty = if status == "completed" { "response.completed" } else { "response.incomplete" };
                out.push(self.emit(ty, json!({ "response": resp })));
            }
        }
        out
    }

    fn error(&mut self, e: &GatewayError) -> Vec<Bytes> {
        let mut resp = envelope(&self.id, &self.model, self.created, "failed", self.output(), Value::Null, Value::Null);
        resp["error"] = json!({ "code": e.openai_type(), "message": e.message });
        let mut err = Map::new();
        err.insert("code".into(), e.openai_type().into());
        err.insert("message".into(), e.message.clone().into());
        err.insert("param".into(), Value::Null);
        vec![self.emit("error", Value::Object(err)), self.emit("response.failed", json!({ "response": resp }))]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_items() {
        let v = json!({
            "model": "m", "instructions": "be nice",
            "input": [
                {"role": "user", "content": "hi"},
                {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}"},
                {"type": "function_call", "call_id": "c2", "name": "f", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c1", "output": "a"},
                {"type": "function_call_output", "call_id": "c2", "output": "b"}
            ],
            "tools": [{"type": "function", "name": "f", "parameters": {"type": "object"}}],
            "reasoning": {"effort": "low", "summary": "auto"}
        });
        let r = parse_request(&v).unwrap();
        assert_eq!(r.system[0].text, "be nice");
        // consecutive tool calls / outputs are merged into single turns
        assert_eq!(r.messages.len(), 3);
        assert_eq!(r.messages[1].content.len(), 2);
        assert!(r.reasoning_summary);
    }

    #[test]
    fn stream_encoder_emits_completed() {
        let mut enc = ResponsesEncoder::new("resp_1".into(), "m".into());
        let mut all = enc.start();
        for ev in [
            StreamEvent::BlockStart { index: 0, kind: BlockKind::Text },
            StreamEvent::TextDelta { index: 0, text: "hi".into() },
            StreamEvent::BlockStop { index: 0 },
            StreamEvent::Finish { stop_reason: StopReason::EndTurn, usage: Usage { input_tokens: 3, output_tokens: 1, ..Default::default() } },
        ] {
            all.extend(enc.event(&ev));
        }
        let text: String = all.iter().map(|b| String::from_utf8_lossy(b).into_owned()).collect();
        assert!(text.contains("event: response.output_text.delta"));
        assert!(text.contains("event: response.output_item.done"));
        assert!(text.contains("\"text\":\"hi\""));
        assert!(text.contains("event: response.completed"));
    }
}
