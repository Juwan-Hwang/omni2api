//! The Agents API (beta): agents, sessions with turns, items and an event
//! stream, plus the objects sessions refer to (environments and their
//! templates, vaults and credentials, automations). The seat is the agent:
//! each turn step sends the session's items as chat history with the
//! agent's instructions and function tools, in one seat conversation per
//! session. Text becomes an assistant message; tool calls put the session
//! in `requires_action` until the caller posts tool results, which go on
//! with the same turn. Everything lives in memory per process, owned by
//! the key's agent.

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
use tokio::sync::broadcast;

use crate::api::stored::{self, Stored};
use crate::auth::AuthPrincipal;
use crate::storage::AppState;

fn now() -> i64 {
    chrono::Utc::now().timestamp()
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

fn bad(msg: &str) -> Response {
    err(StatusCode::BAD_REQUEST, msg)
}

fn of(owner: &str, id: &str, object: &str) -> Option<Stored> {
    stored::get(owner, id).filter(|s| s.obj["object"] == object)
}

fn put(owner: &str, obj: Value) -> Value {
    let id = obj["id"].as_str().unwrap_or("").to_string();
    stored::put(
        &id,
        Stored {
            owner: owner.to_string(),
            obj: obj.clone(),
            input: Vec::new(),
            history: Vec::new(),
        },
    );
    obj
}

#[derive(Debug, serde::Deserialize, Default)]
pub struct ListQuery {
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    after: Option<String>,
    #[serde(default)]
    order: Option<String>,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
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
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let has_more = items.len() > limit;
    items.truncate(limit);
    let id_of = |x: Option<&Value>| x.and_then(|v| v.get("id")).cloned().unwrap_or(Value::Null);
    json!({"object": "list", "data": items, "first_id": id_of(items.first()), "last_id": id_of(items.last()), "has_more": has_more})
}

fn listed(owner: &str, object: &str) -> Vec<Value> {
    stored::list(owner, object)
        .into_iter()
        .map(|(_, s)| s.obj)
        .collect()
}

fn deleted(id: &str, object: &str) -> Response {
    Json(json!({"id": id, "object": object, "deleted": true})).into_response()
}

fn str_map(v: Option<&Value>) -> Value {
    match v {
        Some(Value::Object(m)) => Value::Object(m.clone()),
        _ => json!({}),
    }
}

/// Apply the keys of `b` that are in `keys` onto `obj`; an object's own
/// `id`, `object` and `created_at` are never taken from a body.
fn patch(obj: &mut Value, b: &Value, keys: &[&str]) {
    for k in keys {
        if stored::IMMUTABLE_KEYS.contains(k) {
            continue;
        }
        if let Some(v) = b.get(*k) {
            obj[*k] = v.clone();
        }
    }
}

// ── agents ──

fn tools(v: Option<&Value>) -> Value {
    let list = v.and_then(|t| t.as_array()).cloned().unwrap_or_default();
    Value::Array(
        list.into_iter()
            .map(|mut t| {
                if t["type"] == "function" {
                    if t.get("description").is_none_or(|d| d.is_null()) {
                        t["description"] = json!("");
                    }
                    if t.get("parameters").is_none_or(|d| d.is_null()) {
                        t["parameters"] = json!({"type": "object", "properties": {}});
                    }
                    if t.get("defer_loading").is_none() {
                        t["defer_loading"] = json!(false);
                    }
                }
                t
            })
            .collect(),
    )
}

/// The agent configuration from a request body, over `base`.
fn agent_config(b: &Value, base: Option<&Value>) -> Value {
    let mut a = base.cloned().unwrap_or_else(|| {
        json!({
            "instructions": null, "model": "gpt-5.5", "name": null,
            "multi_agent": {"enabled": false, "max_concurrent_subagents": null},
            "reasoning": {"effort": null, "summary": null}, "service_tier": "auto",
            "text": {"format": {"type": "text"}, "verbosity": "medium"},
            "tools": [], "compaction": null, "experimental": null,
        })
    });
    patch(
        &mut a,
        b,
        &[
            "instructions",
            "model",
            "name",
            "multi_agent",
            "reasoning",
            "service_tier",
            "compaction",
            "experimental",
        ],
    );
    if let Some(t) = b.get("text").filter(|t| t.is_object()) {
        let mut text = a["text"].clone();
        for (k, v) in t.as_object().unwrap() {
            text[k] = v.clone();
        }
        a["text"] = text;
    }
    if b.get("tools").is_some() {
        a["tools"] = tools(b.get("tools"));
    }
    a
}

pub async fn create_agent(principal: AuthPrincipal, Json(b): Json<Value>) -> Response {
    let mut a = agent_config(&b, None);
    a["id"] = json!(uid("agent"));
    a["object"] = json!("agent");
    a["created_at"] = json!(now());
    a["updated_at"] = json!(now());
    a["metadata"] = str_map(b.get("metadata"));
    Json(put(&principal.agent.id, a)).into_response()
}

pub async fn list_agents(principal: AuthPrincipal, Query(q): Query<ListQuery>) -> Response {
    Json(page(listed(&principal.agent.id, "agent"), &q)).into_response()
}

pub async fn get_agent(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    match of(&principal.agent.id, &id, "agent") {
        Some(s) => Json(s.obj).into_response(),
        None => stored::not_found("agent", &id),
    }
}

pub async fn update_agent(
    principal: AuthPrincipal,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> Response {
    let Some(s) = of(&principal.agent.id, &id, "agent") else {
        return stored::not_found("agent", &id);
    };
    let mut a = agent_config(&b, Some(&s.obj));
    if let Some(m) = b.get("metadata") {
        a["metadata"] = str_map(Some(m));
    }
    a["updated_at"] = json!(now());
    stored::update(&id, |s| s.obj = a.clone());
    Json(a).into_response()
}

pub async fn delete_agent(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    if of(&principal.agent.id, &id, "agent").is_none() {
        return stored::not_found("agent", &id);
    }
    stored::remove(&principal.agent.id, &id);
    deleted(&id, "agent.deleted")
}

// ── session runtime ──

struct Live {
    tx: broadcast::Sender<Value>,
}

fn live() -> &'static Mutex<HashMap<String, Live>> {
    static L: OnceLock<Mutex<HashMap<String, Live>>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(HashMap::new()))
}

fn subscribe(sid: &str) -> broadcast::Receiver<Value> {
    let mut g = live().lock().unwrap();
    g.entry(sid.to_string())
        .or_insert_with(|| Live {
            tx: broadcast::channel(4096).0,
        })
        .tx
        .subscribe()
}

fn emit(sid: &str, mut ev: Value) {
    ev["event_id"] = json!(uid("event"));
    if let Some(l) = live().lock().unwrap().get(sid) {
        let _ = l.tx.send(ev);
    }
}

fn session_obj(owner: &str, sid: &str) -> Value {
    of(owner, sid, "agent.session")
        .map(|s| s.obj)
        .unwrap_or(Value::Null)
}

fn emit_session(owner: &str, sid: &str, kind: &str) {
    emit(
        sid,
        json!({"type": kind, "session": session_obj(owner, sid)}),
    );
}

fn usage(input: i64, output: i64) -> Value {
    json!({"input_tokens": input, "input_tokens_details": {"cached_tokens": 0},
           "output_tokens": output, "output_tokens_details": {"reasoning_tokens": 0},
           "total_tokens": input + output})
}

fn set_turn(sid: &str, turn_id: &str, f: impl FnOnce(&mut Value)) -> Value {
    let mut out = Value::Null;
    stored::update(sid, |s| {
        if let Some(t) = s.history.iter_mut().find(|t| t["id"] == json!(turn_id)) {
            f(t);
            out = t.clone();
        }
    });
    out
}

fn add_item(sid: &str, item: &Value) {
    stored::update(sid, |s| s.input.push(item.clone()));
}

/// The session's items as chat messages for the seat.
fn chat_body(session: &Stored) -> Value {
    let agent = &session.obj["agent"];
    let mut messages: Vec<Value> = Vec::new();
    let mut system = agent["instructions"].as_str().unwrap_or("").to_string();
    let env = &session.obj["environment"];
    if env["type"] == "self_hosted" {
        if let Some(d) = env["workspace_directory"].as_str() {
            system.push_str(&format!("\n\nWorkspace: {d}"));
        }
    }
    if !system.trim().is_empty() {
        messages.push(json!({"role": "system", "content": system.trim()}));
    }
    for it in &session.input {
        match it["type"].as_str().unwrap_or("") {
            "message" if it["role"] == "assistant" => {
                let text: String = it["content"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|p| p["text"].as_str()).collect())
                    .unwrap_or_default();
                messages.push(json!({"role": "assistant", "content": text}));
            }
            "message" => {
                let mut parts = Vec::new();
                for p in it["content"].as_array().cloned().unwrap_or_default() {
                    match p["type"].as_str().unwrap_or("") {
                        "input_text" | "text" => {
                            parts.push(json!({"type": "text", "text": p["text"]}))
                        }
                        "input_image" => parts.push(
                            json!({"type": "image_url", "image_url": {"url": p["image_url"]}}),
                        ),
                        _ => {}
                    }
                }
                messages.push(json!({"role": "user", "content": parts}));
            }
            "function_call" => {
                let args = match &it["arguments"] {
                    Value::String(s) => s.clone(),
                    v => v.to_string(),
                };
                let call = json!({"id": it["call_id"], "type": "function",
                                  "function": {"name": it["name"], "arguments": args}});
                match messages.last_mut() {
                    Some(m) if m["role"] == "assistant" && m["tool_calls"].is_array() => {
                        m["tool_calls"].as_array_mut().unwrap().push(call);
                    }
                    _ => messages
                        .push(json!({"role": "assistant", "content": null, "tool_calls": [call]})),
                }
            }
            "function_call_output" => {
                let content = match &it["output"] {
                    Value::String(s) => s.clone(),
                    Value::Array(a) => a
                        .iter()
                        .filter_map(|p| p["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                    Value::Null => it["error"]
                        .as_str()
                        .map(|e| format!("error: {e}"))
                        .unwrap_or_default(),
                    v => v.to_string(),
                };
                messages.push(
                    json!({"role": "tool", "tool_call_id": it["call_id"], "content": content}),
                );
            }
            _ => {}
        }
    }
    let fns: Vec<Value> = agent["tools"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|t| t["type"] == "function")
        .map(|t| json!({"type": "function", "function": {"name": t["name"], "description": t["description"], "parameters": t["parameters"]}}))
        .collect();
    let mut body = json!({"model": agent["model"], "messages": messages, "stream": false});
    if !fns.is_empty() {
        body["tools"] = json!(fns);
    }
    let fmt = &agent["text"]["format"];
    if fmt["type"] == "json_schema" {
        let schema = fmt
            .get("schema")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object"}));
        body["response_format"] = json!({"type": "json_schema", "json_schema": {
            "name": fmt["name"].as_str().unwrap_or("output"), "schema": schema,
            "strict": fmt.get("strict").cloned().unwrap_or(Value::Null)}});
    } else if fmt["type"] == "json_object" {
        body["response_format"] = json!({"type": "json_object"});
    }
    body
}

fn fail_turn(owner: &str, sid: &str, turn_id: &str, msg: &str) {
    let turn = set_turn(sid, turn_id, |t| {
        t["status"] = json!("failed");
        t["completed_at"] = json!(now());
        t["error"] = json!({"code": "server_error", "message": msg});
    });
    emit(
        sid,
        json!({"type": "agent.session.turn.failed", "session_id": sid, "turn_id": turn_id, "turn": turn}),
    );
    stored::update(sid, |s| {
        s.obj["status"] = json!("idle");
        s.obj["error"] = Value::Null;
        s.obj["required_actions"] = json!([]);
        s.obj["last_active_at"] = json!(now());
    });
    emit(
        sid,
        json!({"type": "agent.session.error", "session_id": sid, "turn_id": turn_id, "error": {"code": "server_error", "message": msg}}),
    );
    emit_session(owner, sid, "agent.session.idle");
}

/// One seat step of a turn.
async fn step(
    app: AppState,
    principal: AuthPrincipal,
    headers: HeaderMap,
    sid: String,
    turn_id: String,
) {
    let owner = principal.agent.id.clone();
    let Some(session) = of(&owner, &sid, "agent.session") else {
        return;
    };
    let body = chat_body(&session);
    let mut h = headers;
    if let Ok(v) = axum::http::HeaderValue::from_str(&format!("agent-session:{sid}")) {
        h.insert("x-omni2api-session-id", v);
    }
    h.remove("x-omni2api-mode");
    let req = match serde_json::from_value(body) {
        Ok(r) => r,
        Err(e) => return fail_turn(&owner, &sid, &turn_id, &e.to_string()),
    };
    let resp = crate::api::openai::chat_completions(State(app), principal, h, Json(req)).await;
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap_or_default();
    let v: Value =
        serde_json::from_str(String::from_utf8_lossy(&bytes).trim()).unwrap_or(Value::Null);
    if v["object"] == "chat.completion.async" {
        return fail_turn(&owner, &sid, &turn_id, "the seat did not answer in time");
    }
    let Some(msg) = v.pointer("/choices/0/message").cloned() else {
        let m = v
            .pointer("/error/message")
            .and_then(|m| m.as_str())
            .unwrap_or("no answer");
        return fail_turn(&owner, &sid, &turn_id, m);
    };
    let tin = v
        .pointer("/usage/prompt_tokens")
        .and_then(|x| x.as_i64())
        .unwrap_or(0);
    let tout = v
        .pointer("/usage/completion_tokens")
        .and_then(|x| x.as_i64())
        .unwrap_or(0);
    let text = msg["content"].as_str().unwrap_or("").to_string();
    let calls = msg["tool_calls"].as_array().cloned().unwrap_or_default();
    let mut index = of(&owner, &sid, "agent.session")
        .map(|s| {
            s.input
                .iter()
                .filter(|i| i["turn_id"] == json!(turn_id) && i["role"] != "user")
                .count()
        })
        .unwrap_or(0);
    let base = |id: &str, index: usize| json!({"session_id": sid, "turn_id": turn_id, "item_id": id, "output_index": index, "content_index": 0});
    let with = |mut v: Value, extra: Value| {
        if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
            for (k, x) in e {
                o.insert(k.clone(), x.clone());
            }
        }
        v
    };
    if !text.is_empty() {
        let id = uid("msg");
        let phase = if calls.is_empty() {
            "final_answer"
        } else {
            "commentary"
        };
        let mut item = json!({"id": id, "type": "message", "role": "assistant", "phase": phase,
            "status": "in_progress", "content": [], "turn_id": turn_id});
        emit(
            &sid,
            json!({"type": "agent.session.turn.item.added", "session_id": sid, "turn_id": turn_id, "output_index": index, "item": item}),
        );
        let part = json!({"type": "output_text", "text": ""});
        emit(
            &sid,
            with(
                base(&id, index),
                json!({"type": "agent.session.turn.content_part.added", "part": part}),
            ),
        );
        emit(
            &sid,
            with(
                base(&id, index),
                json!({"type": "agent.session.turn.output_text.delta", "delta": text}),
            ),
        );
        emit(
            &sid,
            with(
                base(&id, index),
                json!({"type": "agent.session.turn.output_text.done", "text": text}),
            ),
        );
        let part = json!({"type": "output_text", "text": text});
        emit(
            &sid,
            with(
                base(&id, index),
                json!({"type": "agent.session.turn.content_part.done", "part": part}),
            ),
        );
        item["content"] = json!([part]);
        item["status"] = json!("completed");
        add_item(&sid, &item);
        emit(
            &sid,
            json!({"type": "agent.session.turn.item.done", "session_id": sid, "turn_id": turn_id, "output_index": index, "item": item}),
        );
        index += 1;
    }
    let mut actions = Vec::new();
    for c in &calls {
        let raw = c
            .pointer("/function/arguments")
            .and_then(|a| a.as_str())
            .unwrap_or("{}");
        let args: Value = serde_json::from_str(raw).unwrap_or_else(|_| json!(raw));
        let name = c.pointer("/function/name").cloned().unwrap_or(Value::Null);
        let item = json!({"id": uid("fc"), "type": "function_call", "call_id": c["id"], "name": name,
            "arguments": args, "status": "completed", "turn_id": turn_id});
        add_item(&sid, &item);
        emit(
            &sid,
            json!({"type": "agent.session.turn.item.added", "session_id": sid, "turn_id": turn_id, "output_index": index, "item": item}),
        );
        emit(
            &sid,
            json!({"type": "agent.session.turn.item.done", "session_id": sid, "turn_id": turn_id, "output_index": index, "item": item}),
        );
        actions.push(json!({"type": "function_call", "call_id": c["id"], "name": name, "arguments": args, "turn_id": turn_id}));
        index += 1;
    }
    let add_usage = |u: &Value| {
        let g = |k: &str| u[k].as_i64().unwrap_or(0);
        usage(g("input_tokens") + tin, g("output_tokens") + tout)
    };
    if !actions.is_empty() {
        set_turn(&sid, &turn_id, |t| {
            t["status"] = json!("waiting");
            t["usage"] = add_usage(&t["usage"]);
        });
        stored::update(&sid, |s| {
            s.obj["status"] = json!("requires_action");
            s.obj["required_actions"] = json!(actions);
            s.obj["usage"] = add_usage(&s.obj["usage"]);
            s.obj["last_active_at"] = json!(now());
        });
        emit_session(&owner, &sid, "agent.session.requires_action");
        return;
    }
    let turn = set_turn(&sid, &turn_id, |t| {
        t["status"] = json!("completed");
        t["completed_at"] = json!(now());
        t["usage"] = add_usage(&t["usage"]);
    });
    emit(
        &sid,
        json!({"type": "agent.session.turn.completed", "session_id": sid, "turn_id": turn_id, "turn": turn, "usage": turn["usage"]}),
    );
    stored::update(&sid, |s| {
        s.obj["status"] = json!("idle");
        s.obj["required_actions"] = json!([]);
        s.obj["usage"] = add_usage(&s.obj["usage"]);
        s.obj["last_active_at"] = json!(now());
    });
    emit_session(&owner, &sid, "agent.session.idle");
}

fn spawn_step(
    app: &AppState,
    principal: &AuthPrincipal,
    headers: &HeaderMap,
    sid: &str,
    turn_id: &str,
) {
    let (app, principal, headers) = (app.clone(), principal.clone(), headers.clone());
    let (s, t) = (sid.to_string(), turn_id.to_string());
    let key = sid.to_string();
    let task = tokio::spawn(async move {
        step(app, principal, headers, s.clone(), t).await;
        stored::done_running(&s);
    });
    stored::set_running(&key, task.abort_handle());
}

/// Normalize input messages (a string, or a list of user messages).
fn input_messages(v: &Value) -> Result<Vec<Value>, String> {
    match v {
        Value::Null => Ok(Vec::new()),
        Value::String(s) => Ok(vec![
            json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": s}]}),
        ]),
        Value::Array(a) => a
            .iter()
            .map(|m| {
                let content = match &m["content"] {
                    Value::String(s) => json!([{"type": "input_text", "text": s}]),
                    Value::Array(p) => Value::Array(p.clone()),
                    _ => return Err("each input message needs content".to_string()),
                };
                Ok(json!({"type": "message", "role": "user", "content": content}))
            })
            .collect(),
        _ => Err("input must be a string or a list of messages".into()),
    }
}

/// Start a turn with these user messages.
fn start_turn(
    app: &AppState,
    principal: &AuthPrincipal,
    headers: &HeaderMap,
    sid: &str,
    msgs: Vec<Value>,
) {
    let owner = principal.agent.id.clone();
    let turn_id = uid("turn");
    let agent_id = session_obj(&owner, sid)["agent"]["id"].clone();
    let turn = json!({"id": turn_id, "object": "agent.session.turn", "session_id": sid, "agent_id": agent_id,
        "created_at": now(), "started_at": now(), "completed_at": null, "status": "in_progress",
        "error": null, "subagent_id": null, "usage": usage(0, 0)});
    stored::update(sid, |s| {
        s.history.push(turn.clone());
        s.obj["status"] = json!("in_progress");
        s.obj["last_active_at"] = json!(now());
    });
    let mut queued = turn.clone();
    queued["status"] = json!("queued");
    emit(
        sid,
        json!({"type": "agent.session.turn.created", "session_id": sid, "turn_id": turn_id, "turn": queued}),
    );
    emit_session(&owner, sid, "agent.session.in_progress");
    emit(
        sid,
        json!({"type": "agent.session.turn.in_progress", "session_id": sid, "turn_id": turn_id, "turn": turn}),
    );
    for mut m in msgs {
        m["id"] = json!(uid("msg"));
        m["status"] = json!("completed");
        m["turn_id"] = json!(turn_id);
        add_item(sid, &m);
        emit(
            sid,
            json!({"type": "agent.session.turn.item.added", "session_id": sid, "turn_id": turn_id, "item": m}),
        );
    }
    spawn_step(app, principal, headers, sid, &turn_id);
}

// ── sessions ──

fn environment(v: &Value) -> Result<Value, String> {
    let ty = v["type"].as_str().unwrap_or("");
    let list = |k: &str| {
        v.get(k)
            .filter(|x| x.is_array())
            .cloned()
            .unwrap_or(json!([]))
    };
    match ty {
        "none" => Ok(json!({"type": "none"})),
        "self_hosted" => {
            let Some(dir) = v["workspace_directory"].as_str() else {
                return Err("environment.workspace_directory is required".into());
            };
            Ok(
                json!({"id": uid("env"), "type": "self_hosted", "workspace_directory": dir,
                "capability_directories": list("capability_directories"), "remote_url": ""}),
            )
        }
        "openai_hosted" => {
            let id = v["environment_id"]
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| uid("env"));
            let net = v
                .get("network")
                .filter(|n| n.is_object())
                .cloned()
                .unwrap_or(json!({"access": "enabled"}));
            let pk = v.get("packages").cloned().unwrap_or(json!({}));
            let arr = |k: &str| {
                pk.get(k)
                    .filter(|x| x.is_array())
                    .cloned()
                    .unwrap_or(json!([]))
            };
            Ok(json!({"id": id, "type": "openai_hosted",
                "capability_directories": list("capability_directories"), "files": list("files"),
                "network": {"access": net["access"].as_str().unwrap_or("enabled"),
                            "allowed_domains": net.get("allowed_domains").filter(|x| x.is_array()).cloned().unwrap_or(json!([]))},
                "packages": {"npm": arr("npm"), "python": arr("python"), "system": arr("system")},
                "plugins": list("plugins"), "skills": list("skills"),
                "container_size": v.get("container_size").cloned().unwrap_or(json!("small")),
                "desktop": v.get("desktop").cloned().unwrap_or(Value::Null)}))
        }
        _ => Err("environment.type must be none, openai_hosted or self_hosted".into()),
    }
}

/// Build a session from a create body (also used by automations).
fn new_session(owner: &str, b: &Value) -> Result<Value, Response> {
    let Some(env) = b.get("environment") else {
        return Err(bad("environment is required"));
    };
    let env = environment(env).map_err(|e| bad(&e))?;
    let mut agent = match b.get("agent_id").and_then(|a| a.as_str()) {
        Some(aid) => match of(owner, aid, "agent") {
            Some(s) => {
                let mut a = s.obj;
                if let Some(o) = a.as_object_mut() {
                    for k in ["object", "created_at", "updated_at", "metadata"] {
                        o.remove(k);
                    }
                }
                a
            }
            None => return Err(stored::not_found("agent", aid)),
        },
        None => {
            let mut a = agent_config(&json!({}), None);
            a["id"] = json!(uid("agent"));
            a
        }
    };
    if let Some(over) = b.get("agent").filter(|a| a.is_object()) {
        let id = agent["id"].clone();
        agent = agent_config(over, Some(&agent));
        agent["id"] = id;
    }
    let sid = uid("sess");
    let obj = json!({"id": sid, "object": "agent.session", "agent": agent, "created_at": now(),
        "environment": env, "error": null, "last_active_at": now(), "metadata": str_map(b.get("metadata")),
        "required_actions": [], "status": "idle", "usage": usage(0, 0),
        "vault_ids": b.get("vault_ids").filter(|v| v.is_array()).cloned().unwrap_or(json!([])),
        "review": b.get("review").cloned().unwrap_or(json!("none")),
        "spend_control": b.get("spend_control").and_then(|s| s.get("limit")).filter(|l| !l.is_null())
            .map(|l| json!({"limit": l, "consumed": 0})).unwrap_or(Value::Null)});
    put(owner, obj.clone());
    crate::api::webhooks::fire(owner, "agent.session.created", &sid);
    Ok(obj)
}

fn sse(ev: &Value) -> Result<Bytes, std::convert::Infallible> {
    Ok(Bytes::from(format!("data: {ev}\n\n")))
}

/// Follow a session's events. `until_turn`: end once a turn has ended and
/// the session is idle again (or failed); otherwise follow until the
/// caller hangs up.
fn follow(mut rx: broadcast::Receiver<Value>, first: Vec<Value>, until_turn: bool) -> Response {
    let out = async_stream::stream! {
        let mut turn_ended = false;
        for e in &first {
            yield sse(e);
        }
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(15));
        tick.tick().await;
        loop {
            tokio::select! {
                _ = tick.tick() => yield Ok(Bytes::from_static(b": keepalive\n\n")),
                ev = rx.recv() => match ev {
                    Ok(ev) => {
                        let kind = ev["type"].as_str().unwrap_or("").to_string();
                        yield sse(&ev);
                        if kind.starts_with("agent.session.turn.") && (kind.ends_with(".completed") || kind.ends_with(".failed") || kind.ends_with(".cancelled")) {
                            turn_ended = true;
                        }
                        if until_turn && (kind == "agent.session.failed" || (kind == "agent.session.idle" && turn_ended)) {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                },
            }
        }
    };
    Response::builder()
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .body(Body::from_stream(out))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

pub async fn create_session(
    State(app): State<AppState>,
    principal: AuthPrincipal,
    headers: HeaderMap,
    Json(b): Json<Value>,
) -> Response {
    let owner = principal.agent.id.clone();
    let msgs = match input_messages(b.get("input").unwrap_or(&Value::Null)) {
        Ok(m) => m,
        Err(e) => return bad(&e),
    };
    let obj = match new_session(&owner, &b) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let sid = obj["id"].as_str().unwrap_or("").to_string();
    let rx = subscribe(&sid);
    let stream = b.get("stream").and_then(|s| s.as_bool()) == Some(true);
    let has_input = !msgs.is_empty();
    if has_input {
        start_turn(&app, &principal, &headers, &sid, msgs);
    }
    if stream {
        let created =
            json!({"type": "agent.session.created", "event_id": uid("event"), "session": obj});
        let mut first = vec![created];
        if !has_input {
            first.push(
                json!({"type": "agent.session.idle", "event_id": uid("event"), "session": obj}),
            );
            return follow_done(first);
        }
        return follow(rx, first, true);
    }
    Json(session_obj(&owner, &sid)).into_response()
}

fn follow_done(first: Vec<Value>) -> Response {
    let body: String = first.iter().map(|e| format!("data: {e}\n\n")).collect();
    Response::builder()
        .header("Content-Type", "text/event-stream")
        .body(Body::from(body))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

pub async fn list_sessions(principal: AuthPrincipal, Query(q): Query<ListQuery>) -> Response {
    let mut v = listed(&principal.agent.id, "agent.session");
    if let Some(a) = &q.agent_id {
        v.retain(|s| s["agent"]["id"] == json!(a));
    }
    Json(page(v, &q)).into_response()
}

pub async fn get_session(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    match of(&principal.agent.id, &id, "agent.session") {
        Some(s) => Json(s.obj).into_response(),
        None => stored::not_found("session", &id),
    }
}

pub async fn update_session(
    principal: AuthPrincipal,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> Response {
    let Some(s) = of(&principal.agent.id, &id, "agent.session") else {
        return stored::not_found("session", &id);
    };
    let mut o = s.obj;
    if let Some(a) = b.get("agent").filter(|a| a.is_object()) {
        let aid = o["agent"]["id"].clone();
        o["agent"] = agent_config(a, Some(&o["agent"]));
        o["agent"]["id"] = aid;
    }
    if let Some(m) = b.get("metadata") {
        o["metadata"] = str_map(Some(m));
    }
    patch(&mut o, &b, &["review"]);
    if let Some(l) = b.get("spend_control").and_then(|s| s.get("limit")) {
        o["spend_control"] = if l.is_null() {
            Value::Null
        } else {
            json!({"limit": l, "consumed": 0})
        };
    }
    stored::update(&id, |s| s.obj = o.clone());
    Json(o).into_response()
}

pub async fn delete_session(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    if of(&principal.agent.id, &id, "agent.session").is_none() {
        return stored::not_found("session", &id);
    }
    stored::abort_running(&id);
    stored::remove(&principal.agent.id, &id);
    live().lock().unwrap().remove(&id);
    deleted(&id, "agent.session.deleted")
}

/// `POST /v1/agents/sessions/{id}/events`: messages, tool results, cancel.
pub async fn create_events(
    State(app): State<AppState>,
    principal: AuthPrincipal,
    headers: HeaderMap,
    Path(sid): Path<String>,
    Json(b): Json<Value>,
) -> Response {
    let owner = principal.agent.id.clone();
    let Some(events) = b.get("events").and_then(|e| e.as_array()).cloned() else {
        return bad("events is required");
    };
    for ev in events {
        let Some(s) = of(&owner, &sid, "agent.session") else {
            return stored::not_found("session", &sid);
        };
        match ev["type"].as_str().unwrap_or("") {
            "agent.session.input.message" => {
                if s.obj["status"] != "idle" {
                    return err(
                        StatusCode::CONFLICT,
                        "the session is busy: wait for agent.session.idle, or cancel the turn",
                    );
                }
                let msgs = match input_messages(&ev["input"]) {
                    Ok(m) if !m.is_empty() => m,
                    Ok(_) => return bad("input is empty"),
                    Err(e) => return bad(&e),
                };
                start_turn(&app, &principal, &headers, &sid, msgs);
            }
            "agent.session.input.tool_result" => {
                let call_id = ev["call_id"].clone();
                let actions = s.obj["required_actions"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let Some(action) = actions.iter().find(|a| a["call_id"] == call_id).cloned() else {
                    return bad(&format!("no pending tool call {call_id}"));
                };
                let turn_id = action["turn_id"].as_str().unwrap_or("").to_string();
                let ok = ev["success"].as_bool().unwrap_or(true);
                let item = json!({"id": uid("fco"), "type": "function_call_output", "call_id": call_id,
                    "output": ev.get("output").cloned().unwrap_or(Value::Null),
                    "error": ev.get("error").cloned().unwrap_or(Value::Null),
                    "status": if ok { "completed" } else { "failed" }, "turn_id": turn_id});
                add_item(&sid, &item);
                emit(
                    &sid,
                    json!({"type": "agent.session.turn.item.added", "session_id": sid, "turn_id": turn_id, "item": item}),
                );
                let rest: Vec<Value> = actions
                    .into_iter()
                    .filter(|a| a["call_id"] != call_id)
                    .collect();
                let done = rest.is_empty();
                stored::update(&sid, |s| {
                    s.obj["required_actions"] = json!(rest);
                    if done {
                        s.obj["status"] = json!("in_progress");
                    }
                    s.obj["last_active_at"] = json!(now());
                });
                if done {
                    let turn = set_turn(&sid, &turn_id, |t| t["status"] = json!("in_progress"));
                    emit_session(&owner, &sid, "agent.session.in_progress");
                    emit(
                        &sid,
                        json!({"type": "agent.session.turn.in_progress", "session_id": sid, "turn_id": turn_id, "turn": turn}),
                    );
                    spawn_step(&app, &principal, &headers, &sid, &turn_id);
                }
            }
            "agent.session.input.cancel" => {
                stored::abort_running(&sid);
                let open = s
                    .history
                    .iter()
                    .rev()
                    .find(|t| {
                        matches!(
                            t["status"].as_str(),
                            Some("queued" | "in_progress" | "waiting")
                        )
                    })
                    .and_then(|t| t["id"].as_str().map(String::from));
                if let Some(tid) = open {
                    let turn = set_turn(&sid, &tid, |t| {
                        t["status"] = json!("cancelled");
                        t["completed_at"] = json!(now());
                    });
                    emit(
                        &sid,
                        json!({"type": "agent.session.turn.cancelled", "session_id": sid, "turn_id": tid, "turn": turn}),
                    );
                }
                stored::update(&sid, |s| {
                    s.obj["status"] = json!("idle");
                    s.obj["required_actions"] = json!([]);
                });
                emit_session(&owner, &sid, "agent.session.idle");
            }
            "agent.session.input.computer_use_approval_request_result" => {
                return bad("this server's agents do not use the computer, so there is no approval to answer");
            }
            other => return bad(&format!("unsupported event type {other}")),
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

/// `GET /v1/agents/sessions/{id}/events`: the live event stream, opening
/// with the session's current state.
pub async fn stream_events(principal: AuthPrincipal, Path(sid): Path<String>) -> Response {
    let Some(s) = of(&principal.agent.id, &sid, "agent.session") else {
        return stored::not_found("session", &sid);
    };
    let rx = subscribe(&sid);
    let kind = match s.obj["status"].as_str().unwrap_or("idle") {
        "in_progress" => "agent.session.in_progress",
        "requires_action" => "agent.session.requires_action",
        "failed" => "agent.session.failed",
        _ => "agent.session.idle",
    };
    follow(
        rx,
        vec![json!({"type": kind, "event_id": uid("event"), "session": s.obj})],
        false,
    )
}

pub async fn list_items(
    principal: AuthPrincipal,
    Path(sid): Path<String>,
    Query(q): Query<ListQuery>,
) -> Response {
    let Some(s) = of(&principal.agent.id, &sid, "agent.session") else {
        return stored::not_found("session", &sid);
    };
    Json(page(s.input, &q)).into_response()
}

pub async fn list_turns(
    principal: AuthPrincipal,
    Path(sid): Path<String>,
    Query(q): Query<ListQuery>,
) -> Response {
    let Some(s) = of(&principal.agent.id, &sid, "agent.session") else {
        return stored::not_found("session", &sid);
    };
    Json(page(s.history, &q)).into_response()
}

pub async fn get_turn(
    principal: AuthPrincipal,
    Path((sid, tid)): Path<(String, String)>,
) -> Response {
    let Some(s) = of(&principal.agent.id, &sid, "agent.session") else {
        return stored::not_found("session", &sid);
    };
    match s.history.into_iter().find(|t| t["id"] == json!(tid)) {
        Some(t) => Json(t).into_response(),
        None => stored::not_found("turn", &tid),
    }
}

pub async fn list_turn_items(
    principal: AuthPrincipal,
    Path((sid, tid)): Path<(String, String)>,
    Query(q): Query<ListQuery>,
) -> Response {
    let Some(s) = of(&principal.agent.id, &sid, "agent.session") else {
        return stored::not_found("session", &sid);
    };
    if !s.history.iter().any(|t| t["id"] == json!(tid)) {
        return stored::not_found("turn", &tid);
    }
    let v: Vec<Value> = s
        .input
        .into_iter()
        .filter(|i| i["turn_id"] == json!(tid))
        .collect();
    Json(page(v, &q)).into_response()
}

/// Lists a session has but this server never fills: traces, artifacts
/// (the seat works on the caller's own files), subagents.
pub async fn empty_list(
    principal: AuthPrincipal,
    Path(p): Path<HashMap<String, String>>,
) -> Response {
    let sid = p.get("id").cloned().unwrap_or_default();
    if of(&principal.agent.id, &sid, "agent.session").is_none() {
        return stored::not_found("session", &sid);
    }
    Json(
        json!({"object": "list", "data": [], "first_id": null, "last_id": null, "has_more": false}),
    )
    .into_response()
}

/// A subagent, artifact or turn under a subagent: none exist here.
pub async fn none_found(
    principal: AuthPrincipal,
    Path(p): Path<HashMap<String, String>>,
) -> Response {
    let sid = p.get("id").cloned().unwrap_or_default();
    if of(&principal.agent.id, &sid, "agent.session").is_none() {
        return stored::not_found("session", &sid);
    }
    let (what, id) = match (p.get("art"), p.get("sub")) {
        (Some(a), _) => ("artifact", a.clone()),
        (None, Some(s)) => ("subagent", s.clone()),
        _ => ("object", String::new()),
    };
    stored::not_found(what, &id)
}

// ── environments and templates ──

pub async fn create_environment(principal: AuthPrincipal, Json(b): Json<Value>) -> Response {
    let spec = b
        .get("environment")
        .cloned()
        .unwrap_or(json!({"type": "openai_hosted"}));
    let env = match environment(&spec) {
        Ok(e) => e,
        Err(e) => return bad(&e),
    };
    if env["type"] == "none" {
        return bad("environment.type must be openai_hosted or self_hosted");
    }
    // A caller-chosen environment_id must never name an existing object: the
    // stored-object map is keyed by id alone, so a reused id would silently
    // overwrite the other object, whatever its type and owner. insert_new is
    // the atomic check-and-insert (an exists + put pair would race).
    let obj = json!({"id": env["id"], "object": "agent.environment", "type": env["type"],
        "status": "ready", "files": env.get("files").cloned().unwrap_or(json!([])),
        "plugins": env.get("plugins").cloned().unwrap_or(json!([])),
        "skills": env.get("skills").cloned().unwrap_or(json!([])),
        "vault_ids": b.get("vault_ids").cloned().unwrap_or(json!([])), "created_at": now()});
    let id = obj["id"].as_str().unwrap_or("").to_string();
    let fresh = stored::insert_new(
        &id,
        Stored {
            owner: principal.agent.id.clone(),
            obj: obj.clone(),
            input: Vec::new(),
            history: Vec::new(),
        },
    );
    if !fresh {
        return err(StatusCode::CONFLICT, "environment_id is already in use");
    }
    crate::api::webhooks::fire(
        &principal.agent.id,
        "agent.environment.ready",
        obj["id"].as_str().unwrap_or(""),
    );
    Json(obj).into_response()
}

pub async fn list_environments(principal: AuthPrincipal, Query(q): Query<ListQuery>) -> Response {
    let mut v = listed(&principal.agent.id, "agent.environment");
    if let Some(t) = &q.kind {
        v.retain(|e| e["type"] == json!(t));
    }
    Json(page(v, &q)).into_response()
}

pub async fn get_environment(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    match of(&principal.agent.id, &id, "agent.environment") {
        Some(s) => Json(s.obj).into_response(),
        None => stored::not_found("environment", &id),
    }
}

/// `POST /v1/agents/environments/{id}/files`: a file by id or inline data.
pub async fn create_environment_file(
    principal: AuthPrincipal,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> Response {
    let owner = principal.agent.id.clone();
    if of(&owner, &id, "agent.environment").is_none() {
        return stored::not_found("environment", &id);
    }
    let Some(path) = b["path"].as_str() else {
        return bad("path is required");
    };
    let size = match (b["file_id"].as_str(), b["data"].as_str()) {
        (Some(fid), _) => match crate::api::files::get(fid).filter(|f| f.owner == owner) {
            Some(f) => f.bytes.len(),
            None => return stored::not_found("file", fid),
        },
        (None, Some(d)) => match crate::openai_compat::b64_decode(d) {
            Some(b) => b.len(),
            None => return bad("data must be base64"),
        },
        _ => return bad("file_id or data is required"),
    };
    let f = json!({"environment_id": id, "object": "agent.environment.file", "path": path, "size_bytes": size});
    stored::update(&id, |s| {
        s.input.retain(|x| x["path"] != json!(path));
        s.input.push(f.clone());
    });
    Json(f).into_response()
}

pub async fn list_environment_files(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    let Some(s) = of(&principal.agent.id, &id, "agent.environment") else {
        return stored::not_found("environment", &id);
    };
    Json(json!({"object": "list", "data": s.input, "next_page": null})).into_response()
}

const TEMPLATE_KEYS: &[&str] = &[
    "capability_directories",
    "desktop",
    "env",
    "files",
    "name",
    "network",
    "packages",
    "plugins",
    "setup_commands",
    "skills",
];

pub async fn create_template(principal: AuthPrincipal, Json(b): Json<Value>) -> Response {
    let mut t = json!({"id": uid("envtpl"), "object": "agent.environment.template", "created_at": now(),
        "updated_at": now(), "capability_directories": [], "files": [], "name": null,
        "network": {"access": "enabled", "allowed_domains": []},
        "packages": {"npm": [], "python": [], "system": []}, "plugins": [], "skills": [], "desktop": null});
    patch(&mut t, &b, TEMPLATE_KEYS);
    if let Some(o) = t.as_object_mut() {
        // Secrets set as env never come back.
        o.remove("env");
    }
    Json(put(&principal.agent.id, t)).into_response()
}

pub async fn list_templates(principal: AuthPrincipal, Query(q): Query<ListQuery>) -> Response {
    Json(page(
        listed(&principal.agent.id, "agent.environment.template"),
        &q,
    ))
    .into_response()
}

pub async fn get_template(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    match of(&principal.agent.id, &id, "agent.environment.template") {
        Some(s) => Json(s.obj).into_response(),
        None => stored::not_found("environment template", &id),
    }
}

pub async fn update_template(
    principal: AuthPrincipal,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> Response {
    let Some(s) = of(&principal.agent.id, &id, "agent.environment.template") else {
        return stored::not_found("environment template", &id);
    };
    let mut t = s.obj;
    patch(&mut t, &b, TEMPLATE_KEYS);
    if let Some(o) = t.as_object_mut() {
        o.remove("env");
    }
    t["updated_at"] = json!(now());
    stored::update(&id, |s| s.obj = t.clone());
    Json(t).into_response()
}

pub async fn delete_template(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    if of(&principal.agent.id, &id, "agent.environment.template").is_none() {
        return stored::not_found("environment template", &id);
    }
    stored::remove(&principal.agent.id, &id);
    deleted(&id, "agent.environment.template.deleted")
}

// ── vaults and credentials ──

pub async fn create_vault(principal: AuthPrincipal, body: Option<Json<Value>>) -> Response {
    let b = body.map(|Json(b)| b).unwrap_or(json!({}));
    let v = json!({"id": uid("vault"), "object": "vault", "created_at": now(),
        "name": b.get("name").cloned().unwrap_or(Value::Null), "metadata": str_map(b.get("metadata")),
        "status": "active"});
    Json(put(&principal.agent.id, v)).into_response()
}

pub async fn list_vaults(principal: AuthPrincipal, Query(q): Query<ListQuery>) -> Response {
    let mut v = listed(&principal.agent.id, "vault");
    if let Some(st) = &q.status {
        if st != "all" {
            v.retain(|x| x["status"] == json!(st));
        }
    }
    Json(page(v, &q)).into_response()
}

pub async fn get_vault(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    match of(&principal.agent.id, &id, "vault") {
        Some(s) => Json(s.obj).into_response(),
        None => stored::not_found("vault", &id),
    }
}

pub async fn update_vault(
    principal: AuthPrincipal,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> Response {
    let Some(s) = of(&principal.agent.id, &id, "vault") else {
        return stored::not_found("vault", &id);
    };
    let mut v = s.obj;
    patch(&mut v, &b, &["name"]);
    if let Some(m) = b.get("metadata") {
        v["metadata"] = str_map(Some(m));
    }
    stored::update(&id, |s| s.obj = v.clone());
    Json(v).into_response()
}

pub async fn delete_vault(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    if of(&principal.agent.id, &id, "vault").is_none() {
        return stored::not_found("vault", &id);
    }
    stored::remove(&principal.agent.id, &id);
    deleted(&id, "vault.deleted")
}

/// A credential's auth as it is shown: every secret taken out.
///
/// Write-only by design, as in OpenAI's vaults: a secret given to a
/// credential is never returned by any read (create, get, list, update all
/// show this stripped form). This server keeps nothing but the stripped
/// form, so a secret also cannot be vended to a session later; the
/// credential records what was configured, not the secret itself.
fn public_auth(a: &Value) -> Value {
    let mut a = a.clone();
    fn strip(v: &mut Value) {
        if let Some(o) = v.as_object_mut() {
            for k in [
                "access_token",
                "refresh_token",
                "token",
                "secret_value",
                "client_secret",
            ] {
                o.remove(k);
            }
            for (_, x) in o.iter_mut() {
                strip(x);
            }
        }
    }
    strip(&mut a);
    a
}

pub async fn create_credential(
    principal: AuthPrincipal,
    Path(vid): Path<String>,
    Json(b): Json<Value>,
) -> Response {
    if of(&principal.agent.id, &vid, "vault").is_none() {
        return stored::not_found("vault", &vid);
    }
    let Some(auth) = b.get("auth").filter(|a| a.is_object()) else {
        return bad("auth is required");
    };
    let Some(name) = b["name"].as_str() else {
        return bad("name is required");
    };
    let id = uid("cred");
    let c = json!({"id": id, "object": "vault.credential", "vault_id": vid, "name": name,
        "auth": public_auth(auth), "metadata": str_map(b.get("metadata")),
        "created_at": now(), "updated_at": now(), "status": "active"});
    stored::update(&vid, |s| s.input.push(c.clone()));
    Json(c).into_response()
}

fn credential(owner: &str, vid: &str, cid: &str) -> Result<Value, Response> {
    let Some(v) = of(owner, vid, "vault") else {
        return Err(stored::not_found("vault", vid));
    };
    v.input
        .into_iter()
        .find(|c| c["id"] == json!(cid))
        .ok_or_else(|| stored::not_found("credential", cid))
}

pub async fn list_credentials(
    principal: AuthPrincipal,
    Path(vid): Path<String>,
    Query(q): Query<ListQuery>,
) -> Response {
    let Some(v) = of(&principal.agent.id, &vid, "vault") else {
        return stored::not_found("vault", &vid);
    };
    Json(page(v.input, &q)).into_response()
}

pub async fn get_credential(
    principal: AuthPrincipal,
    Path((vid, cid)): Path<(String, String)>,
) -> Response {
    match credential(&principal.agent.id, &vid, &cid) {
        Ok(c) => Json(c).into_response(),
        Err(r) => r,
    }
}

pub async fn update_credential(
    principal: AuthPrincipal,
    Path((vid, cid)): Path<(String, String)>,
    Json(b): Json<Value>,
) -> Response {
    let mut c = match credential(&principal.agent.id, &vid, &cid) {
        Ok(c) => c,
        Err(r) => return r,
    };
    if let Some(a) = b.get("auth").filter(|a| a.is_object()) {
        let mut auth = c["auth"].clone();
        for (k, v) in public_auth(a).as_object().unwrap() {
            auth[k] = v.clone();
        }
        c["auth"] = auth;
    }
    if let Some(m) = b.get("metadata") {
        c["metadata"] = str_map(Some(m));
    }
    c["updated_at"] = json!(now());
    stored::update(&vid, |s| {
        if let Some(x) = s.input.iter_mut().find(|x| x["id"] == json!(cid)) {
            *x = c.clone();
        }
    });
    Json(c).into_response()
}

pub async fn delete_credential(
    principal: AuthPrincipal,
    Path((vid, cid)): Path<(String, String)>,
) -> Response {
    if let Err(r) = credential(&principal.agent.id, &vid, &cid) {
        return r;
    }
    stored::update(&vid, |s| s.input.retain(|x| x["id"] != json!(cid)));
    deleted(&cid, "vault.credential.deleted")
}

// ── automations ──

fn automation_target(owner: &str, t: &Value) -> Result<Value, Response> {
    match t["type"].as_str().unwrap_or("") {
        "existing_session" => {
            let sid = t["session_id"].as_str().unwrap_or("");
            if of(owner, sid, "agent.session").is_none() {
                return Err(stored::not_found("session", sid));
            }
            Ok(json!({"type": "existing_session", "session_id": sid,
                      "instructions": t["instructions"].as_str().unwrap_or("")}))
        }
        "new_session" => {
            if t.get("environment").is_none() {
                return Err(bad("target.environment is required"));
            }
            let input =
                input_messages(t.get("input").unwrap_or(&Value::Null)).map_err(|e| bad(&e))?;
            Ok(
                json!({"type": "new_session", "agent": t.get("agent").cloned().unwrap_or(Value::Null),
                "agent_id": t.get("agent_id").cloned().unwrap_or(Value::Null), "environment": t["environment"],
                "input": input, "metadata": str_map(t.get("metadata")),
                "vault_ids": t.get("vault_ids").cloned().unwrap_or(json!([]))}),
            )
        }
        _ => Err(bad("target.type must be new_session or existing_session")),
    }
}

pub async fn create_automation(
    State(app): State<AppState>,
    principal: AuthPrincipal,
    headers: HeaderMap,
    Json(b): Json<Value>,
) -> Response {
    let owner = principal.agent.id.clone();
    let target = match b.get("target").map(|t| automation_target(&owner, t)) {
        Some(Ok(t)) => t,
        Some(Err(r)) => return r,
        None => return bad("target is required"),
    };
    let Some(name) = b["name"].as_str() else {
        return bad("name is required");
    };
    let mut a = json!({"id": uid("auto"), "object": "agent.automation", "created_at": now(),
        "created_by": {"type": "developer"}, "metadata": str_map(b.get("metadata")), "name": name,
        "status": "active", "target": target});
    match b["type"].as_str().unwrap_or("") {
        "periodic" => {
            let Some(e) = b["expression"].as_str() else {
                return bad("expression is required for a periodic automation");
            };
            if let Err(m) = cron_matches(e, &chrono::Utc::now().naive_utc()) {
                return bad(&format!("expression: {m}"));
            }
            let tz = b["timezone"].as_str().unwrap_or("UTC");
            if tz.parse::<chrono_tz::Tz>().is_err() {
                return bad(&format!("unknown timezone {tz}"));
            }
            a["type"] = json!("periodic");
            a["expression"] = json!(e);
            a["timezone"] = json!(tz);
        }
        "once" => {
            let Some(at) = b["run_at"].as_i64() else {
                return bad("run_at is required for a one-time automation");
            };
            a["type"] = json!("once");
            a["run_at"] = json!(at);
        }
        _ => return bad("type must be periodic or once"),
    }
    let a = put(&owner, a);
    schedule(&a, &app, &principal, &headers);
    Json(a).into_response()
}

// ── the automation clock ──

type Runner = (AppState, AuthPrincipal, HeaderMap, Option<i64>);

/// Who each scheduled automation runs as, and the minute it last fired.
fn runners() -> &'static Mutex<HashMap<String, Runner>> {
    static R: OnceLock<Mutex<HashMap<String, Runner>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn schedule(a: &Value, app: &AppState, principal: &AuthPrincipal, headers: &HeaderMap) {
    let id = a["id"].as_str().unwrap_or("").to_string();
    runners()
        .lock()
        .unwrap()
        .insert(id, (app.clone(), principal.clone(), headers.clone(), None));
    static CLOCK: OnceLock<()> = OnceLock::new();
    CLOCK.get_or_init(|| {
        tokio::spawn(async {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(15));
            loop {
                tick.tick().await;
                clock_tick();
            }
        });
    });
}

fn clock_tick() {
    let now = chrono::Utc::now();
    let minute = now.timestamp() / 60;
    let due: Vec<(String, Runner)> = runners()
        .lock()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    for (id, (app, principal, headers, last)) in due {
        let Some(s) = of(&principal.agent.id, &id, "agent.automation") else {
            runners().lock().unwrap().remove(&id);
            continue;
        };
        let a = s.obj;
        if a["status"] != "active" {
            continue;
        }
        let fire = if a["type"] == "once" {
            a["run_at"].as_i64().is_some_and(|t| t <= now.timestamp())
        } else {
            let tz: chrono_tz::Tz = a["timezone"]
                .as_str()
                .unwrap_or("UTC")
                .parse()
                .unwrap_or(chrono_tz::UTC);
            let local = now.with_timezone(&tz).naive_local();
            last != Some(minute)
                && cron_matches(a["expression"].as_str().unwrap_or(""), &local) == Ok(true)
        };
        if fire {
            if let Some(r) = runners().lock().unwrap().get_mut(&id) {
                r.3 = Some(minute);
            }
            invoke(&app, &principal, &headers, &a, "schedule");
            if a["type"] == "once" {
                runners().lock().unwrap().remove(&id);
            }
        }
    }
}

/// Whether a five-field cron expression (minute hour day month weekday;
/// `*`, `*/n`, `a`, `a-b`, `a-b/n`, lists) matches this minute.
fn cron_matches(expr: &str, t: &chrono::NaiveDateTime) -> Result<bool, String> {
    use chrono::{Datelike, Timelike};
    let f: Vec<&str> = expr.split_whitespace().collect();
    if f.len() != 5 {
        return Err("a cron expression has five fields".into());
    }
    fn field(s: &str, lo: u32, hi: u32, v: u32) -> Result<(bool, bool), String> {
        let mut hit = false;
        for part in s.split(',') {
            let (range, step) = match part.split_once('/') {
                Some((r, n)) => (
                    r,
                    n.parse::<u32>()
                        .map_err(|_| format!("bad step in {part}"))?,
                ),
                None => (part, 1),
            };
            if step == 0 {
                return Err(format!("bad step in {part}"));
            }
            let (a, b) = if range == "*" {
                (lo, hi)
            } else if let Some((a, b)) = range.split_once('-') {
                let p = |x: &str| x.parse::<u32>().map_err(|_| format!("bad value {x}"));
                (p(a)?, p(b)?)
            } else {
                let x = range
                    .parse::<u32>()
                    .map_err(|_| format!("bad value {range}"))?;
                (x, if part.contains('/') { hi } else { x })
            };
            if a < lo || b > hi || a > b {
                return Err(format!("{part} is out of range {lo}-{hi}"));
            }
            if v >= a && v <= b && (v - a).is_multiple_of(step) {
                hit = true;
            }
        }
        Ok((hit, s == "*"))
    }
    let (m, _) = field(f[0], 0, 59, t.minute())?;
    let (h, _) = field(f[1], 0, 23, t.hour())?;
    let (dom, dom_any) = field(f[2], 1, 31, t.day())?;
    let (mon, _) = field(f[3], 1, 12, t.month())?;
    let wd = t.weekday().num_days_from_sunday();
    let (dow0, dow_any) = field(f[4], 0, 7, wd)?;
    let dow = dow0 || (wd == 0 && field(f[4], 0, 7, 7)?.0);
    let day = match (dom_any, dow_any) {
        (true, true) => true,
        (true, false) => dow,
        (false, true) => dom,
        (false, false) => dom || dow,
    };
    Ok(m && h && mon && day)
}

pub async fn list_automations(principal: AuthPrincipal, Query(q): Query<ListQuery>) -> Response {
    let mut v = listed(&principal.agent.id, "agent.automation");
    if let Some(sid) = &q.session_id {
        v.retain(|a| a["target"]["session_id"] == json!(sid));
    }
    Json(page(v, &q)).into_response()
}

pub async fn get_automation(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    match of(&principal.agent.id, &id, "agent.automation") {
        Some(s) => Json(s.obj).into_response(),
        None => stored::not_found("automation", &id),
    }
}

pub async fn update_automation(
    principal: AuthPrincipal,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> Response {
    let owner = principal.agent.id.clone();
    let Some(s) = of(&owner, &id, "agent.automation") else {
        return stored::not_found("automation", &id);
    };
    let mut a = s.obj;
    if let Some(t) = b.get("target") {
        match automation_target(&owner, t) {
            Ok(t) => a["target"] = t,
            Err(r) => return r,
        }
    }
    let keys: &[&str] = if a["type"] == "periodic" {
        &["name", "expression", "timezone"]
    } else {
        &["name", "run_at"]
    };
    patch(&mut a, &b, keys);
    if let Some(m) = b.get("metadata") {
        a["metadata"] = str_map(Some(m));
    }
    stored::update(&id, |s| s.obj = a.clone());
    Json(a).into_response()
}

pub async fn delete_automation(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    if of(&principal.agent.id, &id, "agent.automation").is_none() {
        return stored::not_found("automation", &id);
    }
    stored::remove(&principal.agent.id, &id);
    deleted(&id, "agent.automation.deleted")
}

async fn set_automation_status(owner: &str, id: &str, status: &str) -> Response {
    let Some(s) = of(owner, id, "agent.automation") else {
        return stored::not_found("automation", id);
    };
    let mut a = s.obj;
    a["status"] = json!(status);
    stored::update(id, |s| s.obj = a.clone());
    Json(a).into_response()
}

pub async fn pause_automation(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    set_automation_status(&principal.agent.id, &id, "paused").await
}

pub async fn resume_automation(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    set_automation_status(&principal.agent.id, &id, "active").await
}

/// Run an automation's target now: a turn in a new or existing session.
fn invoke(
    app: &AppState,
    principal: &AuthPrincipal,
    headers: &HeaderMap,
    auto: &Value,
    source: &str,
) -> Value {
    let owner = principal.agent.id.clone();
    let t = &auto["target"];
    let mut inv = json!({"id": uid("autoinv"), "object": "agent.automation.invocation",
        "automation_id": auto["id"], "created_at": now(), "error": null,
        "scheduled_at": null, "session_id": null, "skip_reason": null,
        "source": {"type": source}, "status": "queued"});
    let session = if t["type"] == "existing_session" {
        let sid = t["session_id"].as_str().unwrap_or("").to_string();
        match of(&owner, &sid, "agent.session") {
            Some(s) if s.obj["status"] == "idle" => {
                let text = t["instructions"].as_str().unwrap_or("").to_string();
                Ok((sid, input_messages(&json!(text)).unwrap_or_default()))
            }
            Some(_) => {
                inv["status"] = json!("skipped");
                inv["skip_reason"] = json!("pending_delivery");
                inv["session_id"] = json!(sid);
                Err(None)
            }
            None => Err(Some("the target session is gone".to_string())),
        }
    } else {
        let body = json!({"agent": t["agent"], "agent_id": t["agent_id"], "environment": t["environment"],
                          "metadata": t["metadata"], "vault_ids": t["vault_ids"]});
        let mut body = body;
        if let Some(o) = body.as_object_mut() {
            o.retain(|_, v| !v.is_null());
        }
        match new_session(&owner, &body) {
            Ok(s) => Ok((
                s["id"].as_str().unwrap_or("").to_string(),
                t["input"].as_array().cloned().unwrap_or_default(),
            )),
            Err(_) => Err(Some("the target could not start a session".to_string())),
        }
    };
    match session {
        Ok((sid, msgs)) => {
            subscribe(&sid);
            if !msgs.is_empty() {
                start_turn(app, principal, headers, &sid, msgs);
            }
            inv["session_id"] = json!(sid);
            inv["status"] = json!("completed");
        }
        Err(Some(e)) => {
            inv["status"] = json!("failed");
            inv["error"] = json!({"code": "target_unavailable", "message": e});
        }
        Err(None) => {}
    }
    let aid = auto["id"].as_str().unwrap_or("").to_string();
    stored::update(&aid, |s| s.input.push(inv.clone()));
    if auto["type"] == "once" && source == "schedule" {
        stored::update(&aid, |s| s.obj["status"] = json!("completed"));
    }
    inv
}

pub async fn create_invocation(
    State(app): State<AppState>,
    principal: AuthPrincipal,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let Some(s) = of(&principal.agent.id, &id, "agent.automation") else {
        return stored::not_found("automation", &id);
    };
    Json(invoke(&app, &principal, &headers, &s.obj, "manual")).into_response()
}

pub async fn list_invocations(
    principal: AuthPrincipal,
    Path(id): Path<String>,
    Query(q): Query<ListQuery>,
) -> Response {
    let Some(s) = of(&principal.agent.id, &id, "agent.automation") else {
        return stored::not_found("automation", &id);
    };
    Json(page(s.input, &q)).into_response()
}

pub async fn get_invocation(
    principal: AuthPrincipal,
    Path((id, inv)): Path<(String, String)>,
) -> Response {
    let Some(s) = of(&principal.agent.id, &id, "agent.automation") else {
        return stored::not_found("automation", &id);
    };
    match s.input.into_iter().find(|i| i["id"] == json!(inv)) {
        Some(i) => Json(i).into_response(),
        None => stored::not_found("invocation", &inv),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_never_rewrites_identity() {
        let mut o = json!({"id": "agent_1", "object": "agent", "created_at": 1, "name": "a"});
        patch(
            &mut o,
            &json!({"id": "x", "object": "y", "created_at": 9, "name": "b"}),
            &["id", "object", "created_at", "name"],
        );
        assert_eq!(
            o,
            json!({"id": "agent_1", "object": "agent", "created_at": 1, "name": "b"})
        );
    }

    #[test]
    fn tool_calls_group_and_outputs_follow() {
        let s = Stored {
            owner: "o".into(),
            obj: json!({"agent": agent_config(&json!({"instructions": "be brief",
                "tools": [{"type": "function", "name": "f"}]}), None),
                "environment": {"type": "self_hosted", "workspace_directory": "/w"}}),
            input: vec![
                json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}),
                json!({"type": "function_call", "call_id": "c1", "name": "f", "arguments": {"a": 1}}),
                json!({"type": "function_call", "call_id": "c2", "name": "f", "arguments": {}}),
                json!({"type": "function_call_output", "call_id": "c1", "output": "one"}),
                json!({"type": "function_call_output", "call_id": "c2", "output": [{"type": "input_text", "text": "two"}]}),
            ],
            history: Vec::new(),
        };
        let b = chat_body(&s);
        let m = b["messages"].as_array().unwrap();
        assert!(m[0]["content"].as_str().unwrap().contains("Workspace: /w"));
        assert_eq!(m[2]["tool_calls"].as_array().unwrap().len(), 2);
        assert_eq!(m[2]["tool_calls"][0]["function"]["arguments"], "{\"a\":1}");
        assert_eq!(m[4]["content"], "two");
        assert_eq!(b["tools"][0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn secrets_never_shown() {
        let a = public_auth(
            &json!({"type": "mcp_oauth", "access_token": "x", "mcp_server_url": "u",
            "refresh": {"refresh_token": "y", "client_id": "c"}}),
        );
        assert!(a.get("access_token").is_none());
        assert!(a["refresh"].get("refresh_token").is_none());
        assert_eq!(a["refresh"]["client_id"], "c");
    }

    #[test]
    fn cron() {
        let t = chrono::NaiveDate::from_ymd_opt(2026, 11, 1)
            .unwrap()
            .and_hms_opt(9, 30, 0)
            .unwrap(); // a Sunday
        assert_eq!(cron_matches("30 9 * * *", &t), Ok(true));
        assert_eq!(cron_matches("*/15 9-17 * * 0", &t), Ok(true));
        assert_eq!(cron_matches("30 9 * * 7", &t), Ok(true));
        assert_eq!(cron_matches("30 9 * * 1-5", &t), Ok(false));
        assert_eq!(cron_matches("30 9 2 * 0", &t), Ok(true));
        assert!(cron_matches("61 * * * *", &t).is_err());
        assert!(cron_matches("* * *", &t).is_err());
    }

    #[test]
    fn input_forms() {
        assert_eq!(
            input_messages(&json!("hi")).unwrap()[0]["content"][0]["text"],
            "hi"
        );
        assert!(input_messages(&json!(5)).is_err());
    }
}
