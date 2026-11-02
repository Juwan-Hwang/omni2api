//! Stored objects for the stateful OpenAI endpoints: Responses
//! (`store`, `previous_response_id`, `background`, retrieve, delete,
//! cancel, input items) and stored Chat Completions. In memory per
//! process, owned by the key's agent, oldest dropped past a cap.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::AuthPrincipal;

/// Most objects kept per kind; the oldest go first beyond it.
const KEEP: usize = 2000;

#[derive(Clone)]
pub struct Stored {
    pub owner: String,
    /// The object as the API returns it.
    pub obj: Value,
    /// The items this object was asked with (Responses input items, chat
    /// request messages), each with an id.
    pub input: Vec<Value>,
    /// Everything a follow-up continues from: earlier history, this input,
    /// this output (Responses items).
    pub history: Vec<Value>,
}

type Map = (HashMap<String, Stored>, Vec<String>);

fn store() -> &'static Mutex<Map> {
    static S: OnceLock<Mutex<Map>> = OnceLock::new();
    S.get_or_init(|| Mutex::new((HashMap::new(), Vec::new())))
}

/// Background runs that can still be cancelled, by response id.
fn running() -> &'static Mutex<HashMap<String, tokio::task::AbortHandle>> {
    static R: OnceLock<Mutex<HashMap<String, tokio::task::AbortHandle>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn put(id: &str, s: Stored) {
    let mut g = store().lock().unwrap();
    let event =
        g.0.get(id)
            .and_then(|old| crate::api::webhooks::transition(&old.obj, &s.obj))
            .map(|k| (s.owner.clone(), k));
    if g.0.insert(id.to_string(), s).is_none() {
        g.1.push(id.to_string());
    }
    if let Some((owner, kind)) = event {
        drop(g);
        crate::api::webhooks::fire(&owner, kind, id);
        return;
    }
    while g.1.len() > KEEP {
        let old = g.1.remove(0);
        g.0.remove(&old);
    }
}

pub fn get(owner: &str, id: &str) -> Option<Stored> {
    store()
        .lock()
        .unwrap()
        .0
        .get(id)
        .filter(|s| s.owner == owner)
        .cloned()
}

/// Change a stored object in place.
pub fn update(id: &str, f: impl FnOnce(&mut Stored)) {
    let event = {
        let mut g = store().lock().unwrap();
        let Some(s) = g.0.get_mut(id) else {
            return;
        };
        let before = s.obj.clone();
        f(s);
        crate::api::webhooks::transition(&before, &s.obj).map(|k| (s.owner.clone(), k))
    };
    if let Some((owner, kind)) = event {
        crate::api::webhooks::fire(&owner, kind, id);
    }
}

pub fn remove(owner: &str, id: &str) -> bool {
    let mut g = store().lock().unwrap();
    if g.0.get(id).is_some_and(|s| s.owner == owner) {
        g.0.remove(id);
        g.1.retain(|x| x != id);
        true
    } else {
        false
    }
}

/// Objects of one `object` kind for an owner, oldest first.
pub fn list(owner: &str, object: &str) -> Vec<(String, Stored)> {
    let g = store().lock().unwrap();
    g.1.iter()
        .filter_map(|id| {
            g.0.get(id)
                .filter(|s| {
                    s.owner == owner && s.obj.get("object").and_then(|o| o.as_str()) == Some(object)
                })
                .map(|s| (id.clone(), s.clone()))
        })
        .collect()
}

pub fn set_running(id: &str, h: tokio::task::AbortHandle) {
    running().lock().unwrap().insert(id.to_string(), h);
}

pub fn done_running(id: &str) {
    running().lock().unwrap().remove(id);
}

/// Stop a background run; false when it already ended.
pub fn abort_running(id: &str) -> bool {
    match take_running(id) {
        Some(h) => {
            h.abort();
            true
        }
        None => false,
    }
}

fn take_running(id: &str) -> Option<tokio::task::AbortHandle> {
    running().lock().unwrap().remove(id)
}

pub fn not_found(what: &str, id: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error": {"message": format!("No {what} found with id '{id}'."), "type": "invalid_request_error", "code": "not_found", "param": null}})),
    )
        .into_response()
}

/// Give every input item an id, as stored items have.
pub fn with_ids(items: Vec<Value>, prefix: &str) -> Vec<Value> {
    items
        .into_iter()
        .map(|mut it| {
            if it.is_object() && it.get("id").and_then(|v| v.as_str()).is_none() {
                it["id"] = json!(format!("{prefix}_{}", uuid::Uuid::now_v7().simple()));
            }
            it
        })
        .collect()
}

/// A Responses `input` as a list of items (a string is one user message).
pub fn input_items(input: Option<&Value>) -> Vec<Value> {
    match input {
        Some(Value::String(s)) => vec![json!({
            "type": "message", "role": "user",
            "content": [{"type": "input_text", "text": s}],
        })],
        Some(Value::Array(a)) => a
            .iter()
            .map(|it| {
                let mut it = it.clone();
                if it.get("type").is_none() && it.get("role").is_some() {
                    it["type"] = json!("message");
                }
                it
            })
            .collect(),
        _ => Vec::new(),
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct ListQuery {
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    order: Option<String>,
    #[serde(default)]
    after: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

/// One page of a list, OpenAI cursor style.
fn page(items: Vec<Value>, q: &ListQuery, default_order: &str) -> Value {
    let mut items = items;
    if q.order.as_deref().unwrap_or(default_order) == "desc" {
        items.reverse();
    }
    if let Some(a) = &q.after {
        if let Some(i) = items
            .iter()
            .position(|x| x.get("id").and_then(|v| v.as_str()) == Some(a))
        {
            items.drain(..=i);
        }
    }
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let has_more = items.len() > limit;
    items.truncate(limit);
    let id = |x: Option<&Value>| x.and_then(|v| v.get("id")).cloned().unwrap_or(Value::Null);
    json!({
        "object": "list",
        "data": items,
        "first_id": id(items.first()),
        "last_id": id(items.last()),
        "has_more": has_more,
    })
}

// ── Responses ──

pub async fn get_response(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    match get(&principal.agent.id, &id) {
        Some(s) => Json(s.obj).into_response(),
        None => not_found("response", &id),
    }
}

pub async fn delete_response(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    if let Some(h) = take_running(&id) {
        h.abort();
    }
    if remove(&principal.agent.id, &id) {
        Json(json!({"id": id, "object": "response", "deleted": true})).into_response()
    } else {
        not_found("response", &id)
    }
}

pub async fn cancel_response(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    let Some(s) = get(&principal.agent.id, &id) else {
        return not_found("response", &id);
    };
    if s.obj.get("background").and_then(|b| b.as_bool()) != Some(true) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": {"message": "Only responses created with background=true can be cancelled.", "type": "invalid_request_error", "code": null, "param": null}})),
        )
            .into_response();
    }
    if let Some(h) = take_running(&id) {
        h.abort();
        update(&id, |s| s.obj["status"] = json!("cancelled"));
    }
    Json(
        get(&principal.agent.id, &id)
            .map(|s| s.obj)
            .unwrap_or(s.obj),
    )
    .into_response()
}

pub async fn response_input_items(
    principal: AuthPrincipal,
    Path(id): Path<String>,
    Query(q): Query<ListQuery>,
) -> Response {
    match get(&principal.agent.id, &id) {
        // Newest first by default, as OpenAI lists input items.
        Some(s) => Json(page(s.input, &q, "desc")).into_response(),
        None => not_found("response", &id),
    }
}

// ── Chat Completions ──

pub async fn list_chat(principal: AuthPrincipal, Query(q): Query<ListQuery>) -> Response {
    let items: Vec<Value> = list(&principal.agent.id, "chat.completion")
        .into_iter()
        .map(|(_, s)| s.obj)
        .filter(|o| {
            q.model
                .as_deref()
                .is_none_or(|m| o.get("model").and_then(|v| v.as_str()) == Some(m))
        })
        .collect();
    Json(page(items, &q, "asc")).into_response()
}

pub async fn get_chat(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    match get(&principal.agent.id, &id).filter(|s| s.obj["object"] == "chat.completion") {
        Some(s) => Json(s.obj).into_response(),
        None => not_found("chat completion", &id),
    }
}

pub async fn update_chat(
    principal: AuthPrincipal,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> Response {
    if get(&principal.agent.id, &id)
        .filter(|s| s.obj["object"] == "chat.completion")
        .is_none()
    {
        return not_found("chat completion", &id);
    }
    let md = b.get("metadata").cloned().unwrap_or(Value::Null);
    update(&id, |s| s.obj["metadata"] = md);
    Json(get(&principal.agent.id, &id).map(|s| s.obj)).into_response()
}

pub async fn delete_chat(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    if get(&principal.agent.id, &id)
        .filter(|s| s.obj["object"] == "chat.completion")
        .is_some()
        && remove(&principal.agent.id, &id)
    {
        Json(json!({"id": id, "object": "chat.completion.deleted", "deleted": true}))
            .into_response()
    } else {
        not_found("chat completion", &id)
    }
}

pub async fn chat_messages(
    principal: AuthPrincipal,
    Path(id): Path<String>,
    Query(q): Query<ListQuery>,
) -> Response {
    match get(&principal.agent.id, &id).filter(|s| s.obj["object"] == "chat.completion") {
        Some(s) => Json(page(s.input, &q, "asc")).into_response(),
        None => not_found("chat completion", &id),
    }
}

/// `POST /v1/chat/completions`, keeping the completion when the caller
/// asked for `store: true` (passed through untouched otherwise).
pub async fn chat_create(
    axum::extract::State(app): axum::extract::State<crate::storage::AppState>,
    principal: AuthPrincipal,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let store = body.get("store").and_then(|v| v.as_bool()).unwrap_or(false);
    let metadata = body.get("metadata").cloned().unwrap_or(json!({}));
    let messages: Vec<Value> = body
        .get("messages")
        .and_then(|m| m.as_array())
        .cloned()
        .unwrap_or_default();
    let req: crate::openai_compat::ChatCompletionRequest = match serde_json::from_value(body) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": {"message": format!("invalid request: {e}"), "type": "invalid_request_error", "code": null, "param": null}})),
            )
                .into_response()
        }
    };
    let owner = principal.agent.id.clone();
    let resp = crate::api::openai::chat_completions(
        axum::extract::State(app),
        principal,
        headers,
        Json(req),
    )
    .await;
    if !store || !resp.status().is_success() {
        return resp;
    }
    let (parts, body) = resp.into_parts();
    let sse = parts
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|c| c.starts_with("text/event-stream"));
    let mut upstream = body.into_data_stream();
    let tee = async_stream::stream! {
        use futures::StreamExt;
        let mut buf = Vec::new();
        while let Some(chunk) = upstream.next().await {
            match chunk {
                Ok(b) => {
                    buf.extend_from_slice(&b);
                    yield Ok::<axum::body::Bytes, axum::Error>(b);
                }
                Err(e) => {
                    yield Err(e);
                    return;
                }
            }
        }
        let text = String::from_utf8_lossy(&buf);
        let obj = if sse { chat_from_sse(&text) } else { serde_json::from_str::<Value>(text.trim()).ok() };
        if let Some(mut obj) = obj.filter(|o| o.get("object").and_then(|v| v.as_str()) == Some("chat.completion")) {
            obj["metadata"] = metadata.clone();
            let id = obj.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let input = messages
                .iter()
                .enumerate()
                .map(|(i, m)| {
                    let mut m = m.clone();
                    m["id"] = json!(format!("{id}-{i}"));
                    m
                })
                .collect();
            put(&id, Stored { owner: owner.clone(), obj, input, history: Vec::new() });
        }
    };
    Response::from_parts(parts, axum::body::Body::from_stream(tee))
}

/// A chat completion object rebuilt from its stream chunks.
fn chat_from_sse(text: &str) -> Option<Value> {
    let mut id = String::new();
    let mut model = Value::Null;
    let mut created = Value::Null;
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut calls: Vec<Value> = Vec::new();
    let mut finish = Value::Null;
    let mut usage = Value::Null;
    for line in text.lines() {
        let Some(d) = line.strip_prefix("data:").map(str::trim) else {
            continue;
        };
        let Ok(c) = serde_json::from_str::<Value>(d) else {
            continue;
        };
        if id.is_empty() {
            id = c
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            model = c.get("model").cloned().unwrap_or(Value::Null);
            created = c.get("created").cloned().unwrap_or(Value::Null);
        }
        if let Some(u) = c.get("usage").filter(|u| !u.is_null()) {
            usage = u.clone();
        }
        let Some(ch) = c.pointer("/choices/0") else {
            continue;
        };
        if let Some(f) = ch.get("finish_reason").filter(|f| !f.is_null()) {
            finish = f.clone();
        }
        let Some(delta) = ch.get("delta") else {
            continue;
        };
        if let Some(t) = delta.get("content").and_then(|v| v.as_str()) {
            content.push_str(t);
        }
        if let Some(t) = delta.get("reasoning_content").and_then(|v| v.as_str()) {
            reasoning.push_str(t);
        }
        for tc in delta
            .get("tool_calls")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
        {
            let idx = tc
                .get("index")
                .and_then(|v| v.as_u64())
                .unwrap_or(calls.len() as u64) as usize;
            while calls.len() <= idx {
                calls.push(json!({"id": "", "type": "function", "function": {"name": "", "arguments": ""}}));
            }
            let c = &mut calls[idx];
            if let Some(v) = tc.get("id").and_then(|v| v.as_str()) {
                c["id"] = json!(v);
            }
            if let Some(v) = tc.pointer("/function/name").and_then(|v| v.as_str()) {
                c["function"]["name"] = json!(format!(
                    "{}{v}",
                    c["function"]["name"].as_str().unwrap_or("")
                ));
            }
            if let Some(v) = tc.pointer("/function/arguments").and_then(|v| v.as_str()) {
                c["function"]["arguments"] = json!(format!(
                    "{}{v}",
                    c["function"]["arguments"].as_str().unwrap_or("")
                ));
            }
        }
    }
    if id.is_empty() {
        return None;
    }
    let mut message = json!({"role": "assistant", "content": if content.is_empty() && !calls.is_empty() { Value::Null } else { json!(content) }});
    if !calls.is_empty() {
        message["tool_calls"] = json!(calls);
    }
    if !reasoning.is_empty() {
        message["reasoning_content"] = json!(reasoning);
    }
    Some(json!({
        "id": id, "object": "chat.completion", "created": created, "model": model,
        "choices": [{"index": 0, "message": message, "finish_reason": finish}],
        "usage": usage,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_follow_cursor_and_order() {
        let items: Vec<Value> = (0..5).map(|i| json!({"id": format!("i{i}")})).collect();
        let q = ListQuery {
            limit: Some(2),
            after: Some("i1".into()),
            ..Default::default()
        };
        let p = page(items.clone(), &q, "asc");
        assert_eq!(p["first_id"], "i2");
        assert_eq!(p["has_more"], true);
        let p = page(items, &ListQuery::default(), "desc");
        assert_eq!(p["first_id"], "i4");
    }

    #[test]
    fn chat_rebuilt_from_chunks() {
        let s = concat!(
            "data: {\"id\":\"c1\",\"model\":\"m\",\"created\":1,\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        let o = chat_from_sse(s).unwrap();
        assert_eq!(o["choices"][0]["message"]["content"], "Hello");
        assert_eq!(o["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn string_input_is_one_message() {
        let v = input_items(Some(&json!("hi")));
        assert_eq!(v[0]["role"], "user");
        let v = with_ids(
            input_items(Some(&json!([{"role": "user", "content": "x"}]))),
            "msg",
        );
        assert_eq!(v[0]["type"], "message");
        assert!(v[0]["id"].as_str().unwrap().starts_with("msg_"));
    }
}
