//! What a key or project spent, as budgets count it, for rebuilding Valkey's counters.

use chrono::{DateTime, Duration, DurationRound, Utc};
use serde::Deserialize;

use super::{ClickHouse, Params, Result};
use crate::budget::Spender;

impl ClickHouse {
    /// Spend since `since`: whole hours from `spend_hours`, the first, partial hour from the
    /// request log itself.
    pub async fn spend_since(&self, who: Spender, since: DateTime<Utc>) -> Result<f64> {
        #[derive(Deserialize)]
        struct Spent {
            spent: f64,
        }
        let (kind, col, id) = match who {
            Spender::Key(id) => ("key", "key_id", id),
            Spender::Project(id) => ("project", "project_id", id),
        };
        let hour = since.duration_trunc(Duration::hours(1)).unwrap_or(since);
        let whole = if hour == since { since } else { hour + Duration::hours(1) };
        let sql = format!(
            "SELECT
               (SELECT sum(cost_usd) FROM spend_hours
                WHERE kind = {{kind:String}} AND spender = {{id:UUID}} AND hour >= {{whole:DateTime('UTC')}})
             + (SELECT sum(cost_usd) FROM request_logs
                WHERE {col} = {{id:UUID}} AND parent_id IS NULL
                  AND ts >= {{since:DateTime64(3, 'UTC')}} AND ts < {{whole64:DateTime64(3, 'UTC')}}) AS spent"
        );
        let whole_s = whole.format("%Y-%m-%d %H:%M:%S").to_string();
        let p = Params::new().str("kind", kind).uuid("id", id).str("whole", &whole_s).time("since", since).time("whole64", whole);
        Ok(self.row::<Spent>(&sql, p).await?.map_or(0.0, |s| s.spent))
    }
}
