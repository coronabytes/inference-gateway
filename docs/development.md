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
docker compose --profile replicas up -d --build  # two more gateway replicas on :8081 and :8082
```

- The **mock provider** runs on loopback, which the gateway refuses unless allowed: `.env.example` already sets `GATEWAY_UPSTREAM_ALLOWED_NETWORKS=127.0.0.0/8,::1`. It is also a Bedrock guardrail: a Bedrock credential with `http://127.0.0.1:9999` as its endpoint (any API key) and a guardrail on it mask e-mail addresses and the name "Ada Lovelace", flag "lottery" and "casino", and block "ignore previous instructions".
- The **replicas** share Postgres and Valkey with the gateway you run locally, so you can watch configuration changes, rate limits and budgets propagate between instances.

## Tests

```bash
cargo test                       # gateway
(cd ui && npm run typecheck)     # console
```

Tests that need Valkey are skipped unless one is given:

```bash
GATEWAY_TEST_VALKEY_URL=redis://127.0.0.1:6379 cargo test
GATEWAY_TEST_VALKEY_CLUSTER=true  # in addition, when that Valkey is a cluster
```

CI ([checks.yml](../.github/workflows/checks.yml)) runs the tests, checks the sqlx metadata against a live database, builds the console and runs `cargo audit` and `npm audit` on every push to `main` and every pull request. Accepted advisories are listed, with reasons, in [.cargo/audit.toml](../.cargo/audit.toml).

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

The OpenAPI document is generated from `src/openapi.rs` and the admin API's types. A test fails when a route is served but not described there.

## Docker image

```bash
docker build -t inference-gateway:local .
```

The build compiles a static binary against the committed `.sqlx/` metadata and copies it, with the built console, into a distroless image. `GATEWAY_VERSION` (a build argument) sets the version the gateway reports; outside Docker, `build.rs` takes it from `git describe --tags`.

## Releases

Push a tag such as `v1.2.0`. The [release workflow](../.github/workflows/release.yml) runs the checks, builds the `linux/amd64` and `linux/arm64` images on native runners, pushes them to `ghcr.io/<owner>/inference-gateway` as `1.2.0`, `1.2`, `1` and `latest` (pre-release tags such as `v1.2.0-rc.1` get only their own version), with an SBOM and build provenance attached, and creates a GitHub release with generated notes.
