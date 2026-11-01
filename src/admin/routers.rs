//! Auto routers: a model name whose requests a decision model sends to one of its routes.

use std::collections::BTreeSet;

use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{R, after_change, deployments::follow_rename, search_text, tag_filter, valid_model_name, yes};
use crate::{
    audit::{self, Actor},
    db,
    error::AdminError,
    model::{AliasRow, DeploymentRow, Mode, Route, RouterRow, normalize_tags},
    state::AppState,
};

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct RouterView {
    id: Uuid,
    model_name: String,
    decision_model: String,
    #[schema(required)]
    instructions: Option<String>,
    routes: Vec<Route>,
    #[schema(required)]
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
pub(super) fn check_router(
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

pub(super) fn router_row(id: Uuid, i: RouterInput, created_at: DateTime<Utc>) -> RouterRow {
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

/// List auto routers
///
/// With the models each names that no deployment serves.
#[utoipa::path(get, path = "/routers", tag = "Auto routers", params(RouterQuery),
    responses((status = 200, description = "The routers.", body = Vec<RouterListItem>)),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn list_routers(State(state): State<AppState>, Query(q): Query<RouterQuery>) -> R<Json<Vec<RouterListItem>>> {
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

/// Create auto router
#[utoipa::path(post, path = "/routers", tag = "Auto routers", request_body = RouterInput,
    responses(
        (status = 201, description = "The new router.", body = RouterView),
        (status = 400, description = "Invalid router.", body = crate::error::AdminErrorBody),
        (status = 409, description = "The name is taken.", body = crate::error::AdminErrorBody),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn create_router(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(mut i): Json<RouterInput>,
) -> R<(StatusCode, Json<RouterView>)> {
    validate_router(&state, None, &mut i).await?;
    let row = router_row(Uuid::now_v7(), i, Utc::now());
    db::upsert_router(&state.db, &row).await?;
    let view = router_view(row);
    let event = audit::Event::new("router", "create").on(view.id, &view.model_name).details(audit::snapshot(&view));
    after_change(&state, &actor, "router created", view.id, Some(event)).await;
    Ok((StatusCode::CREATED, Json(view)))
}

/// Update auto router
#[utoipa::path(put, path = "/routers/{id}", tag = "Auto routers", params(("id" = Uuid, Path)),
    request_body = RouterInput,
    responses(
        (status = 200, description = "The router.", body = RouterView),
        (status = 400, description = "Invalid router.", body = crate::error::AdminErrorBody),
        (status = 404, description = "No such router.", body = crate::error::AdminErrorBody),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn update_router(
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
    let view = router_view(row);
    let changes = audit::changes(&router_view(existing), &view);
    let event = (!changes.is_empty()).then(|| audit::Event::new("router", "update").on(id, &view.model_name).details(changes.into()));
    after_change(&state, &actor, "router updated", id, event).await;
    Ok(Json(view))
}

/// Delete auto router
#[utoipa::path(delete, path = "/routers/{id}", tag = "Auto routers", params(("id" = Uuid, Path)),
    responses((status = 204, description = "Deleted."), (status = 404, description = "No such router.", body = crate::error::AdminErrorBody)),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn delete_router(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<Uuid>,
) -> R<StatusCode> {
    let existing = db::get_router(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    if !db::delete_router(&state.db, id).await? {
        return Err(AdminError::not_found());
    }
    after_change(&state, &actor, "router deleted", id, Some(audit::Event::new("router", "delete").on(id, existing.model_name))).await;
    Ok(StatusCode::NO_CONTENT)
}
