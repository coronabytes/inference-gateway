//! Price catalog: LiteLLM's model prices, imported on a schedule or on demand and stored per key
//! as JSONB.
//! The admin UI browses it and uses it to fill in deployment prices; billing itself always
//! uses the prices stored on each deployment.

use std::{
    collections::{BTreeSet, HashMap},
    time::Duration,
};

use axum::{
    Extension, Json,
    extract::{Query, State},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    audit::{self, Actor},
    db,
    error::AdminError,
    model::{DeploymentRow, ProviderKind},
    state::AppState,
    sync::broadcast_reload,
};

type R<T> = Result<T, AdminError>;

/// LiteLLM's catalog. Fixed, so the admin API cannot be used to fetch arbitrary URLs.
const SOURCE_URL: &str = "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
const MAX_SOURCE_BYTES: usize = 32 * 1024 * 1024;
const MAX_KEY_LEN: usize = 300;
/// Held while the catalog is imported, so replicas never import at once.
const IMPORT_LOCK: i64 = 0x6777_7072_6963; // "gwpric"
/// How often each replica looks whether a scheduled import is due.
const SCHEDULE_CHECK: Duration = Duration::from_secs(300);
/// How long a replica waits after its scheduled import failed before trying again.
const SCHEDULE_RETRY: Duration = Duration::from_secs(3600);

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(search)).routes(routes!(best_match)).routes(routes!(check)).routes(routes!(import))
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct SearchQuery {
    #[serde(default)]
    q: String,
    /// One provider, or several separated by commas.
    provider: Option<String>,
    /// One mode, or several separated by commas.
    mode: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
    /// Shortest keys first, for suggestions; alphabetical otherwise.
    #[serde(default)]
    best: bool,
}

/// Search prices
#[utoipa::path(get, path = "/prices", operation_id = "search_prices", tag = "Price catalog", params(SearchQuery),
    responses((status = 200, description = "Matching entries, with the filters' values.", body = Object)),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
async fn search(State(state): State<AppState>, Query(q): Query<SearchQuery>) -> R<Json<Value>> {
    let terms: Vec<String> = q.q.split_whitespace().map(str::to_lowercase).collect();
    let list = |v: &Option<String>| -> Option<Vec<String>> {
        let items: Vec<String> = v.as_deref()?.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect();
        (!items.is_empty()).then_some(items)
    };
    let (providers, modes) = (list(&q.provider), list(&q.mode));
    let (limit, offset) = (q.limit.unwrap_or(100).clamp(1, 500), q.offset.unwrap_or(0).max(0));
    let ((total, items), (providers, modes), last_import) = tokio::try_join!(
        db::search_prices(&state.db, &terms, providers.as_deref(), modes.as_deref(), limit, offset, q.best),
        db::price_facets(&state.db),
        db::last_price_import(&state.db),
    )?;
    let next_import = next_import(&state, last_import.as_ref());
    Ok(Json(json!({
        "total": total,
        "items": entries(items),
        "providers": providers,
        "modes": modes,
        "last_import": last_import,
        "next_import": next_import,
        "source": SOURCE_URL,
    })))
}

/// When the scheduled import is due, if it is on: an interval after the last import, or now.
fn next_import(state: &AppState, last: Option<&db::PriceImport>) -> Option<DateTime<Utc>> {
    let every = state.settings.get().price_import_interval()?;
    Some(last.map_or_else(Utc::now, |i| i.imported_at + every))
}

fn entries(rows: Vec<(String, Value)>) -> Vec<Value> {
    rows.into_iter().map(|(key, data)| json!({ "key": key, "data": data })).collect()
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct MatchQuery {
    provider: String,
    model: String,
}

/// Match price
///
/// The catalog entry for a provider's model, and similar ones.
#[utoipa::path(get, path = "/prices/match", operation_id = "match_price", tag = "Price catalog", params(MatchQuery),
    responses((status = 200, description = "`exact` and `similar` entries.", body = Object)),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
async fn best_match(State(state): State<AppState>, Query(q): Query<MatchQuery>) -> R<Json<Value>> {
    let provider = ProviderKind::parse(&q.provider).ok_or_else(|| AdminError::bad_request("unknown provider"))?;
    let model = q.model.trim();
    if model.is_empty() {
        return Ok(Json(json!({ "exact": null, "similar": [] })));
    }
    let wanted = candidates(provider, model);
    let found = db::get_prices(&state.db, &wanted).await?;
    let exact = wanted.iter().find_map(|k| found.iter().find(|(key, _)| key == k)).cloned();
    let similar = db::similar_prices(&state.db, strip_region(model), 8).await?;
    Ok(Json(json!({
        "exact": exact.map(|(key, data)| json!({ "key": key, "data": data })),
        "similar": entries(similar),
    })))
}

/// Check prices
///
/// Every deployment with its catalog entry, to compare prices.
#[utoipa::path(get, path = "/prices/check", operation_id = "check_prices", tag = "Price catalog",
    responses((status = 200, description = "One entry per deployment.", body = Object)),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
// The entry is the base model's, or else the exact match for the upstream model; `key` is null when there is none (also for a base model the catalog dropped).
async fn check(State(state): State<AppState>) -> R<Json<Value>> {
    let deployments = db::list_deployments(&state.db).await?;
    let entries = catalog_entries(&state, &deployments).await?;
    let out: Vec<Value> = deployments
        .iter()
        .zip(entries)
        .map(|(d, e)| {
            let (key, data) = e.unzip();
            json!({ "deployment_id": d.id, "key": key, "data": data })
        })
        .collect();
    Ok(Json(json!(out)))
}

/// Each deployment's catalog entry (key and data), in the same order; `None` without one.
async fn catalog_entries(state: &AppState, deployments: &[DeploymentRow]) -> R<Vec<Option<(String, Value)>>> {
    let wanted: Vec<Vec<String>> = deployments
        .iter()
        .map(|d| match ProviderKind::parse(&d.provider) {
            Some(p) => catalog_keys(p, d.base_model.as_deref(), &d.upstream_model),
            None => d.base_model.iter().cloned().collect(),
        })
        .collect();
    let keys: Vec<String> = wanted.iter().flatten().cloned().collect::<BTreeSet<_>>().into_iter().collect();
    let found: HashMap<String, Value> = db::get_prices(&state.db, &keys).await?.into_iter().collect();
    Ok(wanted.iter().map(|keys| keys.iter().find_map(|k| found.get(k).map(|data| (k.clone(), data.clone())))).collect())
}

/// A deployment's price fields, as stored (see `DeploymentRow`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Prices {
    pub input_cost_per_mtok: f64,
    pub output_cost_per_mtok: f64,
    pub cache_read_cost_per_mtok: Option<f64>,
    pub cache_write_cost_per_mtok: Option<f64>,
    pub long_context_threshold: Option<i32>,
    pub long_input_cost_per_mtok: Option<f64>,
    pub long_output_cost_per_mtok: Option<f64>,
    pub long_cache_read_cost_per_mtok: Option<f64>,
    pub long_cache_write_cost_per_mtok: Option<f64>,
    pub cost_per_k_searches: Option<f64>,
}

impl Prices {
    fn of(d: &DeploymentRow) -> Self {
        Self {
            input_cost_per_mtok: d.input_cost_per_mtok,
            output_cost_per_mtok: d.output_cost_per_mtok,
            cache_read_cost_per_mtok: d.cache_read_cost_per_mtok,
            cache_write_cost_per_mtok: d.cache_write_cost_per_mtok,
            long_context_threshold: d.long_context_threshold,
            long_input_cost_per_mtok: d.long_input_cost_per_mtok,
            long_output_cost_per_mtok: d.long_output_cost_per_mtok,
            long_cache_read_cost_per_mtok: d.long_cache_read_cost_per_mtok,
            long_cache_write_cost_per_mtok: d.long_cache_write_cost_per_mtok,
            cost_per_k_searches: d.cost_per_k_searches,
        }
    }

    /// The prices a deployment of `mode` gets from a catalog entry, exactly as the console's
    /// deployment form fills them in (`deploymentPrices` in `ui/src/pricing.ts`): cache prices stay
    /// empty when they equal the input price, embedding and rerank models have no output price,
    /// only chat models get cache and long-context prices, and rerank models a price per search,
    /// chat models one per web search.
    fn from_catalog(data: &Value, mode: &str) -> Self {
        // LiteLLM prices per token and per query; to 8 significant digits, as the console rounds.
        let scaled = |v: Option<&Value>, by: f64| v.and_then(Value::as_f64).and_then(|v| format!("{:.7e}", v * by).parse::<f64>().ok());
        let per_m = |k: &str| scaled(data.get(k), 1e6);
        let chat = mode == "chat";
        let differs = |v: Option<f64>, base: Option<f64>| v.filter(|v| Some(*v) != base);
        let (input, cache_read, cache_write) =
            (per_m("input_cost_per_token"), per_m("cache_read_input_token_cost"), per_m("cache_creation_input_token_cost"));
        // The lowest long-context tier, with both an input and an output price.
        let long = data
            .as_object()
            .into_iter()
            .flat_map(|o| o.keys())
            .filter_map(|k| k.strip_prefix("input_cost_per_token_above_")?.strip_suffix("k_tokens")?.parse::<i32>().ok())
            .min()
            .filter(|_| chat)
            .and_then(|k| {
                let tier = |name: &str| per_m(&format!("{name}_above_{k}k_tokens"));
                let (input, output) = (tier("input_cost_per_token")?, tier("output_cost_per_token")?);
                Some((k * 1000, input, output, tier("cache_read_input_token_cost"), tier("cache_creation_input_token_cost")))
            });
        let web_search = data.get("supports_web_search") == Some(&Value::Bool(true))
            && !data.get("litellm_provider").and_then(Value::as_str).unwrap_or("").starts_with("bedrock");
        Self {
            input_cost_per_mtok: input.unwrap_or(0.0),
            output_cost_per_mtok: if mode == "embedding" || mode == "rerank" { 0.0 } else { per_m("output_cost_per_token").unwrap_or(0.0) },
            cache_read_cost_per_mtok: if chat { differs(cache_read, input) } else { None },
            cache_write_cost_per_mtok: if chat { differs(cache_write, input) } else { None },
            long_context_threshold: long.map(|l| l.0),
            long_input_cost_per_mtok: long.map(|l| l.1),
            long_output_cost_per_mtok: long.map(|l| l.2),
            long_cache_read_cost_per_mtok: long.and_then(|l| differs(l.3, Some(l.1))),
            long_cache_write_cost_per_mtok: long.and_then(|l| differs(l.4, Some(l.1))),
            cost_per_k_searches: match mode {
                "rerank" => scaled(data.get("input_cost_per_query"), 1000.0),
                "chat" if web_search => scaled(data.pointer("/search_context_cost_per_query/search_context_size_medium"), 1000.0),
                _ => None,
            },
        }
    }

    /// The prices as billed, field by field: an empty cache price is the input price of its tier.
    fn billed(&self) -> [Option<f64>; 10] {
        let long = self.long_context_threshold.is_some();
        [
            Some(self.input_cost_per_mtok),
            Some(self.output_cost_per_mtok),
            self.cache_read_cost_per_mtok.or(Some(self.input_cost_per_mtok)),
            self.cache_write_cost_per_mtok.or(Some(self.input_cost_per_mtok)),
            self.long_context_threshold.map(f64::from),
            self.long_input_cost_per_mtok,
            self.long_output_cost_per_mtok,
            if long { self.long_cache_read_cost_per_mtok.or(self.long_input_cost_per_mtok) } else { None },
            if long { self.long_cache_write_cost_per_mtok.or(self.long_input_cost_per_mtok) } else { None },
            self.cost_per_k_searches,
        ]
    }

    /// Whether both bill the same, beyond floating-point noise (`priceDifferences` in the console).
    fn bills_like(&self, other: &Self) -> bool {
        let same = |a: Option<f64>, b: Option<f64>| match (a, b) {
            (Some(a), Some(b)) => (a - b).abs() <= 1e-9 * 1f64.max(a.abs()).max(b.abs()),
            (a, b) => a == b,
        };
        self.billed().into_iter().zip(other.billed()).all(|(a, b)| same(a, b))
    }
}

/// Deployments whose prices no longer match their catalog entry take the catalog's, and keep the
/// entry as their base model, as the console's Accept button after a price check does. Each is
/// audited as an update by `actor`. Returns how many changed.
pub(crate) async fn accept_changes(state: &AppState, actor: &Actor) -> R<usize> {
    let deployments = db::list_deployments(&state.db).await?;
    let entries = catalog_entries(state, &deployments).await?;
    let mut accepted = 0;
    for (d, entry) in deployments.iter().zip(entries) {
        let Some((key, data)) = entry else { continue };
        let (before, after) = (Prices::of(d), Prices::from_catalog(&data, &d.mode));
        if before.bills_like(&after) || !db::accept_catalog_prices(&state.db, d.id, d.updated_at, &key, &after).await? {
            continue;
        }
        accepted += 1;
        let mut changes = audit::changes(&before, &after);
        if d.base_model.is_none() {
            changes.insert("base_model".into(), json!({ "from": null, "to": key }));
        }
        tracing::info!(deployment = %d.id, key, "deployment takes the price catalog's prices");
        audit::Event::new("deployment", "update").on(d.id, &d.model_name).details(changes.into()).record(state, actor).await;
    }
    if accepted > 0 {
        broadcast_reload(state, "catalog prices accepted").await;
    }
    Ok(accepted)
}

/// Catalog keys for a deployment, best first: its base model, or else the exact match for its
/// upstream model.
pub(crate) fn catalog_keys(provider: ProviderKind, base_model: Option<&str>, upstream_model: &str) -> Vec<String> {
    match base_model {
        Some(b) => vec![b.to_string()],
        None => candidates(provider, upstream_model),
    }
}

/// Bedrock cross-region inference profiles prefix the model id (`us.`, `eu.`, `global.`, …).
fn strip_region(model: &str) -> &str {
    for prefix in ["us.", "eu.", "apac.", "global.", "jp.", "au.", "ca.", "us-gov."] {
        if let Some(rest) = model.strip_prefix(prefix) {
            return rest;
        }
    }
    model
}

/// Catalog keys to try for a provider's model name, best first. LiteLLM keys OpenAI and
/// Anthropic models by bare name, Azure OpenAI as `azure/…`, Foundry models as `azure_ai/…`.
fn candidates(provider: ProviderKind, model: &str) -> Vec<String> {
    let m = model.to_string();
    let mut out = match provider {
        ProviderKind::Openai => vec![m.clone(), format!("openai/{m}")],
        ProviderKind::Azure => vec![format!("azure/{m}"), format!("azure_ai/{m}"), m.clone()],
        ProviderKind::Anthropic => vec![m.clone(), format!("anthropic/{m}")],
        ProviderKind::Bedrock => {
            let bare = strip_region(model);
            vec![m.clone(), format!("bedrock/{m}"), bare.to_string(), format!("bedrock/{bare}"), format!("bedrock/converse/{bare}")]
        }
        ProviderKind::Typesafe => vec![format!("typesafe/{m}"), m.clone()],
        ProviderKind::Mistral => vec![format!("mistral/{m}"), m.clone()],
        ProviderKind::Gemini => vec![format!("gemini/{m}"), m.clone()],
        // Gemini on Vertex is keyed by bare name, partner models (Claude, …) as `vertex_ai/…`.
        ProviderKind::Vertex => {
            let bare = model.strip_prefix("google/").unwrap_or(model);
            vec![bare.to_string(), format!("vertex_ai/{bare}"), m.clone()]
        }
    };
    out.dedup();
    out
}

/// Import prices
///
/// Replaces the catalog with LiteLLM's current price list. The gateway also does this on its
/// own, as often as the `price_import_hours` setting says, unless `price_import` is off. With
/// `price_auto_accept` on, deployments whose prices no longer match the catalog take its prices.
#[utoipa::path(post, path = "/prices/import", operation_id = "import_prices", tag = "Price catalog",
    responses(
        (status = 200, description = "What changed.", body = ImportSummary),
        (status = 400, description = "The download failed.", body = crate::error::AdminErrorBody),
        (status = 409, description = "An import is running already.", body = crate::error::AdminErrorBody),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
async fn import(State(state): State<AppState>, Extension(actor): Extension<Actor>) -> R<Json<Value>> {
    let summary = db::with_lock(&state.db, IMPORT_LOCK, download_and_replace(&state, &actor))
        .await?
        .ok_or_else(|| AdminError::conflict("the price catalog is being imported already; try again in a minute"))??;
    Ok(Json(json!(summary)))
}

/// Imports the catalog whenever the last import is older than the gateway setting's interval.
/// Every replica looks; the import lock and a second look once it is held let only one import.
pub async fn run_scheduled_import(state: AppState) {
    let mut tick = tokio::time::interval(SCHEDULE_CHECK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut retry_at = None;
    loop {
        tick.tick().await;
        let Some(every) = state.settings.get().price_import_interval() else { continue };
        if retry_at.is_some_and(|t| tokio::time::Instant::now() < t) {
            continue;
        }
        match import_if_due(&state, every).await {
            Ok(_) => retry_at = None,
            Err(e) => {
                tracing::warn!(error = %e.1, "scheduled price catalog import failed; retrying in an hour");
                retry_at = Some(tokio::time::Instant::now() + SCHEDULE_RETRY);
            }
        }
    }
}

/// The scheduled import, if it is due and no other replica is importing.
async fn import_if_due(state: &AppState, every: Duration) -> R<Option<ImportSummary>> {
    if !is_due(state, every).await? {
        return Ok(None);
    }
    let run = async {
        // Another replica may have imported between the first look and taking the lock.
        if !is_due(state, every).await? {
            return Ok(None);
        }
        download_and_replace(state, &Actor::system()).await.map(Some)
    };
    Ok(db::with_lock(&state.db, IMPORT_LOCK, run).await?.transpose()?.flatten())
}

async fn is_due(state: &AppState, every: Duration) -> sqlx::Result<bool> {
    let last = db::last_price_import(&state.db).await?;
    Ok(last.is_none_or(|i| Utc::now() >= i.imported_at + every))
}

/// Download LiteLLM's catalog and make the stored one equal to it.
async fn download_and_replace(state: &AppState, actor: &Actor) -> R<ImportSummary> {
    let resp = state
        .http
        .get(SOURCE_URL)
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .map_err(|e| AdminError::bad_request(format!("could not download the price catalog: {e}")))?;
    if !resp.status().is_success() {
        return Err(AdminError::bad_request(format!("downloading the price catalog failed with HTTP {}", resp.status())));
    }
    if resp.content_length().is_some_and(|n| n as usize > MAX_SOURCE_BYTES) {
        return Err(AdminError::bad_request("the price catalog is unexpectedly large"));
    }
    let bytes = resp.bytes().await.map_err(|e| AdminError::bad_request(format!("could not download the price catalog: {e}")))?;
    if bytes.len() > MAX_SOURCE_BYTES {
        return Err(AdminError::bad_request("the price catalog is unexpectedly large"));
    }
    let catalog = parse(&bytes)?;
    if catalog.is_empty() {
        return Err(AdminError::bad_request("the price catalog has no entries; nothing was changed"));
    }
    let import = db::replace_prices(&state.db, catalog, SOURCE_URL).await?;
    tracing::info!(
        total = import.total,
        added = import.added,
        updated = import.updated,
        removed = import.removed,
        "price catalog imported"
    );
    // The catalog is in; a failure here leaves the deployments for the next import.
    let accepted = match state.settings.get().price_auto_accept {
        true => accept_changes(state, actor)
            .await
            .inspect_err(|e| tracing::warn!(error = %e.1, "accepting the price catalog's prices failed"))
            .ok(),
        false => None,
    };
    let summary = ImportSummary { import, accepted };
    audit::Event::new("price_catalog", "import").details(json!(summary)).record(state, actor).await;
    Ok(summary)
}

/// What an import changed.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ImportSummary {
    #[serde(flatten)]
    pub import: db::PriceImport,
    /// Deployments that took the catalog's new prices, when `price_auto_accept` is on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accepted: Option<usize>,
}

/// Catalog entries: every object-valued key except LiteLLM's `sample_spec` template.
fn parse(bytes: &[u8]) -> R<Vec<(String, Value)>> {
    let all: Map<String, Value> =
        serde_json::from_slice(bytes).map_err(|e| AdminError::bad_request(format!("the price catalog is not a JSON object: {e}")))?;
    Ok(all.into_iter().filter(|(k, v)| k != "sample_spec" && v.is_object() && !k.is_empty() && k.len() <= MAX_KEY_LEN).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_keys() {
        assert_eq!(candidates(ProviderKind::Azure, "gpt-4o"), ["azure/gpt-4o", "azure_ai/gpt-4o", "gpt-4o"]);
        assert_eq!(candidates(ProviderKind::Openai, "gpt-4o")[0], "gpt-4o");
        assert_eq!(candidates(ProviderKind::Typesafe, "jev-latest")[0], "typesafe/jev-latest");
        let b = candidates(ProviderKind::Bedrock, "us.anthropic.claude-sonnet-4-5-v1:0");
        assert!(b.contains(&"anthropic.claude-sonnet-4-5-v1:0".to_string()) && b[0] == "us.anthropic.claude-sonnet-4-5-v1:0");
        assert_eq!(candidates(ProviderKind::Mistral, "mistral-large-latest")[0], "mistral/mistral-large-latest");
        assert_eq!(
            candidates(ProviderKind::Vertex, "google/gemini-2.5-pro"),
            ["gemini-2.5-pro", "vertex_ai/gemini-2.5-pro", "google/gemini-2.5-pro"]
        );
        assert_eq!(candidates(ProviderKind::Vertex, "claude-sonnet-4-5@20250929")[1], "vertex_ai/claude-sonnet-4-5@20250929");
        assert_eq!(strip_region("global.amazon.nova"), "amazon.nova");
        assert_eq!(strip_region("amazon.titan-embed-text-v2:0"), "amazon.titan-embed-text-v2:0");
    }

    #[test]
    fn parse_skips_the_template() {
        let rows =
            parse(br#"{"sample_spec": {"input_cost_per_token": 0}, "gpt-4o": {"input_cost_per_token": 2.5e-6}, "bad": 3}"#).ok().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "gpt-4o");
        assert!(parse(b"[1, 2]").is_err());
    }

    #[test]
    fn catalog_prices_as_the_deployment_form_fills_them_in() {
        let entry = json!({
            "input_cost_per_token": 3e-6, "output_cost_per_token": 1.5e-5,
            "cache_read_input_token_cost": 3e-7, "cache_creation_input_token_cost": 3e-6,
            "input_cost_per_token_above_200k_tokens": 6e-6, "output_cost_per_token_above_200k_tokens": 2.25e-5,
            "cache_read_input_token_cost_above_200k_tokens": 6e-7,
            "input_cost_per_token_above_1000k_tokens": 1e-5,
            "supports_web_search": true, "litellm_provider": "anthropic",
            "search_context_cost_per_query": { "search_context_size_medium": 0.01 },
        });
        let chat = Prices::from_catalog(&entry, "chat");
        assert_eq!((chat.input_cost_per_mtok, chat.output_cost_per_mtok), (3.0, 15.0));
        // Equal to the input price, the cache write price stays empty.
        assert_eq!((chat.cache_read_cost_per_mtok, chat.cache_write_cost_per_mtok), (Some(0.3), None));
        assert_eq!(
            (chat.long_context_threshold, chat.long_input_cost_per_mtok, chat.long_output_cost_per_mtok),
            (Some(200_000), Some(6.0), Some(22.5))
        );
        assert_eq!((chat.long_cache_read_cost_per_mtok, chat.long_cache_write_cost_per_mtok), (Some(0.6), None));
        assert_eq!(chat.cost_per_k_searches, Some(10.0));
        let embedding = Prices::from_catalog(&entry, "embedding");
        assert_eq!(
            (embedding.output_cost_per_mtok, embedding.cache_read_cost_per_mtok, embedding.long_context_threshold),
            (0.0, None, None)
        );
        let rerank = Prices::from_catalog(&json!({ "input_cost_per_query": 0.002 }), "rerank");
        assert_eq!(rerank.cost_per_k_searches, Some(2.0));
    }

    #[test]
    fn prices_compare_as_billed() {
        let catalog = Prices::from_catalog(&json!({ "input_cost_per_token": 2.5e-6, "output_cost_per_token": 1e-5 }), "chat");
        // A cache price equal to the input price bills like an empty one, and noise is ignored.
        let stored = Prices { input_cost_per_mtok: 2.5 + 1e-12, cache_read_cost_per_mtok: Some(2.5), ..catalog.clone() };
        assert!(stored.bills_like(&catalog));
        let raised = Prices { output_cost_per_mtok: 12.0, ..catalog.clone() };
        assert!(!raised.bills_like(&catalog));
        assert!(!Prices { cost_per_k_searches: Some(10.0), ..catalog.clone() }.bills_like(&catalog));
    }
}
