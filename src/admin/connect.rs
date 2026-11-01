//! Admin API for MCP clients signing in to the gateway (see `mcp_auth`): the console shows a
//! waiting request, and the person approves it for one of the keys they may use, or denies it.
//! Each key lists the clients it lets in, and any of them can be revoked.

use axum::{
    Extension, Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{R, check_sees_key};
use crate::{
    admin_auth::Access,
    audit::{self, Actor},
    db,
    error::AdminError,
    mcp_auth::{self, ClientGrant, Pending},
    state::AppState,
};

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct ConnectRequest {
    /// The client's metadata document URL, and the host it is on: who vouches for the client.
    client_id: String,
    client_host: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_uri: Option<String>,
    /// Where the person goes back to, and its host.
    redirect_uri: String,
    redirect_host: String,
    /// It returns to a program on the person's computer: any program could claim to be the client.
    redirect_local: bool,
    /// It returns to another host than the client's own.
    redirect_elsewhere: bool,
    /// The MCP endpoint the client will use.
    resource: String,
}

fn host(url: &str) -> String {
    reqwest::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default()
}

async fn pending(state: &AppState, id: &str) -> R<Pending> {
    let saved = state.cache.peek(&mcp_auth::request_key(id)).await.map_err(|e| AdminError::unavailable(e.to_string()))?;
    saved.and_then(|s| serde_json::from_str(&s).ok()).ok_or_else(|| {
        AdminError(StatusCode::NOT_FOUND, "this sign-in has expired or was answered already; start it again in the app".into())
    })
}

/// Takes the request: it is answered once.
async fn take(state: &AppState, id: &str) -> R<Pending> {
    let saved = state.cache.take(&mcp_auth::request_key(id)).await.map_err(|e| AdminError::unavailable(e.to_string()))?;
    saved.and_then(|s| serde_json::from_str(&s).ok()).ok_or_else(|| {
        AdminError(StatusCode::NOT_FOUND, "this sign-in has expired or was answered already; start it again in the app".into())
    })
}

/// `GET /admin/api/connect/{id}`: a waiting sign-in, as the console shows it.
pub(crate) async fn get_request(State(state): State<AppState>, Path(id): Path<String>) -> R<Json<ConnectRequest>> {
    let p = pending(&state, &id).await?;
    let (client_host, redirect_host) = (host(&p.client_id), host(&p.redirect_uri));
    let redirect_local = matches!(redirect_host.as_str(), "localhost" | "127.0.0.1" | "[::1]" | "::1");
    Ok(Json(ConnectRequest {
        redirect_elsewhere: !redirect_local && redirect_host != client_host,
        client_id: p.client_id,
        client_host,
        client_name: p.client_name,
        client_uri: p.client_uri,
        redirect_uri: p.redirect_uri,
        redirect_host,
        redirect_local,
        resource: p.resource_url,
    }))
}

#[derive(Deserialize, utoipa::ToSchema)]
pub(crate) struct ApproveInput {
    /// The key the client acts as: one the person may use, with the inference scope.
    key_id: Uuid,
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct ConnectAnswer {
    /// Where to send the browser: back to the client.
    redirect: String,
}

/// `POST /admin/api/connect/{id}`: approves the sign-in for a key.
pub(crate) async fn approve(
    State(state): State<AppState>,
    Extension(access): Extension<Access>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
    Json(i): Json<ApproveInput>,
) -> R<Json<ConnectAnswer>> {
    let key = db::get_key(&state.db, i.key_id).await?.ok_or_else(AdminError::not_found)?;
    check_sees_key(&access, &key)?;
    crate::auth::key_by_id(&state, key.id).map_err(|e| AdminError::bad_request(format!("this key cannot be used: {}", e.message)))?;
    let p = take(&state, &id).await?;
    let (client, resource) = (p.client_name.clone().unwrap_or_else(|| p.client_id.clone()), p.resource.clone());
    let by = actor.name.clone().unwrap_or_else(|| actor.kind.to_string());
    let redirect = mcp_auth::approve(&state, p, key.id, Some(by)).await.map_err(AdminError::unavailable)?;
    audit::Event::new("key", "connect_app")
        .on(key.id, &key.name)
        .details(serde_json::json!({ "client": client, "resource": resource }))
        .record(&state, &actor)
        .await;
    Ok(Json(ConnectAnswer { redirect }))
}

/// `POST /admin/api/connect/{id}/deny`.
pub(crate) async fn deny(State(state): State<AppState>, Path(id): Path<String>) -> R<Json<ConnectAnswer>> {
    let p = take(&state, &id).await?;
    Ok(Json(ConnectAnswer { redirect: mcp_auth::denied(&p) }))
}

/// `GET /admin/api/keys/{id}/apps`: the MCP clients signed in as this key.
pub(crate) async fn list_apps(
    State(state): State<AppState>,
    Extension(access): Extension<Access>,
    Path(id): Path<Uuid>,
) -> R<Json<Vec<ClientGrant>>> {
    let key = db::get_key(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    check_sees_key(&access, &key)?;
    Ok(Json(db::list_client_grants(&state.db, id).await?))
}

/// `DELETE /admin/api/keys/{id}/apps/{grant}`: signs the client out. Other replicas stop taking
/// its access token within a minute.
pub(crate) async fn revoke_app(
    State(state): State<AppState>,
    Extension(access): Extension<Access>,
    Extension(actor): Extension<Actor>,
    Path((id, grant)): Path<(Uuid, Uuid)>,
) -> R<StatusCode> {
    let key = db::get_key(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    check_sees_key(&access, &key)?;
    let g = db::delete_client_grant(&state.db, id, grant).await?.ok_or_else(AdminError::not_found)?;
    state.mcp_auth.forget_tokens();
    let client = g.client_name.unwrap_or(g.client_id);
    audit::Event::new("key", "disconnect_app")
        .on(id, &key.name)
        .details(serde_json::json!({ "client": client }))
        .record(&state, &actor)
        .await;
    Ok(StatusCode::NO_CONTENT)
}
