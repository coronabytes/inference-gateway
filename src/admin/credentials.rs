//! Credentials: the providers' keys and identities that deployments and guardrails call with.

use std::collections::BTreeMap;

use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{R, csv, search_text};
use crate::{
    audit::{self, Actor},
    db,
    error::AdminError,
    model::{CredentialRow, CredentialSettings, ProviderKind, Secrets},
    registry,
    state::AppState,
    sync::broadcast_reload,
};

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
pub(super) fn clean_settings(state: &AppState, provider: ProviderKind, s: CredentialSettings) -> R<CredentialSettings> {
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

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct CredentialQuery {
    /// Just the credentials whose name contains this text, ignoring case.
    q: Option<String>,
    /// Just the credentials of these providers, as `openai,azure`.
    provider: Option<String>,
}

pub(super) async fn list_credentials(State(state): State<AppState>, Query(q): Query<CredentialQuery>) -> R<Json<Vec<CredentialView>>> {
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

pub(super) async fn create_credential(
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
    db::insert_credential(&state.db, &row).await.map_err(AdminError::conflict_on_duplicate("a credential"))?;
    tracing::info!(credential = %row.id, provider = %row.provider, "credential created");
    let view = credential_view(&state, row, 0);
    audit::Event::new("credential", "create").on(view.id, &view.name).details(audit::snapshot(&view)).record(&state, &actor).await;
    Ok((StatusCode::CREATED, Json(view)))
}

pub(super) async fn update_credential(
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
    let stored = registry::decrypt_secrets(&state.cfg.secret_box, row.secrets.as_deref()).map_err(AdminError::internal)?;
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
    db::update_credential(&state.db, &row).await.map_err(AdminError::conflict_on_duplicate("a credential"))?;
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

pub(super) async fn delete_credential(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<Uuid>,
) -> R<StatusCode> {
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
pub(super) async fn foundry_deployments(State(state): State<AppState>, Path(id): Path<Uuid>) -> R<Json<crate::entra::FoundryDeployments>> {
    let row = db::get_credential(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    if row.provider != ProviderKind::Azure.as_str() {
        return Err(AdminError::bad_request("only Azure credentials have Foundry deployments"));
    }
    let secrets = registry::decrypt_secrets(&state.cfg.secret_box, row.secrets.as_deref()).map_err(AdminError::internal)?;
    let settings = &row.settings.0;
    let source = crate::entra::source(settings, &secrets)
        .ok_or_else(|| AdminError::bad_request("listing deployments needs a credential that uses Entra ID; an API key cannot read them"))?;
    let base = settings.base_url.as_deref().ok_or_else(|| AdminError::bad_request("the credential has no resource endpoint"))?;
    let found = crate::entra::list_deployments(&state, &source, base).await.map_err(AdminError::bad_gateway)?;
    Ok(Json(found))
}

/// The AWS regions the gateway's SDK knows, for the console's Bedrock region list.
pub(super) async fn aws_regions() -> Json<&'static [crate::aws::AwsRegion]> {
    Json(crate::aws::regions())
}

/// The models and inference profiles a Bedrock credential can call, with its account's quotas,
/// for the console to fill in a new deployment. Listings it may not read are noted, not failed.
pub(super) async fn bedrock_models(State(state): State<AppState>, Path(id): Path<Uuid>) -> R<Json<crate::aws::models::BedrockModels>> {
    let row = db::get_credential(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    if row.provider != ProviderKind::Bedrock.as_str() {
        return Err(AdminError::bad_request("only Bedrock credentials have Bedrock models"));
    }
    let secrets = registry::decrypt_secrets(&state.cfg.secret_box, row.secrets.as_deref()).map_err(AdminError::internal)?;
    if secrets.api_key.is_some() {
        return Err(AdminError::bad_request("a Bedrock API key cannot list models or quotas; use AWS credentials for that"));
    }
    let found = state
        .aws
        .models
        .get(&state, row.id, row.updated_at, &row.settings.0, &secrets)
        .await
        .map_err(|e| AdminError::bad_gateway(e.message))?;
    Ok(Json(found))
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
}
