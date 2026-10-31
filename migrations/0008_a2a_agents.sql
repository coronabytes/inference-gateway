-- A2A agents the gateway proxies at /a2a/{name}: where its agent card is, and how the gateway
-- authenticates to the agent (as for MCP servers). The card says where the agent's interfaces are;
-- clients get it with the gateway's addresses instead. Access works as for deployments.
CREATE TABLE a2a_agents (
    id                UUID PRIMARY KEY,
    -- The path segment clients call: lowercase letters, digits, '-' and '_'.
    name              TEXT NOT NULL UNIQUE,
    description       TEXT,
    card_url          TEXT NOT NULL,
    auth              TEXT NOT NULL DEFAULT 'none' CHECK (auth IN ('none', 'bearer', 'header')),
    auth_header       TEXT,
    -- AES-256-GCM encrypted secret (nonce || ciphertext); NULL without one.
    secret            BYTEA,
    tags              TEXT[] NOT NULL DEFAULT '{}',
    -- No foreign key: a project cannot be deleted while an agent names it.
    project_ids       UUID[] NOT NULL DEFAULT '{}',
    timeout_secs      INTEGER NOT NULL DEFAULT 600 CHECK (timeout_secs > 0),
    -- USD charged per message sent to the agent, once it answered.
    price_per_message DOUBLE PRECISION NOT NULL DEFAULT 0 CHECK (price_per_message >= 0),
    enabled           BOOLEAN NOT NULL DEFAULT TRUE,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT a2a_agents_header_auth CHECK ((auth = 'header') = (auth_header IS NOT NULL))
);

-- For a message sent to an A2A agent (api `a2a`): the state its task ended the exchange in, such
-- as completed or input-required, and its context, which links a conversation's messages.
ALTER TABLE request_logs ADD COLUMN task_state TEXT, ADD COLUMN context_id TEXT;
