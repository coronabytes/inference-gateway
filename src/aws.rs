//! AWS credentials, region, SDK clients and request signing for Bedrock, via the official AWS SDK
//! crates.
//!
//! Deployments with stored access keys use them directly, those with a Bedrock API key send it as
//! a bearer token. Otherwise credentials come from the SDK's default provider chain (environment,
//! shared profile, SSO, web identity / IRSA, EKS Pod Identity, ECS task role, EC2 instance
//! profile). The chain is loaded on first use, so gateways without Bedrock deployments never probe
//! instance metadata.
//!
//! Bedrock calls go through the SDK's clients (`aws-sdk-bedrockruntime`,
//! `aws-sdk-bedrockagentruntime`), whose HTTP client resolves through the gateway's network guard.
//! They do not retry: the gateway fails over to another deployment itself. Only Claude through
//! bedrock-mantle, which no SDK covers, and Rerank with a Bedrock API key, which the Agent Runtime
//! SDK cannot send, are signed or sent here by hand.

use std::{
    sync::OnceLock,
    time::{Duration, SystemTime},
};

use aws_config::{BehaviorVersion, SdkConfig};
use aws_credential_types::{
    Credentials,
    provider::{ProvideCredentials, SharedCredentialsProvider},
};
use aws_sigv4::{
    http_request::{SignableBody, SignableRequest, SigningSettings, sign},
    sign::v4,
};
use aws_smithy_http_client::tls;
use aws_smithy_runtime_api::client::identity::Identity;
use aws_smithy_runtime_api::client::{http::SharedHttpClient, result::SdkError};
use aws_smithy_types::error::metadata::ProvideErrorMetadata;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, OnceCell};

use crate::{
    error::{ErrorKind, GatewayError},
    model::{CredentialSettings, Deployment, Secrets},
    state::AppState,
    util::TtlCache,
};

pub mod models;

/// Refresh temporary credentials this long before they expire.
const REFRESH_MARGIN: Duration = Duration::from_secs(300);

/// An AWS region the official SDK knows, from its built-in endpoint partitions (see build.rs).
#[derive(Serialize, Deserialize, utoipa::ToSchema)]
pub struct AwsRegion {
    /// e.g. `eu-central-1`.
    pub id: String,
    /// e.g. `Europe (Frankfurt)`.
    pub name: String,
    /// `aws`, `aws-eusc` (European Sovereign Cloud), `aws-us-gov` (GovCloud) or `aws-cn` (China).
    pub partition: String,
}

/// The regions of the SDK this gateway was built with; empty if the build could not read them.
pub fn regions() -> &'static [AwsRegion] {
    static REGIONS: std::sync::LazyLock<Vec<AwsRegion>> =
        std::sync::LazyLock::new(|| serde_json::from_str(include_str!(concat!(env!("OUT_DIR"), "/aws_regions.json"))).unwrap_or_default());
    &REGIONS
}

/// Clients for one region, endpoint and set of credentials.
type ClientKey = (String, Option<String>, [u8; 32]);
/// Built clients are kept while they are used, at most this many (credentials rotated many times).
const MAX_CLIENTS: usize = 256;
/// A client unused this long is dropped.
const CLIENT_IDLE: Duration = Duration::from_secs(3600);

pub struct Aws {
    sdk: OnceCell<SdkConfig>,
    cached: Mutex<Option<Credentials>>,
    /// The SDK clients' HTTP client, resolving through the network guard.
    http: OnceLock<SharedHttpClient>,
    runtime: TtlCache<ClientKey, aws_sdk_bedrockruntime::Client>,
    agent: TtlCache<ClientKey, aws_sdk_bedrockagentruntime::Client>,
    /// The console's Bedrock model lists, per credential, for five minutes.
    pub models: models::Cache,
}

impl Default for Aws {
    fn default() -> Self {
        Self {
            sdk: OnceCell::new(),
            cached: Mutex::new(None),
            http: OnceLock::new(),
            runtime: TtlCache::idle(CLIENT_IDLE, MAX_CLIENTS),
            agent: TtlCache::idle(CLIENT_IDLE, MAX_CLIENTS),
            models: models::Cache::default(),
        }
    }
}

impl Aws {
    async fn sdk(&self) -> &SdkConfig {
        self.sdk.get_or_init(|| aws_config::defaults(BehaviorVersion::latest()).load()).await
    }

    /// The deployment's configured region, else the SDK's (env, profile, instance metadata).
    pub async fn region(&self, d: &Deployment) -> Result<String, GatewayError> {
        self.region_for(&d.connection).await
    }

    /// A credential's configured region, else the SDK's.
    pub async fn region_for(&self, settings: &CredentialSettings) -> Result<String, GatewayError> {
        if let Some(r) = &settings.region {
            return Ok(r.clone());
        }
        self.sdk()
            .await
            .region()
            .map(|r| r.to_string())
            .ok_or_else(|| GatewayError::internal("bedrock credential has no region and none was found in the AWS environment"))
    }

    /// SDK configuration for a credential's API calls other than inference (the console's model
    /// list): its access keys, or else the SDK's chain, in its region.
    pub async fn sdk_config(&self, settings: &CredentialSettings, secrets: &Secrets) -> Result<SdkConfig, GatewayError> {
        let region = self.region_for(settings).await?;
        let mut config = self.sdk().await.to_builder();
        config.set_region(Some(aws_config::Region::new(region)));
        if let (Some(ak), Some(sk)) = (&secrets.aws_access_key_id, &secrets.aws_secret_access_key) {
            let keys = Credentials::new(ak, sk, secrets.aws_session_token.clone(), None, "gateway-credential");
            config.set_credentials_provider(Some(SharedCredentialsProvider::new(keys)));
        }
        Ok(config.build())
    }

    /// The SDK clients' HTTP client: rustls with aws-lc, like the gateway's own, and addresses
    /// checked by the network guard. It follows no redirects.
    fn http(&self, state: &AppState) -> SharedHttpClient {
        self.http
            .get_or_init(|| {
                aws_smithy_http_client::Builder::new()
                    .tls_provider(tls::Provider::Rustls(tls::rustls_provider::CryptoMode::AwsLc))
                    .build_with_resolver(crate::netguard::SdkResolver(state.cfg.upstream_policy.clone()))
            })
            .clone()
    }

    /// SDK configuration for Bedrock calls with a credential: its region and credentials, the
    /// guarded HTTP client, no retries and no stalled-stream cutoff (a model may think quietly for a
    /// while; the deployment's timeout applies instead).
    pub async fn service_config(
        &self,
        state: &AppState,
        settings: &CredentialSettings,
        secrets: &Secrets,
    ) -> Result<SdkConfig, GatewayError> {
        let mut config = self.sdk_config(settings, secrets).await?.to_builder();
        config.set_http_client(Some(self.http(state)));
        config.set_retry_config(Some(aws_smithy_types::retry::RetryConfig::disabled()));
        config.set_stalled_stream_protection(Some(aws_sdk_bedrockruntime::config::StalledStreamProtectionConfig::disabled()));
        Ok(config.build())
    }

    /// Which clients a connection and its secrets call for, with an endpoint override.
    async fn client_key(
        &self,
        state: &AppState,
        settings: &CredentialSettings,
        secrets: &Secrets,
        endpoint: Option<String>,
    ) -> Result<ClientKey, GatewayError> {
        if let Some(e) = &endpoint {
            // Hostnames are checked by the resolver; IP literals never reach it.
            state.cfg.upstream_policy.check_url(e).map_err(|why| GatewayError::internal(format!("upstream address not allowed: {why}")))?;
        }
        let mut h = Sha256::new();
        for v in [&secrets.api_key, &secrets.aws_access_key_id, &secrets.aws_secret_access_key, &secrets.aws_session_token] {
            h.update(v.as_deref().unwrap_or_default());
            h.update([0]);
        }
        Ok((self.region_for(settings).await?, endpoint, h.finalize().into()))
    }

    /// The Bedrock Runtime client for a deployment (Converse, InvokeModel, CountTokens).
    pub async fn runtime(&self, state: &AppState, d: &Deployment) -> Result<aws_sdk_bedrockruntime::Client, GatewayError> {
        self.runtime_for(state, &d.connection, &d.secrets, d.connection.base_url.clone()).await
    }

    /// The Bedrock Runtime client for a credential, with an endpoint override (a VPC endpoint).
    pub async fn runtime_for(
        &self,
        state: &AppState,
        settings: &CredentialSettings,
        secrets: &Secrets,
        endpoint: Option<String>,
    ) -> Result<aws_sdk_bedrockruntime::Client, GatewayError> {
        let key = self.client_key(state, settings, secrets, endpoint.clone()).await?;
        if let Some(c) = self.runtime.get(&key) {
            return Ok(c);
        }
        let mut config = aws_sdk_bedrockruntime::config::Builder::from(&self.service_config(state, settings, secrets).await?);
        if let Some(e) = endpoint {
            config = config.endpoint_url(e);
        }
        if let Some(k) = &secrets.api_key {
            config = config
                .bearer_token(aws_sdk_bedrockruntime::config::Token::new(k, None))
                .auth_scheme_preference([aws_smithy_runtime_api::client::auth::http::HTTP_BEARER_AUTH_SCHEME_ID]);
        }
        let client = aws_sdk_bedrockruntime::Client::from_conf(config.build());
        self.runtime.insert(key, client.clone());
        Ok(client)
    }

    /// The Bedrock Agent Runtime client for a deployment (Rerank), with AWS credentials. A runtime
    /// endpoint override moves to the Agent Runtime host.
    pub async fn agent_runtime(&self, state: &AppState, d: &Deployment) -> Result<aws_sdk_bedrockagentruntime::Client, GatewayError> {
        let endpoint = d.connection.base_url.as_ref().map(|u| u.replacen("bedrock-runtime", "bedrock-agent-runtime", 1));
        let key = self.client_key(state, &d.connection, &d.secrets, endpoint.clone()).await?;
        if let Some(c) = self.agent.get(&key) {
            return Ok(c);
        }
        let mut config = aws_sdk_bedrockagentruntime::config::Builder::from(&self.service_config(state, &d.connection, &d.secrets).await?);
        if let Some(e) = endpoint {
            config = config.endpoint_url(e);
        }
        let client = aws_sdk_bedrockagentruntime::Client::from_conf(config.build());
        self.agent.insert(key, client.clone());
        Ok(client)
    }

    async fn credentials(&self, secrets: &Secrets) -> Result<Credentials, GatewayError> {
        if let (Some(ak), Some(sk)) = (&secrets.aws_access_key_id, &secrets.aws_secret_access_key) {
            return Ok(Credentials::new(ak, sk, secrets.aws_session_token.clone(), None, "gateway-credential"));
        }
        let mut cached = self.cached.lock().await;
        if let Some(c) = cached.as_ref()
            && c.expiry().is_none_or(|exp| exp > SystemTime::now() + REFRESH_MARGIN)
        {
            return Ok(c.clone());
        }
        let provider =
            self.sdk().await.credentials_provider().ok_or_else(|| GatewayError::internal("no AWS credentials provider is available"))?;
        let creds = provider.provide_credentials().await.map_err(|e| {
            tracing::error!(error = %e, "failed to resolve AWS credentials");
            GatewayError::internal("bedrock deployment has no usable AWS credentials (see gateway logs)")
        })?;
        *cached = Some(creds.clone());
        Ok(creds)
    }

    /// Add SigV4 headers for a POST of `body` to `url`, for bedrock-mantle. `headers` must already
    /// hold every header that will be sent and signed (e.g. `content-type`).
    pub async fn sign(&self, d: &Deployment, url: &str, headers: &mut HeaderMap, body: &[u8]) -> Result<(), GatewayError> {
        let region = self.region(d).await?;
        let service = d.config.aws_service.as_deref().unwrap_or("bedrock");
        self.sign_as(&d.secrets, &region, service, url, headers, body).await
    }

    /// Like `sign`, with a credential's secrets (or the SDK's chain) for `region` and `service`.
    async fn sign_as(
        &self,
        secrets: &Secrets,
        region: &str,
        service: &str,
        url: &str,
        headers: &mut HeaderMap,
        body: &[u8],
    ) -> Result<(), GatewayError> {
        let identity: Identity = self.credentials(secrets).await?.into();
        sign_with(&identity, region, service, url, headers, body, SystemTime::now())
    }
}

/// Classify an SDK call's failure like an HTTP upstream's (see `GatewayError::from_upstream`):
/// the service's status and message, or a timeout or transport error.
pub fn sdk_error<E>(e: SdkError<E, aws_smithy_runtime_api::http::Response>) -> GatewayError
where
    E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
{
    match &e {
        SdkError::ServiceError(se) => {
            let status = StatusCode::from_u16(se.raw().status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let code = se.err().code().unwrap_or("error");
            let msg = se.err().message().unwrap_or(code);
            GatewayError::from_upstream(status, &serde_json::json!({ "message": format!("{code}: {msg}") }).to_string())
        }
        SdkError::TimeoutError(_) => GatewayError { upstream: true, ..GatewayError::timeout("upstream timed out") },
        _ => {
            tracing::warn!(error = %aws_smithy_types::error::display::DisplayErrorContext(&e), "Bedrock request failed");
            GatewayError {
                upstream: true,
                ..GatewayError::new(ErrorKind::Upstream, StatusCode::BAD_GATEWAY, "could not reach Bedrock (see gateway logs)")
            }
        }
    }
}

/// Classify an error inside a Bedrock response stream by its exception type, e.g.
/// `throttlingException` or `ThrottlingException`.
pub fn stream_error(code: Option<&str>, message: Option<&str>) -> GatewayError {
    let code = code.unwrap_or("exception");
    let kind = match code.to_ascii_lowercase().trim_end_matches("exception") {
        "throttling" => ErrorKind::RateLimit,
        "serviceunavailable" => ErrorKind::Overloaded,
        "validation" => ErrorKind::InvalidRequest,
        "modeltimeout" => ErrorKind::Timeout,
        _ => ErrorKind::Upstream,
    };
    GatewayError::upstream_stream(kind, format!("{code}: {}", message.unwrap_or(code)))
}

/// An error event inside a stream the SDK reads, classified by its exception type.
pub fn stream_sdk_error<E>(e: SdkError<E, aws_smithy_types::event_stream::RawMessage>) -> GatewayError
where
    E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
{
    match &e {
        SdkError::ServiceError(se) => stream_error(se.err().code(), se.err().message()),
        SdkError::TimeoutError(_) => GatewayError { upstream: true, ..GatewayError::timeout("upstream timed out") },
        _ => {
            tracing::warn!(error = %aws_smithy_types::error::display::DisplayErrorContext(&e), "Bedrock stream failed");
            GatewayError::upstream_stream(ErrorKind::Upstream, "the Bedrock stream broke off (see gateway logs)")
        }
    }
}

fn sign_with(
    identity: &Identity,
    region: &str,
    service: &str,
    url: &str,
    headers: &mut HeaderMap,
    body: &[u8],
    time: SystemTime,
) -> Result<(), GatewayError> {
    let fail = |e: &dyn std::fmt::Display| GatewayError::internal(format!("SigV4 signing failed: {e}"));
    let params = v4::SigningParams::builder()
        .identity(identity)
        .region(region)
        .name(service)
        .time(time)
        .settings(SigningSettings::default())
        .build()
        .map_err(|e| fail(&e))?
        .into();
    let pairs: Vec<(&str, &str)> = headers.iter().filter_map(|(k, v)| v.to_str().ok().map(|v| (k.as_str(), v))).collect();
    let signable = SignableRequest::new("POST", url, pairs.into_iter(), SignableBody::Bytes(body)).map_err(|e| fail(&e))?;
    let (instructions, _signature) = sign(signable, &params).map_err(|e| fail(&e))?.into_parts();
    let mut out = Vec::new();
    for (name, value) in instructions.headers() {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|e| fail(&e))?;
        let mut value = HeaderValue::from_str(value).map_err(|e| fail(&e))?;
        value.set_sensitive(true);
        out.push((name, value));
    }
    for (name, value) in out {
        headers.insert(name, value);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knows_the_sdk_regions() {
        let r = regions();
        assert!(r.iter().any(|r| r.id == "eu-central-1" && r.name == "Europe (Frankfurt)" && r.partition == "aws"));
        assert!(r.iter().any(|r| r.partition == "aws-us-gov"));
        assert!(!r.iter().any(|r| r.id.ends_with("-global") || r.partition.starts_with("aws-iso")));
    }

    #[test]
    fn signs_bedrock_request() {
        let identity: Identity =
            Credentials::new("AKIDEXAMPLE", "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY", Some("token".into()), None, "test").into();
        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        let url = "https://bedrock-runtime.us-east-1.amazonaws.com/model/anthropic.claude-v2%3A0/converse-stream";
        let time = SystemTime::UNIX_EPOCH + Duration::from_secs(1_440_938_160); // 2015-08-30T12:36:00Z
        sign_with(&identity, "us-east-1", "bedrock", url, &mut headers, b"{}", time).unwrap();

        let auth = headers["authorization"].to_str().unwrap();
        assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/bedrock/aws4_request"));
        assert!(auth.contains("SignedHeaders=content-type;host;x-amz-date;x-amz-security-token"));
        assert_eq!(headers["x-amz-date"], "20150830T123600Z");
        assert_eq!(headers["x-amz-security-token"], "token");
        assert!(headers["authorization"].is_sensitive());
    }
}
