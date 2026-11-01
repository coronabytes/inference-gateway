//! Model aliases: another name for a deployment's or router's model.

use std::collections::HashSet;

use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{R, deployments::follow_rename, search_text, valid_model_name, yes};
use crate::{
    audit::{self, Actor},
    db,
    error::AdminError,
    model::{AliasRow, DeploymentRow, RouterRow},
    state::AppState,
    sync::broadcast_reload,
};

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
pub(super) fn target_name<'a>(id: Uuid, deployments: &'a [DeploymentRow], routers: &'a [RouterRow]) -> Option<&'a str> {
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
pub(super) fn check_alias(
    i: &mut AliasInput,
    id: Option<Uuid>,
    deployments: &[DeploymentRow],
    routers: &[RouterRow],
    aliases: &[AliasRow],
) -> R<()> {
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

pub(super) fn alias_row(id: Uuid, i: AliasInput, created_at: DateTime<Utc>) -> AliasRow {
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
pub(super) async fn keep_aliases(state: &AppState, id: Uuid, old: &str) -> R<()> {
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

/// List model aliases
#[utoipa::path(get, path = "/aliases", tag = "Model aliases", params(AliasQuery),
    responses((status = 200, description = "The aliases.", body = Vec<AliasView>)),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn list_aliases(State(state): State<AppState>, Query(q): Query<AliasQuery>) -> R<Json<Vec<AliasView>>> {
    let search = search_text(q.q.as_deref());
    let (aliases, deployments, routers) =
        tokio::try_join!(db::search_aliases(&state.db, search.as_deref()), db::list_deployments(&state.db), db::list_routers(&state.db))?;
    Ok(Json(aliases.into_iter().map(|a| alias_view(a, &deployments, &routers)).collect()))
}

/// Create model alias
///
/// A model name that serves the model of a deployment or auto router, named by its id so the
/// alias follows a rename. Keys reach it by its own name in `allowed_models`, and by their tags
/// through the target's deployments or router.
#[utoipa::path(post, path = "/aliases", tag = "Model aliases", request_body = AliasInput,
    responses(
        (status = 201, description = "The new alias.", body = AliasView),
        (status = 400, description = "Invalid alias, or the model name is taken.", body = crate::openapi::AdminError),
        (status = 409, description = "An alias with that name exists.", body = crate::openapi::AdminError),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn create_alias(
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

/// Update model alias
///
/// A renamed alias is followed by keys and its model metadata.
#[utoipa::path(put, path = "/aliases/{id}", tag = "Model aliases", params(("id" = Uuid, Path)),
    request_body = AliasInput,
    responses(
        (status = 200, description = "The alias.", body = AliasView),
        (status = 400, description = "Invalid alias, or the model name is taken.", body = crate::openapi::AdminError),
        (status = 404, description = "No such alias.", body = crate::openapi::AdminError),
        (status = 409, description = "Another alias has that name.", body = crate::openapi::AdminError),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn update_alias(
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

/// Delete model alias
#[utoipa::path(delete, path = "/aliases/{id}", tag = "Model aliases", params(("id" = Uuid, Path)),
    responses((status = 204, description = "Deleted."), (status = 404, description = "No such alias.", body = crate::openapi::AdminError)),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn delete_alias(State(state): State<AppState>, Extension(actor): Extension<Actor>, Path(id): Path<Uuid>) -> R<StatusCode> {
    let existing = db::get_alias(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    if !db::delete_alias(&state.db, id).await? {
        return Err(AdminError::not_found());
    }
    broadcast_reload(&state, "alias deleted").await;
    tracing::info!(alias = %id, "alias deleted");
    audit::Event::new("alias", "delete").on(id, existing.model_name).record(&state, &actor).await;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::admin::deployments::{DeploymentInput, to_row};

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
}
