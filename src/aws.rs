//! AWS credentials, region and request signing for Bedrock, via the official AWS SDK crates.
//!
//! Deployments with stored access keys use them directly. Otherwise credentials come from the
//! SDK's default provider chain (environment, shared profile, SSO, web identity / IRSA, EKS Pod
//! Identity, ECS task role, EC2 instance profile). The chain is loaded on first use, so
//! gateways without Bedrock deployments never probe instance metadata.

use std::time::{Duration, SystemTime};

use aws_config::{BehaviorVersion, SdkConfig};
use aws_credential_types::{Credentials, provider::ProvideCredentials};
use aws_sigv4::{
    http_request::{SignableBody, SignableRequest, SigningSettings, sign},
    sign::v4,
};
use aws_smithy_runtime_api::client::identity::Identity;
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use tokio::sync::{Mutex, OnceCell};

use crate::{
    error::GatewayError,
    model::{CredentialSettings, Deployment, Secrets},
};

/// Refresh temporary credentials this long before they expire.
const REFRESH_MARGIN: Duration = Duration::from_secs(300);

#[derive(Default)]
pub struct Aws {
    sdk: OnceCell<SdkConfig>,
    cached: Mutex<Option<Credentials>>,
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
        let provider = self
            .sdk()
            .await
            .credentials_provider()
            .ok_or_else(|| GatewayError::internal("no AWS credentials provider is available"))?;
        let creds = provider.provide_credentials().await.map_err(|e| {
            tracing::error!(error = %e, "failed to resolve AWS credentials");
            GatewayError::internal("bedrock deployment has no usable AWS credentials (see gateway logs)")
        })?;
        *cached = Some(creds.clone());
        Ok(creds)
    }

    /// Add SigV4 headers for a POST of `body` to `url`. `headers` must already hold every
    /// header that will be sent and signed (e.g. `content-type`).
    pub async fn sign(&self, d: &Deployment, url: &str, headers: &mut HeaderMap, body: &[u8]) -> Result<(), GatewayError> {
        let region = self.region(d).await?;
        let service = d.config.aws_service.as_deref().unwrap_or("bedrock");
        self.sign_as(&d.secrets, &region, service, url, headers, body).await
    }

    /// Like `sign`, with a credential's secrets (or the SDK's chain) for `region` and `service`.
    pub async fn sign_as(
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
    let pairs: Vec<(&str, &str)> = headers
        .iter()
        .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.as_str(), v)))
        .collect();
    let signable =
        SignableRequest::new("POST", url, pairs.into_iter(), SignableBody::Bytes(body)).map_err(|e| fail(&e))?;
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
    fn signs_bedrock_request() {
        let identity: Identity =
            Credentials::new("AKIDEXAMPLE", "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY", Some("token".into()), None, "test")
                .into();
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
