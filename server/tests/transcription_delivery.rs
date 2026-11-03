//! Slow-path delivery per response format (M2): behind a tunnel that
//! kills responses silent for ~100 s, JSON-format transcriptions keep
//! the padded hold (padding is valid leading whitespace), while raw
//! formats (text/srt/vtt) skip it because padding would corrupt them.
//! With the hold at zero the choice is observable: a padded response's
//! headers arrive before the seat answers; a raw one waits.

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

async fn cursor(client: &reqwest::Client, addr: &str) -> u64 {
    let r: Value = client
        .get(format!("http://{addr}/v1/seat/inbox?wait=0"))
        .header("Authorization", format!("Bearer {KEY_A}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    r.get("latest")
        .or_else(|| r.get("cursor"))
        .and_then(|c| c.as_u64().or_else(|| c.as_str()?.parse().ok()))
        .unwrap_or(0)
}

async fn next_conv(client: &reqwest::Client, addr: &str, after: u64) -> String {
    for _ in 0..40 {
        let r: Value = client
            .get(format!("http://{addr}/v1/seat/inbox?wait=1&after={after}"))
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
            return c;
        }
    }
    panic!("seat never saw the task");
}

async fn seat_answer(client: &reqwest::Client, addr: &str, conv: &str, text: &str) {
    let r = client
        .post(format!("http://{addr}/v1/seat/reply"))
        .header("Authorization", format!("Bearer {KEY_A}"))
        .json(&serde_json::json!({
            "conversation_id": conv, "text": text, "end_turn": true,
            "client_msg_id": format!("ohs_tr{}", uuid::Uuid::new_v4().simple()),
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
}

fn multipart(format: &str) -> (String, String) {
    let boundary = format!("trboundary{format}");
    let body = format!(
        "--{b}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nwhisper-1\r\n\
         --{b}\r\nContent-Disposition: form-data; name=\"response_format\"\r\n\r\n{f}\r\n\
         --{b}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"clip.m4a\"\r\n\
         Content-Type: audio/mp4\r\n\r\nFAKEM4A\r\n--{b}--\r\n",
        b = boundary,
        f = format
    );
    (boundary, body)
}

#[tokio::test]
async fn transcription_padding_matches_response_format() {
    // Hold at zero: the seat is "slow", so the delivery choice decides
    // whether headers can leave before the answer exists.
    std::env::set_var("OMNI2API_TASK_HOLD_SECS", "0");
    let app = build_router(AppState::from_store(MemoryStore::seeded()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = reqwest::Client::new();

    // ── json: the padded path — headers arrive before the seat answers ──
    let (boundary, body) = multipart("json");
    let a2 = addr.clone();
    let c2 = client.clone();
    let mut req = tokio::spawn(async move {
        c2.post(format!("http://{a2}/v1/audio/transcriptions"))
            .header("Authorization", format!("Bearer {KEY_A}"))
            .header(
                "Content-Type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(body)
            .send()
            .await
            .unwrap()
    });
    // No seat answer yet: the response must still arrive (padded stream).
    let resp = tokio::time::timeout(std::time::Duration::from_secs(5), &mut req)
        .await
        .expect("json transcription keeps the padded slow path: headers arrive without an answer")
        .unwrap();
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert_eq!(resp.status().as_u16(), 200);
    assert!(
        ct.starts_with("application/json"),
        "json stays json, got {ct}"
    );
    // Now the seat answers; the payload (after any padding) is the JSON.
    let conv = next_conv(&client, &addr, 0).await;
    seat_answer(&client, &addr, &conv, "the json transcript").await;
    let body = resp.text().await.unwrap();
    let v: Value = serde_json::from_str(body.trim_start())
        .unwrap_or_else(|e| panic!("padded body is still JSON: {e}: {body:?}"));
    assert_eq!(v["text"], "the json transcript", "{v}");

    // ── text: the raw path — nothing leaves before the seat answers ──
    let cur = cursor(&client, &addr).await;
    let (boundary, body) = multipart("text");
    let a2 = addr.clone();
    let c2 = client.clone();
    let mut req = tokio::spawn(async move {
        c2.post(format!("http://{a2}/v1/audio/transcriptions"))
            .header("Authorization", format!("Bearer {KEY_A}"))
            .header(
                "Content-Type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(body)
            .send()
            .await
            .unwrap()
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(2), &mut req)
            .await
            .is_err(),
        "a raw text transcription must wait for the answer, never padding"
    );
    let conv = next_conv(&client, &addr, cur).await;
    seat_answer(&client, &addr, &conv, "the raw transcript").await;
    let resp = req.await.unwrap();
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert_eq!(resp.status().as_u16(), 200);
    assert!(ct.starts_with("text/plain"), "raw text as text, got {ct}");
    let body = resp.text().await.unwrap();
    assert_eq!(body, "the raw transcript", "exact bytes, no padding");
}
