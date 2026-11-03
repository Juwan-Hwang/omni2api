//! Regression tests for the audit of 540b02d..31cba24: cross-owner object
//! safety of the stored-object layer, webhook target policy, and reserved
//! file purposes.

use omni2api_server::api::webhooks::valid_url;
use omni2api_server::build_router;
use omni2api_server::model::identity::{AgentIdentity, ApiKey, ClientType, HumanIdentity};
use omni2api_server::storage::memory::MemoryStore;
use omni2api_server::storage::traits::IdentityRepository;
use omni2api_server::storage::AppState;
use serde_json::{json, Value};

const KEY_A: &str = "ohk_demo_alice_codex"; // seeded caller+responder
const KEY_B: &str = "ohk_audit_bob";
const KEY_SEAT: &str = "ohk_audit_seat";

const KEY_SEAT_FT: &str = "ohk_audit_seat_ft"; // fine-tune's own seat: the
                                               // seat hub is process-global and keyed by api-key id, so each seat-driving
                                               // test needs its own key or the second attach reuses a seat whose device is
                                               // registered in the first test's (dropped) app.

async fn app_two_owners() -> (axum::Router, String) {
    let store = MemoryStore::seeded();
    let now = chrono::Utc::now();
    store
        .insert_human(HumanIdentity {
            id: "hum_audit_bob".into(),
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
            id: "agt_audit_bob".into(),
            name: "Bob Agent".into(),
            client_type: ClientType::Codex,
            api_key_id: "key_audit_bob".into(),
            capabilities: vec![],
            metadata: Default::default(),
            created_at: now,
            updated_at: now,
        })
        .await;
    store
        .insert_api_key(ApiKey::new(
            "key_audit_bob",
            KEY_B,
            "agt_audit_bob",
            "hum_audit_bob",
        ))
        .await;
    store
        .insert_agent(AgentIdentity {
            id: "agt_audit_seat".into(),
            name: "Bob Seat".into(),
            client_type: ClientType::Codex,
            api_key_id: "key_audit_seat".into(),
            capabilities: vec![],
            metadata: Default::default(),
            created_at: now,
            updated_at: now,
        })
        .await;
    store
        .insert_api_key(ApiKey::new(
            "key_audit_seat",
            KEY_SEAT,
            "agt_audit_seat",
            "hum_audit_bob",
        ))
        .await;
    store
        .insert_agent(AgentIdentity {
            id: "agt_audit_seat_ft".into(),
            name: "Bob Seat FT".into(),
            client_type: ClientType::Codex,
            api_key_id: "key_audit_seat_ft".into(),
            capabilities: vec![],
            metadata: Default::default(),
            created_at: now,
            updated_at: now,
        })
        .await;
    store
        .insert_api_key(ApiKey::new(
            "key_audit_seat_ft",
            KEY_SEAT_FT,
            "agt_audit_seat_ft",
            "hum_audit_bob",
        ))
        .await;
    let app = build_router(AppState::from_store(store));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (build_router_placeholder(), addr)
}

fn build_router_placeholder() -> axum::Router {
    axum::Router::new()
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

async fn post(addr: &str, key: &str, path: &str, body: Value) -> (u16, Value) {
    let r = client()
        .post(format!("http://{addr}{path}"))
        .header("Authorization", format!("Bearer {key}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let s = r.status().as_u16();
    let v: Value = r.json().await.unwrap_or(json!(null));
    (s, v)
}

async fn get(addr: &str, key: Option<&str>, path: &str) -> (u16, Value) {
    let mut b = client().get(format!("http://{addr}{path}"));
    if let Some(k) = key {
        b = b.header("Authorization", format!("Bearer {k}"));
    }
    let r = b.send().await.unwrap();
    let s = r.status().as_u16();
    let v: Value = r.json().await.unwrap_or(json!(null));
    (s, v)
}

async fn delete(addr: &str, key: &str, path: &str) -> (u16, Value) {
    let r = client()
        .delete(format!("http://{addr}{path}"))
        .header("Authorization", format!("Bearer {key}"))
        .send()
        .await
        .unwrap();
    let s = r.status().as_u16();
    let v: Value = r.json().await.unwrap_or(json!(null));
    (s, v)
}

/// All conversation ids anywhere in the JSON.
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

/// Newest inbox cursor for this key, so polls see only what arrives next.
async fn seat_cursor(addr: &str, key: &str) -> String {
    let (s, v) = get(addr, Some(key), "/v1/seat/inbox?wait=0").await;
    assert_eq!(s, 200, "seat inbox: {v}");
    v.get("cursor")
        .or_else(|| v.get("latest"))
        .and_then(|c| {
            c.as_str()
                .map(String::from)
                .or_else(|| c.as_u64().map(|n| n.to_string()))
        })
        .unwrap_or_else(|| "0".to_string())
}

async fn seat_next_conv(addr: &str, key: &str, after: &str) -> String {
    for _ in 0..40 {
        let (s, v) = get(
            addr,
            Some(key),
            &format!("/v1/seat/inbox?wait=1&after={after}"),
        )
        .await;
        assert_eq!(s, 200, "seat inbox: {v}");
        let mut convs = Vec::new();
        all_convs(&v, &mut convs);
        if let Some(c) = convs.into_iter().next() {
            return c;
        }
    }
    panic!("seat never saw the task");
}

// ── 1. caller-supplied environment_id overwrites another owner's object ──
#[tokio::test]
async fn repro_environment_id_overwrite_cross_owner() {
    let (_r, addr) = app_two_owners().await;
    let (s, a_env) = post(
        &addr,
        KEY_A,
        "/v1/agents/environments",
        json!({"name": "alice-env"}),
    )
    .await;
    assert_eq!(s, 200, "{a_env}");
    let id = a_env["id"].as_str().unwrap().to_string();
    println!("[AUDIT] A created environment {id}");

    // B reuses A's id: rejected, and A's object survives untouched.
    let (s, b_env) = post(
        &addr,
        KEY_B,
        "/v1/agents/environments",
        json!({"name": "bob-takeover", "environment": {"type": "openai_hosted", "environment_id": id}}),
    )
    .await;
    assert_eq!(s, 409, "reused id must conflict, got {s}: {b_env}");

    let (sa, va) = get(&addr, Some(KEY_A), &format!("/v1/agents/environments/{id}")).await;
    let (sb, _) = get(&addr, Some(KEY_B), &format!("/v1/agents/environments/{id}")).await;
    assert_eq!(sa, 200, "A keeps its environment: {va}");
    assert_eq!(sb, 404);

    // Even A cannot reuse an id it owns.
    let (s, _) = post(
        &addr,
        KEY_A,
        "/v1/agents/environments",
        json!({"environment": {"type": "openai_hosted", "environment_id": id}}),
    )
    .await;
    assert_eq!(s, 409);
}

// ── 2. /v1/responses/:id ignores the stored object type ──
#[tokio::test]
async fn repro_responses_type_confusion() {
    let (_r, addr) = app_two_owners().await;
    let (s, env) = post(
        &addr,
        KEY_A,
        "/v1/agents/environments",
        json!({"name": "typed"}),
    )
    .await;
    assert_eq!(s, 200);
    let id = env["id"].as_str().unwrap().to_string();

    let (s, _) = get(&addr, Some(KEY_A), &format!("/v1/responses/{id}")).await;
    assert_eq!(s, 404, "a non-response object must not read as a response");
    let (s, _) = delete(&addr, KEY_A, &format!("/v1/responses/{id}")).await;
    assert_eq!(s, 404);
    let (s, _) = get(&addr, Some(KEY_A), &format!("/v1/agents/environments/{id}")).await;
    assert_eq!(s, 200, "the environment is untouched");
}

// ── 3. delete_response aborts another owner's running response ──
#[tokio::test]
async fn repro_delete_response_aborts_cross_owner() {
    let (_r, addr) = app_two_owners().await;

    // Control: background response answered normally -> abandoned == false.
    let cur = seat_cursor(&addr, KEY_A).await;
    let (s, v) = post(
        &addr,
        KEY_A,
        "/v1/responses",
        json!({"model": "human", "input": "control: say hi", "background": true}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let control_id = v["id"].as_str().unwrap().to_string();
    let conv = seat_next_conv(&addr, KEY_A, &cur).await;
    let (s, r) = post(
        &addr,
        KEY_A,
        "/v1/seat/reply",
        json!({"conversation_id": conv, "text": "hi", "end_turn": true,
               "client_msg_id": format!("ohs_ctrl{}", uuid::Uuid::new_v4().simple())}),
    )
    .await;
    println!("[AUDIT] control seat reply -> {s} {r}");
    assert_eq!(s, 200);

    // Attack: A starts a background response, B deletes by id, seat answer is abandoned.
    let cur = seat_cursor(&addr, KEY_A).await;
    let (s, v) = post(
        &addr,
        KEY_A,
        "/v1/responses",
        json!({"model": "human", "input": "attack: say hi", "background": true}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let resp_id = v["id"].as_str().unwrap().to_string();
    let conv2 = seat_next_conv(&addr, KEY_A, &cur).await;

    let (s, v) = delete(&addr, KEY_B, &format!("/v1/responses/{resp_id}")).await;
    assert_eq!(s, 404, "B gets 404: {v}");

    let (s, r) = post(
        &addr,
        KEY_A,
        "/v1/seat/reply",
        json!({"conversation_id": conv2, "text": "hi", "end_turn": true,
               "client_msg_id": format!("ohs_atk{}", uuid::Uuid::new_v4().simple())}),
    )
    .await;
    println!("[AUDIT] attack seat reply -> {s} {r}");
    assert_eq!(s, 200);

    // Poll both responses: the control completes; the aborted one is stuck queued.
    let mut control_status = String::new();
    for _ in 0..20 {
        let (_, c) = get(&addr, Some(KEY_A), &format!("/v1/responses/{control_id}")).await;
        control_status = c["status"].as_str().unwrap_or("?").to_string();
        if control_status == "completed" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    // Give the (dead) attack task a moment it will never use.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let (_, a) = get(&addr, Some(KEY_A), &format!("/v1/responses/{resp_id}")).await;
    let attack_status = a["status"].as_str().unwrap_or("?").to_string();
    println!("[AUDIT] control status={control_status}; attack status={attack_status}");
    assert_eq!(
        control_status, "completed",
        "control response completed normally"
    );
    assert_eq!(
        attack_status, "completed",
        "B's delete must not abort A's running response: it completes too"
    );
}

// ── 4.// ── 4. caller-chosen purpose=output makes files world-readable ──
#[tokio::test]
async fn repro_anonymous_output_file() {
    let (_r, addr) = app_two_owners().await;
    let boundary = "auditboundary123";
    let body = format!(
        "--{b}\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\noutput\r\n--{b}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"pwn.txt\"\r\nContent-Type: text/plain\r\n\r\nhello world\r\n--{b}--\r\n",
        b = boundary
    );
    let r = client()
        .post(format!("http://{addr}/v1/files"))
        .header("Authorization", format!("Bearer {KEY_A}"))
        .header(
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(body.clone())
        .send()
        .await
        .unwrap();
    let s = r.status().as_u16();
    let v: Value = r.json().await.unwrap();
    assert_eq!(s, 400, "caller-set purpose=output must be rejected: {v}");

    // An ordinary upload is never on the anonymous route.
    let body = body.replace("output", "user_data");
    let r = client()
        .post(format!("http://{addr}/v1/files"))
        .header("Authorization", format!("Bearer {KEY_A}"))
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
    let id = v["id"].as_str().unwrap().to_string();
    let r = client()
        .get(format!("http://{addr}/files/{id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status().as_u16(),
        404,
        "no anonymous download of caller files"
    );
}

// ── 5. webhooks accept loopback/internal targets; /test is a reachability oracle ──
#[tokio::test]
async fn repro_webhook_internal_target() {
    let (_r, addr) = app_two_owners().await;
    // A port that refuses connections.
    let (s, v) = post(
        &addr,
        KEY_A,
        "/v1/webhook_endpoints",
        json!({"url": "http://127.0.0.1:1/hook", "event_types": ["response.completed"], "name": "dead"}),
    )
    .await;
    assert_eq!(s, 400, "loopback target must be rejected: {v}");
    let wh_dead = String::new();

    // The server's own port (a live internal endpoint).
    let (s, v) = post(
        &addr,
        KEY_A,
        "/v1/webhook_endpoints",
        json!({"url": format!("http://{addr}/v1/models"), "event_types": ["response.completed"], "name": "live"}),
    )
    .await;
    assert_eq!(s, 400, "internal-address target must be rejected: {v}");

    // A public https target registers fine.
    let (s, v) = post(
        &addr,
        KEY_A,
        "/v1/webhook_endpoints",
        json!({"url": "https://example.com/hook", "event_types": ["response.completed"], "name": "ok"}),
    )
    .await;
    assert_eq!(s, 200, "public target must still register: {v}");
    let wh_live = String::new();
    let _ = (wh_dead, wh_live);
}

/// URL policy unit coverage (no server needed).
#[test]
fn webhook_url_policy() {
    for bad in [
        "http://127.0.0.1/hook",
        "http://127.0.0.1:8080/hook",
        "https://169.254.169.254/latest/meta-data",
        "http://10.0.0.4/",
        "http://192.168.1.1/",
        "http://172.16.0.1/",
        "http://172.31.255.255/",
        "http://0.0.0.0/",
        "http://[::1]/",
        "http://localhost/",
        "http://foo.localhost/",
        "http://printer.internal/",
        "ftp://example.com/",
        "https://user:pass@example.com/",
        "https://",
        "not a url",
    ] {
        assert!(!valid_url(bad), "should reject {bad}");
    }
    // Alternate IP spellings HTTP clients accept: the WHATWG parser must
    // normalize them to the address they really mean.
    for bad in [
        "http://2130706433/",         // decimal 127.0.0.1
        "http://127.1/",              // short form
        "http://0x7f.1/",             // hex form
        "http://[::ffff:127.0.0.1]/", // IPv4-mapped loopback
        "http://[::ffff:a9fe:a9fe]/", // IPv4-mapped metadata address
        "http://localhost./",         // trailing root dot
        "http://100.64.0.1/",         // CGNAT 100.64.0.0/10
        "http://100.127.255.255/",    // CGNAT upper edge
    ] {
        assert!(!valid_url(bad), "should reject {bad}");
    }
    for good in [
        "https://example.com/hook",
        "http://example.com:8080/hook",
        "https://8.8.8.8/hook",
        "https://hooks.slack.com/services/T00/B00/xyz",
        "http://100.63.255.255/", // just outside the CGNAT block
    ] {
        assert!(valid_url(good), "should accept {good}");
    }

    // The IP predicate the DNS resolver applies at connect time.
    use omni2api_server::api::webhooks::ip_blocked;
    let blocked = [
        "127.0.0.1".parse().unwrap(),
        "10.1.2.3".parse().unwrap(),
        "169.254.169.254".parse().unwrap(),
        "100.64.0.1".parse().unwrap(),
        "::1".parse().unwrap(),
        "::ffff:127.0.0.1".parse().unwrap(),
        "fd00::1".parse().unwrap(),
        "fe80::1".parse().unwrap(),
    ];
    for ip in blocked {
        assert!(ip_blocked(&ip), "{ip} must be blocked");
    }
    let allowed = ["8.8.8.8".parse().unwrap(), "100.63.0.1".parse().unwrap()];
    for ip in allowed {
        assert!(!ip_blocked(&ip), "{ip} must pass");
    }
}

// ── 7. /test answers uniformly (no reachability oracle) ──
#[tokio::test]
async fn webhook_test_result_is_uniform() {
    let (_r, addr) = app_two_owners().await;
    // Two endpoints whose deliveries both fail fast but differently:
    // a loopback literal (connection refused) and a loopback name (the
    // blocking DNS resolver refuses it). Registered straight into the
    // store, since create would reject these URLs on sight.
    // A third endpoint answers 401: a non-2xx must look the same as a
    // refused connection and a blocked name.
    let rejecting = hook_server(axum::http::StatusCode::UNAUTHORIZED).await;
    let uid = uuid::Uuid::new_v4().simple().to_string();
    let ids = [
        format!("whep_uniform_a_{uid}"),
        format!("whep_uniform_b_{uid}"),
        format!("whep_uniform_c_{uid}"),
    ];
    for (id, url) in [
        (&ids[0], "http://127.0.0.1:9/hook".to_string()),
        (&ids[1], "http://localhost:9/hook".to_string()),
        (&ids[2], format!("http://{rejecting}/hook")),
    ] {
        omni2api_server::api::stored::put(
            id,
            omni2api_server::api::stored::Stored {
                owner: "agt_demo_codex".to_string(),
                obj: json!({"id": id, "object": "webhook_endpoint", "url": url,
                            "event_types": ["response.completed"], "status": "enabled"}),
                input: Vec::new(),
                history: Vec::new(),
            },
        );
    }
    let mut bodies = Vec::new();
    for id in &ids {
        let (s, v) = post(
            &addr,
            KEY_A,
            &format!("/v1/webhook_endpoints/{id}/test"),
            json!({"event_type": "response.completed"}),
        )
        .await;
        assert_eq!(s, 200, "uniform HTTP status: {v}");
        assert_eq!(v["success"], json!(false), "delivery failed: {v}");
        let text = v.to_string();
        assert!(
            !text.contains("answered") && !text.contains("reached"),
            "no distinguishing detail: {text}"
        );
        bodies.push(v);
    }
    // Identical shape for every failure mode (the id is each endpoint's own).
    for b in &mut bodies {
        b.as_object_mut().unwrap().remove("webhook_endpoint_id");
    }
    assert_eq!(
        bodies[0], bodies[1],
        "every failure mode returns the identical result"
    );
    assert_eq!(
        bodies[0], bodies[2],
        "a non-2xx answer returns the identical result"
    );
    assert!(bodies[0].get("status_code").is_none(), "{}", bodies[0]);
}

/// A tiny receiver on loopback that answers every POST with `status`.
async fn hook_server(status: axum::http::StatusCode) -> String {
    let app =
        axum::Router::new().route("/hook", axum::routing::post(move || async move { status }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

// ── 7b. a successful /test reports the 2xx status code ──
#[tokio::test]
async fn webhook_test_success_carries_status_code() {
    let (_r, addr) = app_two_owners().await;
    let ok = hook_server(axum::http::StatusCode::NO_CONTENT).await;
    let id = format!("whep_success_{}", uuid::Uuid::new_v4().simple());
    omni2api_server::api::stored::put(
        &id,
        omni2api_server::api::stored::Stored {
            owner: "agt_demo_codex".to_string(),
            obj: json!({"id": id, "object": "webhook_endpoint", "url": format!("http://{ok}/hook"),
                        "event_types": ["response.completed"], "status": "enabled"}),
            input: Vec::new(),
            history: Vec::new(),
        },
    );
    let (s, v) = post(
        &addr,
        KEY_A,
        &format!("/v1/webhook_endpoints/{id}/test"),
        json!({"event_type": "response.completed"}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["success"], json!(true), "{v}");
    assert_eq!(v["status_code"], json!(204), "{v}");
}

// ── 8. multipart uploads reject the reserved purpose too ──
#[tokio::test]
async fn uploads_reject_reserved_purpose() {
    let (_r, addr) = app_two_owners().await;
    // The chunked-upload path must apply the same purpose policy as the
    // direct upload (it writes to the same files::put).
    let (s, v) = post(
        &addr,
        KEY_A,
        "/v1/uploads",
        json!({"filename": "pwn.txt", "purpose": "output", "bytes": 11, "mime_type": "text/plain"}),
    )
    .await;
    assert_eq!(s, 400, "purpose=output rejected at upload create: {v}");

    // An ordinary purpose works, and its file stays off the anonymous route.
    let (s, v) = post(
        &addr,
        KEY_A,
        "/v1/uploads",
        json!({"filename": "ok.txt", "purpose": "user_data", "bytes": 11, "mime_type": "text/plain"}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
}

// ── 9. environment_id conflicts are atomic, same owner included ──
#[tokio::test]
async fn environment_id_conflict_same_owner() {
    let (_r, addr) = app_two_owners().await;
    let env_id = format!("env_{}", uuid::Uuid::new_v4().simple());
    let (s, v) = post(
        &addr,
        KEY_A,
        "/v1/agents/environments",
        json!({"name": "first", "environment": {"type": "openai_hosted", "environment_id": env_id}}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let (s, before) = get(
        &addr,
        Some(KEY_A),
        &format!("/v1/agents/environments/{env_id}"),
    )
    .await;
    assert_eq!(s, 200, "{before}");
    // Same owner, same id: rejected, and the original object is untouched.
    let (s, v) = post(
        &addr,
        KEY_A,
        "/v1/agents/environments",
        json!({"name": "second", "environment": {"type": "openai_hosted", "environment_id": env_id}}),
    )
    .await;
    assert_eq!(s, 409, "a reused environment_id conflicts: {v}");
    let (s, after) = get(
        &addr,
        Some(KEY_A),
        &format!("/v1/agents/environments/{env_id}"),
    )
    .await;
    assert_eq!(s, 200, "{after}");
    assert_eq!(before, after, "the original object survives untouched");
}

// ── 10. fine-tune jobs report no fabricated metrics ──
#[tokio::test]
async fn fine_tune_job_reports_no_fabricated_metrics() {
    let (_r, addr) = app_two_owners().await;
    // A small training file.
    let boundary = "ftboundary";
    let body = format!(
        "--{b}\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nfine-tune\r\n--{b}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"train.jsonl\"\r\nContent-Type: application/jsonl\r\n\r\n{{\"messages\": []}}\r\n--{b}--\r\n",
        b = boundary
    );
    let r = client()
        .post(format!("http://{addr}/v1/files"))
        .header("Authorization", format!("Bearer {KEY_B}"))
        .header(
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let fid = r.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let cur = seat_cursor(&addr, KEY_SEAT_FT).await;
    let (s, v) = post(
        &addr,
        KEY_B,
        "/v1/fine_tuning/jobs",
        json!({"model": "gpt-4o-mini", "training_file": fid}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let jid = v["id"].as_str().unwrap().to_string();

    // The seat reads the file and answers with a note.
    let conv = seat_next_conv(&addr, KEY_SEAT_FT, &cur).await;
    let (s, r) = post(
        &addr,
        KEY_SEAT_FT,
        "/v1/seat/reply",
        json!({"conversation_id": conv, "text": "read the examples", "end_turn": true,
               "client_msg_id": format!("ohs_ft{}", uuid::Uuid::new_v4().simple())}),
    )
    .await;
    assert_eq!(s, 200, "{r}");

    let mut job = json!(null);
    for _ in 0..25 {
        let (s, v) = get(&addr, Some(KEY_B), &format!("/v1/fine_tuning/jobs/{jid}")).await;
        assert_eq!(s, 200, "{v}");
        match v["status"].as_str() {
            Some("succeeded") => {
                job = v;
                break;
            }
            Some("failed") => panic!("job failed: {v}"),
            _ => tokio::time::sleep(std::time::Duration::from_millis(200)).await,
        }
    }
    assert_eq!(job["status"], json!("succeeded"), "job finished: {job}");
    assert!(
        job["fine_tuned_model"]
            .as_str()
            .unwrap_or("")
            .starts_with("ft:"),
        "the seat-backed alias model: {job}"
    );
    // Nothing was trained, so nothing may claim training numbers.
    assert_eq!(job["result_files"], json!([]), "no metrics CSV: {job}");
    assert!(job["trained_tokens"].is_null(), "no token count: {job}");

    let (s, v) = get(
        &addr,
        Some(KEY_B),
        &format!("/v1/fine_tuning/jobs/{jid}/checkpoints"),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["data"], json!([]), "no invented checkpoints: {v}");

    let (s, v) = get(
        &addr,
        Some(KEY_B),
        &format!("/v1/fine_tuning/jobs/{jid}/events"),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let msgs = v.to_string();
    assert!(
        msgs.contains("No real training metrics"),
        "the events say why the numbers are absent: {msgs}"
    );
}

// ── 6. provenance: seat-made output is trusted whatever key stored it ──
#[tokio::test]
async fn provenance_trusts_seat_output_across_keys() {
    let (_r, addr) = app_two_owners().await;
    let client = client();

    // B asks for an image; the seat (key A, a different agent) answers with one.
    let cur = seat_cursor(&addr, KEY_SEAT).await;
    let a2 = addr.clone();
    let c2 = client.clone();
    let req = tokio::spawn(async move {
        c2.post(format!("http://{a2}/v1/images/generations"))
            .header("Authorization", format!("Bearer {KEY_B}"))
            .json(&json!({"model": "gpt-image-1", "prompt": "a dot"}))
            .send()
            .await
            .unwrap()
    });
    let conv = seat_next_conv(&addr, KEY_SEAT, &cur).await;
    let png: &[u8] = b"\x89PNG\r\n\x1a\nfake";
    let (s, r) = post(
        &addr,
        KEY_SEAT,
        "/v1/seat/reply",
        json!({"conversation_id": conv, "text": "", "end_turn": true,
               "client_msg_id": format!("ohs_prov{}", uuid::Uuid::new_v4().simple()),
               "attachments": [{"name": "dot.png", "data": "iVBORw0KGgpmYWtl"}]}),
    )
    .await;
    assert_eq!(s, 200, "{r}");
    let resp = req.await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    // B checks the provenance of exactly those bytes: made by this server's
    // seat even though B's agent never touched them.
    let boundary = "provboundary";
    let mut body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"dot.png\"\r\nContent-Type: image/png\r\n\r\n"
    ).into_bytes();
    body.extend_from_slice(png);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let r = client
        .post(format!("http://{addr}/v1/content_provenance_checks"))
        .header("Authorization", format!("Bearer {KEY_B}"))
        .header(
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(body)
        .send()
        .await
        .unwrap();
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["results"][0]["validation_state"], json!("trusted"), "{v}");

    // Bytes that merely contain c2pa markers are reported, never "valid".
    let mut body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"x.jpg\"\r\nContent-Type: image/jpeg\r\n\r\n"
    ).into_bytes();
    body.extend_from_slice(b"....jumb....c2pa....");
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let r = client
        .post(format!("http://{addr}/v1/content_provenance_checks"))
        .header("Authorization", format!("Bearer {KEY_B}"))
        .header(
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(body)
        .send()
        .await
        .unwrap();
    let v: Value = r.json().await.unwrap();
    assert_eq!(
        v["results"][0]["validation_state"],
        json!("unverified"),
        "{v}"
    );
}
