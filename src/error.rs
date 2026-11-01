use axum::{
    Json,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Serialize;

/// Which client dialect an error should be rendered in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Anthropic,
    OpenAi,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    InvalidRequest,
    Authentication,
    Permission,
    NotFound,
    RateLimit,
    Overloaded,
    Upstream,
    Timeout,
    Internal,
}

#[derive(Debug, Clone)]
pub struct GatewayError {
    pub kind: ErrorKind,
    pub status: StatusCode,
    pub message: String,
    pub retry_after: Option<u64>,
    /// The failure came from an upstream provider (not from validation or our own limits).
    pub upstream: bool,
    /// OpenAI's `error.code`, such as `content_filter`.
    pub code: Option<&'static str>,
}

impl std::fmt::Display for GatewayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.status.as_u16())
    }
}

impl std::error::Error for GatewayError {}

impl GatewayError {
    pub fn new(kind: ErrorKind, status: StatusCode, message: impl Into<String>) -> Self {
        Self { kind, status, message: message.into(), retry_after: None, upstream: false, code: None }
    }

    /// A guardrail refused the prompt; `message` is the guardrail's answer. OpenAI clients know the
    /// `content_filter` code from Azure OpenAI.
    pub fn content_filter(message: impl Into<String>) -> Self {
        Self { code: Some("content_filter"), ..Self::invalid(message) }
    }

    pub fn invalid(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidRequest, StatusCode::BAD_REQUEST, msg)
    }

    pub fn unauthorized(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::Authentication, StatusCode::UNAUTHORIZED, msg)
    }

    pub fn forbidden(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::Permission, StatusCode::FORBIDDEN, msg)
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, StatusCode::NOT_FOUND, msg)
    }

    pub fn rate_limited(msg: impl Into<String>, retry_after: u64) -> Self {
        Self { retry_after: Some(retry_after), ..Self::new(ErrorKind::RateLimit, StatusCode::TOO_MANY_REQUESTS, msg) }
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, StatusCode::INTERNAL_SERVER_ERROR, msg)
    }

    pub fn unavailable(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::Overloaded, StatusCode::SERVICE_UNAVAILABLE, msg)
    }

    pub fn timeout(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::Timeout, StatusCode::GATEWAY_TIMEOUT, msg)
    }

    /// Classify an upstream HTTP failure, keeping the upstream's JSON error message for the
    /// client. Other bodies (HTML pages, plain text from whatever answered) are not relayed; the
    /// gateway log has them.
    pub fn from_upstream(status: StatusCode, body: &str) -> Self {
        let message = extract_message(body).unwrap_or_else(|| format!("HTTP {} without a JSON error body", status.as_u16()));
        let message = format!("upstream error: {message}");
        let kind = match status.as_u16() {
            400 | 404 | 413 | 422 => ErrorKind::InvalidRequest,
            401 => ErrorKind::Upstream,
            403 => ErrorKind::Upstream,
            408 | 504 => ErrorKind::Timeout,
            429 => ErrorKind::RateLimit,
            503 | 529 => ErrorKind::Overloaded,
            _ => ErrorKind::Upstream,
        };
        // Auth problems with *our* provider credentials are a gateway fault, not the client's.
        let status = match status.as_u16() {
            401 | 403 => StatusCode::BAD_GATEWAY,
            s if s >= 500 && s != 503 && s != 504 => StatusCode::BAD_GATEWAY,
            _ => status,
        };
        Self { upstream: true, ..Self::new(kind, status, message) }
    }

    pub fn from_reqwest(e: &reqwest::Error) -> Self {
        // Clients get a fixed message; the full error chain (for example why the network guard
        // refused an address) goes to the gateway log.
        let err = if e.is_timeout() {
            Self::timeout("upstream timed out")
        } else if e.is_connect() {
            tracing::warn!(error = %crate::util::error_chain(e), "could not connect to upstream");
            Self::new(ErrorKind::Upstream, StatusCode::BAD_GATEWAY, "could not connect to upstream (see gateway logs)")
        } else {
            tracing::warn!(error = %crate::util::error_chain(e), "upstream transport error");
            Self::new(ErrorKind::Upstream, StatusCode::BAD_GATEWAY, "upstream transport error (see gateway logs)")
        };
        Self { upstream: true, ..err }
    }

    /// An error reported inside an upstream stream (e.g. an `overloaded_error` event).
    pub fn upstream_stream(kind: ErrorKind, message: impl Into<String>) -> Self {
        let status = match kind {
            ErrorKind::RateLimit => StatusCode::TOO_MANY_REQUESTS,
            ErrorKind::Overloaded => StatusCode::SERVICE_UNAVAILABLE,
            ErrorKind::InvalidRequest => StatusCode::BAD_REQUEST,
            ErrorKind::Timeout => StatusCode::GATEWAY_TIMEOUT,
            _ => StatusCode::BAD_GATEWAY,
        };
        Self { upstream: true, ..Self::new(kind, status, format!("upstream error: {}", message.into())) }
    }

    /// Whether another deployment might succeed where this one failed. Upstream failures of
    /// these kinds also put the failing deployment into cooldown.
    pub fn is_retryable(&self) -> bool {
        self.upstream && matches!(self.kind, ErrorKind::RateLimit | ErrorKind::Overloaded | ErrorKind::Upstream | ErrorKind::Timeout)
    }

    pub fn anthropic_type(&self) -> &'static str {
        match self.kind {
            ErrorKind::InvalidRequest => "invalid_request_error",
            ErrorKind::Authentication => "authentication_error",
            ErrorKind::Permission => "permission_error",
            ErrorKind::NotFound => "not_found_error",
            ErrorKind::RateLimit => "rate_limit_error",
            ErrorKind::Overloaded => "overloaded_error",
            ErrorKind::Timeout | ErrorKind::Upstream | ErrorKind::Internal => "api_error",
        }
    }

    pub fn openai_type(&self) -> &'static str {
        match self.kind {
            ErrorKind::InvalidRequest | ErrorKind::NotFound => "invalid_request_error",
            ErrorKind::Authentication => "authentication_error",
            ErrorKind::Permission => "permission_error",
            ErrorKind::RateLimit => "rate_limit_exceeded",
            ErrorKind::Overloaded | ErrorKind::Timeout | ErrorKind::Upstream | ErrorKind::Internal => "server_error",
        }
    }

    pub fn body(&self, dialect: Dialect) -> serde_json::Value {
        let message = self.message.clone();
        let body = match dialect {
            Dialect::Anthropic => {
                serde_json::to_value(AnthropicError { kind: "error", error: AnthropicErrorDetail { kind: self.anthropic_type(), message } })
            }
            Dialect::OpenAi => serde_json::to_value(OpenAiError {
                error: OpenAiErrorDetail { message, kind: self.openai_type(), param: None, code: self.code },
            }),
        };
        body.expect("error bodies serialize")
    }

    pub fn into_response_for(self, dialect: Dialect) -> Response {
        let mut resp = (self.status, Json(self.body(dialect))).into_response();
        if let Some(secs) = self.retry_after
            && let Ok(v) = HeaderValue::from_str(&secs.to_string())
        {
            resp.headers_mut().insert(header::RETRY_AFTER, v);
        }
        resp
    }
}

/// An error, in the OpenAI API's format.
#[derive(Serialize, utoipa::ToSchema)]
pub struct OpenAiError {
    error: OpenAiErrorDetail,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct OpenAiErrorDetail {
    message: String,
    #[serde(rename = "type")]
    kind: &'static str,
    /// Always null: the gateway does not say which parameter was wrong.
    param: Option<String>,
    /// Such as `content_filter`, when a guardrail refused the prompt.
    code: Option<&'static str>,
}

/// An error, in the Anthropic API's format.
#[derive(Serialize, utoipa::ToSchema)]
pub struct AnthropicError {
    /// Always `error`.
    #[serde(rename = "type")]
    kind: &'static str,
    error: AnthropicErrorDetail,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct AnthropicErrorDetail {
    #[serde(rename = "type")]
    kind: &'static str,
    message: String,
}

fn extract_message(body: &str) -> Option<String> {
    let mut v: serde_json::Value = serde_json::from_str(body).ok()?;
    // Google's OpenAI-compatible endpoints wrap the error object in an array.
    if let serde_json::Value::Array(items) = &mut v {
        v = items.first().cloned()?;
    }
    // Mistral (FastAPI): `detail` is a string, or a list of validation errors.
    if let Some(first) = v.pointer("/detail/0") {
        let field = first["loc"].as_array().and_then(|l| l.last()).and_then(|f| f.as_str());
        let msg = first["msg"].as_str()?;
        return Some(crate::util::truncate(&field.map_or(msg.to_string(), |f| format!("{f}: {msg}")), 1000));
    }
    let msg = v
        .pointer("/error/message")
        .or_else(|| v.get("message"))
        .or_else(|| v.get("Message"))
        .or_else(|| v.get("detail"))
        .or_else(|| v.get("error").filter(|e| e.is_string()))?
        .as_str()?;
    Some(crate::util::truncate(msg, 1000))
}

/// Error type for admin endpoints.
#[derive(Debug)]
pub struct AdminError(pub StatusCode, pub String);

impl AdminError {
    pub fn bad_request(m: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, m.into())
    }
    pub fn not_found() -> Self {
        Self(StatusCode::NOT_FOUND, "not found".into())
    }
    pub fn conflict(m: impl Into<String>) -> Self {
        Self(StatusCode::CONFLICT, m.into())
    }
    pub fn forbidden(m: impl Into<String>) -> Self {
        Self(StatusCode::FORBIDDEN, m.into())
    }
    pub fn internal(m: impl Into<String>) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, m.into())
    }
    /// An upstream the request needed failed.
    pub fn bad_gateway(m: impl Into<String>) -> Self {
        Self(StatusCode::BAD_GATEWAY, m.into())
    }
    /// Valkey is out of reach, or the replica is shutting down.
    pub fn unavailable(m: impl Into<String>) -> Self {
        Self(StatusCode::SERVICE_UNAVAILABLE, m.into())
    }
    /// For `map_err` on an insert or update: a second `what` (such as "a credential") with the
    /// same name is a conflict; any other database error converts as usual.
    pub fn conflict_on_duplicate(what: &'static str) -> impl Fn(sqlx::Error) -> Self {
        move |e| if crate::db::is_unique_violation(&e) { Self::conflict(format!("{what} with this name already exists")) } else { e.into() }
    }
}

impl From<sqlx::Error> for AdminError {
    fn from(e: sqlx::Error) -> Self {
        tracing::error!(error = %e, "database error");
        Self(StatusCode::INTERNAL_SERVER_ERROR, "database error".into())
    }
}

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        (self.0, Json(AdminErrorBody { error: self.1 })).into_response()
    }
}

/// An admin API error.
#[derive(Serialize, utoipa::ToSchema)]
#[schema(as = AdminError)]
pub struct AdminErrorBody {
    error: String,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn error_bodies() {
        let e = GatewayError::content_filter("no");
        assert_eq!(
            e.body(Dialect::OpenAi),
            json!({ "error": { "message": "no", "type": "invalid_request_error", "param": null, "code": "content_filter" } })
        );
        assert_eq!(e.body(Dialect::Anthropic), json!({ "type": "error", "error": { "type": "invalid_request_error", "message": "no" } }));
    }

    #[test]
    fn upstream_error_messages() {
        assert_eq!(extract_message(r#"{"error":{"message":"bad"}}"#).as_deref(), Some("bad"));
        assert_eq!(
            extract_message(r#"[{"error":{"code":400,"message":"Please pass a valid API key"}}]"#).as_deref(),
            Some("Please pass a valid API key")
        );
        assert_eq!(extract_message(r#"{"detail":"Invalid API Key"}"#).as_deref(), Some("Invalid API Key"));
        assert_eq!(
            extract_message(r#"{"detail":[{"type":"extra_forbidden","loc":["body","user"],"msg":"Extra inputs are not permitted"}]}"#)
                .as_deref(),
            Some("user: Extra inputs are not permitted")
        );
        assert_eq!(extract_message("<html>"), None);
    }
}
