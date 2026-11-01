//! The sealed columns, for key rotation: each read with a row lock, so the rotation transaction
//! sees them unchanged, and written back sealed with the new key.

use sqlx::PgExecutor;
use uuid::Uuid;

use super::SignInRef;

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

pub async fn sealed_mcp_secrets(db: impl PgExecutor<'_>) -> sqlx::Result<Vec<(Uuid, String, Vec<u8>)>> {
    let rows =
        sqlx::query!(r#"SELECT id, name, secret AS "secret!" FROM mcp_servers WHERE secret IS NOT NULL FOR UPDATE"#).fetch_all(db).await?;
    Ok(rows.into_iter().map(|r| (r.id, r.name, r.secret)).collect())
}

pub async fn set_mcp_secret(db: impl PgExecutor<'_>, id: Uuid, secret: &[u8]) -> sqlx::Result<()> {
    sqlx::query!("UPDATE mcp_servers SET secret = $1 WHERE id = $2", secret, id).execute(db).await?;
    Ok(())
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
