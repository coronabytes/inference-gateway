//! The audit log in ClickHouse. Mirrors `db::audit`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{ClickHouse, Result, logs::Where, time};
use crate::db::AuditEntry;

#[derive(Serialize)]
struct Row {
    id: Uuid,
    ts: String,
    actor_kind: String,
    actor_id: Option<Uuid>,
    actor_name: Option<String>,
    action: String,
    target_type: String,
    target_id: Option<Uuid>,
    target_name: Option<String>,
    details: String,
}

#[derive(Deserialize)]
struct Read {
    id: Uuid,
    ts: DateTime<Utc>,
    actor_kind: String,
    actor_id: Option<Uuid>,
    actor_name: Option<String>,
    action: String,
    target_type: String,
    target_id: Option<Uuid>,
    target_name: Option<String>,
    details: String,
}

impl ClickHouse {
    /// One entry. Admin changes come one at a time, so ClickHouse gathers them into parts.
    pub async fn insert_audit(&self, e: &AuditEntry) -> Result<()> {
        let row = Row {
            id: e.id,
            ts: time(e.ts),
            actor_kind: e.actor_kind.clone(),
            actor_id: e.actor_id,
            actor_name: e.actor_name.clone(),
            action: e.action.clone(),
            target_type: e.target_type.clone(),
            target_id: e.target_id,
            target_name: e.target_name.clone(),
            details: e.details.to_string(),
        };
        self.insert("audit_log", &[row], true).await
    }

    /// Newest first. `search` matches the target's or the actor's name, ignoring case.
    pub async fn list_audit(
        &self,
        limit: i64,
        target_type: Option<&str>,
        search: Option<&str>,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
    ) -> Result<Vec<AuditEntry>> {
        let mut w = Where::new();
        if let Some(t) = target_type {
            w.and("target_type = {tt:String}");
            w.with(|p| p.str("tt", t));
        }
        if let Some(s) = search {
            w.and("positionCaseInsensitiveUTF8(concat(ifNull(target_name, ''), ' ', ifNull(actor_name, '')), {q:String}) > 0");
            w.with(|p| p.str("q", s));
        }
        w.range("ts", since, until);
        let sql = format!(
            "SELECT id, ts, actor_kind, actor_id, actor_name, action, target_type, target_id, target_name, details
             FROM audit_log WHERE {} ORDER BY ts DESC LIMIT {{limit:UInt32}}",
            w.sql()
        );
        let rows: Vec<Read> = self.rows(&sql, w.into_params().num("limit", limit.max(0))).await?;
        Ok(rows
            .into_iter()
            .map(|r| AuditEntry {
                id: r.id,
                ts: r.ts,
                actor_kind: r.actor_kind,
                actor_id: r.actor_id,
                actor_name: r.actor_name,
                action: r.action,
                target_type: r.target_type,
                target_id: r.target_id,
                target_name: r.target_name,
                details: serde_json::from_str(&r.details).unwrap_or_default(),
            })
            .collect())
    }
}
