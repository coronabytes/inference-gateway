//! Admin API: credentials, deployments, routers, virtual keys, metrics, logs, cluster. Requests
//! authenticate with the master key or an admin session (see `admin::auth`).

use axum::{
    Json, Router,
    extract::State,
    middleware,
    routing::{get, post},
};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{db, error::AdminError, state::AppState};

pub(crate) mod a2a;
mod aliases;
pub(crate) mod auth;
pub(crate) mod connect;
mod credentials;
mod deployments;
pub(crate) mod guardrails;
pub(crate) mod hub;
mod keys;
mod logs;
pub(crate) mod mcp;
mod metrics;
pub(crate) mod oauth;
mod playground;
mod projects;
mod routers;
mod settings;
pub(crate) mod tool_routers;
pub(crate) mod transfer;

// The sections' request and response types, at the paths the OpenAPI document names them by.
pub(crate) use aliases::{AliasInput, AliasQuery, AliasView};
pub(crate) use credentials::{CredentialInput, CredentialQuery, CredentialView};
pub(crate) use deployments::{DeploymentInput, DeploymentQuery, DeploymentView};
pub(crate) use keys::{KeyInput, KeyOptionsQuery, KeyQuery, KeyView};
pub(crate) use logs::{AuditQuery, BodiesView, DeletedBodies, LabelKey, LogQuery, StoredBodies};
pub(crate) use metrics::RangeQuery;
pub(crate) use playground::{PlaygroundOptions, PlaygroundQuery};
pub(crate) use projects::{ProjectInput, ProjectQuery, ProjectView};
pub(crate) use routers::{RouterInput, RouterListItem, RouterQuery, RouterView};
pub(crate) use settings::SettingsView;
// Used by other sections: which keys a caller sees, and the request log scans in flight.
use keys::{check_sees_key, sees_key};
use metrics::LOG_SCANS;

type R<T> = Result<T, AdminError>;

pub fn router(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/status", get(status))
        .route("/credentials", get(credentials::list_credentials).post(credentials::create_credential))
        .route("/credentials/{id}", axum::routing::put(credentials::update_credential).delete(credentials::delete_credential))
        .route("/credentials/{id}/foundry-deployments", get(credentials::foundry_deployments))
        .route("/credentials/{id}/bedrock-models", get(credentials::bedrock_models))
        .route("/aws-regions", get(credentials::aws_regions))
        .route("/deployments", get(deployments::list_deployments).post(deployments::create_deployment))
        .route(
            "/deployments/{id}",
            get(deployments::get_deployment).put(deployments::update_deployment).delete(deployments::delete_deployment),
        )
        .route("/deployments/{id}/test", post(deployments::test_deployment))
        .route("/deployments/{id}/clear-cooldown", post(deployments::clear_cooldown))
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
        .route("/routers", get(routers::list_routers).post(routers::create_router))
        .route("/routers/{id}", axum::routing::put(routers::update_router).delete(routers::delete_router))
        .route("/aliases", get(aliases::list_aliases).post(aliases::create_alias))
        .route("/aliases/{id}", axum::routing::put(aliases::update_alias).delete(aliases::delete_alias))
        .route("/tags", get(keys::list_tags))
        .route("/hub", get(hub::hub))
        .route("/efforts", get(hub::efforts))
        .route("/model-metadata", axum::routing::put(hub::set_metadata))
        .route("/config/export", get(transfer::export))
        .route("/config/import", post(transfer::import))
        .route("/keys", get(keys::list_keys).post(keys::create_key))
        .route("/keys/options", get(keys::key_options))
        .route("/keys/{id}", axum::routing::put(keys::update_key).delete(keys::delete_key))
        .route("/keys/{id}/reset-spend", post(keys::reset_spend))
        .route("/keys/{id}/regenerate", post(keys::regenerate_key))
        .route("/keys/{id}/bodies", get(logs::key_bodies).delete(logs::delete_key_bodies))
        .route("/projects", get(projects::list_projects).post(projects::create_project))
        .route("/projects/options", get(keys::project_options))
        .route("/projects/{id}", axum::routing::put(projects::update_project).delete(projects::delete_project))
        .route("/projects/{id}/reset-spend", post(projects::reset_project_spend))
        .route("/projects/{id}/bodies", get(logs::project_bodies).delete(logs::delete_project_bodies))
        .route("/metrics", get(metrics::metrics))
        .route("/logs", get(logs::logs))
        .route("/logs/labels", get(logs::log_labels))
        .route("/logs/tools", get(logs::log_tools))
        .route("/logs/{id}/bodies", get(logs::log_bodies))
        .route("/audit", get(logs::audit_log))
        .route("/usage/keys", get(metrics::key_usage))
        .route("/cluster", get(settings::cluster))
        .route("/settings", get(settings::get_settings).put(settings::put_settings))
        .route("/playground/options", get(playground::playground_options))
        .route("/playground/{api}", post(playground::playground))
        .route("/playground/a2a/{agent}", post(playground::playground_a2a))
        .route("/playground/a2a/{agent}/card", get(playground::playground_a2a_card))
        .merge(crate::admin::auth::protected_api_routes())
        .merge(crate::prices::routes())
        // A route layer, so the guard knows which route it guards (users reach only some).
        .route_layer(middleware::from_fn_with_state(state, crate::admin::auth::require_admin))
        .merge(crate::admin::auth::api_routes())
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

/// A `labels` filter (`key=value` pairs, as clients send them) as the JSON object the request
/// log is matched against; `None` when empty.
fn label_filter(labels: Option<&str>) -> R<Option<Value>> {
    let labels = crate::labels::Labels::parse(labels.unwrap_or_default()).map_err(AdminError::bad_request)?;
    Ok(labels.to_json())
}

fn project_filter(s: Option<&str>) -> R<db::AnyOf<Uuid>> {
    any_of(s, |p| p.parse().map_err(|_| AdminError::bad_request("projects must be ids, or - for none")))
}

/// A model name a client can send: deployments', routers' and aliases' names.
fn valid_model_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.:/@".contains(c))
}

/// A serde default for flags that start on.
fn yes() -> bool {
    true
}

/// Budgets a key or project list reads at once.
const LIST_BUDGET_LOOKUPS: usize = 8;

async fn status(State(state): State<AppState>) -> Json<Value> {
    let snap = state.registry.snapshot();
    Json(json!({
        "instance_id": state.cfg.instance_id,
        "version": crate::VERSION,
        "registry_loaded_at": snap.loaded_at,
        "registry_errors": snap.errors,
    }))
}
