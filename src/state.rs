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
    /// The gateway's sessions with MCP servers, and their tool lists, for `/mcp`.
    pub mcp: crate::mcp::Upstreams,
    /// A2A agents' cards, for `/a2a`.
    pub a2a: crate::a2a::Cards,
    /// Upstream OAuth tokens in memory.
    pub oauth: crate::oauth::Tokens,
    /// What MCP clients signed in to the gateway with OAuth: their documents and tokens.
    pub mcp_auth: crate::mcp_auth::Store,
    /// Memory for logged prompts and responses until they are written.
    pub bodies: crate::capture::Budget,
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
    // The receiver is dropped: records from tests go nowhere.
    test_state_with(crate::netguard::Policy::default(), mpsc::channel(16).0, None)
}

/// A test state on a real database, such as the one `#[sqlx::test]` gives each test. Valkey stays
/// closed, so what reads it falls back as when Valkey is down.
#[cfg(test)]
pub fn test_state_with_db(db: sqlx::PgPool) -> AppState {
    test_state_with(crate::netguard::Policy::default(), mpsc::channel(16).0, Some(db))
}

/// A test state whose upstreams may be on loopback, with the channel its request records arrive on.
#[cfg(test)]
pub fn test_state_local() -> (AppState, mpsc::Receiver<RequestRecord>) {
    let (records, rx) = mpsc::channel(64);
    (test_state_with(crate::netguard::Policy::from_list("127.0.0.0/8").unwrap(), records, None), rx)
}

#[cfg(test)]
fn test_state_with(upstream_policy: crate::netguard::Policy, records: mpsc::Sender<RequestRecord>, db: Option<sqlx::PgPool>) -> AppState {
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
        upstream_policy,
        prompt_log_buffer_bytes: 1 << 20,
    };
    let settings = Live::default();
    settings.set(Settings { max_attempts: 1, cooldown_secs: 1, ..Settings::default() });
    let db = db.unwrap_or_else(|| {
        sqlx::postgres::PgPoolOptions::new().acquire_timeout(Duration::from_millis(200)).connect_lazy(&cfg.database_url).unwrap()
    });
    let cache = Cache::new(&cfg.valkey_url, cfg.valkey_cluster).unwrap();
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
        mcp: crate::mcp::Upstreams::default(),
        a2a: crate::a2a::Cards::default(),
        oauth: crate::oauth::Tokens::default(),
        mcp_auth: crate::mcp_auth::Store::default(),
        bodies: crate::capture::Budget::new(1 << 20),
        started: Instant::now(),
        started_at: chrono::Utc::now(),
    })
}
