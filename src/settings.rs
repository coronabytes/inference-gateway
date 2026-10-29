//! Gateway settings: one JSON document in Postgres (`gateway_settings`), edited on the console's
//! Cluster page. Every replica keeps the current settings in memory. A change is saved and then
//! announced on the cluster sync channel like other configuration, and the periodic reload picks it
//! up on a replica that missed the announcement. Fields missing from the document take their
//! defaults.
//!
//! What a replica needs before it can reach Postgres, its secrets and its security boundaries stay
//! environment variables (see `config`).

use std::{sync::Arc, time::Duration};

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::db;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(default)]
pub struct Settings {
    /// Deployments tried per request on rate limits, server errors and timeouts.
    pub max_attempts: u32,
    /// How long a failing deployment is skipped.
    pub cooldown_secs: u64,
    /// Deployments get less traffic while their recent calls fail or are slow (see `router`).
    /// Off, requests spread by the configured weights only.
    pub adaptive_routing: bool,
    /// Conversations stay on the deployment, and on an auto router the route, that served them
    /// (see `affinity`). Off, every request is balanced on its own.
    pub sticky_routing: bool,
    /// Periodic full reload of the configuration from Postgres, in addition to the instant
    /// updates over Valkey: it covers updates a replica missed.
    pub reload_secs: u64,
    /// Days request logs, and so metrics, are kept: 7 to 1080, or 0 to keep them forever.
    pub request_log_retention_days: u32,
    /// Days audit log entries are kept: 7 to 1080, or 0 to keep them forever. Independent of the
    /// request log's.
    pub audit_log_retention_days: u32,
    /// How long a `previous_response_id` conversation can go unused and still be continued; each
    /// continuation starts the time again.
    pub response_ttl_minutes: u64,
    /// Conversation history one virtual key may keep in Valkey: the size of its conversations that
    /// have not expired. Valkey also holds rate limits, budgets and sessions.
    pub conversation_quota_mb: u64,
    /// Largest request body.
    pub max_body_mb: u64,
    /// Time a client has to send its request headers.
    pub header_timeout_secs: u64,
    /// Time a client has to send its whole request body. Responses are not limited.
    pub body_timeout_secs: u64,
    /// Open client connections; more wait in the listen backlog.
    pub max_connections: u32,
    /// The OpenAPI document at `/admin/openapi.json` and the API reference at `/admin/docs/`, for
    /// signed-in people and admin and project keys.
    pub api_docs: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            cooldown_secs: 30,
            adaptive_routing: true,
            sticky_routing: true,
            reload_secs: 60,
            request_log_retention_days: 90,
            audit_log_retention_days: 90,
            response_ttl_minutes: 15,
            conversation_quota_mb: 64,
            max_body_mb: 32,
            header_timeout_secs: 30,
            body_timeout_secs: 120,
            max_connections: 10_000,
            api_docs: true,
        }
    }
}

/// Log retention in days when it is not 0 (forever). Neither log can be turned off.
pub const RETENTION_DAYS: std::ops::RangeInclusive<u32> = 7..=1080;

impl Settings {
    /// Each number within its range.
    pub fn check(&self) -> Result<(), String> {
        let within = |name: &str, v: u64, min: u64, max: u64| match v {
            v if (min..=max).contains(&v) => Ok(()),
            _ => Err(format!("{name} must be between {min} and {max}")),
        };
        let retention = |name: &str, days: u32| match days {
            0 => Ok(()),
            d if RETENTION_DAYS.contains(&d) => Ok(()),
            _ => Err(format!("{name} must be 0 (kept forever) or between {} and {}", RETENTION_DAYS.start(), RETENTION_DAYS.end())),
        };
        within("max_attempts", self.max_attempts.into(), 1, 10)?;
        within("cooldown_secs", self.cooldown_secs, 0, 3600)?;
        within("reload_secs", self.reload_secs, 5, 3600)?;
        retention("request_log_retention_days", self.request_log_retention_days)?;
        retention("audit_log_retention_days", self.audit_log_retention_days)?;
        within("response_ttl_minutes", self.response_ttl_minutes, 1, 7 * 24 * 60)?;
        within("conversation_quota_mb", self.conversation_quota_mb, 0, 16 * 1024)?;
        within("max_body_mb", self.max_body_mb, 1, 1024)?;
        within("header_timeout_secs", self.header_timeout_secs, 1, 600)?;
        within("body_timeout_secs", self.body_timeout_secs, 1, 3600)?;
        within("max_connections", self.max_connections.into(), 1, 1_000_000)
    }

    /// Retention saved before `RETENTION_DAYS` applied (anything from 1 to 3650 days) is moved
    /// into it, so the rest of a stored document still takes effect.
    fn fit_retention(mut self) -> Self {
        for days in [&mut self.request_log_retention_days, &mut self.audit_log_retention_days] {
            if *days != 0 {
                *days = (*days).clamp(*RETENTION_DAYS.start(), *RETENTION_DAYS.end());
            }
        }
        self
    }

    pub fn cooldown(&self) -> Duration {
        Duration::from_secs(self.cooldown_secs)
    }

    pub fn reload_interval(&self) -> Duration {
        Duration::from_secs(self.reload_secs.max(5))
    }

    pub fn response_ttl(&self) -> Duration {
        Duration::from_secs(self.response_ttl_minutes.max(1) * 60)
    }

    pub fn conversation_quota_bytes(&self) -> u64 {
        self.conversation_quota_mb * 1024 * 1024
    }

    pub fn max_body_bytes(&self) -> usize {
        self.max_body_mb as usize * 1024 * 1024
    }

    pub fn header_timeout(&self) -> Duration {
        Duration::from_secs(self.header_timeout_secs.max(1))
    }

    pub fn body_timeout(&self) -> Duration {
        Duration::from_secs(self.body_timeout_secs.max(1))
    }
}

/// The settings in effect on this replica.
pub struct Live(watch::Sender<Arc<Settings>>);

impl Default for Live {
    fn default() -> Self {
        Self(watch::Sender::new(Arc::default()))
    }
}

impl Live {
    pub fn get(&self) -> Arc<Settings> {
        self.0.borrow().clone()
    }

    /// Sees every change, for what holds on to a setting (the server's connection limit).
    pub fn subscribe(&self) -> watch::Receiver<Arc<Settings>> {
        self.0.subscribe()
    }

    /// Read the stored settings. A document that does not parse, or is out of range, is reported
    /// and the settings in effect stay.
    pub async fn reload(&self, db: &sqlx::PgPool) -> sqlx::Result<()> {
        let stored = match db::get_gateway_settings(db).await? {
            Some(row) => serde_json::from_value::<Settings>(row.settings)
                .map_err(|e| e.to_string())
                .map(Settings::fit_retention)
                .and_then(|s| s.check().map(|()| s)),
            None => Ok(Settings::default()),
        };
        match stored {
            Ok(s) => self.set(s),
            Err(e) => tracing::error!(error = %e, "stored gateway settings are invalid; keeping the ones in effect"),
        }
        Ok(())
    }

    pub fn set(&self, s: Settings) {
        self.0.send_if_modified(|cur| {
            let changed = **cur != s;
            if changed {
                tracing::info!("gateway settings changed");
                *cur = Arc::new(s);
            }
            changed
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_take_their_defaults() {
        let s: Settings = serde_json::from_value(serde_json::json!({ "max_attempts": 5, "retired_field": 1 })).unwrap();
        assert_eq!(s.max_attempts, 5);
        assert_eq!(s.max_body_mb, Settings::default().max_body_mb);
        assert!(Settings::default().check().is_ok());
        let zero = Settings { max_attempts: 0, ..Settings::default() };
        assert_eq!(zero.check().unwrap_err(), "max_attempts must be between 1 and 10");
    }

    #[test]
    fn retention_is_forever_or_7_to_1080_days() {
        for days in [0, 7, 90, 1080] {
            assert!(Settings { request_log_retention_days: days, audit_log_retention_days: days, ..Settings::default() }.check().is_ok(), "{days}");
        }
        for days in [1, 6, 1081, 3650] {
            let s = Settings { audit_log_retention_days: days, ..Settings::default() };
            assert_eq!(s.check().unwrap_err(), "audit_log_retention_days must be 0 (kept forever) or between 7 and 1080");
        }
        let old = Settings { request_log_retention_days: 3650, audit_log_retention_days: 1, ..Settings::default() }.fit_retention();
        assert_eq!((old.request_log_retention_days, old.audit_log_retention_days), (1080, 7));
        assert_eq!(Settings { request_log_retention_days: 0, ..Settings::default() }.fit_retention().request_log_retention_days, 0);
    }

    #[test]
    fn only_changes_are_announced() {
        let live = Live::default();
        let rx = live.subscribe();
        live.set(Settings::default());
        assert!(!rx.has_changed().unwrap());
        live.set(Settings { sticky_routing: false, ..Settings::default() });
        assert!(rx.has_changed().unwrap());
        assert!(!live.get().sticky_routing);
    }
}
