//! The OpenAPI document served at `/admin/openapi.json` (and rendered by the Scalar API reference at
//! `/admin/docs/`), to signed-in people and admin keys only. The `api_docs` setting turns both off.
//!
//! The admin API is described from its own request and response types, which derive
//! `ToSchema`. The inference endpoints speak the OpenAI and Anthropic APIs, whose full formats
//! are the providers' to document; their schemas here name the fields the gateway itself reads
//! and accept everything else. The operations are declared below rather than on the handlers, so
//! the whole API is in one place; a test checks that every route the gateway serves is here.

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
    paths(
        paths::chat_completions, paths::responses, paths::messages, paths::count_tokens, paths::embeddings,
        paths::rerank, paths::decisions, paths::systemone, paths::list_models, paths::list_models_extended,
        paths::mcp_hub_post, paths::mcp_hub_get, paths::mcp_hub_delete, paths::mcp_post, paths::mcp_get, paths::mcp_delete,
        paths::a2a_card, paths::a2a_jsonrpc, paths::a2a_rest_post, paths::a2a_rest_get, paths::a2a_rest_delete,
        paths::health_live, paths::health_ready,
        paths::session, paths::logout, paths::dev_login, paths::oidc_login, paths::oidc_callback,
        paths::oauth_callback, paths::oauth_discover, paths::key_connections, paths::key_connect, paths::key_disconnect,
        paths::get_oidc_settings, paths::put_oidc_settings, paths::test_oidc_settings, paths::list_admins, paths::delete_admin, paths::set_admin_projects,
        paths::list_credentials, paths::create_credential, paths::update_credential, paths::delete_credential, paths::foundry_deployments, paths::bedrock_models, paths::aws_regions,
        paths::list_guardrails, paths::create_guardrail, paths::update_guardrail, paths::delete_guardrail, paths::test_guardrail,
        paths::guardrail_prices,
        paths::list_a2a_agents, paths::create_a2a_agent, paths::update_a2a_agent, paths::delete_a2a_agent, paths::test_a2a_agent,
        paths::list_mcp_servers, paths::create_mcp_server, paths::update_mcp_server, paths::delete_mcp_server, paths::test_mcp_server,
        paths::list_deployments, paths::create_deployment, paths::get_deployment, paths::update_deployment,
        paths::delete_deployment, paths::test_deployment, paths::clear_cooldown,
        paths::list_routers, paths::create_router, paths::update_router, paths::delete_router,
        paths::list_aliases, paths::create_alias, paths::update_alias, paths::delete_alias,
        paths::list_keys, paths::key_options, paths::create_key, paths::update_key, paths::delete_key, paths::reset_spend, paths::regenerate_key,
        paths::key_bodies, paths::delete_key_bodies,
        paths::list_projects, paths::project_options, paths::create_project, paths::update_project, paths::delete_project, paths::reset_project_spend,
        paths::project_bodies, paths::delete_project_bodies,
        paths::list_tags, paths::export_config, paths::import_config, paths::model_hub, paths::model_efforts, paths::set_model_metadata,
        paths::status, paths::metrics, paths::logs, paths::log_labels, paths::log_tools, paths::log_bodies, paths::audit_log, paths::key_usage, paths::cluster,
        paths::get_settings, paths::put_settings,
        paths::playground, paths::playground_a2a, paths::playground_a2a_card, paths::playground_options,
        paths::search_prices, paths::match_price, paths::check_prices, paths::import_prices,
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
        (name = "Virtual keys", description = "Keys for apps and people, with limits, budgets and access tags, and the projects that group them."),
        (name = "Configuration", description = "Tags, and exporting and importing the configuration."),
        (name = "Model hub", description = "The models a key may use, with their limits, prices and guardrails, and the models' metadata."),
        (name = "Usage", description = "Metrics, the request and audit logs, and the cluster."),
        (name = "Settings", description = "Gateway settings, applied on every replica without a restart."),
        (name = "Playground", description = "Inference as a chosen virtual key, for the console."),
        (name = "Price catalog", description = "Public model prices, to fill in deployment prices."),
    ),
)]
struct ApiDoc;

/// The document, with this build's version.
static DOCUMENT: LazyLock<String> = LazyLock::new(|| {
    let mut doc = ApiDoc::openapi();
    doc.info.version = crate::VERSION.into();
    doc.to_pretty_json().expect("the OpenAPI document serializes")
});

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
            SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::with_description("x-api-key", "A virtual key, as Anthropic clients send it."))),
        );
        c.add_security_scheme("master_key", bearer("GATEWAY_MASTER_KEY", "The gateway's master key."));
        c.add_security_scheme("admin_key", bearer("sk-gw-…", "A virtual key whose scope is `admin`, `all` or `project` (its project only)."));
        c.add_security_scheme(
            "admin_session",
            SecurityScheme::ApiKey(ApiKey::Cookie(ApiKeyValue::with_description(
                "gw_admin",
                "A console session. Requests that change something also need an `x-gateway-csrf` header (any value).",
            ))),
        );
    }
}

/// Request and response shapes that have no type of their own in the gateway: the provider APIs
/// it speaks, and admin answers it builds as JSON.
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
    }

    #[derive(Serialize, ToSchema)]
    pub struct ImportResult {
        dry_run: bool,
        credentials: ImportChanges,
        guardrails: ImportChanges,
        deployments: ImportChanges,
        routers: ImportChanges,
        aliases: ImportChanges,
        /// Keys whose allowed models follow a renamed model (only `updated`).
        keys: ImportChanges,
        /// Models whose metadata the file sets, or that takes a renamed model's along.
        model_metadata: ImportChanges,
        /// New credentials that need their secrets entered before their deployments work.
        needs_secrets: Vec<String>,
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

/// The operations, one per route. Bodies are empty: these exist for their documentation.
#[allow(dead_code)]
mod paths {
    use super::schemas::*;
    use crate::{admin, admin_auth, budget, db, model, oauth};

    // ------------------------------------------------------------ inference

    const LABELS_HEADER: &str = "Up to 5 labels for the request log and metrics, as comma-separated `key=value` pairs, \
        such as `tenant=0192…,feature=core`. Keys are letters, digits, `_`, `-` and `.`; values are printable ASCII \
        without commas. Keys and values are shorter than 128 characters. Invalid labels are refused with `400`.";

    /// Chat Completions
    ///
    /// The OpenAI Chat Completions API, for every model behind the gateway.
    #[utoipa::path(post, path = "/v1/chat/completions", tag = "Inference", request_body = ChatCompletionRequest,
        responses(
            (status = 200, description = "The completion, or server-sent events with `stream: true`.",
                content((ProviderAnswer = "application/json"), (String = "text/event-stream"))),
            (status = 400, description = "Invalid request.", body = OpenAiError),
            (status = 401, description = "Missing, invalid, disabled or expired key.", body = OpenAiError),
            (status = 403, description = "The key may not use this model.", body = OpenAiError),
            (status = 429, description = "A rate limit or the key's budget; see `Retry-After`.", body = OpenAiError),
            (status = 502, description = "The provider failed.", body = OpenAiError),
        ),
        params(("x-gateway-labels" = Option<String>, Header, description = LABELS_HEADER)),
        security(("virtual_key" = []), ("virtual_key_header" = [])))]
    fn chat_completions() {}

    /// Responses
    ///
    /// The OpenAI Responses API, with `previous_response_id` on every model.
    #[utoipa::path(post, path = "/v1/responses", tag = "Inference", request_body = ResponsesRequest,
        responses(
            (status = 200, description = "The response, or server-sent events with `stream: true`.",
                content((ProviderAnswer = "application/json"), (String = "text/event-stream"))),
            (status = 400, description = "Invalid request.", body = OpenAiError),
            (status = 401, description = "Missing, invalid, disabled or expired key.", body = OpenAiError),
            (status = 403, description = "The key may not use this model.", body = OpenAiError),
            (status = 404, description = "The previous response is unknown, expired, or belongs to another key.", body = OpenAiError),
            (status = 429, description = "A rate limit or the key's budget; see `Retry-After`.", body = OpenAiError),
            (status = 502, description = "The provider failed.", body = OpenAiError),
        ),
        params(("x-gateway-labels" = Option<String>, Header, description = LABELS_HEADER)),
        security(("virtual_key" = []), ("virtual_key_header" = [])))]
    fn responses() {}

    /// Messages
    ///
    /// The Anthropic Messages API, for every model behind the gateway.
    #[utoipa::path(post, path = "/v1/messages", tag = "Inference", request_body = MessagesRequest,
        responses(
            (status = 200, description = "The message, or server-sent events with `stream: true`.",
                content((ProviderAnswer = "application/json"), (String = "text/event-stream"))),
            (status = 400, description = "Invalid request.", body = AnthropicError),
            (status = 401, description = "Missing, invalid, disabled or expired key.", body = AnthropicError),
            (status = 403, description = "The key may not use this model.", body = AnthropicError),
            (status = 429, description = "A rate limit or the key's budget; see `Retry-After`.", body = AnthropicError),
            (status = 502, description = "The provider failed.", body = AnthropicError),
        ),
        params(("x-gateway-labels" = Option<String>, Header, description = LABELS_HEADER)),
        security(("virtual_key_header" = []), ("virtual_key" = [])))]
    fn messages() {}

    /// Count tokens
    ///
    /// Anthropic's token count. Forwarded to an Anthropic-style deployment of the model, else
    /// estimated from the request's size. Counts against the key's limits like a request.
    #[utoipa::path(post, path = "/v1/messages/count_tokens", tag = "Inference", request_body = CountTokensRequest,
        responses(
            (status = 200, description = "The token count.", body = TokenCount),
            (status = 401, description = "Missing, invalid, disabled or expired key.", body = AnthropicError),
            (status = 403, description = "The key may not use this model.", body = AnthropicError),
            (status = 429, description = "A rate limit or the key's budget.", body = AnthropicError),
        ),
        params(("x-gateway-labels" = Option<String>, Header, description = LABELS_HEADER)),
        security(("virtual_key_header" = []), ("virtual_key" = [])))]
    fn count_tokens() {}

    /// Embeddings
    ///
    /// OpenAI embeddings, on every provider. Also served at `/embeddings`.
    #[utoipa::path(post, path = "/v1/embeddings", tag = "Inference", request_body = EmbeddingsRequest,
        responses(
            (status = 200, description = "The embeddings, in input order.", body = ProviderAnswer),
            (status = 400, description = "Invalid request.", body = OpenAiError),
            (status = 401, description = "Missing, invalid, disabled or expired key.", body = OpenAiError),
            (status = 403, description = "The key may not use this model.", body = OpenAiError),
            (status = 429, description = "A rate limit or the key's budget.", body = OpenAiError),
        ),
        params(("x-gateway-labels" = Option<String>, Header, description = LABELS_HEADER)),
        security(("virtual_key" = []), ("virtual_key_header" = [])))]
    fn embeddings() {}

    /// Rerank
    ///
    /// Ranks documents by relevance to a query, on Azure AI Foundry (Cohere), Amazon Bedrock and
    /// OpenAI-compatible rerank servers. Also served at `/v2/rerank` and `/rerank`.
    #[utoipa::path(post, path = "/v1/rerank", tag = "Inference", request_body = RerankRequest,
        responses(
            (status = 200, description = "The documents, most relevant first.", body = RerankResponse),
            (status = 400, description = "Invalid request.", body = OpenAiError),
            (status = 401, description = "Missing, invalid, disabled or expired key.", body = OpenAiError),
            (status = 403, description = "The key may not use this model.", body = OpenAiError),
            (status = 429, description = "A rate limit or the key's budget.", body = OpenAiError),
        ),
        params(("x-gateway-labels" = Option<String>, Header, description = LABELS_HEADER)),
        security(("virtual_key" = []), ("virtual_key_header" = [])))]
    fn rerank() {}

    /// Decisions
    ///
    /// Calibrated answers to questions about an input, in the OpenAI Decisions format. Also
    /// served at `/decisions`.
    #[utoipa::path(post, path = "/v1/decisions", tag = "Inference", request_body = DecisionsRequest,
        responses(
            (status = 200, description = "An answer per question.", body = ProviderAnswer),
            (status = 400, description = "Invalid request.", body = OpenAiError),
            (status = 401, description = "Missing, invalid, disabled or expired key.", body = OpenAiError),
            (status = 403, description = "The key may not use this model.", body = OpenAiError),
            (status = 429, description = "A rate limit or the key's budget.", body = OpenAiError),
        ),
        params(("x-gateway-labels" = Option<String>, Header, description = LABELS_HEADER)),
        security(("virtual_key" = []), ("virtual_key_header" = [])))]
    fn decisions() {}

    /// System One
    ///
    /// Decisions in TypeSafe's System One format. Also served at `/systemone`.
    #[utoipa::path(post, path = "/v1/systemone", tag = "Inference", request_body = SystemOneRequest,
        responses(
            (status = 200, description = "The answers.", body = ProviderAnswer),
            (status = 400, description = "Invalid request.", body = OpenAiError),
            (status = 401, description = "Missing, invalid, disabled or expired key.", body = OpenAiError),
            (status = 403, description = "The key may not use this model.", body = OpenAiError),
            (status = 429, description = "A rate limit or the key's budget.", body = OpenAiError),
        ),
        params(("x-gateway-labels" = Option<String>, Header, description = LABELS_HEADER)),
        security(("virtual_key" = []), ("virtual_key_header" = [])))]
    fn systemone() {}

    /// List models
    ///
    /// The models, auto routers and model aliases this key may use. With an `anthropic-version` header the list
    /// is Anthropic-shaped, and also gives each model's `max_input_tokens`, `max_tokens`,
    /// `capabilities` and `lifecycle`, as far as the price catalog and the gateway know them.
    #[utoipa::path(get, path = "/v1/models", tag = "Inference",
        responses(
            (status = 200, description = "The models.", body = ModelList),
            (status = 401, description = "Missing, invalid, disabled or expired key.", body = OpenAiError),
        ),
        security(("virtual_key" = []), ("virtual_key_header" = [])))]
    fn list_models() {}

    /// List models with details
    ///
    /// The models, auto routers and model aliases this key may use, as `GET /v1/models` lists them, with each
    /// model's mode, token limits, structured output support and tags.
    #[utoipa::path(get, path = "/v1/models-extended", tag = "Inference",
        responses(
            (status = 200, description = "The models.", body = ModelExtendedList),
            (status = 401, description = "Missing, invalid, disabled or expired key.", body = OpenAiError),
        ),
        security(("virtual_key" = []), ("virtual_key_header" = [])))]
    fn list_models_extended() {}

    const MCP_SESSION: &str = "The session the server opened, as the gateway handed it out with `initialize`'s answer. It only works \n        with the key that opened it; an unknown one is answered with `404`, after which the client starts a new session.";

    const A2A_STATES: &str = "Each message is an entry in the request log (api `a2a`) with the state its task was left in, counts against \
        the key's limits and budgets, and is charged the agent's price. Tasks belong to the key whose message created them: \
        calls about another key's task are answered as for an unknown task.";

    /// A2A: agent card
    ///
    /// The agent's card as the agent serves it, with the gateway's addresses as its interfaces (the newest \
    /// JSON-RPC and HTTP+JSON ones) and the virtual key as its security.
    #[utoipa::path(get, path = "/a2a/{agent}/.well-known/agent-card.json", tag = "A2A", params(("agent" = String, Path)),
        responses(
            (status = 200, description = "The agent card.", body = Object),
            (status = 401, description = "Missing, invalid, disabled or expired key.", body = OpenAiError),
            (status = 404, description = "No such agent for this key.", body = OpenAiError),
            (status = 502, description = "The agent's card could not be fetched or read.", body = OpenAiError),
        ),
        security(("virtual_key" = []), ("virtual_key_header" = [])))]
    fn a2a_card() {}

    /// A2A: JSON-RPC
    ///
    /// The agent's JSON-RPC interface, in A2A 1.0 (`SendMessage`, …) or 0.3 (`message/send`, …), as the card \
    /// says. Listing tasks and the extended card are not offered.
    #[utoipa::path(post, path = "/a2a/{agent}", tag = "A2A", params(("agent" = String, Path),
            ("x-gateway-labels" = Option<String>, Header, description = LABELS_HEADER)),
        request_body(content = Object, content_type = "application/json"),
        responses(
            (status = 200, description = A2A_STATES, content((Object = "application/json"), (String = "text/event-stream"))),
            (status = 401, description = "Missing, invalid, disabled or expired key.", body = OpenAiError),
            (status = 404, description = "No such agent for this key.", body = OpenAiError),
            (status = 429, description = "A rate limit or the key's budget, for a message; see `Retry-After`.", body = OpenAiError),
            (status = 502, description = "The agent failed, or refused the gateway's credentials.", body = OpenAiError),
        ),
        security(("virtual_key" = []), ("virtual_key_header" = [])))]
    fn a2a_jsonrpc() {}

    /// A2A: HTTP+JSON
    ///
    /// The agent's HTTP+JSON interface: `path` as the A2A specification has it, such as `message:send` or \
    /// `tasks/{id}`.
    #[utoipa::path(post, path = "/a2a/{agent}/rest/{*path}", tag = "A2A", params(("agent" = String, Path), ("*path" = String, Path),
            ("x-gateway-labels" = Option<String>, Header, description = LABELS_HEADER)),
        request_body(content = Object, content_type = "application/json"),
        responses(
            (status = 200, description = A2A_STATES, content((Object = "application/json"), (String = "text/event-stream"))),
            (status = 403, description = "Listing tasks or the extended card, which are not offered.", body = OpenAiError),
            (status = 404, description = "No such agent for this key, or another key's task.", body = OpenAiError),
            (status = 429, description = "A rate limit or the key's budget, for a message; see `Retry-After`.", body = OpenAiError),
            (status = 502, description = "The agent failed, or refused the gateway's credentials.", body = OpenAiError),
        ),
        security(("virtual_key" = []), ("virtual_key_header" = [])))]
    fn a2a_rest_post() {}

    /// A2A: HTTP+JSON, read
    #[utoipa::path(get, path = "/a2a/{agent}/rest/{*path}", tag = "A2A", params(("agent" = String, Path), ("*path" = String, Path)),
        responses(
            (status = 200, description = "The agent's answer.", content((Object = "application/json"), (String = "text/event-stream"))),
            (status = 404, description = "No such agent for this key, or another key's task.", body = OpenAiError),
        ),
        security(("virtual_key" = []), ("virtual_key_header" = [])))]
    fn a2a_rest_get() {}

    /// A2A: HTTP+JSON, delete
    #[utoipa::path(delete, path = "/a2a/{agent}/rest/{*path}", tag = "A2A", params(("agent" = String, Path), ("*path" = String, Path)),
        responses(
            (status = 200, description = "The agent's answer."),
            (status = 404, description = "No such agent for this key, or another key's task.", body = OpenAiError),
        ),
        security(("virtual_key" = []), ("virtual_key_header" = [])))]
    fn a2a_rest_delete() {}

    /// MCP: every server
    ///
    /// One MCP server with the tools of every MCP server the key may use, named \
    /// `{server}__{tool}`, where the server's tool filters allow them. The gateway answers here itself, \
    /// statelessly, in any protocol version, and calls the tool in a session it keeps with the server for \
    /// the key. Each `tools/call` is an entry in the request log (api `mcp`), counts against the key's \
    /// limits and budgets, and is charged the tool's price.
    #[utoipa::path(post, path = "/mcp", tag = "MCP", request_body(content = Object, content_type = "application/json"),
        responses(
            (status = 200, description = "The gateway's answer.", content((Object = "application/json"))),
            (status = 202, description = "Accepted: notifications and responses have no answer."),
            (status = 401, description = "Missing, invalid, disabled or expired key.", body = OpenAiError),
        ),
        params(("x-gateway-labels" = Option<String>, Header, description = LABELS_HEADER)),
        security(("virtual_key" = []), ("virtual_key_header" = [])))]
    fn mcp_hub_post() {}

    /// MCP: every server, listen
    ///
    /// Not offered: the gateway's own MCP server sends nothing unasked.
    #[utoipa::path(get, path = "/mcp", tag = "MCP",
        responses((status = 405, description = "Not offered.")),
        security(("virtual_key" = []), ("virtual_key_header" = [])))]
    fn mcp_hub_get() {}

    /// MCP: every server, end session
    ///
    /// Not needed: the gateway's own MCP server keeps no sessions.
    #[utoipa::path(delete, path = "/mcp", tag = "MCP",
        responses((status = 405, description = "Not needed.")),
        security(("virtual_key" = []), ("virtual_key_header" = [])))]
    fn mcp_hub_delete() {}

    /// MCP: send messages
    ///
    /// The MCP server named `server`, over Streamable HTTP: JSON-RPC messages as the MCP specification \n    /// describes them, forwarded as they are, with the gateway's credentials for the server. The answer is \n    /// JSON or server-sent events, as the server chooses. Each `tools/call` is an entry in the request log \n    /// (api `mcp`) and counts against the key's limits and budgets.
    #[utoipa::path(post, path = "/mcp/{server}", tag = "MCP", request_body(content = Object, content_type = "application/json"),
        responses(
            (status = 200, description = "The server's answer.", content((Object = "application/json"), (String = "text/event-stream"))),
            (status = 202, description = "Accepted: notifications and responses have no answer."),
            (status = 401, description = "Missing, invalid, disabled or expired key.", body = OpenAiError),
            (status = 404, description = "No such server for this key, or an unknown session.", body = OpenAiError),
            (status = 429, description = "A rate limit or the key's budget, for a tool call; see `Retry-After`.", body = OpenAiError),
            (status = 502, description = "The server failed, or refused the gateway's credentials.", body = OpenAiError),
        ),
        params(("server" = String, Path), ("mcp-session-id" = Option<String>, Header, description = MCP_SESSION),
            ("x-gateway-labels" = Option<String>, Header, description = LABELS_HEADER)),
        security(("virtual_key" = []), ("virtual_key_header" = [])))]
    fn mcp_post() {}

    /// MCP: listen
    ///
    /// The server's own messages to the client, as server-sent events, where the server offers them.
    #[utoipa::path(get, path = "/mcp/{server}", tag = "MCP",
        responses(
            (status = 200, description = "The server's messages.", content((String = "text/event-stream"))),
            (status = 405, description = "The server does not offer this stream."),
            (status = 404, description = "No such server for this key, or an unknown session.", body = OpenAiError),
        ),
        params(("server" = String, Path), ("mcp-session-id" = Option<String>, Header, description = MCP_SESSION)),
        security(("virtual_key" = []), ("virtual_key_header" = [])))]
    fn mcp_get() {}

    /// MCP: end session
    #[utoipa::path(delete, path = "/mcp/{server}", tag = "MCP",
        responses(
            (status = 200, description = "The session ended."),
            (status = 404, description = "No such server for this key, or an unknown session.", body = OpenAiError),
        ),
        params(("server" = String, Path), ("mcp-session-id" = Option<String>, Header, description = MCP_SESSION)),
        security(("virtual_key" = []), ("virtual_key_header" = [])))]
    fn mcp_delete() {}

    // ------------------------------------------------------------ health

    /// Liveness
    ///
    /// The process answers.
    #[utoipa::path(get, path = "/health/live", tag = "Health", responses((status = 200, description = "`ok`", body = String)))]
    fn health_live() {}

    /// Readiness
    ///
    /// Postgres answers and the configuration is loaded. Valkey is reported but not required;
    /// without it, limits are not enforced.
    #[utoipa::path(get, path = "/health/ready", tag = "Health",
        responses((status = 200, description = "Ready.", body = Ready), (status = 503, description = "Not ready.", body = Ready)))]
    fn health_ready() {}

    // ------------------------------------------------------------ sign-in

    /// Session
    ///
    /// Who is signed in, and what the sign-in screen offers. Works signed out.
    #[utoipa::path(get, path = "/admin/api/auth/session", tag = "Sign-in", responses((status = 200, description = "The session.", body = SessionInfo)))]
    fn session() {}

    /// Sign out
    ///
    /// Ends the console session.
    #[utoipa::path(post, path = "/admin/api/auth/logout", tag = "Sign-in",
        params(("x-gateway-csrf" = String, Header, description = "Any value.")),
        responses((status = 204, description = "Signed out.")))]
    fn logout() {}

    /// Dev Login
    ///
    /// Local development builds only: signs in without credentials, as an admin or (`role=user`)
    /// as the local development user, whom admins add to projects like anyone else.
    #[utoipa::path(post, path = "/admin/api/auth/dev-login", tag = "Sign-in",
        params(
            ("x-gateway-csrf" = String, Header, description = "Any value."),
            ("role" = Option<String>, Query, description = "`user` for the local development user; an admin otherwise."),
        ),
        responses((status = 204, description = "Signed in."), (status = 404, description = "Not a local development build.")))]
    fn dev_login() {}

    /// Start single sign-on
    ///
    /// Redirects the browser to the identity provider.
    #[utoipa::path(get, path = "/admin/auth/oidc/login", tag = "Sign-in",
        responses((status = 303, description = "To the identity provider, or back to the console with `login_error`.")))]
    fn oidc_login() {}

    /// Single sign-on callback
    ///
    /// Where the identity provider sends the browser back.
    #[utoipa::path(get, path = "/admin/auth/oidc/callback", tag = "Sign-in",
        params(
            ("code" = Option<String>, Query), ("state" = Option<String>, Query),
            ("error" = Option<String>, Query), ("error_description" = Option<String>, Query),
        ),
        responses((status = 303, description = "Into the console, or back to the sign-in screen with `login_error`.")))]
    fn oidc_callback() {}

    /// Upstream sign-in callback
    ///
    /// Where an MCP server's or A2A agent's authorization server sends the browser back after a key \
    /// was signed in. It finishes only in the browser session that started the sign-in.
    #[utoipa::path(get, path = "/admin/auth/oauth/callback", tag = "Sign-in",
        params(
            ("code" = Option<String>, Query), ("state" = Option<String>, Query),
            ("error" = Option<String>, Query), ("error_description" = Option<String>, Query),
        ),
        responses((status = 303, description = "Back to the console's Access page, with `connected` or `connect_error`.")))]
    fn oauth_callback() {}

    /// Discover OAuth
    ///
    /// Finds an MCP server's (RFC 9728) or A2A agent's (its card) authorization server and what its \
    /// metadata says. With `register`, registers the gateway there as a public client (RFC 7591).
    #[utoipa::path(post, path = "/admin/api/oauth/discover", tag = "Upstream OAuth", request_body = admin::oauth::DiscoverInput,
        responses(
            (status = 200, description = "What was found.", body = oauth::Discovered),
            (status = 400, description = "Nothing found, or registration refused.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn oauth_discover() {}

    /// List key's upstream sign-ins
    ///
    /// The MCP servers and A2A agents the key may use whose keys sign in on their own, and whether it has.
    #[utoipa::path(get, path = "/admin/api/keys/{id}/connections", tag = "Upstream OAuth", params(("id" = Uuid, Path)),
        responses(
            (status = 200, description = "The upstreams.", body = Vec<admin::oauth::Connection>),
            (status = 404, description = "No such key.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn key_connections() {}

    /// Sign key in to upstream
    ///
    /// Starts the authorization code flow with PKCE: send the browser to `authorization_url`. The \
    /// upstream's authorization server sends it back to `/admin/auth/oauth/callback`.
    #[utoipa::path(post, path = "/admin/api/keys/{id}/connections/{kind}/{upstream}", tag = "Upstream OAuth",
        params(("id" = Uuid, Path), ("kind" = String, Path, description = "`mcp` or `a2a`."), ("upstream" = Uuid, Path), admin::oauth::ConnectQuery),
        responses(
            (status = 200, description = "Where to send the browser.", body = admin::oauth::ConnectStart),
            (status = 404, description = "No such key, or no such upstream for it that signs keys in.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn key_connect() {}

    /// Sign key out of upstream
    ///
    /// Deletes the key's tokens. Other replicas stop using them within five minutes.
    #[utoipa::path(delete, path = "/admin/api/keys/{id}/connections/{kind}/{upstream}", tag = "Upstream OAuth",
        params(("id" = Uuid, Path), ("kind" = String, Path), ("upstream" = Uuid, Path)),
        responses(
            (status = 204, description = "Signed out."),
            (status = 404, description = "The key had not signed in.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn key_disconnect() {}

    /// Get single sign-on settings
    ///
    /// The client secret is never returned.
    #[utoipa::path(get, path = "/admin/api/settings/oidc", tag = "Sign-in",
        responses((status = 200, description = "The settings.", body = admin_auth::SettingsView)),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn get_oidc_settings() {}

    /// Save single sign-on settings
    #[utoipa::path(put, path = "/admin/api/settings/oidc", tag = "Sign-in", request_body = admin_auth::SettingsInput,
        responses(
            (status = 200, description = "The saved settings.", body = admin_auth::SettingsView),
            (status = 400, description = "Invalid settings.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn put_oidc_settings() {}

    /// Test single sign-on
    ///
    /// Fetches the identity provider's discovery document with the saved settings.
    #[utoipa::path(post, path = "/admin/api/settings/oidc/test", tag = "Sign-in",
        responses((status = 200, description = "Whether discovery worked.", body = OidcTest)),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn test_oidc_settings() {}

    /// List people
    ///
    /// Everyone who has signed in with single sign-on, admins and users, with the projects each
    /// user works in.
    #[utoipa::path(get, path = "/admin/api/admins", tag = "Sign-in",
        responses((status = 200, description = "The people.", body = Vec<admin_auth::PersonView>)),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn list_admins() {}

    /// Remove admin
    ///
    /// Their sessions end on their next request.
    #[utoipa::path(delete, path = "/admin/api/admins/{id}", tag = "Sign-in", params(("id" = Uuid, Path)),
        responses((status = 204, description = "Removed."), (status = 404, description = "No such admin.", body = AdminError)),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn delete_admin() {}

    /// Set a user's projects
    ///
    /// The projects a user works in: there they manage keys and see the playground, the request
    /// log and metrics, and nothing else. Admins see every project.
    #[utoipa::path(put, path = "/admin/api/admins/{id}/projects", tag = "Sign-in", params(("id" = Uuid, Path)),
        request_body = admin_auth::PersonProjects,
        responses(
            (status = 200, description = "The user with their projects.", body = admin_auth::PersonView),
            (status = 400, description = "Not a user, or an unknown project.", body = AdminError),
            (status = 404, description = "No such person.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn set_admin_projects() {}

    // ------------------------------------------------------------ credentials

    /// List credentials
    ///
    /// With which secrets are set; never their values.
    #[utoipa::path(get, path = "/admin/api/credentials", tag = "Credentials", params(admin::CredentialQuery),
        responses((status = 200, description = "The credentials.", body = Vec<admin::CredentialView>)),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn list_credentials() {}

    /// Create credential
    ///
    /// Secrets are encrypted and never returned.
    #[utoipa::path(post, path = "/admin/api/credentials", tag = "Credentials", request_body = admin::CredentialInput,
        responses(
            (status = 201, description = "The new credential.", body = admin::CredentialView),
            (status = 400, description = "Invalid credential.", body = AdminError),
            (status = 409, description = "The name is taken.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn create_credential() {}

    /// Update credential
    ///
    /// Omitted secrets keep their stored value; empty strings clear them.
    #[utoipa::path(put, path = "/admin/api/credentials/{id}", tag = "Credentials", params(("id" = Uuid, Path)),
        request_body = admin::CredentialInput,
        responses(
            (status = 200, description = "The credential.", body = admin::CredentialView),
            (status = 400, description = "Invalid credential.", body = AdminError),
            (status = 404, description = "No such credential.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn update_credential() {}

    /// Delete credential
    ///
    /// Only when no deployment uses it.
    #[utoipa::path(delete, path = "/admin/api/credentials/{id}", tag = "Credentials", params(("id" = Uuid, Path)),
        responses(
            (status = 204, description = "Deleted."),
            (status = 404, description = "No such credential.", body = AdminError),
            (status = 409, description = "Deployments use it.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn delete_credential() {}

    /// List Foundry deployments
    ///
    /// The deployments on an Azure credential's resource, with their model and the rate limits of
    /// their quota, read from Azure Resource Manager. Only for credentials that use Entra ID; the
    /// app needs read access to the resource (Cognitive Services User includes it).
    #[utoipa::path(get, path = "/admin/api/credentials/{id}/foundry-deployments", tag = "Credentials", params(("id" = Uuid, Path)),
        responses(
            (status = 200, description = "The resource and its deployments.", body = crate::entra::FoundryDeployments),
            (status = 400, description = "Not an Azure credential with Entra ID.", body = AdminError),
            (status = 404, description = "No such credential.", body = AdminError),
            (status = 502, description = "Entra ID or Azure Resource Manager refused or failed.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn foundry_deployments() {}

    /// List Bedrock models
    ///
    /// The foundation models and inference profiles a Bedrock credential can call in its region,
    /// with the per-minute request and token quotas of its account, read with the AWS SDK. Needs
    /// `bedrock:ListFoundationModels`, `bedrock:ListInferenceProfiles` and
    /// `servicequotas:ListServiceQuotas`; a listing the credential may not read is described in
    /// `models_error` or `quotas_error` instead of failing the request.
    #[utoipa::path(get, path = "/admin/api/credentials/{id}/bedrock-models", tag = "Credentials", params(("id" = Uuid, Path)),
        responses(
            (status = 200, description = "The models, and what could not be read.", body = crate::aws::models::BedrockModels),
            (status = 400, description = "Not a Bedrock credential, or one with a Bedrock API key.", body = AdminError),
            (status = 404, description = "No such credential.", body = AdminError),
            (status = 502, description = "No AWS region or credentials could be found.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn bedrock_models() {}

    /// List AWS regions
    ///
    /// The regions of the AWS SDK this gateway was built with (commercial, European Sovereign
    /// Cloud, GovCloud and China), with their names, for a Bedrock credential's region. Empty if
    /// the build could not read them.
    #[utoipa::path(get, path = "/admin/api/aws-regions", tag = "Credentials",
        responses((status = 200, description = "The regions.", body = Vec<crate::aws::AwsRegion>)),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn aws_regions() {}

    // ------------------------------------------------------------ A2A agents

    /// List A2A agents
    #[utoipa::path(get, path = "/admin/api/a2a-agents", tag = "A2A",
        responses((status = 200, description = "The A2A agents.", body = Vec<admin::a2a::A2aAgentView>)),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn list_a2a_agents() {}

    /// Create A2A agent
    ///
    /// An agent, found by its card, proxied at `/a2a/{name}`.
    #[utoipa::path(post, path = "/admin/api/a2a-agents", tag = "A2A", request_body = admin::a2a::A2aAgentInput,
        responses(
            (status = 201, description = "The new A2A agent.", body = admin::a2a::A2aAgentView),
            (status = 400, description = "Invalid A2A agent.", body = AdminError),
            (status = 409, description = "The name is taken.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn create_a2a_agent() {}

    /// Update A2A agent
    ///
    /// Without a secret, the stored secret stays.
    #[utoipa::path(put, path = "/admin/api/a2a-agents/{id}", tag = "A2A", params(("id" = Uuid, Path)),
        request_body = admin::a2a::A2aAgentInput,
        responses(
            (status = 200, description = "The A2A agent.", body = admin::a2a::A2aAgentView),
            (status = 400, description = "Invalid A2A agent.", body = AdminError),
            (status = 404, description = "No such A2A agent.", body = AdminError),
            (status = 409, description = "The name is taken.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn update_a2a_agent() {}

    /// Delete A2A agent
    #[utoipa::path(delete, path = "/admin/api/a2a-agents/{id}", tag = "A2A", params(("id" = Uuid, Path)),
        responses(
            (status = 204, description = "Deleted."),
            (status = 404, description = "No such A2A agent.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn delete_a2a_agent() {}

    /// Test A2A agent
    ///
    /// Fetches the agent's card, also while the agent is disabled, and says what the gateway proxies of it. \
    /// No message is sent.
    #[utoipa::path(post, path = "/admin/api/a2a-agents/{id}/test", tag = "A2A", params(("id" = Uuid, Path)),
        responses(
            (status = 200, description = "The card, or why it could not be used with `ok: false`.", body = admin::a2a::A2aAgentTestResult),
            (status = 404, description = "No such A2A agent.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn test_a2a_agent() {}

    // ------------------------------------------------------------ MCP servers

    /// List MCP servers
    #[utoipa::path(get, path = "/admin/api/mcp-servers", tag = "MCP",
        responses((status = 200, description = "The MCP servers.", body = Vec<admin::mcp::McpServerView>)),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn list_mcp_servers() {}

    /// Create MCP server
    ///
    /// An upstream MCP server, proxied at `/mcp/{name}`.
    #[utoipa::path(post, path = "/admin/api/mcp-servers", tag = "MCP", request_body = admin::mcp::McpServerInput,
        responses(
            (status = 201, description = "The new MCP server.", body = admin::mcp::McpServerView),
            (status = 400, description = "Invalid MCP server.", body = AdminError),
            (status = 409, description = "The name is taken.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn create_mcp_server() {}

    /// Update MCP server
    ///
    /// Without a secret, the stored secret stays.
    #[utoipa::path(put, path = "/admin/api/mcp-servers/{id}", tag = "MCP", params(("id" = Uuid, Path)),
        request_body = admin::mcp::McpServerInput,
        responses(
            (status = 200, description = "The MCP server.", body = admin::mcp::McpServerView),
            (status = 400, description = "Invalid MCP server.", body = AdminError),
            (status = 404, description = "No such MCP server.", body = AdminError),
            (status = 409, description = "The name is taken.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn update_mcp_server() {}

    /// Delete MCP server
    #[utoipa::path(delete, path = "/admin/api/mcp-servers/{id}", tag = "MCP", params(("id" = Uuid, Path)),
        responses(
            (status = 204, description = "Deleted."),
            (status = 404, description = "No such MCP server.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn delete_mcp_server() {}

    /// Test MCP server
    ///
    /// Connects as a client would, also while the server is disabled, and lists its tools. Nothing is \n    /// logged or charged to a key.
    #[utoipa::path(post, path = "/admin/api/mcp-servers/{id}/test", tag = "MCP", params(("id" = Uuid, Path)),
        responses(
            (status = 200, description = "The server's tools, or why it could not be used with `ok: false`.", body = admin::mcp::McpServerTestResult),
            (status = 404, description = "No such MCP server.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn test_mcp_server() {}

    // ------------------------------------------------------------ guardrails

    /// List guardrails
    ///
    /// With the projects and keys that apply each one to every request.
    #[utoipa::path(get, path = "/admin/api/guardrails", tag = "Guardrails",
        responses((status = 200, description = "The guardrails.", body = Vec<admin::guardrails::GuardrailListItem>)),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn list_guardrails() {}

    /// Create guardrail
    ///
    /// Refers to a guardrail defined at the provider, called with one of its credentials. Without
    /// prices, the provider's list prices.
    #[utoipa::path(post, path = "/admin/api/guardrails", tag = "Guardrails", request_body = admin::guardrails::GuardrailInput,
        responses(
            (status = 201, description = "The new guardrail.", body = admin::guardrails::GuardrailView),
            (status = 400, description = "Invalid guardrail.", body = AdminError),
            (status = 409, description = "The name is taken.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn create_guardrail() {}

    /// Update guardrail
    ///
    /// Without prices, the stored prices stay.
    #[utoipa::path(put, path = "/admin/api/guardrails/{id}", tag = "Guardrails", params(("id" = Uuid, Path)),
        request_body = admin::guardrails::GuardrailInput,
        responses(
            (status = 200, description = "The guardrail.", body = admin::guardrails::GuardrailView),
            (status = 400, description = "Invalid guardrail.", body = AdminError),
            (status = 404, description = "No such guardrail.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn update_guardrail() {}

    /// Delete guardrail
    #[utoipa::path(delete, path = "/admin/api/guardrails/{id}", tag = "Guardrails", params(("id" = Uuid, Path)),
        responses(
            (status = 204, description = "Deleted."),
            (status = 404, description = "No such guardrail.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn delete_guardrail() {}

    /// Test guardrail
    ///
    /// Checks a text as a prompt or a response: what the guardrail found, the text as it may
    /// pass, and the billed units and cost. The provider bills the check; nothing is stored.
    #[utoipa::path(post, path = "/admin/api/guardrails/{id}/test", tag = "Guardrails", params(("id" = Uuid, Path)),
        request_body = admin::guardrails::GuardrailTest,
        responses(
            (status = 200, description = "The outcome, or the provider's error with `ok: false`.", body = admin::guardrails::GuardrailTestResult),
            (status = 404, description = "No such guardrail.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn test_guardrail() {}

    /// Guardrail list prices
    ///
    /// Each provider's list prices, USD per 1,000 text units for each kind of policy.
    #[utoipa::path(get, path = "/admin/api/guardrails/prices", tag = "Guardrails",
        responses((status = 200, description = "Prices by provider.", body = Object)),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn guardrail_prices() {}

    // ------------------------------------------------------------ deployments

    /// List deployments
    ///
    /// With their cooldowns.
    #[utoipa::path(get, path = "/admin/api/deployments", tag = "Deployments", params(admin::DeploymentQuery),
        responses(
            (status = 200, description = "The deployments.", body = Vec<admin::DeploymentView>),
            (status = 400, description = "An unknown mode, or a filter that is not ids.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn list_deployments() {}

    /// Create deployment
    #[utoipa::path(post, path = "/admin/api/deployments", tag = "Deployments", request_body = admin::DeploymentInput,
        responses(
            (status = 201, description = "The new deployment.", body = admin::DeploymentView),
            (status = 400, description = "Invalid deployment.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn create_deployment() {}

    /// Get deployment
    #[utoipa::path(get, path = "/admin/api/deployments/{id}", tag = "Deployments", params(("id" = Uuid, Path)),
        responses(
            (status = 200, description = "The deployment.", body = admin::DeploymentView),
            (status = 404, description = "No such deployment.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn get_deployment() {}

    /// Update deployment
    ///
    /// Its mode cannot change; a renamed model is followed by routers and keys.
    #[utoipa::path(put, path = "/admin/api/deployments/{id}", tag = "Deployments", params(("id" = Uuid, Path)),
        request_body = admin::DeploymentInput,
        responses(
            (status = 200, description = "The deployment.", body = admin::DeploymentView),
            (status = 400, description = "Invalid deployment.", body = AdminError),
            (status = 404, description = "No such deployment.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn update_deployment() {}

    /// Delete deployment
    #[utoipa::path(delete, path = "/admin/api/deployments/{id}", tag = "Deployments", params(("id" = Uuid, Path)),
        responses((status = 204, description = "Deleted."), (status = 404, description = "No such deployment.", body = AdminError)),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn delete_deployment() {}

    /// Test deployment
    ///
    /// Sends a tiny request through the deployment, bypassing routing.
    #[utoipa::path(post, path = "/admin/api/deployments/{id}/test", tag = "Deployments", params(("id" = Uuid, Path)),
        responses(
            (status = 200, description = "The result.", body = DeploymentTest),
            (status = 400, description = "The deployment is disabled or misconfigured.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn test_deployment() {}

    /// Clear cooldown
    ///
    /// Puts a cooling-down deployment back into rotation.
    #[utoipa::path(post, path = "/admin/api/deployments/{id}/clear-cooldown", tag = "Deployments", params(("id" = Uuid, Path)),
        responses((status = 204, description = "Cleared.")),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn clear_cooldown() {}

    // ------------------------------------------------------------ auto routers

    /// List auto routers
    ///
    /// With the models each names that no deployment serves.
    #[utoipa::path(get, path = "/admin/api/routers", tag = "Auto routers", params(admin::RouterQuery),
        responses((status = 200, description = "The routers.", body = Vec<admin::RouterListItem>)),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn list_routers() {}

    /// Create auto router
    #[utoipa::path(post, path = "/admin/api/routers", tag = "Auto routers", request_body = admin::RouterInput,
        responses(
            (status = 201, description = "The new router.", body = admin::RouterView),
            (status = 400, description = "Invalid router.", body = AdminError),
            (status = 409, description = "The name is taken.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn create_router() {}

    /// Update auto router
    #[utoipa::path(put, path = "/admin/api/routers/{id}", tag = "Auto routers", params(("id" = Uuid, Path)),
        request_body = admin::RouterInput,
        responses(
            (status = 200, description = "The router.", body = admin::RouterView),
            (status = 400, description = "Invalid router.", body = AdminError),
            (status = 404, description = "No such router.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn update_router() {}

    /// Delete auto router
    #[utoipa::path(delete, path = "/admin/api/routers/{id}", tag = "Auto routers", params(("id" = Uuid, Path)),
        responses((status = 204, description = "Deleted."), (status = 404, description = "No such router.", body = AdminError)),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn delete_router() {}

    // ------------------------------------------------------------ model aliases

    /// List model aliases
    #[utoipa::path(get, path = "/admin/api/aliases", tag = "Model aliases", params(admin::AliasQuery),
        responses((status = 200, description = "The aliases.", body = Vec<admin::AliasView>)),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn list_aliases() {}

    /// Create model alias
    ///
    /// A model name that serves the model of a deployment or auto router, named by its id so the
    /// alias follows a rename. Keys reach it by its own name in `allowed_models`, and by their tags
    /// through the target's deployments or router.
    #[utoipa::path(post, path = "/admin/api/aliases", tag = "Model aliases", request_body = admin::AliasInput,
        responses(
            (status = 201, description = "The new alias.", body = admin::AliasView),
            (status = 400, description = "Invalid alias, or the model name is taken.", body = AdminError),
            (status = 409, description = "An alias with that name exists.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn create_alias() {}

    /// Update model alias
    ///
    /// A renamed alias is followed by keys and its model metadata.
    #[utoipa::path(put, path = "/admin/api/aliases/{id}", tag = "Model aliases", params(("id" = Uuid, Path)),
        request_body = admin::AliasInput,
        responses(
            (status = 200, description = "The alias.", body = admin::AliasView),
            (status = 400, description = "Invalid alias, or the model name is taken.", body = AdminError),
            (status = 404, description = "No such alias.", body = AdminError),
            (status = 409, description = "Another alias has that name.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn update_alias() {}

    /// Delete model alias
    #[utoipa::path(delete, path = "/admin/api/aliases/{id}", tag = "Model aliases", params(("id" = Uuid, Path)),
        responses((status = 204, description = "Deleted."), (status = 404, description = "No such alias.", body = AdminError)),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn delete_alias() {}

    // ------------------------------------------------------------ virtual keys

    /// List virtual keys
    ///
    /// With their current budget period.
    #[utoipa::path(get, path = "/admin/api/keys", tag = "Virtual keys", params(admin::KeyQuery),
        responses(
            (status = 200, description = "The keys.", body = Vec<admin::KeyView>),
            (status = 400, description = "A project filter that is not ids.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn list_keys() {}

    /// Key names
    ///
    /// The keys a picker offers, by name: id, name and project, without settings or budgets.
    #[utoipa::path(get, path = "/admin/api/keys/options", tag = "Virtual keys", params(admin::KeyOptionsQuery),
        responses((status = 200, description = "The keys.", body = Vec<db::NamedItem>)),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn key_options() {}

    /// Create virtual key
    ///
    /// The answer holds its secret, this once; the gateway keeps only a hash.
    #[utoipa::path(post, path = "/admin/api/keys", tag = "Virtual keys", request_body = admin::KeyInput,
        responses(
            (status = 201, description = "The new key and its secret.", body = KeyCreated),
            (status = 400, description = "Invalid key.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn create_key() {}

    /// Update virtual key
    #[utoipa::path(put, path = "/admin/api/keys/{id}", tag = "Virtual keys", params(("id" = Uuid, Path)),
        request_body = admin::KeyInput,
        responses(
            (status = 200, description = "The key.", body = model::VirtualKey),
            (status = 400, description = "Invalid key.", body = AdminError),
            (status = 404, description = "No such key.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn update_key() {}

    /// Delete virtual key
    #[utoipa::path(delete, path = "/admin/api/keys/{id}", tag = "Virtual keys", params(("id" = Uuid, Path)),
        responses((status = 204, description = "Deleted."), (status = 404, description = "No such key.", body = AdminError)),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn delete_key() {}

    /// List projects
    ///
    /// Projects group keys in the console. An inactive project (`enabled: false`) blocks its keys,
    /// and its budget, if set, comes on top of theirs. By name, with key counts and budget status.
    #[utoipa::path(get, path = "/admin/api/projects", tag = "Virtual keys", params(admin::ProjectQuery),
        responses((status = 200, description = "The projects.", body = Vec<admin::ProjectView>)),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn list_projects() {}

    /// Project names
    ///
    /// The projects a picker offers, by name: id and name only.
    #[utoipa::path(get, path = "/admin/api/projects/options", tag = "Virtual keys",
        responses((status = 200, description = "The projects.", body = Vec<db::NamedItem>)),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn project_options() {}

    /// Create project
    #[utoipa::path(post, path = "/admin/api/projects", tag = "Virtual keys", request_body = admin::ProjectInput,
        responses(
            (status = 201, description = "Created.", body = admin::ProjectView),
            (status = 400, description = "Invalid project.", body = AdminError),
            (status = 409, description = "A project with this name exists.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn create_project() {}

    /// Update project
    #[utoipa::path(put, path = "/admin/api/projects/{id}", tag = "Virtual keys", params(("id" = Uuid, Path)),
        request_body = admin::ProjectInput,
        responses(
            (status = 200, description = "The project.", body = admin::ProjectView),
            (status = 400, description = "Invalid project.", body = AdminError),
            (status = 404, description = "No such project.", body = AdminError),
            (status = 409, description = "A project with this name exists.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn update_project() {}

    /// Delete project
    ///
    /// Its keys are deleted with it. Refused (`409`) while deployments are limited to it.
    #[utoipa::path(delete, path = "/admin/api/projects/{id}", tag = "Virtual keys", params(("id" = Uuid, Path)),
        responses((status = 204, description = "Deleted."), (status = 404, description = "No such project.", body = AdminError)),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn delete_project() {}

    /// Reset project spend
    ///
    /// Starts the project's current budget period over. Its keys' own budgets are not affected.
    #[utoipa::path(post, path = "/admin/api/projects/{id}/reset-spend", tag = "Virtual keys", params(("id" = Uuid, Path)),
        responses((status = 204, description = "Reset."), (status = 404, description = "No such project.", body = AdminError)),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn reset_project_spend() {}

    /// Reset spend
    ///
    /// Starts the key's current budget period over.
    #[utoipa::path(post, path = "/admin/api/keys/{id}/reset-spend", tag = "Virtual keys", params(("id" = Uuid, Path)),
        responses((status = 204, description = "Reset.")),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn reset_spend() {}

    /// Regenerate virtual key
    ///
    /// Gives the key a new secret. It keeps its settings, spend and history.
    #[utoipa::path(post, path = "/admin/api/keys/{id}/regenerate", tag = "Virtual keys", params(("id" = Uuid, Path)),
        responses(
            (status = 200, description = "The new secret.", body = KeyRegenerated),
            (status = 404, description = "No such key.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn regenerate_key() {}

    // ------------------------------------------------------------ configuration

    /// List tags
    ///
    /// Every tag on deployments, routers and MCP servers, and those keys still hold, with how many
    /// of each carry it.
    #[utoipa::path(get, path = "/admin/api/tags", tag = "Configuration",
        responses((status = 200, description = "The tags.", body = Vec<TagUsage>)),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn list_tags() {}

    // ------------------------------------------------------------ model hub

    /// Model hub
    ///
    /// Every model the gateway serves, or with `key` those that key may use: token limits,
    /// capabilities and prices, per model and per deployment that may serve it, each model's
    /// metadata, and the guardrails the key's requests go through. `mode`, `vendor` and `q` narrow
    /// the models as the console's filters do. Besides admins, a project's users and keys may read
    /// the hub of their project's keys, and a key whose scope is the inference API its own.
    #[utoipa::path(get, path = "/admin/api/hub", tag = "Model hub", params(admin::hub::HubQuery),
        responses(
            (status = 200, description = "The models.", body = admin::hub::HubView),
            (status = 400, description = "No such key, or an unknown mode.", body = AdminError),
            (status = 403, description = "A key whose scope is the inference API asked for another key.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("virtual_key" = []), ("admin_session" = [])))]
    fn model_hub() {}

    /// Reasoning efforts
    ///
    /// The reasoning efforts each chat model takes, by its price catalog entry: those every one of
    /// its deployments accepts. Aliases are listed with their target's; auto routers, which set
    /// their own per route, are left out. For everyone signed in to the console.
    #[utoipa::path(get, path = "/admin/api/efforts", tag = "Model hub",
        responses((status = 200, description = "Model name to its efforts, in order.", body = std::collections::BTreeMap<String, Vec<crate::model::ReasoningEffort>>)),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn model_efforts() {}

    /// Set model metadata
    ///
    /// Replaces a model's metadata, such as `display_name` and `description`; `{}` removes it.
    #[utoipa::path(put, path = "/admin/api/model-metadata", tag = "Model hub", request_body = admin::hub::MetadataInput,
        responses(
            (status = 200, description = "The stored metadata.", body = admin::hub::MetadataView),
            (status = 400, description = "Invalid metadata, or no such model.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn set_model_metadata() {}

    /// Export configuration
    ///
    /// Credentials come without their secrets.
    #[utoipa::path(get, path = "/admin/api/config/export", tag = "Configuration",
        responses((status = 200, description = "The configuration.", body = admin::transfer::Document)),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn export_config() {}

    /// Import configuration
    ///
    /// Merges a configuration file into this gateway, in one transaction. Entries update what has
    /// their id, or else what matches them by name; renames carry over to routers, keys and model
    /// metadata.
    #[utoipa::path(post, path = "/admin/api/config/import", tag = "Configuration", params(admin::transfer::ImportQuery),
        request_body = admin::transfer::Document,
        responses(
            (status = 200, description = "What changed, or would change with `dry_run`.", body = ImportResult),
            (status = 400, description = "The file is invalid or does not fit this gateway.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn import_config() {}

    // ------------------------------------------------------------ usage

    /// Status
    ///
    /// This replica.
    #[utoipa::path(get, path = "/admin/api/status", tag = "Usage",
        responses((status = 200, description = "The replica.", body = Status)),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn status() {}

    /// Metrics
    ///
    /// Requests, errors, tokens, latency and cost over time, and totals by model.
    #[utoipa::path(get, path = "/admin/api/metrics", tag = "Usage", params(admin::RangeQuery),
        responses(
            (status = 200, description = "Totals and series for the range.", body = Object),
            (status = 400, description = "Invalid range.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn metrics() {}

    /// Request log
    ///
    /// Newest first. Routed requests come with the calls they made.
    #[utoipa::path(get, path = "/admin/api/logs", tag = "Usage", params(admin::LogQuery),
        responses((status = 200, description = "Log entries.", body = Vec<db::LogRow>)),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn logs() {}

    /// Request labels in use
    ///
    /// The label keys of the latest 1000 labelled requests, each with up to 50 of its values,
    /// most used first. The console suggests them in its label filters.
    #[utoipa::path(get, path = "/admin/api/logs/labels", tag = "Usage",
        responses((status = 200, description = "Label keys, by name.", body = Vec<admin::LabelKey>)),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn log_labels() {}

    /// Tools in logged responses
    ///
    /// The tools the latest 1000 logged responses called, at most 100, most used first. The
    /// console offers them in its tool filter.
    #[utoipa::path(get, path = "/admin/api/logs/tools", tag = "Usage",
        responses((status = 200, description = "Tools, most used first.", body = Vec<db::ToolUse>)),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn log_tools() {}

    /// A logged request's prompt and response
    ///
    /// For keys with prompt logging on: the request as sent and as the model got it, what each
    /// guardrail did, the response, and the turns before and after it.
    #[utoipa::path(get, path = "/admin/api/logs/{id}/bodies", tag = "Usage",
        params(("id" = Uuid, Path, description = "Request log entry id")),
        responses(
            (status = 200, description = "The stored bodies.", body = admin::BodiesView),
            (status = 404, description = "No stored bodies, or not the caller's project.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn log_bodies() {}

    /// Count a key's stored prompts
    #[utoipa::path(get, path = "/admin/api/keys/{id}/bodies", tag = "Virtual keys",
        params(("id" = Uuid, Path, description = "Key id")),
        responses((status = 200, description = "How many are stored.", body = admin::StoredBodies), (status = 404, description = "No such key.", body = AdminError)),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn key_bodies() {}

    /// Delete a key's stored prompts
    ///
    /// Deletes the key's logged prompts and responses now, rather than at the end of their
    /// retention. Their entries stay in the request log, marked `deleted`.
    #[utoipa::path(delete, path = "/admin/api/keys/{id}/bodies", tag = "Virtual keys",
        params(("id" = Uuid, Path, description = "Key id")),
        responses(
            (status = 200, description = "How many were deleted.", body = admin::DeletedBodies),
            (status = 404, description = "No such key.", body = AdminError),
            (status = 409, description = "Already being deleted.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn delete_key_bodies() {}

    /// Count a project's stored prompts
    #[utoipa::path(get, path = "/admin/api/projects/{id}/bodies", tag = "Virtual keys",
        params(("id" = Uuid, Path, description = "Project id")),
        responses((status = 200, description = "How many are stored.", body = admin::StoredBodies), (status = 404, description = "No such project.", body = AdminError)),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn project_bodies() {}

    /// Delete a project's stored prompts
    ///
    /// Deletes the logged prompts and responses of every request made in the project, also by
    /// keys that have left it since.
    #[utoipa::path(delete, path = "/admin/api/projects/{id}/bodies", tag = "Virtual keys",
        params(("id" = Uuid, Path, description = "Project id")),
        responses(
            (status = 200, description = "How many were deleted.", body = admin::DeletedBodies),
            (status = 404, description = "No such project.", body = AdminError),
            (status = 409, description = "Already being deleted.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn delete_project_bodies() {}

    /// Audit log
    ///
    /// Newest first: who created, changed or deleted credentials, deployments, routers and keys,
    /// imported configuration or prices, changed single sign-on, and signed in or out.
    #[utoipa::path(get, path = "/admin/api/audit", tag = "Usage", params(admin::AuditQuery),
        responses(
            (status = 200, description = "Audit log entries.", body = Vec<db::AuditEntry>),
            (status = 400, description = "Invalid range.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn audit_log() {}

    /// Usage by key
    ///
    /// Spend by key, top 50.
    #[utoipa::path(get, path = "/admin/api/usage/keys", tag = "Usage",
        params(("range" = Option<String>, Query, description = "`1h`, `24h` (default), `7d` or `30d`.")),
        responses((status = 200, description = "Usage per key.", body = Vec<db::KeyUsageRow>)),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn key_usage() {}

    /// Cluster
    ///
    /// Every replica, and the health of Postgres and Valkey.
    #[utoipa::path(get, path = "/admin/api/cluster", tag = "Usage",
        responses((status = 200, description = "The cluster.", body = Object)),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn cluster() {}

    // ------------------------------------------------------------ settings

    /// Get gateway settings
    ///
    /// The settings every replica applies, and their defaults.
    #[utoipa::path(get, path = "/admin/api/settings", tag = "Settings",
        responses((status = 200, description = "The settings.", body = admin::SettingsView)),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn get_settings() {}

    /// Save gateway settings
    ///
    /// Replaces all settings (missing fields take their defaults) and applies them on every
    /// replica: at once for most, for new client connections for the timeouts and the body limit.
    #[utoipa::path(put, path = "/admin/api/settings", tag = "Settings", request_body = crate::settings::Settings,
        responses(
            (status = 200, description = "The saved settings.", body = admin::SettingsView),
            (status = 400, description = "A setting is out of range.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn put_settings() {}

    // ------------------------------------------------------------ playground

    /// Playground
    ///
    /// Runs an inference request as a chosen virtual key, with its limits, budget and logging.
    /// `api` is `chat`, `responses`, `messages`, `embeddings`, `rerank`, `decisions` or `systemone`; the
    /// body is that endpoint's. `models` and `models-extended` list the key's models as
    /// `GET /v1/models` and `GET /v1/models-extended` do; their body is ignored.
    #[utoipa::path(post, path = "/admin/api/playground/{api}", tag = "Playground",
        params(("api" = String, Path), admin::PlaygroundQuery),
        request_body = Object,
        responses((status = 200, description = "The endpoint's answer.", body = ProviderAnswer)),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn playground() {}

    /// Playground: an A2A agent
    ///
    /// A JSON-RPC call of an A2A agent as a chosen virtual key, as at `/a2a/{agent}`: logged,
    /// counted against the key's limits and charged the agent's price.
    #[utoipa::path(post, path = "/admin/api/playground/a2a/{agent}", tag = "Playground",
        params(("agent" = String, Path), admin::PlaygroundQuery),
        request_body = Object,
        responses((status = 200, description = "The agent's answer, or its event stream.", body = Object)),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn playground_a2a() {}

    /// Playground: an A2A agent's card
    ///
    /// The agent's card as a chosen virtual key gets it at `/a2a/{agent}/.well-known/agent-card.json`.
    #[utoipa::path(get, path = "/admin/api/playground/a2a/{agent}/card", tag = "Playground",
        params(("agent" = String, Path), admin::PlaygroundQuery),
        responses((status = 200, description = "The agent card, with the gateway's addresses.", body = Object)),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn playground_a2a_card() {}

    /// Playground options
    ///
    /// The models a key may call, with their modes, the guardrails a request may ask for (those
    /// the key or its project enforce marked), and the MCP servers and A2A agents it may use.
    #[utoipa::path(get, path = "/admin/api/playground/options", tag = "Playground", params(admin::PlaygroundQuery),
        responses(
            (status = 200, description = "The key's models, guardrails, MCP servers and A2A agents.", body = admin::PlaygroundOptions),
            (status = 404, description = "No such key, or not one of the caller's.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn playground_options() {}

    // ------------------------------------------------------------ price catalog

    /// Search prices
    #[utoipa::path(get, path = "/admin/api/prices", tag = "Price catalog", params(crate::prices::SearchQuery),
        responses((status = 200, description = "Matching entries, with the filters' values.", body = Object)),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn search_prices() {}

    /// Match price
    ///
    /// The catalog entry for a provider's model, and similar ones.
    #[utoipa::path(get, path = "/admin/api/prices/match", tag = "Price catalog", params(crate::prices::MatchQuery),
        responses((status = 200, description = "`exact` and `similar` entries.", body = Object)),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn match_price() {}

    /// Check prices
    ///
    /// Every deployment with its catalog entry, to compare prices.
    #[utoipa::path(get, path = "/admin/api/prices/check", tag = "Price catalog",
        responses((status = 200, description = "One entry per deployment.", body = Object)),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn check_prices() {}

    /// Import prices
    ///
    /// Replaces the catalog with LiteLLM's current price list. The gateway also does this on its
    /// own, as often as the `price_import_hours` setting says, unless `price_import` is off.
    #[utoipa::path(post, path = "/admin/api/prices/import", tag = "Price catalog",
        responses(
            (status = 200, description = "What changed.", body = db::PriceImport),
            (status = 400, description = "The download failed.", body = AdminError),
            (status = 409, description = "An import is running already.", body = AdminError),
        ),
        security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
    fn import_prices() {}

    // Types that appear only inside others, named so the derive is not reported unused.
    fn _referenced(_: budget::BudgetStatus) {}
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    /// The routes the gateway registers, as `METHOD path`, read from the router definitions.
    fn served_routes() -> BTreeSet<String> {
        // Where each file's routes are mounted (see `main.rs`).
        let sources: [(&str, &str, &str); 4] = [
            ("", include_str!("main.rs"), ""),
            ("/admin/api", include_str!("admin.rs"), ""),
            ("/admin/api", include_str!("prices.rs"), ""),
            ("/admin/api", include_str!("admin_auth.rs"), "pub fn browser_routes"),
        ];
        let mut out = BTreeSet::new();
        for (prefix, src, split) in sources {
            // In admin_auth.rs, the routes after `browser_routes` are mounted at /admin/auth.
            let parts = match src.split_once(split).filter(|_| !split.is_empty()) {
                Some((api, browser)) => vec![(prefix, api), ("/admin/auth", browser)],
                None => vec![(prefix, src)],
            };
            for (prefix, text) in parts {
                for chunk in text.split(".route(\"").skip(1) {
                    let (path, rest) = chunk.split_once('"').expect("route path");
                    // The handler expression runs until the next builder call.
                    let handlers = rest.split([';', '\n']).next().unwrap_or_default();
                    let handlers = handlers.split(".route(").next().unwrap_or_default();
                    for method in ["get", "post", "put", "delete", "patch"] {
                        if handlers.contains(&format!("{method}(")) {
                            out.insert(format!("{} {prefix}{path}", method.to_uppercase()));
                        }
                    }
                }
            }
        }
        out
    }

    fn documented_routes() -> BTreeSet<String> {
        let doc: serde_json::Value = serde_json::from_str(&DOCUMENT).unwrap();
        let mut out = BTreeSet::new();
        for (path, ops) in doc["paths"].as_object().unwrap() {
            for method in ops.as_object().unwrap().keys() {
                out.insert(format!("{} {path}", method.to_uppercase()));
            }
        }
        out
    }

    /// Every route the gateway serves is documented, and nothing else is.
    #[test]
    fn document_matches_the_routes() {
        let served = served_routes();
        assert!(served.len() > 40, "route discovery found only {served:?}");
        // Aliases of documented `/v1/…` endpoints, the documentation itself, and the redirect to it
        // in debug builds.
        let undocumented: BTreeSet<String> =
            ["POST /decisions", "POST /systemone", "POST /embeddings", "POST /rerank", "POST /v2/rerank", "GET /openapi.json", "GET /"]
                .map(String::from)
                .into();
        let expected: BTreeSet<String> = served.difference(&undocumented).cloned().collect();
        let documented = documented_routes();
        let missing: Vec<_> = expected.difference(&documented).collect();
        let extra: Vec<_> = documented.difference(&expected).collect();
        assert!(missing.is_empty() && extra.is_empty(), "not documented: {missing:?}\ndocumented but not served: {extra:?}");
    }

    #[test]
    fn document_is_complete() {
        let doc: serde_json::Value = serde_json::from_str(&DOCUMENT).unwrap();
        assert_eq!(doc["info"]["version"], crate::VERSION);
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
        // Secrets are write-only: no response schema exposes a credential's or key's secret.
        assert!(!schemas["CredentialView"]["properties"].as_object().unwrap().contains_key("secrets"));
        assert!(!schemas["VirtualKey"]["properties"].as_object().unwrap().contains_key("key_hash"));
    }
}
