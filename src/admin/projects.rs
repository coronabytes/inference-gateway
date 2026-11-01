//! Projects: keys grouped under a budget and guardrails, which also decide whether the keys may
//! search the web or log prompts.

use std::collections::BTreeSet;

use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use chrono::Utc;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use super::{
    LIST_BUDGET_LOOKUPS, R,
    keys::{check_budget, pick_guardrails, restart_spend},
    search_text, yes,
};
use crate::{
    admin::auth::Access,
    audit::{self, Actor},
    budget::Spender,
    db,
    error::AdminError,
    model::{ProjectRow, PromptLogging},
    state::AppState,
    sync::broadcast_reload,
};

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
    #[schema(value_type = Option<crate::budget::BudgetPeriod>)]
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
    #[schema(required)]
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

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct ProjectQuery {
    /// Just the projects whose name or description contains this text, ignoring case.
    q: Option<String>,
}

/// List projects
///
/// Projects group keys in the console. An inactive project (`enabled: false`) blocks its keys,
/// and its budget, if set, comes on top of theirs. By name, with key counts and budget status.
#[utoipa::path(get, path = "/projects", tag = "Virtual keys", params(ProjectQuery),
    responses((status = 200, description = "The projects.", body = Vec<ProjectView>)),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn list_projects(
    State(state): State<AppState>,
    Extension(access): Extension<Access>,
    Query(q): Query<ProjectQuery>,
) -> R<Json<Vec<ProjectView>>> {
    let search = search_text(q.q.as_deref());
    let projects = db::list_projects(&state.db, search.as_deref(), access.projects()).await?;
    let views = futures::stream::iter(projects.into_iter().map(|(project, keys)| ProjectView::new(&state, project, keys)));
    Ok(Json(views.buffered(LIST_BUDGET_LOOKUPS).collect().await))
}

/// Create project
#[utoipa::path(post, path = "/projects", tag = "Virtual keys", request_body = ProjectInput,
    responses(
        (status = 201, description = "Created.", body = ProjectView),
        (status = 400, description = "Invalid project.", body = crate::error::AdminErrorBody),
        (status = 409, description = "A project with this name exists.", body = crate::error::AdminErrorBody),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn create_project(
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
    db::upsert_project(&state.db, &p).await.map_err(AdminError::conflict_on_duplicate("a project"))?;
    tracing::info!(project = %p.id, name = %p.name, "project created");
    audit::Event::new("project", "create").on(p.id, &p.name).details(audit::snapshot(&p)).record(&state, &actor).await;
    Ok((StatusCode::CREATED, Json(ProjectView::new(&state, p, 0).await)))
}

/// Update project
#[utoipa::path(put, path = "/projects/{id}", tag = "Virtual keys", params(("id" = Uuid, Path)),
    request_body = ProjectInput,
    responses(
        (status = 200, description = "The project.", body = ProjectView),
        (status = 400, description = "Invalid project.", body = crate::error::AdminErrorBody),
        (status = 404, description = "No such project.", body = crate::error::AdminErrorBody),
        (status = 409, description = "A project with this name exists.", body = crate::error::AdminErrorBody),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn update_project(
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
    db::upsert_project(&state.db, &p).await.map_err(AdminError::conflict_on_duplicate("a project"))?;
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

/// Reset project spend
///
/// Starts the project's current budget period over. Its keys' own budgets are not affected.
#[utoipa::path(post, path = "/projects/{id}/reset-spend", tag = "Virtual keys", params(("id" = Uuid, Path)),
    responses((status = 204, description = "Reset."), (status = 404, description = "No such project.", body = crate::error::AdminErrorBody)),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn reset_project_spend(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<Uuid>,
) -> R<StatusCode> {
    let p = db::get_project(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    let spent = crate::budget::project_status(&state, &p).await.map(|b| b.spent_usd);
    if !db::reset_project_spend(&state.db, id).await? {
        return Err(AdminError::not_found());
    }
    audit::Event::new("project", "reset_spend").on(id, p.name).details(json!({ "spent_usd": spent })).record(&state, &actor).await;
    restart_spend(&state, Spender::Project(id), "project spend reset").await;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete project
///
/// Its keys are deleted with it. Refused (`409`) while deployments are limited to it.
#[utoipa::path(delete, path = "/projects/{id}", tag = "Virtual keys", params(("id" = Uuid, Path)),
    responses((status = 204, description = "Deleted."), (status = 404, description = "No such project.", body = crate::error::AdminErrorBody)),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
// Without it, the deployments limited to it would be open to more keys, or to none.
pub(super) async fn delete_project(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<Uuid>,
) -> R<StatusCode> {
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
