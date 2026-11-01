//! Projects, their members, and what their keys spent.

use chrono::{DateTime, Utc};
use sqlx::{PgExecutor, PgPool};
use uuid::Uuid;

use crate::model::{ProjectRow, PromptLogging};

use super::{NamedItem, Page};

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

/// Projects by name, with how many keys each groups: those whose name or description contains
/// `search` (lower case), among `projects` (a user's; `None` for all).
/// Projects by name, with how many keys each groups: those `search` matches, among `projects` when
/// given, and of those the ones in `ids` when given, one `page` of them.
pub async fn list_projects(
    pool: &PgPool,
    search: Option<&str>,
    projects: Option<&[Uuid]>,
    ids: Option<&[Uuid]>,
    page: Page,
) -> sqlx::Result<Vec<(ProjectRow, i64)>> {
    let rows = sqlx::query!(
        r#"SELECT p.id, p.name, p.description, p.enabled, p.max_budget_usd, p.budget_period, p.spend_reset_at,
                  p.guardrail_ids, p.web_search, p.prompt_logging AS "prompt_logging: PromptLogging", p.created_at, p.updated_at,
                  count(k.id) AS "keys!"
           FROM projects p LEFT JOIN virtual_keys k ON k.project_id = p.id
           WHERE ($1::text IS NULL OR strpos(lower(p.name || ' ' || coalesce(p.description, '')), $1) > 0)
             AND ($2::uuid[] IS NULL OR p.id = ANY($2))
             AND ($3::uuid[] IS NULL OR p.id = ANY($3))
           GROUP BY p.id ORDER BY lower(p.name), p.id
           LIMIT $4 OFFSET $5"#,
        search,
        projects,
        ids,
        page.limit,
        page.offset,
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
