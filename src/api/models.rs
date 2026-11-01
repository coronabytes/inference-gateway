//! `GET /v1/models`: the models, auto routers and model aliases a key may use, OpenAI- or Anthropic-shaped.
//! `GET /v1/models-extended` lists the same models OpenAI-shaped, with a few more fields.
//!
//! The Anthropic shape also describes each model: token limits, capabilities and a scheduled
//! retirement, from each deployment's price catalog entry (LiteLLM's) and from what the gateway
//! does with a Messages request to that deployment. Where a model has several deployments, or is
//! an auto router, it is described by what holds for all of them.

use std::collections::{BTreeMap, BTreeSet};

use axum::{
    Json,
    extract::State,
    http::HeaderMap,
    response::{IntoResponse, Response},
};
use chrono::NaiveDate;
use serde_json::{Map, Value, json};

use super::{key_can_use, web_search_allowed};
use crate::{
    auth::ClientKey,
    model::{ApiStyle, Deployment, Mode, ProviderKind, VirtualKey},
    registry::Snapshot,
    state::AppState,
};

/// `GET /v1/models` — Anthropic-shaped when the client sends `anthropic-version`, OpenAI-shaped
/// otherwise.
pub async fn list_models(State(state): State<AppState>, ClientKey(key): ClientKey, headers: HeaderMap) -> Response {
    serve(&state, &key, &headers)
}

/// `GET /v1/models-extended` — OpenAI-shaped, with each model's mode, token limits, structured
/// output support and tags.
pub async fn list_models_extended(State(state): State<AppState>, ClientKey(key): ClientKey) -> Response {
    serve_extended(&state, &key)
}

/// The model names `key` may use, sorted.
pub(crate) fn names<'a>(snap: &'a Snapshot, key: &VirtualKey) -> Vec<&'a String> {
    let mut names: Vec<&String> =
        snap.deployments.keys().chain(snap.routers.keys()).chain(snap.aliases.keys()).filter(|m| key_can_use(snap, key, m)).collect();
    names.sort();
    names
}

/// The models `key` may use, as `GET /v1/models` answers. Also used by the admin playground.
pub fn serve(state: &AppState, key: &VirtualKey, headers: &HeaderMap) -> Response {
    let snap = state.registry.snapshot();
    let names = names(&snap, key);
    let started = state.started_at;
    let body = if headers.contains_key("anthropic-version") {
        let data: Vec<Value> = names
            .iter()
            .map(|n| {
                let info = describe(&snap, key, n);
                json!({
                    "type": "model",
                    "id": n,
                    "display_name": n,
                    "created_at": started.to_rfc3339(),
                    "capabilities": info.capabilities.as_ref().map(Caps::to_json),
                    "max_input_tokens": info.max_input_tokens,
                    "max_tokens": info.max_tokens,
                    // A scheduled retirement makes a model deprecated; when it was deprecated is
                    // not known.
                    "lifecycle": if info.retires.is_some() { "deprecated" } else { "active" },
                    "deprecated_at": null,
                    "retires_at": info.retires.map(|d| format!("{d}T00:00:00Z")),
                    "line": null,
                })
            })
            .collect();
        json!({ "data": data, "has_more": false, "first_id": names.first(), "last_id": names.last() })
    } else {
        let data: Vec<Value> = names
            .iter()
            .map(|n| {
                json!({
                    "id": n,
                    "object": "model",
                    "created": started.timestamp(),
                    "owned_by": "inference-gateway",
                    "shutdown_date": describe(&snap, key, n).retires.map(|d| d.to_string()),
                })
            })
            .collect();
        json!({ "object": "list", "data": data })
    };
    Json(body).into_response()
}

/// The models `key` may use, as `GET /v1/models-extended` answers. Also used by the admin
/// playground.
pub fn serve_extended(state: &AppState, key: &VirtualKey) -> Response {
    let snap = state.registry.snapshot();
    let data: Vec<Value> = names(&snap, key)
        .into_iter()
        .map(|n| {
            let info = describe(&snap, key, n);
            let deployments = deployments(&snap, Some(key), n);
            let (mode, tags) = match snap.routers.get(snap.resolve(n)) {
                // Auto routers serve the chat endpoints, and keys reach them by their own tags.
                Some(r) => (Mode::Chat, r.tags.clone()),
                None => {
                    let tags: BTreeSet<&String> = deployments.iter().flat_map(|d| &d.tags).collect();
                    (deployments.first().map_or(Mode::Chat, |d| d.mode), tags.into_iter().cloned().collect())
                }
            };
            let structured = info.capabilities.as_ref().and_then(|c| c.0.get("structured_outputs").copied().flatten());
            json!({
                "id": n,
                "object": "model",
                "created": state.started_at.timestamp(),
                "owned_by": "inference-gateway",
                "shutdown_date": info.retires.map(|d| d.to_string()),
                "mode": mode,
                "max_input_tokens": info.max_input_tokens,
                "max_output_tokens": info.max_tokens,
                "structured_outputs": structured == Some(true),
                "tags": tags,
            })
        })
        .collect();
    Json(json!({ "object": "list", "data": data })).into_response()
}

/// What is known about a model. `None` is unknown.
#[derive(Debug, Default)]
struct Info {
    max_input_tokens: Option<u64>,
    max_tokens: Option<u64>,
    retires: Option<NaiveDate>,
    /// Only for chat models.
    capabilities: Option<Caps>,
}

impl Info {
    fn of(d: &Deployment) -> Self {
        let Some(c) = d.catalog.as_deref() else { return Self::default() };
        let tokens = |k: &str| c.get(k).and_then(Value::as_f64).filter(|n| *n >= 1.0).map(|n| n as u64);
        Self {
            max_input_tokens: tokens("max_input_tokens"),
            max_tokens: tokens("max_output_tokens").or_else(|| tokens("max_tokens")),
            retires: c.get("deprecation_date").and_then(Value::as_str).and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()),
            capabilities: (d.mode == Mode::Chat).then(|| Caps::of(d.style, d.provider, c)),
        }
    }

    /// What holds for both: the lower limits, the capabilities both have, and a retirement only
    /// once both are retired.
    fn and(self, o: Self) -> Self {
        Self {
            max_input_tokens: self.max_input_tokens.zip(o.max_input_tokens).map(|(a, b)| a.min(b)),
            max_tokens: self.max_tokens.zip(o.max_tokens).map(|(a, b)| a.min(b)),
            retires: self.retires.zip(o.retires).map(|(a, b)| a.max(b)),
            capabilities: self.capabilities.zip(o.capabilities).map(|(a, b)| a.and(&b)),
        }
    }
}

/// The deployments `key` reaches through a model name: its own, or for an auto router those of
/// every model it routes to, and for an alias its target's. Without a key, all of them.
pub(crate) fn deployments<'a>(snap: &'a Snapshot, key: Option<&VirtualKey>, name: &'a str) -> Vec<&'a Deployment> {
    let target = snap.resolve(name);
    let models: Vec<&str> = match snap.routers.get(target) {
        Some(r) => r.models().filter(|m| key.is_none_or(|k| k.allows_model(m))).collect(),
        None => vec![target],
    };
    models
        .iter()
        .filter_map(|m| snap.deployments.get(*m))
        .flatten()
        .map(|d| &**d)
        .filter(|d| key.is_none_or(|k| k.allows_deployment(d)))
        .collect()
}

/// A model as `key` reaches it, described by what holds for all its deployments.
fn describe(snap: &Snapshot, key: &VirtualKey, name: &str) -> Info {
    let mut info = deployments(snap, Some(key), name).into_iter().map(Info::of).reduce(Info::and).unwrap_or_default();
    // The key or its project may forbid what the model could do.
    if let Some(caps) = info.capabilities.as_mut().filter(|_| !web_search_allowed(snap, key)) {
        caps.0.insert("server_tools.supported", Some(false));
        caps.0.insert("server_tools.web_search", Some(false));
    }
    info
}

/// Whether a request of `key` to model `name` can search the web: each deployment it may reach
/// has a web search, and neither the key nor its project forbids it.
pub(crate) fn web_search(snap: &Snapshot, key: &VirtualKey, name: &str) -> bool {
    describe(snap, key, name).capabilities.is_some_and(|c| c.0.get("server_tools.web_search").copied().flatten() == Some(true))
}

/// What the console's model hub shows of a model or a deployment: token limits, a scheduled
/// retirement, and for chat models capabilities by `CAPS` path (`None`: unknown).
#[derive(Debug, Default, serde::Serialize, utoipa::ToSchema)]
pub(crate) struct Described {
    pub max_input_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub retires: Option<NaiveDate>,
    #[schema(value_type = Option<BTreeMap<String, Option<bool>>>)]
    pub capabilities: Option<BTreeMap<&'static str, Option<bool>>>,
}

impl From<Info> for Described {
    fn from(i: Info) -> Self {
        Self {
            max_input_tokens: i.max_input_tokens,
            max_output_tokens: i.max_tokens,
            retires: i.retires,
            capabilities: i.capabilities.map(|c| c.0),
        }
    }
}

/// `deployments`, described by what holds for all of them.
pub(crate) fn describe_all(ds: &[&Deployment]) -> Described {
    ds.iter().map(|d| Info::of(d)).reduce(Info::and).unwrap_or_default().into()
}

/// One deployment, described.
pub(crate) fn describe_one(d: &Deployment) -> Described {
    Info::of(d).into()
}

/// Whether `path` names one of the capabilities a model is described with.
pub(crate) fn is_capability(path: &str) -> bool {
    CAPS.iter().any(|(p, _)| *p == path)
}

/// Anthropic's capabilities, by path in its `capabilities` object, and whether the field may be
/// `null`. A path ending in `supported` is a group's own flag; any other is a `{"supported": …}`.
const CAPS: &[(&str, bool)] = &[
    ("batch", false),
    ("citations", false),
    ("code_execution", false),
    ("context_management.supported", false),
    ("context_management.clear_thinking_20251015", true),
    ("context_management.clear_tool_uses_20250919", true),
    ("context_management.compact_20260112", true),
    ("effort.supported", false),
    ("effort.low", false),
    ("effort.medium", false),
    ("effort.high", false),
    ("effort.max", false),
    ("effort.xhigh", true),
    ("image_input", false),
    ("pdf_input", false),
    ("server_tools.supported", false),
    ("server_tools.code_execution", false),
    ("server_tools.web_search", false),
    ("structured_outputs", false),
    ("thinking.supported", false),
    ("thinking.types.adaptive", false),
    ("thinking.types.between_tools", false),
    ("thinking.types.disabled", false),
    ("thinking.types.enabled", false),
];

/// Capabilities by `CAPS` path. `None` is unknown: `null` where the field may be, `false` where
/// it may not.
#[derive(Debug, Clone, PartialEq)]
struct Caps(BTreeMap<&'static str, Option<bool>>);

impl Caps {
    fn of(style: ApiStyle, provider: ProviderKind, c: &Value) -> Self {
        // LiteLLM leaves out flags that are false.
        let has = |k: &str| c.get(k).and_then(Value::as_bool) == Some(true);
        let reasoning = has("supports_reasoning");
        // Each upstream API has a web search (see `ir::WebSearch`) but Bedrock's: Converse cannot
        // ask for one, and Claude through InvokeModel refuses the tool.
        let web_search = has("supports_web_search") && provider != ProviderKind::Bedrock;
        let caps: Vec<(&'static str, Option<bool>)> = if style == ApiStyle::Anthropic {
            // Forwarded as is: the provider decides, and the catalog says what it supports.
            let effort = reasoning && has("supports_output_config");
            let compact = has("supports_anthropic_compaction");
            let adaptive = has("supports_adaptive_thinking");
            vec![
                ("citations", None),
                ("code_execution", None),
                ("context_management.supported", Some(compact)),
                ("context_management.clear_thinking_20251015", None),
                ("context_management.clear_tool_uses_20250919", None),
                ("context_management.compact_20260112", Some(compact)),
                ("effort.supported", Some(effort)),
                ("effort.low", Some(effort)),
                ("effort.medium", Some(effort)),
                ("effort.high", Some(effort)),
                ("effort.max", Some(effort && has("supports_max_reasoning_effort"))),
                ("effort.xhigh", Some(effort && has("supports_xhigh_reasoning_effort"))),
                ("image_input", Some(has("supports_vision"))),
                ("pdf_input", Some(has("supports_pdf_input"))),
                ("server_tools.supported", Some(web_search)),
                ("server_tools.code_execution", None),
                ("server_tools.web_search", Some(web_search)),
                ("thinking.supported", Some(reasoning)),
                ("thinking.types.adaptive", Some(adaptive)),
                ("thinking.types.between_tools", None),
                ("thinking.types.disabled", Some(!has("thinking_always_on"))),
                ("thinking.types.enabled", Some(reasoning)),
            ]
        } else {
            // Translated (see `anthropic::parse_request`): base64 documents become the provider's
            // file input, web search becomes the provider's own, citations and other server tools
            // are refused, context management is dropped, and effort and thinking become the
            // provider's reasoning settings.
            vec![
                ("citations", Some(false)),
                ("code_execution", Some(false)),
                ("context_management.supported", Some(false)),
                ("context_management.clear_thinking_20251015", Some(false)),
                ("context_management.clear_tool_uses_20250919", Some(false)),
                ("context_management.compact_20260112", Some(false)),
                ("effort.supported", Some(reasoning)),
                ("effort.low", Some(reasoning)),
                ("effort.medium", Some(reasoning)),
                ("effort.high", Some(reasoning)),
                ("effort.max", Some(reasoning)),
                ("effort.xhigh", Some(reasoning)),
                ("image_input", Some(has("supports_vision"))),
                ("pdf_input", Some(has("supports_pdf_input"))),
                ("server_tools.supported", Some(web_search)),
                ("server_tools.code_execution", Some(false)),
                ("server_tools.web_search", Some(web_search)),
                ("thinking.supported", Some(reasoning)),
                ("thinking.types.adaptive", Some(reasoning)),
                ("thinking.types.between_tools", Some(true)),
                ("thinking.types.disabled", Some(true)),
                ("thinking.types.enabled", Some(reasoning)),
            ]
        };
        let mut map: BTreeMap<_, _> = caps.into_iter().collect();
        // The gateway has no Batch API.
        map.insert("batch", Some(false));
        map.insert("structured_outputs", Some(has("supports_response_schema")));
        Self(map)
    }

    /// Supported by both; unknown unless one of them rules it out.
    fn and(&self, o: &Self) -> Self {
        let get = |c: &Self, p| c.0.get(p).copied().flatten();
        Self(
            CAPS.iter()
                .map(|(p, _)| {
                    let v = match (get(self, p), get(o, p)) {
                        (Some(false), _) | (_, Some(false)) => Some(false),
                        (Some(true), Some(true)) => Some(true),
                        _ => None,
                    };
                    (*p, v)
                })
                .collect(),
        )
    }

    fn to_json(&self) -> Value {
        let mut root = Map::new();
        for (path, nullable) in CAPS {
            let v = self.0.get(path).copied().flatten();
            let (parents, last) = path.rsplit_once('.').map_or((None, *path), |(p, l)| (Some(p), l));
            let mut node = &mut root;
            for p in parents.into_iter().flat_map(|p| p.split('.')) {
                node = node.entry(p).or_insert_with(|| json!({})).as_object_mut().expect("capability groups are objects");
            }
            let value = match v {
                _ if last == "supported" => json!(v == Some(true)),
                Some(b) => json!({ "supported": b }),
                None if *nullable => Value::Null,
                None => json!({ "supported": false }),
            };
            node.insert(last.into(), value);
        }
        Value::Object(root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_capability_is_described() {
        for style in [ApiStyle::Anthropic, ApiStyle::Responses] {
            let caps = Caps::of(style, ProviderKind::Openai, &json!({}));
            let mut paths: Vec<&str> = caps.0.keys().copied().collect();
            let mut all: Vec<&str> = CAPS.iter().map(|(p, _)| *p).collect();
            paths.sort();
            all.sort();
            assert_eq!(paths, all, "{style:?}");
        }
    }

    #[test]
    fn capabilities_render_in_anthropics_shape() {
        let catalog = json!({
            "supports_reasoning": true, "supports_output_config": true, "supports_adaptive_thinking": true,
            "supports_xhigh_reasoning_effort": true, "supports_vision": true, "supports_pdf_input": true,
        });
        let v = Caps::of(ApiStyle::Anthropic, ProviderKind::Anthropic, &catalog).to_json();
        assert_eq!(v["batch"], json!({ "supported": false }));
        assert_eq!(v["citations"], json!({ "supported": false }));
        assert_eq!(v["pdf_input"], json!({ "supported": true }));
        assert_eq!(v["effort"]["supported"], json!(true));
        assert_eq!(v["effort"]["xhigh"], json!({ "supported": true }));
        assert_eq!(v["effort"]["max"], json!({ "supported": false }));
        assert_eq!(v["thinking"]["types"]["adaptive"], json!({ "supported": true }));
        assert!(v["thinking"]["types"].get("supported").is_none());
        assert_eq!(v["context_management"]["clear_thinking_20251015"], Value::Null);

        // Translated: base64 documents become the provider's file input, so the model decides.
        let v = Caps::of(ApiStyle::Responses, ProviderKind::Azure, &catalog).to_json();
        assert_eq!(v["pdf_input"], json!({ "supported": true }));
        assert_eq!(v["image_input"], json!({ "supported": true }));
        assert_eq!(v["effort"]["max"], json!({ "supported": true }));
    }

    #[test]
    fn web_search_where_the_upstream_has_one() {
        let catalog = json!({ "supports_web_search": true });
        let web = |style, provider| Caps::of(style, provider, &catalog).to_json()["server_tools"].clone();
        let on = json!({ "supported": true, "code_execution": { "supported": false }, "web_search": { "supported": true } });
        assert_eq!(web(ApiStyle::Responses, ProviderKind::Azure), on);
        assert_eq!(web(ApiStyle::Chat, ProviderKind::Vertex), on);
        assert_eq!(web(ApiStyle::Anthropic, ProviderKind::Anthropic)["web_search"], json!({ "supported": true }));
        assert_eq!(web(ApiStyle::Anthropic, ProviderKind::Bedrock)["web_search"], json!({ "supported": false }));
        assert_eq!(web(ApiStyle::Converse, ProviderKind::Bedrock)["web_search"], json!({ "supported": false }));
        let none = Caps::of(ApiStyle::Chat, ProviderKind::Vertex, &json!({})).to_json();
        assert_eq!(none["server_tools"]["web_search"], json!({ "supported": false }));
    }

    #[test]
    fn several_deployments_are_described_by_what_all_of_them_support() {
        let info = |c: Value, style| {
            let caps = Caps::of(style, ProviderKind::Openai, &c);
            Info {
                max_input_tokens: c["max_input_tokens"].as_u64(),
                max_tokens: c["max_output_tokens"].as_u64(),
                retires: c["deprecation_date"].as_str().and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()),
                capabilities: Some(caps),
            }
        };
        let native =
            json!({ "max_input_tokens": 200000, "max_output_tokens": 64000, "supports_pdf_input": true, "deprecation_date": "2027-01-01" });
        let translated = json!({ "max_input_tokens": 100000, "max_output_tokens": 128000, "supports_vision": true });
        let both = info(native.clone(), ApiStyle::Anthropic).and(info(translated, ApiStyle::Chat));
        assert_eq!((both.max_input_tokens, both.max_tokens), (Some(100000), Some(64000)));
        // Still served by the deployment with no retirement scheduled.
        assert_eq!(both.retires, None);
        let caps = both.capabilities.unwrap().to_json();
        assert_eq!(caps["pdf_input"], json!({ "supported": false }));
        assert_eq!(caps["image_input"], json!({ "supported": false }));
        // Unknown for the native deployment and ruled out for the translated one.
        assert_eq!(caps["citations"], json!({ "supported": false }));

        let twice = info(native.clone(), ApiStyle::Anthropic).and(info(native, ApiStyle::Anthropic));
        assert_eq!(twice.retires, NaiveDate::from_ymd_opt(2027, 1, 1));
        assert!(Info::default().and(twice).capabilities.is_none());
    }
}
