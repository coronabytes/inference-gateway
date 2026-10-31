//! Admin API for tool routers: a decision model and routes, each a group of one MCP server's tools
//! with a description of when they are needed. Requests name a router like an MCP server.

use axum::{
    Extension, Json,
    extract::{Path, State},
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::types::Json as SqlJson;
use uuid::Uuid;

use super::{R, mcp::check_name};
use crate::{
    audit::{self, Actor},
    db,
    error::AdminError,
    mcp::{
        hub::SEPARATOR,
        router::{ToolRoute, ToolRouterRow},
    },
    model::Mode,
    state::AppState,
    sync::broadcast_reload,
};

/// Routes per router, at most: each is a question in one decision call.
const MAX_ROUTES: usize = 64;

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct ToolRouterView {
    id: Uuid,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    decision_model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    routes: Vec<ToolRoute>,
    threshold: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_routes: Option<i32>,
    enabled: bool,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

fn view(r: ToolRouterRow) -> ToolRouterView {
    ToolRouterView {
        id: r.id,
        name: r.name,
        description: r.description,
        decision_model: r.decision_model,
        instructions: r.instructions,
        routes: r.routes.0,
        threshold: r.threshold,
        max_routes: r.max_routes,
        enabled: r.enabled,
        created_at: r.created_at,
        updated_at: r.updated_at,
    }
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolRouterInput {
    /// What requests name it by, like an MCP server: lowercase letters, digits, `-` and `_`, and
    /// no MCP server's name.
    name: String,
    #[serde(default)]
    description: Option<String>,
    /// A decision model's name.
    decision_model: String,
    /// The question the decision model gets for every route; empty for the default.
    #[serde(default)]
    instructions: Option<String>,
    /// 1 to 64 routes.
    routes: Vec<ToolRoute>,
    /// A route is offered when the decision model is at least this sure it is needed: above 0,
    /// below 1. Default 0.5.
    #[serde(default)]
    threshold: Option<f64>,
    /// At most this many routes per request, the surest first. Default: no limit.
    #[serde(default)]
    max_routes: Option<i32>,
    #[serde(default = "enabled")]
    enabled: bool,
}

fn enabled() -> bool {
    true
}

/// Checks an input and builds the row it describes.
async fn row(state: &AppState, i: ToolRouterInput, existing: Option<&ToolRouterRow>) -> R<ToolRouterRow> {
    let name = check_name(&i.name, "tool router")?;
    if name.contains(SEPARATOR) {
        return Err(AdminError::bad_request(format!("the tool router's name cannot contain {SEPARATOR}")));
    }
    let servers = db::list_mcp_servers(&state.db).await?;
    if servers.iter().any(|s| s.name == name) {
        return Err(AdminError::conflict("an MCP server has this name: requests could not tell them apart"));
    }
    let description = i.description.as_deref().map(str::trim).filter(|d| !d.is_empty()).map(str::to_string);
    let decision_model = i.decision_model.trim().to_string();
    crate::api::deployments_for(state, &decision_model, Mode::Decision)
        .map_err(|_| AdminError::bad_request(format!("'{decision_model}' is not a decision model")))?;
    let instructions = i.instructions.as_deref().map(str::trim).filter(|d| !d.is_empty()).map(str::to_string);
    if !(1..=MAX_ROUTES).contains(&i.routes.len()) {
        return Err(AdminError::bad_request(format!("a tool router has 1 to {MAX_ROUTES} routes")));
    }
    let mut routes = Vec::with_capacity(i.routes.len());
    for r in i.routes {
        let route_name = r.name.trim().to_string();
        let ok = (1..=64).contains(&route_name.len()) && route_name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if !ok {
            return Err(AdminError::bad_request("a route's name is 1-64 letters, digits, - and _"));
        }
        if routes.iter().any(|x: &ToolRoute| x.name == route_name) {
            return Err(AdminError::bad_request(format!("two routes are named {route_name}")));
        }
        let description = r.description.trim().to_string();
        if description.is_empty() {
            return Err(AdminError::bad_request(format!("route {route_name} needs a description: the decision model picks routes by it")));
        }
        if !servers.iter().any(|s| s.id == r.server_id) {
            return Err(AdminError::bad_request(format!("route {route_name}: no such MCP server")));
        }
        let mut tools: Vec<String> = Vec::new();
        for t in r.tools.iter().map(|t| t.trim()).filter(|t| !t.is_empty()) {
            if !tools.iter().any(|x| x == t) {
                tools.push(t.to_string());
            }
        }
        routes.push(ToolRoute { name: route_name, description, server_id: r.server_id, tools });
    }
    let threshold = i.threshold.unwrap_or(0.5);
    if !(threshold > 0.0 && threshold < 1.0) {
        return Err(AdminError::bad_request("the threshold is above 0 and below 1"));
    }
    if i.max_routes.is_some_and(|m| m < 1) {
        return Err(AdminError::bad_request("at most 1 route or more per request"));
    }
    let now = Utc::now();
    Ok(ToolRouterRow {
        id: existing.map_or_else(Uuid::now_v7, |e| e.id),
        name,
        description,
        decision_model,
        instructions,
        routes: SqlJson(routes),
        threshold,
        max_routes: i.max_routes,
        enabled: i.enabled,
        created_at: existing.map_or(now, |e| e.created_at),
        updated_at: now,
    })
}

fn conflict_on_duplicate_name(e: sqlx::Error) -> AdminError {
    if db::is_unique_violation(&e) { AdminError::conflict("a tool router with this name already exists") } else { e.into() }
}

pub(crate) async fn list_tool_routers(State(state): State<AppState>) -> R<Json<Vec<ToolRouterView>>> {
    Ok(Json(db::list_tool_routers(&state.db).await?.into_iter().map(view).collect()))
}

pub(crate) async fn create_tool_router(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(i): Json<ToolRouterInput>,
) -> R<(StatusCode, Json<ToolRouterView>)> {
    let row = row(&state, i, None).await?;
    db::insert_tool_router(&state.db, &row).await.map_err(conflict_on_duplicate_name)?;
    broadcast_reload(&state, "tool router created").await;
    tracing::info!(tool_router = %row.id, name = %row.name, "tool router created");
    let v = view(row);
    audit::Event::new("tool_router", "create").on(v.id, &v.name).details(audit::snapshot(&v)).record(&state, &actor).await;
    Ok((StatusCode::CREATED, Json(v)))
}

pub(crate) async fn update_tool_router(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<Uuid>,
    Json(i): Json<ToolRouterInput>,
) -> R<Json<ToolRouterView>> {
    let existing = db::get_tool_router(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    let before = view(existing.clone());
    let row = row(&state, i, Some(&existing)).await?;
    db::update_tool_router(&state.db, &row).await.map_err(conflict_on_duplicate_name)?;
    broadcast_reload(&state, "tool router updated").await;
    tracing::info!(tool_router = %id, "tool router updated");
    let v = view(row);
    let changes = audit::changes(&before, &v);
    if !changes.is_empty() {
        audit::Event::new("tool_router", "update").on(id, &v.name).details(changes.into()).record(&state, &actor).await;
    }
    Ok(Json(v))
}

pub(crate) async fn delete_tool_router(State(state): State<AppState>, Extension(actor): Extension<Actor>, Path(id): Path<Uuid>) -> R<StatusCode> {
    let name = db::get_tool_router(&state.db, id).await?.ok_or_else(AdminError::not_found)?.name;
    if !db::delete_tool_router(&state.db, id).await? {
        return Err(AdminError::not_found());
    }
    broadcast_reload(&state, "tool router deleted").await;
    tracing::info!(tool_router = %id, "tool router deleted");
    audit::Event::new("tool_router", "delete").on(id, name).record(&state, &actor).await;
    Ok(StatusCode::NO_CONTENT)
}

/// The tool routers whose routes use an MCP server, by name: it cannot be deleted while they do.
pub(crate) async fn routers_using(state: &AppState, server: Uuid) -> R<Vec<String>> {
    Ok(db::list_tool_routers(&state.db).await?.into_iter().filter(|r| r.routes.0.iter().any(|x| x.server_id == server)).map(|r| r.name).collect())
}
