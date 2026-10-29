//! Google service account authentication for Vertex AI.
//!
//! A credential stores the service account key file as uploaded. For each request the gateway
//! uses an OAuth access token, minted from a self-signed RS256 assertion (JWT bearer grant) and
//! cached until shortly before it expires. The token endpoint is always Google's own: the key
//! file's `token_uri` is ignored, so an uploaded file cannot send the signed assertion elsewhere.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use aws_lc_rs::{rand::SystemRandom, signature::RsaKeyPair};
use base64::Engine;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{error::GatewayError, model::Deployment, state::AppState};

const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
/// Tokens live an hour; a new one is fetched this long before the old one expires.
const REFRESH_MARGIN: Duration = Duration::from_secs(300);

/// The fields of a service account key file the gateway uses.
#[derive(Deserialize)]
pub struct ServiceAccount {
    #[serde(rename = "type")]
    kind: String,
    pub project_id: Option<String>,
    #[serde(default)]
    private_key_id: String,
    private_key: String,
    pub client_email: String,
}

impl ServiceAccount {
    /// Parse and check a key file, including its private key.
    pub fn parse(json: &str) -> Result<Self, String> {
        let sa: Self = serde_json::from_str(json).map_err(|e| format!("not a service account key file: {e}"))?;
        if sa.kind != "service_account" {
            return Err(format!("key file is of type '{}'; a service account key (type service_account) is needed", sa.kind));
        }
        if sa.client_email.trim().is_empty() {
            return Err("key file has no client_email".into());
        }
        sa.key_pair()?;
        Ok(sa)
    }

    fn key_pair(&self) -> Result<RsaKeyPair, String> {
        let b64: String = self.private_key.lines().filter(|l| !l.starts_with("-----")).map(str::trim).collect();
        let der = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|_| "the key file's private_key is not a PEM key".to_string())?;
        RsaKeyPair::from_pkcs8(&der).map_err(|_| "the key file's private_key is not an RSA PKCS#8 key".to_string())
    }

    /// The signed JWT bearer assertion exchanged for an access token.
    fn assertion(&self, now: u64) -> Result<String, String> {
        let b64 = |v: &Value| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).expect("json"));
        let header = json!({ "alg": "RS256", "typ": "JWT", "kid": self.private_key_id });
        let claims = json!({ "iss": self.client_email, "scope": SCOPE, "aud": TOKEN_URL, "iat": now, "exp": now + 3600 });
        let message = format!("{}.{}", b64(&header), b64(&claims));
        let key = self.key_pair()?;
        let mut signature = vec![0; key.public_modulus_len()];
        key.sign(&aws_lc_rs::signature::RSA_PKCS1_SHA256, &SystemRandom::new(), message.as_bytes(), &mut signature)
            .map_err(|_| "signing the service account assertion failed".to_string())?;
        Ok(format!("{message}.{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature)))
    }
}

/// A deployment's service account. Checked when the credential is saved.
pub fn service_account(d: &Deployment) -> Result<ServiceAccount, GatewayError> {
    let json = d
        .secrets
        .google_service_account
        .as_deref()
        .ok_or_else(|| GatewayError::internal("vertex credential has no service account key"))?;
    ServiceAccount::parse(json).map_err(|e| GatewayError::internal(format!("vertex credential: {e}")))
}

/// The project a Vertex deployment calls: the credential's setting, else the service account's.
pub fn project(d: &Deployment, sa: &ServiceAccount) -> Result<String, GatewayError> {
    d.connection
        .project_id
        .clone()
        .or_else(|| sa.project_id.clone())
        .ok_or_else(|| GatewayError::internal("vertex credential has no project_id and the key file names none"))
}

struct Token {
    value: String,
    refresh_at: Instant,
}

/// One key's cached token, behind a lock held while a new one is fetched.
type TokenSlot = Arc<tokio::sync::Mutex<Option<Token>>>;

/// Access tokens per service account key (client email, key ID). Each key has its own lock, so
/// one key's token fetch does not hold up requests that use another.
#[derive(Default)]
pub struct Google {
    tokens: Mutex<HashMap<(String, String), TokenSlot>>,
}

impl Google {
    pub async fn access_token(&self, state: &AppState, sa: &ServiceAccount) -> Result<String, GatewayError> {
        let slot = {
            let mut map = self.tokens.lock().expect("token map");
            map.entry((sa.client_email.clone(), sa.private_key_id.clone())).or_default().clone()
        };
        let mut slot = slot.lock().await;
        if let Some(t) = slot.as_ref()
            && t.refresh_at > Instant::now()
        {
            return Ok(t.value.clone());
        }
        let token = fetch(state, sa).await?;
        let value = token.value.clone();
        *slot = Some(token);
        Ok(value)
    }
}

async fn fetch(state: &AppState, sa: &ServiceAccount) -> Result<Token, GatewayError> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let assertion = sa.assertion(now).map_err(GatewayError::internal)?;
    let enc = |s: &str| utf8_percent_encode(s, NON_ALPHANUMERIC).to_string();
    let body = format!("grant_type={}&assertion={}", enc("urn:ietf:params:oauth:grant-type:jwt-bearer"), enc(&assertion));
    let resp = state
        .http
        .post(TOKEN_URL)
        .header("content-type", "application/x-www-form-urlencoded")
        .timeout(Duration::from_secs(30))
        .body(body)
        .send()
        .await
        .map_err(|e| GatewayError::from_reqwest(&e))?;
    let status = resp.status();
    let v: Value = resp.json().await.unwrap_or_default();
    let Some(token) = v["access_token"].as_str().filter(|_| status.is_success()) else {
        let why = v["error_description"].as_str().or(v["error"].as_str()).unwrap_or("no access token in the response");
        tracing::error!(status = status.as_u16(), client_email = %sa.client_email, error = %why, "Google token request failed");
        return Err(GatewayError::unavailable(format!("could not get a Google access token for {}: {why}", sa.client_email)));
    };
    let lifetime = Duration::from_secs(v["expires_in"].as_u64().unwrap_or(3600));
    Ok(Token { value: token.to_string(), refresh_at: Instant::now() + lifetime.saturating_sub(REFRESH_MARGIN) })
}

/// Vertex AI REST host for a location: the global endpoint, the `eu`/`us` multi-region
/// endpoints, or a regional one.
pub fn vertex_host(location: &str) -> String {
    match location {
        "global" => "https://aiplatform.googleapis.com".into(),
        "eu" | "us" => format!("https://aiplatform.{location}.rep.googleapis.com"),
        region => format!("https://{region}-aiplatform.googleapis.com"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway 2048-bit key, generated for this test only.
    fn test_key() -> String {
        use aws_lc_rs::encoding::{AsDer, Pkcs8V1Der};
        let key = aws_lc_rs::rsa::KeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).unwrap();
        let der = AsDer::<Pkcs8V1Der>::as_der(&key).unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(der.as_ref());
        let lines: Vec<&str> = b64.as_bytes().chunks(64).map(|c| std::str::from_utf8(c).unwrap()).collect();
        format!("-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n", lines.join("\n"))
    }

    #[test]
    fn parses_and_signs() {
        let file = json!({
            "type": "service_account", "project_id": "p1", "private_key_id": "k1",
            "private_key": test_key(), "client_email": "gw@p1.iam.gserviceaccount.com",
            "token_uri": "https://attacker.example/token",
        })
        .to_string();
        let sa = ServiceAccount::parse(&file).unwrap();
        assert_eq!(sa.project_id.as_deref(), Some("p1"));
        let jwt = sa.assertion(1_700_000_000).unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let claims: Value =
            serde_json::from_slice(&base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["aud"], TOKEN_URL);
        assert_eq!(claims["iss"], "gw@p1.iam.gserviceaccount.com");
        assert_eq!(claims["exp"], 1_700_003_600);
    }

    #[test]
    fn rejects_other_files() {
        assert!(ServiceAccount::parse("not json").is_err());
        assert!(ServiceAccount::parse(r#"{"type":"authorized_user","private_key":"x","client_email":"a"}"#).is_err());
        let bad_key = json!({ "type": "service_account", "private_key": "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----", "client_email": "a@b" });
        assert!(ServiceAccount::parse(&bad_key.to_string()).is_err());
    }

    #[test]
    fn vertex_hosts() {
        assert_eq!(vertex_host("global"), "https://aiplatform.googleapis.com");
        assert_eq!(vertex_host("eu"), "https://aiplatform.eu.rep.googleapis.com");
        assert_eq!(vertex_host("europe-west4"), "https://europe-west4-aiplatform.googleapis.com");
    }
}
