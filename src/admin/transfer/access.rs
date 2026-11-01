//! Projects and virtual keys in a configuration document. They name their guardrails, and keys
//! their project, by name.
//!
//! A key is exported with the SHA-256 of its secret, never the secret: imported on another gateway,
//! it works with the same secret. A key matches by id, then by that hash; an existing key keeps its
//! own hash, so a key regenerated since the export is not brought back. Spend is not exported.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{Changes, bad, claim};
use crate::{
    admin::{R, keys::check_budget, valid_model_name},
    guardrails::GuardrailRow,
    model::{KeyScope, ProjectRow, PromptLogging, VirtualKey, normalize_tags},
};

fn yes() -> bool {
    true
}

fn empty_object() -> Value {
    json!({})
}

fn guardrail_names(ids: &[Uuid], guardrails: &[GuardrailRow]) -> Vec<String> {
    let mut names: Vec<String> = ids.iter().filter_map(|id| guardrails.iter().find(|g| g.id == *id).map(|g| g.name.clone())).collect();
    names.sort();
    names
}

/// Guardrail ids by name; each must exist, in the file or on this gateway.
fn guardrail_ids(names: &[String], guardrails: &[GuardrailRow], what: &str) -> R<Vec<Uuid>> {
    let mut ids = Vec::new();
    for name in names.iter().map(|n| n.trim()) {
        let g = guardrails
            .iter()
            .find(|g| g.name == name)
            .ok_or_else(|| bad(format!("{what} uses guardrail '{name}', which is neither in the file nor on this gateway")))?;
        ids.push(g.id);
    }
    ids.sort();
    ids.dedup();
    Ok(ids)
}

// ---------------------------------------------------------------- projects

#[derive(Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProjectDoc {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<Uuid>,
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(default = "yes")]
    enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_budget_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    budget_period: Option<String>,
    /// The names of the guardrails every request of its keys goes through.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    guardrails: Vec<String>,
    #[serde(default = "yes")]
    web_search: bool,
    #[serde(default)]
    prompt_logging: PromptLogging,
}

pub(super) fn project_doc(p: &ProjectRow, guardrails: &[GuardrailRow]) -> ProjectDoc {
    ProjectDoc {
        id: Some(p.id),
        name: p.name.clone(),
        description: p.description.clone(),
        enabled: p.enabled,
        max_budget_usd: p.max_budget_usd,
        budget_period: p.budget_period.clone(),
        guardrails: guardrail_names(&p.guardrail_ids, guardrails),
        web_search: p.web_search,
        prompt_logging: p.prompt_logging,
    }
}

/// Projects: by id, then by name. Returns what changed and the ids to write.
pub(super) fn import_projects(
    docs: Vec<ProjectDoc>,
    projects: &mut Vec<ProjectRow>,
    guardrails: &[GuardrailRow],
    now: DateTime<Utc>,
) -> R<(Changes, HashSet<Uuid>)> {
    let (mut changes, mut changed, mut claimed) = (Changes::default(), HashSet::new(), HashSet::new());
    let file_ids: HashSet<Uuid> = docs.iter().filter_map(|d| d.id).collect();
    for d in docs {
        let name = d.name.trim().to_string();
        let what = format!("project '{name}'");
        if name.is_empty() || name.len() > 100 {
            return Err(bad(format!("{what}: the name must be 1-100 characters")));
        }
        let description = d.description.as_deref().map(str::trim).filter(|x| !x.is_empty()).map(str::to_string);
        if description.as_ref().is_some_and(|x| x.len() > 1000) {
            return Err(bad(format!("{what}: the description can be at most 1000 characters")));
        }
        check_budget(d.max_budget_usd, d.budget_period.as_deref()).map_err(|e| bad(format!("{what}: {}", e.1)))?;
        let guardrail_ids = guardrail_ids(&d.guardrails, guardrails, &what)?;
        let found =
            d.id.and_then(|id| projects.iter().position(|x| x.id == id))
                .or_else(|| projects.iter().position(|x| x.name == name && !claimed.contains(&x.id) && !file_ids.contains(&x.id)));
        let row = |id: Uuid, spend_reset_at: Option<DateTime<Utc>>, created_at: DateTime<Utc>| ProjectRow {
            id,
            name: name.clone(),
            description: description.clone(),
            enabled: d.enabled,
            max_budget_usd: d.max_budget_usd,
            budget_period: d.budget_period.clone(),
            spend_reset_at,
            guardrail_ids: guardrail_ids.clone(),
            web_search: d.web_search,
            prompt_logging: d.prompt_logging,
            created_at,
            updated_at: now,
        };
        match found {
            Some(ix) => {
                claim(&mut claimed, projects[ix].id, &what)?;
                let x = &projects[ix];
                let new = row(x.id, x.spend_reset_at, x.created_at);
                if project_doc(x, guardrails) == project_doc(&new, guardrails) {
                    changes.unchanged += 1;
                } else {
                    changes.updated.push(if x.name == name { name.clone() } else { format!("{name}, renamed from {}", x.name) });
                    changed.insert(new.id);
                    projects[ix] = new;
                }
            }
            None => {
                let new = row(d.id.unwrap_or_else(Uuid::now_v7), None, now);
                claimed.insert(new.id);
                changed.insert(new.id);
                changes.created.push(name.clone());
                projects.push(new);
            }
        }
    }
    let mut names = HashSet::new();
    if let Some(p) = projects.iter().find(|p| !names.insert(p.name.as_str())) {
        return Err(bad(format!("project '{}': another project on this gateway has that name", p.name)));
    }
    Ok((changes, changed))
}

// ---------------------------------------------------------------- keys

#[derive(Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct KeyDoc {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<Uuid>,
    name: String,
    /// The SHA-256 of the key's secret, in hex: imported, the key works with the same secret. Never the secret.
    key_hash: String,
    /// What the console shows of the key, such as `sk-gw-1a2b…9z8y`.
    key_prefix: String,
    #[serde(default)]
    scope: KeyScope,
    /// The name of the project it is grouped under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project: Option<String>,
    #[serde(default)]
    allowed_models: Vec<String>,
    #[serde(default)]
    allowed_tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rpm_limit: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tpm_limit: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_budget_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    budget_period: Option<String>,
    #[serde(default = "yes")]
    enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_at: Option<DateTime<Utc>>,
    #[serde(default = "empty_object")]
    metadata: Value,
    /// The names of the guardrails every request of the key goes through, on top of its project's.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    guardrails: Vec<String>,
    #[serde(default = "yes")]
    web_search: bool,
    #[serde(default)]
    prompt_logging: bool,
}

pub(super) fn key_doc(k: &VirtualKey, projects: &[ProjectRow], guardrails: &[GuardrailRow]) -> KeyDoc {
    KeyDoc {
        id: Some(k.id),
        name: k.name.clone(),
        key_hash: hex::encode(&k.key_hash),
        key_prefix: k.key_prefix.clone(),
        scope: k.scope,
        project: k.project_id.and_then(|id| projects.iter().find(|p| p.id == id).map(|p| p.name.clone())),
        allowed_models: k.allowed_models.clone(),
        allowed_tags: k.allowed_tags.clone(),
        rpm_limit: k.rpm_limit,
        tpm_limit: k.tpm_limit,
        max_budget_usd: k.max_budget_usd,
        budget_period: k.budget_period.clone(),
        enabled: k.enabled,
        expires_at: k.expires_at,
        metadata: k.metadata.clone(),
        guardrails: guardrail_names(&k.guardrail_ids, guardrails),
        web_search: k.web_search,
        prompt_logging: k.prompt_logging,
    }
}

/// Keys: by id, then by the hash of their secret. `changed` gets the ids of the existing keys to
/// update; returns the changes, the ids of the new keys and of every key the file matched.
pub(super) fn import_keys(
    docs: Vec<KeyDoc>,
    keys: &mut Vec<VirtualKey>,
    projects: &[ProjectRow],
    guardrails: &[GuardrailRow],
    changed: &mut HashSet<Uuid>,
    now: DateTime<Utc>,
) -> R<(Changes, HashSet<Uuid>, HashSet<Uuid>)> {
    let (mut changes, mut created, mut claimed) = (Changes::default(), HashSet::new(), HashSet::new());
    for d in docs {
        let name = d.name.trim().to_string();
        let what = format!("key '{name}'");
        if name.is_empty() || name.len() > 200 {
            return Err(bad(format!("{what}: the name must be 1-200 characters")));
        }
        let hash = hex::decode(d.key_hash.trim())
            .ok()
            .filter(|h| h.len() == 32)
            .ok_or_else(|| bad(format!("{what}: key_hash must be 64 hex digits")))?;
        let prefix = d.key_prefix.trim().to_string();
        if prefix.is_empty() || prefix.len() > 64 {
            return Err(bad(format!("{what}: key_prefix must be 1-64 characters")));
        }
        if d.rpm_limit.is_some_and(|v| v < 1) || d.tpm_limit.is_some_and(|v| v < 1) {
            return Err(bad(format!("{what}: limits must be positive")));
        }
        check_budget(d.max_budget_usd, d.budget_period.as_deref()).map_err(|e| bad(format!("{what}: {}", e.1)))?;
        let project = match d.project.as_deref().map(str::trim) {
            None => None,
            Some(p) => Some(
                projects
                    .iter()
                    .find(|x| x.name == p)
                    .ok_or_else(|| bad(format!("{what} is in project '{p}', which is neither in the file nor on this gateway")))?,
            ),
        };
        match (project, d.scope) {
            (Some(_), s) if !s.fits_project() => {
                return Err(bad(format!("{what}: a key with the {} scope stays outside projects", s.as_str())));
            }
            (None, KeyScope::Project) => return Err(bad(format!("{what}: a key with the project scope needs a project"))),
            _ => {}
        }
        let mut models = d.allowed_models.clone();
        if models.iter().any(|m| !valid_model_name(m)) {
            return Err(bad(format!("{what}: invalid model name in allowed_models")));
        }
        models.sort();
        models.dedup();
        let tags = normalize_tags(d.allowed_tags.clone()).map_err(|e| bad(format!("{what}: {e}")))?;
        let guardrail_ids = guardrail_ids(&d.guardrails, guardrails, &what)?;
        let found = d.id.and_then(|id| keys.iter().position(|k| k.id == id)).or_else(|| keys.iter().position(|k| k.key_hash == hash));
        let fill = |k: &mut VirtualKey| {
            k.name = name.clone();
            k.scope = d.scope;
            k.project_id = project.map(|p| p.id);
            k.project_active = project.is_none_or(|p| p.enabled);
            k.allowed_models = models.clone();
            k.allowed_tags = tags.clone();
            k.rpm_limit = d.rpm_limit;
            k.tpm_limit = d.tpm_limit;
            k.max_budget_usd = d.max_budget_usd;
            k.budget_period = d.budget_period.clone();
            k.enabled = d.enabled;
            k.expires_at = d.expires_at;
            k.metadata = d.metadata.clone();
            k.guardrail_ids = guardrail_ids.clone();
            k.web_search = d.web_search;
            k.prompt_logging = d.prompt_logging;
            k.updated_at = now;
        };
        match found {
            Some(ix) => {
                claim(&mut claimed, keys[ix].id, &what)?;
                // Its own secret stays: one regenerated since the export is not brought back.
                let before = key_doc(&keys[ix], projects, guardrails);
                fill(&mut keys[ix]);
                if key_doc(&keys[ix], projects, guardrails) == before {
                    changes.unchanged += 1;
                } else {
                    changes.updated.push(if before.name == name { name.clone() } else { format!("{name}, renamed from {}", before.name) });
                    changed.insert(keys[ix].id);
                }
            }
            None => {
                let mut k = VirtualKey {
                    id: d.id.unwrap_or_else(Uuid::now_v7),
                    name: String::new(),
                    key_hash: hash,
                    key_prefix: prefix,
                    scope: KeyScope::Inference,
                    project_id: None,
                    project_active: true,
                    allowed_models: Vec::new(),
                    allowed_tags: Vec::new(),
                    rpm_limit: None,
                    tpm_limit: None,
                    max_budget_usd: None,
                    budget_period: None,
                    spend_usd: 0.0,
                    spend_reset_at: None,
                    enabled: true,
                    expires_at: None,
                    metadata: json!({}),
                    guardrail_ids: Vec::new(),
                    web_search: true,
                    prompt_logging: false,
                    created_at: now,
                    updated_at: now,
                };
                fill(&mut k);
                claimed.insert(k.id);
                created.insert(k.id);
                changes.created.push(name.clone());
                keys.push(k);
            }
        }
    }
    let mut hashes = HashSet::new();
    if let Some(k) = keys.iter().find(|k| !hashes.insert(k.key_hash.as_slice())) {
        return Err(bad(format!("key '{}': another key has the same secret", k.name)));
    }
    Ok((changes, created, claimed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str, hash: u8) -> VirtualKey {
        let mut k = serde_json::from_value::<KeyDoc>(json!({ "name": name, "key_hash": hex::encode([hash; 32]), "key_prefix": "sk-gw-…" }))
            .unwrap();
        k.id = Some(Uuid::now_v7());
        let mut keys = Vec::new();
        import_keys(vec![k], &mut keys, &[], &[], &mut HashSet::new(), Utc::now()).unwrap();
        keys.pop().unwrap()
    }

    fn doc(k: &VirtualKey) -> Value {
        serde_json::to_value(key_doc(k, &[], &[])).unwrap()
    }

    #[test]
    fn keys_match_by_id_then_secret_and_keep_their_own_secret() {
        let mut keys = vec![key("a", 1), key("b", 2)];
        let (a, b) = (keys[0].clone(), keys[1].clone());
        // By id, with another secret in the file: the key keeps its own, regenerated since.
        let mut renamed = doc(&a);
        renamed["name"] = json!("a2");
        renamed["key_hash"] = json!(hex::encode([9; 32]));
        // By its secret, without an id.
        let mut by_hash = doc(&b);
        by_hash.as_object_mut().unwrap().remove("id");
        // New.
        let new = json!({ "name": "c", "key_hash": hex::encode([3; 32]), "key_prefix": "sk-gw-…", "rpm_limit": 5 });
        let docs: Vec<KeyDoc> = [renamed, by_hash, new].into_iter().map(|v| serde_json::from_value(v).unwrap()).collect();
        let mut changed = HashSet::new();
        let (changes, created, matched) = import_keys(docs, &mut keys, &[], &[], &mut changed, Utc::now()).unwrap();
        assert_eq!(
            (changes.created, changes.updated, changes.unchanged),
            (vec!["c".to_string()], vec!["a2, renamed from a".to_string()], 1)
        );
        assert_eq!(keys[0].key_hash, a.key_hash);
        assert_eq!((matched.len(), created.len(), changed.len()), (3, 1, 1));
        assert_eq!(keys[2].rpm_limit, Some(5));

        // Two keys cannot share a secret, and a project key needs its project.
        let twin = json!({ "name": "d", "key_hash": hex::encode([2; 32]), "key_prefix": "x", "id": Uuid::now_v7() });
        let e = import_keys(vec![serde_json::from_value(twin).unwrap()], &mut keys.clone(), &[], &[], &mut HashSet::new(), Utc::now());
        assert!(e.is_ok(), "matched by id first: an unknown id with a known secret is that key");
        let scoped = json!({ "name": "e", "key_hash": hex::encode([4; 32]), "key_prefix": "x", "scope": "project" });
        let e =
            import_keys(vec![serde_json::from_value(scoped).unwrap()], &mut keys, &[], &[], &mut HashSet::new(), Utc::now()).unwrap_err();
        assert!(e.1.contains("needs a project"));
    }
}
