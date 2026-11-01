//! MCP servers, A2A agents and tool routers.

use std::collections::BTreeMap;

use sqlx::{PgExecutor, PgPool, types::Json};
use uuid::Uuid;

use crate::{
    a2a::A2aAgentRow,
    mcp::{
        McpServerRow,
        router::{ToolRoute, ToolRouterRow},
    },
    oauth::OAuthConfig,
};

pub async fn list_mcp_servers(pool: &PgPool) -> sqlx::Result<Vec<McpServerRow>> {
    sqlx::query_as!(
        McpServerRow,
        r#"SELECT id, name, description, url, auth, auth_header, secret, oauth AS "oauth: Json<OAuthConfig>", tags, project_ids, timeout_secs,
                  enabled, allowed_tools, denied_tools, price_per_call, tool_prices AS "tool_prices: Json<BTreeMap<String, f64>>", created_at, updated_at
           FROM mcp_servers ORDER BY name"#
    )
    .fetch_all(pool)
    .await
}

pub async fn get_mcp_server(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<McpServerRow>> {
    sqlx::query_as!(
        McpServerRow,
        r#"SELECT id, name, description, url, auth, auth_header, secret, oauth AS "oauth: Json<OAuthConfig>", tags, project_ids, timeout_secs,
                  enabled, allowed_tools, denied_tools, price_per_call, tool_prices AS "tool_prices: Json<BTreeMap<String, f64>>", created_at, updated_at
           FROM mcp_servers WHERE id = $1"#,
        id
    )
    .fetch_optional(pool)
    .await
}

pub async fn insert_mcp_server(db: impl PgExecutor<'_>, s: &McpServerRow) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO mcp_servers (id, name, description, url, auth, auth_header, secret, tags, project_ids, timeout_secs, enabled, allowed_tools,
             denied_tools, price_per_call, tool_prices, oauth, created_at, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, now(), now())",
        s.id,
        s.name,
        s.description,
        s.url,
        s.auth,
        s.auth_header,
        s.secret,
        &s.tags,
        &s.project_ids,
        s.timeout_secs,
        s.enabled,
        &s.allowed_tools,
        &s.denied_tools,
        s.price_per_call,
        &s.tool_prices as &Json<BTreeMap<String, f64>>,
        &s.oauth as &Option<Json<OAuthConfig>>,
    )
    .execute(db)
    .await?;
    Ok(())
}

pub async fn update_mcp_server(db: impl PgExecutor<'_>, s: &McpServerRow) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE mcp_servers SET name = $2, description = $3, url = $4, auth = $5, auth_header = $6, secret = $7, tags = $8,
             project_ids = $9, timeout_secs = $10, enabled = $11, allowed_tools = $12, denied_tools = $13, price_per_call = $14,
             tool_prices = $15, oauth = $16, updated_at = now()
         WHERE id = $1",
        s.id,
        s.name,
        s.description,
        s.url,
        s.auth,
        s.auth_header,
        s.secret,
        &s.tags,
        &s.project_ids,
        s.timeout_secs,
        s.enabled,
        &s.allowed_tools,
        &s.denied_tools,
        s.price_per_call,
        &s.tool_prices as &Json<BTreeMap<String, f64>>,
        &s.oauth as &Option<Json<OAuthConfig>>,
    )
    .execute(db)
    .await?;
    Ok(())
}

/// Also deletes the keys' tokens for it.
pub async fn delete_mcp_server(pool: &PgPool, id: Uuid) -> sqlx::Result<bool> {
    let mut tx = pool.begin().await?;
    sqlx::query!("DELETE FROM oauth_tokens WHERE upstream_kind = 'mcp' AND upstream_id = $1", id).execute(&mut *tx).await?;
    let r = sqlx::query!("DELETE FROM mcp_servers WHERE id = $1", id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(r.rows_affected() > 0)
}

pub async fn list_a2a_agents(pool: &PgPool) -> sqlx::Result<Vec<A2aAgentRow>> {
    sqlx::query_as!(
        A2aAgentRow,
        r#"SELECT id, name, description, card_url, auth, auth_header, secret, oauth AS "oauth: Json<OAuthConfig>", tags, project_ids, timeout_secs,
                  price_per_message, enabled, created_at, updated_at
           FROM a2a_agents ORDER BY name"#
    )
    .fetch_all(pool)
    .await
}

pub async fn get_a2a_agent(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<A2aAgentRow>> {
    sqlx::query_as!(
        A2aAgentRow,
        r#"SELECT id, name, description, card_url, auth, auth_header, secret, oauth AS "oauth: Json<OAuthConfig>", tags, project_ids, timeout_secs,
                  price_per_message, enabled, created_at, updated_at
           FROM a2a_agents WHERE id = $1"#,
        id
    )
    .fetch_optional(pool)
    .await
}

pub async fn insert_a2a_agent(db: impl PgExecutor<'_>, a: &A2aAgentRow) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO a2a_agents (id, name, description, card_url, auth, auth_header, secret, tags, project_ids, timeout_secs, price_per_message,
             enabled, oauth, created_at, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, now(), now())",
        a.id,
        a.name,
        a.description,
        a.card_url,
        a.auth,
        a.auth_header,
        a.secret,
        &a.tags,
        &a.project_ids,
        a.timeout_secs,
        a.price_per_message,
        a.enabled,
        &a.oauth as &Option<Json<OAuthConfig>>,
    )
    .execute(db)
    .await?;
    Ok(())
}

pub async fn update_a2a_agent(db: impl PgExecutor<'_>, a: &A2aAgentRow) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE a2a_agents SET name = $2, description = $3, card_url = $4, auth = $5, auth_header = $6, secret = $7, tags = $8,
             project_ids = $9, timeout_secs = $10, price_per_message = $11, enabled = $12, oauth = $13, updated_at = now()
         WHERE id = $1",
        a.id,
        a.name,
        a.description,
        a.card_url,
        a.auth,
        a.auth_header,
        a.secret,
        &a.tags,
        &a.project_ids,
        a.timeout_secs,
        a.price_per_message,
        a.enabled,
        &a.oauth as &Option<Json<OAuthConfig>>,
    )
    .execute(db)
    .await?;
    Ok(())
}

/// Also deletes the keys' tokens for it.
pub async fn delete_a2a_agent(pool: &PgPool, id: Uuid) -> sqlx::Result<bool> {
    let mut tx = pool.begin().await?;
    sqlx::query!("DELETE FROM oauth_tokens WHERE upstream_kind = 'a2a' AND upstream_id = $1", id).execute(&mut *tx).await?;
    let r = sqlx::query!("DELETE FROM a2a_agents WHERE id = $1", id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(r.rows_affected() > 0)
}

pub async fn list_tool_routers(pool: &PgPool) -> sqlx::Result<Vec<ToolRouterRow>> {
    sqlx::query_as!(
        ToolRouterRow,
        r#"SELECT id, name, description, decision_model, instructions, routes AS "routes: Json<Vec<ToolRoute>>", threshold, max_routes, enabled,
                  created_at, updated_at
           FROM tool_routers ORDER BY name"#
    )
    .fetch_all(pool)
    .await
}

pub async fn get_tool_router(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<ToolRouterRow>> {
    sqlx::query_as!(
        ToolRouterRow,
        r#"SELECT id, name, description, decision_model, instructions, routes AS "routes: Json<Vec<ToolRoute>>", threshold, max_routes, enabled,
                  created_at, updated_at
           FROM tool_routers WHERE id = $1"#,
        id
    )
    .fetch_optional(pool)
    .await
}

pub async fn insert_tool_router(db: impl PgExecutor<'_>, r: &ToolRouterRow) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO tool_routers (id, name, description, decision_model, instructions, routes, threshold, max_routes, enabled, created_at, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, now(), now())",
        r.id,
        r.name,
        r.description,
        r.decision_model,
        r.instructions,
        &r.routes as &Json<Vec<ToolRoute>>,
        r.threshold,
        r.max_routes,
        r.enabled,
    )
    .execute(db)
    .await?;
    Ok(())
}

pub async fn update_tool_router(db: impl PgExecutor<'_>, r: &ToolRouterRow) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE tool_routers SET name = $2, description = $3, decision_model = $4, instructions = $5, routes = $6, threshold = $7,
             max_routes = $8, enabled = $9, updated_at = now()
         WHERE id = $1",
        r.id,
        r.name,
        r.description,
        r.decision_model,
        r.instructions,
        &r.routes as &Json<Vec<ToolRoute>>,
        r.threshold,
        r.max_routes,
        r.enabled,
    )
    .execute(db)
    .await?;
    Ok(())
}

pub async fn delete_tool_router(pool: &PgPool, id: Uuid) -> sqlx::Result<bool> {
    Ok(sqlx::query!("DELETE FROM tool_routers WHERE id = $1", id).execute(pool).await?.rows_affected() > 0)
}
