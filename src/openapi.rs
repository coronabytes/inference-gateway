//! The OpenAPI document served at `/admin/openapi.json` (and rendered by the Scalar API reference at
//! `/admin/docs/`), to signed-in people and admin keys only. The `api_docs` setting turns both off.
//!
//! Each operation is documented on its handler, with `#[utoipa::path]`, and the routers register
//! the handlers with `utoipa_axum::routes!`, which takes the path and methods from that
//! documentation (see `routes` in main.rs). So the document lists exactly the routes the gateway
//! serves. The admin API is described from its own request and response types, which derive
//! `ToSchema`. The inference endpoints speak the OpenAI and Anthropic APIs, whose full formats
//! are the providers' to document; their schemas here name the fields the gateway itself reads
//! and accept everything else.

use std::sync::LazyLock;

use axum::{http::header, response::IntoResponse};
use utoipa::{
    Modify, OpenApi,
    openapi::security::{ApiKey, ApiKeyValue, HttpAuthScheme, HttpBuilder, SecurityScheme},
};

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Inference Gateway",
        description = "One endpoint and one key for every model your organisation uses.\n\n\
            **Inference** endpoints speak the OpenAI API (Chat Completions, Responses, embeddings, \
            decisions) and the Anthropic Messages API, streaming or not, for any model behind the \
            gateway. Authenticate with a virtual key (`sk-gw-…`) whose scope is `inference` or \
            `all`, as `Authorization: Bearer` or `x-api-key`. The `model` is the model name chosen \
            in the gateway.\n\n\
            The **admin API** (`/admin/api/…`) is what the console uses. Authenticate with the \
            master key or a virtual key whose scope is `admin`, `all` or `project` (its project's \
            keys, playground, request log and metrics only), as `Authorization: Bearer`, or with a \
            console session cookie; requests with the cookie that change something also need an \
            `x-gateway-csrf` header.",
        license(name = "AGPL-3.0-only", identifier = "AGPL-3.0-only"),
    ),
    modifiers(&SecuritySchemes),
    tags(
        (name = "Inference", description = "OpenAI- and Anthropic-compatible endpoints, authenticated with a virtual key."),
        (name = "Health", description = "Liveness and readiness probes. No authentication."),
        (name = "Sign-in", description = "Console sessions and single sign-on."),
        (name = "Credentials", description = "Provider accounts. Secrets are write-only: responses only say which are set."),
        (name = "Deployments", description = "Model names and the provider models that serve them."),
        (name = "Auto routers", description = "Model names that pick one of several chat models per request."),
        (name = "Model aliases", description = "Model names that serve another model, which they follow when it is renamed."),
        (name = "Guardrails", description = "Provider guardrails that mask or block what prompts and responses must not contain."),
        (name = "MCP", description = "MCP servers behind the gateway: one key for every server, the servers' credentials kept in the gateway."),
        (name = "Upstream OAuth", description = "Signing the gateway, or each key, in to MCP servers and A2A agents with OAuth."),
        (name = "A2A", description = "A2A agents behind the gateway: one key for every agent, the agents' credentials kept in the gateway."),
        (name = "MCP client sign-in", description = "MCP clients (such as Claude or ChatGPT) signed in to the gateway with OAuth, each as a virtual key."),
        (name = "Tool routers", description = "A name requests use like an MCP server's, whose decision model picks which servers' tools a request gets."),
        (name = "Virtual keys", description = "Keys for apps and people, with limits, budgets and access tags, and the projects that group them."),
        (name = "Configuration", description = "Tags, and exporting and importing the configuration."),
        (name = "Model hub", description = "The models a key may use, with their limits, prices and guardrails, and the models' metadata."),
        (name = "Usage", description = "Metrics, the request and audit logs, and the cluster."),
        (name = "Settings", description = "Gateway settings, applied on every replica without a restart."),
        (name = "Playground", description = "Inference as a chosen virtual key, for the console."),
        (name = "Price catalog", description = "Public model prices, to fill in deployment prices."),
    ),
)]
pub(crate) struct ApiDoc;

/// The document, from the routers' handlers, with this build's version. `inference-gateway
/// openapi` prints it, so it must not need a database or any configuration.
pub fn spec() -> utoipa::openapi::OpenApi {
    let mut doc = crate::routes(None).into_openapi();
    doc.info.version = crate::VERSION.into();
    doc
}

static DOCUMENT: LazyLock<String> = LazyLock::new(|| spec().to_pretty_json().expect("the OpenAPI document serializes"));

/// `GET /admin/openapi.json`.
pub async fn document() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/json")], DOCUMENT.as_str())
}

struct SecuritySchemes;

impl Modify for SecuritySchemes {
    fn modify(&self, doc: &mut utoipa::openapi::OpenApi) {
        let c = doc.components.get_or_insert_with(Default::default);
        let bearer = |format: &str, description: &str| {
            SecurityScheme::Http(
                HttpBuilder::new().scheme(HttpAuthScheme::Bearer).bearer_format(format).description(Some(description)).build(),
            )
        };
        c.add_security_scheme("virtual_key", bearer("sk-gw-…", "A virtual key, as OpenAI clients send it."));
        c.add_security_scheme(
            "virtual_key_header",
            SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::with_description(
                "x-api-key",
                "A virtual key, as Anthropic clients send it.",
            ))),
        );
        c.add_security_scheme("master_key", bearer("GATEWAY_MASTER_KEY", "The gateway's master key."));
        c.add_security_scheme(
            "admin_key",
            bearer("sk-gw-…", "A virtual key whose scope is `admin`, `all` or `project` (its project only)."),
        );
        c.add_security_scheme(
            "admin_session",
            SecurityScheme::ApiKey(ApiKey::Cookie(ApiKeyValue::with_description(
                "gw_admin",
                "A console session. Requests that change something also need an `x-gateway-csrf` header (any value).",
            ))),
        );
    }
}

pub(crate) use schemas::*;

/// Request and response shapes that have no type of their own in the gateway: the provider APIs
/// it speaks, and admin answers it builds as JSON. They are never built, only described.
#[allow(dead_code)]
mod schemas {
    use chrono::{DateTime, Utc};
    use serde::Serialize;
    use serde_json::Value;
    use utoipa::ToSchema;

    use crate::model::VirtualKey;

    /// An OpenAI Chat Completions request. Any other field of the OpenAI API is accepted and
    /// passed on, as far as the deployment's provider supports it.
    #[derive(ToSchema)]
    pub struct ChatCompletionRequest {
        /// A model name, auto router or model alias configured in the gateway.
        model: String,
        messages: Vec<Value>,
        stream: Option<bool>,
        /// On streams, the gateway always asks for `include_usage`, for accounting.
        stream_options: Option<Value>,
        max_completion_tokens: Option<u32>,
        tools: Option<Vec<Value>>,
        /// `auto`, `default` or `flex`; tiers billed above the deployment's prices are refused.
        service_tier: Option<String>,
    }

    /// An OpenAI Responses request. Any other field of the OpenAI API is accepted.
    #[derive(ToSchema)]
    pub struct ResponsesRequest {
        model: String,
        /// A string or a list of input items.
        input: Value,
        instructions: Option<String>,
        /// Continue a conversation. Works on every model: the gateway keeps the history for
        /// providers that do not, until it goes unused for the `response_ttl_minutes` setting, within
        /// the key's quota.
        previous_response_id: Option<String>,
        /// Defaults to true; false keeps no history.
        store: Option<bool>,
        stream: Option<bool>,
        /// Refused: the gateway can neither account for nor retrieve background responses.
        background: Option<bool>,
        service_tier: Option<String>,
    }

    /// An Anthropic Messages request. Any other field of the Anthropic API is accepted.
    #[derive(ToSchema)]
    pub struct MessagesRequest {
        model: String,
        messages: Vec<Value>,
        /// Defaults to the deployment's `default_max_tokens` when omitted.
        max_tokens: Option<u32>,
        system: Option<Value>,
        stream: Option<bool>,
        tools: Option<Vec<Value>>,
        thinking: Option<Value>,
    }

    /// An Anthropic token count request.
    #[derive(ToSchema)]
    pub struct CountTokensRequest {
        model: String,
        messages: Vec<Value>,
        system: Option<Value>,
        tools: Option<Vec<Value>>,
    }

    #[derive(Serialize, ToSchema)]
    pub struct TokenCount {
        input_tokens: u64,
    }

    /// An OpenAI embeddings request, on every provider.
    #[derive(ToSchema)]
    pub struct EmbeddingsRequest {
        model: String,
        /// A string, a list of strings, or token arrays.
        input: Value,
        dimensions: Option<u32>,
        /// `float` (default) or `base64`.
        encoding_format: Option<String>,
    }

    /// A rerank request, as Cohere's API takes it.
    #[derive(ToSchema)]
    pub struct RerankRequest {
        model: String,
        query: String,
        /// Up to 1,000 documents: strings, or objects with a `text`.
        documents: Vec<Value>,
        /// How many of the most relevant documents to return (default: all).
        top_n: Option<u32>,
        /// Include each document's text in the results.
        return_documents: Option<bool>,
        /// Cohere models only: truncate documents to this many tokens.
        max_tokens_per_doc: Option<u32>,
    }

    /// Ranked documents, most relevant first, as Cohere's v2 API returns them.
    #[derive(Serialize, ToSchema)]
    pub struct RerankResponse {
        /// The request's id, as in the request log.
        id: String,
        model: String,
        /// `{ index, relevance_score, document? }`: the document's position in the request, its
        /// score, and its text with `return_documents`.
        results: Vec<Value>,
        /// `{ billed_units: { search_units, input_tokens? } }`.
        meta: Value,
    }

    /// An OpenAI Decisions request.
    #[derive(ToSchema)]
    pub struct DecisionsRequest {
        model: String,
        /// A string or a list of messages.
        input: Value,
        /// 1 to 128 questions: yes/no, choices, scores or boxes.
        questions: Vec<Value>,
    }

    /// A TypeSafe System One request.
    #[derive(ToSchema)]
    pub struct SystemOneRequest {
        model: String,
        state: Value,
        /// 1 to 128 named questions.
        questions: Value,
        /// Images and other assets; TypeSafe deployments only, with `TypeSafe-Danger-Zone-Images: true`.
        assets: Option<Value>,
    }

    /// The provider's answer, in the API's own format: a JSON object, or server-sent events when
    /// streaming. Responses carry `x-request-id`, and routed ones `x-gateway-routed-model` and
    /// `x-gateway-route`.
    #[derive(ToSchema)]
    #[schema(value_type = Object)]
    pub struct ProviderAnswer(Value);

    /// An error, in the OpenAI API's format.
    #[derive(Serialize, ToSchema)]
    pub struct OpenAiError {
        error: OpenAiErrorDetail,
    }

    #[derive(Serialize, ToSchema)]
    pub struct OpenAiErrorDetail {
        message: String,
        #[serde(rename = "type")]
        kind: String,
        param: Option<String>,
        code: Option<String>,
    }

    /// An error, in the Anthropic API's format.
    #[derive(Serialize, ToSchema)]
    pub struct AnthropicError {
        /// Always `error`.
        #[serde(rename = "type")]
        kind: String,
        error: AnthropicErrorDetail,
    }

    #[derive(Serialize, ToSchema)]
    pub struct AnthropicErrorDetail {
        #[serde(rename = "type")]
        kind: String,
        message: String,
    }

    /// The models a key may use, in the OpenAI format (or Anthropic's, for clients that send
    /// `anthropic-version`).
    #[derive(Serialize, ToSchema)]
    pub struct ModelList {
        object: String,
        data: Vec<ModelEntry>,
    }

    #[derive(Serialize, ToSchema)]
    pub struct ModelEntry {
        id: String,
        object: String,
        created: i64,
        owned_by: String,
        /// When the model will shut down (`YYYY-MM-DD`), from the price catalog; null if not
        /// announced.
        shutdown_date: Option<String>,
    }

    #[derive(Serialize, ToSchema)]
    pub struct ModelExtendedList {
        object: String,
        data: Vec<ModelExtendedEntry>,
    }

    /// A model with what the gateway knows about it. Where a model has several deployments, or
    /// is an auto router, the limits are the lowest among them and `structured_outputs` is true
    /// only if all of them support it.
    #[derive(Serialize, ToSchema)]
    pub struct ModelExtendedEntry {
        id: String,
        object: String,
        created: i64,
        owned_by: String,
        /// When the model will shut down (`YYYY-MM-DD`), from the price catalog; null if not
        /// announced.
        shutdown_date: Option<String>,
        /// What the model serves. Auto routers are `chat`.
        mode: crate::model::Mode,
        /// From the price catalog; null if unknown.
        max_input_tokens: Option<u64>,
        /// From the price catalog; null if unknown.
        max_output_tokens: Option<u64>,
        /// Whether the model supports JSON schema output, from the price catalog. False if unknown.
        structured_outputs: bool,
        /// The model's deployment tags, or an auto router's own tags.
        tags: Vec<String>,
    }

    #[derive(Serialize, ToSchema)]
    pub struct Ready {
        postgres: bool,
        valkey: bool,
        registry_loaded: bool,
    }

    /// An admin API error.
    #[derive(Serialize, ToSchema)]
    pub struct AdminError {
        error: String,
    }

    #[derive(Serialize, ToSchema)]
    pub struct SessionInfo {
        authenticated: bool,
        /// `master_key`, `virtual_key`, `session` or `dev`.
        via: Option<String>,
        user: Option<Value>,
        /// Set when single sign-on is enabled: `{ "display_name": … }`.
        sso: Option<Value>,
        /// Whether `GATEWAY_MASTER_KEY` is set.
        master_key: bool,
        /// Local development builds offer Dev Login.
        dev: bool,
        /// The git tag the gateway was built from.
        version: String,
        /// Whether `/admin/openapi.json` and the API reference at `/admin/docs/` are served.
        api_docs: bool,
    }

    #[derive(Serialize, ToSchema)]
    pub struct OidcTest {
        ok: bool,
        authorization_endpoint: Option<String>,
        error: Option<String>,
    }

    #[derive(Serialize, ToSchema)]
    pub struct DeploymentTest {
        ok: bool,
        latency_ms: u64,
        /// The model's reply (chat), dimensions (embeddings) or an answer (decisions).
        text: Option<String>,
        usage: Option<Value>,
        /// The HTTP status of a failed test.
        status: Option<u16>,
        error: Option<String>,
    }

    /// A new key. `key` is the secret, shown this once; the gateway keeps only its hash.
    #[derive(Serialize, ToSchema)]
    pub struct KeyCreated {
        key: String,
        record: VirtualKey,
    }

    /// A key's new secret, shown this once. The old one stops working.
    #[derive(Serialize, ToSchema)]
    pub struct KeyRegenerated {
        key: String,
        key_prefix: String,
    }

    #[derive(Serialize, ToSchema)]
    pub struct TagUsage {
        tag: String,
        /// Deployments carrying the tag.
        deployments: usize,
        /// Auto routers carrying the tag.
        routers: usize,
        /// MCP servers and A2A agents carrying the tag.
        mcp_servers: usize,
        /// Keys holding the tag (for a user, their projects' keys).
        keys: usize,
    }

    #[derive(Serialize, ToSchema)]
    pub struct ImportChanges {
        created: Vec<String>,
        updated: Vec<String>,
        /// Entries of the file that match one here that is already the same.
        unchanged: usize,
    }

    #[derive(Serialize, ToSchema)]
    pub struct ImportResult {
        dry_run: bool,
        credentials: ImportChanges,
        guardrails: ImportChanges,
        projects: ImportChanges,
        deployments: ImportChanges,
        routers: ImportChanges,
        aliases: ImportChanges,
        /// The file's keys, and keys whose allowed models follow a renamed model.
        keys: ImportChanges,
        /// Models whose metadata the file sets, or that takes a renamed model's along.
        model_metadata: ImportChanges,
        mcp_servers: ImportChanges,
        a2a_agents: ImportChanges,
        tool_routers: ImportChanges,
        /// New credentials that need their secrets entered before their deployments work.
        needs_secrets: Vec<String>,
        /// New MCP servers that work once their secret is entered.
        mcp_servers_need_secrets: Vec<String>,
        /// New A2A agents that work once their secret is entered.
        a2a_agents_need_secrets: Vec<String>,
    }

    #[derive(Serialize, ToSchema)]
    pub struct Status {
        instance_id: String,
        /// The git tag the gateway was built from.
        version: String,
        registry_loaded_at: Option<DateTime<Utc>>,
        /// Deployments, routers and aliases that could not be loaded, and why.
        registry_errors: Vec<String>,
    }
}

/// The `x-gateway-labels` header, as the inference endpoints describe it.
pub(crate) const LABELS_HEADER: &str = "Up to 5 labels for the request log and metrics, as comma-separated `key=value` pairs, \
    such as `tenant=0192…,feature=core`. Keys are letters, digits, `_`, `-` and `.`; values are printable ASCII \
    without commas. Keys and values are shorter than 128 characters. Invalid labels are refused with `400`.";

/// The `mcp-session-id` header of `/mcp/{server}`.
pub(crate) const MCP_SESSION: &str = "The session the server opened, as the gateway handed it out with `initialize`'s answer. It only works \
    with the key that opened it; an unknown one is answered with `404`, after which the client starts a new session.";

/// What an A2A message does, for the A2A endpoints' answers.
pub(crate) const A2A_STATES: &str = "Each message is an entry in the request log (api `a2a`) with the state its task was left in, counts against \
    the key's limits and budgets, and is charged the agent's price. Tasks belong to the key whose message created them: \
    calls about another key's task are answered as for an unknown task.";

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    /// The paths of routes registered with a plain `.route(…)` rather than with `routes!`, which
    /// the document therefore leaves out, read from the router definitions.
    fn undocumented_routes() -> BTreeSet<String> {
        let sources = [include_str!("main.rs"), include_str!("admin.rs"), include_str!("prices.rs"), include_str!("admin/auth.rs")];
        let mut out = BTreeSet::new();
        for src in sources {
            let src = src.split("#[cfg(test)]\nmod tests").next().unwrap_or_default();
            for chunk in src.split(".route(\"").skip(1) {
                out.insert(chunk.split('"').next().expect("route path").to_string());
            }
        }
        out
    }

    /// Every route is documented, except the aliases of documented `/v1/…` endpoints, the
    /// documentation itself, and the redirect to it in debug builds.
    #[test]
    fn only_aliases_are_undocumented() {
        let expected: BTreeSet<String> =
            ["/decisions", "/systemone", "/embeddings", "/rerank", "/v2/rerank", "/openapi.json", "/"].map(String::from).into();
        assert_eq!(undocumented_routes(), expected);
    }

    /// The document, built without a state, describes the router the gateway serves, which is
    /// built with one. Building it also checks that no two routes overlap, which axum refuses.
    #[tokio::test]
    async fn document_describes_the_served_routes() {
        let served = crate::routes(Some(&crate::state::test_state())).into_openapi();
        let documented = crate::routes(None).into_openapi();
        assert!(served.paths == documented.paths, "the served routes differ from the documented ones");
        assert!(documented.paths.paths.len() > 100, "only {} paths", documented.paths.paths.len());
    }

    #[test]
    fn document_is_complete() {
        let doc: serde_json::Value = serde_json::from_str(&DOCUMENT).unwrap();
        assert_eq!(doc["info"]["version"], crate::VERSION);
        assert_eq!(doc["info"]["title"], "Inference Gateway");
        // Every schema a path refers to is defined.
        let text = DOCUMENT.as_str();
        let schemas = doc["components"]["schemas"].as_object().unwrap();
        for r in text.split("\"#/components/schemas/").skip(1) {
            let name = r.split('"').next().unwrap();
            assert!(schemas.contains_key(name), "{name} is referenced but not defined");
        }
        let schemes = doc["components"]["securitySchemes"].as_object().unwrap();
        for s in ["virtual_key", "virtual_key_header", "master_key", "admin_key", "admin_session"] {
            assert!(schemes.contains_key(s), "{s}");
        }
        // Operation ids are unique, as clients generated from the document need them to be.
        let mut ids = BTreeSet::new();
        for ops in doc["paths"].as_object().unwrap().values() {
            for op in ops.as_object().unwrap().values() {
                let id = op["operationId"].as_str().unwrap();
                assert!(ids.insert(id.to_string()), "{id} is used twice");
            }
        }
        // Secrets are write-only: no response schema exposes a credential's or key's secret.
        assert!(!schemas["CredentialView"]["properties"].as_object().unwrap().contains_key("secrets"));
        assert!(!schemas["VirtualKey"]["properties"].as_object().unwrap().contains_key("key_hash"));
    }
}
