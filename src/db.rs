//! Postgres access. Every query is checked against the schema at compile time by sqlx's
//! macros; `.sqlx/` holds the offline metadata so builds need no database
//! (regenerate with `cargo sqlx prepare` after changing a query or migration).

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{PgExecutor, PgPool, migrate::Migrator, postgres::PgPoolOptions, types::Json};
use uuid::Uuid;

use crate::{
    accounting::RequestRecord,
    guardrails::{GuardrailConfig, GuardrailPrices, GuardrailRow},
    model::{
        AdminUserRow, AliasRow, CredentialRow, CredentialSettings, DeploymentConfig, DeploymentRow, KeyScope, OidcSettingsRow, PersonRole, ProjectRow, Route,
        RouterRow, VirtualKey,
    },
};

/// Postgres error codes the admin API turns into 409 responses.
pub fn is_foreign_key_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(d) if d.code().as_deref() == Some("23503"))
}

pub fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(d) if d.code().as_deref() == Some("23505"))
}

pub async fn connect(url: &str, max_connections: u32) -> Result<PgPool, sqlx::Error> {
    let pool = PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect(url)
        .await?;
    // sqlx takes a Postgres advisory lock, so concurrent replicas migrate safely.
    MIGRATOR.run(&pool).await?;
    Ok(pool)
}

static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

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
    sqlx::query_scalar!("SELECT project_id FROM project_members WHERE user_id = $1 ORDER BY project_id", user_id)
        .fetch_all(pool)
        .await
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
    sqlx::query!(
        "INSERT INTO project_members (project_id, user_id) SELECT p, $2 FROM unnest($1::uuid[]) AS t(p)",
        projects,
        user_id
    )
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
    let modes = sqlx::query_scalar!(
        r#"SELECT DISTINCT data->>'mode' AS "m!" FROM model_prices WHERE data->>'mode' IS NOT NULL ORDER BY 1"#
    )
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
    let rows = sqlx::query!(r#"SELECT credential_id, count(*) AS "n!" FROM deployments GROUP BY credential_id"#)
        .fetch_all(pool)
        .await?;
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

/// Number of guardrails calling with each credential.
pub async fn credential_guardrail_usage(pool: &PgPool) -> sqlx::Result<HashMap<Uuid, i64>> {
    let rows = sqlx::query!(r#"SELECT credential_id, count(*) AS "n!" FROM guardrails GROUP BY credential_id"#)
        .fetch_all(pool)
        .await?;
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

/// Projects by name, with how many keys each groups.
pub async fn list_projects(pool: &PgPool) -> sqlx::Result<Vec<(ProjectRow, i64)>> {
    let rows = sqlx::query!(
        r#"SELECT p.id, p.name, p.description, p.enabled, p.max_budget_usd, p.budget_period, p.spend_reset_at,
                  p.guardrail_ids, p.created_at, p.updated_at, count(k.id) AS "keys!"
           FROM projects p LEFT JOIN virtual_keys k ON k.project_id = p.id
           GROUP BY p.id ORDER BY lower(p.name)"#
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
        "SELECT id, name, description, enabled, max_budget_usd, budget_period, spend_reset_at, guardrail_ids, created_at, updated_at
         FROM projects WHERE id = $1",
        id
    )
    .fetch_optional(pool)
    .await
}

/// Creates the project or updates its name, description, state and budget. Spend resets go
/// through `reset_project_spend`.
pub async fn upsert_project(pool: &PgPool, p: &ProjectRow) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO projects (id, name, description, enabled, max_budget_usd, budget_period, guardrail_ids, created_at, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, now())
         ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name, description = EXCLUDED.description, enabled = EXCLUDED.enabled,
             max_budget_usd = EXCLUDED.max_budget_usd, budget_period = EXCLUDED.budget_period, guardrail_ids = EXCLUDED.guardrail_ids,
             updated_at = now()",
        p.id,
        p.name,
        p.description,
        p.enabled,
        p.max_budget_usd,
        p.budget_period,
        &p.guardrail_ids,
        p.created_at,
    )
    .execute(pool)
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
        "SELECT id, name, description, enabled, max_budget_usd, budget_period, spend_reset_at, guardrail_ids, created_at, updated_at FROM projects"
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
    let keys = sqlx::query!("DELETE FROM virtual_keys WHERE project_id = $1 RETURNING id, name, key_prefix", id).fetch_all(&mut *tx).await?;
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
                  k.guardrail_ids, k.created_at, k.updated_at
           FROM virtual_keys k LEFT JOIN projects p ON p.id = k.project_id ORDER BY k.created_at DESC"#
    )
    .fetch_all(pool)
    .await
}

pub async fn get_key(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<VirtualKey>> {
    sqlx::query_as!(
        VirtualKey,
        r#"SELECT k.id, k.name, k.key_hash, k.key_prefix, k.scope AS "scope: KeyScope", k.project_id,
                  coalesce(p.enabled, true) AS "project_active!", k.allowed_models, k.allowed_tags, k.rpm_limit, k.tpm_limit,
                  k.max_budget_usd, k.budget_period, k.spend_usd, k.spend_reset_at, k.enabled, k.expires_at, k.metadata,
                  k.guardrail_ids, k.created_at, k.updated_at
           FROM virtual_keys k LEFT JOIN projects p ON p.id = k.project_id WHERE k.id = $1"#,
        id
    )
    .fetch_optional(pool)
    .await
}

pub async fn insert_key(pool: &PgPool, k: &VirtualKey) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO virtual_keys (id, name, key_hash, key_prefix, allowed_models, allowed_tags, rpm_limit, tpm_limit,
             max_budget_usd, budget_period, spend_usd, enabled, expires_at, metadata, scope, project_id, guardrail_ids, created_at, updated_at)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,0,$11,$12,$13,$14,$15,$16,now(),now())",
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
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn update_key(db: impl PgExecutor<'_>, k: &VirtualKey) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE virtual_keys SET name = $2, allowed_models = $3, allowed_tags = $4, rpm_limit = $5, tpm_limit = $6,
             max_budget_usd = $7, budget_period = $8, enabled = $9, expires_at = $10, metadata = $11, scope = $12,
             project_id = $13, guardrail_ids = $14, updated_at = now()
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
    let r = sqlx::query!(
        "UPDATE virtual_keys SET key_hash = $2, key_prefix = $3, updated_at = now() WHERE id = $1",
        id,
        key_hash,
        key_prefix
    )
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
    }

    let mut tx = pool.begin().await?;
    // Nullable columns are passed as arrays with NULL elements; `as _` skips the macro's
    // element-type check, which does not model nullable array elements.
    sqlx::query!(
        "INSERT INTO request_logs (id, ts, key_id, model_name, deployment_id, provider, api, stream,
             status_code, latency_ms, ttft_ms, input_tokens, output_tokens, cache_read_tokens,
             cache_write_tokens, cost_usd, attempts, error, parent_id, project_id, labels, guardrails, guardrail_cost_usd)
         SELECT * FROM UNNEST($1::uuid[], $2::timestamptz[], $3::uuid[], $4::text[], $5::uuid[],
             $6::text[], $7::text[], $8::bool[], $9::int[], $10::int[], $11::int[], $12::bigint[],
             $13::bigint[], $14::bigint[], $15::bigint[], $16::float8[], $17::int[], $18::text[], $19::uuid[],
             $20::uuid[], $21::jsonb[], $22::jsonb[], $23::float8[])",
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

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct LogRow {
    pub id: Uuid,
    pub ts: DateTime<Utc>,
    pub key_id: Option<Uuid>,
    pub key_name: Option<String>,
    pub model_name: String,
    pub deployment_id: Option<Uuid>,
    pub provider: Option<String>,
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
}

/// `labels` (a JSON object) keeps the entries that have all of these labels. `deleted_keys` keeps
/// those of keys that no longer exist: the log keeps their id.
pub async fn recent_logs(
    pool: &PgPool,
    limit: i64,
    model: Option<&str>,
    key_id: Option<Uuid>,
    deleted_keys: bool,
    errors_only: bool,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
    projects: Option<&[Uuid]>,
    labels: Option<&serde_json::Value>,
) -> sqlx::Result<Vec<LogRow>> {
    let mut rows = sqlx::query_as!(
        LogRow,
        r#"SELECT l.id, l.ts, l.key_id, k.name AS "key_name?", l.model_name, l.deployment_id, l.provider,
                  l.api, l.stream, l.status_code, l.latency_ms, l.ttft_ms, l.input_tokens, l.output_tokens,
                  l.cache_read_tokens, l.cache_write_tokens, l.cost_usd, l.attempts, l.error, l.parent_id, l.labels,
                  l.guardrails, l.guardrail_cost_usd
           FROM request_logs l LEFT JOIN virtual_keys k ON k.id = l.key_id
           WHERE ($2::text IS NULL OR strpos(lower(l.model_name), lower($2)) > 0)
             AND ($3::uuid IS NULL OR l.key_id = $3)
             AND (NOT $4 OR l.status_code >= 400)
             AND ($5::timestamptz IS NULL OR l.ts >= $5)
             AND ($6::timestamptz IS NULL OR l.ts < $6)
             AND ($7::uuid[] IS NULL OR l.project_id = ANY($7))
             AND ($8::jsonb IS NULL OR l.labels @> $8)
             AND (NOT $9 OR (l.key_id IS NOT NULL AND k.id IS NULL))
           ORDER BY l.ts DESC LIMIT $1"#,
        limit,
        model,
        key_id,
        errors_only,
        since,
        until,
        projects,
        labels,
        deleted_keys,
    )
    .fetch_all(pool)
    .await?;
    // Router entries come with the calls they made, so the log can show them together.
    let mut seen: std::collections::HashSet<Uuid> = rows.iter().map(|r| r.id).collect();
    let parents: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
    let children = sqlx::query_as!(
        LogRow,
        r#"SELECT l.id, l.ts, l.key_id, k.name AS "key_name?", l.model_name, l.deployment_id, l.provider,
                  l.api, l.stream, l.status_code, l.latency_ms, l.ttft_ms, l.input_tokens, l.output_tokens,
                  l.cache_read_tokens, l.cache_write_tokens, l.cost_usd, l.attempts, l.error, l.parent_id, l.labels,
                  l.guardrails, l.guardrail_cost_usd
           FROM request_logs l LEFT JOIN virtual_keys k ON k.id = l.key_id
           WHERE l.parent_id = ANY($1) ORDER BY l.ts"#,
        &parents,
    )
    .fetch_all(pool)
    .await?;
    rows.extend(children.into_iter().filter(|c| seen.insert(c.id)));
    Ok(rows)
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
    /// `master_key`, `virtual_key`, `session` (a signed-in admin) or `dev` (Dev Login).
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
    pub p50_latency_ms: Option<i32>,
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
                  NULL::int AS "p50_latency_ms?", NULL::int AS "p95_latency_ms?"
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
                  percentile_disc(0.5) WITHIN GROUP (ORDER BY latency_ms) AS "p50_latency_ms?",
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
                  percentile_disc(0.5) WITHIN GROUP (ORDER BY latency_ms) AS "p50_latency_ms?",
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

/// Retention cleanup of the request log and the audit log, each by its own number of days (0
/// keeps that log forever). Returns how many entries each lost. Guarded by an advisory lock so
/// only one replica runs it at a time.
pub async fn prune_logs(pool: &PgPool, request_days: u32, audit_days: u32) -> sqlx::Result<(u64, u64)> {
    const LOCK_ID: i64 = 0x6777_6c6f_6773; // "gwlogs"
    let mut conn = pool.acquire().await?;
    let locked = sqlx::query_scalar!(r#"SELECT pg_try_advisory_lock($1) AS "locked!""#, LOCK_ID)
        .fetch_one(&mut *conn)
        .await?;
    if !locked {
        return Ok((0, 0));
    }
    let result = async {
        let requests = match request_days {
            0 => 0,
            days => {
                sqlx::query!("DELETE FROM request_logs WHERE ts < now() - make_interval(days => $1)", days as i32)
                    .execute(&mut *conn)
                    .await?
                    .rows_affected()
            }
        };
        let audit = match audit_days {
            0 => 0,
            days => {
                sqlx::query!("DELETE FROM audit_log WHERE ts < now() - make_interval(days => $1)", days as i32)
                    .execute(&mut *conn)
                    .await?
                    .rows_affected()
            }
        };
        Ok::<_, sqlx::Error>((requests, audit))
    }
    .await;
    let _ = sqlx::query_scalar!("SELECT pg_advisory_unlock($1)", LOCK_ID).fetch_one(&mut *conn).await;
    result
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
