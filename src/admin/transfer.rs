//! Exporting and importing the gateway's configuration: credentials, guardrails, deployments,
//! auto routers, model aliases and the models' metadata (the model hub's display names and descriptions) as one
//! JSON document, to back it up or move it to another gateway.
//!
//! Credentials are exported without their secrets: only the names of the secret fields that were
//! set. Deployments and guardrails refer to their credential, and deployments to the projects they
//! are limited to, by name, so a document works on any gateway. Projects and keys themselves are
//! not exported: those named must exist where the document is imported, and the guardrails set on
//! them stay as they are.
//!
//! Every entry carries its id (from version 2 on). Import merges: an entry updates what has its id,
//! and otherwise what matches it as before ids were exported: credentials by name, deployments by
//! model name, credential and upstream model, guardrails, routers and aliases by name. What matches nothing is created
//! with the entry's id, so the next import of the same document finds it again. Matched by id, a
//! credential, deployment, router or alias can be renamed; routers, keys and model metadata on this
//! gateway that name a renamed model follow it, as they do after a rename in the admin API. An
//! alias names its target by id and by model name: the id where it is a deployment or router in
//! the file or on this gateway, the model name otherwise. Model
//! metadata is matched by model name: an entry replaces that model's metadata, `{}` removes it,
//! and models the file has no entry for keep theirs. A deployment without
//! `projects` keeps the projects it has. Everything is checked as it will be after the import,
//! with the same rules as the admin API, before anything is written, and then written in one
//! transaction. `dry_run` only reports what would change.

use std::collections::{BTreeSet, HashMap, HashSet};

use axum::{
    Extension, Json,
    extract::{Query, State},
    http::header,
    response::IntoResponse,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::{
    AliasInput, DeploymentInput, R, RouterInput, alias_row, check_alias, check_deployment, check_deployment_fits, check_router, clean_settings,
    guardrails::{check_credential, check_guardrail},
    hub::check_metadata,
    rename_model, retarget_aliases, router_row, target_name, to_row,
};
use crate::{
    audit::{self, Actor},
    db,
    error::AdminError,
    guardrails::{GuardrailConfig, GuardrailPrices, GuardrailProvider, GuardrailRow},
    model::{AliasRow, CredentialRow, CredentialSettings, DeploymentRow, ProviderKind, RouterRow, normalize_tags},
    registry,
    state::AppState,
    sync::broadcast_reload,
};

const FORMAT: &str = "inference-gateway-config";
/// 2 added the ids of credentials, deployments and routers, 3 guardrails, 4 model metadata, 5
/// model aliases. Older documents still import.
const VERSION: u32 = 5;

#[derive(Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Document {
    format: String,
    version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    exported_at: Option<DateTime<Utc>>,
    #[serde(default)]
    credentials: Vec<CredentialDoc>,
    /// From version 3.
    #[serde(default)]
    guardrails: Vec<GuardrailDoc>,
    /// Deployments as the admin API takes them, with their `id`, the credential's name in
    /// `credential` and the names of the projects they are limited to in `projects`.
    #[serde(default)]
    deployments: Vec<Map<String, Value>>,
    /// Auto routers as the admin API takes them, with their `id`.
    #[serde(default)]
    routers: Vec<Map<String, Value>>,
    /// From version 5: model aliases as the admin API takes them, with their `id`, and their
    /// target's model name in `target`.
    #[serde(default)]
    aliases: Vec<Map<String, Value>>,
    /// From version 4: what the model hub shows of each model besides its deployments.
    #[serde(default)]
    model_metadata: Vec<ModelMetadataDoc>,
}

#[derive(Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelMetadataDoc {
    /// The public model name: a deployment's, an auto router's or an alias's, in the file or on the gateway.
    model: String,
    /// `display_name`, `vendor` and `description`, and any other fields; `{}` removes the model's metadata.
    metadata: Map<String, Value>,
}

#[derive(Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CredentialDoc {
    /// The credential's id on the exporting gateway; absent in version 1 documents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<Uuid>,
    name: String,
    provider: String,
    #[serde(default)]
    settings: CredentialSettings,
    /// Which secret fields were set on the exporting gateway. Never their values.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    secrets: Vec<String>,
}

#[derive(Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct GuardrailDoc {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<Uuid>,
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    provider: String,
    /// The name of the credential it is called with.
    credential: String,
    config: GuardrailConfig,
    /// Omitted: the provider's list prices for a new guardrail, the stored ones for an existing one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prices: Option<GuardrailPrices>,
}

fn guardrail_doc(g: &GuardrailRow, credential: &str) -> GuardrailDoc {
    GuardrailDoc {
        id: Some(g.id),
        name: g.name.clone(),
        description: g.description.clone(),
        provider: g.provider.clone(),
        credential: credential.to_string(),
        config: g.config.0.clone(),
        prices: Some(g.prices.0),
    }
}

fn deployment_doc(d: &DeploymentRow, credential: &str, projects: &HashMap<Uuid, String>) -> Map<String, Value> {
    let v = json!({
        "id": d.id,
        "model_name": d.model_name, "mode": d.mode, "provider": d.provider, "upstream_model": d.upstream_model,
        "credential": credential, "config": d.config.0.redacted(),
        "weight": d.weight, "priority": d.priority, "rpm_limit": d.rpm_limit, "tpm_limit": d.tpm_limit,
        "timeout_secs": d.timeout_secs,
        "input_cost_per_mtok": d.input_cost_per_mtok, "output_cost_per_mtok": d.output_cost_per_mtok,
        "cache_read_cost_per_mtok": d.cache_read_cost_per_mtok, "cache_write_cost_per_mtok": d.cache_write_cost_per_mtok,
        "long_context_threshold": d.long_context_threshold,
        "long_input_cost_per_mtok": d.long_input_cost_per_mtok, "long_output_cost_per_mtok": d.long_output_cost_per_mtok,
        "long_cache_read_cost_per_mtok": d.long_cache_read_cost_per_mtok,
        "long_cache_write_cost_per_mtok": d.long_cache_write_cost_per_mtok,
        "base_model": d.base_model, "tags": d.tags, "enabled": d.enabled,
    });
    match v {
        // Only rerank deployments have it, and only some deployments projects; left out otherwise.
        Value::Object(mut m) => {
            if let Some(p) = d.cost_per_k_searches {
                m.insert("cost_per_k_searches".into(), p.into());
            }
            if !d.project_ids.is_empty() {
                let mut names: Vec<&str> = d.project_ids.iter().filter_map(|id| projects.get(id).map(String::as_str)).collect();
                names.sort();
                m.insert("projects".into(), names.into());
            }
            m
        }
        _ => unreachable!("json! of an object"),
    }
}

fn router_doc(r: &RouterRow) -> Map<String, Value> {
    let v = json!({
        "id": r.id,
        "model_name": r.model_name, "decision_model": r.decision_model, "instructions": r.instructions,
        "routes": r.routes.0, "fallback_route": r.fallback_route, "tags": r.tags, "enabled": r.enabled,
        "sticky": r.sticky,
    });
    match v {
        Value::Object(m) => m,
        _ => unreachable!("json! of an object"),
    }
}

fn alias_doc(a: &AliasRow, deployments: &[DeploymentRow], routers: &[RouterRow]) -> Map<String, Value> {
    let v = json!({
        "id": a.id, "model_name": a.model_name,
        "target_id": a.target_id, "target": target_name(a.target_id, deployments, routers), "enabled": a.enabled,
    });
    match v {
        Value::Object(m) => m,
        _ => unreachable!("json! of an object"),
    }
}

/// `GET /admin/api/config/export`: the configuration as a JSON download.
pub(super) async fn export(State(state): State<AppState>) -> R<impl IntoResponse> {
    let (creds, deployments, routers, aliases, projects, guardrails, metadata) = tokio::try_join!(
        db::list_credentials(&state.db),
        db::list_deployments(&state.db),
        db::list_routers(&state.db),
        db::list_aliases(&state.db),
        db::list_project_rows(&state.db),
        db::list_guardrails(&state.db),
        db::list_model_metadata(&state.db)
    )?;
    let mut model_metadata: Vec<ModelMetadataDoc> = metadata
        .into_iter()
        .filter_map(|(model, m)| match m {
            Value::Object(metadata) => Some(ModelMetadataDoc { model, metadata }),
            _ => None,
        })
        .collect();
    model_metadata.sort_by(|a, b| a.model.cmp(&b.model));
    let names: HashMap<Uuid, &str> = creds.iter().map(|c| (c.id, c.name.as_str())).collect();
    let projects: HashMap<Uuid, String> = projects.into_iter().map(|p| (p.id, p.name)).collect();
    let credentials = creds
        .iter()
        .map(|c| {
            let secrets = registry::decrypt_secrets(&state.cfg.secret_box, c.secrets.as_deref()).unwrap_or_default();
            CredentialDoc {
                id: Some(c.id),
                name: c.name.clone(),
                provider: c.provider.clone(),
                settings: c.settings.0.clone(),
                secrets: secrets.presence().into_iter().filter(|(_, set)| *set).map(|(k, _)| k.to_string()).collect(),
            }
        })
        .collect();
    let doc = Document {
        format: FORMAT.into(),
        version: VERSION,
        exported_at: Some(Utc::now()),
        credentials,
        guardrails: guardrails.iter().map(|g| guardrail_doc(g, names.get(&g.credential_id).copied().unwrap_or_default())).collect(),
        deployments: deployments
            .iter()
            .map(|d| deployment_doc(d, names.get(&d.credential_id).copied().unwrap_or_default(), &projects))
            .collect(),
        routers: routers.iter().map(router_doc).collect(),
        aliases: aliases.iter().map(|a| alias_doc(a, &deployments, &routers)).collect(),
        model_metadata,
    };
    let disposition = format!("attachment; filename=\"inference-gateway-config-{}.json\"", Utc::now().format("%Y-%m-%d"));
    Ok(([(header::CONTENT_DISPOSITION, disposition)], Json(doc)))
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct ImportQuery {
    #[serde(default)]
    dry_run: bool,
}

#[derive(Serialize, Default)]
struct Changes {
    created: Vec<String>,
    updated: Vec<String>,
}

/// Providers whose credentials do not work without a secret. OpenAI-compatible servers may need
/// none, and Bedrock can use the AWS credential chain.
fn needs_secret(p: ProviderKind) -> bool {
    !matches!(p, ProviderKind::Openai | ProviderKind::Bedrock)
}

fn bad(msg: impl Into<String>) -> AdminError {
    AdminError::bad_request(msg)
}

/// Prefixes an error with what it is about.
fn about(what: &str) -> impl Fn(AdminError) -> AdminError + '_ {
    move |e| AdminError(e.0, format!("{what}: {}", e.1))
}

/// Takes an entry's `id` out of it, for the admin API's input that has none.
fn take_id(m: &mut Map<String, Value>, what: &str) -> R<Option<Uuid>> {
    match m.remove("id") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => serde_json::from_value(v).map(Some).map_err(|e| bad(format!("{what}: id: {e}"))),
    }
}

/// The ids a document's entries carry. Entries without one only match what no entry claims by
/// id, so an entry renamed elsewhere in the document is not matched by its old name too.
fn ids_in(entries: &[Map<String, Value>]) -> HashSet<Uuid> {
    entries.iter().filter_map(|m| m.get("id")).filter_map(|v| serde_json::from_value(v.clone()).ok()).collect()
}

/// Records that an entry matched `id`; two entries of a document cannot update the same thing.
fn claim(claimed: &mut HashSet<Uuid>, id: Uuid, what: &str) -> R<()> {
    match claimed.insert(id) {
        true => Ok(()),
        false => Err(bad(format!("{what} matches the same entry on this gateway as another one in the file"))),
    }
}

/// `POST /admin/api/config/import[?dry_run=true]`.
pub(super) async fn import(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Query(q): Query<ImportQuery>,
    Json(body): Json<Value>,
) -> R<Json<Value>> {
    let doc: Document = serde_json::from_value(body).map_err(|e| bad(format!("not a configuration file: {e}")))?;
    if doc.format != FORMAT {
        return Err(bad("not an Inference Gateway configuration file"));
    }
    if !(1..=VERSION).contains(&doc.version) {
        return Err(bad(format!("configuration version {} is not supported; this gateway reads versions 1 to {VERSION}", doc.version)));
    }
    let (mut creds, mut deployments, mut routers, mut aliases, mut keys, projects, mut guardrails, mut metadata) = tokio::try_join!(
        db::list_credentials(&state.db),
        db::list_deployments(&state.db),
        db::list_routers(&state.db),
        db::list_aliases(&state.db),
        db::list_keys(&state.db),
        db::list_project_rows(&state.db),
        db::list_guardrails(&state.db),
        db::list_model_metadata(&state.db)
    )?;
    let projects: HashMap<Uuid, String> = projects.into_iter().map(|p| (p.id, p.name)).collect();
    let now = Utc::now();

    // Credentials: by id, then by name. Existing ones keep their secrets; new ones get none.
    let (mut cred_changes, mut needs_secrets) = (Changes::default(), Vec::new());
    let (mut new_creds, mut updated_creds, mut claimed) = (HashSet::new(), HashSet::new(), HashSet::new());
    let file_ids: HashSet<Uuid> = doc.credentials.iter().filter_map(|c| c.id).collect();
    let mut seen = HashSet::new();
    for c in doc.credentials {
        let name = c.name.trim().to_string();
        if name.is_empty() || name.len() > 100 {
            return Err(bad(format!("credential '{name}': the name must be 1-100 characters")));
        }
        if !seen.insert(name.clone()) {
            return Err(bad(format!("credential '{name}' is in the file twice")));
        }
        let what = format!("credential '{name}'");
        let provider = ProviderKind::parse(&c.provider).ok_or_else(|| bad(format!("{what}: unknown provider '{}'", c.provider)))?;
        let settings = clean_settings(&state, provider, c.settings).map_err(about(&what))?;
        let found = c
            .id
            .and_then(|id| creds.iter().position(|x| x.id == id))
            .or_else(|| creds.iter().position(|x| x.name == name && !claimed.contains(&x.id) && !file_ids.contains(&x.id)));
        match found {
            Some(ix) if creds[ix].provider != provider.as_str() => {
                let x = &creds[ix];
                return Err(bad(format!("{what} is for {} on this gateway but for {} in the file", x.provider, provider.as_str())));
            }
            Some(ix) => {
                claim(&mut claimed, creds[ix].id, &what)?;
                let x = &mut creds[ix];
                if x.settings.0 != settings || x.name != name {
                    let label = if x.name == name { name.clone() } else { format!("{name}, renamed from {}", x.name) };
                    x.settings = sqlx::types::Json(settings);
                    x.name = name;
                    updated_creds.insert(x.id);
                    cred_changes.updated.push(label);
                }
            }
            None => {
                let row = CredentialRow {
                    id: c.id.unwrap_or_else(Uuid::now_v7),
                    name: name.clone(),
                    provider: provider.as_str().to_string(),
                    secrets: None,
                    settings: sqlx::types::Json(settings),
                    created_at: now,
                    updated_at: now,
                };
                claimed.insert(row.id);
                new_creds.insert(row.id);
                creds.push(row);
                if !c.secrets.is_empty() || needs_secret(provider) {
                    needs_secrets.push(name.clone());
                }
                cred_changes.created.push(name);
            }
        }
    }
    // Names are unique; a rename in the file may take a name another credential here still has.
    let mut names = HashSet::new();
    if let Some(c) = creds.iter().find(|c| !names.insert(c.name.as_str())) {
        return Err(bad(format!("credential '{}': another credential on this gateway has that name", c.name)));
    }

    // Guardrails: by id, then by name, with their credential by name as it will be.
    let (mut guard_changes, mut new_guards, mut changed_guards, mut claimed) = (Changes::default(), HashSet::new(), HashSet::new(), HashSet::new());
    let file_ids: HashSet<Uuid> = doc.guardrails.iter().filter_map(|g| g.id).collect();
    for g in doc.guardrails {
        let what = format!("guardrail '{}'", g.name.trim());
        let provider = GuardrailProvider::parse(&g.provider).ok_or_else(|| bad(format!("{what}: unknown provider '{}'", g.provider)))?;
        let cred = creds
            .iter()
            .find(|c| c.name == g.credential)
            .ok_or_else(|| bad(format!("{what} uses credential '{}', which is neither in the file nor on this gateway", g.credential)))?;
        check_credential(provider, &cred.provider).map_err(about(&what))?;
        let (name, description, config) = check_guardrail(provider, &g.name, g.description.as_deref(), &g.config, g.prices).map_err(about(&what))?;
        let found = g
            .id
            .and_then(|id| guardrails.iter().position(|x| x.id == id))
            .or_else(|| guardrails.iter().position(|x| x.name == name && !claimed.contains(&x.id) && !file_ids.contains(&x.id)));
        match found {
            Some(ix) if guardrails[ix].provider != provider.as_str() => {
                return Err(bad(format!("{what} is for {} on this gateway but for {} in the file", guardrails[ix].provider, provider.as_str())));
            }
            Some(ix) => {
                claim(&mut claimed, guardrails[ix].id, &what)?;
                let cred_name = cred.name.clone();
                let x = &mut guardrails[ix];
                let before = guardrail_doc(x, &cred_name);
                let renamed = (x.name != name).then(|| x.name.clone());
                x.name = name.clone();
                x.description = description;
                x.credential_id = cred.id;
                x.config = sqlx::types::Json(config);
                x.prices = sqlx::types::Json(g.prices.unwrap_or(x.prices.0));
                if guardrail_doc(x, &cred_name) != before {
                    changed_guards.insert(x.id);
                    guard_changes.updated.push(renamed.map_or(name.clone(), |old| format!("{name}, renamed from {old}")));
                }
            }
            None => {
                let row = GuardrailRow {
                    id: g.id.unwrap_or_else(Uuid::now_v7),
                    name: name.clone(),
                    description,
                    provider: provider.as_str().to_string(),
                    credential_id: cred.id,
                    config: sqlx::types::Json(config),
                    prices: sqlx::types::Json(g.prices.unwrap_or_else(|| GuardrailPrices::list(provider))),
                    created_at: now,
                    updated_at: now,
                };
                claimed.insert(row.id);
                new_guards.insert(row.id);
                guardrails.push(row);
                guard_changes.created.push(name);
            }
        }
    }
    let mut names = HashSet::new();
    if let Some(g) = guardrails.iter().find(|g| !names.insert(g.name.as_str())) {
        return Err(bad(format!("guardrail '{}': another guardrail on this gateway has that name", g.name)));
    }

    // Deployments: by id, then by model name, credential and upstream model.
    let (mut dep_changes, mut changed_deps, mut claimed) = (Changes::default(), HashSet::new(), HashSet::new());
    // Renamed deployments, as (old, new) names and as (id, old name) for the aliases.
    let (mut renames, mut left) = (Vec::new(), Vec::new());
    let file_ids = ids_in(&doc.deployments);
    for mut m in doc.deployments {
        let model = m.get("model_name").and_then(Value::as_str).unwrap_or("?").to_string();
        let file_id = take_id(&mut m, &format!("deployment '{model}'"))?;
        let Some(Value::String(cred_name)) = m.remove("credential") else {
            return Err(bad(format!("deployment '{model}': credential (the credential's name) is required")));
        };
        let cred = creds
            .iter()
            .find(|c| c.name == cred_name)
            .ok_or_else(|| bad(format!("deployment '{model}' uses credential '{cred_name}', which is neither in the file nor on this gateway")))?
            .clone();
        m.insert("credential_id".into(), json!(cred.id));
        // By name, to this gateway's projects; left out, an existing deployment keeps its own.
        if m.contains_key("project_ids") {
            return Err(bad(format!("deployment '{model}': name its projects in projects, not by id in project_ids")));
        }
        let project_ids = match m.remove("projects") {
            None => None,
            Some(v) => {
                let wanted: Vec<String> = serde_json::from_value(v).map_err(|e| bad(format!("deployment '{model}': projects: {e}")))?;
                let mut ids = Vec::new();
                for name in wanted.iter().map(|n| n.trim()) {
                    let id = projects.iter().find(|(_, p)| *p == name).map(|(id, _)| *id).ok_or_else(|| {
                        bad(format!("deployment '{model}' is limited to project '{name}', which is not on this gateway; create it first"))
                    })?;
                    ids.push(id);
                }
                ids.sort();
                ids.dedup();
                Some(ids)
            }
        };
        m.insert("project_ids".into(), json!(project_ids));
        let mut input: DeploymentInput = serde_json::from_value(Value::Object(m)).map_err(|e| bad(format!("deployment '{model}': {e}")))?;
        let label = format!("{model} ({} via {})", input.upstream_model.trim(), cred.name);
        input.tags = normalize_tags(std::mem::take(&mut input.tags)).map_err(|e| bad(format!("deployment {label}: {e}")))?;
        let provider = check_deployment(&input).map_err(about(&format!("deployment {label}")))?;
        let existing = file_id.and_then(|id| deployments.iter().position(|d| d.id == id)).or_else(|| {
            deployments.iter().position(|d| {
                d.model_name == input.model_name
                    && d.credential_id == cred.id
                    && d.upstream_model == input.upstream_model.trim()
                    && !claimed.contains(&d.id)
                    && !file_ids.contains(&d.id)
            })
        });
        let (id, created_at) = match existing {
            Some(ix) if deployments[ix].mode != input.mode.as_str() => {
                return Err(bad(format!("deployment {label} is a {} deployment here; its mode cannot change", deployments[ix].mode)));
            }
            Some(ix) => (deployments[ix].id, deployments[ix].created_at),
            None => (file_id.unwrap_or_else(Uuid::now_v7), now),
        };
        claim(&mut claimed, id, &format!("deployment {label}"))?;
        if let Some(ix) = existing {
            input.project_ids.get_or_insert_with(|| deployments[ix].project_ids.clone());
        }
        check_deployment_fits(&input, provider, Some(id), &cred, &deployments, &routers, &aliases).map_err(about(&format!("deployment {label}")))?;
        let row = to_row(id, input, created_at);
        match existing {
            Some(ix) => {
                let before = &deployments[ix];
                if before.credential_id != row.credential_id || deployment_doc(before, "", &projects) != deployment_doc(&row, "", &projects) {
                    if before.model_name != row.model_name {
                        left.push((id, before.model_name.clone()));
                        renames.push((before.model_name.clone(), row.model_name.clone()));
                        dep_changes.updated.push(format!("{label}, renamed from {}", before.model_name));
                    } else {
                        dep_changes.updated.push(label);
                    }
                    deployments[ix] = row;
                    changed_deps.insert(id);
                }
            }
            None => {
                deployments.push(row);
                changed_deps.insert(id);
                dep_changes.created.push(label);
            }
        }
    }

    // Routers and keys here that name a renamed model follow it, before the routers in the file
    // are checked against the models as they will be.
    let (mut followed_routers, mut changed_keys) = (HashSet::new(), HashSet::new());
    for (old, new) in &renames {
        let (r, k) = rename_model(old, new, &deployments, &mut routers, &mut keys);
        followed_routers.extend(r);
        changed_keys.extend(k);
    }

    // Routers: by id, then by name, checked against the deployments as they will be.
    let (mut router_changes, mut changed_routers, mut claimed) = (Changes::default(), HashSet::new(), HashSet::new());
    let mut router_renames = Vec::new();
    let file_ids = ids_in(&doc.routers);
    for mut m in doc.routers {
        let name = m.get("model_name").and_then(Value::as_str).unwrap_or("?").trim().to_string();
        let what = format!("router '{name}'");
        let file_id = take_id(&mut m, &what)?;
        let mut input: RouterInput = serde_json::from_value(Value::Object(m)).map_err(|e| bad(format!("{what}: {e}")))?;
        let existing = file_id
            .and_then(|id| routers.iter().position(|r| r.id == id))
            .or_else(|| routers.iter().position(|r| r.model_name == name && !claimed.contains(&r.id) && !file_ids.contains(&r.id)));
        let (id, created_at) = existing.map_or((file_id.unwrap_or_else(Uuid::now_v7), now), |ix| (routers[ix].id, routers[ix].created_at));
        claim(&mut claimed, id, &what)?;
        check_router(&mut input, Some(id), &deployments, &routers, &aliases).map_err(about(&what))?;
        let row = router_row(id, input, created_at);
        match existing {
            Some(ix) => {
                let before = &routers[ix];
                if router_doc(before) != router_doc(&row) {
                    if before.model_name != row.model_name {
                        router_renames.push((before.model_name.clone(), row.model_name.clone()));
                        router_changes.updated.push(format!("{}, renamed from {}", row.model_name, before.model_name));
                    } else {
                        router_changes.updated.push(name);
                    }
                    routers[ix] = row;
                    changed_routers.insert(id);
                }
            }
            None => {
                routers.push(row);
                changed_routers.insert(id);
                router_changes.created.push(name);
            }
        }
    }
    for (old, new) in &router_renames {
        let (r, k) = rename_model(old, new, &deployments, &mut routers, &mut keys);
        followed_routers.extend(r);
        changed_keys.extend(k);
    }
    for r in routers.iter().filter(|r| followed_routers.contains(&r.id) && !changed_routers.contains(&r.id)) {
        router_changes.updated.push(format!("{} (follows a rename)", r.model_name));
    }
    changed_routers.extend(followed_routers);

    // Aliases here whose deployment left its model stay on the model, as in the admin API. Then
    // the file's aliases: by id, then by name, with their target as the models will be.
    let (mut alias_changes, mut changed_aliases, mut claimed) = (Changes::default(), HashSet::new(), HashSet::new());
    for (id, old) in &left {
        changed_aliases.extend(retarget_aliases(*id, old, &deployments, &mut aliases));
    }
    for a in aliases.iter().filter(|a| changed_aliases.contains(&a.id)) {
        alias_changes.updated.push(format!("{} (stays on its model)", a.model_name));
    }
    let mut alias_renames = Vec::new();
    let file_ids = ids_in(&doc.aliases);
    for mut m in doc.aliases {
        let name = m.get("model_name").and_then(Value::as_str).unwrap_or("?").trim().to_string();
        let what = format!("alias '{name}'");
        let file_id = take_id(&mut m, &what)?;
        let target = match m.remove("target") {
            None | Some(Value::Null) => None,
            Some(Value::String(t)) => Some(t.trim().to_string()),
            Some(_) => return Err(bad(format!("{what}: target must be a model name"))),
        };
        // The target's id where it names a deployment or router, its model name otherwise.
        let target_id = m.get("target_id").and_then(|v| serde_json::from_value::<Uuid>(v.clone()).ok());
        if !target_id.is_some_and(|t| target_name(t, &deployments, &routers).is_some()) {
            let t = target.ok_or_else(|| bad(format!("{what}: target_id is not a deployment or router in the file or on this gateway")))?;
            let named = || deployments.iter().filter(|d| d.model_name == t);
            let id = named()
                .find(|d| d.enabled)
                .or_else(|| named().next())
                .map(|d| d.id)
                .or_else(|| routers.iter().find(|r| r.model_name == t).map(|r| r.id))
                .ok_or_else(|| bad(format!("{what}: its target '{t}' is neither in the file nor on this gateway")))?;
            m.insert("target_id".into(), json!(id));
        }
        let mut input: AliasInput = serde_json::from_value(Value::Object(m)).map_err(|e| bad(format!("{what}: {e}")))?;
        let existing = file_id
            .and_then(|id| aliases.iter().position(|a| a.id == id))
            .or_else(|| aliases.iter().position(|a| a.model_name == name && !claimed.contains(&a.id) && !file_ids.contains(&a.id)));
        let (id, created_at) = existing.map_or((file_id.unwrap_or_else(Uuid::now_v7), now), |ix| (aliases[ix].id, aliases[ix].created_at));
        claim(&mut claimed, id, &what)?;
        check_alias(&mut input, Some(id), &deployments, &routers, &aliases).map_err(about(&what))?;
        let row = alias_row(id, input, created_at);
        match existing {
            Some(ix) => {
                let before = &aliases[ix];
                if alias_doc(before, &deployments, &routers) != alias_doc(&row, &deployments, &routers) {
                    if before.model_name != row.model_name {
                        alias_renames.push((before.model_name.clone(), row.model_name.clone()));
                        alias_changes.updated.push(format!("{}, renamed from {}", row.model_name, before.model_name));
                    } else if !changed_aliases.contains(&id) {
                        alias_changes.updated.push(name);
                    }
                    aliases[ix] = row;
                    changed_aliases.insert(id);
                }
            }
            None => {
                aliases.push(row);
                changed_aliases.insert(id);
                alias_changes.created.push(name);
            }
        }
    }
    for (old, new) in &alias_renames {
        let (_, k) = rename_model(old, new, &deployments, &mut routers, &mut keys);
        changed_keys.extend(k);
    }
    let key_changes = Changes {
        created: vec![],
        updated: keys.iter().filter(|k| changed_keys.contains(&k.id)).map(|k| format!("{} (follows a rename)", k.name)).collect(),
    };

    // Model metadata: renamed models take theirs along, as in the admin API, unless the new name
    // has its own; then the file's entries, by model name as the models will be.
    let named = |m: &str| {
        deployments.iter().any(|d| d.model_name == m) || routers.iter().any(|r| r.model_name == m) || aliases.iter().any(|a| a.model_name == m)
    };
    let (mut meta_changes, mut changed_meta) = (Changes::default(), BTreeSet::new());
    for (old, new) in renames.iter().chain(&router_renames).chain(&alias_renames) {
        if old == new || named(old) || metadata.contains_key(new) {
            continue;
        }
        if let Some(m) = metadata.remove(old) {
            metadata.insert(new.clone(), m);
            changed_meta.extend([old.clone(), new.clone()]);
            meta_changes.updated.push(format!("{new} (follows a rename)"));
        }
    }
    let mut seen = HashSet::new();
    for e in doc.model_metadata {
        let model = e.model.trim().to_string();
        let what = format!("metadata of model '{model}'");
        if !seen.insert(model.clone()) {
            return Err(bad(format!("{what} is in the file twice")));
        }
        if !named(&model) {
            return Err(bad(format!("{what}: no deployment, router or alias is named '{model}', in the file or on this gateway")));
        }
        let new = Value::Object(check_metadata(e.metadata).map_err(about(&what))?);
        let empty = new.as_object().is_some_and(Map::is_empty);
        match metadata.get(&model) {
            Some(old) if *old == new => continue,
            None if empty => continue,
            Some(_) => meta_changes.updated.push(if empty { format!("{model} (removed)") } else { model.clone() }),
            None => meta_changes.created.push(model.clone()),
        }
        if empty {
            metadata.remove(&model);
        } else {
            metadata.insert(model.clone(), new);
        }
        changed_meta.insert(model);
    }

    if !q.dry_run {
        let mut tx = state.db.begin().await?;
        for c in creds.iter().filter(|c| new_creds.contains(&c.id)) {
            db::insert_credential(&mut *tx, c).await?;
        }
        for c in creds.iter().filter(|c| updated_creds.contains(&c.id)) {
            db::update_credential(&mut *tx, c).await?;
        }
        for g in guardrails.iter().filter(|g| new_guards.contains(&g.id)) {
            db::insert_guardrail(&mut *tx, g).await?;
        }
        for g in guardrails.iter().filter(|g| changed_guards.contains(&g.id)) {
            db::update_guardrail(&mut *tx, g).await?;
        }
        for d in deployments.iter().filter(|d| changed_deps.contains(&d.id)) {
            db::upsert_deployment(&mut *tx, d).await?;
        }
        for r in routers.iter().filter(|r| changed_routers.contains(&r.id)) {
            db::upsert_router(&mut *tx, r).await?;
        }
        for a in aliases.iter().filter(|a| changed_aliases.contains(&a.id)) {
            db::upsert_alias(&mut *tx, a).await?;
        }
        for k in keys.iter().filter(|k| changed_keys.contains(&k.id)) {
            db::update_key(&mut *tx, k).await?;
        }
        for model in &changed_meta {
            let m = metadata.get(model).cloned().unwrap_or_else(|| Value::Object(Map::new()));
            db::set_model_metadata(&mut *tx, model, &m).await?;
        }
        tx.commit().await?;
        broadcast_reload(&state, "configuration imported").await;
        tracing::info!(
            credentials = new_creds.len() + updated_creds.len(),
            guardrails = new_guards.len() + changed_guards.len(),
            deployments = changed_deps.len(),
            routers = changed_routers.len(),
            aliases = changed_aliases.len(),
            keys = changed_keys.len(),
            model_metadata = changed_meta.len(),
            "configuration imported"
        );
        let details = json!({
            "credentials": cred_changes, "guardrails": guard_changes, "deployments": dep_changes, "routers": router_changes,
            "aliases": alias_changes, "keys": key_changes, "model_metadata": meta_changes,
        });
        audit::Event::new("configuration", "import").details(details).record(&state, &actor).await;
    }
    Ok(Json(json!({
        "dry_run": q.dry_run,
        "credentials": cred_changes,
        "guardrails": guard_changes,
        "deployments": dep_changes,
        "routers": router_changes,
        "aliases": alias_changes,
        // Keys whose allowed models follow a renamed model.
        "keys": key_changes,
        "model_metadata": meta_changes,
        // New credentials that need their secrets entered before their deployments work.
        "needs_secrets": needs_secrets,
    })))
}
