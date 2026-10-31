-- MCP servers the gateway proxies at /mcp/{name}: an upstream Streamable HTTP endpoint and how the
-- gateway authenticates to it. Clients authenticate with their virtual key; the upstream's secret
-- never leaves the gateway. Access works as for deployments: a server limited to projects is only
-- for keys in one of them, and a key with tags may only use untagged servers and those sharing a tag
-- with it.
CREATE TABLE mcp_servers (
    id            UUID PRIMARY KEY,
    -- The path segment clients call: lowercase letters, digits, '-' and '_'.
    name          TEXT NOT NULL UNIQUE,
    description   TEXT,
    url           TEXT NOT NULL,
    -- none, `Authorization: Bearer <secret>`, or the secret as the value of `auth_header`.
    auth          TEXT NOT NULL DEFAULT 'none' CHECK (auth IN ('none', 'bearer', 'header')),
    auth_header   TEXT,
    -- AES-256-GCM encrypted secret (nonce || ciphertext); NULL without one.
    secret        BYTEA,
    tags          TEXT[] NOT NULL DEFAULT '{}',
    -- No foreign key: a project cannot be deleted while a server names it.
    project_ids   UUID[] NOT NULL DEFAULT '{}',
    timeout_secs  INTEGER NOT NULL DEFAULT 300 CHECK (timeout_secs > 0),
    enabled       BOOLEAN NOT NULL DEFAULT TRUE,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT mcp_servers_header_auth CHECK ((auth = 'header') = (auth_header IS NOT NULL))
);

-- The tool an MCP call (api `mcp`) called; its `model_name` is the server's name. Nullable without
-- a default, so adding it does not rewrite the table.
ALTER TABLE request_logs ADD COLUMN tool TEXT;
