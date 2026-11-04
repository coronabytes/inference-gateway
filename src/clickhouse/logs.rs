//! The request log and logged bodies in ClickHouse: writing batches, listing and filtering
//! entries, their bodies, and the tools and labels they carry. Mirrors `db::logs`; what Postgres
//! joins in (key names, deployments, credentials) comes from Postgres after the query.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use super::{ClickHouse, Params, Result, time};
use crate::{
    accounting::RequestRecord,
    capture::Stored,
    db::{BodiesOf, BodiesRow, LabelUse, LogFilter, LogRow, ToolUse},
};

#[derive(Serialize)]
pub(super) struct LogInsert {
    pub id: Uuid,
    pub ts: String,
    pub key_id: Option<Uuid>,
    pub project_id: Option<Uuid>,
    pub model_name: String,
    pub deployment_id: Option<Uuid>,
    pub provider: Option<String>,
    pub api: String,
    pub stream: bool,
    pub status_code: u16,
    pub latency_ms: u32,
    pub ttft_ms: Option<u32>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cost_usd: f64,
    pub attempts: u32,
    pub error: Option<String>,
    pub parent_id: Option<Uuid>,
    pub labels: serde_json::Value,
    pub guardrails: Option<String>,
    pub guardrail_cost_usd: f64,
    pub searches: u64,
    pub search_cost_usd: f64,
    pub audio_seconds: u64,
    pub characters: u64,
    pub bodies: Option<String>,
    pub tool: Option<String>,
    pub task_state: Option<String>,
    pub context_id: Option<String>,
}

impl From<&RequestRecord> for LogInsert {
    fn from(r: &RequestRecord) -> Self {
        Self {
            id: r.id,
            ts: time(r.ts),
            key_id: r.key_id,
            project_id: r.project_id,
            model_name: r.model.clone(),
            deployment_id: r.deployment_id,
            provider: r.provider.map(|p| p.as_str().to_string()),
            api: r.api.to_string(),
            stream: r.stream,
            status_code: r.status_code,
            latency_ms: r.latency_ms.min(u32::MAX as u64) as u32,
            ttft_ms: r.ttft_ms.map(|v| v.min(u32::MAX as u64) as u32),
            input_tokens: r.usage.input_tokens,
            output_tokens: r.usage.output_tokens,
            cache_read_tokens: r.usage.cache_read_tokens,
            cache_write_tokens: r.usage.cache_write_tokens,
            cost_usd: r.cost_usd,
            attempts: r.attempts,
            error: r.error.clone(),
            parent_id: r.parent_id,
            labels: r.labels.to_json().unwrap_or_else(|| serde_json::json!({})),
            guardrails: r.guardrails.as_ref().map(|g| g.to_string()),
            guardrail_cost_usd: r.guardrail_cost_usd,
            searches: r.usage.search_units,
            search_cost_usd: r.search_cost_usd,
            audio_seconds: r.usage.audio_seconds,
            characters: r.usage.characters,
            bodies: r.bodies_status.map(str::to_string),
            tool: r.tool.clone(),
            task_state: r.task_state.clone(),
            context_id: r.context_id.clone(),
        }
    }
}

#[derive(Serialize)]
struct BodyInsert {
    id: Uuid,
    ts: String,
    key_id: Option<Uuid>,
    project_id: Option<Uuid>,
    request: Option<String>,
    screened_request: Option<String>,
    guardrails: Option<String>,
    response: Option<String>,
    request_bytes: u32,
    response_bytes: u32,
    tool_calls: Vec<String>,
    search: String,
    response_id: Option<String>,
    previous_response_id: Option<String>,
    conversation: Option<String>,
}

impl ClickHouse {
    /// One insert of the batch's entries. No spend is rolled into Postgres: budgets count from
    /// `spend_hours`, which the insert feeds.
    pub async fn write_request_batch(&self, batch: &[RequestRecord]) -> Result<()> {
        let rows: Vec<LogInsert> = batch.iter().map(LogInsert::from).collect();
        self.insert("request_logs", &rows, false).await
    }

    /// Stores the bodies of logged requests, once their entries are written. An entry whose bodies
    /// fail to store reads as `dropped` (see `bodies_status`), so nothing needs marking.
    pub async fn write_body_batch(&self, rows: Vec<(Uuid, DateTime<Utc>, Option<Uuid>, Option<Uuid>, Stored)>) -> Result<()> {
        let json = |v: Option<serde_json::Value>| v.map(|v| v.to_string());
        let rows: Vec<BodyInsert> = rows
            .into_iter()
            .map(|(id, ts, key_id, project_id, s)| BodyInsert {
                id,
                ts: time(ts),
                key_id,
                project_id,
                request: json(s.request),
                screened_request: json(s.screened),
                guardrails: json(s.guardrails),
                response: json(s.response),
                request_bytes: s.request_bytes.min(u32::MAX as usize) as u32,
                response_bytes: s.response_bytes.min(u32::MAX as usize) as u32,
                tool_calls: s.tools,
                search: s.text.to_lowercase(),
                response_id: s.response_id,
                previous_response_id: s.previous_response_id,
                conversation: s.conversation.map(hex::encode),
            })
            .collect();
        self.insert("request_bodies", &rows, false).await
    }
}

/// A request log entry as ClickHouse returns it, before Postgres fills in names.
#[derive(Deserialize)]
struct Entry {
    id: Uuid,
    ts: DateTime<Utc>,
    key_id: Option<Uuid>,
    model_name: String,
    deployment_id: Option<Uuid>,
    provider: Option<String>,
    api: String,
    stream: bool,
    status_code: i32,
    latency_ms: i32,
    ttft_ms: Option<i32>,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    cost_usd: f64,
    attempts: i32,
    error: Option<String>,
    parent_id: Option<Uuid>,
    labels: HashMap<String, String>,
    guardrails: Option<String>,
    guardrail_cost_usd: f64,
    searches: i64,
    search_cost_usd: f64,
    audio_seconds: i64,
    characters: i64,
    bodies: Option<String>,
    tool: Option<String>,
    task_state: Option<String>,
    context_id: Option<String>,
}

const ENTRY_COLUMNS: &str = "l.id, l.ts, l.key_id, l.model_name, l.deployment_id, l.provider, l.api, l.stream, l.status_code, l.latency_ms,
    l.ttft_ms, l.input_tokens, l.output_tokens, l.cache_read_tokens, l.cache_write_tokens, l.cost_usd, l.attempts, l.error,
    l.parent_id, l.labels, l.guardrails, l.guardrail_cost_usd, l.searches, l.search_cost_usd, l.audio_seconds, l.characters,
    l.bodies, l.tool, l.task_state, l.context_id";

/// SQL conditions and their parameters, built up from a filter.
#[derive(Default)]
pub(super) struct Where {
    clauses: Vec<String>,
    params: Params,
    n: usize,
}

impl Where {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn and(&mut self, clause: impl Into<String>) {
        self.clauses.push(clause.into());
    }

    /// A fresh parameter name.
    pub fn name(&mut self) -> String {
        self.n += 1;
        format!("w{}", self.n)
    }

    pub fn with(&mut self, f: impl FnOnce(Params) -> Params) {
        self.params = f(std::mem::take(&mut self.params));
    }

    pub fn sql(&self) -> String {
        if self.clauses.is_empty() { "1".into() } else { self.clauses.join("\n  AND ") }
    }

    pub fn into_params(self) -> Params {
        self.params
    }

    /// Entries with all of `labels` (a JSON object of strings).
    pub fn labels(&mut self, col: &str, labels: Option<&serde_json::Value>) {
        let Some(serde_json::Value::Object(m)) = labels else { return };
        for (k, v) in m {
            let (kn, vn) = (self.name(), self.name());
            let v = v.as_str().map_or_else(|| v.to_string(), str::to_string);
            self.and(format!("{col}[{{{kn}:String}}] = {{{vn}:String}}"));
            self.with(|p| p.str(&kn, k).str(&vn, &v));
        }
    }

    pub fn projects(&mut self, col: &str, projects: Option<&[Uuid]>) {
        if let Some(p) = projects {
            let n = self.name();
            self.and(format!("{col} IS NOT NULL AND has({{{n}:Array(UUID)}}, assumeNotNull({col}))"));
            self.with(|q| q.uuids(&n, p));
        }
    }

    /// The entries of keys that no longer exist: `existing` holds every key there is.
    pub fn deleted_keys(&mut self, col: &str, existing: &[Uuid]) {
        let n = self.name();
        self.and(format!("{col} IS NOT NULL AND NOT has({{{n}:Array(UUID)}}, assumeNotNull({col}))"));
        self.with(|q| q.uuids(&n, existing));
    }

    pub fn range(&mut self, col: &str, since: Option<DateTime<Utc>>, until: Option<DateTime<Utc>>) {
        if let Some(s) = since {
            let n = self.name();
            self.and(format!("{col} >= {{{n}:DateTime64(3, 'UTC')}}"));
            self.with(|q| q.time(&n, s));
        }
        if let Some(u) = until {
            let n = self.name();
            self.and(format!("{col} < {{{n}:DateTime64(3, 'UTC')}}"));
            self.with(|q| q.time(&n, u));
        }
    }
}

/// Every key id in Postgres, for "entries of deleted keys".
pub(super) async fn existing_keys(pg: &PgPool) -> sqlx::Result<Vec<Uuid>> {
    sqlx::query_scalar::<_, Uuid>("SELECT id FROM virtual_keys").fetch_all(pg).await
}

/// Full-text search in websearch style: words (all must be there), `"quoted phrases"`, `-word`
/// to exclude, and `or` between alternatives. Words match whole tokens of the lower-cased text.
pub(super) fn text_condition(w: &mut Where, col: &str, query: &str) -> Option<String> {
    let mut groups: Vec<Vec<String>> = vec![Vec::new()];
    let mut rest = query.trim();
    while !rest.is_empty() {
        let negate = rest.starts_with('-');
        if negate {
            rest = &rest[1..];
        }
        let (term, phrase, tail) = if let Some(r) = rest.strip_prefix('"') {
            let end = r.find('"').unwrap_or(r.len());
            (&r[..end], true, r.get(end + 1..).unwrap_or(""))
        } else {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            (&rest[..end], false, &rest[end..])
        };
        rest = tail.trim_start();
        let term = term.to_lowercase();
        if !phrase && !negate && term == "or" {
            groups.push(Vec::new());
            continue;
        }
        let tokens: Vec<&str> = term.split(|c: char| c.is_ascii() && !c.is_ascii_alphanumeric()).filter(|t| !t.is_empty()).collect();
        if tokens.is_empty() {
            continue;
        }
        let cond = if phrase && tokens.len() > 1 {
            let n = w.name();
            let needle = term.trim().to_string();
            w.with(|p| p.str(&n, &needle));
            // The tokens narrow by index; the position checks the order.
            let mut parts: Vec<String> = tokens
                .iter()
                .map(|t| {
                    let tn = w.name();
                    let t = t.to_string();
                    w.with(|p| p.str(&tn, &t));
                    format!("hasToken({col}, {{{tn}:String}})")
                })
                .collect();
            parts.push(format!("position({col}, {{{n}:String}}) > 0"));
            format!("({})", parts.join(" AND "))
        } else {
            let parts: Vec<String> = tokens
                .iter()
                .map(|t| {
                    let tn = w.name();
                    let t = t.to_string();
                    w.with(|p| p.str(&tn, &t));
                    format!("hasToken({col}, {{{tn}:String}})")
                })
                .collect();
            format!("({})", parts.join(" AND "))
        };
        groups.last_mut().expect("a group").push(if negate { format!("NOT {cond}") } else { cond });
    }
    let groups: Vec<String> = groups.into_iter().filter(|g| !g.is_empty()).map(|g| format!("({})", g.join(" AND "))).collect();
    (!groups.is_empty()).then(|| format!("({})", groups.join(" OR ")))
}

/// What Postgres knows about the entries' keys and deployments.
struct Names {
    keys: HashMap<Uuid, String>,
    deployments: HashMap<Uuid, (String, Option<String>)>,
}

async fn names(pg: &PgPool, entries: &[Entry]) -> sqlx::Result<Names> {
    let keys: Vec<Uuid> = entries.iter().filter_map(|e| e.key_id).collect::<HashSet<_>>().into_iter().collect();
    let deps: Vec<Uuid> = entries.iter().filter_map(|e| e.deployment_id).collect::<HashSet<_>>().into_iter().collect();
    let (keys, deployments) = tokio::try_join!(
        sqlx::query_as::<_, (Uuid, String)>("SELECT id, name FROM virtual_keys WHERE id = ANY($1)").bind(&keys).fetch_all(pg),
        sqlx::query_as::<_, (Uuid, String, Option<String>)>(
            "SELECT d.id, d.upstream_model, c.name FROM deployments d LEFT JOIN credentials c ON c.id = d.credential_id WHERE d.id = ANY($1)"
        )
        .bind(&deps)
        .fetch_all(pg),
    )?;
    Ok(Names { keys: keys.into_iter().collect(), deployments: deployments.into_iter().map(|(id, m, c)| (id, (m, c))).collect() })
}

/// A body row's presence and tools, for the entries' `bodies` and `tool_calls`.
#[derive(Deserialize)]
struct BodyTools {
    id: Uuid,
    tool_calls: Vec<String>,
}

impl ClickHouse {
    async fn entries(&self, w: Where, limit: i64) -> Result<Vec<Entry>> {
        let sql = format!("SELECT {ENTRY_COLUMNS} FROM request_logs l WHERE {} ORDER BY l.ts DESC LIMIT {{limit:UInt32}}", w.sql());
        self.rows(&sql, w.into_params().num("limit", limit.clamp(0, u32::MAX as i64))).await
    }

    async fn body_tools(&self, entries: &[Entry]) -> Result<HashMap<Uuid, Vec<String>>> {
        let ids: Vec<Uuid> = entries.iter().filter(|e| e.bodies.is_some()).map(|e| e.id).collect();
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let (lo, hi) =
            entries.iter().fold((DateTime::<Utc>::MAX_UTC, DateTime::<Utc>::MIN_UTC), |(lo, hi), e| (lo.min(e.ts), hi.max(e.ts)));
        let rows: Vec<BodyTools> = self
            .rows(
                "SELECT id, tool_calls FROM request_bodies
                 WHERE ts >= {lo:DateTime64(3, 'UTC')} AND ts <= {hi:DateTime64(3, 'UTC')} AND has({ids:Array(UUID)}, id)",
                Params::new().time("lo", lo).time("hi", hi).uuids("ids", &ids),
            )
            .await?;
        Ok(rows.into_iter().map(|r| (r.id, r.tool_calls)).collect())
    }

    /// The entries as the admin API returns them: names from Postgres, bodies' state and tools.
    async fn finish(&self, pg: &PgPool, entries: Vec<Entry>, body_days: u32) -> super::super::logstore::Result<Vec<LogRow>> {
        let (names, tools) = tokio::join!(names(pg, &entries), self.body_tools(&entries));
        let (names, tools) = (names?, tools?);
        let expired_before = Utc::now() - chrono::Duration::days(body_days as i64);
        Ok(entries
            .into_iter()
            .map(|e| {
                let body = tools.get(&e.id);
                let bodies = match e.bodies.as_deref() {
                    Some("stored" | "truncated") if e.ts < expired_before => Some("expired".to_string()),
                    Some("stored" | "truncated") if body.is_none() => Some("dropped".to_string()),
                    other => other.map(str::to_string),
                };
                let dep = e.deployment_id.and_then(|d| names.deployments.get(&d));
                LogRow {
                    id: e.id,
                    ts: e.ts,
                    key_id: e.key_id,
                    key_name: e.key_id.and_then(|k| names.keys.get(&k).cloned()),
                    model_name: e.model_name,
                    deployment_id: e.deployment_id,
                    provider: e.provider,
                    upstream_model: dep.map(|d| d.0.clone()),
                    credential_name: dep.and_then(|d| d.1.clone()),
                    tool_calls: body.cloned(),
                    tool: e.tool,
                    task_state: e.task_state,
                    context_id: e.context_id,
                    api: e.api,
                    stream: e.stream,
                    status_code: e.status_code,
                    latency_ms: e.latency_ms,
                    ttft_ms: e.ttft_ms,
                    input_tokens: e.input_tokens,
                    output_tokens: e.output_tokens,
                    cache_read_tokens: e.cache_read_tokens,
                    cache_write_tokens: e.cache_write_tokens,
                    cost_usd: e.cost_usd,
                    attempts: e.attempts,
                    error: e.error,
                    parent_id: e.parent_id,
                    labels: (!e.labels.is_empty()).then(|| serde_json::to_value(&e.labels).unwrap_or_default()),
                    guardrails: e.guardrails.and_then(|g| serde_json::from_str(&g).ok()),
                    guardrail_cost_usd: e.guardrail_cost_usd,
                    searches: e.searches,
                    search_cost_usd: e.search_cost_usd,
                    audio_seconds: e.audio_seconds,
                    characters: e.characters,
                    bodies,
                }
            })
            .collect())
    }

    /// The request log, newest first, with the calls routed requests made (see `db::recent_logs`).
    pub async fn recent_logs(&self, pg: &PgPool, f: &LogFilter<'_>) -> super::super::logstore::Result<Vec<LogRow>> {
        let mut w = Where::new();
        if f.errors_only {
            w.and("l.status_code >= 400");
        }
        if let Some(m) = f.model {
            let n = w.name();
            w.and(format!("positionCaseInsensitiveUTF8(l.model_name, {{{n}:String}}) > 0"));
            w.with(|p| p.str(&n, m));
        }
        if let Some(k) = f.key_id {
            let n = w.name();
            w.and(format!("l.key_id = {{{n}:UUID}}"));
            w.with(|p| p.uuid(&n, k));
        }
        w.range("l.ts", f.since, f.until);
        w.projects("l.project_id", f.projects);
        w.labels("l.labels", f.labels);
        if f.deleted_keys {
            w.deleted_keys("l.key_id", &existing_keys(pg).await?);
        }
        if let Some(g) = f.guardrail {
            let n = w.name();
            w.and(format!("has({{{n}:Array(String)}}, l.guardrail_action)"));
            w.with(|p| p.strs(&n, g));
        }
        // Content: the ids of matching bodies, from the same time range.
        if f.bodies || f.text.is_some() {
            let mut b = Where::new();
            b.n = 1000;
            b.range("b.ts", f.since, f.until);
            if let Some(t) = f.text {
                match text_condition(&mut b, "b.search", t) {
                    Some(c) => b.and(c),
                    None => b.and("0"),
                }
            }
            w.and(format!("l.id IN (SELECT b.id FROM request_bodies b WHERE {})", b.sql()));
            let bp = b.into_params();
            w.with(|p| Params(p.0.into_iter().chain(bp.0).collect()));
        }
        if let Some(t) = f.tool {
            let n = w.name();
            let mut since = String::new();
            if let Some(s) = f.since {
                let sn = w.name();
                since = format!(" AND b.ts >= {{{sn}:DateTime64(3, 'UTC')}}");
                w.with(|p| p.time(&sn, s));
            }
            w.and(format!(
                "(l.tool = {{{n}:String}} OR l.id IN (SELECT b.id FROM request_bodies b WHERE has(b.tool_calls, {{{n}:String}}){since}))"
            ));
            w.with(|p| p.str(&n, t));
        }
        let mut rows = self.entries(w, f.limit).await?;
        let mut seen: HashSet<Uuid> = rows.iter().map(|r| r.id).collect();
        if f.by_content() {
            let parents: Vec<Uuid> = rows.iter().filter_map(|r| r.parent_id).filter(|p| !seen.contains(p)).collect();
            let found = self.entries_by_id(&parents).await?;
            rows.extend(found.into_iter().filter(|r| seen.insert(r.id)));
        }
        // Router entries come with the calls they made, so the log can show them together.
        let parents: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
        if let Some(lo) = rows.iter().map(|r| r.ts).min() {
            let children: Vec<Entry> = self
                .rows(
                    &format!(
                        "SELECT {ENTRY_COLUMNS} FROM request_logs l
                         WHERE l.ts >= {{lo:DateTime64(3, 'UTC')}} - INTERVAL 1 DAY AND l.parent_id IS NOT NULL
                           AND has({{ids:Array(UUID)}}, assumeNotNull(l.parent_id))
                         ORDER BY l.ts"
                    ),
                    Params::new().time("lo", lo).uuids("ids", &parents),
                )
                .await?;
            rows.extend(children.into_iter().filter(|c| seen.insert(c.id)));
        }
        self.finish(pg, rows, f.body_days).await
    }

    async fn entries_by_id(&self, ids: &[Uuid]) -> Result<Vec<Entry>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        self.rows(
            &format!("SELECT {ENTRY_COLUMNS} FROM request_logs l WHERE has({{ids:Array(UUID)}}, l.id)"),
            Params::new().uuids("ids", ids),
        )
        .await
    }

    pub async fn log_bodies(&self, id: Uuid) -> Result<Option<BodiesRow>> {
        #[derive(Deserialize)]
        struct Row {
            id: Uuid,
            ts: DateTime<Utc>,
            project_id: Option<Uuid>,
            key_id: Option<Uuid>,
            status: Option<String>,
            request: Option<String>,
            screened_request: Option<String>,
            guardrails: Option<String>,
            response: Option<String>,
            request_bytes: i32,
            response_bytes: i32,
            response_id: Option<String>,
            previous_response_id: Option<String>,
            conversation: Option<String>,
        }
        let row: Option<Row> = self
            .row(
                "SELECT b.id AS id, b.ts AS ts, l.project_id AS project_id, b.key_id AS key_id, l.bodies AS status, b.request AS request,
                        b.screened_request AS screened_request, b.guardrails AS guardrails, b.response AS response,
                        b.request_bytes AS request_bytes, b.response_bytes AS response_bytes, b.response_id AS response_id,
                        b.previous_response_id AS previous_response_id, b.conversation AS conversation
                 FROM (SELECT * FROM request_bodies WHERE id = {id:UUID} LIMIT 1) b
                 JOIN (SELECT id, project_id, bodies FROM request_logs WHERE id = {id:UUID} LIMIT 1) l ON l.id = b.id",
                Params::new().uuid("id", id),
            )
            .await?;
        let json = |v: Option<String>| v.and_then(|s| serde_json::from_str(&s).ok());
        Ok(row.map(|r| BodiesRow {
            id: r.id,
            ts: r.ts,
            project_id: r.project_id,
            key_id: r.key_id,
            status: r.status,
            request: json(r.request),
            screened_request: json(r.screened_request),
            guardrails: json(r.guardrails),
            response: json(r.response),
            request_bytes: r.request_bytes,
            response_bytes: r.response_bytes,
            response_id: r.response_id,
            previous_response_id: r.previous_response_id,
            conversation: r.conversation.and_then(|c| hex::decode(c).ok()),
        }))
    }

    /// The logged turns before and after `row` in its conversation (see `db::turn_links`).
    pub async fn turn_links(&self, row: &BodiesRow, projects: Option<&[Uuid]>) -> Result<(Option<Uuid>, Option<Uuid>)> {
        #[derive(Deserialize)]
        struct Id {
            id: Uuid,
        }
        let Some(key) = row.key_id else { return Ok((None, None)) };
        let conversation = row.conversation.as_ref().map(hex::encode);
        let link = |cond: String, w: Where, order: &str| {
            let sql = format!("SELECT b.id AS id FROM request_bodies b WHERE {} AND {cond} ORDER BY b.ts {order} LIMIT 1", w.sql());
            (sql, w.into_params())
        };
        let base = || {
            let mut w = Where::new();
            w.and("b.key_id = {key:UUID} AND b.id != {me:UUID}");
            w.with(|p| p.uuid("key", key).uuid("me", row.id).time("at", row.ts));
            w.projects("b.project_id", projects);
            w
        };
        let previous = match (&row.previous_response_id, &conversation) {
            (Some(prev), _) => {
                let mut w = base();
                w.with(|p| p.str("rid", prev));
                Some(link("b.response_id = {rid:String}".into(), w, "DESC"))
            }
            (None, Some(c)) => {
                let mut w = base();
                w.with(|p| p.str("conv", c));
                Some(link("b.conversation = {conv:String} AND b.ts < {at:DateTime64(3, 'UTC')}".into(), w, "DESC"))
            }
            (None, None) => None,
        };
        let mut nexts = Vec::new();
        if row.response_id.is_some() {
            nexts.push("b.previous_response_id = {rid:String}");
        }
        if conversation.is_some() {
            nexts.push("(b.conversation = {conv:String} AND b.ts > {at:DateTime64(3, 'UTC')})");
        }
        let next = (!nexts.is_empty()).then(|| {
            let mut w = base();
            w.with(|p| p.opt_str("rid", row.response_id.as_deref()).opt_str("conv", conversation.as_deref()));
            link(format!("({})", nexts.join(" OR ")), w, "ASC")
        });
        let run = |q: Option<(String, Params)>| async move {
            match q {
                Some((sql, p)) => Ok::<_, super::Error>(self.row::<Id>(&sql, p).await?.map(|r| r.id)),
                None => Ok(None),
            }
        };
        let (p, n) = tokio::try_join!(run(previous), run(next))?;
        Ok((p, n))
    }

    pub async fn recent_tools(&self, limit: i64, projects: Option<&[Uuid]>) -> Result<Vec<ToolUse>> {
        let mut w = Where::new();
        w.and("notEmpty(b.tool_calls)");
        w.projects("b.project_id", projects);
        let sql = format!(
            "SELECT t AS name, count() AS requests
             FROM (SELECT b.tool_calls FROM request_bodies b WHERE {} ORDER BY b.ts DESC LIMIT {{limit:UInt32}}) ARRAY JOIN tool_calls AS t
             GROUP BY t ORDER BY requests DESC, name LIMIT 100",
            w.sql()
        );
        self.rows(&sql, w.into_params().num("limit", limit.max(0))).await
    }

    fn bodies_of(of: BodiesOf) -> (&'static str, Uuid) {
        match of {
            BodiesOf::Key(id) => ("key_id", id),
            BodiesOf::Project(id) => ("project_id", id),
        }
    }

    pub async fn count_bodies(&self, of: BodiesOf) -> Result<i64> {
        #[derive(Deserialize)]
        struct N {
            n: i64,
        }
        let (col, id) = Self::bodies_of(of);
        let n: Option<N> =
            self.row(&format!("SELECT count() AS n FROM request_bodies WHERE {col} = {{id:UUID}}"), Params::new().uuid("id", id)).await?;
        Ok(n.map_or(0, |n| n.n))
    }

    /// Deletes a key's or project's stored bodies and marks their entries as `deleted`. Returns
    /// how many were deleted.
    pub async fn delete_bodies(&self, of: BodiesOf) -> Result<u64> {
        let n = self.count_bodies(of).await?;
        if n == 0 {
            return Ok(0);
        }
        let (col, id) = Self::bodies_of(of);
        // A mutation, applied in the background: deleting bodies is rare, and their entries read as
        // `dropped` (no body row) until it is done.
        self.execute(
            &format!("ALTER TABLE request_logs UPDATE bodies = 'deleted' WHERE {col} = {{id:UUID}} AND bodies IN ('stored', 'truncated')"),
            Params::new().uuid("id", id),
        )
        .await?;
        self.execute(&format!("DELETE FROM request_bodies WHERE {col} = {{id:UUID}}"), Params::new().uuid("id", id)).await?;
        Ok(n as u64)
    }

    pub async fn recent_labels(&self, limit: i64, projects: Option<&[Uuid]>) -> Result<Vec<LabelUse>> {
        #[derive(Deserialize)]
        struct Row {
            key: String,
            value: String,
            requests: i64,
        }
        let mut w = Where::new();
        w.and("notEmpty(l.labels)");
        w.projects("l.project_id", projects);
        let sql = format!(
            "SELECT k AS key, v AS value, count() AS requests
             FROM (SELECT l.labels FROM request_logs l WHERE {} ORDER BY l.ts DESC LIMIT {{limit:UInt32}})
             ARRAY JOIN mapKeys(labels) AS k, mapValues(labels) AS v
             GROUP BY k, v ORDER BY k, requests DESC, v",
            w.sql()
        );
        let rows: Vec<Row> = self.rows(&sql, w.into_params().num("limit", limit.max(0))).await?;
        Ok(rows.into_iter().map(|r| LabelUse { key: r.key, value: r.value, requests: r.requests }).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_terms_become_token_checks() {
        let mut w = Where::new();
        let c = text_condition(&mut w, "s", "Hello -world \"new york\" or café").unwrap();
        assert_eq!(c.matches("hasToken").count(), 5, "{c}");
        assert!(c.contains("NOT (hasToken"), "{c}");
        assert!(c.contains("position(s,"), "{c}");
        assert!(c.contains(" OR "), "{c}");
        assert!(text_condition(&mut Where::new(), "s", "  - \"\" ").is_none());
    }
}
