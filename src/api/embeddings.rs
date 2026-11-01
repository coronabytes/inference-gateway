//! `POST /v1/embeddings`: OpenAI embeddings. OpenAI-compatible and Azure OpenAI deployments get
//! the request with the model name mapped, and the request and response pass through unchanged
//! otherwise, so `dimensions`, `encoding_format` and `user` work as upstream. Bedrock
//! deployments serve Amazon Titan Text Embeddings V2 through InvokeModel, and Vertex AI
//! deployments Google's embedding models through `predict`, translated both ways.

use std::sync::Arc;

use axum::{
    extract::State,
    http::{HeaderValue, header},
    response::{IntoResponse, Response},
};
use base64::Engine;
use bytes::Bytes;
use futures::{FutureExt, StreamExt, future::BoxFuture};
use serde_json::{Value, json};

use crate::{
    accounting::Tracker,
    auth::ClientKey,
    error::{Dialect, GatewayError},
    ir::Usage,
    labels::{ClientLabels, Labels},
    model::{Deployment, Mode, ProviderKind, VirtualKey},
    providers::{self, Op},
    state::AppState,
};

/// Embeddings
///
/// OpenAI embeddings, on every provider. Also served at `/embeddings`.
#[utoipa::path(post, path = "/v1/embeddings", tag = "Inference", request_body = crate::openapi::EmbeddingsRequest,
    responses(
        (status = 200, description = "The embeddings, in input order.", body = crate::openapi::ProviderAnswer),
        (status = 400, description = "Invalid request.", body = crate::openapi::OpenAiError),
        (status = 401, description = "Missing, invalid, disabled or expired key.", body = crate::openapi::OpenAiError),
        (status = 403, description = "The key may not use this model.", body = crate::openapi::OpenAiError),
        (status = 429, description = "A rate limit or the key's budget.", body = crate::openapi::OpenAiError),
    ),
    params(("x-gateway-labels" = Option<String>, Header, description = crate::openapi::LABELS_HEADER)),
    security(("virtual_key" = []), ("virtual_key_header" = [])))]
pub async fn embeddings(
    State(state): State<AppState>,
    ClientKey(key): ClientKey,
    ClientLabels(labels): ClientLabels,
    body: Bytes,
) -> Response {
    serve(&state, key, labels, body).await.unwrap_or_else(|e| e.into_response_for(Dialect::OpenAi))
}

/// Run one embeddings request as an authenticated key; also used by the admin playground. It
/// runs to completion, and is accounted, even if the client stops waiting.
pub async fn serve(state: &AppState, key: Arc<VirtualKey>, labels: Labels, body: Bytes) -> Result<Response, GatewayError> {
    let state = state.clone();
    super::to_completion(async move { run(&state, key, &labels, &body).await }).await
}

async fn run(state: &AppState, key: Arc<VirtualKey>, labels: &Labels, body: &Bytes) -> Result<Response, GatewayError> {
    let raw: Value = serde_json::from_slice(body).map_err(|e| GatewayError::invalid(format!("invalid JSON body: {e}")))?;
    if !raw.is_object() {
        return Err(GatewayError::invalid("request body must be a JSON object"));
    }
    let model = raw.get("model").and_then(Value::as_str).ok_or_else(|| GatewayError::invalid("model is required"))?.to_string();
    let input_ok = match raw.get("input") {
        Some(Value::String(_)) => true,
        Some(Value::Array(items)) => !items.is_empty(),
        _ => false,
    };
    if !input_ok {
        return Err(GatewayError::invalid("input must be a string or a non-empty array of strings or token arrays"));
    }
    if !key.allows_model(&model) {
        return Err(GatewayError::forbidden(format!("this key is not allowed to use model '{model}'")));
    }
    let candidates = super::usable_deployments(state, &key, &model, Mode::Embedding)?;

    let mut tracker = Tracker::new(state, Some(&key), labels, &model, "embeddings", false);
    tracker.estimate_input(body.len());
    if let Some(c) = tracker.log() {
        c.request(crate::capture::Body::Bytes(body.clone()));
    }
    if let Err(e) = crate::auth::check_key_limits(state, &key).await {
        tracker.fail(&e);
        return Err(e);
    }
    let raw = &raw;
    let (bytes, usage) =
        super::with_failover(state, &candidates, &model, &mut tracker, |d| async move { call(state, &d, raw).await }).await?;
    tracker.set_usage(usage);
    if let Some(c) = tracker.log() {
        c.response(crate::capture::Body::Bytes(bytes.clone()));
    }
    let request_id = HeaderValue::from_str(&tracker.id.to_string()).expect("uuid header");
    let mut resp = ([(header::CONTENT_TYPE, "application/json")], bytes).into_response();
    resp.headers_mut().insert("x-request-id", request_id);
    Ok(resp)
}

async fn call(state: &AppState, d: &Deployment, raw: &Value) -> Result<(Bytes, Usage), GatewayError> {
    match d.provider {
        ProviderKind::Bedrock => return titan(state, d, raw).await,
        ProviderKind::Vertex => return vertex(state, d, raw).await,
        _ => {}
    }
    let mut body = raw.clone();
    body["model"] = d.upstream_model.clone().into();
    let url = providers::url(state, d, Op::Embeddings).await?;
    let resp = providers::post(state, d, &url, providers::headers(d)?, providers::json_bytes(&body)).await?;
    let bytes = resp.bytes().await.map_err(|e| GatewayError::from_reqwest(&e))?;
    Ok((bytes.clone(), usage(&bytes)))
}

// ---------------------------------------------------------------- Bedrock: Titan Text Embeddings V2

/// Titan and Vertex embed one text per call; a batch runs this many calls at a time.
const PER_TEXT_CONCURRENCY: usize = 8;
/// OpenAI's limit on inputs per request.
const MAX_INPUTS: usize = 2048;

#[derive(Debug, PartialEq)]
struct TitanRequest {
    texts: Vec<String>,
    dimensions: u32,
    base64: bool,
}

/// An OpenAI request for upstreams that embed one text per call.
#[derive(Debug, PartialEq)]
struct TextRequest {
    texts: Vec<String>,
    dimensions: Option<u32>,
    base64: bool,
}

/// Text inputs only; `who` names the upstream in errors.
fn text_request(raw: &Value, who: &str) -> Result<TextRequest, GatewayError> {
    let not_text = || GatewayError::invalid(format!("{who} embeddings take text: input must be a string or a list of strings"));
    let texts = match raw.get("input") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(items)) => items.iter().map(|i| i.as_str().map(str::to_string).ok_or_else(not_text)).collect::<Result<_, _>>()?,
        _ => return Err(not_text()),
    };
    if texts.len() > MAX_INPUTS {
        return Err(GatewayError::invalid(format!("at most {MAX_INPUTS} inputs per request")));
    }
    if texts.iter().any(|t| t.trim().is_empty()) {
        return Err(GatewayError::invalid("inputs must not be empty"));
    }
    let dimensions = match raw.get("dimensions") {
        None | Some(Value::Null) => None,
        Some(v) => match v.as_u64().and_then(|n| u32::try_from(n).ok()) {
            Some(n) if n > 0 => Some(n),
            _ => return Err(GatewayError::invalid("dimensions must be a positive integer")),
        },
    };
    let base64 = match raw.get("encoding_format").and_then(Value::as_str) {
        None | Some("float") => false,
        Some("base64") => true,
        Some(_) => return Err(GatewayError::invalid("encoding_format must be float or base64")),
    };
    Ok(TextRequest { texts, dimensions, base64 })
}

/// The OpenAI request as Titan V2 calls: text only, 256, 512 or 1024 dimensions.
fn titan_request(raw: &Value) -> Result<TitanRequest, GatewayError> {
    let TextRequest { texts, dimensions, base64 } = text_request(raw, "Titan")?;
    let dimensions = match dimensions {
        None => 1024,
        Some(d @ (256 | 512 | 1024)) => d,
        Some(_) => return Err(GatewayError::invalid("Titan Text Embeddings V2 supports dimensions 256, 512 or 1024")),
    };
    Ok(TitanRequest { texts, dimensions, base64 })
}

/// One embedding and its input's token count.
type EmbedResult = Result<(Vec<f32>, u64), GatewayError>;

/// Embed each text with its own upstream call, a few at a time, as one OpenAI response.
async fn per_text<'a>(
    d: &Deployment,
    texts: &'a [String],
    base64: bool,
    one: impl Fn(&'a str) -> BoxFuture<'a, EmbedResult>,
) -> Result<(Bytes, Usage), GatewayError> {
    // Boxed so the batch future stays `Send` for axum.
    let calls: Vec<BoxFuture<'a, EmbedResult>> = texts.iter().map(|text| one(text)).collect();
    let results: Vec<_> = futures::stream::iter(calls).buffered(PER_TEXT_CONCURRENCY).collect().await;
    let results = results.into_iter().collect::<Result<Vec<_>, _>>()?;
    let tokens: u64 = results.iter().map(|(_, n)| n).sum();
    let body = openai_response(&d.upstream_model, &results, base64);
    Ok((Bytes::from(serde_json::to_vec(&body).expect("json")), Usage { input_tokens: tokens, ..Default::default() }))
}

async fn titan(state: &AppState, d: &Deployment, raw: &Value) -> Result<(Bytes, Usage), GatewayError> {
    let req = titan_request(raw)?;
    per_text(d, &req.texts, req.base64, |text| titan_one(state, d, text, req.dimensions).boxed()).await
}

/// Vertex AI `predict` for Google's text embedding models. Gemini embedding models take one
/// text per call. Any `dimensions` goes up as `outputDimensionality`.
async fn vertex(state: &AppState, d: &Deployment, raw: &Value) -> Result<(Bytes, Usage), GatewayError> {
    let req = text_request(raw, "Vertex AI")?;
    let url = providers::url(state, d, Op::Embeddings).await?;
    per_text(d, &req.texts, req.base64, |text| vertex_one(state, d, &url, text, req.dimensions).boxed()).await
}

async fn vertex_one(state: &AppState, d: &Deployment, url: &str, text: &str, dimensions: Option<u32>) -> EmbedResult {
    let mut body = json!({ "instances": [{ "content": text }] });
    if let Some(n) = dimensions {
        body["parameters"] = json!({ "outputDimensionality": n });
    }
    let resp = providers::post(state, d, url, providers::headers(d)?, providers::json_bytes(&body)).await?;
    let v: Value = resp.json().await.map_err(|e| GatewayError::from_reqwest(&e))?;
    vertex_prediction(&v)
}

fn vertex_prediction(v: &Value) -> EmbedResult {
    let e = v.pointer("/predictions/0/embeddings").unwrap_or(&Value::Null);
    let values: Vec<f32> = e["values"]
        .as_array()
        .ok_or_else(|| GatewayError::upstream_stream(crate::error::ErrorKind::Upstream, "Vertex AI returned no embedding"))?
        .iter()
        .map(|x| x.as_f64().unwrap_or_default() as f32)
        .collect();
    let tokens = e.pointer("/statistics/token_count").and_then(Value::as_f64).unwrap_or(0.0) as u64;
    Ok((values, tokens))
}

/// One Titan call, through the SDK's InvokeModel.
async fn titan_one(state: &AppState, d: &Deployment, text: &str, dimensions: u32) -> EmbedResult {
    // Normalized like OpenAI's embeddings, so cosine similarity is a dot product.
    let body = json!({ "inputText": text, "dimensions": dimensions, "normalize": true });
    let v = providers::bedrock::invoke_json(state, d, &body).await?;
    let embedding: Vec<f32> = v["embedding"]
        .as_array()
        .ok_or_else(|| GatewayError::upstream_stream(crate::error::ErrorKind::Upstream, "Titan returned no embedding"))?
        .iter()
        .map(|x| x.as_f64().unwrap_or_default() as f32)
        .collect();
    Ok((embedding, v["inputTextTokenCount"].as_u64().unwrap_or(0)))
}

/// Per-text results as an OpenAI embeddings response. `base64` packs each vector as
/// little-endian f32, as OpenAI does.
fn openai_response(model: &str, results: &[(Vec<f32>, u64)], base64: bool) -> Value {
    let data: Vec<Value> = results
        .iter()
        .enumerate()
        .map(|(index, (v, _))| {
            let embedding = if base64 {
                let bytes: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
                Value::String(base64::engine::general_purpose::STANDARD.encode(bytes))
            } else {
                json!(v)
            };
            json!({ "object": "embedding", "index": index, "embedding": embedding })
        })
        .collect();
    let tokens: u64 = results.iter().map(|(_, n)| n).sum();
    json!({ "object": "list", "data": data, "model": model, "usage": { "prompt_tokens": tokens, "total_tokens": tokens } })
}

/// Embeddings bill input tokens only.
fn usage(bytes: &[u8]) -> Usage {
    let v: Value = serde_json::from_slice(bytes).unwrap_or_default();
    let u = &v["usage"];
    let tokens = u.get("prompt_tokens").or_else(|| u.get("total_tokens")).and_then(Value::as_u64).unwrap_or(0);
    Usage { input_tokens: tokens, ..Default::default() }
}

/// Health check for the admin API: embeds one short string, outside of any key's accounting.
pub async fn probe(state: &AppState, d: &Deployment) -> Result<(usize, Usage), GatewayError> {
    let body = serde_json::json!({ "model": d.upstream_model, "input": "Health check from the inference gateway." });
    let (bytes, usage) = call(state, d, &body).await?;
    let v: Value = serde_json::from_slice(&bytes).unwrap_or_default();
    let dims = v.pointer("/data/0/embedding").and_then(Value::as_array).map_or(0, Vec::len);
    Ok((dims, usage))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titan_requests() {
        let r = titan_request(&json!({ "input": ["a", "b"], "dimensions": 512, "encoding_format": "base64" })).unwrap();
        assert_eq!(r, TitanRequest { texts: vec!["a".into(), "b".into()], dimensions: 512, base64: true });
        assert_eq!(titan_request(&json!({ "input": "x" })).unwrap().dimensions, 1024);
        for bad in [
            json!({ "input": [1, 2, 3] }),
            json!({ "input": [[1, 2]] }),
            json!({ "input": "x", "dimensions": 768 }),
            json!({ "input": "x", "encoding_format": "int8" }),
            json!({ "input": ["ok", " "] }),
        ] {
            assert!(titan_request(&bad).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn vertex_requests_and_predictions() {
        let r = text_request(&json!({ "input": "x", "dimensions": 768 }), "Vertex AI").unwrap();
        assert_eq!(r, TextRequest { texts: vec!["x".into()], dimensions: Some(768), base64: false });
        assert!(text_request(&json!({ "input": "x", "dimensions": 0 }), "Vertex AI").is_err());
        let v = json!({ "predictions": [{ "embeddings": { "values": [0.5, -0.25], "statistics": { "token_count": 3.0, "truncated": false } } }] });
        assert_eq!(vertex_prediction(&v).unwrap(), (vec![0.5, -0.25], 3));
        assert!(vertex_prediction(&json!({ "predictions": [] })).is_err());
    }

    #[test]
    fn titan_responses() {
        let results = vec![(vec![0.5f32, -1.0], 3), (vec![0.25, 0.0], 4)];
        let v = openai_response("amazon.titan-embed-text-v2:0", &results, false);
        assert_eq!(v["data"][1], json!({ "object": "embedding", "index": 1, "embedding": [0.25, 0.0] }));
        assert_eq!(v["usage"], json!({ "prompt_tokens": 7, "total_tokens": 7 }));
        let b = openai_response("m", &results, true);
        let bytes = base64::engine::general_purpose::STANDARD.decode(b["data"][0]["embedding"].as_str().unwrap()).unwrap();
        let floats: Vec<f32> = bytes.chunks(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect();
        assert_eq!(floats, [0.5, -1.0]);
        assert_eq!(usage(&serde_json::to_vec(&v).unwrap()).input_tokens, 7);
    }

    #[test]
    fn usage_counts_prompt_tokens() {
        assert_eq!(usage(br#"{"usage":{"prompt_tokens":7,"total_tokens":7}}"#).input_tokens, 7);
        assert_eq!(usage(br#"{"usage":{"total_tokens":5}}"#).input_tokens, 5);
        assert_eq!(usage(b"not json").input_tokens, 0);
    }
}
