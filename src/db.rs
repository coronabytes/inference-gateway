//! Postgres access. Every query is checked against the schema at compile time by sqlx's
//! macros; `.sqlx/` holds the offline metadata so builds need no database
//! (regenerate with `cargo sqlx prepare` after changing a query or migration).

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{Connection, PgExecutor, PgPool, migrate::Migrator, postgres::PgPoolOptions, types::Json};
use uuid::Uuid;

use crate::{
    a2a::A2aAgentRow,
    accounting::RequestRecord,
    capture::Stored,
    guardrails::{GuardrailConfig, GuardrailPrices, GuardrailRow},
    mcp::{
        McpServerRow,
        router::{ToolRoute, ToolRouterRow},
    },
    mcp_auth::ClientGrant,
    model::{
        AdminUserRow, AliasRow, CredentialRow, CredentialSettings, DeploymentConfig, DeploymentRow, KeyScope, OidcSettingsRow, PersonRole,
        ProjectRow, PromptLogging, Route, RouterRow, VirtualKey,
    },
    oauth::OAuthConfig,
    prices,
};

/// Postgres error codes the admin API turns into 409 responses.
pub fn is_foreign_key_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(d) if d.code().as_deref() == Some("23503"))
}

pub fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(d) if d.code().as_deref() == Some("23505"))
}

pub async fn connect(url: &str, max_connections: u32) -> Result<PgPool, sqlx::Error> {
    let pool =
        PgPoolOptions::new().max_connections(max_connections).acquire_timeout(std::time::Duration::from_secs(10)).connect(url).await?;
    // sqlx takes a Postgres advisory lock, so concurrent replicas migrate safely.
    MIGRATOR.run(&pool).await?;
    ensure_log_partitions(&pool).await?;
    Ok(pool)
}

static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// Days of request log partitions kept ready ahead of today (see migration 0016).
const LOG_PARTITIONS_AHEAD: i32 = 7;

/// Creates the request log's daily partitions from today to `LOG_PARTITIONS_AHEAD` days on, so
/// entries always have one to go to. Run at startup and with every retention run.
async fn ensure_log_partitions(db: impl PgExecutor<'_>) -> sqlx::Result<()> {
    sqlx::query_scalar!("SELECT gw_request_log_partitions($1)", LOG_PARTITIONS_AHEAD).fetch_one(db).await?;
    Ok(())
}

pub async fn list_deployments(pool: &PgPool) -> sqlx::Result<Vec<DeploymentRow>> {
    sqlx::query_as!(
        DeploymentRow,
        r#"SELECT id, model_name, mode, provider, upstream_model, config AS "config: Json<DeploymentConfig>",
                  credential_id, weight, priority, rpm_limit, tpm_limit, timeout_secs, input_cost_per_mtok,
                  output_cost_per_mtok, cache_read_cost_per_mtok, cache_write_cost_per_mtok, long_context_threshold,
                  long_input_cost_per_mtok, long_output_cost_per_mtok, long_cache_read_cost_per_mtok,
                  long_cache_write_cost_per_mtok, cost_per_k_searches, base_model, tags, project_ids, enabled,
                  created_at, updated_at
           FROM deployments ORDER BY model_name, priority, created_at"#
    )
    .fetch_all(pool)
    .await
}

/// A list filter on a text array column (tags, projects): rows holding any of `any`, or, with
/// `none`, rows holding nothing. The default does not narrow the list.
#[derive(Default)]
pub struct AnyOf<T> {
    /// `None`: no filter.
    pub any: Option<Vec<T>>,
    pub none: bool,
}

/// The deployments the console's model list shows: `search` is part of the model name (lower
/// case); the other filters keep deployments with this mode, tags, credentials, projects or ids.
pub async fn search_deployments(
    pool: &PgPool,
    search: Option<&str>,
    mode: Option<&str>,
    tags: &AnyOf<String>,
    credentials: Option<&[Uuid]>,
    projects: &AnyOf<Uuid>,
    ids: Option<&[Uuid]>,
) -> sqlx::Result<Vec<DeploymentRow>> {
    sqlx::query_as!(
        DeploymentRow,
        r#"SELECT id, model_name, mode, provider, upstream_model, config AS "config: Json<DeploymentConfig>",
                  credential_id, weight, priority, rpm_limit, tpm_limit, timeout_secs, input_cost_per_mtok,
                  output_cost_per_mtok, cache_read_cost_per_mtok, cache_write_cost_per_mtok, long_context_threshold,
                  long_input_cost_per_mtok, long_output_cost_per_mtok, long_cache_read_cost_per_mtok,
                  long_cache_write_cost_per_mtok, cost_per_k_searches, base_model, tags, project_ids, enabled,
                  created_at, updated_at
           FROM deployments
           WHERE ($1::text IS NULL OR strpos(lower(model_name), $1) > 0)
             AND ($2::text IS NULL OR mode = $2)
             AND ($3::text[] IS NULL OR tags && $3 OR ($4 AND cardinality(tags) = 0))
             AND ($5::uuid[] IS NULL OR credential_id = ANY($5))
             AND ($6::uuid[] IS NULL OR project_ids && $6 OR ($7 AND cardinality(project_ids) = 0))
             AND ($8::uuid[] IS NULL OR id = ANY($8))
           ORDER BY model_name, priority, created_at"#,
        search,
        mode,
        tags.any.as_deref(),
        tags.none,
        credentials,
        projects.any.as_deref(),
        projects.none,
        ids,
    )
    .fetch_all(pool)
    .await
}

pub async fn get_deployment(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<DeploymentRow>> {
    sqlx::query_as!(
        DeploymentRow,
        r#"SELECT id, model_name, mode, provider, upstream_model, config AS "config: Json<DeploymentConfig>",
                  credential_id, weight, priority, rpm_limit, tpm_limit, timeout_secs, input_cost_per_mtok,
                  output_cost_per_mtok, cache_read_cost_per_mtok, cache_write_cost_per_mtok, long_context_threshold,
                  long_input_cost_per_mtok, long_output_cost_per_mtok, long_cache_read_cost_per_mtok,
                  long_cache_write_cost_per_mtok, cost_per_k_searches, base_model, tags, project_ids, enabled,
                  created_at, updated_at
           FROM deployments WHERE id = $1"#,
        id
    )
    .fetch_optional(pool)
    .await
}

pub async fn upsert_deployment(db: impl PgExecutor<'_>, d: &DeploymentRow) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO deployments (id, model_name, mode, provider, upstream_model, config, credential_id, weight, priority,
             rpm_limit, tpm_limit, timeout_secs, input_cost_per_mtok, output_cost_per_mtok,
             cache_read_cost_per_mtok, cache_write_cost_per_mtok, long_context_threshold, long_input_cost_per_mtok,
             long_output_cost_per_mtok, long_cache_read_cost_per_mtok, long_cache_write_cost_per_mtok,
             cost_per_k_searches, base_model, tags, project_ids, enabled, created_at, updated_at)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23,$24,$25,$26,$27,now())
         ON CONFLICT (id) DO UPDATE SET
             model_name = EXCLUDED.model_name, mode = EXCLUDED.mode, provider = EXCLUDED.provider,
             upstream_model = EXCLUDED.upstream_model, config = EXCLUDED.config,
             credential_id = EXCLUDED.credential_id, weight = EXCLUDED.weight, priority = EXCLUDED.priority,
             rpm_limit = EXCLUDED.rpm_limit, tpm_limit = EXCLUDED.tpm_limit,
             timeout_secs = EXCLUDED.timeout_secs, input_cost_per_mtok = EXCLUDED.input_cost_per_mtok,
             output_cost_per_mtok = EXCLUDED.output_cost_per_mtok,
             cache_read_cost_per_mtok = EXCLUDED.cache_read_cost_per_mtok,
             cache_write_cost_per_mtok = EXCLUDED.cache_write_cost_per_mtok,
             long_context_threshold = EXCLUDED.long_context_threshold,
             long_input_cost_per_mtok = EXCLUDED.long_input_cost_per_mtok,
             long_output_cost_per_mtok = EXCLUDED.long_output_cost_per_mtok,
             long_cache_read_cost_per_mtok = EXCLUDED.long_cache_read_cost_per_mtok,
             long_cache_write_cost_per_mtok = EXCLUDED.long_cache_write_cost_per_mtok,
             cost_per_k_searches = EXCLUDED.cost_per_k_searches,
             base_model = EXCLUDED.base_model, tags = EXCLUDED.tags, project_ids = EXCLUDED.project_ids,
             enabled = EXCLUDED.enabled,
             updated_at = now()",
        d.id,
        d.model_name,
        d.mode,
        d.provider,
        d.upstream_model,
        &d.config as &Json<DeploymentConfig>,
        d.credential_id,
        d.weight,
        d.priority,
        d.rpm_limit,
        d.tpm_limit,
        d.timeout_secs,
        d.input_cost_per_mtok,
        d.output_cost_per_mtok,
        d.cache_read_cost_per_mtok,
        d.cache_write_cost_per_mtok,
        d.long_context_threshold,
        d.long_input_cost_per_mtok,
        d.long_output_cost_per_mtok,
        d.long_cache_read_cost_per_mtok,
        d.long_cache_write_cost_per_mtok,
        d.cost_per_k_searches,
        d.base_model,
        &d.tags,
        &d.project_ids,
        d.enabled,
        d.created_at,
    )
    .execute(db)
    .await?;
    Ok(())
}

/// Give a deployment the catalog's prices and, where it has none, the entry as its base model.
/// Only while it is unchanged since `seen` (its `updated_at`), so an edit saved meanwhile wins;
/// false when it was changed or deleted.
pub async fn accept_catalog_prices(pool: &PgPool, id: Uuid, seen: DateTime<Utc>, key: &str, p: &prices::Prices) -> sqlx::Result<bool> {
    let done = sqlx::query!(
        "UPDATE deployments SET input_cost_per_mtok = $3, output_cost_per_mtok = $4, cache_read_cost_per_mtok = $5,
             cache_write_cost_per_mtok = $6, long_context_threshold = $7, long_input_cost_per_mtok = $8,
             long_output_cost_per_mtok = $9, long_cache_read_cost_per_mtok = $10, long_cache_write_cost_per_mtok = $11,
             cost_per_k_searches = $12, base_model = COALESCE(base_model, $13), updated_at = now()
         WHERE id = $1 AND updated_at = $2",
        id,
        seen,
        p.input_cost_per_mtok,
        p.output_cost_per_mtok,
        p.cache_read_cost_per_mtok,
        p.cache_write_cost_per_mtok,
        p.long_context_threshold,
        p.long_input_cost_per_mtok,
        p.long_output_cost_per_mtok,
        p.long_cache_read_cost_per_mtok,
        p.long_cache_write_cost_per_mtok,
        p.cost_per_k_searches,
        key,
    )
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

pub async fn get_oidc_settings(pool: &PgPool) -> sqlx::Result<Option<OidcSettingsRow>> {
    sqlx::query_as!(
        OidcSettingsRow,
        "SELECT enabled, display_name, issuer_url, client_id, client_secret, public_url, roles_claim,
                allowed_role, user_role, updated_at
         FROM oidc_settings WHERE id = 1"
    )
    .fetch_optional(pool)
    .await
}

pub async fn save_oidc_settings(pool: &PgPool, s: &OidcSettingsRow) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO oidc_settings (id, enabled, display_name, issuer_url, client_id, client_secret, public_url,
             roles_claim, allowed_role, user_role, updated_at)
         VALUES (1, $1, $2, $3, $4, $5, $6, $7, $8, $9, now())
         ON CONFLICT (id) DO UPDATE SET
             enabled = EXCLUDED.enabled, display_name = EXCLUDED.display_name, issuer_url = EXCLUDED.issuer_url,
             client_id = EXCLUDED.client_id, client_secret = EXCLUDED.client_secret, public_url = EXCLUDED.public_url,
             roles_claim = EXCLUDED.roles_claim, allowed_role = EXCLUDED.allowed_role, user_role = EXCLUDED.user_role,
             updated_at = now()",
        s.enabled,
        s.display_name,
        s.issuer_url,
        s.client_id,
        s.client_secret,
        s.public_url,
        s.roles_claim,
        s.allowed_role,
        s.user_role,
    )
    .execute(pool)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------- gateway settings

pub struct GatewaySettingsRow {
    pub settings: serde_json::Value,
    pub updated_at: DateTime<Utc>,
}

pub async fn get_gateway_settings(pool: &PgPool) -> sqlx::Result<Option<GatewaySettingsRow>> {
    sqlx::query_as!(GatewaySettingsRow, "SELECT settings, updated_at FROM gateway_settings WHERE id = 1").fetch_optional(pool).await
}

pub async fn save_gateway_settings(pool: &PgPool, settings: &serde_json::Value) -> sqlx::Result<DateTime<Utc>> {
    sqlx::query_scalar!(
        "INSERT INTO gateway_settings (id, settings, updated_at) VALUES (1, $1, now())
         ON CONFLICT (id) DO UPDATE SET settings = EXCLUDED.settings, updated_at = now()
         RETURNING updated_at",
        settings,
    )
    .fetch_one(pool)
    .await
}

// ---------------------------------------------------------------- key rotation
//
// The sealed columns, read with a row lock so the rotation transaction sees them unchanged.

pub async fn sealed_credentials(db: impl PgExecutor<'_>) -> sqlx::Result<Vec<(Uuid, String, Vec<u8>)>> {
    let rows = sqlx::query!(r#"SELECT id, name, secrets AS "secrets!" FROM credentials WHERE secrets IS NOT NULL FOR UPDATE"#)
        .fetch_all(db)
        .await?;
    Ok(rows.into_iter().map(|r| (r.id, r.name, r.secrets)).collect())
}

pub async fn set_credential_secrets(db: impl PgExecutor<'_>, id: Uuid, secrets: &[u8]) -> sqlx::Result<()> {
    sqlx::query!("UPDATE credentials SET secrets = $1 WHERE id = $2", secrets, id).execute(db).await?;
    Ok(())
}

pub async fn sealed_oidc_client_secret(db: impl PgExecutor<'_>) -> sqlx::Result<Option<Vec<u8>>> {
    let row = sqlx::query!("SELECT client_secret FROM oidc_settings WHERE id = 1 FOR UPDATE").fetch_optional(db).await?;
    Ok(row.and_then(|r| r.client_secret))
}

pub async fn set_oidc_client_secret(db: impl PgExecutor<'_>, secret: &[u8]) -> sqlx::Result<()> {
    sqlx::query!("UPDATE oidc_settings SET client_secret = $1 WHERE id = 1", secret).execute(db).await?;
    Ok(())
}

/// Record a sign-in: creates the person on first sign-in, refreshes email, name and role
/// afterwards.
pub async fn upsert_admin_user(
    pool: &PgPool,
    issuer: &str,
    subject: &str,
    email: Option<&str>,
    name: Option<&str>,
    role: PersonRole,
) -> sqlx::Result<AdminUserRow> {
    sqlx::query_as!(
        AdminUserRow,
        r#"INSERT INTO admin_users (id, issuer, subject, email, name, created_at, last_login_at, role)
         VALUES ($1, $2, $3, $4, $5, now(), now(), $6)
         ON CONFLICT (issuer, subject) DO UPDATE SET email = EXCLUDED.email, name = EXCLUDED.name, last_login_at = now(),
             role = EXCLUDED.role
         RETURNING id, issuer, subject, email, name, created_at, last_login_at, role AS "role: PersonRole""#,
        Uuid::now_v7(),
        issuer,
        subject,
        email,
        name,
        role as PersonRole,
    )
    .fetch_one(pool)
    .await
}

pub async fn get_admin_user(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<AdminUserRow>> {
    sqlx::query_as!(
        AdminUserRow,
        r#"SELECT id, issuer, subject, email, name, created_at, last_login_at, role AS "role: PersonRole"
           FROM admin_users WHERE id = $1"#,
        id
    )
    .fetch_optional(pool)
    .await
}

pub async fn list_admin_users(pool: &PgPool) -> sqlx::Result<Vec<AdminUserRow>> {
    sqlx::query_as!(
        AdminUserRow,
        r#"SELECT id, issuer, subject, email, name, created_at, last_login_at, role AS "role: PersonRole"
           FROM admin_users ORDER BY last_login_at DESC"#
    )
    .fetch_all(pool)
    .await
}

pub async fn delete_admin_user(pool: &PgPool, id: Uuid) -> sqlx::Result<bool> {
    let r = sqlx::query!("DELETE FROM admin_users WHERE id = $1", id).execute(pool).await?;
    Ok(r.rows_affected() > 0)
}

/// The projects a user works in.
pub async fn user_projects(pool: &PgPool, user_id: Uuid) -> sqlx::Result<Vec<Uuid>> {
    sqlx::query_scalar!("SELECT project_id FROM project_members WHERE user_id = $1 ORDER BY project_id", user_id).fetch_all(pool).await
}

/// Every membership, as (user, project).
pub async fn list_project_members(pool: &PgPool) -> sqlx::Result<Vec<(Uuid, Uuid)>> {
    let rows = sqlx::query!("SELECT user_id, project_id FROM project_members").fetch_all(pool).await?;
    Ok(rows.into_iter().map(|r| (r.user_id, r.project_id)).collect())
}

/// Replaces the projects a user works in.
pub async fn set_user_projects(pool: &PgPool, user_id: Uuid, projects: &[Uuid]) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query!("DELETE FROM project_members WHERE user_id = $1", user_id).execute(&mut *tx).await?;
    sqlx::query!("INSERT INTO project_members (project_id, user_id) SELECT p, $2 FROM unnest($1::uuid[]) AS t(p)", projects, user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await
}

// ---------------------------------------------------------------- price catalog

/// Catalog entries whose key contains every term (lowercase), optionally limited to some
/// providers and modes, with the total number of matches. Alphabetical, or shortest keys first (best matches).
pub async fn search_prices(
    pool: &PgPool,
    terms: &[String],
    providers: Option<&[String]>,
    modes: Option<&[String]>,
    limit: i64,
    offset: i64,
    shortest_first: bool,
) -> sqlx::Result<(i64, Vec<(String, serde_json::Value)>)> {
    let rows = sqlx::query!(
        r#"SELECT key, data, count(*) OVER () AS "total!"
           FROM model_prices
           WHERE NOT EXISTS (SELECT 1 FROM unnest($1::text[]) AS t(term) WHERE strpos(lower(key), t.term) = 0)
             AND ($2::text[] IS NULL OR data->>'litellm_provider' = ANY($2))
             AND ($3::text[] IS NULL OR data->>'mode' = ANY($3))
           ORDER BY CASE WHEN $6 THEN length(key) END, key LIMIT $4 OFFSET $5"#,
        terms,
        providers,
        modes,
        limit,
        offset,
        shortest_first,
    )
    .fetch_all(pool)
    .await?;
    let total = rows.first().map_or(0, |r| r.total);
    Ok((total, rows.into_iter().map(|r| (r.key, r.data)).collect()))
}

/// Distinct providers and modes in the catalog, for the grid's filters.
pub async fn price_facets(pool: &PgPool) -> sqlx::Result<(Vec<String>, Vec<String>)> {
    let providers = sqlx::query_scalar!(
        r#"SELECT DISTINCT data->>'litellm_provider' AS "p!" FROM model_prices
           WHERE data->>'litellm_provider' IS NOT NULL ORDER BY 1"#
    )
    .fetch_all(pool)
    .await?;
    let modes =
        sqlx::query_scalar!(r#"SELECT DISTINCT data->>'mode' AS "m!" FROM model_prices WHERE data->>'mode' IS NOT NULL ORDER BY 1"#)
            .fetch_all(pool)
            .await?;
    Ok((providers, modes))
}

pub async fn get_prices(pool: &PgPool, keys: &[String]) -> sqlx::Result<Vec<(String, serde_json::Value)>> {
    let rows = sqlx::query!("SELECT key, data FROM model_prices WHERE key = ANY($1)", keys).fetch_all(pool).await?;
    Ok(rows.into_iter().map(|r| (r.key, r.data)).collect())
}

/// Up to `limit` entries whose key contains `term`, shortest keys first.
pub async fn similar_prices(pool: &PgPool, term: &str, limit: i64) -> sqlx::Result<Vec<(String, serde_json::Value)>> {
    let rows = sqlx::query!(
        "SELECT key, data FROM model_prices WHERE strpos(lower(key), lower($1)) > 0 ORDER BY length(key), key LIMIT $2",
        term,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| (r.key, r.data)).collect())
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct PriceImport {
    pub imported_at: DateTime<Utc>,
    pub source: String,
    pub total: i32,
    pub added: i32,
    pub updated: i32,
    pub removed: i32,
}

/// Make the catalog equal to `entries`: add new keys, update changed ones, remove the rest.
pub async fn replace_prices(pool: &PgPool, entries: Vec<(String, serde_json::Value)>, source: &str) -> sqlx::Result<PriceImport> {
    let (keys, data): (Vec<String>, Vec<serde_json::Value>) = entries.into_iter().unzip();
    let mut tx = pool.begin().await?;
    let removed = sqlx::query!("DELETE FROM model_prices WHERE key <> ALL($1)", &keys).execute(&mut *tx).await?.rows_affected();
    // Unchanged rows are skipped; xmax = 0 tells inserted rows from updated ones.
    let written = sqlx::query_scalar!(
        r#"INSERT INTO model_prices (key, data, updated_at)
           SELECT key, data, now() FROM unnest($1::text[], $2::jsonb[]) AS t(key, data)
           ON CONFLICT (key) DO UPDATE SET data = EXCLUDED.data, updated_at = now()
               WHERE model_prices.data IS DISTINCT FROM EXCLUDED.data
           RETURNING (xmax = 0) AS "inserted!""#,
        &keys,
        &data,
    )
    .fetch_all(&mut *tx)
    .await?;
    let added = written.iter().filter(|i| **i).count();
    let summary = PriceImport {
        imported_at: Utc::now(),
        source: source.to_string(),
        total: keys.len() as i32,
        added: added as i32,
        updated: (written.len() - added) as i32,
        removed: removed as i32,
    };
    sqlx::query!(
        "INSERT INTO model_price_imports (imported_at, source, total, added, updated, removed) VALUES ($1, $2, $3, $4, $5, $6)",
        summary.imported_at,
        summary.source,
        summary.total,
        summary.added,
        summary.updated,
        summary.removed,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(summary)
}

pub async fn last_price_import(pool: &PgPool) -> sqlx::Result<Option<PriceImport>> {
    sqlx::query_as!(
        PriceImport,
        "SELECT imported_at, source, total, added, updated, removed FROM model_price_imports ORDER BY id DESC LIMIT 1"
    )
    .fetch_optional(pool)
    .await
}

pub async fn list_routers(pool: &PgPool) -> sqlx::Result<Vec<RouterRow>> {
    sqlx::query_as!(
        RouterRow,
        r#"SELECT id, model_name, decision_model, instructions, routes AS "routes: Json<Vec<Route>>",
                  fallback_route, tags, enabled, sticky, created_at, updated_at
           FROM routers ORDER BY model_name"#
    )
    .fetch_all(pool)
    .await
}

/// The routers the console's model list shows: `search` is part of the model name (lower case).
pub async fn search_routers(pool: &PgPool, search: Option<&str>, tags: &AnyOf<String>) -> sqlx::Result<Vec<RouterRow>> {
    sqlx::query_as!(
        RouterRow,
        r#"SELECT id, model_name, decision_model, instructions, routes AS "routes: Json<Vec<Route>>",
                  fallback_route, tags, enabled, sticky, created_at, updated_at
           FROM routers
           WHERE ($1::text IS NULL OR strpos(lower(model_name), $1) > 0)
             AND ($2::text[] IS NULL OR tags && $2 OR ($3 AND cardinality(tags) = 0))
           ORDER BY model_name"#,
        search,
        tags.any.as_deref(),
        tags.none,
    )
    .fetch_all(pool)
    .await
}

pub async fn get_router(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<RouterRow>> {
    sqlx::query_as!(
        RouterRow,
        r#"SELECT id, model_name, decision_model, instructions, routes AS "routes: Json<Vec<Route>>",
                  fallback_route, tags, enabled, sticky, created_at, updated_at
           FROM routers WHERE id = $1"#,
        id
    )
    .fetch_optional(pool)
    .await
}

pub async fn upsert_router(db: impl PgExecutor<'_>, r: &RouterRow) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO routers (id, model_name, decision_model, instructions, routes, fallback_route, tags, enabled, sticky,
             created_at, updated_at)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,now())
         ON CONFLICT (id) DO UPDATE SET
             model_name = EXCLUDED.model_name, decision_model = EXCLUDED.decision_model,
             instructions = EXCLUDED.instructions, routes = EXCLUDED.routes,
             fallback_route = EXCLUDED.fallback_route, tags = EXCLUDED.tags, enabled = EXCLUDED.enabled, sticky = EXCLUDED.sticky,
             updated_at = now()",
        r.id,
        r.model_name,
        r.decision_model,
        r.instructions,
        &r.routes as &Json<Vec<Route>>,
        r.fallback_route,
        &r.tags,
        r.enabled,
        r.sticky,
        r.created_at,
    )
    .execute(db)
    .await?;
    Ok(())
}

pub async fn delete_router(pool: &PgPool, id: Uuid) -> sqlx::Result<bool> {
    let r = sqlx::query!("DELETE FROM routers WHERE id = $1", id).execute(pool).await?;
    Ok(r.rows_affected() > 0)
}

pub async fn list_aliases(pool: &PgPool) -> sqlx::Result<Vec<AliasRow>> {
    sqlx::query_as!(AliasRow, "SELECT id, model_name, target_id, enabled, created_at, updated_at FROM model_aliases ORDER BY model_name")
        .fetch_all(pool)
        .await
}

/// The aliases whose name, or whose target's model name, contains `search` (lower case).
pub async fn search_aliases(pool: &PgPool, search: Option<&str>) -> sqlx::Result<Vec<AliasRow>> {
    sqlx::query_as!(
        AliasRow,
        r#"SELECT a.id, a.model_name, a.target_id, a.enabled, a.created_at, a.updated_at
           FROM model_aliases a
           WHERE $1::text IS NULL OR strpos(lower(a.model_name), $1) > 0
              OR EXISTS (SELECT 1 FROM deployments d WHERE d.id = a.target_id AND strpos(lower(d.model_name), $1) > 0)
              OR EXISTS (SELECT 1 FROM routers r WHERE r.id = a.target_id AND strpos(lower(r.model_name), $1) > 0)
           ORDER BY a.model_name"#,
        search,
    )
    .fetch_all(pool)
    .await
}

pub async fn get_alias(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<AliasRow>> {
    sqlx::query_as!(AliasRow, "SELECT id, model_name, target_id, enabled, created_at, updated_at FROM model_aliases WHERE id = $1", id)
        .fetch_optional(pool)
        .await
}

pub async fn upsert_alias(db: impl PgExecutor<'_>, a: &AliasRow) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO model_aliases (id, model_name, target_id, enabled, created_at, updated_at)
         VALUES ($1,$2,$3,$4,$5,now())
         ON CONFLICT (id) DO UPDATE SET
             model_name = EXCLUDED.model_name, target_id = EXCLUDED.target_id, enabled = EXCLUDED.enabled,
             updated_at = now()",
        a.id,
        a.model_name,
        a.target_id,
        a.enabled,
        a.created_at,
    )
    .execute(db)
    .await?;
    Ok(())
}

pub async fn delete_alias(pool: &PgPool, id: Uuid) -> sqlx::Result<bool> {
    let r = sqlx::query!("DELETE FROM model_aliases WHERE id = $1", id).execute(pool).await?;
    Ok(r.rows_affected() > 0)
}

pub async fn delete_deployment(pool: &PgPool, id: Uuid) -> sqlx::Result<bool> {
    let r = sqlx::query!("DELETE FROM deployments WHERE id = $1", id).execute(pool).await?;
    Ok(r.rows_affected() > 0)
}

pub async fn list_credentials(pool: &PgPool) -> sqlx::Result<Vec<CredentialRow>> {
    sqlx::query_as!(
        CredentialRow,
        r#"SELECT id, name, provider, secrets, settings AS "settings: Json<CredentialSettings>", created_at, updated_at
           FROM credentials ORDER BY provider, name"#
    )
    .fetch_all(pool)
    .await
}

/// The credentials whose name contains `search` (lower case), of these providers.
pub async fn search_credentials(pool: &PgPool, search: Option<&str>, providers: Option<&[String]>) -> sqlx::Result<Vec<CredentialRow>> {
    sqlx::query_as!(
        CredentialRow,
        r#"SELECT id, name, provider, secrets, settings AS "settings: Json<CredentialSettings>", created_at, updated_at
           FROM credentials
           WHERE ($1::text IS NULL OR strpos(lower(name), $1) > 0) AND ($2::text[] IS NULL OR provider = ANY($2))
           ORDER BY provider, name"#,
        search,
        providers,
    )
    .fetch_all(pool)
    .await
}

pub async fn get_credential(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<CredentialRow>> {
    sqlx::query_as!(
        CredentialRow,
        r#"SELECT id, name, provider, secrets, settings AS "settings: Json<CredentialSettings>", created_at, updated_at
           FROM credentials WHERE id = $1"#,
        id
    )
    .fetch_optional(pool)
    .await
}

/// Number of deployments referencing each credential.
pub async fn credential_usage(pool: &PgPool) -> sqlx::Result<HashMap<Uuid, i64>> {
    let rows = sqlx::query!(r#"SELECT credential_id, count(*) AS "n!" FROM deployments GROUP BY credential_id"#).fetch_all(pool).await?;
    Ok(rows.into_iter().map(|r| (r.credential_id, r.n)).collect())
}

pub async fn insert_credential(db: impl PgExecutor<'_>, c: &CredentialRow) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO credentials (id, name, provider, secrets, settings, created_at, updated_at)
         VALUES ($1, $2, $3, $4, $5, now(), now())",
        c.id,
        c.name,
        c.provider,
        c.secrets,
        &c.settings as &Json<CredentialSettings>,
    )
    .execute(db)
    .await?;
    Ok(())
}

/// The provider is fixed at creation; deployments rely on it matching theirs.
pub async fn update_credential(db: impl PgExecutor<'_>, c: &CredentialRow) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE credentials SET name = $2, secrets = $3, settings = $4, updated_at = now() WHERE id = $1",
        c.id,
        c.name,
        c.secrets,
        &c.settings as &Json<CredentialSettings>,
    )
    .execute(db)
    .await?;
    Ok(())
}

/// Fails with a foreign-key violation while any deployment or guardrail still references the
/// credential.
pub async fn delete_credential(pool: &PgPool, id: Uuid) -> sqlx::Result<bool> {
    let r = sqlx::query!("DELETE FROM credentials WHERE id = $1", id).execute(pool).await?;
    Ok(r.rows_affected() > 0)
}

/// The projects and keys that apply guardrails to every request, as (guardrail, `project` or
/// `key`, name), by name.
pub async fn guardrail_holders(pool: &PgPool) -> sqlx::Result<Vec<(Uuid, String, String)>> {
    let rows = sqlx::query!(
        r#"SELECT g AS "guardrail!", 'project' AS "kind!", name AS "name!" FROM projects, unnest(guardrail_ids) g
           UNION ALL
           SELECT g, 'key', name FROM virtual_keys, unnest(guardrail_ids) g
           ORDER BY 3"#
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| (r.guardrail, r.kind, r.name)).collect())
}

/// Number of guardrails calling with each credential.
pub async fn credential_guardrail_usage(pool: &PgPool) -> sqlx::Result<HashMap<Uuid, i64>> {
    let rows = sqlx::query!(r#"SELECT credential_id, count(*) AS "n!" FROM guardrails GROUP BY credential_id"#).fetch_all(pool).await?;
    Ok(rows.into_iter().map(|r| (r.credential_id, r.n)).collect())
}

// ---------------------------------------------------------------- guardrails

pub async fn list_guardrails(pool: &PgPool) -> sqlx::Result<Vec<GuardrailRow>> {
    sqlx::query_as!(
        GuardrailRow,
        r#"SELECT id, name, description, provider, credential_id, config AS "config: Json<GuardrailConfig>",
                  prices AS "prices: Json<GuardrailPrices>", created_at, updated_at
           FROM guardrails ORDER BY name"#
    )
    .fetch_all(pool)
    .await
}

pub async fn get_guardrail(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<GuardrailRow>> {
    sqlx::query_as!(
        GuardrailRow,
        r#"SELECT id, name, description, provider, credential_id, config AS "config: Json<GuardrailConfig>",
                  prices AS "prices: Json<GuardrailPrices>", created_at, updated_at
           FROM guardrails WHERE id = $1"#,
        id
    )
    .fetch_optional(pool)
    .await
}

pub async fn insert_guardrail(db: impl PgExecutor<'_>, g: &GuardrailRow) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO guardrails (id, name, description, provider, credential_id, config, prices, created_at, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, now(), now())",
        g.id,
        g.name,
        g.description,
        g.provider,
        g.credential_id,
        &g.config as &Json<GuardrailConfig>,
        &g.prices as &Json<GuardrailPrices>,
    )
    .execute(db)
    .await?;
    Ok(())
}

/// The provider is fixed at creation, like a credential's.
pub async fn update_guardrail(db: impl PgExecutor<'_>, g: &GuardrailRow) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE guardrails SET name = $2, description = $3, credential_id = $4, config = $5, prices = $6, updated_at = now()
         WHERE id = $1",
        g.id,
        g.name,
        g.description,
        g.credential_id,
        &g.config as &Json<GuardrailConfig>,
        &g.prices as &Json<GuardrailPrices>,
    )
    .execute(db)
    .await?;
    Ok(())
}

/// Also takes it off the keys and projects it was set on.
pub async fn delete_guardrail(pool: &PgPool, id: Uuid) -> sqlx::Result<bool> {
    let mut tx = pool.begin().await?;
    sqlx::query!("UPDATE virtual_keys SET guardrail_ids = array_remove(guardrail_ids, $1) WHERE $1 = ANY(guardrail_ids)", id)
        .execute(&mut *tx)
        .await?;
    sqlx::query!("UPDATE projects SET guardrail_ids = array_remove(guardrail_ids, $1) WHERE $1 = ANY(guardrail_ids)", id)
        .execute(&mut *tx)
        .await?;
    let r = sqlx::query!("DELETE FROM guardrails WHERE id = $1", id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(r.rows_affected() > 0)
}

// ---------------------------------------------------------------- MCP servers

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

pub async fn sealed_mcp_secrets(db: impl PgExecutor<'_>) -> sqlx::Result<Vec<(Uuid, String, Vec<u8>)>> {
    let rows =
        sqlx::query!(r#"SELECT id, name, secret AS "secret!" FROM mcp_servers WHERE secret IS NOT NULL FOR UPDATE"#).fetch_all(db).await?;
    Ok(rows.into_iter().map(|r| (r.id, r.name, r.secret)).collect())
}

pub async fn set_mcp_secret(db: impl PgExecutor<'_>, id: Uuid, secret: &[u8]) -> sqlx::Result<()> {
    sqlx::query!("UPDATE mcp_servers SET secret = $1 WHERE id = $2", secret, id).execute(db).await?;
    Ok(())
}

// ---------------------------------------------------------------- A2A agents

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

// ---------------------------------------------------------------- MCP clients' grants

pub async fn insert_client_grant(pool: &PgPool, g: &ClientGrant, access_hash: &[u8], refresh_hash: &[u8]) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO client_grants (id, key_id, client_id, client_name, redirect_uri, resource, scope, access_token_hash, access_expires_at,
             refresh_token_hash, refresh_expires_at, created_by, created_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, now())",
        g.id,
        g.key_id,
        g.client_id,
        g.client_name,
        g.redirect_uri,
        g.resource,
        g.scope,
        access_hash,
        g.access_expires_at,
        refresh_hash,
        g.refresh_expires_at,
        g.created_by,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// The grant an access token belongs to, while the token is valid.
pub async fn client_grant_by_access(pool: &PgPool, access_hash: &[u8]) -> sqlx::Result<Option<ClientGrant>> {
    sqlx::query_as!(
        ClientGrant,
        "SELECT id, key_id, client_id, client_name, redirect_uri, resource, scope, access_expires_at, refresh_expires_at, created_by, created_at,
                last_used_at
         FROM client_grants WHERE access_token_hash = $1 AND access_expires_at > now()",
        access_hash
    )
    .fetch_optional(pool)
    .await
}

/// Revokes the grant whose replaced refresh token is presented again: its own client has the new
/// one, so whoever presents the old one got it elsewhere (RFC 6819 5.2.2.3).
pub async fn revoke_replayed_client_grant(pool: &PgPool, client_id: &str, refresh_hash: &[u8]) -> sqlx::Result<Option<Uuid>> {
    let r =
        sqlx::query!("DELETE FROM client_grants WHERE previous_refresh_hash = $1 AND client_id = $2 RETURNING id", refresh_hash, client_id)
            .fetch_optional(pool)
            .await?;
    Ok(r.map(|r| r.id))
}

/// Rotates a grant's tokens in exchange for its valid refresh token, presented by its own client,
/// in one statement: a refresh token works once. The new refresh token lasts `refresh_expires_at`,
/// but never past 90 days from the grant's approval.
pub async fn rotate_client_grant(
    pool: &PgPool,
    client_id: &str,
    refresh_hash: &[u8],
    access_hash: &[u8],
    access_expires_at: DateTime<Utc>,
    new_refresh_hash: &[u8],
    refresh_expires_at: DateTime<Utc>,
) -> sqlx::Result<Option<ClientGrant>> {
    sqlx::query_as!(
        ClientGrant,
        "UPDATE client_grants SET access_token_hash = $2, access_expires_at = $3, previous_refresh_hash = refresh_token_hash,
             refresh_token_hash = $4, refresh_expires_at = LEAST($5, created_at + interval '90 days')
         WHERE refresh_token_hash = $1 AND refresh_expires_at > now() AND client_id = $6
         RETURNING id, key_id, client_id, client_name, redirect_uri, resource, scope, access_expires_at, refresh_expires_at, created_by,
                   created_at, last_used_at",
        refresh_hash,
        access_hash,
        access_expires_at,
        new_refresh_hash,
        refresh_expires_at,
        client_id,
    )
    .fetch_optional(pool)
    .await
}

pub async fn touch_client_grant(pool: &PgPool, id: Uuid) -> sqlx::Result<()> {
    sqlx::query!("UPDATE client_grants SET last_used_at = now() WHERE id = $1", id).execute(pool).await?;
    Ok(())
}

pub async fn list_client_grants(pool: &PgPool, key_id: Uuid) -> sqlx::Result<Vec<ClientGrant>> {
    sqlx::query_as!(
        ClientGrant,
        "SELECT id, key_id, client_id, client_name, redirect_uri, resource, scope, access_expires_at, refresh_expires_at, created_by, created_at,
                last_used_at
         FROM client_grants WHERE key_id = $1 ORDER BY created_at DESC",
        key_id
    )
    .fetch_all(pool)
    .await
}

pub async fn delete_client_grant(pool: &PgPool, key_id: Uuid, id: Uuid) -> sqlx::Result<Option<ClientGrant>> {
    sqlx::query_as!(
        ClientGrant,
        "DELETE FROM client_grants WHERE key_id = $1 AND id = $2
         RETURNING id, key_id, client_id, client_name, redirect_uri, resource, scope, access_expires_at, refresh_expires_at, created_by,
                   created_at, last_used_at",
        key_id,
        id
    )
    .fetch_optional(pool)
    .await
}

/// Revokes the grant of an access or refresh token (RFC 7009); the client's `client_id` must be its own.
pub async fn revoke_client_grant(pool: &PgPool, token_hash: &[u8], client_id: &str) -> sqlx::Result<Option<Uuid>> {
    let r = sqlx::query!(
        "DELETE FROM client_grants WHERE (access_token_hash = $1 OR refresh_token_hash = $1) AND client_id = $2 RETURNING id",
        token_hash,
        client_id
    )
    .fetch_optional(pool)
    .await?;
    Ok(r.map(|r| r.id))
}

// ---------------------------------------------------------------- tool routers

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

// ---------------------------------------------------------------- upstream OAuth tokens

/// A sign-in to an `oauth_user` upstream: the upstream, the key, and the labels the upstream tells
/// the key's sign-ins apart by (`tenant=…,user=…`; empty when the key signs in once).
#[derive(Debug, Clone, Copy)]
pub struct SignInRef<'a> {
    pub kind: &'a str,
    pub upstream: Uuid,
    pub key: Uuid,
    pub labels: &'a str,
}

/// A sign-in's tokens, sealed.
pub struct OAuthTokenRow {
    pub access_token: Vec<u8>,
    pub refresh_token: Option<Vec<u8>>,
    pub expires_at: Option<DateTime<Utc>>,
}

pub async fn get_oauth_token(pool: &PgPool, s: SignInRef<'_>) -> sqlx::Result<Option<OAuthTokenRow>> {
    sqlx::query_as!(
        OAuthTokenRow,
        "SELECT access_token, refresh_token, expires_at FROM oauth_tokens
         WHERE upstream_kind = $1 AND upstream_id = $2 AND key_id = $3 AND labels = $4",
        s.kind,
        s.upstream,
        s.key,
        s.labels
    )
    .fetch_optional(pool)
    .await
}

/// The row, locked until the transaction ends: for a refresh.
pub async fn lock_oauth_token(db: impl PgExecutor<'_>, s: SignInRef<'_>) -> sqlx::Result<Option<OAuthTokenRow>> {
    sqlx::query_as!(
        OAuthTokenRow,
        "SELECT access_token, refresh_token, expires_at FROM oauth_tokens
         WHERE upstream_kind = $1 AND upstream_id = $2 AND key_id = $3 AND labels = $4 FOR UPDATE",
        s.kind,
        s.upstream,
        s.key,
        s.labels
    )
    .fetch_optional(db)
    .await
}

pub async fn update_oauth_token(
    db: impl PgExecutor<'_>,
    s: SignInRef<'_>,
    access: &[u8],
    refresh: Option<&[u8]>,
    expires_at: Option<DateTime<Utc>>,
) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE oauth_tokens SET access_token = $5, refresh_token = $6, expires_at = $7, updated_at = now(), last_used_at = now()
         WHERE upstream_kind = $1 AND upstream_id = $2 AND key_id = $3 AND labels = $4",
        s.kind,
        s.upstream,
        s.key,
        s.labels,
        access,
        refresh,
        expires_at
    )
    .execute(db)
    .await?;
    Ok(())
}

/// A new sign-in: replaces what was there for the same key and labels.
pub async fn save_oauth_token(
    pool: &PgPool,
    s: SignInRef<'_>,
    access: &[u8],
    refresh: Option<&[u8]>,
    expires_at: Option<DateTime<Utc>>,
    scope: Option<&str>,
    by: &str,
) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO oauth_tokens (upstream_kind, upstream_id, key_id, labels, access_token, refresh_token, expires_at, scope, connected_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         ON CONFLICT (upstream_kind, upstream_id, key_id, labels) DO UPDATE SET access_token = EXCLUDED.access_token,
             refresh_token = EXCLUDED.refresh_token, expires_at = EXCLUDED.expires_at, scope = EXCLUDED.scope,
             connected_by = EXCLUDED.connected_by, created_at = now(), updated_at = now(), last_used_at = now()",
        s.kind,
        s.upstream,
        s.key,
        s.labels,
        access,
        refresh,
        expires_at,
        scope,
        by
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// The upstream refused the token: the next call refreshes it.
pub async fn expire_oauth_token(pool: &PgPool, s: SignInRef<'_>) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE oauth_tokens SET expires_at = now() WHERE upstream_kind = $1 AND upstream_id = $2 AND key_id = $3 AND labels = $4",
        s.kind,
        s.upstream,
        s.key,
        s.labels
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_oauth_token(db: impl PgExecutor<'_>, s: SignInRef<'_>) -> sqlx::Result<bool> {
    let r = sqlx::query!(
        "DELETE FROM oauth_tokens WHERE upstream_kind = $1 AND upstream_id = $2 AND key_id = $3 AND labels = $4",
        s.kind,
        s.upstream,
        s.key,
        s.labels
    )
    .execute(db)
    .await?;
    Ok(r.rows_affected() > 0)
}

/// Every sign-in to an upstream, of every key: when it starts telling sign-ins apart differently.
pub async fn delete_upstream_oauth_tokens(db: impl PgExecutor<'_>, kind: &str, upstream: Uuid) -> sqlx::Result<u64> {
    Ok(sqlx::query!("DELETE FROM oauth_tokens WHERE upstream_kind = $1 AND upstream_id = $2", kind, upstream)
        .execute(db)
        .await?
        .rows_affected())
}

/// A key's sign-in to an upstream, for the console.
pub struct KeySignIn {
    pub upstream_kind: String,
    pub upstream_id: Uuid,
    pub labels: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub refreshable: bool,
    pub scope: Option<String>,
    pub connected_by: Option<String>,
    pub last_used_at: DateTime<Utc>,
}

/// A key's sign-ins of the whole key (no labels), and how many sign-ins it has per upstream.
pub async fn key_connections(pool: &PgPool, key: Uuid) -> sqlx::Result<(Vec<KeySignIn>, Vec<(String, Uuid, i64)>)> {
    let own = sqlx::query_as!(
        KeySignIn,
        r#"SELECT upstream_kind, upstream_id, labels, created_at, expires_at, refresh_token IS NOT NULL AS "refreshable!", scope, connected_by, last_used_at
           FROM oauth_tokens WHERE key_id = $1 AND labels = ''"#,
        key
    )
    .fetch_all(pool)
    .await?;
    let counts = sqlx::query!(
        r#"SELECT upstream_kind, upstream_id, count(*) AS "n!" FROM oauth_tokens WHERE key_id = $1 AND labels <> '' GROUP BY upstream_kind, upstream_id"#,
        key
    )
    .fetch_all(pool)
    .await?;
    Ok((own, counts.into_iter().map(|r| (r.upstream_kind, r.upstream_id, r.n)).collect()))
}

/// A key's sign-ins to an upstream by labels, newest first: those with every `key=value` pair in
/// `having`, and how many there are in all.
pub async fn sign_ins(
    pool: &PgPool,
    kind: &str,
    upstream: Uuid,
    key: Uuid,
    having: &[String],
    limit: i64,
    offset: i64,
) -> sqlx::Result<(Vec<KeySignIn>, i64)> {
    let rows = sqlx::query_as!(
        KeySignIn,
        r#"SELECT upstream_kind, upstream_id, labels, created_at, expires_at, refresh_token IS NOT NULL AS "refreshable!", scope, connected_by, last_used_at
           FROM oauth_tokens
           WHERE upstream_kind = $1 AND upstream_id = $2 AND key_id = $3 AND labels <> '' AND string_to_array(labels, ',') @> $4
           ORDER BY created_at DESC, labels LIMIT $5 OFFSET $6"#,
        kind,
        upstream,
        key,
        having,
        limit,
        offset
    )
    .fetch_all(pool)
    .await?;
    let total = sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM oauth_tokens
           WHERE upstream_kind = $1 AND upstream_id = $2 AND key_id = $3 AND labels <> '' AND string_to_array(labels, ',') @> $4"#,
        kind,
        upstream,
        key,
        having
    )
    .fetch_one(pool)
    .await?;
    Ok((rows, total))
}

/// One sign-in, for the console and the API.
pub async fn get_sign_in(pool: &PgPool, s: SignInRef<'_>) -> sqlx::Result<Option<KeySignIn>> {
    sqlx::query_as!(
        KeySignIn,
        r#"SELECT upstream_kind, upstream_id, labels, created_at, expires_at, refresh_token IS NOT NULL AS "refreshable!", scope, connected_by, last_used_at
           FROM oauth_tokens WHERE upstream_kind = $1 AND upstream_id = $2 AND key_id = $3 AND labels = $4"#,
        s.kind,
        s.upstream,
        s.key,
        s.labels
    )
    .fetch_optional(pool)
    .await
}

/// The sign-in was used just now.
pub async fn touch_oauth_token(pool: &PgPool, s: SignInRef<'_>) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE oauth_tokens SET last_used_at = now() WHERE upstream_kind = $1 AND upstream_id = $2 AND key_id = $3 AND labels = $4",
        s.kind,
        s.upstream,
        s.key,
        s.labels
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Deletes sign-ins unused for longer than their upstream keeps them (`sign_in_retention_days`,
/// 7 when not set), and those that cannot be used again: their access token expired, and they
/// have no refresh token. Returns how many went.
pub async fn prune_sign_ins(pool: &PgPool, default_days: i32) -> sqlx::Result<u64> {
    let unused = sqlx::query!(
        "DELETE FROM oauth_tokens t
         WHERE t.last_used_at < now() - make_interval(days => COALESCE(
             (SELECT (s.oauth->>'sign_in_retention_days')::int FROM mcp_servers s WHERE t.upstream_kind = 'mcp' AND s.id = t.upstream_id),
             (SELECT (a.oauth->>'sign_in_retention_days')::int FROM a2a_agents a WHERE t.upstream_kind = 'a2a' AND a.id = t.upstream_id),
             $1))",
        default_days
    )
    .execute(pool)
    .await?
    .rows_affected();
    let dead =
        sqlx::query!("DELETE FROM oauth_tokens WHERE refresh_token IS NULL AND expires_at < now()").execute(pool).await?.rows_affected();
    Ok(unused + dead)
}

/// Every sealed token, for `rotate-key`: (kind, upstream, key, labels, access, refresh).
#[allow(clippy::type_complexity)]
pub async fn sealed_oauth_tokens(db: impl PgExecutor<'_>) -> sqlx::Result<Vec<(String, Uuid, Uuid, String, Vec<u8>, Option<Vec<u8>>)>> {
    let rows = sqlx::query!("SELECT upstream_kind, upstream_id, key_id, labels, access_token, refresh_token FROM oauth_tokens FOR UPDATE")
        .fetch_all(db)
        .await?;
    Ok(rows.into_iter().map(|r| (r.upstream_kind, r.upstream_id, r.key_id, r.labels, r.access_token, r.refresh_token)).collect())
}

pub async fn set_oauth_token_secrets(db: impl PgExecutor<'_>, s: SignInRef<'_>, access: &[u8], refresh: Option<&[u8]>) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE oauth_tokens SET access_token = $5, refresh_token = $6 WHERE upstream_kind = $1 AND upstream_id = $2 AND key_id = $3 AND labels = $4",
        s.kind,
        s.upstream,
        s.key,
        s.labels,
        access,
        refresh
    )
    .execute(db)
    .await?;
    Ok(())
}

pub async fn sealed_a2a_secrets(db: impl PgExecutor<'_>) -> sqlx::Result<Vec<(Uuid, String, Vec<u8>)>> {
    let rows =
        sqlx::query!(r#"SELECT id, name, secret AS "secret!" FROM a2a_agents WHERE secret IS NOT NULL FOR UPDATE"#).fetch_all(db).await?;
    Ok(rows.into_iter().map(|r| (r.id, r.name, r.secret)).collect())
}

pub async fn set_a2a_secret(db: impl PgExecutor<'_>, id: Uuid, secret: &[u8]) -> sqlx::Result<()> {
    sqlx::query!("UPDATE a2a_agents SET secret = $1 WHERE id = $2", secret, id).execute(db).await?;
    Ok(())
}

// ---------------------------------------------------------------- model metadata

/// Every model's metadata, by public model name.
pub async fn list_model_metadata(pool: &PgPool) -> sqlx::Result<HashMap<String, serde_json::Value>> {
    let rows = sqlx::query!("SELECT model_name, metadata FROM model_metadata").fetch_all(pool).await?;
    Ok(rows.into_iter().map(|r| (r.model_name, r.metadata)).collect())
}

pub async fn get_model_metadata(pool: &PgPool, model: &str) -> sqlx::Result<Option<serde_json::Value>> {
    sqlx::query_scalar!("SELECT metadata FROM model_metadata WHERE model_name = $1", model).fetch_optional(pool).await
}

/// Stores a model's metadata; an empty object removes the row.
pub async fn set_model_metadata(db: impl PgExecutor<'_>, model: &str, metadata: &serde_json::Value) -> sqlx::Result<()> {
    if metadata.as_object().is_some_and(|m| m.is_empty()) {
        sqlx::query!("DELETE FROM model_metadata WHERE model_name = $1", model).execute(db).await?;
    } else {
        sqlx::query!(
            "INSERT INTO model_metadata (model_name, metadata) VALUES ($1, $2)
             ON CONFLICT (model_name) DO UPDATE SET metadata = EXCLUDED.metadata, updated_at = now()",
            model,
            metadata
        )
        .execute(db)
        .await?;
    }
    Ok(())
}

/// Moves model `old`'s metadata to `new`, unless `new` has its own. Returns whether it moved.
pub async fn rename_model_metadata(pool: &PgPool, old: &str, new: &str) -> sqlx::Result<bool> {
    let r = sqlx::query!(
        "UPDATE model_metadata SET model_name = $2, updated_at = now()
         WHERE model_name = $1 AND NOT EXISTS (SELECT 1 FROM model_metadata WHERE model_name = $2)",
        old,
        new
    )
    .execute(pool)
    .await?;
    Ok(r.rows_affected() > 0)
}

// ---------------------------------------------------------------- projects

/// Projects by name, with how many keys each groups: those whose name or description contains
/// `search` (lower case), among `projects` (a user's; `None` for all).
pub async fn list_projects(pool: &PgPool, search: Option<&str>, projects: Option<&[Uuid]>) -> sqlx::Result<Vec<(ProjectRow, i64)>> {
    let rows = sqlx::query!(
        r#"SELECT p.id, p.name, p.description, p.enabled, p.max_budget_usd, p.budget_period, p.spend_reset_at,
                  p.guardrail_ids, p.web_search, p.prompt_logging AS "prompt_logging: PromptLogging", p.created_at, p.updated_at,
                  count(k.id) AS "keys!"
           FROM projects p LEFT JOIN virtual_keys k ON k.project_id = p.id
           WHERE ($1::text IS NULL OR strpos(lower(p.name || ' ' || coalesce(p.description, '')), $1) > 0)
             AND ($2::uuid[] IS NULL OR p.id = ANY($2))
           GROUP BY p.id ORDER BY lower(p.name)"#,
        search,
        projects,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let p = ProjectRow {
                id: r.id,
                name: r.name,
                description: r.description,
                enabled: r.enabled,
                max_budget_usd: r.max_budget_usd,
                budget_period: r.budget_period,
                spend_reset_at: r.spend_reset_at,
                guardrail_ids: r.guardrail_ids,
                web_search: r.web_search,
                prompt_logging: r.prompt_logging,
                created_at: r.created_at,
                updated_at: r.updated_at,
            };
            (p, r.keys)
        })
        .collect())
}

pub async fn get_project(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<ProjectRow>> {
    sqlx::query_as!(
        ProjectRow,
        r#"SELECT id, name, description, enabled, max_budget_usd, budget_period, spend_reset_at, guardrail_ids, web_search,
                  prompt_logging AS "prompt_logging: PromptLogging", created_at, updated_at
           FROM projects WHERE id = $1"#,
        id
    )
    .fetch_optional(pool)
    .await
}

/// Creates the project or updates its name, description, state and budget. Spend resets go
/// through `reset_project_spend`.
pub async fn upsert_project(db: impl PgExecutor<'_>, p: &ProjectRow) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO projects (id, name, description, enabled, max_budget_usd, budget_period, guardrail_ids, web_search, prompt_logging,
             created_at, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, now())
         ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name, description = EXCLUDED.description, enabled = EXCLUDED.enabled,
             max_budget_usd = EXCLUDED.max_budget_usd, budget_period = EXCLUDED.budget_period, guardrail_ids = EXCLUDED.guardrail_ids,
             web_search = EXCLUDED.web_search, prompt_logging = EXCLUDED.prompt_logging, updated_at = now()",
        p.id,
        p.name,
        p.description,
        p.enabled,
        p.max_budget_usd,
        p.budget_period,
        &p.guardrail_ids,
        p.web_search,
        p.prompt_logging.as_str(),
        p.created_at,
    )
    .execute(db)
    .await?;
    Ok(())
}

pub async fn reset_project_spend(pool: &PgPool, id: Uuid) -> sqlx::Result<bool> {
    let r = sqlx::query!("UPDATE projects SET spend_reset_at = now(), updated_at = now() WHERE id = $1", id).execute(pool).await?;
    Ok(r.rows_affected() > 0)
}

pub async fn list_project_rows(pool: &PgPool) -> sqlx::Result<Vec<ProjectRow>> {
    sqlx::query_as!(
        ProjectRow,
        r#"SELECT id, name, description, enabled, max_budget_usd, budget_period, spend_reset_at, guardrail_ids, web_search,
                  prompt_logging AS "prompt_logging: PromptLogging", created_at, updated_at FROM projects"#
    )
    .fetch_all(pool)
    .await
}

/// Deletes its keys with it and returns them (id, name, prefix); `None` if there is no such
/// project. The project is locked first, so no key can join it meanwhile and outlive it.
pub async fn delete_project(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<Vec<(Uuid, String, String)>>> {
    let mut tx = pool.begin().await?;
    if sqlx::query!("SELECT id FROM projects WHERE id = $1 FOR UPDATE", id).fetch_optional(&mut *tx).await?.is_none() {
        return Ok(None);
    }
    let keys =
        sqlx::query!("DELETE FROM virtual_keys WHERE project_id = $1 RETURNING id, name, key_prefix", id).fetch_all(&mut *tx).await?;
    sqlx::query!("DELETE FROM projects WHERE id = $1", id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Some(keys.into_iter().map(|k| (k.id, k.name, k.key_prefix)).collect()))
}

pub async fn list_keys(pool: &PgPool) -> sqlx::Result<Vec<VirtualKey>> {
    sqlx::query_as!(
        VirtualKey,
        r#"SELECT k.id, k.name, k.key_hash, k.key_prefix, k.scope AS "scope: KeyScope", k.project_id,
                  coalesce(p.enabled, true) AS "project_active!", k.allowed_models, k.allowed_tags, k.rpm_limit, k.tpm_limit,
                  k.max_budget_usd, k.budget_period, k.spend_usd, k.spend_reset_at, k.enabled, k.expires_at, k.metadata,
                  k.guardrail_ids, k.web_search, k.prompt_logging, k.created_at, k.updated_at
           FROM virtual_keys k LEFT JOIN projects p ON p.id = k.project_id ORDER BY k.created_at DESC"#
    )
    .fetch_all(pool)
    .await
}

/// The keys the console's key list shows, newest first. `search` (lower case) is part of the
/// name or of the shown prefix (`sk-gw-c9d0…bb2d`), or a whole key, which starts and ends like
/// its prefix. `visible` limits a user to their projects' keys that fit a project. `logging` keeps
/// the keys whose prompts are logged (as `api::prompt_logged` decides), where `gateway_allows` is
/// the gateway's policy for keys outside projects.
pub async fn search_keys(
    pool: &PgPool,
    search: Option<&str>,
    tags: &AnyOf<String>,
    projects: &AnyOf<Uuid>,
    visible: Option<&[Uuid]>,
    logging: bool,
    gateway_allows: bool,
) -> sqlx::Result<Vec<VirtualKey>> {
    sqlx::query_as!(
        VirtualKey,
        r#"SELECT k.id, k.name, k.key_hash, k.key_prefix, k.scope AS "scope: KeyScope", k.project_id,
                  coalesce(p.enabled, true) AS "project_active!", k.allowed_models, k.allowed_tags, k.rpm_limit, k.tpm_limit,
                  k.max_budget_usd, k.budget_period, k.spend_usd, k.spend_reset_at, k.enabled, k.expires_at, k.metadata,
                  k.guardrail_ids, k.web_search, k.prompt_logging, k.created_at, k.updated_at
           FROM virtual_keys k LEFT JOIN projects p ON p.id = k.project_id
           WHERE ($1::text IS NULL OR strpos(lower(k.name), $1) > 0 OR strpos(lower(k.key_prefix), $1) > 0
                  OR (split_part(k.key_prefix, '…', 2) <> ''
                      AND starts_with($1, lower(split_part(k.key_prefix, '…', 1)))
                      AND right($1, char_length(split_part(k.key_prefix, '…', 2))) = lower(split_part(k.key_prefix, '…', 2))))
             AND ($2::text[] IS NULL OR k.allowed_tags && $2 OR ($3 AND cardinality(k.allowed_tags) = 0))
             AND ($4::uuid[] IS NULL OR k.project_id = ANY($4) OR ($5 AND k.project_id IS NULL))
             AND ($6::uuid[] IS NULL OR (k.project_id = ANY($6) AND k.scope IN ('inference', 'project')))
             AND (NOT $7 OR p.prompt_logging = 'force'
                  OR (k.prompt_logging AND (p.prompt_logging = 'allow' OR (k.project_id IS NULL AND $8))))
           ORDER BY k.created_at DESC"#,
        search,
        tags.any.as_deref(),
        tags.none,
        projects.any.as_deref(),
        projects.none,
        visible,
        logging,
        gateway_allows,
    )
    .fetch_all(pool)
    .await
}

/// A key or project as a picker offers it.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct NamedItem {
    pub id: Uuid,
    pub name: String,
    /// A key's project.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
}

/// The keys a picker offers, by name: those `visible` to a user (as in `search_keys`), and with
/// `usable` only those that may call models now (enabled, unexpired, an inference scope).
pub async fn key_names(pool: &PgPool, visible: Option<&[Uuid]>, usable: bool) -> sqlx::Result<Vec<NamedItem>> {
    sqlx::query_as!(
        NamedItem,
        r#"SELECT id, name, project_id FROM virtual_keys
           WHERE ($1::uuid[] IS NULL OR (project_id = ANY($1) AND scope IN ('inference', 'project')))
             AND (NOT $2 OR (enabled AND scope IN ('inference', 'all') AND (expires_at IS NULL OR expires_at > now())))
           ORDER BY lower(name)"#,
        visible,
        usable,
    )
    .fetch_all(pool)
    .await
}

/// The projects a picker offers, by name: all, or those in `visible`.
pub async fn project_names(pool: &PgPool, visible: Option<&[Uuid]>) -> sqlx::Result<Vec<NamedItem>> {
    sqlx::query_as!(
        NamedItem,
        r#"SELECT id, name, NULL::uuid AS project_id FROM projects WHERE $1::uuid[] IS NULL OR id = ANY($1) ORDER BY lower(name)"#,
        visible,
    )
    .fetch_all(pool)
    .await
}

/// Every tag keys hold, with how many keys hold it, among the keys in `visible` projects.
pub async fn key_tag_counts(pool: &PgPool, visible: Option<&[Uuid]>) -> sqlx::Result<HashMap<String, i64>> {
    let rows = sqlx::query!(
        r#"SELECT t AS "tag!", count(*) AS "n!" FROM virtual_keys, unnest(allowed_tags) t
           WHERE $1::uuid[] IS NULL OR project_id = ANY($1) GROUP BY t"#,
        visible,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| (r.tag, r.n)).collect())
}

pub async fn get_key(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<VirtualKey>> {
    sqlx::query_as!(
        VirtualKey,
        r#"SELECT k.id, k.name, k.key_hash, k.key_prefix, k.scope AS "scope: KeyScope", k.project_id,
                  coalesce(p.enabled, true) AS "project_active!", k.allowed_models, k.allowed_tags, k.rpm_limit, k.tpm_limit,
                  k.max_budget_usd, k.budget_period, k.spend_usd, k.spend_reset_at, k.enabled, k.expires_at, k.metadata,
                  k.guardrail_ids, k.web_search, k.prompt_logging, k.created_at, k.updated_at
           FROM virtual_keys k LEFT JOIN projects p ON p.id = k.project_id WHERE k.id = $1"#,
        id
    )
    .fetch_optional(pool)
    .await
}

pub async fn insert_key(db: impl PgExecutor<'_>, k: &VirtualKey) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO virtual_keys (id, name, key_hash, key_prefix, allowed_models, allowed_tags, rpm_limit, tpm_limit,
             max_budget_usd, budget_period, spend_usd, enabled, expires_at, metadata, scope, project_id, guardrail_ids, web_search,
             prompt_logging, created_at, updated_at)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,0,$11,$12,$13,$14,$15,$16,$17,$18,now(),now())",
        k.id,
        k.name,
        k.key_hash,
        k.key_prefix,
        &k.allowed_models,
        &k.allowed_tags,
        k.rpm_limit,
        k.tpm_limit,
        k.max_budget_usd,
        k.budget_period,
        k.enabled,
        k.expires_at,
        k.metadata,
        k.scope.as_str(),
        k.project_id,
        &k.guardrail_ids,
        k.web_search,
        k.prompt_logging,
    )
    .execute(db)
    .await?;
    Ok(())
}

pub async fn update_key(db: impl PgExecutor<'_>, k: &VirtualKey) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE virtual_keys SET name = $2, allowed_models = $3, allowed_tags = $4, rpm_limit = $5, tpm_limit = $6,
             max_budget_usd = $7, budget_period = $8, enabled = $9, expires_at = $10, metadata = $11, scope = $12,
             project_id = $13, guardrail_ids = $14, web_search = $15, prompt_logging = $16, updated_at = now()
         WHERE id = $1",
        k.id,
        k.name,
        &k.allowed_models,
        &k.allowed_tags,
        k.rpm_limit,
        k.tpm_limit,
        k.max_budget_usd,
        k.budget_period,
        k.enabled,
        k.expires_at,
        k.metadata,
        k.scope.as_str(),
        k.project_id,
        &k.guardrail_ids,
        k.web_search,
        k.prompt_logging,
    )
    .execute(db)
    .await?;
    Ok(())
}

pub async fn reset_key_spend(pool: &PgPool, id: Uuid) -> sqlx::Result<()> {
    sqlx::query!("UPDATE virtual_keys SET spend_usd = 0, spend_reset_at = now(), updated_at = now() WHERE id = $1", id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Replace a key's secret. Everything else (limits, spend, logs) stays with the key's id.
pub async fn rotate_key(pool: &PgPool, id: Uuid, key_hash: &[u8], key_prefix: &str) -> sqlx::Result<bool> {
    let r =
        sqlx::query!("UPDATE virtual_keys SET key_hash = $2, key_prefix = $3, updated_at = now() WHERE id = $1", id, key_hash, key_prefix)
            .execute(pool)
            .await?;
    Ok(r.rows_affected() > 0)
}

pub async fn delete_key(pool: &PgPool, id: Uuid) -> sqlx::Result<bool> {
    let r = sqlx::query!("DELETE FROM virtual_keys WHERE id = $1", id).execute(pool).await?;
    Ok(r.rows_affected() > 0)
}

/// Bulk insert request logs and roll spend into the owning keys, in one transaction.
pub async fn write_request_batch(pool: &PgPool, batch: &[RequestRecord]) -> sqlx::Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let mut ids = Vec::with_capacity(batch.len());
    let mut ts = Vec::with_capacity(batch.len());
    let mut key_ids = Vec::with_capacity(batch.len());
    let mut models = Vec::with_capacity(batch.len());
    let mut dep_ids = Vec::with_capacity(batch.len());
    let mut providers = Vec::with_capacity(batch.len());
    let mut apis = Vec::with_capacity(batch.len());
    let mut streams = Vec::with_capacity(batch.len());
    let mut statuses = Vec::with_capacity(batch.len());
    let mut latencies = Vec::with_capacity(batch.len());
    let mut ttfts = Vec::with_capacity(batch.len());
    let mut ins = Vec::with_capacity(batch.len());
    let mut outs = Vec::with_capacity(batch.len());
    let mut crs = Vec::with_capacity(batch.len());
    let mut cws = Vec::with_capacity(batch.len());
    let mut costs = Vec::with_capacity(batch.len());
    let mut attempts = Vec::with_capacity(batch.len());
    let mut errors = Vec::with_capacity(batch.len());
    let mut parents = Vec::with_capacity(batch.len());
    let mut projects = Vec::with_capacity(batch.len());
    let mut labels = Vec::with_capacity(batch.len());
    let mut guardrails = Vec::with_capacity(batch.len());
    let mut guardrail_costs = Vec::with_capacity(batch.len());
    let mut searches = Vec::with_capacity(batch.len());
    let mut search_costs = Vec::with_capacity(batch.len());
    let mut bodies = Vec::with_capacity(batch.len());
    let mut mcp_tools = Vec::with_capacity(batch.len());
    let mut task_states = Vec::with_capacity(batch.len());
    let mut context_ids = Vec::with_capacity(batch.len());
    for r in batch {
        ids.push(r.id);
        ts.push(r.ts);
        key_ids.push(r.key_id);
        models.push(r.model.clone());
        dep_ids.push(r.deployment_id);
        providers.push(r.provider.map(|p| p.as_str().to_string()));
        apis.push(r.api.to_string());
        streams.push(r.stream);
        statuses.push(r.status_code as i32);
        latencies.push(r.latency_ms as i32);
        ttfts.push(r.ttft_ms.map(|v| v as i32));
        ins.push(r.usage.input_tokens as i64);
        outs.push(r.usage.output_tokens as i64);
        crs.push(r.usage.cache_read_tokens as i64);
        cws.push(r.usage.cache_write_tokens as i64);
        costs.push(r.cost_usd);
        attempts.push(r.attempts as i32);
        errors.push(r.error.clone());
        parents.push(r.parent_id);
        projects.push(r.project_id);
        labels.push(r.labels.to_json());
        guardrails.push(r.guardrails.clone());
        guardrail_costs.push(r.guardrail_cost_usd);
        searches.push(r.usage.search_units as i64);
        search_costs.push(r.search_cost_usd);
        bodies.push(r.bodies_status.map(str::to_string));
        mcp_tools.push(r.tool.clone());
        task_states.push(r.task_state.clone());
        context_ids.push(r.context_id.clone());
    }

    let mut tx = pool.begin().await?;
    // Nullable columns are passed as arrays with NULL elements; `as _` skips the macro's
    // element-type check, which does not model nullable array elements.
    sqlx::query!(
        "INSERT INTO request_logs (id, ts, key_id, model_name, deployment_id, provider, api, stream,
             status_code, latency_ms, ttft_ms, input_tokens, output_tokens, cache_read_tokens,
             cache_write_tokens, cost_usd, attempts, error, parent_id, project_id, labels, guardrails, guardrail_cost_usd,
             searches, search_cost_usd, bodies, tool, task_state, context_id)
         SELECT * FROM UNNEST($1::uuid[], $2::timestamptz[], $3::uuid[], $4::text[], $5::uuid[],
             $6::text[], $7::text[], $8::bool[], $9::int[], $10::int[], $11::int[], $12::bigint[],
             $13::bigint[], $14::bigint[], $15::bigint[], $16::float8[], $17::int[], $18::text[], $19::uuid[],
             $20::uuid[], $21::jsonb[], $22::jsonb[], $23::float8[], $24::bigint[], $25::float8[], $26::text[], $27::text[], $28::text[], $29::text[])",
        &ids,
        &ts,
        &key_ids as _,
        &models,
        &dep_ids as _,
        &providers as _,
        &apis,
        &streams,
        &statuses,
        &latencies,
        &ttfts as _,
        &ins,
        &outs,
        &crs,
        &cws,
        &costs,
        &attempts,
        &errors as _,
        &parents as _,
        &projects as _,
        &labels as _,
        &guardrails as _,
        &guardrail_costs,
        &searches,
        &search_costs,
        &bodies as _,
        &mcp_tools as _,
        &task_states as _,
        &context_ids as _,
    )
    .execute(&mut *tx)
    .await?;

    // A routed request's parent entry carries the cost of its children; count it once.
    let mut spend: HashMap<Uuid, f64> = Default::default();
    for r in batch {
        if let Some(k) = r.key_id
            && r.parent_id.is_none()
            && r.cost_usd > 0.0
        {
            *spend.entry(k).or_default() += r.cost_usd;
        }
    }
    if !spend.is_empty() {
        let (kids, amounts): (Vec<Uuid>, Vec<f64>) = spend.into_iter().unzip();
        sqlx::query!(
            "UPDATE virtual_keys k SET spend_usd = k.spend_usd + s.amount
             FROM UNNEST($1::uuid[], $2::float8[]) AS s(id, amount) WHERE k.id = s.id",
            &kids,
            &amounts,
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
}

/// Stores the bodies of logged requests, once their entries are written. Each row's tool names
/// travel joined by U+001F (UNNEST cannot take an array of arrays).
pub async fn write_body_batch(pool: &PgPool, rows: Vec<(Uuid, DateTime<Utc>, Option<Uuid>, Stored)>) -> sqlx::Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let n = rows.len();
    let (mut ids, mut ts, mut keys) = (Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n));
    let (mut requests, mut screened, mut guardrails, mut responses) =
        (Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n));
    let (mut request_bytes, mut response_bytes) = (Vec::with_capacity(n), Vec::with_capacity(n));
    let (mut tools, mut texts) = (Vec::with_capacity(n), Vec::with_capacity(n));
    let (mut response_ids, mut previous_ids, mut conversations) = (Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n));
    for (id, at, key, s) in rows {
        ids.push(id);
        ts.push(at);
        keys.push(key);
        requests.push(s.request);
        screened.push(s.screened);
        guardrails.push(s.guardrails);
        responses.push(s.response);
        request_bytes.push(s.request_bytes.min(i32::MAX as usize) as i32);
        response_bytes.push(s.response_bytes.min(i32::MAX as usize) as i32);
        tools.push(s.tools.join("\u{1f}"));
        texts.push(s.text);
        response_ids.push(s.response_id);
        previous_ids.push(s.previous_response_id);
        conversations.push(s.conversation.map(|c| c.to_vec()));
    }
    sqlx::query!(
        r#"INSERT INTO request_bodies (id, ts, key_id, request, screened_request, guardrails, response, request_bytes,
               response_bytes, tool_calls, search, response_id, previous_response_id, conversation)
           SELECT b.id, b.ts, b.key_id, b.request, b.screened, b.guardrails, b.response, b.request_bytes, b.response_bytes,
                  CASE WHEN b.tools = '' THEN '{}'::text[] ELSE string_to_array(b.tools, chr(31)) END,
                  to_tsvector('simple', b.text), b.response_id, b.previous_response_id, b.conversation
           FROM UNNEST($1::uuid[], $2::timestamptz[], $3::uuid[], $4::jsonb[], $5::jsonb[], $6::jsonb[], $7::jsonb[],
                       $8::int[], $9::int[], $10::text[], $11::text[], $12::text[], $13::text[], $14::bytea[])
                AS b(id, ts, key_id, request, screened, guardrails, response, request_bytes, response_bytes, tools, text,
                     response_id, previous_response_id, conversation)
           ON CONFLICT (id) DO NOTHING"#,
        &ids,
        &ts,
        &keys as _,
        &requests as _,
        &screened as _,
        &guardrails as _,
        &responses as _,
        &request_bytes,
        &response_bytes,
        &tools,
        &texts,
        &response_ids as _,
        &previous_ids as _,
        &conversations as _,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Marks the entries whose bodies could not be stored.
pub async fn mark_bodies_dropped(pool: &PgPool, ids: &[Uuid]) -> sqlx::Result<()> {
    sqlx::query!("UPDATE request_logs SET bodies = 'dropped' WHERE id = ANY($1)", ids).execute(pool).await?;
    Ok(())
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct LogRow {
    pub id: Uuid,
    pub ts: DateTime<Utc>,
    pub key_id: Option<Uuid>,
    pub key_name: Option<String>,
    pub model_name: String,
    pub deployment_id: Option<Uuid>,
    pub provider: Option<String>,
    /// The deployment's provider model and the credential it calls with, as configured now;
    /// none once the deployment is deleted.
    pub upstream_model: Option<String>,
    pub credential_name: Option<String>,
    /// The tools its response called, where its bodies are logged (see `capture`).
    pub tool_calls: Option<Vec<String>>,
    /// The tool an MCP call (api `mcp`) called, on the server named by `model_name`.
    pub tool: Option<String>,
    /// A message to an A2A agent (api `a2a`): the state its task was left in, such as `completed`.
    pub task_state: Option<String>,
    /// A message to an A2A agent: its context, shared by a conversation's messages.
    pub context_id: Option<String>,
    pub api: String,
    pub stream: bool,
    pub status_code: i32,
    pub latency_ms: i32,
    pub ttft_ms: Option<i32>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub cost_usd: f64,
    pub attempts: i32,
    pub error: Option<String>,
    /// Set on the calls a routed request made (decision, routed call): the router's entry.
    pub parent_id: Option<Uuid>,
    /// The labels the client sent, as `{"key": "value"}`.
    #[schema(value_type = Option<HashMap<String, String>>)]
    pub labels: Option<serde_json::Value>,
    /// What the guardrails did: `{"applied": [names], "action": "masked", "findings": ["pii:NAME"]}`.
    #[schema(value_type = Option<Object>)]
    pub guardrails: Option<serde_json::Value>,
    /// The guardrails' part of `cost_usd`.
    pub guardrail_cost_usd: f64,
    /// Searches billed: web searches a chat model ran, or a rerank model's searches.
    pub searches: i64,
    /// The searches' part of `cost_usd`.
    pub search_cost_usd: f64,
    /// Whether its bodies were logged: `stored`, `truncated` (cut to the size limit), `expired`
    /// (past their retention), `dropped` (could not be kept) or `deleted`; none when not logged.
    pub bodies: Option<String>,
}

/// What the request log is filtered by (see `recent_logs`).
#[derive(Default)]
pub struct LogFilter<'a> {
    pub limit: i64,
    /// Part of the model name, ignoring case.
    pub model: Option<&'a str>,
    pub key_id: Option<Uuid>,
    /// The entries of keys that no longer exist: the log keeps their id.
    pub deleted_keys: bool,
    pub errors_only: bool,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub projects: Option<&'a [Uuid]>,
    /// A JSON object: entries with all of these labels.
    pub labels: Option<&'a serde_json::Value>,
    /// Entries whose bodies are stored (prompt logging).
    pub bodies: bool,
    /// Full-text search of the stored bodies (`websearch_to_tsquery` syntax).
    pub text: Option<&'a str>,
    /// Entries whose response called this tool, and MCP calls of it.
    pub tool: Option<&'a str>,
    /// Entries the guardrails did any of these to (`passed`, `flagged`, `masked`, `blocked`).
    pub guardrail: Option<&'a [String]>,
    /// Days logged bodies are kept: older entries report theirs as expired.
    pub body_days: u32,
}

impl LogFilter<'_> {
    /// Whether the filter looks inside stored bodies.
    pub fn by_content(&self) -> bool {
        self.bodies || self.text.is_some() || self.tool.is_some()
    }
}

/// The request log, newest first, with the calls routed requests made. Filtered by content, it
/// also returns the router entries of matching calls, so they can be shown together.
pub async fn recent_logs(pool: &PgPool, f: &LogFilter<'_>) -> sqlx::Result<Vec<LogRow>> {
    let mut rows = match f.errors_only {
        true => error_logs(pool, f).await?,
        false => all_logs(pool, f).await?,
    };
    let mut seen: std::collections::HashSet<Uuid> = rows.iter().map(|r| r.id).collect();
    if f.by_content() {
        let parents: Vec<Uuid> = rows.iter().filter_map(|r| r.parent_id).filter(|p| !seen.contains(p)).collect();
        let found = logs_by_id(pool, &parents, f.body_days).await?;
        rows.extend(found.into_iter().filter(|r| seen.insert(r.id)));
    }
    // Router entries come with the calls they made, so the log can show them together.
    let parents: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
    let children = sqlx::query_as!(
        LogRow,
        r#"SELECT l.id, l.ts, l.key_id, k.name AS "key_name?", l.model_name, l.deployment_id, l.provider,
                  l.api, l.stream, l.status_code, l.latency_ms, l.ttft_ms, l.input_tokens, l.output_tokens,
                  l.cache_read_tokens, l.cache_write_tokens, l.cost_usd, l.attempts, l.error, l.parent_id, l.labels,
                  l.guardrails, l.guardrail_cost_usd, l.searches, l.search_cost_usd, l.tool, l.task_state, l.context_id,
                  (SELECT d.upstream_model FROM deployments d WHERE d.id = l.deployment_id) AS "upstream_model?",
                  (SELECT c.name FROM deployments d JOIN credentials c ON c.id = d.credential_id WHERE d.id = l.deployment_id) AS "credential_name?", (SELECT b.tool_calls FROM request_bodies b WHERE b.id = l.id) AS "tool_calls?",
                  CASE WHEN l.bodies IN ('stored', 'truncated') AND l.ts < now() - make_interval(days => $2)
                       THEN 'expired' ELSE l.bodies END AS "bodies?"
           FROM request_logs l LEFT JOIN virtual_keys k ON k.id = l.key_id
           WHERE l.parent_id = ANY($1) ORDER BY l.ts"#,
        &parents,
        f.body_days as i32,
    )
    .fetch_all(pool)
    .await?;
    rows.extend(children.into_iter().filter(|c| seen.insert(c.id)));
    Ok(rows)
}

/// The failed requests of the log, newest first: `all_logs` with `status_code >= 400` written out,
/// so Postgres walks the partial index of failed entries (`request_logs_errors_ts_idx`) rather than
/// every entry until it has found enough failures.
async fn error_logs(pool: &PgPool, f: &LogFilter<'_>) -> sqlx::Result<Vec<LogRow>> {
    sqlx::query_as!(
        LogRow,
        r#"SELECT l.id, l.ts, l.key_id, k.name AS "key_name?", l.model_name, l.deployment_id, l.provider,
                  l.api, l.stream, l.status_code, l.latency_ms, l.ttft_ms, l.input_tokens, l.output_tokens,
                  l.cache_read_tokens, l.cache_write_tokens, l.cost_usd, l.attempts, l.error, l.parent_id, l.labels,
                  l.guardrails, l.guardrail_cost_usd, l.searches, l.search_cost_usd, l.tool, l.task_state, l.context_id,
                  (SELECT d.upstream_model FROM deployments d WHERE d.id = l.deployment_id) AS "upstream_model?",
                  (SELECT c.name FROM deployments d JOIN credentials c ON c.id = d.credential_id WHERE d.id = l.deployment_id) AS "credential_name?", b.tool_calls AS "tool_calls?",
                  CASE WHEN l.bodies IN ('stored', 'truncated') AND l.ts < now() - make_interval(days => $12)
                       THEN 'expired' ELSE l.bodies END AS "bodies?"
           FROM request_logs l LEFT JOIN virtual_keys k ON k.id = l.key_id
                LEFT JOIN request_bodies b ON b.id = l.id
           WHERE l.status_code >= 400
             AND ($2::text IS NULL OR strpos(lower(l.model_name), lower($2)) > 0)
             AND ($3::uuid IS NULL OR l.key_id = $3)
             AND ($4::timestamptz IS NULL OR l.ts >= $4)
             AND ($5::timestamptz IS NULL OR l.ts < $5)
             AND ($6::uuid[] IS NULL OR l.project_id = ANY($6))
             AND ($7::jsonb IS NULL OR l.labels @> $7)
             AND (NOT $8 OR (l.key_id IS NOT NULL AND k.id IS NULL))
             AND (NOT $9 OR b.id IS NOT NULL)
             AND ($10::text IS NULL OR b.search @@ websearch_to_tsquery('simple', $10))
             AND ($11::text IS NULL OR b.tool_calls @> ARRAY[$11::text] OR l.tool = $11)
             AND ($13::text[] IS NULL OR l.guardrails->>'action' = ANY($13))
           ORDER BY l.ts DESC LIMIT $1"#,
        f.limit,
        f.model,
        f.key_id,
        f.since,
        f.until,
        f.projects,
        f.labels,
        f.deleted_keys,
        f.bodies,
        f.text,
        f.tool,
        f.body_days as i32,
        f.guardrail,
    )
    .fetch_all(pool)
    .await
}

/// The request log, newest first, by `f` except `errors_only`.
async fn all_logs(pool: &PgPool, f: &LogFilter<'_>) -> sqlx::Result<Vec<LogRow>> {
    sqlx::query_as!(
        LogRow,
        r#"SELECT l.id, l.ts, l.key_id, k.name AS "key_name?", l.model_name, l.deployment_id, l.provider,
                  l.api, l.stream, l.status_code, l.latency_ms, l.ttft_ms, l.input_tokens, l.output_tokens,
                  l.cache_read_tokens, l.cache_write_tokens, l.cost_usd, l.attempts, l.error, l.parent_id, l.labels,
                  l.guardrails, l.guardrail_cost_usd, l.searches, l.search_cost_usd, l.tool, l.task_state, l.context_id,
                  (SELECT d.upstream_model FROM deployments d WHERE d.id = l.deployment_id) AS "upstream_model?",
                  (SELECT c.name FROM deployments d JOIN credentials c ON c.id = d.credential_id WHERE d.id = l.deployment_id) AS "credential_name?", b.tool_calls AS "tool_calls?",
                  CASE WHEN l.bodies IN ('stored', 'truncated') AND l.ts < now() - make_interval(days => $12)
                       THEN 'expired' ELSE l.bodies END AS "bodies?"
           FROM request_logs l LEFT JOIN virtual_keys k ON k.id = l.key_id
                LEFT JOIN request_bodies b ON b.id = l.id
           WHERE ($2::text IS NULL OR strpos(lower(l.model_name), lower($2)) > 0)
             AND ($3::uuid IS NULL OR l.key_id = $3)
             AND ($4::timestamptz IS NULL OR l.ts >= $4)
             AND ($5::timestamptz IS NULL OR l.ts < $5)
             AND ($6::uuid[] IS NULL OR l.project_id = ANY($6))
             AND ($7::jsonb IS NULL OR l.labels @> $7)
             AND (NOT $8 OR (l.key_id IS NOT NULL AND k.id IS NULL))
             AND (NOT $9 OR b.id IS NOT NULL)
             AND ($10::text IS NULL OR b.search @@ websearch_to_tsquery('simple', $10))
             AND ($11::text IS NULL OR b.tool_calls @> ARRAY[$11::text] OR l.tool = $11)
             AND ($13::text[] IS NULL OR l.guardrails->>'action' = ANY($13))
           ORDER BY l.ts DESC LIMIT $1"#,
        f.limit,
        f.model,
        f.key_id,
        f.since,
        f.until,
        f.projects,
        f.labels,
        f.deleted_keys,
        f.bodies,
        f.text,
        f.tool,
        f.body_days as i32,
        f.guardrail,
    )
    .fetch_all(pool)
    .await
}

async fn logs_by_id(pool: &PgPool, ids: &[Uuid], body_days: u32) -> sqlx::Result<Vec<LogRow>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as!(
        LogRow,
        // No join: on an empty table Postgres plans one with request_logs as the inner side, and
        // sqlx then takes its columns for nullable, unlike on a table with rows.
        r#"SELECT l.id, l.ts, l.key_id, (SELECT k.name FROM virtual_keys k WHERE k.id = l.key_id) AS "key_name?", l.model_name,
                  l.deployment_id, l.provider, l.api, l.stream, l.status_code, l.latency_ms, l.ttft_ms, l.input_tokens, l.output_tokens,
                  l.cache_read_tokens, l.cache_write_tokens, l.cost_usd, l.attempts, l.error, l.parent_id, l.labels,
                  l.guardrails, l.guardrail_cost_usd, l.searches, l.search_cost_usd, l.tool, l.task_state, l.context_id,
                  (SELECT d.upstream_model FROM deployments d WHERE d.id = l.deployment_id) AS "upstream_model?",
                  (SELECT c.name FROM deployments d JOIN credentials c ON c.id = d.credential_id WHERE d.id = l.deployment_id) AS "credential_name?", (SELECT b.tool_calls FROM request_bodies b WHERE b.id = l.id) AS "tool_calls?",
                  CASE WHEN l.bodies IN ('stored', 'truncated') AND l.ts < now() - make_interval(days => $2)
                       THEN 'expired' ELSE l.bodies END AS "bodies?"
           FROM request_logs l
           WHERE l.id = ANY($1)"#,
        ids,
        body_days as i32,
    )
    .fetch_all(pool)
    .await
}

/// One entry's stored bodies, with what decides who may read them.
pub struct BodiesRow {
    pub id: Uuid,
    pub ts: DateTime<Utc>,
    pub project_id: Option<Uuid>,
    pub key_id: Option<Uuid>,
    pub status: Option<String>,
    pub request: Option<serde_json::Value>,
    pub screened_request: Option<serde_json::Value>,
    pub guardrails: Option<serde_json::Value>,
    pub response: Option<serde_json::Value>,
    pub request_bytes: i32,
    pub response_bytes: i32,
    pub response_id: Option<String>,
    pub previous_response_id: Option<String>,
    pub conversation: Option<Vec<u8>>,
}

pub async fn log_bodies(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<BodiesRow>> {
    sqlx::query_as!(
        BodiesRow,
        r#"SELECT l.id, l.ts, l.project_id, l.key_id, l.bodies AS status, b.request, b.screened_request,
                  b.guardrails, b.response, b.request_bytes, b.response_bytes, b.response_id, b.previous_response_id,
                  b.conversation
           FROM request_logs l JOIN request_bodies b ON b.id = l.id WHERE l.id = $1"#,
        id
    )
    .fetch_optional(pool)
    .await
}

/// The logged turns before and after `row` in its conversation, of the same key and in
/// `projects`: by Responses id where the turns chain that way, otherwise by the conversation's
/// opening (see `affinity::Affinity::fingerprint`).
pub async fn turn_links(pool: &PgPool, row: &BodiesRow, projects: Option<&[Uuid]>) -> sqlx::Result<(Option<Uuid>, Option<Uuid>)> {
    let r = sqlx::query!(
        r#"SELECT
             (SELECT p.id FROM request_bodies p JOIN request_logs pl ON pl.id = p.id
              WHERE p.key_id = $1 AND p.id <> $2 AND ($6::uuid[] IS NULL OR pl.project_id = ANY($6))
                AND CASE WHEN $3::text IS NOT NULL THEN p.response_id = $3
                         ELSE $4::bytea IS NOT NULL AND p.conversation = $4 AND p.ts < $5 END
              ORDER BY p.ts DESC LIMIT 1) AS previous,
             (SELECT n.id FROM request_bodies n JOIN request_logs nl ON nl.id = n.id
              WHERE n.key_id = $1 AND n.id <> $2 AND ($6::uuid[] IS NULL OR nl.project_id = ANY($6))
                AND (($7::text IS NOT NULL AND n.previous_response_id = $7)
                     OR ($4::bytea IS NOT NULL AND n.conversation = $4 AND n.ts > $5))
              ORDER BY n.ts LIMIT 1) AS next"#,
        row.key_id,
        row.id,
        row.previous_response_id,
        row.conversation,
        row.ts,
        projects,
        row.response_id,
    )
    .fetch_one(pool)
    .await?;
    Ok((r.previous, r.next))
}

/// A tool the latest logged responses called, with how many of them did (`recent_tools`).
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ToolUse {
    pub name: String,
    pub requests: i64,
}

/// The tools the latest `limit` logged responses in `projects` called, most used first.
pub async fn recent_tools(pool: &PgPool, limit: i64, projects: Option<&[Uuid]>) -> sqlx::Result<Vec<ToolUse>> {
    sqlx::query_as!(
        ToolUse,
        r#"SELECT t AS "name!", count(*) AS "requests!"
           FROM (SELECT b.tool_calls FROM request_bodies b JOIN request_logs l ON l.id = b.id
                 WHERE cardinality(b.tool_calls) > 0 AND ($2::uuid[] IS NULL OR l.project_id = ANY($2))
                 ORDER BY b.ts DESC LIMIT $1) s, unnest(s.tool_calls) t
           GROUP BY t ORDER BY 2 DESC, 1 LIMIT 100"#,
        limit,
        projects,
    )
    .fetch_all(pool)
    .await
}

/// Whose stored bodies to count or delete: a key's, or those of the requests made in a project.
#[derive(Clone, Copy)]
pub enum BodiesOf {
    Key(Uuid),
    Project(Uuid),
}

pub async fn count_bodies(pool: &PgPool, of: BodiesOf) -> sqlx::Result<i64> {
    match of {
        BodiesOf::Key(id) => {
            sqlx::query_scalar!(r#"SELECT count(*) AS "n!" FROM request_bodies WHERE key_id = $1"#, id).fetch_one(pool).await
        }
        BodiesOf::Project(id) => {
            sqlx::query_scalar!(
                r#"SELECT count(*) AS "n!" FROM request_bodies b JOIN request_logs l ON l.id = b.id WHERE l.project_id = $1"#,
                id
            )
            .fetch_one(pool)
            .await
        }
    }
}

/// Deletes the stored bodies of a key or project, in chunks, and marks their entries as
/// `deleted`. Returns how many were deleted.
pub async fn delete_bodies(pool: &PgPool, of: BodiesOf) -> sqlx::Result<u64> {
    let mut deleted = 0;
    loop {
        let n = match of {
            BodiesOf::Key(id) => sqlx::query!(
                "WITH d AS (DELETE FROM request_bodies WHERE id IN (SELECT id FROM request_bodies WHERE key_id = $1 LIMIT 5000) RETURNING id)
                 UPDATE request_logs l SET bodies = 'deleted' FROM d WHERE l.id = d.id",
                id
            )
            .execute(pool)
            .await?
            .rows_affected(),
            BodiesOf::Project(id) => sqlx::query!(
                "WITH d AS (DELETE FROM request_bodies WHERE id IN (
                     SELECT b.id FROM request_bodies b JOIN request_logs l ON l.id = b.id WHERE l.project_id = $1 LIMIT 5000)
                     RETURNING id)
                 UPDATE request_logs l SET bodies = 'deleted' FROM d WHERE l.id = d.id",
                id
            )
            .execute(pool)
            .await?
            .rows_affected(),
        };
        deleted += n;
        if n < 5000 {
            return Ok(deleted);
        }
    }
}

/// A label value of the latest labelled requests, with how many of them carry it (`recent_labels`).
pub struct LabelUse {
    pub key: String,
    pub value: String,
    pub requests: i64,
}

/// The label keys and values of the latest `limit` labelled requests in `projects`, ordered by key
/// and then by use, most used first. The console suggests them in its label filters.
pub async fn recent_labels(pool: &PgPool, limit: i64, projects: Option<&[Uuid]>) -> sqlx::Result<Vec<LabelUse>> {
    sqlx::query_as!(
        LabelUse,
        r#"SELECT e.key AS "key!", e.value AS "value!", count(*) AS "requests!"
           FROM (SELECT labels FROM request_logs
                 WHERE labels IS NOT NULL AND ($2::uuid[] IS NULL OR project_id = ANY($2))
                 ORDER BY ts DESC LIMIT $1) l
           CROSS JOIN LATERAL jsonb_each_text(l.labels) e
           GROUP BY e.key, e.value
           ORDER BY e.key, count(*) DESC, e.value"#,
        limit,
        projects,
    )
    .fetch_all(pool)
    .await
}

/// An audit log entry (see `audit`).
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct AuditEntry {
    pub id: Uuid,
    pub ts: DateTime<Utc>,
    /// `master_key`, `virtual_key`, `session` (a signed-in admin), `dev` (Dev Login) or `system`
    /// (the gateway's scheduled work).
    pub actor_kind: String,
    /// The admin user or virtual key.
    pub actor_id: Option<Uuid>,
    /// The admin's email or name, or the virtual key's name.
    pub actor_name: Option<String>,
    /// `create`, `update`, `delete`, `regenerate`, `reset_spend`, `import`, `sign_in`, ...
    pub action: String,
    /// `credential`, `deployment`, `router`, `key`, `project`, `configuration`, `price_catalog`,
    /// `oidc_settings`, `admin` or `session`.
    pub target_type: String,
    pub target_id: Option<Uuid>,
    pub target_name: Option<String>,
    /// For an update, the changed fields as `{"field": {"from": .., "to": ..}}`; for a create,
    /// the new object. Never secrets.
    #[schema(value_type = Object)]
    pub details: serde_json::Value,
}

pub async fn insert_audit(pool: &PgPool, e: &AuditEntry) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO audit_log (id, ts, actor_kind, actor_id, actor_name, action, target_type, target_id, target_name, details)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        e.id,
        e.ts,
        e.actor_kind,
        e.actor_id,
        e.actor_name,
        e.action,
        e.target_type,
        e.target_id,
        e.target_name,
        e.details,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Newest first. `search` matches the target's or the actor's name, ignoring case.
pub async fn list_audit(
    pool: &PgPool,
    limit: i64,
    target_type: Option<&str>,
    search: Option<&str>,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
) -> sqlx::Result<Vec<AuditEntry>> {
    sqlx::query_as!(
        AuditEntry,
        r#"SELECT id, ts, actor_kind, actor_id, actor_name, action, target_type, target_id, target_name, details
           FROM audit_log
           WHERE ($2::text IS NULL OR target_type = $2)
             AND ($3::text IS NULL OR strpos(lower(coalesce(target_name, '') || ' ' || coalesce(actor_name, '')), lower($3)) > 0)
             AND ($4::timestamptz IS NULL OR ts >= $4)
             AND ($5::timestamptz IS NULL OR ts < $5)
           ORDER BY ts DESC LIMIT $1"#,
        limit,
        target_type,
        search,
        since,
        until,
    )
    .fetch_all(pool)
    .await
}

/// Request-log aggregates for the Metrics page: per subject (a model, a deployment, or one row
/// for everything), or per subject and time bucket.
#[derive(Debug, Default, Clone)]
pub struct LogTotals {
    pub subject: String,
    /// Start of the time bucket (Unix seconds); 0 for totals.
    pub bucket: i64,
    pub requests: i64,
    pub errors: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub cost_usd: f64,
    pub latency_ms_sum: i64,
    pub ttft_ms_sum: i64,
    pub ttft_count: i64,
    pub p95_latency_ms: Option<i32>,
}

/// Per time bucket of `step` seconds, from `since` up to `until`, over the entries of `models`
/// (all when `None`) except those of `exclude`, optionally for one key (or with `deleted_keys` the
/// keys that no longer exist, whose id the log keeps) and for the entries with all of `labels` (a
/// JSON object), as in every query below.
pub async fn selection_series(
    pool: &PgPool,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    step: i64,
    key_id: Option<Uuid>,
    deleted_keys: bool,
    models: Option<&[String]>,
    exclude: &[String],
    projects: Option<&[Uuid]>,
    labels: Option<&serde_json::Value>,
) -> sqlx::Result<Vec<LogTotals>> {
    sqlx::query_as!(
        LogTotals,
        r#"SELECT '' AS "subject!",
                  (floor(extract(epoch FROM ts) / $2) * $2)::bigint AS "bucket!",
                  count(*) AS "requests!", count(*) FILTER (WHERE status_code >= 400) AS "errors!",
                  sum(input_tokens)::bigint AS "input_tokens!", sum(output_tokens)::bigint AS "output_tokens!",
                  sum(cache_read_tokens)::bigint AS "cache_read_tokens!", sum(cache_write_tokens)::bigint AS "cache_write_tokens!",
                  sum(cost_usd) AS "cost_usd!", sum(latency_ms)::bigint AS "latency_ms_sum!",
                  coalesce(sum(ttft_ms), 0)::bigint AS "ttft_ms_sum!", count(ttft_ms) AS "ttft_count!",
                  NULL::int AS "p95_latency_ms?"
           FROM request_logs
           WHERE ts >= $1 AND ts < $4 AND ($3::uuid IS NULL OR key_id = $3) AND ($5::uuid[] IS NULL OR project_id = ANY($5))
             AND ($6::jsonb IS NULL OR labels @> $6)
             AND (NOT $7 OR (key_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM virtual_keys k WHERE k.id = request_logs.key_id)))
             AND ($8::text[] IS NULL OR model_name = ANY($8)) AND NOT (model_name = ANY($9))
           GROUP BY 2"#,
        since,
        step as f64,
        key_id,
        until,
        projects,
        labels,
        deleted_keys,
        models,
        exclude,
    )
    .fetch_all(pool)
    .await
}

/// Totals per model, with exact latency percentiles.
pub async fn model_totals(
    pool: &PgPool,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    key_id: Option<Uuid>,
    deleted_keys: bool,
    projects: Option<&[Uuid]>,
    labels: Option<&serde_json::Value>,
) -> sqlx::Result<Vec<LogTotals>> {
    sqlx::query_as!(
        LogTotals,
        r#"SELECT model_name AS "subject!", 0::bigint AS "bucket!",
                  count(*) AS "requests!", count(*) FILTER (WHERE status_code >= 400) AS "errors!",
                  sum(input_tokens)::bigint AS "input_tokens!", sum(output_tokens)::bigint AS "output_tokens!",
                  sum(cache_read_tokens)::bigint AS "cache_read_tokens!", sum(cache_write_tokens)::bigint AS "cache_write_tokens!",
                  sum(cost_usd) AS "cost_usd!", sum(latency_ms)::bigint AS "latency_ms_sum!",
                  coalesce(sum(ttft_ms), 0)::bigint AS "ttft_ms_sum!", count(ttft_ms) AS "ttft_count!",
                  percentile_disc(0.95) WITHIN GROUP (ORDER BY latency_ms) AS "p95_latency_ms?"
           FROM request_logs
           WHERE ts >= $1 AND ts < $3 AND ($2::uuid IS NULL OR key_id = $2) AND ($4::uuid[] IS NULL OR project_id = ANY($4))
             AND ($5::jsonb IS NULL OR labels @> $5)
             AND (NOT $6 OR (key_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM virtual_keys k WHERE k.id = request_logs.key_id)))
           GROUP BY model_name"#,
        since,
        key_id,
        until,
        projects,
        labels,
        deleted_keys,
    )
    .fetch_all(pool)
    .await
}

/// Totals over the entries of `models` (all when `None`) except those of `exclude`, with exact
/// latency percentiles.
pub async fn selection_totals(
    pool: &PgPool,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    key_id: Option<Uuid>,
    deleted_keys: bool,
    models: Option<&[String]>,
    exclude: &[String],
    projects: Option<&[Uuid]>,
    labels: Option<&serde_json::Value>,
) -> sqlx::Result<LogTotals> {
    sqlx::query_as!(
        LogTotals,
        r#"SELECT '' AS "subject!", 0::bigint AS "bucket!",
                  count(*) AS "requests!", count(*) FILTER (WHERE status_code >= 400) AS "errors!",
                  coalesce(sum(input_tokens), 0)::bigint AS "input_tokens!", coalesce(sum(output_tokens), 0)::bigint AS "output_tokens!",
                  coalesce(sum(cache_read_tokens), 0)::bigint AS "cache_read_tokens!",
                  coalesce(sum(cache_write_tokens), 0)::bigint AS "cache_write_tokens!",
                  coalesce(sum(cost_usd), 0) AS "cost_usd!", coalesce(sum(latency_ms), 0)::bigint AS "latency_ms_sum!",
                  coalesce(sum(ttft_ms), 0)::bigint AS "ttft_ms_sum!", count(ttft_ms) AS "ttft_count!",
                  percentile_disc(0.95) WITHIN GROUP (ORDER BY latency_ms) AS "p95_latency_ms?"
           FROM request_logs
           WHERE ts >= $1 AND ts < $4 AND ($2::uuid IS NULL OR key_id = $2) AND NOT (model_name = ANY($3))
             AND ($5::uuid[] IS NULL OR project_id = ANY($5)) AND ($6::jsonb IS NULL OR labels @> $6)
             AND (NOT $7 OR (key_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM virtual_keys k WHERE k.id = request_logs.key_id)))
             AND ($8::text[] IS NULL OR model_name = ANY($8))"#,
        since,
        key_id,
        exclude,
        until,
        projects,
        labels,
        deleted_keys,
        models,
    )
    .fetch_one(pool)
    .await
}

/// One model's requests in one hour, for the model hub's status bar.
#[derive(Debug)]
pub struct ModelHour {
    pub model_name: String,
    /// Start of the hour (Unix seconds).
    pub bucket: i64,
    pub requests: i64,
    pub errors: i64,
}

/// Each of `models`' requests per hour since `since`, of one key or within `projects` when given.
pub async fn model_hours(
    pool: &PgPool,
    since: DateTime<Utc>,
    models: &[String],
    key_id: Option<Uuid>,
    projects: Option<&[Uuid]>,
) -> sqlx::Result<Vec<ModelHour>> {
    sqlx::query_as!(
        ModelHour,
        r#"SELECT model_name, (floor(extract(epoch FROM ts) / 3600) * 3600)::bigint AS "bucket!",
                  count(*) AS "requests!", count(*) FILTER (WHERE status_code >= 400) AS "errors!"
           FROM request_logs
           WHERE ts >= $1 AND model_name = ANY($2) AND ($3::uuid IS NULL OR key_id = $3) AND ($4::uuid[] IS NULL OR project_id = ANY($4))
           GROUP BY 1, 2"#,
        since,
        models,
        key_id,
        projects,
    )
    .fetch_all(pool)
    .await
}

/// How fast one model answered its successful requests, for the model hub.
#[derive(Debug)]
pub struct ModelSpeed {
    pub model_name: String,
    pub p50_latency_ms: Option<i32>,
    /// Output tokens and the milliseconds spent generating them (after the first token) in
    /// streamed answers; non-streamed ones count their whole latency, in `whole_*`.
    pub stream_tokens: i64,
    pub stream_ms: i64,
    pub whole_tokens: i64,
    pub whole_ms: i64,
}

/// The successful requests' median latency and generation speed per model, filtered as `model_hours`.
pub async fn model_speeds(
    pool: &PgPool,
    since: DateTime<Utc>,
    models: &[String],
    key_id: Option<Uuid>,
    projects: Option<&[Uuid]>,
) -> sqlx::Result<Vec<ModelSpeed>> {
    sqlx::query_as!(
        ModelSpeed,
        r#"SELECT model_name,
                  percentile_disc(0.5) WITHIN GROUP (ORDER BY latency_ms) AS "p50_latency_ms?",
                  coalesce(sum(output_tokens) FILTER (WHERE ttft_ms IS NOT NULL AND latency_ms > ttft_ms), 0)::bigint AS "stream_tokens!",
                  coalesce(sum(latency_ms - ttft_ms) FILTER (WHERE ttft_ms IS NOT NULL AND latency_ms > ttft_ms), 0)::bigint AS "stream_ms!",
                  coalesce(sum(output_tokens) FILTER (WHERE ttft_ms IS NULL), 0)::bigint AS "whole_tokens!",
                  coalesce(sum(latency_ms) FILTER (WHERE ttft_ms IS NULL), 0)::bigint AS "whole_ms!"
           FROM request_logs
           WHERE ts >= $1 AND model_name = ANY($2) AND ($3::uuid IS NULL OR key_id = $3) AND ($4::uuid[] IS NULL OR project_id = ANY($4))
             AND status_code < 400
           GROUP BY 1"#,
        since,
        models,
        key_id,
        projects,
    )
    .fetch_all(pool)
    .await
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct KeyUsageRow {
    pub key_id: Option<Uuid>,
    pub key_name: Option<String>,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_usd: f64,
}

pub async fn key_usage_since(pool: &PgPool, since: DateTime<Utc>, projects: Option<&[Uuid]>) -> sqlx::Result<Vec<KeyUsageRow>> {
    sqlx::query_as!(
        KeyUsageRow,
        r#"SELECT l.key_id, k.name AS "key_name?", count(*) AS "requests!",
                  COALESCE(sum(l.input_tokens + l.cache_read_tokens + l.cache_write_tokens), 0)::bigint AS "input_tokens!",
                  COALESCE(sum(l.output_tokens), 0)::bigint AS "output_tokens!",
                  COALESCE(sum(l.cost_usd), 0) AS "cost_usd!"
           FROM request_logs l LEFT JOIN virtual_keys k ON k.id = l.key_id
           WHERE l.ts >= $1 AND l.parent_id IS NULL AND ($2::uuid[] IS NULL OR l.project_id = ANY($2))
           GROUP BY l.key_id, k.name ORDER BY 6 DESC, 3 DESC LIMIT 50"#,
        since,
        projects,
    )
    .fetch_all(pool)
    .await
}

/// Retention cleanup of the request log, the audit log and logged bodies, each by its own number
/// of days (0 keeps that log forever; bodies always go). Returns how many entries each lost.
/// Guarded by an advisory lock so only one replica runs it at a time.
pub async fn prune_logs(pool: &PgPool, request_days: u32, audit_days: u32, body_days: u32) -> sqlx::Result<(u64, u64, u64)> {
    const LOCK_ID: i64 = 0x6777_6c6f_6773; // "gwlogs"
    let mut conn = pool.acquire().await?;
    let locked = sqlx::query_scalar!(r#"SELECT pg_try_advisory_lock($1) AS "locked!""#, LOCK_ID).fetch_one(&mut *conn).await?;
    if !locked {
        return Ok((0, 0, 0));
    }
    let result = async {
        ensure_log_partitions(&mut *conn).await?;
        // Whole days drop with their partition; what remains older (the legacy partition, and
        // the start of the newest expired day) is deleted in chunks, so one delete does not hold
        // locks long: each entry updates several indexes, and its bodies go with it.
        let mut requests = 0;
        if request_days > 0 {
            let dropped =
                sqlx::query_scalar!("SELECT gw_drop_request_log_partitions(now() - make_interval(days => $1))", request_days as i32)
                    .fetch_one(&mut *conn)
                    .await?;
            requests += dropped.unwrap_or(0) as u64;
        }
        while request_days > 0 {
            let n = sqlx::query!(
                "DELETE FROM request_logs WHERE id IN (
                     SELECT id FROM request_logs WHERE ts < now() - make_interval(days => $1) LIMIT 5000)",
                request_days as i32
            )
            .execute(&mut *conn)
            .await?
            .rows_affected();
            requests += n;
            if n < 5000 {
                break;
            }
        }
        let audit = match audit_days {
            0 => 0,
            days => sqlx::query!("DELETE FROM audit_log WHERE ts < now() - make_interval(days => $1)", days as i32)
                .execute(&mut *conn)
                .await?
                .rows_affected(),
        };
        // In chunks: bodies are large, and a long delete would hold locks and write a burst of WAL.
        let mut bodies = 0;
        loop {
            let n = sqlx::query!(
                "DELETE FROM request_bodies WHERE id IN (
                     SELECT id FROM request_bodies WHERE ts < now() - make_interval(days => $1) LIMIT 5000)",
                body_days as i32
            )
            .execute(&mut *conn)
            .await?
            .rows_affected();
            bodies += n;
            if n < 5000 {
                break;
            }
        }
        Ok::<_, sqlx::Error>((requests, audit, bodies))
    }
    .await;
    let _ = sqlx::query_scalar!("SELECT pg_advisory_unlock($1)", LOCK_ID).fetch_one(&mut *conn).await;
    result
}

/// Runs `f` while holding the advisory lock `id`, or returns `None` if another replica holds it.
/// The lock is taken on a connection of its own that is closed afterwards, so it is released
/// even when `f` is cancelled (an admin request whose client went away) or panics.
pub async fn with_lock<T>(pool: &PgPool, id: i64, f: impl Future<Output = T>) -> sqlx::Result<Option<T>> {
    let mut conn = pool.acquire().await?.detach();
    let locked = sqlx::query_scalar!(r#"SELECT pg_try_advisory_lock($1) AS "locked!""#, id).fetch_one(&mut conn).await?;
    if !locked {
        return Ok(None);
    }
    let out = f.await;
    let _ = conn.close().await;
    Ok(Some(out))
}

pub async fn ping(pool: &PgPool) -> bool {
    sqlx::query_scalar!("SELECT 1 AS one").fetch_one(pool).await.is_ok()
}

/// What a project's keys spent since `since`, as recorded on each request (rebuilds budget counters).
pub async fn project_spend_since(pool: &PgPool, project_id: Uuid, since: DateTime<Utc>) -> sqlx::Result<f64> {
    sqlx::query_scalar!(
        r#"SELECT COALESCE(sum(cost_usd), 0) AS "spent!" FROM request_logs
           WHERE project_id = $1 AND ts >= $2 AND parent_id IS NULL"#,
        project_id,
        since
    )
    .fetch_one(pool)
    .await
}

/// What a key spent since `since`, from the durable request log (rebuilds budget counters).
pub async fn key_spend_since(pool: &PgPool, key_id: Uuid, since: DateTime<Utc>) -> sqlx::Result<f64> {
    sqlx::query_scalar!(
        r#"SELECT COALESCE(sum(cost_usd), 0) AS "spent!" FROM request_logs
           WHERE key_id = $1 AND ts >= $2 AND parent_id IS NULL"#,
        key_id,
        since
    )
    .fetch_one(pool)
    .await
}
