# Development

You need Docker, a Rust toolchain (1.99 or newer) and Node.js 24.

## Set up

```bash
cp .env.example .env            # fill in the secrets as described at the top of the file
docker compose up -d            # Postgres and Valkey on 127.0.0.1
cargo run                       # the gateway on http://localhost:8080
(cd ui && npm ci && npm run dev) # the console with hot reload on http://localhost:5173
```

Open <http://localhost:5173> and use **Dev Login as admin** (or **as user**), offered by debug builds listening on loopback. The development user shows under **Access → Users** like anyone else.

`.env` is read by `docker compose` and by debug builds of the gateway (`cargo run`, IDE debugging). Variables already set in the environment win. Release builds never read it. The full list of variables is in [Configuration](configuration.md).

Debug builds serve no console of their own: Vite's dev server proxies the API to `:8080`, and `/` on the gateway redirects to the API reference. To serve a built console from the gateway, run `npm run build` in `ui/` and set `GATEWAY_UI=true`.

## Tools

```bash
node dev/mock-upstream.mjs                       # fake OpenAI/Anthropic provider on :9999
node dev/mock-mcp.mjs                            # fake MCP server on :9998
node dev/mock-a2a.mjs                            # fake A2A agent on :9997
node dev/mock-mcp-client.mjs                     # fake MCP client signing in with OAuth, on :9996
docker compose --profile replicas up -d --build  # two more gateway replicas on :8081 and :8082
```

- The **mock provider** runs on loopback, which the gateway refuses unless allowed: `.env.example` already sets `GATEWAY_UPSTREAM_ALLOWED_NETWORKS=127.0.0.0/8,::1`. It is also a Bedrock guardrail: a Bedrock credential with `http://127.0.0.1:9999` as its endpoint (any API key) and a guardrail on it mask e-mail addresses and the name "Ada Lovelace", flag "lottery" and "casino", and block "ignore previous instructions".
- The **mock MCP server** answers at `http://127.0.0.1:9998/mcp` with the bearer token `mock-mcp-secret`, as JSON, or as server-sent events with `?sse` in the URL. Its tools are `echo`, `add`, `fail` (a tool error), `slow` and `whoami`.
  - It is also an OAuth authorization server, for trying [upstream sign-ins](upstream-oauth.md). Use `http://localhost:9998/mcp` with **Discover**. Its client for the gateway's own tokens is `gateway-client`, with the secret `gateway-secret`.
  - It approves sign-ins at once, and `whoami` says which token called. `MOCK_MCP_TOKEN_SECONDS` sets how long its tokens last, to watch them refresh.
- The **mock A2A agent** has its card at `http://127.0.0.1:9997/.well-known/agent-card.json` and wants the bearer token `mock-a2a-secret`. It speaks A2A 1.0 over JSON-RPC and HTTP+JSON and echoes messages; one containing `fail` fails its task, one containing `ask` waits for input.
- The **mock MCP client** signs in to the gateway's MCP endpoints with OAuth, as Claude or ChatGPT would ([OAuth for MCP clients](mcp.md#sign-in-with-oauth-claude-chatgpt)). It serves its own client ID metadata document at `http://127.0.0.1:9996/client.json`. Open `http://127.0.0.1:9996` and click **Connect**: it finds the gateway's sign-in from the `401` of `/mcp`, sends you to the console to approve it, then lists the tools, refreshes its token and checks that the old tokens no longer work. It goes to the console's address, `http://localhost:5173` (Vite passes `/.well-known` and `/oauth` on to the gateway); `GATEWAY_URL` and `MCP_PATH` change that.
- The **replicas** share Postgres and Valkey with the gateway you run locally, so you can watch configuration changes, rate limits and budgets propagate between instances.

## Tests

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings
cargo test                                        # gateway
(cd ui && npm run typecheck && npm run lint && npm test)   # console
```

After changing an endpoint or a type the admin API sends or takes, regenerate the OpenAPI document
and the console's types from it, and commit both (CI fails when they are out of date):

```bash
SQLX_OFFLINE=true cargo run -- openapi > ui/openapi.json
(cd ui && npm run gen:api)                         # ui/src/api.gen.ts
```

Tests marked `#[sqlx::test]` (the request log, budgets, key rotation, configuration import) run on
the Postgres in `DATABASE_URL`: each gets a database of its own, created and dropped there, and
the gateway's own database is left alone. The console is linted with Biome (`ui/biome.json`, lint
rules only) and tested with Vitest.

Tests that need Valkey are skipped unless one is given:

```bash
GATEWAY_TEST_VALKEY_URL=redis://127.0.0.1:6379 cargo test
GATEWAY_TEST_VALKEY_CLUSTER=true  # in addition, when that Valkey is a cluster
```

CI ([checks.yml](../.github/workflows/checks.yml)) checks formatting, runs clippy with warnings as errors, runs the tests (the database ones on a Postgres service), checks the sqlx metadata against a live database, checks that `ui/openapi.json` and `ui/src/api.gen.ts` are up to date, lints, tests and builds the console, and runs `cargo audit` and `npm audit` on every push to `main` and every pull request. Accepted advisories are listed, with reasons, in [.cargo/audit.toml](../.cargo/audit.toml).

## Database

SQL lives in `src/db.rs` and is checked at compile time by sqlx: against the database when `DATABASE_URL` is set, or against the committed `.sqlx/` metadata when `SQLX_OFFLINE=true` (the default in `.env.example`, and in the Docker build). After changing a query or adding a migration, apply it and regenerate the metadata:

```bash
cargo install sqlx-cli --no-default-features --features postgres,rustls
cargo sqlx migrate run
cargo sqlx prepare -- --all-targets
```

Migrations live in `migrations/` and run when the gateway starts. Add a migration as a new file; never edit a released one.

## Console

The console is a React app in `ui/`, built with Vite. Shared form controls live in `ui/src/components/controls.tsx`; reuse them before writing new ones (see [CLAUDE.md](../CLAUDE.md) for the conventions).

`npm run dev` and `npm run build` first fetch Scalar's prebuilt bundle for the API reference at `/admin/docs/` (`ui/scripts/fetch-scalar.mjs`): only that release's tarball, checked against a pinned hash, rather than the npm package and its dependencies. To update Scalar, change the version in the script, run it and copy the hash it reports.

## API description

The OpenAPI document is generated from the handlers: each has its `#[utoipa::path]`, and the routers register them with `utoipa_axum::routes!`, which takes the route from it, so a route cannot be served undocumented by accident (the `/v1`-less aliases are the exception, and a test lists them). Request and response bodies are the handlers' own types, which derive `ToSchema`; `src/openapi.rs` adds the document's description, tags and security schemes, and the provider formats the inference endpoints pass on.

`inference-gateway openapi` prints the document without a database or any configuration, with the package version rather than the build's. The console's API types in `ui/src/api.ts` are aliases of those `npm run gen:api` generates from it with openapi-typescript (`ui/src/api.gen.ts`), apart from a few answers the gateway builds as plain JSON. openapi-typescript needs TypeScript 5's compiler API, which TypeScript 7 no longer has, so it lives in `ui/tools/gen-api` with a TypeScript 5 of its own.

## Docker image

```bash
docker build -t inference-gateway:local .
```

The build compiles a static binary against the committed `.sqlx/` metadata and copies it, with the built console, into a distroless image. `GATEWAY_VERSION` (a build argument) sets the version the gateway reports; outside Docker, `build.rs` takes it from `git describe --tags`.

## Releases

Push a tag such as `v1.2.0`. The [release workflow](../.github/workflows/release.yml) runs the checks, builds the `linux/amd64` and `linux/arm64` images on native runners, pushes them to `ghcr.io/<owner>/inference-gateway` as `1.2.0`, `1.2`, `1` and `latest` (pre-release tags such as `v1.2.0-rc.1` get only their own version), with an SBOM and build provenance attached, and creates a GitHub release with generated notes.
