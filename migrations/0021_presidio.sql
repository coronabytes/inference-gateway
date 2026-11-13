-- Generic HTTP credentials (a base URL with no, Basic or Bearer authentication), for Presidio
-- guardrails: a self-hosted analyzer whose findings the gateway masks, blocks or flags.
ALTER TABLE credentials DROP CONSTRAINT credentials_provider_check,
    ADD CONSTRAINT credentials_provider_check
        CHECK (provider IN ('openai', 'azure', 'bedrock', 'anthropic', 'typesafe', 'mistral', 'vertex', 'gemini', 'http'));
ALTER TABLE guardrails DROP CONSTRAINT guardrails_provider_check,
    ADD CONSTRAINT guardrails_provider_check CHECK (provider IN ('bedrock', 'mistral', 'model_armor', 'presidio'));
