//! The console's model hub: every model the gateway serves, or those one key may use, with what
//! its deployments say about it (token limits, capabilities, prices), the guardrails the key's
//! requests go through, and each model's metadata (a display name and a description, kept in
//! `model_metadata` by public model name).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use axum::{
    Extension, Json,
    extract::{Query, State},
    http::StatusCode,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

use super::R;
use crate::{
    admin_auth::{Access, OwnKey},
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
    /// Only models with all of these capabilities, by path in `capabilities`, as
    /// `image_input,thinking.supported`.
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

/// Whether a model passes the hub's filters.
fn matches(m: &HubModel, modes: &[&str], vendors: &[&str], caps: &[&str], term: &str) -> bool {
    let text = |field: &str| m.metadata.get(field).and_then(Value::as_str);
    let mode_ok = modes.is_empty() || modes.contains(&if m.kind == "router" { "router" } else { m.mode.as_str() });
    let vendor_ok = vendors.is_empty() || vendors.contains(&m.vendor.as_deref().unwrap_or("-"));
    let has = |c: &&str| m.described.capabilities.as_ref().is_some_and(|cs| cs.get(*c).copied().flatten() == Some(true));
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
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct HubKey {
    id: Uuid,
    name: String,
    scope: KeyScope,
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
    description: Option<String>,
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
    vendor: Option<String>,
    tags: Vec<String>,
    /// A model alias's target: the model name it serves.
    #[serde(skip_serializing_if = "Option::is_none")]
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
    p50_latency_ms: Option<i32>,
    /// Output tokens per second while generating: after the first token for streamed answers.
    #[serde(skip_serializing_if = "Option::is_none")]
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
    let _scan = super::LOG_SCANS.acquire().await.map_err(|_| AdminError(StatusCode::SERVICE_UNAVAILABLE, "shutting down".into()))?;
    let (hours, speeds) = tokio::try_join!(
        db::model_hours(&state.db, since, models, key_id, projects),
        db::model_speeds(&state.db, since, models, key_id, projects),
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
    provider: String,
    upstream_model: String,
    priority: i32,
    weight: u32,
    /// Tried before deployments with a higher priority, which serve only when these fail.
    first_choice: bool,
    /// The deployment's tags, which a key's allowed tags must cover.
    tags: Vec<String>,
    #[serde(flatten)]
    described: models::Described,
    prices: HubPrices,
}

/// USD per 1M tokens; `per_k_searches` per 1,000 rerank queries or web searches.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct HubPrices {
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
    /// Requests whose input exceeds this many tokens are billed at the long-context prices.
    #[serde(skip_serializing_if = "Option::is_none")]
    long_context_threshold: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    long_input: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    long_output: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    long_cache_read: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    long_cache_write: Option<f64>,
    per_k_searches: f64,
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
    }
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
            described: models::describe_one(d),
            prices: prices(d),
        })
        .collect();
    out.sort_by(|a, b| {
        a.priority.cmp(&b.priority).then_with(|| a.model.cmp(&b.model)).then_with(|| a.upstream_model.cmp(&b.upstream_model))
    });
    out
}

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
        Some(_) => return Err(AdminError(StatusCode::FORBIDDEN, "this key may see only its own models".into())),
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
    if let Some(c) = caps.iter().find(|c| !models::is_capability(c)) {
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
            let saved = metadata.get("vendor").and_then(Value::as_str).map(str::to_string);
            let suggested = || {
                let names =
                    std::iter::once(name.as_str()).chain(ds.iter().map(|d| d.upstream_model.as_str())).collect::<Vec<_>>().join(" ");
                suggest_vendor(&names.to_lowercase()).map(str::to_string)
            };
            let vendor = saved.or_else(suggested);
            HubModel {
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
    Ok(Json(HubView { key, guardrails, models, total, modes: known_modes, vendors: known_vendors }))
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

/// `GET /admin/api/efforts`: the reasoning efforts each chat model takes, for the console's
/// effort selectors, to everyone signed in (users included). Routers set their own per route and
/// are left out, as are aliases of routers.
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
}
