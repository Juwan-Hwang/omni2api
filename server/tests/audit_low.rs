//! Regression tests for the LOW findings of the audit of 540b02d..31cba24
//! that are observable over HTTP without a seat: L2 (updates never rewrite
//! identity keys), L5 (filters are checked by the server), L6 (batch line
//! cap), L7 (ChatKit client secrets are rate limited and act without the
//! raw key), L8 (bad base64 is a 400), L9 (permissions need a checkpoint).
//! `OMNI2API_BATCH_MAX_LINES` is set for this binary, so it is its own file.

use omni2api_server::build_router;
use omni2api_server::storage::memory::MemoryStore;
use omni2api_server::storage::AppState;
use serde_json::{json, Value};

const KEY: &str = "ohk_demo_alice_codex";

async fn app() -> String {
    std::env::set_var("OMNI2API_BATCH_MAX_LINES", "2");
    let app = build_router(AppState::from_store(MemoryStore::seeded()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

async fn post_as(addr: &str, key: &str, path: &str, body: Value) -> (u16, Value) {
    let r = reqwest::Client::new()
        .post(format!("http://{addr}{path}"))
        .header("Authorization", format!("Bearer {key}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let s = r.status().as_u16();
    (s, r.json().await.unwrap_or(json!(null)))
}

async fn post(addr: &str, path: &str, body: Value) -> (u16, Value) {
    post_as(addr, KEY, path, body).await
}

async fn upload(addr: &str, name: &str, purpose: &str, content: &str) -> String {
    let b = "auditlowboundary";
    let body = format!(
        "--{b}\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\n{purpose}\r\n--{b}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\nContent-Type: application/jsonl\r\n\r\n{content}\r\n--{b}--\r\n"
    );
    let r = reqwest::Client::new()
        .post(format!("http://{addr}/v1/files"))
        .header("Authorization", format!("Bearer {KEY}"))
        .header("Content-Type", format!("multipart/form-data; boundary={b}"))
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let v: Value = r.json().await.unwrap();
    v["id"].as_str().unwrap().to_string()
}

// L2: an assistant update cannot rewrite id, object or created_at.
#[tokio::test]
async fn assistant_update_keeps_identity() {
    let addr = app().await;
    let (s, a) = post(
        &addr,
        "/v1/assistants",
        json!({"model": "m", "name": "one"}),
    )
    .await;
    assert_eq!(s, 200, "{a}");
    let id = a["id"].as_str().unwrap().to_string();
    let (s, u) = post(
        &addr,
        &format!("/v1/assistants/{id}"),
        json!({"id": "asst_other", "object": "thread", "created_at": 1, "name": "two"}),
    )
    .await;
    assert_eq!(s, 200, "{u}");
    assert_eq!(u["id"], json!(id));
    assert_eq!(u["object"], "assistant");
    assert_eq!(u["created_at"], a["created_at"]);
    assert_eq!(u["name"], "two", "ordinary keys still update");
}

// L8: environment file data that is not base64 is a 400, not a size.
#[tokio::test]
async fn environment_file_bad_base64_is_400() {
    let addr = app().await;
    let (s, env) = post(&addr, "/v1/agents/environments", json!({})).await;
    assert_eq!(s, 200, "{env}");
    let id = env["id"].as_str().unwrap();
    let (s, v) = post(
        &addr,
        &format!("/v1/agents/environments/{id}/files"),
        json!({"path": "/a.txt", "data": "not base64!!"}),
    )
    .await;
    assert_eq!(s, 400, "{v}");
    let (s, v) = post(
        &addr,
        &format!("/v1/agents/environments/{id}/files"),
        json!({"path": "/a.txt", "data": "aGVsbG8="}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["size_bytes"], 5);
}

// L9: permissions for a checkpoint that does not exist are a 404.
#[tokio::test]
async fn permission_for_missing_checkpoint_is_404() {
    let addr = app().await;
    let (s, v) = post(
        &addr,
        "/v1/fine_tuning/checkpoints/ft:gpt-4o-mini:nope:ckpt-step-1/permissions",
        json!({"project_ids": ["proj_1"]}),
    )
    .await;
    assert_eq!(s, 404, "{v}");
}

// L6: a batch input file past the line cap is refused up front.
#[tokio::test]
async fn batch_line_cap() {
    let addr = app().await;
    let line = |i: usize| {
        json!({"custom_id": format!("r{i}"), "method": "POST", "url": "/v1/chat/completions",
               "body": {"model": "m", "messages": [{"role": "user", "content": "hi"}]}})
        .to_string()
    };
    let three = (0..3).map(line).collect::<Vec<_>>().join("\n");
    let fid = upload(&addr, "in.jsonl", "batch", &three).await;
    let (s, v) = post(
        &addr,
        "/v1/batches",
        json!({"input_file_id": fid, "endpoint": "/v1/chat/completions", "completion_window": "24h"}),
    )
    .await;
    assert_eq!(s, 400, "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("at most 2"),
        "{v}"
    );
}

// L5: a malformed filter is rejected by the server, not handed to the seat.
#[tokio::test]
async fn vector_search_checks_filters() {
    let addr = app().await;
    let (s, vs) = post(&addr, "/v1/vector_stores", json!({"name": "s"})).await;
    assert_eq!(s, 200, "{vs}");
    let id = vs["id"].as_str().unwrap();
    let (s, v) = post(
        &addr,
        &format!("/v1/vector_stores/{id}/search"),
        json!({"query": "x", "filters": {"type": "like", "key": "a", "value": 1}}),
    )
    .await;
    assert_eq!(s, 400, "{v}");
}

// L7: a ChatKit client secret acts for its key (rebuilt from ids) and is
// held to max_requests_per_1_minute.
#[tokio::test]
async fn chatkit_secret_is_rate_limited() {
    let addr = app().await;
    let (s, sess) = post(
        &addr,
        "/v1/chatkit/sessions",
        json!({"user": "u1", "workflow": {"id": "wf_1"},
               "rate_limits": {"max_requests_per_1_minute": 2}}),
    )
    .await;
    assert_eq!(s, 200, "{sess}");
    let secret = sess["client_secret"].as_str().unwrap().to_string();
    let list = json!({"type": "threads.list", "params": {"limit": 5}});
    for _ in 0..2 {
        let (s, v) = post_as(&addr, &secret, "/v1/chatkit", list.clone()).await;
        assert_eq!(s, 200, "the secret acts for its key: {v}");
    }
    let (s, v) = post_as(&addr, &secret, "/v1/chatkit", list).await;
    assert_eq!(s, 429, "{v}");
    assert_eq!(v["error"]["code"], "rate_limit_exceeded");
}
