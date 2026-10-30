# Configuration

The gateway is configured in two places:

- **Environment variables** hold what a replica needs to start: its connections, secrets and security boundaries. They are read once at startup.
- **Settings** hold everything else. They are changed in the console under **Cluster → Settings** (or `PUT /admin/api/settings`), stored in Postgres and applied on every replica within seconds, without a restart.

Release builds and the Docker image never read a `.env` file; set variables in the environment. Debug builds (`cargo run`) also read `.env`, see [Development](development.md).

## Environment variables

### Connections

The gateway needs both Postgres and Valkey.

| Variable | Default | Description |
|---|---|---|
| `DATABASE_URL` | required | Postgres connection string, for example `postgres://gateway:secret@db:5432/gateway`. |
| `DATABASE_MAX_CONNECTIONS` | `20` | Postgres connections per replica. |
| `VALKEY_URL` | required | Valkey or Redis, for example `redis://valkey:6379`. `rediss://` for TLS. For a cluster, one or more of its nodes, comma-separated. |
| `VALKEY_CLUSTER` | `false` | `true` for a Valkey or Redis cluster, including ElastiCache Serverless and ElastiCache with cluster mode enabled. A primary with replicas (cluster mode disabled) needs only its primary endpoint and `false`. |

### Access and security

| Variable | Default | Description |
|---|---|---|
| `GATEWAY_ENCRYPTION_KEY` | required | 32 random bytes, base64 (`openssl rand -base64 32`). Encrypts provider secrets, the OIDC client secret and conversation state. Keep it safe: without it, stored secrets cannot be read. To change it, see [Rotate the encryption key](production.md#rotate-the-encryption-key). |
| `GATEWAY_MASTER_KEY` | unset | At least 32 characters (`openssl rand -hex 24`). Works as an admin API bearer token and as a fallback console sign-in. Unset or empty, admins sign in with [single sign-on](single-sign-on.md) only. |
| `GATEWAY_PREVIOUS_ENCRYPTION_KEY` | unset | The key being retired, only while [rotating the encryption key](production.md#rotate-the-encryption-key). |
| `GATEWAY_UPSTREAM_ALLOWED_NETWORKS` | empty | Private or loopback networks (CIDR, comma-separated) the gateway may connect to, for self-hosted models, private endpoints or an identity provider on your network. By default it connects to public addresses only; cloud metadata addresses are always blocked. Example: `10.0.0.0/8,192.168.1.20/32`. |
| `GATEWAY_COOKIE_SECURE` | `true` | `Secure` flag on the console's session cookie. Browsers accept it on `http://localhost`. Set `false` only to serve the console over plain HTTP on another host. |

`GATEWAY_UPSTREAM_ALLOWED_NETWORKS` and `GATEWAY_COOKIE_SECURE` are environment variables on purpose: the first keeps an admin account from pointing the gateway at your internal network, and the second, set wrong, would keep everyone from signing in to change it back.

### Server and logs

| Variable | Default | Description |
|---|---|---|
| `GATEWAY_LISTEN` | `0.0.0.0:8080` | Address and port to listen on. |
| `GATEWAY_UI` | `true` (`false` in debug builds) | Serve the console. `false` turns it off; the API keeps working. |
| `GATEWAY_INSTANCE_ID` | `HOSTNAME`, else random | The replica's name on **Cluster → Instances** and in telemetry (`service.instance.id`). |
| `GATEWAY_LOG` | `info` | Log filter, for example `info,inference_gateway=debug`. |
| `GATEWAY_LOG_FORMAT` | text (`json` in the image) | `json` for structured logs. |
| `PROMPT_LOG_BUFFER_MB` | `256` | Memory each replica may use for [logged prompts and responses](security.md#prompt-logging) not yet written to Postgres. It is not reserved: normally a few MB are in use, and it fills only while Postgres is slow or many large logged requests run at once. Then the bodies of further requests are skipped (their entry says `dropped`) and requests are served as usual. `0` turns prompt logging off on the replica. |

### OpenTelemetry

Nothing is exported unless an endpoint is set. The gateway exports OTLP over HTTP (protobuf) only; use port 4318, not 4317. See [Monitoring](monitoring.md#opentelemetry) for what is exported.

| Variable | Description |
|---|---|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | Collector or backend, for example `http://otel-collector:4318`. |
| `OTEL_EXPORTER_OTLP_HEADERS` | Headers for the backend, for example `authorization=Bearer%20<token>`. |
| `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, `…_METRICS_ENDPOINT`, `…_LOGS_ENDPOINT` | Per-signal endpoints, instead of or in addition to the common one. |
| `OTEL_TRACES_EXPORTER`, `OTEL_METRICS_EXPORTER`, `OTEL_LOGS_EXPORTER` | `none` turns one signal off. |
| `OTEL_SERVICE_NAME` | Defaults to `inference-gateway`. |
| `OTEL_RESOURCE_ATTRIBUTES` | Extra resource attributes, for example `deployment.environment=prod`. |
| `OTEL_TRACES_SAMPLER`, `OTEL_TRACES_SAMPLER_ARG` | Trace sampling. |
| `OTEL_METRIC_EXPORT_INTERVAL`, `OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE` | Metric export interval and temporality. |
| `OTEL_SDK_DISABLED` | `true` turns all export off. |

`OTEL_EXPORTER_OTLP_PROTOCOL` (and its per-signal forms) may only be `http/protobuf`; anything else stops the gateway at startup.

### AWS

Bedrock credentials that use the AWS credential chain read the standard AWS sources: `AWS_REGION`, `AWS_PROFILE`, `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` / `AWS_SESSION_TOKEN`, SSO profiles, EKS (IRSA or Pod Identity), the ECS task role and the EC2 instance profile. `credential_process` profiles are not supported.

## Settings

Changed in the console under **Cluster → Settings**, or with `GET`/`PUT /admin/api/settings`. Every replica reloads them when one announces a change over Valkey, and at the periodic full reload. Timeouts and the body limit apply to client connections opened after the change. Every change is in the audit log.

| Setting | API field | Default | Range | Description |
|---|---|---|---|---|
| Attempts per request | `max_attempts` | 3 | 1–10 | Deployments tried per request on rate limits, server errors and timeouts. |
| Cooldown after a failure | `cooldown_secs` | 30 s | 0–3600 | How long a failing deployment is skipped. |
| Full reload every | `reload_secs` | 60 s | 5–3600 | Periodic reload from Postgres, which catches any change a replica missed over Valkey. |
| Adaptive routing | `adaptive_routing` | on | | Deployments get less traffic while their recent calls fail or are slow. Off, requests spread by the configured weights only. |
| Sticky routing | `sticky_routing` | on | | Conversations stay on the deployment, and on an auto router the route, that served them. Off, every request is balanced on its own. |
| Request log | `request_log_retention_days` | 90 days | 0 or 7–1080 | How long request logs, and so metrics, are kept. `0` keeps them forever. The request log cannot be turned off. |
| Audit log | `audit_log_retention_days` | 90 days | 0 or 7–1080 | How long audit log entries are kept, independently of the request log. `0` keeps them forever. The audit log cannot be turned off. |
| Conversation state, idle | `response_ttl_minutes` | 15 min | 1–10080 | How long a `previous_response_id` conversation can go unused and still be continued. Each continuation starts the time again. |
| Conversation state per key | `conversation_quota_mb` | 64 MB | 0–16384 | History one key may keep in Valkey: the size of its conversations that have not expired. Turns beyond it are answered but cannot be continued. |
| Request body | `max_body_mb` | 32 MB | 1–1024 | Largest request body. |
| Header timeout | `header_timeout_secs` | 30 s | 1–600 | Time a client has to send its request headers. |
| Body timeout | `body_timeout_secs` | 120 s | 1–3600 | Time a client has to send its whole request body. Responses are not limited. |
| Client connections | `max_connections` | 10,000 | 1–1,000,000 | Open client connections per replica; more wait in the listen backlog. |
| Scheduled import | `price_import` | on | | Imports LiteLLM's model prices into the price catalog on a schedule, on one replica at a time (a Postgres advisory lock). Off, only the **Run Import** button on the Pricing tab does. |
| Import every | `price_import_hours` | 6 h | 1–720 | Hours between scheduled imports, counted from the last import, scheduled or not. A failed import is retried after an hour. |
| API reference | `api_docs` | on | | The OpenAPI document at `/admin/openapi.json` and the reference at `/admin/docs/`, for signed-in people and admin and project keys. |
| Prompt logging, kept for | `prompt_log_retention_days` | 7 days | 1–60 | How long logged prompts and responses are kept, and no longer than the request log. Shortening it removes older ones within the hour. |
| Prompt logging, largest body | `prompt_log_max_kb` | 256 KB | 16–4096 | The largest logged request or response. Over it, its longest texts are cut; images, files, embedding vectors and encrypted provider data are never stored. |
| Prompt logging, keys outside projects | `prompt_logging` | `deny` | `deny`, `allow` | Whether the prompts and responses of keys outside projects (admin keys among them) are stored, for debugging: never, or for the keys that turn it on. Projects set their own, and can also force it for all their keys (see [Projects](access.md#projects)). |

## Commands

The binary (`/gateway` in the image) takes one optional command:

| Command | Description |
|---|---|
| *(none)* | Run the gateway. |
| `healthcheck` | Exit 0 if the local gateway answers `/health/live`. Used by the image's `HEALTHCHECK`, since the image has no shell or curl. |
| `rotate-key` | Re-encrypt stored secrets with the current encryption key. See [Rotate the encryption key](production.md#rotate-the-encryption-key). |
