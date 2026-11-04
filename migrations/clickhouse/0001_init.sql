-- The log store in ClickHouse (see src/clickhouse.rs). Statements are separated by a line holding
-- only a semicolon. Times are UTC milliseconds.

-- The request log, as in Postgres (migrations 0001, 0004, 0005, 0017). Weekly partitions: retention
-- drops whole weeks, and a gateway keeping its log forever still has few partitions.
CREATE TABLE IF NOT EXISTS request_logs (
    id                 UUID,
    ts                 DateTime64(3, 'UTC'),
    key_id             Nullable(UUID),
    project_id         Nullable(UUID),
    model_name         LowCardinality(String),
    deployment_id      Nullable(UUID),
    provider           LowCardinality(Nullable(String)),
    api                LowCardinality(String),
    stream             Bool,
    status_code        UInt16,
    latency_ms         UInt32,
    ttft_ms            Nullable(UInt32),
    input_tokens       UInt64,
    output_tokens      UInt64,
    cache_read_tokens  UInt64,
    cache_write_tokens UInt64,
    -- Includes guardrail_cost_usd and search_cost_usd.
    cost_usd           Float64,
    attempts           UInt32,
    error              Nullable(String),
    parent_id          Nullable(UUID),
    labels             Map(LowCardinality(String), String),
    -- What the guardrails did, as JSON text; its action, for the filter.
    guardrails         Nullable(String),
    guardrail_action   LowCardinality(String) MATERIALIZED JSONExtractString(ifNull(guardrails, ''), 'action'),
    guardrail_cost_usd Float64,
    searches           UInt64,
    search_cost_usd    Float64,
    audio_seconds      UInt64,
    characters         UInt64,
    -- stored, truncated, dropped or deleted; NULL when not logged. A stored entry without a row in
    -- request_bodies reads as dropped.
    bodies             LowCardinality(Nullable(String)),
    tool               Nullable(String),
    task_state         LowCardinality(Nullable(String)),
    context_id         Nullable(String),
    INDEX id_idx id TYPE bloom_filter GRANULARITY 4,
    INDEX key_idx key_id TYPE bloom_filter GRANULARITY 4,
    INDEX project_idx project_id TYPE bloom_filter GRANULARITY 4,
    INDEX parent_idx parent_id TYPE bloom_filter GRANULARITY 4,
    INDEX status_idx status_code TYPE minmax GRANULARITY 4
) ENGINE = MergeTree
PARTITION BY toMonday(ts)
ORDER BY (ts, id)
;
-- Logged bodies (Postgres migration 0005). The search text is lower-cased words of the request as
-- the client sent it and of the response, for `hasToken`. Daily partitions: kept 1 to 60 days.
CREATE TABLE IF NOT EXISTS request_bodies (
    id                   UUID,
    ts                   DateTime64(3, 'UTC'),
    key_id               Nullable(UUID),
    project_id           Nullable(UUID),
    request              Nullable(String) CODEC(ZSTD(3)),
    screened_request     Nullable(String) CODEC(ZSTD(3)),
    guardrails           Nullable(String) CODEC(ZSTD(3)),
    response             Nullable(String) CODEC(ZSTD(3)),
    request_bytes        UInt32,
    response_bytes       UInt32,
    tool_calls           Array(String),
    search               String CODEC(ZSTD(3)),
    response_id          Nullable(String),
    previous_response_id Nullable(String),
    -- Hex of the keyed hash of the conversation's opening.
    conversation         Nullable(String),
    INDEX id_idx id TYPE bloom_filter GRANULARITY 1,
    INDEX key_idx key_id TYPE bloom_filter GRANULARITY 1,
    INDEX project_idx project_id TYPE bloom_filter GRANULARITY 1,
    INDEX tools_idx tool_calls TYPE bloom_filter GRANULARITY 1,
    INDEX search_idx search TYPE tokenbf_v1(32768, 3, 0) GRANULARITY 1,
    INDEX response_idx response_id TYPE bloom_filter GRANULARITY 1,
    INDEX previous_idx previous_response_id TYPE bloom_filter GRANULARITY 1,
    INDEX conversation_idx conversation TYPE bloom_filter GRANULARITY 1
) ENGINE = MergeTree
PARTITION BY toDate(ts)
ORDER BY (ts, id)
;
-- The audit log (Postgres migration 0001). Monthly partitions; retention deletes by row.
CREATE TABLE IF NOT EXISTS audit_log (
    id          UUID,
    ts          DateTime64(3, 'UTC'),
    actor_kind  LowCardinality(String),
    actor_id    Nullable(UUID),
    actor_name  Nullable(String),
    action      LowCardinality(String),
    target_type LowCardinality(String),
    target_id   Nullable(UUID),
    target_name Nullable(String),
    details     String
) ENGINE = MergeTree
PARTITION BY toYYYYMM(ts)
ORDER BY ts
;
-- Spend per key and per project and hour, as budgets count it (a routed request's calls are in its
-- router entry's cost). Budget counters in Valkey are rebuilt from it: whole hours from here, the
-- first, partial hour from request_logs. Kept longer than any budget period with its reset.
CREATE TABLE IF NOT EXISTS spend_hours (
    kind     Enum8('key' = 1, 'project' = 2),
    spender  UUID,
    hour     DateTime('UTC'),
    cost_usd SimpleAggregateFunction(sum, Float64)
) ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(hour)
ORDER BY (kind, spender, hour)
TTL hour + INTERVAL 70 DAY
;
CREATE MATERIALIZED VIEW IF NOT EXISTS spend_hours_keys TO spend_hours AS
SELECT 'key' AS kind, assumeNotNull(key_id) AS spender, toStartOfHour(ts) AS hour, sum(request_logs.cost_usd) AS cost_usd
FROM request_logs
WHERE key_id IS NOT NULL AND parent_id IS NULL AND request_logs.cost_usd > 0
GROUP BY spender, hour
;
CREATE MATERIALIZED VIEW IF NOT EXISTS spend_hours_projects TO spend_hours AS
SELECT 'project' AS kind, assumeNotNull(project_id) AS spender, toStartOfHour(ts) AS hour, sum(request_logs.cost_usd) AS cost_usd
FROM request_logs
WHERE project_id IS NOT NULL AND parent_id IS NULL AND request_logs.cost_usd > 0
GROUP BY spender, hour
;
-- The one-time copy of the current budget periods from Postgres (see src/clickhouse/import.rs).
CREATE TABLE IF NOT EXISTS _pg_import (
    since       DateTime64(3, 'UTC'),
    rows        UInt64,
    imported_at DateTime DEFAULT now()
) ENGINE = MergeTree
ORDER BY imported_at
