//! Restoring pseudonyms in the answer (see `crate::pseudonyms`): the model got `Person A` for
//! Alice, and the client gets Alice back. This works on the response as the client gets it, in
//! its own API, whichever upstream served it. A full answer has every string restored; a stream
//! has its text and tool-call argument deltas restored as they arrive, holding back only a tail
//! that may still become a pseudonym, and the rest when its block ends.
//!
//! Reasoning is left as the model wrote it: Anthropic signs thinking blocks, which must come back
//! unchanged, and OpenAI's reasoning is encrypted.

use std::{collections::HashMap, sync::Arc};

use axum::{
    body::Body,
    http::{StatusCode, header},
    response::Response,
};
use bytes::Bytes;
use futures::StreamExt;
use serde_json::{Value, json};

use super::Inbound;
use crate::{
    pseudonyms::{Holdback, Pseudonyms},
    sse::{SseEvent, SseParser},
};

/// The most a full answer may hold to be restored; a larger one passes as it came.
const MAX_BODY: usize = 64 * 1024 * 1024;

/// Fields holding reasoning, and objects of these types, are left as they are.
const REASONING_FIELDS: &[&str] = &["reasoning", "reasoning_content", "thinking", "signature", "encrypted_content"];
const REASONING_TYPES: &[&str] = &["reasoning", "thinking", "redacted_thinking"];

/// `resp` with its pseudonyms restored. Errors pass as they are.
pub(crate) async fn response(resp: Response, inbound: Inbound, p: Arc<Pseudonyms>) -> Response {
    if resp.status() != StatusCode::OK {
        return resp;
    }
    let sse = resp.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|t| t.starts_with("text/event-stream"));
    let (mut parts, body) = resp.into_parts();
    parts.headers.remove(header::CONTENT_LENGTH);
    if sse {
        let state = (body.into_data_stream(), SseParser::default(), Restorer::new(inbound, p), false);
        let stream = futures::stream::unfold(state, |(mut upstream, mut parser, mut restorer, done)| async move {
            if done {
                return None;
            }
            let events: Vec<SseEvent> = match upstream.next().await {
                Some(Ok(chunk)) => parser.feed(&chunk).into_iter().flat_map(|e| restorer.event(e)).collect(),
                // The end, or an upstream that broke off: what is held goes out.
                _ => {
                    let mut events: Vec<SseEvent> = parser.finish().into_iter().flat_map(|e| restorer.event(e)).collect();
                    events.extend(restorer.finish());
                    let out = Ok::<_, std::convert::Infallible>(encode(&events));
                    return Some((out, (upstream, parser, restorer, true)));
                }
            };
            Some((Ok(encode(&events)), (upstream, parser, restorer, false)))
        });
        return Response::from_parts(parts, Body::from_stream(stream));
    }
    let bytes = match axum::body::to_bytes(body, MAX_BODY).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "answer too large to restore pseudonyms in");
            return Response::from_parts(parts, Body::from("{\"error\":{\"message\":\"answer too large\"}}"));
        }
    };
    let restored = match serde_json::from_slice::<Value>(&bytes) {
        Ok(mut v) => {
            restore_strings(&mut v, &p);
            Bytes::from(serde_json::to_vec(&v).unwrap_or_else(|_| bytes.to_vec()))
        }
        Err(_) => bytes,
    };
    Response::from_parts(parts, Body::from(restored))
}

/// Restores every string in `v`, but reasoning.
fn restore_strings(v: &mut Value, p: &Pseudonyms) {
    match v {
        Value::String(s) => *s = p.restore(s),
        Value::Array(items) => items.iter_mut().for_each(|i| restore_strings(i, p)),
        Value::Object(fields) => {
            if fields.get("type").and_then(Value::as_str).is_some_and(|t| REASONING_TYPES.contains(&t)) {
                return;
            }
            for (k, f) in fields.iter_mut() {
                if !REASONING_FIELDS.contains(&k.as_str()) {
                    restore_strings(f, p);
                }
            }
        }
        _ => {}
    }
}

fn encode(events: &[SseEvent]) -> Bytes {
    let mut out = String::new();
    for e in events {
        if let Some(name) = &e.event {
            out.push_str("event: ");
            out.push_str(name);
            out.push('\n');
        }
        for line in e.data.split('\n') {
            out.push_str("data: ");
            out.push_str(line);
            out.push('\n');
        }
        out.push('\n');
    }
    Bytes::from(out)
}

/// Restores a stream's events, in the client's API.
struct Restorer {
    inbound: Inbound,
    p: Arc<Pseudonyms>,
    /// A holdback for each streamed text: a choice's content or a tool call's arguments (Chat
    /// Completions), a content block (Messages), an output text or call's arguments (Responses).
    held: HashMap<String, Holdback>,
    /// Messages: each content block's delta type, for the delta that flushes it.
    delta_types: HashMap<u64, String>,
    /// Chat Completions: the last chunk, whose id and model a flushing chunk takes.
    last_chunk: Option<Value>,
}

impl Restorer {
    fn new(inbound: Inbound, p: Arc<Pseudonyms>) -> Self {
        Self { inbound, p, held: HashMap::new(), delta_types: HashMap::new(), last_chunk: None }
    }

    /// `piece` of the text at `key`, restored as far as it can be.
    fn push(&mut self, key: String, json: bool, piece: &str) -> String {
        let p = &self.p;
        self.held.entry(key).or_insert_with(|| p.holdback(json)).push(piece)
    }

    /// What the text at `key` still holds, restored.
    fn flush(&mut self, key: &str) -> String {
        self.held.remove(key).map(|mut h| h.flush()).unwrap_or_default()
    }

    fn event(&mut self, e: SseEvent) -> Vec<SseEvent> {
        let Ok(mut v) = serde_json::from_str::<Value>(&e.data) else {
            // `[DONE]` ends a Chat Completions stream: what is held goes first.
            let mut out = self.finish();
            out.push(e);
            return out;
        };
        let mut before = Vec::new();
        match self.inbound {
            Inbound::Chat => self.chat(&mut v),
            Inbound::Messages => before = self.messages(&mut v),
            Inbound::Responses => before = self.responses(&mut v),
        }
        before.push(SseEvent { event: e.event, data: v.to_string() });
        before
    }

    /// Chat Completions: each choice's `delta.content`, and its tool calls' arguments. A choice's
    /// last chunk (with its `finish_reason`) takes what is held.
    fn chat(&mut self, v: &mut Value) {
        let Some(choices) = v.get_mut("choices").and_then(Value::as_array_mut) else {
            return restore_strings(v, &self.p);
        };
        for c in choices.iter_mut() {
            let i = c["index"].as_u64().unwrap_or(0);
            let finished = c.get("finish_reason").is_some_and(|f| !f.is_null());
            let content_key = format!("c{i}");
            if let Some(text) = c["delta"]["content"].as_str().map(str::to_string) {
                c["delta"]["content"] = self.push(content_key.clone(), false, &text).into();
            }
            if let Some(calls) = c["delta"].get_mut("tool_calls").and_then(Value::as_array_mut) {
                let mut restored = Vec::new();
                for call in calls.iter() {
                    let t = call["index"].as_u64().unwrap_or(0);
                    restored.push(call["function"]["arguments"].as_str().map(|a| (t, a.to_string())));
                }
                for (call, r) in calls.iter_mut().zip(restored) {
                    if let Some((t, args)) = r {
                        call["function"]["arguments"] = self.push(format!("t{i}.{t}"), true, &args).into();
                    }
                }
            }
            if finished {
                let rest = self.flush(&content_key);
                if !rest.is_empty() {
                    let text = c["delta"]["content"].as_str().unwrap_or_default().to_string();
                    c["delta"]["content"] = (text + &rest).into();
                }
                let calls: Vec<String> = self.held.keys().filter(|k| k.starts_with(&format!("t{i}."))).cloned().collect();
                for key in calls {
                    let rest = self.flush(&key);
                    if rest.is_empty() {
                        continue;
                    }
                    let t: u64 = key.rsplit('.').next().and_then(|t| t.parse().ok()).unwrap_or(0);
                    if !c["delta"]["tool_calls"].is_array() {
                        c["delta"]["tool_calls"] = json!([]);
                    }
                    if let Some(list) = c["delta"]["tool_calls"].as_array_mut() {
                        list.push(json!({ "index": t, "function": { "arguments": rest } }));
                    }
                }
            }
        }
        self.last_chunk = Some(v.clone());
    }

    /// Messages: text and tool input deltas of each content block; its stop takes what is held,
    /// in a delta of its own just before.
    fn messages(&mut self, v: &mut Value) -> Vec<SseEvent> {
        let index = v["index"].as_u64().unwrap_or(0);
        match v["type"].as_str().unwrap_or_default() {
            "content_block_delta" => {
                let ty = v["delta"]["type"].as_str().unwrap_or_default().to_string();
                let field = match ty.as_str() {
                    "text_delta" => "text",
                    "input_json_delta" => "partial_json",
                    _ => return vec![],
                };
                if let Some(piece) = v["delta"][field].as_str().map(str::to_string) {
                    v["delta"][field] = self.push(format!("b{index}"), field == "partial_json", &piece).into();
                    self.delta_types.insert(index, ty);
                }
                vec![]
            }
            "content_block_stop" => {
                let rest = self.flush(&format!("b{index}"));
                let ty = self.delta_types.remove(&index).unwrap_or_else(|| "text_delta".into());
                if rest.is_empty() {
                    return vec![];
                }
                let field = if ty == "input_json_delta" { "partial_json" } else { "text" };
                let delta = json!({ "type": "content_block_delta", "index": index, "delta": { "type": ty, field: rest } });
                vec![SseEvent { event: Some("content_block_delta".into()), data: delta.to_string() }]
            }
            _ => {
                restore_strings(v, &self.p);
                vec![]
            }
        }
    }

    /// Responses: output text and function call argument deltas; their `.done` events take what
    /// is held, in a delta of their own just before, and carry the whole text restored.
    fn responses(&mut self, v: &mut Value) -> Vec<SseEvent> {
        let ty = v["type"].as_str().unwrap_or_default().to_string();
        let key = format!("{}#{}", v["item_id"].as_str().unwrap_or_default(), v["content_index"].as_u64().unwrap_or(0));
        let (json, kind) = match ty.as_str() {
            "response.output_text.delta" | "response.output_text.done" => (false, "response.output_text"),
            "response.function_call_arguments.delta" | "response.function_call_arguments.done" => {
                (true, "response.function_call_arguments")
            }
            _ => {
                restore_strings(v, &self.p);
                return vec![];
            }
        };
        if ty.ends_with(".delta") {
            if let Some(piece) = v["delta"].as_str().map(str::to_string) {
                v["delta"] = self.push(key, json, &piece).into();
            }
            return vec![];
        }
        let rest = self.flush(&key);
        restore_strings(v, &self.p);
        if rest.is_empty() {
            return vec![];
        }
        let mut delta = json!({ "type": format!("{kind}.delta"), "delta": rest });
        for field in ["item_id", "output_index", "content_index", "sequence_number"] {
            if let Some(x) = v.get(field) {
                delta[field] = x.clone();
            }
        }
        vec![SseEvent { event: Some(format!("{kind}.delta")), data: delta.to_string() }]
    }

    /// At the stream's end: whatever is still held, as deltas of their own.
    fn finish(&mut self) -> Vec<SseEvent> {
        let keys: Vec<String> = self.held.keys().cloned().collect();
        let mut out = Vec::new();
        for key in keys {
            let rest = self.flush(&key);
            if rest.is_empty() {
                continue;
            }
            match self.inbound {
                Inbound::Chat => {
                    let mut chunk = self.last_chunk.clone().unwrap_or_else(|| json!({ "object": "chat.completion.chunk" }));
                    let (choice, call) = key[1..].split_once('.').map_or((&key[1..], None), |(c, t)| (c, Some(t)));
                    let index: u64 = choice.parse().unwrap_or(0);
                    let delta = match call {
                        Some(t) => json!({ "tool_calls": [{ "index": t.parse::<u64>().unwrap_or(0), "function": { "arguments": rest } }] }),
                        None => json!({ "content": rest }),
                    };
                    chunk["choices"] = json!([{ "index": index, "delta": delta, "finish_reason": null }]);
                    if let Some(o) = chunk.as_object_mut() {
                        o.remove("usage");
                    }
                    out.push(SseEvent { event: None, data: chunk.to_string() });
                }
                // Their blocks and items end with events that flush them; a stream cut short
                // ends without.
                Inbound::Messages | Inbound::Responses => {}
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapping() -> Arc<Pseudonyms> {
        let mut p = Pseudonyms::default();
        p.of("PERSON", "Alice");
        p.of("LOCATION", "Berlin");
        Arc::new(p)
    }

    fn run(inbound: Inbound, events: &[(Option<&str>, Value)]) -> Vec<(Option<String>, Value)> {
        let mut r = Restorer::new(inbound, mapping());
        let mut out = Vec::new();
        for (name, v) in events {
            out.extend(r.event(SseEvent { event: name.map(str::to_string), data: v.to_string() }));
        }
        out.extend(r.finish());
        out.into_iter().map(|e| (e.event, serde_json::from_str(&e.data).unwrap_or(Value::Null))).collect()
    }

    #[test]
    fn restores_chat_completions_streams() {
        let chunk =
            |delta: Value, finish: Value| json!({ "id": "c1", "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }] });
        let out = run(
            Inbound::Chat,
            &[
                (None, chunk(json!({ "content": "Hello Per" }), Value::Null)),
                (None, chunk(json!({ "content": "son A from Location" }), Value::Null)),
                (None, chunk(json!({ "content": " A" }), Value::Null)),
                (None, chunk(json!({}), json!("stop"))),
            ],
        );
        let text: String = out.iter().filter_map(|(_, v)| v["choices"][0]["delta"]["content"].as_str()).collect();
        assert_eq!(text, "Hello Alice from Berlin");
        // The last piece came with the chunk that finished the choice.
        assert_eq!(out[3].1["choices"][0]["delta"]["content"], "Berlin");
    }

    #[test]
    fn restores_messages_streams() {
        let delta = |i: u64, d: Value| (Some("content_block_delta"), json!({ "type": "content_block_delta", "index": i, "delta": d }));
        let out = run(
            Inbound::Messages,
            &[
                (
                    Some("content_block_start"),
                    json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "thinking", "thinking": "" } }),
                ),
                delta(0, json!({ "type": "thinking_delta", "thinking": "Person A lives in Location A" })),
                (Some("content_block_stop"), json!({ "type": "content_block_stop", "index": 0 })),
                delta(1, json!({ "type": "text_delta", "text": "Write to Person A" })),
                (Some("content_block_stop"), json!({ "type": "content_block_stop", "index": 1 })),
                delta(2, json!({ "type": "input_json_delta", "partial_json": "{\"to\": \"Person A\"}" })),
                (Some("content_block_stop"), json!({ "type": "content_block_stop", "index": 2 })),
            ],
        );
        // Thinking stays as the model wrote it: its signature covers it.
        assert_eq!(out[1].1["delta"]["thinking"], "Person A lives in Location A");
        assert_eq!(out[3].1["delta"]["text"], "Write to ");
        assert_eq!((out[4].0.as_deref(), &out[4].1["delta"]["text"]), (Some("content_block_delta"), &json!("Alice")));
        assert_eq!(out[5].0.as_deref(), Some("content_block_stop"));
        assert_eq!(out[6].1["delta"]["partial_json"], "{\"to\": \"Alice\"}");
    }

    #[test]
    fn restores_responses_streams() {
        let out = run(
            Inbound::Responses,
            &[
                (
                    Some("response.output_text.delta"),
                    json!({ "type": "response.output_text.delta", "item_id": "m", "output_index": 0, "content_index": 0, "delta": "In Location A" }),
                ),
                (
                    Some("response.output_text.done"),
                    json!({ "type": "response.output_text.done", "item_id": "m", "output_index": 0, "content_index": 0, "text": "In Location A" }),
                ),
                (
                    Some("response.completed"),
                    json!({ "type": "response.completed", "response": { "output": [
                { "type": "reasoning", "summary": [{ "type": "summary_text", "text": "Person A" }] },
                { "type": "message", "content": [{ "type": "output_text", "text": "In Location A" }] },
            ] } }),
                ),
            ],
        );
        assert_eq!(out[0].1["delta"], "In ");
        assert_eq!((out[1].1["type"].as_str(), out[1].1["delta"].as_str()), (Some("response.output_text.delta"), Some("Berlin")));
        assert_eq!(out[2].1["text"], "In Berlin");
        assert_eq!(out[3].1["response"]["output"][0]["summary"][0]["text"], "Person A");
        assert_eq!(out[3].1["response"]["output"][1]["content"][0]["text"], "In Berlin");
    }

    #[test]
    fn restores_full_answers_but_reasoning() {
        let mut v = json!({ "content": [
            { "type": "thinking", "thinking": "Person A", "signature": "s" },
            { "type": "text", "text": "Dear Person A," },
            { "type": "tool_use", "input": { "to": "Person A", "city": "Location A" } },
        ], "choices": [{ "message": { "content": "Person A", "reasoning_content": "Person A" } }] });
        restore_strings(&mut v, &mapping());
        assert_eq!(v["content"][0]["thinking"], "Person A");
        assert_eq!(v["content"][1]["text"], "Dear Alice,");
        assert_eq!(v["content"][2]["input"], json!({ "to": "Alice", "city": "Berlin" }));
        assert_eq!(v["choices"][0]["message"]["reasoning_content"], "Person A");
    }
}
