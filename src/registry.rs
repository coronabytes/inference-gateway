//! In-memory snapshot of routing config and virtual keys, rebuilt from Postgres whenever any
//! replica publishes a change (and periodically as a safety net).

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Arc,
    time::Duration,
};

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::{
    config::Config,
    crypto::SecretBox,
    db,
    model::{
        AliasRow, AutoRouter, CredentialRow, CredentialSettings, Deployment, DeploymentRow, Mode, ModelAlias, Pricing, ProjectRow, RouterRow, ProviderKind,
        Rates, Secrets, VirtualKey,
    },
};

#[derive(Default)]
pub struct Snapshot {
    /// Enabled deployments grouped by public model name.
    pub deployments: HashMap<String, Vec<Arc<Deployment>>>,
    /// Enabled auto routers by public model name.
    pub routers: HashMap<String, Arc<AutoRouter>>,
    /// Enabled model aliases by their own model name; see `resolve`.
    pub aliases: HashMap<String, Arc<ModelAlias>>,
    pub keys: HashMap<[u8; 32], Arc<VirtualKey>>,
    /// Projects by id, for their budgets.
    pub projects: HashMap<Uuid, Arc<ProjectRow>>,
    /// Guardrails by name, ready to call.
    pub guardrails: BTreeMap<String, Arc<crate::guardrails::Active>>,
    /// Enabled MCP servers by name, ready to call.
    pub mcp_servers: HashMap<String, Arc<crate::mcp::McpServer>>,
    /// Enabled A2A agents by name, ready to call.
    pub a2a_agents: HashMap<String, Arc<crate::a2a::A2aAgent>>,
    /// Enabled tool routers by name.
    pub tool_routers: HashMap<String, Arc<crate::mcp::router::ToolRouter>>,
    pub loaded_at: Option<DateTime<Utc>>,
    pub errors: Vec<String>,
}

impl Snapshot {
    /// The model name a request for `model` is served by: an alias's target, otherwise `model`.
    pub fn resolve<'a>(&'a self, model: &'a str) -> &'a str {
        self.aliases.get(model).map_or(model, |a| a.target.as_str())
    }
}

pub struct Registry {
    snap: ArcSwap<Snapshot>,
}

impl Registry {
    pub fn new() -> Self {
        Self { snap: ArcSwap::from_pointee(Snapshot::default()) }
    }

    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.snap.load_full()
    }

    #[cfg(test)]
    pub fn set_snapshot(&self, snap: Snapshot) {
        self.snap.store(Arc::new(snap));
    }

    pub async fn reload(&self, pool: &sqlx::PgPool, cfg: &Config) -> Result<(), sqlx::Error> {
        let (rows, creds, keys, routers, aliases, projects, guardrails, mcp_servers, a2a_agents, tool_routers) = tokio::try_join!(
            db::list_deployments(pool),
            db::list_credentials(pool),
            db::list_keys(pool),
            db::list_routers(pool),
            db::list_aliases(pool),
            db::list_project_rows(pool),
            db::list_guardrails(pool),
            db::list_mcp_servers(pool),
            db::list_a2a_agents(pool),
            db::list_tool_routers(pool)
        )?;
        // Decrypt each credential once, however many deployments share it.
        let creds: HashMap<Uuid, Result<ResolvedCredential, String>> =
            creds.into_iter().map(|c| (c.id, resolve_credential(&c, &cfg.secret_box))).collect();
        let mut snap = Snapshot { loaded_at: Some(Utc::now()), ..Default::default() };
        // What aliases may target, enabled or not, by id.
        let targets: HashMap<Uuid, String> =
            rows.iter().map(|d| (d.id, d.model_name.clone())).chain(routers.iter().map(|r| (r.id, r.model_name.clone()))).collect();
        let mut built = Vec::new();
        for row in rows.into_iter().filter(|r| r.enabled) {
            let base_model = row.base_model.clone();
            match build_deployment(row, &creds) {
                Ok(d) => {
                    let keys = crate::prices::catalog_keys(d.provider, base_model.as_deref(), &d.upstream_model);
                    built.push((d, keys));
                }
                Err(e) => {
                    tracing::error!(error = %e, "skipping misconfigured deployment");
                    snap.errors.push(e);
                }
            }
        }
        // Catalog entries, for what /v1/models says about each model. Without them the models
        // are still served; they are only described less.
        let wanted: Vec<String> = built.iter().flat_map(|(_, keys)| keys.iter().cloned()).collect::<BTreeSet<_>>().into_iter().collect();
        let catalog: HashMap<String, Arc<serde_json::Value>> = match db::get_prices(pool, &wanted).await {
            Ok(found) => found.into_iter().map(|(k, v)| (k, Arc::new(v))).collect(),
            Err(e) => {
                tracing::warn!(error = %e, "could not read the price catalog");
                HashMap::new()
            }
        };
        for (mut d, keys) in built {
            d.catalog = keys.iter().find_map(|k| catalog.get(k)).cloned();
            snap.deployments.entry(d.model_name.clone()).or_default().push(Arc::new(d));
        }
        for r in routers.into_iter().filter(|r| r.enabled) {
            match build_router(r, &snap) {
                Ok(r) => {
                    snap.routers.insert(r.model_name.clone(), Arc::new(r));
                }
                Err(e) => {
                    tracing::error!(error = %e, "skipping misconfigured router");
                    snap.errors.push(e);
                }
            }
        }
        for a in aliases.into_iter().filter(|a| a.enabled) {
            match build_alias(a, &targets, &snap) {
                Ok(a) => {
                    snap.aliases.insert(a.model_name.clone(), Arc::new(a));
                }
                Err(e) => {
                    tracing::error!(error = %e, "skipping misconfigured alias");
                    snap.errors.push(e);
                }
            }
        }
        for k in keys {
            if let Ok(hash) = <[u8; 32]>::try_from(k.key_hash.as_slice()) {
                snap.keys.insert(hash, Arc::new(k));
            }
        }
        snap.projects = projects.into_iter().map(|p| (p.id, Arc::new(p))).collect();
        for g in guardrails {
            let resolved = match creds.get(&g.credential_id) {
                Some(Ok(c)) if crate::guardrails::GuardrailProvider::parse(&g.provider).map(|p| p.credential_provider()) == Some(c.provider.as_str()) => {
                    Ok(c)
                }
                Some(Ok(_)) => Err("its credential is for another provider".to_string()),
                Some(Err(e)) => Err(e.clone()),
                None => Err("its credential is gone".to_string()),
            };
            match resolved {
                Ok(c) => {
                    let active = crate::guardrails::Active { settings: c.settings.clone(), secrets: c.secrets.clone(), row: g };
                    snap.guardrails.insert(active.row.name.clone(), Arc::new(active));
                }
                Err(e) => {
                    let e = format!("guardrail {} ({}): {e}", g.id, g.name);
                    tracing::error!(error = %e, "skipping misconfigured guardrail");
                    snap.errors.push(e);
                }
            }
        }
        for s in mcp_servers.into_iter().filter(|s| s.enabled) {
            match crate::mcp::McpServer::build(s, &cfg.secret_box) {
                Ok(s) => {
                    snap.mcp_servers.insert(s.name.clone(), Arc::new(s));
                }
                Err(e) => {
                    tracing::error!(error = %e, "skipping misconfigured MCP server");
                    snap.errors.push(e);
                }
            }
        }
        for a in a2a_agents.into_iter().filter(|a| a.enabled) {
            match crate::a2a::A2aAgent::build(a, &cfg.secret_box) {
                Ok(a) => {
                    snap.a2a_agents.insert(a.name.clone(), Arc::new(a));
                }
                Err(e) => {
                    tracing::error!(error = %e, "skipping misconfigured A2A agent");
                    snap.errors.push(e);
                }
            }
        }
        for r in tool_routers.into_iter().filter(|r| r.enabled) {
            let r = crate::mcp::router::ToolRouter::build(r);
            snap.tool_routers.insert(r.name.clone(), Arc::new(r));
        }
        let n_dep: usize = snap.deployments.values().map(Vec::len).sum();
        tracing::debug!(deployments = n_dep, keys = snap.keys.len(), "registry reloaded");
        self.snap.store(Arc::new(snap));
        Ok(())
    }
}

pub fn decrypt_secrets(sb: &SecretBox, sealed: Option<&[u8]>) -> Result<Secrets, String> {
    match sealed {
        None => Ok(Secrets::default()),
        Some(b) => {
            let plain = sb.open(b)?;
            serde_json::from_slice(&plain).map_err(|e| format!("corrupt secrets: {e}"))
        }
    }
}

pub fn encrypt_secrets(sb: &SecretBox, s: &Secrets) -> Option<Vec<u8>> {
    let json = serde_json::to_vec(s).expect("secrets serialize");
    if json == b"{}" { None } else { Some(sb.seal(&json)) }
}

struct ResolvedCredential {
    provider: ProviderKind,
    settings: CredentialSettings,
    secrets: Secrets,
}

fn resolve_credential(c: &CredentialRow, sb: &SecretBox) -> Result<ResolvedCredential, String> {
    let provider = ProviderKind::parse(&c.provider).ok_or_else(|| "unknown provider".to_string())?;
    let secrets = decrypt_secrets(sb, c.secrets.as_deref())?;
    // Secrets saved by older versions may still be `env:NAME` references. Those are no longer read
    // from the environment, and must not be sent upstream as if they were the key itself.
    if secrets.values().any(|v| v.starts_with("env:")) {
        return Err(format!(
            "credential '{}' uses an env: reference, which is no longer supported; re-enter the secret on the Credentials page",
            c.name
        ));
    }
    Ok(ResolvedCredential { provider, settings: c.settings.0.clone(), secrets })
}

/// Routes are checked when the router is saved; models removed since then make the decision
/// fail over to the fallback, or the request fail, at request time.
fn build_router(r: RouterRow, snap: &Snapshot) -> Result<AutoRouter, String> {
    let ctx = |e: &str| format!("router {} ({}): {e}", r.id, r.model_name);
    if snap.deployments.contains_key(&r.model_name) {
        return Err(ctx("a deployment uses the same model name"));
    }
    let routes = r.routes.0;
    let fallback_route = r
        .fallback_route
        .filter(|f| routes.iter().any(|x| x.name == *f))
        .or_else(|| routes.first().map(|x| x.name.clone()))
        .ok_or_else(|| ctx("no routes"))?;
    Ok(AutoRouter {
        id: r.id,
        model_name: r.model_name,
        decision_model: r.decision_model,
        instructions: r.instructions,
        routes,
        fallback_route,
        tags: r.tags,
        sticky: r.sticky,
    })
}

/// An alias serves its target's model name as it is now. A target that is disabled serves nothing
/// under either name; one that is gone is reported here.
fn build_alias(a: AliasRow, targets: &HashMap<Uuid, String>, snap: &Snapshot) -> Result<ModelAlias, String> {
    let ctx = |e: &str| format!("alias {} ({}): {e}", a.id, a.model_name);
    if snap.deployments.contains_key(&a.model_name) || snap.routers.contains_key(&a.model_name) {
        return Err(ctx("a deployment or router uses the same model name"));
    }
    let target = targets.get(&a.target_id).ok_or_else(|| ctx("its target model was deleted"))?;
    Ok(ModelAlias { model_name: a.model_name, target: target.clone() })
}

fn build_deployment(
    row: DeploymentRow,
    creds: &HashMap<Uuid, Result<ResolvedCredential, String>>,
) -> Result<Deployment, String> {
    let ctx = |e: String| format!("deployment {} ({}): {e}", row.id, row.model_name);
    let provider = ProviderKind::parse(&row.provider).ok_or_else(|| ctx("unknown provider".into()))?;
    let mode = Mode::parse(&row.mode).ok_or_else(|| ctx(format!("unknown mode {}", row.mode)))?;
    let config = row.config.0;
    config.check_stored().map_err(ctx)?;
    // Once per deployment and header: the registry reloads every minute.
    static WARNED: std::sync::Mutex<std::collections::BTreeSet<(Uuid, String)>> = std::sync::Mutex::new(std::collections::BTreeSet::new());
    for h in config.credential_headers() {
        if !WARNED.lock().expect("warned lock").insert((row.id, h.to_ascii_lowercase())) {
            continue;
        }
        tracing::warn!(
            deployment = %row.id, model = %row.model_name, header = h,
            "deployment header carries a credential in plain text; move the secret to the credential's api_key \
             (edits to this deployment are refused until then)"
        );
    }
    let style = mode.style(provider, config.api_style).map_err(ctx)?;
    let cred = match creds.get(&row.credential_id) {
        Some(Ok(c)) => c,
        Some(Err(e)) => return Err(ctx(format!("credential {}: {e}", row.credential_id))),
        None => return Err(ctx(format!("credential {} not found", row.credential_id))),
    };
    if cred.provider != provider {
        return Err(ctx(format!("credential is for {}, deployment is {}", cred.provider.as_str(), provider.as_str())));
    }
    let (connection, secrets) = (cred.settings.clone(), cred.secrets.clone());
    let input = row.input_cost_per_mtok;
    Ok(Deployment {
        id: row.id,
        model_name: row.model_name,
        mode,
        provider,
        style,
        upstream_model: row.upstream_model,
        config,
        connection,
        secrets,
        weight: row.weight.max(1) as u32,
        priority: row.priority,
        rpm_limit: row.rpm_limit.map(|v| v as u64),
        tpm_limit: row.tpm_limit.map(|v| v as u64),
        timeout: Duration::from_secs(row.timeout_secs.max(1) as u64),
        tags: row.tags,
        project_ids: row.project_ids,
        pricing: Pricing {
            standard: Rates::new(input, row.output_cost_per_mtok, row.cache_read_cost_per_mtok, row.cache_write_cost_per_mtok),
            long_context: match (row.long_context_threshold, row.long_input_cost_per_mtok, row.long_output_cost_per_mtok) {
                (Some(threshold), Some(long_in), Some(long_out)) => Some((
                    threshold.max(1) as u64,
                    Rates::new(long_in, long_out, row.long_cache_read_cost_per_mtok, row.long_cache_write_cost_per_mtok),
                )),
                _ => None,
            },
            per_k_searches: row.cost_per_k_searches.unwrap_or(0.0),
        },
        catalog: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn credential(sb: &SecretBox, secrets: &Secrets) -> CredentialRow {
        CredentialRow {
            id: Uuid::nil(),
            name: "c".into(),
            provider: "openai".into(),
            secrets: encrypt_secrets(sb, secrets),
            settings: sqlx::types::Json(CredentialSettings::default()),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn legacy_env_references_are_rejected_not_sent_upstream() {
        let sb = SecretBox::from_base64(&base64::engine::general_purpose::STANDARD.encode([1u8; 32])).unwrap();
        let legacy = Secrets { api_key: Some("env:GW_SECRET_OPENAI".into()), ..Default::default() };
        let err = resolve_credential(&credential(&sb, &legacy), &sb).err().unwrap();
        assert!(err.contains("no longer supported"));

        let stored = Secrets { api_key: Some("sk-real".into()), ..Default::default() };
        let ok = resolve_credential(&credential(&sb, &stored), &sb).unwrap();
        assert_eq!(ok.secrets.api_key.as_deref(), Some("sk-real"));
    }
}
