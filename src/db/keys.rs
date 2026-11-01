//! Virtual keys, and what they spent.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{PgExecutor, PgPool};
use uuid::Uuid;

use crate::model::{KeyScope, VirtualKey};

use super::{AnyOf, Page};

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
    page: Page,
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
           ORDER BY lower(p.name) NULLS LAST, p.id, k.created_at DESC, k.id
           LIMIT $9 OFFSET $10"#,
        search,
        tags.any.as_deref(),
        tags.none,
        projects.any.as_deref(),
        projects.none,
        visible,
        logging,
        gateway_allows,
        page.limit,
        page.offset,
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
    #[schema(nullable = false)]
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
