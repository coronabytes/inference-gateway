mod a2a;
mod accounting;
mod admin;
mod affinity;
mod api;
mod audit;
mod auth;
mod aws;
mod budget;
mod cache;
mod capture;
mod config;
mod conversations;
mod crypto;
mod db;
mod entra;
mod error;
mod google;
mod guardrails;
mod ir;
mod labels;
mod mcp;
mod model;
mod netguard;
mod oauth;
mod openapi;
mod prices;
mod providers;
mod registry;
mod rotate;
mod router;
mod server;
mod settings;
mod sse;
mod state;
mod sync;
mod telemetry;
mod util;

use std::time::{Duration, Instant};

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderName, HeaderValue, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use serde_json::json;
use tower_http::{
    services::{ServeDir, ServeFile},
    set_header::SetResponseHeaderLayer,
};

use utoipa::OpenApi;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{config::Config, state::AppState};

/// The git tag this gateway was built from (see build.rs), e.g. `v1.0.0`.
pub const VERSION: &str = env!("GATEWAY_VERSION");

#[tokio::main]
async fn main() {
    // `inference-gateway healthcheck` for container health probes (the image has no shell or curl).
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        std::process::exit(healthcheck());
    }
    // For `rediss://`: the Valkey client uses rustls's process-wide provider, which rustls cannot
    // choose itself while both ring and aws-lc-rs are in the build.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    // Local development convenience. Variables already set in the environment win, and release
    // builds (including the Docker image) never read a .env file.
    #[cfg(debug_assertions)]
    let dotenv = dotenvy::dotenv();
    // `inference-gateway rotate-key` re-encrypts stored secrets after the key changed.
    let rotate_key = std::env::args().nth(1).as_deref() == Some("rotate-key");
    let telemetry = init_tracing(!rotate_key);
    #[cfg(debug_assertions)]
    match dotenv {
        Ok(path) => tracing::info!(path = %path.display(), "loaded .env (debug build)"),
        Err(e) if e.not_found() => {}
        Err(e) => tracing::warn!(error = %e, "could not parse .env"),
    }
    let result = match telemetry {
        Err(e) => Err(e),
        Ok(()) if rotate_key => rotate::run().await,
        Ok(()) => run().await,
    };
    telemetry::shutdown().await;
    if let Err(e) = result {
        tracing::error!(error = %e, "fatal");
        std::process::exit(1);
    }
}

/// Logs to the console and, when OpenTelemetry logs are exported, to the collector. `telemetry`:
/// start the OpenTelemetry exporters (the server only).
fn init_tracing(telemetry: bool) -> Result<(), String> {
    use tracing_subscriber::{EnvFilter, Layer, fmt, layer::SubscriberExt, util::SubscriberInitExt};
    let filter = || EnvFilter::try_from_env("GATEWAY_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let console = if std::env::var("GATEWAY_LOG_FORMAT").as_deref() == Ok("json") {
        fmt::layer().json().flatten_event(true).boxed()
    } else {
        fmt::layer().boxed()
    };
    let otel = if telemetry { telemetry::init() } else { Ok(None) };
    let export = otel.as_ref().ok().and_then(Option::as_ref).map(|provider| {
        // The exporters' own logs, and their HTTP client's, stay on the console: exporting them
        // would feed back into the exporter.
        let quiet = ["opentelemetry", "reqwest", "hyper", "hyper_util", "h2"]
            .into_iter()
            .fold(filter(), |f, target| f.add_directive(format!("{target}=off").parse().expect("valid directive")));
        opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(provider).with_filter(quiet)
    });
    tracing_subscriber::registry().with(console.with_filter(filter())).with(export).init();
    otel.map(|_| ())
}

async fn run() -> Result<(), String> {
    let cfg = Config::from_env()?;
    tracing::info!(instance = %cfg.instance_id, listen = %cfg.listen_addr, "starting inference-gateway");

    let db = connect_db(&cfg).await?;
    let cache = cache::Cache::new(&cfg.valkey_url, cfg.valkey_cluster).map_err(|e| format!("invalid VALKEY_URL: {e}"))?;
    if !cache.ping().await {
        tracing::warn!("valkey is not reachable; rate limits and cluster sync are degraded until it is");
    }
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_keepalive(Duration::from_secs(30))
        .user_agent(format!("inference-gateway/{VERSION}"))
        // Upstream URLs are admin-configured: resolve them only to allowed addresses, and do not
        // follow redirects, which could lead to an address the resolver never saw.
        .dns_resolver(cfg.upstream_policy.resolver())
        // A proxy from the environment (HTTPS_PROXY, ALL_PROXY) would resolve hostnames itself,
        // past the resolver above.
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("http client: {e}"))?;

    let (tx, rx) = tokio::sync::mpsc::channel(10_000);
    let prompt_log_buffer = cfg.prompt_log_buffer_bytes;
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let listen = cfg.listen_addr;
    let ui_dir = cfg.ui_dir.clone();
    let state = AppState::new(state::Inner {
        cfg,
        settings: settings::Live::default(),
        db,
        cache,
        registry: registry::Registry::new(),
        aws: aws::Aws::default(),
        google: google::Google::default(),
        entra: entra::Entra::default(),
        http,
        records: tx,
        mcp: mcp::Upstreams::default(),
        a2a: a2a::Cards::default(),
        oauth: oauth::Tokens::default(),
        mcp_auth: oauth::server::Store::default(),
        bodies: capture::Budget::new(prompt_log_buffer),
        started: Instant::now(),
        started_at: chrono::Utc::now(),
    });
    state.settings.reload(&state.db).await.map_err(|e| format!("initial settings load: {e}"))?;
    state.registry.reload(&state.db, &state.cfg).await.map_err(|e| format!("initial registry load: {e}"))?;
    admin::auth::warn_if_no_sign_in(&state).await;

    let writer = tokio::spawn(accounting::run_writer(state.clone(), rx, shutdown_rx));
    tokio::spawn(sync::run_subscriber(state.clone()));
    tokio::spawn(sync::run_periodic_reload(state.clone()));
    tokio::spawn(sync::run_heartbeat(state.clone()));
    tokio::spawn(sync::run_maintenance(state.clone()));
    tokio::spawn(prices::run_scheduled_import(state.clone()));

    if let Some(signals) = telemetry::describe() {
        tracing::info!(%signals, "exporting OpenTelemetry");
    }
    let mut app: Router<AppState> = routes(Some(&state)).into();

    // The former public addresses: off, or on to the new ones. Either way, not the file server,
    // which would hand out the reference's files and answer /openapi.json with the console's page.
    for path in ["/openapi.json", "/docs", "/docs/", "/docs/{*rest}"] {
        let to = if path == "/openapi.json" { "/admin/openapi.json" } else { "/admin/docs/" };
        app = app.route(
            path,
            axum::routing::any(move |State(state): State<AppState>| async move {
                match state.settings.get().api_docs {
                    true => Redirect::permanent(to).into_response(),
                    false => StatusCode::NOT_FOUND.into_response(),
                }
            }),
        );
    }

    if let Some(dir) = ui_dir.as_ref().filter(|d| !d.join("index.html").exists()) {
        tracing::warn!(dir = %dir.display(), "GATEWAY_UI is on, but there is no console build; run `npm run build` in ui/");
    }
    if let Some(dir) = ui_dir.filter(|d| d.join("index.html").exists()) {
        tracing::info!(dir = %dir.display(), "serving admin UI");
        // Vite names everything under assets/ after its content hash, so those files can be cached
        // for good. index.html (and anything else at the root) is revalidated on every load, so a
        // rebuild shows up on a plain reload instead of needing a hard refresh.
        let assets: Router = Router::new().fallback_service(ServeDir::new(dir.join("assets"))).layer(SetResponseHeaderLayer::overriding(
            header::CACHE_CONTROL,
            |res: &Response| {
                // Only hits are immutable; a 404 for a hash that is not there yet must not stick.
                res.status().is_success().then(|| HeaderValue::from_static("public, max-age=31536000, immutable"))
            },
        ));
        let index: Router = Router::new()
            .fallback_service(ServeDir::new(&dir).fallback(ServeFile::new(dir.join("index.html"))))
            .layer(cache_control("no-cache"));
        app = app.nest_service("/assets", assets).fallback_service(index);
    } else if cfg!(debug_assertions) {
        // Development without a console build: the console is on Vite's dev server, so the
        // gateway's own address opens the API reference, served from the console's sources.
        tracing::info!("admin UI not served; run `npm run dev` in ui/ for the console, API reference at /admin/docs/");
        app = app.route("/", get(|| async { Redirect::to("/admin/docs/") }));
    }

    let app = app
        .layer(header("x-content-type-options", "nosniff"))
        .layer(header("x-frame-options", "DENY"))
        .layer(header("referrer-policy", "no-referrer"))
        .layer(header(
            "content-security-policy",
            "default-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; frame-ancestors 'none'",
        ))
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(listen).await.map_err(|e| format!("bind {listen}: {e}"))?;
    tracing::info!("listening on {listen}");
    server::serve(listener, app, state.settings.subscribe(), shutdown_signal()).await;

    tracing::info!("shutting down; flushing request logs");
    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(10), writer).await;
    Ok(())
}

/// The gateway's API: the inference endpoints, the health probes and the admin API, each handler
/// registered with its `#[utoipa::path]`, so the OpenAPI document comes from these routers. `state`
/// runs the middleware that guards them; without one, the routes are only read for the document,
/// which so needs neither a database nor any configuration.
fn routes(state: Option<&AppState>) -> OpenApiRouter<AppState> {
    // The paths without `/v1` serve the same handlers for clients that leave it out; they are not
    // documented a second time.
    let mut inference = OpenApiRouter::with_openapi(openapi::ApiDoc::openapi())
        .routes(routes!(api::messages))
        .routes(routes!(api::count_tokens))
        .routes(routes!(api::responses))
        .routes(routes!(api::chat_completions))
        .routes(routes!(api::decisions::decisions))
        .route("/decisions", post(api::decisions::decisions))
        .routes(routes!(api::decisions::systemone))
        .route("/systemone", post(api::decisions::systemone))
        .routes(routes!(api::embeddings::embeddings))
        .route("/embeddings", post(api::embeddings::embeddings))
        // Cohere-style clients call /v1/rerank or /v2/rerank, Jina and vLLM ones also /rerank.
        .routes(routes!(api::rerank::rerank))
        .route("/v2/rerank", post(api::rerank::rerank))
        .route("/rerank", post(api::rerank::rerank))
        .routes(routes!(api::models::list_models))
        .routes(routes!(api::models::list_models_extended))
        // Sign-ins of tenants and users to MCP servers and A2A agents (see `api::sign_ins`).
        .routes(routes!(api::sign_ins::list, api::sign_ins::link))
        .routes(routes!(api::sign_ins::sign_out))
        .routes(routes!(mcp::hub::hub, mcp::hub::hub_get, mcp::hub::hub_delete))
        .routes(routes!(mcp::proxy, mcp::proxy_get, mcp::proxy_delete))
        .routes(routes!(a2a::jsonrpc))
        .routes(routes!(a2a::card))
        .routes(routes!(a2a::rest_get, a2a::rest, a2a::rest_delete))
        // The gateway as the OAuth authorization server of its MCP endpoints (see `oauth::server`).
        .routes(routes!(oauth::server::resource_root))
        .routes(routes!(oauth::server::resource_hub))
        .routes(routes!(oauth::server::resource_server))
        .routes(routes!(oauth::server::metadata))
        .routes(routes!(oauth::server::authorize))
        .routes(routes!(oauth::server::token))
        .routes(routes!(oauth::server::revoke));
    if telemetry::enabled() {
        inference = inference.route_layer(axum::middleware::from_fn(telemetry::http_span));
    }

    let mut admin = OpenApiRouter::new().nest("/api", admin::router(state)).nest("/auth", admin::auth::browser_routes());
    if let Some(state) = state {
        // The OpenAPI document, and the API reference that renders it (static files in the console's
        // build, from ui/public/docs), for signed-in people and admin keys only. They live under
        // /admin, the only path the session cookie is sent to. They answer 404 while the `api_docs`
        // setting is off.
        let docs_dir = state
            .cfg
            .ui_dir
            .as_ref()
            .map(|d| d.join("docs"))
            .filter(|d| d.join("index.html").exists())
            .or_else(|| cfg!(debug_assertions).then(|| "ui/public/docs".into()));
        let mut docs = Router::new().route("/openapi.json", get(openapi::document));
        if let Some(dir) = docs_dir {
            docs = docs.nest_service("/docs", ServeDir::new(dir));
        }
        let docs = docs
            .layer(axum::middleware::from_fn_with_state(state.clone(), admin::auth::require_signed_in))
            .layer(axum::middleware::from_fn_with_state(state.clone(), docs_enabled));
        admin = admin.merge(docs.into()).layer(admin::auth::layer(state));
    }

    inference.routes(routes!(live)).routes(routes!(ready)).nest("/admin", admin)
}

/// Liveness
///
/// The process answers.
#[utoipa::path(get, path = "/health/live", operation_id = "health_live", tag = "Health", responses((status = 200, description = "`ok`", body = String)))]
async fn live() -> &'static str {
    "ok"
}

fn healthcheck() -> i32 {
    use std::io::{Read, Write};
    let port = std::env::var("GATEWAY_LISTEN").ok().and_then(|a| a.parse::<std::net::SocketAddr>().ok()).map_or(8080, |a| a.port());
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut s) = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2)) else { return 1 };
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    if s.write_all(b"GET /health/live HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").is_err() {
        return 1;
    }
    let mut buf = [0u8; 16];
    match s.read(&mut buf) {
        Ok(n) if buf[..n].starts_with(b"HTTP/1.1 200") => 0,
        _ => 1,
    }
}

fn header(name: &'static str, value: &'static str) -> SetResponseHeaderLayer<HeaderValue> {
    SetResponseHeaderLayer::if_not_present(HeaderName::from_static(name), HeaderValue::from_static(value))
}

fn cache_control(value: &'static str) -> SetResponseHeaderLayer<HeaderValue> {
    SetResponseHeaderLayer::overriding(header::CACHE_CONTROL, HeaderValue::from_static(value))
}

async fn connect_db(cfg: &Config) -> Result<sqlx::PgPool, String> {
    let mut delay = Duration::from_secs(1);
    for attempt in 1..=10 {
        match db::connect(&cfg.database_url, cfg.database_max_connections).await {
            Ok(p) => return Ok(p),
            Err(e) if attempt < 10 => {
                tracing::warn!(error = %e, attempt, "postgres not ready; retrying");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(10));
            }
            Err(e) => return Err(format!("postgres: {e}")),
        }
    }
    unreachable!()
}

/// Readiness
///
/// Postgres answers and the configuration is loaded. Valkey is reported but not required;
/// without it, limits are not enforced.
#[utoipa::path(get, path = "/health/ready", operation_id = "health_ready", tag = "Health",
    responses((status = 200, description = "Ready.", body = crate::openapi::Ready), (status = 503, description = "Not ready.", body = crate::openapi::Ready)))]
async fn ready(State(state): State<AppState>) -> impl IntoResponse {
    let (pg, valkey) = tokio::join!(db::ping(&state.db), state.cache.ping());
    let loaded = state.registry.snapshot().loaded_at.is_some();
    let status = if pg && loaded { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    (status, Json(json!({ "postgres": pg, "valkey": valkey, "registry_loaded": loaded })))
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
}

/// The OpenAPI document and the API reference answer 404 while the `api_docs` setting is off.
async fn docs_enabled(State(state): State<AppState>, req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    match state.settings.get().api_docs {
        true => next.run(req).await,
        false => StatusCode::NOT_FOUND.into_response(),
    }
}
