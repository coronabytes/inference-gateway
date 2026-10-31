-- MCP clients signed in to the gateway with OAuth: each grant lets one client (by its Client ID
-- Metadata Document URL) use one MCP endpoint as one virtual key, until revoked. Tokens are stored
-- as SHA-256 hashes only.
CREATE TABLE client_grants (
    id                 UUID PRIMARY KEY,
    key_id             UUID NOT NULL REFERENCES virtual_keys (id) ON DELETE CASCADE,
    -- The client's metadata document URL, and what it called itself when the grant was made.
    client_id          TEXT NOT NULL,
    client_name        TEXT,
    redirect_uri       TEXT NOT NULL,
    -- The MCP endpoint's path the tokens are for: /mcp or /mcp/{server}.
    resource           TEXT NOT NULL,
    scope              TEXT NOT NULL,
    access_token_hash  BYTEA NOT NULL UNIQUE,
    access_expires_at  TIMESTAMPTZ NOT NULL,
    refresh_token_hash BYTEA NOT NULL UNIQUE,
    refresh_expires_at TIMESTAMPTZ NOT NULL,
    -- Who approved it in the console.
    created_by         TEXT,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_used_at       TIMESTAMPTZ
);
CREATE INDEX client_grants_key ON client_grants (key_id);
