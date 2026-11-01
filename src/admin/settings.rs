//! Gateway settings, and the cluster and instance settings they sit beside.

use axum::{Extension, Json, extract::State};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};

use super::R;
use crate::{
    audit::{self, Actor},
    db,
    error::AdminError,
    settings::Settings,
    state::AppState,
    sync::broadcast_reload,
};

pub(super) async fn cluster(State(state): State<AppState>) -> Json<Value> {
    let (instances, valkey, pg) = tokio::join!(state.cache.instances(), state.cache.ping(), db::ping(&state.db));
    Json(json!({
        "instances": instances, "valkey_ok": valkey, "postgres_ok": pg, "this_instance": state.cfg.instance_id,
        "settings": instance_settings(&state.cfg),
    }))
}

/// The settings of the instance answering, from its environment. Never secrets: only whether the
/// master key is set, and no connection URLs (they carry passwords).
fn instance_settings(cfg: &crate::config::Config) -> Value {
    json!({
        "database_max_connections": cfg.database_max_connections,
        "valkey_cluster": cfg.valkey_cluster,
        "upstream_allowed_networks": cfg.upstream_policy.networks(),
        "master_key": cfg.master_key.is_some(),
        "dev_login": cfg.dev_login,
        "cookie_secure": cfg.cookie_secure,
        "console": cfg.ui_dir.is_some(),
        "listen": cfg.listen_addr.to_string(),
    })
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct SettingsView {
    settings: Settings,
    /// What each setting is when it was never changed.
    defaults: Settings,
    /// When the settings were last saved; absent while they are all defaults.
    updated_at: Option<DateTime<Utc>>,
}

/// The stored settings. A stored document that no longer parses shows the settings in effect.
async fn settings_view(state: &AppState) -> R<SettingsView> {
    let row = db::get_gateway_settings(&state.db).await?;
    let updated_at = row.as_ref().map(|r| r.updated_at);
    let settings = row.and_then(|r| serde_json::from_value(r.settings).ok()).unwrap_or_else(|| (*state.settings.get()).clone());
    Ok(SettingsView { settings, defaults: Settings::default(), updated_at })
}

pub(super) async fn get_settings(State(state): State<AppState>) -> R<Json<SettingsView>> {
    Ok(Json(settings_view(&state).await?))
}

/// Save the settings (all of them; missing fields take their defaults) and apply them on every
/// replica.
pub(super) async fn put_settings(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(settings): Json<Settings>,
) -> R<Json<SettingsView>> {
    settings.check().map_err(AdminError::bad_request)?;
    let before = settings_view(&state).await?.settings;
    db::save_gateway_settings(&state.db, &serde_json::to_value(&settings).expect("settings serialize")).await?;
    broadcast_reload(&state, "settings").await;
    let changes = audit::changes(&before, &settings);
    if !changes.is_empty() {
        tracing::info!(changed = ?changes.keys().collect::<Vec<_>>(), "gateway settings saved");
        audit::Event::new("settings", "update").named("Gateway settings").details(changes.into()).record(&state, &actor).await;
    }
    Ok(Json(settings_view(&state).await?))
}
