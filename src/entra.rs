//! Microsoft Entra ID authentication for Azure AI Foundry, and its deployment listing.
//!
//! An Azure credential can use an app registration's client credentials (tenant, client ID and
//! client secret) instead of an API key. For each request the gateway sends an access token for
//! Azure AI services, fetched with the client credentials grant and cached until shortly before it
//! expires. The app needs the Cognitive Services OpenAI User role (Azure OpenAI models) or
//! Cognitive Services User (all Foundry models) on the resource.
//!
//! The same credentials read the resource's deployments from Azure Resource Manager (Cognitive
//! Services User includes read access), so the console can fill in a deployment's name, model and
//! rate limits. Tokens are only ever sent to Microsoft's own hosts.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{
    error::GatewayError,
    model::{ApiStyle, CredentialSettings, Mode, Secrets},
    state::AppState,
};

const LOGIN_HOST: &str = "https://login.microsoftonline.com";
const ARM_HOST: &str = "https://management.azure.com";
/// Azure OpenAI and Foundry resources (`*.openai.azure.com`, `*.services.ai.azure.com`, ...).
const COGNITIVE_SCOPE: &str = "https://cognitiveservices.azure.com/.default";
/// Serverless endpoints (`*.models.ai.azure.com`).
const ML_SCOPE: &str = "https://ml.azure.com/.default";
const ARM_SCOPE: &str = "https://management.azure.com/.default";
const ACCOUNTS_API: &str = "2024-10-01";
const SUBSCRIPTIONS_API: &str = "2022-12-01";
/// Tokens live about an hour; a new one is fetched this long before the old one expires.
const REFRESH_MARGIN: Duration = Duration::from_secs(300);
/// List pages followed at most, so a misbehaving listing cannot keep a request going.
const MAX_PAGES: usize = 20;

/// An app registration's client credentials, from an Azure credential.
pub struct Client<'a> {
    tenant_id: &'a str,
    client_id: &'a str,
    secret: &'a str,
}

/// The credential's client credentials, when it uses Entra ID rather than an API key.
pub fn client<'a>(settings: &'a CredentialSettings, secrets: &'a Secrets) -> Option<Client<'a>> {
    Some(Client {
        tenant_id: settings.tenant_id.as_deref()?,
        client_id: settings.client_id.as_deref()?,
        secret: secrets.client_secret.as_deref()?,
    })
}

/// A directory (tenant) ID, or one of its domain names such as `contoso.onmicrosoft.com`.
pub fn valid_tenant(t: &str) -> bool {
    !t.is_empty() && t.len() <= 100 && t.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.'))
}

/// An application (client) ID: a GUID.
pub fn valid_client_id(c: &str) -> bool {
    uuid::Uuid::try_parse(c).is_ok()
}

/// The token scope for a resource endpoint.
pub fn scope_for(base_url: Option<&str>) -> &'static str {
    let serverless = base_url
        .and_then(|u| reqwest::Url::parse(u).ok())
        .is_some_and(|u| u.host_str().is_some_and(|h| h.ends_with(".models.ai.azure.com")));
    if serverless { ML_SCOPE } else { COGNITIVE_SCOPE }
}

struct Token {
    value: String,
    refresh_at: Instant,
}

/// One app's cached token for one scope, behind a lock held while a new one is fetched.
type TokenSlot = Arc<tokio::sync::Mutex<Option<Token>>>;
/// Tenant, client ID, SHA-256 of the client secret, scope.
type TokenKey = (String, String, [u8; 32], &'static str);

/// Access tokens per app registration, secret and scope. The secret is part of the key (as a
/// hash), so a credential with a wrong secret never gets another credential's token.
#[derive(Default)]
pub struct Entra {
    tokens: Mutex<HashMap<TokenKey, TokenSlot>>,
}

impl Entra {
    pub async fn access_token(&self, state: &AppState, c: &Client<'_>, scope: &'static str) -> Result<String, GatewayError> {
        let slot = {
            let secret: [u8; 32] = Sha256::digest(c.secret.as_bytes()).into();
            let mut map = self.tokens.lock().expect("token map");
            map.entry((c.tenant_id.to_ascii_lowercase(), c.client_id.to_ascii_lowercase(), secret, scope)).or_default().clone()
        };
        let mut slot = slot.lock().await;
        if let Some(t) = slot.as_ref()
            && t.refresh_at > Instant::now()
        {
            return Ok(t.value.clone());
        }
        let token = fetch(state, c, scope).await?;
        let value = token.value.clone();
        *slot = Some(token);
        Ok(value)
    }
}

async fn fetch(state: &AppState, c: &Client<'_>, scope: &str) -> Result<Token, GatewayError> {
    let enc = |s: &str| utf8_percent_encode(s, NON_ALPHANUMERIC).to_string();
    let url = format!("{LOGIN_HOST}/{}/oauth2/v2.0/token", enc(c.tenant_id));
    let body = format!(
        "grant_type=client_credentials&client_id={}&client_secret={}&scope={}",
        enc(c.client_id),
        enc(c.secret),
        enc(scope)
    );
    let resp = state
        .http
        .post(url)
        .header("content-type", "application/x-www-form-urlencoded")
        .timeout(Duration::from_secs(30))
        .body(body)
        .send()
        .await
        .map_err(|e| GatewayError::from_reqwest(&e))?;
    let status = resp.status();
    let v: Value = resp.json().await.unwrap_or_default();
    let Some(token) = v["access_token"].as_str().filter(|_| status.is_success()) else {
        // Entra's descriptions end with trace and correlation IDs on further lines.
        let why = v["error_description"].as_str().or(v["error"].as_str()).unwrap_or("no access token in the response");
        let why = why.lines().next().unwrap_or(why);
        tracing::error!(status = status.as_u16(), client_id = %c.client_id, error = %why, "Entra ID token request failed");
        return Err(GatewayError::unavailable(format!("could not get an Entra ID access token for app {}: {why}", c.client_id)));
    };
    let lifetime = Duration::from_secs(v["expires_in"].as_u64().unwrap_or(3600));
    Ok(Token { value: token.to_string(), refresh_at: Instant::now() + lifetime.saturating_sub(REFRESH_MARGIN) })
}

/// A deployment on a Foundry resource, as Azure Resource Manager describes it.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct FoundryDeployment {
    /// The deployment name: what the gateway sends as the upstream model.
    pub name: String,
    /// The model deployed, e.g. `gpt-4o`.
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_version: Option<String>,
    /// The model's format (its vendor), e.g. `OpenAI`, `Anthropic`, `Cohere`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// Deployment type, e.g. `GlobalStandard`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sku: Option<String>,
    /// For a data zone deployment (`DataZoneStandard`, ...): the zone it processes in, `eu` or
    /// `us`, from the resource's region. Data zone prices are keyed `azure/eu/…` and `azure/us/…`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_zone: Option<&'static str>,
    /// Requests per minute the deployment's quota allows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rpm: Option<i64>,
    /// Tokens per minute the deployment's quota allows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tpm: Option<i64>,
    /// What the model serves, as far as its capabilities tell.
    pub mode: Mode,
    /// The protocol to call a chat model with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_style: Option<ApiStyle>,
    /// Provisioning state, e.g. `Succeeded`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
}

/// The deployments of the resource an Azure credential's endpoint points at.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct FoundryDeployments {
    /// The resource's Azure Resource Manager ID.
    pub resource: String,
    /// The resource's region, e.g. `swedencentral`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    pub deployments: Vec<FoundryDeployment>,
}

/// Find the resource behind `base_url` among the subscriptions the app can see, and list its
/// deployments.
pub async fn list_deployments(state: &AppState, c: &Client<'_>, base_url: &str) -> Result<FoundryDeployments, String> {
    let token = state.entra.access_token(state, c, ARM_SCOPE).await.map_err(|e| e.message)?;
    let endpoint = reqwest::Url::parse(base_url).map_err(|_| "the resource endpoint is not a URL".to_string())?;
    let host = endpoint.host_str().unwrap_or_default().to_ascii_lowercase();
    if host.ends_with(".models.ai.azure.com") {
        return Err("serverless endpoints have no deployments to list; enter the model name yourself".into());
    }
    let subdomain = host.split('.').next().unwrap_or_default().to_string();

    let subscriptions = arm_list(state, &token, &format!("{ARM_HOST}/subscriptions?api-version={SUBSCRIPTIONS_API}")).await?;
    let mut resource = None;
    'subs: for sub in &subscriptions {
        let Some(id) = sub["subscriptionId"].as_str() else { continue };
        let url = format!(
            "{ARM_HOST}/subscriptions/{}/providers/Microsoft.CognitiveServices/accounts?api-version={ACCOUNTS_API}",
            utf8_percent_encode(id, NON_ALPHANUMERIC)
        );
        // A subscription the app may see but not list accounts in is skipped.
        let Ok(accounts) = arm_list(state, &token, &url).await else { continue };
        for a in accounts {
            if account_matches(&a, &host, &subdomain)
                && let Some(id) = a["id"].as_str()
            {
                resource = Some((id.to_string(), a["location"].as_str().map(str::to_ascii_lowercase)));
                break 'subs;
            }
        }
    }
    let Some((resource, location)) = resource else {
        return Err(format!(
            "no Azure AI resource with the endpoint {host} is visible to the app: give it the Cognitive Services User role on \
             the resource (or Reader on its resource group)"
        ));
    };
    // ARM IDs are paths (`/subscriptions/.../accounts/name`); anything else is not used in a URL.
    if !resource.starts_with("/subscriptions/") || resource.contains("://") || resource.contains('?') {
        return Err("Azure Resource Manager returned an unexpected resource ID".into());
    }
    let url = format!("{ARM_HOST}{resource}/deployments?api-version={ACCOUNTS_API}");
    let zone = location.as_deref().and_then(data_zone);
    let mut deployments: Vec<FoundryDeployment> =
        arm_list(state, &token, &url).await?.iter().filter_map(|v| deployment(v, zone)).collect();
    deployments.sort_by_key(|d| d.name.to_lowercase());
    Ok(FoundryDeployments { resource, location, deployments })
}

/// Whether a Cognitive Services account serves `host`: by its custom subdomain, which every
/// endpoint of the resource starts with, or by one of its endpoint URLs.
fn account_matches(a: &Value, host: &str, subdomain: &str) -> bool {
    let p = &a["properties"];
    if p["customSubDomainName"].as_str().is_some_and(|s| s.eq_ignore_ascii_case(subdomain)) {
        return true;
    }
    let host_of = |v: &Value| v.as_str().and_then(|u| reqwest::Url::parse(u).ok()).and_then(|u| u.host_str().map(str::to_ascii_lowercase));
    host_of(&p["endpoint"]).as_deref() == Some(host)
        || p["endpoints"].as_object().is_some_and(|m| m.values().any(|v| host_of(v).as_deref() == Some(host)))
}

/// The data zone a region belongs to: Azure's EU and US data zones.
fn data_zone(location: &str) -> Option<&'static str> {
    const EU: &[&str] = &[
        "francecentral",
        "germanywestcentral",
        "italynorth",
        "northeurope",
        "polandcentral",
        "spaincentral",
        "swedencentral",
        "westeurope",
    ];
    const US: &[&str] = &[
        "centralus",
        "eastus",
        "eastus2",
        "northcentralus",
        "southcentralus",
        "westcentralus",
        "westus",
        "westus2",
        "westus3",
    ];
    if EU.contains(&location) {
        Some("eu")
    } else if US.contains(&location) {
        Some("us")
    } else {
        None
    }
}

/// One ARM deployment as the console shows it. `zone` is the resource's data zone, which applies
/// to its data zone deployments.
fn deployment(v: &Value, zone: Option<&'static str>) -> Option<FoundryDeployment> {
    let name = v["name"].as_str()?.to_string();
    let p = &v["properties"];
    let model = p["model"]["name"].as_str().unwrap_or(&name).to_string();
    let format = p["model"]["format"].as_str().map(str::to_string);
    let capable = |c: &str| p["capabilities"][c].as_str() == Some("true");
    let lower = model.to_ascii_lowercase();
    let mode = if capable("embeddings") || lower.contains("embed") {
        Mode::Embedding
    } else if lower.contains("rerank") {
        Mode::Rerank
    } else if lower.contains("decision") {
        Mode::Decision
    } else {
        Mode::Chat
    };
    let api_style = (mode == Mode::Chat).then(|| {
        if format.as_deref().is_some_and(|f| f.eq_ignore_ascii_case("anthropic")) {
            ApiStyle::Anthropic
        } else if !capable("chatCompletion") && capable("responses") {
            ApiStyle::Responses
        } else {
            ApiStyle::Chat
        }
    });
    // Rate limits come per renewal period (requests per 10 s, tokens per minute).
    let per_minute = |key: &str| {
        p["rateLimits"].as_array()?.iter().find(|l| l["key"] == key).and_then(|l| {
            let count = l["count"].as_f64()?;
            let period = l["renewalPeriod"].as_f64().filter(|p| *p > 0.0).unwrap_or(60.0);
            Some((count * 60.0 / period).round() as i64)
        })
    };
    Some(FoundryDeployment {
        name,
        model,
        model_version: p["model"]["version"].as_str().map(str::to_string),
        format,
        sku: v["sku"]["name"].as_str().map(str::to_string),
        data_zone: zone.filter(|_| v["sku"]["name"].as_str().is_some_and(|s| s.starts_with("DataZone"))),
        rpm: per_minute("request"),
        tpm: per_minute("token"),
        mode,
        api_style,
        state: p["provisioningState"].as_str().map(str::to_string),
    })
}

/// GET an ARM list and follow its `nextLink`s, which must stay on Resource Manager.
async fn arm_list(state: &AppState, token: &str, first: &str) -> Result<Vec<Value>, String> {
    let mut items = Vec::new();
    let mut next = Some(first.to_string());
    for _ in 0..MAX_PAGES {
        let Some(url) = next.take() else { break };
        if !url.starts_with(&format!("{ARM_HOST}/")) {
            return Err("Azure Resource Manager returned a next page on another host".into());
        }
        let resp = state
            .http
            .get(&url)
            .bearer_auth(token)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| format!("Azure Resource Manager is not reachable: {e}"))?;
        let status = resp.status();
        let v: Value = resp.json().await.unwrap_or_default();
        if !status.is_success() {
            let why = v["error"]["message"].as_str().unwrap_or("no details");
            return Err(format!("Azure Resource Manager answered {}: {why}", status.as_u16()));
        }
        items.extend(v["value"].as_array().cloned().unwrap_or_default());
        next = v["nextLink"].as_str().map(str::to_string);
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn scopes() {
        assert_eq!(scope_for(Some("https://my-resource.services.ai.azure.com")), COGNITIVE_SCOPE);
        assert_eq!(scope_for(Some("https://my-resource.openai.azure.com/openai/v1")), COGNITIVE_SCOPE);
        assert_eq!(scope_for(Some("https://cohere-rerank.eastus.models.ai.azure.com")), ML_SCOPE);
    }

    #[test]
    fn ids() {
        assert!(valid_tenant("72f988bf-86f1-41af-91ab-2d7cd011db47"));
        assert!(valid_tenant("contoso.onmicrosoft.com"));
        assert!(!valid_tenant("evil.example/../x"));
        assert!(!valid_tenant(""));
        assert!(valid_client_id("00000000-0000-0000-0000-000000000001"));
        assert!(!valid_client_id("my-app"));
    }

    #[test]
    fn matches_accounts() {
        let a = json!({ "properties": {
            "customSubDomainName": "My-Resource",
            "endpoints": { "OpenAI Language Model Instance API": "https://my-resource.openai.azure.com/" }
        }});
        assert!(account_matches(&a, "my-resource.services.ai.azure.com", "my-resource"));
        assert!(account_matches(&a, "my-resource.openai.azure.com", "my-resource"));
        assert!(!account_matches(&a, "other.openai.azure.com", "other"));
        let by_endpoint = json!({ "properties": { "endpoint": "https://westeurope.api.cognitive.microsoft.com/" } });
        assert!(account_matches(&by_endpoint, "westeurope.api.cognitive.microsoft.com", "westeurope"));
    }

    #[test]
    fn reads_deployments() {
        let d = deployment(&json!({
            "name": "prod-4o",
            "sku": { "name": "GlobalStandard", "capacity": 450 },
            "properties": {
                "model": { "format": "OpenAI", "name": "gpt-4o", "version": "2024-11-20" },
                "capabilities": { "chatCompletion": "true" },
                "rateLimits": [
                    { "key": "request", "renewalPeriod": 10, "count": 450 },
                    { "key": "token", "renewalPeriod": 60, "count": 450000 }
                ],
                "provisioningState": "Succeeded"
            }
        }), Some("eu"))
        .unwrap();
        assert_eq!((d.name.as_str(), d.model.as_str()), ("prod-4o", "gpt-4o"));
        assert_eq!(d.data_zone, None, "a global deployment");
        assert_eq!((d.rpm, d.tpm), (Some(2700), Some(450000)));
        assert_eq!((d.mode, d.api_style), (Mode::Chat, Some(ApiStyle::Chat)));

        let claude = deployment(&json!({ "name": "claude", "properties": { "model": { "format": "Anthropic", "name": "claude-opus-4-5" } } }), None).unwrap();
        assert_eq!(claude.api_style, Some(ApiStyle::Anthropic));
        assert_eq!((claude.rpm, claude.tpm), (None, None));

        let embed = deployment(&json!({ "name": "e", "properties": { "model": { "name": "text-embedding-3-large" }, "capabilities": { "embeddings": "true" } } }), None).unwrap();
        assert_eq!((embed.mode, embed.api_style), (Mode::Embedding, None));
        let rerank = deployment(&json!({ "name": "r", "properties": { "model": { "format": "Cohere", "name": "Cohere-rerank-v4.0-pro" } } }), None).unwrap();
        assert_eq!(rerank.mode, Mode::Rerank);

        let zoned = json!({ "name": "z", "sku": { "name": "DataZoneStandard" }, "properties": { "model": { "name": "gpt-4o" } } });
        assert_eq!(deployment(&zoned, Some("eu")).unwrap().data_zone, Some("eu"));
        assert_eq!(deployment(&zoned, None).unwrap().data_zone, None);
    }

    #[test]
    fn data_zones() {
        assert_eq!(data_zone("swedencentral"), Some("eu"));
        assert_eq!(data_zone("westeurope"), Some("eu"));
        assert_eq!(data_zone("eastus2"), Some("us"));
        assert_eq!(data_zone("japaneast"), None);
        assert_eq!(data_zone("switzerlandnorth"), None);
    }
}
