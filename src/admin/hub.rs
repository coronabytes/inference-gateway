//! The console's model hub: every model the gateway serves, or those one key may use, with what
//! its deployments say about it (token limits, capabilities, prices), the guardrails the key's
//! requests go through, and each model's metadata (a display name and a description, kept in
//! `model_metadata` by public model name).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use axum::{
    Extension, Json,
    extract::{Query, State},
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

use super::R;
use crate::{
    admin::auth::{Access, OwnKey},
    api::{key_can_use, models},
    audit::{self, Actor},
    db,
    error::AdminError,
    guardrails::GuardrailPrices,
    model::{Deployment, KeyScope, Mode, ReasoningEffort},
    registry::Snapshot,
    state::AppState,
};

#[derive(Deserialize, utoipa::IntoParams)]
pub(crate) struct HubQuery {
    /// Only the models this virtual key may use, and the guardrails its requests go through.
    /// Required for a project's users and keys (a key of their project); a key whose scope is
    /// the inference API gets its own, and may leave it out.
    key: Option<Uuid>,
    /// Only models of these modes (`chat`, `embedding`, `decision`, `rerank`, and `router` for the
    /// auto routers), as `chat,router`.
    mode: Option<String>,
    /// Only models of these vendors (their `vendor`: saved, or suggested from their names), as
    /// `Anthropic,OpenAI`; `-` stands for models without one.
    vendor: Option<String>,
    /// Only models with all of these capabilities, by path in `capabilities` or among
    /// `custom_capabilities`, as `image_input,thinking.supported,eu-hosted`.
    capability: Option<String>,
    /// Only models whose name, display name, vendor, description or an upstream model contains
    /// this text, ignoring case.
    q: Option<String>,
    /// With `true`, each model's requests over the last 24 hours (`stats`): all of them for an
    /// admin, the projects' for a project's users, its own for a key.
    stats: Option<bool>,
}

/// A comma-separated filter's values: `chat, router` → ["chat", "router"].
fn list(s: Option<&str>) -> Vec<&str> {
    s.unwrap_or("").split(',').map(str::trim).filter(|v| !v.is_empty()).collect()
}

/// A part of a model name that tells who makes it: anywhere, at the start of a word, or a whole word.
enum Hint {
    Has(&'static str),
    Starts(&'static str),
    Word(&'static str),
}

/// Who likely makes a model, by its names: claude → Anthropic. The first vendor that matches wins.
const VENDOR_HINTS: &[(&str, &[Hint])] = {
    use Hint::*;
    &[
        ("Anthropic", &[Has("claude"), Has("anthropic")]),
        (
            "OpenAI",
            &[
                Starts("gpt"),
                Word("o1"),
                Word("o2"),
                Word("o3"),
                Word("o4"),
                Word("o5"),
                Word("o6"),
                Word("o7"),
                Word("o8"),
                Word("o9"),
                Has("openai"),
                Has("text-embedding-"),
                Has("davinci"),
                Has("whisper"),
            ],
        ),
        ("Google", &[Has("gemini"), Has("gemma"), Has("google"), Has("palm")]),
        ("Mistral AI", &[Has("mistral"), Has("mixtral"), Has("codestral"), Has("pixtral"), Has("ministral"), Has("magistral")]),
        ("Meta", &[Has("llama"), Word("meta")]),
        ("DeepSeek", &[Has("deepseek")]),
        ("Alibaba", &[Has("qwen"), Has("alibaba")]),
        ("Zhipu AI", &[Starts("glm"), Has("zhipu"), Has("z-ai")]),
        ("Moonshot AI", &[Has("kimi"), Has("moonshot")]),
        ("Cohere", &[Has("cohere"), Has("command-r"), Starts("embed-english"), Starts("embed-multilingual")]),
        ("Amazon", &[Has("titan"), Has("amazon.nova"), Starts("nova-")]),
        ("Microsoft", &[Has("microsoft"), Starts("phi-"), Has("decision-1")]),
        ("xAI", &[Has("grok"), Has("x-ai"), Has("xai")]),
        ("TypeSafe", &[Starts("jev")]),
    ]
};

/// The vendor a model's names (lowercase) suggest, if any.
fn suggest_vendor(names: &str) -> Option<&'static str> {
    let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    let found = |part: &str, start: bool, end: bool| {
        names
            .match_indices(part)
            .any(|(i, _)| (!start || !word(names[..i].chars().next_back())) && (!end || !word(names[i + part.len()..].chars().next())))
    };
    VENDOR_HINTS.iter().find_map(|(vendor, hints)| {
        hints
            .iter()
            .any(|h| match h {
                Hint::Has(p) => names.contains(p),
                Hint::Starts(p) => found(p, true, false),
                Hint::Word(p) => found(p, true, true),
            })
            .then_some(*vendor)
    })
}

/// Who makes a model: its metadata's `vendor`, or without one the vendor its name and upstream
/// models suggest.
fn vendor_of<'a>(metadata: &Value, name: &'a str, upstream: impl Iterator<Item = &'a str>) -> Option<String> {
    let saved = metadata.get("vendor").and_then(Value::as_str).map(str::to_string);
    saved.or_else(|| {
        let names = std::iter::once(name).chain(upstream).collect::<Vec<_>>().join(" ");
        suggest_vendor(&names.to_lowercase()).map(str::to_string)
    })
}

/// Whether a model passes the hub's filters.
fn matches(m: &HubModel, modes: &[&str], vendors: &[&str], caps: &[&str], term: &str) -> bool {
    let text = |field: &str| m.metadata.get(field).and_then(Value::as_str);
    let mode_ok = modes.is_empty() || modes.contains(&if m.kind == "router" { "router" } else { m.mode.as_str() });
    let vendor_ok = vendors.is_empty() || vendors.contains(&m.vendor.as_deref().unwrap_or("-"));
    let has = |c: &&str| {
        if models::is_capability(c) {
            m.described.capabilities.as_ref().is_some_and(|cs| cs.get(*c).copied().flatten() == Some(true))
        } else {
            m.custom_capabilities.iter().any(|x| x == c)
        }
    };
    let caps_ok = caps.iter().all(has);
    let term_ok = term.is_empty()
        || [Some(m.name.as_str()), m.target.as_deref(), text("display_name"), m.vendor.as_deref(), text("description")]
            .into_iter()
            .flatten()
            .chain(m.deployments.iter().map(|d| d.upstream_model.as_str()))
            .any(|s| s.to_lowercase().contains(term));
    mode_ok && vendor_ok && caps_ok && term_ok
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct HubView {
    /// The key the list is for; absent for the whole gateway.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    key: Option<HubKey>,
    /// Guardrails every request of the key goes through (set on the key or its project), then
    /// those a client may ask for by name in `x-gateway-guardrails`.
    guardrails: Vec<HubGuardrail>,
    models: Vec<HubModel>,
    /// How many models the key reaches (or the gateway serves) before `mode`, `vendor` and `q`.
    total: usize,
    /// The modes among those models (`router` for the auto routers), for the mode filter.
    modes: Vec<String>,
    /// The vendors among those models (saved or suggested), for the vendor filter; models without
    /// one are not listed.
    vendors: Vec<String>,
    /// The custom capabilities among those models, for the capability filter.
    custom_capabilities: Vec<String>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct HubKey {
    id: Uuid,
    name: String,
    scope: KeyScope,
    #[schema(required)]
    project: Option<String>,
    /// Whether requests with the key are served now: `active`, `disabled`, `expired`,
    /// `project inactive`, or `no inference` for a key whose scope is the admin API only.
    status: &'static str,
    allowed_models: Vec<String>,
    allowed_tags: Vec<String>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct HubGuardrail {
    id: Uuid,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    description: Option<String>,
    #[schema(value_type = crate::guardrails::GuardrailProvider)]
    provider: String,
    /// `key` or `project`: applied to every request. `on request`: when a client asks for it.
    applied: &'static str,
    prices: GuardrailPrices,
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct HubModel {
    /// The public model name clients send as `model`.
    name: String,
    /// `model`, or `router` for an auto router; for a model alias, what its target is.
    kind: &'static str,
    mode: Mode,
    /// The model's metadata, `{}` without any: `display_name`, `vendor` and `description` to start with.
    metadata: Value,
    /// Who makes it: the metadata's `vendor`, or without one the vendor its names suggest.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    vendor: Option<String>,
    tags: Vec<String>,
    /// The custom capabilities all the deployments it reaches carry.
    custom_capabilities: Vec<String>,
    /// A model alias's target: the model name it serves.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    target: Option<String>,
    /// What holds for all the deployments it reaches.
    #[serde(flatten)]
    described: models::Described,
    /// An auto router's models that the key reaches.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    routes: Vec<String>,
    /// Each deployment that may serve it, lowest priority (tried first) first.
    deployments: Vec<HubDeployment>,
    /// With `stats=true`: its requests over the last 24 hours.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    stats: Option<HubStats>,
}

/// A model's requests over the last 24 hours, from the request log.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct HubStats {
    requests: i64,
    /// Answered with a status of 400 or above, as the request log's errors.
    errors: i64,
    /// The successful requests' median latency.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    p50_latency_ms: Option<i32>,
    /// Output tokens per second while generating: after the first token for streamed answers.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    tokens_per_second: Option<f64>,
    /// `[requests, errors]` per hour, oldest first; the last is the current hour.
    hours: Vec<[i64; 2]>,
}

const STATS_HOURS: i64 = 24;

/// The last 24 hours' stats of `models`, by model name; models without requests have none.
async fn stats(state: &AppState, models: &[String], key_id: Option<Uuid>, projects: Option<&[Uuid]>) -> R<HashMap<String, HubStats>> {
    let now = Utc::now().timestamp();
    let first = now - now.rem_euclid(3600) - (STATS_HOURS - 1) * 3600;
    let since = chrono::DateTime::from_timestamp(first, 0).unwrap_or_default();
    let _scan = super::LOG_SCANS.acquire().await.map_err(|_| AdminError::unavailable("shutting down"))?;
    let (hours, speeds) = tokio::try_join!(
        crate::logstore::model_hours(state, since, models, key_id, projects),
        crate::logstore::model_speeds(state, since, models, key_id, projects),
    )?;
    let mut out: HashMap<String, HubStats> = HashMap::new();
    for h in hours {
        let s = out.entry(h.model_name).or_insert_with(|| HubStats {
            requests: 0,
            errors: 0,
            p50_latency_ms: None,
            tokens_per_second: None,
            hours: vec![[0, 0]; STATS_HOURS as usize],
        });
        s.requests += h.requests;
        s.errors += h.errors;
        if let Some(slot) = usize::try_from((h.bucket - first) / 3600).ok().and_then(|i| s.hours.get_mut(i)) {
            *slot = [h.requests, h.errors];
        }
    }
    for sp in speeds {
        if let Some(s) = out.get_mut(&sp.model_name) {
            s.p50_latency_ms = sp.p50_latency_ms;
            // Streamed answers measure generation alone; without any, the whole latency.
            let (tokens, ms) = if sp.stream_ms > 0 { (sp.stream_tokens, sp.stream_ms) } else { (sp.whole_tokens, sp.whole_ms) };
            s.tokens_per_second = (tokens > 0 && ms > 0).then(|| tokens as f64 * 1000.0 / ms as f64);
        }
    }
    Ok(out)
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct HubDeployment {
    id: Uuid,
    /// The model name it is deployed under; differs from the model's for an auto router.
    model: String,
    #[schema(value_type = crate::model::ProviderKind)]
    provider: String,
    upstream_model: String,
    priority: i32,
    weight: u32,
    /// Tried before deployments with a higher priority, which serve only when these fail.
    first_choice: bool,
    /// The deployment's tags, which a key's allowed tags must cover.
    tags: Vec<String>,
    /// The deployment's custom capabilities.
    custom_capabilities: Vec<String>,
    #[serde(flatten)]
    described: models::Described,
    prices: HubPrices,
}

/// USD per 1M tokens; `per_k_searches` per 1,000 rerank queries or web searches, `per_audio_minute` per
/// minute of transcribed audio, `per_m_characters` per 1M spoken characters.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct HubPrices {
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
    /// Requests whose input exceeds this many tokens are billed at the long-context prices.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    long_context_threshold: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    long_input: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    long_output: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    long_cache_read: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    long_cache_write: Option<f64>,
    per_k_searches: f64,
    per_audio_minute: f64,
    per_m_characters: f64,
}

fn prices(d: &Deployment) -> HubPrices {
    let p = &d.pricing;
    let long = p.long_context.map(|(_, r)| r);
    HubPrices {
        input: p.standard.input,
        output: p.standard.output,
        cache_read: p.standard.cache_read,
        cache_write: p.standard.cache_write,
        long_context_threshold: p.long_context.map(|(t, _)| t),
        long_input: long.map(|r| r.input),
        long_output: long.map(|r| r.output),
        long_cache_read: long.map(|r| r.cache_read),
        long_cache_write: long.map(|r| r.cache_write),
        per_k_searches: p.per_k_searches,
        per_audio_minute: p.per_audio_minute,
        per_m_characters: p.per_m_characters,
    }
}

/// The custom capabilities every one of `ds` carries, sorted; none without deployments.
fn shared_capabilities(ds: &[&Deployment]) -> Vec<String> {
    let Some((first, rest)) = ds.split_first() else { return Vec::new() };
    first.capabilities.iter().filter(|c| rest.iter().all(|d| d.capabilities.contains(c))).cloned().collect()
}

fn hub_deployments(ds: &[&Deployment]) -> Vec<HubDeployment> {
    // Per model name: an auto router picks the model, then that model's deployments fail over.
    let first = |model: &str| ds.iter().filter(|d| d.model_name == model).map(|d| d.priority).min();
    let mut out: Vec<HubDeployment> = ds
        .iter()
        .map(|d| HubDeployment {
            id: d.id,
            model: d.model_name.clone(),
            provider: d.provider.as_str().to_string(),
            upstream_model: d.upstream_model.clone(),
            priority: d.priority,
            weight: d.weight,
            first_choice: Some(d.priority) == first(&d.model_name),
            tags: d.tags.clone(),
            custom_capabilities: d.capabilities.clone(),
            described: models::describe_one(d),
            prices: prices(d),
        })
        .collect();
    out.sort_by(|a, b| {
        a.priority.cmp(&b.priority).then_with(|| a.model.cmp(&b.model)).then_with(|| a.upstream_model.cmp(&b.upstream_model))
    });
    out
}

/// Model hub
///
/// Every model the gateway serves, or with `key` those that key may use: token limits,
/// capabilities and prices, per model and per deployment that may serve it, each model's
/// metadata, and the guardrails the key's requests go through. `mode`, `vendor` and `q` narrow
/// the models as the console's filters do. Besides admins, a project's users and keys may read
/// the hub of their project's keys, and a key whose scope is the inference API its own.
#[utoipa::path(get, path = "/hub", operation_id = "model_hub", tag = "Model hub", params(HubQuery),
    responses(
        (status = 200, description = "The models.", body = HubView),
        (status = 400, description = "No such key, or an unknown mode.", body = crate::error::AdminErrorBody),
        (status = 403, description = "A key whose scope is the inference API asked for another key.", body = crate::error::AdminErrorBody),
    ),
    security(("master_key" = []), ("admin_key" = []), ("virtual_key" = []), ("admin_session" = [])))]
pub(crate) async fn hub(
    State(state): State<AppState>,
    Extension(access): Extension<Access>,
    own: Option<Extension<OwnKey>>,
    Query(q): Query<HubQuery>,
) -> R<Json<HubView>> {
    let snap = state.registry.snapshot();
    let metadata = db::list_model_metadata(&state.db).await?;
    // A key whose scope is the inference API sees its own hub; a project's users and keys see
    // their projects' keys' hubs; admins any.
    let key_id = match own {
        Some(Extension(OwnKey(id))) if q.key.is_none_or(|k| k == id) => Some(id),
        Some(_) => return Err(AdminError::forbidden("this key may see only its own models")),
        None if access.projects().is_some() && q.key.is_none() => return Err(AdminError::bad_request("pick a key of your project")),
        None => q.key,
    };
    let key = match key_id {
        Some(id) => Some(
            snap.keys
                .values()
                .find(|k| k.id == id && (own.is_some() || access.sees(k.project_id)))
                .cloned()
                .ok_or_else(|| AdminError::bad_request("no such key"))?,
        ),
        None => None,
    };
    let key = key.as_deref();
    let (modes, vendors, caps) = (list(q.mode.as_deref()), list(q.vendor.as_deref()), list(q.capability.as_deref()));
    if let Some(m) = modes.iter().find(|m| **m != "router" && Mode::parse(m).is_none()) {
        return Err(AdminError::bad_request(format!("unknown mode '{m}'")));
    }
    if let Some(c) = caps.iter().find(|c| !models::is_capability(c) && !crate::model::valid_name(c)) {
        return Err(AdminError::bad_request(format!("unknown capability '{c}'")));
    }
    let term = q.q.as_deref().unwrap_or("").trim().to_lowercase();

    let mut names: Vec<&String> = snap
        .deployments
        .keys()
        .chain(snap.routers.keys())
        .chain(snap.aliases.keys())
        .filter(|m| key.is_none_or(|k| key_can_use(&snap, k, m)))
        .collect();
    names.sort();
    let all: Vec<HubModel> = names
        .into_iter()
        .map(|name| {
            let ds = models::deployments(&snap, key, name);
            let target = snap.aliases.get(name).map(|a| a.target.clone());
            let (kind, mode, tags, routes) = match snap.routers.get(snap.resolve(name)) {
                Some(r) => {
                    let routes: BTreeSet<String> = ds.iter().map(|d| d.model_name.clone()).collect();
                    ("router", Mode::Chat, r.tags.clone(), routes.into_iter().collect())
                }
                None => {
                    let tags: BTreeSet<&String> = ds.iter().flat_map(|d| &d.tags).collect();
                    ("model", ds.first().map_or(Mode::Chat, |d| d.mode), tags.into_iter().cloned().collect(), Vec::new())
                }
            };
            let metadata = metadata.get(name.as_str()).cloned().unwrap_or_else(|| Value::Object(Map::new()));
            let vendor = vendor_of(&metadata, name, ds.iter().map(|d| d.upstream_model.as_str()));
            HubModel {
                custom_capabilities: shared_capabilities(&ds),
                name: name.clone(),
                kind,
                mode,
                vendor,
                metadata,
                tags,
                target,
                described: models::describe_all(&ds),
                routes,
                deployments: hub_deployments(&ds),
                stats: None,
            }
        })
        .collect();
    let total = all.len();
    let mode_of = |m: &HubModel| if m.kind == "router" { "router".to_string() } else { m.mode.as_str().to_string() };
    let known_modes: Vec<String> = all.iter().map(mode_of).collect::<BTreeSet<_>>().into_iter().collect();
    let known_vendors: Vec<String> = all.iter().filter_map(|m| m.vendor.clone()).collect::<BTreeSet<_>>().into_iter().collect();
    let known_custom: Vec<String> =
        all.iter().flat_map(|m| m.custom_capabilities.iter().cloned()).collect::<BTreeSet<_>>().into_iter().collect();
    let mut models: Vec<HubModel> = all.into_iter().filter(|m| matches(m, &modes, &vendors, &caps, &term)).collect();
    if q.stats == Some(true) && !models.is_empty() {
        // A key sees its own traffic, a project's users their projects', admins everyone's.
        let own_key = own.as_ref().map(|Extension(OwnKey(id))| *id);
        let names: Vec<String> = models.iter().map(|m| m.name.clone()).collect();
        let mut by_model = stats(&state, &names, own_key, access.projects()).await?;
        for m in &mut models {
            m.stats = by_model.remove(&m.name);
        }
    }

    // Enforced first, in the order the request path runs them, then the rest on request.
    let project = key.and_then(|k| k.project_id).and_then(|p| snap.projects.get(&p));
    let enforced: Vec<(Uuid, &'static str)> = key
        .map(|k| k.guardrail_ids.iter().map(|id| (*id, "key")).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .chain(project.into_iter().flat_map(|p| p.guardrail_ids.iter().map(|id| (*id, "project"))))
        .collect();
    let mut guardrails: Vec<HubGuardrail> = Vec::new();
    let view = |g: &crate::guardrails::Active, applied| HubGuardrail {
        id: g.row.id,
        name: g.row.name.clone(),
        description: g.row.description.clone(),
        provider: g.row.provider.clone(),
        applied,
        prices: g.row.prices.0,
    };
    for (id, applied) in &enforced {
        if guardrails.iter().any(|g| g.id == *id) {
            continue;
        }
        if let Some(g) = snap.guardrails.values().find(|g| g.row.id == *id) {
            guardrails.push(view(g, applied));
        }
    }
    for g in snap.guardrails.values().filter(|g| !enforced.iter().any(|(id, _)| *id == g.row.id)) {
        guardrails.push(view(g, "on request"));
    }

    let key = key.map(|k| {
        let today = Utc::now();
        let status = if !k.enabled {
            "disabled"
        } else if k.expires_at.is_some_and(|e| e <= today) {
            "expired"
        } else if !k.project_active {
            "project inactive"
        } else if !matches!(k.scope, KeyScope::Inference | KeyScope::All) {
            "no inference"
        } else {
            "active"
        };
        HubKey {
            id: k.id,
            name: k.name.clone(),
            scope: k.scope,
            project: project.map(|p| p.name.clone()),
            status,
            allowed_models: k.allowed_models.clone(),
            allowed_tags: k.allowed_tags.clone(),
        }
    });
    Ok(Json(HubView { key, guardrails, models, total, modes: known_modes, vendors: known_vendors, custom_capabilities: known_custom }))
}

/// A model as the hub exports it: what a client needs to pick and call it, without its deployments.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct ExportModel {
    /// The public model name clients send as `model`.
    name: String,
    /// `router` for an auto router; absent for a model or an alias.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    kind: Option<&'static str>,
    mode: Mode,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    display_name: Option<String>,
    /// Who makes it: saved, or suggested from its names.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    vendor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    description: Option<String>,
    /// A model alias's target: the model name it serves.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    target: Option<String>,
    /// An auto router's models.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    routes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    max_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    max_output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    retires: Option<chrono::NaiveDate>,
    /// The capabilities all its deployments have, by path in the hub's `capabilities`, such as
    /// `image_input` or `thinking.supported`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    capabilities: Vec<&'static str>,
    /// The custom capabilities all its deployments carry.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    custom_capabilities: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tags: Vec<String>,
    /// What its deployments charge, once per distinct set of prices, the lowest priority's first.
    prices: Vec<ExportPrices>,
}

/// USD per 1M tokens, as `HubPrices`, each only where it is charged: an absent price is not charged.
#[derive(Serialize, PartialEq, utoipa::ToSchema)]
pub(crate) struct ExportPrices {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    input: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    output: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    cache_read: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    cache_write: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    long_context_threshold: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    long_input: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    long_output: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    long_cache_read: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    long_cache_write: Option<f64>,
    /// Per 1,000 rerank queries or web searches.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    per_k_searches: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    per_audio_minute: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    per_m_characters: Option<f64>,
}

impl From<&HubPrices> for ExportPrices {
    fn from(p: &HubPrices) -> Self {
        let charged = |v: f64| (v > 0.0).then_some(v);
        Self {
            input: charged(p.input),
            output: charged(p.output),
            cache_read: charged(p.cache_read),
            cache_write: charged(p.cache_write),
            long_context_threshold: p.long_context_threshold,
            long_input: p.long_input.and_then(charged),
            long_output: p.long_output.and_then(charged),
            long_cache_read: p.long_cache_read.and_then(charged),
            long_cache_write: p.long_cache_write.and_then(charged),
            per_k_searches: charged(p.per_k_searches),
            per_audio_minute: charged(p.per_audio_minute),
            per_m_characters: charged(p.per_m_characters),
        }
    }
}

fn export_model(m: HubModel) -> ExportModel {
    let text = |field: &str| m.metadata.get(field).and_then(Value::as_str).map(str::to_string);
    let (display_name, description) = (text("display_name"), text("description"));
    let mut prices: Vec<ExportPrices> = Vec::new();
    for p in m.deployments.iter().map(|d| ExportPrices::from(&d.prices)) {
        if !prices.contains(&p) {
            prices.push(p);
        }
    }
    let capabilities = m.described.capabilities.iter().flatten().filter(|(_, on)| **on == Some(true)).map(|(c, _)| *c).collect();
    ExportModel {
        name: m.name,
        kind: (m.kind == "router").then_some("router"),
        mode: m.mode,
        display_name,
        vendor: m.vendor,
        description,
        target: m.target,
        routes: m.routes,
        max_input_tokens: m.described.max_input_tokens,
        max_output_tokens: m.described.max_output_tokens,
        retires: m.described.retires,
        capabilities,
        custom_capabilities: m.custom_capabilities,
        tags: m.tags,
        prices,
    }
}

/// Export the model hub
///
/// The hub's models as a client needs them, without their deployments: each model's name,
/// metadata, mode, token limits, capabilities, tags and the prices its deployments charge. Takes
/// the hub's `key`, `mode`, `vendor`, `capability` and `q` (not `stats`), and is open to the same
/// callers.
#[utoipa::path(get, path = "/hub/export", operation_id = "export_model_hub", tag = "Model hub", params(HubQuery),
    responses(
        (status = 200, description = "The models.", body = Vec<ExportModel>),
        (status = 400, description = "No such key, or an unknown mode.", body = crate::error::AdminErrorBody),
        (status = 403, description = "A key whose scope is the inference API asked for another key.", body = crate::error::AdminErrorBody),
    ),
    security(("master_key" = []), ("admin_key" = []), ("virtual_key" = []), ("admin_session" = [])))]
pub(crate) async fn export(
    State(state): State<AppState>,
    Extension(access): Extension<Access>,
    own: Option<Extension<OwnKey>>,
    Query(mut q): Query<HubQuery>,
) -> R<Json<Vec<ExportModel>>> {
    q.stats = None;
    let Json(view) = hub(State(state), Extension(access), own, Query(q)).await?;
    Ok(Json(view.models.into_iter().map(export_model).collect()))
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct MetadataInput {
    /// The public model name: a deployment's or an auto router's.
    model: String,
    /// Replaces the stored metadata; `{}` removes it. `display_name`, `vendor` and `description`, when
    /// present, are strings.
    metadata: Map<String, Value>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct MetadataView {
    model: String,
    metadata: Value,
}

#[derive(Deserialize, utoipa::IntoParams)]
pub(crate) struct MetadataQuery {
    /// The public model names, as `claude-opus-5-5,gpt-6-luna`.
    models: String,
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct MetadataItem {
    model: String,
    /// `{}` without any: `display_name`, `vendor` and `description` to start with.
    metadata: Value,
    /// Who makes it: the metadata's `vendor`, or without one the vendor its names suggest.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    vendor: Option<String>,
    /// The capabilities all its enabled deployments have, as the hub's `capabilities` with only
    /// those that hold; absent when none do or nothing is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false, value_type = Option<BTreeMap<String, bool>>)]
    capabilities: Option<BTreeMap<&'static str, bool>>,
}

/// Get model metadata
///
/// The metadata of the named models, in the order asked, each with its vendor (saved, or
/// suggested from its name and its enabled deployments' upstream models) and the capabilities
/// the hub shows for it.
#[utoipa::path(get, path = "/model-metadata", operation_id = "get_model_metadata", tag = "Model hub", params(MetadataQuery),
    responses((status = 200, description = "The models' metadata.", body = Vec<MetadataItem>)),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(crate) async fn get_metadata(State(state): State<AppState>, Query(q): Query<MetadataQuery>) -> R<Json<Vec<MetadataItem>>> {
    let mut names: Vec<String> = Vec::new();
    for n in list(Some(&q.models)) {
        if !names.iter().any(|x| x == n) {
            names.push(n.to_string());
        }
    }
    let mut saved = db::model_metadata_of(&state.db, &names).await?;
    let snap = state.registry.snapshot();
    Ok(Json(
        names
            .into_iter()
            .map(|model| {
                let metadata = saved.remove(&model).unwrap_or_else(|| Value::Object(Map::new()));
                let ds = models::deployments(&snap, None, &model);
                let vendor = vendor_of(&metadata, &model, ds.iter().map(|d| d.upstream_model.as_str()));
                let capabilities = models::describe_all(&ds)
                    .capabilities
                    .map(|cs| cs.into_iter().filter(|(_, on)| *on == Some(true)).map(|(c, _)| (c, true)).collect::<BTreeMap<_, _>>())
                    .filter(|cs| !cs.is_empty());
                MetadataItem { model, metadata, vendor, capabilities }
            })
            .collect(),
    ))
}

/// The largest metadata accepted, serialized.
const MAX_METADATA_BYTES: usize = 16 * 1024;

/// Trims the text fields and drops empty ones.
pub(super) fn check_metadata(mut m: Map<String, Value>) -> R<Map<String, Value>> {
    for (field, max) in [("display_name", 100), ("vendor", 100), ("description", 4000)] {
        match m.get(field) {
            None | Some(Value::Null) => {
                m.remove(field);
            }
            Some(Value::String(s)) => {
                let s = s.trim();
                if s.chars().count() > max {
                    return Err(AdminError::bad_request(format!("{field} must be up to {max} characters")));
                }
                if s.is_empty() {
                    m.remove(field);
                } else {
                    m.insert(field.to_string(), Value::String(s.to_string()));
                }
            }
            Some(_) => return Err(AdminError::bad_request(format!("{field} must be a string"))),
        }
    }
    if serde_json::to_vec(&m).map_or(0, |b| b.len()) > MAX_METADATA_BYTES {
        return Err(AdminError::bad_request(format!("metadata must be up to {} KB", MAX_METADATA_BYTES / 1024)));
    }
    Ok(m)
}

/// Set model metadata
///
/// Replaces a model's metadata, such as `display_name` and `description`; `{}` removes it.
#[utoipa::path(put, path = "/model-metadata", operation_id = "set_model_metadata", tag = "Model hub", request_body = MetadataInput,
    responses(
        (status = 200, description = "The stored metadata.", body = MetadataView),
        (status = 400, description = "Invalid metadata, or no such model.", body = crate::error::AdminErrorBody),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(crate) async fn set_metadata(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(i): Json<MetadataInput>,
) -> R<Json<MetadataView>> {
    let model = i.model.trim().to_string();
    let (deployments, routers, aliases) =
        tokio::try_join!(db::list_deployments(&state.db), db::list_routers(&state.db), db::list_aliases(&state.db))?;
    let named = deployments.iter().any(|d| d.model_name == model)
        || routers.iter().any(|r| r.model_name == model)
        || aliases.iter().any(|a| a.model_name == model);
    if !named {
        return Err(AdminError::bad_request(format!("no deployment, router or alias is named '{model}'")));
    }
    let metadata = Value::Object(check_metadata(i.metadata)?);
    let before = db::get_model_metadata(&state.db, &model).await?.unwrap_or_else(|| Value::Object(Map::new()));
    db::set_model_metadata(&state.db, &model, &metadata).await?;
    let changes = audit::changes(&before, &metadata);
    if !changes.is_empty() {
        audit::Event::new("model", "update").named(&model).details(changes.into()).record(&state, &actor).await;
    }
    Ok(Json(MetadataView { model, metadata }))
}

/// Reasoning efforts
///
/// The reasoning efforts each chat model takes, by its price catalog entry: those every one of
/// its deployments accepts. Aliases are listed with their target's; auto routers, which set
/// their own per route, are left out. For everyone signed in to the console.
#[utoipa::path(get, path = "/efforts", operation_id = "model_efforts", tag = "Model hub",
    responses((status = 200, description = "Model name to its efforts, in order.", body = std::collections::BTreeMap<String, Vec<crate::model::ReasoningEffort>>)),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
// Aliases of auto routers are left out with the routers.
pub(crate) async fn efforts(State(state): State<AppState>) -> Json<BTreeMap<String, Vec<ReasoningEffort>>> {
    Json(efforts_by_model(&state.registry.snapshot()))
}

/// Each chat model name, aliases by their target's, with the efforts every one of its enabled
/// deployments takes (see `Deployment::efforts`).
fn efforts_by_model(snap: &Snapshot) -> BTreeMap<String, Vec<ReasoningEffort>> {
    let of = |model: &str| -> Option<Vec<ReasoningEffort>> {
        let chat: Vec<_> = snap.deployments.get(model)?.iter().filter(|d| d.mode == Mode::Chat).map(|d| d.efforts()).collect();
        let (first, rest) = chat.split_first()?;
        Some(first.iter().copied().filter(|e| rest.iter().all(|r| r.contains(e))).collect())
    };
    let names = snap.deployments.keys().chain(snap.aliases.keys());
    names.filter_map(|name| Some((name.clone(), of(snap.resolve(name))?))).collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn efforts_are_shared_by_every_deployment_and_follow_aliases() {
        use std::sync::Arc;

        use crate::{
            ir::Effort::*,
            model::{ApiStyle, ModelAlias, ProviderKind},
            providers::tests::deployment,
        };
        let with = |model: &str, style, catalog: Value| {
            let mut d = deployment(ProviderKind::Azure, style, "m");
            d.model_name = model.into();
            d.catalog = Some(Arc::new(catalog));
            Arc::new(d)
        };
        let luna = json!({ "supports_minimal_reasoning_effort": false, "supports_none_reasoning_effort": true, "supports_max_reasoning_effort": true });
        let mut snap = Snapshot::default();
        snap.deployments.insert("luna".into(), vec![with("luna", ApiStyle::Responses, luna)]);
        // Two deployments: only what both take.
        snap.deployments.insert(
            "mixed".into(),
            vec![
                with("mixed", ApiStyle::Chat, json!({ "supports_minimal_reasoning_effort": true })),
                with("mixed", ApiStyle::Chat, json!({})),
            ],
        );
        snap.aliases.insert("gpt-4".into(), Arc::new(ModelAlias { model_name: "gpt-4".into(), target: "luna".into() }));
        snap.aliases.insert("smart".into(), Arc::new(ModelAlias { model_name: "smart".into(), target: "auto".into() }));
        let efforts = efforts_by_model(&snap);
        assert_eq!(efforts["luna"], [None, Low, Medium, High, Max]);
        assert_eq!(efforts["gpt-4"], efforts["luna"], "an alias takes its target's");
        assert_eq!(efforts["mixed"], [Low, Medium, High]);
        assert!(!efforts.contains_key("smart"), "an alias of a router is left out");
    }

    #[test]
    fn metadata_text_is_trimmed_and_checked() {
        let m = |v: Value| v.as_object().unwrap().clone();
        let out = check_metadata(m(json!({ "display_name": "  Opus  ", "vendor": " Anthropic ", "description": " ", "family": "claude" })))
            .unwrap();
        assert_eq!(Value::Object(out), json!({ "display_name": "Opus", "vendor": "Anthropic", "family": "claude" }));
        assert!(check_metadata(m(json!({ "vendor": ["Anthropic"] }))).is_err());
        assert!(check_metadata(m(json!({ "display_name": 3 }))).is_err());
        assert!(check_metadata(m(json!({ "display_name": "x".repeat(101) }))).is_err());
        assert!(check_metadata(m(json!({ "notes": "x".repeat(MAX_METADATA_BYTES) }))).is_err());
    }

    #[test]
    fn filters_match_mode_vendor_and_text() {
        let model = |name: &str, kind, mode, metadata: Value| HubModel {
            name: name.into(),
            kind,
            mode,
            vendor: metadata.get("vendor").and_then(Value::as_str).map(str::to_string),
            metadata,
            tags: Vec::new(),
            custom_capabilities: Vec::new(),
            target: None,
            described: models::describe_all(&[]),
            routes: Vec::new(),
            deployments: Vec::new(),
            stats: None,
        };
        let opus = model("opus", "model", Mode::Chat, json!({ "vendor": "Anthropic", "description": "Deep reasoning" }));
        let embed = model("embed", "model", Mode::Embedding, json!({}));
        let auto = model("auto", "router", Mode::Chat, json!({}));

        assert!(matches(&opus, &["chat"], &[], &[], ""));
        assert!(!matches(&auto, &["chat"], &[], &[], ""), "a router is not a chat model");
        assert!(matches(&auto, &["embedding", "router"], &[], &[], ""));
        assert!(matches(&embed, &["embedding"], &["Anthropic", "-"], &[], ""), "- is no vendor");
        assert!(!matches(&embed, &[], &["Anthropic"], &[], ""));
        assert!(matches(&opus, &[], &["Anthropic"], &[], "reasoning"));
        assert!(!matches(&opus, &[], &[], &[], "gpt"));

        let mut seeing = model("seeing", "model", Mode::Chat, json!({}));
        seeing.described.capabilities =
            Some([("image_input", Some(true)), ("pdf_input", None), ("thinking.supported", Some(false))].into());
        assert!(matches(&seeing, &[], &[], &["image_input"], ""));
        assert!(!matches(&seeing, &[], &[], &["image_input", "thinking.supported"], ""), "all of them");
        assert!(!matches(&seeing, &[], &[], &["pdf_input"], ""), "unknown is not supported");
        assert!(!matches(&embed, &[], &[], &["image_input"], ""), "no capabilities at all");
        seeing.custom_capabilities = vec!["eu-hosted".into()];
        assert!(matches(&seeing, &[], &[], &["image_input", "eu-hosted"], ""), "built-in and custom together");
        assert!(!matches(&opus, &[], &[], &["eu-hosted"], ""));
    }

    #[test]
    fn vendors_are_suggested_from_names() {
        assert_eq!(suggest_vendor("claude-opus-5-5 eu.anthropic.claude-opus"), Some("Anthropic"));
        assert_eq!(suggest_vendor("auto-gpt gpt-6-luna"), Some("OpenAI"));
        assert_eq!(suggest_vendor("o3-mini"), Some("OpenAI"));
        assert_eq!(suggest_vendor("foo3-mini"), None, "o3 is a whole word");
        assert_eq!(suggest_vendor("meta-llama-4"), Some("Meta"));
        assert_eq!(suggest_vendor("metadata-tool"), None, "meta is a whole word");
        assert_eq!(suggest_vendor("amazon.nova-pro-v1:0"), Some("Amazon"));
        assert_eq!(suggest_vendor("supernova-1"), None, "nova- starts a word");
        assert_eq!(suggest_vendor("glm-5.3"), Some("Zhipu AI"));
        assert_eq!(suggest_vendor("cohere-rerank-v3-5"), Some("Cohere"));
        assert_eq!(suggest_vendor("my-model"), None);
    }

    #[test]
    fn exported_prices_leave_out_what_is_not_charged() {
        let rerank = HubPrices {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
            long_context_threshold: None,
            long_input: None,
            long_output: None,
            long_cache_read: None,
            long_cache_write: None,
            per_k_searches: 2.0,
            per_audio_minute: 0.0,
            per_m_characters: 0.0,
        };
        assert_eq!(serde_json::to_value(ExportPrices::from(&rerank)).unwrap(), json!({ "per_k_searches": 2.0 }));
        let chat = HubPrices {
            input: 3.0,
            output: 15.0,
            cache_read: 0.3,
            long_context_threshold: Some(200_000),
            long_input: Some(6.0),
            long_output: Some(22.5),
            ..rerank
        };
        assert_eq!(
            serde_json::to_value(ExportPrices::from(&chat)).unwrap(),
            json!({ "input": 3.0, "output": 15.0, "cache_read": 0.3, "long_context_threshold": 200_000,
                    "long_input": 6.0, "long_output": 22.5, "per_k_searches": 2.0 })
        );
    }
}
