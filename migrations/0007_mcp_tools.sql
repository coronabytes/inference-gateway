-- Which of an MCP server's tools keys may call, and what a call costs. `allowed_tools` empty means
-- every tool the server offers, new ones included; `denied_tools` are refused either way. A call is
-- charged `tool_prices[tool]` (USD per call), else `price_per_call`, once the tool has answered.
ALTER TABLE mcp_servers
    ADD COLUMN allowed_tools  TEXT[] NOT NULL DEFAULT '{}',
    ADD COLUMN denied_tools   TEXT[] NOT NULL DEFAULT '{}',
    ADD COLUMN price_per_call DOUBLE PRECISION NOT NULL DEFAULT 0 CHECK (price_per_call >= 0),
    ADD COLUMN tool_prices    JSONB NOT NULL DEFAULT '{}'::jsonb;
