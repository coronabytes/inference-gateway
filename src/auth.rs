use std::sync::Arc;

use axum::{
    extract::FromRequestParts,
    http::{HeaderMap, header, request::Parts},
    response::Response,
};
use subtle::ConstantTimeEq;

use crate::{
    error::{Dialect, GatewayError},
    model::VirtualKey,
    state::AppState,
};

pub const KEY_PREFIX: &str = "sk-gw-";

/// The calling virtual key, authenticated from the request headers alone. Handlers take it
/// before their body: axum runs header extractors first, so a request without a valid key is
/// refused before any of its body is read or buffered.
pub struct ClientKey(pub Arc<VirtualKey>);

impl FromRequestParts<AppState> for ClientKey {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Response> {
        authenticate(state, &parts.headers).map(ClientKey).map_err(|e| e.into_response_for(dialect_of(parts)))
    }
}

/// Which error shape a refused request gets: Anthropic's on the Messages endpoints (and on
/// `/v1/models` for Anthropic clients, which send `anthropic-version`), OpenAI's elsewhere.
pub(crate) fn dialect_of(parts: &Parts) -> Dialect {
    let path = parts.uri.path();
    if path.starts_with("/v1/messages") || (path == "/v1/models" && parts.headers.contains_key("anthropic-version")) {
        Dialect::Anthropic
    } else {
        Dialect::OpenAi
    }
}

/// Accepts `Authorization: Bearer <key>` (OpenAI clients) and `x-api-key` (Anthropic clients).
pub fn presented_key(headers: &HeaderMap) -> Option<&str> {
    if let Some(v) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        return Some(v.trim());
    }
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")))
        .map(str::trim)
}

pub fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<Arc<VirtualKey>, GatewayError> {
    let presented = presented_key(headers).ok_or_else(|| GatewayError::unauthorized("missing API key"))?;
    let hash = crate::util::sha256(presented.as_bytes());
    // Lookup by SHA-256 of a 256-bit random secret: no timing oracle on the secret itself.
    let key = state
        .registry
        .snapshot()
        .keys
        .get(&hash)
        .cloned()
        .ok_or_else(|| GatewayError::unauthorized("invalid API key"))?;
    for_inference(usable(key)?)
}

/// A key selected by id, for admin-authenticated callers such as the playground. The secret is
/// never stored, so the admin cannot present it; everything else (enabled, expiry, model access,
/// limits, budget, logging) applies exactly as for a client using the key.
pub fn key_by_id(state: &AppState, id: uuid::Uuid) -> Result<Arc<VirtualKey>, GatewayError> {
    let key = state
        .registry
        .snapshot()
        .keys
        .values()
        .find(|k| k.id == id)
        .cloned()
        .ok_or_else(|| GatewayError::not_found("virtual key not found"))?;
    for_inference(usable(key)?)
}

/// The usable virtual key presented in the headers, if any, for the admin guard: the caller
/// checks its scope. Unknown, disabled and expired keys are all `None`.
pub fn presented_virtual_key(state: &AppState, headers: &HeaderMap) -> Option<Arc<VirtualKey>> {
    let hash = crate::util::sha256(presented_key(headers)?.as_bytes());
    usable(state.registry.snapshot().keys.get(&hash).cloned()?).ok()
}

fn usable(key: Arc<VirtualKey>) -> Result<Arc<VirtualKey>, GatewayError> {
    if !key.enabled {
        return Err(GatewayError::unauthorized("API key is disabled"));
    }
    if !key.project_active {
        return Err(GatewayError::unauthorized("API key's project is inactive"));
    }
    if key.expires_at.is_some_and(|e| e <= chrono::Utc::now()) {
        return Err(GatewayError::unauthorized("API key has expired"));
    }
    Ok(key)
}

fn for_inference(key: Arc<VirtualKey>) -> Result<Arc<VirtualKey>, GatewayError> {
    if !key.scope.inference() {
        return Err(GatewayError::forbidden("this key's scope is the admin API only; it cannot call the inference API"));
    }
    Ok(key)
}

pub fn is_master(state: &AppState, headers: &HeaderMap) -> bool {
    let Some(presented) = presented_key(headers) else { return false };
    let a = crate::util::sha256(presented.as_bytes());
    let Some(master) = &state.cfg.master_key else { return false };
    let b = crate::util::sha256(master.as_bytes());
    a.ct_eq(&b).into()
}

/// New virtual key: `sk-gw-` + 32 random bytes hex. Returns (plaintext, hash, display prefix).
pub fn generate_key() -> (String, Vec<u8>, String) {
    let secret = format!("{KEY_PREFIX}{}", hex::encode(crate::util::random_bytes::<32>()));
    let hash = crate::util::sha256(secret.as_bytes()).to_vec();
    let prefix = format!("{}…{}", &secret[..KEY_PREFIX.len() + 4], &secret[secret.len() - 4..]);
    (secret, hash, prefix)
}

/// Enforce per-key request rate, token rate and budget before routing.
pub async fn check_key_limits(state: &AppState, key: &VirtualKey) -> Result<(), GatewayError> {
    if let Some(limit) = key.rpm_limit {
        let d = state.cache.rate_check(&format!("k:{}:rpm", key.id), limit as u64, 1).await;
        if !d.allowed {
            return Err(GatewayError::rate_limited("key request rate limit exceeded", d.retry_after));
        }
    }
    if let Some(limit) = key.tpm_limit {
        let d = state.cache.rate_check(&format!("k:{}:tpm", key.id), limit as u64, 0).await;
        if !d.allowed {
            return Err(GatewayError::rate_limited("key token rate limit exceeded", d.retry_after));
        }
    }
    if let Some(b) = crate::budget::status(state, key).await
        && b.exhausted()
    {
        let retry_after = (b.resets_at - chrono::Utc::now()).num_seconds().max(1) as u64;
        return Err(GatewayError::rate_limited(
            format!(
                "key budget of ${} per {} exhausted; it resets at {}",
                b.limit_usd,
                b.period.as_str(),
                b.resets_at.format("%Y-%m-%d %H:%M UTC")
            ),
            retry_after,
        ));
    }
    // The project's budget comes on top of the key's, with its own period.
    let project = key.project_id.and_then(|id| state.registry.snapshot().projects.get(&id).cloned());
    if let Some(p) = project
        && let Some(b) = crate::budget::project_status(state, &p).await
        && b.exhausted()
    {
        let retry_after = (b.resets_at - chrono::Utc::now()).num_seconds().max(1) as u64;
        return Err(GatewayError::rate_limited(
            format!(
                "project budget of ${} per {} exhausted; it resets at {}",
                b.limit_usd,
                b.period.as_str(),
                b.resets_at.format("%Y-%m-%d %H:%M UTC")
            ),
            retry_after,
        ));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::time::Duration;

    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode},
        routing::post,
    };
    use tower::ServiceExt;

    use crate::model::{KeyScope, VirtualKey};

    /// A test state whose registry holds a key per `(secret, scope, enabled)`.
    pub(crate) fn state_with_keys(keys: &[(&str, KeyScope, bool)]) -> crate::state::AppState {
        let state = crate::state::test_state();
        let mut snap = crate::registry::Snapshot::default();
        for (secret, scope, enabled) in keys {
            let hash = crate::util::sha256(secret.as_bytes());
            let key = VirtualKey {
                id: uuid::Uuid::now_v7(),
                name: secret.to_string(),
                key_hash: hash.to_vec(),
                key_prefix: String::new(),
                scope: *scope,
                project_id: None,
                project_active: true,
                allowed_models: vec![],
                allowed_tags: vec![],
                rpm_limit: None,
                tpm_limit: None,
                max_budget_usd: None,
                budget_period: None,
                spend_usd: 0.0,
                spend_reset_at: None,
                enabled: *enabled,
                expires_at: None,
                metadata: serde_json::json!({}),
                guardrail_ids: Vec::new(),
                web_search: true,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            };
            snap.keys.insert(hash, std::sync::Arc::new(key));
        }
        state.registry.set_snapshot(snap);
        state
    }

    /// A key scoped to the admin API is refused by the inference API, before its body is read.
    #[tokio::test]
    async fn admin_only_keys_cannot_call_models() {
        let state = state_with_keys(&[("sk-gw-admin", KeyScope::Admin, true)]);
        assert_eq!(super::authenticate(&state, &bearer("sk-gw-admin")).unwrap_err().status, StatusCode::FORBIDDEN);
        let app = Router::new().route("/v1/chat/completions", post(crate::api::chat_completions)).with_state(state);
        let endless = Body::from_stream(futures::stream::pending::<Result<bytes::Bytes, std::io::Error>>());
        let req = Request::post("/v1/chat/completions").header("authorization", "Bearer sk-gw-admin").body(endless).unwrap();
        let resp = tokio::time::timeout(Duration::from_secs(2), app.oneshot(req)).await.expect("refused from the headers").unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn inference_and_all_keys_can_call_models() {
        let state = state_with_keys(&[("sk-gw-inf", KeyScope::Inference, true), ("sk-gw-all", KeyScope::All, true)]);
        assert!(super::authenticate(&state, &bearer("sk-gw-inf")).is_ok());
        assert!(super::authenticate(&state, &bearer("sk-gw-all")).is_ok());
    }

    /// An inactive project blocks its keys on the inference API and the admin API alike.
    #[tokio::test]
    async fn inactive_projects_block_their_keys() {
        let state = state_with_keys(&[("sk-gw-blocked", KeyScope::All, true), ("sk-gw-open", KeyScope::All, true)]);
        let mut snap = crate::registry::Snapshot::default();
        for (hash, k) in &state.registry.snapshot().keys {
            let mut k = (**k).clone();
            k.project_active = k.name != "sk-gw-blocked";
            snap.keys.insert(*hash, std::sync::Arc::new(k));
        }
        state.registry.set_snapshot(snap);
        let refused = super::authenticate(&state, &bearer("sk-gw-blocked")).unwrap_err();
        assert_eq!((refused.status, refused.message.as_str()), (StatusCode::UNAUTHORIZED, "API key's project is inactive"));
        assert!(super::presented_virtual_key(&state, &bearer("sk-gw-blocked")).is_none());
        assert!(super::authenticate(&state, &bearer("sk-gw-open")).is_ok());
    }

    pub(crate) fn bearer(key: &str) -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        h.insert("authorization", format!("Bearer {key}").parse().unwrap());
        h
    }

    /// A request without a valid key is refused from its headers alone: its body, which here
    /// never finishes arriving, is not read. Before, every endpoint buffered the whole body
    /// (up to the `max_body_mb` setting) first.
    #[tokio::test]
    async fn refuses_before_reading_the_body() {
        let app = Router::new()
            .route("/v1/chat/completions", post(crate::api::chat_completions))
            .route("/v1/messages", post(crate::api::messages))
            .route("/v1/embeddings", post(crate::api::embeddings::embeddings))
            .route("/v1/decisions", post(crate::api::decisions::decisions))
            .with_state(crate::state::test_state());
        for (path, key, anthropic) in [
            ("/v1/chat/completions", None, false),
            ("/v1/chat/completions", Some("Bearer sk-gw-not-a-key"), false),
            ("/v1/messages", None, true),
            ("/v1/embeddings", None, false),
            ("/v1/decisions", None, false),
        ] {
            let endless = Body::from_stream(futures::stream::pending::<Result<bytes::Bytes, std::io::Error>>());
            let mut req = Request::post(path).header("content-type", "application/json");
            if let Some(k) = key {
                req = req.header("authorization", k);
            }
            let resp = tokio::time::timeout(Duration::from_secs(2), app.clone().oneshot(req.body(endless).unwrap()))
                .await
                .unwrap_or_else(|_| panic!("{path}: waited for the body instead of refusing"))
                .unwrap();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{path}");
            let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
            // Each endpoint answers in its own client's error shape.
            assert_eq!(v["type"] == "error", anthropic, "{path}: {v}");
            assert!(v["error"]["message"].is_string(), "{path}: {v}");
        }
    }
}
