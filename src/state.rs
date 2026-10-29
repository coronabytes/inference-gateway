use std::{sync::Arc, time::Instant};

use tokio::sync::mpsc;

use crate::{accounting::RequestRecord, cache::Cache, config::Config, registry::Registry};

#[derive(Clone)]
pub struct AppState(Arc<Inner>);

pub struct Inner {
    pub cfg: Config,
    /// The gateway settings in effect, from Postgres.
    pub settings: crate::settings::Live,
    pub db: sqlx::PgPool,
    pub cache: Cache,
    pub registry: Registry,
    pub aws: crate::aws::Aws,
    pub google: crate::google::Google,
    pub entra: crate::entra::Entra,
    pub http: reqwest::Client,
    pub records: mpsc::Sender<RequestRecord>,
    pub started: Instant,
    pub started_at: chrono::DateTime<chrono::Utc>,
}

impl AppState {
    pub fn new(inner: Inner) -> Self {
        Self(Arc::new(inner))
    }
}

impl std::ops::Deref for AppState {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}

/// An app state for tests that never connects anywhere: Postgres and Valkey handles are lazy
/// and point at closed ports, so anything that does reach them fails (and Valkey fails open).
#[cfg(test)]
pub fn test_state() -> AppState {
    use std::time::Duration;

    use base64::Engine;

    use crate::settings::{Live, Settings};

    let cfg = Config {
        listen_addr: ([127, 0, 0, 1], 0).into(),
        database_url: "postgres://test@127.0.0.1:1/test".into(),
        database_max_connections: 1,
        valkey_url: "redis://127.0.0.1:1".into(),
        valkey_cluster: false,
        master_key: None,
        secret_box: crate::crypto::SecretBox::from_base64(&base64::engine::general_purpose::STANDARD.encode([7u8; 32])).unwrap(),
        ui_dir: None,
        affinity_salt: [9; 32],
        instance_id: "test".into(),
        dev_login: false,
        cookie_secure: true,
        upstream_policy: crate::netguard::Policy::default(),
    };
    let settings = Live::default();
    settings.set(Settings { max_attempts: 1, cooldown_secs: 1, ..Settings::default() });
    let db = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(200))
        .connect_lazy(&cfg.database_url)
        .unwrap();
    let cache = Cache::new(&cfg.valkey_url, cfg.valkey_cluster).unwrap();
    // The receiver is dropped: records from tests go nowhere.
    let (records, _) = mpsc::channel(16);
    AppState::new(Inner {
        cfg,
        settings,
        db,
        cache,
        registry: Registry::new(),
        aws: crate::aws::Aws::default(),
        google: crate::google::Google::default(),
        entra: crate::entra::Entra::default(),
        http: reqwest::Client::new(),
        records,
        started: Instant::now(),
        started_at: chrono::Utc::now(),
    })
}
