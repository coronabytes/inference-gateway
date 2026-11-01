//! Screening prompts with guardrails (see `crate::guardrails`): which texts of a request make up
//! the prompt, checking them with each guardrail in turn, and putting masked texts back. This runs
//! on the request as the client sent it, before routing, so the masked request is what every
//! deployment, auto router decision and stored conversation sees, translated or passed through.

use std::{collections::BTreeSet, sync::Arc};

use serde_json::{Value, json};

use super::Inbound;
use crate::{
    accounting::GuardrailCharge,
    error::GatewayError,
    guardrails::{self, Action, Active, Source},
    state::AppState,
};

/// A string, or the `text` of the text parts of an array of parts.
fn content(out: &mut Vec<String>, v: &Value, at: String, text_types: &[&str]) {
    match v {
        Value::String(_) => out.push(at),
        Value::Array(parts) => {
            for (j, p) in parts.iter().enumerate() {
                let ty = p.get("type").and_then(Value::as_str).unwrap_or("text");
                if text_types.contains(&ty) && p.get("text").is_some_and(Value::is_string) {
                    out.push(format!("{at}/{j}/text"));
                }
                // Anthropic tool results nest their own content.
                if ty == "tool_result"
                    && let Some(inner) = p.get("content")
                {
                    match inner {
                        Value::String(_) => out.push(format!("{at}/{j}/content")),
                        Value::Array(items) => {
                            for (k, it) in items.iter().enumerate() {
                                if it.get("type").and_then(Value::as_str) == Some("text") && it.get("text").is_some_and(Value::is_string) {
                                    out.push(format!("{at}/{j}/content/{k}/text"));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        _ => {}
    }
}

/// What the guardrails did to a request that passed them.
pub(crate) struct Screened {
    pub action: Action,
    /// The prompt's last text as the model gets it, when the guardrails masked it: usually the
    /// message just sent, for clients to show what left.
    pub masked_prompt: Option<String>,
}

/// JSON pointers to the strings that make up the prompt: system instructions, and user and tool
/// content. The assistant's turns are not screened: they are the model's earlier output.
fn prompt_texts(inbound: Inbound, raw: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let not_assistant = |m: &Value| m.get("role").and_then(Value::as_str) != Some("assistant");
    match inbound {
        Inbound::Chat => {
            for (i, m) in raw["messages"].as_array().into_iter().flatten().enumerate() {
                if not_assistant(m)
                    && let Some(c) = m.get("content")
                {
                    content(&mut out, c, format!("/messages/{i}/content"), &["text"]);
                }
            }
        }
        Inbound::Messages => {
            if let Some(s) = raw.get("system") {
                content(&mut out, s, "/system".into(), &["text"]);
            }
            for (i, m) in raw["messages"].as_array().into_iter().flatten().enumerate() {
                if not_assistant(m)
                    && let Some(c) = m.get("content")
                {
                    content(&mut out, c, format!("/messages/{i}/content"), &["text"]);
                }
            }
        }
        Inbound::Responses => {
            if raw.get("instructions").is_some_and(Value::is_string) {
                content(&mut out, &raw["instructions"], "/instructions".into(), &[]);
            }
            match raw.get("input") {
                Some(Value::String(_)) => content(&mut out, &raw["input"], "/input".into(), &[]),
                Some(Value::Array(items)) => {
                    for (i, it) in items.iter().enumerate() {
                        match it.get("type").and_then(Value::as_str).unwrap_or("message") {
                            "message" if not_assistant(it) => {
                                if let Some(c) = it.get("content") {
                                    content(&mut out, c, format!("/input/{i}/content"), &["input_text", "text"]);
                                }
                            }
                            "function_call_output" if it.get("output").is_some_and(Value::is_string) => {
                                out.push(format!("/input/{i}/output"))
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// Screens the prompt of `raw` with `guards`, in order, and masks it in place: each guardrail sees
/// the text as the ones before it left it. A guardrail that blocks refuses the request with its
/// message; one that cannot be reached refuses it too, as the text would otherwise pass unscreened.
/// Either way the charge for the guardrails that ran comes along, for the request log.
///
/// With `trace` (a logged request, see `capture`), the charge also says what each guardrail was
/// given, found (matched text included), did and answered, and which texts it changed.
pub(crate) async fn screen_prompt(
    state: &AppState,
    inbound: Inbound,
    raw: &mut Value,
    guards: &[Arc<Active>],
    trace: bool,
) -> Result<(GuardrailCharge, Screened), (GuardrailCharge, GatewayError)> {
    let pointers = prompt_texts(inbound, raw);
    let mut texts: Vec<String> = pointers.iter().map(|p| raw.pointer(p).and_then(Value::as_str).unwrap_or_default().to_string()).collect();
    let last_sent = texts.last().cloned();
    let mut cost = 0.0;
    let mut action = Action::Passed;
    let mut findings = BTreeSet::new();
    let mut applied = Vec::new();
    let mut steps: Vec<Value> = Vec::new();
    let charge = |applied: &Vec<String>, action: Action, findings: &BTreeSet<String>, cost: f64, steps: Vec<Value>| GuardrailCharge {
        cost_usd: cost,
        summary: Some(json!({ "applied": applied, "action": action, "findings": findings })),
        trace: trace.then_some(steps),
    };
    for g in guards {
        applied.push(g.row.name.clone());
        let step =
            |action: &str| json!({ "guardrail": g.row.name, "guardrail_id": g.row.id, "provider": g.row.provider, "action": action });
        if texts.iter().all(|t| t.trim().is_empty()) {
            steps.push(step("skipped"));
            continue;
        }
        let started = std::time::Instant::now();
        let v = match guardrails::check(state, &g.callable(), Source::Input, &texts).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(guardrail = %g.row.name, error = %e.message, "guardrail failed; refusing the request");
                let mut s = step("failed");
                s["duration_ms"] = (started.elapsed().as_millis() as u64).into();
                s["error"] = e.message.clone().into();
                steps.push(s);
                let e = GatewayError::unavailable(format!("guardrail '{}' could not screen the request: {}", g.row.name, e.message));
                return Err((charge(&applied, action, &findings, cost, steps), e));
            }
        };
        cost += v.cost_usd;
        action = action.max(v.action);
        findings.extend(v.findings.iter().map(|f| format!("{}:{}", f.policy, f.kind)));
        if trace {
            let mut s = step(v.action.as_str());
            s["duration_ms"] = (started.elapsed().as_millis() as u64).into();
            s["findings"] = serde_json::to_value(&v.findings).unwrap_or_default();
            s["usage"] = serde_json::to_value(v.usage).unwrap_or_default();
            s["cost_usd"] = v.cost_usd.into();
            if v.action == Action::Blocked {
                s["message"] = v.outputs.first().cloned().into();
            }
            if v.action == Action::Masked && v.outputs.len() == texts.len() {
                let changes: Vec<Value> = pointers
                    .iter()
                    .zip(texts.iter().zip(&v.outputs))
                    .filter(|(_, (before, after))| before != after)
                    .map(|(p, (before, after))| json!({ "pointer": p, "before": before, "after": after }))
                    .collect();
                s["changes"] = changes.into();
            }
            s["provider_response"] = v.raw.clone().unwrap_or_default();
            steps.push(s);
        }
        match v.action {
            Action::Blocked => {
                let message = v.outputs.first().cloned().unwrap_or_else(|| "The request was blocked.".into());
                let e = GatewayError::content_filter(format!("{message} (blocked by guardrail '{}')", g.row.name));
                return Err((charge(&applied, action, &findings, cost, steps), e));
            }
            Action::Masked if v.outputs.len() == texts.len() => texts = v.outputs,
            Action::Masked => {
                let e = GatewayError::unavailable(format!(
                    "guardrail '{}' answered with {} texts for {}",
                    g.row.name,
                    v.outputs.len(),
                    texts.len()
                ));
                return Err((charge(&applied, action, &findings, cost, steps), e));
            }
            Action::Passed | Action::Flagged => {}
        }
    }
    if action == Action::Masked {
        for (p, t) in pointers.iter().zip(&texts) {
            if let Some(slot) = raw.pointer_mut(p) {
                *slot = Value::String(t.clone());
            }
        }
    }
    let masked_prompt = texts.last().filter(|t| action == Action::Masked && Some(*t) != last_sent.as_ref()).cloned();
    Ok((charge(&applied, action, &findings, cost, steps), Screened { action, masked_prompt }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(inbound: Inbound, raw: Value) -> Vec<String> {
        prompt_texts(inbound, &raw).iter().map(|p| raw.pointer(p).unwrap().as_str().unwrap().to_string()).collect()
    }

    #[test]
    fn finds_the_prompt_in_every_api() {
        let chat = json!({ "messages": [
            { "role": "system", "content": "sys" },
            { "role": "user", "content": [{ "type": "text", "text": "u1" }, { "type": "image_url", "image_url": { "url": "data:" } }] },
            { "role": "assistant", "content": "a1" },
            { "role": "tool", "tool_call_id": "c", "content": "t1" },
        ] });
        assert_eq!(texts(Inbound::Chat, chat), ["sys", "u1", "t1"]);

        let messages = json!({ "system": [{ "type": "text", "text": "sys" }], "messages": [
            { "role": "user", "content": "u1" },
            { "role": "assistant", "content": [{ "type": "text", "text": "a1" }] },
            { "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": "t", "content": [{ "type": "text", "text": "t1" }] },
                { "type": "document", "source": { "type": "base64" } },
                { "type": "text", "text": "u2" },
            ] },
        ] });
        assert_eq!(texts(Inbound::Messages, messages), ["sys", "u1", "t1", "u2"]);

        let responses = json!({ "instructions": "sys", "input": [
            { "role": "user", "content": [{ "type": "input_text", "text": "u1" }, { "type": "input_file", "file_data": "data:" }] },
            { "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "a1" }] },
            { "type": "function_call_output", "call_id": "c", "output": "t1" },
            { "role": "user", "content": "u2" },
        ] });
        assert_eq!(texts(Inbound::Responses, responses), ["sys", "u1", "t1", "u2"]);
        assert_eq!(texts(Inbound::Responses, json!({ "input": "just this" })), ["just this"]);
    }
}
