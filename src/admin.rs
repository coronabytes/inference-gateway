//! Admin API: credentials, deployments, routers, virtual keys, metrics, logs, cluster. Requests
//! authenticate with the master key or an admin session (see `admin::auth`).

use axum::{Json, extract::State, middleware};
use serde_json::Value;
use utoipa_axum::{router::OpenApiRouter, routes};
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

// Used by other sections: which keys a caller sees, and the request log scans in flight.
use keys::{check_sees_key, sees_key};
use metrics::LOG_SCANS;

type R<T> = Result<T, AdminError>;

/// The admin API. `state` runs the guard; without one, the routes are only read for the OpenAPI
/// document (see `crate::routes`).
pub fn router(state: Option<&AppState>) -> OpenApiRouter<AppState> {
    let guarded = OpenApiRouter::new()
        .routes(routes!(status))
        .routes(routes!(credentials::list_credentials, credentials::create_credential))
        .routes(routes!(credentials::update_credential, credentials::delete_credential))
        .routes(routes!(credentials::foundry_deployments))
        .routes(routes!(credentials::bedrock_models))
        .routes(routes!(credentials::aws_regions))
        .routes(routes!(deployments::list_deployments, deployments::create_deployment))
        .routes(routes!(deployments::get_deployment, deployments::update_deployment, deployments::delete_deployment))
        .routes(routes!(deployments::test_deployment))
        .routes(routes!(deployments::clear_cooldown))
        .routes(routes!(guardrails::list_guardrails, guardrails::create_guardrail))
        .routes(routes!(guardrails::list_prices))
        .routes(routes!(guardrails::update_guardrail, guardrails::delete_guardrail))
        .routes(routes!(guardrails::test_guardrail))
        .routes(routes!(mcp::list_mcp_servers, mcp::create_mcp_server))
        .routes(routes!(mcp::update_mcp_server, mcp::delete_mcp_server))
        .routes(routes!(mcp::test_mcp_server))
        .routes(routes!(oauth::discover))
        .routes(routes!(oauth::list_connections))
        .routes(routes!(connect::list_apps))
        .routes(routes!(connect::revoke_app))
        .routes(routes!(connect::get_request, connect::approve))
        .routes(routes!(connect::deny))
        .routes(routes!(oauth::connect, oauth::disconnect))
        .routes(routes!(oauth::list_sign_ins))
        .routes(routes!(a2a::list_a2a_agents, a2a::create_a2a_agent))
        .routes(routes!(a2a::update_a2a_agent, a2a::delete_a2a_agent))
        .routes(routes!(a2a::test_a2a_agent))
        .routes(routes!(tool_routers::list_tool_routers, tool_routers::create_tool_router))
        .routes(routes!(tool_routers::update_tool_router, tool_routers::delete_tool_router))
        .routes(routes!(routers::list_routers, routers::create_router))
        .routes(routes!(routers::update_router, routers::delete_router))
        .routes(routes!(aliases::list_aliases, aliases::create_alias))
        .routes(routes!(aliases::update_alias, aliases::delete_alias))
        .routes(routes!(keys::list_tags))
        .routes(routes!(hub::hub))
        .routes(routes!(hub::efforts))
        .routes(routes!(hub::set_metadata))
        .routes(routes!(transfer::export))
        .routes(routes!(transfer::import))
        .routes(routes!(keys::list_keys, keys::create_key))
        .routes(routes!(keys::key_options))
        .routes(routes!(keys::update_key, keys::delete_key))
        .routes(routes!(keys::reset_spend))
        .routes(routes!(keys::regenerate_key))
        .routes(routes!(logs::key_bodies, logs::delete_key_bodies))
        .routes(routes!(projects::list_projects, projects::create_project))
        .routes(routes!(keys::project_options))
        .routes(routes!(projects::update_project, projects::delete_project))
        .routes(routes!(projects::reset_project_spend))
        .routes(routes!(logs::project_bodies, logs::delete_project_bodies))
        .routes(routes!(metrics::metrics))
        .routes(routes!(logs::logs))
        .routes(routes!(logs::log_labels))
        .routes(routes!(logs::log_tools))
        .routes(routes!(logs::log_bodies))
        .routes(routes!(logs::audit_log))
        .routes(routes!(metrics::key_usage))
        .routes(routes!(settings::cluster))
        .routes(routes!(settings::get_settings, settings::put_settings))
        .routes(routes!(playground::playground_options))
        .routes(routes!(playground::playground))
        .routes(routes!(playground::playground_a2a))
        .routes(routes!(playground::playground_a2a_card))
        .merge(crate::admin::auth::protected_api_routes())
        .merge(crate::prices::routes());
    let guarded = match state {
        // A route layer, so the guard knows which route it guards (users reach only some).
        Some(state) => guarded.route_layer(middleware::from_fn_with_state(state.clone(), crate::admin::auth::require_admin)),
        None => guarded,
    };
    guarded.merge(crate::admin::auth::api_routes())
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

/// Status
///
/// This replica.
#[utoipa::path(get, path = "/status", tag = "Usage",
    responses((status = 200, description = "The replica.", body = Status)),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
async fn status(State(state): State<AppState>) -> Json<Status> {
    let snap = state.registry.snapshot();
    Json(Status {
        instance_id: state.cfg.instance_id.clone(),
        version: crate::VERSION,
        registry_loaded_at: snap.loaded_at,
        registry_errors: snap.errors.clone(),
    })
}

/// This replica, and how its configuration last loaded.
#[derive(serde::Serialize, utoipa::ToSchema)]
struct Status {
    instance_id: String,
    /// The git tag the gateway was built from.
    version: &'static str,
    registry_loaded_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Deployments, routers and aliases that could not be loaded, and why.
    registry_errors: Vec<String>,
}
