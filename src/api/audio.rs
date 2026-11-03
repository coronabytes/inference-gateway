//! `POST /v1/audio/transcriptions` and `POST /v1/audio/speech`: OpenAI's audio API, speech to text
//! and text to speech. OpenAI-compatible and Azure OpenAI deployments get the request with the
//! model name mapped; Mistral's Voxtral models are translated both ways.
//!
//! Transcriptions bill the audio's minutes or the tokens the upstream reports. A client asking
//! for `text`, `srt` or `vtt` gets them made from the JSON the gateway asks the upstream for, so
//! the usage is always there to bill. Speech bills the characters of its input. Its audio is
//! returned in one piece: `stream_format: "sse"` is not supported yet.

use std::sync::Arc;

use axum::{
    extract::State,
    http::{HeaderMap, HeaderValue, header},
    response::{IntoResponse, Response},
};
use base64::Engine;
use bytes::Bytes;
use serde_json::{Value, json};

use crate::{
    accounting::Tracker,
    auth::ClientKey,
    error::{Dialect, ErrorKind, GatewayError},
    ir::Usage,
    labels::{ClientLabels, Labels},
    model::{Deployment, Mode, ProviderKind, VirtualKey},
    multipart::{self, Part},
    providers::{self, Op},
    state::AppState,
};

/// OpenAI's limit on the text of one speech request.
const MAX_SPEECH_CHARS: usize = 4096;
/// OpenAI's built-in voices. Mistral has voices of its own, so it speaks these in `MISTRAL_VOICE`.
const OPENAI_VOICES: [&str; 13] =
    ["alloy", "ash", "ballad", "coral", "echo", "fable", "onyx", "nova", "sage", "shimmer", "verse", "marin", "cedar"];
/// The voice OpenAI-compatible speech gets when the request names none, as OpenAI requires one.
const DEFAULT_VOICE: &str = "alloy";
/// Mistral's voice for requests that name none of its own, as Mistral requires one: the preset of
/// its documentation's examples (English, UK), which Voxtral 2 and 3 both take.
const MISTRAL_VOICE: &str = "gb_jane_neutral";

/// Transcribe audio
///
/// Speech to text, on OpenAI-compatible, Azure OpenAI and Mistral deployments. Also served at
/// `/audio/transcriptions`.
#[utoipa::path(post, path = "/v1/audio/transcriptions", tag = "Inference",
    request_body(content = crate::openapi::TranscriptionRequest, content_type = "multipart/form-data"),
    responses(
        (status = 200, description = "The transcript: JSON, or text for `text`, `srt` and `vtt`.", body = crate::openapi::ProviderAnswer),
        (status = 400, description = "Invalid request.", body = crate::error::OpenAiError),
        (status = 401, description = "Missing, invalid, disabled or expired key.", body = crate::error::OpenAiError),
        (status = 403, description = "The key may not use this model.", body = crate::error::OpenAiError),
        (status = 429, description = "A rate limit or the key's budget.", body = crate::error::OpenAiError),
    ),
    params(("x-gateway-labels" = Option<String>, Header, description = crate::openapi::LABELS_HEADER)),
    security(("virtual_key" = []), ("virtual_key_header" = [])))]
pub async fn transcriptions(
    State(state): State<AppState>,
    ClientKey(key): ClientKey,
    ClientLabels(labels): ClientLabels,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    serve_transcription(&state, key, labels, &headers, body).await.unwrap_or_else(|e| e.into_response_for(Dialect::OpenAi))
}

/// Run one transcription as an authenticated key; also used by the admin playground. It runs to
/// completion, and is accounted, even if the client stops waiting.
pub async fn serve_transcription(
    state: &AppState,
    key: Arc<VirtualKey>,
    labels: Labels,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<Response, GatewayError> {
    let content_type = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
    let state = state.clone();
    super::to_completion(async move { transcribe(&state, key, &labels, &content_type, &body).await }).await
}

/// Speak text
///
/// Text to speech, on OpenAI-compatible, Azure OpenAI and Mistral deployments. Returns the audio.
/// Also served at `/audio/speech`.
#[utoipa::path(post, path = "/v1/audio/speech", tag = "Inference", request_body = crate::openapi::SpeechRequest,
    responses(
        (status = 200, description = "The audio, in the requested format.", content_type = "application/octet-stream", body = Vec<u8>),
        (status = 400, description = "Invalid request.", body = crate::error::OpenAiError),
        (status = 401, description = "Missing, invalid, disabled or expired key.", body = crate::error::OpenAiError),
        (status = 403, description = "The key may not use this model.", body = crate::error::OpenAiError),
        (status = 429, description = "A rate limit or the key's budget.", body = crate::error::OpenAiError),
    ),
    params(("x-gateway-labels" = Option<String>, Header, description = crate::openapi::LABELS_HEADER)),
    security(("virtual_key" = []), ("virtual_key_header" = [])))]
pub async fn speech(State(state): State<AppState>, ClientKey(key): ClientKey, ClientLabels(labels): ClientLabels, body: Bytes) -> Response {
    serve_speech(&state, key, labels, body).await.unwrap_or_else(|e| e.into_response_for(Dialect::OpenAi))
}

/// Run one speech request as an authenticated key; also used by the admin playground.
pub async fn serve_speech(state: &AppState, key: Arc<VirtualKey>, labels: Labels, body: Bytes) -> Result<Response, GatewayError> {
    let state = state.clone();
    super::to_completion(async move { speak(&state, key, &labels, &body).await }).await
}

// ---------------------------------------------------------------- transcriptions

/// A validated transcription request.
#[derive(Debug)]
struct Transcription {
    model: String,
    file: Part,
    /// The other fields, passed on to OpenAI-compatible upstreams as sent.
    fields: Vec<Part>,
    /// What the client asked for: `json`, `text`, `verbose_json`, `srt`, `vtt` or `diarized_json`.
    format: String,
}

impl Transcription {
    fn field(&self, name: &str) -> Option<&str> {
        self.fields.iter().find(|p| p.name == name).and_then(Part::as_str).map(str::trim).filter(|v| !v.is_empty())
    }
}

fn parse_transcription(content_type: &str, body: &Bytes) -> Result<Transcription, GatewayError> {
    let parts = multipart::parse(content_type, body).map_err(GatewayError::invalid)?;
    let (mut model, mut file, mut fields) = (None, None, Vec::new());
    for p in parts {
        match p.name.as_str() {
            "model" => model = p.as_str().map(|m| m.trim().to_string()),
            "file" => file = Some(p),
            _ => fields.push(p),
        }
    }
    let model = model.filter(|m| !m.is_empty()).ok_or_else(|| GatewayError::invalid("model is required"))?;
    let file = file.filter(|f| !f.data.is_empty()).ok_or_else(|| GatewayError::invalid("file is required: the audio to transcribe"))?;
    let mut req = Transcription { model, file, fields, format: String::new() };
    req.format = req.field("response_format").unwrap_or("json").to_string();
    if !matches!(req.format.as_str(), "json" | "text" | "verbose_json" | "srt" | "vtt" | "diarized_json") {
        return Err(GatewayError::invalid("response_format must be json, text, verbose_json, srt, vtt or diarized_json"));
    }
    if req.field("stream") == Some("true") {
        return Err(GatewayError::invalid("streamed transcriptions are not supported; leave stream unset"));
    }
    Ok(req)
}

async fn transcribe(
    state: &AppState,
    key: Arc<VirtualKey>,
    labels: &Labels,
    content_type: &str,
    body: &Bytes,
) -> Result<Response, GatewayError> {
    let req = parse_transcription(content_type, body)?;
    if !key.allows_model(&req.model) {
        return Err(GatewayError::forbidden(format!("this key is not allowed to use model '{}'", req.model)));
    }
    let mut candidates = super::usable_deployments(state, &key, &req.model, Mode::Transcription)?;
    candidates.retain(|d| !d.config.realtime);
    if candidates.is_empty() {
        return Err(GatewayError::invalid(format!(
            "'{}' is a realtime transcription model: stream audio to it at /v1/realtime?intent=transcription",
            req.model
        )));
    }

    let mut tracker = Tracker::new(state, Some(&key), labels, &req.model, "transcriptions", false);
    if let Some(c) = tracker.log() {
        c.request(crate::capture::Body::Json(logged_request(&req)));
    }
    if let Err(e) = crate::auth::check_key_limits(state, &key).await {
        tracker.fail(&e);
        return Err(e);
    }
    let req = &req;
    let (transcript, usage) =
        super::with_failover(state, &candidates, &req.model, &mut tracker, |d| async move { call_transcription(state, &d, req).await })
            .await?;
    tracker.set_usage(usage);
    if let Some(c) = tracker.log() {
        c.response(crate::capture::Body::Json(transcript.clone()));
    }
    let (kind, bytes) = render(&req.format, &transcript);
    let mut resp = ([(header::CONTENT_TYPE, kind)], bytes).into_response();
    resp.headers_mut().insert("x-request-id", HeaderValue::from_str(&tracker.id.to_string()).expect("uuid header"));
    Ok(resp)
}

/// The request as the log keeps it: the fields, and the file's name, type and size.
fn logged_request(req: &Transcription) -> Value {
    let mut v = json!({
        "model": req.model,
        "file": { "filename": req.file.filename, "content_type": req.file.content_type, "bytes": req.file.data.len() },
    });
    for p in &req.fields {
        v[&p.name] = p.as_str().map_or_else(|| json!({ "bytes": p.data.len() }), |s| json!(s));
    }
    v
}

/// The transcript as OpenAI's JSON (`json`, `verbose_json` or `diarized_json`), and its usage.
async fn call_transcription(state: &AppState, d: &Deployment, req: &Transcription) -> Result<(Value, Usage), GatewayError> {
    let mistral = d.provider == ProviderKind::Mistral;
    let parts = if mistral { mistral_transcription(d, req)? } else { openai_transcription(d, req) };
    let (content_type, body) = multipart::build(&parts);
    let mut headers = providers::headers(d)?;
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_str(&content_type).expect("multipart content type"));
    let url = providers::url(state, d, Op::Transcriptions).await?;
    let resp = providers::post(state, d, &url, headers, body).await?;
    let v: Value = resp.json().await.map_err(|e| GatewayError::from_reqwest(&e))?;
    if !v.is_object() {
        return Err(GatewayError::upstream_stream(ErrorKind::Upstream, "the transcription model returned no transcript"));
    }
    Ok(if mistral { from_mistral(&req.format, &v) } else { (v.clone(), transcription_usage(&v)) })
}

/// The format to ask an OpenAI-compatible upstream for: JSON, which reports the usage, and
/// `verbose_json` for subtitles, which have its segments' times.
fn upstream_format(format: &str) -> &str {
    match format {
        "text" => "json",
        "srt" | "vtt" => "verbose_json",
        f => f,
    }
}

fn openai_transcription(d: &Deployment, req: &Transcription) -> Vec<Part> {
    let mut parts = vec![Part::text("model", d.upstream_model.clone()), req.file.clone()];
    parts.extend(req.fields.iter().filter(|p| p.name != "response_format").cloned());
    parts.push(Part::text("response_format", upstream_format(&req.format)));
    parts
}

/// Mistral's request: the audio, its language and temperature, and segment timestamps when the
/// client wants them. Mistral takes no prompt.
fn mistral_transcription(d: &Deployment, req: &Transcription) -> Result<Vec<Part>, GatewayError> {
    if req.format == "diarized_json" {
        return Err(GatewayError::invalid("Mistral transcriptions support json, text, verbose_json, srt and vtt"));
    }
    let mut parts = vec![Part::text("model", d.upstream_model.clone()), req.file.clone()];
    for name in ["language", "temperature"] {
        if let Some(v) = req.field(name) {
            parts.push(Part::text(name, v));
        }
    }
    if matches!(req.format.as_str(), "verbose_json" | "srt" | "vtt") {
        parts.push(Part::text("timestamp_granularities", "segment"));
    }
    Ok(parts)
}

/// Mistral's transcript as OpenAI's: `verbose_json` when the client wants the segments (also for
/// subtitles), `json` otherwise. Mistral bills the audio's seconds, so only those are counted.
fn from_mistral(format: &str, v: &Value) -> (Value, Usage) {
    let seconds = v.pointer("/usage/prompt_audio_seconds").and_then(Value::as_f64).map_or(0, |s| s.ceil() as u64);
    let usage = json!({ "type": "duration", "seconds": seconds });
    let text = v["text"].as_str().unwrap_or_default();
    let out = if matches!(format, "verbose_json" | "srt" | "vtt") {
        let segments: Vec<Value> = v["segments"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(id, s)| json!({ "id": id, "start": s["start"], "end": s["end"], "text": s["text"] }))
            .collect();
        json!({ "task": "transcribe", "language": v["language"], "duration": seconds, "text": text, "segments": segments, "usage": usage })
    } else {
        json!({ "text": text, "usage": usage })
    };
    (out, Usage { audio_seconds: seconds, ..Default::default() })
}

/// What an OpenAI transcript reports: tokens (gpt-4o-transcribe) or seconds (Whisper), or else
/// the audio's `duration` (`verbose_json`).
fn transcription_usage(v: &Value) -> Usage {
    let u = &v["usage"];
    let n = |k: &str| u[k].as_u64().unwrap_or(0);
    let seconds = |s: Option<f64>| s.map_or(0, |s| s.max(0.0).ceil() as u64);
    match u["type"].as_str() {
        Some("tokens") => Usage { input_tokens: n("input_tokens"), output_tokens: n("output_tokens"), ..Default::default() },
        Some("duration") => Usage { audio_seconds: seconds(u["seconds"].as_f64()), ..Default::default() },
        _ => Usage { audio_seconds: seconds(v["duration"].as_f64()), ..Default::default() },
    }
}

/// The transcript in the format the client asked for, with its content type.
fn render(format: &str, v: &Value) -> (&'static str, Bytes) {
    match format {
        "text" => ("text/plain; charset=utf-8", Bytes::from(v["text"].as_str().unwrap_or_default().to_string())),
        "srt" => ("text/plain; charset=utf-8", Bytes::from(subtitles(v, false))),
        "vtt" => ("text/vtt; charset=utf-8", Bytes::from(subtitles(v, true))),
        _ => ("application/json", Bytes::from(serde_json::to_vec(v).expect("json"))),
    }
}

/// SubRip or WebVTT cues from a `verbose_json` transcript's segments; one cue for all of it when
/// it has none.
fn subtitles(v: &Value, vtt: bool) -> String {
    let mut cues: Vec<(f64, f64, &str)> = v["segments"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|s| (s["start"].as_f64().unwrap_or(0.0), s["end"].as_f64().unwrap_or(0.0), s["text"].as_str().unwrap_or_default().trim()))
        .filter(|c| !c.2.is_empty())
        .collect();
    let text = v["text"].as_str().unwrap_or_default().trim();
    if cues.is_empty() && !text.is_empty() {
        cues.push((0.0, v["duration"].as_f64().unwrap_or(0.0), text));
    }
    let mut out = if vtt { String::from("WEBVTT\n\n") } else { String::new() };
    for (i, (start, end, text)) in cues.into_iter().enumerate() {
        if !vtt {
            out.push_str(&format!("{}\n", i + 1));
        }
        out.push_str(&format!("{} --> {}\n{text}\n\n", timestamp(start, vtt), timestamp(end, vtt)));
    }
    out
}

fn timestamp(seconds: f64, vtt: bool) -> String {
    let ms = (seconds.max(0.0) * 1000.0).round() as u64;
    let (h, m, s, ms) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    format!("{h:02}:{m:02}:{s:02}{}{ms:03}", if vtt { '.' } else { ',' })
}

// ---------------------------------------------------------------- speech

/// A validated speech request.
#[derive(Debug)]
struct Speech {
    model: String,
    input: String,
    voice: Option<String>,
    /// `mp3` (default), `opus`, `aac`, `flac`, `wav` or `pcm`.
    format: String,
    /// The request as sent, for OpenAI-compatible upstreams.
    raw: Value,
}

fn parse_speech(raw: Value) -> Result<Speech, GatewayError> {
    if !raw.is_object() {
        return Err(GatewayError::invalid("request body must be a JSON object"));
    }
    let model = raw.get("model").and_then(Value::as_str).ok_or_else(|| GatewayError::invalid("model is required"))?.to_string();
    let input = match raw.get("input") {
        Some(Value::String(s)) if !s.trim().is_empty() => s.clone(),
        _ => return Err(GatewayError::invalid("input must be a non-empty string: the text to speak")),
    };
    if input.chars().count() > MAX_SPEECH_CHARS {
        return Err(GatewayError::invalid(format!("input must be at most {MAX_SPEECH_CHARS} characters")));
    }
    let voice = match raw.get("voice") {
        None | Some(Value::Null) => None,
        Some(Value::String(v)) => Some(v.clone()),
        Some(v) => Some(
            v.get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| GatewayError::invalid("voice must be a name or an object with an id"))?
                .to_string(),
        ),
    };
    let format = match raw.get("response_format") {
        None | Some(Value::Null) => "mp3".to_string(),
        Some(Value::String(f)) if matches!(f.as_str(), "mp3" | "opus" | "aac" | "flac" | "wav" | "pcm") => f.clone(),
        Some(_) => return Err(GatewayError::invalid("response_format must be mp3, opus, aac, flac, wav or pcm")),
    };
    if raw.get("stream_format").and_then(Value::as_str) == Some("sse") {
        return Err(GatewayError::invalid("stream_format sse is not supported; the audio comes in one piece"));
    }
    Ok(Speech { model, input, voice, format, raw })
}

async fn speak(state: &AppState, key: Arc<VirtualKey>, labels: &Labels, body: &Bytes) -> Result<Response, GatewayError> {
    let raw: Value = serde_json::from_slice(body).map_err(|e| GatewayError::invalid(format!("invalid JSON body: {e}")))?;
    let req = parse_speech(raw)?;
    if !key.allows_model(&req.model) {
        return Err(GatewayError::forbidden(format!("this key is not allowed to use model '{}'", req.model)));
    }
    let mut candidates = super::usable_deployments(state, &key, &req.model, Mode::Speech)?;
    if let Some(voice) = &req.voice {
        let listed = voices(&candidates).join(", ");
        candidates.retain(|d| d.config.speaks(voice));
        if candidates.is_empty() {
            return Err(GatewayError::invalid(format!("'{}' has no voice '{voice}'; its voices: {listed}", req.model)));
        }
    }

    let mut tracker = Tracker::new(state, Some(&key), labels, &req.model, "speech", false);
    if let Some(c) = tracker.log() {
        c.request(crate::capture::Body::Bytes(body.clone()));
    }
    if let Err(e) = crate::auth::check_key_limits(state, &key).await {
        tracker.fail(&e);
        return Err(e);
    }
    let req = &req;
    let (audio, kind) =
        super::with_failover(state, &candidates, &req.model, &mut tracker, |d| async move { call_speech(state, &d, req).await }).await?;
    tracker.set_usage(Usage { characters: req.input.chars().count() as u64, ..Default::default() });
    if let Some(c) = tracker.log() {
        c.response(crate::capture::Body::Json(json!({ "content_type": kind, "bytes": audio.len() })));
    }
    let kind = HeaderValue::from_str(&kind).unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
    let mut resp = ([(header::CONTENT_TYPE, kind)], audio).into_response();
    resp.headers_mut().insert("x-request-id", HeaderValue::from_str(&tracker.id.to_string()).expect("uuid header"));
    Ok(resp)
}

/// The voices deployments list, each once, in their order.
fn voices(deployments: &[Arc<Deployment>]) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    for v in deployments.iter().flat_map(|d| &d.config.voices) {
        if v.id != crate::model::ANY_VOICE && !out.contains(&v.id.as_str()) {
            out.push(&v.id);
        }
    }
    out
}

/// The voice to ask `d` for: the request's, else the deployment's first.
fn voice_for<'a>(d: &'a Deployment, req: &'a Speech) -> Option<&'a str> {
    req.voice.as_deref().or_else(|| d.config.default_voice())
}

/// The audio and its content type.
async fn call_speech(state: &AppState, d: &Deployment, req: &Speech) -> Result<(Bytes, String), GatewayError> {
    let mistral = d.provider == ProviderKind::Mistral;
    let body = if mistral { mistral_speech(d, req)? } else { openai_speech(d, req) };
    let url = providers::url(state, d, Op::Speech).await?;
    let resp = providers::post(state, d, &url, providers::headers(d)?, providers::json_bytes(&body)).await?;
    if mistral {
        let v: Value = resp.json().await.map_err(|e| GatewayError::from_reqwest(&e))?;
        return Ok((mistral_audio(&v)?, audio_type(&req.format).to_string()));
    }
    let kind = resp.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).map(str::to_string);
    let audio = resp.bytes().await.map_err(|e| GatewayError::from_reqwest(&e))?;
    if audio.is_empty() {
        return Err(GatewayError::upstream_stream(ErrorKind::Upstream, "the speech model returned no audio"));
    }
    Ok((audio, kind.unwrap_or_else(|| audio_type(&req.format).to_string())))
}

fn openai_speech(d: &Deployment, req: &Speech) -> Value {
    let mut body = req.raw.clone();
    body["model"] = d.upstream_model.clone().into();
    body["voice"] = voice_for(d, req).unwrap_or(DEFAULT_VOICE).into();
    body
}

/// Mistral's request. Without a voice of Mistral's (none, or one of OpenAI's names) it speaks in
/// `MISTRAL_VOICE`; `instructions` and `speed` have no counterpart and are not sent.
fn mistral_speech(d: &Deployment, req: &Speech) -> Result<Value, GatewayError> {
    if req.format == "aac" {
        return Err(GatewayError::invalid("Mistral speaks mp3, opus, flac, wav or pcm"));
    }
    Ok(
        json!({ "model": d.upstream_model, "input": req.input, "voice_id": mistral_voice(voice_for(d, req)), "response_format": req.format }),
    )
}

fn mistral_voice(voice: Option<&str>) -> &str {
    voice.filter(|v| !OPENAI_VOICES.contains(v)).unwrap_or(MISTRAL_VOICE)
}

/// Mistral returns the audio base64-encoded in JSON.
fn mistral_audio(v: &Value) -> Result<Bytes, GatewayError> {
    let data = v["audio_data"].as_str().filter(|s| !s.is_empty());
    let data = data.ok_or_else(|| GatewayError::upstream_stream(ErrorKind::Upstream, "the speech model returned no audio"))?;
    let audio = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|_| GatewayError::upstream_stream(ErrorKind::Upstream, "the speech model's audio is not base64"))?;
    Ok(Bytes::from(audio))
}

/// The content type of a speech `response_format`, as OpenAI sends it.
fn audio_type(format: &str) -> &'static str {
    match format {
        "opus" => "audio/ogg",
        "aac" => "audio/aac",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "pcm" => "audio/pcm",
        _ => "audio/mpeg",
    }
}

// ---------------------------------------------------------------- health checks

/// Health check for the admin API: transcribes a second of silence, outside of any key's
/// accounting. Returns the transcript (usually empty).
pub async fn probe_transcription(state: &AppState, d: &Deployment) -> Result<(String, Usage), GatewayError> {
    let file =
        Part { name: "file".into(), filename: Some("silence.wav".into()), content_type: Some("audio/wav".into()), data: silent_wav(1) };
    let req = Transcription { model: d.model_name.clone(), file, fields: Vec::new(), format: "json".into() };
    let (v, usage) = call_transcription(state, d, &req).await?;
    Ok((v["text"].as_str().unwrap_or_default().trim().to_string(), usage))
}

/// Health check for the admin API: speaks a short sentence, outside of any key's accounting.
/// Returns the audio's size and type.
pub async fn probe_speech(state: &AppState, d: &Deployment) -> Result<(usize, String, Usage), GatewayError> {
    let raw = json!({ "model": d.model_name, "input": "Health check from the inference gateway." });
    let req = parse_speech(raw)?;
    let (audio, kind) = call_speech(state, d, &req).await?;
    Ok((audio.len(), kind, Usage { characters: req.input.chars().count() as u64, ..Default::default() }))
}

/// A WAV file of `seconds` of silence: 16 kHz, mono, 16 bit.
fn silent_wav(seconds: u32) -> Bytes {
    const RATE: u32 = 16_000;
    let data = RATE * 2 * seconds;
    let mut w = Vec::with_capacity(44 + data as usize);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes()); // PCM
    w.extend_from_slice(&1u16.to_le_bytes()); // mono
    w.extend_from_slice(&RATE.to_le_bytes());
    w.extend_from_slice(&(RATE * 2).to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data.to_le_bytes());
    w.resize(44 + data as usize, 0);
    Bytes::from(w)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(fields: &[(&str, &str)]) -> (String, Bytes) {
        let mut parts: Vec<Part> = fields.iter().map(|(k, v)| Part::text(k, *v)).collect();
        parts.push(Part { name: "file".into(), filename: Some("a.mp3".into()), content_type: None, data: Bytes::from_static(b"ID3") });
        let (ct, body) = multipart::build(&parts);
        (ct, Bytes::from(body))
    }

    #[test]
    fn transcription_requests_are_checked() {
        let (ct, body) = form(&[("model", "stt"), ("response_format", "srt"), ("language", "de")]);
        let r = parse_transcription(&ct, &body).unwrap();
        assert_eq!((r.model.as_str(), r.format.as_str(), r.field("language")), ("stt", "srt", Some("de")));
        for bad in
            [&[("response_format", "json")][..], &[("model", "stt"), ("response_format", "xml")], &[("model", "stt"), ("stream", "true")]]
        {
            let (ct, body) = form(bad);
            assert!(parse_transcription(&ct, &body).is_err(), "accepted {bad:?}");
        }
        let (ct, _) = form(&[]);
        let no_file = multipart::build(&[Part::text("model", "stt")]).1;
        assert!(parse_transcription(&ct, &Bytes::from(no_file)).is_err());
    }

    #[test]
    fn transcription_usage_from_tokens_seconds_or_duration() {
        let tokens = json!({ "text": "hi", "usage": { "type": "tokens", "input_tokens": 14, "output_tokens": 45, "total_tokens": 59 } });
        assert_eq!((transcription_usage(&tokens).input_tokens, transcription_usage(&tokens).output_tokens), (14, 45));
        assert_eq!(transcription_usage(&json!({ "usage": { "type": "duration", "seconds": 27 } })).audio_seconds, 27);
        assert_eq!(transcription_usage(&json!({ "duration": 9.2, "text": "" })).audio_seconds, 10);
        assert_eq!(transcription_usage(&json!({ "text": "" })), Usage::default());
    }

    #[test]
    fn mistral_transcripts_become_openai_ones() {
        let v = json!({ "model": "voxtral", "text": " Hello there.", "language": "en",
                        "segments": [{ "text": " Hello", "start": 0.0, "end": 1.25 }, { "text": " there.", "start": 1.25, "end": 2.5 }],
                        "usage": { "prompt_audio_seconds": 3, "prompt_tokens": 4, "total_tokens": 300, "completion_tokens": 20 } });
        let (json_out, usage) = from_mistral("json", &v);
        assert_eq!(json_out, json!({ "text": " Hello there.", "usage": { "type": "duration", "seconds": 3 } }));
        assert_eq!(usage, Usage { audio_seconds: 3, ..Default::default() });
        let (verbose, _) = from_mistral("srt", &v);
        assert_eq!(verbose["segments"][1], json!({ "id": 1, "start": 1.25, "end": 2.5, "text": " there." }));
        assert_eq!(render("srt", &verbose).1, "1\n00:00:00,000 --> 00:00:01,250\nHello\n\n2\n00:00:01,250 --> 00:00:02,500\nthere.\n\n");
        assert_eq!(
            render("vtt", &verbose).1,
            "WEBVTT\n\n00:00:00.000 --> 00:00:01.250\nHello\n\n00:00:01.250 --> 00:00:02.500\nthere.\n\n"
        );
        assert_eq!(render("text", &json_out), ("text/plain; charset=utf-8", Bytes::from(" Hello there.")));
        assert_eq!(timestamp(3723.4567, false), "01:02:03,457");
    }

    #[test]
    fn upstreams_get_formats_that_report_usage() {
        assert_eq!(
            [upstream_format("text"), upstream_format("srt"), upstream_format("vtt"), upstream_format("json")],
            ["json", "verbose_json", "verbose_json", "json"]
        );
    }

    #[test]
    fn speech_requests_are_checked_and_translated() {
        let r = parse_speech(json!({ "model": "tts", "input": "Hi", "voice": { "id": "voice_478" }, "response_format": "wav" })).unwrap();
        assert_eq!((r.voice.as_deref(), r.format.as_str()), (Some("voice_478"), "wav"));
        assert_eq!(parse_speech(json!({ "model": "tts", "input": "Hi" })).unwrap().format, "mp3");
        for bad in [
            json!({ "input": "Hi" }),
            json!({ "model": "tts", "input": " " }),
            json!({ "model": "tts", "input": "Hi", "response_format": "ogg" }),
            json!({ "model": "tts", "input": "Hi", "stream_format": "sse" }),
            json!({ "model": "tts", "input": "x".repeat(MAX_SPEECH_CHARS + 1) }),
        ] {
            assert!(parse_speech(bad.clone()).is_err(), "accepted {bad}");
        }
        assert_eq!(mistral_audio(&json!({ "audio_data": "SUQz" })).unwrap(), Bytes::from_static(b"ID3"));
        assert!(mistral_audio(&json!({ "audio_data": "" })).is_err());
        assert_eq!(audio_type("opus"), "audio/ogg");
        // Mistral needs a voice of its own.
        assert_eq!(
            [mistral_voice(None), mistral_voice(Some("alloy")), mistral_voice(Some("voice_478"))],
            [MISTRAL_VOICE, MISTRAL_VOICE, "voice_478"]
        );
    }

    #[test]
    fn silence_is_a_valid_wav() {
        let w = silent_wav(1);
        assert_eq!(w.len(), 44 + 32_000);
        assert_eq!((&w[0..4], &w[8..12], &w[36..40]), (&b"RIFF"[..], &b"WAVE"[..], &b"data"[..]));
    }
}
