//! The request log and the prompts and responses stored with it: writing batches, listing and
//! filtering entries, their bodies, and the tools and labels they carry.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::{accounting::RequestRecord, capture::Stored};

/// Bulk insert request logs and roll spend into the owning keys, in one transaction.
pub async fn write_request_batch(pool: &PgPool, batch: &[RequestRecord]) -> sqlx::Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let mut ids = Vec::with_capacity(batch.len());
    let mut ts = Vec::with_capacity(batch.len());
    let mut key_ids = Vec::with_capacity(batch.len());
    let mut models = Vec::with_capacity(batch.len());
    let mut dep_ids = Vec::with_capacity(batch.len());
    let mut providers = Vec::with_capacity(batch.len());
    let mut apis = Vec::with_capacity(batch.len());
    let mut streams = Vec::with_capacity(batch.len());
    let mut statuses = Vec::with_capacity(batch.len());
    let mut latencies = Vec::with_capacity(batch.len());
    let mut ttfts = Vec::with_capacity(batch.len());
    let mut ins = Vec::with_capacity(batch.len());
    let mut outs = Vec::with_capacity(batch.len());
    let mut crs = Vec::with_capacity(batch.len());
    let mut cws = Vec::with_capacity(batch.len());
    let mut costs = Vec::with_capacity(batch.len());
    let mut attempts = Vec::with_capacity(batch.len());
    let mut errors = Vec::with_capacity(batch.len());
    let mut parents = Vec::with_capacity(batch.len());
    let mut projects = Vec::with_capacity(batch.len());
    let mut labels = Vec::with_capacity(batch.len());
    let mut guardrails = Vec::with_capacity(batch.len());
    let mut guardrail_costs = Vec::with_capacity(batch.len());
    let mut searches = Vec::with_capacity(batch.len());
    let mut search_costs = Vec::with_capacity(batch.len());
    let mut bodies = Vec::with_capacity(batch.len());
    let mut mcp_tools = Vec::with_capacity(batch.len());
    let mut task_states = Vec::with_capacity(batch.len());
    let mut context_ids = Vec::with_capacity(batch.len());
    for r in batch {
        ids.push(r.id);
        ts.push(r.ts);
        key_ids.push(r.key_id);
        models.push(r.model.clone());
        dep_ids.push(r.deployment_id);
        providers.push(r.provider.map(|p| p.as_str().to_string()));
        apis.push(r.api.to_string());
        streams.push(r.stream);
        statuses.push(r.status_code as i32);
        latencies.push(r.latency_ms as i32);
        ttfts.push(r.ttft_ms.map(|v| v as i32));
        ins.push(r.usage.input_tokens as i64);
        outs.push(r.usage.output_tokens as i64);
        crs.push(r.usage.cache_read_tokens as i64);
        cws.push(r.usage.cache_write_tokens as i64);
        costs.push(r.cost_usd);
        attempts.push(r.attempts as i32);
        errors.push(r.error.clone());
        parents.push(r.parent_id);
        projects.push(r.project_id);
        labels.push(r.labels.to_json());
        guardrails.push(r.guardrails.clone());
        guardrail_costs.push(r.guardrail_cost_usd);
        searches.push(r.usage.search_units as i64);
        search_costs.push(r.search_cost_usd);
        bodies.push(r.bodies_status.map(str::to_string));
        mcp_tools.push(r.tool.clone());
        task_states.push(r.task_state.clone());
        context_ids.push(r.context_id.clone());
    }

    let mut tx = pool.begin().await?;
    // Nullable columns are passed as arrays with NULL elements; `as _` skips the macro's
    // element-type check, which does not model nullable array elements.
    sqlx::query!(
        "INSERT INTO request_logs (id, ts, key_id, model_name, deployment_id, provider, api, stream,
             status_code, latency_ms, ttft_ms, input_tokens, output_tokens, cache_read_tokens,
             cache_write_tokens, cost_usd, attempts, error, parent_id, project_id, labels, guardrails, guardrail_cost_usd,
             searches, search_cost_usd, bodies, tool, task_state, context_id)
         SELECT * FROM UNNEST($1::uuid[], $2::timestamptz[], $3::uuid[], $4::text[], $5::uuid[],
             $6::text[], $7::text[], $8::bool[], $9::int[], $10::int[], $11::int[], $12::bigint[],
             $13::bigint[], $14::bigint[], $15::bigint[], $16::float8[], $17::int[], $18::text[], $19::uuid[],
             $20::uuid[], $21::jsonb[], $22::jsonb[], $23::float8[], $24::bigint[], $25::float8[], $26::text[], $27::text[], $28::text[], $29::text[])",
        &ids,
        &ts,
        &key_ids as _,
        &models,
        &dep_ids as _,
        &providers as _,
        &apis,
        &streams,
        &statuses,
        &latencies,
        &ttfts as _,
        &ins,
        &outs,
        &crs,
        &cws,
        &costs,
        &attempts,
        &errors as _,
        &parents as _,
        &projects as _,
        &labels as _,
        &guardrails as _,
        &guardrail_costs,
        &searches,
        &search_costs,
        &bodies as _,
        &mcp_tools as _,
        &task_states as _,
        &context_ids as _,
    )
    .execute(&mut *tx)
    .await?;

    // A routed request's parent entry carries the cost of its children; count it once.
    let mut spend: HashMap<Uuid, f64> = Default::default();
    for r in batch {
        if let Some(k) = r.key_id
            && r.parent_id.is_none()
            && r.cost_usd > 0.0
        {
            *spend.entry(k).or_default() += r.cost_usd;
        }
    }
    if !spend.is_empty() {
        let (kids, amounts): (Vec<Uuid>, Vec<f64>) = spend.into_iter().unzip();
        sqlx::query!(
            "UPDATE virtual_keys k SET spend_usd = k.spend_usd + s.amount
             FROM UNNEST($1::uuid[], $2::float8[]) AS s(id, amount) WHERE k.id = s.id",
            &kids,
            &amounts,
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
}

/// Stores the bodies of logged requests, once their entries are written. Each row's tool names
/// travel joined by U+001F (UNNEST cannot take an array of arrays).
pub async fn write_body_batch(pool: &PgPool, rows: Vec<(Uuid, DateTime<Utc>, Option<Uuid>, Stored)>) -> sqlx::Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let n = rows.len();
    let (mut ids, mut ts, mut keys) = (Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n));
    let (mut requests, mut screened, mut guardrails, mut responses) =
        (Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n));
    let (mut request_bytes, mut response_bytes) = (Vec::with_capacity(n), Vec::with_capacity(n));
    let (mut tools, mut texts) = (Vec::with_capacity(n), Vec::with_capacity(n));
    let (mut response_ids, mut previous_ids, mut conversations) = (Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n));
    for (id, at, key, s) in rows {
        ids.push(id);
        ts.push(at);
        keys.push(key);
        requests.push(s.request);
        screened.push(s.screened);
        guardrails.push(s.guardrails);
        responses.push(s.response);
        request_bytes.push(s.request_bytes.min(i32::MAX as usize) as i32);
        response_bytes.push(s.response_bytes.min(i32::MAX as usize) as i32);
        tools.push(s.tools.join("\u{1f}"));
        texts.push(s.text);
        response_ids.push(s.response_id);
        previous_ids.push(s.previous_response_id);
        conversations.push(s.conversation.map(|c| c.to_vec()));
    }
    sqlx::query!(
        r#"INSERT INTO request_bodies (id, ts, key_id, request, screened_request, guardrails, response, request_bytes,
               response_bytes, tool_calls, search, response_id, previous_response_id, conversation)
           SELECT b.id, b.ts, b.key_id, b.request, b.screened, b.guardrails, b.response, b.request_bytes, b.response_bytes,
                  CASE WHEN b.tools = '' THEN '{}'::text[] ELSE string_to_array(b.tools, chr(31)) END,
                  to_tsvector('simple', b.text), b.response_id, b.previous_response_id, b.conversation
           FROM UNNEST($1::uuid[], $2::timestamptz[], $3::uuid[], $4::jsonb[], $5::jsonb[], $6::jsonb[], $7::jsonb[],
                       $8::int[], $9::int[], $10::text[], $11::text[], $12::text[], $13::text[], $14::bytea[])
                AS b(id, ts, key_id, request, screened, guardrails, response, request_bytes, response_bytes, tools, text,
                     response_id, previous_response_id, conversation)
           ON CONFLICT (id) DO NOTHING"#,
        &ids,
        &ts,
        &keys as _,
        &requests as _,
        &screened as _,
        &guardrails as _,
        &responses as _,
        &request_bytes,
        &response_bytes,
        &tools,
        &texts,
        &response_ids as _,
        &previous_ids as _,
        &conversations as _,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Marks the entries whose bodies could not be stored.
pub async fn mark_bodies_dropped(pool: &PgPool, ids: &[Uuid]) -> sqlx::Result<()> {
    sqlx::query!("UPDATE request_logs SET bodies = 'dropped' WHERE id = ANY($1)", ids).execute(pool).await?;
    Ok(())
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct LogRow {
    pub id: Uuid,
    pub ts: DateTime<Utc>,
    #[schema(required)]
    pub key_id: Option<Uuid>,
    #[schema(required)]
    pub key_name: Option<String>,
    pub model_name: String,
    #[schema(required)]
    pub deployment_id: Option<Uuid>,
    #[schema(required)]
    pub provider: Option<String>,
    /// The deployment's provider model and the credential it calls with, as configured now;
    /// none once the deployment is deleted.
    #[schema(required)]
    pub upstream_model: Option<String>,
    #[schema(required)]
    pub credential_name: Option<String>,
    /// The tools its response called, where its bodies are logged (see `capture`).
    #[schema(required)]
    pub tool_calls: Option<Vec<String>>,
    /// The tool an MCP call (api `mcp`) called, on the server named by `model_name`.
    #[schema(required)]
    pub tool: Option<String>,
    /// A message to an A2A agent (api `a2a`): the state its task was left in, such as `completed`.
    #[schema(required)]
    pub task_state: Option<String>,
    /// A message to an A2A agent: its context, shared by a conversation's messages.
    #[schema(required)]
    pub context_id: Option<String>,
    pub api: String,
    pub stream: bool,
    pub status_code: i32,
    pub latency_ms: i32,
    #[schema(required)]
    pub ttft_ms: Option<i32>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub cost_usd: f64,
    pub attempts: i32,
    #[schema(required)]
    pub error: Option<String>,
    /// Set on the calls a routed request made (decision, routed call): the router's entry.
    #[schema(required)]
    pub parent_id: Option<Uuid>,
    /// The labels the client sent, as `{"key": "value"}`.
    #[schema(value_type = Option<HashMap<String, String>>)]
    #[schema(required)]
    pub labels: Option<serde_json::Value>,
    /// What the guardrails did, stored with the entry.
    #[schema(value_type = Option<crate::api::guard::GuardrailSummary>)]
    #[schema(required)]
    pub guardrails: Option<serde_json::Value>,
    /// The guardrails' part of `cost_usd`.
    pub guardrail_cost_usd: f64,
    /// Searches billed: web searches a chat model ran, or a rerank model's searches.
    pub searches: i64,
    /// The searches' part of `cost_usd`.
    pub search_cost_usd: f64,
    /// Whether its bodies were logged: `stored`, `truncated` (cut to the size limit), `expired`
    /// (past their retention), `dropped` (could not be kept) or `deleted`; none when not logged.
    #[schema(required)]
    pub bodies: Option<String>,
}

/// What the request log is filtered by (see `recent_logs`).
#[derive(Default)]
pub struct LogFilter<'a> {
    pub limit: i64,
    /// Part of the model name, ignoring case.
    pub model: Option<&'a str>,
    pub key_id: Option<Uuid>,
    /// The entries of keys that no longer exist: the log keeps their id.
    pub deleted_keys: bool,
    pub errors_only: bool,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub projects: Option<&'a [Uuid]>,
    /// A JSON object: entries with all of these labels.
    pub labels: Option<&'a serde_json::Value>,
    /// Entries whose bodies are stored (prompt logging).
    pub bodies: bool,
    /// Full-text search of the stored bodies (`websearch_to_tsquery` syntax).
    pub text: Option<&'a str>,
    /// Entries whose response called this tool, and MCP calls of it.
    pub tool: Option<&'a str>,
    /// Entries the guardrails did any of these to (`passed`, `flagged`, `masked`, `blocked`).
    pub guardrail: Option<&'a [String]>,
    /// Days logged bodies are kept: older entries report theirs as expired.
    pub body_days: u32,
}

impl LogFilter<'_> {
    /// Whether the filter looks inside stored bodies.
    pub fn by_content(&self) -> bool {
        self.bodies || self.text.is_some() || self.tool.is_some()
    }
}

/// The request log, newest first, with the calls routed requests made. Filtered by content, it
/// also returns the router entries of matching calls, so they can be shown together.
pub async fn recent_logs(pool: &PgPool, f: &LogFilter<'_>) -> sqlx::Result<Vec<LogRow>> {
    let mut rows = match f.errors_only {
        true => error_logs(pool, f).await?,
        false => all_logs(pool, f).await?,
    };
    let mut seen: std::collections::HashSet<Uuid> = rows.iter().map(|r| r.id).collect();
    if f.by_content() {
        let parents: Vec<Uuid> = rows.iter().filter_map(|r| r.parent_id).filter(|p| !seen.contains(p)).collect();
        let found = logs_by_id(pool, &parents, f.body_days).await?;
        rows.extend(found.into_iter().filter(|r| seen.insert(r.id)));
    }
    // Router entries come with the calls they made, so the log can show them together.
    let parents: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
    let children = sqlx::query_as!(
        LogRow,
        r#"SELECT l.id, l.ts, l.key_id, k.name AS "key_name?", l.model_name, l.deployment_id, l.provider,
                  l.api, l.stream, l.status_code, l.latency_ms, l.ttft_ms, l.input_tokens, l.output_tokens,
                  l.cache_read_tokens, l.cache_write_tokens, l.cost_usd, l.attempts, l.error, l.parent_id, l.labels,
                  l.guardrails, l.guardrail_cost_usd, l.searches, l.search_cost_usd, l.tool, l.task_state, l.context_id,
                  (SELECT d.upstream_model FROM deployments d WHERE d.id = l.deployment_id) AS "upstream_model?",
                  (SELECT c.name FROM deployments d JOIN credentials c ON c.id = d.credential_id WHERE d.id = l.deployment_id) AS "credential_name?", (SELECT b.tool_calls FROM request_bodies b WHERE b.id = l.id) AS "tool_calls?",
                  CASE WHEN l.bodies IN ('stored', 'truncated') AND l.ts < now() - make_interval(days => $2)
                       THEN 'expired' ELSE l.bodies END AS "bodies?"
           FROM request_logs l LEFT JOIN virtual_keys k ON k.id = l.key_id
           WHERE l.parent_id = ANY($1) ORDER BY l.ts"#,
        &parents,
        f.body_days as i32,
    )
    .fetch_all(pool)
    .await?;
    rows.extend(children.into_iter().filter(|c| seen.insert(c.id)));
    Ok(rows)
}

/// The failed requests of the log, newest first: `all_logs` with `status_code >= 400` written out,
/// so Postgres walks the partial index of failed entries (`request_logs_errors_ts_idx`) rather than
/// every entry until it has found enough failures.
async fn error_logs(pool: &PgPool, f: &LogFilter<'_>) -> sqlx::Result<Vec<LogRow>> {
    sqlx::query_as!(
        LogRow,
        r#"SELECT l.id, l.ts, l.key_id, k.name AS "key_name?", l.model_name, l.deployment_id, l.provider,
                  l.api, l.stream, l.status_code, l.latency_ms, l.ttft_ms, l.input_tokens, l.output_tokens,
                  l.cache_read_tokens, l.cache_write_tokens, l.cost_usd, l.attempts, l.error, l.parent_id, l.labels,
                  l.guardrails, l.guardrail_cost_usd, l.searches, l.search_cost_usd, l.tool, l.task_state, l.context_id,
                  (SELECT d.upstream_model FROM deployments d WHERE d.id = l.deployment_id) AS "upstream_model?",
                  (SELECT c.name FROM deployments d JOIN credentials c ON c.id = d.credential_id WHERE d.id = l.deployment_id) AS "credential_name?", b.tool_calls AS "tool_calls?",
                  CASE WHEN l.bodies IN ('stored', 'truncated') AND l.ts < now() - make_interval(days => $12)
                       THEN 'expired' ELSE l.bodies END AS "bodies?"
           FROM request_logs l LEFT JOIN virtual_keys k ON k.id = l.key_id
                LEFT JOIN request_bodies b ON b.id = l.id
           WHERE l.status_code >= 400
             AND ($2::text IS NULL OR strpos(lower(l.model_name), lower($2)) > 0)
             AND ($3::uuid IS NULL OR l.key_id = $3)
             AND ($4::timestamptz IS NULL OR l.ts >= $4)
             AND ($5::timestamptz IS NULL OR l.ts < $5)
             AND ($6::uuid[] IS NULL OR l.project_id = ANY($6))
             AND ($7::jsonb IS NULL OR l.labels @> $7)
             AND (NOT $8 OR (l.key_id IS NOT NULL AND k.id IS NULL))
             AND (NOT $9 OR b.id IS NOT NULL)
             AND ($10::text IS NULL OR b.search @@ websearch_to_tsquery('simple', $10))
             AND ($11::text IS NULL OR b.tool_calls @> ARRAY[$11::text] OR l.tool = $11)
             AND ($13::text[] IS NULL OR l.guardrails->>'action' = ANY($13))
           ORDER BY l.ts DESC LIMIT $1"#,
        f.limit,
        f.model,
        f.key_id,
        f.since,
        f.until,
        f.projects,
        f.labels,
        f.deleted_keys,
        f.bodies,
        f.text,
        f.tool,
        f.body_days as i32,
        f.guardrail,
    )
    .fetch_all(pool)
    .await
}

/// The request log, newest first, by `f` except `errors_only`.
async fn all_logs(pool: &PgPool, f: &LogFilter<'_>) -> sqlx::Result<Vec<LogRow>> {
    sqlx::query_as!(
        LogRow,
        r#"SELECT l.id, l.ts, l.key_id, k.name AS "key_name?", l.model_name, l.deployment_id, l.provider,
                  l.api, l.stream, l.status_code, l.latency_ms, l.ttft_ms, l.input_tokens, l.output_tokens,
                  l.cache_read_tokens, l.cache_write_tokens, l.cost_usd, l.attempts, l.error, l.parent_id, l.labels,
                  l.guardrails, l.guardrail_cost_usd, l.searches, l.search_cost_usd, l.tool, l.task_state, l.context_id,
                  (SELECT d.upstream_model FROM deployments d WHERE d.id = l.deployment_id) AS "upstream_model?",
                  (SELECT c.name FROM deployments d JOIN credentials c ON c.id = d.credential_id WHERE d.id = l.deployment_id) AS "credential_name?", b.tool_calls AS "tool_calls?",
                  CASE WHEN l.bodies IN ('stored', 'truncated') AND l.ts < now() - make_interval(days => $12)
                       THEN 'expired' ELSE l.bodies END AS "bodies?"
           FROM request_logs l LEFT JOIN virtual_keys k ON k.id = l.key_id
                LEFT JOIN request_bodies b ON b.id = l.id
           WHERE ($2::text IS NULL OR strpos(lower(l.model_name), lower($2)) > 0)
             AND ($3::uuid IS NULL OR l.key_id = $3)
             AND ($4::timestamptz IS NULL OR l.ts >= $4)
             AND ($5::timestamptz IS NULL OR l.ts < $5)
             AND ($6::uuid[] IS NULL OR l.project_id = ANY($6))
             AND ($7::jsonb IS NULL OR l.labels @> $7)
             AND (NOT $8 OR (l.key_id IS NOT NULL AND k.id IS NULL))
             AND (NOT $9 OR b.id IS NOT NULL)
             AND ($10::text IS NULL OR b.search @@ websearch_to_tsquery('simple', $10))
             AND ($11::text IS NULL OR b.tool_calls @> ARRAY[$11::text] OR l.tool = $11)
             AND ($13::text[] IS NULL OR l.guardrails->>'action' = ANY($13))
           ORDER BY l.ts DESC LIMIT $1"#,
        f.limit,
        f.model,
        f.key_id,
        f.since,
        f.until,
        f.projects,
        f.labels,
        f.deleted_keys,
        f.bodies,
        f.text,
        f.tool,
        f.body_days as i32,
        f.guardrail,
    )
    .fetch_all(pool)
    .await
}

async fn logs_by_id(pool: &PgPool, ids: &[Uuid], body_days: u32) -> sqlx::Result<Vec<LogRow>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as!(
        LogRow,
        // No join: on an empty table Postgres plans one with request_logs as the inner side, and
        // sqlx then takes its columns for nullable, unlike on a table with rows.
        r#"SELECT l.id, l.ts, l.key_id, (SELECT k.name FROM virtual_keys k WHERE k.id = l.key_id) AS "key_name?", l.model_name,
                  l.deployment_id, l.provider, l.api, l.stream, l.status_code, l.latency_ms, l.ttft_ms, l.input_tokens, l.output_tokens,
                  l.cache_read_tokens, l.cache_write_tokens, l.cost_usd, l.attempts, l.error, l.parent_id, l.labels,
                  l.guardrails, l.guardrail_cost_usd, l.searches, l.search_cost_usd, l.tool, l.task_state, l.context_id,
                  (SELECT d.upstream_model FROM deployments d WHERE d.id = l.deployment_id) AS "upstream_model?",
                  (SELECT c.name FROM deployments d JOIN credentials c ON c.id = d.credential_id WHERE d.id = l.deployment_id) AS "credential_name?", (SELECT b.tool_calls FROM request_bodies b WHERE b.id = l.id) AS "tool_calls?",
                  CASE WHEN l.bodies IN ('stored', 'truncated') AND l.ts < now() - make_interval(days => $2)
                       THEN 'expired' ELSE l.bodies END AS "bodies?"
           FROM request_logs l
           WHERE l.id = ANY($1)"#,
        ids,
        body_days as i32,
    )
    .fetch_all(pool)
    .await
}

/// One entry's stored bodies, with what decides who may read them.
pub struct BodiesRow {
    pub id: Uuid,
    pub ts: DateTime<Utc>,
    pub project_id: Option<Uuid>,
    pub key_id: Option<Uuid>,
    pub status: Option<String>,
    pub request: Option<serde_json::Value>,
    pub screened_request: Option<serde_json::Value>,
    pub guardrails: Option<serde_json::Value>,
    pub response: Option<serde_json::Value>,
    pub request_bytes: i32,
    pub response_bytes: i32,
    pub response_id: Option<String>,
    pub previous_response_id: Option<String>,
    pub conversation: Option<Vec<u8>>,
}

pub async fn log_bodies(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<BodiesRow>> {
    sqlx::query_as!(
        BodiesRow,
        r#"SELECT l.id, l.ts, l.project_id, l.key_id, l.bodies AS status, b.request, b.screened_request,
                  b.guardrails, b.response, b.request_bytes, b.response_bytes, b.response_id, b.previous_response_id,
                  b.conversation
           FROM request_logs l JOIN request_bodies b ON b.id = l.id WHERE l.id = $1"#,
        id
    )
    .fetch_optional(pool)
    .await
}

/// The logged turns before and after `row` in its conversation, of the same key and in
/// `projects`: by Responses id where the turns chain that way, otherwise by the conversation's
/// opening (see `affinity::Affinity::fingerprint`).
pub async fn turn_links(pool: &PgPool, row: &BodiesRow, projects: Option<&[Uuid]>) -> sqlx::Result<(Option<Uuid>, Option<Uuid>)> {
    let r = sqlx::query!(
        r#"SELECT
             (SELECT p.id FROM request_bodies p JOIN request_logs pl ON pl.id = p.id
              WHERE p.key_id = $1 AND p.id <> $2 AND ($6::uuid[] IS NULL OR pl.project_id = ANY($6))
                AND CASE WHEN $3::text IS NOT NULL THEN p.response_id = $3
                         ELSE $4::bytea IS NOT NULL AND p.conversation = $4 AND p.ts < $5 END
              ORDER BY p.ts DESC LIMIT 1) AS previous,
             (SELECT n.id FROM request_bodies n JOIN request_logs nl ON nl.id = n.id
              WHERE n.key_id = $1 AND n.id <> $2 AND ($6::uuid[] IS NULL OR nl.project_id = ANY($6))
                AND (($7::text IS NOT NULL AND n.previous_response_id = $7)
                     OR ($4::bytea IS NOT NULL AND n.conversation = $4 AND n.ts > $5))
              ORDER BY n.ts LIMIT 1) AS next"#,
        row.key_id,
        row.id,
        row.previous_response_id,
        row.conversation,
        row.ts,
        projects,
        row.response_id,
    )
    .fetch_one(pool)
    .await?;
    Ok((r.previous, r.next))
}

/// A tool the latest logged responses called, with how many of them did (`recent_tools`).
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ToolUse {
    pub name: String,
    pub requests: i64,
}

/// The tools the latest `limit` logged responses in `projects` called, most used first.
pub async fn recent_tools(pool: &PgPool, limit: i64, projects: Option<&[Uuid]>) -> sqlx::Result<Vec<ToolUse>> {
    sqlx::query_as!(
        ToolUse,
        r#"SELECT t AS "name!", count(*) AS "requests!"
           FROM (SELECT b.tool_calls FROM request_bodies b JOIN request_logs l ON l.id = b.id
                 WHERE cardinality(b.tool_calls) > 0 AND ($2::uuid[] IS NULL OR l.project_id = ANY($2))
                 ORDER BY b.ts DESC LIMIT $1) s, unnest(s.tool_calls) t
           GROUP BY t ORDER BY 2 DESC, 1 LIMIT 100"#,
        limit,
        projects,
    )
    .fetch_all(pool)
    .await
}

/// Whose stored bodies to count or delete: a key's, or those of the requests made in a project.
#[derive(Clone, Copy)]
pub enum BodiesOf {
    Key(Uuid),
    Project(Uuid),
}

pub async fn count_bodies(pool: &PgPool, of: BodiesOf) -> sqlx::Result<i64> {
    match of {
        BodiesOf::Key(id) => {
            sqlx::query_scalar!(r#"SELECT count(*) AS "n!" FROM request_bodies WHERE key_id = $1"#, id).fetch_one(pool).await
        }
        BodiesOf::Project(id) => {
            sqlx::query_scalar!(
                r#"SELECT count(*) AS "n!" FROM request_bodies b JOIN request_logs l ON l.id = b.id WHERE l.project_id = $1"#,
                id
            )
            .fetch_one(pool)
            .await
        }
    }
}

/// Deletes the stored bodies of a key or project, in chunks, and marks their entries as
/// `deleted`. Returns how many were deleted.
pub async fn delete_bodies(pool: &PgPool, of: BodiesOf) -> sqlx::Result<u64> {
    let mut deleted = 0;
    loop {
        let n = match of {
            BodiesOf::Key(id) => sqlx::query!(
                "WITH d AS (DELETE FROM request_bodies WHERE id IN (SELECT id FROM request_bodies WHERE key_id = $1 LIMIT 5000) RETURNING id)
                 UPDATE request_logs l SET bodies = 'deleted' FROM d WHERE l.id = d.id",
                id
            )
            .execute(pool)
            .await?
            .rows_affected(),
            BodiesOf::Project(id) => sqlx::query!(
                "WITH d AS (DELETE FROM request_bodies WHERE id IN (
                     SELECT b.id FROM request_bodies b JOIN request_logs l ON l.id = b.id WHERE l.project_id = $1 LIMIT 5000)
                     RETURNING id)
                 UPDATE request_logs l SET bodies = 'deleted' FROM d WHERE l.id = d.id",
                id
            )
            .execute(pool)
            .await?
            .rows_affected(),
        };
        deleted += n;
        if n < 5000 {
            return Ok(deleted);
        }
    }
}

/// A label value of the latest labelled requests, with how many of them carry it (`recent_labels`).
pub struct LabelUse {
    pub key: String,
    pub value: String,
    pub requests: i64,
}

/// The label keys and values of the latest `limit` labelled requests in `projects`, ordered by key
/// and then by use, most used first. The console suggests them in its label filters.
pub async fn recent_labels(pool: &PgPool, limit: i64, projects: Option<&[Uuid]>) -> sqlx::Result<Vec<LabelUse>> {
    sqlx::query_as!(
        LabelUse,
        r#"SELECT e.key AS "key!", e.value AS "value!", count(*) AS "requests!"
           FROM (SELECT labels FROM request_logs
                 WHERE labels IS NOT NULL AND ($2::uuid[] IS NULL OR project_id = ANY($2))
                 ORDER BY ts DESC LIMIT $1) l
           CROSS JOIN LATERAL jsonb_each_text(l.labels) e
           GROUP BY e.key, e.value
           ORDER BY e.key, count(*) DESC, e.value"#,
        limit,
        projects,
    )
    .fetch_all(pool)
    .await
}
