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
    match (into, from) {
        (Value::Object(a), Value::Object(b)) => {
            for (k, v) in b {
                if k == "type" || k == "id" || k == "object" {
                    continue;
                }
                match a.get_mut(k) {
                    Some(x) if x.is_object() && v.is_object() => merge(x, v),
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
    let mut session = default_session(&model);
    merge(&mut session, &cfg);
    let value = format!("ek_{}", uuid::Uuid::new_v4().simple());
    let expires_at = chrono::Utc::now().timestamp() + secs;
    {
        let mut g = ephemeral().lock().unwrap();
        let now = chrono::Utc::now().timestamp();
        g.retain(|_, e| e.expires_at > now);
        g.insert(
            value.clone(),
            Ephemeral {
                principal,
                session: session.clone(),
                expires_at,
            },
        );
    }
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
    call_id: Option<String>,
}

pub async fn realtime(
    ws: WebSocketUpgrade,
    State(app): State<AppState>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<RtQuery>,
) -> Response {
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
    let Some(key) = key else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": {"message": "Missing API key", "code": "missing_api_key"}})),
        )
            .into_response();
    };
    let _ = q.call_id;
    let (principal, preset) = if key.starts_with("ek_") {
        let g = ephemeral().lock().unwrap();
        match g.get(&key).filter(|e| e.expires_at > chrono::Utc::now().timestamp()) {
            Some(e) => (e.principal.clone(), Some(e.session.clone())),
            None => {
                return (StatusCode::UNAUTHORIZED, Json(json!({"error": {"message": "Expired or unknown client secret", "code": "invalid_api_key"}}))).into_response()
            }
        }
    } else {
        match app.identity.resolve_principal(&key).await {
            Some(p) if p.api_key.scope.may_call() => (p, None),
            _ => {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(
                        json!({"error": {"message": "Invalid API key", "code": "invalid_api_key"}}),
                    ),
                )
                    .into_response()
            }
        }
    };
    let model = q.model.unwrap_or_else(|| "gpt-realtime".into());
    let mut session = default_session(&model);
    if let Some(p) = preset {
        merge(&mut session, &p);
    }
    let ws = if protocols.iter().any(|p| p == "realtime") {
        ws.protocols(["realtime"])
    } else {
        ws
    };
    ws.on_upgrade(move |socket| run(socket, app, principal, headers, session))
}

/// What a connection holds.
struct Conn {
    session: Value,
    items: Vec<Value>,
    /// Committed audio per user item id (pcm16 or the session's format).
    audio: HashMap<String, Vec<u8>>,
    buffer: Vec<u8>,
    /// Server VAD: whether speech is under way, and quiet bytes since.
    speaking: bool,
    quiet: usize,
    running: Option<tokio::task::AbortHandle>,
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
    let conn = std::sync::Arc::new(tokio::sync::Mutex::new(Conn {
        session,
        items: Vec::new(),
        audio: HashMap::new(),
        buffer: Vec::new(),
        speaking: false,
        quiet: 0,
        running: None,
    }));
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

fn commit(c: &mut Conn, tx: &mpsc::UnboundedSender<Value>) -> Option<String> {
    if c.buffer.is_empty() {
        return None;
    }
    let pcm = std::mem::take(&mut c.buffer);
    let prev = c
        .items
        .last()
        .map(|i| i["id"].clone())
        .unwrap_or(Value::Null);
    let item_id = uid("item");
    let _ = tx.send(json!({"type": "input_audio_buffer.committed", "previous_item_id": prev, "item_id": item_id}));
    c.audio.insert(item_id.clone(), pcm);
    add_item(
        c,
        json!({"id": item_id, "type": "message", "role": "user",
               "content": [{"type": "input_audio", "transcript": null}]}),
        None,
        tx,
    );
    Some(item_id)
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
            c.buffer.extend_from_slice(&b);
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
                let pcm = c
                    .session
                    .pointer("/audio/input/format/type")
                    .and_then(|t| t.as_str())
                    .unwrap_or("audio/pcm")
                    == "audio/pcm";
                let threshold = td.get("threshold").and_then(|v| v.as_f64()).unwrap_or(0.5) * 0.1;
                let silence_ms = td
                    .get("silence_duration_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(500) as usize;
                let loud = !pcm || pcm_level(&b) > threshold;
                if loud {
                    c.quiet = 0;
                    if !c.speaking {
                        c.speaking = true;
                        let at = (c.buffer.len() / 48) as u64;
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
                    if c.quiet >= silence_ms * 48 {
                        c.speaking = false;
                        c.quiet = 0;
                        let at = (c.buffer.len() / 48) as u64;
                        let _ = tx.send(json!({"type": "input_audio_buffer.speech_stopped", "audio_end_ms": at, "item_id": Value::Null}));
                        let committed = commit(&mut c, tx);
                        let create = td
                            .get("create_response")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(true);
                        if committed.is_some() && create {
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
        "input_audio_buffer.commit" => {
            if commit(&mut c, tx).is_none() {
                let _ = tx.send(error_ev(
                    "invalid_request_error",
                    "input_audio_buffer_commit_empty",
                    "buffer is empty",
                    eid,
                ));
            }
        }
        "input_audio_buffer.clear" => {
            c.buffer.clear();
            c.speaking = false;
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
            let before = c.items.len();
            c.items.retain(|i| i["id"] != json!(id));
            c.audio.remove(&id);
            if c.items.len() < before {
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
    let rate = c
        .session
        .pointer("/audio/input/format/rate")
        .and_then(|r| r.as_u64())
        .unwrap_or(24000) as u32;
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
                                .or_else(|| p["audio"].as_str().and_then(crate::openai_compat::b64_decode));
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
            let tmp = Conn {
                session: c.session.clone(),
                items: inp.clone(),
                audio: HashMap::new(),
                buffer: Vec::new(),
                speaking: false,
                quiet: 0,
                running: None,
            };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_round_trip() {
        let pcm = vec![1u8, 2, 3, 4];
        let w = wav(&pcm, 24000);
        assert_eq!(&w[..4], b"RIFF");
        assert_eq!(wav_data(&w), pcm);
        assert_eq!(wav_data(b"raw"), b"raw".to_vec());
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
    }
}
