//! Persistent entities (deployments, virtual keys) and their runtime forms.

use std::{collections::BTreeMap, time::Duration};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Openai,
    Azure,
    Bedrock,
    Anthropic,
    /// TypeSafe decision models (Jev).
    Typesafe,
    /// Mistral AI (La Plateforme), global or pinned to its EU or US endpoint.
    Mistral,
    /// Google Vertex AI: Gemini through its OpenAI-compatible endpoint, Claude through
    /// `rawPredict`, embeddings through `predict`. Authenticated with a service account.
    Vertex,
    /// Google AI Studio (the Gemini API) through its OpenAI-compatible endpoint.
    Gemini,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Openai => "openai",
            Self::Azure => "azure",
            Self::Bedrock => "bedrock",
            Self::Anthropic => "anthropic",
            Self::Typesafe => "typesafe",
            Self::Mistral => "mistral",
            Self::Vertex => "vertex",
            Self::Gemini => "gemini",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "openai" => Some(Self::Openai),
            "azure" => Some(Self::Azure),
            "bedrock" => Some(Self::Bedrock),
            "anthropic" => Some(Self::Anthropic),
            "typesafe" => Some(Self::Typesafe),
            "mistral" => Some(Self::Mistral),
            "vertex" => Some(Self::Vertex),
            "gemini" => Some(Self::Gemini),
            _ => None,
        }
    }

    pub fn default_style(self) -> ApiStyle {
        match self {
            Self::Openai | Self::Azure | Self::Mistral | Self::Vertex | Self::Gemini => ApiStyle::Chat,
            Self::Bedrock => ApiStyle::Converse,
            Self::Anthropic => ApiStyle::Anthropic,
            Self::Typesafe => ApiStyle::Systemone,
        }
    }

    pub fn supports(self, style: ApiStyle) -> bool {
        use ApiStyle::*;
        matches!(
            (self, style),
            (Self::Openai, Chat | Responses | Embeddings | Rerank)
                | (Self::Azure, Chat | Responses | Anthropic | Systemone | Embeddings | Rerank)
                | (Self::Anthropic, Anthropic)
                | (Self::Bedrock, Converse | Anthropic | Embeddings | Rerank)
                | (Self::Typesafe, Systemone)
                | (Self::Mistral | Self::Gemini, Chat | Embeddings)
                | (Self::Vertex, Chat | Anthropic | Embeddings)
        )
    }
}

/// What a deployment serves, and so which endpoints route to it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Messages, Responses and Chat Completions.
    #[default]
    Chat,
    /// `/v1/embeddings`.
    Embedding,
    /// `/v1/decisions` and `/v1/systemone`.
    Decision,
    /// `/v1/rerank`.
    Rerank,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Embedding => "embedding",
            Self::Decision => "decision",
            Self::Rerank => "rerank",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "chat" => Some(Self::Chat),
            "embedding" => Some(Self::Embedding),
            "decision" => Some(Self::Decision),
            "rerank" => Some(Self::Rerank),
            _ => None,
        }
    }

    /// The endpoints that serve this mode, for error messages.
    pub fn endpoints(self) -> &'static str {
        match self {
            Self::Chat => "/v1/messages, /v1/responses or /v1/chat/completions",
            Self::Embedding => "/v1/embeddings",
            Self::Decision => "/v1/decisions or /v1/systemone",
            Self::Rerank => "/v1/rerank",
        }
    }

    /// The upstream protocol of a deployment. Only chat deployments choose one (`api_style`);
    /// embedding, decision and rerank deployments each have exactly one.
    pub fn style(self, provider: ProviderKind, configured: Option<ApiStyle>) -> Result<ApiStyle, String> {
        let style = match self {
            Self::Chat => {
                let s = configured.unwrap_or_else(|| provider.default_style());
                if matches!(s, ApiStyle::Embeddings | ApiStyle::Systemone | ApiStyle::Rerank) {
                    return Err(match configured {
                        Some(_) => format!("api_style {s:?} is not a chat protocol"),
                        None => format!("provider {} does not serve chat models", provider.as_str()),
                    });
                }
                s
            }
            Self::Embedding => ApiStyle::Embeddings,
            Self::Decision => ApiStyle::Systemone,
            Self::Rerank => ApiStyle::Rerank,
        };
        if !provider.supports(style) {
            return Err(format!("provider {} does not serve {} models", provider.as_str(), self.as_str()));
        }
        Ok(style)
    }
}

/// Wire protocol spoken with the upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ApiStyle {
    /// OpenAI Chat Completions.
    Chat,
    /// OpenAI Responses.
    Responses,
    /// Anthropic Messages (native, Azure Foundry, or Bedrock InvokeModel).
    Anthropic,
    /// Bedrock Converse.
    Converse,
    /// TypeSafe System One, for decision models (TypeSafe Jev, Microsoft Foundry's
    /// Microsoft-Decision-1).
    Systemone,
    /// OpenAI embeddings, for embedding models.
    Embeddings,
    /// Rerank, for rerank models: Cohere's API (Azure AI Foundry, OpenAI-compatible servers) or
    /// Bedrock's Rerank API.
    Rerank,
}

/// Where and how to reach a provider: stored with the credential, because an Azure key only works
/// for its own resource and AWS keys are used in a region. Not secret.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CredentialSettings {
    /// openai: API base incl. `/v1` (default OpenAI); azure: resource endpoint (required);
    /// anthropic: API base incl. `/v1` (default Anthropic); bedrock: optional endpoint override
    /// such as a VPC endpoint; typesafe: API base (default `https://api.typesafe.ai`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Azure: optional `api-version` query parameter (the v1 API needs none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_version: Option<String>,
    /// Bedrock region (falls back to the AWS SDK's region: env, profile, instance metadata).
    /// Mistral: `global` (default), `eu` or `us`. Vertex: `global` (default), the `eu` or
    /// `us` multi-region, or a single region such as `europe-west4`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// Vertex: the Google Cloud project to call. Defaults to the service account's own project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// OpenAI-compatible: the header that carries `api_key`, as is, for servers that do not take
    /// `Authorization: Bearer` (for example `X-Api-Key`). The header name is not secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_header: Option<String>,
    /// Azure and Vertex AI: authenticate as the gateway's own identity from its environment
    /// instead of a stored secret (see `Identity`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<Identity>,
    /// Azure with Entra ID: the app registration's directory (tenant) ID or domain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
    /// Azure with Entra ID: the app registration's application (client) ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
}

/// Per-model settings, stored as JSONB on the deployment.
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DeploymentConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_style: Option<ApiStyle>,
    /// Extra static request headers sent upstream.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Chat Completions: `max_completion_tokens` (default) or `max_tokens`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens_field: Option<String>,
    /// Used when a client omits a max token value but the upstream requires one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_max_tokens: Option<u32>,
    /// Anthropic-style upstreams, translated requests: `adaptive` (default; Claude 4.6+ with
    /// `output_config.effort`) or `budget` (older models with `budget_tokens`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_mode: Option<String>,
    /// Anthropic-style upstreams, translated requests: forward temperature/top_p. Off by
    /// default because current Claude models reject sampling parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forward_sampling: Option<bool>,
    /// Bedrock with bedrock-mantle: SigV4 service name override (default `bedrock`). Other Bedrock
    /// calls go through the SDK, which signs for its own service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aws_service: Option<String>,
    /// Bedrock with the Anthropic Messages protocol: how Claude is called (see `BedrockApi`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bedrock_api: Option<BedrockApi>,
}

/// Where a credential without stored secrets gets its identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Identity {
    /// The gateway's environment, through the provider's official SDK. Azure: AKS workload
    /// identity (`AZURE_FEDERATED_TOKEN_FILE`), a client secret in `AZURE_TENANT_ID`,
    /// `AZURE_CLIENT_ID` and `AZURE_CLIENT_SECRET`, or else the managed identity (App Service,
    /// Container Apps, VMs). Google: Application Default Credentials.
    Environment,
}

/// How a Bedrock deployment with the Anthropic Messages protocol calls Claude.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum BedrockApi {
    /// The Messages body through InvokeModel (Bedrock Runtime, with the SDK): any region and
    /// inference profile that serves the model.
    Invoke,
    /// bedrock-mantle's Messages API. The default, as before this option existed.
    Mantle,
}

/// Headers a deployment's `headers` may not set: they would change where or how the request is
/// framed, or interfere with request signing.
pub fn reserved_header(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    matches!(
        n.as_str(),
        "host" | "content-length" | "transfer-encoding" | "connection" | "keep-alive" | "upgrade" | "te" | "trailer" | "expect"
    ) || n.starts_with("proxy-")
        || n.starts_with("x-amz-")
}

/// Headers that carry credentials. A deployment's `headers` are stored and shown in plain text
/// and exported, so these belong in a credential, whose secrets are encrypted and never shown:
/// its `api_key`, sent in the credential's `auth_header` for servers that want another header.
pub fn credential_header(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == "cookie"
        || n.ends_with("-key")
        || n.ends_with("_key")
        || ["auth", "token", "secret", "password", "apikey", "api-key", "signature", "session"].iter().any(|w| n.contains(w))
}

/// Shown in place of a credential header's value in the admin API and exports.
pub const REDACTED: &str = "(redacted: move this header to the credential)";

impl DeploymentConfig {
    /// Checked when a deployment is saved (admin API and import).
    pub fn check(&self) -> Result<(), String> {
        self.check_stored()?;
        if let Some(h) = self.headers.keys().find(|h| credential_header(h)) {
            return Err(format!(
                "header '{h}' carries a credential, which deployments store in plain text; put the secret in the \
                 credential's api_key instead (OpenAI-compatible credentials can send it in a header of your choice)"
            ));
        }
        Ok(())
    }

    /// The config as the admin API shows and exports it: credential headers that deployments
    /// saved before they were refused keep working, but their values are not shown.
    pub fn redacted(&self) -> Self {
        let mut c = self.clone();
        for (name, value) in c.headers.iter_mut() {
            if credential_header(name) {
                *value = REDACTED.into();
            }
        }
        c
    }

    /// Credential headers in a stored config, which `check` refuses on save.
    pub fn credential_headers(&self) -> impl Iterator<Item = &str> {
        self.headers.keys().map(String::as_str).filter(|h| credential_header(h))
    }

    /// Checked when a deployment is loaded. Credential headers saved before `check` refused them
    /// still work, so upgrading does not take deployments offline; the log says to move them.
    pub fn check_stored(&self) -> Result<(), String> {
        // SigV4 credentials sign for one service. Only Bedrock signing names are accepted, so a
        // deployment cannot be used to sign requests for other AWS APIs with the gateway's role.
        if let Some(s) = &self.aws_service {
            let ok = s.strip_prefix("bedrock").is_some_and(|rest| {
                rest.is_empty()
                    || rest.strip_prefix('-').is_some_and(|r| {
                        !r.is_empty()
                            && r.split('-').all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()))
                    })
            });
            if !ok {
                return Err(format!("aws_service '{s}' is not a Bedrock signing name (bedrock or bedrock-<name>)"));
            }
        }
        if let Some(h) = self.headers.keys().find(|h| reserved_header(h)) {
            return Err(format!("header '{h}' cannot be set on a deployment"));
        }
        Ok(())
    }
}

/// Secret values of a credential. Entered through the admin API/UI and encrypted at rest.
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Secrets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aws_access_key_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aws_secret_access_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aws_session_token: Option<String>,
    /// Vertex: the service account key file (JSON), as downloaded from Google Cloud.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub google_service_account: Option<String>,
    /// Azure with Entra ID: the app registration's client secret, used instead of an API key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
}

impl Secrets {
    /// Overlay `patch` on top of `self`: provided fields replace, empty strings clear.
    pub fn merge(&mut self, patch: Secrets) {
        fn apply(slot: &mut Option<String>, v: Option<String>) {
            if let Some(v) = v {
                *slot = if v.is_empty() { None } else { Some(v) };
            }
        }
        apply(&mut self.api_key, patch.api_key);
        apply(&mut self.aws_access_key_id, patch.aws_access_key_id);
        apply(&mut self.aws_secret_access_key, patch.aws_secret_access_key);
        apply(&mut self.aws_session_token, patch.aws_session_token);
        apply(&mut self.google_service_account, patch.google_service_account);
        apply(&mut self.client_secret, patch.client_secret);
    }

    /// All values that are set.
    pub fn values(&self) -> impl Iterator<Item = &str> {
        [
            &self.api_key,
            &self.aws_access_key_id,
            &self.aws_secret_access_key,
            &self.aws_session_token,
            &self.google_service_account,
            &self.client_secret,
        ]
        .into_iter()
        .filter_map(|v| v.as_deref())
    }

    pub fn presence(&self) -> BTreeMap<&'static str, bool> {
        BTreeMap::from([
            ("api_key", self.api_key.is_some()),
            ("aws_access_key_id", self.aws_access_key_id.is_some()),
            ("aws_secret_access_key", self.aws_secret_access_key.is_some()),
            ("aws_session_token", self.aws_session_token.is_some()),
            ("google_service_account", self.google_service_account.is_some()),
            ("client_secret", self.client_secret.is_some()),
        ])
    }
}

#[derive(Debug, Clone)]
pub struct DeploymentRow {
    pub id: Uuid,
    pub model_name: String,
    pub mode: String,
    pub provider: String,
    pub upstream_model: String,
    pub config: sqlx::types::Json<DeploymentConfig>,
    pub credential_id: Uuid,
    pub weight: i32,
    pub priority: i32,
    pub rpm_limit: Option<i32>,
    pub tpm_limit: Option<i32>,
    pub timeout_secs: i32,
    pub input_cost_per_mtok: f64,
    pub output_cost_per_mtok: f64,
    pub cache_read_cost_per_mtok: Option<f64>,
    pub cache_write_cost_per_mtok: Option<f64>,
    pub long_context_threshold: Option<i32>,
    pub long_input_cost_per_mtok: Option<f64>,
    pub long_output_cost_per_mtok: Option<f64>,
    pub long_cache_read_cost_per_mtok: Option<f64>,
    pub long_cache_write_cost_per_mtok: Option<f64>,
    /// USD per 1,000 searches: rerank queries, or the web searches a chat model runs.
    pub cost_per_k_searches: Option<f64>,
    /// Price catalog key this deployment corresponds to (e.g. `azure/gpt-4o`).
    pub base_model: Option<String>,
    /// Permission tags (e.g. `azure`, `cheap`); see `VirtualKey::holds_all`.
    pub tags: Vec<String>,
    /// Projects whose keys alone may use the deployment; empty for any key. See
    /// `VirtualKey::allows_deployment`.
    pub project_ids: Vec<Uuid>,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A named, reusable set of provider secrets. Deployments reference one by id.
#[derive(Debug, Clone)]
pub struct CredentialRow {
    pub id: Uuid,
    pub name: String,
    pub provider: String,
    pub secrets: Option<Vec<u8>>,
    pub settings: sqlx::types::Json<CredentialSettings>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Rates {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

impl Rates {
    /// Cache prices default to the tier's input price.
    pub fn new(input: f64, output: f64, cache_read: Option<f64>, cache_write: Option<f64>) -> Self {
        Self { input, output, cache_read: cache_read.unwrap_or(input), cache_write: cache_write.unwrap_or(input) }
    }
}

/// USD per 1M tokens, with an optional long-context tier, and per 1,000 rerank searches.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Pricing {
    pub standard: Rates,
    /// `(threshold, rates)`: requests whose input tokens (including cached ones) exceed the
    /// threshold are billed entirely at these rates.
    pub long_context: Option<(u64, Rates)>,
    pub per_k_searches: f64,
}

impl Pricing {
    pub fn rates_for(&self, u: &crate::ir::Usage) -> Rates {
        match self.long_context {
            Some((threshold, long)) if u.total_input() > threshold => long,
            _ => self.standard,
        }
    }

    pub fn cost(&self, u: &crate::ir::Usage) -> f64 {
        let r = self.rates_for(u);
        (u.input_tokens as f64 * r.input
            + u.output_tokens as f64 * r.output
            + u.cache_read_tokens as f64 * r.cache_read
            + u.cache_write_tokens as f64 * r.cache_write)
            / 1_000_000.0
            + self.search_cost(u)
    }

    /// The searches' part of `cost`.
    pub fn search_cost(&self, u: &crate::ir::Usage) -> f64 {
        u.search_units as f64 * self.per_k_searches / 1_000.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Usage;

    #[test]
    fn routes_read_both_shapes() {
        let pool: Route = serde_json::from_value(serde_json::json!({
            "name": "Complex", "description": "hard ones",
            "models": [{ "model": "luna" }, { "model": "sonnet", "reasoning_effort": "high" }]
        }))
        .unwrap();
        assert_eq!(pool.models[1].reasoning_effort, Some(ReasoningEffort::High));
        // As stored and exported before routes had several models.
        let single: Route = serde_json::from_value(serde_json::json!({ "model": "luna", "description": "d" })).unwrap();
        assert_eq!((single.name.as_str(), single.model_names().collect::<Vec<_>>()), ("luna", vec!["luna"]));
        // Written back in the new shape, without an unset effort.
        assert_eq!(
            serde_json::to_value(&single).unwrap(),
            serde_json::json!({ "name": "luna", "description": "d", "models": [{ "model": "luna" }] })
        );
        assert!(serde_json::from_value::<Route>(serde_json::json!({ "name": "x", "description": "d", "models": [], "extra": 1 })).is_err());
    }

    #[test]
    fn modes_pick_their_protocol() {
        assert_eq!(Mode::Decision.style(ProviderKind::Typesafe, None), Ok(ApiStyle::Systemone));
        assert_eq!(Mode::Decision.style(ProviderKind::Azure, None), Ok(ApiStyle::Systemone));
        assert_eq!(Mode::Embedding.style(ProviderKind::Openai, None), Ok(ApiStyle::Embeddings));
        assert_eq!(Mode::Chat.style(ProviderKind::Azure, None), Ok(ApiStyle::Chat));
        assert!(Mode::Chat.style(ProviderKind::Typesafe, None).is_err());
        assert_eq!(Mode::Embedding.style(ProviderKind::Bedrock, None), Ok(ApiStyle::Embeddings));
        assert!(Mode::Embedding.style(ProviderKind::Anthropic, None).is_err());
        assert!(Mode::Decision.style(ProviderKind::Openai, None).is_err());
        assert!(Mode::Chat.style(ProviderKind::Openai, Some(ApiStyle::Embeddings)).is_err());
        assert_eq!(Mode::Chat.style(ProviderKind::Vertex, Some(ApiStyle::Anthropic)), Ok(ApiStyle::Anthropic));
        assert_eq!(Mode::Embedding.style(ProviderKind::Mistral, None), Ok(ApiStyle::Embeddings));
        assert!(Mode::Chat.style(ProviderKind::Gemini, Some(ApiStyle::Responses)).is_err());
        assert!(Mode::Decision.style(ProviderKind::Vertex, None).is_err());
    }

    #[test]
    fn deployment_config_checks() {
        let with = |svc: Option<&str>, header: Option<&str>| DeploymentConfig {
            aws_service: svc.map(str::to_string),
            headers: header.map(|h| BTreeMap::from([(h.to_string(), "v".to_string())])).unwrap_or_default(),
            ..Default::default()
        };
        for ok in [None, Some("bedrock"), Some("bedrock-mantle"), Some("bedrock-agent-runtime")] {
            assert!(with(ok, None).check().is_ok(), "{ok:?}");
        }
        for bad in ["sts", "s3", "iam", "bedrock-", "bedrockx", "bedrock-Mantle", "bedrock--x"] {
            assert!(with(Some(bad), None).check().is_err(), "{bad}");
        }
        for ok in ["x-custom", "OpenAI-Organization", "x-ms-useragent", "anthropic-beta", "x-goog-user-project"] {
            assert!(with(None, Some(ok)).check().is_ok(), "{ok}");
        }
        for bad in ["Host", "content-length", "Transfer-Encoding", "x-amz-security-token", "proxy-authorization"] {
            assert!(with(None, Some(bad)).check().is_err(), "{bad}");
            assert!(with(None, Some(bad)).check_stored().is_err(), "{bad}");
        }
        // Credentials are refused on save, but configs saved before keep loading.
        for secret in ["Authorization", "x-api-key", "api-key", "Ocp-Apim-Subscription-Key", "X-Auth-Token", "Cookie", "x-client-secret"] {
            let c = with(None, Some(secret));
            assert!(c.check().is_err(), "{secret}");
            assert!(c.check_stored().is_ok(), "{secret}");
            assert_eq!(c.credential_headers().collect::<Vec<_>>(), [secret]);
            assert_eq!(c.redacted().headers[secret], REDACTED);
        }
        assert_eq!(with(None, Some("x-custom")).redacted().headers["x-custom"], "v");
    }

    #[test]
    fn tags_normalize() {
        let t = normalize_tags(vec![" Azure ".into(), "cheap".into(), "azure".into(), "".into()]).unwrap();
        assert_eq!(t, ["azure", "cheap"]);
        assert!(normalize_tags(vec!["two words".into()]).is_err());
        assert!(normalize_tags((0..21).map(|i| format!("t{i}")).collect()).is_err());
    }

    #[test]
    fn keys_need_every_tag_of_a_deployment() {
        let d = |tags: &[&str]| Deployment {
            id: Uuid::nil(),
            model_name: "m".into(),
            mode: Mode::Chat,
            provider: ProviderKind::Openai,
            style: ApiStyle::Chat,
            upstream_model: "m".into(),
            config: DeploymentConfig::default(),
            connection: CredentialSettings::default(),
            secrets: Secrets::default(),
            weight: 1,
            priority: 0,
            rpm_limit: None,
            tpm_limit: None,
            timeout: Duration::from_secs(1),
            pricing: Pricing::default(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            project_ids: vec![],
            catalog: None,
        };
        let key = |tags: &[&str]| VirtualKey {
            id: Uuid::nil(),
            name: "k".into(),
            key_hash: vec![],
            key_prefix: "gw-".into(),
            scope: KeyScope::Inference,
            project_id: None,
            project_active: true,
            allowed_models: vec![],
            allowed_tags: tags.iter().map(|t| t.to_string()).collect(),
            rpm_limit: None,
            tpm_limit: None,
            max_budget_usd: None,
            budget_period: None,
            spend_usd: 0.0,
            spend_reset_at: None,
            enabled: true,
            expires_at: None,
            metadata: serde_json::json!({}),
            guardrail_ids: Vec::new(),
            web_search: true,
            prompt_logging: false,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        // No tags on the key: everything.
        assert!(key(&[]).allows_deployment(&d(&["azure", "expensive"])));
        // Tags on the key: every tag of the deployment, untagged deployments always.
        assert!(key(&["azure", "cheap"]).allows_deployment(&d(&["azure", "cheap"])));
        assert!(key(&["azure", "cheap"]).allows_deployment(&d(&["azure"])));
        assert!(key(&["azure"]).allows_deployment(&d(&[])));
        assert!(!key(&["azure"]).allows_deployment(&d(&["azure", "expensive"])));
        assert!(!key(&["cheap"]).allows_deployment(&d(&["anthropic"])));
        assert!(!key(&["cheap"]).allows_deployment(&d(&["anthropic", "expensive"])));
        assert!(key(&["anthropic", "cheap", "expensive"]).allows_deployment(&d(&["anthropic", "expensive"])));

        // Limited to projects: only keys in one of them, whatever their tags.
        let (a, b) = (Uuid::from_u128(1), Uuid::from_u128(2));
        let limited = |tags: &[&str]| Deployment { project_ids: vec![a, b], ..d(tags) };
        let in_project = |p: Option<Uuid>, tags: &[&str]| VirtualKey { project_id: p, ..key(tags) };
        assert!(in_project(Some(a), &[]).allows_deployment(&limited(&[])));
        assert!(in_project(Some(b), &[]).allows_deployment(&limited(&["azure"])));
        assert!(!in_project(Some(Uuid::from_u128(3)), &[]).allows_deployment(&limited(&[])));
        assert!(!in_project(None, &[]).allows_deployment(&limited(&[])));
        assert!(!in_project(None, &["azure"]).allows_deployment(&limited(&["azure"])));
        // Tags still apply within the project.
        assert!(!in_project(Some(a), &["cheap"]).allows_deployment(&limited(&["azure"])));
        // Unlimited deployments stay open to keys in any project.
        assert!(in_project(Some(a), &[]).allows_deployment(&d(&[])));
    }

    #[test]
    fn long_context_tier_applies_above_threshold() {
        let p = Pricing {
            standard: Rates::new(3.0, 15.0, Some(0.3), None),
            long_context: Some((200_000, Rates::new(6.0, 22.5, None, None))),
            per_k_searches: 0.0,
        };
        // 150K uncached + 50K cached = 200K: not above the threshold, standard rates.
        let at = Usage { input_tokens: 150_000, cache_read_tokens: 50_000, output_tokens: 1_000, ..Default::default() };
        assert!((p.cost(&at) - (0.45 + 0.015 + 0.015)).abs() < 1e-9);
        // One more token: the whole request moves to the long-context tier (cache read defaults to long input).
        let above = Usage { input_tokens: 150_001, ..at };
        assert!((p.cost(&above) - (150_001.0 * 6.0 + 50_000.0 * 6.0 + 1_000.0 * 22.5) / 1e6).abs() < 1e-9);
        // Without a tier, everything is standard.
        let flat = Pricing { long_context: None, ..p };
        assert_eq!(flat.rates_for(&above).input, 3.0);
    }

    #[test]
    fn searches_are_priced_per_thousand() {
        let p = Pricing { standard: Rates::new(1.0, 0.0, None, None), long_context: None, per_k_searches: 2.0 };
        let u = Usage { input_tokens: 1_000, search_units: 3, ..Default::default() };
        assert!((p.cost(&u) - (0.001 + 0.006)).abs() < 1e-12);
    }
}

/// A deployment ready for routing, with credentials decrypted and resolved.
#[derive(Debug)]
pub struct Deployment {
    pub id: Uuid,
    pub model_name: String,
    pub mode: Mode,
    pub provider: ProviderKind,
    pub style: ApiStyle,
    pub upstream_model: String,
    pub config: DeploymentConfig,
    /// The referenced credential's connection settings.
    pub connection: CredentialSettings,
    /// The referenced credential's secrets, decrypted.
    pub secrets: Secrets,
    pub weight: u32,
    pub priority: i32,
    pub rpm_limit: Option<u64>,
    pub tpm_limit: Option<u64>,
    pub timeout: Duration,
    pub pricing: Pricing,
    pub tags: Vec<String>,
    /// Projects whose keys alone may use it; empty for any key.
    pub project_ids: Vec<Uuid>,
    /// The deployment's price catalog entry (LiteLLM's), if there is one: token limits and
    /// capabilities for `/v1/models`, and the reasoning efforts it accepts.
    pub catalog: Option<std::sync::Arc<serde_json::Value>>,
}

impl Deployment {
    /// Claude on Bedrock through InvokeModel rather than bedrock-mantle.
    pub fn bedrock_invoke(&self) -> bool {
        self.provider == ProviderKind::Bedrock && self.style == ApiStyle::Anthropic && self.config.bedrock_api == Some(BedrockApi::Invoke)
    }

    /// The reasoning efforts this deployment accepts, in order. From its catalog entry: the
    /// entry's `reasoning_effort_levels`, or else each level its `supports_<level>_reasoning_effort`
    /// flag allows, with low, medium and high allowed unless flagged off (LiteLLM leaves out flags
    /// for what most models do). Without an entry, what the gateway always sent: minimal to high
    /// for OpenAI's APIs, low to high for Anthropic's. An entry's Bedrock effort ceiling caps them.
    /// Anthropic models also take `none`: the gateway then asks for no thinking.
    pub fn efforts(&self) -> Vec<ReasoningEffort> {
        use ReasoningEffort as E;
        let anthropic = matches!(self.style, ApiStyle::Anthropic | ApiStyle::Converse);
        let mut efforts: Vec<E> = match self.catalog.as_deref() {
            Some(c) if c.get("reasoning_effort_levels").is_some_and(serde_json::Value::is_array) => {
                c["reasoning_effort_levels"].as_array().into_iter().flatten().filter_map(|l| l.as_str().and_then(E::parse)).collect()
            }
            Some(c) => E::ALL
                .into_iter()
                .filter(|e| match c.get(format!("supports_{}_reasoning_effort", e.as_str())).and_then(serde_json::Value::as_bool) {
                    Some(flag) => flag,
                    None => matches!(e, E::Low | E::Medium | E::High),
                })
                .collect(),
            None if anthropic => vec![E::Low, E::Medium, E::High],
            None => vec![E::Minimal, E::Low, E::Medium, E::High],
        };
        // Bedrock may cap the efforts below what the model itself takes.
        if let Some(ceiling) =
            self.catalog.as_deref().and_then(|c| c.get("bedrock_output_config_effort_ceiling")?.as_str()).and_then(E::parse)
        {
            efforts.retain(|e| *e <= ceiling);
        }
        if anthropic && !efforts.contains(&E::None) {
            efforts.insert(0, E::None);
        }
        efforts.sort();
        efforts.dedup();
        efforts
    }

    /// The effort to send for a requested one: the nearest the model takes. None to send none:
    /// for a model its catalog entry says does not reason, or for `none` on a model that cannot
    /// turn reasoning off (it keeps its default rather than reasoning more than asked).
    pub fn fit_effort(&self, requested: ReasoningEffort) -> Option<ReasoningEffort> {
        if self.catalog.as_deref().is_some_and(|c| c.get("supports_reasoning").and_then(serde_json::Value::as_bool) != Some(true)) {
            return None;
        }
        let fitted = requested.nearest(&self.efforts());
        (requested != ReasoningEffort::None || fitted == ReasoningEffort::None).then_some(fitted)
    }
}

/// Admin sign-in with OpenID Connect (single row).
#[derive(Debug, Clone)]
pub struct OidcSettingsRow {
    pub enabled: bool,
    pub display_name: String,
    pub issuer_url: String,
    pub client_id: String,
    /// Encrypted with the gateway's secret box.
    pub client_secret: Option<Vec<u8>>,
    pub public_url: String,
    /// ID token claim holding the user's roles (a dotted path reaches nested claims).
    pub roles_claim: String,
    /// The role that grants admin access. Required while sign-in is enabled.
    pub allowed_role: Option<String>,
    /// The role that lets someone sign in as a user, who sees only the projects an admin adds
    /// them to. None: only admins sign in.
    pub user_role: Option<String>,
    pub updated_at: DateTime<Utc>,
}

/// A group of virtual keys. It organises the console and can block its keys: while inactive,
/// they are refused like disabled keys. It grants, limits and budgets nothing.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct ProjectRow {
    pub id: Uuid,
    pub name: String,
    #[schema(required)]
    pub description: Option<String>,
    pub enabled: bool,
    /// Budget per `budget_period`, on top of each key's own; None = unlimited.
    #[schema(required)]
    pub max_budget_usd: Option<f64>,
    #[schema(required, value_type = Option<crate::budget::BudgetPeriod>)]
    pub budget_period: Option<String>,
    /// Set by "reset spend": the current period counts only spend after this.
    #[schema(required)]
    pub spend_reset_at: Option<DateTime<Utc>>,
    /// Guardrails every request of the project's keys goes through.
    pub guardrail_ids: Vec<Uuid>,
    /// Whether its keys' requests may have the model search the web. False forbids it for every
    /// key in the project, whatever the key's own says.
    pub web_search: bool,
    /// Whether its keys' prompts and responses are logged, for debugging: never, where a key
    /// opts in, or always (see `api::prompt_logged`).
    pub prompt_logging: PromptLogging,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A project's (or, for keys outside projects, the gateway's) prompt logging policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum PromptLogging {
    /// No key's requests are logged, whatever the key says.
    #[default]
    Deny,
    /// The keys that opt in are logged.
    Allow,
    /// Every key's requests are logged, whatever the key says. Projects only.
    Force,
}

impl PromptLogging {
    pub fn as_str(self) -> &'static str {
        match self {
            PromptLogging::Deny => "deny",
            PromptLogging::Allow => "allow",
            PromptLogging::Force => "force",
        }
    }
}

/// Someone who signed in to the admin UI with OIDC (or the Dev Login user).
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct AdminUserRow {
    pub id: Uuid,
    pub issuer: String,
    pub subject: String,
    #[schema(required)]
    pub email: Option<String>,
    #[schema(required)]
    pub name: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_login_at: DateTime<Utc>,
    /// What they may do, from their roles at their last sign-in.
    pub role: PersonRole,
}

/// An admin may do everything; a user only works in the projects an admin adds them to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum PersonRole {
    Admin,
    User,
}

/// One option of an auto router: a named pool of chat models and when to use it. The decision
/// model picks a route by its name; the request then goes to one of its models at random.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(try_from = "serde_json::Value")]
pub struct Route {
    /// What the decision model chooses, e.g. "Simple" or "Complex"; unique within the router.
    pub name: String,
    pub description: String,
    pub models: Vec<RouteModel>,
}

/// A chat model in a route.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RouteModel {
    pub model: String,
    /// Set on the requests the route sends to this model, replacing the client's; unset keeps it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// A route model's reasoning effort: the gateway's one effort type.
pub use crate::ir::Effort as ReasoningEffort;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PoolRoute {
    name: String,
    description: String,
    models: Vec<RouteModel>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SingleRoute {
    model: String,
    description: String,
}

/// A route as stored or sent: a pool, or a single `model` as before routes had several (older
/// rows and configuration exports), which becomes a pool named after it. The shape is chosen
/// first, so a mistake is reported for the field it is in.
impl TryFrom<serde_json::Value> for Route {
    type Error = serde_json::Error;

    fn try_from(v: serde_json::Value) -> Result<Self, Self::Error> {
        if v.get("model").is_some() && v.get("models").is_none() {
            let s: SingleRoute = serde_json::from_value(v)?;
            return Ok(Route {
                name: s.model.clone(),
                description: s.description,
                models: vec![RouteModel { model: s.model, reasoning_effort: None }],
            });
        }
        let p: PoolRoute = serde_json::from_value(v)?;
        Ok(Route { name: p.name, description: p.description, models: p.models })
    }
}

impl Route {
    pub fn model_names(&self) -> impl Iterator<Item = &str> {
        self.models.iter().map(|m| m.model.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct RouterRow {
    pub id: Uuid,
    pub model_name: String,
    pub decision_model: String,
    pub instructions: Option<String>,
    pub routes: sqlx::types::Json<Vec<Route>>,
    /// The name of the route used when the decision fails; the first route when unset.
    pub fallback_route: Option<String>,
    /// Permission tags, like a deployment's; see `VirtualKey::holds_all`.
    pub tags: Vec<String>,
    pub enabled: bool,
    /// Conversations stay on the route and model their first turn was sent to (see `affinity`).
    pub sticky: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// An auto router ready for routing: a model name served by asking `decision_model` which of
/// the routes fits a request best.
#[derive(Debug, Clone)]
pub struct AutoRouter {
    pub id: Uuid,
    pub model_name: String,
    pub decision_model: String,
    pub instructions: Option<String>,
    pub routes: Vec<Route>,
    /// The route requests take when the decision fails: the configured fallback or the first route.
    pub fallback_route: String,
    pub tags: Vec<String>,
    /// Conversations stay on the route and model their first turn was sent to (see `affinity`).
    pub sticky: bool,
}

impl AutoRouter {
    pub fn route(&self, name: &str) -> Option<&Route> {
        self.routes.iter().find(|r| r.name == name)
    }

    /// Every chat model of every route (a model in several routes repeats).
    pub fn models(&self) -> impl Iterator<Item = &str> {
        self.routes.iter().flat_map(Route::model_names)
    }
}

/// A model alias: a public model name that serves the model a deployment or auto router has.
#[derive(Debug, Clone)]
pub struct AliasRow {
    pub id: Uuid,
    pub model_name: String,
    /// A deployment's or an auto router's id. The alias serves the model name it has now, so it
    /// follows a rename; see `admin::retarget_aliases` for a deployment that leaves its model.
    pub target_id: Uuid,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// An alias ready for routing: requests for `model_name` are served as requests for `target`.
#[derive(Debug, Clone)]
pub struct ModelAlias {
    pub model_name: String,
    /// The target's model name.
    pub target: String,
}

/// What a virtual key may call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum KeyScope {
    /// The inference endpoints (`/v1/...`).
    #[default]
    Inference,
    /// The admin API (`/admin/api/...`), with the same rights as the master key. Not in a project.
    Admin,
    /// Both. Not in a project.
    All,
    /// The admin API for the key's project only: what a user who works in it may see and change.
    Project,
}

impl KeyScope {
    pub fn as_str(self) -> &'static str {
        match self {
            KeyScope::Inference => "inference",
            KeyScope::Admin => "admin",
            KeyScope::All => "all",
            KeyScope::Project => "project",
        }
    }

    pub fn inference(self) -> bool {
        matches!(self, KeyScope::Inference | KeyScope::All)
    }

    /// Whether the key reaches the admin API at all; a project key only its project's part.
    pub fn admin(self) -> bool {
        matches!(self, KeyScope::Admin | KeyScope::All | KeyScope::Project)
    }

    /// Whether a key in a project may have this scope: every right an admin has is not a project's.
    pub fn fits_project(self) -> bool {
        matches!(self, KeyScope::Inference | KeyScope::Project)
    }
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct VirtualKey {
    pub id: Uuid,
    pub name: String,
    #[serde(skip)]
    pub key_hash: Vec<u8>,
    pub key_prefix: String,
    /// Whether the key calls the inference API, the admin API or both.
    pub scope: KeyScope,
    /// The project the key is grouped under in the console.
    #[schema(required)]
    pub project_id: Option<Uuid>,
    /// False while the key's project is inactive; the key is then refused like a disabled one.
    pub project_active: bool,
    pub allowed_models: Vec<String>,
    /// Deployment tags this key holds; empty means no tag restriction.
    pub allowed_tags: Vec<String>,
    #[schema(required)]
    pub rpm_limit: Option<i32>,
    #[schema(required)]
    pub tpm_limit: Option<i32>,
    /// Budget per `budget_period` (day, week or month); None = unlimited.
    #[schema(required)]
    pub max_budget_usd: Option<f64>,
    #[schema(required, value_type = Option<crate::budget::BudgetPeriod>)]
    pub budget_period: Option<String>,
    /// Lifetime spend (informational; budgets use per-period spend).
    pub spend_usd: f64,
    /// Set by "reset spend": the current period counts only spend after this.
    #[schema(required)]
    pub spend_reset_at: Option<DateTime<Utc>>,
    pub enabled: bool,
    #[schema(required)]
    pub expires_at: Option<DateTime<Utc>>,
    pub metadata: serde_json::Value,
    /// Guardrails every request of this key goes through, on top of its project's.
    pub guardrail_ids: Vec<Uuid>,
    /// Whether its requests may have the model search the web (billed per search). Its project
    /// may forbid it anyway: see `api::web_search_allowed`.
    pub web_search: bool,
    /// Whether its prompts and responses are logged, where its project allows it. A project that
    /// denies or forces logging decides instead: see `api::prompt_logged`.
    pub prompt_logging: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl VirtualKey {
    pub fn allows_model(&self, model: &str) -> bool {
        self.allowed_models.is_empty() || self.allowed_models.iter().any(|m| m == model)
    }

    /// Whether this key may use something carrying `tags` (a deployment or an auto router): a key
    /// without tags may use everything, a key with tags what carries no tag it lacks (untagged
    /// things included).
    pub fn holds_all(&self, tags: &[String]) -> bool {
        self.allowed_tags.is_empty() || tags.iter().all(|t| self.allowed_tags.contains(t))
    }

    /// A deployment limited to projects is only for keys in one of them, whatever their tags;
    /// within that, tags apply as everywhere.
    pub fn allows_deployment(&self, d: &Deployment) -> bool {
        let in_project = d.project_ids.is_empty() || self.project_id.is_some_and(|p| d.project_ids.contains(&p));
        in_project && self.holds_all(&d.tags)
    }

    pub fn allows_router(&self, r: &AutoRouter) -> bool {
        self.holds_all(&r.tags)
    }
}

/// Trim and lowercase tags, drop empty ones and duplicates, and sort them.
pub fn normalize_tags(tags: Vec<String>) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = tags.into_iter().map(|t| t.trim().to_lowercase()).filter(|t| !t.is_empty()).collect();
    if let Some(bad) = out
        .iter()
        .find(|t| t.len() > 40 || !t.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.')))
    {
        return Err(format!("tag '{bad}' must be up to 40 letters, digits, '-', '_' or '.'"));
    }
    out.sort();
    out.dedup();
    if out.len() > 20 {
        return Err("at most 20 tags".into());
    }
    Ok(out)
}
