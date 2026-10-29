# Inference Gateway

**One endpoint, one key, every model.**

A self-hosted gateway for large language models. Your apps keep using the OpenAI and Anthropic SDKs; the gateway routes each request to the right provider, keeps provider credentials safe, and shows who used what and what it cost.

Single Rust service · distroless, locked-down container image · Postgres + Valkey · AGPL-3.0

> [!IMPORTANT]
> After the latest LiteLLM incident we decided to just roll our own. 
> Claude finished all features we required within 3 days including Enterprise.
> Use at your own discretion.

## Features

- **Every provider, one API.** OpenAI, Anthropic and Cohere APIs in front of OpenAI, Azure AI Foundry, Amazon Bedrock, Anthropic, Google Vertex AI, Google AI Studio, Mistral and any OpenAI-compatible server (vLLM, Ollama).
- **Virtual keys.** One key per team or app, with rate limits, budgets, expiry and model access. Provider secrets never leave the gateway.
- **Reliable routing.** Load balancing, failover and retries across deployments and regions, with traffic moving away from failing or slow ones.
- **Auto routing.** One model name that picks the best model for each request.
- **Guardrails.** Mask personal data and block prompt attacks with Bedrock Guardrails, Model Armor or Mistral moderation.
- **Cost and usage.** Metrics and a request log by model, key, project and your own labels. OpenTelemetry export.
- **Admin console.** Web UI with single sign-on, playground and model catalog.
- **Scales out.** Stateless replicas; configuration changes apply everywhere without restarts.

## Screenshots

**Metrics:** requests, tokens, latency and cost over time, per model, key, project or label.

![Metrics page with requests, tokens, latency and cost per day, and a table by model](docs/images/metrics.png)

**Model hub:** every model the gateway serves, with its context window, prices and capabilities, or only those a given key may use.

![Model hub listing models with vendor, context window, prices and capabilities](docs/images/hub.png)

**Sign-in:** single sign-on with your identity provider.

![Admin console sign-in screen with a single sign-on button](docs/images/login.png)

## Quick start

The gateway needs Postgres and Valkey (or Redis). Save this as `compose.yml`, change the Postgres password in both places, and run `docker compose up -d`:

```yaml
services:
  gateway:
    image: ghcr.io/coronabytes/inference-gateway:latest
    restart: unless-stopped
    read_only: true
    cap_drop: [ALL]
    security_opt: [no-new-privileges:true]
    ports: ["8080:8080"]
    environment:
      DATABASE_URL: postgres://gateway:secret-change-me@postgres:5432/gateway
      VALKEY_URL: redis://valkey:6379
      GATEWAY_ENCRYPTION_KEY: ${GATEWAY_ENCRYPTION_KEY}  # openssl rand -base64 32
      GATEWAY_MASTER_KEY: ${GATEWAY_MASTER_KEY}          # openssl rand -hex 24
    depends_on:
      postgres: { condition: service_healthy }
      valkey: { condition: service_healthy }

  postgres:
    image: postgres:18-alpine
    restart: unless-stopped
    environment:
      POSTGRES_USER: gateway
      POSTGRES_PASSWORD: secret-change-me
      POSTGRES_DB: gateway
    volumes: [pgdata:/var/lib/postgresql]
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U gateway -d gateway"]
      interval: 5s

  valkey:
    image: valkey/valkey:9-alpine
    restart: unless-stopped
    command: ["valkey-server", "--appendonly", "yes"]
    volumes: [valkeydata:/data]
    healthcheck:
      test: ["CMD", "valkey-cli", "ping"]
      interval: 5s

volumes:
  pgdata:
  valkeydata:
```

Open <http://localhost:8080>, sign in with the master key, then:

1. **Models → Credentials:** connect a provider.
2. **Models → Deployments:** add a model.
3. **Access → Keys:** create a key for your app.
4. Point your app at the gateway.

For a full stack with Postgres, Valkey and several replicas, see [Running in production](docs/production.md).

## Documentation

| | |
|---|---|
| [Using the API](docs/api.md) | SDK setup, endpoints, labels, conversations |
| [Providers](docs/providers.md) · [Models](docs/models.md) | Connect providers, add models, routing and pricing |
| [Access control](docs/access.md) | Keys, budgets, projects and tags |
| [Auto routers](docs/auto-routers.md) · [Guardrails](docs/guardrails.md) | Smart routing and prompt screening |
| [Monitoring](docs/monitoring.md) | Metrics, logs and OpenTelemetry |
| [Production](docs/production.md) · [Configuration](docs/configuration.md) | Deployment, every variable and setting |
| [Single sign-on](docs/single-sign-on.md) · [Security](docs/security.md) | OIDC, roles, data protection |
| [Development](docs/development.md) | Build, test and contribute |

## License

[AGPL-3.0-only](LICENSE). If you run a modified version as a network service, you must offer its users the source code.
