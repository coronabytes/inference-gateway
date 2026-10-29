-- The schema as of v1.0.0. Add changes as new migrations; never edit a released one.

-- Reusable provider credentials. Deployments must reference one; credentials cannot be
-- deleted while in use.
CREATE TABLE credentials (
    id          UUID PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    provider    TEXT NOT NULL
                CHECK (provider IN ('openai', 'azure', 'bedrock', 'anthropic', 'typesafe', 'mistral', 'vertex', 'gemini')),
    -- AES-256-GCM encrypted JSON blob (nonce || ciphertext). NULL means "no secret":
    -- an unauthenticated OpenAI-compatible server, or the AWS default credential chain.
    secrets     BYTEA,
    -- Connection settings (endpoint, API version, region): an Azure key only works for its own
    -- resource, and AWS keys are used in a region.
    settings    JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Guardrails screen prompts and responses with a provider's guardrail service: Amazon Bedrock
-- Guardrails, Mistral's moderation API (scores texts by category, including personal data and
-- jailbreaking, and cannot mask) or Google Cloud Model Armor (called with a Vertex AI credential;
-- blocks and, with a de-identify template, masks). A guardrail refers to one defined at the
-- provider (for Bedrock its id or ARN and version, in `config`), calls it with one of the
-- gateway's credentials of the same provider, and keeps the prices it is billed at: USD per 1,000
-- text units for each kind of policy.
CREATE TABLE guardrails (
    id            UUID PRIMARY KEY,
    name          TEXT NOT NULL UNIQUE,
    description   TEXT,
    provider      TEXT NOT NULL CHECK (provider IN ('bedrock', 'mistral', 'model_armor')),
    credential_id UUID NOT NULL REFERENCES credentials (id),
    config        JSONB NOT NULL DEFAULT '{}'::jsonb,
    prices        JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Model deployments: one public model name can be served by many deployments (load balanced).
CREATE TABLE deployments (
    id              UUID PRIMARY KEY,
    model_name      TEXT NOT NULL,
    -- What a deployment serves: chat models (Messages, Responses, Chat Completions), embedding
    -- models (/v1/embeddings), decision models (/v1/decisions, /v1/systemone) or rerank models
    -- (/v1/rerank).
    mode            TEXT NOT NULL DEFAULT 'chat' CHECK (mode IN ('chat', 'embedding', 'decision', 'rerank')),
    provider        TEXT NOT NULL
                    CHECK (provider IN ('openai', 'azure', 'bedrock', 'anthropic', 'typesafe', 'mistral', 'vertex', 'gemini')),
    upstream_model  TEXT NOT NULL,
    -- The price catalog entry a deployment corresponds to, e.g. azure/gpt-4o for an Azure
    -- deployment named "prod-4o". Used to look up and fill in prices.
    base_model      TEXT,
    config          JSONB NOT NULL DEFAULT '{}'::jsonb,
    credential_id   UUID NOT NULL REFERENCES credentials (id) ON DELETE RESTRICT,
    weight          INTEGER NOT NULL DEFAULT 1 CHECK (weight > 0),
    priority        INTEGER NOT NULL DEFAULT 0,
    rpm_limit       INTEGER CHECK (rpm_limit > 0),
    tpm_limit       INTEGER CHECK (tpm_limit > 0),
    timeout_secs    INTEGER NOT NULL DEFAULT 600 CHECK (timeout_secs > 0),
    input_cost_per_mtok        DOUBLE PRECISION NOT NULL DEFAULT 0,
    output_cost_per_mtok       DOUBLE PRECISION NOT NULL DEFAULT 0,
    cache_read_cost_per_mtok   DOUBLE PRECISION,
    cache_write_cost_per_mtok  DOUBLE PRECISION,
    -- Optional long-context price tier. When a request's input tokens (including cached ones)
    -- exceed long_context_threshold, the whole request is billed at the long_* prices. NULL
    -- threshold means the deployment has a single price tier.
    long_context_threshold          INTEGER CHECK (long_context_threshold > 0),
    long_input_cost_per_mtok        DOUBLE PRECISION,
    long_output_cost_per_mtok       DOUBLE PRECISION,
    long_cache_read_cost_per_mtok   DOUBLE PRECISION,
    long_cache_write_cost_per_mtok  DOUBLE PRECISION,
    -- Rerankers bill per search (a query over up to 100 documents), some also per token.
    cost_per_k_searches DOUBLE PRECISION CHECK (cost_per_k_searches >= 0),
    -- Tags (e.g. azure, cheap). A key with tags may only use untagged deployments and those
    -- sharing a tag with it; a key without tags may use every deployment.
    tags            TEXT[] NOT NULL DEFAULT '{}',
    -- Projects a deployment is limited to. Empty means any key may use it (as its tags allow);
    -- otherwise only keys in one of these projects may, whatever tags they hold. No foreign key: a
    -- project cannot be deleted while a deployment names it.
    project_ids     UUID[] NOT NULL DEFAULT '{}',
    enabled         BOOLEAN NOT NULL DEFAULT TRUE,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX deployments_model_name_idx ON deployments (model_name);
CREATE INDEX deployments_credential_id_idx ON deployments (credential_id);

-- Projects group virtual keys. An inactive project blocks its keys: they are refused like
-- disabled keys, on the inference API and the admin API alike. A project budget is an amount per
-- day, week or month, on top of each key's own budget and with its own period. Deleting a project
-- leaves its keys without one.
CREATE TABLE projects (
    id              UUID PRIMARY KEY,
    name            TEXT NOT NULL UNIQUE,
    description     TEXT,
    enabled         BOOLEAN NOT NULL DEFAULT true,
    max_budget_usd  DOUBLE PRECISION,
    budget_period   TEXT CHECK (budget_period IN ('day', 'week', 'month')),
    -- "Reset spend" moves the start of the current period.
    spend_reset_at  TIMESTAMPTZ,
    -- See virtual_keys.guardrail_ids.
    guardrail_ids   UUID[] NOT NULL DEFAULT '{}',
    created_at      TIMESTAMPTZ NOT NULL,
    updated_at      TIMESTAMPTZ NOT NULL
);

CREATE TABLE virtual_keys (
    id              UUID PRIMARY KEY,
    name            TEXT NOT NULL,
    key_hash        BYTEA NOT NULL UNIQUE,
    key_prefix      TEXT NOT NULL,
    -- What the key may call: the inference API, the admin API, or both. A key in a project calls
    -- the inference API or, with the project scope, the admin API for that project only: what a
    -- user who works in it may see and change. Keys with every right an admin has (admin, all)
    -- stay outside projects. A project key whose project is deleted keeps its scope and sees
    -- nothing until it is put in another project.
    scope           TEXT NOT NULL DEFAULT 'inference' CHECK (scope IN ('inference', 'admin', 'all', 'project')),
    project_id      UUID REFERENCES projects (id) ON DELETE SET NULL,
    allowed_models  TEXT[] NOT NULL DEFAULT '{}',
    -- See deployments.tags.
    allowed_tags    TEXT[] NOT NULL DEFAULT '{}',
    -- Every request of a key goes through the guardrails set on the key and on its project,
    -- whatever it asks for; it may ask for more by name in `x-gateway-guardrails`. Deleting a
    -- guardrail removes it from these lists (no foreign keys on array elements).
    guardrail_ids   UUID[] NOT NULL DEFAULT '{}',
    rpm_limit       INTEGER CHECK (rpm_limit > 0),
    tpm_limit       INTEGER CHECK (tpm_limit > 0),
    -- Budgets are per period (day, week or month, UTC calendar periods). spend_reset_at lets
    -- "reset spend" start the current period over.
    max_budget_usd  DOUBLE PRECISION CHECK (max_budget_usd >= 0),
    budget_period   TEXT CHECK (budget_period IN ('day', 'week', 'month')),
    spend_reset_at  TIMESTAMPTZ,
    spend_usd       DOUBLE PRECISION NOT NULL DEFAULT 0,
    enabled         BOOLEAN NOT NULL DEFAULT TRUE,
    expires_at      TIMESTAMPTZ,
    metadata        JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT virtual_keys_budget_needs_period CHECK ((max_budget_usd IS NULL) = (budget_period IS NULL)),
    CONSTRAINT virtual_keys_admin_outside_projects CHECK (project_id IS NULL OR scope IN ('inference', 'project'))
);
CREATE INDEX virtual_keys_project ON virtual_keys (project_id);

-- Request metadata only; prompts and completions are never stored.
CREATE TABLE request_logs (
    id              UUID PRIMARY KEY,
    ts              TIMESTAMPTZ NOT NULL,
    key_id          UUID,
    -- The project the key was in at the time, so spend stays with the project it was made under
    -- when a key moves. No foreign key: the log outlives deleted projects.
    project_id      UUID,
    model_name      TEXT NOT NULL,
    deployment_id   UUID,
    provider        TEXT,
    api             TEXT NOT NULL,
    stream          BOOLEAN NOT NULL,
    status_code     INTEGER NOT NULL,
    latency_ms      INTEGER NOT NULL,
    ttft_ms         INTEGER,
    input_tokens    BIGINT NOT NULL DEFAULT 0,
    output_tokens   BIGINT NOT NULL DEFAULT 0,
    cache_read_tokens  BIGINT NOT NULL DEFAULT 0,
    cache_write_tokens BIGINT NOT NULL DEFAULT 0,
    -- Includes guardrail_cost_usd.
    cost_usd        DOUBLE PRECISION NOT NULL DEFAULT 0,
    attempts        INTEGER NOT NULL DEFAULT 1,
    error           TEXT,
    -- A routed request is logged as a parent entry (the router, summing tokens and cost) with
    -- the decision call and the routed call as children.
    parent_id       UUID,
    -- The labels a client sent with the request (`x-gateway-labels`): a JSON object of up to five
    -- string values, NULL without labels.
    labels          JSONB,
    -- What the guardrails did to the request: which ran, the strongest action (passed, flagged,
    -- masked, blocked) and the kinds of findings (`pii:NAME`, `content:PROMPT_ATTACK`), never the
    -- text they matched.
    guardrails      JSONB,
    guardrail_cost_usd DOUBLE PRECISION NOT NULL DEFAULT 0
);
CREATE INDEX request_logs_ts_idx ON request_logs (ts DESC);
CREATE INDEX request_logs_key_ts_idx ON request_logs (key_id, ts DESC);
CREATE INDEX request_logs_model_ts_idx ON request_logs (model_name, ts DESC);
CREATE INDEX request_logs_parent_idx ON request_logs (parent_id) WHERE parent_id IS NOT NULL;
CREATE INDEX request_logs_project_ts ON request_logs (project_id, ts) WHERE project_id IS NOT NULL;
-- The request log and the metrics are filtered by labels with `labels @> '{"tenant": "…"}'`.
CREATE INDEX request_logs_labels_idx ON request_logs USING GIN (labels jsonb_path_ops) WHERE labels IS NOT NULL;
-- The latest labelled requests, which the console reads the label keys and values it suggests in
-- its label filters from (`db::recent_labels`).
CREATE INDEX request_logs_labelled_ts_idx ON request_logs (ts DESC) WHERE labels IS NOT NULL;

-- Auto routers: a public model name that asks a decision model which route fits a request
-- best, then sends the request to one of that route's models.
CREATE TABLE routers (
    id              UUID PRIMARY KEY,
    model_name      TEXT NOT NULL UNIQUE,
    -- Model name of decision deployments that pick the route.
    decision_model  TEXT NOT NULL,
    -- Optional question for the decision model; a generic one is used when empty.
    instructions    TEXT,
    -- A route is a named pool of chat models, each with an optional reasoning effort:
    -- [{"name": "<label>", "description": "<when to use it>", "models": [{"model": "<chat model
    --   name>", "reasoning_effort": "low"}, ...]}, ...].
    routes          JSONB NOT NULL,
    -- The route used when the decision fails; defaults to the first route.
    fallback_route  TEXT,
    -- Keeps a conversation on the route (and model) its first turn was sent to, instead of asking
    -- the decision model again on every turn: the provider's prompt cache for the conversation
    -- stays warm, and later turns cost no decision call. Off, every turn is decided anew.
    sticky          BOOLEAN NOT NULL DEFAULT true,
    -- Same rule as deployments.tags.
    tags            TEXT[] NOT NULL DEFAULT '{}',
    enabled         BOOLEAN NOT NULL DEFAULT true,
    created_at      TIMESTAMPTZ NOT NULL,
    updated_at      TIMESTAMPTZ NOT NULL
);

-- Model aliases: a public model name that serves another model, like LiteLLM's model group
-- aliases. The target is a deployment's or an auto router's id, not its name, so an alias keeps
-- working when its target is renamed; it serves every deployment with the target's model name.
-- No foreign key, since the target is in one of two tables; an alias whose target is gone is
-- reported by the registry and serves nothing.
CREATE TABLE model_aliases (
    id          UUID PRIMARY KEY,
    model_name  TEXT NOT NULL UNIQUE,
    target_id   UUID NOT NULL,
    enabled     BOOLEAN NOT NULL DEFAULT true,
    created_at  TIMESTAMPTZ NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL
);

-- What the console's model hub says about a model besides what its deployments say: a display
-- name and a description of what it is good for, to start with. Keyed by the public model name
-- (a deployment's or an auto router's), so it covers every deployment of the model, and it stays
-- when a model's deployments are deleted, in case the name is deployed again.
CREATE TABLE model_metadata (
    model_name TEXT PRIMARY KEY,
    metadata   JSONB NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(metadata) = 'object'),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Admin sign-in with OpenID Connect. One provider, configured in the admin UI; the client
-- secret is AES-256-GCM encrypted like provider credentials. The role comes from an ID token
-- claim at each sign-in: allowed_role makes an admin, user_role a user.
CREATE TABLE oidc_settings (
    id              SMALLINT PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    enabled         BOOLEAN NOT NULL DEFAULT false,
    -- Shown on the sign-in button.
    display_name    TEXT NOT NULL DEFAULT 'Single sign-on',
    issuer_url      TEXT NOT NULL,
    client_id       TEXT NOT NULL,
    client_secret   BYTEA,
    -- External URL of the gateway; the redirect URI is {public_url}/admin/auth/oidc/callback.
    public_url      TEXT NOT NULL,
    roles_claim     TEXT NOT NULL DEFAULT 'roles',
    allowed_role    TEXT,
    user_role       TEXT,
    updated_at      TIMESTAMPTZ NOT NULL
);

-- People who have signed in. Deleting one ends their sessions. Admins may do everything; a user
-- sees only the projects an admin adds them to (project_members), and in those manages keys, uses
-- the playground and reads the request log and metrics.
CREATE TABLE admin_users (
    id              UUID PRIMARY KEY,
    issuer          TEXT NOT NULL,
    subject         TEXT NOT NULL,
    email           TEXT,
    name            TEXT,
    role            TEXT NOT NULL DEFAULT 'admin' CHECK (role IN ('admin', 'user')),
    created_at      TIMESTAMPTZ NOT NULL,
    last_login_at   TIMESTAMPTZ NOT NULL,
    UNIQUE (issuer, subject)
);

CREATE TABLE project_members (
    project_id  UUID NOT NULL REFERENCES projects (id) ON DELETE CASCADE,
    user_id     UUID NOT NULL REFERENCES admin_users (id) ON DELETE CASCADE,
    PRIMARY KEY (project_id, user_id)
);
CREATE INDEX project_members_user ON project_members (user_id);

-- Price catalog imported from LiteLLM's model_prices_and_context_window.json: one row per
-- catalog key, stored as JSONB as is, because the upstream format changes often.
CREATE TABLE model_prices (
    key         TEXT PRIMARY KEY,
    data        JSONB NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL
);

-- One row per import, for "last imported" in the UI.
CREATE TABLE model_price_imports (
    id          BIGSERIAL PRIMARY KEY,
    imported_at TIMESTAMPTZ NOT NULL,
    source      TEXT NOT NULL,
    total       INTEGER NOT NULL,
    added       INTEGER NOT NULL,
    updated     INTEGER NOT NULL,
    removed     INTEGER NOT NULL
);

-- Gateway settings edited in the console (Cluster → Settings): one JSON document, whose missing
-- fields take their defaults (see src/settings.rs). Every replica reloads it when it changes.
CREATE TABLE gateway_settings (
    id          SMALLINT PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    settings    JSONB NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL
);

-- Who changed what through the admin API. Kept regardless of the request log's retention.
-- actor_kind: master_key, virtual_key, session (a signed-in admin) or dev (Dev Login).
-- target_type: credential, deployment, router, key, configuration, price_catalog, oidc_settings,
-- admin or session; action: create, update, delete, regenerate, reset_spend, sign_in, ....
-- details: for an update, the changed fields as {"field": {"from": ..., "to": ...}}; for a
-- create, the new object. Never secrets.
CREATE TABLE audit_log (
    id          UUID PRIMARY KEY,
    ts          TIMESTAMPTZ NOT NULL,
    actor_kind  TEXT NOT NULL,
    actor_id    UUID,
    actor_name  TEXT,
    action      TEXT NOT NULL,
    target_type TEXT NOT NULL,
    target_id   UUID,
    target_name TEXT,
    details     JSONB NOT NULL DEFAULT '{}'
);
CREATE INDEX audit_log_ts ON audit_log (ts DESC);
