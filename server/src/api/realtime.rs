//! `/v1/realtime`: the Realtime API over WebSocket, with the seat as the
//! model. The connection keeps the session and the conversation items;
//! each `response.create` (or a committed turn under server VAD) becomes
//! one seat task in a conversation kept for the whole connection, with the
//! items as chat history and the session's tools as the caller's tools.
//! Input audio reaches the seat as WAV attachments; when the session wants
//! audio out, the seat attaches its spoken answer and its text is the
//! transcript. `POST /v1/realtime/client_secrets` mints short-lived keys
//! for browsers (also accepted as the `openai-insecure-api-key.` protocol).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Map, Value};
use tokio::sync::mpsc;

use crate::auth::AuthPrincipal;
use crate::storage::AppState;

fn uid(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::now_v7().simple())
}

// ── ephemeral keys ──

struct Ephemeral {
    principal: AuthPrincipal,
    session: Value,
    expires_at: i64,
}

fn ephemeral() -> &'static Mutex<HashMap<String, Ephemeral>> {
    static E: OnceLock<Mutex<HashMap<String, Ephemeral>>> = OnceLock::new();
    E.get_or_init(|| Mutex::new(HashMap::new()))
}

fn default_session(model: &str) -> Value {
    json!({
        "type": "realtime",
        "object": "realtime.session",
        "id": uid("sess"),
        "model": model,
        "output_modalities": ["audio"],
        "instructions": "",
        "tools": [],
        "tool_choice": "auto",
        "max_output_tokens": "inf",
        "audio": {
            "input": {
                "format": {"type": "audio/pcm", "rate": 24000},
                "transcription": null,
                "noise_reduction": null,
                "turn_detection": {"type": "server_vad", "threshold": 0.5, "prefix_padding_ms": 300,
                                   "silence_duration_ms": 500, "create_response": true, "interrupt_response": true}
            },
            "output": {"format": {"type": "audio/pcm", "rate": 24000}, "voice": "alloy", "speed": 1.0}
        },
    })
}

/// Merge a session update into the session (objects merge, the rest is
/// replaced).
fn merge(into: &mut Value, from: &Value) {
    // The session's own kind and id stay; nested `type`s (formats, turn
    // detection) are ordinary settings.
    if let (Value::Object(a), Value::Object(b)) = (&mut *into, from) {
        for (k, v) in b {
            if k == "type" || k == "id" || k == "object" {
                continue;
            }
            match a.get_mut(k) {
                Some(x) => merge_value(x, v),
                None => {
                    a.insert(k.clone(), v.clone());
                }
            }
        }
    }
}

fn merge_value(into: &mut Value, from: &Value) {
    match (into, from) {
        (Value::Object(a), Value::Object(b)) => {
            for (k, v) in b {
                match a.get_mut(k) {
                    Some(x) if x.is_object() && v.is_object() => merge_value(x, v),
                    _ => {
                        a.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (a, b) => *a = b.clone(),
    }
}

/// `POST /v1/realtime/client_secrets` (and the beta `/v1/realtime/sessions`).
pub async fn client_secrets(principal: AuthPrincipal, body: Option<Json<Value>>) -> Response {
    let b = body.map(|Json(b)| b).unwrap_or(json!({}));
    let secs = b
        .pointer("/expires_after/seconds")
        .and_then(|v| v.as_i64())
        .unwrap_or(600)
        .clamp(10, 7200);
    let cfg = b.get("session").cloned().unwrap_or_else(|| b.clone());
    let model = cfg
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("gpt-realtime")
        .to_string();
    let mut session = if cfg["type"] == "transcription" {
        transcription_session(&json!({}))
    } else {
        default_session(&model)
    };
    merge(&mut session, &cfg);
    let (value, expires_at) = mint(principal, session.clone(), secs);
    Json(json!({
        "value": value,
        "expires_at": expires_at,
        "session": session,
        // Beta shape (`/v1/realtime/sessions`).
        "client_secret": {"value": value, "expires_at": expires_at},
    }))
    .into_response()
}

// ── connection ──

#[derive(Debug, serde::Deserialize, Default)]
pub struct RtQuery {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    intent: Option<String>,
    #[serde(default)]
    call_id: Option<String>,
}

pub async fn realtime(
    ws: WebSocketUpgrade,
    State(app): State<AppState>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<RtQuery>,
) -> Response {
    let (principal, preset, protocols) = match caller(&app, &headers).await {
        Ok(r) => r,
        Err(e) => return e,
    };
    let _ = q.call_id;
    let model = q.model.unwrap_or_else(|| "gpt-realtime".into());
    let mut session = default_session(&model);
    if q.intent.as_deref() == Some("transcription") {
        session = transcription_session(&json!({}));
    }
    if let Some(p) = preset {
        if p["type"] == "translation" {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": {"message": "This client secret is for /v1/realtime/translations", "code": "invalid_session_type"}}))).into_response();
        }
        merge(&mut session, &p);
    }
    let ws = if protocols.iter().any(|p| p == "realtime") {
        ws.protocols(["realtime"])
    } else {
        ws
    };
    ws.on_upgrade(move |socket| run(socket, app, principal, headers, session))
}

/// Per-connection memory bounds. Each is overridable by env:
/// `OMNI2API_REALTIME_MAX_BUFFER_BYTES` (uncommitted input audio, default
/// 15 MiB, OpenAI's input-audio-buffer limit),
/// `OMNI2API_REALTIME_MAX_AUDIO_BYTES` (committed audio kept for the
/// conversation, default 64 MiB) and `OMNI2API_REALTIME_MAX_ITEMS`
/// (conversation items, default 4096). An append, commit or item past a
/// bound is refused with an `error` event and changes nothing; the
/// connection stays open.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub buffer_bytes: usize,
    pub audio_bytes: usize,
    pub items: usize,
}

impl Limits {
    pub fn from_env() -> Self {
        let get = |k: &str, d: usize| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.trim().parse::<usize>().ok())
                .filter(|v| *v > 0)
                .unwrap_or(d)
        };
        Limits {
            buffer_bytes: get("OMNI2API_REALTIME_MAX_BUFFER_BYTES", 15 * 1024 * 1024),
            audio_bytes: get("OMNI2API_REALTIME_MAX_AUDIO_BYTES", 64 * 1024 * 1024),
            items: get("OMNI2API_REALTIME_MAX_ITEMS", 4096),
        }
    }
}

/// What a connection holds.
struct Conn {
    session: Value,
    items: Vec<Value>,
    /// Committed audio per user item id (pcm16; g711 input is decoded on
    /// commit).
    audio: HashMap<String, Vec<u8>>,
    /// Sum of `audio`'s lengths.
    audio_bytes: usize,
    buffer: Vec<u8>,
    /// Server VAD: whether speech is under way, and quiet bytes since.
    speaking: bool,
    quiet: usize,
    running: Option<tokio::task::AbortHandle>,
    limits: Limits,
}

impl Conn {
    fn new(session: Value, items: Vec<Value>, limits: Limits) -> Self {
        Conn {
            session,
            items,
            audio: HashMap::new(),
            audio_bytes: 0,
            buffer: Vec::new(),
            speaking: false,
            quiet: 0,
            running: None,
            limits,
        }
    }
}

/// The session's input audio encoding.
#[derive(Clone, Copy, Debug, PartialEq)]
enum InFormat {
    Pcm16,
    Ulaw,
    Alaw,
}

/// The negotiated input format and its sample rate. G.711 is always
/// 8 kHz (a stale `rate` left over from a pcm format does not apply).
fn input_format(session: &Value) -> (InFormat, u32) {
    let f = session.pointer("/audio/input/format");
    let ty = f
        .and_then(|f| f.get("type"))
        .and_then(|t| t.as_str())
        .unwrap_or("audio/pcm");
    match ty {
        "audio/pcmu" | "g711_ulaw" => (InFormat::Ulaw, 8000),
        "audio/pcma" | "g711_alaw" => (InFormat::Alaw, 8000),
        _ => (
            InFormat::Pcm16,
            f.and_then(|f| f.get("rate"))
                .and_then(|r| r.as_u64())
                .filter(|r| *r > 0)
                .unwrap_or(24000) as u32,
        ),
    }
}

/// Input bytes per millisecond of audio for the negotiated format
/// (24 kHz pcm16: 48; G.711: 8).
fn bytes_per_ms(session: &Value) -> usize {
    let (fmt, rate) = input_format(session);
    let per_sample = if fmt == InFormat::Pcm16 { 2 } else { 1 };
    ((rate as usize * per_sample) / 1000).max(1)
}

/// G.711 mu-law sample to linear pcm16.
fn ulaw_to_pcm(u: u8) -> i16 {
    let u = !u;
    let sign = u & 0x80;
    let exp = (u >> 4) & 0x07;
    let mant = (u & 0x0f) as i32;
    let mag = (((mant << 3) + 0x84) << exp) - 0x84;
    (if sign != 0 { -mag } else { mag }) as i16
}

/// G.711 A-law sample to linear pcm16.
fn alaw_to_pcm(a: u8) -> i16 {
    let a = a ^ 0x55;
    let sign = a & 0x80;
    let exp = (a >> 4) & 0x07;
    let mant = (a & 0x0f) as i32;
    let mag = if exp == 0 {
        (mant << 4) + 8
    } else {
        ((mant << 4) + 0x108) << (exp - 1)
    };
    (if sign != 0 { mag } else { -mag }) as i16
}

/// Input bytes in the session's format as pcm16 little-endian.
fn to_pcm16(fmt: InFormat, b: &[u8]) -> Vec<u8> {
    let dec = match fmt {
        InFormat::Pcm16 => return b.to_vec(),
        InFormat::Ulaw => ulaw_to_pcm,
        InFormat::Alaw => alaw_to_pcm,
    };
    b.iter().flat_map(|x| dec(*x).to_le_bytes()).collect()
}

/// Loudness (RMS, 0..1) of input bytes in the session's format.
fn input_level(fmt: InFormat, b: &[u8]) -> f64 {
    match fmt {
        InFormat::Pcm16 => pcm_level(b),
        _ => pcm_level(&to_pcm16(fmt, b)),
    }
}

/// Append to the input buffer within its bound; the error event otherwise.
fn append_audio(c: &mut Conn, b: &[u8], eid: Option<&Value>) -> Result<(), Value> {
    if c.buffer.len() + b.len() > c.limits.buffer_bytes {
        return Err(error_ev(
            "invalid_request_error",
            "input_audio_buffer_too_large",
            &format!(
                "the input audio buffer is limited to {} bytes; commit or clear it first",
                c.limits.buffer_bytes
            ),
            eid,
        ));
    }
    c.buffer.extend_from_slice(b);
    Ok(())
}

/// Bytes of inline base64 audio an item carries (decoded size).
fn inline_audio_len(item: &Value) -> usize {
    item["content"]
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p["audio"].as_str())
                .map(|a| a.len() / 4 * 3)
                .sum()
        })
        .unwrap_or(0)
}

/// Remove an item and release the audio it held; whether it existed.
fn remove_item(c: &mut Conn, id: &str) -> bool {
    let Some(at) = c.items.iter().position(|i| i["id"] == json!(id)) else {
        return false;
    };
    let item = c.items.remove(at);
    let freed = inline_audio_len(&item) + c.audio.remove(id).map(|a| a.len()).unwrap_or(0);
    c.audio_bytes = c.audio_bytes.saturating_sub(freed);
    true
}

fn items_full(c: &Conn, eid: Option<&Value>) -> Option<Value> {
    (c.items.len() >= c.limits.items).then(|| {
        error_ev(
            "invalid_request_error",
            "conversation_items_limit_exceeded",
            &format!(
                "the conversation is limited to {} items; delete some first",
                c.limits.items
            ),
            eid,
        )
    })
}

async fn run(
    socket: WebSocket,
    app: AppState,
    principal: AuthPrincipal,
    headers: HeaderMap,
    session: Value,
) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        while let Some(mut ev) = rx.recv().await {
            if ev.get("event_id").is_none() {
                ev["event_id"] = json!(uid("event"));
            }
            if sink.send(WsMessage::Text(ev.to_string())).await.is_err() {
                break;
            }
        }
    });
    let conn = std::sync::Arc::new(tokio::sync::Mutex::new(Conn::new(
        session,
        Vec::new(),
        Limits::from_env(),
    )));
    let sid = conn.lock().await.session["id"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let _ = tx.send(json!({"type": "session.created", "session": conn.lock().await.session}));
    while let Some(Ok(msg)) = stream.next().await {
        let text = match msg {
            WsMessage::Text(t) => t,
            WsMessage::Close(_) => break,
            _ => continue,
        };
        let Ok(ev) = serde_json::from_str::<Value>(&text) else {
            let _ = tx.send(error_ev(
                "invalid_request_error",
                "invalid_json",
                "event is not JSON",
                None,
            ));
            continue;
        };
        handle(&ev, &conn, &tx, &app, &principal, &headers, &sid).await;
    }
    if let Some(h) = conn.lock().await.running.take() {
        h.abort();
    }
    drop(tx);
    let _ = writer.await;
}

fn error_ev(kind: &str, code: &str, msg: &str, event_id: Option<&Value>) -> Value {
    json!({"type": "error", "error": {"type": kind, "code": code, "message": msg, "param": null,
           "event_id": event_id.cloned().unwrap_or(Value::Null)}})
}

/// Add an item after `previous` (end when none), with its events.
fn add_item(
    c: &mut Conn,
    mut item: Value,
    previous: Option<&str>,
    tx: &mpsc::UnboundedSender<Value>,
) -> String {
    if item.get("id").and_then(|v| v.as_str()).is_none() {
        item["id"] = json!(uid("item"));
    }
    item["object"] = json!("realtime.item");
    if item.get("status").is_none() {
        item["status"] = json!("completed");
    }
    let id = item["id"].as_str().unwrap_or("").to_string();
    let at = match previous {
        Some("root") => 0,
        Some(p) => c
            .items
            .iter()
            .position(|i| i["id"] == json!(p))
            .map(|i| i + 1)
            .unwrap_or(c.items.len()),
        None => c.items.len(),
    };
    let prev = if at == 0 {
        Value::Null
    } else {
        c.items[at - 1]["id"].clone()
    };
    c.items.insert(at, item.clone());
    let _ =
        tx.send(json!({"type": "conversation.item.added", "previous_item_id": prev, "item": item}));
    let _ =
        tx.send(json!({"type": "conversation.item.done", "previous_item_id": prev, "item": item}));
    id
}

fn pcm_level(b: &[u8]) -> f64 {
    let n = b.len() / 2;
    if n == 0 {
        return 0.0;
    }
    let (samples, _) = b.as_chunks::<2>();
    let sum: f64 = samples
        .iter()
        .map(|s| {
            let v = i16::from_le_bytes(*s) as f64 / 32768.0;
            v * v
        })
        .sum();
    (sum / n as f64).sqrt()
}

/// PCM16 mono as a WAV file.
pub fn wav(pcm: &[u8], rate: u32) -> Vec<u8> {
    let mut w = Vec::with_capacity(44 + pcm.len());
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&rate.to_le_bytes());
    w.extend_from_slice(&(rate * 2).to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    w.extend_from_slice(pcm);
    w
}

/// The sample data of a WAV file (the bytes themselves when not WAV).
pub fn wav_data(b: &[u8]) -> Vec<u8> {
    if !(b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WAVE")) {
        return b.to_vec();
    }
    let mut i = 12;
    while i + 8 <= b.len() {
        let len = u32::from_le_bytes([b[i + 4], b[i + 5], b[i + 6], b[i + 7]]) as usize;
        if &b[i..i + 4] == b"data" {
            return b[i + 8..(i + 8 + len).min(b.len())].to_vec();
        }
        i += 8 + len + (len & 1);
    }
    b.to_vec()
}

/// Commit the input buffer as a user item. `Ok(None)` when the buffer is
/// empty; the error event when the committed-audio or item bound would be
/// passed (the buffer is then left as it was).
fn commit(
    c: &mut Conn,
    tx: &mpsc::UnboundedSender<Value>,
    eid: Option<&Value>,
) -> Result<Option<String>, Value> {
    if c.buffer.is_empty() {
        return Ok(None);
    }
    if let Some(e) = items_full(c, eid) {
        return Err(e);
    }
    let (fmt, _) = input_format(&c.session);
    let pcm = to_pcm16(fmt, &c.buffer);
    if c.audio_bytes + pcm.len() > c.limits.audio_bytes {
        return Err(error_ev(
            "invalid_request_error",
            "conversation_audio_limit_exceeded",
            &format!(
                "committed audio is limited to {} bytes per connection; delete earlier audio items or clear the buffer",
                c.limits.audio_bytes
            ),
            eid,
        ));
    }
    c.buffer.clear();
    let prev = c
        .items
        .last()
        .map(|i| i["id"].clone())
        .unwrap_or(Value::Null);
    let item_id = uid("item");
    let _ = tx.send(json!({"type": "input_audio_buffer.committed", "previous_item_id": prev, "item_id": item_id}));
    c.audio_bytes += pcm.len();
    c.audio.insert(item_id.clone(), pcm);
    add_item(
        c,
        json!({"id": item_id, "type": "message", "role": "user",
               "content": [{"type": "input_audio", "transcript": null}]}),
        None,
        tx,
    );
    Ok(Some(item_id))
}

#[allow(clippy::too_many_arguments)]
async fn handle(
    ev: &Value,
    conn: &std::sync::Arc<tokio::sync::Mutex<Conn>>,
    tx: &mpsc::UnboundedSender<Value>,
    app: &AppState,
    principal: &AuthPrincipal,
    headers: &HeaderMap,
    sid: &str,
) {
    let kind = ev.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let eid = ev.get("event_id");
    let mut c = conn.lock().await;
    match kind {
        "session.update" => {
            let upd = ev.get("session").cloned().unwrap_or(json!({}));
            merge(&mut c.session, &upd);
            let _ = tx.send(json!({"type": "session.updated", "session": c.session}));
        }
        "transcription_session.update" => {
            let upd = beta_transcription(&ev.get("session").cloned().unwrap_or(json!({})));
            merge(&mut c.session, &upd);
            let _ = tx.send(
                json!({"type": "transcription_session.updated", "session": beta_view(&c.session)}),
            );
        }
        "input_audio_buffer.append" => {
            let Some(b) = ev
                .get("audio")
                .and_then(|a| a.as_str())
                .and_then(crate::openai_compat::b64_decode)
            else {
                let _ = tx.send(error_ev(
                    "invalid_request_error",
                    "invalid_audio",
                    "audio must be base64",
                    eid,
                ));
                return;
            };
            if let Err(e) = append_audio(&mut c, &b, eid) {
                let _ = tx.send(e);
                return;
            }
            // Server VAD: speech when the level passes the threshold,
            // a turn when it stays quiet for silence_duration_ms.
            let td = c
                .session
                .pointer("/audio/input/turn_detection")
                .cloned()
                .unwrap_or(Value::Null);
            if td
                .get("type")
                .and_then(|t| t.as_str())
                .is_some_and(|t| t.ends_with("vad"))
            {
                // Byte math and loudness follow the negotiated format:
                // G.711 is decoded to pcm16 before its level is taken.
                let (fmt, _) = input_format(&c.session);
                let per_ms = bytes_per_ms(&c.session);
                let threshold = td.get("threshold").and_then(|v| v.as_f64()).unwrap_or(0.5) * 0.1;
                let silence_ms = td
                    .get("silence_duration_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(500) as usize;
                let loud = input_level(fmt, &b) > threshold;
                if loud {
                    c.quiet = 0;
                    if !c.speaking {
                        c.speaking = true;
                        let at = (c.buffer.len() / per_ms) as u64;
                        let _ = tx.send(json!({"type": "input_audio_buffer.speech_started", "audio_start_ms": at, "item_id": uid("item")}));
                        if td
                            .get("interrupt_response")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(true)
                        {
                            if let Some(h) = c.running.take() {
                                h.abort();
                            }
                        }
                    }
                } else if c.speaking {
                    c.quiet += b.len();
                    if c.quiet >= silence_ms * per_ms {
                        c.speaking = false;
                        c.quiet = 0;
                        let at = (c.buffer.len() / per_ms) as u64;
                        let _ = tx.send(json!({"type": "input_audio_buffer.speech_stopped", "audio_end_ms": at, "item_id": Value::Null}));
                        let committed = match commit(&mut c, tx, eid) {
                            Ok(id) => id,
                            Err(e) => {
                                let _ = tx.send(e);
                                None
                            }
                        };
                        if let Some(id) = &committed {
                            transcribe(
                                &c,
                                id,
                                conn.clone(),
                                tx.clone(),
                                app,
                                principal,
                                headers,
                                sid,
                            );
                        }
                        let create = td
                            .get("create_response")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(true)
                            && c.session["type"] != "transcription";
                        if committed.is_some() && create {
                            if let Some(e) = items_full(&c, eid) {
                                let _ = tx.send(e);
                                return;
                            }
                            start_response(
                                &mut c,
                                json!({}),
                                conn.clone(),
                                tx.clone(),
                                app.clone(),
                                principal.clone(),
                                headers.clone(),
                                sid,
                            );
                        }
                    }
                }
            }
        }
        "input_audio_buffer.commit" => match commit(&mut c, tx, eid) {
            Err(e) => {
                let _ = tx.send(e);
            }
            Ok(Some(id)) => {
                transcribe(
                    &c,
                    &id,
                    conn.clone(),
                    tx.clone(),
                    app,
                    principal,
                    headers,
                    sid,
                );
            }
            Ok(None) => {
                let _ = tx.send(error_ev(
                    "invalid_request_error",
                    "input_audio_buffer_commit_empty",
                    "buffer is empty",
                    eid,
                ));
            }
        },
        "input_audio_buffer.clear" => {
            c.buffer.clear();
            c.speaking = false;
            c.quiet = 0;
            let _ = tx.send(json!({"type": "input_audio_buffer.cleared"}));
        }
        "output_audio_buffer.clear" => {
            let _ = tx.send(json!({"type": "output_audio_buffer.cleared"}));
        }
        "conversation.item.create" => {
            let Some(item) = ev.get("item").cloned() else {
                let _ = tx.send(error_ev(
                    "invalid_request_error",
                    "missing_item",
                    "item is required",
                    eid,
                ));
                return;
            };
            if let Some(e) = items_full(&c, eid) {
                let _ = tx.send(e);
                return;
            }
            // Inline audio counts toward the committed-audio bound.
            let inline = inline_audio_len(&item);
            if c.audio_bytes + inline > c.limits.audio_bytes {
                let _ = tx.send(error_ev(
                    "invalid_request_error",
                    "conversation_audio_limit_exceeded",
                    &format!(
                        "conversation audio is limited to {} bytes per connection",
                        c.limits.audio_bytes
                    ),
                    eid,
                ));
                return;
            }
            c.audio_bytes += inline;
            let prev = ev
                .get("previous_item_id")
                .and_then(|v| v.as_str())
                .map(String::from);
            add_item(&mut c, item, prev.as_deref(), tx);
        }
        "conversation.item.retrieve" => {
            let id = ev.get("item_id").and_then(|v| v.as_str()).unwrap_or("");
            match c.items.iter().find(|i| i["id"] == json!(id)) {
                Some(i) => {
                    let _ = tx.send(json!({"type": "conversation.item.retrieved", "item": i}));
                }
                None => {
                    let _ = tx.send(error_ev(
                        "invalid_request_error",
                        "item_not_found",
                        &format!("no item {id}"),
                        eid,
                    ));
                }
            }
        }
        "conversation.item.delete" => {
            let id = ev
                .get("item_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let removed = remove_item(&mut c, &id);
            if removed {
                let _ = tx.send(json!({"type": "conversation.item.deleted", "item_id": id}));
            } else {
                let _ = tx.send(error_ev(
                    "invalid_request_error",
                    "item_not_found",
                    &format!("no item {id}"),
                    eid,
                ));
            }
        }
        "conversation.item.truncate" => {
            let id = ev.get("item_id").cloned().unwrap_or(Value::Null);
            let _ = tx.send(json!({"type": "conversation.item.truncated", "item_id": id,
                "content_index": ev.get("content_index").cloned().unwrap_or(json!(0)),
                "audio_end_ms": ev.get("audio_end_ms").cloned().unwrap_or(json!(0))}));
        }
        "response.create" if c.session["type"] == "transcription" => {
            let _ = tx.send(error_ev(
                "invalid_request_error",
                "invalid_session_type",
                "a transcription session makes no responses",
                eid,
            ));
        }
        "response.create" => {
            let opts = ev.get("response").cloned().unwrap_or(json!({}));
            if c.running.is_some() {
                let _ = tx.send(error_ev(
                    "invalid_request_error",
                    "conversation_already_has_active_response",
                    "a response is already in progress",
                    eid,
                ));
                return;
            }
            // A response adds items; refuse it when the conversation is full.
            if let Some(e) = items_full(&c, eid) {
                let _ = tx.send(e);
                return;
            }
            start_response(
                &mut c,
                opts,
                conn.clone(),
                tx.clone(),
                app.clone(),
                principal.clone(),
                headers.clone(),
                sid,
            );
        }
        "response.cancel" => match c.running.take() {
            Some(h) => {
                h.abort();
                let rid = ev.get("response_id").cloned().unwrap_or(Value::Null);
                let _ = tx.send(json!({"type": "response.done", "response": {"id": rid, "object": "realtime.response", "status": "cancelled", "output": []}}));
            }
            None => {
                let _ = tx.send(error_ev(
                    "invalid_request_error",
                    "response_cancel_not_active",
                    "no active response",
                    eid,
                ));
            }
        },
        other => {
            let _ = tx.send(error_ev(
                "invalid_request_error",
                "unknown_event",
                &format!("unsupported event type {other}"),
                eid,
            ));
        }
    }
}

/// The conversation as chat messages for the seat.
fn chat_messages(c: &Conn, instructions: &str) -> Vec<Value> {
    let mut out = Vec::new();
    if !instructions.trim().is_empty() {
        out.push(json!({"role": "system", "content": instructions}));
    }
    let (fmt, rate) = input_format(&c.session);
    for it in &c.items {
        match it["type"].as_str().unwrap_or("") {
            "message" => {
                let role = match it["role"].as_str().unwrap_or("user") {
                    "assistant" => "assistant",
                    "system" => "system",
                    _ => "user",
                };
                let mut parts = Vec::new();
                for p in it["content"].as_array().cloned().unwrap_or_default() {
                    match p["type"].as_str().unwrap_or("") {
                        "input_text" | "text" | "output_text" => parts.push(json!({"type": "text", "text": p["text"]})),
                        "input_audio" => {
                            let id = it["id"].as_str().unwrap_or("");
                            let bytes = c
                                .audio
                                .get(id)
                                .cloned()
                                .or_else(|| p["audio"].as_str().and_then(crate::openai_compat::b64_decode).map(|b| to_pcm16(fmt, &b)));
                            if let Some(t) = p["transcript"].as_str() {
                                parts.push(json!({"type": "text", "text": t}));
                            } else if let Some(b) = bytes {
                                let url = crate::openai_compat::named_data_url(
                                    "audio/wav",
                                    Some("speech.wav"),
                                    &crate::openai_compat::b64_encode(&wav(&b, rate)),
                                );
                                parts.push(json!({"type": "image_url", "image_url": {"url": url}}));
                            }
                        }
                        "output_audio" | "audio" => {
                            if let Some(t) = p["transcript"].as_str() {
                                parts.push(json!({"type": "text", "text": t}));
                            }
                        }
                        "input_image" => {
                            if let Some(u) = p["image_url"].as_str() {
                                parts.push(json!({"type": "image_url", "image_url": {"url": u}}));
                            }
                        }
                        _ => {}
                    }
                }
                if role == "assistant" {
                    let text: String = parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("");
                    out.push(json!({"role": "assistant", "content": text}));
                } else {
                    out.push(json!({"role": role, "content": parts}));
                }
            }
            "function_call" => out.push(json!({"role": "assistant", "content": null, "tool_calls": [{
                "id": it["call_id"], "type": "function",
                "function": {"name": it["name"], "arguments": it["arguments"].as_str().unwrap_or("{}")}}]})),
            "function_call_output" => out.push(json!({"role": "tool", "tool_call_id": it["call_id"], "content": it["output"]})),
            _ => {}
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn start_response(
    c: &mut Conn,
    opts: Value,
    conn: std::sync::Arc<tokio::sync::Mutex<Conn>>,
    tx: mpsc::UnboundedSender<Value>,
    app: AppState,
    principal: AuthPrincipal,
    headers: HeaderMap,
    sid: &str,
) {
    let g = |k: &str| {
        opts.get(k)
            .filter(|v| !v.is_null())
            .cloned()
            .or_else(|| c.session.get(k).cloned())
            .unwrap_or(Value::Null)
    };
    let modalities: Vec<String> = g("output_modalities")
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_else(|| vec!["audio".into()]);
    let audio_out = modalities.iter().any(|m| m == "audio");
    let instructions = g("instructions").as_str().unwrap_or("").to_string();
    let tools: Vec<Value> = g("tools")
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|t| t["type"] == "function")
        .map(|t| json!({"type": "function", "function": {"name": t["name"], "description": t["description"], "parameters": t.get("parameters").cloned().unwrap_or(json!({"type": "object"}))}}))
        .collect();
    let voice = c
        .session
        .pointer("/audio/output/voice")
        .and_then(|v| v.as_str())
        .unwrap_or("alloy")
        .to_string();
    let rate = c
        .session
        .pointer("/audio/output/format/rate")
        .and_then(|v| v.as_u64())
        .unwrap_or(24000);
    let out_fmt = c
        .session
        .pointer("/audio/output/format/type")
        .and_then(|v| v.as_str())
        .unwrap_or("audio/pcm")
        .to_string();
    // Out-of-band responses (conversation: "none") use only their own input.
    let messages = match (
        opts.get("conversation").and_then(|v| v.as_str()),
        opts.get("input").and_then(|v| v.as_array()),
    ) {
        (Some("none"), Some(inp)) => {
            let tmp = Conn::new(c.session.clone(), inp.clone(), c.limits);
            chat_messages(&tmp, &instructions)
        }
        _ => chat_messages(c, &instructions),
    };
    let expected = if audio_out {
        format!("realtime voice turn: answer the conversation's last message and attach your spoken answer as audio (WAV, 16-bit mono {rate} Hz; voice {voice}): `oh-seat reply 'what you say' -a answer.wav` (the text is the transcript); or call one of its tools")
    } else {
        "realtime text turn: answer the conversation's last message (`oh-seat reply '...'`), or call one of its tools".to_string()
    };
    let rid = uid("resp");
    let _ = tx.send(json!({"type": "response.created", "response": {"id": rid, "object": "realtime.response", "status": "in_progress", "output": [], "output_modalities": modalities}}));
    let sid = sid.to_string();
    let task = tokio::spawn(async move {
        let mut req = json!({
            "messages": messages,
            "stream": false,
            "metadata": {crate::api::openai::TASK_KEY: {"api": "realtime", "expected": expected, "files": audio_out, "session": sid}},
        });
        if !tools.is_empty() {
            req["tools"] = json!(tools);
        }
        let answer = match serde_json::from_value(req) {
            Ok(r) => {
                let resp =
                    crate::api::openai::chat_completions(State(app), principal, headers, Json(r))
                        .await;
                let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .unwrap_or_default();
                crate::api::task::answer_from_chat(&bytes).await
            }
            Err(e) => Err(e.to_string()),
        };
        let mut output = Vec::new();
        match answer {
            Err(e) => {
                let _ = tx.send(error_ev("server_error", "seat_unavailable", &e, None));
                let _ = tx.send(json!({"type": "response.done", "response": {"id": rid, "object": "realtime.response", "status": "failed", "output": [], "status_details": {"type": "failed", "error": {"message": e}}}}));
                conn.lock().await.running = None;
                return;
            }
            Ok(a) => {
                let mut c = conn.lock().await;
                let mut index = 0;
                if !a.text.is_empty() || audio_out {
                    let item_id = uid("item");
                    let part_kind = if audio_out {
                        "output_audio"
                    } else {
                        "output_text"
                    };
                    let mut item = json!({"id": item_id, "object": "realtime.item", "type": "message", "role": "assistant", "status": "in_progress", "content": []});
                    let _ = tx.send(json!({"type": "response.output_item.added", "response_id": rid, "output_index": index, "item": item}));
                    let prev = c
                        .items
                        .last()
                        .map(|i| i["id"].clone())
                        .unwrap_or(Value::Null);
                    let _ = tx.send(json!({"type": "conversation.item.added", "previous_item_id": prev, "item": item}));
                    let empty_part = if audio_out {
                        json!({"type": part_kind, "transcript": ""})
                    } else {
                        json!({"type": part_kind, "text": ""})
                    };
                    let _ = tx.send(json!({"type": "response.content_part.added", "response_id": rid, "item_id": item_id, "output_index": index, "content_index": 0, "part": empty_part}));
                    let base = json!({"response_id": rid, "item_id": item_id, "output_index": index, "content_index": 0});
                    let with = |mut v: Value, extra: Value| {
                        if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
                            for (k, x) in e {
                                o.insert(k.clone(), x.clone());
                            }
                        }
                        v
                    };
                    let part = if audio_out {
                        if !a.text.is_empty() {
                            let _ = tx.send(with(base.clone(), json!({"type": "response.output_audio_transcript.delta", "delta": a.text})));
                        }
                        let audio = a.files.iter().find_map(|o| match o {
                            crate::api::task::Output::Bytes { mime, bytes, .. }
                                if mime.starts_with("audio/")
                                    || mime == "application/octet-stream" =>
                            {
                                Some(bytes.clone())
                            }
                            _ => None,
                        });
                        if let Some(b) = audio {
                            let pcm = if out_fmt == "audio/pcm" {
                                wav_data(&b)
                            } else {
                                b
                            };
                            for chunk in pcm.chunks(rate as usize) {
                                let _ = tx.send(with(base.clone(), json!({"type": "response.output_audio.delta", "delta": crate::openai_compat::b64_encode(chunk)})));
                            }
                        }
                        let _ = tx.send(with(
                            base.clone(),
                            json!({"type": "response.output_audio.done"}),
                        ));
                        let _ = tx.send(with(base.clone(), json!({"type": "response.output_audio_transcript.done", "transcript": a.text})));
                        json!({"type": "output_audio", "transcript": a.text})
                    } else {
                        let _ = tx.send(with(
                            base.clone(),
                            json!({"type": "response.output_text.delta", "delta": a.text}),
                        ));
                        let _ = tx.send(with(
                            base.clone(),
                            json!({"type": "response.output_text.done", "text": a.text}),
                        ));
                        json!({"type": "output_text", "text": a.text})
                    };
                    let _ = tx.send(with(
                        base.clone(),
                        json!({"type": "response.content_part.done", "part": part}),
                    ));
                    item["content"] = json!([part]);
                    item["status"] = json!("completed");
                    let _ = tx.send(json!({"type": "response.output_item.done", "response_id": rid, "output_index": index, "item": item}));
                    let _ = tx.send(json!({"type": "conversation.item.done", "previous_item_id": prev, "item": item}));
                    c.items.push(item.clone());
                    output.push(item);
                    index += 1;
                }
                for call in &a.tool_calls {
                    let item_id = uid("item");
                    let call_id = call["id"].clone();
                    let name = call
                        .pointer("/function/name")
                        .cloned()
                        .unwrap_or(Value::Null);
                    let args = call
                        .pointer("/function/arguments")
                        .and_then(|v| v.as_str())
                        .unwrap_or("{}")
                        .to_string();
                    let mut item = json!({"id": item_id, "object": "realtime.item", "type": "function_call", "status": "in_progress", "call_id": call_id, "name": name, "arguments": ""});
                    let prev = c
                        .items
                        .last()
                        .map(|i| i["id"].clone())
                        .unwrap_or(Value::Null);
                    let _ = tx.send(json!({"type": "response.output_item.added", "response_id": rid, "output_index": index, "item": item}));
                    let _ = tx.send(json!({"type": "conversation.item.added", "previous_item_id": prev, "item": item}));
                    let _ = tx.send(json!({"type": "response.function_call_arguments.delta", "response_id": rid, "item_id": item_id, "output_index": index, "call_id": call_id, "delta": args}));
                    let _ = tx.send(json!({"type": "response.function_call_arguments.done", "response_id": rid, "item_id": item_id, "output_index": index, "call_id": call_id, "name": name, "arguments": args}));
                    item["arguments"] = json!(args);
                    item["status"] = json!("completed");
                    let _ = tx.send(json!({"type": "response.output_item.done", "response_id": rid, "output_index": index, "item": item}));
                    let _ = tx.send(json!({"type": "conversation.item.done", "previous_item_id": prev, "item": item}));
                    c.items.push(item.clone());
                    output.push(item);
                    index += 1;
                }
                c.running = None;
            }
        }
        let mut usage = Map::new();
        usage.insert("total_tokens".into(), json!(0));
        usage.insert("input_tokens".into(), json!(0));
        usage.insert("output_tokens".into(), json!(0));
        let _ = tx.send(json!({"type": "response.done", "response": {"id": rid, "object": "realtime.response", "status": "completed", "output": output, "usage": usage}}));
    });
    c.running = Some(task.abort_handle());
}

// ── shared by the WebSocket endpoints ──

/// The caller of a WebSocket: an API key or a client secret (header or the
/// `openai-insecure-api-key.` protocol), with the secret's preset session.
#[allow(clippy::result_large_err)] // the error is the HTTP response itself
async fn caller(
    app: &AppState,
    headers: &HeaderMap,
) -> Result<(AuthPrincipal, Option<Value>, Vec<String>), Response> {
    let protocols: Vec<String> = headers
        .get("sec-websocket-protocol")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').map(|p| p.trim().to_string()).collect())
        .unwrap_or_default();
    let key = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(|s| s.trim().to_string())
        .or_else(|| {
            protocols
                .iter()
                .find_map(|p| p.strip_prefix("openai-insecure-api-key.").map(String::from))
        });
    let unauthorized = |m: &str, code: &str| {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": {"message": m, "type": "invalid_request_error", "code": code}})),
        )
            .into_response()
    };
    let Some(key) = key else {
        return Err(unauthorized("Missing API key", "missing_api_key"));
    };
    if key.starts_with("ek_") {
        let g = ephemeral().lock().unwrap();
        return match g
            .get(&key)
            .filter(|e| e.expires_at > chrono::Utc::now().timestamp())
        {
            Some(e) => Ok((e.principal.clone(), Some(e.session.clone()), protocols)),
            None => Err(unauthorized(
                "Expired or unknown client secret",
                "invalid_api_key",
            )),
        };
    }
    match app.identity.resolve_principal(&key).await {
        Some(p) if p.api_key.scope.may_call() => Ok((p, None, protocols)),
        _ => Err(unauthorized("Invalid API key", "invalid_api_key")),
    }
}

/// Mint a client secret for a session.
fn mint(principal: AuthPrincipal, session: Value, secs: i64) -> (String, i64) {
    let value = format!("ek_{}", uuid::Uuid::new_v4().simple());
    let expires_at = chrono::Utc::now().timestamp() + secs;
    let mut g = ephemeral().lock().unwrap();
    let now = chrono::Utc::now().timestamp();
    g.retain(|_, e| e.expires_at > now);
    g.insert(
        value.clone(),
        Ephemeral {
            principal,
            session,
            expires_at,
        },
    );
    (value, expires_at)
}

fn expiry(b: &Value) -> i64 {
    b.pointer("/expires_after/seconds")
        .or_else(|| b.pointer("/client_secret/expires_at/seconds"))
        .and_then(|v| v.as_i64())
        .unwrap_or(600)
        .clamp(10, 7200)
}

/// One seat task outside a response (an input transcription, a
/// translated segment), in its own conversation per connection.
#[allow(clippy::too_many_arguments)]
async fn seat_task(
    app: AppState,
    principal: AuthPrincipal,
    headers: HeaderMap,
    api: &str,
    expected: String,
    files: bool,
    session: String,
    audio_url: String,
) -> Result<crate::api::task::Answer, String> {
    let req = json!({
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "(speech attached)"},
            {"type": "image_url", "image_url": {"url": audio_url}}]}],
        "stream": false,
        "metadata": {crate::api::openai::TASK_KEY: {"api": api, "expected": expected, "files": files, "session": session}},
    });
    let req = serde_json::from_value(req).map_err(|e| e.to_string())?;
    let resp =
        crate::api::openai::chat_completions(State(app), principal, headers, Json(req)).await;
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap_or_default();
    crate::api::task::answer_from_chat(&bytes).await
}

fn speech_url(pcm: &[u8], rate: u32) -> String {
    crate::openai_compat::named_data_url(
        "audio/wav",
        Some("speech.wav"),
        &crate::openai_compat::b64_encode(&wav(pcm, rate)),
    )
}

/// The first audio file of an answer.
fn answer_audio(a: &crate::api::task::Answer) -> Option<Vec<u8>> {
    a.files.iter().find_map(|o| match o {
        crate::api::task::Output::Bytes { mime, bytes, .. }
            if mime.starts_with("audio/") || mime == "application/octet-stream" =>
        {
            Some(bytes.clone())
        }
        _ => None,
    })
}

// ── input transcription and transcription sessions ──

fn transcription_session(cfg: &Value) -> Value {
    let mut s = json!({
        "type": "transcription",
        "object": "realtime.transcription_session",
        "id": uid("sess"),
        "audio": {
            "input": {
                "format": {"type": "audio/pcm", "rate": 24000},
                "transcription": {"model": "gpt-4o-transcribe", "language": null, "prompt": ""},
                "noise_reduction": null,
                "turn_detection": {"type": "server_vad", "threshold": 0.5, "prefix_padding_ms": 300,
                                   "silence_duration_ms": 500}
            }
        },
        "include": [],
    });
    merge(&mut s, cfg);
    s
}

/// A beta `transcription_session` body (flat fields) as a GA session update.
fn beta_transcription(b: &Value) -> Value {
    let mut input = Map::new();
    if let Some(t) = b.get("input_audio_transcription") {
        input.insert("transcription".into(), t.clone());
    }
    if let Some(t) = b.get("turn_detection") {
        input.insert("turn_detection".into(), t.clone());
    }
    if let Some(t) = b.get("input_audio_noise_reduction") {
        input.insert("noise_reduction".into(), t.clone());
    }
    if let Some(f) = b.get("input_audio_format").and_then(|v| v.as_str()) {
        let ty = match f {
            "g711_ulaw" => "audio/pcmu",
            "g711_alaw" => "audio/pcma",
            _ => "audio/pcm",
        };
        input.insert("format".into(), json!({"type": ty}));
    }
    let mut out = json!({"audio": {"input": input}});
    if let Some(i) = b.get("include") {
        out["include"] = i.clone();
    }
    out
}

/// A GA transcription session in the beta shape.
fn beta_view(s: &Value) -> Value {
    let i = s.pointer("/audio/input").cloned().unwrap_or(json!({}));
    let fmt = match i.pointer("/format/type").and_then(|v| v.as_str()) {
        Some("audio/pcmu") => "g711_ulaw",
        Some("audio/pcma") => "g711_alaw",
        _ => "pcm16",
    };
    json!({
        "id": s["id"], "object": "realtime.transcription_session",
        "modalities": ["text"], "input_audio_format": fmt,
        "input_audio_transcription": i["transcription"],
        "turn_detection": i["turn_detection"],
        "input_audio_noise_reduction": i["noise_reduction"],
        "include": s["include"],
    })
}

/// `POST /v1/realtime/transcription_sessions` (beta): a transcription
/// session and its client secret; connect with `?intent=transcription`.
pub async fn transcription_sessions(
    principal: AuthPrincipal,
    body: Option<Json<Value>>,
) -> Response {
    let b = body.map(|Json(b)| b).unwrap_or(json!({}));
    let session = transcription_session(&beta_transcription(&b));
    let (value, expires_at) = mint(principal, session.clone(), expiry(&b));
    let mut v = beta_view(&session);
    v["client_secret"] = json!({"value": value, "expires_at": expires_at});
    Json(v).into_response()
}

/// Transcribe a committed user turn when the session asks for it, and
/// report it as `conversation.item.input_audio_transcription.*`.
#[allow(clippy::too_many_arguments)]
fn transcribe(
    c: &Conn,
    item_id: &str,
    conn: std::sync::Arc<tokio::sync::Mutex<Conn>>,
    tx: mpsc::UnboundedSender<Value>,
    app: &AppState,
    principal: &AuthPrincipal,
    headers: &HeaderMap,
    sid: &str,
) {
    let t = c
        .session
        .pointer("/audio/input/transcription")
        .cloned()
        .unwrap_or(Value::Null);
    if t.is_null() && c.session["type"] != "transcription" {
        return;
    }
    let Some(pcm) = c.audio.get(item_id) else {
        return;
    };
    let (_, rate) = input_format(&c.session);
    let mut expected = match t.get("language").and_then(|l| l.as_str()) {
        Some(l) if !l.is_empty() => {
            format!("realtime input transcription: transcribe the attached audio (language {l})")
        }
        _ => "realtime input transcription: transcribe the attached audio".to_string(),
    };
    expected.push_str("; answer with the text alone");
    if let Some(p) = t
        .get("prompt")
        .and_then(|p| p.as_str())
        .filter(|p| !p.is_empty())
    {
        expected.push_str(&format!("; context from the caller: {p}"));
    }
    let url = speech_url(pcm, rate);
    let (app, principal, headers) = (app.clone(), principal.clone(), headers.clone());
    let (item_id, session) = (item_id.to_string(), format!("{sid}.transcribe"));
    tokio::spawn(async move {
        let a = seat_task(
            app,
            principal,
            headers,
            "realtime.transcription",
            expected,
            false,
            session,
            url,
        )
        .await;
        match a {
            Ok(a) => {
                let text = a.text.trim().to_string();
                let _ = tx.send(json!({"type": "conversation.item.input_audio_transcription.delta", "item_id": item_id, "content_index": 0, "delta": text}));
                let _ = tx.send(json!({"type": "conversation.item.input_audio_transcription.completed", "item_id": item_id, "content_index": 0, "transcript": text,
                    "usage": {"type": "duration", "seconds": 0}}));
                let mut c = conn.lock().await;
                if let Some(it) = c.items.iter_mut().find(|i| i["id"] == json!(item_id)) {
                    it["content"][0]["transcript"] = json!(text);
                }
            }
            Err(e) => {
                let _ = tx.send(json!({"type": "conversation.item.input_audio_transcription.failed", "item_id": item_id, "content_index": 0,
                    "error": {"type": "server_error", "code": "seat_unavailable", "message": e}}));
            }
        }
    });
}

// ── realtime translation ──

fn translation_session(model: &str, cfg: &Value) -> Value {
    let mut s = json!({
        "type": "translation",
        "id": uid("sess"),
        "model": model,
        "expires_at": 0,
        "audio": {"input": {"transcription": null, "noise_reduction": null}, "output": {"language": "en"}},
    });
    merge(&mut s, cfg);
    s
}

/// `POST /v1/realtime/translations/client_secrets`
pub async fn translation_client_secrets(
    principal: AuthPrincipal,
    body: Option<Json<Value>>,
) -> Response {
    let b = body.map(|Json(b)| b).unwrap_or(json!({}));
    let cfg = b.get("session").cloned().unwrap_or(json!({}));
    let model = cfg["model"]
        .as_str()
        .unwrap_or("gpt-realtime-translate")
        .to_string();
    let mut session = translation_session(&model, &cfg);
    let (value, expires_at) = mint(principal, session.clone(), expiry(&b));
    session["expires_at"] = json!(expires_at);
    Json(json!({"value": value, "expires_at": expires_at, "session": session})).into_response()
}

/// `GET /v1/realtime/translations` (WebSocket): source audio in, the
/// translation out as speech and transcript. Each pause (or 15 s of
/// speech) closes a segment, which becomes one seat task; segments are
/// answered in order.
pub async fn translations(
    ws: WebSocketUpgrade,
    State(app): State<AppState>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<RtQuery>,
) -> Response {
    let (principal, preset, protocols) = match caller(&app, &headers).await {
        Ok(r) => r,
        Err(e) => return e,
    };
    let model = q.model.unwrap_or_else(|| "gpt-realtime-translate".into());
    let session = match preset {
        Some(p) if p["type"] != "translation" => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": {"message": "This client secret is not for a translation session", "type": "invalid_request_error", "code": "invalid_session_type"}}))).into_response()
        }
        Some(p) => p,
        None => translation_session(&model, &json!({})),
    };
    let ws = if protocols.iter().any(|p| p == "realtime") {
        ws.protocols(["realtime"])
    } else {
        ws
    };
    ws.on_upgrade(move |socket| run_translation(socket, app, principal, headers, session))
}

async fn run_translation(
    socket: WebSocket,
    app: AppState,
    principal: AuthPrincipal,
    headers: HeaderMap,
    mut session: Value,
) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        while let Some(mut ev) = rx.recv().await {
            if ev.get("event_id").is_none() {
                ev["event_id"] = json!(uid("event"));
            }
            if sink.send(WsMessage::Text(ev.to_string())).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });
    let sid = session["id"].as_str().unwrap_or("").to_string();
    let _ = tx.send(json!({"type": "session.created", "session": session}));
    // Segments go to one worker so the output keeps the speaker's order.
    let (seg_tx, mut seg_rx) = mpsc::unbounded_channel::<(Vec<u8>, u64, Value)>();
    let worker = {
        let tx = tx.clone();
        tokio::spawn(async move {
            while let Some((pcm, end_ms, s)) = seg_rx.recv().await {
                let lang = s
                    .pointer("/audio/output/language")
                    .and_then(|v| v.as_str())
                    .unwrap_or("en")
                    .to_string();
                let heard = !s
                    .pointer("/audio/input/transcription")
                    .cloned()
                    .unwrap_or(Value::Null)
                    .is_null();
                let mut expected = format!("realtime translation: translate the attached speech into {lang} and attach the translation spoken as audio (WAV, 16-bit mono 24000 Hz): `oh-seat reply 'the translation' -a out.wav` (the text is the translation's transcript)");
                if heard {
                    expected.push_str("; start the text with a line `heard: <what the speaker said, in their own language>`");
                }
                let a = seat_task(
                    app.clone(),
                    principal.clone(),
                    headers.clone(),
                    "realtime.translation",
                    expected,
                    true,
                    format!("{sid}.translate"),
                    speech_url(&pcm, 24000),
                )
                .await;
                match a {
                    Ok(a) => {
                        let mut text = a.text.trim().to_string();
                        if let Some(rest) = text.strip_prefix("heard:") {
                            let (h, t) = rest.split_once('\n').unwrap_or((rest, ""));
                            let _ = tx.send(json!({"type": "session.input_transcript.delta", "delta": h.trim(), "elapsed_ms": end_ms}));
                            text = t.trim().to_string();
                        }
                        if !text.is_empty() {
                            let _ = tx.send(json!({"type": "session.output_transcript.delta", "delta": text, "elapsed_ms": end_ms}));
                        }
                        if let Some(b) = answer_audio(&a) {
                            for chunk in wav_data(&b).chunks(24000) {
                                let _ = tx.send(json!({"type": "session.output_audio.delta", "delta": crate::openai_compat::b64_encode(chunk),
                                    "format": "pcm16", "sample_rate": 24000, "channels": 1, "elapsed_ms": end_ms}));
                            }
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(error_ev("server_error", "seat_unavailable", &e, None));
                    }
                }
            }
        })
    };
    let (mut buffer, mut total, mut speaking, mut quiet) =
        (Vec::<u8>::new(), 0usize, false, 0usize);
    let mut closed = false;
    while let Some(Ok(msg)) = stream.next().await {
        let text = match msg {
            WsMessage::Text(t) => t,
            WsMessage::Close(_) => break,
            _ => continue,
        };
        let Ok(ev) = serde_json::from_str::<Value>(&text) else {
            let _ = tx.send(error_ev(
                "invalid_request_error",
                "invalid_json",
                "event is not JSON",
                None,
            ));
            continue;
        };
        let eid = ev.get("event_id");
        match ev["type"].as_str().unwrap_or("") {
            "session.update" => {
                let upd = ev.get("session").cloned().unwrap_or(json!({}));
                merge(&mut session, &upd);
                let _ = tx.send(json!({"type": "session.updated", "session": session}));
            }
            "session.input_audio_buffer.append" => {
                let Some(b) = ev["audio"]
                    .as_str()
                    .and_then(crate::openai_compat::b64_decode)
                else {
                    let _ = tx.send(error_ev(
                        "invalid_request_error",
                        "invalid_audio",
                        "audio must be base64",
                        eid,
                    ));
                    continue;
                };
                total += b.len();
                buffer.extend_from_slice(&b);
                if pcm_level(&b) > 0.05 {
                    speaking = true;
                    quiet = 0;
                } else if speaking {
                    quiet += b.len();
                }
                // 500 ms of quiet after speech, or 15 s of audio, ends a segment.
                if (speaking && quiet >= 500 * 48) || buffer.len() >= 15 * 48_000 {
                    if speaking || quiet < buffer.len() {
                        let _ = seg_tx.send((
                            std::mem::take(&mut buffer),
                            (total / 48) as u64,
                            session.clone(),
                        ));
                    }
                    buffer.clear();
                    speaking = false;
                    quiet = 0;
                }
            }
            "session.close" => {
                closed = true;
                break;
            }
            other => {
                let _ = tx.send(error_ev(
                    "invalid_request_error",
                    "unknown_event",
                    &format!("unsupported event type {other}"),
                    eid,
                ));
            }
        }
    }
    if closed {
        // Finish what was said before the close.
        if speaking && !buffer.is_empty() {
            let _ = seg_tx.send((
                std::mem::take(&mut buffer),
                (total / 48) as u64,
                session.clone(),
            ));
        }
        drop(seg_tx);
        let _ = worker.await;
        let _ = tx.send(json!({"type": "session.closed"}));
    } else {
        worker.abort();
    }
    drop(tx);
    let _ = writer.await;
}

// ── SIP / WebRTC calls ──

/// `/v1/realtime/calls*`: calls need a WebRTC or SIP media stack, which
/// this server does not have; the WebSocket carries the same sessions.
pub async fn calls() -> Response {
    crate::api::task::error(
        StatusCode::NOT_IMPLEMENTED,
        "invalid_request_error",
        "not_implemented",
        "Realtime calls (WebRTC and SIP) are not supported by this server; connect over the WebSocket at /v1/realtime instead.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small(buffer: usize, audio: usize, items: usize) -> Conn {
        Conn::new(
            default_session("m"),
            Vec::new(),
            Limits {
                buffer_bytes: buffer,
                audio_bytes: audio,
                items,
            },
        )
    }

    #[test]
    fn input_buffer_is_bounded() {
        let mut c = small(10, 100, 10);
        assert!(append_audio(&mut c, &[0; 6], None).is_ok());
        let e = append_audio(&mut c, &[0; 6], Some(&json!("ev1"))).unwrap_err();
        assert_eq!(e["type"], "error");
        assert_eq!(e["error"]["code"], "input_audio_buffer_too_large");
        assert_eq!(e["error"]["event_id"], "ev1");
        assert_eq!(c.buffer.len(), 6, "a refused append changes nothing");
        assert!(append_audio(&mut c, &[0; 4], None).is_ok());
    }

    #[test]
    fn committed_audio_is_bounded_and_freed_on_delete() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut c = small(100, 10, 10);
        append_audio(&mut c, &[1; 8], None).unwrap();
        let id = commit(&mut c, &tx, None).unwrap().unwrap();
        assert_eq!(c.audio_bytes, 8);
        append_audio(&mut c, &[1; 4], None).unwrap();
        let e = commit(&mut c, &tx, None).unwrap_err();
        assert_eq!(e["error"]["code"], "conversation_audio_limit_exceeded");
        assert_eq!(c.buffer.len(), 4, "a refused commit keeps the buffer");
        assert!(remove_item(&mut c, &id));
        assert_eq!(c.audio_bytes, 0);
        assert!(commit(&mut c, &tx, None).unwrap().is_some());
    }

    #[test]
    fn items_are_bounded() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut c = small(100, 100, 2);
        add_item(
            &mut c,
            json!({"type": "message", "role": "user", "content": []}),
            None,
            &tx,
        );
        assert!(items_full(&c, None).is_none());
        add_item(
            &mut c,
            json!({"type": "message", "role": "user", "content": []}),
            None,
            &tx,
        );
        let e = items_full(&c, None).unwrap();
        assert_eq!(e["error"]["code"], "conversation_items_limit_exceeded");
        append_audio(&mut c, &[1; 2], None).unwrap();
        assert!(commit(&mut c, &tx, None).is_err(), "commit adds an item");
    }

    #[test]
    fn limits_default_and_env_override() {
        let d = Limits::from_env();
        assert!(d.buffer_bytes > 0 && d.audio_bytes > 0 && d.items > 0);
        assert_eq!(
            inline_audio_len(&json!({"content": [{"type": "input_audio", "audio": "AAAA"}]})),
            3
        );
    }

    #[test]
    fn vad_byte_math_follows_the_format() {
        let mut s = default_session("m");
        assert_eq!(bytes_per_ms(&s), 48);
        merge(
            &mut s,
            &json!({"audio": {"input": {"format": {"type": "audio/pcm", "rate": 16000}}}}),
        );
        assert_eq!(bytes_per_ms(&s), 32);
        // A G.711 format keeps a stale `rate` from the pcm default; it is 8 kHz.
        merge(
            &mut s,
            &json!({"audio": {"input": {"format": {"type": "audio/pcmu"}}}}),
        );
        assert_eq!(input_format(&s), (InFormat::Ulaw, 8000));
        assert_eq!(bytes_per_ms(&s), 8);
        merge(
            &mut s,
            &json!({"audio": {"input": {"format": {"type": "audio/pcma"}}}}),
        );
        assert_eq!(input_format(&s), (InFormat::Alaw, 8000));
    }

    #[test]
    fn g711_is_decoded_before_loudness() {
        // Silence in G.711 is 0xff (mu-law) / 0xd5 (A-law): as raw bytes it
        // would look loud; decoded it is quiet.
        assert!(input_level(InFormat::Ulaw, &[0xff; 160]) < 0.01);
        assert!(input_level(InFormat::Alaw, &[0xd5; 160]) < 0.01);
        assert!(
            pcm_level(&[0xd5; 160]) > 0.3,
            "raw A-law silence reads loud"
        );
        // Full-scale codes decode loud.
        assert!(input_level(InFormat::Ulaw, &[0x00, 0x80].repeat(80)) > 0.5);
        assert!(input_level(InFormat::Alaw, &[0x2a, 0xaa].repeat(80)) > 0.5);
        // Reference points of the G.711 tables.
        assert_eq!(ulaw_to_pcm(0x00), -32124);
        assert_eq!(ulaw_to_pcm(0x80), 32124);
        assert_eq!(ulaw_to_pcm(0xff), 0);
        assert_eq!(alaw_to_pcm(0xd5), 8);
        assert_eq!(alaw_to_pcm(0x2a), -32256);
        assert_eq!(to_pcm16(InFormat::Ulaw, &[0xff]).len(), 2);
    }

    #[test]
    fn wav_round_trip() {
        let pcm = vec![1u8, 2, 3, 4];
        let w = wav(&pcm, 24000);
        assert_eq!(&w[..4], b"RIFF");
        assert_eq!(wav_data(&w), pcm);
        assert_eq!(wav_data(b"raw"), b"raw".to_vec());
    }

    #[test]
    fn beta_transcription_maps_to_ga() {
        let s = transcription_session(&beta_transcription(&json!({
            "input_audio_format": "g711_ulaw",
            "input_audio_transcription": {"model": "whisper-1", "language": "zh"}
        })));
        assert_eq!(s["type"], "transcription");
        assert_eq!(s["audio"]["input"]["format"]["type"], "audio/pcmu");
        let v = beta_view(&s);
        assert_eq!(v["input_audio_format"], "g711_ulaw");
        assert_eq!(v["input_audio_transcription"]["language"], "zh");
    }

    #[test]
    fn session_updates_merge() {
        let mut s = default_session("m");
        merge(
            &mut s,
            &json!({"instructions": "hi", "audio": {"output": {"voice": "echo"}}}),
        );
        assert_eq!(s["instructions"], "hi");
        assert_eq!(s["audio"]["output"]["voice"], "echo");
        assert_eq!(s["audio"]["output"]["format"]["rate"], 24000);
        merge(
            &mut s,
            &json!({"type": "transcription", "audio": {"input": {"format": {"type": "audio/pcmu"}}}}),
        );
        assert_eq!(s["type"], "realtime");
        assert_eq!(s["audio"]["input"]["format"]["type"], "audio/pcmu");
    }
}
