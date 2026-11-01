//! The audit log: who changed what, and who signed in (see `audit`).

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

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
