//! OpenAI Responses upstream (OpenAI, Azure OpenAI), used for translated requests.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use super::{EventResult, EventStream, Op, Sse, SseHandler};
use crate::{
    error::{ErrorKind, GatewayError},
    ir::{BlockKind, BlockTracker, ChatRequest, Part, Role, StopReason, StreamEvent, ToolChoice},
    model::Deployment,
    state::AppState,
};

pub async fn send(state: &AppState, d: &Deployment, req: &ChatRequest) -> Result<EventStream, GatewayError> {
    let body = build_body(req, d);
    let url = super::url(state, d, Op::Responses).await?;
    let resp = super::post(state, d, &url, super::headers(d)?, super::json_bytes(&body)).await?;
    Ok(super::decode_stream(super::body_stream(resp), Sse::new(ResponsesHandler::default())))
}

pub fn build_body(req: &ChatRequest, d: &Deployment) -> Value {
    let mut input: Vec<Value> = Vec::new();
    for m in &req.messages {
        let mut content: Vec<Value> = Vec::new();
        let flush = |content: &mut Vec<Value>, input: &mut Vec<Value>| {
            if !content.is_empty() {
                let role = if m.role == Role::User { "user" } else { "assistant" };
                input.push(json!({ "type": "message", "role": role, "content": std::mem::take(content) }));
            }
        };
        for p in &m.content {
            match (m.role, p) {
                (Role::User, Part::Text { text, .. }) => content.push(json!({ "type": "input_text", "text": text })),
                (Role::User, Part::Image { source, .. }) => {
                    content.push(json!({ "type": "input_image", "image_url": source.to_url() }))
                }
                (Role::User, Part::File { media_type, data, filename, .. }) => content.push(json!({
                    "type": "input_file",
                    "filename": crate::ir::file_name(filename, media_type),
                    "file_data": format!("data:{media_type};base64,{data}")
                })),
                (Role::Assistant, Part::Text { text, .. }) => {
                    content.push(json!({ "type": "output_text", "text": text, "annotations": [] }))
                }
                (_, Part::ToolCall { id, name, arguments }) => {
                    flush(&mut content, &mut input);
                    input.push(json!({
                        "type": "function_call", "call_id": id, "name": name,
                        "arguments": if arguments.is_empty() { "{}" } else { arguments }
                    }));
                }
                (_, Part::ToolResult { call_id, content: inner, .. }) => {
                    flush(&mut content, &mut input);
                    input.push(json!({
                        "type": "function_call_output", "call_id": call_id,
                        "output": crate::ir::parts_text(inner)
                    }));
                }
                _ => {}
            }
        }
        flush(&mut content, &mut input);
    }

    let mut body = Map::new();
    body.insert("model".into(), d.upstream_model.clone().into());
    body.insert("input".into(), Value::Array(input));
    body.insert("stream".into(), true.into());
    body.insert("store".into(), false.into());
    if !req.system.is_empty() {
        let text = req.system.iter().map(|b| b.text.as_str()).collect::<Vec<_>>().join("\n\n");
        body.insert("instructions".into(), text.into());
    }
    if let Some(mt) = req.max_tokens.or(d.config.default_max_tokens) {
        body.insert("max_output_tokens".into(), mt.into());
    }
    if let Some(t) = req.temperature {
        body.insert("temperature".into(), t.into());
    }
    if let Some(p) = req.top_p {
        body.insert("top_p".into(), p.into());
    }
    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                let mut o = Map::new();
                o.insert("type".into(), "function".into());
                o.insert("name".into(), t.name.clone().into());
                if let Some(desc) = &t.description {
                    o.insert("description".into(), desc.clone().into());
                }
                o.insert("parameters".into(), t.parameters.clone());
                o.insert("strict".into(), false.into());
                Value::Object(o)
            })
            .collect();
        body.insert("tools".into(), Value::Array(tools));
        if let Some(tc) = &req.tool_choice {
            body.insert(
                "tool_choice".into(),
                match tc {
                    ToolChoice::Auto => "auto".into(),
                    ToolChoice::None => "none".into(),
                    ToolChoice::Required => "required".into(),
                    ToolChoice::Tool(n) => json!({ "type": "function", "name": n }),
                },
            );
        }
    }
    // The nearest effort the model takes: e.g. `minimal` becomes `low` on a model without it.
    if let Some(effort) = req.reasoning.and_then(|r| d.fit_effort(r.effort())) {
        let mut reasoning = json!({ "effort": effort.as_str() });
        if req.reasoning_summary {
            reasoning["summary"] = "auto".into();
        }
        body.insert("reasoning".into(), reasoning);
    }
    if let Some(schema) = &req.json_schema {
        let mut format = json!({ "type": "json_schema" });
        if let Some(o) = schema.as_object() {
            for (k, v) in o {
                format[k] = v.clone();
            }
        }
        body.insert("text".into(), json!({ "format": format }));
    }
    if let Some(u) = &req.user {
        body.insert("user".into(), u.clone().into());
    }
    Value::Object(body)
}

#[derive(Default)]
pub struct ResponsesHandler {
    blocks: BlockTracker,
    /// (output_index, content_index) -> text block
    texts: HashMap<(u64, u64), usize>,
    /// output_index -> tool call or reasoning block
    items: HashMap<u64, usize>,
    tool_args_seen: HashMap<u64, bool>,
    saw_tool: bool,
}

impl ResponsesHandler {
    fn finish(&mut self, resp: &Value, out: &mut Vec<StreamEvent>) {
        self.blocks.stop_all(out);
        let usage = resp.get("usage").map(super::openai_chat::usage).unwrap_or_default();
        let reason = resp.pointer("/incomplete_details/reason").and_then(Value::as_str);
        let stop = match reason {
            Some("max_output_tokens") => StopReason::MaxTokens,
            Some("content_filter") => StopReason::ContentFilter,
            _ if self.saw_tool => StopReason::ToolUse,
            _ => StopReason::EndTurn,
        };
        out.push(StreamEvent::Finish { stop_reason: stop, usage });
    }
}

impl SseHandler for ResponsesHandler {
    fn on_event(&mut self, event: Option<&str>, data: &str, out: &mut Vec<EventResult>) {
        let Ok(v) = serde_json::from_str::<Value>(data) else { return };
        let ty = v.get("type").and_then(Value::as_str).or(event).unwrap_or("");
        let oi = v.get("output_index").and_then(Value::as_u64).unwrap_or(0);
        let ci = v.get("content_index").and_then(Value::as_u64).unwrap_or(0);
        let delta = || v.get("delta").and_then(Value::as_str).unwrap_or_default().to_string();
        let mut evs = Vec::new();
        match ty {
            "response.output_item.added" => {
                let item = &v["item"];
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    self.saw_tool = true;
                    let id = item
                        .get("call_id")
                        .or_else(|| item.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let name = item.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
                    let i = self.blocks.start(&mut evs, BlockKind::ToolCall { id, name });
                    self.items.insert(oi, i);
                }
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                let i = match self.texts.get(&(oi, ci)) {
                    Some(&i) => i,
                    None => {
                        let i = self.blocks.start(&mut evs, BlockKind::Text);
                        self.texts.insert((oi, ci), i);
                        i
                    }
                };
                evs.push(StreamEvent::TextDelta { index: i, text: delta() });
            }
            "response.output_text.done" | "response.refusal.done" => {
                if let Some(&i) = self.texts.get(&(oi, ci)) {
                    self.blocks.stop(&mut evs, i);
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(&i) = self.items.get(&oi) {
                    self.tool_args_seen.insert(oi, true);
                    evs.push(StreamEvent::ToolArgsDelta { index: i, json: delta() });
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let i = match self.items.get(&oi) {
                    Some(&i) => i,
                    None => {
                        let i = self.blocks.start(&mut evs, BlockKind::Reasoning);
                        self.items.insert(oi, i);
                        i
                    }
                };
                evs.push(StreamEvent::ReasoningDelta { index: i, text: delta() });
            }
            "response.output_item.done" => {
                let item = &v["item"];
                if let Some(&i) = self.items.get(&oi) {
                    if item.get("type").and_then(Value::as_str) == Some("function_call")
                        && !self.tool_args_seen.get(&oi).copied().unwrap_or(false)
                        && let Some(a) = item.get("arguments").and_then(Value::as_str)
                    {
                        evs.push(StreamEvent::ToolArgsDelta { index: i, json: a.to_string() });
                    }
                    self.blocks.stop(&mut evs, i);
                }
                let texts: Vec<usize> = self.texts.iter().filter(|((o, _), _)| *o == oi).map(|(_, &i)| i).collect();
                for i in texts {
                    self.blocks.stop(&mut evs, i);
                }
            }
            "response.completed" | "response.incomplete" => self.finish(&v["response"], &mut evs),
            "response.failed" => {
                let msg = v.pointer("/response/error/message").and_then(Value::as_str).unwrap_or("response failed");
                out.push(Err(GatewayError::upstream_stream(ErrorKind::Upstream, msg)));
                return;
            }
            "error" => {
                let msg = v
                    .get("message")
                    .or_else(|| v.pointer("/error/message"))
                    .and_then(Value::as_str)
                    .unwrap_or("stream error");
                let kind = match v.get("code").and_then(Value::as_str) {
                    Some("rate_limit_exceeded") => ErrorKind::RateLimit,
                    Some("server_error") | None => ErrorKind::Upstream,
                    _ => ErrorKind::InvalidRequest,
                };
                out.push(Err(GatewayError::upstream_stream(kind, msg)));
                return;
            }
            _ => {}
        }
        out.extend(evs.into_iter().map(Ok));
    }
}
