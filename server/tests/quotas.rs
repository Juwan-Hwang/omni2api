//! Per-owner quotas: one owner's burst evicts its own oldest objects and
//! files, never another owner's. Small caps via env keep the test fast.

use omni2api_server::build_router;
use omni2api_server::model::identity::{AgentIdentity, ApiKey, ClientType, HumanIdentity};
use omni2api_server::storage::memory::MemoryStore;
use omni2api_server::storage::traits::IdentityRepository;
use omni2api_server::storage::AppState;
use serde_json::{json, Value};

const KEY_A: &str = "ohk_demo_alice_codex";
const KEY_B: &str = "ohk_quota_bob";

async fn app_two_owners() -> String {
    let store = MemoryStore::seeded();
    let now = chrono::Utc::now();
    store
        .insert_human(HumanIdentity {
            id: "hum_quota_bob".into(),
            display_name: "Bob".into(),
            avatar_url: None,
            status: None,
            capabilities: vec![],
            metadata: Default::default(),
            created_at: now,
            updated_at: now,
        })
        .await;
    store
        .insert_agent(AgentIdentity {
            id: "agt_quota_bob".into(),
            name: "Bob Agent".into(),
            client_type: ClientType::Codex,
            api_key_id: "key_quota_bob".into(),
            capabilities: vec![],
            metadata: Default::default(),
            created_at: now,
            updated_at: now,
        })
        .await;
    store
        .insert_api_key(ApiKey::new(
            "key_quota_bob",
            KEY_B,
            "agt_quota_bob",
            "hum_quota_bob",
        ))
        .await;
    let app = build_router(AppState::from_store(store));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

async fn post(addr: &str, key: &str, path: &str, body: Value) -> (u16, Value) {
    let r = reqwest::Client::new()
        .post(format!("http://{addr}{path}"))
        .header("Authorization", format!("Bearer {key}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    (r.status().as_u16(), r.json().await.unwrap_or(json!(null)))
}

async fn get(addr: &str, key: &str, path: &str) -> u16 {
    reqwest::Client::new()
        .get(format!("http://{addr}{path}"))
        .header("Authorization", format!("Bearer {key}"))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

async fn upload(addr: &str, key: &str, name: &str, bytes: Vec<u8>) -> String {
    let boundary = "quotaboundary";
    let body = format!(
        "--{b}\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nuser_data\r\n--{b}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\nContent-Type: application/octet-stream\r\n\r\n",
        b = boundary
    );
    let mut body = body.into_bytes();
    body.extend_from_slice(&bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let r = reqwest::Client::new()
        .post(format!("http://{addr}/v1/files"))
        .header("Authorization", format!("Bearer {key}"))
        .header(
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let v: Value = r.json().await.unwrap();
    v["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn object_quota_is_per_owner() {
    std::env::set_var("OMNI2API_STORE_MAX_PER_OWNER", "3");
    std::env::set_var("OMNI2API_STORE_MAX_TOTAL", "10000");
    let addr = app_two_owners().await;

    // A overflows its own quota: its oldest environment goes, the rest stay.
    let mut ids = Vec::new();
    for i in 0..4 {
        let (s, v) = post(
            &addr,
            KEY_A,
            "/v1/agents/environments",
            json!({"name": format!("a{i}")}),
        )
        .await;
        assert_eq!(s, 200, "{v}");
        ids.push(v["id"].as_str().unwrap().to_string());
    }
    assert_eq!(
        get(&addr, KEY_A, &format!("/v1/agents/environments/{}", ids[0])).await,
        404,
        "oldest evicted"
    );
    assert_eq!(
        get(&addr, KEY_A, &format!("/v1/agents/environments/{}", ids[3])).await,
        200
    );

    // B's objects are unaffected, and B's burst does not touch A's.
    let (s, v) = post(
        &addr,
        KEY_B,
        "/v1/agents/environments",
        json!({"name": "b1"}),
    )
    .await;
    assert_eq!(s, 200);
    let b1 = v["id"].as_str().unwrap().to_string();
    assert_eq!(
        get(&addr, KEY_B, &format!("/v1/agents/environments/{b1}")).await,
        200
    );
    assert_eq!(
        get(&addr, KEY_A, &format!("/v1/agents/environments/{}", ids[3])).await,
        200,
        "B's writes must not evict A's"
    );
}

#[tokio::test]
async fn file_bytes_quota_is_per_owner() {
    std::env::set_var("OMNI2API_FILES_MAX_BYTES_PER_OWNER", "100");
    std::env::set_var("OMNI2API_FILES_MAX_BYTES_TOTAL", "100000");
    let addr = app_two_owners().await;

    let f1 = upload(&addr, KEY_A, "a1.bin", vec![1u8; 60]).await;
    let f2 = upload(&addr, KEY_A, "a2.bin", vec![2u8; 60]).await;
    assert_eq!(
        get(&addr, KEY_A, &format!("/v1/files/{f1}/content")).await,
        404,
        "A's oldest evicted over 100B"
    );
    assert_eq!(
        get(&addr, KEY_A, &format!("/v1/files/{f2}/content")).await,
        200
    );

    let fb = upload(&addr, KEY_B, "b1.bin", vec![3u8; 90]).await;
    assert_eq!(
        get(&addr, KEY_B, &format!("/v1/files/{fb}/content")).await,
        200
    );
    assert_eq!(
        get(&addr, KEY_A, &format!("/v1/files/{f2}/content")).await,
        200,
        "B's upload must not evict A's file"
    );
}
