//! Price catalog: LiteLLM's model prices, imported on a schedule or on demand and stored per key
//! as JSONB.
//! The admin UI browses it and uses it to fill in deployment prices; billing itself always
//! uses the prices stored on each deployment.

use std::{
    collections::{BTreeSet, HashMap},
    time::Duration,
};

use axum::{
    Extension, Json, Router,
    extract::{Query, State},
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::{
    audit::{self, Actor},
    db,
    error::AdminError,
    model::ProviderKind,
    state::AppState,
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

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/prices", get(search))
        .route("/prices/match", get(best_match))
        .route("/prices/check", get(check))
        .route("/prices/import", post(import))
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

/// The catalog entry for a provider's model name, if one matches exactly, and similar entries.
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

/// Every deployment with its catalog entry: the base model, or else the exact match for its
/// upstream model. `key` is null when there is none (also for a base model the catalog dropped).
/// The admin UI compares the entry with the deployment's prices.
async fn check(State(state): State<AppState>) -> R<Json<Value>> {
    let deployments = db::list_deployments(&state.db).await?;
    let wanted: Vec<Vec<String>> = deployments
        .iter()
        .map(|d| match ProviderKind::parse(&d.provider) {
            Some(p) => catalog_keys(p, d.base_model.as_deref(), &d.upstream_model),
            None => d.base_model.iter().cloned().collect(),
        })
        .collect();
    let keys: Vec<String> = wanted.iter().flatten().cloned().collect::<BTreeSet<_>>().into_iter().collect();
    let found: HashMap<String, Value> = db::get_prices(&state.db, &keys).await?.into_iter().collect();
    let out: Vec<Value> = deployments
        .iter()
        .zip(&wanted)
        .map(|(d, keys)| {
            let key = keys.iter().find(|k| found.contains_key(*k));
            json!({ "deployment_id": d.id, "key": key, "data": key.map(|k| &found[k]) })
        })
        .collect();
    Ok(Json(json!(out)))
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
async fn import_if_due(state: &AppState, every: Duration) -> R<Option<db::PriceImport>> {
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
async fn download_and_replace(state: &AppState, actor: &Actor) -> R<db::PriceImport> {
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
    let summary = db::replace_prices(&state.db, catalog, SOURCE_URL).await?;
    tracing::info!(total = summary.total, added = summary.added, updated = summary.updated, removed = summary.removed, "price catalog imported");
    audit::Event::new("price_catalog", "import").details(json!(summary)).record(state, actor).await;
    Ok(summary)
}

/// Catalog entries: every object-valued key except LiteLLM's `sample_spec` template.
fn parse(bytes: &[u8]) -> R<Vec<(String, Value)>> {
    let all: Map<String, Value> =
        serde_json::from_slice(bytes).map_err(|e| AdminError::bad_request(format!("the price catalog is not a JSON object: {e}")))?;
    Ok(all
        .into_iter()
        .filter(|(k, v)| k != "sample_spec" && v.is_object() && !k.is_empty() && k.len() <= MAX_KEY_LEN)
        .collect())
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
        assert_eq!(candidates(ProviderKind::Vertex, "google/gemini-2.5-pro"), ["gemini-2.5-pro", "vertex_ai/gemini-2.5-pro", "google/gemini-2.5-pro"]);
        assert_eq!(candidates(ProviderKind::Vertex, "claude-sonnet-4-5@20250929")[1], "vertex_ai/claude-sonnet-4-5@20250929");
        assert_eq!(strip_region("global.amazon.nova"), "amazon.nova");
        assert_eq!(strip_region("amazon.titan-embed-text-v2:0"), "amazon.titan-embed-text-v2:0");
    }

    #[test]
    fn parse_skips_the_template() {
        let rows = parse(br#"{"sample_spec": {"input_cost_per_token": 0}, "gpt-4o": {"input_cost_per_token": 2.5e-6}, "bad": 3}"#).ok().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "gpt-4o");
        assert!(parse(b"[1, 2]").is_err());
    }
}
