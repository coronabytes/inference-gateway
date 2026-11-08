//! Tests against a real ClickHouse, each in a database of its own that is dropped afterwards.
//! They are skipped unless `GATEWAY_TEST_CLICKHOUSE_URL` is set (with `_USER` and `_PASSWORD`;
//! `docker compose up -d clickhouse` and the values from `.env.example`).

use chrono::{Duration, DurationRound, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use super::{ClickHouse, ClickHouseConfig, Params};
use crate::{
    accounting::{Logged, RequestRecord},
    budget::Spender,
    capture::Stored,
    db::{BodiesOf, LogFilter},
    ir::Usage,
    labels::Labels,
};

/// A migrated, empty database and what drops it, or `None` when no test ClickHouse is configured.
async fn fresh() -> Option<(ClickHouse, Cleanup)> {
    let _ = dotenvy::dotenv();
    let url = std::env::var("GATEWAY_TEST_CLICKHOUSE_URL").ok().filter(|u| !u.is_empty())?;
    let cfg = |database: &str| ClickHouseConfig {
        url: url.clone(),
        user: std::env::var("GATEWAY_TEST_CLICKHOUSE_USER").unwrap_or_else(|_| "default".into()),
        password: std::env::var("GATEWAY_TEST_CLICKHOUSE_PASSWORD").unwrap_or_default(),
        database: database.into(),
    };
    let name = format!("test_{}", Uuid::now_v7().simple());
    let admin = ClickHouse::new(cfg("default")).unwrap();
    admin.execute(&format!("CREATE DATABASE {name}"), Params::new()).await.unwrap();
    let ch = ClickHouse::new(cfg(&name)).unwrap();
    ch.migrate().await.unwrap();
    Some((ch, Cleanup { admin, name }))
}

/// Drops the test database.
struct Cleanup {
    admin: ClickHouse,
    name: String,
}

impl Cleanup {
    async fn run(self) {
        self.admin.execute(&format!("DROP DATABASE {}", self.name), Params::new()).await.unwrap();
    }
}

fn record(key: Uuid, project: Option<Uuid>, ago: Duration, status: u16, cost: f64) -> RequestRecord {
    RequestRecord {
        id: Uuid::now_v7(),
        ts: Utc::now() - ago,
        key_id: Some(key),
        key_name: None,
        project_id: project,
        model: "gpt-test".into(),
        deployment_id: None,
        provider: None,
        api: "chat",
        stream: false,
        status_code: status,
        latency_ms: 100,
        ttft_ms: None,
        usage: Usage { input_tokens: 10, output_tokens: 5, ..Default::default() },
        cost_usd: cost,
        attempts: 1,
        error: (status >= 400).then(|| "failed".into()),
        parent_id: None,
        labels: Labels::default(),
        guardrails: None,
        guardrail_cost_usd: 0.0,
        search_cost_usd: 0.0,
        bodies: Logged::default(),
        bodies_status: None,
        tool: None,
        task_state: None,
        context_id: None,
    }
}

fn stored(text: &str, tools: &[&str], response_id: Option<&str>, previous: Option<&str>) -> Stored {
    Stored {
        request: Some(serde_json::json!({ "input": text })),
        screened: None,
        guardrails: None,
        response: Some(serde_json::json!({ "output": "ok" })),
        request_bytes: text.len(),
        response_bytes: 2,
        truncated: false,
        tools: tools.iter().map(|t| t.to_string()).collect(),
        text: text.into(),
        response_id: response_id.map(str::to_string),
        previous_response_id: previous.map(str::to_string),
        conversation: None,
    }
}

#[sqlx::test]
async fn the_request_log_filters_and_aggregates(pg: PgPool) {
    let Some((ch, cleanup)) = fresh().await else { return };
    let (key, project) = (Uuid::now_v7(), Uuid::now_v7());
    let mut router = record(key, Some(project), Duration::minutes(5), 200, 1.0);
    router.labels = Labels::parse("tenant=acme,env=prod").unwrap();
    router.guardrails = Some(serde_json::json!({ "action": "masked" }));
    let mut call = record(key, Some(project), Duration::minutes(5), 200, 0.4);
    call.parent_id = Some(router.id);
    let failed = record(key, Some(project), Duration::minutes(3), 500, 0.0);
    let other = record(Uuid::now_v7(), None, Duration::minutes(2), 200, 7.0);
    ch.write_request_batch(&[router.clone(), call.clone(), failed.clone(), other.clone()]).await.unwrap();

    let all = ch.recent_logs(&pg, &LogFilter { limit: 10, body_days: 7, ..Default::default() }).await.unwrap();
    assert_eq!(all.len(), 4);
    assert_eq!(all[0].id, other.id, "newest first");
    let r = all.iter().find(|r| r.id == router.id).unwrap();
    assert_eq!(r.labels, Some(serde_json::json!({ "env": "prod", "tenant": "acme" })));
    assert_eq!(r.guardrails, Some(serde_json::json!({ "action": "masked" })));

    let errors = ch.recent_logs(&pg, &LogFilter { limit: 10, errors_only: true, body_days: 7, ..Default::default() }).await.unwrap();
    assert_eq!(errors.iter().map(|r| r.id).collect::<Vec<_>>(), [failed.id]);

    let labels = serde_json::json!({ "tenant": "acme" });
    let labelled = ch.recent_logs(&pg, &LogFilter { limit: 10, labels: Some(&labels), body_days: 7, ..Default::default() }).await.unwrap();
    // The router entry, and the call it made.
    assert_eq!(labelled.iter().map(|r| r.id).collect::<Vec<_>>(), [router.id, call.id]);

    let masked = ["masked".to_string()];
    let guarded =
        ch.recent_logs(&pg, &LogFilter { limit: 10, guardrail: Some(&masked), body_days: 7, ..Default::default() }).await.unwrap();
    assert_eq!(guarded[0].id, router.id);

    let projects = [project];
    let in_project =
        ch.recent_logs(&pg, &LogFilter { limit: 10, projects: Some(&projects), body_days: 7, ..Default::default() }).await.unwrap();
    assert_eq!(in_project.len(), 3);
    // No key exists in Postgres, so every entry is a deleted key's.
    let deleted = ch.recent_logs(&pg, &LogFilter { limit: 10, deleted_keys: true, body_days: 7, ..Default::default() }).await.unwrap();
    assert_eq!(deleted.len(), 4);

    let s = super::Selection {
        since: Utc::now() - Duration::hours(1),
        until: Utc::now() + Duration::minutes(1),
        key_id: Some(key),
        deleted_keys: false,
        models: None,
        exclude: &[],
        projects: None,
        labels: None,
        api: None,
    };
    let totals = ch.selection_totals(&pg, &s).await.unwrap();
    assert_eq!((totals.requests, totals.errors, totals.input_tokens), (3, 1, 30));
    assert_eq!(totals.p95_latency_ms, Some(100));
    let series = ch.selection_series(&pg, &s, 3600).await.unwrap();
    assert_eq!(series.iter().map(|b| b.requests).sum::<i64>(), 3);
    let by_model = ch.group_totals(&pg, &s, crate::db::Group::Model).await.unwrap();
    assert_eq!(by_model.len(), 1);
    let by_key = ch.group_totals(&pg, &super::Selection { key_id: None, ..s }, crate::db::Group::Key).await.unwrap();
    let key_row = by_key.iter().find(|t| t.subject == key.to_string()).unwrap();
    assert_eq!(key_row.requests, 3);
    let by_project = ch.group_totals(&pg, &super::Selection { key_id: None, ..s }, crate::db::Group::Project).await.unwrap();
    assert!(by_project.iter().any(|t| t.subject == project.to_string()));
    assert_eq!(by_project.iter().map(|t| t.requests).sum::<i64>(), by_key.iter().map(|t| t.requests).sum::<i64>());
    let billing = ch.billing_totals(&pg, &super::Selection { key_id: None, ..s }).await.unwrap();
    let ours = billing.iter().find(|t| t.key_id == Some(key)).unwrap();
    assert_eq!((ours.project_id, ours.model.as_str(), ours.requests, ours.input_tokens), (Some(project), "gpt-test", 3, 30));
    let theirs = billing.iter().find(|t| t.key_id == other.key_id).unwrap();
    assert_eq!(theirs.project_id, None);
    assert!((theirs.cost_usd - 7.0).abs() < 1e-9);
    let tenants = ch.label_totals(&pg, &super::Selection { key_id: None, ..s }, &["tenant".into()], None, 10).await.unwrap();
    let tenants: Vec<_> = tenants.iter().map(|t| (t.v1.as_deref(), t.v2.as_deref(), t.model.as_str(), t.requests)).collect();
    // Costliest first: the entries without labels here (the call, the failure and 7.0), then the router entry.
    assert_eq!(tenants, [(None, None, "", 3), (Some("acme"), None, "", 1)]);
    let acme = ch.label_totals(&pg, &s, &["tenant".into()], Some([Some("acme"), None, None]), 10).await.unwrap();
    assert_eq!(acme.iter().map(|t| (t.model.as_str(), t.requests)).collect::<Vec<_>>(), [("gpt-test", 1)]);
    let empty = ch.selection_totals(&pg, &super::Selection { key_id: Some(Uuid::now_v7()), ..s }).await.unwrap();
    assert_eq!((empty.requests, empty.p95_latency_ms), (0, None));

    let usage = ch.key_usage_since(&pg, Utc::now() - Duration::hours(1), None).await.unwrap();
    assert_eq!(usage[0].key_id, Some(other.key_id.unwrap()));
    // A routed request's call is in its router entry's cost.
    assert!((usage[1].cost_usd - 1.0).abs() < 1e-9, "{}", usage[1].cost_usd);
    let hours = ch.model_hours(Utc::now() - Duration::hours(2), &["gpt-test".into()], Some(key), None).await.unwrap();
    assert_eq!(hours.iter().map(|h| h.requests).sum::<i64>(), 3);
    let speeds = ch.model_speeds(Utc::now() - Duration::hours(2), &["gpt-test".into()], None, None).await.unwrap();
    assert_eq!(speeds[0].p50_latency_ms, Some(100));
    assert_eq!(ch.key_minutes(key, Utc::now() - Duration::hours(1)).await.unwrap().iter().map(|m| m.requests).sum::<i64>(), 2);
    let days = ch.key_daily_spend(key, Utc::now() - Duration::days(1)).await.unwrap();
    assert!((days.iter().map(|d| d.1).sum::<f64>() - 1.0).abs() < 1e-9);
    let label_uses = ch.recent_labels(100, None).await.unwrap();
    assert_eq!(label_uses.iter().map(|l| (l.key.as_str(), l.value.as_str())).collect::<Vec<_>>(), [("env", "prod"), ("tenant", "acme")]);
    cleanup.run().await;
}

#[sqlx::test]
async fn spend_counts_whole_hours_and_the_partial_first_one(pg: PgPool) {
    let _ = pg;
    let Some((ch, cleanup)) = fresh().await else { return };
    let (key, project) = (Uuid::now_v7(), Uuid::now_v7());
    let hour = Utc::now().duration_trunc(Duration::hours(1)).unwrap();
    let at = |r: &mut RequestRecord, ts| r.ts = ts;
    let mut early = record(key, Some(project), Duration::zero(), 200, 2.0);
    at(&mut early, hour - Duration::minutes(90));
    let mut before_reset = record(key, Some(project), Duration::zero(), 200, 4.0);
    at(&mut before_reset, hour - Duration::minutes(50));
    let mut after_reset = record(key, Some(project), Duration::zero(), 200, 8.0);
    at(&mut after_reset, hour - Duration::minutes(20));
    let mut now = record(key, Some(project), Duration::zero(), 200, 16.0);
    at(&mut now, hour + Duration::seconds(1));
    let mut routed = record(key, Some(project), Duration::zero(), 200, 32.0);
    routed.parent_id = Some(now.id);
    ch.write_request_batch(&[early, before_reset, after_reset, now, routed]).await.unwrap();

    let all = ch.spend_since(Spender::Key(key), hour - Duration::hours(2)).await.unwrap();
    assert!((all - 30.0).abs() < 1e-9, "{all}");
    // A reset 30 minutes into the previous hour: that hour's rest from the log, then whole hours.
    let since_reset = ch.spend_since(Spender::Key(key), hour - Duration::minutes(30)).await.unwrap();
    assert!((since_reset - 24.0).abs() < 1e-9, "{since_reset}");
    let project_spend = ch.spend_since(Spender::Project(project), hour).await.unwrap();
    assert!((project_spend - 16.0).abs() < 1e-9, "{project_spend}");
    assert_eq!(ch.spend_since(Spender::Key(Uuid::now_v7()), hour).await.unwrap(), 0.0);
    cleanup.run().await;
}

#[sqlx::test]
async fn bodies_are_searched_linked_and_deleted(pg: PgPool) {
    let Some((ch, cleanup)) = fresh().await else { return };
    let (key, project) = (Uuid::now_v7(), Uuid::now_v7());
    let mut first = record(key, Some(project), Duration::minutes(3), 200, 0.0);
    let mut second = record(key, Some(project), Duration::minutes(2), 200, 0.0);
    let mut lost = record(key, Some(project), Duration::minutes(1), 200, 0.0);
    for r in [&mut first, &mut second, &mut lost] {
        r.bodies_status = Some("stored");
    }
    ch.write_request_batch(&[first.clone(), second.clone(), lost.clone()]).await.unwrap();
    ch.write_body_batch(vec![
        (first.id, first.ts, Some(key), Some(project), stored("The Quick brown-fox", &["web_search"], Some("resp_1"), None)),
        (second.id, second.ts, Some(key), Some(project), stored("lazy dog in New York", &[], Some("resp_2"), Some("resp_1"))),
    ])
    .await
    .unwrap();

    let f = |text: &'static str| LogFilter { limit: 10, text: Some(text), body_days: 7, ..Default::default() };
    let ids = |rows: Vec<crate::db::LogRow>| rows.into_iter().map(|r| r.id).collect::<Vec<_>>();
    assert_eq!(ids(ch.recent_logs(&pg, &f("quick fox")).await.unwrap()), [first.id]);
    assert_eq!(ids(ch.recent_logs(&pg, &f("\"new york\"")).await.unwrap()), [second.id]);
    assert_eq!(ids(ch.recent_logs(&pg, &f("\"york new\"")).await.unwrap()), Vec::<Uuid>::new());
    assert_eq!(ids(ch.recent_logs(&pg, &f("dog -lazy")).await.unwrap()), Vec::<Uuid>::new());
    assert_eq!(ids(ch.recent_logs(&pg, &f("quick or lazy")).await.unwrap()), [second.id, first.id]);
    let tool = LogFilter { limit: 10, tool: Some("web_search"), body_days: 7, ..Default::default() };
    assert_eq!(ids(ch.recent_logs(&pg, &tool).await.unwrap()), [first.id]);

    let all = ch.recent_logs(&pg, &LogFilter { limit: 10, body_days: 7, ..Default::default() }).await.unwrap();
    let status = |id| all.iter().find(|r| r.id == id).unwrap().bodies.clone();
    assert_eq!(status(first.id).as_deref(), Some("stored"));
    assert_eq!(status(lost.id).as_deref(), Some("dropped"), "no body row");
    assert_eq!(all.iter().find(|r| r.id == first.id).unwrap().tool_calls, Some(vec!["web_search".to_string()]));

    let row = ch.log_bodies(second.id).await.unwrap().unwrap();
    assert_eq!(row.request, Some(serde_json::json!({ "input": "lazy dog in New York" })));
    assert_eq!(row.project_id, Some(project));
    assert_eq!(ch.turn_links(&row, None).await.unwrap(), (Some(first.id), None));
    let row = ch.log_bodies(first.id).await.unwrap().unwrap();
    assert_eq!(ch.turn_links(&row, None).await.unwrap(), (None, Some(second.id)));
    assert_eq!(ch.turn_links(&row, Some(&[])).await.unwrap(), (None, None), "outside the caller's projects");

    assert_eq!(ch.recent_tools(10, None).await.unwrap().iter().map(|u| u.name.as_str()).collect::<Vec<_>>(), ["web_search"]);
    assert_eq!(ch.count_bodies(BodiesOf::Project(project)).await.unwrap(), 2);
    assert_eq!(ch.delete_bodies(BodiesOf::Key(key)).await.unwrap(), 2);
    assert_eq!(ch.count_bodies(BodiesOf::Key(key)).await.unwrap(), 0);
    assert!(ch.log_bodies(first.id).await.unwrap().is_none());
    cleanup.run().await;
}

#[sqlx::test]
async fn the_audit_log_lists_newest_first(pg: PgPool) {
    let _ = pg;
    let Some((ch, cleanup)) = fresh().await else { return };
    for (name, ago) in [("alpha", 2), ("beta", 1)] {
        let e = crate::db::AuditEntry {
            id: Uuid::now_v7(),
            ts: Utc::now() - Duration::minutes(ago),
            actor_kind: "dev".into(),
            actor_id: None,
            actor_name: Some("Local development".into()),
            action: "create".into(),
            target_type: "key".into(),
            target_id: Some(Uuid::now_v7()),
            target_name: Some(name.into()),
            details: serde_json::json!({ "name": name }),
        };
        ch.insert_audit(&e).await.unwrap();
    }
    let all = ch.list_audit(10, Some("key"), None, None, None).await.unwrap();
    assert_eq!(all.iter().map(|e| e.target_name.as_deref().unwrap()).collect::<Vec<_>>(), ["beta", "alpha"]);
    assert_eq!(all[0].details, serde_json::json!({ "name": "beta" }));
    let found = ch.list_audit(10, None, Some("ALP"), None, None).await.unwrap();
    assert_eq!(found.len(), 1);
    assert!(ch.list_audit(10, Some("deployment"), None, None, None).await.unwrap().is_empty());
    cleanup.run().await;
}

#[sqlx::test]
async fn retention_drops_whole_expired_partitions(pg: PgPool) {
    let _ = pg;
    let Some((ch, cleanup)) = fresh().await else { return };
    let key = Uuid::now_v7();
    let old = record(key, None, Duration::days(40), 200, 0.0);
    let new = record(key, None, Duration::hours(1), 200, 0.0);
    ch.write_request_batch(&[old, new.clone()]).await.unwrap();
    let (requests, _, _) = super::prune(&ch, 30, 0, 7).await.unwrap();
    assert_eq!(requests, 1);
    let left: Vec<serde_json::Value> = ch.rows("SELECT id FROM request_logs", Params::new()).await.unwrap();
    assert_eq!(left, [serde_json::json!({ "id": new.id })]);
    cleanup.run().await;
}

#[sqlx::test]
async fn a_spent_budget_is_refused_from_clickhouse_while_valkey_is_down(pg: PgPool) {
    let Some((ch, cleanup)) = fresh().await else { return };
    let mut key = crate::auth::tests::key();
    key.budget_period = Some("day".into());
    key.max_budget_usd = Some(1.0);
    ch.write_request_batch(&[record(key.id, None, Duration::seconds(1), 200, 1.5)]).await.unwrap();
    let state = crate::state::test_state_with_logs(pg, crate::logstore::LogStore::ClickHouse(std::sync::Arc::new(ch)));
    let err = crate::auth::check_key_limits(&state, &key).await.unwrap_err();
    assert_eq!(err.status, axum::http::StatusCode::TOO_MANY_REQUESTS);
    cleanup.run().await;
}
