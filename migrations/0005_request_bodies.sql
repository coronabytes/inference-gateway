-- Whether an entry's bodies were logged (see request_bodies): stored, truncated (stored, cut to the
-- size limit), dropped (the gateway could not keep them: its buffer was full, or the database
-- refused them) or deleted (on demand, for a key or a project; the audit log says who). NULL: not
-- logged. Retention does not update it: the request log reports bodies past their retention as
-- expired. Nullable without a default, so adding it does not rewrite the table.
ALTER TABLE request_logs ADD COLUMN bodies TEXT CHECK (bodies IN ('stored', 'truncated', 'dropped', 'deleted'));

-- The bodies of logged requests, for debugging, in their API's own JSON shape: the request as the
-- client sent it, as the model got it when guardrails changed it, what each guardrail found and did
-- (including the text it matched), and the response as the client got it, a stream put back
-- together into one document. Images, files, embedding vectors and encrypted provider blobs are
-- replaced by placeholders, and long strings cut to the size limit. Not encrypted. Kept for
-- `prompt_log_retention_days`, and never longer than their entry.
CREATE TABLE request_bodies (
    id                   UUID PRIMARY KEY REFERENCES request_logs (id) ON DELETE CASCADE,
    -- The entry's ts, so pruning and searching need no join.
    ts                   TIMESTAMPTZ NOT NULL,
    request              JSONB COMPRESSION lz4,
    screened_request     JSONB COMPRESSION lz4,
    guardrails           JSONB COMPRESSION lz4,
    response             JSONB COMPRESSION lz4,
    -- Sizes as sent and received, before placeholders and cuts.
    request_bytes        INTEGER NOT NULL,
    response_bytes       INTEGER NOT NULL,
    -- The tools the model called in the response (function tools, and server tools such as
    -- web_search), for "which requests called tool X".
    tool_calls           TEXT[] NOT NULL DEFAULT '{}',
    -- Full-text index of the request as the client sent it, whatever the guardrails did, and of
    -- the response: their text, tool arguments and search queries. Not the masked request.
    search               TSVECTOR,
    -- Links between the logged turns of a conversation: the entry's key, the Responses ids that
    -- chain turns, and for other APIs a keyed hash of how the conversation starts (the one sticky
    -- routing uses), never its text.
    key_id               UUID,
    response_id          TEXT,
    previous_response_id TEXT,
    conversation         BYTEA
);
CREATE INDEX request_bodies_ts_idx ON request_bodies (ts);
CREATE INDEX request_bodies_tools_idx ON request_bodies USING GIN (tool_calls);
CREATE INDEX request_bodies_search_idx ON request_bodies USING GIN (search);
CREATE INDEX request_bodies_response_id_idx ON request_bodies (key_id, response_id) WHERE response_id IS NOT NULL;
CREATE INDEX request_bodies_previous_idx ON request_bodies (key_id, previous_response_id) WHERE previous_response_id IS NOT NULL;
CREATE INDEX request_bodies_conversation_idx ON request_bodies (key_id, conversation, ts) WHERE conversation IS NOT NULL;
