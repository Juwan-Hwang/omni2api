//! Protocol adapters: the Anthropic Messages API (`POST /v1/messages`, what
//! Claude Code speaks) and the OpenAI Responses API (`POST /v1/responses`,
//! the only wire Codex CLI still speaks) in front of the Chat Completions
//! pipeline.
//!
//! Both are thin: translate the request into a `ChatCompletionRequest`, run
//! it through [`chat_completions`] exactly as a Chat Completions caller
//! would (same conversations, seat, tool routing, usage counting), and
//! translate the response stream back. The Anthropic side uses the
//! `anyllm_translate` crate; no maintained library translates *inbound*
//! Responses traffic, so that one is written here.

use axum::body::Bytes;
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use futures::StreamExt;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::api::openai::chat_completions;
use crate::openai_compat::ChatCompletionRequest;
use crate::state::AuthPrincipal;
use crate::AppState;

/// Split an SSE byte stream into `data:` payloads (comments → `None`).
struct SseLines {
    buf: String,
}

impl SseLines {
    fn new() -> Self {
        Self { buf: String::new() }
    }

    /// Feed bytes; return complete events: `Some(data)` for a data event,
    /// `None` for a comment (heartbeat).
    fn feed(&mut self, bytes: &[u8]) -> Vec<Option<String>> {
        self.buf.push_str(&String::from_utf8_lossy(bytes));
        let mut out = Vec::new();
        while let Some(pos) = self.buf.find("\n\n") {
            let block: String = self.buf.drain(..pos + 2).collect();
            let mut data = Vec::new();
            let mut comment = false;
            for line in block.lines() {
                if let Some(d) = line.strip_prefix("data:") {
                    data.push(d.strip_prefix(' ').unwrap_or(d).to_string());
                } else if line.starts_with(':') {
                    comment = true;
                }
            }
            if !data.is_empty() {
                out.push(Some(data.join("\n")));
            } else if comment {
                out.push(None);
            }
        }
        out
    }
}

fn sse_frame(event: &str, data: &Value) -> Bytes {
    Bytes::from(format!("event: {}\ndata: {}\n\n", event, data))
}

fn is_sse(resp: &Response) -> bool {
    resp.headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.starts_with("text/event-stream"))
        .unwrap_or(false)
}

async fn body_json(resp: Response) -> Option<Value> {
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024 * 1024)
        .await
        .ok()?;
    // The non-streaming path pads with newlines while it waits.
    let text = String::from_utf8_lossy(&bytes);
    serde_json::from_str(text.trim()).ok()
}

#[allow(clippy::result_large_err)] // the error is the HTTP response itself, built once
fn chat_request(v: Value) -> Result<ChatCompletionRequest, Response> {
    serde_json::from_value::<ChatCompletionRequest>(v).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": {"message": format!("untranslatable request: {e}"), "type": "invalid_request_error"}})),
        )
            .into_response()
    })
}

// ════════════════════════════════════════════════════════════
//  Anthropic Messages  (POST /v1/messages)
// ════════════════════════════════════════════════════════════

fn anthropic_error(status: StatusCode, kind: &str, message: &str) -> Response {
    (
        status,
        Json(json!({"type": "error", "error": {"type": kind, "message": message}})),
    )
        .into_response()
}

/// Claude Code's ToolSearch answers with `tool_reference` blocks (the tool
/// schema is then added to `tools`) and a bare "Tool loaded." text beside
/// the result. The translator drops the references, so the responder saw an
/// empty result and "Tool loaded." as if the person had typed it. Turn each
/// reference into text naming the tool and drop that filler line.
fn fold_tool_references(body: &mut Value) {
    let Some(msgs) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return;
    };
    for m in msgs {
        let Some(parts) = m.get_mut("content").and_then(|c| c.as_array_mut()) else {
            continue;
        };
        let mut referenced = false;
        for p in parts.iter_mut() {
            if p.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
                continue;
            }
            let Some(inner) = p.get_mut("content").and_then(|c| c.as_array_mut()) else {
                continue;
            };
            for b in inner.iter_mut() {
                if b.get("type").and_then(|t| t.as_str()) == Some("tool_reference") {
                    let name = b
                        .get("tool_name")
                        .or_else(|| b.get("name"))
                        .and_then(|n| n.as_str())
                        .unwrap_or("?")
                        .to_string();
                    *b = json!({"type": "text", "text": format!("loaded tool {name}: callable now")});
                    referenced = true;
                }
            }
        }
        if referenced {
            parts.retain(|p| {
                !(p.get("type").and_then(|t| t.as_str()) == Some("text")
                    && p.get("text").and_then(|t| t.as_str()).map(str::trim)
                        == Some("Tool loaded."))
            });
        }
    }
}

pub async fn messages(
    State(app): State<AppState>,
    principal: AuthPrincipal,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    use anyllm_translate::anthropic::MessageCreateRequest;

    let mut body = body;
    fold_tool_references(&mut body);
    let req: MessageCreateRequest = match serde_json::from_value(body) {
        Ok(r) => r,
        Err(e) => {
            return anthropic_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &e.to_string(),
            )
        }
    };
    let model = req.model.clone();
    let stream = req.stream.unwrap_or(false);
    let config = anyllm_translate::TranslationConfig::builder()
        .passthrough_unknown_models(true)
        .build();
    let openai_req = match anyllm_translate::translate_request(&req, &config) {
        Ok(r) => r,
        Err(e) => {
            return anthropic_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &e.to_string(),
            )
        }
    };
    let mut v = serde_json::to_value(&openai_req).unwrap_or(Value::Null);
    if stream {
        v["stream"] = json!(true);
        v["stream_options"] = json!({"include_usage": true});
    }
    let chat = match chat_request(v) {
        Ok(c) => c,
        Err(r) => return r,
    };
    // Anthropic reports input tokens up front (message_start) and again in
    // message_delta; count them here with the same tokenizer the chat
    // pipeline uses.
    let tokenizer = headers
        .get("x-omni2api-tokenizer")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_string())
        .filter(|v| crate::session::tokenizer_available(v))
        .unwrap_or_else(crate::session::default_tokenizer);
    let input_tokens = crate::session::count_request_tokens(&tokenizer, &chat);

    let resp = chat_completions(State(app), principal, headers, Json(chat)).await;
    let status = resp.status();
    if !status.is_success() {
        let msg = body_json(resp)
            .await
            .and_then(|v| {
                v.pointer("/error/message")
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| "request failed".to_string());
        let kind = if status == StatusCode::CONFLICT {
            "overloaded_error"
        } else {
            "api_error"
        };
        return anthropic_error(status, kind, &msg);
    }

    if !stream || !is_sse(&resp) {
        let Some(v) = body_json(resp).await else {
            return anthropic_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                "unreadable completion",
            );
        };
        return match serde_json::from_value::<anyllm_translate::openai::ChatCompletionResponse>(v) {
            Ok(c) => Json(anyllm_translate::translate_response(&c, &model)).into_response(),
            Err(e) => anthropic_error(StatusCode::BAD_GATEWAY, "api_error", &e.to_string()),
        };
    }

    let mut upstream = resp.into_body().into_data_stream();
    let out = async_stream::stream! {
        use anyllm_translate::openai::ChatCompletionChunk;
        let mut tr = anyllm_translate::new_stream_translator(model.clone());
        let mut lines = SseLines::new();
        // The usage chunk arrives after the finish chunk; the translator
        // closes the message on the finish chunk, so hold it until we know
        // whether usage follows.
        let mut held: Option<ChatCompletionChunk> = None;
        let emit = |events: Vec<anyllm_translate::anthropic::StreamEvent>| -> Vec<Bytes> {
            events
                .into_iter()
                .map(|e| {
                    let mut v = serde_json::to_value(&e).unwrap_or(Value::Null);
                    let name = v.get("type").and_then(|t| t.as_str()).unwrap_or("message").to_string();
                    match name.as_str() {
                        "message_start" => {
                            if let Some(u) = v.pointer_mut("/message/usage") {
                                u["input_tokens"] = json!(input_tokens);
                            }
                        }
                        "message_delta" => {
                            if let Some(u) = v.get_mut("usage").filter(|u| u.is_object()) {
                                u["input_tokens"] = json!(input_tokens);
                            }
                        }
                        _ => {}
                    }
                    sse_frame(&name, &v)
                })
                .collect()
        };
        'outer: while let Some(chunk) = upstream.next().await {
            let Ok(bytes) = chunk else { break };
            for item in lines.feed(&bytes) {
                match item {
                    None => {
                        yield Ok::<Bytes, std::convert::Infallible>(sse_frame("ping", &json!({"type": "ping"})));
                    }
                    Some(data) if data.trim() == "[DONE]" => {
                        if let Some(h) = held.take() {
                            for b in emit(tr.process_chunk(&h)) { yield Ok(b); }
                        }
                        for b in emit(tr.finish()) { yield Ok(b); }
                        break 'outer;
                    }
                    Some(data) => {
                        let Ok(mut c) = serde_json::from_str::<ChatCompletionChunk>(&data) else { continue };
                        if let Some(mut h) = held.take() {
                            if c.choices.is_empty() && c.usage.is_some() {
                                h.usage = c.usage.take();
                                for b in emit(tr.process_chunk(&h)) { yield Ok(b); }
                                continue;
                            }
                            for b in emit(tr.process_chunk(&h)) { yield Ok(b); }
                        }
                        if c.choices.iter().any(|ch| ch.finish_reason.is_some()) {
                            held = Some(c);
                        } else {
                            for b in emit(tr.process_chunk(&c)) { yield Ok(b); }
                        }
                    }
                }
            }
        }
    };

    Response::builder()
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .header("X-Accel-Buffering", "no")
        .body(Body::from_stream(out))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// `POST /v1/messages/count_tokens` — counted with the server's tokenizer.
pub async fn count_tokens(
    _principal: AuthPrincipal,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let tokenizer = headers
        .get("x-omni2api-tokenizer")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_string())
        .filter(|v| crate::session::tokenizer_available(v))
        .unwrap_or_else(crate::session::default_tokenizer);
    let mut n = 0u32;
    if let Some(sys) = body.get("system") {
        n += crate::session::count_tokens_with(&tokenizer, &value_text(sys));
    }
    if let Some(msgs) = body.get("messages").and_then(|m| m.as_array()) {
        for m in msgs {
            n += 4 + crate::session::count_tokens_with(
                &tokenizer,
                &value_text(m.get("content").unwrap_or(&Value::Null)),
            );
        }
    }
    if let Some(t) = body.get("tools") {
        n += crate::session::count_tokens_with(&tokenizer, &t.to_string());
    }
    Json(json!({"input_tokens": n})).into_response()
}

/// Text of a string or content-block array.
fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .map(|b| {
                b.get("text")
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| b.to_string())
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

// ════════════════════════════════════════════════════════════
//  OpenAI Responses  (POST /v1/responses)
// ════════════════════════════════════════════════════════════

fn responses_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(json!({"error": {"message": message, "type": "invalid_request_error"}})),
    )
        .into_response()
}

/// Content of a Responses message item as chat content (string or parts).
fn responses_content(content: &Value) -> Value {
    match content {
        Value::String(s) => Value::String(s.clone()),
        Value::Array(parts) => {
            let mut out = Vec::new();
            let mut only_text = true;
            for p in parts {
                match p.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                    "input_text" | "output_text" | "text" | "summary_text" => {
                        out.push(json!({"type": "text", "text": p.get("text").cloned().unwrap_or(json!(""))}));
                    }
                    "input_image" => {
                        only_text = false;
                        let url = p
                            .get("image_url")
                            .and_then(|u| {
                                u.as_str().map(|s| s.to_string()).or_else(|| {
                                    u.get("url").and_then(|x| x.as_str()).map(|s| s.to_string())
                                })
                            })
                            .unwrap_or_default();
                        out.push(json!({"type": "image_url", "image_url": {"url": url}}));
                    }
                    "refusal" => {
                        out.push(json!({"type": "text", "text": p.get("refusal").cloned().unwrap_or(json!(""))}));
                    }
                    _ => {}
                }
            }
            if only_text {
                Value::String(
                    out.iter()
                        .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                        .collect::<Vec<_>>()
                        .join("\n"),
                )
            } else {
                Value::Array(out)
            }
        }
        _ => Value::String(String::new()),
    }
}

/// One flat function name for a tool inside a Responses `namespace`
/// (Codex groups each MCP server's tools as `mcp__<server>` or
/// `mcp__<server>__`): `mcp__seat` + `next` = `mcp__seat__next`.
fn ns_join(ns: &str, name: &str) -> String {
    if ns.ends_with("__") {
        format!("{ns}{name}")
    } else {
        format!("{ns}__{name}")
    }
}

/// (flat name, namespace, inner name) for every function inside a
/// `namespace` tool: the seat sees the flat name, the client must get the
/// call back as `name` + `namespace` or it cannot route it.
fn namespaced_tool_names(tools: &[Value]) -> Vec<(String, String, String)> {
    let mut v = Vec::new();
    for t in tools {
        if t.get("type").and_then(|x| x.as_str()) != Some("namespace") {
            continue;
        }
        let ns = t.get("name").and_then(|x| x.as_str()).unwrap_or("");
        for inner in t
            .get("tools")
            .and_then(|x| x.as_array())
            .into_iter()
            .flatten()
        {
            if let Some(n) = inner.get("name").and_then(|x| x.as_str()) {
                v.push((ns_join(ns, n), ns.to_string(), n.to_string()));
            }
        }
    }
    v
}

/// Tool kinds the client declared, by name: `custom` tools (Codex's
/// freeform `apply_patch`) take one raw string and are answered with a
/// `custom_tool_call`, not a `function_call`.
fn custom_tool_names(tools: &[Value]) -> Vec<String> {
    tools
        .iter()
        .filter(|t| t.get("type").and_then(|v| v.as_str()) == Some("custom"))
        .filter_map(|t| {
            t.get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .collect()
}

/// Translate a Responses request body to a Chat Completions request body.
pub(crate) fn responses_to_chat(body: &Value) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    if let Some(ins) = body.get("instructions").and_then(|v| v.as_str()) {
        if !ins.trim().is_empty() {
            messages.push(json!({"role": "system", "content": ins}));
        }
    }
    match body.get("input") {
        Some(Value::String(s)) => messages.push(json!({"role": "user", "content": s})),
        Some(Value::Array(items)) => {
            for it in items {
                let kind = it.get("type").and_then(|v| v.as_str()).unwrap_or("message");
                match kind {
                    "message" => {
                        let role = match it.get("role").and_then(|v| v.as_str()).unwrap_or("user") {
                            "developer" | "system" => "system",
                            "assistant" => "assistant",
                            _ => "user",
                        };
                        let content = responses_content(it.get("content").unwrap_or(&Value::Null));
                        // Consecutive assistant text and calls belong to one
                        // assistant turn on the chat wire.
                        messages.push(json!({"role": role, "content": content}));
                    }
                    "function_call" | "custom_tool_call" | "local_shell_call" => {
                        let call_id = it
                            .get("call_id")
                            .or_else(|| it.get("id"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let (name, args) = match kind {
                            "custom_tool_call" => (
                                it.get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                                json!({"input": it.get("input").cloned().unwrap_or(json!(""))})
                                    .to_string(),
                            ),
                            "local_shell_call" => (
                                "local_shell".to_string(),
                                it.get("action").cloned().unwrap_or(json!({})).to_string(),
                            ),
                            _ => (
                                {
                                    let n = it.get("name").and_then(|v| v.as_str()).unwrap_or("");
                                    match it.get("namespace").and_then(|v| v.as_str()) {
                                        Some(ns) if !ns.is_empty() => ns_join(ns, n),
                                        _ => n.to_string(),
                                    }
                                },
                                it.get("arguments")
                                    .map(|a| {
                                        a.as_str()
                                            .map(|s| s.to_string())
                                            .unwrap_or_else(|| a.to_string())
                                    })
                                    .unwrap_or_else(|| "{}".to_string()),
                            ),
                        };
                        let call = json!({"id": call_id, "type": "function", "function": {"name": name, "arguments": args}});
                        let merged = match messages.last_mut() {
                            Some(last)
                                if last.get("role").and_then(|r| r.as_str())
                                    == Some("assistant") =>
                            {
                                let calls = last
                                    .as_object_mut()
                                    .unwrap()
                                    .entry("tool_calls")
                                    .or_insert_with(|| json!([]));
                                calls.as_array_mut().map(|a| a.push(call.clone())).is_some()
                            }
                            _ => false,
                        };
                        if !merged {
                            messages.push(json!({"role": "assistant", "content": Value::Null, "tool_calls": [call]}));
                        }
                    }
                    "function_call_output"
                    | "custom_tool_call_output"
                    | "local_shell_call_output" => {
                        // Codex's view_image answers with an input_image
                        // part: keep it as an image, not as nothing.
                        if let Some(Value::Array(parts)) = it.get("output") {
                            if parts.iter().any(|p| {
                                p.get("type").and_then(|t| t.as_str()) == Some("input_image")
                            }) {
                                messages.push(json!({
                                    "role": "tool",
                                    "tool_call_id": it.get("call_id").and_then(|v| v.as_str()).unwrap_or(""),
                                    "content": responses_content(&Value::Array(parts.clone())),
                                }));
                                continue;
                            }
                        }
                        let output = match it.get("output") {
                            Some(Value::String(s)) => s.clone(),
                            Some(Value::Array(parts)) => parts
                                .iter()
                                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                                .collect::<Vec<_>>()
                                .join("\n"),
                            Some(other) => other
                                .get("content")
                                .and_then(|c| c.as_str())
                                .map(|s| s.to_string())
                                .unwrap_or_else(|| other.to_string()),
                            None => String::new(),
                        };
                        messages.push(json!({
                            "role": "tool",
                            "tool_call_id": it.get("call_id").and_then(|v| v.as_str()).unwrap_or(""),
                            "content": output,
                        }));
                    }
                    // Reasoning items, web search calls and the like carry
                    // nothing a responder needs to read back.
                    _ => {}
                }
            }
        }
        _ => {}
    }

    let mut tools = Vec::new();
    if let Some(ts) = body.get("tools").and_then(|v| v.as_array()) {
        for t in ts {
            match t.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                "function" => tools.push(json!({
                    "type": "function",
                    "function": {
                        "name": t.get("name"),
                        "description": t.get("description"),
                        "parameters": t.get("parameters").cloned().unwrap_or(json!({"type": "object", "properties": {}})),
                    }
                })),
                "custom" => tools.push(json!({
                    "type": "function",
                    "function": {
                        "name": t.get("name"),
                        "description": format!(
                            "{} (freeform tool: pass the raw text as `input`)",
                            t.get("description").and_then(|d| d.as_str()).unwrap_or("")
                        ),
                        "parameters": {"type": "object", "properties": {"input": {"type": "string"}}, "required": ["input"]},
                    }
                })),
                "namespace" => {
                    let ns = t.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let ns_desc = t.get("description").and_then(|v| v.as_str()).unwrap_or("");
                    for inner in t.get("tools").and_then(|v| v.as_array()).into_iter().flatten() {
                        let Some(n) = inner.get("name").and_then(|v| v.as_str()) else {
                            continue;
                        };
                        let desc = inner.get("description").and_then(|v| v.as_str()).unwrap_or("");
                        let first = ns_desc.lines().next().unwrap_or("");
                        let desc = if first.is_empty() {
                            desc.to_string()
                        } else {
                            format!("{desc} [{ns}: {first}]")
                        };
                        let params = match inner.get("type").and_then(|v| v.as_str()) {
                            Some("custom") => json!({"type": "object", "properties": {"input": {"type": "string"}}, "required": ["input"]}),
                            _ => inner.get("parameters").cloned().unwrap_or(json!({"type": "object", "properties": {}})),
                        };
                        tools.push(json!({
                            "type": "function",
                            "function": {"name": ns_join(ns, n), "description": desc, "parameters": params},
                        }));
                    }
                }
                "local_shell" => tools.push(json!({
                    "type": "function",
                    "function": {
                        "name": "local_shell",
                        "description": "Run a command in the caller's shell.",
                        "parameters": {"type": "object", "properties": {"command": {"type": "array", "items": {"type": "string"}}, "workdir": {"type": "string"}, "timeout_ms": {"type": "integer"}}, "required": ["command"]},
                    }
                })),
                _ => {}
            }
        }
    }

    let mut out = json!({
        "model": body.get("model").cloned().unwrap_or(json!("omni2api")),
        "messages": messages,
        "stream": body.get("stream").and_then(|v| v.as_bool()).unwrap_or(false),
    });
    if !tools.is_empty() {
        out["tools"] = Value::Array(tools);
    }
    if let Some(n) = body.get("max_output_tokens") {
        out["max_tokens"] = n.clone();
    }
    for k in ["user", "metadata"] {
        if let Some(v) = body.get(k) {
            out[k] = v.clone();
        }
    }
    // Codex threads every request of a session with one id: use it as the
    // conversation id so sessions never merge.
    if let Some(id) = body.get("prompt_cache_key").and_then(|v| v.as_str()) {
        out["metadata"] = json!({"session_id": id});
    }
    if out["stream"] == json!(true) {
        out["stream_options"] = json!({"include_usage": true});
    }
    out
}

/// Builds the Responses output from Chat Completions chunks.
struct ResponsesBuilder {
    response_id: String,
    model: String,
    created: i64,
    custom: Vec<String>,
    namespaced: Vec<(String, String, String)>,
    seq: u64,
    text_item: Option<(usize, String, String)>, // (output_index, item_id, text)
    reasoning_item: Option<(usize, String, String)>,
    calls: Vec<(usize, String, String, String, String)>, // (output_index, item_id, call_id, name, args)
    output: Vec<Value>,
    usage: Value,
}

impl ResponsesBuilder {
    fn new(model: String, custom: Vec<String>, namespaced: Vec<(String, String, String)>) -> Self {
        Self {
            response_id: format!("resp_{}", Uuid::now_v7().simple()),
            model,
            created: chrono::Utc::now().timestamp(),
            custom,
            namespaced,
            seq: 0,
            text_item: None,
            reasoning_item: None,
            calls: Vec::new(),
            output: Vec::new(),
            // Strict Responses clients (grok) reject usage without the
            // details objects, and a turn can end before any usage arrives.
            usage: json!({
                "input_tokens": 0,
                "input_tokens_details": {"cached_tokens": 0},
                "output_tokens": 0,
                "output_tokens_details": {"reasoning_tokens": 0},
                "total_tokens": 0,
            }),
        }
    }

    fn ev(&mut self, kind: &str, mut data: Value) -> Bytes {
        self.seq += 1;
        data["type"] = json!(kind);
        data["sequence_number"] = json!(self.seq);
        sse_frame(kind, &data)
    }

    fn response_obj(&self, status: &str) -> Value {
        json!({
            "id": self.response_id,
            "object": "response",
            "created_at": self.created,
            "status": status,
            "model": self.model,
            "output": self.output,
            "usage": self.usage,
        })
    }

    fn start(&mut self) -> Vec<Bytes> {
        let created = self.response_obj("in_progress");
        let a = self.ev("response.created", json!({"response": created}));
        let created = self.response_obj("in_progress");
        let b = self.ev("response.in_progress", json!({"response": created}));
        vec![a, b]
    }

    fn next_index(&self) -> usize {
        self.output.len()
            + self.text_item.iter().count()
            + self.reasoning_item.iter().count()
            + self.calls.len()
    }

    fn close_reasoning(&mut self) -> Vec<Bytes> {
        let Some((idx, id, text)) = self.reasoning_item.take() else {
            return vec![];
        };
        let item = json!({"type": "reasoning", "id": id, "summary": [{"type": "summary_text", "text": text}]});
        let mut v = vec![
            self.ev(
                "response.reasoning_summary_text.done",
                json!({"item_id": id, "output_index": idx, "summary_index": 0, "text": text}),
            ),
            self.ev(
                "response.output_item.done",
                json!({"output_index": idx, "item": item}),
            ),
        ];
        self.output.push(item);
        v.shrink_to_fit();
        v
    }

    fn close_text(&mut self) -> Vec<Bytes> {
        let Some((idx, id, text)) = self.text_item.take() else {
            return vec![];
        };
        let part = json!({"type": "output_text", "text": text, "annotations": []});
        let item = json!({"type": "message", "id": id, "role": "assistant", "status": "completed", "content": [part]});
        let v = vec![
            self.ev(
                "response.output_text.done",
                json!({"item_id": id, "output_index": idx, "content_index": 0, "text": text}),
            ),
            self.ev(
                "response.content_part.done",
                json!({"item_id": id, "output_index": idx, "content_index": 0, "part": part}),
            ),
            self.ev(
                "response.output_item.done",
                json!({"output_index": idx, "item": item}),
            ),
        ];
        self.output.push(item);
        v
    }

    fn reasoning(&mut self, delta: &str) -> Vec<Bytes> {
        let mut v = Vec::new();
        if self.reasoning_item.is_none() {
            let idx = self.next_index();
            let id = format!("rs_{}", Uuid::now_v7().simple());
            v.push(self.ev("response.output_item.added", json!({"output_index": idx, "item": {"type": "reasoning", "id": id, "summary": []}})));
            v.push(self.ev("response.reasoning_summary_part.added", json!({"item_id": id, "output_index": idx, "summary_index": 0, "part": {"type": "summary_text", "text": ""}})));
            self.reasoning_item = Some((idx, id, String::new()));
        }
        let (idx, id) = {
            let r = self.reasoning_item.as_mut().unwrap();
            r.2.push_str(delta);
            (r.0, r.1.clone())
        };
        v.push(self.ev(
            "response.reasoning_summary_text.delta",
            json!({"item_id": id, "output_index": idx, "summary_index": 0, "delta": delta}),
        ));
        v
    }

    fn text(&mut self, delta: &str) -> Vec<Bytes> {
        let mut v = self.close_reasoning();
        if self.text_item.is_none() {
            let idx = self.next_index();
            let id = format!("msg_{}", Uuid::now_v7().simple());
            v.push(self.ev("response.output_item.added", json!({"output_index": idx, "item": {"type": "message", "id": id, "role": "assistant", "status": "in_progress", "content": []}})));
            v.push(self.ev("response.content_part.added", json!({"item_id": id, "output_index": idx, "content_index": 0, "part": {"type": "output_text", "text": "", "annotations": []}})));
            self.text_item = Some((idx, id, String::new()));
        }
        let (idx, id) = {
            let t = self.text_item.as_mut().unwrap();
            t.2.push_str(delta);
            (t.0, t.1.clone())
        };
        v.push(self.ev(
            "response.output_text.delta",
            json!({"item_id": id, "output_index": idx, "content_index": 0, "delta": delta}),
        ));
        v
    }

    fn call(&mut self, call_id: &str, name: &str, args: &str) -> Vec<Bytes> {
        let mut v = self.close_reasoning();
        v.extend(self.close_text());
        let idx = self.next_index();
        if self.custom.iter().any(|c| c == name) {
            let input = serde_json::from_str::<Value>(args)
                .ok()
                .and_then(|a| {
                    a.get("input")
                        .and_then(|i| i.as_str())
                        .map(|s| s.to_string())
                })
                .unwrap_or_else(|| args.to_string());
            let id = format!("ctc_{}", Uuid::now_v7().simple());
            let item = json!({"type": "custom_tool_call", "id": id, "call_id": call_id, "name": name, "input": input, "status": "completed"});
            v.push(self.ev("response.output_item.added", json!({"output_index": idx, "item": {"type": "custom_tool_call", "id": id, "call_id": call_id, "name": name, "input": ""}})));
            v.push(self.ev(
                "response.custom_tool_call_input.delta",
                json!({"item_id": id, "output_index": idx, "delta": input}),
            ));
            v.push(self.ev(
                "response.custom_tool_call_input.done",
                json!({"item_id": id, "output_index": idx, "input": input}),
            ));
            v.push(self.ev(
                "response.output_item.done",
                json!({"output_index": idx, "item": item}),
            ));
            self.output.push(item);
        } else {
            let id = format!("fc_{}", Uuid::now_v7().simple());
            let mut item = json!({"type": "function_call", "id": id, "call_id": call_id, "name": name, "arguments": args, "status": "completed"});
            let mut added = json!({"type": "function_call", "id": id, "call_id": call_id, "name": name, "arguments": ""});
            if let Some((_, ns, inner)) = self.namespaced.iter().find(|(f, _, _)| f == name) {
                for it in [&mut item, &mut added] {
                    it["name"] = json!(inner);
                    it["namespace"] = json!(ns);
                }
            }
            v.push(self.ev(
                "response.output_item.added",
                json!({"output_index": idx, "item": added}),
            ));
            v.push(self.ev(
                "response.function_call_arguments.delta",
                json!({"item_id": id, "output_index": idx, "delta": args}),
            ));
            v.push(self.ev(
                "response.function_call_arguments.done",
                json!({"item_id": id, "output_index": idx, "arguments": args}),
            ));
            v.push(self.ev(
                "response.output_item.done",
                json!({"output_index": idx, "item": item}),
            ));
            self.output.push(item);
        }
        v
    }

    fn set_usage(&mut self, u: &Value) {
        let input = u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        let output = u
            .get("completion_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        self.usage = json!({
            "input_tokens": input,
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens": output,
            "output_tokens_details": {"reasoning_tokens": 0},
            "total_tokens": input + output,
        });
    }

    fn finish(&mut self) -> Vec<Bytes> {
        let mut v = self.close_reasoning();
        v.extend(self.close_text());
        let done = self.response_obj("completed");
        v.push(self.ev("response.completed", json!({"response": done})));
        v
    }
}

pub async fn responses(
    State(app): State<AppState>,
    principal: AuthPrincipal,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let model = body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("omni2api")
        .to_string();
    let stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let declared = body
        .get("tools")
        .and_then(|t| t.as_array())
        .map(|a| a.as_slice())
        .unwrap_or(&[]);
    let custom = custom_tool_names(declared);
    let namespaced = namespaced_tool_names(declared);
    let chat = match chat_request(responses_to_chat(&body)) {
        Ok(c) => c,
        Err(r) => return r,
    };

    let resp = chat_completions(State(app), principal, headers, Json(chat)).await;
    let status = resp.status();
    if !status.is_success() {
        let msg = body_json(resp)
            .await
            .and_then(|v| {
                v.pointer("/error/message")
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| "request failed".to_string());
        return responses_error(status, &msg);
    }

    if !stream || !is_sse(&resp) {
        let Some(v) = body_json(resp).await else {
            return responses_error(StatusCode::BAD_GATEWAY, "unreadable completion");
        };
        let mut b = ResponsesBuilder::new(model, custom, namespaced);
        let msg = v
            .pointer("/choices/0/message")
            .cloned()
            .unwrap_or(Value::Null);
        if let Some(r) = msg.get("reasoning_content").and_then(|v| v.as_str()) {
            b.reasoning(r);
        }
        if let Some(t) = msg.get("content").and_then(|v| v.as_str()) {
            if !t.is_empty() {
                b.text(t);
            }
        }
        if let Some(calls) = msg.get("tool_calls").and_then(|v| v.as_array()) {
            for c in calls {
                let id = c.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let name = c
                    .pointer("/function/name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let args = c
                    .pointer("/function/arguments")
                    .and_then(|v| v.as_str())
                    .unwrap_or("{}");
                b.call(id, name, args);
            }
        }
        if let Some(u) = v.get("usage") {
            b.set_usage(u);
        }
        b.finish();
        return Json(b.response_obj("completed")).into_response();
    }

    let mut upstream = resp.into_body().into_data_stream();
    let out = async_stream::stream! {
        let mut b = ResponsesBuilder::new(model, custom, namespaced);
        let mut lines = SseLines::new();
        for f in b.start() { yield Ok::<Bytes, std::convert::Infallible>(f); }
        'outer: while let Some(chunk) = upstream.next().await {
            let Ok(bytes) = chunk else { break };
            for item in lines.feed(&bytes) {
                match item {
                    // Heartbeat: an SSE comment keeps the connection alive
                    // without adding an event Codex would have to parse.
                    None => yield Ok(Bytes::from_static(b": keepalive\n\n")),
                    Some(data) if data.trim() == "[DONE]" => {
                        for f in b.finish() { yield Ok(f); }
                        break 'outer;
                    }
                    Some(data) => {
                        let Ok(c) = serde_json::from_str::<Value>(&data) else { continue };
                        if let Some(u) = c.get("usage").filter(|u| !u.is_null()) {
                            b.set_usage(u);
                        }
                        let Some(delta) = c.pointer("/choices/0/delta") else { continue };
                        if let Some(r) = delta.get("reasoning_content").and_then(|v| v.as_str()) {
                            for f in b.reasoning(r) { yield Ok(f); }
                        }
                        if let Some(t) = delta.get("content").and_then(|v| v.as_str()) {
                            if !t.is_empty() {
                                for f in b.text(t) { yield Ok(f); }
                            }
                        }
                        if let Some(calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                            // The chat pipeline sends each call whole in one chunk.
                            for tc in calls {
                                let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                let name = tc.pointer("/function/name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                let args = tc.pointer("/function/arguments").and_then(|v| v.as_str()).unwrap_or("{}").to_string();
                                for f in b.call(&id, &name, &args) { yield Ok(f); }
                            }
                        }
                    }
                }
            }
        }
    };

    Response::builder()
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .header("X-Accel-Buffering", "no")
        .body(Body::from_stream(out))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_usage_always_has_details() {
        let b = ResponsesBuilder::new("m".into(), Vec::new(), Vec::new());
        let u = &b.response_obj("completed")["usage"];
        assert_eq!(u["input_tokens_details"]["cached_tokens"], 0);
        assert_eq!(u["output_tokens_details"]["reasoning_tokens"], 0);
    }

    #[test]
    fn tool_search_references_become_text() {
        let mut b = json!({"messages": [{"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": [
                {"type": "tool_reference", "tool_name": "WebFetch"}]},
            {"type": "text", "text": "Tool loaded."}
        ]}]});
        fold_tool_references(&mut b);
        let parts = b["messages"][0]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(
            parts[0]["content"][0]["text"],
            "loaded tool WebFetch: callable now"
        );
    }

    #[test]
    fn view_image_output_keeps_the_image() {
        let body = json!({
            "model": "m",
            "input": [
                {"type": "function_call", "call_id": "c1", "name": "view_image", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c1", "output": [
                    {"type": "input_image", "image_url": "data:image/png;base64,AAAA"}
                ]}
            ]
        });
        let chat = responses_to_chat(&body);
        let tool = chat["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "tool")
            .unwrap()
            .clone();
        assert_eq!(
            tool["content"][0]["image_url"]["url"],
            "data:image/png;base64,AAAA"
        );
    }

    #[test]
    fn responses_input_becomes_chat_messages() {
        let body = json!({
            "model": "gpt-5",
            "instructions": "be brief",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "running"}]},
                {"type": "function_call", "call_id": "c1", "name": "shell", "arguments": "{\"cmd\":\"ls\"}"},
                {"type": "function_call_output", "call_id": "c1", "output": "a.txt"},
                {"type": "custom_tool_call", "call_id": "c2", "name": "apply_patch", "input": "*** Begin Patch"},
                {"type": "custom_tool_call_output", "call_id": "c2", "output": "ok"}
            ],
            "tools": [
                {"type": "function", "name": "shell", "parameters": {"type": "object"}},
                {"type": "custom", "name": "apply_patch", "description": "patch"}
            ],
            "stream": true,
            "prompt_cache_key": "sess-1"
        });
        let chat = responses_to_chat(&body);
        let m = chat["messages"].as_array().unwrap();
        assert_eq!(m[0]["role"], "system");
        assert_eq!(m[1]["content"], "hi");
        assert_eq!(m[2]["role"], "assistant");
        assert_eq!(m[2]["tool_calls"][0]["function"]["name"], "shell");
        assert_eq!(m[3]["role"], "tool");
        assert_eq!(m[3]["tool_call_id"], "c1");
        assert_eq!(chat["tools"][1]["function"]["name"], "apply_patch");
        assert_eq!(chat["metadata"]["session_id"], "sess-1");
        assert_eq!(chat["stream_options"]["include_usage"], true);
        serde_json::from_value::<ChatCompletionRequest>(chat).expect("valid chat request");
    }

    #[test]
    fn custom_tools_are_answered_as_custom_calls() {
        let mut b = ResponsesBuilder::new("m".into(), vec!["apply_patch".into()], Vec::new());
        b.call("c1", "apply_patch", r#"{"input":"*** Begin Patch"}"#);
        assert_eq!(b.output[0]["type"], "custom_tool_call");
        assert_eq!(b.output[0]["input"], "*** Begin Patch");
        b.call("c2", "shell", r#"{"cmd":"ls"}"#);
        assert_eq!(b.output[1]["type"], "function_call");
    }

    #[test]
    fn namespace_tools_flatten_and_calls_come_back_namespaced() {
        // Codex groups each MCP server's tools in a `namespace`.
        let body = json!({
            "model": "m",
            "input": [
                {"type": "message", "role": "user", "content": "go"},
                {"type": "function_call", "call_id": "c1", "namespace": "mcp__seat", "name": "next", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c1", "output": "nothing"}
            ],
            "tools": [
                {"type": "function", "name": "exec_command", "parameters": {"type": "object"}},
                {"type": "namespace", "name": "mcp__seat", "description": "Seat tools", "tools": [
                    {"type": "function", "name": "next", "description": "wait", "parameters": {"type": "object", "properties": {"wait": {"type": "integer"}}}}
                ]},
                {"type": "namespace", "name": "mcp__web__", "tools": [{"type": "function", "name": "search"}]}
            ]
        });
        let chat = responses_to_chat(&body);
        let names: Vec<&str> = chat["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            ["exec_command", "mcp__seat__next", "mcp__web__search"]
        );
        assert_eq!(
            chat["messages"][1]["tool_calls"][0]["function"]["name"],
            "mcp__seat__next"
        );
        let ns = namespaced_tool_names(body["tools"].as_array().unwrap());
        let mut b = ResponsesBuilder::new("m".into(), Vec::new(), ns);
        b.call("c2", "mcp__seat__next", "{}");
        assert_eq!(b.output[0]["name"], "next");
        assert_eq!(b.output[0]["namespace"], "mcp__seat");
        b.call("c3", "exec_command", "{}");
        assert!(b.output[1].get("namespace").is_none());
    }

    #[test]
    fn sse_lines_split_events_and_comments() {
        let mut l = SseLines::new();
        let got = l.feed(b"data: {\"a\":1}\n\n: keepalive\n\ndata: [DO");
        assert_eq!(got, vec![Some("{\"a\":1}".to_string()), None]);
        let got = l.feed(b"NE]\n\n");
        assert_eq!(got, vec![Some("[DONE]".to_string())]);
    }
}
