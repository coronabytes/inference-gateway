//! Admin API for guardrails: which guardrail at a provider to call, with which credential, and at
//! what prices; and trying one on a text.

use std::collections::HashMap;

use axum::{
    Extension, Json,
    extract::{Path, State},
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::R;
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
    let mut block: Vec<String> = config.block.iter().map(|c| c.trim().to_string()).collect();
    block.sort();
    block.dedup();
    let config = GuardrailConfig {
        guardrail_identifier: tidy(&config.guardrail_identifier),
        guardrail_version: tidy(&config.guardrail_version),
        model: tidy(&config.model),
        block,
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
    match provider {
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
        .ok_or_else(|| AdminError::bad_request("provider must be bedrock, mistral or model_armor"))?;
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
    broadcast_reload(&state, "guardrail deleted").await;
    tracing::info!(guardrail = %id, "guardrail deleted");
    audit::Event::new("guardrail", "delete").on(id, name).record(&state, &actor).await;
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
    }))
}
