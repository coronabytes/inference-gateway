//! OpenAI Chat Completions upstream (OpenAI, Azure OpenAI, vLLM, Ollama, ...).

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use super::{EventResult, EventStream, Op, Sse, SseHandler};
use crate::{
    error::{ErrorKind, GatewayError},
    ir::{BlockKind, BlockTracker, ChatRequest, Part, Role, StopReason, StreamEvent, ToolChoice, Usage},
    model::Deployment,
    state::AppState,
};

pub async fn send(state: &AppState, d: &Deployment, req: &ChatRequest) -> Result<EventStream, GatewayError> {
    let mut body = build_body(req, d);
    super::prepare_body(d, Op::ChatCompletions, &mut body);
    let url = super::url(state, d, Op::ChatCompletions).await?;
    let resp = super::post(state, d, &url, super::headers(d)?, super::json_bytes(&body)).await?;
    Ok(super::decode_stream(super::body_stream(resp), Sse::new(ChatHandler::default())))
}

pub fn build_body(req: &ChatRequest, d: &Deployment) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    if !req.system.is_empty() {
        let text = req.system.iter().map(|b| b.text.as_str()).collect::<Vec<_>>().join("\n\n");
        messages.push(json!({ "role": "system", "content": text }));
    }
    for m in &req.messages {
        match m.role {
            Role::User => {
                let mut content = Vec::new();
                for p in &m.content {
                    match p {
                        Part::ToolResult { call_id, content: inner, is_error, .. } => {
                            let mut text = crate::ir::parts_text(inner);
                            if *is_error && !text.starts_with("Error") {
                                text = format!("Error: {text}");
                            }
                            messages.push(json!({ "role": "tool", "tool_call_id": call_id, "content": text }));
                        }
                        Part::Text { text, .. } => content.push(json!({ "type": "text", "text": text })),
                        Part::Image { source, .. } => {
                            content.push(json!({ "type": "image_url", "image_url": { "url": source.to_url() } }))
                        }
                        Part::File { media_type, data, filename, .. } => content.push(json!({
                            "type": "file",
                            "file": {
                                "filename": crate::ir::file_name(filename, media_type),
                                "file_data": format!("data:{media_type};base64,{data}")
                            }
                        })),
                        _ => {}
                    }
                }
                if content.is_empty() {
                    continue;
                }
                let content = match content.as_slice() {
                    [one] if one["type"] == "text" => one["text"].clone(),
                    _ => Value::Array(content),
                };
                messages.push(json!({ "role": "user", "content": content }));
            }
            Role::Assistant => {
                let mut text = String::new();
                let mut calls = Vec::new();
                for p in &m.content {
                    match p {
                        Part::Text { text: t, .. } => text.push_str(t),
                        Part::ToolCall { id, name, arguments } => calls.push(json!({
                            "id": id, "type": "function",
                            "function": { "name": name, "arguments": if arguments.is_empty() { "{}" } else { arguments } }
                        })),
                        _ => {}
                    }
                }
                let mut msg = json!({ "role": "assistant", "content": if text.is_empty() { Value::Null } else { text.into() } });
                if !calls.is_empty() {
                    msg["tool_calls"] = Value::Array(calls);
                }
                messages.push(msg);
            }
        }
    }

    let mut body = Map::new();
    body.insert("model".into(), d.upstream_model.clone().into());
    body.insert("messages".into(), Value::Array(messages));
    body.insert("stream".into(), true.into());
    body.insert("stream_options".into(), json!({ "include_usage": true }));
    if let Some(mt) = req.max_tokens.or(d.config.default_max_tokens) {
        let field = d.config.max_tokens_field.as_deref().unwrap_or("max_completion_tokens");
        body.insert(field.into(), mt.into());
    }
    if let Some(t) = req.temperature {
        body.insert("temperature".into(), t.into());
    }
    if let Some(p) = req.top_p {
        body.insert("top_p".into(), p.into());
    }
    if !req.stop.is_empty() {
        body.insert("stop".into(), json!(req.stop));
    }
    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                let mut f = Map::new();
                f.insert("name".into(), t.name.clone().into());
                if let Some(desc) = &t.description {
                    f.insert("description".into(), desc.clone().into());
                }
                f.insert("parameters".into(), t.parameters.clone());
                json!({ "type": "function", "function": f })
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
                    ToolChoice::Tool(n) => json!({ "type": "function", "function": { "name": n } }),
                },
            );
        }
    }
    // The nearest effort the model takes: e.g. `minimal` becomes `low` on a model without it.
    if let Some(effort) = req.reasoning.and_then(|r| d.fit_effort(r.effort())) {
        body.insert("reasoning_effort".into(), effort.as_str().into());
    }
    if let Some(schema) = &req.json_schema {
        body.insert("response_format".into(), json!({ "type": "json_schema", "json_schema": schema }));
    }
    // OpenAI's search models and Gemini (as Google Search grounding) take it; it has no domain lists.
    if let Some(ws) = &req.web_search {
        let mut o = Map::new();
        if let Some(size) = &ws.context_size {
            o.insert("search_context_size".into(), size.clone().into());
        }
        if let Some(l) = &ws.location {
            o.insert("user_location".into(), json!({ "type": "approximate", "approximate": l.fields() }));
        }
        body.insert("web_search_options".into(), Value::Object(o));
    }
    if let Some(u) = &req.user {
        body.insert("user".into(), u.clone().into());
    }
    Value::Object(body)
}

pub fn usage(u: &Value) -> Usage {
    let g = |p: &str| u.pointer(p).and_then(Value::as_u64).unwrap_or(0);
    let prompt = g("/prompt_tokens").max(g("/input_tokens"));
    let cached = g("/prompt_tokens_details/cached_tokens").max(g("/input_tokens_details/cached_tokens"));
    Usage {
        input_tokens: prompt.saturating_sub(cached),
        output_tokens: g("/completion_tokens").max(g("/output_tokens")),
        cache_read_tokens: cached,
        cache_write_tokens: 0,
        reasoning_tokens: g("/completion_tokens_details/reasoning_tokens").max(g("/output_tokens_details/reasoning_tokens")),
        search_units: 0,
    }
}

/// Web searches a Chat Completions message (or stream delta) reports. Gemini lists the queries it
/// ran, each billed; OpenAI's search models bill one search per answer, which cites its sources.
pub fn web_searches(m: &Value) -> u64 {
    if let Some(queries) = m.pointer("/extra_properties/google/web_search_queries").and_then(Value::as_array) {
        return queries.len() as u64;
    }
    let cites = m.get("annotations").and_then(Value::as_array).is_some_and(|a| a.iter().any(|x| x["type"] == "url_citation"));
    u64::from(cites)
}

#[derive(Default)]
pub struct ChatHandler {
    blocks: BlockTracker,
    text: Option<usize>,
    reasoning: Option<usize>,
    tools: HashMap<u64, usize>,
    finish: Option<StopReason>,
    usage: Usage,
    /// The most any delta reported (see `web_searches`): the usage chunk carries none.
    searches: u64,
    /// Gemini's search grounding as last passed on, so a repeat is not passed on again.
    google: Option<Value>,
    done: bool,
}

impl ChatHandler {
    fn close_text(&mut self, evs: &mut Vec<StreamEvent>) {
        if let Some(i) = self.text.take() {
            self.blocks.stop(evs, i);
        }
        if let Some(i) = self.reasoning.take() {
            self.blocks.stop(evs, i);
        }
    }

    fn reasoning_delta(&mut self, evs: &mut Vec<StreamEvent>, text: &str) {
        if text.is_empty() {
            return;
        }
        let i = match self.reasoning {
            Some(i) => i,
            None => {
                let i = self.blocks.start(evs, BlockKind::Reasoning);
                self.reasoning = Some(i);
                i
            }
        };
        evs.push(StreamEvent::ReasoningDelta { index: i, text: text.to_string() });
    }

    /// Answer text ends the reasoning before it.
    fn text_delta(&mut self, evs: &mut Vec<StreamEvent>, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(i) = self.reasoning.take() {
            self.blocks.stop(evs, i);
        }
        let i = match self.text {
            Some(i) => i,
            None => {
                let i = self.blocks.start(evs, BlockKind::Text);
                self.text = Some(i);
                i
            }
        };
        evs.push(StreamEvent::TextDelta { index: i, text: text.to_string() });
    }

    fn finish(&mut self, out: &mut Vec<EventResult>) {
        if self.done {
            return;
        }
        self.done = true;
        let mut evs = Vec::new();
        self.blocks.stop_all(&mut evs);
        let mut stop = self.finish.unwrap_or_default();
        if stop == StopReason::EndTurn && !self.tools.is_empty() {
            stop = StopReason::ToolUse;
        }
        let usage = Usage { search_units: self.searches, ..self.usage };
        evs.push(StreamEvent::Finish { stop_reason: stop, usage });
        out.extend(evs.into_iter().map(Ok));
    }
}

impl SseHandler for ChatHandler {
    fn on_event(&mut self, _event: Option<&str>, data: &str, out: &mut Vec<EventResult>) {
        if self.done {
            return;
        }
        if data.trim() == "[DONE]" {
            self.finish(out);
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else { return };
        if let Some(err) = v.get("error") {
            let msg = err.get("message").and_then(Value::as_str).unwrap_or("stream error");
            out.push(Err(GatewayError::upstream_stream(ErrorKind::Upstream, msg)));
            return;
        }
        if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            self.usage = usage(u);
        }
        let mut evs = Vec::new();
        if let Some(choice) = v.get("choices").and_then(|c| c.get(0)) {
            let delta = &choice["delta"];
            self.searches = self.searches.max(web_searches(delta));
            // What the IR has no place for: citations (OpenAI's search models) and Gemini's search
            // queries and sources (see `StreamEvent::ServerTool`).
            for a in delta["annotations"].as_array().into_iter().flatten() {
                evs.push(StreamEvent::Annotation { annotation: a.clone() });
            }
            if let Some(g) = delta.pointer("/extra_properties/google").filter(|g| self.google.as_ref() != Some(*g)) {
                self.google = Some(g.clone());
                evs.push(StreamEvent::ServerTool { block: json!({ "type": "google_search", "google": g }) });
            }
            let reasoning = delta.get("reasoning_content").or_else(|| delta.get("reasoning")).and_then(Value::as_str);
            if let Some(r) = reasoning {
                self.reasoning_delta(&mut evs, r);
            }
            match delta.get("content") {
                Some(Value::String(t)) => self.text_delta(&mut evs, t),
                // Mistral's reasoning models send parts: `thinking` (its text in parts of its own)
                // and `text`, both of which one chunk can hold as the reasoning ends.
                Some(Value::Array(parts)) => {
                    for part in parts {
                        match part.get("type").and_then(Value::as_str) {
                            Some("thinking") => match &part["thinking"] {
                                Value::String(r) => self.reasoning_delta(&mut evs, r),
                                Value::Array(rs) => {
                                    for r in rs.iter().filter_map(|r| r.get("text").and_then(Value::as_str)) {
                                        self.reasoning_delta(&mut evs, r);
                                    }
                                }
                                _ => {}
                            },
                            Some("text") => self.text_delta(&mut evs, part["text"].as_str().unwrap_or_default()),
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            if let Some(t) = delta.get("refusal").and_then(Value::as_str) {
                self.text_delta(&mut evs, t);
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for (n, tc) in calls.iter().enumerate() {
                    let key = tc.get("index").and_then(Value::as_u64).unwrap_or(n as u64);
                    let i = match self.tools.get(&key) {
                        Some(&i) => i,
                        None => {
                            self.close_text(&mut evs);
                            let id = tc
                                .get("id")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                                .unwrap_or_else(|| crate::util::prefixed_id("call"));
                            let name = tc.pointer("/function/name").and_then(Value::as_str).unwrap_or_default().to_string();
                            let i = self.blocks.start(&mut evs, BlockKind::ToolCall { id, name });
                            self.tools.insert(key, i);
                            i
                        }
                    };
                    if let Some(a) = tc.pointer("/function/arguments").and_then(Value::as_str).filter(|a| !a.is_empty()) {
                        evs.push(StreamEvent::ToolArgsDelta { index: i, json: a.to_string() });
                    }
                }
            }
            if let Some(fr) = choice.get("finish_reason").and_then(Value::as_str) {
                self.finish = Some(match fr {
                    "length" => StopReason::MaxTokens,
                    "tool_calls" | "function_call" => StopReason::ToolUse,
                    "content_filter" => StopReason::ContentFilter,
                    _ => StopReason::EndTurn,
                });
            }
        }
        out.extend(evs.into_iter().map(Ok));
    }

    fn on_end(&mut self, out: &mut Vec<EventResult>) {
        // Some OpenAI-compatible servers close the stream without `[DONE]`.
        if self.finish.is_some() {
            self.finish(out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::Decoder;

    /// Citations and Gemini's search grounding reach the aggregate beside the answer, the
    /// grounding once however often Gemini repeats it.
    #[test]
    fn citations_and_search_grounding_are_passed_on_beside_the_answer() {
        let google = json!({ "web_search_queries": ["rome weather"] });
        let events = [
            json!({ "choices": [{ "index": 0, "delta": { "content": "Sunny.", "extra_properties": { "google": google } } }] }),
            json!({ "choices": [{ "index": 0, "delta": { "content": "", "extra_properties": { "google": google },
                "annotations": [{ "type": "url_citation", "url_citation": { "url": "https://example.com" } }] }, "finish_reason": "stop" }] }),
        ];
        let mut sse: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
        sse.push_str("data: [DONE]\n\n");
        let mut dec = Sse::new(ChatHandler::default());
        let mut out = Vec::new();
        dec.feed(sse.as_bytes(), &mut out);
        let mut agg = crate::ir::Aggregator::default();
        out.iter().for_each(|e| agg.push(e.as_ref().unwrap()));
        let r = agg.finish();
        assert!(matches!(&r.content[..], [Part::Text { text, .. }] if text == "Sunny."));
        assert_eq!(r.server_tools, [json!({ "type": "google_search", "google": google })]);
        assert_eq!(r.annotations.len(), 1);
    }

    #[test]
    fn decodes_text_and_tool_calls() {
        let sse = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hel\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"f\",\"arguments\":\"{\\\"x\\\"\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\":2}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":5,\"prompt_tokens_details\":{\"cached_tokens\":4}}}\n\n",
            "data: [DONE]\n\n",
        );
        let mut dec = Sse::new(ChatHandler::default());
        let mut out = Vec::new();
        dec.feed(sse.as_bytes(), &mut out);
        let mut agg = crate::ir::Aggregator::default();
        out.iter().for_each(|e| agg.push(e.as_ref().unwrap()));
        let r = agg.finish();
        assert!(matches!(&r.content[0], Part::Text { text, .. } if text == "Hello"));
        assert!(matches!(&r.content[1], Part::ToolCall { arguments, id, .. } if arguments == "{\"x\":2}" && id == "call_1"));
        assert_eq!(r.stop_reason, StopReason::ToolUse);
        assert_eq!(r.usage.input_tokens, 16);
        assert_eq!(r.usage.cache_read_tokens, 4);
    }

    #[test]
    fn counts_web_searches() {
        // Gemini: the queries it ran, in a delta before the citations.
        let sse = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Mild.\",\"extra_properties\":{\"google\":{\"web_search_queries\":[\"a\",\"b\"]}}}}]}

",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"\",\"annotations\":[{\"type\":\"url_citation\"}]},\"finish_reason\":\"stop\"}]}

",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":5}}

",
            "data: [DONE]

",
        );
        let mut dec = Sse::new(ChatHandler::default());
        let mut out = Vec::new();
        dec.feed(sse.as_bytes(), &mut out);
        let mut agg = crate::ir::Aggregator::default();
        out.iter().for_each(|e| agg.push(e.as_ref().unwrap()));
        assert_eq!(agg.finish().usage.search_units, 2);
        // OpenAI's search models: one search for an answer with citations.
        assert_eq!(web_searches(&json!({ "annotations": [{ "type": "url_citation" }] })), 1);
        assert_eq!(web_searches(&json!({ "content": "no search" })), 0);
    }

    #[test]
    fn decodes_mistral_thinking_parts() {
        // The last of the reasoning and the first of the answer can come in one chunk.
        let sse = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":[{\"type\":\"thinking\",\"thinking\":[{\"type\":\"text\",\"text\":\"Easy\"}]}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":[{\"type\":\"thinking\",\"thinking\":[{\"type\":\"text\",\"text\":\".\"}]},{\"type\":\"text\",\"text\":\"{\\\"\"}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"a\\\": 1}\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let mut dec = Sse::new(ChatHandler::default());
        let mut out = Vec::new();
        dec.feed(sse.as_bytes(), &mut out);
        let mut agg = crate::ir::Aggregator::default();
        out.iter().for_each(|e| agg.push(e.as_ref().unwrap()));
        let r = agg.finish();
        assert!(matches!(&r.content[0], Part::Reasoning { text, .. } if text == "Easy."), "{:?}", r.content);
        assert!(matches!(&r.content[1], Part::Text { text, .. } if text == "{\"a\": 1}"), "{:?}", r.content);
    }
}
