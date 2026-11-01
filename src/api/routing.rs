//! Auto routers: a model name that asks a decision model which of several routes fits a request
//! best, then serves the request on one of that route's chat models, with the reasoning effort
//! the route sets for it. The router's log entry is the parent of the two calls it makes (the
//! decision and the routed call) and sums their usage and cost.

use std::sync::Arc;

use axum::{
    http::{HeaderMap, HeaderValue},
    response::Response,
};
use bytes::Bytes;
use serde_json::{Value, json};

use super::{Inbound, anthropic, chat, responses};
use crate::{
    accounting::Rollup,
    affinity::{Affinity, StickyRoute},
    conversations::{self, Previous},
    error::GatewayError,
    ir::{ChatRequest, Part, Role},
    labels::Labels,
    model::{AutoRouter, ReasoningEffort, Route, RouteModel, VirtualKey},
    state::AppState,
};

/// The decision model sees at most this much of the conversation: its end, where the request is.
const MAX_TRANSCRIPT_CHARS: usize = 16_000;

pub async fn route(
    state: &AppState,
    key: Arc<VirtualKey>,
    labels: &Labels,
    headers: &HeaderMap,
    mut raw: Value,
    body: &Bytes,
    inbound: Inbound,
    router: Arc<AutoRouter>,
    guards: &[Arc<crate::guardrails::Active>],
) -> Result<Response, GatewayError> {
    let stream = raw.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let rollup = Rollup::new(state, Some(&key), labels, &router.model_name, inbound.name(), stream);
    // A logged request: the router's entry keeps it as the client sent it and what the guardrails
    // did; its calls keep their own requests and responses.
    rollup.log(|c| c.request(crate::capture::Body::Bytes(body.clone())));
    // Before the guardrails and the decision, so a key over its limits pays for neither.
    if let Err(e) = crate::auth::check_key_limits(state, &key).await {
        rollup.fail(&e);
        return Err(e);
    }
    // Before the decision, so the decision model sees the masked prompt too.
    let mut guarded = None;
    if !guards.is_empty() {
        match super::guard::screen_prompt(state, inbound, &mut raw, guards, rollup.logging()).await {
            Ok((charge, screened)) => {
                rollup.guardrails(charge);
                if screened.action == crate::guardrails::Action::Masked {
                    rollup.log(|c| c.screened(raw.clone()));
                }
                guarded = Some(screened);
            }
            Err((charge, e)) => {
                rollup.guardrails(charge);
                rollup.fail(&e);
                return Err(e);
            }
        }
    }
    if rollup.logging() {
        let conversation = Affinity::fingerprint(state, &raw, inbound);
        rollup.log(|c| c.conversation(conversation));
    }
    let affinity = router.sticky.then(|| Affinity::of(state, &raw, inbound)).flatten();
    let picked = pick(state, &key, &raw, inbound, &router, &rollup, affinity.as_ref()).await;
    let Picked { route, target, how } = match picked {
        Ok(p) => p,
        Err(e) => {
            rollup.fail(&e);
            return Err(e);
        }
    };
    let Some(fields) = raw.as_object_mut() else {
        let e = GatewayError::invalid("the request body must be a JSON object");
        rollup.fail(&e);
        return Err(e);
    };
    fields.insert("model".into(), target.model.clone().into());
    if let Some(effort) = target.reasoning_effort {
        set_effort(fields, inbound, effort);
    }
    let mut resp = super::run_model(state, key, labels, headers, raw, body, inbound, Some(rollup.clone()), &[]).await?;
    super::guardrail_header(&mut resp, guarded.as_ref());
    let h = resp.headers_mut();
    // The client's request id is the router's entry; the routed call is logged beneath it.
    h.insert("x-request-id", HeaderValue::from_str(&rollup.id.to_string()).expect("uuid header"));
    if let Ok(v) = HeaderValue::from_str(&target.model) {
        h.insert("x-gateway-routed-model", v);
    }
    if let Ok(v) = HeaderValue::from_str(&route) {
        h.insert("x-gateway-route-name", v);
    }
    h.insert("x-gateway-route", HeaderValue::from_str(&how).expect("ascii"));
    Ok(resp)
}

/// Where a routed request goes: the route's name, the model in it, and how the route was chosen
/// (`decision:<confidence>`, `fallback`, `pinned`, `sticky` for a later turn of a conversation, or
/// `only` when the key may use just one route).
struct Picked {
    route: String,
    target: RouteModel,
    how: String,
}

/// With `affinity` (a sticky router), a conversation's later turns go where its first was sent,
/// as long as the key may still use that route and model; the decision is only asked once.
async fn pick(
    state: &AppState,
    key: &VirtualKey,
    raw: &Value,
    inbound: Inbound,
    router: &AutoRouter,
    rollup: &Arc<Rollup>,
    affinity: Option<&Affinity>,
) -> Result<Picked, GatewayError> {
    let usable = usable_routes(state, key, router);
    if let Some(a) = affinity
        && let Some(s) = a.route(state, &router.model_name).await
        && let Some(route) = usable.iter().find(|r| r.name == s.route)
        && let Some(target) = sticky_target(route, &s)
    {
        return Ok(Picked { route: route.name.clone(), target: target.clone(), how: "sticky".into() });
    }
    let picked = decide(state, key, raw, inbound, router, rollup, usable).await?;
    // A failed decision is not kept: the next turn asks again.
    if let Some(a) = affinity
        && picked.how != "fallback"
    {
        let s = StickyRoute {
            route: picked.route.clone(),
            model: picked.target.model.clone(),
            reasoning_effort: picked.target.reasoning_effort.map(|e| e.as_str().to_string()),
        };
        let (state, a, router) = (state.clone(), a.clone(), router.model_name.clone());
        tokio::spawn(async move { a.set_route(&state, &router, &s).await });
    }
    Ok(picked)
}

/// The routes this key may use, each with only the models the key may use; routes left without
/// models are dropped.
fn usable_routes(state: &AppState, key: &VirtualKey, router: &AutoRouter) -> Vec<Route> {
    let snap = state.registry.snapshot();
    router
        .routes
        .iter()
        .map(|r| Route { models: r.models.iter().filter(|m| super::key_can_use(&snap, key, &m.model)).cloned().collect(), ..r.clone() })
        .filter(|r| !r.models.is_empty())
        .collect()
}

/// The model of `route` a conversation was sent to: the same model with the same reasoning
/// effort, or the same model if its effort has changed since.
fn sticky_target<'a>(route: &'a Route, s: &StickyRoute) -> Option<&'a RouteModel> {
    let effort = |m: &RouteModel| m.reasoning_effort.map(|e| e.as_str().to_string());
    let same_model = || route.models.iter().filter(|m| m.model == s.model);
    same_model().find(|m| effort(m) == s.reasoning_effort).or_else(|| same_model().next())
}

async fn decide(
    state: &AppState,
    key: &VirtualKey,
    raw: &Value,
    inbound: Inbound,
    router: &AutoRouter,
    rollup: &Arc<Rollup>,
    usable: Vec<Route>,
) -> Result<Picked, GatewayError> {
    // The decision only chooses among the routes this key may use, and a route only among its
    // models the key may use.
    let snap = state.registry.snapshot();
    // A Responses conversation stored upstream can only continue on the deployment that holds it.
    if let Some(id) = raw.get("previous_response_id").and_then(Value::as_str).filter(|_| inbound == Inbound::Responses)
        && let Previous::Upstream(pin) = conversations::resolve(state, id, key.id).await?
        && let Some(model) = snap.deployments.values().flatten().find(|d| d.id == pin.deployment_id).map(|d| &d.model_name)
        && let Some((route, target)) = usable.iter().find_map(|r| r.models.iter().find(|m| m.model == *model).map(|m| (r, m)))
    {
        return Ok(Picked { route: route.name.clone(), target: target.clone(), how: "pinned".into() });
    }
    let restricted;
    let router = match usable.as_slice() {
        [] => return Err(GatewayError::forbidden(format!("this key may not use any route of '{}'", router.model_name))),
        [only] => return Ok(Picked { route: only.name.clone(), target: any_of(only), how: "only".into() }),
        _ => {
            let fallback_route = match usable.iter().any(|r| r.name == router.fallback_route) {
                true => router.fallback_route.clone(),
                false => usable[0].name.clone(),
            };
            restricted = AutoRouter { routes: usable, fallback_route, ..router.clone() };
            &restricted
        }
    };
    let parsed = match inbound {
        Inbound::Messages => anthropic::parse_request(raw),
        Inbound::Responses => responses::parse_request(raw),
        Inbound::Chat => chat::parse_request(raw),
    }?;
    let (name, how) = match crate::api::decisions::choose_route(state, router, transcript(&parsed), rollup).await {
        Ok((name, confidence)) => (name, format!("decision:{confidence:.3}")),
        Err(e) => {
            tracing::warn!(router = %router.model_name, router_id = %router.id, error = %e, fallback = %router.fallback_route, "route decision failed; using the fallback");
            (router.fallback_route.clone(), "fallback".into())
        }
    };
    let route = router.route(&name).expect("the decision picks one of the routes, the fallback is one");
    Ok(Picked { route: name.clone(), target: any_of(route), how })
}

/// One of a route's models, at random: spreads its requests evenly over the pool.
fn any_of(route: &Route) -> RouteModel {
    let i = getrandom::u64().unwrap_or(0) as usize % route.models.len().max(1);
    route.models[i].clone()
}

/// Sets the reasoning effort in the request's own API format, replacing the client's. The router
/// editor offers only efforts the route's model takes; a model of another API style gets the
/// nearest it takes when the request is translated. In Anthropic's format, `none` turns thinking
/// off.
fn set_effort(body: &mut serde_json::Map<String, Value>, inbound: Inbound, effort: ReasoningEffort) {
    let (field, value) = match inbound {
        Inbound::Chat => {
            body.insert("reasoning_effort".into(), effort.as_str().into());
            return;
        }
        Inbound::Responses => ("reasoning", effort.as_str()),
        Inbound::Messages if effort == ReasoningEffort::None => {
            body.insert("thinking".into(), json!({ "type": "disabled" }));
            if let Some(Value::Object(o)) = body.get_mut("output_config") {
                o.remove("effort");
            }
            return;
        }
        Inbound::Messages => ("output_config", effort.as_str()),
    };
    match body.get_mut(field) {
        Some(Value::Object(o)) => {
            o.insert("effort".into(), value.into());
        }
        _ => {
            body.insert(field.into(), json!({ "effort": value }));
        }
    }
}

/// The conversation as plain text for the decision model, trimmed to its end.
pub(crate) fn transcript(req: &ChatRequest) -> String {
    let mut out = String::new();
    let system: Vec<&str> = req.system.iter().map(|b| b.text.as_str()).collect();
    if !system.is_empty() {
        out.push_str("system: ");
        out.push_str(&system.join("\n"));
        out.push_str("\n\n");
    }
    for m in &req.messages {
        out.push_str(match m.role {
            Role::User => "user: ",
            Role::Assistant => "assistant: ",
        });
        let parts: Vec<String> = m
            .content
            .iter()
            .filter_map(|p| match p {
                Part::Text { text, .. } => Some(text.clone()),
                Part::Image { .. } => Some("[image]".into()),
                Part::File { filename: Some(f), .. } => Some(format!("[file {f}]")),
                Part::File { .. } => Some("[file]".into()),
                Part::ToolCall { name, arguments, .. } => Some(format!("[tool call {name}: {arguments}]")),
                Part::ToolResult { content, .. } => Some(format!("[tool result: {}]", crate::ir::parts_text(content))),
                Part::Reasoning { .. } => None,
            })
            .collect();
        out.push_str(&parts.join("\n"));
        out.push_str("\n\n");
    }
    let out = out.trim_end();
    match out.char_indices().rev().nth(MAX_TRANSCRIPT_CHARS.saturating_sub(1)) {
        Some((i, _)) if i > 0 => format!("[earlier conversation omitted]\n{}", &out[i..]),
        _ => out.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Message, TextBlock};

    #[test]
    fn effort_goes_where_each_api_reads_it() {
        let with = |inbound, effort, body: Value| {
            let Value::Object(mut b) = body else { unreachable!() };
            set_effort(&mut b, inbound, effort);
            Value::Object(b)
        };
        let chat = with(Inbound::Chat, ReasoningEffort::Low, json!({ "reasoning_effort": "high" }));
        assert_eq!(chat["reasoning_effort"], "low");
        // The client's other reasoning settings stay.
        let responses = with(Inbound::Responses, ReasoningEffort::Xhigh, json!({ "reasoning": { "summary": "auto" } }));
        assert_eq!(responses["reasoning"], json!({ "summary": "auto", "effort": "xhigh" }));
        // As is: a translated request gets the nearest effort its model takes later.
        let messages = with(Inbound::Messages, ReasoningEffort::Minimal, json!({ "output_config": null }));
        assert_eq!(messages["output_config"], json!({ "effort": "minimal" }));
        let off = with(
            Inbound::Messages,
            ReasoningEffort::None,
            json!({ "thinking": { "type": "adaptive" }, "output_config": { "effort": "high", "format": {} } }),
        );
        assert_eq!(off["thinking"], json!({ "type": "disabled" }));
        assert_eq!(off["output_config"], json!({ "format": {} }));
    }

    #[test]
    fn transcript_keeps_the_end() {
        let req = ChatRequest {
            system: vec![TextBlock { text: "Be brief.".into(), cache: false }],
            messages: vec![
                Message { role: Role::User, content: vec![Part::Text { text: "Fix my SQL".into(), cache: false }] },
                Message {
                    role: Role::Assistant,
                    content: vec![Part::ToolCall { id: "1".into(), name: "run".into(), arguments: "{}".into() }],
                },
            ],
            ..Default::default()
        };
        assert_eq!(transcript(&req), "system: Be brief.\n\nuser: Fix my SQL\n\nassistant: [tool call run: {}]");
        let long = ChatRequest {
            messages: vec![Message { role: Role::User, content: vec![Part::Text { text: "é".repeat(20_000) + "END", cache: false }] }],
            ..Default::default()
        };
        let t = transcript(&long);
        assert!(t.starts_with("[earlier conversation omitted]\n") && t.ends_with("END"));
        assert_eq!(t.chars().count(), "[earlier conversation omitted]\n".chars().count() + MAX_TRANSCRIPT_CHARS);
    }
}
