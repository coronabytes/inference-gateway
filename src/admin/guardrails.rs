//! Admin API for guardrails: which guardrail at a provider to call, with which credential, and at
//! what prices; and trying one on a text.

use std::collections::HashMap;

use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{R, after_change};
use crate::{
    audit::{self, Actor},
    db,
    error::AdminError,
    guardrails::{
        self, Callable, GuardrailConfig, GuardrailPrices, GuardrailProvider, GuardrailRow, MISTRAL_CATEGORIES, MODEL_ARMOR_FILTERS, Source,
        Verdict,
    },
    registry,
    state::AppState,
    sync::broadcast_reload,
};

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct GuardrailView {
    id: Uuid,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    description: Option<String>,
    #[schema(value_type = crate::guardrails::GuardrailProvider)]
    provider: String,
    credential_id: Uuid,
    #[schema(required)]
    credential_name: Option<String>,
    config: GuardrailConfig,
    prices: GuardrailPrices,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

fn view(g: GuardrailRow, credential_name: Option<String>) -> GuardrailView {
    GuardrailView {
        id: g.id,
        name: g.name,
        description: g.description,
        provider: g.provider,
        credential_id: g.credential_id,
        credential_name,
        config: g.config.0,
        prices: g.prices.0,
        created_at: g.created_at,
        updated_at: g.updated_at,
    }
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct GuardrailInput {
    name: String,
    #[serde(default)]
    description: Option<String>,
    /// Required on create (`bedrock`). Fixed afterwards.
    provider: Option<String>,
    /// A credential of the same provider, which the guardrail is called with.
    credential_id: Uuid,
    config: GuardrailConfig,
    /// Omitted: the provider's list prices.
    #[serde(default)]
    prices: Option<GuardrailPrices>,
}

/// Checks the input against the provider and the credential, and returns the cleaned config.
async fn validate(state: &AppState, provider: GuardrailProvider, i: &GuardrailInput) -> R<(String, Option<String>, GuardrailConfig)> {
    let credential = db::get_credential(&state.db, i.credential_id).await?.ok_or_else(|| AdminError::bad_request("no such credential"))?;
    check_credential(provider, &credential.provider)?;
    check_guardrail(provider, &i.name, i.description.as_deref(), &i.config, i.prices)
}

/// A guardrail is called with a credential of its own provider.
pub(super) fn check_credential(provider: GuardrailProvider, credential_provider: &str) -> R<()> {
    if credential_provider != provider.credential_provider() {
        return Err(AdminError::bad_request(format!(
            "a {} guardrail needs a {} credential",
            provider.as_str(),
            provider.credential_provider()
        )));
    }
    Ok(())
}

/// Checks a guardrail's name, config and prices for its provider, as the admin API and the import
/// do; returns the name, description and config, trimmed.
pub(super) fn check_guardrail(
    provider: GuardrailProvider,
    name: &str,
    description: Option<&str>,
    config: &GuardrailConfig,
    prices: Option<GuardrailPrices>,
) -> R<(String, Option<String>, GuardrailConfig)> {
    let name = name.trim().to_string();
    if name.is_empty() || name.len() > 100 {
        return Err(AdminError::bad_request("name must be 1-100 characters"));
    }
    let description = description.map(str::trim).filter(|d| !d.is_empty()).map(str::to_string);
    let tidy = |v: &Option<String>| v.as_deref().map(str::trim).filter(|v| !v.is_empty()).map(str::to_string);
    let set = |v: &[String]| {
        let mut v: Vec<String> = v.iter().map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect();
        v.sort();
        v.dedup();
        v
    };
    // In the order given: the first is the one used when a text's language is unclear.
    let mut languages: Vec<String> = Vec::new();
    for l in config.languages.iter().map(|l| l.trim().to_ascii_lowercase()).filter(|l| !l.is_empty()) {
        if !languages.contains(&l) {
            languages.push(l);
        }
    }
    let config = GuardrailConfig {
        guardrail_identifier: tidy(&config.guardrail_identifier),
        guardrail_version: tidy(&config.guardrail_version),
        model: tidy(&config.model),
        block: set(&config.block),
        mask: set(&config.mask),
        pseudonymize: config.pseudonymize,
        restore: config.restore,
        languages,
        threshold: config.threshold,
        template: tidy(&config.template),
        location: tidy(&config.location),
        project_id: tidy(&config.project_id),
    };
    let only = |what: &str, p: &str| AdminError::bad_request(format!("{what} only applies to {p} guardrails"));
    let bedrock_fields = config.guardrail_identifier.is_some() || config.guardrail_version.is_some();
    let model_armor_fields = config.template.is_some() || config.location.is_some() || config.project_id.is_some();
    // A path segment of Google's API: letters, digits, `-` and `_`.
    let google_id = |s: &str| !s.is_empty() && s.len() <= 63 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if provider != GuardrailProvider::Presidio && (!config.mask.is_empty() || !config.languages.is_empty() || config.pseudonymize) {
        return Err(only("entities to mask, pseudonyms and languages", "Presidio"));
    }
    if config.restore && !config.pseudonymize {
        return Err(AdminError::bad_request("restoring values in answers needs pseudonyms"));
    }
    match provider {
        GuardrailProvider::Presidio => {
            if bedrock_fields {
                return Err(only("a guardrail id and version", "Bedrock"));
            }
            if model_armor_fields {
                return Err(only("a template, location and project", "Model Armor"));
            }
            if config.model.is_some() {
                return Err(only("a model", "Mistral"));
            }
            if config.languages.is_empty() {
                return Err(AdminError::bad_request("a Presidio guardrail needs at least one language, such as en"));
            }
            if let Some(l) = config.languages.iter().find(|l| !guardrails::LANGUAGE_CODES.contains(&l.as_str())) {
                return Err(AdminError::bad_request(format!("'{l}' is not an ISO 639-1 language code, such as en or de")));
            }
            // Several languages are told apart by the gateway's detector, which knows only some.
            let detectable = guardrails::detectable_languages();
            if config.languages.len() > 1
                && let Some(l) = config.languages.iter().find(|l| !detectable.contains(l))
            {
                return Err(AdminError::bad_request(format!(
                    "'{l}' cannot be detected; a guardrail with several languages lists only {}. With one language, any works",
                    detectable.join(", ")
                )));
            }
            // Presidio's own entity types are upper case; custom recognizers name theirs freely.
            let entity = |e: &str| e.len() <= 100 && e.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'));
            if let Some(e) = config.mask.iter().chain(&config.block).find(|e| !entity(e)) {
                return Err(AdminError::bad_request(format!("'{e}' is not an entity type, such as PERSON or IBAN_CODE")));
            }
            if let Some(e) = config.mask.iter().find(|e| config.block.contains(e)) {
                return Err(AdminError::bad_request(format!("{e} is both masked and blocked; choose one")));
            }
            if config.threshold.is_some_and(|t| !(0.0..=1.0).contains(&t)) {
                return Err(AdminError::bad_request("the threshold is a score from 0 to 1"));
            }
        }
        GuardrailProvider::ModelArmor => {
            if bedrock_fields {
                return Err(only("a guardrail id and version", "Bedrock"));
            }
            if config.model.is_some() || config.threshold.is_some() {
                return Err(only("a model and a threshold", "Mistral"));
            }
            let template =
                config.template.as_deref().ok_or_else(|| AdminError::bad_request("the Model Armor template's id is required"))?;
            let location = config
                .location
                .as_deref()
                .ok_or_else(|| AdminError::bad_request("the Model Armor template's location is required, such as europe-west4"))?;
            if !google_id(template) || !google_id(location) || config.project_id.as_deref().is_some_and(|p| !google_id(p)) {
                return Err(AdminError::bad_request("the template id, location and project are letters, digits, - and _"));
            }
            if let Some(f) = config.block.iter().find(|f| !MODEL_ARMOR_FILTERS.contains(&f.as_str())) {
                return Err(AdminError::bad_request(format!("'{f}' is not a Model Armor filter; use {}", MODEL_ARMOR_FILTERS.join(", "))));
            }
        }
        GuardrailProvider::Mistral => {
            if bedrock_fields {
                return Err(only("a guardrail id and version", "Bedrock"));
            }
            if model_armor_fields {
                return Err(only("a template, location and project", "Model Armor"));
            }
            if let Some(c) = config.block.iter().find(|c| !MISTRAL_CATEGORIES.contains(&c.as_str())) {
                return Err(AdminError::bad_request(format!("'{c}' is not a moderation category; use {}", MISTRAL_CATEGORIES.join(", "))));
            }
            if config.threshold.is_some_and(|t| !(t > 0.0 && t <= 1.0)) {
                return Err(AdminError::bad_request("the threshold is a score above 0 and up to 1"));
            }
        }
        GuardrailProvider::Bedrock => {
            if config.model.is_some() || !config.block.is_empty() || config.threshold.is_some() {
                return Err(only("a model, categories and a threshold", "Mistral"));
            }
            if model_armor_fields {
                return Err(only("a template, location and project", "Model Armor"));
            }
            let id =
                config.guardrail_identifier.as_deref().ok_or_else(|| AdminError::bad_request("the guardrail's id or ARN is required"))?;
            let plain_id = |s: &str| !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
            let arn_ok = id.starts_with("arn:aws")
                && guardrails::arn_region(id).is_some()
                && id.rsplit_once(":guardrail/").is_some_and(|(_, g)| plain_id(g));
            if !arn_ok && !plain_id(id) {
                return Err(AdminError::bad_request(
                    "the guardrail id is lowercase letters and digits (such as 50hag4klvigp), or an ARN such as arn:aws:bedrock:eu-central-1:123456789012:guardrail/50hag4klvigp",
                ));
            }
            let version = config
                .guardrail_version
                .as_deref()
                .ok_or_else(|| AdminError::bad_request("the guardrail version is required (a number or DRAFT)"))?;
            let numbered = !version.starts_with('0') && version.len() <= 8 && version.bytes().all(|b| b.is_ascii_digit());
            if version != "DRAFT" && !numbered {
                return Err(AdminError::bad_request("the guardrail version is a number such as 1, or DRAFT"));
            }
        }
    }
    if prices.is_some_and(|p| !p.is_valid()) {
        return Err(AdminError::bad_request("prices must be zero or more"));
    }
    Ok((name, description, config))
}

async fn credential_name(state: &AppState, id: Uuid) -> R<Option<String>> {
    Ok(db::get_credential(&state.db, id).await?.map(|c| c.name))
}

/// A guardrail in the list, with the projects and keys that apply it to every request.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct GuardrailListItem {
    #[serde(flatten)]
    guardrail: GuardrailView,
    enforced_on: EnforcedOn,
}

/// The names of the projects and keys whose every request goes through a guardrail.
#[derive(Serialize, Default, utoipa::ToSchema)]
pub(crate) struct EnforcedOn {
    projects: Vec<String>,
    keys: Vec<String>,
}

/// List guardrails
///
/// With the projects and keys that apply each one to every request.
#[utoipa::path(get, path = "/guardrails", tag = "Guardrails",
    responses((status = 200, description = "The guardrails.", body = Vec<GuardrailListItem>)),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(crate) async fn list_guardrails(State(state): State<AppState>) -> R<Json<Vec<GuardrailListItem>>> {
    let (rows, creds, holders) =
        tokio::try_join!(db::list_guardrails(&state.db), db::list_credentials(&state.db), db::guardrail_holders(&state.db))?;
    let name = |id: Uuid| creds.iter().find(|c| c.id == id).map(|c| c.name.clone());
    let mut enforced: HashMap<Uuid, EnforcedOn> = HashMap::new();
    for (guardrail, kind, holder) in holders {
        let on = enforced.entry(guardrail).or_default();
        if kind == "project" { on.projects.push(holder) } else { on.keys.push(holder) }
    }
    Ok(Json(
        rows.into_iter()
            .map(|g| {
                let enforced_on = enforced.remove(&g.id).unwrap_or_default();
                let n = name(g.credential_id);
                GuardrailListItem { guardrail: view(g, n), enforced_on }
            })
            .collect(),
    ))
}

/// Create guardrail
///
/// Refers to a guardrail defined at the provider, called with one of its credentials. Without
/// prices, the provider's list prices.
#[utoipa::path(post, path = "/guardrails", tag = "Guardrails", request_body = GuardrailInput,
    responses(
        (status = 201, description = "The new guardrail.", body = GuardrailView),
        (status = 400, description = "Invalid guardrail.", body = crate::error::AdminErrorBody),
        (status = 409, description = "The name is taken.", body = crate::error::AdminErrorBody),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(crate) async fn create_guardrail(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(i): Json<GuardrailInput>,
) -> R<(StatusCode, Json<GuardrailView>)> {
    let provider = i
        .provider
        .as_deref()
        .and_then(GuardrailProvider::parse)
        .ok_or_else(|| AdminError::bad_request("provider must be bedrock, mistral, model_armor or presidio"))?;
    let (name, description, config) = validate(&state, provider, &i).await?;
    let now = Utc::now();
    let row = GuardrailRow {
        id: Uuid::now_v7(),
        name,
        description,
        provider: provider.as_str().to_string(),
        credential_id: i.credential_id,
        config: sqlx::types::Json(config),
        prices: sqlx::types::Json(i.prices.unwrap_or_else(|| GuardrailPrices::list(provider))),
        created_at: now,
        updated_at: now,
    };
    db::insert_guardrail(&state.db, &row).await.map_err(AdminError::conflict_on_duplicate("a guardrail"))?;
    broadcast_reload(&state, "guardrail created").await;
    tracing::info!(guardrail = %row.id, provider = %row.provider, "guardrail created");
    let cred = credential_name(&state, row.credential_id).await?;
    let v = view(row, cred);
    audit::Event::new("guardrail", "create").on(v.id, &v.name).details(audit::snapshot(&v)).record(&state, &actor).await;
    Ok((StatusCode::CREATED, Json(v)))
}

/// Update guardrail
///
/// Without prices, the stored prices stay.
#[utoipa::path(put, path = "/guardrails/{id}", tag = "Guardrails", params(("id" = Uuid, Path)),
    request_body = GuardrailInput,
    responses(
        (status = 200, description = "The guardrail.", body = GuardrailView),
        (status = 400, description = "Invalid guardrail.", body = crate::error::AdminErrorBody),
        (status = 404, description = "No such guardrail.", body = crate::error::AdminErrorBody),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(crate) async fn update_guardrail(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<Uuid>,
    Json(i): Json<GuardrailInput>,
) -> R<Json<GuardrailView>> {
    let mut row = db::get_guardrail(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    if i.provider.as_deref().is_some_and(|p| p != row.provider) {
        return Err(AdminError::bad_request("a guardrail's provider cannot be changed; create a new guardrail"));
    }
    let provider = GuardrailProvider::parse(&row.provider).ok_or_else(|| AdminError::bad_request("unknown provider"))?;
    let before = view(row.clone(), credential_name(&state, row.credential_id).await?);
    let (name, description, config) = validate(&state, provider, &i).await?;
    row.name = name;
    row.description = description;
    row.credential_id = i.credential_id;
    row.config = sqlx::types::Json(config);
    row.prices = sqlx::types::Json(i.prices.unwrap_or(row.prices.0));
    db::update_guardrail(&state.db, &row).await.map_err(AdminError::conflict_on_duplicate("a guardrail"))?;
    broadcast_reload(&state, "guardrail updated").await;
    tracing::info!(guardrail = %id, "guardrail updated");
    let cred = credential_name(&state, row.credential_id).await?;
    let v = view(row, cred);
    let changes = audit::changes(&before, &v);
    if !changes.is_empty() {
        audit::Event::new("guardrail", "update").on(id, &v.name).details(changes.into()).record(&state, &actor).await;
    }
    Ok(Json(v))
}

/// Delete guardrail
#[utoipa::path(delete, path = "/guardrails/{id}", tag = "Guardrails", params(("id" = Uuid, Path)),
    responses(
        (status = 204, description = "Deleted."),
        (status = 404, description = "No such guardrail.", body = crate::error::AdminErrorBody),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(crate) async fn delete_guardrail(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<Uuid>,
) -> R<StatusCode> {
    let name = db::get_guardrail(&state.db, id).await?.ok_or_else(AdminError::not_found)?.name;
    if !db::delete_guardrail(&state.db, id).await? {
        return Err(AdminError::not_found());
    }
    after_change(&state, &actor, "guardrail deleted", id, Some(audit::Event::new("guardrail", "delete").on(id, name))).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct GuardrailTest {
    /// Check it as a prompt (`input`, the default) or as a model's response (`output`).
    #[serde(default = "input")]
    source: Source,
    text: String,
}

fn input() -> Source {
    Source::Input
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct GuardrailTestResult {
    ok: bool,
    latency_ms: u64,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    verdict: Option<Verdict>,
    /// The provider's error, when `ok` is false: an unknown guardrail, missing permissions, …
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    error: Option<String>,
}

/// Test guardrail
///
/// Checks a text as a prompt or a response: what the guardrail found, the text as it may
/// pass, and the billed units and cost. The provider bills the check; nothing is stored.
#[utoipa::path(post, path = "/guardrails/{id}/test", tag = "Guardrails", params(("id" = Uuid, Path)),
    request_body = GuardrailTest,
    responses(
        (status = 200, description = "The outcome, or the provider's error with `ok: false`.", body = GuardrailTestResult),
        (status = 404, description = "No such guardrail.", body = crate::error::AdminErrorBody),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
// Nothing is charged to a key.
pub(crate) async fn test_guardrail(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(t): Json<GuardrailTest>,
) -> R<Json<GuardrailTestResult>> {
    if t.text.trim().is_empty() {
        return Err(AdminError::bad_request("text is required"));
    }
    let row = db::get_guardrail(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    let cred = db::get_credential(&state.db, row.credential_id)
        .await?
        .ok_or_else(|| AdminError::bad_request("the guardrail's credential is gone"))?;
    let secrets = registry::decrypt_secrets(&state.cfg.secret_box, cred.secrets.as_deref()).map_err(AdminError::internal)?;
    let g = Callable { row: &row, settings: &cred.settings.0, secrets: &secrets };
    let started = std::time::Instant::now();
    let result = guardrails::check(&state, &g, t.source, &[t.text]).await;
    let latency_ms = started.elapsed().as_millis() as u64;
    Ok(Json(match result {
        Ok(v) => GuardrailTestResult { ok: true, latency_ms, verdict: Some(v), status: None, error: None },
        Err(e) => GuardrailTestResult { ok: false, latency_ms, verdict: None, status: Some(e.status.as_u16()), error: Some(e.message) },
    }))
}

/// Guardrail list prices
///
/// Each provider's list prices, USD per 1,000 text units for each kind of policy.
#[utoipa::path(get, path = "/guardrails/prices", operation_id = "guardrail_prices", tag = "Guardrails",
    responses((status = 200, description = "Prices by provider.", body = Object)),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(crate) async fn list_prices() -> Json<Value> {
    Json(json!({
        "bedrock": GuardrailPrices::list(GuardrailProvider::Bedrock),
        "mistral": GuardrailPrices::list(GuardrailProvider::Mistral),
        "model_armor": GuardrailPrices::list(GuardrailProvider::ModelArmor),
        "presidio": GuardrailPrices::list(GuardrailProvider::Presidio),
    }))
}

/// List detected languages
///
/// The languages the gateway detects, as ISO 639-1 codes: those a Presidio guardrail with several
/// languages can list. A guardrail with one language can use any its analyzer serves.
#[utoipa::path(get, path = "/guardrails/presidio/languages", tag = "Guardrails",
    responses((status = 200, description = "The languages' codes, sorted.", body = Vec<String>)),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(crate) async fn presidio_languages() -> Json<Vec<String>> {
    Json(guardrails::detectable_languages())
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct PresidioAnalyzerQuery {
    /// The generic HTTP credential of the Presidio analyzer.
    credential_id: Uuid,
}

/// What a Presidio analyzer serves: the languages it has a model for, each with the entity types
/// it finds in it.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct PresidioAnalyzer {
    languages: Vec<PresidioLanguage>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct PresidioLanguage {
    /// ISO 639-1, such as `de`.
    code: String,
    /// The entity types, sorted.
    entities: Vec<String>,
}

/// How many languages an analyzer is asked about at once.
const PROBES: usize = 32;

/// Read a Presidio analyzer
///
/// The languages a Presidio analyzer serves, with the entity types it finds in each, read from its
/// `/supportedentities`. The analyzer has no list of its languages, so it is asked about every
/// ISO 639-1 code; one it has no model for is left out.
#[utoipa::path(get, path = "/guardrails/presidio/analyzer", tag = "Guardrails", params(PresidioAnalyzerQuery),
    responses(
        (status = 200, description = "The analyzer's languages, sorted by code.", body = PresidioAnalyzer),
        (status = 400, description = "Not a generic HTTP credential.", body = crate::error::AdminErrorBody),
        (status = 502, description = "The analyzer could not be reached or refused the credential, or serves no language.", body = crate::error::AdminErrorBody),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(crate) async fn presidio_analyzer(State(state): State<AppState>, Query(q): Query<PresidioAnalyzerQuery>) -> R<Json<PresidioAnalyzer>> {
    use futures::StreamExt;
    let cred = db::get_credential(&state.db, q.credential_id).await?.ok_or_else(|| AdminError::bad_request("no such credential"))?;
    check_credential(GuardrailProvider::Presidio, &cred.provider)?;
    let secrets = registry::decrypt_secrets(&state.cfg.secret_box, cred.secrets.as_deref()).map_err(AdminError::internal)?;
    let (state, settings, secrets) = (&state, &cred.settings.0, &secrets);
    let unreachable = |e: crate::error::GatewayError| AdminError::bad_gateway(e.message);
    // English first: an analyzer that cannot be reached or refuses the credential fails once,
    // rather than once for every language.
    let mut languages = Vec::new();
    if let Some(entities) = guardrails::presidio_entities(state, settings, secrets, "en").await.map_err(unreachable)? {
        languages.push(PresidioLanguage { code: "en".into(), entities });
    }
    let mut probes = Vec::new();
    for &code in guardrails::LANGUAGE_CODES.iter().filter(|c| **c != "en") {
        probes.push(async move { (code, guardrails::presidio_entities(state, settings, secrets, code).await) });
    }
    let answers: Vec<_> = futures::stream::iter(probes).buffer_unordered(PROBES).collect().await;
    for (code, answer) in answers {
        if let Some(entities) = answer.map_err(unreachable)? {
            languages.push(PresidioLanguage { code: code.into(), entities });
        }
    }
    if languages.is_empty() {
        return Err(AdminError::bad_gateway("the analyzer serves no language"));
    }
    languages.sort_by(|a, b| a.code.cmp(&b.code));
    Ok(Json(PresidioAnalyzer { languages }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_presidio_guardrails() {
        let check = |c: GuardrailConfig| check_guardrail(GuardrailProvider::Presidio, "pii", None, &c, None).map(|(_, _, c)| c);
        let langs = |l: &[&str]| l.iter().map(|l| l.to_string()).collect::<Vec<_>>();
        let base = GuardrailConfig { languages: langs(&["de"]), mask: langs(&["PERSON"]), ..Default::default() };

        // Languages keep their order (the first is the fallback), lower-cased and once each.
        let c = check(GuardrailConfig { languages: langs(&[" DE", "en", "de"]), ..base.clone() }).unwrap();
        assert_eq!(c.languages, ["de", "en"]);
        assert!(check(GuardrailConfig { languages: vec![], ..base.clone() }).is_err());
        assert!(check(GuardrailConfig { languages: langs(&["xx"]), ..base.clone() }).is_err());
        // Alone, any language the analyzer may serve; several only among those the gateway detects.
        assert!(check(GuardrailConfig { languages: langs(&["ja"]), ..base.clone() }).is_ok());
        assert!(check(GuardrailConfig { languages: langs(&["en", "ja"]), ..base.clone() }).is_err());

        // An entity type is masked or blocked, not both, and looks like one.
        assert!(check(GuardrailConfig { block: langs(&["PERSON"]), ..base.clone() }).is_err());
        assert!(check(GuardrailConfig { mask: langs(&["NO SPACES"]), ..base.clone() }).is_err());
        assert!(check(GuardrailConfig { threshold: Some(1.5), ..base.clone() }).is_err());
        assert!(check(GuardrailConfig { threshold: Some(0.0), ..base.clone() }).is_ok());

        // Other providers' fields, and Presidio's on other providers, are refused.
        assert!(check(GuardrailConfig { model: Some("m".into()), ..base.clone() }).is_err());
        assert!(check(GuardrailConfig { template: Some("t".into()), ..base.clone() }).is_err());
        let mistral = GuardrailConfig { languages: langs(&["en"]), ..Default::default() };
        assert!(check_guardrail(GuardrailProvider::Mistral, "m", None, &mistral, None).is_err());
    }
}
