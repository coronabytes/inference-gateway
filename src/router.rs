//! Deployment selection. The lowest priority tier serves first; within a tier a deployment is
//! picked at random by its weight, scaled down by its recent health (adaptive routing), and a
//! tier that is mostly unhealthy passes that share of its requests on to the next tier. A
//! preferred deployment (the one a conversation used before, see `affinity`) is kept while it is
//! healthy. Deployments that are cooling down or at their (cluster-wide) rate limits are skipped.

use std::{collections::HashSet, sync::Arc};

use uuid::Uuid;

use crate::{
    cache::{Health, Outcome},
    error::GatewayError,
    model::Deployment,
    state::AppState,
};

/// Below this many recent (decayed) calls, a deployment's health says too little to act on.
const MIN_CALLS: f64 = 5.0;
/// Every deployment keeps at least this share of its weight, so it is still tried and recovers.
const MIN_FACTOR: f64 = 0.05;
/// A conversation moves off its deployment once the deployment's health is below this.
const STICKY_MIN_FACTOR: f64 = 0.5;
/// A time to first token this far above its tier's median starts to cost a deployment traffic.
const SLOW: f64 = 1.5;

/// The share of its configured weight a deployment gets: less the more of its recent calls
/// failed (none at half of them), and less when its time to first token is well above the median
/// of its tier (`tier_ttft`). Without enough recent calls, all of it.
pub fn health_factor(h: &Health, tier_ttft: Option<f64>) -> f64 {
    let calls = h.ok + h.err;
    let errors = if calls < MIN_CALLS { 1.0 } else { 1.0 - 2.0 * h.err / calls };
    let latency = match (h.ttft_ms.filter(|_| h.ok >= MIN_CALLS), tier_ttft) {
        (Some(own), Some(median)) if own > SLOW * median => (SLOW * median / own).max(0.25),
        _ => 1.0,
    };
    (errors * latency).clamp(MIN_FACTOR, 1.0)
}

/// The health factors of one model's deployments, of the given priorities: each against the
/// median time to first token of its tier.
pub fn factors(priorities: &[i32], health: &[Health]) -> Vec<f64> {
    priorities
        .iter()
        .zip(health)
        .map(|(priority, h)| {
            let mut ttfts: Vec<f64> =
                priorities.iter().zip(health).filter(|(p, h)| *p == priority && h.ok >= MIN_CALLS).filter_map(|(_, h)| h.ttft_ms).collect();
            ttfts.sort_by(f64::total_cmp);
            // A median of one is the deployment itself: nothing to compare with.
            let median = (ttfts.len() >= 2).then(|| ttfts[ttfts.len() / 2]);
            health_factor(h, median)
        })
        .collect()
}

/// A deployment failed with a retryable error: cool it down and count it against its health.
pub async fn failed(state: &AppState, d: &Deployment) {
    tokio::join!(state.cache.set_cooldown(d.id, state.settings.get().cooldown()), async {
        if state.settings.get().adaptive_routing {
            state.cache.record_health(d.id, Outcome::Failed).await;
        }
    });
}

/// `preferred` (the deployment that served the conversation before) is kept when it is healthy,
/// has capacity and no better priority tier is available; otherwise normal balancing applies.
pub async fn pick(
    state: &AppState,
    candidates: &[Arc<Deployment>],
    exclude: &HashSet<Uuid>,
    preferred: Option<Uuid>,
) -> Result<Arc<Deployment>, GatewayError> {
    let pool: Vec<&Arc<Deployment>> = candidates.iter().filter(|d| !exclude.contains(&d.id)).collect();
    if pool.is_empty() {
        return Err(GatewayError::unavailable("no deployments left to try for this model"));
    }
    // With one deployment, cooldowns and health cannot change the pick: one that is cooling down
    // is tried anyway. Only its capacity is checked, which spares two Valkey round trips.
    if let [d] = pool[..] {
        return match has_capacity(state, d).await {
            Ok(()) => Ok(d.clone()),
            Err(retry) => Err(GatewayError::rate_limited("all deployments for this model are at capacity", retry)),
        };
    }

    let ids: Vec<Uuid> = pool.iter().map(|d| d.id).collect();
    let (cooling, health) = tokio::join!(state.cache.cooldowns(&ids), async {
        match state.settings.get().adaptive_routing {
            true => state.cache.health(&ids).await,
            false => vec![Health::default(); ids.len()],
        }
    });
    let factor = factors(&pool.iter().map(|d| d.priority).collect::<Vec<_>>(), &health);
    if let Some(pos) = preferred.and_then(|p| pool.iter().position(|d| d.id == p))
        && !cooling[pos]
        && factor[pos] >= STICKY_MIN_FACTOR
        && pool.iter().zip(&cooling).all(|(d, c)| *c || d.priority >= pool[pos].priority)
        && has_capacity(state, pool[pos]).await.is_ok()
    {
        return Ok(pool[pos].clone());
    }
    let mut healthy: Vec<usize> = (0..pool.len()).filter(|&i| !cooling[i]).collect();
    // If everything is cooling down, trying one beats failing outright.
    if healthy.is_empty() {
        healthy = (0..pool.len()).collect();
    }

    let mut min_retry = u64::MAX;
    for tier in tier_order(&pool, &healthy, &factor, random) {
        let mut remaining: Vec<usize> = healthy.iter().copied().filter(|&i| pool[i].priority == tier).collect();
        while !remaining.is_empty() {
            let weights: Vec<f64> = remaining.iter().map(|&i| pool[i].weight as f64 * factor[i]).collect();
            let d = pool[remaining.swap_remove(weighted_index(&weights, random()))];
            match has_capacity(state, d).await {
                Ok(()) => return Ok(d.clone()),
                Err(retry) => min_retry = min_retry.min(retry),
            }
        }
    }
    Err(GatewayError::rate_limited("all deployments for this model are at capacity", if min_retry == u64::MAX { 1 } else { min_retry }))
}

/// The priority tiers to try, best first. A tier keeps the share of requests its health allows
/// (its weight-averaged health factor) and passes the rest on to the next tier; tiers passed over
/// are still tried last, before the request fails. The last tier keeps everything.
fn tier_order(pool: &[&Arc<Deployment>], healthy: &[usize], factor: &[f64], mut random: impl FnMut() -> f64) -> Vec<i32> {
    let mut tiers: Vec<i32> = healthy.iter().map(|&i| pool[i].priority).collect();
    tiers.sort_unstable();
    tiers.dedup();
    let (mut kept, mut passed) = (Vec::new(), Vec::new());
    let last = tiers.len().saturating_sub(1);
    for (n, tier) in tiers.into_iter().enumerate() {
        let (mut weight, mut share) = (0.0, 0.0);
        for &i in healthy.iter().filter(|&&i| pool[i].priority == tier) {
            weight += pool[i].weight as f64;
            share += pool[i].weight as f64 * factor[i];
        }
        let health = if weight > 0.0 { share / weight } else { 1.0 };
        if n == last || random() < health {
            kept.push(tier);
        } else {
            passed.push(tier);
        }
    }
    kept.extend(passed);
    kept
}

/// A uniform random number in [0, 1).
fn random() -> f64 {
    (getrandom::u64().unwrap_or(0) >> 11) as f64 / (1u64 << 53) as f64
}

/// The index `r` (in [0, 1)) falls on when the weights are laid end to end.
fn weighted_index(weights: &[f64], r: f64) -> usize {
    let mut at = r * weights.iter().sum::<f64>();
    for (i, w) in weights.iter().enumerate() {
        if at < *w {
            return i;
        }
        at -= w;
    }
    weights.len() - 1
}

async fn has_capacity(state: &AppState, d: &Deployment) -> Result<(), u64> {
    if let Some(limit) = d.tpm_limit {
        let r = state.cache.rate_check(&format!("d:{}:tpm", d.id), limit, 0).await;
        if !r.allowed {
            return Err(r.retry_after);
        }
    }
    if let Some(limit) = d.rpm_limit {
        let r = state.cache.rate_check(&format!("d:{}:rpm", d.id), limit, 1).await;
        if !r.allowed {
            return Err(r.retry_after);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn health(ok: f64, err: f64, ttft_ms: Option<f64>) -> Health {
        Health { ok, err, ttft_ms }
    }

    #[test]
    fn errors_and_slowness_cost_weight() {
        // Too few calls to judge: full weight.
        assert_eq!(health_factor(&health(1.0, 3.0, None), None), 1.0);
        assert_eq!(health_factor(&health(100.0, 0.0, Some(400.0)), Some(400.0)), 1.0);
        // 10% errors leave 80%, half failing leaves the minimum.
        assert!((health_factor(&health(90.0, 10.0, None), None) - 0.8).abs() < 1e-9);
        assert_eq!(health_factor(&health(50.0, 50.0, None), None), MIN_FACTOR);
        // Up to 1.5x the tier's median is fine; 3x halves the weight, and errors compound.
        assert_eq!(health_factor(&health(100.0, 0.0, Some(600.0)), Some(400.0)), 1.0);
        assert!((health_factor(&health(100.0, 0.0, Some(1200.0)), Some(400.0)) - 0.5).abs() < 1e-9);
        assert!((health_factor(&health(90.0, 10.0, Some(1200.0)), Some(400.0)) - 0.4).abs() < 1e-9);
        // Far slower still keeps a quarter.
        assert!((health_factor(&health(100.0, 0.0, Some(40_000.0)), Some(400.0)) - 0.25).abs() < 1e-9);
    }

    #[test]
    fn weighted_index_lays_weights_end_to_end() {
        let w = [1.0, 0.0, 3.0];
        assert_eq!(weighted_index(&w, 0.0), 0);
        assert_eq!(weighted_index(&w, 0.24), 0);
        assert_eq!(weighted_index(&w, 0.26), 2);
        assert_eq!(weighted_index(&w, 0.999), 2);
    }

    fn deployment(priority: i32, weight: u32) -> Arc<Deployment> {
        use std::time::Duration;

        use crate::model::{ApiStyle, CredentialSettings, DeploymentConfig, Mode, Pricing, ProviderKind, Rates, Secrets};
        Arc::new(Deployment {
            id: Uuid::now_v7(),
            model_name: "m".into(),
            mode: Mode::Chat,
            provider: ProviderKind::Openai,
            style: ApiStyle::Chat,
            upstream_model: "m".into(),
            config: DeploymentConfig::default(),
            connection: CredentialSettings::default(),
            secrets: Secrets::default(),
            weight,
            priority,
            rpm_limit: None,
            tpm_limit: None,
            timeout: Duration::from_secs(1),
            pricing: Pricing { standard: Rates::new(1.0, 2.0, None, None), long_context: None, per_k_searches: 0.0 },
            tags: vec![],
            project_ids: vec![],
            catalog: None,
        })
    }

    #[test]
    fn the_tier_median_is_over_its_known_peers() {
        let h = [
            health(10.0, 0.0, Some(300.0)),
            health(10.0, 0.0, Some(400.0)),
            health(10.0, 0.0, Some(2400.0)),
            // Another tier: not compared with the first three, and alone in its own.
            health(10.0, 0.0, Some(9000.0)),
        ];
        let f = factors(&[0, 0, 0, 1], &h);
        assert_eq!(&f[..2], &[1.0, 1.0]);
        assert!((f[2] - 0.25).abs() < 1e-9, "1.5 x 400 / 2400");
        assert_eq!(f[3], 1.0);
    }

    #[test]
    fn unhealthy_tiers_pass_requests_on() {
        let ds = [deployment(0, 3), deployment(0, 1), deployment(1, 1), deployment(2, 1)];
        let pool: Vec<&Arc<Deployment>> = ds.iter().collect();
        let all = [0, 1, 2, 3];
        // Healthy: best tier first.
        assert_eq!(tier_order(&pool, &all, &[1.0; 4], || 0.99), vec![0, 1, 2]);
        // Tier 0 at (3 x 0.2 + 1 x 0.2) / 4 = 20% health keeps a draw below 0.2 and passes 0.5 on.
        let sick = [0.2, 0.2, 1.0, 1.0];
        assert_eq!(tier_order(&pool, &all, &sick, || 0.1), vec![0, 1, 2]);
        assert_eq!(tier_order(&pool, &all, &sick, || 0.5), vec![1, 2, 0]);
        // The last tier keeps everything, however unhealthy.
        assert_eq!(tier_order(&pool, &[3], &[0.05; 4], || 0.99), vec![2]);
    }
}
