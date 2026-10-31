//! Cluster coordination over Valkey.
//!
//! Admin writes go to Postgres first, then a command is published on [`SYNC_CHANNEL`]; every
//! replica (including the writer) reloads its in-memory registry and the gateway settings. A
//! periodic full reload covers messages missed during Valkey disconnects.

use std::time::Duration;

use futures::StreamExt;
use serde::{Deserialize, Serialize};

use crate::{cache::SYNC_CHANNEL, state::AppState};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum SyncCommand {
    /// Configuration or settings changed; reload from Postgres.
    Reload { origin: String, reason: String },
}

/// The registry and the settings, from Postgres.
pub async fn reload(state: &AppState) -> sqlx::Result<()> {
    state.settings.reload(&state.db).await?;
    state.registry.reload(&state.db, &state.cfg).await
}

/// Reload locally, then tell the other replicas.
pub async fn broadcast_reload(state: &AppState, reason: &str) {
    if let Err(e) = reload(state).await {
        tracing::error!(error = %e, "local reload failed");
    }
    let cmd = SyncCommand::Reload { origin: state.cfg.instance_id.clone(), reason: reason.to_string() };
    state.cache.publish(&serde_json::to_string(&cmd).expect("serialize")).await;
}

pub async fn run_subscriber(state: AppState) {
    loop {
        match subscribe_once(&state).await {
            Ok(()) => tracing::warn!("valkey sync subscription ended; reconnecting"),
            Err(e) => tracing::warn!(error = %e, "valkey sync subscription failed; retrying"),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        // We may have missed messages while disconnected.
        if let Err(e) = reload(&state).await {
            tracing::error!(error = %e, "reload after resubscribe failed");
        }
    }
}

async fn subscribe_once(state: &AppState) -> redis::RedisResult<()> {
    let mut messages = state.cache.subscribe(SYNC_CHANNEL).await?;
    tracing::info!(channel = SYNC_CHANNEL, "subscribed to cluster sync channel");
    while let Some(msg) = messages.next().await {
        let payload: String = match msg.get_payload() {
            Ok(p) => p,
            Err(_) => continue,
        };
        match serde_json::from_str::<SyncCommand>(&payload) {
            Ok(SyncCommand::Reload { origin, reason }) => {
                if origin == state.cfg.instance_id {
                    continue;
                }
                tracing::info!(%origin, %reason, "reloading registry on peer request");
                if let Err(e) = reload(state).await {
                    tracing::error!(error = %e, "reload failed");
                }
            }
            Err(e) => tracing::warn!(error = %e, "ignoring malformed sync message"),
        }
    }
    Ok(())
}

pub async fn run_periodic_reload(state: AppState) {
    loop {
        // The period is a setting, so it is read again each time.
        tokio::time::sleep(state.settings.get().reload_interval()).await;
        if let Err(e) = reload(&state).await {
            tracing::error!(error = %e, "periodic reload failed");
        }
    }
}

pub async fn run_heartbeat(state: AppState) {
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    loop {
        tick.tick().await;
        let snap = state.registry.snapshot();
        let info = serde_json::json!({
            "id": state.cfg.instance_id,
            "version": crate::VERSION,
            "started_at": state.started_at,
            "uptime_secs": state.started.elapsed().as_secs(),
            "deployments": snap.deployments.values().map(Vec::len).sum::<usize>(),
            "keys": snap.keys.len(),
            "registry_loaded_at": snap.loaded_at,
            "registry_errors": snap.errors.len(),
            "last_seen": chrono::Utc::now(),
        });
        state.cache.heartbeat(&state.cfg.instance_id, &info.to_string(), 20).await;
    }
}

pub async fn run_maintenance(state: AppState) {
    let mut tick = tokio::time::interval(Duration::from_secs(3600));
    loop {
        tick.tick().await;
        let settings = state.settings.get();
        let (request_days, audit_days) = (settings.request_log_retention_days, settings.audit_log_retention_days);
        // Logged bodies are always pruned, so this runs whatever the other two retentions are.
        match crate::db::prune_logs(&state.db, request_days, audit_days, settings.prompt_log_retention_days).await {
            Ok((requests, audit, bodies)) => {
                if requests > 0 {
                    tracing::info!(deleted = requests, "pruned old request logs");
                }
                if audit > 0 {
                    tracing::info!(deleted = audit, "pruned old audit log entries");
                }
                if bodies > 0 {
                    tracing::info!(deleted = bodies, "pruned old logged prompts and responses");
                }
            }
            Err(e) => tracing::warn!(error = %e, "log pruning failed"),
        }
        // Sign-ins to MCP servers and A2A agents unused for longer than their upstream keeps them.
        // Replicas that hold one in memory stop using it within five minutes.
        match crate::db::prune_sign_ins(&state.db, crate::oauth::SIGN_IN_RETENTION_DAYS as i32).await {
            Ok(n) if n > 0 => tracing::info!(deleted = n, "removed unused upstream sign-ins"),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "removing unused upstream sign-ins failed"),
        }
    }
}
