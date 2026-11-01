//! Guardrails: a provider's service that screens text, masking what must not leave (PII) and
//! blocking what must not pass (prompt attacks, denied topics, harmful content). Amazon Bedrock
//! Guardrails, through its standalone `ApplyGuardrail` API, which works on any text and so for any
//! model's prompts and responses; Mistral's moderation API, which scores texts by category and
//! blocks or flags, but cannot mask; and Google Cloud Model Armor, which blocks and, with a
//! de-identify template, masks.

use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

use axum::http::{HeaderMap, HeaderValue};
use chrono::{DateTime, Utc};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    error::GatewayError,
    model::{CredentialSettings, Secrets},
    state::AppState,
};

/// The header in which a request asks for guardrails by name, comma-separated, on top of those its
/// key or project always gets.
pub const HEADER: &str = "x-gateway-guardrails";

/// How long one check may take.
const TIMEOUT: Duration = Duration::from_secs(10);

const PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.').remove(b'~');

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum GuardrailProvider {
    Bedrock,
    Mistral,
    ModelArmor,
}

impl GuardrailProvider {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "bedrock" => Some(Self::Bedrock),
            "mistral" => Some(Self::Mistral),
            "model_armor" => Some(Self::ModelArmor),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bedrock => "bedrock",
            Self::Mistral => "mistral",
            Self::ModelArmor => "model_armor",
        }
    }

    /// The provider of the credentials it is called with.
    pub fn credential_provider(self) -> &'static str {
        match self {
            Self::Bedrock => "bedrock",
            Self::Mistral => "mistral",
            Self::ModelArmor => "vertex",
        }
    }
}

/// Model Armor's filters, as its responses name them.
pub const MODEL_ARMOR_FILTERS: &[&str] = &["pi_and_jailbreak", "rai", "malicious_uris", "csam", "sdp"];

/// The categories Mistral's moderation model scores.
pub const MISTRAL_CATEGORIES: &[&str] = &[
    "sexual",
    "hate_and_discrimination",
    "violence_and_threats",
    "dangerous",
    "criminal",
    "selfharm",
    "health",
    "financial",
    "law",
    "pii",
    "jailbreaking",
];

/// The moderation model a Mistral guardrail uses unless it names one.
pub const MISTRAL_MODEL: &str = "mistral-moderation-latest";

/// Which guardrail at the provider to call. Stored as JSONB.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GuardrailConfig {
    /// Bedrock: the guardrail's id or ARN. With an ARN the guardrail is called in the ARN's
    /// region, otherwise in the credential's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guardrail_identifier: Option<String>,
    /// Bedrock: a version number, or `DRAFT` for the working draft.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guardrail_version: Option<String>,
    /// Mistral: the moderation model, `mistral-moderation-latest` by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Mistral: the categories that block a request (see `MISTRAL_CATEGORIES`); Model Armor: the
    /// filters that do (see `MODEL_ARMOR_FILTERS`). Others that find something flag it, and the
    /// request goes on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub block: Vec<String>,
    /// Mistral: the score (0 to 1) from which a category counts as found; Mistral's own decision
    /// when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<f64>,
    /// Model Armor: the template's id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    /// Model Armor: the template's location, such as `europe-west4` or `eu`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    /// Model Armor: the template's project; the credential's, or its service account's, by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
}

/// USD per 1,000 text units (up to 1,000 characters each) for each kind of policy, as the
/// provider bills them; images per image. Stored as JSONB.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct GuardrailPrices {
    /// Content filters, prompt attacks included.
    pub content: f64,
    /// Content filters on images, per image.
    pub content_image: f64,
    /// Denied topics.
    pub topic: f64,
    /// Sensitive information (PII). Regular expressions are free.
    pub sensitive_information: f64,
    /// Word filters.
    pub word: f64,
    /// Contextual grounding.
    pub contextual_grounding: f64,
    /// Automated Reasoning, per policy.
    pub automated_reasoning: f64,
    /// Mistral and Model Armor: per million input tokens (Model Armor's estimated from the text).
    pub tokens: f64,
}

impl Default for GuardrailPrices {
    /// Amazon Bedrock Guardrails' list prices, as of October 2026.
    fn default() -> Self {
        Self {
            content: 0.15,
            content_image: 0.00075,
            topic: 0.15,
            sensitive_information: 0.10,
            word: 0.0,
            contextual_grounding: 0.10,
            automated_reasoning: 0.17,
            tokens: 0.0,
        }
    }
}

impl GuardrailPrices {
    /// The provider's list prices: Bedrock's as of October 2026; Mistral's moderation model and
    /// Model Armor (beyond its free monthly allowance) per million input tokens.
    pub fn list(provider: GuardrailProvider) -> Self {
        match provider {
            GuardrailProvider::Bedrock => Self::default(),
            GuardrailProvider::Mistral | GuardrailProvider::ModelArmor => Self {
                content: 0.0,
                content_image: 0.0,
                topic: 0.0,
                sensitive_information: 0.0,
                word: 0.0,
                contextual_grounding: 0.0,
                automated_reasoning: 0.0,
                tokens: 0.10,
            },
        }
    }

    pub fn is_valid(&self) -> bool {
        [
            self.content,
            self.content_image,
            self.topic,
            self.sensitive_information,
            self.word,
            self.contextual_grounding,
            self.automated_reasoning,
            self.tokens,
        ]
        .iter()
        .all(|p| p.is_finite() && *p >= 0.0)
    }

    pub fn cost(&self, u: &GuardrailUsage) -> f64 {
        let per_k = |units: u64, price: f64| units as f64 * price / 1000.0;
        per_k(u.content, self.content)
            + u.content_images as f64 * self.content_image
            + per_k(u.topic, self.topic)
            + per_k(u.sensitive_information, self.sensitive_information)
            + per_k(u.word, self.word)
            + per_k(u.contextual_grounding, self.contextual_grounding)
            + per_k(u.automated_reasoning, self.automated_reasoning)
            + u.tokens as f64 * self.tokens / 1e6
    }
}

/// A guardrail as stored.
#[derive(Debug, Clone)]
pub struct GuardrailRow {
    pub id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub provider: String,
    pub credential_id: Uuid,
    pub config: sqlx::types::Json<GuardrailConfig>,
    pub prices: sqlx::types::Json<GuardrailPrices>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A guardrail ready to call: with its credential's settings and secrets, decrypted once.
pub struct Active {
    pub row: GuardrailRow,
    pub settings: CredentialSettings,
    pub secrets: Secrets,
}

impl Active {
    pub fn callable(&self) -> Callable<'_> {
        Callable { row: &self.row, settings: &self.settings, secrets: &self.secrets }
    }
}

/// The guardrails a request goes through, in name order: those set on its key and on the key's
/// project (`enforced`), and those it asks for in `HEADER`. A name that is not a guardrail is
/// refused rather than skipped, as the client expects its text to be screened; so is a request
/// whose key or project must go through a guardrail that did not load (see `registry`).
pub fn for_request(
    all: &BTreeMap<String, Arc<Active>>,
    enforced: &[Uuid],
    requested: Option<&str>,
) -> Result<Vec<Arc<Active>>, GatewayError> {
    let mut picked: BTreeMap<&str, Arc<Active>> = BTreeMap::new();
    for id in enforced {
        let g = all
            .values()
            .find(|g| g.row.id == *id)
            .ok_or_else(|| GatewayError::unavailable("a guardrail this key must go through is misconfigured; see the Guardrails page"))?;
        picked.insert(g.row.name.as_str(), g.clone());
    }
    for name in requested.unwrap_or_default().split(',').map(str::trim).filter(|n| !n.is_empty()) {
        let g = all.get(name).ok_or_else(|| GatewayError::invalid(format!("unknown guardrail '{name}' in {HEADER}")))?;
        picked.insert(g.row.name.as_str(), g.clone());
    }
    Ok(picked.into_values().collect())
}

/// Whose text is checked: the prompt, or the model's response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Input,
    Output,
}

/// What the guardrail did, the strongest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// Nothing found.
    Passed,
    /// Found something its policy only detects.
    Flagged,
    /// Replaced what it found, such as a name with `{NAME}`.
    Masked,
    /// Refused the text.
    Blocked,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Flagged => "flagged",
            Self::Masked => "masked",
            Self::Blocked => "blocked",
        }
    }
}

/// One thing a policy found.
#[derive(Debug, Clone, PartialEq, Serialize, utoipa::ToSchema)]
pub struct Finding {
    /// `content`, `topic`, `word`, `pii`, `regex` or `grounding` (Bedrock), `moderation` (Mistral),
    /// or Model Armor's filter (see `MODEL_ARMOR_FILTERS`).
    pub policy: &'static str,
    /// The filter, entity, topic or regex: `PROMPT_ATTACK`, `NAME`, an `Investments` topic, …
    pub kind: String,
    /// What the policy did with it: `BLOCKED`, `ANONYMIZED` or `NONE` (detected only).
    pub action: String,
    /// The text that matched, where the provider reports it. Never in the request log; only the
    /// bodies of logged requests keep it (see `capture`).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub matched: Option<String>,
    /// The filter's confidence (`LOW` to `HIGH`) or the grounding score.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub confidence: Option<String>,
}

/// Billed text units for each kind of policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, utoipa::ToSchema)]
pub struct GuardrailUsage {
    pub content: u64,
    pub content_images: u64,
    pub topic: u64,
    pub sensitive_information: u64,
    pub word: u64,
    pub contextual_grounding: u64,
    pub automated_reasoning: u64,
    /// Mistral: input tokens; Model Armor: estimated from the text's length.
    pub tokens: u64,
}

/// The outcome of one check.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct Verdict {
    pub action: Action,
    /// The texts as they may pass, one for each text checked: masked where something was masked.
    /// When blocked, the guardrail's message instead, once.
    pub outputs: Vec<String>,
    pub findings: Vec<Finding>,
    pub usage: GuardrailUsage,
    pub cost_usd: f64,
    /// The provider's answer as it came, for logged requests (see `api::guard::screen_prompt`).
    #[serde(skip)]
    pub raw: Option<Value>,
}

/// A guardrail with what it needs to be called: its credential's settings and secrets.
pub struct Callable<'a> {
    pub row: &'a GuardrailRow,
    pub settings: &'a CredentialSettings,
    pub secrets: &'a Secrets,
}

/// Checks `texts` (each one a block, such as one message) as the prompt or the response.
pub async fn check(state: &AppState, g: &Callable<'_>, source: Source, texts: &[String]) -> Result<Verdict, GatewayError> {
    match GuardrailProvider::parse(&g.row.provider) {
        Some(GuardrailProvider::Bedrock) => bedrock(state, g, source, texts).await,
        Some(GuardrailProvider::Mistral) => mistral(state, g, texts).await,
        Some(GuardrailProvider::ModelArmor) => model_armor(state, g, source, texts).await,
        None => Err(GatewayError::internal(format!("unknown guardrail provider {}", g.row.provider))),
    }
}

/// The region an ARN names, if `identifier` is one: `arn:aws:bedrock:<region>:<account>:guardrail/<id>`.
pub fn arn_region(identifier: &str) -> Option<&str> {
    identifier.strip_prefix("arn:").and_then(|rest| rest.split(':').nth(2)).filter(|r| !r.is_empty())
}

/// Bedrock's ApplyGuardrail, through the SDK. A guardrail named by ARN is called in the ARN's
/// region, others in the credential's.
async fn bedrock(state: &AppState, g: &Callable<'_>, source: Source, texts: &[String]) -> Result<Verdict, GatewayError> {
    use aws_sdk_bedrockruntime::types as bt;
    let missing = || GatewayError::internal("guardrail has no Bedrock guardrail identifier or version");
    let identifier = g.row.config.guardrail_identifier.as_deref().ok_or_else(missing)?;
    let version = g.row.config.guardrail_version.as_deref().ok_or_else(missing)?;
    let mut settings = g.settings.clone();
    if let Some(r) = arn_region(identifier) {
        settings.region = Some(r.to_string());
    }
    let build = |e: aws_smithy_types::error::operation::BuildError| GatewayError::internal(format!("guardrail request: {e}"));
    let content = texts
        .iter()
        .map(|t| bt::GuardrailTextBlock::builder().text(t).build().map(bt::GuardrailContentBlock::Text))
        .collect::<Result<Vec<_>, _>>()
        .map_err(build)?;
    let client = state.aws.runtime_for(state, &settings, g.secrets, g.settings.base_url.clone()).await?;
    let call = client
        .apply_guardrail()
        .guardrail_identifier(identifier)
        .guardrail_version(version)
        .source(match source {
            Source::Input => bt::GuardrailContentSource::Input,
            Source::Output => bt::GuardrailContentSource::Output,
        })
        .set_content(Some(content))
        // Every block's output, masked or not, so they map back one to one.
        .output_scope(bt::GuardrailOutputScope::Full)
        .send();
    let started = Instant::now();
    let out = match tokio::time::timeout(TIMEOUT, call).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            let e = crate::aws::sdk_error(e);
            tracing::warn!(guardrail = %g.row.id, error = %e.message, "guardrail error");
            return Err(e);
        }
        Err(_) => return Err(GatewayError { upstream: true, ..GatewayError::timeout("guardrail timed out") }),
    };
    tracing::debug!(guardrail = %g.row.id, ms = started.elapsed().as_millis() as u64, "guardrail checked");
    Ok(parse_bedrock(&apply_guardrail_json(&out), &g.row.prices))
}

/// An ApplyGuardrail answer as Bedrock's JSON, the fields `parse_bedrock` reads.
fn apply_guardrail_json(out: &aws_sdk_bedrockruntime::operation::apply_guardrail::ApplyGuardrailOutput) -> Value {
    let assessments: Vec<Value> = out
        .assessments()
        .iter()
        .map(|a| {
            let mut v = serde_json::Map::new();
            if let Some(p) = a.content_policy() {
                let filters: Vec<Value> = p
                    .filters()
                    .iter()
                    .map(|f| json!({ "type": f.r#type().as_str(), "action": f.action().as_str(), "detected": f.detected(), "confidence": f.confidence().as_str() }))
                    .collect();
                v.insert("contentPolicy".into(), json!({ "filters": filters }));
            }
            if let Some(p) = a.topic_policy() {
                let topics: Vec<Value> = p
                    .topics()
                    .iter()
                    .map(|t| json!({ "name": t.name(), "type": t.r#type().as_str(), "action": t.action().as_str(), "detected": t.detected() }))
                    .collect();
                v.insert("topicPolicy".into(), json!({ "topics": topics }));
            }
            if let Some(p) = a.word_policy() {
                let custom: Vec<Value> =
                    p.custom_words().iter().map(|w| json!({ "match": w.r#match(), "action": w.action().as_str(), "detected": w.detected() })).collect();
                let managed: Vec<Value> = p
                    .managed_word_lists()
                    .iter()
                    .map(|w| json!({ "match": w.r#match(), "type": w.r#type().as_str(), "action": w.action().as_str(), "detected": w.detected() }))
                    .collect();
                v.insert("wordPolicy".into(), json!({ "customWords": custom, "managedWordLists": managed }));
            }
            if let Some(p) = a.sensitive_information_policy() {
                let pii: Vec<Value> = p
                    .pii_entities()
                    .iter()
                    .map(|e| json!({ "match": e.r#match(), "type": e.r#type().as_str(), "action": e.action().as_str(), "detected": e.detected() }))
                    .collect();
                let regexes: Vec<Value> = p
                    .regexes()
                    .iter()
                    .map(|r| json!({ "name": r.name(), "match": r.r#match(), "action": r.action().as_str(), "detected": r.detected() }))
                    .collect();
                v.insert("sensitiveInformationPolicy".into(), json!({ "piiEntities": pii, "regexes": regexes }));
            }
            if let Some(p) = a.contextual_grounding_policy() {
                let filters: Vec<Value> = p
                    .filters()
                    .iter()
                    .map(|f| json!({ "type": f.r#type().as_str(), "action": f.action().as_str(), "detected": f.detected(), "score": f.score() }))
                    .collect();
                v.insert("contextualGroundingPolicy".into(), json!({ "filters": filters }));
            }
            Value::Object(v)
        })
        .collect();
    let usage = out.usage().map(|u| {
        json!({
            "contentPolicyUnits": u.content_policy_units(),
            "contentPolicyImageUnits": u.content_policy_image_units().unwrap_or(0),
            "topicPolicyUnits": u.topic_policy_units(),
            "sensitiveInformationPolicyUnits": u.sensitive_information_policy_units(),
            "wordPolicyUnits": u.word_policy_units(),
            "contextualGroundingPolicyUnits": u.contextual_grounding_policy_units(),
            "automatedReasoningPolicyUnits": u.automated_reasoning_policy_units().unwrap_or(0),
        })
    });
    json!({
        "action": out.action().as_str(),
        "outputs": out.outputs().iter().map(|o| json!({ "text": o.text() })).collect::<Vec<_>>(),
        "assessments": assessments,
        "usage": usage,
    })
}

/// Posts a guardrail request and returns the JSON it answers.
async fn send(state: &AppState, g: &Callable<'_>, url: &str, headers: HeaderMap, body: Vec<u8>) -> Result<Value, GatewayError> {
    let started = Instant::now();
    let resp =
        state.http.post(url).headers(headers).timeout(TIMEOUT).body(body).send().await.map_err(|e| GatewayError::from_reqwest(&e))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| GatewayError::from_reqwest(&e))?;
    if !status.is_success() {
        tracing::warn!(guardrail = %g.row.id, status = status.as_u16(), body = %crate::util::truncate(&text, 500), "guardrail error");
        return Err(GatewayError::from_upstream(status, &text));
    }
    tracing::debug!(guardrail = %g.row.id, ms = started.elapsed().as_millis() as u64, "guardrail checked");
    serde_json::from_str(&text).map_err(|_| GatewayError::internal("guardrail answered with invalid JSON"))
}

/// Mistral's moderation API: every text scored in one call, the same for prompts and responses.
async fn mistral(state: &AppState, g: &Callable<'_>, texts: &[String]) -> Result<Verdict, GatewayError> {
    let key = g.secrets.api_key.as_deref().ok_or_else(|| GatewayError::internal("the guardrail's Mistral credential has no API key"))?;
    let base = g.settings.base_url.clone().unwrap_or_else(|| crate::providers::mistral_base(g.settings.region.as_deref()));
    let url = format!("{}/moderations", base.trim_end_matches('/'));
    if let Err(why) = state.cfg.upstream_policy.check_url(&url) {
        return Err(GatewayError::internal(format!("guardrail address not allowed: {why}")));
    }
    let model = g.row.config.model.as_deref().unwrap_or(MISTRAL_MODEL);
    let body = serde_json::to_vec(&json!({ "model": model, "input": texts })).map_err(|e| GatewayError::internal(e.to_string()))?;
    let mut headers = HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    let mut auth = HeaderValue::from_str(&format!("Bearer {key}")).map_err(|_| GatewayError::internal("invalid Mistral API key"))?;
    auth.set_sensitive(true);
    headers.insert("authorization", auth);
    let v = send(state, g, &url, headers, body).await?;
    Ok(parse_mistral(&v, &g.row.config, &g.row.prices))
}

/// Google Cloud Model Armor: one call per text, all at once, with the guardrail's template.
async fn model_armor(state: &AppState, g: &Callable<'_>, source: Source, texts: &[String]) -> Result<Verdict, GatewayError> {
    let missing = || GatewayError::internal("guardrail has no Model Armor template or location");
    let template = g.row.config.template.as_deref().ok_or_else(missing)?;
    let location = g.row.config.location.as_deref().ok_or_else(missing)?;
    let project = match g.row.config.project_id.clone() {
        Some(p) => p,
        None => crate::google::project(g.settings, g.secrets)
            .map_err(|_| GatewayError::internal("no Google Cloud project for the Model Armor template"))?,
    };
    let seg = |s: &str| utf8_percent_encode(s, PATH_SEGMENT).to_string();
    let (method, field) = match source {
        Source::Input => ("sanitizeUserPrompt", "userPromptData"),
        Source::Output => ("sanitizeModelResponse", "modelResponseData"),
    };
    let url = format!(
        "https://modelarmor.{loc}.rep.googleapis.com/v1/projects/{}/locations/{loc}/templates/{}:{method}",
        seg(&project),
        seg(template),
        loc = seg(location),
    );
    if let Err(why) = state.cfg.upstream_policy.check_url(&url) {
        return Err(GatewayError::internal(format!("guardrail address not allowed: {why}")));
    }
    let token = state.google.access_token(g.settings, g.secrets).await?;
    let mut auth = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| GatewayError::internal("invalid Google access token"))?;
    auth.set_sensitive(true);
    let calls = texts.iter().map(|t| {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        headers.insert("authorization", auth.clone());
        let body = serde_json::to_vec(&json!({ field: { "text": t } })).unwrap_or_default();
        send(state, g, &url, headers, body)
    });
    let answers = futures::future::try_join_all(calls).await?;
    parse_model_armor(&answers, texts, &g.row.config, &g.row.prices)
}

/// Reads Model Armor's answers, one for each text: a filter that matched blocks when the guardrail
/// blocks on it and is flagged otherwise; sensitive data a de-identify template rewrote is masked
/// instead. Model Armor reports no usage, so tokens are estimated from the texts' length.
fn parse_model_armor(
    answers: &[Value],
    texts: &[String],
    cfg: &GuardrailConfig,
    prices: &GuardrailPrices,
) -> Result<Verdict, GatewayError> {
    let mut findings: Vec<Finding> = Vec::new();
    let mut outputs: Vec<String> = Vec::with_capacity(texts.len());
    let mut add = |f: Finding| {
        if !findings.iter().any(|x| x.policy == f.policy && x.kind == f.kind) {
            findings.push(f);
        }
    };
    let matched = |v: &Value| v["matchState"] == "MATCH_FOUND";
    let action = |filter: &str| if cfg.block.iter().any(|b| b == filter) { "BLOCKED" } else { "NONE" }.to_string();
    for (answer, text) in answers.iter().zip(texts) {
        let r = &answer["sanitizationResult"];
        if r.is_null() {
            return Err(GatewayError::internal("Model Armor answered without a sanitization result"));
        }
        if r["invocationResult"].as_str().is_some_and(|i| i != "SUCCESS") {
            return Err(GatewayError::unavailable(format!("Model Armor could not run every filter ({})", r["invocationResult"])));
        }
        let f = &r["filterResults"];
        let rai = &f["rai"]["raiFilterResult"];
        for (kind, t) in rai["raiFilterTypeResults"].as_object().into_iter().flatten() {
            if matched(t) {
                add(Finding {
                    policy: "rai",
                    kind: kind.clone(),
                    action: action("rai"),
                    matched: None,
                    confidence: t["confidenceLevel"].as_str().map(str::to_string),
                });
            }
        }
        let pi = &f["pi_and_jailbreak"]["piAndJailbreakFilterResult"];
        if matched(pi) {
            let confidence = pi["confidenceLevel"].as_str().map(str::to_string);
            add(Finding {
                policy: "pi_and_jailbreak",
                kind: "prompt injection or jailbreak".into(),
                action: action("pi_and_jailbreak"),
                matched: None,
                confidence,
            });
        }
        let uris = &f["malicious_uris"]["maliciousUriFilterResult"];
        if matched(uris) {
            for u in uris["maliciousUriMatchedItems"].as_array().into_iter().flatten() {
                let uri = u["uri"].as_str().map(str::to_string);
                add(Finding {
                    policy: "malicious_uris",
                    kind: "malicious URI".into(),
                    action: action("malicious_uris"),
                    matched: uri,
                    confidence: None,
                });
            }
            add(Finding {
                policy: "malicious_uris",
                kind: "malicious URI".into(),
                action: action("malicious_uris"),
                matched: None,
                confidence: None,
            });
        }
        if matched(&f["csam"]["csamFilterFilterResult"]) {
            add(Finding {
                policy: "csam",
                kind: "child sexual abuse material".into(),
                action: action("csam"),
                matched: None,
                confidence: None,
            });
        }
        let sdp = &f["sdp"]["sdpFilterResult"];
        let deidentified = &sdp["deidentifyResult"];
        let masked = deidentified["text"].as_str().or_else(|| deidentified["data"]["text"].as_str()).filter(|_| matched(deidentified));
        if let Some(m) = masked {
            for t in deidentified["infoTypes"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                add(Finding { policy: "sdp", kind: t.to_string(), action: "ANONYMIZED".into(), matched: None, confidence: None });
            }
            outputs.push(m.to_string());
        } else {
            let inspect = &sdp["inspectResult"];
            if matched(inspect) {
                let found: Vec<(String, Option<String>)> = inspect["findings"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|x| Some((x["infoType"].as_str()?.to_string(), x["likelihood"].as_str().map(str::to_string))))
                    .collect();
                for (kind, confidence) in if found.is_empty() { vec![("sensitive data".to_string(), None)] } else { found } {
                    add(Finding { policy: "sdp", kind, action: action("sdp"), matched: None, confidence });
                }
            }
            outputs.push(text.clone());
        }
    }
    let blocked: Vec<&str> = {
        let mut b: Vec<&str> = findings.iter().filter(|f| f.action == "BLOCKED").map(|f| f.policy).collect();
        b.dedup();
        b
    };
    let action = if !blocked.is_empty() {
        Action::Blocked
    } else if findings.iter().any(|f| f.action == "ANONYMIZED") {
        Action::Masked
    } else if findings.is_empty() {
        Action::Passed
    } else {
        Action::Flagged
    };
    let outputs = match action {
        Action::Blocked => vec![format!("The request was blocked by Model Armor ({}).", blocked.join(", "))],
        Action::Masked => outputs,
        Action::Passed | Action::Flagged => vec![],
    };
    // About four characters to a token, as Google counts for billing.
    let chars: usize = texts.iter().map(|t| t.chars().count()).sum();
    let usage = GuardrailUsage { tokens: chars.div_ceil(4) as u64, ..Default::default() };
    Ok(Verdict { action, outputs, findings, cost_usd: prices.cost(&usage), usage, raw: Some(Value::Array(answers.to_vec())) })
}

/// Reads a moderation response: a category found in any text blocks when the guardrail blocks on
/// it, and is flagged otherwise. Each category is reported once, with its highest score.
fn parse_mistral(v: &Value, cfg: &GuardrailConfig, prices: &GuardrailPrices) -> Verdict {
    let mut found: BTreeMap<String, f64> = BTreeMap::new();
    for r in v["results"].as_array().into_iter().flatten() {
        for (category, score) in r["category_scores"].as_object().into_iter().flatten() {
            let score = score.as_f64().unwrap_or(0.0);
            let hit = match cfg.threshold {
                Some(t) => score >= t,
                None => r["categories"][category].as_bool().unwrap_or(false),
            };
            if hit {
                let best = found.entry(category.clone()).or_insert(score);
                *best = best.max(score);
            }
        }
    }
    let findings: Vec<Finding> = found
        .iter()
        .map(|(category, score)| Finding {
            policy: "moderation",
            kind: category.clone(),
            action: if cfg.block.contains(category) { "BLOCKED" } else { "NONE" }.into(),
            matched: None,
            confidence: Some(format!("{score:.2}")),
        })
        .collect();
    let blocked: Vec<&str> = findings.iter().filter(|f| f.action == "BLOCKED").map(|f| f.kind.as_str()).collect();
    let action = if !blocked.is_empty() {
        Action::Blocked
    } else if findings.is_empty() {
        Action::Passed
    } else {
        Action::Flagged
    };
    let outputs =
        if blocked.is_empty() { vec![] } else { vec![format!("The request was blocked by moderation ({}).", blocked.join(", "))] };
    let usage = GuardrailUsage { tokens: v["usage"]["prompt_tokens"].as_u64().unwrap_or(0), ..Default::default() };
    Verdict { action, outputs, findings, cost_usd: prices.cost(&usage), usage, raw: Some(v.clone()) }
}

/// Reads an `ApplyGuardrail` response.
fn parse_bedrock(v: &Value, prices: &GuardrailPrices) -> Verdict {
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    let mut findings = Vec::new();
    for a in v["assessments"].as_array().into_iter().flatten() {
        // Each policy lists its findings under its own keys and names them differently.
        let groups: [(&'static str, &str, &str, &str); 7] = [
            ("content", "contentPolicy", "filters", "type"),
            ("topic", "topicPolicy", "topics", "name"),
            ("word", "wordPolicy", "customWords", "match"),
            ("word", "wordPolicy", "managedWordLists", "type"),
            ("pii", "sensitiveInformationPolicy", "piiEntities", "type"),
            ("regex", "sensitiveInformationPolicy", "regexes", "name"),
            ("grounding", "contextualGroundingPolicy", "filters", "type"),
        ];
        for (policy, key, list, kind) in groups {
            for f in a[key][list].as_array().into_iter().flatten() {
                let action = s(f, "action").unwrap_or_else(|| "NONE".into());
                let detected = f.get("detected").and_then(Value::as_bool).unwrap_or(action != "NONE");
                if !detected {
                    continue;
                }
                findings.push(Finding {
                    policy,
                    kind: s(f, kind).unwrap_or_default(),
                    action,
                    matched: s(f, "match").filter(|m| !m.is_empty()),
                    confidence: s(f, "confidence").or_else(|| f.get("score").and_then(Value::as_f64).map(|x| format!("{x:.2}"))),
                });
            }
        }
    }
    let action = if findings.iter().any(|f| f.action == "BLOCKED") {
        Action::Blocked
    } else if findings.iter().any(|f| f.action == "ANONYMIZED") {
        Action::Masked
    } else if v["action"] == "GUARDRAIL_INTERVENED" {
        // Intervened without saying how, such as an Automated Reasoning finding.
        Action::Blocked
    } else if findings.is_empty() {
        Action::Passed
    } else {
        Action::Flagged
    };
    let n = |k: &str| v["usage"][k].as_u64().unwrap_or(0);
    let usage = GuardrailUsage {
        content: n("contentPolicyUnits"),
        content_images: n("contentPolicyImageUnits"),
        topic: n("topicPolicyUnits"),
        sensitive_information: n("sensitiveInformationPolicyUnits"),
        word: n("wordPolicyUnits"),
        contextual_grounding: n("contextualGroundingPolicyUnits"),
        automated_reasoning: n("automatedReasoningPolicyUnits"),
        tokens: 0,
    };
    let outputs = v["outputs"].as_array().into_iter().flatten().filter_map(|o| s(o, "text")).collect();
    Verdict { action, outputs, findings, cost_usd: prices.cost(&usage), usage, raw: Some(v.clone()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_masking_response() {
        // As Bedrock answered for a guardrail that masks names and phone numbers.
        let v = json!({
            "usage": { "topicPolicyUnits": 0, "contentPolicyUnits": 1, "wordPolicyUnits": 0, "sensitiveInformationPolicyUnits": 1,
                       "sensitiveInformationPolicyFreeUnits": 0, "contextualGroundingPolicyUnits": 0, "contentPolicyImageUnits": 0 },
            "action": "GUARDRAIL_INTERVENED", "actionReason": "Guardrail masked.",
            "outputs": [{ "text": "Call {NAME} at {PHONE}." }, { "text": "Nothing personal here." }],
            "assessments": [{
                "contentPolicy": { "filters": [{ "type": "PROMPT_ATTACK", "confidence": "NONE", "filterStrength": "HIGH", "action": "NONE", "detected": false }] },
                "sensitiveInformationPolicy": { "piiEntities": [
                    { "match": "Jane Doe", "type": "NAME", "action": "ANONYMIZED", "detected": true },
                    { "match": "+49 151 2223334", "type": "PHONE", "action": "ANONYMIZED", "detected": true },
                    { "match": "", "type": "ADDRESS", "action": "NONE", "detected": false }
                ], "regexes": [] }
            }]
        });
        let r = parse_bedrock(&v, &GuardrailPrices::default());
        assert_eq!(r.action, Action::Masked);
        assert_eq!(r.outputs, ["Call {NAME} at {PHONE}.", "Nothing personal here."]);
        assert_eq!(r.findings.iter().map(|f| (f.policy, f.kind.as_str())).collect::<Vec<_>>(), [("pii", "NAME"), ("pii", "PHONE")]);
        assert_eq!(r.usage, GuardrailUsage { content: 1, sensitive_information: 1, ..Default::default() });
        assert!((r.cost_usd - 0.00025).abs() < 1e-12);
    }

    #[test]
    fn reads_a_blocking_response() {
        let v = json!({
            "usage": { "contentPolicyUnits": 1, "sensitiveInformationPolicyUnits": 1 },
            "action": "GUARDRAIL_INTERVENED", "actionReason": "Guardrail blocked.",
            "outputs": [{ "text": "Sorry, the model cannot answer this question." }],
            "assessments": [{ "contentPolicy": { "filters": [
                { "type": "PROMPT_ATTACK", "confidence": "HIGH", "filterStrength": "HIGH", "action": "BLOCKED", "detected": true }
            ] } }]
        });
        let r = parse_bedrock(&v, &GuardrailPrices::default());
        assert_eq!(r.action, Action::Blocked);
        assert_eq!(r.outputs, ["Sorry, the model cannot answer this question."]);
        assert_eq!(r.findings[0].confidence.as_deref(), Some("HIGH"));
    }

    #[test]
    fn detect_only_findings_flag() {
        let v = json!({ "action": "NONE", "outputs": [], "assessments": [{ "topicPolicy": { "topics": [
            { "name": "Investments", "type": "DENY", "action": "NONE", "detected": true }
        ] } }] });
        let r = parse_bedrock(&v, &GuardrailPrices::default());
        assert_eq!(r.action, Action::Flagged);
        assert_eq!(r.findings[0].kind, "Investments");
        assert_eq!(parse_bedrock(&json!({ "action": "NONE", "outputs": [] }), &GuardrailPrices::default()).action, Action::Passed);
    }

    #[test]
    fn reads_a_moderation_response() {
        // As Mistral answered for a phone number, and for a question about building a bomb.
        let v = json!({
            "usage": { "prompt_tokens": 179 },
            "results": [
                { "categories": { "pii": true, "dangerous": false }, "category_scores": { "pii": 0.999, "dangerous": 0.00002 } },
                { "categories": { "pii": false, "dangerous": true, "criminal": true }, "category_scores": { "pii": 0.0005, "dangerous": 0.9986, "criminal": 0.976 } },
            ]
        });
        let prices = GuardrailPrices::list(GuardrailProvider::Mistral);
        let block = |cats: &[&str]| GuardrailConfig { block: cats.iter().map(|c| c.to_string()).collect(), ..Default::default() };

        let r = parse_mistral(&v, &block(&["dangerous"]), &prices);
        assert_eq!(r.action, Action::Blocked);
        assert_eq!(r.outputs, ["The request was blocked by moderation (dangerous)."]);
        assert_eq!(
            r.findings.iter().map(|f| (f.kind.as_str(), f.action.as_str())).collect::<Vec<_>>(),
            [("criminal", "NONE"), ("dangerous", "BLOCKED"), ("pii", "NONE")]
        );
        assert_eq!(r.findings[2].confidence.as_deref(), Some("1.00"));
        assert!((r.cost_usd - 179.0 * 0.10 / 1e6).abs() < 1e-15);

        // Blocking on nothing only flags; a threshold replaces Mistral's own decision.
        assert_eq!(parse_mistral(&v, &block(&[]), &prices).action, Action::Flagged);
        let strict = GuardrailConfig { threshold: Some(0.99), ..block(&["criminal"]) };
        assert_eq!(parse_mistral(&v, &strict, &prices).findings.iter().map(|f| f.kind.as_str()).collect::<Vec<_>>(), ["dangerous", "pii"]);
    }

    #[test]
    fn reads_model_armor_answers() {
        // As Google documents them: a prompt matching the dangerous and prompt-injection filters,
        // and one whose IP address a de-identify template rewrote.
        let blocked = json!({ "sanitizationResult": { "filterMatchState": "MATCH_FOUND", "invocationResult": "SUCCESS", "filterResults": {
            "csam": { "csamFilterFilterResult": { "executionState": "EXECUTION_SUCCESS", "matchState": "NO_MATCH_FOUND" } },
            "rai": { "raiFilterResult": { "matchState": "MATCH_FOUND", "raiFilterTypeResults": {
                "hate_speech": { "matchState": "NO_MATCH_FOUND" },
                "dangerous": { "matchState": "MATCH_FOUND", "confidenceLevel": "MEDIUM_AND_ABOVE" }
            } } },
            "pi_and_jailbreak": { "piAndJailbreakFilterResult": { "matchState": "MATCH_FOUND" } },
            "sdp": { "sdpFilterResult": { "inspectResult": { "matchState": "NO_MATCH_FOUND" } } }
        } } });
        let masked = json!({ "sanitizationResult": { "filterMatchState": "MATCH_FOUND", "invocationResult": "SUCCESS", "filterResults": {
            "sdp": { "sdpFilterResult": { "deidentifyResult": {
                "executionState": "EXECUTION_SUCCESS", "matchState": "MATCH_FOUND",
                "text": "is there anything malicious running on [IP_ADDRESS]?", "transformedBytes": "7", "infoTypes": ["IP_ADDRESS"]
            } } }
        } } });
        let clean =
            json!({ "sanitizationResult": { "filterMatchState": "NO_MATCH_FOUND", "invocationResult": "SUCCESS", "filterResults": {} } });
        let prices = GuardrailPrices::list(GuardrailProvider::ModelArmor);
        let all = GuardrailConfig { block: MODEL_ARMOR_FILTERS.iter().map(|f| f.to_string()).collect(), ..Default::default() };
        let texts = |n: usize| (0..n).map(|i| format!("text {i}")).collect::<Vec<_>>();

        let r = parse_model_armor(std::slice::from_ref(&blocked), &texts(1), &all, &prices).unwrap();
        assert_eq!(r.action, Action::Blocked);
        assert_eq!(r.outputs, ["The request was blocked by Model Armor (rai, pi_and_jailbreak)."]);
        assert_eq!(r.findings[0].confidence.as_deref(), Some("MEDIUM_AND_ABOVE"));
        assert_eq!(r.usage.tokens, 2);

        // Blocking on nothing only flags.
        assert_eq!(parse_model_armor(&[blocked], &texts(1), &GuardrailConfig::default(), &prices).unwrap().action, Action::Flagged);

        // Masked texts keep their order; untouched ones pass as they were.
        let r = parse_model_armor(&[clean.clone(), masked], &texts(2), &all, &prices).unwrap();
        assert_eq!(r.action, Action::Masked);
        assert_eq!(r.outputs, ["text 0", "is there anything malicious running on [IP_ADDRESS]?"]);
        assert_eq!(
            r.findings.iter().map(|f| (f.policy, f.kind.as_str(), f.action.as_str())).collect::<Vec<_>>(),
            [("sdp", "IP_ADDRESS", "ANONYMIZED")]
        );

        assert_eq!(parse_model_armor(&[clean], &texts(1), &all, &prices).unwrap().action, Action::Passed);
        assert!(parse_model_armor(&[json!({})], &texts(1), &all, &prices).is_err());
    }

    #[test]
    fn arn_names_the_region() {
        assert_eq!(arn_region("arn:aws:bedrock:eu-central-1:058264479455:guardrail/50hag4klvigp"), Some("eu-central-1"));
        assert_eq!(arn_region("50hag4klvigp"), None);
    }
}
