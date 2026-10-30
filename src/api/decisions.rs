//! Decision models: `POST /v1/systemone` (TypeSafe System One format) and `POST /v1/decisions`
//! (OpenAI Decisions format), compatible with LiteLLM's decisions API. Both go upstream as
//! System One, which TypeSafe Jev and Microsoft Foundry's Microsoft-Decision-1 speak. Decisions
//! are never streamed.

use std::{collections::HashSet, sync::Arc};

use axum::{
    Json,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::{
    accounting::{Rollup, Tracker},
    auth::ClientKey,
    labels::{ClientLabels, Labels},
    error::{Dialect, ErrorKind, GatewayError},
    ir::Usage,
    model::{AutoRouter, Deployment, Mode, ProviderKind, VirtualKey},
    providers::{self, Op},
    state::AppState,
};

const MAX_QUESTIONS: usize = 128;
const MAX_ASSET_REFERENCE_BYTES: usize = 1024;
/// TypeSafe opt-in headers. Forwarded to TypeSafe deployments only.
const DANGER_ZONE_PREFIX: &str = "typesafe-danger-zone-";
const IMAGES_HEADER: &str = "typesafe-danger-zone-images";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    SystemOne,
    Decisions,
}

impl Route {
    fn name(self) -> &'static str {
        match self {
            Self::SystemOne => "systemone",
            Self::Decisions => "decisions",
        }
    }
}

pub async fn systemone(
    State(state): State<AppState>,
    ClientKey(key): ClientKey,
    ClientLabels(labels): ClientLabels,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    serve(&state, key, labels, &headers, body, Route::SystemOne).await
}

pub async fn decisions(
    State(state): State<AppState>,
    ClientKey(key): ClientKey,
    ClientLabels(labels): ClientLabels,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    serve(&state, key, labels, &headers, body, Route::Decisions).await
}

/// Run one decisions request as an authenticated key; also used by the admin playground. It
/// runs to completion, and is accounted, even if the client stops waiting.
pub async fn serve(state: &AppState, key: Arc<VirtualKey>, labels: Labels, headers: &HeaderMap, body: Bytes, route: Route) -> Response {
    let (state, headers) = (state.clone(), headers.clone());
    super::to_completion(async move { run(&state, key, &labels, &headers, body, route).await })
        .await
        .unwrap_or_else(|e| e.into_response_for(Dialect::OpenAi))
}

/// What the client asked, to put a Decisions answer back in its shape.
#[derive(Debug)]
enum Asked {
    Predicate,
    /// Choice values as sent (strings or booleans), in order.
    Choice(Vec<Value>),
    /// Level labels, lowest first.
    Score(Vec<String>),
    Box,
}

#[derive(Debug)]
struct Question {
    /// Key in the System One `questions` and `answers` maps.
    key: String,
    name: Option<String>,
    asked: Asked,
}

/// A validated request in System One form.
#[derive(Debug)]
struct Prepared {
    state: Value,
    questions: Map<String, Value>,
    assets: Map<String, Value>,
    /// Decisions requests only: the questions in order.
    order: Vec<Question>,
    /// Images or box questions: TypeSafe only, behind its images header.
    needs_images: bool,
}

async fn run(
    state: &AppState,
    key: Arc<VirtualKey>,
    labels: &Labels,
    headers: &HeaderMap,
    body: Bytes,
    route: Route,
) -> Result<Response, GatewayError> {
    let raw: Value = serde_json::from_slice(&body).map_err(|e| GatewayError::invalid(format!("invalid JSON body: {e}")))?;
    if !raw.is_object() {
        return Err(GatewayError::invalid("request body must be a JSON object"));
    }
    let model = raw.get("model").and_then(Value::as_str).ok_or_else(|| GatewayError::invalid("model is required"))?.to_string();
    if raw.get("stream").and_then(Value::as_bool) == Some(true) {
        return Err(GatewayError::invalid("decision models do not stream"));
    }
    let prepared = match route {
        Route::SystemOne => parse_systemone(&raw)?,
        Route::Decisions => parse_decisions(&raw)?,
    };
    if !key.allows_model(&model) {
        return Err(GatewayError::forbidden(format!("this key is not allowed to use model '{model}'")));
    }
    let mut candidates = super::usable_deployments(state, &key, &model, Mode::Decision)?;
    if prepared.needs_images {
        let opted_in = headers
            .get(IMAGES_HEADER)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("true"));
        if !opted_in {
            return Err(GatewayError::invalid(
                "images, assets and box questions need the TypeSafe-Danger-Zone-Images: true header",
            ));
        }
        candidates.retain(|d| d.provider == ProviderKind::Typesafe);
        if candidates.is_empty() {
            return Err(GatewayError::invalid(format!(
                "images, assets and box questions are only supported by TypeSafe deployments, and '{model}' has none"
            )));
        }
    }

    let mut tracker = Tracker::new(state, Some(&key), labels, &model, route.name(), false);
    tracker.estimate_input(body.len());
    if let Some(c) = tracker.log() {
        c.request(crate::capture::Body::Bytes(body));
    }
    if let Err(e) = crate::auth::check_key_limits(state, &key).await {
        tracker.fail(&e);
        return Err(e);
    }
    let prepared = &prepared;
    let upstream =
        super::with_failover(state, &candidates, &model, &mut tracker, |d| async move { call(state, &d, prepared, headers).await }).await?;
    tracker.set_usage(usage(&upstream));
    let request_id = HeaderValue::from_str(&tracker.id.to_string()).expect("uuid header");
    let body = match route {
        Route::SystemOne => upstream,
        Route::Decisions => render_decisions(&prepared.order, &upstream, &model),
    };
    if let Some(c) = tracker.log() {
        c.response(crate::capture::Body::Json(body.clone()));
    }
    let mut resp = Json(body).into_response();
    resp.headers_mut().insert("x-request-id", request_id);
    Ok(resp)
}

/// One System One call. Returns the upstream response, validated and normalized.
async fn call(state: &AppState, d: &Deployment, p: &Prepared, client_headers: &HeaderMap) -> Result<Value, GatewayError> {
    let mut body = Map::new();
    body.insert("model".into(), d.upstream_model.clone().into());
    body.insert("state".into(), p.state.clone());
    body.insert("questions".into(), Value::Object(p.questions.clone()));
    if !p.assets.is_empty() {
        body.insert("assets".into(), Value::Object(p.assets.clone()));
    }
    let url = providers::url(state, d, Op::SystemOne).await?;
    let mut headers = providers::headers(d)?;
    if d.provider == ProviderKind::Typesafe {
        for (name, value) in client_headers {
            if name.as_str().starts_with(DANGER_ZONE_PREFIX) {
                headers.insert(name.clone(), value.clone());
            }
        }
    }
    let resp = providers::post(state, d, &url, headers, providers::json_bytes(&Value::Object(body))).await?;
    let bytes = resp.bytes().await.map_err(|e| GatewayError::from_reqwest(&e))?;
    let v = serde_json::from_slice(&bytes).map_err(|_| bad_upstream("the response is not JSON"))?;
    normalize_response(v)
}

/// The route an auto router's decision model picks for a request, with its confidence. The
/// decision call is logged as a child of the router's entry.
/// Asks the router's decision model which route fits the transcript; returns its name and the
/// decision's confidence.
pub async fn choose_route(state: &AppState, router: &AutoRouter, transcript: String, parent: &Arc<Rollup>) -> Result<(String, f64), GatewayError> {
    let model = &router.decision_model;
    let mut tracker = Tracker::child(state, parent, false, model, "decisions", false);
    let candidates = match super::deployments_for(state, model, Mode::Decision) {
        Ok(c) => c,
        Err(e) => {
            tracker.fail(&e);
            return Err(e);
        }
    };
    let criteria: Map<String, Value> = router.routes.iter().map(|r| (r.name.clone(), r.description.clone().into())).collect();
    let instructions = router.instructions.as_deref().filter(|i| !i.trim().is_empty()).unwrap_or(DEFAULT_ROUTE_INSTRUCTIONS);
    let mut questions = Map::new();
    questions.insert(ROUTE_QUESTION.into(), json!({ "type": "choice", "instructions": instructions, "criteria": criteria }));
    let p = Prepared { state: transcript.into(), questions, assets: Map::new(), order: Vec::new(), needs_images: false };
    if let Some(c) = tracker.log() {
        c.request(crate::capture::Body::Json(json!({ "model": model, "state": p.state, "questions": p.questions })));
    }
    let (p, no_headers) = (&p, &HeaderMap::new());
    let resp = super::with_failover(state, &candidates, model, &mut tracker, |d| async move { call(state, &d, p, no_headers).await }).await?;
    tracker.set_usage(usage(&resp));
    if let Some(c) = tracker.log() {
        c.response(crate::capture::Body::Json(resp.clone()));
    }
    let answer = &resp["answers"][ROUTE_QUESTION];
    let picked = answer["choice"].as_str().filter(|c| router.route(c).is_some());
    match picked {
        Some(c) => Ok((c.to_string(), answer["confidence"].as_f64().unwrap_or_default())),
        None => {
            let e = bad_upstream("the decision model did not pick one of the routes");
            tracker.fail(&e);
            Err(e)
        }
    }
}

const ROUTE_QUESTION: &str = "route";
const DEFAULT_ROUTE_INSTRUCTIONS: &str =
    "Which route should handle this request? Pick the route whose description fits the request best.";

/// Health check for the admin API: one yes/no question, outside of any key's accounting.
pub async fn probe(state: &AppState, d: &Deployment) -> Result<Value, GatewayError> {
    let mut questions = Map::new();
    questions.insert("reachable".into(), json!({ "type": "noul", "instructions": "Is the service reachable?" }));
    let p = Prepared {
        state: "Health check from the inference gateway.".into(),
        questions,
        assets: Map::new(),
        order: Vec::new(),
        needs_images: false,
    };
    call(state, d, &p, &HeaderMap::new()).await
}

fn bad_upstream(msg: &str) -> GatewayError {
    let mut e = GatewayError::new(
        ErrorKind::Upstream,
        StatusCode::BAD_GATEWAY,
        format!("decision provider returned an unexpected response: {msg}"),
    );
    e.upstream = true;
    e
}

/// Checks the answer shapes and turns a score `legend` sent as a list into the documented map.
fn normalize_response(mut v: Value) -> Result<Value, GatewayError> {
    let answers = v.get_mut("answers").and_then(Value::as_object_mut).ok_or_else(|| bad_upstream("no answers object"))?;
    for (name, a) in answers.iter_mut() {
        let a = a.as_object_mut().ok_or_else(|| bad_upstream(&format!("answers.{name} is not an object")))?;
        if a.get("type").and_then(Value::as_str).is_none() {
            return Err(bad_upstream(&format!("answers.{name} has no type")));
        }
        if let Some(Value::Array(labels)) = a.get("legend") {
            let legend: Map<String, Value> = labels.iter().enumerate().map(|(i, l)| (i.to_string(), l.clone())).collect();
            a.insert("legend".into(), Value::Object(legend));
        }
    }
    Ok(v)
}

fn count(u: &Value, field: &str) -> u64 {
    u.get(field).and_then(Value::as_u64).unwrap_or(0)
}

fn usage(resp: &Value) -> Usage {
    let u = &resp["usage"];
    let (cached, cache_write) = (count(u, "cached_tokens"), count(u, "cache_write_tokens"));
    Usage {
        input_tokens: count(u, "input_tokens").saturating_sub(cached + cache_write),
        output_tokens: count(u, "output_tokens"),
        cache_read_tokens: cached,
        cache_write_tokens: cache_write,
        reasoning_tokens: 0,
        search_units: 0,
    }
}

// ---------------------------------------------------------------- System One requests

fn parse_systemone(raw: &Value) -> Result<Prepared, GatewayError> {
    let state = raw.get("state").ok_or_else(|| GatewayError::invalid("state is required"))?;
    json_text(state, "state")?;
    reject_media(raw)?;
    if raw.get("images").is_some_and(|v| v.as_array().is_none_or(|a| !a.is_empty()) && !v.is_null()) {
        return Err(GatewayError::invalid(
            "images are not supported by these decision models; TypeSafe takes images as assets, with the \
             TypeSafe-Danger-Zone-Images: true header",
        ));
    }
    let questions = raw
        .get("questions")
        .and_then(Value::as_object)
        .ok_or_else(|| GatewayError::invalid("questions must be an object of named questions"))?;
    if questions.is_empty() || questions.len() > MAX_QUESTIONS {
        return Err(GatewayError::invalid(format!("ask 1 to {MAX_QUESTIONS} questions")));
    }
    let mut needs_images = false;
    for (name, q) in questions {
        if name.is_empty() {
            return Err(GatewayError::invalid("question names must not be empty"));
        }
        needs_images |= systemone_question(name, q)?;
    }
    let assets = parse_assets(raw.get("assets"))?;
    needs_images |= !assets.is_empty();
    Ok(Prepared { state: state.clone(), questions: questions.clone(), assets, order: Vec::new(), needs_images })
}

/// Validates one System One question; extra fields pass through. Returns whether it is a box question.
fn systemone_question(name: &str, q: &Value) -> Result<bool, GatewayError> {
    let at = |field: &str| format!("questions.{name}.{field}");
    let o = q.as_object().ok_or_else(|| GatewayError::invalid(format!("questions.{name} must be an object")))?;
    let ty = o.get("type").and_then(Value::as_str).ok_or_else(|| GatewayError::invalid(format!("{} is required", at("type"))))?;
    let instructions = o.get("instructions").filter(|v| !v.is_null());
    if let Some(i) = instructions {
        json_text(i, &at("instructions"))?;
    }
    match ty {
        "noul" => {
            let criteria = o.get("criteria").filter(|v| !v.is_null());
            if let Some(c) = criteria {
                let c = c.as_object().ok_or_else(|| GatewayError::invalid(format!("{} must be an object", at("criteria"))))?;
                if c.keys().any(|k| k != "true" && k != "false") {
                    return Err(GatewayError::invalid(format!("{} takes only true and false", at("criteria"))));
                }
            }
            if instructions.is_none() && criteria.is_none() {
                return Err(GatewayError::invalid(format!("questions.{name}: a noul question needs instructions or criteria")));
            }
        }
        "choice" => {
            let c = o.get("criteria").and_then(Value::as_object).filter(|c| (1..=255).contains(&c.len()));
            if c.is_none() {
                return Err(GatewayError::invalid(format!("{} must map 1 to 255 labels to descriptions", at("criteria"))));
            }
        }
        "score" => {
            let c = o.get("criteria").and_then(Value::as_array).filter(|c| (1..=10).contains(&c.len()));
            if c.is_none() {
                return Err(GatewayError::invalid(format!("{} must list 1 to 10 levels, lowest first", at("criteria"))));
            }
        }
        "box" => {
            if instructions.is_none() {
                return Err(GatewayError::invalid(format!("{} is required", at("instructions"))));
            }
            return Ok(true);
        }
        _ => return Err(GatewayError::invalid(format!("{} must be noul, choice, score or box", at("type")))),
    }
    Ok(false)
}

fn parse_assets(v: Option<&Value>) -> Result<Map<String, Value>, GatewayError> {
    let Some(v) = v.filter(|v| !v.is_null()) else { return Ok(Map::new()) };
    let assets = v.as_object().ok_or_else(|| GatewayError::invalid("assets must be an object keyed by reference"))?;
    for (reference, a) in assets {
        if reference.trim().is_empty() || reference.len() > MAX_ASSET_REFERENCE_BYTES {
            return Err(GatewayError::invalid(format!(
                "asset references must be non-blank and at most {MAX_ASSET_REFERENCE_BYTES} bytes"
            )));
        }
        let invalid = || GatewayError::invalid(format!("assets.{reference} must be {{type: image, format: png|jpg|jpeg, data_base64}}"));
        let o = a.as_object().ok_or_else(invalid)?;
        let known = o.keys().all(|k| matches!(k.as_str(), "type" | "format" | "data_base64"));
        let ty_ok = o.get("type").is_none_or(|t| t.is_null() || t == "image");
        let format_ok = matches!(o.get("format").and_then(Value::as_str), Some("png" | "jpg" | "jpeg"));
        let data_ok = o.get("data_base64").and_then(Value::as_str).is_some_and(is_base64);
        if !(known && ty_ok && format_ok && data_ok) {
            return Err(invalid());
        }
    }
    Ok(assets.clone())
}

// ---------------------------------------------------------------- Decisions requests

fn parse_decisions(raw: &Value) -> Result<Prepared, GatewayError> {
    if raw.get("safety_identifier").is_some_and(|v| !v.is_null()) {
        return Err(GatewayError::invalid("safety_identifier is only supported by OpenAI decision models"));
    }
    reject_media(raw)?;
    let mut texts = Vec::new();
    let mut assets = Map::new();
    match raw.get("input") {
        Some(Value::String(s)) => texts.push(s.clone()),
        Some(Value::Array(messages)) => {
            for (i, m) in messages.iter().enumerate() {
                input_message(i, m, &mut texts, &mut assets)?;
            }
        }
        _ => return Err(GatewayError::invalid("input must be a string or a list of messages")),
    }
    let questions = raw
        .get("questions")
        .and_then(Value::as_array)
        .filter(|q| (1..=MAX_QUESTIONS).contains(&q.len()))
        .ok_or_else(|| GatewayError::invalid(format!("questions must list 1 to {MAX_QUESTIONS} questions")))?;
    let parsed = questions.iter().enumerate().map(|(i, q)| decisions_question(i, q)).collect::<Result<Vec<_>, _>>()?;
    // Named questions keep their names as System One keys when the names are unique.
    let names: HashSet<&str> = parsed.iter().filter_map(|(name, ..)| name.as_deref()).collect();
    let by_name = names.len() == parsed.len();
    let mut out = Map::new();
    let mut order = Vec::with_capacity(parsed.len());
    let mut needs_images = !assets.is_empty();
    for (i, (name, question, asked)) in parsed.into_iter().enumerate() {
        let key = if by_name { name.clone().expect("all named") } else { i.to_string() };
        needs_images |= matches!(asked, Asked::Box);
        out.insert(key.clone(), question);
        order.push(Question { key, name, asked });
    }
    Ok(Prepared { state: Value::String(texts.join("\n\n")), questions: out, assets, order, needs_images })
}

/// Collects a message's text, and its images as TypeSafe assets referenced from the text.
fn input_message(i: usize, m: &Value, texts: &mut Vec<String>, assets: &mut Map<String, Value>) -> Result<(), GatewayError> {
    let at = format!("input[{i}]");
    let o = m.as_object().ok_or_else(|| GatewayError::invalid(format!("{at} must be a message object")))?;
    if let Some(k) = o.keys().find(|k| !matches!(k.as_str(), "type" | "role" | "content")) {
        return Err(GatewayError::invalid(format!("{at}.{k} is not supported")));
    }
    if o.get("type").is_some_and(|t| t != "message") {
        return Err(GatewayError::invalid(format!("{at}.type must be message")));
    }
    if o.get("role").is_some_and(|r| r != "user") {
        return Err(GatewayError::invalid(format!("{at}.role must be user")));
    }
    match o.get("content") {
        Some(Value::String(s)) => texts.push(s.clone()),
        Some(Value::Array(parts)) => {
            for (j, p) in parts.iter().enumerate() {
                let at = format!("{at}.content[{j}]");
                let po = p.as_object().ok_or_else(|| GatewayError::invalid(format!("{at} must be an object")))?;
                let (allowed, ty): (&[&str], _) = match po.get("type").and_then(Value::as_str) {
                    Some("input_text") => (&["type", "text"], "input_text"),
                    Some("input_image") => (&["type", "image_url", "detail"], "input_image"),
                    _ => return Err(GatewayError::invalid(format!("{at}.type must be input_text or input_image"))),
                };
                if let Some(k) = po.keys().find(|k| !allowed.contains(&k.as_str())) {
                    return Err(GatewayError::invalid(format!("{at}.{k} is not supported")));
                }
                if ty == "input_text" {
                    let text = po.get("text").and_then(Value::as_str).ok_or_else(|| GatewayError::invalid(format!("{at}.text must be a string")))?;
                    texts.push(text.to_string());
                } else {
                    let url = po.get("image_url").and_then(Value::as_str).unwrap_or_default();
                    let (reference, asset) = image_asset(url).ok_or_else(|| {
                        GatewayError::invalid(format!(
                            "{at}.image_url must be a PNG or JPEG base64 data URL; the gateway does not download images"
                        ))
                    })?;
                    texts.push(reference.clone());
                    assets.insert(reference, asset);
                }
            }
        }
        _ => return Err(GatewayError::invalid(format!("{at}.content must be a string or a list of parts"))),
    }
    Ok(())
}

/// A PNG or JPEG data URL as a TypeSafe asset, named by a hash of the URL like LiteLLM does.
fn image_asset(url: &str) -> Option<(String, Value)> {
    let rest = url.get(..5).filter(|p| p.eq_ignore_ascii_case("data:")).map(|_| &url[5..])?;
    let (meta, data) = rest.split_once(',')?;
    let meta = meta.to_ascii_lowercase();
    let mut params = meta.split(';');
    let format = match params.next()?.trim() {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        _ => return None,
    };
    if !params.any(|p| p.trim() == "base64") {
        return None;
    }
    let data: String = data.chars().map(|c| match c { '-' => '+', '_' => '/', c => c }).collect();
    if !is_base64(&data) {
        return None;
    }
    let reference = format!("image-{}", &hex::encode(Sha256::digest(url.as_bytes()))[..32]);
    Some((reference, json!({ "type": "image", "format": format, "data_base64": data })))
}

/// One Decisions question as `(name, System One question, what was asked)`.
fn decisions_question(i: usize, q: &Value) -> Result<(Option<String>, Value, Asked), GatewayError> {
    let at = |field: &str| format!("questions[{i}].{field}");
    let o = q.as_object().ok_or_else(|| GatewayError::invalid(format!("questions[{i}] must be an object")))?;
    let ty = o.get("type").and_then(Value::as_str).unwrap_or_default();
    let allowed: &[&str] = match ty {
        "predicate" | "box" => &["type", "name", "instructions"],
        "choice" => &["type", "name", "instructions", "choices"],
        "score" => &["type", "name", "instructions", "levels"],
        _ => return Err(GatewayError::invalid(format!("{} must be predicate, choice, score or box", at("type")))),
    };
    if let Some(k) = o.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(GatewayError::invalid(format!("{} is not a field of a {ty} question", at(k))));
    }
    let name = match o.get("name") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err(GatewayError::invalid(format!("{} must be a string", at("name")))),
    };
    let instructions = o
        .get("instructions")
        .and_then(Value::as_str)
        .ok_or_else(|| GatewayError::invalid(format!("{} is required", at("instructions"))))?;
    let optional_text = |v: Option<&Value>, field: String| match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(GatewayError::invalid(format!("{field} must be a string"))),
    };
    let (question, asked) = match ty {
        "predicate" => (json!({ "type": "noul", "instructions": instructions }), Asked::Predicate),
        "box" => (json!({ "type": "box", "instructions": instructions }), Asked::Box),
        "choice" => {
            let choices = o
                .get("choices")
                .and_then(Value::as_array)
                .filter(|c| (2..=255).contains(&c.len()))
                .ok_or_else(|| GatewayError::invalid(format!("{} must list 2 to 255 options", at("choices"))))?;
            let mut criteria = Map::new();
            let mut values = Vec::with_capacity(choices.len());
            for (j, c) in choices.iter().enumerate() {
                let field = format!("{}[{j}]", at("choices"));
                let co = c.as_object().ok_or_else(|| GatewayError::invalid(format!("{field} must be an object")))?;
                if let Some(k) = co.keys().find(|k| !matches!(k.as_str(), "value" | "description")) {
                    return Err(GatewayError::invalid(format!("{field}.{k} is not supported")));
                }
                let value = co.get("value").cloned().unwrap_or(Value::Null);
                let key = match &value {
                    Value::String(s) => s.clone(),
                    Value::Bool(b) => b.to_string(),
                    _ => return Err(GatewayError::invalid(format!("{field}.value must be a string or boolean"))),
                };
                let description = optional_text(co.get("description"), format!("{field}.description"))?;
                if criteria.insert(key, description.map_or(Value::Null, Value::String)).is_some() {
                    return Err(GatewayError::invalid(format!(
                        "{}: values must be unique, and a boolean cannot share its text with a string",
                        at("choices")
                    )));
                }
                values.push(value);
            }
            (json!({ "type": "choice", "instructions": instructions, "criteria": criteria }), Asked::Choice(values))
        }
        "score" => {
            let levels = o
                .get("levels")
                .and_then(Value::as_array)
                .filter(|l| (2..=10).contains(&l.len()))
                .ok_or_else(|| GatewayError::invalid(format!("{} must list 2 to 10 levels, lowest first", at("levels"))))?;
            let mut criteria = Vec::with_capacity(levels.len());
            let mut labels = Vec::with_capacity(levels.len());
            for (j, l) in levels.iter().enumerate() {
                let field = format!("{}[{j}]", at("levels"));
                let lo = l.as_object().ok_or_else(|| GatewayError::invalid(format!("{field} must be an object")))?;
                if let Some(k) = lo.keys().find(|k| !matches!(k.as_str(), "label" | "description")) {
                    return Err(GatewayError::invalid(format!("{field}.{k} is not supported")));
                }
                let label = lo
                    .get("label")
                    .and_then(Value::as_str)
                    .ok_or_else(|| GatewayError::invalid(format!("{field}.label must be a string")))?;
                let description = optional_text(lo.get("description"), format!("{field}.description"))?;
                criteria.push(Value::String(match description {
                    Some(d) => format!("{label}: {d}"),
                    None => label.to_string(),
                }));
                labels.push(label.to_string());
            }
            (json!({ "type": "score", "instructions": instructions, "criteria": criteria }), Asked::Score(labels))
        }
        _ => unreachable!("type checked above"),
    };
    Ok((name, question, asked))
}

/// System One answers in Decisions shape: one answer per question, in order. A missing or
/// mismatched answer becomes a refusal.
fn render_decisions(order: &[Question], resp: &Value, requested_model: &str) -> Value {
    let answers = resp.get("answers").and_then(Value::as_object);
    let out: Vec<Value> = order
        .iter()
        .map(|q| {
            answers
                .and_then(|a| a.get(&q.key))
                .and_then(|a| decisions_answer(q, a))
                .unwrap_or_else(|| json!({ "type": "refusal", "name": q.name }))
        })
        .collect();
    let u = &resp["usage"];
    let (input, output) = (count(u, "input_tokens"), count(u, "output_tokens"));
    json!({
        "model": resp.get("model").and_then(Value::as_str).unwrap_or(requested_model),
        "answers": out,
        "usage": {
            "input_tokens": input,
            "input_tokens_details": { "cached_tokens": count(u, "cached_tokens"), "cache_write_tokens": count(u, "cache_write_tokens") },
            "output_tokens": output,
            "output_tokens_details": { "reasoning_tokens": 0 },
            "total_tokens": input + output,
        },
    })
}

fn decisions_answer(q: &Question, a: &Value) -> Option<Value> {
    let ty = a.get("type")?.as_str()?;
    let number = |field: &str| a.get(field).and_then(Value::as_f64);
    let probability = |key: &str| a.pointer("/probabilities").and_then(|p| p.get(key)).and_then(Value::as_f64).unwrap_or(0.0);
    Some(match (&q.asked, ty) {
        (Asked::Predicate, "noul") => json!({ "type": "predicate", "name": q.name, "probability": number("noul")? }),
        (Asked::Choice(values), "choice") => {
            let key = |v: &Value| match v {
                Value::Bool(b) => b.to_string(),
                v => v.as_str().unwrap_or_default().to_string(),
            };
            let picked = a.get("choice")?.as_str()?;
            let choice = values.iter().find(|v| key(v) == picked).cloned().unwrap_or_else(|| picked.into());
            let probabilities: Vec<Value> =
                values.iter().map(|v| json!({ "value": v, "probability": probability(&key(v)) })).collect();
            json!({ "type": "choice", "name": q.name, "choice": choice, "probabilities": probabilities, "confidence": number("confidence")? })
        }
        (Asked::Score(labels), "score") => {
            let probabilities: Vec<Value> = labels
                .iter()
                .enumerate()
                .map(|(i, label)| json!({ "value": i, "label": label, "probability": probability(&i.to_string()) }))
                .collect();
            json!({ "type": "score", "name": q.name, "score": number("score")?, "probabilities": probabilities, "confidence": number("confidence")? })
        }
        (Asked::Box, "box") => {
            json!({ "type": "box", "name": q.name, "box": a.get("box")?.as_object()?, "confidence": number("confidence") })
        }
        _ => return None,
    })
}

// ---------------------------------------------------------------- shared checks

/// `state` and `instructions` are text or JSON (object or array).
fn json_text(v: &Value, field: &str) -> Result<(), GatewayError> {
    if v.is_string() || v.is_object() || v.is_array() {
        Ok(())
    } else {
        Err(GatewayError::invalid(format!("{field} must be a string, object or array")))
    }
}

fn reject_media(raw: &Value) -> Result<(), GatewayError> {
    for field in ["audio", "videos"] {
        if raw.get(field).is_some_and(|v| v.as_array().is_none_or(|a| !a.is_empty()) && !v.is_null()) {
            return Err(GatewayError::invalid(format!("{field} input is not supported by these decision models")));
        }
    }
    Ok(())
}

fn is_base64(s: &str) -> bool {
    let body = s.trim_end_matches('=');
    !body.is_empty() && s.len() - body.len() <= 2 && body.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// LiteLLM's TypeSafe translation case: Decisions request to System One and back.
    #[test]
    fn decisions_round_trip() {
        let raw = json!({
            "model": "jev",
            "input": "Ticket (billing): The export job hangs at 99% and never finishes",
            "questions": [
                { "type": "predicate", "name": "defect", "instructions": "Is this a defect?" },
                { "type": "choice", "name": "severity", "instructions": "How severe is it?",
                  "choices": [{ "value": "low", "description": "cosmetic" }, { "value": "high", "description": "blocks users" }] },
                { "type": "score", "name": "confidence", "instructions": "How sure are you?",
                  "levels": [{ "label": "unsure" }, { "label": "sure" }] },
            ],
        });
        let p = parse_decisions(&raw).unwrap();
        assert_eq!(p.state, json!("Ticket (billing): The export job hangs at 99% and never finishes"));
        assert_eq!(
            Value::Object(p.questions.clone()),
            json!({
                "defect": { "type": "noul", "instructions": "Is this a defect?" },
                "severity": { "type": "choice", "instructions": "How severe is it?", "criteria": { "low": "cosmetic", "high": "blocks users" } },
                "confidence": { "type": "score", "instructions": "How sure are you?", "criteria": ["unsure", "sure"] },
            })
        );
        assert!(!p.needs_images);
        let upstream = normalize_response(json!({
            "model": "jev-1.13.0",
            "answers": {
                "defect": { "type": "noul", "noul": 0.78 },
                "severity": { "type": "choice", "choice": "high", "confidence": 0.99, "probabilities": { "low": 0.01, "high": 0.99 } },
                "confidence": { "type": "score", "score": 0.51, "confidence": 0.03, "legend": ["unsure", "sure"], "probabilities": { "0": 0.49, "1": 0.51 } },
            },
            "usage": { "input_tokens": 377, "output_tokens": 62 },
        }))
        .unwrap();
        assert_eq!(upstream["answers"]["confidence"]["legend"], json!({ "0": "unsure", "1": "sure" }));
        assert_eq!(
            render_decisions(&p.order, &upstream, "jev"),
            json!({
                "model": "jev-1.13.0",
                "answers": [
                    { "type": "predicate", "name": "defect", "probability": 0.78 },
                    { "type": "choice", "name": "severity", "choice": "high",
                      "probabilities": [{ "value": "low", "probability": 0.01 }, { "value": "high", "probability": 0.99 }], "confidence": 0.99 },
                    { "type": "score", "name": "confidence", "score": 0.51,
                      "probabilities": [{ "value": 0, "label": "unsure", "probability": 0.49 }, { "value": 1, "label": "sure", "probability": 0.51 }], "confidence": 0.03 },
                ],
                "usage": {
                    "input_tokens": 377,
                    "input_tokens_details": { "cached_tokens": 0, "cache_write_tokens": 0 },
                    "output_tokens": 62,
                    "output_tokens_details": { "reasoning_tokens": 0 },
                    "total_tokens": 439,
                },
            })
        );
        assert_eq!(usage(&upstream).input_tokens, 377);
    }

    #[test]
    fn unnamed_questions_booleans_and_refusals() {
        let raw = json!({
            "input": [{ "role": "user", "content": [{ "type": "input_text", "text": "a" }, { "type": "input_text", "text": "b" }] }],
            "questions": [
                { "type": "choice", "instructions": "Spam?", "choices": [{ "value": true }, { "value": false }] },
                { "type": "score", "name": "q", "instructions": "Rate", "levels": [{ "label": "bad", "description": "broken" }, { "label": "good" }] },
            ],
        });
        let p = parse_decisions(&raw).unwrap();
        assert_eq!(p.state, json!("a\n\nb"));
        // Not every question is named, so keys are positions.
        assert_eq!(p.questions.keys().collect::<Vec<_>>(), ["0", "1"]);
        assert_eq!(p.questions["0"]["criteria"], json!({ "true": null, "false": null }));
        assert_eq!(p.questions["1"]["criteria"], json!(["bad: broken", "good"]));
        let upstream = json!({ "answers": { "0": { "type": "choice", "choice": "false", "confidence": 0.8, "probabilities": { "true": 0.1, "false": 0.9 } } } });
        let out = render_decisions(&p.order, &upstream, "jev");
        assert_eq!(out["model"], "jev");
        assert_eq!(out["answers"][0]["choice"], json!(false));
        assert_eq!(out["answers"][0]["probabilities"][1], json!({ "value": false, "probability": 0.9 }));
        assert_eq!(out["answers"][1], json!({ "type": "refusal", "name": "q" }));
    }

    #[test]
    fn rejects_invalid_requests() {
        let base = |q: Value| json!({ "input": "x", "questions": [q] });
        let bad = [
            base(json!({ "type": "predicate" })),
            base(json!({ "type": "choice", "instructions": "?", "choices": [{ "value": "a" }] })),
            base(json!({ "type": "choice", "instructions": "?", "choices": [{ "value": true }, { "value": "true" }] })),
            base(json!({ "type": "score", "instructions": "?", "levels": [{ "label": "a" }] })),
            base(json!({ "type": "noul", "instructions": "?" })),
            json!({ "input": "x", "questions": [] }),
            json!({ "input": "x", "questions": [{ "type": "predicate", "instructions": "?" }], "safety_identifier": "u1" }),
            json!({ "input": [{ "role": "user", "content": [{ "type": "input_image", "image_url": "https://example.com/a.png" }] }],
                    "questions": [{ "type": "predicate", "instructions": "?" }] }),
        ];
        for raw in bad {
            assert!(parse_decisions(&raw).is_err(), "accepted {raw}");
        }
        let so = |q: Value| json!({ "state": "x", "questions": { "q": q } });
        for raw in [
            so(json!({ "type": "noul" })),
            so(json!({ "type": "noul", "criteria": { "maybe": "?" } })),
            so(json!({ "type": "choice", "criteria": {} })),
            so(json!({ "type": "score", "criteria": [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11] })),
            json!({ "state": 3, "questions": { "q": { "type": "noul", "instructions": "?" } } }),
            json!({ "state": "x", "questions": { "q": { "type": "noul", "instructions": "?" } }, "images": ["data:image/png;base64,AAAA"] }),
            json!({ "state": "x", "questions": { "q": { "type": "noul", "instructions": "?" } }, "assets": { "a": { "format": "gif", "data_base64": "AAAA" } } }),
        ] {
            assert!(parse_systemone(&raw).is_err(), "accepted {raw}");
        }
    }

    #[test]
    fn images_become_assets() {
        let url = "data:image/png;base64,iVBO-_==";
        let raw = json!({
            "input": [{ "content": [{ "type": "input_text", "text": "Is there a mug?" }, { "type": "input_image", "image_url": url }] }],
            "questions": [{ "type": "box", "name": "mug", "instructions": "the mug" }],
        });
        let p = parse_decisions(&raw).unwrap();
        assert!(p.needs_images);
        let (reference, asset) = p.assets.iter().next().unwrap();
        assert!(reference.starts_with("image-") && reference.len() == 38);
        assert_eq!(asset, &json!({ "type": "image", "format": "png", "data_base64": "iVBO+/==" }));
        assert_eq!(p.state, json!(format!("Is there a mug?\n\n{reference}")));
        // System One passes question extras and assets through untouched.
        let so = json!({
            "state": "see photo", "questions": { "q": { "type": "noul", "instructions": "?", "weight": 2 } },
            "assets": { "photo": { "type": "image", "format": "jpg", "data_base64": "AAAA" } }, "cache": { "no-cache": true },
        });
        let p = parse_systemone(&so).unwrap();
        assert!(p.needs_images);
        assert_eq!(p.questions["q"]["weight"], 2);
    }

    #[test]
    fn foundry_paths_trim_to_the_resource() {
        use crate::providers::foundry_resource_path as trim;
        assert_eq!(trim("/"), "");
        assert_eq!(trim("/api/projects/p1/openai/v1"), "");
        assert_eq!(trim("/api/projects/p1"), "");
        assert_eq!(trim("/openai/v1/"), "");
        assert_eq!(trim("/models"), "");
        assert_eq!(trim("/custom/prefix"), "/custom/prefix");
    }
}
