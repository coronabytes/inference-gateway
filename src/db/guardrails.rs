//! Guardrails, and the projects and keys that apply them.

use std::collections::HashMap;

use sqlx::{PgExecutor, PgPool, types::Json};
use uuid::Uuid;

use crate::guardrails::{GuardrailConfig, GuardrailPrices, GuardrailRow};

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
