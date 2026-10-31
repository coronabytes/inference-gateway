-- OAuth for MCP servers and A2A agents: `oauth`, where the gateway gets tokens with its own client
-- credentials, and `oauth_user`, where each virtual key signs in to the upstream once and the
-- gateway calls it with that key's token. `oauth` holds the client: token and authorization URLs,
-- client id, scopes, resource; its secret, if any, is the sealed `secret`.
ALTER TABLE mcp_servers DROP CONSTRAINT mcp_servers_auth_check,
    ADD CONSTRAINT mcp_servers_auth_check CHECK (auth IN ('none', 'bearer', 'header', 'oauth', 'oauth_user')),
    ADD COLUMN oauth JSONB,
    ADD CONSTRAINT mcp_servers_oauth CHECK ((auth IN ('oauth', 'oauth_user')) = (oauth IS NOT NULL));
ALTER TABLE a2a_agents DROP CONSTRAINT a2a_agents_auth_check,
    ADD CONSTRAINT a2a_agents_auth_check CHECK (auth IN ('none', 'bearer', 'header', 'oauth', 'oauth_user')),
    ADD COLUMN oauth JSONB,
    ADD CONSTRAINT a2a_agents_oauth CHECK ((auth IN ('oauth', 'oauth_user')) = (oauth IS NOT NULL));

-- The tokens a key got by signing in to an `oauth_user` upstream, sealed with the gateway's
-- encryption key. Deleted with the key, and with the upstream (no foreign key: two tables).
CREATE TABLE oauth_tokens (
    upstream_kind TEXT NOT NULL CHECK (upstream_kind IN ('mcp', 'a2a')),
    upstream_id   UUID NOT NULL,
    key_id        UUID NOT NULL REFERENCES virtual_keys (id) ON DELETE CASCADE,
    access_token  BYTEA NOT NULL,
    refresh_token BYTEA,
    expires_at    TIMESTAMPTZ,
    scope         TEXT,
    -- Who signed in, for the console: the admin or user who completed the sign-in.
    connected_by  TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (upstream_kind, upstream_id, key_id)
);
CREATE INDEX oauth_tokens_key_idx ON oauth_tokens (key_id);
