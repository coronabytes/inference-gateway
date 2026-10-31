# Running in production

The gateway is a stateless service. Run any number of identical replicas behind a load balancer, all using the same Postgres and Valkey.

## Requirements

- **The image:** `ghcr.io/coronabytes/inference-gateway`, for `linux/amd64` and `linux/arm64`. Tags: `latest`, `<major>` (for example `1`), `<major>.<minor>` (`1.0`) and the full version (`1.0.0`). Pin a version in production. Each image carries an SBOM and build provenance (`docker buildx imagetools inspect <image> --format '{{json .SBOM}}'`).
- **Postgres:** stores configuration, keys, the request log and the audit log. Any managed Postgres works (RDS, Cloud SQL, Azure Database for PostgreSQL).
- **Valkey or Redis:** a single primary (with or without replicas) or a cluster, including ElastiCache and ElastiCache Serverless (`VALKEY_CLUSTER=true`). It holds rate limit counters, budget spend, admin sessions, conversation state and the change notifications between replicas. Enable persistence with `appendonly yes`. Sentinel is not supported.

The image runs as a non-root user, needs no writable file system and no Linux capabilities.

## Docker Compose

A complete single-host stack: Postgres, Valkey and two gateway replicas. Put the secrets in a `.env` file next to it:

```bash
GATEWAY_ENCRYPTION_KEY=   # openssl rand -base64 32
GATEWAY_MASTER_KEY=       # openssl rand -hex 24
POSTGRES_PASSWORD=        # openssl rand -hex 16
```

```yaml
x-gateway: &gateway
  image: ghcr.io/coronabytes/inference-gateway:1.0
  restart: unless-stopped
  read_only: true
  cap_drop: [ALL]
  security_opt: [no-new-privileges:true]
  environment:
    DATABASE_URL: postgres://gateway:${POSTGRES_PASSWORD:?}@postgres:5432/gateway
    VALKEY_URL: redis://valkey:6379
    GATEWAY_ENCRYPTION_KEY: ${GATEWAY_ENCRYPTION_KEY:?}
    GATEWAY_MASTER_KEY: ${GATEWAY_MASTER_KEY:-}
  depends_on:
    postgres: { condition: service_healthy }
    valkey: { condition: service_healthy }

services:
  gateway-1:
    <<: *gateway
    hostname: gateway-1
    ports: ["8081:8080"]

  gateway-2:
    <<: *gateway
    hostname: gateway-2
    ports: ["8082:8080"]

  postgres:
    image: postgres:18-alpine
    restart: unless-stopped
    environment:
      POSTGRES_USER: gateway
      POSTGRES_PASSWORD: ${POSTGRES_PASSWORD:?}
      POSTGRES_DB: gateway
    volumes: [pgdata:/var/lib/postgresql]
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U gateway -d gateway"]
      interval: 5s
      retries: 20

  valkey:
    image: valkey/valkey:9-alpine
    restart: unless-stopped
    command: ["valkey-server", "--appendonly", "yes"]
    volumes: [valkeydata:/data]
    healthcheck:
      test: ["CMD", "valkey-cli", "ping"]
      interval: 5s
      retries: 20

volumes:
  pgdata:
  valkeydata:
```

Put a TLS-terminating load balancer or reverse proxy in front of the replicas (see [Reverse proxy](#reverse-proxy)). Run Valkey with `appendonly yes` on a volume, so that a restart keeps rate limit windows, stored conversations and admin sessions. If Valkey's data is lost anyway, the gateway recovers: budget spend is rebuilt from the request log in Postgres, rate limit windows and stored conversations start over, and admins sign in again.

## Kubernetes

A minimal Deployment. Keep the secrets in a Secret, and add a Service and an Ingress in front of it as usual.

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: inference-gateway
spec:
  replicas: 2
  selector:
    matchLabels: { app: inference-gateway }
  template:
    metadata:
      labels: { app: inference-gateway }
    spec:
      securityContext:
        runAsNonRoot: true
      containers:
        - name: gateway
          image: ghcr.io/coronabytes/inference-gateway:1.0
          ports: [{ containerPort: 8080 }]
          envFrom:
            - secretRef: { name: inference-gateway }  # DATABASE_URL, VALKEY_URL, GATEWAY_ENCRYPTION_KEY, GATEWAY_MASTER_KEY
          env:
            - name: GATEWAY_INSTANCE_ID
              valueFrom: { fieldRef: { fieldPath: metadata.name } }
          livenessProbe:
            httpGet: { path: /health/live, port: 8080 }
          readinessProbe:
            httpGet: { path: /health/ready, port: 8080 }
          securityContext:
            readOnlyRootFilesystem: true
            allowPrivilegeEscalation: false
            capabilities: { drop: [ALL] }
```

On EKS, Bedrock credentials can use the pod's IAM role (IRSA or Pod Identity) through the AWS credential chain, so no AWS secret is stored.

## Reverse proxy

- Terminate TLS in front of the gateway. The console's session cookie is `Secure`, so it needs HTTPS (or `http://localhost`).
- Allow long requests: a model call may take minutes. Set the proxy's read timeout at least as high as the longest deployment timeout (600 s by default).
- Streamed responses are sent with `X-Accel-Buffering: no`, so nginx does not buffer them. Other proxies should not buffer `text/event-stream` responses either.
- Request bodies are limited by the gateway itself (32 MB by default, a [setting](configuration.md#settings)); keep the proxy's limit at least as high.

## Health checks

| Endpoint | Meaning |
|---|---|
| `/health/live` | The process is up. Use it for liveness. |
| `/health/ready` | Postgres is reachable and the configuration is loaded. Use it for readiness. The response also reports Valkey. |

The image has a built-in `HEALTHCHECK` (`/gateway healthcheck`).

If Valkey is unreachable, requests are still served, but rate limits are not enforced until it is back. The gateway shuts down gracefully on `SIGTERM` and writes pending request logs before it exits.

## Scaling

Every replica serves every request; there is no leader. Configuration changes made on one replica reach the others over Valkey within moments, and a periodic reload from Postgres catches anything missed. Rate limits, budgets, cooldowns and sticky routing are shared across replicas through Valkey.

Each replica opens up to `DATABASE_MAX_CONNECTIONS` (20) Postgres connections; size Postgres for the total.

## Upgrades

Database migrations run on startup and are safe with several replicas starting at once. A rolling update needs no manual steps.

## Private networks

By default the gateway connects to public addresses only. Self-hosted models (vLLM, Ollama), Azure private endpoints, Bedrock VPC endpoints or an identity provider on your network need their network listed in `GATEWAY_UPSTREAM_ALLOWED_NETWORKS`, for example `10.20.0.0/16`. Cloud metadata addresses are always blocked.

## Backup & restore

- **Postgres** holds everything that matters: back it up like any other database. Valkey needs no backup; its append-only file only keeps state across restarts. Logged prompts and responses (`request_bodies`, see [prompt logging](security.md#prompt-logging)) are debugging data, not encrypted: exclude them with `pg_dump --exclude-table-data=request_bodies` unless backups may hold them. Leave prompt logging off for production keys.
- **`GATEWAY_ENCRYPTION_KEY`** must be kept with the backup: stored provider secrets cannot be read without it.
- **Cluster → Backup & Restore** in the console exports credentials (without their secrets), guardrails, deployments, auto routers and model descriptions as one JSON file, and merges such a file into a gateway, for example to copy configuration from staging to production. Imports match entries by id, then by name, never delete anything, and offer a dry run. Keys and projects are not exported.

## Rotate the encryption key

Nothing stops while the key changes: the gateway reads with both keys and writes with the new one until the stored secrets have been re-encrypted.

1. Generate a new key: `openssl rand -base64 32`.
2. Restart every replica with the new key in `GATEWAY_ENCRYPTION_KEY` and the old one in `GATEWAY_PREVIOUS_ENCRYPTION_KEY`.
3. Re-encrypt what is stored, with the same variables set:

   ```bash
   docker run --rm --env-file gateway.env ghcr.io/coronabytes/inference-gateway:1.0 rotate-key
   ```

   This re-encrypts provider secrets, MCP server and A2A agent secrets, keys' upstream sign-ins, the OIDC client secret and conversation state, skips what is already under the new key, and writes nothing if a stored secret cannot be read with either key. It is safe to run again.
4. Remove `GATEWAY_PREVIOUS_ENCRYPTION_KEY` and restart.

## Checklist

- [ ] TLS in front of the gateway, with long read timeouts.
- [ ] `GATEWAY_ENCRYPTION_KEY` stored in a secret manager and backed up.
- [ ] [Single sign-on](single-sign-on.md) set up, then `GATEWAY_MASTER_KEY` removed (an admin-scoped virtual key can replace it for scripts).
- [ ] Postgres backups.
- [ ] Valkey with `appendonly yes`.
- [ ] Retention set for the request and audit logs ([settings](configuration.md#settings)).
- [ ] [OpenTelemetry](monitoring.md#opentelemetry) export, if you run an observability stack.

All environment variables and settings are listed in [Configuration](configuration.md).
