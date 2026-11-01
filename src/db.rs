//! Postgres access. Every query is checked against the schema at compile time by sqlx's
//! macros; `.sqlx/` holds the offline metadata so builds need no database
//! (regenerate with `cargo sqlx prepare` after changing a query or migration).

use sqlx::{Connection, PgPool, migrate::Migrator, postgres::PgPoolOptions};

mod audit;
mod catalog;
mod guardrails;
mod keys;
mod logs;
mod metrics;
mod oauth;
mod projects;
mod retention;
mod rotation;
mod settings;
mod upstreams;

pub use audit::*;
pub use catalog::*;
pub use guardrails::*;
pub use keys::*;
pub use logs::*;
pub use metrics::*;
pub use oauth::*;
pub use projects::*;
pub use retention::*;
pub use rotation::*;
pub use settings::*;
pub use upstreams::*;

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
    retention::ensure_log_partitions(&pool).await?;
    Ok(pool)
}

static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// One page of a list: at most `limit` rows (all when `None`) after skipping `offset`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Page {
    pub limit: Option<i64>,
    pub offset: i64,
}

/// A list filter on a text array column (tags, projects): rows holding any of `any`, or, with
/// `none`, rows holding nothing. The default does not narrow the list.
#[derive(Default)]
pub struct AnyOf<T> {
    /// `None`: no filter.
    pub any: Option<Vec<T>>,
    pub none: bool,
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

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Duration, Utc};
    use uuid::Uuid;

    use super::*;

    /// A request log entry `ago` before now, for `key`, as a call of `parent` when given.
    async fn entry(pool: &PgPool, key: Uuid, ago: Duration, status: i32, cost: f64, parent: Option<Uuid>) -> (Uuid, DateTime<Utc>) {
        let (id, ts) = (Uuid::now_v7(), Utc::now() - ago);
        sqlx::query(
            "INSERT INTO request_logs (id, ts, key_id, model_name, api, stream, status_code, latency_ms, cost_usd, parent_id)
             VALUES ($1, $2, $3, 'm', 'chat', false, $4, 10, $5, $6)",
        )
        .bind(id)
        .bind(ts)
        .bind(key)
        .bind(status)
        .bind(cost)
        .bind(parent)
        .execute(pool)
        .await
        .unwrap();
        (id, ts)
    }

    #[sqlx::test]
    async fn spend_counts_a_keys_requests_since_the_start(pool: PgPool) {
        let key = Uuid::now_v7();
        let (router, _) = entry(&pool, key, Duration::hours(1), 200, 1.0, None).await;
        // A routed call is already in its router entry's cost.
        entry(&pool, key, Duration::hours(1), 200, 0.4, Some(router)).await;
        entry(&pool, key, Duration::hours(48), 200, 5.0, None).await;
        entry(&pool, Uuid::now_v7(), Duration::hours(1), 200, 7.0, None).await;
        let spent = key_spend_since(&pool, key, Utc::now() - Duration::hours(24)).await.unwrap();
        assert!((spent - 1.0).abs() < 1e-9, "{spent}");
    }

    #[sqlx::test]
    async fn the_errors_only_log_lists_failures_newest_first(pool: PgPool) {
        let key = Uuid::now_v7();
        entry(&pool, key, Duration::minutes(3), 500, 0.0, None).await;
        entry(&pool, key, Duration::minutes(2), 200, 0.0, None).await;
        entry(&pool, key, Duration::minutes(1), 429, 0.0, None).await;
        let f = LogFilter { limit: 10, errors_only: true, body_days: 7, ..Default::default() };
        let statuses: Vec<i32> = recent_logs(&pool, &f).await.unwrap().iter().map(|r| r.status_code).collect();
        assert_eq!(statuses, [429, 500]);
    }

    #[sqlx::test]
    async fn retention_removes_old_entries_with_their_bodies(pool: PgPool) {
        let key = Uuid::now_v7();
        let (old, old_ts) = entry(&pool, key, Duration::days(40), 200, 0.0, None).await;
        let (new, _) = entry(&pool, key, Duration::hours(1), 200, 0.0, None).await;
        sqlx::query("INSERT INTO request_bodies (id, ts, request_bytes, response_bytes) VALUES ($1, $2, 1, 1)")
            .bind(old)
            .bind(old_ts)
            .execute(&pool)
            .await
            .unwrap();
        let (requests, _, _) = prune_logs(&pool, 30, 0, 60).await.unwrap();
        assert_eq!(requests, 1);
        let left: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM request_logs").fetch_all(&pool).await.unwrap();
        assert_eq!(left, [new]);
        let bodies: i64 = sqlx::query_scalar("SELECT count(*) FROM request_bodies").fetch_one(&pool).await.unwrap();
        assert_eq!(bodies, 0);
    }
}
