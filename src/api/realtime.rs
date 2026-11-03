//! `GET /v1/realtime?intent=transcription`: OpenAI's Realtime API for transcription sessions, over
//! a WebSocket. Clients configure the session with `session.update`, stream PCM16 audio with
//! `input_audio_buffer.append`, and get `conversation.item.input_audio_transcription.delta` events
//! while they speak and a `.completed` one for each item they commit.
//!
//! Realtime transcription deployments are Mistral's (Voxtral realtime), spoken to in Mistral's own
//! protocol: each item is one Mistral session, opened by its first audio and ended with
//! `input_audio.end` when the client commits it, whose `transcription.done` completes the item.
//! Turn detection is not offered: items end when the client commits them.
//!
//! A session is one entry in the request log, billed for the seconds of audio it sent upstream at
//! the deployment's price per minute.

use std::{
    collections::HashMap,
    sync::{Arc, OnceLock},
    time::Duration,
};

use axum::{
    extract::{
        Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, HeaderValue, header},
    response::{IntoResponse, Response},
};
use base64::Engine;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::mpsc,
};
use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};
use uuid::Uuid;

use crate::{
    accounting::Tracker,
    error::{Dialect, ErrorKind, GatewayError},
    ir::Usage,
    labels::{ClientLabels, Labels},
    model::{Deployment, Mode, ProviderKind, VirtualKey},
    providers::{self, Op},
    state::AppState,
};

/// OpenAI's rate for transcription sessions, and the default.
const DEFAULT_RATE: u32 = 24_000;
/// How long a session may last.
const MAX_SESSION: Duration = Duration::from_secs(60 * 60);
/// Connecting to the upstream and its first event.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// How long an ended item may take to be transcribed, between the upstream's events.
const DONE_TIMEOUT: Duration = Duration::from_secs(30);
/// Audio events an item buffers while its upstream connects: about 100 s of 100 ms chunks.
const AUDIO_QUEUE: usize = 1024;
/// OpenAI's limit on one client event.
const MAX_EVENT_BYTES: usize = 15 * 1024 * 1024;
/// The WebSocket subprotocol browsers send their key in, as OpenAI's clients do.
const KEY_PROTOCOL: &str = "openai-insecure-api-key.";

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct RealtimeQuery {
    /// `transcription`: the only kind of session served.
    intent: Option<String>,
    /// The transcription model, instead of `session.audio.input.transcription.model`.
    model: Option<String>,
}

/// Realtime transcription
///
/// OpenAI's Realtime API for transcription sessions, over a WebSocket: stream audio, get its
/// transcript as it is spoken. Browsers may send their key as the `openai-insecure-api-key.<key>`
/// subprotocol.
#[utoipa::path(get, path = "/v1/realtime", tag = "Inference", params(RealtimeQuery,
        ("x-gateway-labels" = Option<String>, Header, description = crate::openapi::LABELS_HEADER)),
    responses(
        (status = 101, description = "The session: OpenAI Realtime events over a WebSocket."),
        (status = 400, description = "Not a transcription session, or not a WebSocket.", body = crate::error::OpenAiError),
        (status = 401, description = "Missing, invalid, disabled or expired key.", body = crate::error::OpenAiError),
    ),
    security(("virtual_key" = []), ("virtual_key_header" = [])))]
pub async fn realtime(
    State(state): State<AppState>,
    Query(q): Query<RealtimeQuery>,
    ClientLabels(labels): ClientLabels,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let key = match crate::auth::authenticate(&state, &with_protocol_key(&headers)) {
        Ok(key) => key,
        Err(e) => return e.into_response_for(Dialect::OpenAi),
    };
    serve(state, key, labels, q.intent.as_deref(), q.model, ws)
}

/// The headers, with a key sent as the `openai-insecure-api-key.<key>` subprotocol as a bearer token.
fn with_protocol_key(headers: &HeaderMap) -> HeaderMap {
    let mut h = headers.clone();
    if let Some(v) = protocol_key(headers).and_then(|k| HeaderValue::from_str(&format!("Bearer {k}")).ok()) {
        h.insert(header::AUTHORIZATION, v);
    }
    h
}

/// A key sent as the `openai-insecure-api-key.<key>` WebSocket subprotocol: browsers cannot set
/// headers on a WebSocket. The admin API takes the console's key this way too.
pub(crate) fn protocol_key(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .find_map(|p| p.trim().strip_prefix(KEY_PROTOCOL).map(str::to_string))
        .filter(|k| !k.is_empty())
}

/// Start a transcription session as an authenticated key; also used by the admin playground.
pub fn serve(
    state: AppState,
    key: Arc<VirtualKey>,
    labels: Labels,
    intent: Option<&str>,
    model: Option<String>,
    ws: WebSocketUpgrade,
) -> Response {
    if intent != Some("transcription") {
        return GatewayError::invalid("only transcription sessions are served: connect with ?intent=transcription")
            .into_response_for(Dialect::OpenAi);
    }
    // Browsers offer `realtime` with the key's subprotocol; the answer names one they offered.
    ws.protocols(["realtime"])
        .max_message_size(MAX_EVENT_BYTES + 1024)
        .on_upgrade(move |socket| Session::new(state, key, labels, model).run(socket))
        .into_response()
}

// ---------------------------------------------------------------- the client's session

/// Audio for an item's upstream session.
#[derive(Debug)]
enum Audio {
    /// Base64 PCM16.
    Append(String),
    End,
}

/// What an item's upstream session reports.
#[derive(Debug)]
enum Event {
    Delta { item: String, text: String },
    Done { item: String, text: String },
    Failed { item: String, message: String },
}

/// The item audio is appended to: its upstream session, and how much audio it got.
struct Item {
    id: String,
    audio: mpsc::Sender<Audio>,
    bytes: u64,
}

struct Session {
    state: AppState,
    key: Arc<VirtualKey>,
    labels: Labels,
    id: String,
    model: Option<String>,
    language: Option<String>,
    rate: u32,
    deployment: Option<Arc<Deployment>>,
    /// The request log entry, from the first audio on.
    tracker: Option<Tracker>,
    current: Option<Item>,
    /// Committed items whose transcript has not come yet: their upstream stays open until then.
    finishing: HashMap<String, mpsc::Sender<Audio>>,
    last_committed: Option<String>,
    /// Seconds of audio sent upstream: what the session is billed for.
    seconds: f64,
    events_tx: mpsc::Sender<Event>,
    events: mpsc::Receiver<Event>,
}

impl Session {
    fn new(state: AppState, key: Arc<VirtualKey>, labels: Labels, model: Option<String>) -> Self {
        let (events_tx, events) = mpsc::channel(256);
        Self {
            state,
            key,
            labels,
            id: format!("sess_{}", Uuid::new_v4().simple()),
            model: model.filter(|m| !m.trim().is_empty()),
            language: None,
            rate: DEFAULT_RATE,
            deployment: None,
            tracker: None,
            current: None,
            finishing: HashMap::new(),
            last_committed: None,
            seconds: 0.0,
            events_tx,
            events,
        }
    }

    async fn run(mut self, mut socket: WebSocket) {
        // A model named in the URL is checked at once.
        let mut first = vec![self.created()];
        if let Some(m) = self.model.clone()
            && let Err(e) = self.pick(&m)
        {
            first.push(error(&e, None));
        }
        for event in first {
            if send(&mut socket, event).await.is_err() {
                return;
            }
        }
        let deadline = tokio::time::sleep(MAX_SESSION);
        tokio::pin!(deadline);
        loop {
            let out = tokio::select! {
                msg = socket.recv() => match msg {
                    Some(Ok(Message::Text(text))) => self.on_client(&text).await,
                    Some(Ok(Message::Binary(_))) => vec![invalid("events are JSON text messages", None)],
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(_)) => Vec::new(),
                },
                Some(event) = self.events.recv() => self.on_upstream(event),
                () = &mut deadline => {
                    let _ = send(&mut socket, error_event("invalid_request_error", Some("session_expired"), "the session reached its time limit of 60 minutes", None)).await;
                    break;
                }
            };
            for event in out {
                if send(&mut socket, event).await.is_err() {
                    return self.finish();
                }
            }
        }
        // Closes, or answers the client's close, and sends it before the connection goes.
        let _ = socket.close().await;
        self.finish();
    }

    /// The request log entry: the seconds of audio sent, whatever became of them.
    fn finish(mut self) {
        if let Some(mut t) = self.tracker.take() {
            t.set_usage(Usage { audio_seconds: self.seconds.ceil() as u64, ..Default::default() });
        }
    }

    async fn on_client(&mut self, text: &str) -> Vec<Value> {
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            return vec![invalid("the event is not valid JSON", None)];
        };
        let event_id = v["event_id"].as_str().map(str::to_string);
        let eid = event_id.as_deref();
        match v["type"].as_str() {
            Some("session.update") | Some("transcription_session.update") => match self.configure(&v["session"], &v) {
                Ok(()) => vec![self.updated(v["type"] == "transcription_session.update")],
                Err(e) => vec![error(&e, eid)],
            },
            Some("input_audio_buffer.append") => match v["audio"].as_str() {
                Some(audio) => self.append(audio, eid).await,
                None => vec![invalid("audio must be a base64 string of PCM16 audio", eid)],
            },
            Some("input_audio_buffer.commit") => self.commit(eid),
            Some("input_audio_buffer.clear") => {
                // Dropping the item's sender closes its upstream session.
                self.current = None;
                vec![json!({ "event_id": event_id_new(), "type": "input_audio_buffer.cleared" })]
            }
            Some(t) if t.starts_with("response.") || t.starts_with("conversation.") || t.starts_with("output_audio_buffer.") => {
                vec![invalid(&format!("{t} is not part of transcription sessions"), eid)]
            }
            Some(t) => vec![error_event("invalid_request_error", Some("invalid_event"), &format!("unknown event type '{t}'"), eid)],
            None => vec![error_event("invalid_request_error", Some("invalid_event"), "the 'type' field is missing", eid)],
        }
    }

    /// `session.update`: OpenAI's session shape (`session.audio.input`), or the older one
    /// (`input_audio_format`, `input_audio_transcription`).
    fn configure(&mut self, s: &Value, event: &Value) -> Result<(), GatewayError> {
        let s = if s.is_object() { s } else { event };
        if let Some(t) = s["type"].as_str()
            && t != "transcription"
        {
            return Err(GatewayError::invalid("only transcription sessions are served: session.type must be transcription"));
        }
        let input = &s["audio"]["input"];
        let transcription = if input["transcription"].is_object() { &input["transcription"] } else { &s["input_audio_transcription"] };
        let mut rate = self.rate;
        if let Some(format) = input.get("format").filter(|f| !f.is_null()) {
            if format["type"].as_str().is_some_and(|t| t != "audio/pcm") {
                return Err(GatewayError::invalid("the audio format must be audio/pcm (PCM16)"));
            }
            if let Some(r) = format.get("rate").filter(|r| !r.is_null()) {
                rate =
                    r.as_u64().and_then(|r| u32::try_from(r).ok()).filter(|r| (8_000..=48_000).contains(r)).ok_or_else(|| {
                        GatewayError::invalid("the audio rate must be a sample rate from 8000 to 48000 (24000 by default)")
                    })?;
            }
        }
        if let Some(f) = s["input_audio_format"].as_str()
            && f != "pcm16"
        {
            return Err(GatewayError::invalid("the input audio format must be pcm16"));
        }
        if rate != self.rate && self.current.as_ref().is_some_and(|i| i.bytes > 0) {
            return Err(GatewayError::invalid("the audio rate cannot change while audio is buffered; commit or clear it first"));
        }
        if let Some(m) = transcription["model"].as_str().map(str::trim).filter(|m| !m.is_empty()) {
            if self.tracker.is_some() && self.model.as_deref() != Some(m) {
                return Err(GatewayError::invalid("the transcription model cannot change once audio has been sent"));
            }
            self.pick(m)?;
        }
        if let Some(lang) = transcription["language"].as_str().or_else(|| transcription["languages"][0].as_str()) {
            self.language = Some(lang.to_string()).filter(|l| !l.trim().is_empty());
        }
        self.rate = rate;
        Ok(())
    }

    /// Take `model` for the session's items: a realtime transcription model the key may use.
    fn pick(&mut self, model: &str) -> Result<(), GatewayError> {
        self.deployment = Some(deployment(&self.state, &self.key, model)?);
        self.model = Some(model.to_string());
        Ok(())
    }

    async fn append(&mut self, audio: &str, eid: Option<&str>) -> Vec<Value> {
        let Some(bytes) = pcm_len(audio) else {
            return vec![invalid("audio must be base64 PCM16: whole 16-bit samples", eid)];
        };
        if self.current.is_none()
            && let Err(e) = self.open().await
        {
            return vec![error(&e, eid)];
        }
        let item = self.current.as_mut().expect("an item is open");
        match item.audio.try_send(Audio::Append(audio.to_string())) {
            Ok(()) => {
                item.bytes += bytes;
                self.seconds += bytes as f64 / (2.0 * self.rate as f64);
                Vec::new()
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                vec![invalid("audio arrives faster than the upstream takes it; send it at the pace it is spoken", eid)]
            }
            // The upstream session failed; its failure is reported on its own.
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.current = None;
                Vec::new()
            }
        }
    }

    /// A new item, with its upstream session, for the audio that follows.
    async fn open(&mut self) -> Result<(), GatewayError> {
        let (Some(model), Some(d)) = (self.model.clone(), self.deployment.clone()) else {
            return Err(GatewayError::invalid(
                "no transcription model: set session.audio.input.transcription.model with session.update first",
            ));
        };
        crate::auth::check_key_limits(&self.state, &self.key).await?;
        if self.tracker.is_none() {
            let mut t = Tracker::new(&self.state, Some(&self.key), &self.labels, &model, "realtime", true);
            t.attempt(d.clone());
            self.tracker = Some(t);
        }
        let id = format!("item_{}", Uuid::new_v4().simple());
        let (tx, rx) = mpsc::channel(AUDIO_QUEUE);
        let upstream = Upstream { state: self.state.clone(), d, item: id.clone(), rate: self.rate, language: self.language.clone() };
        tokio::spawn(upstream.run(rx, self.events_tx.clone()));
        self.current = Some(Item { id, audio: tx, bytes: 0 });
        Ok(())
    }

    fn commit(&mut self, eid: Option<&str>) -> Vec<Value> {
        let Some(item) = self.current.take().filter(|i| i.bytes > 0) else {
            return vec![error_event(
                "invalid_request_error",
                Some("input_audio_buffer_commit_empty"),
                "Error committing input audio buffer: the buffer is empty.",
                eid,
            )];
        };
        if item.audio.try_send(Audio::End).is_err() {
            // Failed meanwhile: reported on its own.
            return Vec::new();
        }
        let previous = self.last_committed.replace(item.id.clone());
        let committed = json!({
            "event_id": event_id_new(), "type": "input_audio_buffer.committed",
            "previous_item_id": previous, "item_id": item.id,
        });
        self.finishing.insert(item.id, item.audio);
        vec![committed]
    }

    fn on_upstream(&mut self, event: Event) -> Vec<Value> {
        match event {
            Event::Delta { item, text } => vec![json!({
                "event_id": event_id_new(), "type": "conversation.item.input_audio_transcription.delta",
                "item_id": item, "content_index": 0, "delta": text,
            })],
            Event::Done { item, text } => {
                self.finishing.remove(&item);
                vec![json!({
                    "event_id": event_id_new(), "type": "conversation.item.input_audio_transcription.completed",
                    "item_id": item, "content_index": 0, "transcript": text,
                })]
            }
            Event::Failed { item, message } => {
                self.finishing.remove(&item);
                if self.current.as_ref().is_some_and(|i| i.id == item) {
                    self.current = None;
                }
                if let Some(t) = self.tracker.as_mut() {
                    t.fail(&GatewayError::upstream_stream(ErrorKind::Upstream, message.clone()));
                }
                vec![json!({
                    "event_id": event_id_new(), "type": "conversation.item.input_audio_transcription.failed",
                    "item_id": item, "content_index": 0,
                    "error": { "type": "transcription_error", "code": null, "message": message, "param": null },
                })]
            }
        }
    }

    fn session(&self) -> Value {
        let transcription = self.model.as_ref().map(|m| json!({ "model": m, "language": self.language }));
        json!({
            "id": self.id, "object": "realtime.transcription_session", "type": "transcription",
            "audio": { "input": {
                "format": { "type": "audio/pcm", "rate": self.rate },
                "transcription": transcription,
                "turn_detection": null,
                "noise_reduction": null,
            }},
        })
    }

    fn created(&self) -> Value {
        json!({ "event_id": event_id_new(), "type": "session.created", "session": self.session() })
    }

    fn updated(&self, older: bool) -> Value {
        let kind = if older { "transcription_session.updated" } else { "session.updated" };
        json!({ "event_id": event_id_new(), "type": kind, "session": self.session() })
    }
}

/// The realtime transcription deployment for `model` that the key may use.
fn deployment(state: &AppState, key: &VirtualKey, model: &str) -> Result<Arc<Deployment>, GatewayError> {
    if !key.allows_model(model) {
        return Err(GatewayError::forbidden(format!("this key is not allowed to use model '{model}'")));
    }
    let mut candidates = super::usable_deployments(state, key, model, Mode::Transcription)?;
    candidates.retain(|d| d.config.realtime);
    let d = candidates.into_iter().next().ok_or_else(|| {
        GatewayError::invalid(format!("'{model}' is not a realtime transcription model; send its audio files to /v1/audio/transcriptions"))
    })?;
    if d.provider != ProviderKind::Mistral {
        return Err(GatewayError::invalid(format!(
            "'{model}' is on {}; realtime transcription is served by Mistral deployments",
            d.provider.as_str()
        )));
    }
    Ok(d)
}

/// The bytes of PCM in base64 `audio`, if they are whole 16-bit samples.
fn pcm_len(audio: &str) -> Option<u64> {
    let n = audio.len();
    if !n.is_multiple_of(4) {
        return None;
    }
    let padding = audio.bytes().rev().take(2).filter(|b| *b == b'=').count();
    let bytes = (n / 4 * 3 - padding) as u64;
    bytes.is_multiple_of(2).then_some(bytes)
}

fn event_id_new() -> String {
    format!("event_{}", Uuid::new_v4().simple())
}

fn error_event(kind: &str, code: Option<&str>, message: &str, event_id: Option<&str>) -> Value {
    json!({
        "event_id": event_id_new(), "type": "error",
        "error": { "type": kind, "code": code, "message": message, "param": null, "event_id": event_id },
    })
}

fn invalid(message: &str, event_id: Option<&str>) -> Value {
    error_event("invalid_request_error", None, message, event_id)
}

fn error(e: &GatewayError, event_id: Option<&str>) -> Value {
    error_event(e.openai_type(), e.code, &e.message, event_id)
}

async fn send(socket: &mut WebSocket, event: Value) -> Result<(), axum::Error> {
    socket.send(Message::Text(event.to_string().into())).await
}

// ---------------------------------------------------------------- Mistral

/// One item's session with Mistral's realtime transcription.
struct Upstream {
    state: AppState,
    d: Arc<Deployment>,
    item: String,
    rate: u32,
    language: Option<String>,
}

impl Upstream {
    async fn run(self, audio: mpsc::Receiver<Audio>, events: mpsc::Sender<Event>) {
        if let Err(message) = self.transcribe(audio, &events).await {
            tracing::warn!(deployment = %self.d.id, %message, "realtime transcription failed");
            let _ = events.send(Event::Failed { item: self.item.clone(), message }).await;
        }
    }

    async fn transcribe(&self, mut audio: mpsc::Receiver<Audio>, events: &mpsc::Sender<Event>) -> Result<(), String> {
        let mut ws = connect(&self.state, &self.d).await.map_err(|e| e.message)?;
        // Mistral opens with session.created; nothing may be sent before it.
        let created = tokio::time::timeout(CONNECT_TIMEOUT, async {
            while let Some(m) = ws.next().await {
                match m.map_err(|e| e.to_string())? {
                    tungstenite::Message::Text(t) => {
                        let v: Value = serde_json::from_str(&t).unwrap_or_default();
                        match v["type"].as_str() {
                            Some("session.created") => return Ok(()),
                            Some("error") => return Err(mistral_error(&v)),
                            _ => {}
                        }
                    }
                    tungstenite::Message::Close(_) => break,
                    _ => {}
                }
            }
            Err("the upstream closed the session".to_string())
        });
        created.await.map_err(|_| "the upstream did not open the session".to_string())??;
        let mut session = json!({ "audio_format": { "encoding": "pcm_s16le", "sample_rate": self.rate } });
        if let Some(l) = &self.language {
            session["language"] = l.as_str().into();
        }
        send_upstream(&mut ws, json!({ "type": "session.update", "session": session })).await?;
        let mut ended = false;
        loop {
            tokio::select! {
                a = audio.recv(), if !ended => match a {
                    Some(Audio::Append(b64)) => send_upstream(&mut ws, json!({ "type": "input_audio.append", "audio": b64 })).await?,
                    Some(Audio::End) => {
                        send_upstream(&mut ws, json!({ "type": "input_audio.end" })).await?;
                        ended = true;
                    }
                    // Cleared, or the client is gone.
                    None => {
                        let _ = ws.close(None).await;
                        return Ok(());
                    }
                },
                m = ws.next() => match m {
                    Some(Ok(tungstenite::Message::Text(t))) => {
                        let v: Value = serde_json::from_str(&t).unwrap_or_default();
                        let text = || v["text"].as_str().unwrap_or_default().to_string();
                        match v["type"].as_str() {
                            Some("transcription.text.delta") => {
                                let _ = events.send(Event::Delta { item: self.item.clone(), text: text() }).await;
                            }
                            Some("transcription.done") => {
                                let _ = events.send(Event::Done { item: self.item.clone(), text: text() }).await;
                                let _ = ws.close(None).await;
                                return Ok(());
                            }
                            Some("error") => return Err(mistral_error(&v)),
                            _ => {}
                        }
                    }
                    Some(Ok(tungstenite::Message::Close(_))) | None => return Err("the upstream closed the session".into()),
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e.to_string()),
                },
                () = tokio::time::sleep(DONE_TIMEOUT), if ended => return Err("the upstream did not finish the transcript".into()),
                () = events.closed() => {
                    let _ = ws.close(None).await;
                    return Ok(());
                }
            }
        }
    }
}

fn mistral_error(v: &Value) -> String {
    v.pointer("/error/message")
        .or_else(|| v.get("message"))
        .and_then(Value::as_str)
        .map_or_else(|| "the upstream reported an error".to_string(), str::to_string)
}

/// A connection to the upstream, TLS or not.
trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

type UpstreamSocket = tokio_tungstenite::WebSocketStream<Box<dyn Io>>;

async fn send_upstream(ws: &mut UpstreamSocket, event: Value) -> Result<(), String> {
    ws.send(tungstenite::Message::Text(event.to_string().into())).await.map_err(|e| e.to_string())
}

/// The deployment's realtime transcription WebSocket: its API base as `wss://`, the model in the
/// query, its credential's headers, and the gateway's outbound address checks.
async fn connect(state: &AppState, d: &Deployment) -> Result<UpstreamSocket, GatewayError> {
    let bad = |m: String| GatewayError::upstream_stream(ErrorKind::Upstream, m);
    let mut url = reqwest::Url::parse(&providers::url(state, d, Op::RealtimeTranscription).await?)
        .map_err(|_| GatewayError::internal("the realtime transcription URL is not valid"))?;
    let tls = match url.scheme() {
        "https" => true,
        "http" => false,
        s => return Err(GatewayError::internal(format!("unsupported scheme {s}"))),
    };
    url.set_scheme(if tls { "wss" } else { "ws" }).map_err(|()| GatewayError::internal("cannot use a WebSocket URL"))?;
    url.query_pairs_mut().append_pair("model", &d.upstream_model);
    let host = url.host_str().ok_or_else(|| GatewayError::internal("the upstream URL has no host"))?.to_string();
    let port = url.port_or_known_default().unwrap_or(if tls { 443 } else { 80 });
    let tcp = tokio::time::timeout(CONNECT_TIMEOUT, crate::netguard::connect(&state.cfg.upstream_policy, &host, port))
        .await
        .map_err(|_| GatewayError { upstream: true, ..GatewayError::timeout("upstream timed out") })?
        .map_err(|e| GatewayError::internal(format!("upstream address not allowed or unreachable: {e}")))?;
    let io: Box<dyn Io> = if tls {
        let name = rustls::pki_types::ServerName::try_from(host.clone()).map_err(|_| GatewayError::internal("invalid upstream host"))?;
        Box::new(tls_connector()?.connect(name, tcp).await.map_err(|e| bad(format!("TLS with the upstream failed: {e}")))?)
    } else {
        Box::new(tcp)
    };
    let mut req = url.as_str().into_client_request().map_err(|e| GatewayError::internal(e.to_string()))?;
    for (name, value) in providers::headers(d)?.iter() {
        if name != header::CONTENT_TYPE {
            req.headers_mut().insert(name.clone(), value.clone());
        }
    }
    let (ws, _) = tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::client_async(req, io))
        .await
        .map_err(|_| GatewayError { upstream: true, ..GatewayError::timeout("upstream timed out") })?
        .map_err(|e| match e {
            tungstenite::Error::Http(resp) => {
                let body = resp.body().as_deref().map(String::from_utf8_lossy).unwrap_or_default().to_string();
                GatewayError::from_upstream(resp.status(), &body)
            }
            e => bad(format!("the upstream WebSocket failed: {e}")),
        })?;
    Ok(ws)
}

/// TLS for upstream WebSockets: aws-lc-rs and the platform's certificate roots, as reqwest uses.
fn tls_connector() -> Result<tokio_rustls::TlsConnector, GatewayError> {
    static CONFIG: OnceLock<Result<Arc<rustls::ClientConfig>, String>> = OnceLock::new();
    let config = CONFIG.get_or_init(|| {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let verifier = rustls_platform_verifier::Verifier::new(provider.clone()).map_err(|e| e.to_string())?;
        let mut config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Arc::new(config))
    });
    config.clone().map(tokio_rustls::TlsConnector::from).map_err(|e| GatewayError::internal(format!("TLS setup failed: {e}")))
}

// ---------------------------------------------------------------- health check

/// Health check for the admin API: streams a second of silence and ends it, outside of any key's
/// accounting. Returns the transcript (usually empty).
pub async fn probe(state: &AppState, d: &Arc<Deployment>) -> Result<(String, Usage), GatewayError> {
    const RATE: u32 = 16_000;
    let (audio_tx, audio_rx) = mpsc::channel(4);
    let (events_tx, mut events) = mpsc::channel(64);
    let upstream = Upstream { state: state.clone(), d: d.clone(), item: "probe".into(), rate: RATE, language: None };
    tokio::spawn(upstream.run(audio_rx, events_tx));
    let silence = base64::engine::general_purpose::STANDARD.encode(vec![0u8; RATE as usize * 2]);
    let _ = audio_tx.send(Audio::Append(silence)).await;
    let _ = audio_tx.send(Audio::End).await;
    let wait = async {
        while let Some(event) = events.recv().await {
            match event {
                Event::Done { text, .. } => return Ok(text.trim().to_string()),
                Event::Failed { message, .. } => return Err(GatewayError::upstream_stream(ErrorKind::Upstream, message)),
                Event::Delta { .. } => {}
            }
        }
        Err(GatewayError::upstream_stream(ErrorKind::Upstream, "the upstream session ended without a transcript"))
    };
    let text = tokio::time::timeout(CONNECT_TIMEOUT + DONE_TIMEOUT, wait)
        .await
        .map_err(|_| GatewayError { upstream: true, ..GatewayError::timeout("upstream timed out") })??;
    drop(audio_tx);
    Ok((text, Usage { audio_seconds: 1, ..Default::default() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_lengths() {
        let b64 = |n: usize| base64::engine::general_purpose::STANDARD.encode(vec![0u8; n]);
        for n in [2, 4, 6, 3200, 9600] {
            assert_eq!(pcm_len(&b64(n)), Some(n as u64), "{n}");
        }
        assert_eq!(pcm_len(&b64(3)), None, "half a sample");
        assert_eq!(pcm_len("abc"), None, "not base64 length");
    }

    #[test]
    fn keys_in_the_subprotocol() {
        let mut h = HeaderMap::new();
        h.insert(header::SEC_WEBSOCKET_PROTOCOL, HeaderValue::from_static("realtime, openai-insecure-api-key.sk-gw-123"));
        assert_eq!(with_protocol_key(&h)[header::AUTHORIZATION], "Bearer sk-gw-123");
        assert!(!with_protocol_key(&HeaderMap::new()).contains_key(header::AUTHORIZATION));
    }

    #[test]
    fn mistral_errors() {
        assert_eq!(mistral_error(&json!({ "type": "error", "error": { "message": "bad delay", "code": 1 } })), "bad delay");
        assert_eq!(mistral_error(&json!({ "type": "error" })), "the upstream reported an error");
    }
}
