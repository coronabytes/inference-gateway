-- Tool routers: a name that model requests use like an MCP server's, whose decision model picks,
-- per request, which of its routes' tools the model gets. A route is a group of one MCP server's
-- tools with a description of when it is needed.
CREATE TABLE tool_routers (
    id             UUID PRIMARY KEY,
    -- Lowercase letters, digits, '-' and '_', and no MCP server's name.
    name           TEXT NOT NULL UNIQUE,
    description    TEXT,
    -- A decision model (mode decision) by its model name.
    decision_model TEXT NOT NULL,
    -- The question the decision model gets for every route; NULL for the default.
    instructions   TEXT,
    -- [{"name", "description", "server_id", "tools": [...]}]; empty tools for all of the server's.
    routes         JSONB NOT NULL DEFAULT '[]',
    -- A route is offered when the decision model is at least this sure it is needed.
    threshold      DOUBLE PRECISION NOT NULL DEFAULT 0.5 CHECK (threshold > 0 AND threshold < 1),
    -- At most this many routes per request, the surest first; NULL for no limit.
    max_routes     INTEGER CHECK (max_routes > 0),
    enabled        BOOLEAN NOT NULL DEFAULT TRUE,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);
