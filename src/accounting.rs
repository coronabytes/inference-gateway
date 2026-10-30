//! Per-request bookkeeping. A [`Tracker`] travels with the request (and with the response
//! stream) and emits a [`RequestRecord`] when dropped, so client disconnects are accounted too.
//! A routed request also gets a [`Rollup`]: the parent entry of its calls, emitted once the
//! last of them has finished. Both carry the call's trace span (see `telemetry`), ended with
//! their record.

use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

use chrono::{DateTime, Utc};
use opentelemetry::Context;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::{
    cache::Outcome,
    error::GatewayError,
    ir::Usage,
    labels::Labels,
    model::{Deployment, ProviderKind, VirtualKey},
    state::AppState,
    telemetry,
};

#[derive(Debug, Clone)]
pub struct RequestRecord {
    pub id: Uuid,
    pub ts: DateTime<Utc>,
    pub key_id: Option<Uuid>,
    /// For telemetry; the request log has the key's id.
    pub key_name: Option<String>,
    /// The key's project when the request was made; its spend counts there, even if the key moves.
    pub project_id: Option<Uuid>,
    pub model: String,
    pub deployment_id: Option<Uuid>,
    pub provider: Option<ProviderKind>,
    pub api: &'static str,
    pub stream: bool,
    pub status_code: u16,
    pub latency_ms: u64,
    pub ttft_ms: Option<u64>,
    pub usage: Usage,
    pub cost_usd: f64,
    pub attempts: u32,
    pub error: Option<String>,
    /// The router entry this call belongs to. Its cost is counted there, not here.
    pub parent_id: Option<Uuid>,
    /// The client's labels (see `labels`); a routed request's calls carry the router's.
    pub labels: Labels,
    /// What the guardrails did (see `GuardrailCharge::summary`), on the request's own entry.
    pub guardrails: Option<serde_json::Value>,
    /// The guardrails' part of `cost_usd`.
    pub guardrail_cost_usd: f64,
    /// The searches' part of `cost_usd` (`usage.search_units` of them).
    pub search_cost_usd: f64,
}

/// What screening a request with guardrails cost and found, for its entry in the request log.
#[derive(Debug, Clone, Default)]
pub struct GuardrailCharge {
    pub cost_usd: f64,
    /// `{"applied": [names], "action": "masked", "findings": ["pii:NAME"]}`; never matched text.
    pub summary: Option<serde_json::Value>,
}

pub struct Tracker {
    tx: mpsc::Sender<RequestRecord>,
    pub id: Uuid,
    start: Instant,
    ts: DateTime<Utc>,
    key_id: Option<Uuid>,
    key_name: Option<String>,
    project_id: Option<Uuid>,
    labels: Labels,
    model: String,
    api: &'static str,
    stream: bool,
    /// This call's trace span.
    cx: Context,
    deployment: Option<Arc<Deployment>>,
    attempts: u32,
    status: u16,
    ttft_ms: Option<u64>,
    usage: Usage,
    error: Option<String>,
    /// Set for the calls of a routed request; `bool` = this call decides the router's status.
    parent: Option<(Arc<Rollup>, bool)>,
    /// The upstream reported this request's final usage. Until then `usage` may be partial.
    usage_reported: bool,
    /// Input tokens to charge if the upstream never reports usage (see `estimated_usage`).
    input_estimate: u64,
    /// Bytes of generated text, tool arguments and reasoning streamed to the client so far.
    output_bytes: u64,
    /// The upstream stream ran to its end (rather than the client going away first).
    stream_ended: bool,
    guardrails: GuardrailCharge,
}

/// Rough tokens for a number of bytes of text, for when a provider reports no usage. About four
/// bytes per token for English; rounded up so estimates never charge nothing for something.
pub fn estimate_tokens(bytes: usize) -> u64 {
    (bytes as u64).div_ceil(4)
}

impl Tracker {
    pub fn new(state: &AppState, key: Option<&VirtualKey>, labels: &Labels, model: &str, api: &'static str, stream: bool) -> Self {
        let mut t = Self::with_parent(state, key.map(|k| k.id), key.map(|k| k.name.clone()), model, api, stream, None);
        t.project_id = key.and_then(|k| k.project_id);
        t.labels = labels.clone();
        t
    }

    fn with_parent(
        state: &AppState,
        key_id: Option<Uuid>,
        key_name: Option<String>,
        model: &str,
        api: &'static str,
        stream: bool,
        parent: Option<&Context>,
    ) -> Self {
        Self {
            tx: state.records.clone(),
            id: Uuid::now_v7(),
            start: Instant::now(),
            ts: Utc::now(),
            key_id,
            key_name,
            project_id: None,
            labels: Labels::default(),
            model: model.to_string(),
            api,
            stream,
            cx: telemetry::start_call(api, model, parent),
            deployment: None,
            attempts: 0,
            status: 200,
            ttft_ms: None,
            usage: Usage::default(),
            error: None,
            parent: None,
            usage_reported: false,
            input_estimate: 0,
            output_bytes: 0,
            stream_ended: false,
            guardrails: GuardrailCharge::default(),
        }
    }

    /// The guardrails this request went through; their cost is added to the request's.
    pub fn guardrails(&mut self, charge: GuardrailCharge) {
        self.guardrails = charge;
    }

    /// What to charge for input if the upstream reports no usage: the request body's size.
    pub fn estimate_input(&mut self, body_bytes: usize) {
        self.input_estimate = estimate_tokens(body_bytes);
    }

    /// Generated content sent on to the client, counted for the output estimate.
    pub fn streamed(&mut self, bytes: usize) {
        self.output_bytes += bytes as u64;
    }

    /// The upstream's stream ended (with or without a usage report).
    pub fn stream_ended(&mut self) {
        self.stream_ended = true;
    }

    /// A call made by a routed request. `decides_status`: the router's entry takes this call's
    /// status (the routed call), rather than only its usage and cost (the decision).
    pub fn child(state: &AppState, parent: &Arc<Rollup>, decides_status: bool, model: &str, api: &'static str, stream: bool) -> Self {
        let mut t = Self::with_parent(state, parent.key_id, parent.key_name.clone(), model, api, stream, Some(&parent.cx));
        t.project_id = parent.project_id;
        t.labels = parent.labels.clone();
        t.parent = Some((parent.clone(), decides_status));
        t
    }

    /// The call goes to `d`. Returns the attempt's trace span (see `telemetry::start_attempt`).
    pub fn attempt(&mut self, d: Arc<Deployment>) -> Context {
        self.attempts += 1;
        let cx = telemetry::start_attempt(&self.cx, self.api, &d);
        self.deployment = Some(d);
        cx
    }

    pub fn first_token(&mut self) {
        if self.ttft_ms.is_none() {
            self.ttft_ms = Some(self.start.elapsed().as_millis() as u64);
        }
    }

    /// The upstream's final usage report. An empty one does not count as a report: upstreams
    /// that leave usage out (or a body without it) are estimated instead of charged nothing.
    /// The whole usage. Web searches a stream reported before it (see `searched`) are kept.
    pub fn set_usage(&mut self, u: Usage) {
        self.usage = Usage { search_units: self.usage.search_units.max(u.search_units), ..u };
        self.usage_reported |= u.total() > 0 || u.search_units > 0;
    }

    /// Web searches a passthrough stream reported apart from its usage (Chat Completions).
    pub fn searched(&mut self, n: u64) {
        self.usage.search_units = self.usage.search_units.max(n);
    }

    /// Part of the usage (Anthropic streams report input first and output at the end).
    pub fn merge_usage(&mut self, u: &Usage) {
        self.usage.merge_max(u);
    }

    /// The last part of the usage has arrived.
    pub fn usage_complete(&mut self) {
        self.usage_reported |= self.usage.total() > 0;
    }

    /// Usage to charge for a request whose upstream never reported its final usage: a client
    /// that closes the connection before the usage event (or a non-streamed request whose
    /// response it stops waiting for) must not get the generation for free. Anything the
    /// upstream did report is kept; the rest is estimated from the request and the streamed
    /// output. `None` when nothing needs estimating: usage was reported, no deployment was
    /// reached, or the upstream refused the request before producing anything.
    fn estimated_usage(&self) -> Option<Usage> {
        if self.usage_reported || self.deployment.is_none() || (self.status >= 400 && self.output_bytes == 0) {
            return None;
        }
        let mut u = self.usage;
        if u.total_input() == 0 {
            u.input_tokens = self.input_estimate;
        }
        u.output_tokens = u.output_tokens.max(estimate_tokens(self.output_bytes as usize));
        (u.total() > 0).then_some(u)
    }

    pub fn fail(&mut self, e: &GatewayError) {
        self.status = e.status.as_u16();
        self.error = Some(crate::util::truncate(&e.message, 500));
    }

    pub fn fail_status(&mut self, status: u16, msg: impl Into<String>) {
        self.status = status;
        self.error = Some(crate::util::truncate(&msg.into(), 500));
    }
}

impl Drop for Tracker {
    fn drop(&mut self) {
        if let Some(u) = self.estimated_usage() {
            self.usage = u;
            let note = if self.stream_ended || !self.stream {
                "the upstream reported no usage; tokens are estimated"
            } else {
                // 499, as nginx logs it: the client closed the request before it finished.
                if self.status < 400 {
                    self.status = 499;
                }
                "client disconnected before the upstream reported usage; tokens are estimated"
            };
            if self.error.is_none() {
                self.error = Some(note.into());
            }
        }
        let guardrails = std::mem::take(&mut self.guardrails);
        let cost = self.deployment.as_ref().map_or(0.0, |d| d.pricing.cost(&self.usage)) + guardrails.cost_usd;
        let rec = RequestRecord {
            id: self.id,
            ts: self.ts,
            key_id: self.key_id,
            key_name: self.key_name.take(),
            project_id: self.project_id,
            model: std::mem::take(&mut self.model),
            deployment_id: self.deployment.as_ref().map(|d| d.id),
            provider: self.deployment.as_ref().map(|d| d.provider),
            api: self.api,
            stream: self.stream,
            status_code: self.status,
            latency_ms: self.start.elapsed().as_millis() as u64,
            ttft_ms: self.ttft_ms,
            usage: self.usage,
            cost_usd: cost,
            attempts: self.attempts.max(1),
            error: self.error.take(),
            parent_id: self.parent.as_ref().map(|(p, _)| p.id),
            labels: std::mem::take(&mut self.labels),
            guardrails: guardrails.summary,
            guardrail_cost_usd: guardrails.cost_usd,
            search_cost_usd: self.deployment.as_ref().map_or(0.0, |d| d.pricing.search_cost(&self.usage)),
        };
        telemetry::end_call(&self.cx, &rec, self.deployment.as_ref().map(|d| d.upstream_model.as_str()));
        if let Some((parent, decides_status)) = self.parent.take() {
            parent.add(&rec, decides_status);
        }
        if self.tx.try_send(rec).is_err() {
            tracing::warn!("request record queue full; dropping record");
        }
    }
}

#[derive(Default)]
struct RollupState {
    usage: Usage,
    cost_usd: f64,
    search_cost_usd: f64,
    guardrails: GuardrailCharge,
    attempts: u32,
    status: Option<u16>,
    error: Option<String>,
    ttft_ms: Option<u64>,
}

/// The entry of a routed request: the router's model name, with the usage and cost of its calls
/// (the decision and the routed call) summed, and the routed call's status and attempts. Each call
/// holds an `Arc`, so the entry is emitted when the last one finishes, streams included.
pub struct Rollup {
    tx: mpsc::Sender<RequestRecord>,
    pub id: Uuid,
    start: Instant,
    ts: DateTime<Utc>,
    key_id: Option<Uuid>,
    key_name: Option<String>,
    project_id: Option<Uuid>,
    labels: Labels,
    model: String,
    api: &'static str,
    stream: bool,
    /// The router's trace span, parent of its calls' spans.
    cx: Context,
    state: Mutex<RollupState>,
}

impl Rollup {
    pub fn new(state: &AppState, key: Option<&VirtualKey>, labels: &Labels, model: &str, api: &'static str, stream: bool) -> Arc<Self> {
        Arc::new(Self {
            tx: state.records.clone(),
            id: Uuid::now_v7(),
            start: Instant::now(),
            ts: Utc::now(),
            key_id: key.map(|k| k.id),
            key_name: key.map(|k| k.name.clone()),
            project_id: key.and_then(|k| k.project_id),
            labels: labels.clone(),
            model: model.to_string(),
            api,
            stream,
            cx: telemetry::start_call(api, model, None),
            state: Mutex::default(),
        })
    }

    /// The guardrails the routed request went through, before its decision; their cost is added
    /// to the request's.
    pub fn guardrails(&self, charge: GuardrailCharge) {
        self.state.lock().expect("rollup lock").guardrails = charge;
    }

    /// The request failed before or outside its calls (validation, key limits).
    pub fn fail(&self, e: &GatewayError) {
        let mut s = self.state.lock().expect("rollup lock");
        s.status = Some(e.status.as_u16());
        s.error = Some(crate::util::truncate(&e.message, 500));
    }

    fn add(&self, child: &RequestRecord, decides_status: bool) {
        let mut s = self.state.lock().expect("rollup lock");
        s.usage.add(&child.usage);
        s.cost_usd += child.cost_usd;
        s.search_cost_usd += child.search_cost_usd;
        if decides_status {
            s.attempts = child.attempts;
            s.status = Some(child.status_code);
            s.error.clone_from(&child.error);
            // Time to first token as the client saw it: the call's, offset by when it started.
            let offset = (child.ts - self.ts).num_milliseconds().max(0) as u64;
            s.ttft_ms = child.ttft_ms.map(|t| t + offset);
        }
    }
}

impl Drop for Rollup {
    fn drop(&mut self) {
        let s = std::mem::take(self.state.get_mut().expect("rollup lock"));
        let rec = RequestRecord {
            id: self.id,
            ts: self.ts,
            key_id: self.key_id,
            key_name: self.key_name.take(),
            project_id: self.project_id,
            model: std::mem::take(&mut self.model),
            deployment_id: None,
            provider: None,
            api: self.api,
            stream: self.stream,
            status_code: s.status.unwrap_or(500),
            latency_ms: self.start.elapsed().as_millis() as u64,
            ttft_ms: s.ttft_ms,
            usage: s.usage,
            cost_usd: s.cost_usd + s.guardrails.cost_usd,
            attempts: s.attempts.max(1),
            error: s.error,
            parent_id: None,
            labels: std::mem::take(&mut self.labels),
            guardrails: s.guardrails.summary,
            guardrail_cost_usd: s.guardrails.cost_usd,
            search_cost_usd: s.search_cost_usd,
        };
        telemetry::end_call(&self.cx, &rec, None);
        if self.tx.try_send(rec).is_err() {
            tracing::warn!("request record queue full; dropping record");
        }
    }
}

/// Drains records into Valkey (limit counters) and Postgres (the request log, which also feeds
/// the Metrics page, and spend).
pub async fn run_writer(
    state: AppState,
    mut rx: mpsc::Receiver<RequestRecord>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let mut batch: Vec<RequestRecord> = Vec::with_capacity(512);
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
    loop {
        tokio::select! {
            rec = rx.recv() => {
                let Some(rec) = rec else { break };
                accept(&state, rec, &mut batch);
                if batch.len() >= 500 {
                    flush(&state, &mut batch).await;
                }
            }
            _ = tick.tick() => flush(&state, &mut batch).await,
            _ = shutdown.changed() => {
                while let Ok(rec) = rx.try_recv() {
                    accept(&state, rec, &mut batch);
                }
                break;
            }
        }
    }
    flush(&state, &mut batch).await;
}

fn accept(state: &AppState, rec: RequestRecord, batch: &mut Vec<RequestRecord>) {
    tracing::info!(
        request_id = %rec.id, model = %rec.model, status = rec.status_code,
        latency_ms = rec.latency_ms, input = rec.usage.total_input(),
        output = rec.usage.output_tokens, attempts = rec.attempts, "request"
    );
    telemetry::record(&rec);
    let cache = state.cache.clone();
    let r = rec.clone();
    // A deployment's health counts its successes here; failures are counted as they happen (see
    // `router::failed`). Streamed responses also report how quickly they started.
    let succeeded = r.deployment_id.filter(|_| state.settings.get().adaptive_routing && r.status_code < 400);
    tokio::spawn(async move {
        let health = async {
            if let Some(d) = succeeded {
                cache.record_health(d, Outcome::Succeeded { ttft_ms: r.ttft_ms.filter(|_| r.stream) }).await;
            }
        };
        tokio::join!(cache.record(&r), health);
    });
    batch.push(rec);
}

async fn flush(state: &AppState, batch: &mut Vec<RequestRecord>) {
    if batch.is_empty() {
        return;
    }
    if let Err(e) = crate::db::write_request_batch(&state.db, batch).await {
        tracing::error!(error = %e, n = batch.len(), "failed to persist request logs");
        // Keep a bounded backlog so a short database outage does not lose spend.
        if batch.len() < 20_000 {
            return;
        }
    }
    batch.clear();
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::model::{ApiStyle, CredentialSettings, DeploymentConfig, Mode, Pricing, Rates, Secrets};

    /// A deployment at $1 per million input and $2 per million output tokens.
    fn deployment() -> Arc<Deployment> {
        priced(0.0)
    }

    /// `deployment`, with a price per 1,000 searches.
    fn priced(per_k_searches: f64) -> Arc<Deployment> {
        Arc::new(Deployment {
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
            pricing: Pricing { standard: Rates::new(1.0, 2.0, None, None), long_context: None, per_k_searches },
            tags: vec![],
            project_ids: vec![],
            catalog: None,
        })
    }

    /// A tracker for a streamed request with a 4000-byte body that reached the deployment, and
    /// the channel its record arrives on.
    fn tracker(stream: bool) -> (Tracker, mpsc::Receiver<RequestRecord>) {
        let (tx, rx) = mpsc::channel(4);
        let mut t = Tracker::new(&crate::state::test_state(), None, &Labels::default(), "m", "chat", stream);
        t.tx = tx;
        t.estimate_input(4000);
        t.attempt(deployment());
        (t, rx)
    }

    fn record(t: Tracker, rx: &mut mpsc::Receiver<RequestRecord>) -> RequestRecord {
        drop(t);
        rx.try_recv().expect("a record")
    }

    #[tokio::test]
    async fn client_disconnect_before_usage_is_estimated_and_charged() {
        let (mut t, mut rx) = tracker(true);
        t.streamed(400);
        let r = record(t, &mut rx);
        assert_eq!((r.usage.input_tokens, r.usage.output_tokens), (1000, 100));
        assert_eq!(r.status_code, 499);
        assert!(r.error.as_deref().unwrap().contains("estimated"));
        assert!((r.cost_usd - (1000.0 * 1.0 + 100.0 * 2.0) / 1e6).abs() < 1e-12);
    }

    #[tokio::test]
    async fn web_searches_are_logged_with_their_cost() {
        let (tx, mut rx) = mpsc::channel(4);
        let mut t = Tracker::new(&crate::state::test_state(), None, &Labels::default(), "m", "chat", false);
        t.tx = tx;
        t.attempt(priced(14.0));
        // A stream reports its searches before its usage (Chat Completions from Gemini).
        t.searched(2);
        t.set_usage(Usage { input_tokens: 1000, output_tokens: 100, ..Default::default() });
        let r = record(t, &mut rx);
        assert_eq!(r.usage.search_units, 2);
        assert!((r.search_cost_usd - 0.028).abs() < 1e-12);
        assert!((r.cost_usd - (0.0012 + 0.028)).abs() < 1e-12);
    }

    #[tokio::test]
    async fn reported_usage_is_kept() {
        let (mut t, mut rx) = tracker(true);
        t.streamed(400);
        t.set_usage(Usage { input_tokens: 7, output_tokens: 3, ..Default::default() });
        t.stream_ended();
        let r = record(t, &mut rx);
        assert_eq!((r.usage.input_tokens, r.usage.output_tokens, r.status_code, r.error), (7, 3, 200, None));
    }

    #[tokio::test]
    async fn partial_anthropic_usage_keeps_its_input() {
        let (mut t, mut rx) = tracker(true);
        // message_start reported the input; the client left before message_delta.
        t.merge_usage(&Usage { input_tokens: 50, ..Default::default() });
        t.streamed(80);
        let r = record(t, &mut rx);
        assert_eq!((r.usage.input_tokens, r.usage.output_tokens, r.status_code), (50, 20, 499));
    }

    #[tokio::test]
    async fn stream_that_ends_without_usage_is_estimated_but_not_a_disconnect() {
        let (mut t, mut rx) = tracker(true);
        t.streamed(40);
        t.set_usage(Usage::default()); // an upstream that sends an empty usage object
        t.stream_ended();
        let r = record(t, &mut rx);
        assert_eq!((r.usage.input_tokens, r.usage.output_tokens, r.status_code), (1000, 10, 200));
        assert!(r.error.as_deref().unwrap().contains("reported no usage"));
    }

    #[tokio::test]
    async fn refused_or_free_requests_are_not_charged() {
        // The upstream refused before producing anything: providers do not bill that.
        let (mut t, mut rx) = tracker(true);
        t.fail_status(429, "rate limited");
        let r = record(t, &mut rx);
        assert_eq!((r.usage.total(), r.cost_usd, r.status_code), (0, 0.0, 429));
        // No input estimate (token counting) and nothing generated: nothing to charge.
        let (tx, mut rx) = mpsc::channel(4);
        let mut t = Tracker::new(&crate::state::test_state(), None, &Labels::default(), "m", "count_tokens", false);
        t.tx = tx;
        t.attempt(deployment());
        let r = record(t, &mut rx);
        assert_eq!((r.usage.total(), r.status_code, r.error), (0, 200, None));
    }
}
