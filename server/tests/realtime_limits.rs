//! Realtime WebSocket per-connection bounds (audit M9) and VAD byte math
//! for G.711 sessions (audit L10), over a real socket. The bounds come
//! from env, so this file is its own test binary.

use futures::{SinkExt, StreamExt};
use omni2api_server::build_router;
use omni2api_server::storage::memory::MemoryStore;
use omni2api_server::storage::AppState;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

const KEY: &str = "ohk_demo_alice_codex";

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect() -> Ws {
    std::env::set_var("OMNI2API_REALTIME_MAX_BUFFER_BYTES", "1000");
    std::env::set_var("OMNI2API_REALTIME_MAX_ITEMS", "2");
    let app = build_router(AppState::from_store(MemoryStore::seeded()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let mut req = format!("ws://{addr}/v1/realtime?model=gpt-realtime")
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {KEY}").parse().unwrap());
    let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    ws
}

async fn next(ws: &mut Ws) -> Value {
    loop {
        let m = tokio::time::timeout(std::time::Duration::from_secs(10), ws.next())
            .await
            .expect("an event in time")
            .expect("open")
            .expect("ok");
        if let Message::Text(t) = m {
            return serde_json::from_str(&t).unwrap();
        }
    }
}

/// Events until one of `kind` arrives.
async fn until(ws: &mut Ws, kind: &str) -> Value {
    loop {
        let v = next(ws).await;
        if v["type"] == kind {
            return v;
        }
    }
}

async fn send(ws: &mut Ws, v: Value) {
    ws.send(Message::Text(v.to_string())).await.unwrap();
}

fn b64(b: &[u8]) -> String {
    omni2api_server::openai_compat::b64_encode(b)
}

#[tokio::test]
async fn realtime_bounds_and_g711_vad() {
    let mut ws = connect().await;
    assert_eq!(next(&mut ws).await["type"], "session.created");

    // M9: the input buffer refuses an append past its bound, then recovers.
    send(&mut ws, json!({"type": "session.update", "session": {"audio": {"input": {"turn_detection": null}}}})).await;
    until(&mut ws, "session.updated").await;
    send(
        &mut ws,
        json!({"type": "input_audio_buffer.append", "audio": b64(&[0; 800])}),
    )
    .await;
    send(
        &mut ws,
        json!({"event_id": "big", "type": "input_audio_buffer.append", "audio": b64(&[0; 800])}),
    )
    .await;
    let e = until(&mut ws, "error").await;
    assert_eq!(e["error"]["code"], "input_audio_buffer_too_large", "{e}");
    assert_eq!(e["error"]["event_id"], "big");
    send(&mut ws, json!({"type": "input_audio_buffer.clear"})).await;
    until(&mut ws, "input_audio_buffer.cleared").await;

    // M9: the conversation refuses items past its bound.
    for _ in 0..2 {
        send(&mut ws, json!({"type": "conversation.item.create", "item": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}})).await;
        until(&mut ws, "conversation.item.done").await;
    }
    send(&mut ws, json!({"event_id": "third", "type": "conversation.item.create", "item": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}})).await;
    let e = until(&mut ws, "error").await;
    assert_eq!(
        e["error"]["code"], "conversation_items_limit_exceeded",
        "{e}"
    );
    send(
        &mut ws,
        json!({"event_id": "resp", "type": "response.create"}),
    )
    .await;
    let e = until(&mut ws, "error").await;
    assert_eq!(
        e["error"]["code"], "conversation_items_limit_exceeded",
        "{e}"
    );
    assert_eq!(e["error"]["event_id"], "resp");

    // L10: a mu-law session under server VAD measures time at 8 bytes/ms
    // and judges loudness on decoded samples.
    send(&mut ws, json!({"type": "session.update", "session": {"audio": {"input": {
        "format": {"type": "audio/pcmu"},
        "turn_detection": {"type": "server_vad", "threshold": 0.5, "silence_duration_ms": 50, "create_response": false}}}}})).await;
    until(&mut ws, "session.updated").await;
    // 0xff is mu-law silence: no speech.
    send(
        &mut ws,
        json!({"type": "input_audio_buffer.append", "audio": b64(&[0xff; 80])}),
    )
    .await;
    // 10 ms of loud mu-law (80 bytes at 8 kHz).
    send(
        &mut ws,
        json!({"type": "input_audio_buffer.append", "audio": b64(&[0x00, 0x80].repeat(40))}),
    )
    .await;
    let started = until(&mut ws, "input_audio_buffer.speech_started").await;
    assert_eq!(started["audio_start_ms"], json!(20), "{started}");
    // 50 ms of silence (400 bytes) ends the turn: 80 + 80 + 400 bytes = 70 ms.
    send(
        &mut ws,
        json!({"type": "input_audio_buffer.append", "audio": b64(&[0xff; 400])}),
    )
    .await;
    let stopped = until(&mut ws, "input_audio_buffer.speech_stopped").await;
    assert_eq!(stopped["audio_end_ms"], json!(70), "{stopped}");
    // The commit that follows is refused: the conversation is full.
    let e = until(&mut ws, "error").await;
    assert_eq!(
        e["error"]["code"], "conversation_items_limit_exceeded",
        "{e}"
    );
}
