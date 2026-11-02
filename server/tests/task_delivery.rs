//! Delivery correctness: raw-media answers keep their real status and
//! content type no matter how long the seat takes (the JSON slow path
//! commits padding headers, which raw bytes must never share).

use omni2api_server::build_router;
use omni2api_server::storage::memory::MemoryStore;
use omni2api_server::storage::AppState;
use serde_json::Value;

const KEY_A: &str = "ohk_demo_alice_codex";

fn all_convs(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            if let Some(Value::String(s)) = m.get("conversation_id") {
                out.push(s.clone());
            }
            for x in m.values() {
                all_convs(x, out);
            }
        }
        Value::Array(a) => {
            for x in a {
                all_convs(x, out);
            }
        }
        _ => {}
    }
}

#[tokio::test]
async fn slow_speech_answer_keeps_audio_headers() {
    // Even with the status hold at zero (the seat is "slow"), a speech
    // answer must arrive as audio, not as padded JSON.
    std::env::set_var("OMNI2API_TASK_HOLD_SECS", "0");
    let app = build_router(AppState::from_store(MemoryStore::seeded()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = reqwest::Client::new();

    let a2 = addr.clone();
    let client2 = client.clone();
    let req = tokio::spawn(async move {
        client2
            .post(format!("http://{a2}/v1/audio/speech"))
            .header("Authorization", format!("Bearer {KEY_A}"))
            .json(&serde_json::json!({"model": "gpt-4o-mini-tts", "voice": "alloy", "input": "hello"}))
            .send()
            .await
            .unwrap()
    });

    let mut conv = None;
    for _ in 0..40 {
        let r: Value = client
            .get(format!("http://{addr}/v1/seat/inbox?wait=1"))
            .header("Authorization", format!("Bearer {KEY_A}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let mut convs = Vec::new();
        all_convs(&r, &mut convs);
        if let Some(c) = convs.into_iter().next() {
            conv = Some(c);
            break;
        }
    }
    let conv = conv.expect("seat saw the task");
    let audio = b"FAKEMP3AUDIODATA".to_vec();
    let r = client
        .post(format!("http://{addr}/v1/seat/reply"))
        .header("Authorization", format!("Bearer {KEY_A}"))
        .json(&serde_json::json!({
            "conversation_id": conv, "text": "", "end_turn": true,
            "client_msg_id": format!("ohs_slow{}", uuid::Uuid::new_v4().simple()),
            "attachments": [{"name": "answer.mp3", "data": "RkFLRU1QM0FVRElPREFUQQ=="}],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);

    let resp = req.await.unwrap();
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let status = resp.status().as_u16();
    let body = resp.bytes().await.unwrap().to_vec();
    assert_eq!(status, 200);
    assert!(ct.starts_with("audio/"), "audio answer as audio, got {ct}");
    assert_eq!(body, audio, "exact bytes, no padding");
}
