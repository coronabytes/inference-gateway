//! Gateway settings, the OIDC sign-in settings, and the people who sign in to the console.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::model::{AdminUserRow, OidcSettingsRow, PersonRole};

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
