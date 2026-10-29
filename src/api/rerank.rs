//! `POST /v1/rerank` (also `/rerank` and `/v2/rerank`): Cohere's rerank API, which vLLM, Jina
//! and most rerank servers speak too. Clients send a query and documents (strings, or objects
//! with a `text`); the response lists the documents by relevance, as Cohere's v2 API does, with
//! the documents' text when `return_documents` is set.
//!
//! Azure AI Foundry and OpenAI-compatible deployments get Cohere's v2 request; Bedrock
//! deployments (Amazon Rerank, Cohere Rerank) go through Bedrock's Rerank API. Each response is
//! rebuilt in the same shape, so clients see one format whichever deployment served them.
//!
//! Rerank bills per search: one query over up to 100 documents. Upstreams that report their
//! billed searches (Cohere) or tokens are charged what they report; the others are counted the
//! way they bill.

use std::sync::Arc;

use axum::{
    Json,
    extract::State,
    http::HeaderValue,
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use serde_json::{Value, json};

use crate::{
    accounting::Tracker,
    auth::ClientKey,
    labels::{ClientLabels, Labels},
    error::{Dialect, ErrorKind, GatewayError},
    ir::Usage,
    model::{Deployment, Mode, ProviderKind, VirtualKey},
    providers::{self, Op},
    state::AppState,
};

/// The most documents one request may rank (Cohere's and Bedrock's limit).
const MAX_DOCUMENTS: usize = 1000;
/// Documents per billed search.
const DOCUMENTS_PER_SEARCH: usize = 100;

pub async fn rerank(State(state): State<AppState>, ClientKey(key): ClientKey, ClientLabels(labels): ClientLabels, body: Bytes) -> Response {
    serve(&state, key, labels, body).await.unwrap_or_else(|e| e.into_response_for(Dialect::OpenAi))
}

/// Run one rerank request as an authenticated key; also used by the admin playground. It runs to
/// completion, and is accounted, even if the client stops waiting.
pub async fn serve(state: &AppState, key: Arc<VirtualKey>, labels: Labels, body: Bytes) -> Result<Response, GatewayError> {
    let state = state.clone();
    super::to_completion(async move { run(&state, key, &labels, &body).await }).await
}

/// A validated rerank request.
#[derive(Debug, PartialEq)]
struct RerankRequest {
    model: String,
    query: String,
    documents: Vec<String>,
    top_n: Option<usize>,
    return_documents: bool,
    max_tokens_per_doc: Option<u64>,
}

/// One ranked document: its index in the request and its relevance score.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Ranked {
    index: usize,
    score: f64,
}

fn parse(raw: &Value) -> Result<RerankRequest, GatewayError> {
    if !raw.is_object() {
        return Err(GatewayError::invalid("request body must be a JSON object"));
    }
    let model = raw.get("model").and_then(Value::as_str).ok_or_else(|| GatewayError::invalid("model is required"))?;
    let query = match raw.get("query") {
        Some(Value::String(q)) if !q.trim().is_empty() => q.clone(),
        _ => return Err(GatewayError::invalid("query must be a non-empty string")),
    };
    let not_text = || GatewayError::invalid("documents must be strings or objects with a text field");
    let documents: Vec<String> = match raw.get("documents") {
        Some(Value::Array(items)) if !items.is_empty() => items
            .iter()
            .map(|d| match d {
                Value::String(s) => Ok(s.clone()),
                Value::Object(o) => o.get("text").and_then(Value::as_str).map(str::to_string).ok_or_else(not_text),
                _ => Err(not_text()),
            })
            .collect::<Result<_, _>>()?,
        _ => return Err(GatewayError::invalid("documents must be a non-empty array")),
    };
    if documents.len() > MAX_DOCUMENTS {
        return Err(GatewayError::invalid(format!("at most {MAX_DOCUMENTS} documents per request")));
    }
    let positive = |field: &str| -> Result<Option<u64>, GatewayError> {
        match raw.get(field) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => match v.as_u64() {
                Some(n) if n > 0 => Ok(Some(n)),
                _ => Err(GatewayError::invalid(format!("{field} must be a positive integer"))),
            },
        }
    };
    let top_n = positive("top_n")?.map(|n| (n as usize).min(documents.len()));
    let max_tokens_per_doc = positive("max_tokens_per_doc")?;
    let return_documents = match raw.get("return_documents") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(GatewayError::invalid("return_documents must be a boolean")),
    };
    Ok(RerankRequest { model: model.to_string(), query, documents, top_n, return_documents, max_tokens_per_doc })
}

async fn run(state: &AppState, key: Arc<VirtualKey>, labels: &Labels, body: &[u8]) -> Result<Response, GatewayError> {
    let raw: Value = serde_json::from_slice(body).map_err(|e| GatewayError::invalid(format!("invalid JSON body: {e}")))?;
    let req = parse(&raw)?;
    if !key.allows_model(&req.model) {
        return Err(GatewayError::forbidden(format!("this key is not allowed to use model '{}'", req.model)));
    }
    let candidates = super::usable_deployments(state, &key, &req.model, Mode::Rerank)?;

    let mut tracker = Tracker::new(state, Some(&key), labels, &req.model, "rerank", false);
    if let Err(e) = crate::auth::check_key_limits(state, &key).await {
        tracker.fail(&e);
        return Err(e);
    }
    let req = &req;
    let (ranked, usage) =
        super::with_failover(state, &candidates, &req.model, &mut tracker, |d| async move { call(state, &d, req).await }).await?;
    tracker.set_usage(usage);
    let body = response(&tracker.id.to_string(), req, ranked, &usage);
    let mut resp = Json(body).into_response();
    resp.headers_mut().insert("x-request-id", HeaderValue::from_str(&tracker.id.to_string()).expect("uuid header"));
    Ok(resp)
}

async fn call(state: &AppState, d: &Deployment, req: &RerankRequest) -> Result<(Vec<Ranked>, Usage), GatewayError> {
    let url = providers::url(state, d, Op::Rerank).await?;
    let body = match d.provider {
        ProviderKind::Bedrock => bedrock_request(req, &model_arn(state, d).await?),
        _ => cohere_request(req, &d.upstream_model),
    };
    let resp = providers::post(state, d, &url, providers::headers(d)?, providers::json_bytes(&body)).await?;
    let v: Value = resp.json().await.map_err(|e| GatewayError::from_reqwest(&e))?;
    let ranked = match d.provider {
        ProviderKind::Bedrock => bedrock_results(&v),
        _ => cohere_results(&v),
    }?;
    let usage = match d.provider {
        ProviderKind::Bedrock => Usage { search_units: searches(req.documents.len()), ..Default::default() },
        _ => cohere_usage(&v, req.documents.len()),
    };
    Ok((ranked, usage))
}

/// Searches billed for ranking `documents` against one query.
fn searches(documents: usize) -> u64 {
    documents.div_ceil(DOCUMENTS_PER_SEARCH) as u64
}

/// Cohere's v2 rerank request.
fn cohere_request(req: &RerankRequest, model: &str) -> Value {
    let mut body = json!({ "model": model, "query": req.query, "documents": req.documents });
    if let Some(n) = req.top_n {
        body["top_n"] = n.into();
    }
    if let Some(n) = req.max_tokens_per_doc {
        body["max_tokens_per_doc"] = n.into();
    }
    body
}

fn cohere_results(v: &Value) -> Result<Vec<Ranked>, GatewayError> {
    ranked(v, "relevance_score")
}

/// What a Cohere-style upstream billed: Cohere reports searches, vLLM and others tokens. With
/// neither, the searches are counted as Cohere bills them.
fn cohere_usage(v: &Value, documents: usize) -> Usage {
    let units = |p: &str| v.pointer(p).and_then(Value::as_u64);
    let search_units = units("/meta/billed_units/search_units");
    let tokens = units("/meta/billed_units/input_tokens")
        .or_else(|| units("/usage/prompt_tokens"))
        .or_else(|| units("/usage/total_tokens"))
        .unwrap_or(0);
    let search_units = search_units.unwrap_or(if tokens > 0 { 0 } else { searches(documents) });
    Usage { input_tokens: tokens, search_units, ..Default::default() }
}

/// Bedrock names its models by ARN: a foundation model's in the deployment's region, unless the
/// deployment already names an ARN.
async fn model_arn(state: &AppState, d: &Deployment) -> Result<String, GatewayError> {
    if d.upstream_model.starts_with("arn:") {
        return Ok(d.upstream_model.clone());
    }
    let region = state.aws.region(d).await?;
    let partition = if region.starts_with("cn-") {
        "aws-cn"
    } else if region.starts_with("us-gov-") {
        "aws-us-gov"
    } else {
        "aws"
    };
    Ok(format!("arn:{partition}:bedrock:{region}::foundation-model/{}", d.upstream_model))
}

/// Bedrock's Rerank request (Agent Runtime). `max_tokens_per_doc` is Cohere's and not sent.
fn bedrock_request(req: &RerankRequest, model_arn: &str) -> Value {
    let sources: Vec<Value> = req
        .documents
        .iter()
        .map(|text| json!({ "type": "INLINE", "inlineDocumentSource": { "type": "TEXT", "textDocument": { "text": text } } }))
        .collect();
    json!({
        "queries": [{ "type": "TEXT", "textQuery": { "text": req.query } }],
        "sources": sources,
        "rerankingConfiguration": {
            "type": "BEDROCK_RERANKING_MODEL",
            "bedrockRerankingConfiguration": {
                "modelConfiguration": { "modelArn": model_arn },
                "numberOfResults": req.top_n.unwrap_or(req.documents.len()),
            },
        },
    })
}

fn bedrock_results(v: &Value) -> Result<Vec<Ranked>, GatewayError> {
    ranked(v, "relevanceScore")
}

/// `results[]` with an `index` and a score named `score_field`.
fn ranked(v: &Value, score_field: &str) -> Result<Vec<Ranked>, GatewayError> {
    let bad = || GatewayError::upstream_stream(ErrorKind::Upstream, "the rerank model returned no results");
    v["results"]
        .as_array()
        .ok_or_else(bad)?
        .iter()
        .map(|r| {
            let index = r["index"].as_u64().ok_or_else(bad)? as usize;
            let score = r[score_field].as_f64().ok_or_else(bad)?;
            Ok(Ranked { index, score })
        })
        .collect()
}

/// The response: the ranked documents, most relevant first, at most `top_n` of them.
fn response(id: &str, req: &RerankRequest, mut ranked: Vec<Ranked>, usage: &Usage) -> Value {
    ranked.retain(|r| r.index < req.documents.len());
    ranked.sort_by(|a, b| b.score.total_cmp(&a.score));
    ranked.truncate(req.top_n.unwrap_or(req.documents.len()));
    let results: Vec<Value> = ranked
        .iter()
        .map(|r| {
            let mut item = json!({ "index": r.index, "relevance_score": r.score });
            if req.return_documents {
                item["document"] = json!({ "text": req.documents[r.index] });
            }
            item
        })
        .collect();
    let mut billed = json!({ "search_units": usage.search_units });
    if usage.input_tokens > 0 {
        billed["input_tokens"] = usage.input_tokens.into();
    }
    json!({ "id": id, "model": req.model, "results": results, "meta": { "billed_units": billed } })
}

/// Health check for the admin API: ranks two short documents, outside of any key's accounting.
/// Returns the index and score of the best one.
pub async fn probe(state: &AppState, d: &Deployment) -> Result<(usize, f64, Usage), GatewayError> {
    let req = RerankRequest {
        model: d.model_name.clone(),
        query: "What is the capital of France?".into(),
        documents: vec!["Berlin is the capital of Germany.".into(), "Paris is the capital of France.".into()],
        top_n: None,
        return_documents: false,
        max_tokens_per_doc: None,
    };
    let (mut ranked, usage) = call(state, d, &req).await?;
    ranked.sort_by(|a, b| b.score.total_cmp(&a.score));
    let best = ranked.first().copied().ok_or_else(|| GatewayError::upstream_stream(ErrorKind::Upstream, "no results"))?;
    Ok((best.index, best.score, usage))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(top_n: Option<usize>, return_documents: bool) -> RerankRequest {
        RerankRequest {
            model: "rr".into(),
            query: "q".into(),
            documents: vec!["a".into(), "b".into(), "c".into()],
            top_n,
            return_documents,
            max_tokens_per_doc: None,
        }
    }

    #[test]
    fn requests_are_checked() {
        let r = parse(&json!({ "model": "rr", "query": "q", "documents": ["a", { "text": "b" }], "top_n": 5, "return_documents": true }))
            .unwrap();
        assert_eq!(r.documents, ["a", "b"]);
        assert_eq!((r.top_n, r.return_documents), (Some(2), true));
        for bad in [
            json!({ "query": "q", "documents": ["a"] }),
            json!({ "model": "rr", "query": " ", "documents": ["a"] }),
            json!({ "model": "rr", "query": "q", "documents": [] }),
            json!({ "model": "rr", "query": "q", "documents": [{ "title": "x" }] }),
            json!({ "model": "rr", "query": "q", "documents": [1] }),
            json!({ "model": "rr", "query": "q", "documents": ["a"], "top_n": 0 }),
            json!({ "model": "rr", "query": "q", "documents": ["a"], "return_documents": "yes" }),
            json!({ "model": "rr", "query": "q", "documents": vec!["a"; MAX_DOCUMENTS + 1] }),
        ] {
            assert!(parse(&bad).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn cohere_round_trip() {
        let r = RerankRequest { max_tokens_per_doc: Some(512), ..req(Some(2), false) };
        assert_eq!(
            cohere_request(&r, "Cohere-rerank-v3.5"),
            json!({ "model": "Cohere-rerank-v3.5", "query": "q", "documents": ["a", "b", "c"], "top_n": 2, "max_tokens_per_doc": 512 })
        );
        let v = json!({ "id": "x", "results": [{ "index": 2, "relevance_score": 0.9 }, { "index": 0, "relevance_score": 0.1 }],
                        "meta": { "billed_units": { "search_units": 1 } } });
        assert_eq!(cohere_results(&v).unwrap(), [Ranked { index: 2, score: 0.9 }, Ranked { index: 0, score: 0.1 }]);
        assert_eq!(cohere_usage(&v, 3).search_units, 1);
        // vLLM reports tokens; nothing at all is counted as searches.
        let vllm = json!({ "results": [], "usage": { "total_tokens": 40 } });
        assert_eq!((cohere_usage(&vllm, 3).input_tokens, cohere_usage(&vllm, 3).search_units), (40, 0));
        assert_eq!(cohere_usage(&json!({ "results": [] }), 250).search_units, 3);
        assert!(cohere_results(&json!({ "message": "nope" })).is_err());
    }

    #[test]
    fn bedrock_round_trip() {
        let arn = "arn:aws:bedrock:eu-central-1::foundation-model/amazon.rerank-v1:0";
        let b = bedrock_request(&req(None, false), arn);
        assert_eq!(b["queries"], json!([{ "type": "TEXT", "textQuery": { "text": "q" } }]));
        assert_eq!(b["sources"][1], json!({ "type": "INLINE", "inlineDocumentSource": { "type": "TEXT", "textDocument": { "text": "b" } } }));
        assert_eq!(b.pointer("/rerankingConfiguration/bedrockRerankingConfiguration/numberOfResults"), Some(&json!(3)));
        assert_eq!(b.pointer("/rerankingConfiguration/bedrockRerankingConfiguration/modelConfiguration/modelArn"), Some(&json!(arn)));
        let v = json!({ "results": [{ "index": 1, "relevanceScore": 0.7 }] });
        assert_eq!(bedrock_results(&v).unwrap(), [Ranked { index: 1, score: 0.7 }]);
    }

    #[test]
    fn responses_are_sorted_and_cut_to_top_n() {
        let ranked = vec![Ranked { index: 0, score: 0.2 }, Ranked { index: 2, score: 0.8 }, Ranked { index: 1, score: 0.5 }, Ranked { index: 9, score: 1.0 }];
        let usage = Usage { search_units: 1, ..Default::default() };
        let v = response("id", &req(Some(2), true), ranked, &usage);
        assert_eq!(
            v["results"],
            json!([{ "index": 2, "relevance_score": 0.8, "document": { "text": "c" } }, { "index": 1, "relevance_score": 0.5, "document": { "text": "b" } }])
        );
        assert_eq!(v["meta"], json!({ "billed_units": { "search_units": 1 } }));
        assert_eq!(searches(100), 1);
        assert_eq!(searches(101), 2);
    }
}
