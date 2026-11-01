//! Retention of the request log, the audit log and stored bodies, and the request log's daily
//! partitions.

use sqlx::{PgExecutor, PgPool};

/// Days of request log partitions kept ready ahead of today (see migration 0016).
const LOG_PARTITIONS_AHEAD: i32 = 7;

/// Creates the request log's daily partitions from today to `LOG_PARTITIONS_AHEAD` days on, so
/// entries always have one to go to. Run at startup and with every retention run.
pub(super) async fn ensure_log_partitions(db: impl PgExecutor<'_>) -> sqlx::Result<()> {
    sqlx::query_scalar!("SELECT gw_request_log_partitions($1)", LOG_PARTITIONS_AHEAD).fetch_one(db).await?;
    Ok(())
}

/// Retention cleanup of the request log, the audit log and logged bodies, each by its own number
/// of days (0 keeps that log forever; bodies always go). Returns how many entries each lost.
/// Guarded by an advisory lock so only one replica runs it at a time.
pub async fn prune_logs(pool: &PgPool, request_days: u32, audit_days: u32, body_days: u32) -> sqlx::Result<(u64, u64, u64)> {
    const LOCK_ID: i64 = 0x6777_6c6f_6773; // "gwlogs"
    let mut conn = pool.acquire().await?;
    let locked = sqlx::query_scalar!(r#"SELECT pg_try_advisory_lock($1) AS "locked!""#, LOCK_ID).fetch_one(&mut *conn).await?;
    if !locked {
        return Ok((0, 0, 0));
    }
    let result = async {
        ensure_log_partitions(&mut *conn).await?;
        // Whole days drop with their partition; what remains older (the legacy partition, and
        // the start of the newest expired day) is deleted in chunks, so one delete does not hold
        // locks long: each entry updates several indexes, and its bodies go with it.
        let mut requests = 0;
        if request_days > 0 {
            let dropped =
                sqlx::query_scalar!("SELECT gw_drop_request_log_partitions(now() - make_interval(days => $1))", request_days as i32)
                    .fetch_one(&mut *conn)
                    .await?;
            requests += dropped.unwrap_or(0) as u64;
            loop {
                let n = sqlx::query!(
                    "DELETE FROM request_logs WHERE id IN (
                     SELECT id FROM request_logs WHERE ts < now() - make_interval(days => $1) LIMIT 5000)",
                    request_days as i32
                )
                .execute(&mut *conn)
                .await?
                .rows_affected();
                requests += n;
                if n < 5000 {
                    break;
                }
            }
        }
        let audit = match audit_days {
            0 => 0,
            days => sqlx::query!("DELETE FROM audit_log WHERE ts < now() - make_interval(days => $1)", days as i32)
                .execute(&mut *conn)
                .await?
                .rows_affected(),
        };
        // In chunks: bodies are large, and a long delete would hold locks and write a burst of WAL.
        let mut bodies = 0;
        loop {
            let n = sqlx::query!(
                "DELETE FROM request_bodies WHERE id IN (
                     SELECT id FROM request_bodies WHERE ts < now() - make_interval(days => $1) LIMIT 5000)",
                body_days as i32
            )
            .execute(&mut *conn)
            .await?
            .rows_affected();
            bodies += n;
            if n < 5000 {
                break;
            }
        }
        Ok::<_, sqlx::Error>((requests, audit, bodies))
    }
    .await;
    let _ = sqlx::query_scalar!("SELECT pg_advisory_unlock($1)", LOCK_ID).fetch_one(&mut *conn).await;
    result
}
