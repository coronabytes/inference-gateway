//! Admin API: credentials, deployments, routers, virtual keys, metrics, logs, cluster. Requests
//! authenticate with the master key or an admin session (see `admin_auth`).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::{DateTime, Datelike, Utc};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    admin_auth::Access,
    audit::{self, Actor},
    budget::Spender,
    db,
    error::AdminError,
    ir::{Aggregator, ChatRequest, Message, Part, Role},
    model::{
        AliasRow, CredentialRow, CredentialSettings, DeploymentConfig, DeploymentRow, KeyScope, Mode, ProjectRow, PromptLogging,
        ProviderKind, Route, RouterRow, Secrets, VirtualKey, normalize_tags,
    },
    registry,
    settings::Settings,
    state::AppState,
    sync::broadcast_reload,
};

pub(crate) mod a2a;
pub(crate) mod connect;
pub(crate) mod guardrails;
pub(crate) mod hub;
pub(crate) mod mcp;
pub(crate) mod oauth;
pub(crate) mod tool_routers;
pub(crate) mod transfer;

type R<T> = Result<T, AdminError>;

pub fn router(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/status", get(status))
        .route("/credentials", get(list_credentials).post(create_credential))
        .route("/credentials/{id}", axum::routing::put(update_credential).delete(delete_credential))
        .route("/credentials/{id}/foundry-deployments", get(foundry_deployments))
        .route("/credentials/{id}/bedrock-models", get(bedrock_models))
        .route("/aws-regions", get(aws_regions))
        .route("/deployments", get(list_deployments).post(create_deployment))
        .route("/deployments/{id}", get(get_deployment).put(update_deployment).delete(delete_deployment))
        .route("/deployments/{id}/test", post(test_deployment))
        .route("/deployments/{id}/clear-cooldown", post(clear_cooldown))
        .route("/guardrails", get(guardrails::list_guardrails).post(guardrails::create_guardrail))
        .route("/guardrails/prices", get(guardrails::list_prices))
        .route("/guardrails/{id}", axum::routing::put(guardrails::update_guardrail).delete(guardrails::delete_guardrail))
        .route("/guardrails/{id}/test", post(guardrails::test_guardrail))
        .route("/mcp-servers", get(mcp::list_mcp_servers).post(mcp::create_mcp_server))
        .route("/mcp-servers/{id}", axum::routing::put(mcp::update_mcp_server).delete(mcp::delete_mcp_server))
        .route("/mcp-servers/{id}/test", post(mcp::test_mcp_server))
        .route("/oauth/discover", post(oauth::discover))
        .route("/keys/{id}/connections", get(oauth::list_connections))
        .route("/keys/{id}/apps", get(connect::list_apps))
        .route("/keys/{id}/apps/{grant}", axum::routing::delete(connect::revoke_app))
        .route("/connect/{id}", get(connect::get_request).post(connect::approve))
        .route("/connect/{id}/deny", post(connect::deny))
        .route("/keys/{id}/connections/{kind}/{upstream}", post(oauth::connect).delete(oauth::disconnect))
        .route("/keys/{id}/connections/{kind}/{upstream}/sign-ins", get(oauth::list_sign_ins))
        .route("/a2a-agents", get(a2a::list_a2a_agents).post(a2a::create_a2a_agent))
        .route("/a2a-agents/{id}", axum::routing::put(a2a::update_a2a_agent).delete(a2a::delete_a2a_agent))
        .route("/a2a-agents/{id}/test", post(a2a::test_a2a_agent))
        .route("/tool-routers", get(tool_routers::list_tool_routers).post(tool_routers::create_tool_router))
        .route("/tool-routers/{id}", axum::routing::put(tool_routers::update_tool_router).delete(tool_routers::delete_tool_router))
        .route("/routers", get(list_routers).post(create_router))
        .route("/routers/{id}", axum::routing::put(update_router).delete(delete_router))
        .route("/aliases", get(list_aliases).post(create_alias))
        .route("/aliases/{id}", axum::routing::put(update_alias).delete(delete_alias))
        .route("/tags", get(list_tags))
        .route("/hub", get(hub::hub))
        .route("/efforts", get(hub::efforts))
        .route("/model-metadata", axum::routing::put(hub::set_metadata))
        .route("/config/export", get(transfer::export))
        .route("/config/import", post(transfer::import))
        .route("/keys", get(list_keys).post(create_key))
        .route("/keys/options", get(key_options))
        .route("/keys/{id}", axum::routing::put(update_key).delete(delete_key))
        .route("/keys/{id}/reset-spend", post(reset_spend))
        .route("/keys/{id}/regenerate", post(regenerate_key))
        .route("/keys/{id}/bodies", get(key_bodies).delete(delete_key_bodies))
        .route("/projects", get(list_projects).post(create_project))
        .route("/projects/options", get(project_options))
        .route("/projects/{id}", axum::routing::put(update_project).delete(delete_project))
        .route("/projects/{id}/reset-spend", post(reset_project_spend))
        .route("/projects/{id}/bodies", get(project_bodies).delete(delete_project_bodies))
        .route("/metrics", get(metrics))
        .route("/logs", get(logs))
        .route("/logs/labels", get(log_labels))
        .route("/logs/tools", get(log_tools))
        .route("/logs/{id}/bodies", get(log_bodies))
        .route("/audit", get(audit_log))
        .route("/usage/keys", get(key_usage))
        .route("/cluster", get(cluster))
        .route("/settings", get(get_settings).put(put_settings))
        .route("/playground/options", get(playground_options))
        .route("/playground/{api}", post(playground))
        .route("/playground/a2a/{agent}", post(playground_a2a))
        .route("/playground/a2a/{agent}/card", get(playground_a2a_card))
        .merge(crate::admin_auth::protected_api_routes())
        .merge(crate::prices::routes())
        // A route layer, so the guard knows which route it guards (users reach only some).
        .route_layer(middleware::from_fn_with_state(state, crate::admin_auth::require_admin))
        .merge(crate::admin_auth::api_routes())
}

/// A list page's search text, trimmed and in lower case; `None` when empty.
fn search_text(q: Option<&str>) -> Option<String> {
    q.map(|q| q.trim().to_lowercase()).filter(|q| !q.is_empty())
}

/// A comma-separated list filter (`a,b`); `None` when empty.
fn csv(s: Option<&str>) -> Option<Vec<&str>> {
    let items: Vec<&str> = s.unwrap_or("").split(',').map(str::trim).filter(|v| !v.is_empty()).collect();
    (!items.is_empty()).then_some(items)
}

/// A comma-separated list of ids; `None` when empty.
fn csv_ids(s: Option<&str>, what: &str) -> R<Option<Vec<Uuid>>> {
    csv(s)
        .map(|items| items.into_iter().map(|v| v.parse().map_err(|_| AdminError::bad_request(format!("{what} must be ids")))).collect())
        .transpose()
}

/// A comma-separated list filter on tags or projects, where `-` stands for none (no tags, no project).
fn any_of<T>(s: Option<&str>, parse: impl Fn(&str) -> R<T>) -> R<db::AnyOf<T>> {
    let Some(items) = csv(s) else { return Ok(db::AnyOf { any: None, none: false }) };
    let none = items.contains(&"-");
    let any = items.into_iter().filter(|v| *v != "-").map(parse).collect::<R<Vec<T>>>()?;
    Ok(db::AnyOf { any: Some(any), none })
}

fn tag_filter(s: Option<&str>) -> R<db::AnyOf<String>> {
    any_of(s, |t| Ok(t.to_string()))
}

fn project_filter(s: Option<&str>) -> R<db::AnyOf<Uuid>> {
    any_of(s, |p| p.parse().map_err(|_| AdminError::bad_request("projects must be ids, or - for none")))
}

async fn status(State(state): State<AppState>) -> Json<Value> {
    let snap = state.registry.snapshot();
    Json(json!({
        "instance_id": state.cfg.instance_id,
        "version": crate::VERSION,
        "registry_loaded_at": snap.loaded_at,
        "registry_errors": snap.errors,
    }))
}

// ---------------------------------------------------------------- credentials

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct CredentialView {
    id: Uuid,
    name: String,
    provider: String,
    settings: CredentialSettings,
    secrets_set: BTreeMap<&'static str, bool>,
    /// Vertex: who the stored key file authenticates as. Not secret.
    #[serde(skip_serializing_if = "Option::is_none")]
    service_account: Option<Value>,
    deployments: i64,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

fn credential_view(state: &AppState, c: CredentialRow, deployments: i64) -> CredentialView {
    let secrets = registry::decrypt_secrets(&state.cfg.secret_box, c.secrets.as_deref()).unwrap_or_default();
    CredentialView {
        id: c.id,
        name: c.name,
        provider: c.provider,
        settings: c.settings.0,
        secrets_set: secrets.presence(),
        service_account: secrets
            .google_service_account
            .as_deref()
            .and_then(|j| crate::google::ServiceAccount::parse(j).ok())
            .map(|sa| json!({ "client_email": sa.client_email, "project_id": sa.project_id })),
        deployments,
        created_at: c.created_at,
        updated_at: c.updated_at,
    }
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CredentialInput {
    name: String,
    /// Required on create. Fixed afterwards: deployments depend on it matching theirs.
    provider: Option<String>,
    /// Endpoint, API version, region. Not secret, so always sent in full.
    #[serde(default)]
    settings: CredentialSettings,
    /// On update, omitted fields keep their stored value and empty strings clear them.
    #[serde(default)]
    secrets: Secrets,
}

/// Trim, drop empty values and check the settings that apply to `provider`.
fn clean_settings(state: &AppState, provider: ProviderKind, s: CredentialSettings) -> R<CredentialSettings> {
    let tidy = |v: Option<String>| v.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let s = CredentialSettings {
        base_url: tidy(s.base_url).map(|u| u.trim_end_matches('/').to_string()),
        api_version: tidy(s.api_version),
        region: tidy(s.region),
        project_id: tidy(s.project_id),
        auth_header: tidy(s.auth_header),
        tenant_id: tidy(s.tenant_id),
        client_id: tidy(s.client_id),
        identity: s.identity,
    };
    let environment = s.identity == Some(crate::model::Identity::Environment);
    if environment && !matches!(provider, ProviderKind::Azure | ProviderKind::Vertex) {
        return Err(AdminError::bad_request("identity only applies to Azure and Vertex AI credentials"));
    }
    if provider != ProviderKind::Azure && (s.tenant_id.is_some() || s.client_id.is_some()) {
        return Err(AdminError::bad_request("tenant_id and client_id only apply to Azure credentials"));
    }
    // The environment's identity: at most a client ID, for a user-assigned managed identity or a
    // workload identity app. The tenant comes from the environment.
    if environment && s.tenant_id.is_some() {
        return Err(AdminError::bad_request(
            "with the environment's identity, the tenant comes from the environment; leave tenant_id empty",
        ));
    }
    if !environment && s.tenant_id.is_some() != s.client_id.is_some() {
        return Err(AdminError::bad_request("Entra ID needs both tenant_id and client_id"));
    }
    if environment && provider == ProviderKind::Vertex && s.project_id.is_none() {
        return Err(AdminError::bad_request("a Vertex AI credential with Application Default Credentials needs its project_id"));
    }
    if s.tenant_id.as_deref().is_some_and(|t| !crate::entra::valid_tenant(t)) {
        return Err(AdminError::bad_request("tenant_id must be a directory (tenant) ID or domain, e.g. contoso.onmicrosoft.com"));
    }
    if s.client_id.as_deref().is_some_and(|c| !crate::entra::valid_client_id(c)) {
        return Err(AdminError::bad_request("client_id must be the app registration's application (client) ID, a GUID"));
    }
    if let Some(h) = &s.auth_header {
        if provider != ProviderKind::Openai {
            return Err(AdminError::bad_request("auth_header only applies to OpenAI-compatible credentials"));
        }
        if axum::http::HeaderName::try_from(h.as_str()).is_err()
            || crate::model::reserved_header(h)
            || h.eq_ignore_ascii_case("content-type")
        {
            return Err(AdminError::bad_request(format!("auth_header '{h}' is not a header the key can be sent in")));
        }
    }
    if let Some(u) = &s.base_url {
        let parsed = reqwest::Url::parse(u).map_err(|_| AdminError::bad_request("base_url is not a valid URL"))?;
        if !matches!(parsed.scheme(), "https" | "http") {
            return Err(AdminError::bad_request("base_url must be http(s)"));
        }
        // Hostnames are checked on every connection; an IP literal can be refused right away.
        state.cfg.upstream_policy.check_url(u).map_err(|why| AdminError::bad_request(format!("base_url is not allowed: {why}")))?;
    }
    if provider == ProviderKind::Azure && s.base_url.is_none() {
        return Err(AdminError::bad_request("an Azure credential needs its resource endpoint (base_url)"));
    }
    if provider != ProviderKind::Azure && s.api_version.is_some() {
        return Err(AdminError::bad_request("api_version only applies to Azure credentials"));
    }
    if !matches!(provider, ProviderKind::Bedrock | ProviderKind::Mistral | ProviderKind::Vertex) && s.region.is_some() {
        return Err(AdminError::bad_request("region only applies to Bedrock, Mistral and Vertex AI credentials"));
    }
    if let Some(r) = &s.region
        && !r.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(AdminError::bad_request("region must look like us-east-1"));
    }
    if provider == ProviderKind::Mistral && s.region.as_deref().is_some_and(|r| !matches!(r, "global" | "eu" | "us")) {
        return Err(AdminError::bad_request("Mistral regions are global, eu and us"));
    }
    if provider != ProviderKind::Vertex && s.project_id.is_some() {
        return Err(AdminError::bad_request("project_id only applies to Vertex AI credentials"));
    }
    // Project IDs are lowercase letters, digits and hyphens; legacy ones have a `domain:` prefix.
    if let Some(p) = &s.project_id
        && (p.len() > 100 || !p.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '.' | ':')))
    {
        return Err(AdminError::bad_request("project_id must be a Google Cloud project ID, e.g. my-project-123"));
    }
    Ok(s)
}

fn validate_credential(provider: ProviderKind, name: &str, s: &Secrets, settings: &CredentialSettings) -> R<()> {
    if name.trim().is_empty() || name.len() > 100 {
        return Err(AdminError::bad_request("name must be 1-100 characters"));
    }
    let has_aws = s.aws_access_key_id.is_some() || s.aws_secret_access_key.is_some() || s.aws_session_token.is_some();
    if provider != ProviderKind::Vertex && s.google_service_account.is_some() {
        return Err(AdminError::bad_request("a service account key only applies to Vertex AI credentials"));
    }
    if provider != ProviderKind::Azure && s.client_secret.is_some() {
        return Err(AdminError::bad_request("a client_secret only applies to Azure credentials (Entra ID)"));
    }
    let entra = settings.tenant_id.is_some();
    let environment = settings.identity == Some(crate::model::Identity::Environment);
    if environment && (s.api_key.is_some() || s.client_secret.is_some() || s.google_service_account.is_some() || has_aws) {
        return Err(AdminError::bad_request("a credential with the environment's identity stores no secrets"));
    }
    match provider {
        // The gateway's own identity: nothing stored.
        ProviderKind::Azure | ProviderKind::Vertex if environment => {}
        // Entra ID: the app's client credentials instead of the resource's key.
        ProviderKind::Azure if entra && s.client_secret.is_none() => {
            return Err(AdminError::bad_request("Entra ID needs the app registration's client_secret"));
        }
        ProviderKind::Azure if entra && s.api_key.is_some() => {
            return Err(AdminError::bad_request("an Azure credential takes an api_key or Entra ID client credentials, not both"));
        }
        ProviderKind::Azure if !entra && s.client_secret.is_some() => {
            return Err(AdminError::bad_request("a client_secret needs tenant_id and client_id"));
        }
        ProviderKind::Azure if entra => {}
        ProviderKind::Azure | ProviderKind::Anthropic | ProviderKind::Typesafe | ProviderKind::Mistral | ProviderKind::Gemini
            if s.api_key.is_none() =>
        {
            return Err(AdminError::bad_request("an api_key is required for this provider"));
        }
        ProviderKind::Vertex => {
            let json = s
                .google_service_account
                .as_deref()
                .ok_or_else(|| AdminError::bad_request("a Vertex AI credential needs a service account key file"))?;
            crate::google::ServiceAccount::parse(json).map_err(AdminError::bad_request)?;
            if s.api_key.is_some() || has_aws {
                return Err(AdminError::bad_request("Vertex AI credentials take a service account key only"));
            }
        }
        // A half-configured key pair would silently fall back to the default credential chain.
        ProviderKind::Bedrock if s.aws_access_key_id.is_some() != s.aws_secret_access_key.is_some() => {
            return Err(AdminError::bad_request(
                "set both aws_access_key_id and aws_secret_access_key, or neither to use the AWS credential chain",
            ));
        }
        ProviderKind::Bedrock if s.aws_session_token.is_some() && s.aws_access_key_id.is_none() => {
            return Err(AdminError::bad_request("aws_session_token needs an access key pair"));
        }
        _ if provider != ProviderKind::Bedrock && has_aws => {
            return Err(AdminError::bad_request("AWS keys only apply to bedrock credentials"));
        }
        _ => {}
    }
    // Secrets are only ever stored, encrypted, from what is entered here. Reject `env:` values
    // explicitly: older versions treated them as environment references.
    if s.values().any(|v| v.starts_with("env:")) {
        return Err(AdminError::bad_request("enter the secret value itself; env: references to environment variables are not supported"));
    }
    Ok(())
}

fn conflict_on_duplicate_name(e: sqlx::Error) -> AdminError {
    if db::is_unique_violation(&e) { AdminError::conflict("a credential with this name already exists") } else { e.into() }
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct CredentialQuery {
    /// Just the credentials whose name contains this text, ignoring case.
    q: Option<String>,
    /// Just the credentials of these providers, as `openai,azure`.
    provider: Option<String>,
}

async fn list_credentials(State(state): State<AppState>, Query(q): Query<CredentialQuery>) -> R<Json<Vec<CredentialView>>> {
    let search = search_text(q.q.as_deref());
    let providers: Option<Vec<String>> = csv(q.provider.as_deref()).map(|p| p.into_iter().map(String::from).collect());
    let (creds, usage) =
        tokio::try_join!(db::search_credentials(&state.db, search.as_deref(), providers.as_deref()), db::credential_usage(&state.db))?;
    Ok(Json(
        creds
            .into_iter()
            .map(|c| {
                let n = usage.get(&c.id).copied().unwrap_or(0);
                credential_view(&state, c, n)
            })
            .collect(),
    ))
}

async fn create_credential(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(i): Json<CredentialInput>,
) -> R<(StatusCode, Json<CredentialView>)> {
    let provider = i.provider.as_deref().and_then(ProviderKind::parse).ok_or_else(|| {
        AdminError::bad_request("provider must be one of openai, azure, bedrock, anthropic, typesafe, mistral, vertex, gemini")
    })?;
    let mut secrets = Secrets::default();
    secrets.merge(i.secrets);
    let settings = clean_settings(&state, provider, i.settings)?;
    validate_credential(provider, &i.name, &secrets, &settings)?;
    let now = Utc::now();
    let row = CredentialRow {
        id: Uuid::now_v7(),
        name: i.name.trim().to_string(),
        provider: provider.as_str().to_string(),
        secrets: registry::encrypt_secrets(&state.cfg.secret_box, &secrets),
        settings: sqlx::types::Json(settings),
        created_at: now,
        updated_at: now,
    };
    db::insert_credential(&state.db, &row).await.map_err(conflict_on_duplicate_name)?;
    tracing::info!(credential = %row.id, provider = %row.provider, "credential created");
    let view = credential_view(&state, row, 0);
    audit::Event::new("credential", "create").on(view.id, &view.name).details(audit::snapshot(&view)).record(&state, &actor).await;
    Ok((StatusCode::CREATED, Json(view)))
}

async fn update_credential(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<Uuid>,
    Json(i): Json<CredentialInput>,
) -> R<Json<CredentialView>> {
    let mut row = db::get_credential(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    let before = credential_view(&state, row.clone(), 0);
    if i.provider.as_deref().is_some_and(|p| p != row.provider) {
        return Err(AdminError::bad_request("a credential's provider cannot be changed; create a new credential"));
    }
    let provider = ProviderKind::parse(&row.provider).ok_or_else(|| AdminError::bad_request("unknown provider"))?;
    let stored = registry::decrypt_secrets(&state.cfg.secret_box, row.secrets.as_deref())
        .map_err(|e| AdminError(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let secrets_replaced = i.secrets.values().next().is_some();
    let settings = clean_settings(&state, provider, i.settings)?;
    // Stored secrets only go where they went before. Pointed at another host, the credential
    // keeps none of them, so a changed endpoint cannot collect them; the provider's own default
    // endpoint is always safe.
    let moved = settings
        .base_url
        .as_deref()
        .is_some_and(|new| !row.settings.0.base_url.as_deref().is_some_and(|old| crate::util::same_origin(old, new)));
    let mut secrets = if moved {
        if !secrets_replaced && stored.values().next().is_some() {
            return Err(AdminError::bad_request("the endpoint is on another host now: enter the credential's secrets again"));
        }
        Secrets::default()
    } else {
        stored
    };
    secrets.merge(i.secrets);
    validate_credential(provider, &i.name, &secrets, &settings)?;
    row.settings = sqlx::types::Json(settings);
    row.name = i.name.trim().to_string();
    row.secrets = registry::encrypt_secrets(&state.cfg.secret_box, &secrets);
    db::update_credential(&state.db, &row).await.map_err(conflict_on_duplicate_name)?;
    // Deployments using this credential pick up the new secrets and settings on every replica.
    broadcast_reload(&state, "credential updated").await;
    tracing::info!(credential = %id, "credential updated");
    let usage = db::credential_usage(&state.db).await?;
    let n = usage.get(&id).copied().unwrap_or(0);
    let view = credential_view(&state, row, n);
    // Secrets show only as set or not set (`secrets_set`); a replaced secret is not a change there.
    let mut changes = audit::changes(&before, &view);
    if secrets_replaced {
        changes.insert("secrets".into(), json!("replaced"));
    }
    if !changes.is_empty() {
        audit::Event::new("credential", "update").on(id, &view.name).details(changes.into()).record(&state, &actor).await;
    }
    Ok(Json(view))
}

async fn delete_credential(State(state): State<AppState>, Extension(actor): Extension<Actor>, Path(id): Path<Uuid>) -> R<StatusCode> {
    let in_use = |n: i64| AdminError::conflict(format!("credential is used by {n} deployment(s); reassign or delete them first"));
    if let Some(&n) = db::credential_usage(&state.db).await?.get(&id) {
        return Err(in_use(n));
    }
    if let Some(&n) = db::credential_guardrail_usage(&state.db).await?.get(&id) {
        return Err(AdminError::conflict(format!("credential is used by {n} guardrail(s); reassign or delete them first")));
    }
    let name = db::get_credential(&state.db, id).await?.ok_or_else(AdminError::not_found)?.name;
    match db::delete_credential(&state.db, id).await {
        Ok(true) => {
            tracing::info!(credential = %id, "credential deleted");
            audit::Event::new("credential", "delete").on(id, name).record(&state, &actor).await;
            Ok(StatusCode::NO_CONTENT)
        }
        Ok(false) => Err(AdminError::not_found()),
        // A deployment or guardrail started using it after the checks above.
        Err(e) if db::is_foreign_key_violation(&e) => {
            Err(AdminError::conflict("credential is in use; reassign or delete what uses it first"))
        }
        Err(e) => Err(e.into()),
    }
}

/// The deployments on an Azure credential's resource, read from Azure Resource Manager with its
/// Entra ID app, for the console to fill in a new deployment.
async fn foundry_deployments(State(state): State<AppState>, Path(id): Path<Uuid>) -> R<Json<crate::entra::FoundryDeployments>> {
    let row = db::get_credential(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    if row.provider != ProviderKind::Azure.as_str() {
        return Err(AdminError::bad_request("only Azure credentials have Foundry deployments"));
    }
    let secrets = registry::decrypt_secrets(&state.cfg.secret_box, row.secrets.as_deref())
        .map_err(|e| AdminError(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let settings = &row.settings.0;
    let source = crate::entra::source(settings, &secrets)
        .ok_or_else(|| AdminError::bad_request("listing deployments needs a credential that uses Entra ID; an API key cannot read them"))?;
    let base = settings.base_url.as_deref().ok_or_else(|| AdminError::bad_request("the credential has no resource endpoint"))?;
    let found = crate::entra::list_deployments(&state, &source, base).await.map_err(|e| AdminError(StatusCode::BAD_GATEWAY, e))?;
    Ok(Json(found))
}

/// The AWS regions the gateway's SDK knows, for the console's Bedrock region list.
async fn aws_regions() -> Json<&'static [crate::aws::AwsRegion]> {
    Json(crate::aws::regions())
}

/// The models and inference profiles a Bedrock credential can call, with its account's quotas,
/// for the console to fill in a new deployment. Listings it may not read are noted, not failed.
async fn bedrock_models(State(state): State<AppState>, Path(id): Path<Uuid>) -> R<Json<crate::aws::models::BedrockModels>> {
    let row = db::get_credential(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    if row.provider != ProviderKind::Bedrock.as_str() {
        return Err(AdminError::bad_request("only Bedrock credentials have Bedrock models"));
    }
    let secrets = registry::decrypt_secrets(&state.cfg.secret_box, row.secrets.as_deref())
        .map_err(|e| AdminError(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    if secrets.api_key.is_some() {
        return Err(AdminError::bad_request("a Bedrock API key cannot list models or quotas; use AWS credentials for that"));
    }
    let found = state
        .aws
        .models
        .get(&state, row.id, row.updated_at, &row.settings.0, &secrets)
        .await
        .map_err(|e| AdminError(StatusCode::BAD_GATEWAY, e.message))?;
    Ok(Json(found))
}

// ---------------------------------------------------------------- deployments

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct DeploymentView {
    id: Uuid,
    model_name: String,
    mode: String,
    provider: String,
    upstream_model: String,
    config: DeploymentConfig,
    credential_id: Uuid,
    credential_name: Option<String>,
    weight: i32,
    priority: i32,
    rpm_limit: Option<i32>,
    tpm_limit: Option<i32>,
    timeout_secs: i32,
    input_cost_per_mtok: f64,
    output_cost_per_mtok: f64,
    cache_read_cost_per_mtok: Option<f64>,
    cache_write_cost_per_mtok: Option<f64>,
    long_context_threshold: Option<i32>,
    long_input_cost_per_mtok: Option<f64>,
    long_output_cost_per_mtok: Option<f64>,
    long_cache_read_cost_per_mtok: Option<f64>,
    long_cache_write_cost_per_mtok: Option<f64>,
    cost_per_k_searches: Option<f64>,
    base_model: Option<String>,
    tags: Vec<String>,
    project_ids: Vec<Uuid>,
    enabled: bool,
    cooldown_secs: Option<i64>,
    /// Its recent calls, as adaptive routing sees them; absent without any.
    health: Option<HealthView>,
    /// The reasoning efforts it accepts, from its price catalog entry; absent while it is not
    /// loaded (disabled).
    reasoning_efforts: Option<Vec<crate::model::ReasoningEffort>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

/// A deployment's recent calls, each counting half as much a minute later.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct HealthView {
    calls: f64,
    /// Share of the calls that failed with a retryable error (rate limit, server error, timeout).
    error_rate: f64,
    /// Average time to first token of streamed responses.
    ttft_ms: Option<f64>,
    /// The share of its weight the deployment gets now: 1 when healthy, down to 0.05.
    weight_factor: f64,
}

fn view(r: DeploymentRow, credential_name: Option<String>, cooldown: Option<i64>) -> DeploymentView {
    DeploymentView {
        id: r.id,
        model_name: r.model_name,
        mode: r.mode,
        provider: r.provider,
        upstream_model: r.upstream_model,
        // Credential headers saved before they were refused are not shown.
        config: r.config.0.redacted(),
        credential_id: r.credential_id,
        credential_name,
        weight: r.weight,
        priority: r.priority,
        rpm_limit: r.rpm_limit,
        tpm_limit: r.tpm_limit,
        timeout_secs: r.timeout_secs,
        input_cost_per_mtok: r.input_cost_per_mtok,
        output_cost_per_mtok: r.output_cost_per_mtok,
        cache_read_cost_per_mtok: r.cache_read_cost_per_mtok,
        cache_write_cost_per_mtok: r.cache_write_cost_per_mtok,
        long_context_threshold: r.long_context_threshold,
        long_input_cost_per_mtok: r.long_input_cost_per_mtok,
        long_output_cost_per_mtok: r.long_output_cost_per_mtok,
        long_cache_read_cost_per_mtok: r.long_cache_read_cost_per_mtok,
        long_cache_write_cost_per_mtok: r.long_cache_write_cost_per_mtok,
        cost_per_k_searches: r.cost_per_k_searches,
        base_model: r.base_model,
        tags: r.tags,
        project_ids: r.project_ids,
        enabled: r.enabled,
        cooldown_secs: cooldown,
        health: None,
        reasoning_efforts: None,
        created_at: r.created_at,
        updated_at: r.updated_at,
    }
}

/// The health of every deployment with recent calls, each weighed against the others of its
/// model, as `router::pick` does.
async fn deployment_health(state: &AppState, rows: &[DeploymentRow]) -> HashMap<Uuid, HealthView> {
    if !state.settings.get().adaptive_routing {
        return HashMap::new();
    }
    let ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
    let health = state.cache.health(&ids).await;
    let mut models: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, r) in rows.iter().enumerate() {
        models.entry(r.model_name.as_str()).or_default().push(i);
    }
    let mut out = HashMap::new();
    for members in models.values() {
        let priorities: Vec<i32> = members.iter().map(|&i| rows[i].priority).collect();
        let hs: Vec<crate::cache::Health> = members.iter().map(|&i| health[i]).collect();
        for (&i, factor) in members.iter().zip(crate::router::factors(&priorities, &hs)) {
            let h = health[i];
            let calls = h.ok + h.err;
            if calls >= 0.05 {
                out.insert(rows[i].id, HealthView { calls, error_rate: h.err / calls, ttft_ms: h.ttft_ms, weight_factor: factor });
            }
        }
    }
    out
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeploymentInput {
    model_name: String,
    /// chat (default), embedding, decision or rerank.
    #[serde(default)]
    mode: Mode,
    provider: String,
    upstream_model: String,
    /// An existing credential for the same provider. Create credentials first.
    credential_id: Uuid,
    #[serde(default)]
    config: DeploymentConfig,
    #[serde(default = "one")]
    weight: i32,
    #[serde(default)]
    priority: i32,
    rpm_limit: Option<i32>,
    tpm_limit: Option<i32>,
    #[serde(default = "default_timeout")]
    timeout_secs: i32,
    #[serde(default)]
    input_cost_per_mtok: f64,
    #[serde(default)]
    output_cost_per_mtok: f64,
    cache_read_cost_per_mtok: Option<f64>,
    cache_write_cost_per_mtok: Option<f64>,
    /// Input tokens above which the long_* prices apply to the whole request. None = one price tier.
    long_context_threshold: Option<i32>,
    long_input_cost_per_mtok: Option<f64>,
    long_output_cost_per_mtok: Option<f64>,
    long_cache_read_cost_per_mtok: Option<f64>,
    long_cache_write_cost_per_mtok: Option<f64>,
    /// USD per 1,000 searches: for rerank models a query over up to 100 documents, for chat
    /// models a web search the model ran.
    cost_per_k_searches: Option<f64>,
    /// Price catalog key, e.g. `azure/gpt-4o` for an Azure deployment with its own name.
    #[serde(default)]
    base_model: Option<String>,
    /// Permission tags; a key with tags needs all of them to use this deployment.
    #[serde(default)]
    tags: Vec<String>,
    /// Projects whose keys alone may use this deployment, whatever tags other keys hold; empty
    /// for any key. Omitted, an update keeps the deployment's projects.
    #[serde(default)]
    project_ids: Option<Vec<Uuid>>,
    #[serde(default = "yes")]
    enabled: bool,
}

fn one() -> i32 {
    1
}
fn default_timeout() -> i32 {
    600
}
fn yes() -> bool {
    true
}

fn valid_model_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.:/@".contains(c))
}

/// Validates the input and returns the referenced credential's name.
/// `id` is the deployment being updated, if any.
async fn validate(state: &AppState, id: Option<Uuid>, i: &DeploymentInput) -> R<String> {
    let provider = check_deployment(i)?;
    let cred = db::get_credential(&state.db, i.credential_id)
        .await?
        .ok_or_else(|| AdminError::bad_request("credential_id does not refer to an existing credential"))?;
    let (deployments, routers, aliases) =
        tokio::try_join!(db::list_deployments(&state.db), db::list_routers(&state.db), db::list_aliases(&state.db))?;
    check_deployment_fits(i, provider, id, &cred, &deployments, &routers, &aliases)?;
    Ok(cred.name)
}

/// Sorts the projects a deployment is limited to, drops repeats, and checks that each exists.
async fn check_projects(state: &AppState, ids: Option<&mut Vec<Uuid>>) -> R<()> {
    let Some(ids) = ids else { return Ok(()) };
    ids.sort();
    ids.dedup();
    if ids.is_empty() {
        return Ok(());
    }
    let projects = db::list_project_rows(&state.db).await?;
    match ids.iter().find(|id| !projects.iter().any(|p| p.id == **id)) {
        Some(id) => Err(AdminError::bad_request(format!("project_ids: {id} names no project"))),
        None => Ok(()),
    }
}

/// The checks of a deployment that need nothing but the input itself.
fn check_deployment(i: &DeploymentInput) -> R<ProviderKind> {
    let provider = ProviderKind::parse(&i.provider).ok_or_else(|| AdminError::bad_request("unknown provider"))?;
    if !valid_model_name(&i.model_name) {
        return Err(AdminError::bad_request("model_name must be 1-128 chars of [A-Za-z0-9-_.:/@]"));
    }
    if i.upstream_model.trim().is_empty() {
        return Err(AdminError::bad_request("upstream_model is required"));
    }
    if i.base_model.as_ref().is_some_and(|b| b.len() > 300) {
        return Err(AdminError::bad_request("base_model can be at most 300 characters"));
    }
    let style = i.mode.style(provider, i.config.api_style).map_err(AdminError::bad_request)?;
    if i.config.bedrock_api.is_some() && !(provider == ProviderKind::Bedrock && style == crate::model::ApiStyle::Anthropic) {
        return Err(AdminError::bad_request("bedrock_api only applies to Bedrock deployments with the Anthropic Messages protocol"));
    }
    i.config.check().map_err(AdminError::bad_request)?;
    if i.weight < 1 || i.timeout_secs < 1 {
        return Err(AdminError::bad_request("weight and timeout_secs must be positive"));
    }
    if i.rpm_limit.is_some_and(|v| v < 1) || i.tpm_limit.is_some_and(|v| v < 1) {
        return Err(AdminError::bad_request("limits must be positive"));
    }
    let prices = [
        Some(i.input_cost_per_mtok),
        Some(i.output_cost_per_mtok),
        i.cache_read_cost_per_mtok,
        i.cache_write_cost_per_mtok,
        i.long_input_cost_per_mtok,
        i.long_output_cost_per_mtok,
        i.long_cache_read_cost_per_mtok,
        i.long_cache_write_cost_per_mtok,
        i.cost_per_k_searches,
    ];
    if prices.into_iter().flatten().any(|p| !p.is_finite() || p < 0.0) {
        return Err(AdminError::bad_request("prices must be non-negative numbers"));
    }
    match i.long_context_threshold {
        Some(t) if t < 1 => return Err(AdminError::bad_request("long_context_threshold must be a positive token count")),
        Some(_) if i.long_input_cost_per_mtok.is_none() || i.long_output_cost_per_mtok.is_none() => {
            return Err(AdminError::bad_request("long-context pricing needs long_input_cost_per_mtok and long_output_cost_per_mtok"));
        }
        _ => {}
    }
    Ok(provider)
}

/// The checks of a deployment against its credential and the rest of the configuration
/// (`deployments`, `routers` and `aliases`; `deployments` may include the deployment itself, as `id`).
fn check_deployment_fits(
    i: &DeploymentInput,
    provider: ProviderKind,
    id: Option<Uuid>,
    cred: &CredentialRow,
    deployments: &[DeploymentRow],
    routers: &[RouterRow],
    aliases: &[AliasRow],
) -> R<()> {
    if cred.provider != provider.as_str() {
        return Err(AdminError::bad_request(format!("credential '{}' is for {}, not {}", cred.name, cred.provider, i.provider)));
    }
    if routers.iter().any(|r| r.model_name == i.model_name) {
        return Err(AdminError::bad_request(format!("'{}' is an auto router's model name", i.model_name)));
    }
    if aliases.iter().any(|a| a.model_name == i.model_name) {
        return Err(AdminError::bad_request(format!("'{}' is a model alias", i.model_name)));
    }
    // One model name is one kind of model: clients call one family of endpoints for it.
    let other = deployments.iter().find(|d| d.model_name == i.model_name && Some(d.id) != id && d.mode != i.mode.as_str());
    if let Some(d) = other {
        return Err(AdminError::bad_request(format!(
            "'{}' already has {} deployments; all deployments of a model name need the same mode",
            i.model_name, d.mode
        )));
    }
    Ok(())
}

fn to_row(id: Uuid, i: DeploymentInput, created: DateTime<Utc>) -> DeploymentRow {
    DeploymentRow {
        id,
        model_name: i.model_name,
        mode: i.mode.as_str().into(),
        provider: i.provider,
        upstream_model: i.upstream_model.trim().to_string(),
        // Only chat deployments choose a protocol.
        config: sqlx::types::Json(DeploymentConfig { api_style: i.config.api_style.filter(|_| i.mode == Mode::Chat), ..i.config }),
        credential_id: i.credential_id,
        weight: i.weight,
        priority: i.priority,
        rpm_limit: i.rpm_limit,
        tpm_limit: i.tpm_limit,
        timeout_secs: i.timeout_secs,
        input_cost_per_mtok: i.input_cost_per_mtok,
        output_cost_per_mtok: i.output_cost_per_mtok,
        cache_read_cost_per_mtok: i.cache_read_cost_per_mtok,
        cache_write_cost_per_mtok: i.cache_write_cost_per_mtok,
        // Without a threshold there is no long-context tier, so its prices are not kept.
        long_context_threshold: i.long_context_threshold,
        long_input_cost_per_mtok: i.long_context_threshold.and(i.long_input_cost_per_mtok),
        long_output_cost_per_mtok: i.long_context_threshold.and(i.long_output_cost_per_mtok),
        long_cache_read_cost_per_mtok: i.long_context_threshold.and(i.long_cache_read_cost_per_mtok),
        long_cache_write_cost_per_mtok: i.long_context_threshold.and(i.long_cache_write_cost_per_mtok),
        // Rerank models bill their searches, chat models the web searches they run.
        cost_per_k_searches: i.cost_per_k_searches.filter(|_| matches!(i.mode, Mode::Rerank | Mode::Chat)),
        base_model: i.base_model.map(|b| b.trim().to_string()).filter(|b| !b.is_empty()),
        tags: i.tags,
        project_ids: i.project_ids.unwrap_or_default(),
        enabled: i.enabled,
        created_at: created,
        updated_at: Utc::now(),
    }
}

async fn credential_names(state: &AppState) -> R<HashMap<Uuid, String>> {
    Ok(db::list_credentials(&state.db).await?.into_iter().map(|c| (c.id, c.name)).collect())
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct DeploymentQuery {
    /// Just the deployments whose model name contains this text, ignoring case.
    q: Option<String>,
    /// Just the deployments of this mode: `chat`, `embedding`, `decision` or `rerank`.
    mode: Option<String>,
    /// Just the deployments carrying any of these tags, as `azure,cheap`; `-` stands for none.
    tags: Option<String>,
    /// Just the deployments using any of these credentials (ids, comma-separated).
    credentials: Option<String>,
    /// Just the deployments limited to any of these projects (ids, comma-separated); `-` stands
    /// for those open to every project.
    projects: Option<String>,
    /// Just these deployments (ids, comma-separated).
    ids: Option<String>,
}

async fn list_deployments(State(state): State<AppState>, Query(q): Query<DeploymentQuery>) -> R<Json<Vec<DeploymentView>>> {
    let search = search_text(q.q.as_deref());
    let mode = q.mode.as_deref().map(str::trim).filter(|m| !m.is_empty());
    if let Some(m) = mode.filter(|m| Mode::parse(m).is_none()) {
        return Err(AdminError::bad_request(format!("unknown mode '{m}'")));
    }
    let (tags, projects) = (tag_filter(q.tags.as_deref())?, project_filter(q.projects.as_deref())?);
    let (credentials, ids) = (csv_ids(q.credentials.as_deref(), "credentials")?, csv_ids(q.ids.as_deref(), "ids")?);
    let rows = db::search_deployments(&state.db, search.as_deref(), mode, &tags, credentials.as_deref(), &projects, ids.as_deref());
    let (rows, names) = tokio::try_join!(async { Ok(rows.await?) }, credential_names(&state))?;
    let ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
    let (cds, mut health) = tokio::join!(state.cache.cooldown_ttls(&ids), deployment_health(&state, &rows));
    let snap = state.registry.snapshot();
    let efforts: HashMap<Uuid, Vec<crate::model::ReasoningEffort>> =
        snap.deployments.values().flatten().map(|d| (d.id, d.efforts())).collect();
    Ok(Json(
        rows.into_iter()
            .map(|r| {
                let (name, cd, h) = (names.get(&r.credential_id).cloned(), cds.get(&r.id).copied(), health.remove(&r.id));
                let reasoning_efforts = efforts.get(&r.id).cloned();
                DeploymentView { health: h, reasoning_efforts, ..view(r, name, cd) }
            })
            .collect(),
    ))
}

async fn get_deployment(State(state): State<AppState>, Path(id): Path<Uuid>) -> R<Json<DeploymentView>> {
    let row = db::get_deployment(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    let name = credential_names(&state).await?.remove(&row.credential_id);
    Ok(Json(view(row, name, None)))
}

async fn create_deployment(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(mut i): Json<DeploymentInput>,
) -> R<(StatusCode, Json<DeploymentView>)> {
    i.tags = normalize_tags(std::mem::take(&mut i.tags)).map_err(AdminError::bad_request)?;
    check_projects(&state, i.project_ids.as_mut()).await?;
    let cred_name = validate(&state, None, &i).await?;
    let row = to_row(Uuid::now_v7(), i, Utc::now());
    db::upsert_deployment(&state.db, &row).await?;
    broadcast_reload(&state, "deployment created").await;
    tracing::info!(deployment = %row.id, model = %row.model_name, "deployment created");
    let view = view(row, Some(cred_name), None);
    audit::Event::new("deployment", "create").on(view.id, &view.model_name).details(audit::snapshot(&view)).record(&state, &actor).await;
    Ok((StatusCode::CREATED, Json(view)))
}

async fn update_deployment(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<Uuid>,
    Json(mut i): Json<DeploymentInput>,
) -> R<Json<DeploymentView>> {
    let existing = db::get_deployment(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    // Clients of a model call the endpoints of its mode; changing it would break them.
    if existing.mode != i.mode.as_str() {
        return Err(AdminError::bad_request("a deployment's mode cannot be changed; create a new deployment"));
    }
    i.tags = normalize_tags(std::mem::take(&mut i.tags)).map_err(AdminError::bad_request)?;
    // Clients that predate projects on deployments leave them as they are.
    check_projects(&state, i.project_ids.as_mut()).await?;
    i.project_ids.get_or_insert_with(|| existing.project_ids.clone());
    let cred_name = validate(&state, Some(id), &i).await?;
    let row = to_row(id, i, existing.created_at);
    db::upsert_deployment(&state.db, &row).await?;
    if existing.model_name != row.model_name {
        keep_aliases(&state, id, &existing.model_name).await?;
        follow_rename(&state, &existing.model_name, &row.model_name).await?;
    }
    broadcast_reload(&state, "deployment updated").await;
    tracing::info!(deployment = %id, "deployment updated");
    let before_cred = if existing.credential_id == row.credential_id {
        Some(cred_name.clone())
    } else {
        credential_names(&state).await?.remove(&existing.credential_id)
    };
    let before = view(existing, before_cred, None);
    let view = view(row, Some(cred_name), None);
    let changes = audit::changes(&before, &view);
    if !changes.is_empty() {
        audit::Event::new("deployment", "update").on(id, &view.model_name).details(changes.into()).record(&state, &actor).await;
    }
    Ok(Json(view))
}

/// Once no deployment or router keeps a model's old name, routers and keys that named it follow
/// the rename, so routing and key access keep working, and so does the model's metadata.
async fn follow_rename(state: &AppState, old: &str, new: &str) -> R<()> {
    let (deployments, mut routers, mut keys) =
        tokio::try_join!(db::list_deployments(&state.db), db::list_routers(&state.db), db::list_keys(&state.db))?;
    let (changed_routers, changed_keys) = rename_model(old, new, &deployments, &mut routers, &mut keys);
    for r in routers.iter().filter(|r| changed_routers.contains(&r.id)) {
        db::upsert_router(&state.db, r).await?;
        tracing::info!(router = %r.id, old, new, "router follows model rename");
    }
    for k in keys.iter().filter(|k| changed_keys.contains(&k.id)) {
        db::update_key(&state.db, k).await?;
        tracing::info!(key = %k.id, old, new, "key follows model rename");
    }
    // The model hub's name and description too, once nothing has the old name.
    let still_named = deployments.iter().any(|d| d.model_name == old) || routers.iter().any(|r| r.model_name == old);
    if old != new && !still_named && db::rename_model_metadata(&state.db, old, new).await? {
        tracing::info!(old, new, "model metadata follows model rename");
    }
    Ok(())
}

/// Points the routers and keys that name model `old` at `new`, unless a deployment or router is
/// still called `old`. Returns the ids of the routers and keys it changed. (Model metadata follows
/// separately, see `follow_rename` and the import.)
pub(crate) fn rename_model(
    old: &str,
    new: &str,
    deployments: &[DeploymentRow],
    routers: &mut [RouterRow],
    keys: &mut [VirtualKey],
) -> (HashSet<Uuid>, HashSet<Uuid>) {
    let (mut changed_routers, mut changed_keys) = (HashSet::new(), HashSet::new());
    if old == new || deployments.iter().any(|d| d.model_name == old) || routers.iter().any(|r| r.model_name == old) {
        return (changed_routers, changed_keys);
    }
    for r in routers.iter_mut() {
        if rename_in_router(r, old, new) {
            changed_routers.insert(r.id);
        }
    }
    for k in keys.iter_mut().filter(|k| k.allowed_models.iter().any(|m| m == old)) {
        for m in k.allowed_models.iter_mut().filter(|m| *m == old) {
            *m = new.to_string();
        }
        k.allowed_models.sort();
        k.allowed_models.dedup();
        changed_keys.insert(k.id);
    }
    (changed_routers, changed_keys)
}

/// Point a router's references to `old` at `new`; returns whether anything changed. If a route
/// already had `new` among its models, the first of the two is kept.
fn rename_in_router(r: &mut RouterRow, old: &str, new: &str) -> bool {
    let mut changed = false;
    if r.decision_model == old {
        r.decision_model = new.to_string();
        changed = true;
    }
    for route in r.routes.0.iter_mut() {
        for m in route.models.iter_mut().filter(|m| m.model == old) {
            m.model = new.to_string();
            changed = true;
        }
        let mut seen = BTreeSet::new();
        route.models.retain(|m| seen.insert(m.model.clone()));
    }
    changed
}

async fn delete_deployment(State(state): State<AppState>, Extension(actor): Extension<Actor>, Path(id): Path<Uuid>) -> R<StatusCode> {
    let existing = db::get_deployment(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    if !db::delete_deployment(&state.db, id).await? {
        return Err(AdminError::not_found());
    }
    keep_aliases(&state, id, &existing.model_name).await?;
    broadcast_reload(&state, "deployment deleted").await;
    tracing::info!(deployment = %id, "deployment deleted");
    let details = json!({ "upstream_model": existing.upstream_model, "provider": existing.provider });
    audit::Event::new("deployment", "delete").on(id, existing.model_name).details(details).record(&state, &actor).await;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- auto routers

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct RouterView {
    id: Uuid,
    model_name: String,
    decision_model: String,
    instructions: Option<String>,
    routes: Vec<Route>,
    fallback_route: Option<String>,
    tags: Vec<String>,
    enabled: bool,
    /// Conversations stay on the route and model their first turn was sent to.
    sticky: bool,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

fn router_view(r: RouterRow) -> RouterView {
    RouterView {
        id: r.id,
        model_name: r.model_name,
        decision_model: r.decision_model,
        instructions: r.instructions,
        routes: r.routes.0,
        fallback_route: r.fallback_route,
        tags: r.tags,
        enabled: r.enabled,
        sticky: r.sticky,
        created_at: r.created_at,
        updated_at: r.updated_at,
    }
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RouterInput {
    model_name: String,
    /// Model name of decision-mode deployments.
    decision_model: String,
    #[serde(default)]
    instructions: Option<String>,
    /// The routes to choose from: each a name, a description of when to use it, and the chat
    /// models it sends requests to (one at random), with an optional reasoning effort each. A
    /// route given as `{"model", "description"}`, as before, is a one-model route named after it.
    routes: Vec<Route>,
    /// The name of the route used when the decision fails. Defaults to the first route.
    /// `fallback_model` is accepted too, as before.
    #[serde(default, alias = "fallback_model")]
    fallback_route: Option<String>,
    /// Permission tags, with the same rule as a deployment's.
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default = "yes")]
    enabled: bool,
    /// Keep each conversation on the route and model its first turn was sent to, instead of
    /// deciding every turn anew: the prompt cache stays warm and later turns need no decision.
    #[serde(default = "yes")]
    sticky: bool,
}

/// Routing is decided by choice labels, so at most this many routes (the decision API allows 255).
const MAX_ROUTES: usize = 32;
/// Models in one route.
const MAX_ROUTE_MODELS: usize = 16;

async fn validate_router(state: &AppState, id: Option<Uuid>, i: &mut RouterInput) -> R<()> {
    let (deployments, routers, aliases) =
        tokio::try_join!(db::list_deployments(&state.db), db::list_routers(&state.db), db::list_aliases(&state.db))?;
    check_router(i, id, &deployments, &routers, &aliases)
}

/// Tidies a router's input and checks it against `deployments`, `routers` (which may include
/// the router itself, as `id`) and `aliases`. Routes name deployments' models, not aliases.
fn check_router(
    i: &mut RouterInput,
    id: Option<Uuid>,
    deployments: &[DeploymentRow],
    routers: &[RouterRow],
    aliases: &[AliasRow],
) -> R<()> {
    i.model_name = i.model_name.trim().to_string();
    i.instructions = i.instructions.take().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    i.fallback_route = i.fallback_route.take().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    i.tags = normalize_tags(std::mem::take(&mut i.tags)).map_err(AdminError::bad_request)?;
    if !valid_model_name(&i.model_name) {
        return Err(AdminError::bad_request("model_name must be 1-128 chars of [A-Za-z0-9-_.:/@]"));
    }
    if i.instructions.as_ref().is_some_and(|s| s.len() > 2000) {
        return Err(AdminError::bad_request("instructions can be at most 2000 characters"));
    }
    if i.routes.is_empty() || i.routes.len() > MAX_ROUTES {
        return Err(AdminError::bad_request(format!("a router needs 1 to {MAX_ROUTES} routes")));
    }
    let mode_of = |name: &str| deployments.iter().find(|d| d.model_name == name).map(|d| d.mode.as_str());
    if mode_of(&i.model_name).is_some() {
        return Err(AdminError::bad_request(format!("'{}' is already a deployment's model name", i.model_name)));
    }
    if routers.iter().any(|r| r.model_name == i.model_name && Some(r.id) != id) {
        return Err(AdminError::conflict(format!("a router named '{}' already exists", i.model_name)));
    }
    if aliases.iter().any(|a| a.model_name == i.model_name) {
        return Err(AdminError::bad_request(format!("'{}' is a model alias", i.model_name)));
    }
    if mode_of(&i.decision_model) != Some(Mode::Decision.as_str()) {
        return Err(AdminError::bad_request(format!("'{}' is not a decision model", i.decision_model)));
    }
    // Route names are the decision's choice labels: unique, ignoring case.
    let mut names = std::collections::HashSet::new();
    for r in &mut i.routes {
        r.name = r.name.trim().to_string();
        r.description = r.description.trim().to_string();
        if r.name.is_empty() || r.name.chars().count() > 128 || r.name.chars().any(char::is_control) {
            return Err(AdminError::bad_request("every route needs a name of 1-128 characters"));
        }
        if !names.insert(r.name.to_lowercase()) {
            return Err(AdminError::bad_request(format!("route '{}' is listed twice", r.name)));
        }
        if r.description.is_empty() || r.description.len() > 1000 {
            return Err(AdminError::bad_request(format!(
                "route '{}' needs a description of when to use it (at most 1000 characters)",
                r.name
            )));
        }
        if r.models.is_empty() || r.models.len() > MAX_ROUTE_MODELS {
            return Err(AdminError::bad_request(format!("route '{}' needs 1 to {MAX_ROUTE_MODELS} models", r.name)));
        }
        let mut models = std::collections::HashSet::new();
        for m in &mut r.models {
            m.model = m.model.trim().to_string();
            if mode_of(&m.model) != Some(Mode::Chat.as_str()) {
                return Err(AdminError::bad_request(format!("'{}' in route '{}' is not a chat model", m.model, r.name)));
            }
            if !models.insert(m.model.clone()) {
                return Err(AdminError::bad_request(format!("'{}' is listed twice in route '{}'", m.model, r.name)));
            }
        }
    }
    if let Some(f) = &i.fallback_route
        && !i.routes.iter().any(|r| r.name == *f)
    {
        return Err(AdminError::bad_request("fallback_route must be the name of one of the routes"));
    }
    Ok(())
}

fn router_row(id: Uuid, i: RouterInput, created_at: DateTime<Utc>) -> RouterRow {
    RouterRow {
        id,
        model_name: i.model_name,
        decision_model: i.decision_model,
        instructions: i.instructions,
        routes: sqlx::types::Json(i.routes),
        fallback_route: i.fallback_route,
        tags: i.tags,
        enabled: i.enabled,
        sticky: i.sticky,
        created_at,
        updated_at: Utc::now(),
    }
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct RouterQuery {
    /// Just the routers whose model name contains this text, ignoring case.
    q: Option<String>,
    /// Just the routers carrying any of these tags, as `azure,cheap`; `-` stands for none.
    tags: Option<String>,
}

/// A router in the list, with the models it names that no deployment serves.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct RouterListItem {
    #[serde(flatten)]
    router: RouterView,
    /// Its decision model when no decision deployment has that name, and its routes' models that
    /// no chat deployment has.
    missing: Vec<String>,
}

async fn list_routers(State(state): State<AppState>, Query(q): Query<RouterQuery>) -> R<Json<Vec<RouterListItem>>> {
    let search = search_text(q.q.as_deref());
    let tags = tag_filter(q.tags.as_deref())?;
    let (routers, deployments) =
        tokio::try_join!(db::search_routers(&state.db, search.as_deref(), &tags), db::list_deployments(&state.db))?;
    let serves = |name: &str, mode: Mode| deployments.iter().any(|d| d.model_name == name && d.mode == mode.as_str());
    Ok(Json(
        routers
            .into_iter()
            .map(|r| {
                let mut missing: BTreeSet<String> =
                    r.routes.0.iter().flat_map(|route| &route.models).map(|m| m.model.clone()).filter(|m| !serves(m, Mode::Chat)).collect();
                if !serves(&r.decision_model, Mode::Decision) {
                    missing.insert(r.decision_model.clone());
                }
                RouterListItem { router: router_view(r), missing: missing.into_iter().collect() }
            })
            .collect(),
    ))
}

async fn create_router(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(mut i): Json<RouterInput>,
) -> R<(StatusCode, Json<RouterView>)> {
    validate_router(&state, None, &mut i).await?;
    let row = router_row(Uuid::now_v7(), i, Utc::now());
    db::upsert_router(&state.db, &row).await?;
    broadcast_reload(&state, "router created").await;
    tracing::info!(router = %row.id, model = %row.model_name, "router created");
    let view = router_view(row);
    audit::Event::new("router", "create").on(view.id, &view.model_name).details(audit::snapshot(&view)).record(&state, &actor).await;
    Ok((StatusCode::CREATED, Json(view)))
}

async fn update_router(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<Uuid>,
    Json(mut i): Json<RouterInput>,
) -> R<Json<RouterView>> {
    let existing = db::get_router(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    validate_router(&state, Some(id), &mut i).await?;
    let row = router_row(id, i, existing.created_at);
    db::upsert_router(&state.db, &row).await?;
    if existing.model_name != row.model_name {
        follow_rename(&state, &existing.model_name, &row.model_name).await?;
    }
    broadcast_reload(&state, "router updated").await;
    tracing::info!(router = %id, "router updated");
    let view = router_view(row);
    let changes = audit::changes(&router_view(existing), &view);
    if !changes.is_empty() {
        audit::Event::new("router", "update").on(id, &view.model_name).details(changes.into()).record(&state, &actor).await;
    }
    Ok(Json(view))
}

async fn delete_router(State(state): State<AppState>, Extension(actor): Extension<Actor>, Path(id): Path<Uuid>) -> R<StatusCode> {
    let existing = db::get_router(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    if !db::delete_router(&state.db, id).await? {
        return Err(AdminError::not_found());
    }
    broadcast_reload(&state, "router deleted").await;
    tracing::info!(router = %id, "router deleted");
    audit::Event::new("router", "delete").on(id, existing.model_name).record(&state, &actor).await;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- model aliases

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct AliasView {
    id: Uuid,
    model_name: String,
    /// The deployment or auto router whose model the alias serves.
    target_id: Uuid,
    /// The target's model name now; absent once the target is deleted.
    target: Option<String>,
    enabled: bool,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

fn alias_view(a: AliasRow, deployments: &[DeploymentRow], routers: &[RouterRow]) -> AliasView {
    AliasView {
        id: a.id,
        target: target_name(a.target_id, deployments, routers).map(str::to_string),
        model_name: a.model_name,
        target_id: a.target_id,
        enabled: a.enabled,
        created_at: a.created_at,
        updated_at: a.updated_at,
    }
}

/// The model name of the deployment or router `id`.
fn target_name<'a>(id: Uuid, deployments: &'a [DeploymentRow], routers: &'a [RouterRow]) -> Option<&'a str> {
    deployments
        .iter()
        .find(|d| d.id == id)
        .map(|d| d.model_name.as_str())
        .or_else(|| routers.iter().find(|r| r.id == id).map(|r| r.model_name.as_str()))
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AliasInput {
    /// The model name clients send as `model`.
    model_name: String,
    /// The id of a deployment or auto router: the alias serves its model name, whatever it is
    /// renamed to, with every deployment that has that name.
    target_id: Uuid,
    #[serde(default = "yes")]
    enabled: bool,
}

/// Tidies an alias's input and checks it against the models it may target and the other
/// aliases (`aliases` may include the alias itself, as `id`).
fn check_alias(i: &mut AliasInput, id: Option<Uuid>, deployments: &[DeploymentRow], routers: &[RouterRow], aliases: &[AliasRow]) -> R<()> {
    i.model_name = i.model_name.trim().to_string();
    if !valid_model_name(&i.model_name) {
        return Err(AdminError::bad_request("model_name must be 1-128 chars of [A-Za-z0-9-_.:/@]"));
    }
    if deployments.iter().any(|d| d.model_name == i.model_name) {
        return Err(AdminError::bad_request(format!("'{}' is already a deployment's model name", i.model_name)));
    }
    if routers.iter().any(|r| r.model_name == i.model_name) {
        return Err(AdminError::bad_request(format!("'{}' is an auto router's model name", i.model_name)));
    }
    if aliases.iter().any(|a| a.model_name == i.model_name && Some(a.id) != id) {
        return Err(AdminError::conflict(format!("an alias named '{}' already exists", i.model_name)));
    }
    if target_name(i.target_id, deployments, routers).is_none() {
        return Err(AdminError::bad_request("target_id is neither a deployment's nor an auto router's id"));
    }
    Ok(())
}

fn alias_row(id: Uuid, i: AliasInput, created_at: DateTime<Utc>) -> AliasRow {
    AliasRow { id, model_name: i.model_name, target_id: i.target_id, enabled: i.enabled, created_at, updated_at: Utc::now() }
}

/// Points the aliases that target a deployment which left model `old` (renamed or deleted) at
/// another deployment of `old`, an enabled one if there is any, so they keep serving the model
/// rather than the one deployment. Returns the ids of the aliases it changed; aliases whose
/// deployment was the model's last follow it to its new name, or are left without a target.
pub(crate) fn retarget_aliases(left: Uuid, old: &str, deployments: &[DeploymentRow], aliases: &mut [AliasRow]) -> HashSet<Uuid> {
    let mut changed = HashSet::new();
    let others = || deployments.iter().filter(|d| d.model_name == old && d.id != left);
    let Some(next) = others().find(|d| d.enabled).or_else(|| others().next()) else { return changed };
    for a in aliases.iter_mut().filter(|a| a.target_id == left) {
        a.target_id = next.id;
        changed.insert(a.id);
    }
    changed
}

/// `retarget_aliases` after deployment `id` left model `old`.
async fn keep_aliases(state: &AppState, id: Uuid, old: &str) -> R<()> {
    let (deployments, mut aliases) = tokio::try_join!(db::list_deployments(&state.db), db::list_aliases(&state.db))?;
    let changed = retarget_aliases(id, old, &deployments, &mut aliases);
    for a in aliases.iter().filter(|a| changed.contains(&a.id)) {
        db::upsert_alias(&state.db, a).await?;
        tracing::info!(alias = %a.id, deployment = %id, target = %a.target_id, model = old, "alias stays on its model");
    }
    Ok(())
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct AliasQuery {
    /// Just the aliases whose name, or whose target's model name, contains this text, ignoring case.
    q: Option<String>,
}

async fn list_aliases(State(state): State<AppState>, Query(q): Query<AliasQuery>) -> R<Json<Vec<AliasView>>> {
    let search = search_text(q.q.as_deref());
    let (aliases, deployments, routers) =
        tokio::try_join!(db::search_aliases(&state.db, search.as_deref()), db::list_deployments(&state.db), db::list_routers(&state.db))?;
    Ok(Json(aliases.into_iter().map(|a| alias_view(a, &deployments, &routers)).collect()))
}

async fn create_alias(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(mut i): Json<AliasInput>,
) -> R<(StatusCode, Json<AliasView>)> {
    let (deployments, routers, aliases) =
        tokio::try_join!(db::list_deployments(&state.db), db::list_routers(&state.db), db::list_aliases(&state.db))?;
    check_alias(&mut i, None, &deployments, &routers, &aliases)?;
    let row = alias_row(Uuid::now_v7(), i, Utc::now());
    db::upsert_alias(&state.db, &row).await?;
    broadcast_reload(&state, "alias created").await;
    tracing::info!(alias = %row.id, model = %row.model_name, "alias created");
    let view = alias_view(row, &deployments, &routers);
    audit::Event::new("alias", "create").on(view.id, &view.model_name).details(audit::snapshot(&view)).record(&state, &actor).await;
    Ok((StatusCode::CREATED, Json(view)))
}

async fn update_alias(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<Uuid>,
    Json(mut i): Json<AliasInput>,
) -> R<Json<AliasView>> {
    let existing = db::get_alias(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    let (deployments, routers, aliases) =
        tokio::try_join!(db::list_deployments(&state.db), db::list_routers(&state.db), db::list_aliases(&state.db))?;
    check_alias(&mut i, Some(id), &deployments, &routers, &aliases)?;
    let row = alias_row(id, i, existing.created_at);
    db::upsert_alias(&state.db, &row).await?;
    if existing.model_name != row.model_name {
        follow_rename(&state, &existing.model_name, &row.model_name).await?;
    }
    broadcast_reload(&state, "alias updated").await;
    tracing::info!(alias = %id, "alias updated");
    let view = alias_view(row, &deployments, &routers);
    let changes = audit::changes(&alias_view(existing, &deployments, &routers), &view);
    if !changes.is_empty() {
        audit::Event::new("alias", "update").on(id, &view.model_name).details(changes.into()).record(&state, &actor).await;
    }
    Ok(Json(view))
}

async fn delete_alias(State(state): State<AppState>, Extension(actor): Extension<Actor>, Path(id): Path<Uuid>) -> R<StatusCode> {
    let existing = db::get_alias(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    if !db::delete_alias(&state.db, id).await? {
        return Err(AdminError::not_found());
    }
    broadcast_reload(&state, "alias deleted").await;
    tracing::info!(alias = %id, "alias deleted");
    audit::Event::new("alias", "delete").on(id, existing.model_name).record(&state, &actor).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn clear_cooldown(State(state): State<AppState>, Extension(actor): Extension<Actor>, Path(id): Path<Uuid>) -> StatusCode {
    state.cache.clear_cooldown(id).await;
    let name = state.registry.snapshot().deployments.values().flatten().find(|d| d.id == id).map(|d| d.model_name.clone());
    audit::Event::new("deployment", "clear_cooldown").on(id, name.unwrap_or_default()).record(&state, &actor).await;
    StatusCode::NO_CONTENT
}

/// Send a tiny completion through the deployment (bypassing routing) to verify credentials.
async fn test_deployment(State(state): State<AppState>, Path(id): Path<Uuid>) -> R<Json<Value>> {
    let snap = state.registry.snapshot();
    let d = snap
        .deployments
        .values()
        .flatten()
        .find(|d| d.id == id)
        .cloned()
        .ok_or_else(|| AdminError::bad_request("deployment is not active (disabled or misconfigured)"))?;
    if d.mode == Mode::Embedding {
        let start = std::time::Instant::now();
        let result = crate::api::embeddings::probe(&state, &d).await;
        let latency_ms = start.elapsed().as_millis() as u64;
        return Ok(Json(match result {
            Ok((dims, usage)) => json!({
                "ok": true,
                "latency_ms": latency_ms,
                "text": format!("{dims} dimensions"),
                "usage": { "input_tokens": usage.input_tokens, "output_tokens": 0 },
            }),
            Err(e) => json!({ "ok": false, "latency_ms": latency_ms, "status": e.status.as_u16(), "error": e.message }),
        }));
    }
    if d.mode == Mode::Rerank {
        let start = std::time::Instant::now();
        let result = crate::api::rerank::probe(&state, &d).await;
        let latency_ms = start.elapsed().as_millis() as u64;
        return Ok(Json(match result {
            Ok((best, score, usage)) => json!({
                "ok": true,
                "latency_ms": latency_ms,
                // The second document answers the question.
                "text": format!("{} first (score {score:.3})", if best == 1 { "relevant document" } else { "unrelated document" }),
                "usage": { "input_tokens": usage.input_tokens, "output_tokens": 0 },
            }),
            Err(e) => json!({ "ok": false, "latency_ms": latency_ms, "status": e.status.as_u16(), "error": e.message }),
        }));
    }
    if d.mode == Mode::Decision {
        let start = std::time::Instant::now();
        let result = crate::api::decisions::probe(&state, &d).await;
        let latency_ms = start.elapsed().as_millis() as u64;
        return Ok(Json(match result {
            Ok(r) => json!({
                "ok": true,
                "latency_ms": latency_ms,
                "text": format!("reachable: {}", r.pointer("/answers/reachable/noul").and_then(Value::as_f64).unwrap_or_default()),
                "usage": { "input_tokens": r.pointer("/usage/input_tokens"), "output_tokens": r.pointer("/usage/output_tokens") },
            }),
            Err(e) => json!({ "ok": false, "latency_ms": latency_ms, "status": e.status.as_u16(), "error": e.message }),
        }));
    }
    let req = ChatRequest {
        messages: vec![Message {
            role: Role::User,
            content: vec![Part::Text { text: "Reply with the single word: OK".into(), cache: false }],
        }],
        max_tokens: Some(256),
        ..Default::default()
    };
    let start = std::time::Instant::now();
    let result = async {
        let mut events = crate::providers::stream_chat(&state, &d, &req).await?;
        let mut agg = Aggregator::default();
        while let Some(ev) = events.next().await {
            agg.push(&ev?);
        }
        Ok::<_, crate::error::GatewayError>(agg.finish())
    }
    .await;
    let latency_ms = start.elapsed().as_millis() as u64;
    Ok(Json(match result {
        Ok(r) => json!({
            "ok": true,
            "latency_ms": latency_ms,
            "text": crate::util::truncate(&crate::ir::parts_text(&r.content), 200),
            "usage": { "input_tokens": r.usage.total_input(), "output_tokens": r.usage.output_tokens },
        }),
        Err(e) => json!({ "ok": false, "latency_ms": latency_ms, "status": e.status.as_u16(), "error": e.message }),
    }))
}

// ---------------------------------------------------------------- keys

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct KeyInput {
    name: String,
    /// `inference` (the default), `admin` or `all`; in a project, `inference` or `project`.
    #[serde(default)]
    scope: KeyScope,
    /// The project to group the key under; none when omitted.
    #[serde(default)]
    project_id: Option<Uuid>,
    #[serde(default)]
    allowed_models: Vec<String>,
    /// Tags of existing deployments. Empty = no tag restriction.
    #[serde(default)]
    allowed_tags: Vec<String>,
    rpm_limit: Option<i32>,
    tpm_limit: Option<i32>,
    /// Budget in USD per `budget_period`; both None = unlimited.
    max_budget_usd: Option<f64>,
    /// "day", "week" or "month" (UTC calendar periods).
    budget_period: Option<String>,
    expires_at: Option<DateTime<Utc>>,
    #[serde(default = "yes")]
    enabled: bool,
    #[serde(default)]
    metadata: Option<Value>,
    /// Guardrails every request of the key goes through, on top of its project's. Admins only;
    /// omitted, they stay as they are.
    #[serde(default)]
    guardrail_ids: Option<Vec<Uuid>>,
    /// Whether requests may have the model search the web. A project that forbids it overrides this.
    #[serde(default = "yes")]
    web_search: bool,
    /// Whether its prompts and responses are logged, for debugging, where its project allows it.
    /// A project that denies or forces logging overrides this.
    #[serde(default)]
    prompt_logging: bool,
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct KeyView {
    #[serde(flatten)]
    key: VirtualKey,
    /// Current budget period and its spend; None for unlimited keys.
    budget: Option<crate::budget::BudgetStatus>,
    /// Whether its prompts and responses are logged now: its own `prompt_logging` where its
    /// project allows it, always where the project forces it.
    prompt_logged: bool,
}

fn validate_key(i: &KeyInput) -> R<()> {
    if i.name.trim().is_empty() || i.name.len() > 200 {
        return Err(AdminError::bad_request("name must be 1-200 characters"));
    }
    if i.rpm_limit.is_some_and(|v| v < 1) || i.tpm_limit.is_some_and(|v| v < 1) {
        return Err(AdminError::bad_request("limits must be positive"));
    }
    check_budget(i.max_budget_usd, i.budget_period.as_deref())?;
    match (i.project_id, i.scope) {
        (Some(_), s) if !s.fits_project() => {
            return Err(AdminError::bad_request(
                "a key with every right an admin has stays outside projects; in a project, use the inference or project scope",
            ));
        }
        (None, KeyScope::Project) => return Err(AdminError::bad_request("a key with the project scope needs a project_id")),
        _ => {}
    }
    if i.allowed_models.iter().any(|m| !valid_model_name(m)) {
        return Err(AdminError::bad_request("invalid model name in allowed_models"));
    }
    Ok(())
}

/// The guardrails a key or project gets: `wanted`, each once and in order, all existing; `held`
/// when not given. Only admins choose them, so that no one takes a guardrail off their own keys.
async fn pick_guardrails(state: &AppState, access: Option<&Access>, wanted: Option<Vec<Uuid>>, held: &[Uuid]) -> R<Vec<Uuid>> {
    let Some(mut ids) = wanted else { return Ok(held.to_vec()) };
    ids.sort();
    ids.dedup();
    let mut current = held.to_vec();
    current.sort();
    if ids == current {
        return Ok(held.to_vec());
    }
    if access.is_some_and(|a| a.projects().is_some()) {
        return Err(AdminError(StatusCode::FORBIDDEN, "only admins choose a key's guardrails".into()));
    }
    let known = db::list_guardrails(&state.db).await?;
    if let Some(id) = ids.iter().find(|id| !known.iter().any(|g| g.id == **id)) {
        return Err(AdminError::bad_request(format!("no such guardrail {id}")));
    }
    Ok(ids)
}

/// Whether a key in `project` may be used: false while the project is inactive.
async fn check_project(state: &AppState, project: Option<Uuid>) -> R<bool> {
    let Some(id) = project else { return Ok(true) };
    let p = db::get_project(&state.db, id).await?.ok_or_else(|| AdminError::bad_request("project_id names no project"))?;
    Ok(p.enabled)
}

/// A key's or project's budget: an amount with a period, or neither for unlimited.
fn check_budget(limit: Option<f64>, period: Option<&str>) -> R<()> {
    if limit.is_some_and(|v| v < 0.0 || !v.is_finite()) {
        return Err(AdminError::bad_request("max_budget_usd must be a non-negative number"));
    }
    match (limit, period) {
        (None, None) => Ok(()),
        (Some(_), Some(p)) if crate::budget::BudgetPeriod::parse(p).is_some() => Ok(()),
        (Some(_), Some(_)) => Err(AdminError::bad_request("budget_period must be day, week or month")),
        _ => Err(AdminError::bad_request("set max_budget_usd together with budget_period, or neither for unlimited")),
    }
}

/// Every tag on a deployment, router, MCP server or A2A agent, with how many deployments, routers
/// and MCP servers or agents carry it.
async fn known_tags(state: &AppState) -> R<BTreeMap<String, (usize, usize, usize)>> {
    let (deployments, routers, servers, agents) = tokio::try_join!(
        db::list_deployments(&state.db),
        db::list_routers(&state.db),
        db::list_mcp_servers(&state.db),
        db::list_a2a_agents(&state.db)
    )?;
    let mut tags: BTreeMap<String, (usize, usize, usize)> = BTreeMap::new();
    for t in agents.into_iter().flat_map(|a| a.tags) {
        tags.entry(t).or_default().2 += 1;
    }
    for t in deployments.into_iter().flat_map(|d| d.tags) {
        tags.entry(t).or_default().0 += 1;
    }
    for t in routers.into_iter().flat_map(|r| r.tags) {
        tags.entry(t).or_default().1 += 1;
    }
    for t in servers.into_iter().flat_map(|s| s.tags) {
        tags.entry(t).or_default().2 += 1;
    }
    Ok(tags)
}

async fn list_tags(State(state): State<AppState>, Extension(access): Extension<Access>) -> R<Json<Value>> {
    let (tags, held) = tokio::try_join!(known_tags(&state), async { Ok(db::key_tag_counts(&state.db, access.projects()).await?) })?;
    // Tags only keys still hold, once nothing carries them any more, come last.
    let mut out: Vec<Value> =
        tags.iter()
            .map(|(tag, (d, r, m))| json!({ "tag": tag, "deployments": d, "routers": r, "mcp_servers": m, "keys": held.get(tag).copied().unwrap_or(0) }))
            .collect();
    let mut orphans: Vec<(&String, &i64)> = held.iter().filter(|(t, _)| !tags.contains_key(*t)).collect();
    orphans.sort();
    out.extend(orphans.into_iter().map(|(tag, n)| json!({ "tag": tag, "deployments": 0, "routers": 0, "mcp_servers": 0, "keys": n })));
    Ok(Json(json!(out)))
}

/// A key's tags, normalized. Keys only pick tags that deployments, routers or MCP servers define;
/// tags a key already holds stay valid after the last one carrying them is gone.
async fn key_tags(state: &AppState, wanted: Vec<String>, held: &[String]) -> R<Vec<String>> {
    let tags = normalize_tags(wanted).map_err(AdminError::bad_request)?;
    let known = known_tags(state).await?;
    if let Some(t) = tags.iter().find(|t| !known.contains_key(*t) && !held.contains(t)) {
        return Err(AdminError::bad_request(format!(
            "no deployment, router, MCP server or A2A agent has the tag '{t}'; add tags on the Models or Tools page"
        )));
    }
    Ok(tags)
}

/// Whether the caller sees this key: a user only their projects' keys, which call the inference
/// API or reach what the user does (the project scope).
fn sees_key(access: &Access, k: &VirtualKey) -> bool {
    access.sees(k.project_id) && (access.projects().is_none() || k.scope.fits_project())
}

/// What a user may not touch looks like a key that does not exist.
fn check_sees_key(access: &Access, k: &VirtualKey) -> R<()> {
    if sees_key(access, k) { Ok(()) } else { Err(AdminError::not_found()) }
}

/// A user's keys stay in their projects, where a key has no more rights than the user
/// (`validate_key` keeps admin and all keys out of projects).
fn check_user_key(access: &Access, i: &KeyInput) -> R<()> {
    if access.projects().is_none() {
        return Ok(());
    }
    if !access.sees(i.project_id) {
        return Err(AdminError(StatusCode::FORBIDDEN, "put the key in one of your projects".into()));
    }
    Ok(())
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct KeyQuery {
    /// Just the keys whose name or shown prefix contains this text, ignoring case, or the key
    /// this whole secret belongs to by its prefix.
    q: Option<String>,
    /// Just the keys holding any of these tags, as `azure,cheap`; `-` stands for none.
    tags: Option<String>,
    /// Just the keys in any of these projects (ids, comma-separated); `-` stands for none.
    projects: Option<String>,
    /// Just the keys whose prompts and responses are logged now.
    logging: Option<bool>,
}

async fn list_keys(
    State(state): State<AppState>,
    Extension(access): Extension<Access>,
    Query(q): Query<KeyQuery>,
) -> R<Json<Vec<KeyView>>> {
    let search = search_text(q.q.as_deref());
    let (tags, projects) = (tag_filter(q.tags.as_deref())?, project_filter(q.projects.as_deref())?);
    let settings = state.settings.get();
    let (logging, gateway_allows) = (q.logging.unwrap_or(false), settings.prompt_logging == PromptLogging::Allow);
    let keys = db::search_keys(&state.db, search.as_deref(), &tags, &projects, access.projects(), logging, gateway_allows).await?;
    let keys: Vec<VirtualKey> = keys.into_iter().filter(|k| sees_key(&access, k)).collect();
    let budgets = futures::future::join_all(keys.iter().map(|k| crate::budget::status(&state, k))).await;
    let snap = state.registry.snapshot();
    let view = |(key, budget)| KeyView { prompt_logged: crate::api::prompt_logged(&snap, &settings, &key), key, budget };
    Ok(Json(keys.into_iter().zip(budgets).map(view).collect()))
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct KeyOptionsQuery {
    /// Just the keys that may call models now: enabled, unexpired, in an active project, with an
    /// inference scope.
    usable: Option<bool>,
    /// Just the keys that may call this model (its tags, allowed models and projects all apply).
    model: Option<String>,
}

/// `GET /keys/options`: the keys a picker offers, by name, without their settings or budgets.
async fn key_options(
    State(state): State<AppState>,
    Extension(access): Extension<Access>,
    Query(q): Query<KeyOptionsQuery>,
) -> R<Json<Vec<db::NamedItem>>> {
    let usable = q.usable.unwrap_or(false);
    let mut keys = db::key_names(&state.db, access.projects(), usable).await?;
    // Whether a project is active and what a key may call, the registry knows.
    if usable || q.model.is_some() {
        let snap = state.registry.snapshot();
        let by_id: HashMap<Uuid, &std::sync::Arc<VirtualKey>> = snap.keys.values().map(|k| (k.id, k)).collect();
        keys.retain(|n| {
            by_id.get(&n.id).is_some_and(|k| {
                (!usable || crate::auth::can_call_models(k)) && q.model.as_deref().is_none_or(|m| crate::api::key_can_use(&snap, k, m))
            })
        });
    }
    Ok(Json(keys))
}

/// `GET /projects/options`: the projects a picker offers, by name.
async fn project_options(State(state): State<AppState>, Extension(access): Extension<Access>) -> R<Json<Vec<db::NamedItem>>> {
    Ok(Json(db::project_names(&state.db, access.projects()).await?))
}

async fn create_key(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(access): Extension<Access>,
    Json(i): Json<KeyInput>,
) -> R<(StatusCode, Json<Value>)> {
    validate_key(&i)?;
    check_user_key(&access, &i)?;
    let project_active = check_project(&state, i.project_id).await?;
    let allowed_tags = key_tags(&state, i.allowed_tags, &[]).await?;
    let guardrail_ids = pick_guardrails(&state, Some(&access), i.guardrail_ids, &[]).await?;
    let (secret, hash, prefix) = crate::auth::generate_key();
    let now = Utc::now();
    let mut models = i.allowed_models;
    models.sort();
    models.dedup();
    let k = VirtualKey {
        id: Uuid::now_v7(),
        name: i.name.trim().to_string(),
        key_hash: hash,
        key_prefix: prefix,
        scope: i.scope,
        project_id: i.project_id,
        project_active,
        allowed_models: models,
        allowed_tags,
        rpm_limit: i.rpm_limit,
        tpm_limit: i.tpm_limit,
        max_budget_usd: i.max_budget_usd,
        budget_period: i.budget_period,
        spend_usd: 0.0,
        spend_reset_at: None,
        enabled: i.enabled,
        expires_at: i.expires_at,
        metadata: i.metadata.unwrap_or_else(|| json!({})),
        guardrail_ids,
        web_search: i.web_search,
        prompt_logging: i.prompt_logging,
        created_at: now,
        updated_at: now,
    };
    db::insert_key(&state.db, &k).await?;
    broadcast_reload(&state, "key created").await;
    tracing::info!(key = %k.id, name = %k.name, scope = k.scope.as_str(), "virtual key created");
    audit::Event::new("key", "create").on(k.id, &k.name).details(audit::snapshot(&k)).record(&state, &actor).await;
    // The plaintext key is returned exactly once and never stored.
    Ok((StatusCode::CREATED, Json(json!({ "key": secret, "record": k }))))
}

async fn update_key(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(access): Extension<Access>,
    Path(id): Path<Uuid>,
    Json(i): Json<KeyInput>,
) -> R<Json<VirtualKey>> {
    validate_key(&i)?;
    let mut k = db::get_key(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    check_sees_key(&access, &k)?;
    check_user_key(&access, &i)?;
    let project_active = check_project(&state, i.project_id).await?;
    let before = k.clone();
    k.allowed_tags = key_tags(&state, i.allowed_tags, &k.allowed_tags).await?;
    k.guardrail_ids = pick_guardrails(&state, Some(&access), i.guardrail_ids, &k.guardrail_ids).await?;
    let mut models = i.allowed_models;
    models.sort();
    models.dedup();
    k.name = i.name.trim().to_string();
    if k.scope != i.scope {
        tracing::info!(key = %k.id, name = %k.name, from = k.scope.as_str(), to = i.scope.as_str(), "virtual key scope changed");
    }
    k.scope = i.scope;
    k.project_id = i.project_id;
    k.project_active = project_active;
    k.allowed_models = models;
    k.rpm_limit = i.rpm_limit;
    k.tpm_limit = i.tpm_limit;
    // A changed period starts counting from its own start; the request log has the history.
    k.max_budget_usd = i.max_budget_usd;
    k.budget_period = i.budget_period;
    k.enabled = i.enabled;
    k.expires_at = i.expires_at;
    k.web_search = i.web_search;
    k.prompt_logging = i.prompt_logging;
    if let Some(m) = i.metadata {
        k.metadata = m;
    }
    db::update_key(&state.db, &k).await?;
    broadcast_reload(&state, "key updated").await;
    let changes = audit::changes(&before, &k);
    if !changes.is_empty() {
        audit::Event::new("key", "update").on(id, &k.name).details(changes.into()).record(&state, &actor).await;
    }
    Ok(Json(k))
}

async fn delete_key(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(access): Extension<Access>,
    Path(id): Path<Uuid>,
) -> R<StatusCode> {
    let k = db::get_key(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    check_sees_key(&access, &k)?;
    if !db::delete_key(&state.db, id).await? {
        return Err(AdminError::not_found());
    }
    broadcast_reload(&state, "key deleted").await;
    tracing::info!(key = %id, "virtual key deleted");
    audit::Event::new("key", "delete").on(id, k.name).details(json!({ "key_prefix": k.key_prefix })).record(&state, &actor).await;
    Ok(StatusCode::NO_CONTENT)
}

/// New secret for an existing key. The old secret stops working as soon as each replica
/// reloads; the key keeps its id, limits and spend.
async fn regenerate_key(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(access): Extension<Access>,
    Path(id): Path<Uuid>,
) -> R<Json<Value>> {
    let k = db::get_key(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    check_sees_key(&access, &k)?;
    let (secret, hash, prefix) = crate::auth::generate_key();
    if !db::rotate_key(&state.db, id, &hash, &prefix).await? {
        return Err(AdminError::not_found());
    }
    broadcast_reload(&state, "key regenerated").await;
    tracing::info!(key = %id, "virtual key regenerated");
    let details = json!({ "key_prefix": { "from": k.key_prefix, "to": prefix } });
    audit::Event::new("key", "regenerate").on(id, k.name).details(details).record(&state, &actor).await;
    // As on create, the plaintext key is returned exactly once and never stored.
    Ok(Json(json!({ "key": secret, "key_prefix": prefix })))
}

async fn reset_spend(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(access): Extension<Access>,
    Path(id): Path<Uuid>,
) -> R<StatusCode> {
    let k = db::get_key(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    check_sees_key(&access, &k)?;
    db::reset_key_spend(&state.db, id).await?;
    let details = json!({ "spend_usd": k.spend_usd });
    audit::Event::new("key", "reset_spend").on(id, k.name).details(details).record(&state, &actor).await;
    restart_spend(&state, Spender::Key(id), "key spend reset").await;
    Ok(StatusCode::NO_CONTENT)
}

/// After a spend reset in Postgres: replicas must know the new reset time before the counters
/// go, or a request in between could rebuild a counter from the old history. The counters are
/// dropped again shortly after, for peers that apply the reload a moment later.
async fn restart_spend(state: &AppState, who: Spender, reason: &str) {
    broadcast_reload(state, reason).await;
    state.cache.reset_period_spend(who).await;
    let cache = state.cache.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        cache.reset_period_spend(who).await;
    });
}

// ---------------------------------------------------------------- projects

#[derive(Deserialize, utoipa::ToSchema)]
pub(crate) struct ProjectInput {
    name: String,
    #[serde(default)]
    description: Option<String>,
    /// While false, the project's keys are refused like disabled keys.
    #[serde(default = "yes")]
    enabled: bool,
    /// Budget in USD per `budget_period`, on top of each key's own; both None = unlimited.
    #[serde(default)]
    max_budget_usd: Option<f64>,
    /// "day", "week" or "month" (UTC calendar periods).
    #[serde(default)]
    budget_period: Option<String>,
    /// Guardrails every request of the project's keys goes through. Omitted, they stay as they are.
    #[serde(default)]
    guardrail_ids: Option<Vec<Uuid>>,
    /// While false, no key in the project may have the model search the web, whatever its own setting.
    #[serde(default = "yes")]
    web_search: bool,
    /// Whether its keys' prompts and responses are logged, for debugging: `deny` (the default),
    /// `allow` for the keys that opt in, or `force` for every key.
    #[serde(default)]
    prompt_logging: PromptLogging,
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct ProjectView {
    #[serde(flatten)]
    project: ProjectRow,
    /// How many keys the project groups.
    keys: i64,
    /// Current budget period and its spend; None for unlimited projects.
    budget: Option<crate::budget::BudgetStatus>,
}

impl ProjectView {
    async fn new(state: &AppState, project: ProjectRow, keys: i64) -> Self {
        let budget = crate::budget::project_status(state, &project).await;
        Self { project, keys, budget }
    }
}

/// The input's name and description, trimmed; an empty description is none.
fn project_fields(i: ProjectInput) -> R<(String, Option<String>)> {
    let name = i.name.trim().to_string();
    if name.is_empty() || name.len() > 100 {
        return Err(AdminError::bad_request("name must be 1-100 characters"));
    }
    let description = i.description.map(|d| d.trim().to_string()).filter(|d| !d.is_empty());
    if description.as_ref().is_some_and(|d| d.len() > 1000) {
        return Err(AdminError::bad_request("description can be at most 1000 characters"));
    }
    Ok((name, description))
}

fn duplicate_project(e: sqlx::Error) -> AdminError {
    if db::is_unique_violation(&e) { AdminError::conflict("a project with this name already exists") } else { e.into() }
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct ProjectQuery {
    /// Just the projects whose name or description contains this text, ignoring case.
    q: Option<String>,
}

async fn list_projects(
    State(state): State<AppState>,
    Extension(access): Extension<Access>,
    Query(q): Query<ProjectQuery>,
) -> R<Json<Vec<ProjectView>>> {
    let search = search_text(q.q.as_deref());
    let projects = db::list_projects(&state.db, search.as_deref(), access.projects()).await?;
    Ok(Json(futures::future::join_all(projects.into_iter().map(|(project, keys)| ProjectView::new(&state, project, keys))).await))
}

async fn create_project(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(i): Json<ProjectInput>,
) -> R<(StatusCode, Json<ProjectView>)> {
    check_budget(i.max_budget_usd, i.budget_period.as_deref())?;
    let guardrail_ids = pick_guardrails(&state, None, i.guardrail_ids.clone(), &[]).await?;
    let (enabled, max_budget_usd, budget_period, web_search, prompt_logging) =
        (i.enabled, i.max_budget_usd, i.budget_period.clone(), i.web_search, i.prompt_logging);
    let (name, description) = project_fields(i)?;
    let now = Utc::now();
    let p = ProjectRow {
        id: Uuid::now_v7(),
        name,
        description,
        enabled,
        max_budget_usd,
        budget_period,
        spend_reset_at: None,
        guardrail_ids,
        web_search,
        prompt_logging,
        created_at: now,
        updated_at: now,
    };
    db::upsert_project(&state.db, &p).await.map_err(duplicate_project)?;
    tracing::info!(project = %p.id, name = %p.name, "project created");
    audit::Event::new("project", "create").on(p.id, &p.name).details(audit::snapshot(&p)).record(&state, &actor).await;
    Ok((StatusCode::CREATED, Json(ProjectView::new(&state, p, 0).await)))
}

async fn update_project(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<Uuid>,
    Json(i): Json<ProjectInput>,
) -> R<Json<ProjectView>> {
    let before = db::get_project(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    check_budget(i.max_budget_usd, i.budget_period.as_deref())?;
    let guardrail_ids = pick_guardrails(&state, None, i.guardrail_ids.clone(), &before.guardrail_ids).await?;
    let (enabled, max_budget_usd, budget_period, web_search, prompt_logging) =
        (i.enabled, i.max_budget_usd, i.budget_period.clone(), i.web_search, i.prompt_logging);
    let (name, description) = project_fields(i)?;
    let p = ProjectRow {
        name,
        description,
        enabled,
        max_budget_usd,
        budget_period,
        guardrail_ids,
        web_search,
        prompt_logging,
        updated_at: Utc::now(),
        ..before.clone()
    };
    db::upsert_project(&state.db, &p).await.map_err(duplicate_project)?;
    // Keys carry whether their project is active, and requests check the project's budget in
    // the registry: every replica reloads to apply either.
    if before.enabled != p.enabled {
        broadcast_reload(&state, if p.enabled { "project activated" } else { "project deactivated" }).await;
        tracing::info!(project = %id, enabled = p.enabled, "project state changed");
    } else if (before.max_budget_usd, &before.budget_period) != (p.max_budget_usd, &p.budget_period) {
        broadcast_reload(&state, "project budget changed").await;
    } else if before.guardrail_ids != p.guardrail_ids {
        broadcast_reload(&state, "project guardrails changed").await;
    } else if before.web_search != p.web_search {
        broadcast_reload(&state, if p.web_search { "project web search allowed" } else { "project web search forbidden" }).await;
    } else if before.prompt_logging != p.prompt_logging {
        broadcast_reload(&state, "project prompt logging changed").await;
        tracing::info!(project = %id, prompt_logging = p.prompt_logging.as_str(), "project prompt logging changed");
    }
    let changes = audit::changes(&before, &p);
    if !changes.is_empty() {
        audit::Event::new("project", "update").on(id, &p.name).details(changes.into()).record(&state, &actor).await;
    }
    let keys = db::list_projects(&state.db, None, Some(&[id])).await?.into_iter().find(|(x, _)| x.id == id).map_or(0, |(_, n)| n);
    Ok(Json(ProjectView::new(&state, p, keys).await))
}

/// The current budget period of the project counts only spend from now on.
async fn reset_project_spend(State(state): State<AppState>, Extension(actor): Extension<Actor>, Path(id): Path<Uuid>) -> R<StatusCode> {
    let p = db::get_project(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    let spent = crate::budget::project_status(&state, &p).await.map(|b| b.spent_usd);
    if !db::reset_project_spend(&state.db, id).await? {
        return Err(AdminError::not_found());
    }
    audit::Event::new("project", "reset_spend").on(id, p.name).details(json!({ "spent_usd": spent })).record(&state, &actor).await;
    restart_spend(&state, Spender::Project(id), "project spend reset").await;
    Ok(StatusCode::NO_CONTENT)
}

/// Its keys are deleted with it. Refused while deployments are limited to it: without it they
/// would be open to more keys, or to none.
async fn delete_project(State(state): State<AppState>, Extension(actor): Extension<Actor>, Path(id): Path<Uuid>) -> R<StatusCode> {
    let p = db::get_project(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    let models: BTreeSet<String> =
        db::list_deployments(&state.db).await?.into_iter().filter(|d| d.project_ids.contains(&id)).map(|d| d.model_name).collect();
    if !models.is_empty() {
        let models = models.into_iter().collect::<Vec<_>>().join(", ");
        return Err(AdminError::conflict(format!("deployments of {models} are limited to this project; remove it from them first")));
    }
    let servers: Vec<String> =
        db::list_mcp_servers(&state.db).await?.into_iter().filter(|s| s.project_ids.contains(&id)).map(|s| s.name).collect();
    if !servers.is_empty() {
        return Err(AdminError::conflict(format!(
            "MCP servers {} are limited to this project; remove it from them first",
            servers.join(", ")
        )));
    }
    let agents: Vec<String> =
        db::list_a2a_agents(&state.db).await?.into_iter().filter(|a| a.project_ids.contains(&id)).map(|a| a.name).collect();
    if !agents.is_empty() {
        return Err(AdminError::conflict(format!(
            "A2A agents {} are limited to this project; remove it from them first",
            agents.join(", ")
        )));
    }
    let keys = db::delete_project(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    // Replicas reload their keys so the deleted ones stop working there too.
    if !keys.is_empty() {
        broadcast_reload(&state, "project deleted").await;
    }
    tracing::info!(project = %id, keys = keys.len(), "project deleted");
    audit::Event::new("project", "delete").on(id, &p.name).details(json!({ "keys": keys.len() })).record(&state, &actor).await;
    for (key, name, key_prefix) in keys {
        audit::Event::new("key", "delete")
            .on(key, name)
            .details(json!({ "key_prefix": key_prefix, "project": p.name }))
            .record(&state, &actor)
            .await;
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- metrics

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct RangeQuery {
    range: Option<String>,
    /// Metrics only: just this virtual key's traffic.
    key_id: Option<Uuid>,
    /// Metrics only: just the traffic of this project's keys.
    project_id: Option<Uuid>,
    /// Metrics only: just the traffic of keys that were deleted since.
    deleted_keys: Option<bool>,
    /// Metrics only: just the requests with all of these labels, as `tenant=…,feature=core`.
    labels: Option<String>,
    /// Metrics only: the series and totals of just these models, comma-separated; omitted means
    /// every model except auto routers, whose entries already count their calls.
    models: Option<String>,
    /// Metrics with `range=custom`: first and last day, inclusive (UTC).
    from: Option<chrono::NaiveDate>,
    to: Option<chrono::NaiveDate>,
}

/// A `labels` filter (`key=value` pairs, as clients send them) as the JSON object the request
/// log is matched against; `None` when empty.
fn label_filter(labels: Option<&str>) -> R<Option<Value>> {
    let labels = crate::labels::Labels::parse(labels.unwrap_or_default()).map_err(AdminError::bad_request)?;
    Ok(labels.to_json())
}

/// The time a metrics view covers, split into buckets of `step` seconds starting at `stamps`.
#[derive(Debug, PartialEq)]
struct Window {
    label: &'static str,
    step: i64,
    stamps: Vec<i64>,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
}

const DAY: i64 = 86_400;
/// Custom ranges are shown with daily buckets; this many days at most.
const MAX_CUSTOM_DAYS: i64 = 400;

fn window(q: &RangeQuery, now: DateTime<Utc>) -> R<Window> {
    let ts = |secs: i64| DateTime::from_timestamp(secs, 0).unwrap_or(now);
    let midnight = |d: chrono::NaiveDate| d.and_hms_opt(0, 0, 0).map(|t| t.and_utc().timestamp()).unwrap_or(0);
    // Hourly buckets up to two days, daily ones beyond.
    let step_for = |span: i64| if span <= 2 * DAY { 3600 } else { DAY };
    let (label, step, since, until) = match q.range.as_deref() {
        None | Some("1h") => ("1h", 60, now.timestamp() - 59 * 60, now.timestamp()),
        Some("24h") => ("24h", 3600, now.timestamp() - 23 * 3600, now.timestamp()),
        Some("7d") => ("7d", 3600, now.timestamp() - 167 * 3600, now.timestamp()),
        Some("month") => {
            let first = midnight(now.date_naive().with_day(1).unwrap_or(now.date_naive()));
            ("month", step_for(now.timestamp() - first), first, now.timestamp())
        }
        Some("custom") => {
            let (Some(from), Some(to)) = (q.from, q.to) else {
                return Err(AdminError::bad_request("a custom range needs from and to dates (YYYY-MM-DD)"));
            };
            let days = (to - from).num_days() + 1;
            if days < 1 {
                return Err(AdminError::bad_request("the range ends before it starts"));
            }
            if days > MAX_CUSTOM_DAYS {
                return Err(AdminError::bad_request(format!("a custom range spans at most {MAX_CUSTOM_DAYS} days")));
            }
            let (start, end) = (midnight(from), midnight(to) + DAY);
            ("custom", step_for(end - start), start, end - 1)
        }
        Some(_) => return Err(AdminError::bad_request("range must be 1h, 24h, 7d, month or custom")),
    };
    // Buckets are aligned to their size (UTC midnight for days); the last one holds `until`.
    let first = since - since.rem_euclid(step);
    let stamps: Vec<i64> = (0..).map(|i| first + i * step).take_while(|t| *t <= until).collect();
    Ok(Window { label, step, stamps, since: ts(first), until: ts(until + 1) })
}

#[derive(Serialize, Default)]
struct Totals {
    requests: i64,
    errors: i64,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    cost_usd: f64,
    avg_latency_ms: Option<f64>,
    p50_latency_ms: Option<i32>,
    p95_latency_ms: Option<i32>,
    avg_ttft_ms: Option<f64>,
}

fn totals(t: Option<&db::LogTotals>) -> Totals {
    let Some(t) = t else { return Totals::default() };
    Totals {
        requests: t.requests,
        errors: t.errors,
        input_tokens: t.input_tokens,
        output_tokens: t.output_tokens,
        cache_read_tokens: t.cache_read_tokens,
        cache_write_tokens: t.cache_write_tokens,
        cost_usd: t.cost_usd,
        avg_latency_ms: (t.requests > 0).then(|| t.latency_ms_sum as f64 / t.requests as f64),
        p50_latency_ms: t.p50_latency_ms,
        p95_latency_ms: t.p95_latency_ms,
        avg_ttft_ms: (t.ttft_count > 0).then(|| t.ttft_ms_sum as f64 / t.ttft_count as f64),
    }
}

/// One point per bucket start in `stamps`; buckets without requests are zero.
fn series_json(stamps: &[i64], buckets: &HashMap<i64, &db::LogTotals>) -> Vec<Value> {
    stamps
        .iter()
        .map(|ts| match buckets.get(ts) {
            Some(b) => json!({
                "ts": ts,
                "requests": b.requests,
                "errors": b.errors,
                "input_tokens": b.input_tokens + b.cache_read_tokens + b.cache_write_tokens,
                "output_tokens": b.output_tokens,
                "cost_usd": b.cost_usd,
                "avg_latency_ms": (b.requests > 0).then(|| b.latency_ms_sum as f64 / b.requests as f64),
            }),
            None => json!({
                "ts": ts, "requests": 0, "errors": 0, "input_tokens": 0, "output_tokens": 0, "cost_usd": 0.0,
                "avg_latency_ms": null,
            }),
        })
        .collect()
}

/// Metrics come from the request log in Postgres, so they cover its whole retention and every
/// replica's traffic.
async fn metrics(State(state): State<AppState>, Extension(access): Extension<Access>, Query(q): Query<RangeQuery>) -> R<Json<Value>> {
    let Window { label, step, stamps, since, until } = window(&q, Utc::now())?;
    let projects = access.projects();
    if q.project_id.is_some_and(|p| !access.sees(Some(p))) {
        return Err(AdminError(StatusCode::FORBIDDEN, "not your project".into()));
    }
    // The traffic counted: the picked project's, within what the caller may see.
    let picked = q.project_id.map(|p| [p]);
    let counted = picked.as_ref().map(|p| &p[..]).or(projects);
    let labels = label_filter(q.labels.as_deref())?;
    let labels = labels.as_ref();
    let deleted = q.deleted_keys.unwrap_or(false);
    let picked_models: Option<Vec<String>> = q
        .models
        .as_deref()
        .map(|m| m.split(',').map(str::trim).filter(|m| !m.is_empty()).map(String::from).collect::<Vec<_>>())
        .filter(|m| !m.is_empty());

    let snap = state.registry.snapshot();
    // Router entries are logged under the name requested, so under an alias of a router too.
    let is_router = |m: &str| snap.routers.contains_key(snap.resolve(m));
    // Without a pick, the routers are left out: their calls are counted under their own models.
    let exclude: Vec<String> = match picked_models {
        Some(_) => Vec::new(),
        None => snap.routers.keys().chain(snap.aliases.keys()).filter(|m| is_router(m)).cloned().collect(),
    };
    let (series, model_totals, selected) = tokio::try_join!(
        db::selection_series(&state.db, since, until, step, q.key_id, deleted, picked_models.as_deref(), &exclude, counted, labels),
        db::model_totals(&state.db, since, until, q.key_id, deleted, counted, labels),
        db::selection_totals(&state.db, since, until, q.key_id, deleted, picked_models.as_deref(), &exclude, counted, labels),
    )?;

    let buckets: HashMap<i64, &db::LogTotals> = series.iter().map(|b| (b.bucket, b)).collect();
    let model_totals: HashMap<&str, &db::LogTotals> = model_totals.iter().map(|t| (t.subject.as_str(), t)).collect();

    // Configured models and routers, plus models that have since been removed but still have traffic.
    // A user sees only the models that served their projects.
    let mut models: BTreeSet<&str> = match projects {
        None => snap.deployments.keys().chain(snap.routers.keys()).chain(snap.aliases.keys()).map(String::as_str).collect(),
        Some(_) => BTreeSet::new(),
    };
    models.extend(model_totals.keys().copied());
    let model_out: Vec<Value> = models
        .into_iter()
        .map(|m| {
            let router = is_router(m);
            json!({
                "model": m, "active": router || snap.deployments.contains_key(snap.resolve(m)), "router": router,
                "totals": totals(model_totals.get(m).copied()),
            })
        })
        .collect();
    Ok(Json(json!({
        "range": label, "step_secs": step, "from": since, "to": until, "models": model_out,
        // The picked models', or everything except router entries.
        "totals": totals(Some(&selected)), "series": series_json(&stamps, &buckets),
    })))
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct LogQuery {
    limit: Option<i64>,
    /// Matches model names containing this text, ignoring case.
    model: Option<String>,
    key_id: Option<Uuid>,
    /// Just the requests of this project's keys.
    project_id: Option<Uuid>,
    /// Just the requests of keys that were deleted since.
    deleted_keys: Option<bool>,
    errors: Option<bool>,
    /// Just the requests with all of these labels, as `tenant=…,feature=core`.
    labels: Option<String>,
    /// `1h`, `24h`, `7d`, `30d`, `month` or `custom`; omitted means no time limit.
    range: Option<String>,
    /// With `range=custom`: first and last day, inclusive (UTC).
    from: Option<chrono::NaiveDate>,
    to: Option<chrono::NaiveDate>,
    /// Just the requests whose prompts and responses are stored (prompt logging).
    bodies: Option<bool>,
    /// Just the requests whose stored prompt (after guardrails) or response contains these words:
    /// `"a phrase"`, `or`, `-word` and `prefix:*` work as in Postgres' `websearch_to_tsquery`.
    q: Option<String>,
    /// Just the requests whose stored response called this tool (`web_search` for web searches),
    /// and the MCP calls of a tool of this name.
    tool: Option<String>,
    /// Just the requests the guardrails did any of these to, as `blocked,masked`: `passed`,
    /// `flagged`, `masked` or `blocked`.
    guardrail: Option<String>,
}

async fn logs(State(state): State<AppState>, Extension(access): Extension<Access>, Query(q): Query<LogQuery>) -> R<Json<Vec<db::LogRow>>> {
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let model = q.model.as_deref().map(str::trim).filter(|m| !m.is_empty());
    let (since, until) = log_window(q.range.as_deref(), q.from, q.to, Utc::now())?;
    let errors = q.errors.unwrap_or(false);
    let labels = label_filter(q.labels.as_deref())?;
    if q.project_id.is_some_and(|p| !access.sees(Some(p))) {
        return Err(AdminError(StatusCode::FORBIDDEN, "not your project".into()));
    }
    let text = q.q.as_deref().map(str::trim).filter(|t| !t.is_empty());
    if text.is_some_and(|t| t.chars().count() > 200) {
        return Err(AdminError::bad_request("q can be at most 200 characters"));
    }
    let tool = q.tool.as_deref().map(str::trim).filter(|t| !t.is_empty());
    let guardrail: Option<Vec<String>> = match q.guardrail.as_deref().map(str::trim).filter(|g| !g.is_empty()) {
        Some(list) => {
            let actions: Vec<String> = list.split(',').map(|a| a.trim().to_string()).collect();
            if let Some(bad) = actions.iter().find(|a| !matches!(a.as_str(), "passed" | "flagged" | "masked" | "blocked")) {
                return Err(AdminError::bad_request(format!("guardrail must list passed, flagged, masked or blocked, not '{bad}'")));
            }
            Some(actions)
        }
        None => None,
    };
    // The picked project's requests, within what the caller may see.
    let picked = q.project_id.map(|p| [p]);
    let filter = db::LogFilter {
        limit,
        model,
        key_id: q.key_id,
        deleted_keys: q.deleted_keys.unwrap_or(false),
        errors_only: errors,
        since,
        until,
        projects: picked.as_ref().map(|p| &p[..]).or(access.projects()),
        labels: labels.as_ref(),
        bodies: q.bodies.unwrap_or(false),
        text,
        tool,
        guardrail: guardrail.as_deref(),
        body_days: state.settings.get().prompt_log_retention_days,
    };
    Ok(Json(db::recent_logs(&state.db, &filter).await?))
}

/// What the latest logged responses' tools are sampled from.
const TOOL_SAMPLE: i64 = 1000;

/// `GET /logs/tools`: the tools the latest logged responses called, most used first, for the
/// request log's tool filter.
async fn log_tools(State(state): State<AppState>, Extension(access): Extension<Access>) -> R<Json<Vec<db::ToolUse>>> {
    Ok(Json(db::recent_tools(&state.db, TOOL_SAMPLE, access.projects()).await?))
}

/// A logged request's bodies (see `capture`).
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct BodiesView {
    /// The request as the client sent it.
    #[schema(value_type = Option<Object>)]
    request: Option<Value>,
    /// The request as the model got it, when guardrails changed it.
    #[schema(value_type = Option<Object>)]
    screened_request: Option<Value>,
    /// What each guardrail was given, found (matched text included), did and answered.
    #[schema(value_type = Option<Vec<Object>>)]
    guardrails: Option<Value>,
    /// The response as the client got it; a stream put back together into one document.
    #[schema(value_type = Option<Object>)]
    response: Option<Value>,
    /// Sizes as sent and received, before placeholders and cuts.
    request_bytes: i32,
    response_bytes: i32,
    /// `stored` or `truncated`.
    status: Option<String>,
    /// The logged turn before this one in its conversation, and the one after, where the caller
    /// may see them.
    previous: Option<Uuid>,
    next: Option<Uuid>,
}

/// `GET /logs/{id}/bodies`: a logged request's prompt and response. An entry the caller may not
/// see looks like one without bodies.
async fn log_bodies(State(state): State<AppState>, Extension(access): Extension<Access>, Path(id): Path<Uuid>) -> R<Json<BodiesView>> {
    let row = db::log_bodies(&state.db, id).await?.filter(|r| access.sees(r.project_id)).ok_or_else(AdminError::not_found)?;
    let (previous, next) = db::turn_links(&state.db, &row, access.projects()).await?;
    Ok(Json(BodiesView {
        request: row.request,
        screened_request: row.screened_request,
        guardrails: row.guardrails,
        response: row.response,
        request_bytes: row.request_bytes,
        response_bytes: row.response_bytes,
        status: row.status,
        previous,
        next,
    }))
}

/// How many prompts and responses are stored for a key or project.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct StoredBodies {
    stored: i64,
}

/// How many were deleted.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct DeletedBodies {
    deleted: u64,
}

/// `GET /keys/{id}/bodies`: how many of the key's prompts and responses are stored.
async fn key_bodies(State(state): State<AppState>, Extension(access): Extension<Access>, Path(id): Path<Uuid>) -> R<Json<StoredBodies>> {
    let k = db::get_key(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    check_sees_key(&access, &k)?;
    Ok(Json(StoredBodies { stored: db::count_bodies(&state.db, db::BodiesOf::Key(id)).await? }))
}

/// `DELETE /keys/{id}/bodies`: deletes the key's stored prompts and responses now, rather than
/// at the end of their retention.
async fn delete_key_bodies(
    State(state): State<AppState>,
    Extension(access): Extension<Access>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<Uuid>,
) -> R<Json<DeletedBodies>> {
    let k = db::get_key(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    check_sees_key(&access, &k)?;
    let deleted = remove_bodies(&state, db::BodiesOf::Key(id)).await?;
    audit::Event::new("key", "delete_bodies").on(id, &k.name).details(json!({ "deleted": deleted })).record(&state, &actor).await;
    Ok(Json(DeletedBodies { deleted }))
}

/// `GET /projects/{id}/bodies`: how many prompts and responses of requests made in the project
/// are stored.
async fn project_bodies(State(state): State<AppState>, Path(id): Path<Uuid>) -> R<Json<StoredBodies>> {
    db::get_project(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    Ok(Json(StoredBodies { stored: db::count_bodies(&state.db, db::BodiesOf::Project(id)).await? }))
}

/// `DELETE /projects/{id}/bodies`: deletes the stored prompts and responses of every request made
/// in the project, also by keys that have left it since.
async fn delete_project_bodies(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<Uuid>,
) -> R<Json<DeletedBodies>> {
    let p = db::get_project(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    let deleted = remove_bodies(&state, db::BodiesOf::Project(id)).await?;
    audit::Event::new("project", "delete_bodies").on(id, &p.name).details(json!({ "deleted": deleted })).record(&state, &actor).await;
    Ok(Json(DeletedBodies { deleted }))
}

/// Deletes stored bodies in their own task, so a client that stops waiting does not cut it
/// short, under a lock per key or project, so two clicks do not run it twice.
async fn remove_bodies(state: &AppState, of: db::BodiesOf) -> R<u64> {
    let id = match of {
        db::BodiesOf::Key(id) | db::BodiesOf::Project(id) => id,
    };
    let lock = 0x626f_6469_6573_0000 ^ (id.as_u128() as i64 & 0xffff_ffff); // "bodies"
    let state = state.clone();
    let task = tokio::spawn(async move { db::with_lock(&state.db, lock, db::delete_bodies(&state.db, of)).await });
    let done = task.await.map_err(|e| AdminError(StatusCode::INTERNAL_SERVER_ERROR, format!("deleting the bodies failed: {e}")))?;
    match done? {
        Some(deleted) => Ok(deleted?),
        None => Err(AdminError::conflict("these bodies are already being deleted")),
    }
}

/// Label suggestions are read from this many of the latest labelled requests...
const LABEL_SAMPLE: i64 = 1000;
/// ...and offer at most this many values of each key, the most used.
const LABEL_VALUES: usize = 50;

/// A label key the latest labelled requests carry, with its values.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct LabelKey {
    key: String,
    /// Most used first.
    values: Vec<LabelValue>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct LabelValue {
    value: String,
    /// How many of the sampled requests carry it.
    requests: i64,
}

async fn log_labels(State(state): State<AppState>, Extension(access): Extension<Access>) -> R<Json<Vec<LabelKey>>> {
    let mut keys: Vec<LabelKey> = Vec::new();
    for u in db::recent_labels(&state.db, LABEL_SAMPLE, access.projects()).await? {
        if keys.last().is_none_or(|k| k.key != u.key) {
            keys.push(LabelKey { key: u.key, values: Vec::new() });
        }
        let values = &mut keys.last_mut().expect("pushed above").values;
        if values.len() < LABEL_VALUES {
            values.push(LabelValue { value: u.value, requests: u.requests });
        }
    }
    Ok(Json(keys))
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct AuditQuery {
    limit: Option<i64>,
    /// `credential`, `deployment`, `router`, `key`, `project`, `configuration`, `price_catalog`,
    /// `oidc_settings`, `settings`, `admin` or `session`.
    target_type: Option<String>,
    /// Matches the target's or the actor's name containing this text, ignoring case.
    q: Option<String>,
    /// `1h`, `24h`, `7d`, `30d`, `month` or `custom`; omitted means no time limit.
    range: Option<String>,
    /// With `range=custom`: first and last day, inclusive (UTC).
    from: Option<chrono::NaiveDate>,
    to: Option<chrono::NaiveDate>,
}

async fn audit_log(State(state): State<AppState>, Query(q): Query<AuditQuery>) -> R<Json<Vec<db::AuditEntry>>> {
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let target_type = q.target_type.as_deref().map(str::trim).filter(|t| !t.is_empty());
    let search = q.q.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let (since, until) = log_window(q.range.as_deref(), q.from, q.to, Utc::now())?;
    Ok(Json(db::list_audit(&state.db, limit, target_type, search, since, until).await?))
}

/// Where a log range starts and where it ends (exclusive); None is unbounded.
type LogWindow = (Option<DateTime<Utc>>, Option<DateTime<Utc>>);

/// The start and end of a log range: `1h`, `24h`, `7d`, `30d`, `month` (since the first of the
/// month, UTC) or `custom` (whole UTC days `from` to `to`); none or `all` means no limit.
fn log_window(range: Option<&str>, from: Option<chrono::NaiveDate>, to: Option<chrono::NaiveDate>, now: DateTime<Utc>) -> R<LogWindow> {
    let midnight = |d: chrono::NaiveDate| d.and_hms_opt(0, 0, 0).map(|t| t.and_utc());
    Ok(match range {
        None | Some("" | "all") => (None, None),
        Some("1h") => (Some(now - chrono::Duration::hours(1)), None),
        Some("24h") => (Some(now - chrono::Duration::hours(24)), None),
        Some("7d") => (Some(now - chrono::Duration::days(7)), None),
        Some("30d") => (Some(now - chrono::Duration::days(30)), None),
        Some("month") => (now.date_naive().with_day(1).and_then(midnight), None),
        Some("custom") => {
            let (Some(from), Some(to)) = (from, to) else {
                return Err(AdminError::bad_request("a custom range needs from and to dates (YYYY-MM-DD)"));
            };
            if to < from {
                return Err(AdminError::bad_request("the range ends before it starts"));
            }
            (midnight(from), midnight(to).map(|t| t + chrono::Duration::days(1)))
        }
        Some(_) => return Err(AdminError::bad_request("range must be 1h, 24h, 7d, 30d, month or custom")),
    })
}

async fn key_usage(
    State(state): State<AppState>,
    Extension(access): Extension<Access>,
    Query(q): Query<RangeQuery>,
) -> R<Json<Vec<db::KeyUsageRow>>> {
    let since = Utc::now()
        - match q.range.as_deref() {
            Some("1h") => chrono::Duration::hours(1),
            Some("7d") => chrono::Duration::days(7),
            Some("30d") => chrono::Duration::days(30),
            _ => chrono::Duration::hours(24),
        };
    Ok(Json(db::key_usage_since(&state.db, since, access.projects()).await?))
}

async fn cluster(State(state): State<AppState>) -> Json<Value> {
    let (instances, valkey, pg) = tokio::join!(state.cache.instances(), state.cache.ping(), db::ping(&state.db));
    Json(json!({
        "instances": instances, "valkey_ok": valkey, "postgres_ok": pg, "this_instance": state.cfg.instance_id,
        "settings": instance_settings(&state.cfg),
    }))
}

/// The settings of the instance answering, from its environment. Never secrets: only whether the
/// master key is set, and no connection URLs (they carry passwords).
fn instance_settings(cfg: &crate::config::Config) -> Value {
    json!({
        "database_max_connections": cfg.database_max_connections,
        "valkey_cluster": cfg.valkey_cluster,
        "upstream_allowed_networks": cfg.upstream_policy.networks(),
        "master_key": cfg.master_key.is_some(),
        "dev_login": cfg.dev_login,
        "cookie_secure": cfg.cookie_secure,
        "console": cfg.ui_dir.is_some(),
        "listen": cfg.listen_addr.to_string(),
    })
}

// ---------------------------------------------------------------- gateway settings

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct SettingsView {
    settings: Settings,
    /// What each setting is when it was never changed.
    defaults: Settings,
    /// When the settings were last saved; absent while they are all defaults.
    updated_at: Option<DateTime<Utc>>,
}

/// The stored settings. A stored document that no longer parses shows the settings in effect.
async fn settings_view(state: &AppState) -> R<SettingsView> {
    let row = db::get_gateway_settings(&state.db).await?;
    let updated_at = row.as_ref().map(|r| r.updated_at);
    let settings = row.and_then(|r| serde_json::from_value(r.settings).ok()).unwrap_or_else(|| (*state.settings.get()).clone());
    Ok(SettingsView { settings, defaults: Settings::default(), updated_at })
}

async fn get_settings(State(state): State<AppState>) -> R<Json<SettingsView>> {
    Ok(Json(settings_view(&state).await?))
}

/// Save the settings (all of them; missing fields take their defaults) and apply them on every
/// replica.
async fn put_settings(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(settings): Json<Settings>,
) -> R<Json<SettingsView>> {
    settings.check().map_err(AdminError::bad_request)?;
    let before = settings_view(&state).await?.settings;
    db::save_gateway_settings(&state.db, &serde_json::to_value(&settings).expect("settings serialize")).await?;
    broadcast_reload(&state, "settings").await;
    let changes = audit::changes(&before, &settings);
    if !changes.is_empty() {
        tracing::info!(changed = ?changes.keys().collect::<Vec<_>>(), "gateway settings saved");
        audit::Event::new("settings", "update").named("Gateway settings").details(changes.into()).record(&state, &actor).await;
    }
    Ok(Json(settings_view(&state).await?))
}

// ---------------------------------------------------------------- playground

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct PlaygroundQuery {
    key_id: Uuid,
}

/// What the playground offers for a key: the models it may call, the guardrails a request may
/// ask for (those the key or its project enforce marked), and the MCP servers and A2A agents it
/// may use.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct PlaygroundOptions {
    models: Vec<PlaygroundModel>,
    guardrails: Vec<PlaygroundGuardrail>,
    mcp_servers: Vec<PlaygroundUpstream>,
    a2a_agents: Vec<PlaygroundUpstream>,
}

/// An MCP server or A2A agent the key may use, or a tool router.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct PlaygroundUpstream {
    name: String,
    /// The key signs in to it itself, and has not yet.
    needs_sign_in: bool,
    /// A tool router, listed with the MCP servers: requests name it like one.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    router: bool,
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct PlaygroundModel {
    name: String,
    mode: Mode,
    /// An auto router (chat mode).
    router: bool,
    /// Requests may have it search the web: it has a web search, and the key and its project allow it.
    web_search: bool,
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct PlaygroundGuardrail {
    name: String,
    /// Set on the key or its project: every request goes through it.
    enforced: bool,
}

/// `GET /admin/api/playground/options?key_id=…`.
async fn playground_options(
    State(state): State<AppState>,
    Extension(access): Extension<Access>,
    Query(q): Query<PlaygroundQuery>,
) -> R<Json<PlaygroundOptions>> {
    let snap = state.registry.snapshot();
    // As in `playground`: a user sees only the inference keys of their projects.
    let key = snap.keys.values().find(|k| k.id == q.key_id && sees_key(&access, k)).ok_or_else(AdminError::not_found)?;
    let models = crate::api::models::names(&snap, key)
        .into_iter()
        .map(|name| {
            let router = snap.routers.contains_key(snap.resolve(name));
            let mode = if router {
                Mode::Chat
            } else {
                crate::api::models::deployments(&snap, Some(key), name).first().map_or(Mode::Chat, |d| d.mode)
            };
            let web_search = mode == Mode::Chat && crate::api::models::web_search(&snap, key, name);
            PlaygroundModel { name: name.clone(), mode, router, web_search }
        })
        .collect();
    let project = key.project_id.and_then(|p| snap.projects.get(&p));
    let enforced = |id: Uuid| key.guardrail_ids.contains(&id) || project.is_some_and(|p| p.guardrail_ids.contains(&id));
    let guardrails =
        snap.guardrails.values().map(|g| PlaygroundGuardrail { name: g.row.name.clone(), enforced: enforced(g.row.id) }).collect();
    let (signed_in, _) = db::key_connections(&state.db, key.id).await?;
    // Only the key's own sign-in is asked for here; one per tenant or user is asked for by the call
    // that needs it, with a link.
    let upstream = |kind: crate::oauth::Kind, id: Uuid, name: &str, auth: &crate::oauth::UpstreamAuth| PlaygroundUpstream {
        name: name.to_string(),
        needs_sign_in: auth.per_key()
            && auth.sign_in_by().is_empty()
            && !signed_in.iter().any(|r| r.upstream_kind == kind.as_str() && r.upstream_id == id),
        router: false,
    };
    let mut mcp_servers: Vec<PlaygroundUpstream> =
        snap.mcp_servers.values().filter(|s| s.allows(key)).map(|s| upstream(crate::oauth::Kind::Mcp, s.id, &s.name, &s.auth)).collect();
    let mut a2a_agents: Vec<PlaygroundUpstream> =
        snap.a2a_agents.values().filter(|a| a.allows(key)).map(|a| upstream(crate::oauth::Kind::A2a, a.id, &a.name, &a.auth)).collect();
    // Tool routers with a route the key may use.
    mcp_servers.extend(snap.tool_routers.values().filter(|r| !r.usable(&snap, key).is_empty()).map(|r| PlaygroundUpstream {
        name: r.name.clone(),
        needs_sign_in: false,
        router: true,
    }));
    mcp_servers.sort_by(|a, b| a.name.cmp(&b.name));
    a2a_agents.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(PlaygroundOptions { models, guardrails, mcp_servers, a2a_agents }))
}

/// The key the playground plays as: for a user, only an inference key of their projects; others
/// look like keys that do not exist.
#[allow(clippy::result_large_err)]
fn playground_key(state: &AppState, access: &Access, id: Uuid) -> Result<std::sync::Arc<VirtualKey>, Response> {
    let not_found = |e: crate::error::GatewayError| e.into_response_for(crate::error::Dialect::OpenAi);
    if !state.registry.snapshot().keys.values().any(|k| k.id == id && sees_key(access, k)) {
        return Err(not_found(crate::error::GatewayError::not_found("virtual key not found")));
    }
    crate::auth::key_by_id(state, id).map_err(not_found)
}

/// `POST /admin/api/playground/a2a/{agent}?key_id=…`: a JSON-RPC call of the agent as the key,
/// as at `/a2a/{agent}`, logged and charged as there.
async fn playground_a2a(
    State(state): State<AppState>,
    Extension(access): Extension<Access>,
    Path(agent): Path<String>,
    Query(q): Query<PlaygroundQuery>,
    headers: axum::http::HeaderMap,
    body: bytes::Bytes,
) -> Response {
    match playground_key(&state, &access, q.key_id) {
        Ok(key) => crate::a2a::jsonrpc_for(&state, &key, &agent, &headers, body).await,
        Err(r) => r,
    }
}

/// `GET /admin/api/playground/a2a/{agent}/card?key_id=…`: the agent's card as the key gets it.
async fn playground_a2a_card(
    State(state): State<AppState>,
    Extension(access): Extension<Access>,
    Path(agent): Path<String>,
    Query(q): Query<PlaygroundQuery>,
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
) -> Response {
    match playground_key(&state, &access, q.key_id) {
        Ok(key) => crate::a2a::card_for(&state, &key, &agent, &uri, &headers).await,
        Err(r) => r,
    }
}

/// `POST /admin/api/playground/{chat|responses|messages|embeddings|rerank|decisions|systemone|models|models-extended}?key_id=…`
/// — the admin UI's playground. Runs the normal inference pipeline as the chosen virtual key,
/// so its model access, rate limits, budget and request logging all apply. `models` and
/// `models-extended` answer as `GET /v1/models` and `GET /v1/models-extended` do for the key,
/// and ignore the body.
async fn playground(
    State(state): State<AppState>,
    Extension(access): Extension<Access>,
    Path(api): Path<String>,
    Query(q): Query<PlaygroundQuery>,
    headers: axum::http::HeaderMap,
    body: bytes::Bytes,
) -> Response {
    use crate::api::{Inbound, decisions::Route};
    // A user plays only as an inference key of their projects; others look like keys that do not exist.
    if !state.registry.snapshot().keys.values().any(|k| k.id == q.key_id && sees_key(&access, k)) {
        return crate::error::GatewayError::not_found("virtual key not found").into_response_for(crate::error::Dialect::OpenAi);
    }
    let labels = match crate::labels::Labels::from_headers(&headers) {
        Ok(labels) => labels,
        Err(e) => return e.into_response_for(crate::error::Dialect::OpenAi),
    };
    let inbound = match api.as_str() {
        "chat" => Inbound::Chat,
        "responses" => Inbound::Responses,
        "messages" => Inbound::Messages,
        "models" | "models-extended" => {
            return match crate::auth::key_by_id(&state, q.key_id) {
                Ok(key) if api == "models" => crate::api::models::serve(&state, &key, &headers),
                Ok(key) => crate::api::models::serve_extended(&state, &key),
                Err(e) => e.into_response_for(crate::error::Dialect::OpenAi),
            };
        }
        "embeddings" | "rerank" | "decisions" | "systemone" => {
            let key = match crate::auth::key_by_id(&state, q.key_id) {
                Ok(key) => key,
                Err(e) => return e.into_response_for(crate::error::Dialect::OpenAi),
            };
            return match api.as_str() {
                "embeddings" => crate::api::embeddings::serve(&state, key, labels, body)
                    .await
                    .unwrap_or_else(|e| e.into_response_for(crate::error::Dialect::OpenAi)),
                "rerank" => crate::api::rerank::serve(&state, key, labels, body)
                    .await
                    .unwrap_or_else(|e| e.into_response_for(crate::error::Dialect::OpenAi)),
                "decisions" => crate::api::decisions::serve(&state, key, labels, &headers, body, Route::Decisions).await,
                _ => crate::api::decisions::serve(&state, key, labels, &headers, body, Route::SystemOne).await,
            };
        }
        _ => return AdminError::not_found().into_response(),
    };
    match crate::auth::key_by_id(&state, q.key_id) {
        Ok(key) => crate::api::serve(&state, key, labels, &headers, body, inbound).await,
        Err(e) => e.into_response_for(inbound.dialect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn environment_identity_credentials() {
        use crate::model::Identity;
        let state = crate::state::test_state();
        let env = |provider, s: CredentialSettings| {
            clean_settings(&state, provider, CredentialSettings { identity: Some(Identity::Environment), ..s })
        };
        let azure = CredentialSettings { base_url: Some("https://r.openai.azure.com".into()), ..Default::default() };
        // Azure: nothing stored; an optional client ID for a user-assigned identity, but no tenant.
        let s = env(ProviderKind::Azure, azure.clone()).unwrap();
        assert!(validate_credential(ProviderKind::Azure, "a", &Secrets::default(), &s).is_ok());
        let uai = CredentialSettings { client_id: Some("00000000-0000-0000-0000-000000000001".into()), ..azure.clone() };
        assert!(env(ProviderKind::Azure, uai).is_ok());
        assert!(env(ProviderKind::Azure, CredentialSettings { tenant_id: Some("contoso.onmicrosoft.com".into()), ..azure }).is_err());
        let key = Secrets { api_key: Some("k".into()), ..Default::default() };
        assert!(validate_credential(ProviderKind::Azure, "a", &key, &s).is_err(), "no secrets with the environment's identity");
        // Vertex AI: the project is required, the key file not.
        assert!(env(ProviderKind::Vertex, CredentialSettings::default()).is_err());
        let v = env(ProviderKind::Vertex, CredentialSettings { project_id: Some("p1".into()), ..Default::default() }).unwrap();
        assert!(validate_credential(ProviderKind::Vertex, "v", &Secrets::default(), &v).is_ok());
        // Other providers have their own chains (Bedrock) or none.
        assert!(env(ProviderKind::Openai, CredentialSettings::default()).is_err());
    }

    #[test]
    fn metric_series_fill_empty_buckets() {
        let b = db::LogTotals { subject: "m".into(), bucket: 120, requests: 4, errors: 1, latency_ms_sum: 400, ..Default::default() };
        let buckets = HashMap::from([(120, &b)]);
        let s = series_json(&[60, 120, 180], &buckets);
        assert_eq!(s.len(), 3);
        assert_eq!(s[0]["requests"], 0);
        assert_eq!(s[0]["avg_latency_ms"], Value::Null);
        assert_eq!(s[1]["requests"], 4);
        assert_eq!(s[1]["avg_latency_ms"], 100.0);
        assert_eq!(series_json(&[60], &HashMap::new())[0]["errors"], 0);
        let t = totals(Some(&db::LogTotals { requests: 2, latency_ms_sum: 300, ttft_count: 1, ttft_ms_sum: 50, ..Default::default() }));
        assert_eq!((t.avg_latency_ms, t.avg_ttft_ms), (Some(150.0), Some(50.0)));
        assert_eq!(totals(None).requests, 0);
    }

    #[test]
    fn routers_follow_renames() {
        let route = |name: &str, models: &[&str]| Route {
            name: name.into(),
            description: "d".into(),
            models: models.iter().map(|m| crate::model::RouteModel { model: m.to_string(), reasoning_effort: None }).collect(),
        };
        let mut r = RouterRow {
            id: Uuid::nil(),
            model_name: "auto".into(),
            decision_model: "eu/decision".into(),
            instructions: None,
            routes: sqlx::types::Json(vec![route("Simple", &["eu/luna", "haiku"]), route("Complex", &["eu/luna", "luna"])]),
            fallback_route: Some("Simple".into()),
            tags: vec![],
            enabled: true,
            sticky: true,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        assert!(rename_in_router(&mut r, "eu/luna", "luna"));
        let models = |i: usize| r.routes.0[i].model_names().map(str::to_string).collect::<Vec<_>>();
        assert_eq!(models(0), ["luna", "haiku"]);
        assert_eq!(models(1), ["luna"]);
        assert_eq!(r.fallback_route.as_deref(), Some("Simple"));
        assert!(rename_in_router(&mut r, "eu/decision", "decision"));
        assert_eq!(r.decision_model, "decision");
        assert!(!rename_in_router(&mut r, "unrelated", "x"));
    }

    /// A user sees their projects' inference keys only; an admin every key.
    #[test]
    fn users_see_only_their_projects_inference_keys() {
        let (mine, other) = (Uuid::from_u128(1), Uuid::from_u128(2));
        let key = |project: Option<Uuid>, scope: KeyScope| VirtualKey {
            id: Uuid::now_v7(),
            name: "k".into(),
            key_hash: vec![],
            key_prefix: String::new(),
            scope,
            project_id: project,
            project_active: true,
            allowed_models: vec![],
            allowed_tags: vec![],
            rpm_limit: None,
            tpm_limit: None,
            max_budget_usd: None,
            budget_period: None,
            spend_usd: 0.0,
            spend_reset_at: None,
            enabled: true,
            expires_at: None,
            metadata: json!({}),
            guardrail_ids: Vec::new(),
            web_search: true,
            prompt_logging: false,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let user = Access::User { projects: vec![mine] };
        assert!(sees_key(&user, &key(Some(mine), KeyScope::Inference)));
        assert!(!sees_key(&user, &key(Some(other), KeyScope::Inference)));
        assert!(!sees_key(&user, &key(None, KeyScope::Inference)));
        // Its secret would give them an admin's rights.
        assert!(!sees_key(&user, &key(Some(mine), KeyScope::Admin)));
        assert!(!sees_key(&user, &key(Some(mine), KeyScope::All)));
        for k in [key(None, KeyScope::Admin), key(Some(other), KeyScope::Inference)] {
            assert!(sees_key(&Access::Admin, &k));
        }
    }

    #[test]
    fn keys_and_routers_follow_a_model_rename() {
        let route = |models: &[&str]| Route {
            name: "Only".into(),
            description: "d".into(),
            models: models.iter().map(|m| crate::model::RouteModel { model: m.to_string(), reasoning_effort: None }).collect(),
        };
        let router = |name: &str, models: &[&str]| RouterRow {
            id: Uuid::now_v7(),
            model_name: name.into(),
            decision_model: "decision".into(),
            instructions: None,
            routes: sqlx::types::Json(vec![route(models)]),
            fallback_route: None,
            tags: vec![],
            enabled: true,
            sticky: true,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let key = |models: &[&str]| VirtualKey {
            id: Uuid::now_v7(),
            name: "k".into(),
            key_hash: vec![],
            key_prefix: String::new(),
            scope: crate::model::KeyScope::Inference,
            project_id: None,
            project_active: true,
            allowed_models: models.iter().map(|m| m.to_string()).collect(),
            allowed_tags: vec![],
            rpm_limit: None,
            tpm_limit: None,
            max_budget_usd: None,
            budget_period: None,
            spend_usd: 0.0,
            spend_reset_at: None,
            enabled: true,
            expires_at: None,
            metadata: json!({}),
            guardrail_ids: Vec::new(),
            web_search: true,
            prompt_logging: false,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let mut routers = vec![router("auto", &["luna", "haiku"]), router("other", &["haiku"])];
        let mut keys = vec![key(&["luna", "auto"]), key(&["haiku"]), key(&["luna", "sol"])];
        let (r, k) = rename_model("luna", "sol", &[], &mut routers, &mut keys);
        assert_eq!(r, HashSet::from([routers[0].id]));
        assert_eq!(k, HashSet::from([keys[0].id, keys[2].id]));
        assert_eq!(routers[0].routes.0[0].model_names().collect::<Vec<_>>(), ["sol", "haiku"]);
        assert_eq!(keys[0].allowed_models, ["auto", "sol"]);
        assert_eq!(keys[2].allowed_models, ["sol"]);

        // A router's rename: keys that name it follow.
        routers[0].model_name = "smart".into();
        let (r, k) = rename_model("auto", "smart", &[], &mut routers, &mut keys);
        assert!(r.is_empty());
        assert_eq!(k, HashSet::from([keys[0].id]));
        assert_eq!(keys[0].allowed_models, ["smart", "sol"]);

        // While something still has the old name, nothing follows.
        let (r, k) = rename_model("other", "x", &[], &mut routers, &mut keys);
        assert!(r.is_empty() && k.is_empty());
    }

    fn deployment(name: &str, enabled: bool) -> DeploymentRow {
        let i: DeploymentInput = serde_json::from_value(json!({
            "model_name": name, "provider": "openai", "upstream_model": "gpt", "credential_id": Uuid::nil(), "enabled": enabled,
        }))
        .unwrap();
        to_row(Uuid::now_v7(), i, Utc::now())
    }

    fn alias(name: &str, target_id: Uuid) -> AliasRow {
        AliasRow { id: Uuid::now_v7(), model_name: name.into(), target_id, enabled: true, created_at: Utc::now(), updated_at: Utc::now() }
    }

    #[test]
    fn aliases_target_a_model_by_id_under_a_name_of_their_own() {
        let deps = [deployment("luna", true)];
        let aliases = [alias("smart", deps[0].id)];
        let input = |name: &str, target_id: Uuid| AliasInput { model_name: name.into(), target_id, enabled: true };
        let mut i = input(" fast ", deps[0].id);
        check_alias(&mut i, None, &deps, &[], &aliases).unwrap();
        assert_eq!(i.model_name, "fast");
        assert!(check_alias(&mut input("luna", deps[0].id), None, &deps, &[], &aliases).is_err(), "a deployment's name");
        assert_eq!(check_alias(&mut input("smart", deps[0].id), None, &deps, &[], &aliases).unwrap_err().0, StatusCode::CONFLICT);
        assert!(check_alias(&mut input("smart", deps[0].id), Some(aliases[0].id), &deps, &[], &aliases).is_ok(), "itself");
        assert!(check_alias(&mut input("fast", Uuid::now_v7()), None, &deps, &[], &aliases).is_err(), "no such target");
        assert!(check_alias(&mut input("fast", aliases[0].id), None, &deps, &[], &aliases).is_err(), "not an alias of an alias");
    }

    #[test]
    fn aliases_stay_on_their_model_when_its_deployment_leaves() {
        let (a, b, c) = (deployment("luna", true), deployment("luna", false), deployment("luna", true));
        let mut aliases = [alias("smart", a.id), alias("other", Uuid::now_v7())];
        // `a` renamed: the alias moves to the other enabled deployment of luna.
        let mut moved = a.clone();
        moved.model_name = "sol".into();
        let deps = [moved.clone(), b.clone(), c.clone()];
        assert_eq!(retarget_aliases(a.id, "luna", &deps, &mut aliases), HashSet::from([aliases[0].id]));
        assert_eq!(aliases[0].target_id, c.id);
        // `c` deleted: to the disabled one, the last of luna.
        assert_eq!(retarget_aliases(c.id, "luna", &[moved.clone(), b.clone()], &mut aliases).len(), 1);
        assert_eq!(aliases[0].target_id, b.id);
        // `b` renamed, the last of luna: the alias follows it by its id.
        let mut renamed = b.clone();
        renamed.model_name = "nova".into();
        assert!(retarget_aliases(b.id, "luna", &[moved, renamed], &mut aliases).is_empty());
        assert_eq!(aliases[0].target_id, b.id);
    }

    #[test]
    fn metric_windows() {
        let q = |range: &str, from: Option<&str>, to: Option<&str>| RangeQuery {
            range: Some(range.into()),
            key_id: None,
            project_id: None,
            deleted_keys: None,
            labels: None,
            models: None,
            from: from.map(|d| d.parse().unwrap()),
            to: to.map(|d| d.parse().unwrap()),
        };
        // 2026-10-25 17:42:30 UTC.
        let now = DateTime::parse_from_rfc3339("2026-10-25T17:42:30Z").unwrap().with_timezone(&Utc);
        let w = window(&q("1h", None, None), now).unwrap();
        assert_eq!((w.step, w.stamps.len()), (60, 60));
        assert_eq!(*w.stamps.last().unwrap(), now.timestamp() - 30);
        assert_eq!(window(&q("7d", None, None), now).unwrap().stamps.len(), 168);
        // This month: daily buckets from the 1st (UTC) through today.
        let m = window(&q("month", None, None), now).unwrap();
        assert_eq!((m.step, m.stamps.len()), (DAY, 25));
        assert_eq!(m.since.to_rfc3339(), "2026-10-01T00:00:00+00:00");
        // Custom: whole days, inclusive; hourly up to two days.
        let c = window(&q("custom", Some("2026-09-01"), Some("2026-09-30")), now).unwrap();
        assert_eq!((c.step, c.stamps.len()), (DAY, 30));
        assert_eq!(c.until.to_rfc3339(), "2026-10-01T00:00:00+00:00");
        let short = window(&q("custom", Some("2026-09-01"), Some("2026-09-02")), now).unwrap();
        assert_eq!((short.step, short.stamps.len()), (3600, 48));
        assert!(window(&q("custom", Some("2026-09-02"), Some("2026-09-01")), now).is_err());
        assert!(window(&q("custom", None, None), now).is_err());
        assert!(window(&q("custom", Some("2024-01-01"), Some("2026-01-01")), now).is_err());
        assert!(window(&q("year", None, None), now).is_err());
    }

    #[test]
    fn log_windows() {
        let now = DateTime::parse_from_rfc3339("2026-10-25T17:42:30Z").unwrap().with_timezone(&Utc);
        let day = |d: &str| Some(d.parse::<chrono::NaiveDate>().unwrap());
        let text = |(since, until): LogWindow| (since.map(|t| t.to_rfc3339()), until.map(|t| t.to_rfc3339()));
        assert_eq!(log_window(None, None, None, now).unwrap(), (None, None));
        assert_eq!(text(log_window(Some("24h"), None, None, now).unwrap()), (Some("2026-10-24T17:42:30+00:00".into()), None));
        assert_eq!(text(log_window(Some("month"), None, None, now).unwrap()), (Some("2026-10-01T00:00:00+00:00".into()), None));
        // Custom: whole UTC days, both inclusive.
        assert_eq!(
            text(log_window(Some("custom"), day("2026-09-01"), day("2026-09-30"), now).unwrap()),
            (Some("2026-09-01T00:00:00+00:00".into()), Some("2026-10-01T00:00:00+00:00".into()))
        );
        assert!(log_window(Some("custom"), day("2026-09-02"), day("2026-09-01"), now).is_err());
        assert!(log_window(Some("custom"), None, None, now).is_err());
        assert!(log_window(Some("year"), None, None, now).is_err());
    }
}
