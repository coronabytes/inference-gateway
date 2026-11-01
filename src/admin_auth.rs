//! Admin authentication: sign-in with OpenID Connect, sessions in Valkey (tower-sessions) and
//! login state with axum-login. The master key keeps working as a bearer token, for scripts
//! and as the break-glass login. Debug builds listening on loopback also offer a "Dev Login":
//! a session like any other, without a password, that signing out ends.

use std::{
    future::Future,
    pin::Pin,
    sync::Mutex,
    time::{Duration as StdDuration, Instant},
};

use axum::{
    Extension, Json, Router,
    extract::{MatchedPath, Path, Query, Request, State},
    http::{Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use axum_login::{AuthManagerLayer, AuthManagerLayerBuilder, AuthSession, AuthUser, AuthnBackend, UserId};
use chrono::{DateTime, Utc};
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointMaybeSet, EndpointNotSet, EndpointSet, IssuerUrl, Nonce,
    PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope, TokenResponse,
    core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata},
};
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower_sessions::{
    Expiry, Session, SessionManagerLayer, SessionStore,
    cookie::{SameSite, time::Duration},
    session::{Id, Record},
    session_store,
};
use uuid::Uuid;

use crate::{
    audit::{self, Actor},
    db,
    error::AdminError,
    model::{KeyScope, OidcSettingsRow, PersonRole},
    netguard::Policy,
    state::AppState,
};

type R<T> = Result<T, AdminError>;

const COOKIE_NAME: &str = "gw_admin";
/// Cookie-authenticated requests that change state must carry this header. Browsers only send
/// custom headers cross-origin after a CORS preflight, which the gateway never allows.
pub const CSRF_HEADER: &str = "x-gateway-csrf";
const PENDING_KEY: &str = "oidc.pending";
/// Set in the session by the local-development sign-in as an admin.
const DEV_KEY: &str = "dev.signed_in";
/// Set in the session by the local-development sign-in as a user: the development user's id.
const DEV_USER_KEY: &str = "dev.user";
/// A sign-in must come back from the provider within this time.
const PENDING_TTL_SECS: i64 = 600;
/// Set in the session when an admin signs in (single sign-on or Dev Login).
const SIGNED_IN_AT_KEY: &str = "signed_in_at";
/// A session ends this long after sign-in however active it is, on top of the inactivity expiry.
const SESSION_MAX_AGE_SECS: i64 = 24 * 3600;
/// Sign-in starts per minute, gateway-wide: each is a session write (and at times a discovery
/// lookup) for a request nobody has authenticated yet.
const LOGIN_STARTS_PER_MINUTE: u64 = 30;
/// How long a provider's discovery document is reused.
const DISCOVERY_TTL: StdDuration = StdDuration::from_secs(600);
const CALLBACK_PATH: &str = "/admin/auth/oidc/callback";

// ---------------------------------------------------------------- session store

/// tower-sessions store on the gateway's Valkey connection, so sessions work on every replica.
#[derive(Clone)]
pub struct ValkeyStore {
    conn: crate::cache::Conn,
}

impl std::fmt::Debug for ValkeyStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ValkeyStore")
    }
}

fn session_key(id: &Id) -> String {
    format!("gw:sess:{id}")
}

fn backend_err(e: redis::RedisError) -> session_store::Error {
    session_store::Error::Backend(e.to_string())
}

impl ValkeyStore {
    async fn set(&self, record: &Record, only_new: bool) -> session_store::Result<bool> {
        let value = serde_json::to_vec(record).map_err(|e| session_store::Error::Encode(e.to_string()))?;
        let mut cmd = redis::cmd("SET");
        cmd.arg(session_key(&record.id)).arg(value).arg("EXAT").arg(record.expiry_date.unix_timestamp());
        if only_new {
            cmd.arg("NX");
        }
        let reply: Option<String> = cmd.query_async(&mut self.conn.clone()).await.map_err(backend_err)?;
        Ok(reply.is_some())
    }
}

#[async_trait::async_trait]
impl SessionStore for ValkeyStore {
    async fn create(&self, record: &mut Record) -> session_store::Result<()> {
        // Session ids are 128 random bits; regenerate on the (theoretical) collision.
        while !self.set(record, true).await? {
            record.id = Id::default();
        }
        Ok(())
    }

    async fn save(&self, record: &Record) -> session_store::Result<()> {
        self.set(record, false).await.map(|_| ())
    }

    async fn load(&self, id: &Id) -> session_store::Result<Option<Record>> {
        let raw: Option<Vec<u8>> = self.conn.clone().get(session_key(id)).await.map_err(backend_err)?;
        raw.map(|r| serde_json::from_slice(&r).map_err(|e| session_store::Error::Decode(e.to_string()))).transpose()
    }

    async fn delete(&self, id: &Id) -> session_store::Result<()> {
        let _: () = self.conn.clone().del(session_key(id)).await.map_err(backend_err)?;
        Ok(())
    }
}

// ---------------------------------------------------------------- axum-login backend

#[derive(Clone, Serialize, Deserialize)]
pub struct AdminUser {
    pub id: Uuid,
    pub email: Option<String>,
    pub name: Option<String>,
    /// Read from the person's record on every request, so it is the one their last sign-in set.
    pub role: PersonRole,
    /// Changes when the admin record is recreated, which ends older sessions.
    auth_hash: Vec<u8>,
}

impl std::fmt::Debug for AdminUser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminUser").field("id", &self.id).field("email", &self.email).finish()
    }
}

impl AuthUser for AdminUser {
    type Id = Uuid;

    fn id(&self) -> Uuid {
        self.id
    }

    fn session_auth_hash(&self) -> &[u8] {
        &self.auth_hash
    }
}

impl From<crate::model::AdminUserRow> for AdminUser {
    fn from(r: crate::model::AdminUserRow) -> Self {
        let mut h = Sha256::new();
        h.update(r.id.as_bytes());
        h.update(r.created_at.timestamp_micros().to_be_bytes());
        Self { id: r.id, email: r.email, name: r.name, role: r.role, auth_hash: h.finalize().to_vec() }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
    #[error("{0}")]
    Oidc(String),
    #[error("{0}")]
    Denied(String),
}

#[derive(Clone)]
pub struct AdminBackend {
    state: AppState,
}

/// What the provider sent back, plus what this browser's session stored when the sign-in started.
pub struct OidcCallback {
    code: String,
    state: String,
    pending: Pending,
}

impl AuthnBackend for AdminBackend {
    type User = AdminUser;
    type Credentials = OidcCallback;
    type Error = AuthError;

    async fn authenticate(&self, creds: OidcCallback) -> Result<Option<AdminUser>, AuthError> {
        let settings = enabled_settings(&self.state).await?.ok_or_else(|| AuthError::Denied("single sign-on is not enabled".into()))?;
        if creds.state.as_bytes() != creds.pending.state.as_bytes() {
            return Err(AuthError::Denied("the sign-in request did not match this browser; start again".into()));
        }
        let identity = exchange(&self.state.cfg.upstream_policy, &settings, creds).await?;
        let role = settings.role_of(&identity).map_err(|reason| {
            tracing::warn!(subject = %identity.subject, email = ?identity.email, reason, "admin sign-in denied");
            AuthError::Denied(format!("{} may not use this gateway's admin UI", identity.label()))
        })?;
        let (email, name) = (identity.email.as_deref(), identity.name.as_deref());
        let row = db::upsert_admin_user(&self.state.db, &identity.issuer, &identity.subject, email, name, role).await?;
        tracing::info!(admin = %row.id, email = ?row.email, role = ?row.role, "admin signed in");
        Ok(Some(row.into()))
    }

    async fn get_user(&self, id: &UserId<Self>) -> Result<Option<AdminUser>, AuthError> {
        // Turning single sign-on off ends every session it created.
        if enabled_settings(&self.state).await?.is_none() {
            return Ok(None);
        }
        Ok(db::get_admin_user(&self.state.db, *id).await?.map(Into::into))
    }
}

pub type Auth = AuthSession<AdminBackend>;

/// Sessions and login state for everything under `/admin`.
pub fn layer(state: &AppState) -> AuthManagerLayer<AdminBackend, ValkeyStore> {
    let store = ValkeyStore { conn: state.cache.connection() };
    let sessions = SessionManagerLayer::new(store)
        .with_name(COOKIE_NAME)
        .with_path("/admin")
        .with_same_site(SameSite::Lax)
        .with_http_only(true)
        .with_secure(state.cfg.cookie_secure)
        .with_expiry(Expiry::OnInactivity(Duration::hours(8)));
    AuthManagerLayerBuilder::new(AdminBackend { state: state.clone() }, sessions).build()
}

/// Without a master key, single sign-on is the only way into the admin UI and API.
pub async fn warn_if_no_sign_in(state: &AppState) {
    if state.cfg.master_key.is_some() {
        return;
    }
    if state.cfg.dev_login {
        tracing::info!("GATEWAY_MASTER_KEY is not set; local development signs in with Dev Login");
        return;
    }
    match enabled_settings(state).await {
        Ok(Some(_)) => tracing::info!("GATEWAY_MASTER_KEY is not set; admins sign in with single sign-on only"),
        Ok(None) => tracing::warn!(
            "GATEWAY_MASTER_KEY is not set and single sign-on is not enabled: nobody can sign in to the admin UI or API. \
             Set GATEWAY_MASTER_KEY to configure single sign-on"
        ),
        Err(e) => tracing::warn!(error = %e, "GATEWAY_MASTER_KEY is not set and the single sign-on settings could not be read"),
    }
}

// ---------------------------------------------------------------- admin guard

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Via {
    MasterKey,
    /// A virtual key with every right an admin has (scope admin or all).
    VirtualKey,
    /// A virtual key for its project's part of the admin API (scope project).
    ProjectKey,
    Session,
    /// Dev Login as an admin.
    Dev,
    /// Dev Login as the local development user.
    DevUser,
}

/// How a request is authenticated, and for Dev Login as a user, as whom; for a project key, its
/// project (none once the project is deleted). Everything is read from the request up front: the
/// request is not `Sync`, so the returned future must not borrow it.
fn authenticated_via(state: &AppState, req: &Request) -> impl Future<Output = Option<(Via, Option<Uuid>)>> + Send + 'static {
    let master = crate::auth::is_master(state, req.headers());
    let admin_key = crate::auth::presented_virtual_key(state, req.headers()).filter(|k| k.scope.admin());
    let signed_in = req.extensions().get::<Auth>().is_some_and(|a| a.user.is_some());
    let session = req.extensions().get::<Session>().cloned();
    let dev_login = state.cfg.dev_login;
    async move {
        if master {
            return Some((Via::MasterKey, None));
        }
        if let Some(k) = admin_key {
            return Some(if k.scope == KeyScope::Project { (Via::ProjectKey, k.project_id) } else { (Via::VirtualKey, None) });
        }
        let session = session?;
        let (via, dev_user) = if signed_in {
            (Via::Session, None)
        } else if dev_login && session.get::<bool>(DEV_KEY).await.ok().flatten() == Some(true) {
            (Via::Dev, None)
        } else if dev_login && let Some(id) = session.get::<Uuid>(DEV_USER_KEY).await.ok().flatten() {
            (Via::DevUser, Some(id))
        } else {
            return None;
        };
        // Absolute lifetime: the inactivity expiry alone would keep a busy session alive forever.
        let signed_in_at: Option<i64> = session.get(SIGNED_IN_AT_KEY).await.ok().flatten();
        if signed_in_at.is_none_or(|t| Utc::now().timestamp() - t > SESSION_MAX_AGE_SECS) {
            if let Err(e) = session.flush().await {
                tracing::warn!(error = %e, "could not end an expired admin session");
            }
            return None;
        }
        Some((via, dev_user))
    }
}

/// What an authenticated caller may reach. Admins, the master key, admin-scope keys and Dev Login
/// as admin reach everything; a user only the keys, playground, request log and metrics of the
/// projects an admin added them to (`USER_ROUTES`), and nothing else. A project key reaches what a
/// user who works in its project does.
#[derive(Clone, Debug)]
pub enum Access {
    Admin,
    User { projects: Vec<Uuid> },
}

impl Access {
    /// The projects the caller is limited to; None for an admin.
    pub fn projects(&self) -> Option<&[Uuid]> {
        match self {
            Access::Admin => None,
            Access::User { projects, .. } => Some(projects),
        }
    }

    /// Whether the caller may see what belongs to `project` (None: no project, admins only).
    pub fn sees(&self, project: Option<Uuid>) -> bool {
        self.projects().is_none_or(|ps| project.is_some_and(|p| ps.contains(&p)))
    }
}

/// What a user may call: (method, route under `/admin/api`). Everything else is for admins.
const USER_ROUTES: &[(Method, &str)] = &[
    (Method::GET, "/keys"),
    (Method::GET, "/keys/options"),
    (Method::POST, "/keys"),
    (Method::PUT, "/keys/{id}"),
    (Method::DELETE, "/keys/{id}"),
    (Method::POST, "/keys/{id}/reset-spend"),
    (Method::POST, "/keys/{id}/regenerate"),
    (Method::GET, "/keys/{id}/connections"),
    (Method::POST, "/keys/{id}/connections/{kind}/{upstream}"),
    (Method::DELETE, "/keys/{id}/connections/{kind}/{upstream}"),
    (Method::GET, "/keys/{id}/apps"),
    (Method::DELETE, "/keys/{id}/apps/{grant}"),
    (Method::GET, "/connect/{id}"),
    (Method::POST, "/connect/{id}"),
    (Method::POST, "/connect/{id}/deny"),
    (Method::GET, "/projects"),
    (Method::GET, "/projects/options"),
    (Method::GET, "/tags"),
    (Method::GET, "/metrics"),
    (Method::GET, "/logs"),
    (Method::GET, "/logs/labels"),
    (Method::GET, "/logs/tools"),
    (Method::GET, "/logs/{id}/bodies"),
    (Method::GET, "/keys/{id}/bodies"),
    (Method::DELETE, "/keys/{id}/bodies"),
    (Method::GET, "/usage/keys"),
    (Method::GET, "/playground/options"),
    (Method::POST, "/playground/{api}"),
    (Method::POST, "/playground/a2a/{agent}"),
    (Method::GET, "/playground/a2a/{agent}/card"),
    (Method::GET, "/hub"),
    (Method::GET, "/efforts"),
];

/// What a key whose scope is the inference API only may call: its own model hub (`OwnKey`).
const KEY_ROUTES: &[(Method, &str)] = &[(Method::GET, "/hub")];

/// The caller is a key whose scope is the inference API only; handlers show it only itself.
#[derive(Clone, Copy)]
pub struct OwnKey(pub Uuid);

/// Whether the request is one of `routes`. Needs the matched route, so the guard is a route layer.
fn may_call(req: &Request, routes: &[(Method, &str)]) -> bool {
    let Some(route) = req.extensions().get::<MatchedPath>().and_then(|p| p.as_str().strip_prefix("/admin/api")) else {
        return false;
    };
    routes.iter().any(|(m, r)| m == req.method() && *r == route)
}

/// The caller's access: a signed-in person's from their role, with a user's projects.
/// `signed_in` is the session's person, read from the request before any await (it is not `Sync`).
async fn access(state: &AppState, signed_in: Option<AdminUser>, via: Via, dev_user: Option<Uuid>) -> R<Access> {
    let user = match via {
        Via::MasterKey | Via::VirtualKey | Via::Dev => return Ok(Access::Admin),
        // `dev_user` is the key's project here.
        Via::ProjectKey => return Ok(Access::User { projects: dev_user.into_iter().collect() }),
        Via::Session => {
            let user = signed_in.ok_or_else(unauthenticated)?;
            if user.role == PersonRole::Admin {
                return Ok(Access::Admin);
            }
            user.id
        }
        // Removing the development user on the Admins tab ends its sessions, as for anyone.
        Via::DevUser => db::get_admin_user(&state.db, dev_user.ok_or_else(unauthenticated)?).await?.ok_or_else(unauthenticated)?.id,
    };
    Ok(Access::User { projects: db::user_projects(&state.db, user).await? })
}

fn unauthenticated() -> AdminError {
    AdminError(StatusCode::UNAUTHORIZED, "admin authentication required".into())
}

/// Admin API guard: the master key, a virtual key with the admin scope, a signed-in admin or
/// user, or a local-development session. Users only reach `USER_ROUTES`, and keys whose scope is
/// the inference API only `KEY_ROUTES`.
pub async fn require_admin(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    match authenticated_via(&state, &req).await {
        Some((Via::Session | Via::Dev | Via::DevUser, _)) if !csrf_ok(&req) => {
            AdminError(StatusCode::FORBIDDEN, format!("missing {CSRF_HEADER} header")).into_response()
        }
        Some((via, dev_user)) => {
            let signed_in = req.extensions().get::<Auth>().and_then(|a| a.user.clone());
            let access = match access(&state, signed_in, via, dev_user).await {
                Ok(a) => a,
                Err(e) => return e.into_response(),
            };
            if matches!(access, Access::User { .. }) && !may_call(&req, USER_ROUTES) {
                return AdminError(StatusCode::FORBIDDEN, "only admins may do this".into()).into_response();
            }
            let Some(actor) = actor(&state, &req, via, dev_user) else {
                return unauthenticated().into_response();
            };
            // Changes made with a key name the key in the log, since no admin is signed in.
            if matches!(via, Via::VirtualKey | Via::ProjectKey) && !matches!(*req.method(), Method::GET | Method::HEAD) {
                tracing::info!(key = ?actor.id, name = ?actor.name, method = %req.method(), path = %req.uri().path(), "admin API call with a virtual key");
            }
            // Handlers that change something record it in the audit log under this actor, and
            // limit what they show and change to the caller's access.
            req.extensions_mut().insert(actor);
            req.extensions_mut().insert(access);
            next.run(req).await
        }
        // A working key whose scope is the inference API only: its own model hub, nothing else.
        None => match crate::auth::presented_virtual_key(&state, req.headers()) {
            Some(k) if may_call(&req, KEY_ROUTES) => {
                req.extensions_mut().insert(Actor::virtual_key(k.id, &k.name));
                req.extensions_mut().insert(Access::User { projects: Vec::new() });
                req.extensions_mut().insert(OwnKey(k.id));
                next.run(req).await
            }
            Some(_) => AdminError(StatusCode::FORBIDDEN, "this key's scope does not include the admin API".into()).into_response(),
            None => unauthenticated().into_response(),
        },
    }
}

/// API reference guard (`/admin/docs/` and `/admin/openapi.json`): whoever the admin API takes,
/// whatever they may do there. Others get 401: the document as an admin API error, the page as
/// a short note that points to the console's sign-in.
pub async fn require_signed_in(State(state): State<AppState>, req: Request, next: Next) -> Response {
    if authenticated_via(&state, &req).await.is_some() {
        return next.run(req).await;
    }
    if req.uri().path().ends_with(".json") {
        return unauthenticated().into_response();
    }
    let page = "<!doctype html><meta charset=\"utf-8\"><title>Inference Gateway API</title>\
                <p>Sign in to the <a href=\"/\">console</a> to see the API reference.</p>";
    (StatusCode::UNAUTHORIZED, axum::response::Html(page)).into_response()
}

/// The local development user's name, in the audit log and the console.
const DEV_USER_NAME: &str = "Local development user";

/// Who the audit log names for an authenticated request.
fn actor(state: &AppState, req: &Request, via: Via, dev_user: Option<Uuid>) -> Option<Actor> {
    Some(match via {
        Via::MasterKey => Actor::master_key(),
        Via::VirtualKey | Via::ProjectKey => {
            let key = crate::auth::presented_virtual_key(state, req.headers())?;
            Actor::virtual_key(key.id, &key.name)
        }
        Via::Session => {
            let user = req.extensions().get::<Auth>()?.user.clone()?;
            Actor::admin(user.id, user.email.as_deref(), user.name.as_deref())
        }
        Via::Dev => Actor::dev(),
        Via::DevUser => Actor::admin(dev_user?, None, Some(DEV_USER_NAME)),
    })
}

fn csrf_ok(req: &Request) -> bool {
    matches!(*req.method(), Method::GET | Method::HEAD) || req.headers().contains_key(CSRF_HEADER)
}

// ---------------------------------------------------------------- routes

/// Admin API routes that work signed out (`/admin/api/auth/...`).
pub fn api_routes() -> Router<AppState> {
    Router::new().route("/auth/session", get(session_info)).route("/auth/logout", post(logout)).route("/auth/dev-login", post(dev_login))
}

/// Admin API routes behind the guard.
pub fn protected_api_routes() -> Router<AppState> {
    Router::new()
        .route("/settings/oidc", get(get_settings).put(put_settings))
        .route("/settings/oidc/test", post(test_settings))
        .route("/admins", get(list_admins))
        .route("/admins/{id}", axum::routing::delete(delete_admin))
        .route("/admins/{id}/projects", axum::routing::put(set_admin_projects))
}

/// Browser redirects for the sign-in flow (`/admin/auth/...`).
pub fn browser_routes() -> Router<AppState> {
    Router::new()
        .route("/oidc/login", get(start_login))
        .route("/oidc/callback", get(callback))
        .route("/oauth/callback", get(crate::admin::oauth::callback))
        .route("/oauth/client.json", get(crate::admin::oauth::client_metadata))
        .route("/oauth/sign-in/{ticket}", get(crate::admin::oauth::sign_in_link))
}

/// Who is signed in, what they may do, and how the sign-in screen should look. Works signed out.
async fn session_info(State(state): State<AppState>, req: Request) -> R<Json<Value>> {
    let via = authenticated_via(&state, &req).await;
    let user = req.extensions().get::<Auth>().and_then(|a| a.user.clone());
    let access = match via {
        Some((v, dev_user)) => access(&state, user.clone(), v, dev_user).await.ok(),
        None => None,
    };
    let sso = enabled_settings(&state).await.map_err(|e| AdminError::bad_request(e.to_string()))?;
    Ok(Json(json!({
        "authenticated": access.is_some(),
        "via": access.as_ref().and(via.map(|(v, _)| v)),
        "user": match via {
            Some((Via::Session, _)) => user.map(|u| json!({ "id": u.id, "email": u.email, "name": u.name })),
            Some((Via::Dev, _)) => Some(json!({ "name": "Local development" })),
            Some((Via::DevUser, id)) => Some(json!({ "id": id, "name": DEV_USER_NAME })),
            Some((Via::VirtualKey | Via::ProjectKey, _)) => crate::auth::presented_virtual_key(&state, req.headers()).map(|k| json!({ "id": k.id, "name": k.name })),
            _ => None,
        },
        // "admin" or "user"; the console shows a user only their projects' keys, playground,
        // request log and metrics.
        "role": access.as_ref().map(|a| if a.projects().is_some() { "user" } else { "admin" }),
        "projects": access.as_ref().and_then(Access::projects),
        "sso": sso.map(|s| json!({ "display_name": s.display_name })),
        "master_key": state.cfg.master_key.is_some(),
        // The sign-in screen offers "Dev Login".
        "dev": state.cfg.dev_login,
        // Shown on the sign-in screen and in the console header.
        "version": crate::VERSION,
        // The console links to the API reference at /docs.
        "api_docs": state.settings.get().api_docs,
    })))
}

#[derive(Deserialize)]
struct DevLoginQuery {
    /// `user` signs in as the local development user; anything else as an admin.
    role: Option<String>,
}

/// Local development only: sign this browser in without credentials, as an admin or as the
/// local development user, whom admins add to projects like anyone else.
async fn dev_login(
    State(state): State<AppState>,
    session: Session,
    Query(q): Query<DevLoginQuery>,
    req_headers: axum::http::HeaderMap,
) -> R<StatusCode> {
    if !state.cfg.dev_login {
        return Err(AdminError::not_found());
    }
    if !req_headers.contains_key(CSRF_HEADER) {
        return Err(AdminError(StatusCode::FORBIDDEN, format!("missing {CSRF_HEADER} header")));
    }
    let fail = |e: tower_sessions::session::Error| AdminError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    // A new session id on sign-in, as for single sign-on.
    session.cycle_id().await.map_err(fail)?;
    let actor = if q.role.as_deref() == Some("user") {
        let user = db::upsert_admin_user(&state.db, "dev", "user", None, Some(DEV_USER_NAME), PersonRole::User).await?;
        session.remove::<bool>(DEV_KEY).await.map_err(fail)?;
        session.insert(DEV_USER_KEY, user.id).await.map_err(fail)?;
        tracing::info!(user = %user.id, "signed in with Dev Login as the local development user");
        Actor::admin(user.id, None, Some(DEV_USER_NAME))
    } else {
        session.remove::<Uuid>(DEV_USER_KEY).await.map_err(fail)?;
        session.insert(DEV_KEY, true).await.map_err(fail)?;
        tracing::info!("admin signed in with Dev Login");
        Actor::dev()
    };
    session.insert(SIGNED_IN_AT_KEY, Utc::now().timestamp()).await.map_err(fail)?;
    audit::Event::new("session", "sign_in").record(&state, &actor).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn logout(State(state): State<AppState>, mut auth: Auth, session: Session, req_headers: axum::http::HeaderMap) -> R<StatusCode> {
    if !req_headers.contains_key(CSRF_HEADER) {
        return Err(AdminError(StatusCode::FORBIDDEN, format!("missing {CSRF_HEADER} header")));
    }
    let fail = |e: String| AdminError(StatusCode::INTERNAL_SERVER_ERROR, e);
    let dev = session.remove::<bool>(DEV_KEY).await.map_err(|e| fail(e.to_string()))? == Some(true);
    let dev_user = session.remove::<Uuid>(DEV_USER_KEY).await.map_err(|e| fail(e.to_string()))?;
    let actor = match auth.logout().await.map_err(|e| fail(e.to_string()))? {
        Some(u) => Some(Actor::admin(u.id, u.email.as_deref(), u.name.as_deref())),
        None if dev => Some(Actor::dev()),
        None => dev_user.map(|id| Actor::admin(id, None, Some(DEV_USER_NAME))),
    };
    if let Some(actor) = actor {
        audit::Event::new("session", "sign_out").record(&state, &actor).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// What a sign-in in progress keeps in the browser's session.
#[derive(Clone, Serialize, Deserialize)]
struct Pending {
    state: String,
    nonce: String,
    verifier: String,
    started: i64,
}

/// Back to the UI with a message for the sign-in screen.
fn to_login(message: &str) -> Response {
    let q = percent_encoding::utf8_percent_encode(message, percent_encoding::NON_ALPHANUMERIC);
    Redirect::to(&format!("/?login_error={q}")).into_response()
}

async fn start_login(State(state): State<AppState>, session: Session) -> Response {
    // Nobody is authenticated yet, so what a start costs is bounded for the gateway as a whole.
    // Admins locked out by a flood still have the master key.
    if !state.cache.rate_check("oidc:login", LOGIN_STARTS_PER_MINUTE, 1).await.allowed {
        return to_login("Too many sign-in attempts right now; try again in a minute.");
    }
    let result: Result<String, AuthError> = async {
        let settings = enabled_settings(&state).await?.ok_or_else(|| AuthError::Denied("single sign-on is not enabled".into()))?;
        let client = client(&state.cfg.upstream_policy, &settings, true).await?;
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let (url, csrf, nonce) = client
            .authorize_url(CoreAuthenticationFlow::AuthorizationCode, CsrfToken::new_random, Nonce::new_random)
            .add_scope(Scope::new("email".into()))
            .add_scope(Scope::new("profile".into()))
            .set_pkce_challenge(challenge)
            .url();
        let pending = Pending {
            state: csrf.secret().clone(),
            nonce: nonce.secret().clone(),
            verifier: verifier.secret().clone(),
            started: Utc::now().timestamp(),
        };
        session.insert(PENDING_KEY, pending).await.map_err(|e| AuthError::Oidc(e.to_string()))?;
        Ok(url.to_string())
    }
    .await;
    match result {
        Ok(url) => Redirect::to(&url).into_response(),
        Err(AuthError::Denied(m)) => to_login(&m),
        // Discovery and store errors describe the gateway's surroundings; they go to the log only.
        Err(e) => {
            tracing::warn!(error = %e, "could not start admin sign-in");
            to_login("Sign-in could not be started; the gateway log has the details.")
        }
    }
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

async fn callback(State(state): State<AppState>, mut auth: Auth, Query(q): Query<CallbackQuery>) -> Response {
    if let Some(err) = q.error {
        return to_login(&format!("The identity provider refused the sign-in: {}", q.error_description.unwrap_or(err)));
    }
    let pending: Option<Pending> = auth.session.remove(PENDING_KEY).await.ok().flatten();
    let (Some(code), Some(returned_state), Some(pending)) = (q.code, q.state, pending) else {
        return to_login("The sign-in expired or was started in another browser; try again.");
    };
    if Utc::now().timestamp() - pending.started > PENDING_TTL_SECS {
        return to_login("The sign-in took too long; try again.");
    }
    match auth.authenticate(OidcCallback { code, state: returned_state, pending }).await {
        Ok(Some(user)) => {
            let started = match auth.login(&user).await {
                Ok(()) => auth.session.insert(SIGNED_IN_AT_KEY, Utc::now().timestamp()).await.map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            };
            match started {
                Ok(()) => {
                    let actor = Actor::admin(user.id, user.email.as_deref(), user.name.as_deref());
                    audit::Event::new("session", "sign_in").record(&state, &actor).await;
                    Redirect::to("/").into_response()
                }
                Err(e) => {
                    tracing::warn!(error = %e, "could not start an admin session");
                    to_login("Could not start a session; the gateway log has the details.")
                }
            }
        }
        Ok(None) => to_login("Sign-in failed."),
        Err(axum_login::Error::Backend(AuthError::Denied(m))) => to_login(&m),
        Err(e) => {
            tracing::warn!(error = %e, "admin sign-in failed");
            to_login("Sign-in failed; the gateway log has the details.")
        }
    }
}

// ---------------------------------------------------------------- OIDC

/// The stored settings when single sign-on is enabled, with the client secret decrypted.
struct Sso {
    display_name: String,
    issuer_url: String,
    client_id: String,
    client_secret: Option<String>,
    public_url: String,
    roles_claim: String,
    allowed_role: Option<String>,
    user_role: Option<String>,
    /// When the settings were saved; a change invalidates the cached discovery document.
    updated_at: DateTime<Utc>,
}

async fn enabled_settings(state: &AppState) -> Result<Option<Sso>, AuthError> {
    let Some(row) = db::get_oidc_settings(&state.db).await? else { return Ok(None) };
    if !row.enabled {
        return Ok(None);
    }
    Ok(Some(decrypt(state, row)?))
}

fn decrypt(state: &AppState, row: OidcSettingsRow) -> Result<Sso, AuthError> {
    let client_secret = match row.client_secret {
        Some(sealed) => {
            let plain = state.cfg.secret_box.open(&sealed).map_err(|_| AuthError::Oidc("cannot decrypt the OIDC client secret".into()))?;
            Some(String::from_utf8(plain).map_err(|_| AuthError::Oidc("the OIDC client secret is not text".into()))?)
        }
        None => None,
    };
    Ok(Sso {
        display_name: row.display_name,
        issuer_url: row.issuer_url,
        client_id: row.client_id,
        client_secret,
        public_url: row.public_url,
        roles_claim: row.roles_claim,
        allowed_role: row.allowed_role,
        user_role: row.user_role,
        updated_at: row.updated_at,
    })
}

/// Who signed in, from a verified ID token.
struct Identity {
    issuer: String,
    subject: String,
    email: Option<String>,
    name: Option<String>,
    roles: Vec<String>,
}

impl Identity {
    fn label(&self) -> String {
        self.email.clone().or_else(|| self.name.clone()).unwrap_or_else(|| self.subject.clone())
    }
}

impl Sso {
    /// What someone may do: admin with the allowed role in the roles claim, user with the user
    /// role, nothing without either. Roles are compared exactly; the allowed role wins.
    fn role_of(&self, id: &Identity) -> Result<PersonRole, &'static str> {
        let Some(admin) = &self.allowed_role else { return Err("no allowed role is configured") };
        if id.roles.contains(admin) {
            return Ok(PersonRole::Admin);
        }
        match &self.user_role {
            Some(user) if id.roles.contains(user) => Ok(PersonRole::User),
            _ => Err("neither the allowed role nor the user role is in the roles claim"),
        }
    }
}

/// The roles in a claim: a string, a list of strings, or an object keyed by role (Zitadel).
/// The claim name is tried as is, then as a dotted path (Keycloak's `realm_access.roles`).
fn claim_roles(payload: &serde_json::Map<String, Value>, claim: &str) -> Vec<String> {
    let value = payload.get(claim).or_else(|| {
        let mut parts = claim.split('.');
        let first = payload.get(parts.next()?)?;
        parts.try_fold(first, |v, p| v.get(p))
    });
    match value {
        Some(Value::String(one)) => vec![one.clone()],
        Some(Value::Array(items)) => items.iter().filter_map(|i| i.as_str().map(str::to_string)).collect(),
        Some(Value::Object(by_role)) => by_role.keys().cloned().collect(),
        _ => Vec::new(),
    }
}

type Client = CoreClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointMaybeSet, EndpointMaybeSet>;

/// The last discovery document, with the settings' save time it was fetched for and when.
static DISCOVERY: Mutex<Option<(DateTime<Utc>, Instant, CoreProviderMetadata)>> = Mutex::new(None);

/// The provider's discovery document. `cached`: reuse a recent one for the same settings, so a
/// sign-in start (unauthenticated) does not cost an outbound request each time.
async fn discover(policy: &Policy, s: &Sso, cached: bool) -> Result<CoreProviderMetadata, AuthError> {
    if cached
        && let Some((saved, at, metadata)) = DISCOVERY.lock().expect("discovery lock").as_ref()
        && *saved == s.updated_at
        && at.elapsed() < DISCOVERY_TTL
    {
        return Ok(metadata.clone());
    }
    let http = HttpClient::new(policy);
    let issuer = IssuerUrl::new(s.issuer_url.clone()).map_err(|e| AuthError::Oidc(format!("issuer URL: {e}")))?;
    let metadata = CoreProviderMetadata::discover_async(issuer, &http)
        .await
        .map_err(|e| AuthError::Oidc(format!("OIDC discovery failed: {}", error_chain(&e))))?;
    *DISCOVERY.lock().expect("discovery lock") = Some((s.updated_at, Instant::now(), metadata.clone()));
    Ok(metadata)
}

async fn client(policy: &Policy, s: &Sso, cached: bool) -> Result<Client, AuthError> {
    let metadata = discover(policy, s, cached).await?;
    let redirect =
        RedirectUrl::new(format!("{}{CALLBACK_PATH}", s.public_url)).map_err(|e| AuthError::Oidc(format!("redirect URL: {e}")))?;
    Ok(CoreClient::from_provider_metadata(metadata, ClientId::new(s.client_id.clone()), s.client_secret.clone().map(ClientSecret::new))
        .set_redirect_uri(redirect))
}

async fn exchange(policy: &Policy, s: &Sso, creds: OidcCallback) -> Result<Identity, AuthError> {
    let client = client(policy, s, true).await?;
    let http = HttpClient::new(policy);
    let tokens = client
        .exchange_code(AuthorizationCode::new(creds.code))
        .map_err(|e| AuthError::Oidc(format!("the provider has no token endpoint: {e}")))?
        .set_pkce_verifier(PkceCodeVerifier::new(creds.pending.verifier))
        .request_async(&http)
        .await
        .map_err(|e| AuthError::Oidc(format!("code exchange failed: {}", error_chain(&e))))?;
    let id_token = tokens.id_token().ok_or_else(|| AuthError::Oidc("the provider sent no ID token".into()))?;
    let claims = id_token
        .claims(&client.id_token_verifier(), &Nonce::new(creds.pending.nonce))
        .map_err(|e| AuthError::Oidc(format!("invalid ID token: {e}")))?;
    // The token's signature is verified above; read the roles claim, which the typed claims omit.
    let roles = token_payload(&id_token.to_string()).map(|p| claim_roles(&p, &s.roles_claim)).unwrap_or_default();
    Ok(Identity {
        issuer: claims.issuer().to_string(),
        subject: claims.subject().to_string(),
        email: claims.email().map(|e| e.to_string()),
        name: claims.name().and_then(|n| n.get(None)).map(|n| n.to_string()).or_else(|| claims.preferred_username().map(|u| u.to_string())),
        roles,
    })
}

fn token_payload(jwt: &str) -> Option<serde_json::Map<String, Value>> {
    use base64::Engine;
    let payload = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// An error and its causes, `a: b: c`. A cause whose text the message already includes (as some
/// libraries' errors do) is not repeated.
pub fn error_chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut cur = e.source();
    while let Some(c) = cur {
        let text = c.to_string();
        if !s.contains(&text) {
            s.push_str(": ");
            s.push_str(&text);
        }
        cur = c.source();
    }
    s
}

/// openidconnect's HTTP client on the gateway's reqwest. Redirects are not followed, as the
/// openidconnect docs advise (a redirect could point token requests elsewhere). Addresses follow
/// the same network policy as upstream calls.
struct HttpClient(reqwest::Client, Policy);

impl HttpClient {
    fn new(policy: &Policy) -> Self {
        Self(
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .dns_resolver(policy.resolver())
                // Not through an environment proxy, which would resolve hostnames past the policy.
                .no_proxy()
                .timeout(StdDuration::from_secs(15))
                .build()
                .expect("http client"),
            policy.clone(),
        )
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct HttpError(String);

impl<'c> openidconnect::AsyncHttpClient<'c> for HttpClient {
    type Error = HttpError;
    type Future = Pin<Box<dyn Future<Output = Result<openidconnect::HttpResponse, HttpError>> + Send + 'c>>;

    fn call(&'c self, request: openidconnect::HttpRequest) -> Self::Future {
        Box::pin(async move {
            let (parts, body) = request.into_parts();
            let url = parts.uri.to_string();
            self.1.check_url(&url).map_err(|why| HttpError(format!("address not allowed: {why}")))?;
            let resp =
                self.0.request(parts.method, url).headers(parts.headers).body(body).send().await.map_err(|e| HttpError(error_chain(&e)))?;
            let mut out = openidconnect::http::Response::builder().status(resp.status());
            for (k, v) in resp.headers() {
                out = out.header(k, v);
            }
            let bytes = resp.bytes().await.map_err(|e| HttpError(e.to_string()))?;
            out.body(bytes.to_vec()).map_err(|e| HttpError(e.to_string()))
        })
    }
}

// ---------------------------------------------------------------- settings API

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct SettingsView {
    configured: bool,
    enabled: bool,
    display_name: String,
    issuer_url: String,
    client_id: String,
    client_secret_set: bool,
    public_url: String,
    redirect_uri: Option<String>,
    roles_claim: String,
    allowed_role: Option<String>,
    /// Signs in as a user, who sees only the projects an admin adds them to. None: admins only.
    user_role: Option<String>,
    updated_at: Option<DateTime<Utc>>,
}

fn settings_view(row: Option<OidcSettingsRow>) -> SettingsView {
    match row {
        Some(r) => SettingsView {
            configured: true,
            enabled: r.enabled,
            redirect_uri: Some(format!("{}{CALLBACK_PATH}", r.public_url)),
            display_name: r.display_name,
            issuer_url: r.issuer_url,
            client_id: r.client_id,
            client_secret_set: r.client_secret.is_some(),
            public_url: r.public_url,
            roles_claim: r.roles_claim,
            allowed_role: r.allowed_role,
            user_role: r.user_role,
            updated_at: Some(r.updated_at),
        },
        None => SettingsView {
            configured: false,
            enabled: false,
            display_name: "Single sign-on".into(),
            issuer_url: String::new(),
            client_id: String::new(),
            client_secret_set: false,
            public_url: String::new(),
            redirect_uri: None,
            roles_claim: DEFAULT_ROLES_CLAIM.into(),
            allowed_role: None,
            user_role: None,
            updated_at: None,
        },
    }
}

async fn get_settings(State(state): State<AppState>) -> R<Json<SettingsView>> {
    Ok(Json(settings_view(db::get_oidc_settings(&state.db).await?)))
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SettingsInput {
    enabled: bool,
    display_name: String,
    issuer_url: String,
    client_id: String,
    /// Omitted or null keeps the stored secret; an empty string removes it (public client).
    #[serde(default)]
    client_secret: Option<String>,
    public_url: String,
    #[serde(default)]
    roles_claim: String,
    #[serde(default)]
    allowed_role: Option<String>,
    #[serde(default)]
    user_role: Option<String>,
}

const DEFAULT_ROLES_CLAIM: &str = "roles";

/// https, or http for a provider or gateway on this machine (local development).
fn valid_url(s: &str) -> bool {
    reqwest::Url::parse(s).is_ok_and(|u| match u.scheme() {
        "https" => u.host_str().is_some(),
        "http" => matches!(u.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")),
        _ => false,
    })
}

async fn put_settings(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(i): Json<SettingsInput>,
) -> R<Json<SettingsView>> {
    let existing = db::get_oidc_settings(&state.db).await?;
    let issuer_url = i.issuer_url.trim().trim_end_matches('/').to_string();
    let public_url = i.public_url.trim().trim_end_matches('/').to_string();
    // The stored secret goes to the issuer's token endpoint: a provider on another host needs it
    // entered again, so changing the issuer cannot collect it.
    let moved = existing.as_ref().is_some_and(|e| e.client_secret.is_some() && !crate::util::same_origin(&e.issuer_url, &issuer_url));
    let client_secret = match i.client_secret {
        None if moved => {
            return Err(AdminError::bad_request("the issuer is on another host now: enter the client secret again"));
        }
        None => existing.as_ref().and_then(|e| e.client_secret.clone()),
        Some(s) if s.is_empty() => None,
        Some(s) => Some(state.cfg.secret_box.seal(s.as_bytes())),
    };
    let row = OidcSettingsRow {
        enabled: i.enabled,
        display_name: Some(i.display_name.trim()).filter(|s| !s.is_empty()).unwrap_or("Single sign-on").to_string(),
        issuer_url,
        client_id: i.client_id.trim().to_string(),
        client_secret,
        public_url,
        roles_claim: Some(i.roles_claim.trim()).filter(|s| !s.is_empty()).unwrap_or(DEFAULT_ROLES_CLAIM).to_string(),
        allowed_role: i.allowed_role.map(|r| r.trim().to_string()).filter(|r| !r.is_empty()),
        user_role: i.user_role.map(|r| r.trim().to_string()).filter(|r| !r.is_empty()),
        updated_at: Utc::now(),
    };
    // Switched off, the provider may be left unset; what is entered is still checked.
    if row.enabled && row.issuer_url.is_empty() {
        return Err(AdminError::bad_request("set issuer_url to enable single sign-on"));
    }
    if !row.issuer_url.is_empty() && !valid_url(&row.issuer_url) {
        return Err(AdminError::bad_request("issuer_url must be an https URL (http only for localhost)"));
    }
    if !valid_url(&row.public_url) {
        return Err(AdminError::bad_request("public_url must be the gateway's https URL (http only for localhost)"));
    }
    if row.enabled && row.client_id.is_empty() {
        return Err(AdminError::bad_request("set client_id to enable single sign-on"));
    }
    if row.display_name.len() > 60 {
        return Err(AdminError::bad_request("display_name can be at most 60 characters"));
    }
    if row.enabled && row.allowed_role.is_none() {
        return Err(AdminError::bad_request("set the allowed role; otherwise everyone at the provider could sign in"));
    }
    if row.roles_claim.len() > 200 || [&row.allowed_role, &row.user_role].iter().any(|r| r.as_ref().is_some_and(|r| r.len() > 200)) {
        return Err(AdminError::bad_request("roles_claim, allowed_role and user_role can be at most 200 characters"));
    }
    if row.user_role.is_some() && row.user_role == row.allowed_role {
        return Err(AdminError::bad_request("the user role must differ from the allowed (admin) role"));
    }
    let before = settings_view(existing);
    db::save_oidc_settings(&state.db, &row).await?;
    tracing::info!(enabled = row.enabled, issuer = %row.issuer_url, "OIDC settings saved");
    let after = settings_view(Some(row));
    let changes = audit::changes(&before, &after);
    if !changes.is_empty() {
        audit::Event::new("oidc_settings", "update").named("Single sign-on").details(changes.into()).record(&state, &actor).await;
    }
    Ok(Json(after))
}

/// Checks discovery against the stored settings, without signing in.
async fn test_settings(State(state): State<AppState>) -> R<Json<Value>> {
    let Some(row) = db::get_oidc_settings(&state.db).await? else {
        return Err(AdminError::bad_request("save the settings first"));
    };
    let sso = decrypt(&state, row).map_err(|e| AdminError::bad_request(e.to_string()))?;
    // The admin wants to know whether the provider answers now, not what it said ten minutes ago.
    Ok(Json(match client(&state.cfg.upstream_policy, &sso, false).await {
        Ok(c) => json!({ "ok": true, "authorization_endpoint": c.auth_uri().to_string() }),
        Err(e) => json!({ "ok": false, "error": e.to_string() }),
    }))
}

/// Someone who signed in, with the projects they work in (users only; admins see every project).
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct PersonView {
    #[serde(flatten)]
    person: crate::model::AdminUserRow,
    projects: Vec<Uuid>,
}

async fn list_admins(State(state): State<AppState>) -> R<Json<Vec<PersonView>>> {
    let (people, members) = tokio::try_join!(db::list_admin_users(&state.db), db::list_project_members(&state.db))?;
    Ok(Json(
        people
            .into_iter()
            .map(|person| {
                let mut projects: Vec<Uuid> = members.iter().filter(|(u, _)| *u == person.id).map(|(_, p)| *p).collect();
                projects.sort();
                PersonView { person, projects }
            })
            .collect(),
    ))
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PersonProjects {
    project_ids: Vec<Uuid>,
}

/// The projects a user works in: there they manage keys and see the playground, request log and
/// metrics. Takes effect on their next request.
async fn set_admin_projects(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<Uuid>,
    Json(mut i): Json<PersonProjects>,
) -> R<Json<PersonView>> {
    let person = db::get_admin_user(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    if person.role != PersonRole::User {
        return Err(AdminError::bad_request("admins see every project; only users are added to projects"));
    }
    i.project_ids.sort();
    i.project_ids.dedup();
    let known: Vec<Uuid> = db::list_project_rows(&state.db).await?.into_iter().map(|p| p.id).collect();
    if let Some(p) = i.project_ids.iter().find(|p| !known.contains(p)) {
        return Err(AdminError::bad_request(format!("project_ids: {p} names no project")));
    }
    let before = db::user_projects(&state.db, id).await?;
    db::set_user_projects(&state.db, id, &i.project_ids).await?;
    if before != i.project_ids {
        let name = person.email.clone().or(person.name.clone()).unwrap_or(person.subject.clone());
        let details = json!({ "projects": { "from": before, "to": i.project_ids } });
        audit::Event::new("admin", "update").on(id, name).details(details).record(&state, &actor).await;
    }
    Ok(Json(PersonView { person, projects: i.project_ids }))
}

async fn delete_admin(State(state): State<AppState>, Extension(actor): Extension<Actor>, Path(id): Path<Uuid>) -> R<StatusCode> {
    let admin = db::get_admin_user(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    if !db::delete_admin_user(&state.db, id).await? {
        return Err(AdminError::not_found());
    }
    tracing::info!(admin = %id, "admin removed; their sessions end on their next request");
    let name = admin.email.or(admin.name).unwrap_or(admin.subject);
    audit::Event::new("admin", "delete").on(id, name).record(&state, &actor).await;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_chains_skip_repeated_causes() {
        #[derive(Debug)]
        struct E(String, Option<Box<E>>);
        impl std::fmt::Display for E {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
        impl std::error::Error for E {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                self.1.as_deref().map(|e| e as _)
            }
        }
        let inner = E("HTTP 400".into(), None);
        let middle = E("refresh failed: HTTP 400".into(), Some(Box::new(inner)));
        let outer = E("token failed".into(), Some(Box::new(middle)));
        assert_eq!(error_chain(&outer), "token failed: refresh failed: HTTP 400");
    }

    fn sso() -> Sso {
        Sso {
            display_name: String::new(),
            issuer_url: String::new(),
            client_id: String::new(),
            client_secret: None,
            public_url: String::new(),
            roles_claim: "role".into(),
            allowed_role: Some("gateway-admin".into()),
            user_role: None,
            updated_at: Utc::now(),
        }
    }

    fn who(roles: &[&str]) -> Identity {
        Identity {
            issuer: "https://idp".into(),
            subject: "s".into(),
            email: None,
            name: None,
            roles: roles.iter().map(|g| g.to_string()).collect(),
        }
    }

    #[test]
    fn sign_in_needs_a_role() {
        let s = sso();
        assert_eq!(s.role_of(&who(&["user", "gateway-admin"])), Ok(PersonRole::Admin));
        assert!(s.role_of(&who(&["Gateway-Admin"])).is_err());
        assert!(s.role_of(&who(&["gateway-admins"])).is_err());
        assert!(s.role_of(&who(&[])).is_err());
        assert!(Sso { allowed_role: None, ..sso() }.role_of(&who(&["gateway-admin"])).is_err());
        // Without a user role, nobody signs in as a user.
        assert!(s.role_of(&who(&["gateway-user"])).is_err());
        let s = Sso { user_role: Some("gateway-user".into()), ..sso() };
        assert_eq!(s.role_of(&who(&["gateway-user"])), Ok(PersonRole::User));
        // The allowed role wins.
        assert_eq!(s.role_of(&who(&["gateway-user", "gateway-admin"])), Ok(PersonRole::Admin));
        // A user role alone admits nobody without an allowed role.
        assert!(Sso { allowed_role: None, ..s }.role_of(&who(&["gateway-user"])).is_err());
    }

    #[test]
    fn roles_from_claims() {
        let p = json!({
            "role": "admin",
            "roles": ["a", "b", 3],
            "realm_access": { "roles": ["kc-admin"] },
            "urn:zitadel:iam:org:project:roles": { "z-admin": { "123": "org" } },
            "a.b": ["dotted-key"],
        });
        let p = p.as_object().unwrap();
        assert_eq!(claim_roles(p, "role"), ["admin"]);
        assert_eq!(claim_roles(p, "roles"), ["a", "b"]);
        assert_eq!(claim_roles(p, "realm_access.roles"), ["kc-admin"]);
        assert_eq!(claim_roles(p, "urn:zitadel:iam:org:project:roles"), ["z-admin"]);
        assert_eq!(claim_roles(p, "a.b"), ["dotted-key"]);
        assert!(claim_roles(p, "missing.path").is_empty());
    }

    /// The admin API takes keys whose scope includes it; other working keys get 403, unknown and
    /// disabled ones 401.
    #[tokio::test]
    async fn admin_api_takes_admin_scoped_keys() {
        use axum::body::Body;
        use tower::ServiceExt;

        let state = crate::auth::tests::state_with_keys(&[
            ("sk-gw-admin", KeyScope::Admin, true),
            ("sk-gw-all", KeyScope::All, true),
            ("sk-gw-inf", KeyScope::Inference, true),
            ("sk-gw-off", KeyScope::Admin, false),
        ]);
        // Not a literal: the OpenAPI test reads `.route("…")` calls in this file as served routes.
        let path = "/keys";
        let app = Router::new()
            .route(path, get(|| async { "ok" }).post(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(state.clone(), require_admin))
            .with_state(state);
        for (key, status) in [
            (Some("sk-gw-admin"), StatusCode::OK),
            (Some("sk-gw-all"), StatusCode::OK),
            (Some("sk-gw-inf"), StatusCode::FORBIDDEN),
            (Some("sk-gw-off"), StatusCode::UNAUTHORIZED),
            (Some("sk-gw-unknown"), StatusCode::UNAUTHORIZED),
            (None, StatusCode::UNAUTHORIZED),
        ] {
            // A bearer key needs no CSRF header, also for changes.
            for method in [Method::GET, Method::POST] {
                let mut req = Request::builder().method(method.clone()).uri("/keys");
                if let Some(k) = key {
                    req = req.header("authorization", format!("Bearer {k}"));
                }
                let resp = app.clone().oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
                assert_eq!(resp.status(), status, "{key:?} {method}");
            }
        }
    }

    /// A project key reaches what a user who works in its project does: the user routes, limited
    /// to that project, and no others. Once its project is deleted it is limited to none.
    #[tokio::test]
    async fn project_keys_reach_their_project_only() {
        use axum::body::Body;
        use tower::ServiceExt;

        let project = Uuid::now_v7();
        let state =
            crate::auth::tests::state_with_keys(&[("sk-gw-project", KeyScope::Project, true), ("sk-gw-orphan", KeyScope::Project, true)]);
        let mut snap = crate::registry::Snapshot::default();
        for (hash, k) in &state.registry.snapshot().keys {
            let mut k = (**k).clone();
            k.project_id = (k.name == "sk-gw-project").then_some(project);
            snap.keys.insert(*hash, std::sync::Arc::new(k));
        }
        state.registry.set_snapshot(snap);
        // Not literals: the OpenAPI test reads `.route("…")` calls in this file as served routes.
        let (keys, credentials) = ("/keys", "/credentials");
        let api = Router::new()
            .route(keys, get(|axum::Extension(a): axum::Extension<Access>| async move { Json(json!(a.projects())) }))
            .route(credentials, get(|| async { "ok" }))
            .route_layer(axum::middleware::from_fn_with_state(state.clone(), require_admin))
            .with_state(state);
        let app = Router::new().nest("/admin/api", api);
        let call = |key: &'static str, uri: &'static str| {
            let app = app.clone();
            async move {
                let req = Request::get(uri).header("authorization", format!("Bearer {key}")).body(Body::empty()).unwrap();
                let resp = app.oneshot(req).await.unwrap();
                let status = resp.status();
                let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
                (status, serde_json::from_slice::<Value>(&body).ok())
            }
        };
        assert_eq!(call("sk-gw-project", "/admin/api/keys").await, (StatusCode::OK, Some(json!([project]))));
        assert_eq!(call("sk-gw-orphan", "/admin/api/keys").await, (StatusCode::OK, Some(json!([]))));
        assert_eq!(call("sk-gw-project", "/admin/api/credentials").await.0, StatusCode::FORBIDDEN);
    }

    /// The API reference is for whoever the admin API takes; keys whose scope is the inference API
    /// and anonymous callers get 401.
    #[tokio::test]
    async fn api_reference_needs_admin_api_access() {
        use axum::body::Body;
        use tower::ServiceExt;

        let state = crate::auth::tests::state_with_keys(&[
            ("sk-gw-admin", KeyScope::Admin, true),
            ("sk-gw-project", KeyScope::Project, true),
            ("sk-gw-inf", KeyScope::Inference, true),
        ]);
        // Not literals: the OpenAPI test reads `.route("…")` calls in this file as served routes.
        let (doc, page) = ("/openapi.json", "/docs/");
        let app = Router::new()
            .route(doc, get(|| async { "doc" }))
            .route(page, get(|| async { "page" }))
            .layer(axum::middleware::from_fn_with_state(state.clone(), require_signed_in))
            .with_state(state);
        for (key, status) in [
            (Some("sk-gw-admin"), StatusCode::OK),
            (Some("sk-gw-project"), StatusCode::OK),
            (Some("sk-gw-inf"), StatusCode::UNAUTHORIZED),
            (None, StatusCode::UNAUTHORIZED),
        ] {
            for uri in [doc, page] {
                let mut req = Request::get(uri);
                if let Some(k) = key {
                    req = req.header("authorization", format!("Bearer {k}"));
                }
                let resp = app.clone().oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
                assert_eq!(resp.status(), status, "{key:?} {uri}");
                if key.is_none() {
                    let html = resp.headers()[axum::http::header::CONTENT_TYPE].to_str().unwrap().starts_with("text/html");
                    assert_eq!(html, uri == page, "{uri}: the page gets a note, the document an error");
                }
            }
        }
    }

    /// A key whose scope is the inference API reads its own model hub, and nothing else.
    #[tokio::test]
    async fn inference_keys_reach_their_hub_only() {
        use axum::body::Body;
        use tower::ServiceExt;

        let state =
            crate::auth::tests::state_with_keys(&[("sk-gw-inf", KeyScope::Inference, true), ("sk-gw-off", KeyScope::Inference, false)]);
        let id = state.registry.snapshot().keys.values().find(|k| k.name == "sk-gw-inf").unwrap().id;
        // Not literals: the OpenAPI test reads `.route("…")` calls in this file as served routes.
        let (hub, keys) = ("/hub", "/keys");
        let api = Router::new()
            .route(hub, get(|axum::Extension(OwnKey(id)): axum::Extension<OwnKey>| async move { Json(json!(id)) }))
            .route(keys, get(|| async { Json(json!("ok")) }))
            .route_layer(axum::middleware::from_fn_with_state(state.clone(), require_admin))
            .with_state(state);
        let app = Router::new().nest("/admin/api", api);
        let call = |key: &'static str, uri: &'static str| {
            let app = app.clone();
            async move {
                let req = Request::get(uri).header("authorization", format!("Bearer {key}")).body(Body::empty()).unwrap();
                let resp = app.oneshot(req).await.unwrap();
                let status = resp.status();
                let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
                (status, serde_json::from_slice::<Value>(&body).ok())
            }
        };
        assert_eq!(call("sk-gw-inf", "/admin/api/hub").await, (StatusCode::OK, Some(json!(id))));
        assert_eq!(call("sk-gw-inf", "/admin/api/keys").await.0, StatusCode::FORBIDDEN);
        assert_eq!(call("sk-gw-off", "/admin/api/hub").await.0, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn urls() {
        assert!(valid_url("https://login.example.com/realms/x"));
        assert!(valid_url("http://localhost:8080"));
        assert!(!valid_url("http://gateway.internal"));
        assert!(!valid_url("ftp://x"));
    }
}
