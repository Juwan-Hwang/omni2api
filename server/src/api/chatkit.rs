//! ChatKit (beta): sessions with client secrets, the threads REST view,
//! and `POST /v1/chatkit`, the ChatKit server protocol the ChatKit web
//! component speaks (`threads.create`, `threads.add_user_message`, ...),
//! answered by the seat. A thread is one seat conversation; its items are
//! kept in the protocol's own shape and shown in the REST shape on the
//! `/v1/chatkit/threads` routes. In memory per process, owned by the key's
//! agent (a client secret acts for the key that minted it, as its user).

// Handlers return the HTTP error response itself as their `Err`.
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::api::stored::{self, Stored};
use crate::auth::AuthPrincipal;
use crate::storage::AppState;

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn iso(t: i64) -> String {
    chrono::DateTime::from_timestamp(t, 0)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn uid(p: &str) -> String {
    format!("{p}_{}", uuid::Uuid::now_v7().simple())
}

fn err(status: StatusCode, msg: &str) -> Response {
    (
        status,
        Json(json!({"error": {"message": msg, "type": "invalid_request_error", "code": null, "param": null}})),
    )
        .into_response()
}

// ── sessions ──

struct Secret {
    principal: AuthPrincipal,
    session: String,
    user: String,
    expires_at: i64,
}

fn secrets() -> &'static Mutex<HashMap<String, Secret>> {
    static S: OnceLock<Mutex<HashMap<String, Secret>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

pub async fn create_session(principal: AuthPrincipal, Json(b): Json<Value>) -> Response {
    let Some(user) = b["user"].as_str().filter(|u| !u.is_empty()) else {
        return err(StatusCode::BAD_REQUEST, "user is required");
    };
    let Some(wf) = b["workflow"]["id"].as_str() else {
        return err(StatusCode::BAD_REQUEST, "workflow.id is required");
    };
    let secs = b
        .pointer("/expires_after/seconds")
        .and_then(|s| s.as_i64())
        .unwrap_or(600)
        .clamp(1, 600);
    let rpm = b
        .pointer("/rate_limits/max_requests_per_1_minute")
        .and_then(|s| s.as_i64())
        .unwrap_or(10);
    let cfg = &b["chatkit_configuration"];
    let flag = |p: &str, d: bool| cfg.pointer(p).and_then(|v| v.as_bool()).unwrap_or(d);
    let id = uid("cksess");
    let secret = format!("ck_{}", uuid::Uuid::new_v4().simple());
    let expires_at = now() + secs;
    let obj = json!({
        "id": id, "object": "chatkit.session", "client_secret": secret, "expires_at": expires_at,
        "max_requests_per_1_minute": rpm, "rate_limits": {"max_requests_per_1_minute": rpm},
        "status": "active", "user": user,
        "workflow": {"id": wf, "state_variables": b["workflow"].get("state_variables").cloned().unwrap_or(Value::Null),
                     "tracing": {"enabled": b.pointer("/workflow/tracing/enabled").and_then(|v| v.as_bool()).unwrap_or(true)},
                     "version": b["workflow"].get("version").cloned().unwrap_or(Value::Null)},
        "chatkit_configuration": {
            "automatic_thread_titling": {"enabled": flag("/automatic_thread_titling/enabled", true)},
            // Uploads are not offered: the seat reads the caller's own files.
            "file_upload": {"enabled": false, "max_file_size": null, "max_files": null},
            "history": {"enabled": flag("/history/enabled", true),
                        "recent_threads": cfg.pointer("/history/recent_threads").cloned().unwrap_or(Value::Null)},
        },
    });
    {
        let mut g = secrets().lock().unwrap();
        let t = now();
        g.retain(|_, s| s.expires_at > t);
        g.insert(
            secret,
            Secret {
                principal: principal.clone(),
                session: id.clone(),
                user: user.to_string(),
                expires_at,
            },
        );
    }
    stored::put(
        &id,
        Stored {
            owner: principal.agent.id.clone(),
            obj: obj.clone(),
            input: Vec::new(),
            history: Vec::new(),
        },
    );
    Json(obj).into_response()
}

pub async fn cancel_session(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    let Some(s) =
        stored::get(&principal.agent.id, &id).filter(|s| s.obj["object"] == "chatkit.session")
    else {
        return stored::not_found("chatkit session", &id);
    };
    let secret = s.obj["client_secret"].as_str().unwrap_or("").to_string();
    secrets().lock().unwrap().remove(&secret);
    let mut o = s.obj;
    o["status"] = json!("cancelled");
    stored::update(&id, |s| s.obj = o.clone());
    Json(o).into_response()
}

// ── threads ──

fn thread(owner: &str, id: &str) -> Option<Stored> {
    stored::get(owner, id).filter(|s| s.obj["object"] == "chatkit.thread")
}

/// A protocol item in the REST shape.
fn rest_item(it: &Value) -> Value {
    let created = chrono::DateTime::parse_from_rfc3339(it["created_at"].as_str().unwrap_or(""))
        .map(|d| d.timestamp())
        .unwrap_or(0);
    let mut o = json!({"id": it["id"], "object": "chatkit.thread_item", "created_at": created, "thread_id": it["thread_id"]});
    match it["type"].as_str().unwrap_or("") {
        "user_message" => {
            o["type"] = json!("chatkit.user_message");
            o["content"] = it["content"].clone();
            o["attachments"] = json!([]);
            o["inference_options"] = it.get("inference_options").cloned().unwrap_or(Value::Null);
        }
        "assistant_message" => {
            o["type"] = json!("chatkit.assistant_message");
            o["content"] = it["content"].clone();
        }
        other => o["type"] = json!(format!("chatkit.{other}")),
    }
    o
}

#[derive(Debug, serde::Deserialize, Default)]
pub struct ListQuery {
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    after: Option<String>,
    #[serde(default)]
    before: Option<String>,
    #[serde(default)]
    order: Option<String>,
    #[serde(default)]
    user: Option<String>,
}

fn page(mut items: Vec<Value>, q: &ListQuery) -> Value {
    if q.order.as_deref().unwrap_or("desc") == "desc" {
        items.reverse();
    }
    if let Some(a) = &q.after {
        if let Some(i) = items.iter().position(|x| x["id"] == json!(a)) {
            items.drain(..=i);
        }
    }
    if let Some(b) = &q.before {
        if let Some(i) = items.iter().position(|x| x["id"] == json!(b)) {
            items.truncate(i);
        }
    }
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let has_more = items.len() > limit;
    items.truncate(limit);
    let id_of = |x: Option<&Value>| x.and_then(|v| v.get("id")).cloned().unwrap_or(Value::Null);
    json!({"object": "list", "data": items, "first_id": id_of(items.first()), "last_id": id_of(items.last()), "has_more": has_more})
}

pub async fn list_threads(principal: AuthPrincipal, Query(q): Query<ListQuery>) -> Response {
    let mut v: Vec<Value> = stored::list(&principal.agent.id, "chatkit.thread")
        .into_iter()
        .map(|(_, s)| s.obj)
        .collect();
    if let Some(u) = &q.user {
        v.retain(|t| t["user"] == json!(u));
    }
    Json(page(v, &q)).into_response()
}

pub async fn get_thread(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    match thread(&principal.agent.id, &id) {
        Some(s) => Json(s.obj).into_response(),
        None => stored::not_found("thread", &id),
    }
}

pub async fn delete_thread(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    if thread(&principal.agent.id, &id).is_none() {
        return stored::not_found("thread", &id);
    }
    stored::remove(&principal.agent.id, &id);
    Json(json!({"id": id, "object": "chatkit.thread.deleted", "deleted": true})).into_response()
}

pub async fn list_items(
    principal: AuthPrincipal,
    Path(id): Path<String>,
    Query(q): Query<ListQuery>,
) -> Response {
    let Some(t) = thread(&principal.agent.id, &id) else {
        return stored::not_found("thread", &id);
    };
    let items: Vec<Value> = t.input.iter().map(rest_item).collect();
    Json(page(items, &q)).into_response()
}

// ── the ChatKit protocol ──

/// Who a protocol request acts for: an API key (user from the request's
/// metadata, else "default") or a session's client secret.
async fn caller(app: &AppState, headers: &HeaderMap) -> Result<(AuthPrincipal, String), Response> {
    let key = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(|s| s.trim().to_string());
    let Some(key) = key else {
        return Err(err(
            StatusCode::UNAUTHORIZED,
            "Missing API key or client secret",
        ));
    };
    if key.starts_with("ck_") {
        let g = secrets().lock().unwrap();
        return match g.get(&key).filter(|s| s.expires_at > now()) {
            Some(s) => {
                let _ = &s.session;
                Ok((s.principal.clone(), s.user.clone()))
            }
            None => Err(err(
                StatusCode::UNAUTHORIZED,
                "Expired or unknown client secret",
            )),
        };
    }
    match app.identity.resolve_principal(&key).await {
        Some(p) if p.api_key.scope.may_call() => Ok((p, String::new())),
        _ => Err(err(StatusCode::UNAUTHORIZED, "Invalid API key")),
    }
}

/// A thread in the protocol's shape, with a page of its items.
fn proto_thread(t: &Stored, with_items: bool) -> Value {
    let items = if with_items {
        t.input.clone()
    } else {
        Vec::new()
    };
    let mut v = json!({"id": t.obj["id"], "created_at": iso(t.obj["created_at"].as_i64().unwrap_or(0)),
        "status": t.obj["status"], "items": {"data": items, "has_more": false}});
    if let Some(title) = t.obj["title"].as_str() {
        v["title"] = json!(title);
    }
    v
}

fn proto_page(mut items: Vec<Value>, p: &Value) -> Value {
    if p["order"].as_str().unwrap_or("desc") == "desc" {
        items.reverse();
    }
    if let Some(a) = p["after"].as_str() {
        if let Some(i) = items.iter().position(|x| x["id"] == json!(a)) {
            items.drain(..=i);
        }
    }
    let limit = p["limit"].as_u64().unwrap_or(20).clamp(1, 100) as usize;
    let has_more = items.len() > limit;
    items.truncate(limit);
    let mut v = json!({"data": items, "has_more": has_more});
    if has_more {
        if let Some(last) = v["data"]
            .as_array()
            .and_then(|d| d.last())
            .map(|x| x["id"].clone())
        {
            v["after"] = last;
        }
    }
    v
}

fn user_text(input: &Value) -> String {
    let mut s: String = input["content"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|p| p["text"].as_str())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    if let Some(q) = input["quoted_text"].as_str().filter(|q| !q.is_empty()) {
        s = format!("> {q}\n\n{s}");
    }
    s
}

/// The thread as chat messages for the seat.
fn chat_messages(items: &[Value]) -> Vec<Value> {
    items
        .iter()
        .filter_map(|it| match it["type"].as_str()? {
            "user_message" => Some(json!({"role": "user", "content": user_text(it)})),
            "assistant_message" => {
                let text: String = it["content"]
                    .as_array()?
                    .iter()
                    .filter_map(|p| p["text"].as_str())
                    .collect();
                Some(json!({"role": "assistant", "content": text}))
            }
            _ => None,
        })
        .collect()
}

fn sse(ev: &Value) -> Result<Bytes, std::convert::Infallible> {
    Ok(Bytes::from(format!("data: {ev}\n\n")))
}

/// Ask the seat for the thread's next assistant message and stream it.
fn respond(
    app: AppState,
    principal: AuthPrincipal,
    headers: HeaderMap,
    tid: String,
    first: Vec<Value>,
    title_from: Option<String>,
) -> Response {
    let owner = principal.agent.id.clone();
    let out = async_stream::stream! {
        for e in &first {
            yield sse(e);
        }
        let Some(t) = thread(&owner, &tid) else { return };
        let model = t.input.iter().rev()
            .find_map(|i| i.pointer("/inference_options/model").and_then(|m| m.as_str()).map(String::from))
            .unwrap_or_else(|| "chatkit".into());
        let mut h = headers;
        if let Ok(v) = axum::http::HeaderValue::from_str(&format!("chatkit:{tid}")) {
            h.insert("x-omni2api-session-id", v);
        }
        h.remove("x-omni2api-mode");
        let req = json!({"model": model, "messages": chat_messages(&t.input), "stream": false});
        let call = async move {
            let req = serde_json::from_value(req).map_err(|e| e.to_string())?;
            let resp = crate::api::openai::chat_completions(State(app), principal, h, Json(req)).await;
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap_or_default();
            let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            if v["object"] == "chat.completion.async" {
                return Err("the seat did not answer in time".to_string());
            }
            v.pointer("/choices/0/message/content").and_then(|c| c.as_str()).map(String::from)
                .ok_or_else(|| v.pointer("/error/message").and_then(|m| m.as_str()).unwrap_or("no answer").to_string())
        };
        tokio::pin!(call);
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(10));
        tick.tick().await;
        let answer = loop {
            tokio::select! {
                _ = tick.tick() => yield Ok(Bytes::from_static(b": keepalive\n\n")),
                a = &mut call => break a,
            }
        };
        let text = match answer {
            Ok(t) => t,
            Err(e) => {
                yield sse(&json!({"type": "error", "code": "custom", "message": e, "allow_retry": true}));
                return;
            }
        };
        let id = uid("msg");
        let at = iso(now());
        let empty = json!({"id": id, "thread_id": tid, "created_at": at, "type": "assistant_message", "content": []});
        yield sse(&json!({"type": "thread.item.added", "item": empty}));
        let part = json!({"type": "output_text", "text": "", "annotations": []});
        yield sse(&json!({"type": "thread.item.updated", "item_id": id, "update": {"type": "assistant_message.content_part.added", "content_index": 0, "content": part}}));
        yield sse(&json!({"type": "thread.item.updated", "item_id": id, "update": {"type": "assistant_message.content_part.text_delta", "content_index": 0, "delta": text}}));
        let part = json!({"type": "output_text", "text": text, "annotations": []});
        yield sse(&json!({"type": "thread.item.updated", "item_id": id, "update": {"type": "assistant_message.content_part.done", "content_index": 0, "content": part}}));
        let mut item = empty;
        item["content"] = json!([part]);
        stored::update(&tid, |s| s.input.push(item.clone()));
        yield sse(&json!({"type": "thread.item.done", "item": item}));
        if let Some(src) = title_from {
            let title: String = src.trim().chars().take(48).collect();
            if !title.is_empty() {
                stored::update(&tid, |s| s.obj["title"] = json!(title));
                if let Some(t) = thread(&owner, &tid) {
                    yield sse(&json!({"type": "thread.updated", "thread": proto_thread(&t, false)}));
                }
            }
        }
    };
    Response::builder()
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .body(Body::from_stream(out))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn user_item(tid: &str, input: &Value) -> Value {
    let mut it = json!({"id": uid("msg"), "thread_id": tid, "created_at": iso(now()), "type": "user_message",
        "content": input["content"].as_array().cloned().unwrap_or_default(), "attachments": [],
        "inference_options": input.get("inference_options").filter(|o| o.is_object()).cloned().unwrap_or(json!({}))});
    if let Some(q) = input["quoted_text"].as_str() {
        it["quoted_text"] = json!(q);
    }
    it
}

/// `POST /v1/chatkit`: one ChatKit protocol request.
pub async fn protocol(
    State(app): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<Value>,
) -> Response {
    let (principal, secret_user) = match caller(&app, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let owner = principal.agent.id.clone();
    let user = if secret_user.is_empty() {
        req.pointer("/metadata/user")
            .and_then(|u| u.as_str())
            .unwrap_or("default")
            .to_string()
    } else {
        secret_user
    };
    let p = &req["params"];
    let mine = |tid: &str| thread(&owner, tid).filter(|t| t.obj["user"] == json!(user));
    let missing = |tid: &str| {
        err(
            StatusCode::NOT_FOUND,
            &format!("No thread found with id '{tid}'."),
        )
    };
    let tid = p["thread_id"].as_str().unwrap_or("").to_string();
    match req["type"].as_str().unwrap_or("") {
        "threads.create" => {
            let input = &p["input"];
            let tid = uid("cthr");
            let obj = json!({"id": tid, "object": "chatkit.thread", "created_at": now(),
                "status": {"type": "active"}, "title": null, "user": user});
            let item = user_item(&tid, input);
            stored::put(
                &tid,
                Stored {
                    owner: owner.clone(),
                    obj,
                    input: vec![item.clone()],
                    history: Vec::new(),
                },
            );
            let t = mine(&tid).expect("just stored");
            let mut created = proto_thread(&t, false);
            created["items"] = json!({"data": [], "has_more": false});
            let first = vec![
                json!({"type": "thread.created", "thread": created}),
                json!({"type": "thread.item.done", "item": item}),
            ];
            respond(app, principal, headers, tid, first, Some(user_text(input)))
        }
        "threads.add_user_message" => {
            if mine(&tid).is_none() {
                return missing(&tid);
            }
            let item = user_item(&tid, &p["input"]);
            stored::update(&tid, |s| s.input.push(item.clone()));
            respond(
                app,
                principal,
                headers,
                tid,
                vec![json!({"type": "thread.item.done", "item": item})],
                None,
            )
        }
        "threads.retry_after_item" => {
            let Some(t) = mine(&tid) else {
                return missing(&tid);
            };
            let iid = p["item_id"].clone();
            let Some(i) = t.input.iter().position(|x| x["id"] == iid) else {
                return err(StatusCode::NOT_FOUND, "no such item");
            };
            if t.input[i]["type"] != "user_message" {
                return err(StatusCode::BAD_REQUEST, "the item is not a user message");
            }
            let removed: Vec<Value> = t.input[i + 1..]
                .iter()
                .map(|x| json!({"type": "thread.item.removed", "item_id": x["id"]}))
                .collect();
            stored::update(&tid, |s| s.input.truncate(i + 1));
            respond(app, principal, headers, tid, removed, None)
        }
        "threads.add_client_tool_output"
        | "threads.add_structured_input"
        | "threads.custom_action" => {
            // The seat never asks for client tools, structured input or
            // widget actions, so there is nothing pending to answer.
            err(
                StatusCode::BAD_REQUEST,
                "nothing in this thread is waiting for that input",
            )
        }
        "threads.get_by_id" => match mine(&tid) {
            Some(t) => Json(proto_thread(&t, true)).into_response(),
            None => missing(&tid),
        },
        "threads.list" => {
            let v: Vec<Value> = stored::list(&owner, "chatkit.thread")
                .into_iter()
                .filter(|(_, s)| s.obj["user"] == json!(user))
                .map(|(_, s)| proto_thread(&s, false))
                .collect();
            Json(proto_page(v, p)).into_response()
        }
        "items.list" => match mine(&tid) {
            Some(t) => Json(proto_page(t.input, p)).into_response(),
            None => missing(&tid),
        },
        "threads.update" => {
            if mine(&tid).is_none() {
                return missing(&tid);
            }
            let title = p["title"].as_str().unwrap_or("").to_string();
            stored::update(&tid, |s| s.obj["title"] = json!(title));
            Json(proto_thread(&mine(&tid).expect("exists"), false)).into_response()
        }
        "threads.delete" => {
            if mine(&tid).is_none() {
                return missing(&tid);
            }
            stored::remove(&owner, &tid);
            Json(json!({})).into_response()
        }
        "items.feedback" => {
            if mine(&tid).is_none() {
                return missing(&tid);
            }
            Json(json!({})).into_response()
        }
        "attachments.create" | "attachments.delete" => err(
            StatusCode::BAD_REQUEST,
            "file upload is not enabled for this ChatKit server",
        ),
        "input.transcribe" => err(
            StatusCode::BAD_REQUEST,
            "dictation is not enabled for this ChatKit server",
        ),
        "threads.sync_custom_action" => Json(json!({})).into_response(),
        other => err(
            StatusCode::BAD_REQUEST,
            &format!("unknown ChatKit request type {other}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_become_chat_and_rest() {
        let u = json!({"id": "m1", "thread_id": "t", "created_at": "2026-11-02T00:00:00Z", "type": "user_message",
            "content": [{"type": "input_text", "text": "hi"}], "quoted_text": "earlier"});
        let a = json!({"id": "m2", "thread_id": "t", "created_at": "2026-11-02T00:00:01Z", "type": "assistant_message",
            "content": [{"type": "output_text", "text": "hello", "annotations": []}]});
        let m = chat_messages(&[u.clone(), a.clone()]);
        assert_eq!(m[0]["content"], "> earlier\n\nhi");
        assert_eq!(m[1]["content"], "hello");
        assert_eq!(rest_item(&u)["type"], "chatkit.user_message");
        assert_eq!(rest_item(&a)["created_at"], 1793577601);
    }

    #[test]
    fn protocol_pages() {
        let items = vec![json!({"id": "a"}), json!({"id": "b"}), json!({"id": "c"})];
        let p = proto_page(items, &json!({"order": "asc", "limit": 2}));
        assert_eq!(p["data"].as_array().unwrap().len(), 2);
        assert_eq!(p["after"], "b");
    }
}
