//! Screening prompts with guardrails (see `crate::guardrails`): which texts of a request make up
//! the prompt, checking them with each guardrail in turn, and putting masked texts back. This runs
//! on the request as the client sent it, before routing, so the masked request is what every
//! deployment, auto router decision and stored conversation sees, translated or passed through.

use std::{collections::BTreeSet, sync::Arc};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

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

/// How long a guardrail's verdict on a text is remembered. A conversation sends its earlier turns
/// with every request: within this time each text is screened once, not on every turn.
const REMEMBER: std::time::Duration = std::time::Duration::from_secs(3600);

/// A remembered verdict: `PASSED` alone, or `MASKED` followed by the masked text, sealed with the
/// gateway's encryption key so no prompt text is kept in Valkey in the clear.
const PASSED: u8 = b'p';
const MASKED: u8 = b'm';

/// A keyed hash of `text` as `g` screens it, for its remembered verdict. The guardrail's last
/// change is part of it, so editing a guardrail forgets its verdicts; the key (derived from the
/// encryption key) keeps the hash of a short text from giving the text away.
fn text_hash(state: &AppState, g: &Active, text: &str) -> String {
    let mut h = Sha256::new();
    h.update(state.cfg.affinity_salt);
    h.update(b"guardrail verdict\n");
    h.update(g.row.updated_at.timestamp_micros().to_le_bytes());
    h.update(text.as_bytes());
    hex::encode(&h.finalize()[..16])
}

/// The text as a remembered verdict leaves it, if one is remembered and can be read.
fn recall(state: &AppState, text: &str, remembered: Option<Vec<u8>>) -> Option<String> {
    let remembered = remembered?;
    match remembered.split_first()? {
        (&PASSED, []) => Some(text.to_string()),
        (&MASKED, sealed) => String::from_utf8(state.cfg.secret_box.open(sealed).ok()?).ok(),
        _ => None,
    }
}

/// Remembers what `g` did to the texts it was sent (`todo`, indexes into `texts` and `after`),
/// in the background. A guardrail answers for all the texts it gets at once, so a text is only
/// remembered when its verdict is known on its own: every text when nothing was found, and the
/// texts it changed when it masked. Flagged texts, and unchanged texts next to masked ones, may
/// have been what the guardrail found, so they are screened again.
fn remember(state: &AppState, g: &Active, action: Action, todo: &[usize], hashes: &[String], texts: &[String], after: &[String]) {
    let verdicts: Vec<(String, Vec<u8>)> = match action {
        Action::Passed => todo.iter().map(|&i| (hashes[i].clone(), vec![PASSED])).collect(),
        Action::Masked => todo
            .iter()
            .filter(|&&i| after[i] != texts[i])
            .map(|&i| {
                let mut v = vec![MASKED];
                v.extend(state.cfg.secret_box.seal(after[i].as_bytes()));
                (hashes[i].clone(), v)
            })
            .collect(),
        Action::Flagged | Action::Blocked => return,
    };
    if verdicts.is_empty() {
        return;
    }
    let (state, id) = (state.clone(), g.row.id);
    tokio::spawn(async move { state.cache.save_guard_verdicts(id, verdicts, REMEMBER).await });
}

/// Screens the prompt of `raw` with `guards`, in order, and masks it in place: each guardrail sees
/// the text as the ones before it left it. Texts a guardrail screened in an earlier request keep
/// its verdict (see `REMEMBER`), so a conversation's earlier turns are not screened again. A guardrail that blocks refuses the request with its
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
        // Texts this guardrail screened in an earlier request (a conversation's earlier turns)
        // keep their verdict; only the others are sent. Blank texts need no screening.
        let hashes: Vec<String> = texts.iter().map(|t| text_hash(state, g, t)).collect();
        let remembered = state.cache.guard_verdicts(g.row.id, &hashes).await;
        let known: Vec<Option<String>> =
            texts.iter().zip(remembered).map(|(t, r)| if t.trim().is_empty() { Some(t.clone()) } else { recall(state, t, r) }).collect();
        let todo: Vec<usize> = (0..texts.len()).filter(|&i| known[i].is_none()).collect();
        let fresh: Vec<String> = todo.iter().map(|&i| texts[i].clone()).collect();
        let verdict = match fresh.is_empty() {
            true => None,
            false => match guardrails::check(state, &g.callable(), Source::Input, &fresh).await {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!(guardrail = %g.row.name, error = %e.message, "guardrail failed; refusing the request");
                    let mut s = step("failed");
                    s["duration_ms"] = (started.elapsed().as_millis() as u64).into();
                    s["error"] = e.message.clone().into();
                    steps.push(s);
                    let e = GatewayError::unavailable(format!("guardrail '{}' could not screen the request: {}", g.row.name, e.message));
                    return Err((charge(&applied, action, &findings, cost, steps), e));
                }
            },
        };
        let masked_before = known.iter().zip(&texts).any(|(k, t)| k.as_ref().is_some_and(|k| k != t));
        let mut g_action = verdict.as_ref().map_or(Action::Passed, |v| v.action);
        if masked_before {
            g_action = g_action.max(Action::Masked);
        }
        let fresh_masked = verdict.as_ref().is_some_and(|v| v.action == Action::Masked);
        let bad_answer = verdict.as_ref().filter(|v| fresh_masked && v.outputs.len() != fresh.len()).map(|v| v.outputs.len());
        // The texts as this guardrail leaves them.
        let mut after: Vec<String> = known.into_iter().map(Option::unwrap_or_default).collect();
        for (n, &i) in todo.iter().enumerate() {
            after[i] = match &verdict {
                Some(v) if fresh_masked && bad_answer.is_none() => v.outputs[n].clone(),
                _ => texts[i].clone(),
            };
        }
        if let Some(v) = &verdict {
            cost += v.cost_usd;
            findings.extend(v.findings.iter().map(|f| format!("{}:{}", f.policy, f.kind)));
        }
        action = action.max(g_action);
        if trace {
            let mut s = step(g_action.as_str());
            s["duration_ms"] = (started.elapsed().as_millis() as u64).into();
            s["remembered"] = (texts.len() - todo.len()).into();
            if let Some(v) = &verdict {
                s["findings"] = serde_json::to_value(&v.findings).unwrap_or_default();
                s["usage"] = serde_json::to_value(v.usage).unwrap_or_default();
                s["cost_usd"] = v.cost_usd.into();
                if v.action == Action::Blocked {
                    s["message"] = v.outputs.first().cloned().into();
                }
                s["provider_response"] = v.raw.clone().unwrap_or_default();
            }
            if g_action == Action::Masked && bad_answer.is_none() {
                let changes: Vec<Value> = pointers
                    .iter()
                    .zip(texts.iter().zip(&after))
                    .filter(|(_, (before, after))| before != after)
                    .map(|(p, (before, after))| json!({ "pointer": p, "before": before, "after": after }))
                    .collect();
                s["changes"] = changes.into();
            }
            steps.push(s);
        }
        if let Some(v) = verdict.as_ref().filter(|v| v.action == Action::Blocked) {
            let message = v.outputs.first().cloned().unwrap_or_else(|| "The request was blocked.".into());
            let e = GatewayError::content_filter(format!("{message} (blocked by guardrail '{}')", g.row.name));
            return Err((charge(&applied, action, &findings, cost, steps), e));
        }
        if let Some(n) = bad_answer {
            let e = GatewayError::unavailable(format!("guardrail '{}' answered with {n} texts for {}", g.row.name, fresh.len()));
            return Err((charge(&applied, action, &findings, cost, steps), e));
        }
        if let Some(v) = &verdict {
            remember(state, g, v.action, &todo, &hashes, &texts, &after);
        }
        texts = after;
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
