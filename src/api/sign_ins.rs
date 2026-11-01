//! `/v1/oauth/sign-ins`: an application's view of its tenants' and users' sign-ins to MCP servers
//! and A2A agents that keep a sign-in per tenant or user (`sign_in_by`). The request's labels say
//! whose: an application lists what the tenant or user has signed in to, gets them a sign-in link,
//! and signs them out.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    auth::ClientKey,
    db,
    error::{Dialect, GatewayError},
    labels::ClientLabels,
    model::VirtualKey,
    oauth::{self, AuthError, Client, Kind, Ticket, UpstreamAuth, Who},
    state::AppState,
};

/// An upstream that keeps a sign-in per tenant or user, for the request's labels.
#[derive(Serialize, utoipa::ToSchema)]
pub struct SignInStatus {
    /// `mcp` or `a2a`.
    kind: &'static str,
    name: String,
    /// The labels its sign-ins are told apart by.
    sign_in_by: Vec<String>,
    /// Whether the request's tenant or user has signed in.
    signed_in: bool,
    /// The label the request lacks for this upstream, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    missing_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    signed_in_at: Option<DateTime<Utc>>,
}

/// The upstreams `key` may use that keep a sign-in per tenant or user.
fn upstreams(state: &AppState, key: &VirtualKey) -> Vec<(Kind, Arc<Client>)> {
    let snap = state.registry.snapshot();
    let mcp = snap.mcp_servers.values().filter(|s| s.allows(key)).map(|s| (Kind::Mcp, &s.auth));
    let a2a = snap.a2a_agents.values().filter(|a| a.allows(key)).map(|a| (Kind::A2a, &a.auth));
    let mut out: Vec<(Kind, Arc<Client>)> = mcp
        .chain(a2a)
        .filter_map(|(kind, auth)| match auth {
            UpstreamAuth::PerKey(c) if !c.config.sign_in_by.is_empty() => Some((kind, c.clone())),
            _ => None,
        })
        .collect();
    out.sort_by(|a, b| (a.0.as_str(), &a.1.name).cmp(&(b.0.as_str(), &b.1.name)));
    out
}

/// The upstream `kind`/`name`, if the key may use it and it keeps a sign-in per tenant or user.
fn upstream(state: &AppState, key: &VirtualKey, kind: &str, name: &str) -> Result<Arc<Client>, GatewayError> {
    let kind = Kind::parse(kind).ok_or_else(|| GatewayError::invalid("kind is mcp or a2a"))?;
    upstreams(state, key).into_iter().find(|(k, c)| *k == kind && c.name == name).map(|(_, c)| c).ok_or_else(|| {
        GatewayError::not_found(format!("no {} '{name}' that this key may use and that keeps a sign-in per tenant or user", kind.as_str()))
    })
}

fn refuse(e: GatewayError) -> Response {
    e.into_response_for(Dialect::OpenAi)
}

/// List tenant's or user's sign-ins
///
/// The MCP servers and A2A agents the key may use that keep a sign-in per tenant or user, and
/// whether the tenant or user in the request's `x-gateway-labels` has signed in to each.
#[utoipa::path(get, path = "/v1/oauth/sign-ins", operation_id = "list_sign_ins", tag = "Upstream OAuth",
    responses(
        (status = 200, description = "The upstreams.", body = SignInList),
        (status = 401, description = "Missing, invalid, disabled or expired key.", body = crate::openapi::OpenAiError),
    ),
    security(("virtual_key" = []), ("virtual_key_header" = [])))]
pub async fn list(State(state): State<AppState>, ClientKey(key): ClientKey, ClientLabels(labels): ClientLabels) -> Response {
    let mut out = Vec::new();
    for (kind, c) in upstreams(&state, &key) {
        let (signed_in_at, missing_label) = match c.slot(Who { key: key.id, labels: &labels }) {
            Ok(slot) => match db::get_sign_in(&state.db, c.sign_in(&slot)).await {
                Ok(row) => (row.map(|r| r.created_at), None),
                Err(e) => return refuse(GatewayError::internal(e.to_string())),
            },
            Err(AuthError::MissingLabel { label, .. }) => (None, Some(label)),
            Err(e) => return refuse(e.into_gateway()),
        };
        out.push(SignInStatus {
            kind: kind.as_str(),
            name: c.name.clone(),
            sign_in_by: c.config.sign_in_by.clone(),
            signed_in: signed_in_at.is_some(),
            missing_label,
            signed_in_at,
        });
    }
    Json(SignInList { data: out }).into_response()
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct LinkInput {
    /// `mcp` or `a2a`.
    kind: String,
    /// The MCP server's or A2A agent's name.
    name: String,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct Link {
    /// Where the tenant or user signs in, in their browser. It works once they open it, and only
    /// in the browser that opened it.
    url: String,
    expires_at: DateTime<Utc>,
}

/// Get a sign-in link
///
/// A link that signs the tenant or user in the request's `x-gateway-labels` in to the upstream,
/// to hand to them. It works for 15 minutes, and only in the browser that opens it.
#[utoipa::path(post, path = "/v1/oauth/sign-ins", operation_id = "sign_in_link", tag = "Upstream OAuth", request_body = LinkInput,
    responses(
        (status = 200, description = "The link.", body = Link),
        (status = 400, description = "The request lacks a label the upstream keeps sign-ins by.", body = crate::openapi::OpenAiError),
        (status = 404, description = "No such upstream for this key that keeps a sign-in per tenant or user.", body = crate::openapi::OpenAiError),
    ),
    security(("virtual_key" = []), ("virtual_key_header" = [])))]
pub async fn link(
    State(state): State<AppState>,
    ClientKey(key): ClientKey,
    ClientLabels(labels): ClientLabels,
    headers: HeaderMap,
    uri: Uri,
    Json(i): Json<LinkInput>,
) -> Response {
    let result = (|| {
        let c = upstream(&state, &key, &i.kind, &i.name)?;
        let slot = c.slot(Who { key: key.id, labels: &labels }).map_err(AuthError::into_gateway)?;
        let ticket = Ticket::new(&c, &slot);
        let expires_at = Utc.timestamp_opt(ticket.expires, 0).single().unwrap_or_else(Utc::now);
        Ok::<_, GatewayError>(Link { url: ticket.url(&crate::a2a::origin(&headers, &uri), &state.cfg.secret_box), expires_at })
    })();
    match result {
        Ok(link) => Json(link).into_response(),
        Err(e) => refuse(e),
    }
}

/// Sign tenant or user out
///
/// Deletes the sign-in of the tenant or user in the request's `x-gateway-labels`.
#[utoipa::path(delete, path = "/v1/oauth/sign-ins/{kind}/{name}", tag = "Upstream OAuth",
    params(("kind" = String, Path, description = "`mcp` or `a2a`."), ("name" = String, Path)),
    responses(
        (status = 204, description = "Signed out."),
        (status = 404, description = "They had not signed in.", body = crate::openapi::OpenAiError),
    ),
    security(("virtual_key" = []), ("virtual_key_header" = [])))]
pub async fn sign_out(
    State(state): State<AppState>,
    ClientKey(key): ClientKey,
    ClientLabels(labels): ClientLabels,
    Path((kind, name)): Path<(String, String)>,
) -> Response {
    let c = match upstream(&state, &key, &kind, &name) {
        Ok(c) => c,
        Err(e) => return refuse(e),
    };
    let slot = match c.slot(Who { key: key.id, labels: &labels }) {
        Ok(s) => s,
        Err(e) => return refuse(e.into_gateway()),
    };
    match db::delete_oauth_token(&state.db, c.sign_in(&slot)).await {
        Ok(true) => {
            oauth::forget(&state, c.kind, c.upstream, &slot);
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => refuse(GatewayError::not_found(format!("no sign-in to '{name}' for {}", slot.labels))),
        Err(e) => refuse(GatewayError::internal(e.to_string())),
    }
}

/// What `GET /v1/oauth/sign-ins` answers.
#[derive(Serialize, utoipa::ToSchema)]
pub struct SignInList {
    data: Vec<SignInStatus>,
}
