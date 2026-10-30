//! The audit log: who changed what through the admin API, kept in Postgres (`audit_log`). An
//! entry is written once a change has succeeded; a failed write goes to the gateway log and does
//! not undo the change.

use serde::Serialize;
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::{db, state::AppState};

/// Who made an admin API call. `require_admin` puts it in the request's extensions.
#[derive(Clone, Debug)]
pub struct Actor {
    /// `master_key`, `virtual_key`, `session`, `dev` or `system`.
    pub kind: &'static str,
    /// The admin user or the virtual key.
    pub id: Option<Uuid>,
    pub name: Option<String>,
}

impl Actor {
    pub fn master_key() -> Self {
        Self { kind: "master_key", id: None, name: Some("Master key".into()) }
    }

    pub fn virtual_key(id: Uuid, name: &str) -> Self {
        Self { kind: "virtual_key", id: Some(id), name: Some(name.to_string()) }
    }

    pub fn admin(id: Uuid, email: Option<&str>, name: Option<&str>) -> Self {
        Self { kind: "session", id: Some(id), name: email.or(name).map(str::to_string) }
    }

    pub fn dev() -> Self {
        Self { kind: "dev", id: None, name: Some("Local development".into()) }
    }

    /// The gateway itself, for scheduled work.
    pub fn system() -> Self {
        Self { kind: "system", id: None, name: Some("Gateway".into()) }
    }
}

/// One entry, built up and then recorded: `Event::new("key", "create").on(id, name).record(..)`.
pub struct Event {
    target_type: &'static str,
    action: &'static str,
    target_id: Option<Uuid>,
    target_name: Option<String>,
    details: Value,
}

impl Event {
    pub fn new(target_type: &'static str, action: &'static str) -> Self {
        Self { target_type, action, target_id: None, target_name: None, details: Value::Object(Map::new()) }
    }

    pub fn on(mut self, id: Uuid, name: impl Into<String>) -> Self {
        self.target_id = Some(id);
        self.target_name = Some(name.into());
        self
    }

    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.target_name = Some(name.into());
        self
    }

    pub fn details(mut self, details: Value) -> Self {
        self.details = details;
        self
    }

    pub async fn record(self, state: &AppState, actor: &Actor) {
        let entry = db::AuditEntry {
            id: Uuid::now_v7(),
            ts: chrono::Utc::now(),
            actor_kind: actor.kind.to_string(),
            actor_id: actor.id,
            actor_name: actor.name.clone(),
            action: self.action.to_string(),
            target_type: self.target_type.to_string(),
            target_id: self.target_id,
            target_name: self.target_name,
            details: self.details,
        };
        if let Err(e) = db::insert_audit(&state.db, &entry).await {
            tracing::warn!(error = %e, action = %entry.action, target = %entry.target_type, "could not write the audit log");
        }
    }
}

/// Bookkeeping fields that change without anyone editing them.
const NOT_EDITS: [&str; 5] = ["created_at", "updated_at", "spend_usd", "spend_reset_at", "deployments"];

/// The object as an audit entry's details, without bookkeeping fields.
pub fn snapshot(v: &impl Serialize) -> Value {
    let mut v = serde_json::to_value(v).unwrap_or_default();
    if let Value::Object(m) = &mut v {
        m.retain(|k, _| !NOT_EDITS.contains(&k.as_str()));
    }
    v
}

/// The fields that differ between two versions of an object, as `{"a.b": {"from": .., "to": ..}}`.
/// Nested objects are compared field by field; arrays and other values as a whole.
pub fn changes(before: &impl Serialize, after: &impl Serialize) -> Map<String, Value> {
    let (before, after) = (snapshot(before), snapshot(after));
    let mut out = Map::new();
    diff("", &before, &after, &mut out);
    out
}

fn diff(path: &str, a: &Value, b: &Value, out: &mut Map<String, Value>) {
    match (a, b) {
        (Value::Object(a), Value::Object(b)) => {
            let null = Value::Null;
            let keys: std::collections::BTreeSet<&String> = a.keys().chain(b.keys()).collect();
            for k in keys {
                let p = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
                diff(&p, a.get(k).unwrap_or(&null), b.get(k).unwrap_or(&null), out);
            }
        }
        _ if a != b => {
            out.insert(path.to_string(), serde_json::json!({ "from": a, "to": b }));
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn changes_name_the_edited_fields() {
        let before = json!({ "name": "a", "models": ["x"], "config": { "effort": "low", "keep": 1 }, "updated_at": 1 });
        let after = json!({ "name": "a", "models": ["x", "y"], "config": { "effort": "high", "keep": 1 }, "updated_at": 2 });
        assert_eq!(
            Value::Object(changes(&before, &after)),
            json!({
                "config.effort": { "from": "low", "to": "high" },
                "models": { "from": ["x"], "to": ["x", "y"] },
            })
        );
        assert!(changes(&before, &before).is_empty());
    }
}
