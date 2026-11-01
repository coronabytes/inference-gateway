//! The Bedrock models a credential can call, with the rate limits of the account's quotas, for the
//! console to fill in a new deployment. Read with the official SDK clients: foundation models and
//! inference profiles from Bedrock, applied quotas from Service Quotas.
//!
//! Each listing needs its own permission (`bedrock:ListFoundationModels`,
//! `bedrock:ListInferenceProfiles`, `servicequotas:ListServiceQuotas`). A listing that is denied or
//! fails leaves a note instead of failing the whole lookup; the console then asks for those values.
//!
//! Bedrock quotas are per account, region and model (on-demand, cross-region and global
//! cross-region inference each have their own), shared by everything that uses the account. They
//! name models by display name, e.g. "On-demand model inference tokens per minute for Anthropic
//! Claude Sonnet 4", so they are matched by that name.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use aws_sdk_bedrock::{
    error::{DisplayErrorContext, ProvideErrorMetadata},
    types::{FoundationModelLifecycleStatus, InferenceProfileStatus, InferenceType, ModelModality},
};
use serde::Serialize;

use crate::{
    error::GatewayError,
    model::{CredentialSettings, Mode, Secrets},
    state::AppState,
};

/// How a model is called, which decides the quota that applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum InferenceKind {
    /// The model ID, in the credential's region.
    OnDemand,
    /// A geographic inference profile (`eu.`, `us.`, `apac.`, …).
    CrossRegion,
    /// The global inference profile (`global.`).
    Global,
}

/// A model or inference profile the credential can call.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct BedrockModel {
    /// The model or inference profile ID: what the gateway sends as the upstream model.
    pub id: String,
    /// Its display name, e.g. `EU Anthropic Claude Sonnet 4`.
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub provider: Option<String>,
    pub kind: InferenceKind,
    /// What the model serves.
    pub mode: Mode,
    /// Requests and tokens per minute the account's quota for this model allows, in this region.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub rpm: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub tpm: Option<i64>,
    /// A legacy model, on its way out.
    pub legacy: bool,
}

/// What the credential's account offers in its region, and what could not be read.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct BedrockModels {
    pub region: String,
    pub models: Vec<BedrockModel>,
    /// Why the models or inference profiles could not be listed, if they could not.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub models_error: Option<String>,
    /// Why the quotas could not be read, if they could not.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub quotas_error: Option<String>,
}

/// A foundation model as the listings need it.
struct Foundation {
    id: String,
    name: String,
    provider: Option<String>,
    mode: Option<Mode>,
    on_demand: bool,
    legacy: bool,
}

/// How long a credential's listing is reused: the console asks each time a deployment form opens.
const CACHE_FOR: Duration = Duration::from_secs(300);

/// One credential's last listing, behind a lock held while a new one is read, so requests that
/// arrive together share one lookup.
type Slot = Arc<tokio::sync::Mutex<Option<(Instant, BedrockModels)>>>;

/// Listings per credential and version (its `updated_at`): saving a credential (new keys, another
/// region) reads it afresh. Kept in memory, per gateway.
#[derive(Default)]
pub struct Cache {
    slots: Mutex<HashMap<(uuid::Uuid, chrono::DateTime<chrono::Utc>), Slot>>,
}

impl Cache {
    /// The credential's listing, read at most once every five minutes. A listing with notes
    /// (a missing permission) is kept as well; it is read again when the five minutes are up or
    /// the credential is saved.
    pub async fn get(
        &self,
        state: &AppState,
        credential: uuid::Uuid,
        version: chrono::DateTime<chrono::Utc>,
        settings: &CredentialSettings,
        secrets: &Secrets,
    ) -> Result<BedrockModels, GatewayError> {
        let slot = {
            let mut slots = self.slots.lock().expect("model cache");
            // Older versions of this credential go, as do listings past their five minutes (unless
            // being read right now, which holds the slot's lock).
            slots.retain(|(id, v), slot| {
                let fresh = slot.try_lock().map_or(true, |s| s.as_ref().is_none_or(|(at, _)| at.elapsed() < CACHE_FOR));
                (*id != credential || *v == version) && fresh
            });
            slots.entry((credential, version)).or_default().clone()
        };
        let mut slot = slot.lock().await;
        if let Some((at, models)) = slot.as_ref()
            && at.elapsed() < CACHE_FOR
        {
            return Ok(models.clone());
        }
        let models = list(state, settings, secrets).await?;
        *slot = Some((Instant::now(), models.clone()));
        Ok(models)
    }
}

pub async fn list(state: &AppState, settings: &CredentialSettings, secrets: &Secrets) -> Result<BedrockModels, GatewayError> {
    let config = state.aws.service_config(state, settings, secrets).await?;
    let region = config.region().map(|r| r.to_string()).unwrap_or_default();
    let bedrock = aws_sdk_bedrock::Client::new(&config);
    let quotas = aws_sdk_servicequotas::Client::new(&config);

    let (foundation, profiles, quotas) = tokio::join!(
        bedrock.list_foundation_models().send(),
        bedrock.list_inference_profiles().into_paginator().items().send().collect::<Result<Vec<_>, _>>(),
        quotas.list_service_quotas().service_code("bedrock").into_paginator().items().send().collect::<Result<Vec<_>, _>>(),
    );

    let mut errors = Vec::new();
    let foundation: Vec<Foundation> = match foundation {
        Ok(out) => out.model_summaries().iter().map(foundation_model).collect(),
        Err(e) => {
            errors.push(denied_or("bedrock:ListFoundationModels", "the models", e.code(), &DisplayErrorContext(&e)));
            Vec::new()
        }
    };
    let profiles = profiles.unwrap_or_else(|e| {
        errors.push(denied_or("bedrock:ListInferenceProfiles", "the inference profiles", e.code(), &DisplayErrorContext(&e)));
        Vec::new()
    });
    let (limits, quotas_error) = match quotas {
        Ok(qs) => (limits(qs.iter().filter_map(|q| Some((q.quota_name()?, q.value()?)))), None),
        Err(e) => (HashMap::new(), Some(denied_or("servicequotas:ListServiceQuotas", "the quotas", e.code(), &DisplayErrorContext(&e)))),
    };

    let by_id: HashMap<&str, &Foundation> = foundation.iter().map(|f| (f.id.as_str(), f)).collect();
    let mut models = Vec::new();
    for f in foundation.iter().filter(|f| f.on_demand) {
        let Some(mode) = f.mode else { continue };
        let name = match &f.provider {
            Some(p) => format!("{p} {}", f.name),
            None => f.name.clone(),
        };
        let (rpm, tpm) = quota(&limits, InferenceKind::OnDemand, &name, &f.name);
        models.push(BedrockModel {
            id: f.id.clone(),
            name,
            provider: f.provider.clone(),
            kind: InferenceKind::OnDemand,
            mode,
            rpm,
            tpm,
            legacy: f.legacy,
        });
    }
    for p in profiles.iter().filter(|p| *p.status() == InferenceProfileStatus::Active) {
        let id = p.inference_profile_id();
        let Some((prefix, model_id)) = id.split_once('.') else { continue };
        let kind = if prefix == "global" { InferenceKind::Global } else { InferenceKind::CrossRegion };
        let underlying = by_id.get(model_id);
        // Without the foundation model list, the profile's own name without its geography.
        let mode = underlying.and_then(|f| f.mode).or_else(|| guess_mode(model_id));
        let Some(mode) = mode else { continue };
        let (quota_name, short) = match underlying {
            Some(f) => (f.provider.as_ref().map_or_else(|| f.name.clone(), |p| format!("{p} {}", f.name)), f.name.clone()),
            None => {
                let n = p.inference_profile_name();
                let n = n.split_once(' ').map_or(n, |(_, rest)| rest).to_string();
                (n.clone(), n)
            }
        };
        let (rpm, tpm) = quota(&limits, kind, &quota_name, &short);
        models.push(BedrockModel {
            id: id.to_string(),
            name: p.inference_profile_name().to_string(),
            provider: underlying.and_then(|f| f.provider.clone()),
            kind,
            mode,
            rpm,
            tpm,
            legacy: underlying.is_some_and(|f| f.legacy),
        });
    }
    // By mode first: the console lists them under one heading per mode.
    let rank = |m: Mode| [Mode::Chat, Mode::Embedding, Mode::Rerank, Mode::Decision].iter().position(|x| *x == m);
    models.sort_by_key(|m| (rank(m.mode), m.legacy, m.name.to_lowercase()));
    Ok(BedrockModels { region, models, models_error: (!errors.is_empty()).then(|| errors.join(" ")), quotas_error })
}

fn foundation_model(m: &aws_sdk_bedrock::types::FoundationModelSummary) -> Foundation {
    let id = m.model_id().to_string();
    let mode = if m.output_modalities().contains(&ModelModality::Embedding) {
        // The gateway calls Bedrock embeddings as Titan Text Embeddings V2.
        id.starts_with("amazon.titan-embed-text-v2").then_some(Mode::Embedding)
    } else {
        guess_mode(&id).filter(|_| m.output_modalities().contains(&ModelModality::Text) || id.contains("rerank"))
    };
    Foundation {
        name: m.model_name().unwrap_or(&id).to_string(),
        provider: m.provider_name().map(str::to_string),
        on_demand: m.inference_types_supported().contains(&InferenceType::OnDemand),
        legacy: m.model_lifecycle().is_some_and(|l| *l.status() == FoundationModelLifecycleStatus::Legacy),
        mode,
        id,
    }
}

/// A model's mode from its ID alone.
fn guess_mode(id: &str) -> Option<Mode> {
    if id.contains("rerank") {
        Some(Mode::Rerank)
    } else if id.contains("embed") {
        id.starts_with("amazon.titan-embed-text-v2").then_some(Mode::Embedding)
    } else {
        Some(Mode::Chat)
    }
}

/// The note for a listing that failed: which permission is missing when it was denied.
fn denied_or(action: &str, what: &str, code: Option<&str>, e: &dyn std::fmt::Display) -> String {
    if code.is_some_and(|c| c.contains("AccessDenied") || c == "UnauthorizedOperation") {
        format!("Not allowed to list {what}: the credential needs {action}.")
    } else {
        tracing::warn!(error = %e, action, "Bedrock model lookup failed");
        format!("Could not list {what}: {e}.")
    }
}

/// Lowercase letters and digits only, so "Claude 3.5 Sonnet v2" matches "Claude 3.5 Sonnet V2".
fn norm(s: &str) -> String {
    s.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_lowercase()).collect()
}

/// Per-minute request and token quotas by inference kind and normalized model name.
type Limits = HashMap<(InferenceKind, String), (Option<i64>, Option<i64>)>;

/// Read Bedrock's per-minute inference quotas from their names, e.g. "Cross-region model inference
/// tokens per minute for Anthropic Claude Sonnet 4". Daily and other quotas are left out.
fn limits<'a>(quotas: impl Iterator<Item = (&'a str, f64)>) -> Limits {
    let mut out = Limits::new();
    for (name, value) in quotas {
        let lower = name.to_ascii_lowercase();
        let kind = if lower.starts_with("global cross-region") {
            InferenceKind::Global
        } else if lower.starts_with("cross-region") {
            InferenceKind::CrossRegion
        } else if lower.starts_with("on-demand") {
            InferenceKind::OnDemand
        } else {
            continue;
        };
        let (requests, model) = if let Some((_, m)) = lower.split_once(" requests per minute for ") {
            (true, m)
        } else if let Some((_, m)) = lower.split_once(" tokens per minute for ") {
            (false, m)
        } else {
            continue;
        };
        // Some names end in a remark, e.g. "(doubled for cross-region calls)".
        let model = model.split_once('(').map_or(model, |(m, _)| m);
        let entry = out.entry((kind, norm(model))).or_default();
        let v = Some(value.round() as i64);
        if requests { entry.0 = v } else { entry.1 = v }
    }
    out
}

/// The quota for a model, by its full display name ("Anthropic Claude Sonnet 4") or its name alone.
fn quota(limits: &Limits, kind: InferenceKind, full: &str, short: &str) -> (Option<i64>, Option<i64>) {
    limits.get(&(kind, norm(full))).or_else(|| limits.get(&(kind, norm(short)))).copied().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_quota_names() {
        let l = limits(
            [
                ("On-demand model inference requests per minute for Anthropic Claude 3.5 Sonnet V2", 50.0),
                ("On-demand model inference tokens per minute for Anthropic Claude 3.5 Sonnet V2", 400000.0),
                ("On-demand model inference tokens per minute for Anthropic Claude 3.5 Sonnet", 200000.0),
                ("Cross-region model inference tokens per minute for Anthropic Claude Sonnet 4", 2000000.0),
                ("Global cross-region model inference requests per minute for Anthropic Claude Sonnet 4.5", 500.0),
                ("On-demand InvokeModel requests per minute for Amazon Titan Text Embeddings V2", 6000.0),
                ("Model invocation max tokens per day for Anthropic Claude Sonnet 4", 1e9),
            ]
            .into_iter(),
        );
        assert_eq!(quota(&l, InferenceKind::OnDemand, "Anthropic Claude 3.5 Sonnet v2", "Claude 3.5 Sonnet v2"), (Some(50), Some(400000)));
        assert_eq!(quota(&l, InferenceKind::OnDemand, "Anthropic Claude 3.5 Sonnet", "Claude 3.5 Sonnet"), (None, Some(200000)));
        assert_eq!(quota(&l, InferenceKind::CrossRegion, "Anthropic Claude Sonnet 4", "Claude Sonnet 4"), (None, Some(2000000)));
        assert_eq!(quota(&l, InferenceKind::OnDemand, "Anthropic Claude Sonnet 4", "Claude Sonnet 4"), (None, None));
        assert_eq!(quota(&l, InferenceKind::Global, "Anthropic Claude Sonnet 4.5", "Claude Sonnet 4.5"), (Some(500), None));
        assert_eq!(quota(&l, InferenceKind::OnDemand, "Amazon Titan Text Embeddings V2", "Titan Text Embeddings V2"), (Some(6000), None));
        assert_eq!(l.len(), 5, "the daily quota is left out");
    }

    #[test]
    fn modes() {
        assert_eq!(guess_mode("anthropic.claude-sonnet-4-20250514-v1:0"), Some(Mode::Chat));
        assert_eq!(guess_mode("cohere.rerank-v3-5:0"), Some(Mode::Rerank));
        assert_eq!(guess_mode("amazon.titan-embed-text-v2:0"), Some(Mode::Embedding));
        assert_eq!(guess_mode("cohere.embed-english-v3"), None, "only Titan V2 embeddings are supported");
    }

    #[test]
    fn denied_names_the_permission() {
        let note = denied_or("servicequotas:ListServiceQuotas", "the quotas", Some("AccessDeniedException"), &"denied");
        assert!(note.contains("servicequotas:ListServiceQuotas"));
    }
}
