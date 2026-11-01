//! Google Cloud authentication for Vertex AI and Model Armor, with Google's official
//! `google-cloud-auth` crate.
//!
//! A credential either stores a service account key file as uploaded, or uses the gateway's own
//! identity from its environment: Application Default Credentials (`GOOGLE_APPLICATION_CREDENTIALS`,
//! the gcloud ADC file, workload identity federation, or the metadata server on GKE, Cloud Run and
//! Compute Engine). With a key file the crate signs its own JWT access tokens and calls no token
//! endpoint, so the file's `token_uri` is never used. Both kinds cache their tokens.

use std::{collections::HashMap, sync::Mutex};

use aws_lc_rs::signature::RsaKeyPair;
use base64::Engine;
use google_cloud_auth::credentials::{AccessTokenCredentials, Builder, service_account};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{
    error::GatewayError,
    model::{CredentialSettings, Identity, Secrets},
};

const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
/// A token request that takes longer (an unreachable metadata server, retried) fails, so requests
/// can go to another deployment instead of waiting.
const TOKEN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// The fields of a service account key file the gateway checks and shows.
#[derive(Deserialize)]
pub struct ServiceAccount {
    #[serde(rename = "type")]
    kind: String,
    pub project_id: Option<String>,
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
        let der =
            base64::engine::general_purpose::STANDARD.decode(b64).map_err(|_| "the key file's private_key is not a PEM key".to_string())?;
        RsaKeyPair::from_pkcs8(&der).map_err(|_| "the key file's private_key is not an RSA PKCS#8 key".to_string())
    }
}

/// Whether a credential uses the gateway's Application Default Credentials.
pub fn uses_environment(settings: &CredentialSettings) -> bool {
    settings.identity == Some(Identity::Environment)
}

/// The project a credential calls: its own setting, else its key file's. A credential with
/// Application Default Credentials names it (checked when saved).
pub fn project(settings: &CredentialSettings, secrets: &Secrets) -> Result<String, GatewayError> {
    if let Some(p) = &settings.project_id {
        return Ok(p.clone());
    }
    secrets
        .google_service_account
        .as_deref()
        .and_then(|j| ServiceAccount::parse(j).ok())
        .and_then(|sa| sa.project_id)
        .ok_or_else(|| GatewayError::internal("google credential has no project_id and its key file names none"))
}

/// Built credentials per key file (by hash) or for the environment, kept for their token caches.
#[derive(Default)]
pub struct Google {
    credentials: Mutex<HashMap<Option<[u8; 32]>, AccessTokenCredentials>>,
}

impl Google {
    /// An access token for a credential: its key file's, or the environment's.
    pub async fn access_token(&self, settings: &CredentialSettings, secrets: &Secrets) -> Result<String, GatewayError> {
        let key = self.key(settings, secrets)?;
        let credentials = self.credentials(key, secrets)?;
        let why = match tokio::time::timeout(TOKEN_TIMEOUT, credentials.access_token()).await {
            Ok(Ok(t)) => return Ok(t.token),
            Ok(Err(e)) => crate::admin_auth::error_chain(&e),
            Err(_) => format!("no answer within {} s", TOKEN_TIMEOUT.as_secs()),
        };
        // A failed credential stays failed ("subsequent calls will also fail"): the next request
        // builds a new one, so fixing the environment (a new login, a granted role) takes effect.
        self.credentials.lock().expect("google credentials").remove(&key);
        tracing::error!(error = %why, environment = uses_environment(settings), "Google access token request failed");
        Err(GatewayError::unavailable(format!("could not get a Google access token: {why}")))
    }

    fn key(&self, settings: &CredentialSettings, secrets: &Secrets) -> Result<Option<[u8; 32]>, GatewayError> {
        Ok(if uses_environment(settings) {
            None
        } else {
            let json = secrets
                .google_service_account
                .as_deref()
                .ok_or_else(|| GatewayError::internal("google credential has no service account key"))?;
            Some(Sha256::digest(json.as_bytes()).into())
        })
    }

    fn credentials(&self, key: Option<[u8; 32]>, secrets: &Secrets) -> Result<AccessTokenCredentials, GatewayError> {
        if let Some(c) = self.credentials.lock().expect("google credentials").get(&key) {
            return Ok(c.clone());
        }
        let built = match key {
            None => Builder::default().with_scopes([SCOPE]).build_access_token_credentials().map_err(|e| {
                GatewayError::unavailable(format!(
                    "no Google Application Default Credentials in the gateway's environment: {}",
                    first_line(&e.to_string())
                ))
            })?,
            Some(_) => {
                let json = secrets.google_service_account.as_deref().unwrap_or_default();
                ServiceAccount::parse(json).map_err(|e| GatewayError::internal(format!("google credential: {e}")))?;
                let value: Value = serde_json::from_str(json).map_err(|e| GatewayError::internal(format!("google credential: {e}")))?;
                service_account::Builder::new(value)
                    .build_access_token_credentials()
                    .map_err(|e| GatewayError::internal(format!("google credential: {}", first_line(&e.to_string()))))?
            }
        };
        self.credentials.lock().expect("google credentials").insert(key, built.clone());
        Ok(built)
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s)
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
    use serde_json::json;

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

    fn key_file() -> String {
        json!({
            "type": "service_account", "project_id": "p1", "private_key_id": "k1",
            "private_key": test_key(), "client_email": "gw@p1.iam.gserviceaccount.com",
            "token_uri": "https://attacker.example/token",
        })
        .to_string()
    }

    #[test]
    fn parses_key_files() {
        let sa = ServiceAccount::parse(&key_file()).unwrap();
        assert_eq!(sa.project_id.as_deref(), Some("p1"));
        assert_eq!(sa.client_email, "gw@p1.iam.gserviceaccount.com");
    }

    #[tokio::test]
    async fn key_files_sign_their_own_tokens() {
        // No token endpoint is called (the file's points elsewhere): the token is a signed JWT.
        let secrets = Secrets { google_service_account: Some(key_file()), ..Default::default() };
        let token = Google::default().access_token(&CredentialSettings::default(), &secrets).await.unwrap();
        let claims: Value =
            serde_json::from_slice(&base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(token.split('.').nth(1).unwrap()).unwrap())
                .unwrap();
        assert_eq!(claims["iss"], "gw@p1.iam.gserviceaccount.com");
        assert_eq!(claims["scope"], SCOPE);
    }

    #[test]
    fn projects() {
        let secrets = Secrets { google_service_account: Some(key_file()), ..Default::default() };
        assert_eq!(project(&CredentialSettings::default(), &secrets).unwrap(), "p1");
        let own = CredentialSettings { project_id: Some("p2".into()), ..Default::default() };
        assert_eq!(project(&own, &secrets).unwrap(), "p2");
        assert!(project(&CredentialSettings::default(), &Secrets::default()).is_err());
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
