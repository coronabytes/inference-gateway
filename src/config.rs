use std::{env, net::SocketAddr, path::PathBuf};

use crate::{crypto::SecretBox, netguard::Policy};

/// Process configuration, read once from the environment at startup: what a replica needs before
/// it can reach Postgres, its secrets, and its security boundaries. Everything else is a gateway
/// setting, kept in Postgres and changed in the console (see `settings`).
pub struct Config {
    pub listen_addr: SocketAddr,
    pub database_url: String,
    pub database_max_connections: u32,
    pub valkey_url: String,
    /// `valkey_url` lists the seed nodes of a Valkey cluster (also ElastiCache Serverless).
    pub valkey_cluster: bool,
    /// Bearer token for the admin API and fallback UI sign-in. Unset or empty disables it, leaving
    /// single sign-on (and local development) as the only ways in.
    pub master_key: Option<String>,
    pub secret_box: SecretBox,
    /// The console's files, when `GATEWAY_UI` is on.
    pub ui_dir: Option<PathBuf>,
    /// Keys the hash that identifies a conversation for sticky routing, derived from the
    /// encryption key so it is the same on every replica.
    pub affinity_salt: [u8; 32],
    pub instance_id: String,
    /// Local development: debug builds listening on loopback offer a "Dev Login" button on the
    /// sign-in screen, unless `GATEWAY_DEV_LOGIN=false`. Release builds never do.
    pub dev_login: bool,
    /// `Secure` flag on the admin session cookie. Browsers accept it on http://localhost too. Not
    /// a setting: set wrong, nobody could sign in to the console to change it back.
    pub cookie_secure: bool,
    /// Which addresses upstream and OIDC requests may connect to. Not a setting: it guards against
    /// an admin account pointing the gateway at the internal network (see `netguard`).
    pub upstream_policy: Policy,
    /// Memory for logged prompts and responses not yet written (`PROMPT_LOG_BUFFER_MB`, see
    /// `capture::Budget`). Not a setting: it sizes the process. 0 turns capture off on a replica.
    pub prompt_log_buffer_bytes: usize,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let master_key = env::var("GATEWAY_MASTER_KEY").ok().filter(|k| !k.is_empty());
        if master_key.as_ref().is_some_and(|k| k.len() < 32) {
            return Err("GATEWAY_MASTER_KEY must be at least 32 characters".into());
        }
        let encryption_key = req("GATEWAY_ENCRYPTION_KEY")?;
        let mut secret_box = SecretBox::from_base64(&encryption_key).map_err(|e| format!("GATEWAY_ENCRYPTION_KEY: {e}"))?;
        let affinity_salt = crate::util::sha256(format!("inference-gateway affinity\n{encryption_key}").as_bytes());
        // During a key rotation: values sealed with the retired key are still read (see docs/production.md).
        if let Some(previous) = env::var("GATEWAY_PREVIOUS_ENCRYPTION_KEY").ok().filter(|k| !k.is_empty()) {
            secret_box = SecretBox::from_base64(&previous)
                .and_then(|p| secret_box.with_previous(p))
                .map_err(|e| format!("GATEWAY_PREVIOUS_ENCRYPTION_KEY: {e}"))?;
        }

        // Debug builds serve no console unless asked to: in development it runs on Vite's dev
        // server (`npm run dev` in ui/), and a stale ui/dist would only get in the way.
        let ui_dir = opt("GATEWAY_UI", !cfg!(debug_assertions))?.then(ui_dir);

        let instance_id = instance_id().to_string();

        let listen_addr = opt("GATEWAY_LISTEN", SocketAddr::from(([0, 0, 0, 0], 8080)))?;
        let dev_login = cfg!(debug_assertions) && listen_addr.ip().is_loopback() && opt("GATEWAY_DEV_LOGIN", true)?;

        Ok(Self {
            listen_addr,
            database_url: req("DATABASE_URL")?,
            database_max_connections: opt("DATABASE_MAX_CONNECTIONS", 20)?,
            valkey_url: req("VALKEY_URL")?,
            valkey_cluster: opt("VALKEY_CLUSTER", false)?,
            master_key,
            secret_box,
            ui_dir,
            affinity_salt,
            instance_id,
            dev_login,
            cookie_secure: opt("GATEWAY_COOKIE_SECURE", true)?,
            upstream_policy: Policy::from_list(&env::var("GATEWAY_UPSTREAM_ALLOWED_NETWORKS").unwrap_or_default())
                .map_err(|e| format!("GATEWAY_UPSTREAM_ALLOWED_NETWORKS: {e}"))?,
            prompt_log_buffer_bytes: opt::<usize>("PROMPT_LOG_BUFFER_MB", 256)?.saturating_mul(1024 * 1024),
        })
    }
}

/// This replica's name: `GATEWAY_INSTANCE_ID`, the host name, or a random one picked once. Also
/// read before the configuration, for telemetry's `service.instance.id`.
pub fn instance_id() -> &'static str {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| env::var("GATEWAY_INSTANCE_ID").or_else(|_| env::var("HOSTNAME")).unwrap_or_else(|_| crate::util::random_hex(6)))
}

/// Where the console's files are: `ui` next to the binary (`/ui` in the image), else `ui/dist`
/// in the working directory (a build in the repository).
fn ui_dir() -> PathBuf {
    env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.join("ui")))
        .filter(|d| d.join("index.html").exists())
        .unwrap_or_else(|| PathBuf::from("ui/dist"))
}

fn req(name: &str) -> Result<String, String> {
    env::var(name).ok().filter(|v| !v.is_empty()).ok_or_else(|| format!("missing required environment variable {name}"))
}

fn opt<T: std::str::FromStr>(name: &str, default: T) -> Result<T, String> {
    match env::var(name) {
        Ok(v) if !v.is_empty() => v.parse().map_err(|_| format!("invalid value for {name}")),
        _ => Ok(default),
    }
}
