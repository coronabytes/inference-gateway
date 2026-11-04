//! Retention in ClickHouse: the request log and logged bodies lose whole partitions (weeks and
//! days) once all of a partition is older than its retention; the audit log deletes by row.

use chrono::{Duration, NaiveDate, Utc};
use serde::Deserialize;

use super::{ClickHouse, Params, Result};

#[derive(Deserialize)]
struct Part {
    partition_id: String,
    rows: u64,
}

impl ClickHouse {
    /// Drops `table`'s partitions that end on or before `cutoff`; a partition starting on day
    /// `d` ends `span` days later. Returns how many rows went.
    async fn drop_before(&self, table: &str, span: i64, cutoff: NaiveDate) -> Result<u64> {
        let parts: Vec<Part> = self
            .rows(
                "SELECT partition_id, sum(rows) AS rows FROM system.parts
                 WHERE database = currentDatabase() AND table = {t:String} AND active GROUP BY partition_id",
                Params::new().str("t", table),
            )
            .await?;
        let mut dropped = 0;
        for p in parts {
            let Ok(start) = NaiveDate::parse_from_str(&p.partition_id, "%Y%m%d") else { continue };
            if start + Duration::days(span) <= cutoff {
                // The id is ClickHouse's own and only digits (checked by the parse above).
                self.execute(&format!("ALTER TABLE {table} DROP PARTITION ID '{}'", p.partition_id), Params::new()).await?;
                dropped += p.rows;
            }
        }
        Ok(dropped)
    }
}

/// Retention of the three logs, each by its own number of days (0 keeps that log forever;
/// bodies always go). Returns how many entries each lost. The caller keeps it to one replica.
pub async fn prune(ch: &ClickHouse, request_days: u32, audit_days: u32, body_days: u32) -> Result<(u64, u64, u64)> {
    let today = Utc::now().date_naive();
    let requests = match request_days {
        0 => 0,
        d => ch.drop_before("request_logs", 7, today - Duration::days(d as i64)).await?,
    };
    let bodies = ch.drop_before("request_bodies", 1, today - Duration::days(body_days as i64)).await?;
    let audit = match audit_days {
        0 => 0,
        d => {
            #[derive(Deserialize)]
            struct N {
                n: u64,
            }
            let cutoff = Utc::now() - Duration::days(d as i64);
            let p = || Params::new().time("cutoff", cutoff);
            let n =
                ch.row::<N>("SELECT count() AS n FROM audit_log WHERE ts < {cutoff:DateTime64(3, 'UTC')}", p()).await?.map_or(0, |n| n.n);
            if n > 0 {
                ch.execute("DELETE FROM audit_log WHERE ts < {cutoff:DateTime64(3, 'UTC')}", p()).await?;
            }
            n
        }
    };
    Ok((requests, audit, bodies))
}
