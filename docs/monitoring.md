# Monitoring

## In the console

- **Metrics:** requests, errors, tokens, average and p95 latency, time to first token and cost, by model, deployment, key, project or [label](api.md#labels). This month by default, or the last hour, 24 hours, 7 days or any date range. Computed from the request log, so they cover all replicas and go back as far as the log is kept.
- **Logs → Request log:** every request with its key, labels, model, deployment, status, tokens, latency, cost and what guardrails did. Prompts and responses are not stored, except for keys with [prompt logging](security.md#prompt-logging) turned on, for debugging.
- **Logs → Audit log:** who created, changed or deleted credentials, guardrails, deployments, routers and keys (with the changed fields, never secrets), regenerated a key or reset its spend, imported configuration or prices, changed single sign-on or settings, and signed in or out.
- **Cluster → Instances:** every running replica, the health of Postgres and Valkey, and what each replica read from its environment.
- **Models → Playground:** chat, embed, rerank or ask decision questions with any model, as any key, with guardrails and labels of your choice. It goes through the same limits, budgets and logging as real traffic. **Show curl** gives the request as a curl command.

Both logs are kept for 90 days by default ([settings](configuration.md#settings)).

## OpenTelemetry

The gateway pushes traces, metrics and logs with OpenTelemetry over OTLP/HTTP (protobuf) to a collector, or to any backend that accepts OTLP: Grafana, Datadog, Honeycomb, Elastic, Prometheus (with its OTLP receiver) and others.

```bash
OTEL_EXPORTER_OTLP_ENDPOINT=http://otel-collector:4318
OTEL_EXPORTER_OTLP_HEADERS=authorization=Bearer%20<token>   # if your backend needs one
```

Nothing is exported without an endpoint. All supported variables are listed in [Configuration](configuration.md#opentelemetry). The service is named `inference-gateway`, with the version and the replica's `GATEWAY_INSTANCE_ID` as `service.instance.id`. gRPC is not supported; use port 4318.

- **Traces:** a span per inference request, continuing the client's trace (`traceparent`). Beneath it, a span per model call with the [GenAI semantic conventions](https://opentelemetry.io/docs/specs/semconv/gen-ai/) (`gen_ai.request.model`, `gen_ai.provider.name`, `gen_ai.usage.input_tokens`, …) plus key, deployment, cost, request id and labels (`gateway.label.<key>`), and a client span per attempt at a deployment, so failover shows.
- **Metrics:** `http.server.request.duration`; per model call `gen_ai.server.request.duration`, `gen_ai.server.time_to_first_token` and `gen_ai.client.token.usage`, by model, provider and deployment; and billing by key and model: `gateway.requests`, `gateway.tokens` (by type) and `gateway.cost` (USD).
- **Logs:** the gateway's log, each record linked to its trace.

Telemetry never contains prompts or responses. There is no Prometheus `/metrics` endpoint to scrape: send OTLP to Prometheus's OTLP receiver, or through a collector.
