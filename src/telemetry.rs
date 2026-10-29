//! OpenTelemetry: traces, metrics and logs, pushed to a collector over OTLP/HTTP (protobuf).
//!
//! Off unless an OTLP endpoint is configured with the standard variables: `OTEL_EXPORTER_OTLP_ENDPOINT`,
//! or a signal's own `OTEL_EXPORTER_OTLP_{TRACES,METRICS,LOGS}_ENDPOINT`. The SDK reads the other
//! standard variables itself (headers, timeout, compression, sampler, export interval, metric
//! temporality, resource attributes). `OTEL_{TRACES,METRICS,LOGS}_EXPORTER=none` turns one signal
//! off, `OTEL_SDK_DISABLED=true` all of them.
//!
//! - Traces: a server span per inference request, continuing the caller's trace (`traceparent`);
//!   beneath it a span per model call the request makes (an auto router's decision and routed call
//!   sit under the router's span) and a client span per attempt at a deployment, so failover shows.
//!   They carry GenAI semantic convention attributes (models, provider, token usage) and the
//!   gateway's (key, deployment, cost, request id); never prompts or outputs.
//! - Metrics: `http.server.request.duration`; per model call `gen_ai.server.request.duration`,
//!   `gen_ai.server.time_to_first_token` and `gen_ai.client.token.usage`; per key, as billed,
//!   `gateway.requests`, `gateway.tokens` and `gateway.cost`.
//! - Logs: the gateway's log events, linked to the trace they happened in.
//!
//! Without an endpoint the global providers stay no-ops and the request middleware is not
//! installed, so the calls below cost next to nothing.

use std::{
    pin::Pin,
    sync::{
        LazyLock, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context as TaskContext, Poll},
    time::Instant,
};

use axum::{
    body::Body,
    extract::{MatchedPath, Request},
    http::HeaderMap,
    middleware::Next,
    response::Response,
};
use bytes::Bytes;
use http_body::{Body as HttpBody, Frame, SizeHint};
use opentelemetry::{
    Context, InstrumentationScope, KeyValue,
    context::FutureExt as _,
    global::{self, BoxedTracer},
    metrics::{Counter, Histogram},
    propagation::{Extractor, TextMapPropagator},
    trace::{SpanKind, Status, TraceContextExt, Tracer},
};
use opentelemetry_sdk::{
    Resource, logs::SdkLoggerProvider, metrics::SdkMeterProvider, propagation::TraceContextPropagator,
    trace::SdkTracerProvider,
};

use crate::{
    accounting::RequestRecord,
    error::GatewayError,
    model::{Deployment, ProviderKind},
};

static TRACES: AtomicBool = AtomicBool::new(false);
static METRICS: AtomicBool = AtomicBool::new(false);
static PROVIDERS: Mutex<Providers> = Mutex::new(Providers { traces: None, metrics: None, logs: None });

struct Providers {
    traces: Option<SdkTracerProvider>,
    metrics: Option<SdkMeterProvider>,
    logs: Option<SdkLoggerProvider>,
}

/// Starts the exporters the environment asks for. Returns the logger provider when logs are
/// exported, for the log layer (see `main::init_tracing`).
pub fn init() -> Result<Option<SdkLoggerProvider>, String> {
    if std::env::var("OTEL_SDK_DISABLED").is_ok_and(|v| v.trim().eq_ignore_ascii_case("true")) {
        return Ok(None);
    }
    let (traces, metrics, logs) = (wanted("TRACES"), wanted("METRICS"), wanted("LOGS"));
    if !(traces || metrics || logs) {
        return Ok(None);
    }
    for signal in ["", "_TRACES", "_METRICS", "_LOGS"] {
        let var = format!("OTEL_EXPORTER_OTLP{signal}_PROTOCOL");
        if let Some(p) = nonempty(&var)
            && p != "http/protobuf"
        {
            return Err(format!("{var}={p} is not supported: the gateway exports OTLP over http/protobuf (usually port 4318)"));
        }
    }
    let resource = resource();
    let mut p = PROVIDERS.lock().expect("telemetry lock");
    if traces {
        let exporter =
            opentelemetry_otlp::SpanExporter::builder().with_http().build().map_err(|e| format!("OTLP trace exporter: {e}"))?;
        let provider = SdkTracerProvider::builder().with_batch_exporter(exporter).with_resource(resource.clone()).build();
        global::set_tracer_provider(provider.clone());
        p.traces = Some(provider);
        TRACES.store(true, Ordering::Relaxed);
    }
    if metrics {
        let exporter =
            opentelemetry_otlp::MetricExporter::builder().with_http().build().map_err(|e| format!("OTLP metric exporter: {e}"))?;
        let provider = SdkMeterProvider::builder().with_periodic_exporter(exporter).with_resource(resource.clone()).build();
        global::set_meter_provider(provider.clone());
        p.metrics = Some(provider);
        METRICS.store(true, Ordering::Relaxed);
    }
    if logs {
        let exporter =
            opentelemetry_otlp::LogExporter::builder().with_http().build().map_err(|e| format!("OTLP log exporter: {e}"))?;
        p.logs = Some(SdkLoggerProvider::builder().with_batch_exporter(exporter).with_resource(resource).build());
    }
    Ok(p.logs.clone())
}

/// Which signals are exported, for the startup log.
pub fn describe() -> Option<String> {
    let p = PROVIDERS.lock().expect("telemetry lock");
    let on: Vec<&str> = [(p.traces.is_some(), "traces"), (p.metrics.is_some(), "metrics"), (p.logs.is_some(), "logs")]
        .into_iter()
        .filter_map(|(on, name)| on.then_some(name))
        .collect();
    (!on.is_empty()).then(|| on.join(", "))
}

/// Whether requests get spans or metrics, and so the request middleware is needed.
pub fn enabled() -> bool {
    TRACES.load(Ordering::Relaxed) || METRICS.load(Ordering::Relaxed)
}

/// Exports what is still buffered and stops the exporters.
pub async fn shutdown() {
    let p = {
        let mut p = PROVIDERS.lock().expect("telemetry lock");
        Providers { traces: p.traces.take(), metrics: p.metrics.take(), logs: p.logs.take() }
    };
    // The exporters' HTTP client blocks.
    let _ = tokio::task::spawn_blocking(move || {
        for result in [
            p.traces.map(|t| t.shutdown()),
            p.metrics.map(|m| m.shutdown()),
            p.logs.map(|l| l.shutdown()),
        ]
        .into_iter()
        .flatten()
        {
            if let Err(e) = result {
                eprintln!("telemetry shutdown: {e}");
            }
        }
    })
    .await;
}

/// A signal is exported when an endpoint is set for it and its exporter is not `none`.
fn wanted(signal: &str) -> bool {
    let endpoint = nonempty("OTEL_EXPORTER_OTLP_ENDPOINT").is_some()
        || nonempty(&format!("OTEL_EXPORTER_OTLP_{signal}_ENDPOINT")).is_some();
    let exporter = nonempty(&format!("OTEL_{signal}_EXPORTER"));
    endpoint && !exporter.is_some_and(|e| e.trim().eq_ignore_ascii_case("none"))
}

fn nonempty(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|v| !v.trim().is_empty())
}

/// The SDK's resource (which reads `OTEL_SERVICE_NAME` and `OTEL_RESOURCE_ATTRIBUTES`), with the
/// gateway's name, version and replica unless those set them.
fn resource() -> Resource {
    let attributes = std::env::var("OTEL_RESOURCE_ATTRIBUTES").unwrap_or_default();
    let given = |key: &str| attributes.split(',').any(|kv| kv.split_once('=').is_some_and(|(k, _)| k.trim() == key));
    let mut b = Resource::builder();
    if nonempty("OTEL_SERVICE_NAME").is_none() && !given("service.name") {
        b = b.with_service_name("inference-gateway");
    }
    b = b.with_attribute(KeyValue::new("service.version", crate::VERSION));
    if !given("service.instance.id") {
        b = b.with_attribute(KeyValue::new("service.instance.id", crate::config::instance_id().to_string()));
    }
    b.build()
}

fn scope() -> InstrumentationScope {
    InstrumentationScope::builder("inference-gateway").with_version(crate::VERSION).build()
}

static TRACER: LazyLock<BoxedTracer> = LazyLock::new(|| global::tracer_with_scope(scope()));

// ------------------------------------------------------------------------------- HTTP requests

static PROPAGATOR: LazyLock<TraceContextPropagator> = LazyLock::new(TraceContextPropagator::new);

struct Headers<'a>(&'a HeaderMap);

impl Extractor for Headers<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|k| k.as_str()).collect()
    }
}

/// Middleware for the inference API: a server span per request, continuing the caller's trace
/// (W3C `traceparent`), that ends when the response body has been sent, streams included; and the
/// request's `http.server.request.duration`. Spans started while handling the request (model
/// calls, log events) belong to it.
pub async fn http_span(req: Request, next: Next) -> Response {
    let start = Instant::now();
    let method = req.method().as_str().to_string();
    let route = req.extensions().get::<MatchedPath>().map_or_else(|| req.uri().path().to_string(), |p| p.as_str().to_string());
    let parent = PROPAGATOR.extract(&Headers(req.headers()));
    let span = TRACER
        .span_builder(format!("{method} {route}"))
        .with_kind(SpanKind::Server)
        .with_attributes([
            KeyValue::new("http.request.method", method.clone()),
            KeyValue::new("http.route", route.clone()),
        ])
        .start_with_context(&*TRACER, &parent);
    let cx = parent.with_span(span);
    let resp = next.run(req).with_context(cx.clone()).await;
    let status = resp.status();
    cx.span().set_attribute(KeyValue::new("http.response.status_code", i64::from(status.as_u16())));
    // Server spans are errors for 5xx only; a 4xx is the client's (OpenTelemetry HTTP conventions).
    if status.is_server_error() {
        cx.span().set_status(Status::error(status.canonical_reason().unwrap_or_default()));
    }
    let attrs = [
        KeyValue::new("http.request.method", method),
        KeyValue::new("http.route", route),
        KeyValue::new("http.response.status_code", i64::from(status.as_u16())),
    ];
    let (parts, body) = resp.into_parts();
    Response::from_parts(parts, Body::new(Traced { inner: body, done: Some(Done { cx, start, attrs }) }))
}

/// A response body that ends its request's span once it has been sent or dropped.
struct Traced {
    inner: Body,
    done: Option<Done>,
}

struct Done {
    cx: Context,
    start: Instant,
    attrs: [KeyValue; 3],
}

impl Done {
    fn finish(self) {
        if METRICS.load(Ordering::Relaxed) {
            INSTRUMENTS.http_duration.record(self.start.elapsed().as_secs_f64(), &self.attrs);
        }
        self.cx.span().end();
    }
}

impl HttpBody for Traced {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        let polled = Pin::new(&mut self.inner).poll_frame(cx);
        if matches!(polled, Poll::Ready(None | Some(Err(_))))
            && let Some(done) = self.done.take()
        {
            done.finish();
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for Traced {
    fn drop(&mut self) {
        if let Some(done) = self.done.take() {
            done.finish();
        }
    }
}

// --------------------------------------------------------------------------------- Model calls

/// GenAI operation name for a request record's `api`.
fn operation(api: &str) -> &str {
    match api {
        "chat" | "messages" | "responses" => "chat",
        other => other,
    }
}

/// `gen_ai.provider.name`, as the GenAI conventions name the providers they know.
fn provider_name(p: ProviderKind) -> &'static str {
    match p {
        ProviderKind::Openai => "openai",
        ProviderKind::Azure => "azure.ai.inference",
        ProviderKind::Bedrock => "aws.bedrock",
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::Vertex => "gcp.vertex_ai",
        ProviderKind::Gemini => "gcp.gemini",
        ProviderKind::Mistral => "mistral_ai",
        ProviderKind::Typesafe => "typesafe",
    }
}

/// Starts the span of a model call (or of an auto router's request), ended by `end_call` when its
/// record is written. Its parent is `parent` (a router's span, for the router's calls) or the
/// current context: the request's server span.
pub fn start_call(api: &str, model: &str, parent: Option<&Context>) -> Context {
    if !TRACES.load(Ordering::Relaxed) {
        return Context::new();
    }
    let parent = parent.cloned().unwrap_or_else(Context::current);
    let span = TRACER
        .span_builder(format!("{} {model}", operation(api)))
        .with_kind(SpanKind::Internal)
        .start_with_context(&*TRACER, &parent);
    parent.with_span(span)
}

/// Ends a model call's span with what its record says.
pub fn end_call(cx: &Context, rec: &RequestRecord, upstream_model: Option<&str>) {
    let span = cx.span();
    if !span.is_recording() {
        return;
    }
    let u = &rec.usage;
    let mut attrs = vec![
        KeyValue::new("gen_ai.operation.name", operation(rec.api).to_string()),
        KeyValue::new("gen_ai.request.model", rec.model.clone()),
        KeyValue::new("gen_ai.usage.input_tokens", u.total_input() as i64),
        KeyValue::new("gen_ai.usage.output_tokens", u.output_tokens as i64),
        KeyValue::new("gateway.request.id", rec.id.to_string()),
        KeyValue::new("gateway.attempts", i64::from(rec.attempts)),
        KeyValue::new("gateway.stream", rec.stream),
        KeyValue::new("gateway.status_code", i64::from(rec.status_code)),
        KeyValue::new("gateway.cost_usd", rec.cost_usd),
    ];
    if u.cache_read_tokens > 0 {
        attrs.push(KeyValue::new("gen_ai.usage.cache_read.input_tokens", u.cache_read_tokens as i64));
    }
    if u.cache_write_tokens > 0 {
        attrs.push(KeyValue::new("gen_ai.usage.cache_creation.input_tokens", u.cache_write_tokens as i64));
    }
    if u.search_units > 0 {
        attrs.push(KeyValue::new("gateway.search_units", u.search_units as i64));
    }
    if let Some(p) = rec.provider {
        attrs.push(KeyValue::new("gen_ai.provider.name", provider_name(p)));
    }
    if let Some(m) = upstream_model {
        attrs.push(KeyValue::new("gen_ai.response.model", m.to_string()));
    }
    if let Some(d) = rec.deployment_id {
        attrs.push(KeyValue::new("gateway.deployment.id", d.to_string()));
    }
    if let Some(k) = rec.key_id {
        attrs.push(KeyValue::new("gateway.key.id", k.to_string()));
    }
    if let Some(name) = &rec.key_name {
        attrs.push(KeyValue::new("gateway.key.name", name.clone()));
    }
    // On spans only: as metric attributes, labels such as a tenant id would be unbounded.
    for (k, v) in rec.labels.iter() {
        attrs.push(KeyValue::new(format!("gateway.label.{k}"), v.to_string()));
    }
    if let Some(t) = rec.ttft_ms {
        attrs.push(KeyValue::new("gateway.time_to_first_token_ms", t as i64));
    }
    span.set_attributes(attrs);
    if rec.status_code >= 400 {
        span.set_attribute(KeyValue::new("error.type", rec.status_code.to_string()));
        span.set_status(Status::error(rec.error.clone().unwrap_or_default()));
    }
    span.end();
}

/// Starts the client span of one attempt at a deployment, beneath its call's span.
pub fn start_attempt(call: &Context, api: &str, d: &Deployment) -> Context {
    if !call.span().is_recording() {
        return Context::new();
    }
    let span = TRACER
        .span_builder(format!("{} {}", operation(api), d.upstream_model))
        .with_kind(SpanKind::Client)
        .with_attributes([
            KeyValue::new("gen_ai.operation.name", operation(api).to_string()),
            KeyValue::new("gen_ai.request.model", d.upstream_model.clone()),
            KeyValue::new("gen_ai.provider.name", provider_name(d.provider)),
            KeyValue::new("gateway.deployment.id", d.id.to_string()),
            KeyValue::new("gateway.model", d.model_name.clone()),
        ])
        .start_with_context(&*TRACER, call);
    call.with_span(span)
}

/// Ends an attempt's span. A streamed response's attempt ends once the stream has started; the
/// call's span covers the rest.
pub fn end_attempt(cx: &Context, error: Option<&GatewayError>) {
    let span = cx.span();
    if let Some(e) = error {
        span.set_attribute(KeyValue::new("error.type", e.status.as_u16().to_string()));
        span.set_status(Status::error(e.message.clone()));
    }
    span.end();
}

// ------------------------------------------------------------------------------------- Metrics

struct Instruments {
    http_duration: Histogram<f64>,
    call_duration: Histogram<f64>,
    ttft: Histogram<f64>,
    token_usage: Histogram<u64>,
    requests: Counter<u64>,
    tokens: Counter<u64>,
    cost: Counter<f64>,
}

static INSTRUMENTS: LazyLock<Instruments> = LazyLock::new(|| {
    let m = global::meter_with_scope(scope());
    // Model calls run from milliseconds (embeddings) to minutes (long generations).
    let seconds = vec![0.01, 0.02, 0.04, 0.08, 0.16, 0.32, 0.64, 1.28, 2.56, 5.12, 10.24, 20.48, 40.96, 81.92, 163.84, 327.68];
    Instruments {
        http_duration: m
            .f64_histogram("http.server.request.duration")
            .with_unit("s")
            .with_description("Duration of inference API requests, until their response body was sent.")
            .with_boundaries(seconds.clone())
            .build(),
        call_duration: m
            .f64_histogram("gen_ai.server.request.duration")
            .with_unit("s")
            .with_description("Duration of model calls, streams included, by model, provider and deployment.")
            .with_boundaries(seconds)
            .build(),
        ttft: m
            .f64_histogram("gen_ai.server.time_to_first_token")
            .with_unit("s")
            .with_description("Time to the first generated token of streamed model calls.")
            .with_boundaries(vec![0.001, 0.005, 0.01, 0.02, 0.04, 0.06, 0.08, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 7.5, 10.0])
            .build(),
        token_usage: m
            .u64_histogram("gen_ai.client.token.usage")
            .with_unit("{token}")
            .with_description("Tokens per model call, input and output.")
            .with_boundaries(vec![
                1.0, 4.0, 16.0, 64.0, 256.0, 1024.0, 4096.0, 16384.0, 65536.0, 262144.0, 1048576.0, 4194304.0, 16777216.0,
            ])
            .build(),
        requests: m
            .u64_counter("gateway.requests")
            .with_unit("{request}")
            .with_description("Requests by virtual key, model and status, as the request log counts them.")
            .build(),
        tokens: m
            .u64_counter("gateway.tokens")
            .with_unit("{token}")
            .with_description("Tokens billed to virtual keys, by type: input, output, cache_read, cache_write.")
            .build(),
        cost: m
            .f64_counter("gateway.cost")
            .with_unit("USD")
            .with_description("Cost billed to virtual keys, at the deployments' prices.")
            .build(),
    }
});

/// Metrics from one request record. Model calls (records with a deployment) feed the GenAI
/// metrics; what keys are billed for (records that are not part of a routed request) feeds the
/// gateway's, so a routed request counts once there, with its calls' usage and cost.
pub fn record(rec: &RequestRecord) {
    if !METRICS.load(Ordering::Relaxed) {
        return;
    }
    let m = &*INSTRUMENTS;
    let op = KeyValue::new("gen_ai.operation.name", operation(rec.api).to_string());
    let model = KeyValue::new("gen_ai.request.model", rec.model.clone());
    let u = &rec.usage;
    if let (Some(d), Some(p)) = (rec.deployment_id, rec.provider) {
        let mut attrs = vec![
            op.clone(),
            model.clone(),
            KeyValue::new("gen_ai.provider.name", provider_name(p)),
            KeyValue::new("gateway.deployment.id", d.to_string()),
        ];
        if let Some(t) = rec.ttft_ms {
            m.ttft.record(t as f64 / 1000.0, &attrs);
        }
        for (ty, n) in [("input", u.total_input()), ("output", u.output_tokens)] {
            if n > 0 {
                let mut a = attrs.clone();
                a.push(KeyValue::new("gen_ai.token.type", ty));
                m.token_usage.record(n, &a);
            }
        }
        if rec.status_code >= 400 {
            attrs.push(KeyValue::new("error.type", rec.status_code.to_string()));
        }
        m.call_duration.record(rec.latency_ms as f64 / 1000.0, &attrs);
    }
    if rec.parent_id.is_none() {
        let mut attrs = vec![op, model];
        if let Some(k) = rec.key_id {
            attrs.push(KeyValue::new("gateway.key.id", k.to_string()));
        }
        if let Some(name) = &rec.key_name {
            attrs.push(KeyValue::new("gateway.key.name", name.clone()));
        }
        if rec.cost_usd > 0.0 {
            m.cost.add(rec.cost_usd, &attrs);
        }
        for (ty, n) in [
            ("input", u.input_tokens),
            ("output", u.output_tokens),
            ("cache_read", u.cache_read_tokens),
            ("cache_write", u.cache_write_tokens),
        ] {
            if n > 0 {
                let mut a = attrs.clone();
                a.push(KeyValue::new("gen_ai.token.type", ty));
                m.tokens.add(n, &a);
            }
        }
        attrs.push(KeyValue::new("http.response.status_code", i64::from(rec.status_code)));
        m.requests.add(1, &attrs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operations_follow_the_genai_conventions() {
        for api in ["chat", "messages", "responses"] {
            assert_eq!(operation(api), "chat");
        }
        assert_eq!(operation("embeddings"), "embeddings");
        assert_eq!(operation("rerank"), "rerank");
    }

    #[test]
    fn traceparent_is_continued() {
        let mut h = HeaderMap::new();
        h.insert("traceparent", "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01".parse().unwrap());
        let cx = PROPAGATOR.extract(&Headers(&h));
        let sc = cx.span().span_context().clone();
        assert_eq!(sc.trace_id().to_string(), "0af7651916cd43dd8448eb211c80319c");
        assert!(sc.is_sampled() && sc.is_remote());
    }
}
