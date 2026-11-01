//! The model catalog: credentials, deployments, auto routers, model aliases, the models'
//! metadata, and the price catalog deployments take their prices from.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{PgExecutor, PgPool, types::Json};
use uuid::Uuid;

use crate::{
    model::{AliasRow, CredentialRow, CredentialSettings, DeploymentConfig, DeploymentRow, Route, RouterRow},
    prices,
};

use super::AnyOf;

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
