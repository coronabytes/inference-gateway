//! OAuth tokens: the sign-ins of keys to upstreams (the gateway as a client), and the grants of
//! MCP clients that signed in to the gateway (the gateway as an authorization server).

use chrono::{DateTime, Utc};
use sqlx::{PgExecutor, PgPool};
use uuid::Uuid;

use crate::mcp_auth::ClientGrant;

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
