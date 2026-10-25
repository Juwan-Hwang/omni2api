//! `POST /v1/chat/completions` — OpenAI Chat Completions compatible endpoint.
//!
//! Phase 6: Realtime message routing.
//! Phase 7.3: Human-triggered tool_calls response.
//!
//! # Reply Payload Modes
//!
//! The Human Console can send two types of reply via `reply.send`:
//!
//! 1. **Text reply** — plain text string → `ChatCompletionResponse::from_human_reply`
//! 2. **Tool call reply** — JSON with `__tool_call__` marker → `ChatCompletionResponse::from_tool_call`
//!
//! The tool call format (sent by Human Console via WS `reply.send`):
//! ```json
//! {
//!   "__tool_call__": true,
//!   "tool_name": "Bash",
//!   "arguments": {"command": "echo hello"}
//! }
//! ```
//!
//! This is an internal wire format between Human Console and Omni2API.
//! The Agent never sees it — the Agent receives a standard OpenAI
//! `tool_calls` response.

use std::collections::HashMap;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Json, Response};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::time::interval;
use uuid::Uuid;

use crate::async_reply::merge_window;
use crate::auth::AuthPrincipal;
use crate::model::content::ContentBlock;
use crate::model::identity::ParticipantRef;
use crate::model::message::{Message, MessageStatus};
use crate::openai_compat::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ToolCallSpec,
};
use crate::profile::AgentProfile;
use crate::storage::AppState;

// ── Reply payload parsing ──

/// Parsed reply from the Human Console — text, a tool call, or an
/// end-of-turn signal.
#[derive(Debug, Clone)]
pub(crate) enum HumanReply {
    /// Plain text reply from the human.
    ///
    /// `ended` says whether the human also closed the turn. Multi-turn
    /// streaming keeps the SSE stream open across replies, so plain prose
    /// waits for more unless an explicit end-of-turn accompanied it.
    Text { text: String, ended: bool },
    /// One or more tool calls — the human wants the agent to execute them.
    ///
    /// A `Vec` because a human may tap several tools before the turn
    /// closes; OpenAI models that as one assistant message with an array of
    /// calls, which is what lets a client run them in parallel.
    ToolCalls {
        calls: Vec<ToolCallRequest>,
        /// Words the human said alongside the call(s).
        ///
        /// OpenAI allows one assistant message to carry `content` *and*
        /// `tool_calls` at the same time. Emitting `content: null` whenever
        /// a tool is involved silently discards whatever the human typed —
        /// "run this and tell them I'm here" loses the second half.
        text: Option<String>,
    },
    /// End-of-turn signal — the human explicitly closed the turn.
    ///
    /// Terminates the SSE stream with a normal `finish_reason: stop`.
    EndTurn,
}

/// A single tool call the human asked for.
#[derive(Debug, Clone)]
pub(crate) struct ToolCallRequest {
    pub tool_name: String,
    pub arguments: Value,
    /// Correlation id from the originating `tool.invoke` message.
    ///
    /// Present when the call was triggered from the Human Console's tool
    /// panel (WS `tool.invoke`). `None` for the legacy `__tool_call__`
    /// payload. Used to pair a later `tool.result` back to this call.
    pub invocation_id: Option<String>,
}

/// Wire sentinel for the end-of-turn signal delivered via the reply channel.
const END_TURN_SIGNAL: &str = "__END_TURN__";

/// Take whatever else arrives inside `window`.
///
/// Used after the first payload of a turn has already been received. Stops
/// early on an explicit `end.turn` — holding a turn open after the human
/// asked to close it would only add latency.
///
/// The `end.turn` is kept in the returned payloads rather than discarded:
/// it closes the collection window *and* closes the turn, and dropping it
/// would leave the turn waiting forever for an end that already arrived.
async fn collect_rest(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    window: Duration,
) -> Vec<String> {
    if window.is_zero() {
        return Vec::new();
    }

    let mut out = Vec::new();
    // Intentionally a `loop`, not `while let`: the timeout arm and the
    // channel-closed arm both break with different meanings, and folding
    // them into a while-let condition hides that.
    #[allow(clippy::while_let_loop)]
    loop {
        match tokio::time::timeout(window, rx.recv()).await {
            Ok(Some(payload)) => {
                let closed = payload.trim() == END_TURN_SIGNAL;
                out.push(payload);
                if closed {
                    break;
                }
            }
            // Window elapsed with nothing more, or the channel went away.
            Ok(None) | Err(_) => break,
        }
    }
    out
}

/// Fold everything collected in one turn into a single reply.
///
/// Prose is joined in order; tool calls keep their own ids so each can be
/// answered separately with `role: "tool"`.
fn merge_replies(payloads: &[String]) -> HumanReply {
    let mut prose: Vec<String> = Vec::new();
    let mut calls: Vec<ToolCallRequest> = Vec::new();
    let mut ended = false;

    for payload in payloads {
        match parse_reply(payload) {
            HumanReply::Text { text: t, .. } => {
                if !t.trim().is_empty() {
                    prose.push(t);
                }
            }
            HumanReply::ToolCalls { calls: mut c, text } => {
                if let Some(t) = text {
                    if !t.trim().is_empty() {
                        prose.push(t);
                    }
                }
                calls.append(&mut c);
            }
            HumanReply::EndTurn => {
                ended = true;
            }
        }
    }

    if !calls.is_empty() {
        let text = prose.join("\n\n").trim().to_string();
        return HumanReply::ToolCalls {
            calls,
            text: if text.is_empty() { None } else { Some(text) },
        };
    }

    let text = prose.join("\n\n");
    if text.trim().is_empty() {
        HumanReply::EndTurn
    } else {
        HumanReply::Text { text, ended }
    }
}

/// Wire format for tool call replies sent via `reply.send`.
///
/// The Human Console sends this JSON object as the `text` field of
/// a `reply.send` WS message. Omni2API detects the `__tool_call__`
/// marker and converts it to a standard OpenAI `tool_calls` response.
#[derive(Debug, Deserialize)]
struct ToolCallPayload {
    #[serde(rename = "__tool_call__")]
    is_tool_call: bool,
    tool_name: String,
    arguments: Value,
}

/// Wire format for tool invocations sent by the Human Console via WS
/// `tool.invoke`, and relayed onto the reply channel by
/// [`crate::ws::handler::handle_tool_invoke`].
///
/// The Human Console sends this when the human taps a tool in the tool
/// panel instead of typing a reply. It lands on the reply channel of the
/// conversation whose SSE request is currently suspended, so the waiting
/// agent turn resolves with a real `tool_calls` response.
///
/// ```json
/// {
///   "type": "tool.invoke",
///   "invocation_id": "inv_01H...",
///   "tool_id": "Bash",
///   "input": {"command": "echo hello"}
/// }
/// ```
///
/// **Why this must be parsed here:** before this existed, the payload fell
/// through to [`HumanReply::Text`], so the agent received the JSON as a
/// quoted string with `finish_reason: "stop"`. OpenAI-compatible clients
/// only act on a `tool_calls` array, so the tool would never run.
///
/// `text` is optional companion prose: the console sends it when the human
/// typed something in the same breath as tapping the tool.
///
/// ```json
/// {
///   "type": "tool.invoke",
///   "invocation_id": "inv_01H...",
///   "tool_id": "Bash",
///   "input": {"command": "echo hello"},
///   "text": "run this and tell them I'm here"
/// }
/// ```
#[derive(Debug, Deserialize)]
struct ToolInvokePayload {
    #[serde(rename = "type")]
    kind: String,
    invocation_id: Option<String>,
    tool_id: String,
    /// Companion prose to surface as `content` alongside `tool_calls`.
    #[serde(default)]
    text: Option<String>,
    /// Tool arguments. Defaults to an empty object rather than `null`, so a
    /// tool invoked with no input still serialises to valid JSON arguments
    /// (`{}`) in the OpenAI `function.arguments` string.
    #[serde(default = "empty_json_object")]
    input: Value,
}

fn empty_json_object() -> Value {
    Value::Object(Default::default())
}

/// Discriminator value marking a [`ToolInvokePayload`].
const TOOL_INVOKE_TYPE: &str = "tool.invoke";

/// Parse a reply payload string into either a text reply or a tool call.
///
/// Recognized shapes, in priority order:
///
/// 1. `__END_TURN__` sentinel → [`HumanReply::EndTurn`]
/// 2. `{"type": "tool.invoke", ...}` — relayed WS `tool.invoke` →
///    [`HumanReply::ToolCall`] with the `invocation_id` preserved
/// 3. `{"__tool_call__": true, ...}` — legacy `reply.send` tool payload →
///    [`HumanReply::ToolCall`] with no `invocation_id`
///
/// Anything else is treated as plain human text.
/// Parse a completed turn, which may be a single payload or a JSON array.
///
/// A merged turn travels as an array of the payloads the human sent inside
/// the merge window, so prose and tool calls can be recombined here rather
/// than being split across responses. A lone payload is unaffected.
pub(crate) fn parse_turn(payload: &str) -> HumanReply {
    if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(payload) {
        if items.len() > 1 {
            let payloads: Vec<String> = items
                .iter()
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect();
            return merge_replies(&payloads);
        }
    }
    parse_reply(payload)
}

pub(crate) fn parse_reply(payload: &str) -> HumanReply {
    // End-of-turn sentinel — highest priority check.
    if payload.trim() == END_TURN_SIGNAL {
        return HumanReply::EndTurn;
    }
    if payload.trim_start().starts_with('{') {
        // Tool invocation relayed from the Human Console's tool panel.
        // Must be checked BEFORE the generic text fallback, otherwise the
        // JSON is handed to the agent as a quoted string and no tool runs.
        if let Ok(invoke) = serde_json::from_str::<ToolInvokePayload>(payload) {
            if invoke.kind == TOOL_INVOKE_TYPE {
                let text = invoke.text.filter(|t| !t.trim().is_empty());
                return HumanReply::ToolCalls {
                    calls: vec![ToolCallRequest {
                        tool_name: invoke.tool_id,
                        arguments: invoke.input,
                        invocation_id: invoke.invocation_id,
                    }],
                    text,
                };
            }
        }
        if let Ok(tc) = serde_json::from_str::<ToolCallPayload>(payload) {
            if tc.is_tool_call {
                return HumanReply::ToolCalls {
                    calls: vec![ToolCallRequest {
                        tool_name: tc.tool_name,
                        arguments: tc.arguments,
                        invocation_id: None,
                    }],
                    text: None,
                };
            }
        }
    }
    HumanReply::Text {
        text: payload.to_string(),
        ended: false,
    }
}

// ── Conversation scoping ──

/// Resolve which conversation an inbound request belongs to.
///
/// The OpenAI Chat Completions API is stateless — agents replay the whole
/// transcript on every turn and carry no session identifier of their own.
/// Without an explicit scoping dimension every chat window opened with the
/// same API key therefore landed in one shared conversation.
///
/// Resolution order (first non-empty wins):
///
/// 1. `x-omni2api-conversation-id` / `x-conversation-id` header
/// 2. `conversation` field in the body — `"conv_x"` or `{"id": "conv_x"}`
/// 3. `x-omni2api-session-id` / `x-session-id` header
/// 4. `user` field in the body
/// 5. `metadata.conversation_id` / `metadata.session_id`
/// 6. SHA-256 fingerprint of the system prompt — agents that change project
///    or mode change their prompt, so this separates those threads
/// 7. Client fingerprint — a digest of stable per-client headers, so two
///    unidentified callers still land in distinct conversations
///
/// Each source is namespaced with a prefix so an agent's `user` value can
/// never collide with a header-derived id of the same string.
fn resolve_session_key(
    headers: &HeaderMap,
    req: &ChatCompletionRequest,
    system_prompt: Option<&str>,
    auxiliary: bool,
) -> String {
    /// Pull a trimmed, non-empty header value (or `None`).
    fn header_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
        let v = headers.get(name)?.to_str().ok()?.trim();
        if v.is_empty() {
            None
        } else {
            Some(v)
        }
    }

    // Side calls (titles, suggestions) never share the main chat's
    // conversation, even when they carry its session id (opencode sends one
    // `x-session-id` for both): they would race it for the reply.
    if auxiliary {
        let client = client_fingerprint(headers);
        let base = match system_prompt.filter(|p| !p.trim().is_empty()) {
            Some(p) => format!("prm:{}:{}", prompt_fingerprint(p), client),
            None => format!("cli:{}", client),
        };
        return format!("aux:{}", base);
    }

    // 1. Explicit conversation id header — the most reliable signal.
    for name in ["x-omni2api-conversation-id", "x-conversation-id"] {
        if let Some(v) = header_value(headers, name) {
            return format!("hdr:{}", v);
        }
    }

    // 2. Conversation handle in the request body (Responses-API style).
    if let Some(conv) = &req.conversation {
        let id = conv.id().trim();
        if !id.is_empty() {
            return format!("cnv:{}", id);
        }
    }

    // 3. Generic session header.
    for name in ["x-omni2api-session-id", "x-session-id"] {
        if let Some(v) = header_value(headers, name) {
            return format!("ses:{}", v);
        }
    }

    // 4. OpenAI `user` field.
    if let Some(u) = &req.user {
        let u = u.trim();
        if !u.is_empty() {
            return format!("usr:{}", u);
        }
    }

    // 5. Caller-supplied metadata.
    if let Some(md) = &req.metadata {
        for k in [
            "conversation_id",
            "conversationId",
            "session_id",
            "sessionId",
        ] {
            if let Some(v) = md.get(k).and_then(|v| v.as_str()) {
                let v = v.trim();
                if !v.is_empty() {
                    return format!("meta:{}", v);
                }
            }
        }
    }

    // 6./7. No explicit id: fall back to fingerprints.
    //
    //    The system-prompt fingerprint distinguishes project/mode switches,
    //    scoped by client so two agents sharing a prompt never share one
    //    outbound queue. On its own it merged every session a client ever
    //    opened into one conversation, so the main chat is further anchored
    //    on its first user message: a session's history always starts with
    //    it, and a new session starts with a different one.
    //
    //    Auxiliary calls (titles, suggestions) carry a fresh transcript as
    //    their only message, so anchoring them would mint a conversation
    //    per call; they share one conversation per prompt instead.
    //    The main chat is not keyed on the system prompt any more: clients
    //    rewrite it between turns (Trae, dsh), which split one session into
    //    many. Continuity is carried by our own replies in the history (see
    //    `history_digests`); this key only decides where a brand-new session
    //    lands.
    let client = client_fingerprint(headers);
    match first_user_anchor(req) {
        Some(anchor) => format!("cli:{}:{}", client, anchor),
        None => format!("cli:{}", client),
    }
}

/// Digests of the assistant turns in a request's history, newest first: the
/// replies and tool calls this server sent, as the caller replays them.
fn history_digests(req: &ChatCompletionRequest) -> Vec<String> {
    let mut out = Vec::new();
    for m in req.messages.iter().rev() {
        if m.role != crate::openai_compat::ChatRole::Assistant {
            continue;
        }
        if let Some(d) = crate::session::text_digest(&m.content.to_text()) {
            out.push(d);
        }
        if let Some(calls) = m.tool_calls.as_ref().and_then(|v| v.as_array()) {
            for c in calls {
                let f = c.get("function").cloned().unwrap_or(Value::Null);
                let name = f.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let args = f.get("arguments").cloned().unwrap_or(Value::Null);
                out.push(crate::session::call_digest(name, &args));
            }
        }
    }
    out
}

/// Digest of the first `user` message, the anchor of a session's history.
///
/// Injected blocks that change from turn to turn (`<system-reminder>`,
/// attachment lists) are stripped first, so the anchor is what the person
/// typed.
fn first_user_anchor(req: &ChatCompletionRequest) -> Option<String> {
    req.messages
        .iter()
        .filter(|m| m.role == crate::openai_compat::ChatRole::User)
        // A part that is only harness blocks (Grok's `<user_info>`, the same
        // in every session in one directory) must not anchor: every new
        // session would land in the old conversation.
        .map(|m| crate::session::strip_injections_part(&m.content.to_text()))
        .find(|t| !t.trim().is_empty())
        .map(|t| prompt_fingerprint(&t))
}

/// A client with no dedicated detection still names itself in its
/// User-Agent (`reasonix/2.37`, `Cline/3.1`, `copilot-cli/1.0`): use the
/// product token so the responder knows what it is talking to. Generic SDK
/// and runtime agents tell nothing and are skipped.
fn client_from_user_agent(headers: &HeaderMap) -> Option<String> {
    // GitHub Copilot CLI's BYOK requests carry the stock OpenAI SDK agent,
    // but mark who started each turn.
    if headers.contains_key("x-interaction-type")
        || headers.contains_key("x-initiator")
        || headers.contains_key("copilot-integration-id")
    {
        return Some("copilot".to_string());
    }
    let ua = headers.get("user-agent")?.to_str().ok()?;
    let generic = [
        "openai",
        "anthropic",
        "node",
        "node-fetch",
        "undici",
        "axios",
        "python",
        "python-requests",
        "python-httpx",
        "httpx",
        "aiohttp",
        "go-http-client",
        "curl",
        "reqwest",
        "okhttp",
        "bun",
        "deno",
        "mozilla",
        "ai-sdk",
        "ai",
    ];
    for tok in ua.split_whitespace().filter(|t| t.contains('/')) {
        let name = tok
            .split('/')
            .next()
            .unwrap_or("")
            .trim_matches(|c| c == '(' || c == ')');
        let low = name.to_ascii_lowercase();
        if low.len() < 2
            || !low.starts_with(|c: char| c.is_ascii_alphabetic())
            || !low
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        {
            continue;
        }
        if generic
            .iter()
            .any(|g| low == *g || low.starts_with(&format!("{g}-")) && *g != "ai")
        {
            continue;
        }
        return Some(low);
    }
    None
}

/// A request offering exactly one tool and forcing the model to call it
/// (`tool_choice` "required" or naming it): a structured side call, such as
/// Grok's `session_title`. Returns the tool's name and required parameters.
fn forced_tool(req: &ChatCompletionRequest) -> Option<(String, Vec<String>)> {
    let tools = req.tools.as_ref()?.as_array()?;
    if tools.len() != 1 {
        return None;
    }
    let f = tools[0].get("function").unwrap_or(&tools[0]);
    let name = f.get("name")?.as_str()?.to_string();
    let forced = match req.tool_choice.as_ref().unwrap_or(&Value::Null) {
        Value::String(s) => s == "required" || s == "any",
        Value::Object(o) => {
            o.get("type").and_then(|t| t.as_str()) == Some("function")
                || o.get("type").and_then(|t| t.as_str()) == Some("tool")
                || o.get("name").is_some()
        }
        _ => false,
    };
    // Not every client sets tool_choice (Grok's title call does not): one
    // tool and a system prompt about a title or summary is the same thing.
    let head: String = req
        .messages
        .iter()
        .filter(|m| m.role == crate::openai_compat::ChatRole::System)
        .map(|m| m.content.to_text())
        .collect::<Vec<_>>()
        .join("\n")
        .chars()
        .take(400)
        .collect::<String>()
        .to_lowercase();
    let side = ["title", "summar", "标题", "摘要"]
        .iter()
        .any(|k| head.contains(k));
    if !forced && !side {
        return None;
    }
    let required = f
        .pointer("/parameters/required")
        .or_else(|| f.pointer("/input_schema/required"))
        .and_then(|r| r.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    Some((name, required))
}

/// A side call the client makes around the real chat: a session title, an
/// input-box suggestion, a summary. Recognized by shape and purpose: no
/// tools, no earlier turns of a conversation (only user messages), and a
/// system prompt that says what it is for, or a structured output.
///
/// A token cap alone is not enough: plenty of plain chat clients send
/// `max_tokens` on every request (opencode sends 32000 on its main chat).
fn is_auxiliary_request(req: &ChatCompletionRequest) -> bool {
    use crate::openai_compat::ChatRole;
    let has_tools = req
        .tools
        .as_ref()
        .and_then(|t| t.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    if has_tools && forced_tool(req).is_none() {
        return false;
    }
    let only_user = req
        .messages
        .iter()
        .filter(|m| m.role != ChatRole::System)
        .all(|m| m.role == ChatRole::User);
    let has_user = req.messages.iter().any(|m| m.role == ChatRole::User);
    if !only_user || !has_user {
        return false;
    }
    if req.response_format.is_some() || has_tools {
        return true;
    }
    // Claude Code's WebFetch hands the fetched page to the model with a
    // question: a side call, never part of the chat.
    if req.messages.iter().any(|m| {
        m.role == ChatRole::User
            && m.content
                .to_text()
                .trim_start()
                .starts_with("Web page content:")
    }) {
        return true;
    }
    let prompt: String = req
        .messages
        .iter()
        .filter(|m| m.role == ChatRole::System)
        .map(|m| m.content.to_text())
        .collect::<Vec<_>>()
        .join("\n");
    let head: String = prompt.chars().take(400).collect::<String>().to_lowercase();
    [
        "title",
        "summar",
        "suggest",
        "dashboard line",
        "status row",
        "commit message",
        "标题",
        "摘要",
        "建议",
    ]
    .iter()
    .any(|k| head.contains(k))
}

/// Whether this capability snapshot differs from the last one sent to this
/// human for this agent (process-local; a restart sends one fresh copy).
fn capability_changed(human_id: &str, agent_id: &str, data: &Value) -> bool {
    use sha2::{Digest, Sha256};
    use std::sync::{Mutex, OnceLock};
    static LAST: OnceLock<Mutex<HashMap<String, Vec<u8>>>> = OnceLock::new();
    let digest = Sha256::digest(data.to_string().as_bytes()).to_vec();
    let key = format!("{}\u{1f}{}", human_id, agent_id);
    let mut map = LAST.get_or_init(Default::default).lock().unwrap();
    if map.get(&key) == Some(&digest) {
        return false;
    }
    map.insert(key, digest);
    true
}

/// What a responder needs to know about the request a message rode in on:
/// which client, whether it is an auxiliary call, and for those the output
/// contract (the instructions live in the system prompt).
fn request_info(
    req: &ChatCompletionRequest,
    profile: &AgentProfile,
    system_prompt: Option<&str>,
    auxiliary: bool,
) -> Value {
    let sp = system_prompt.unwrap_or("");
    let head: String = sp.chars().take(600).collect();
    json!({
        "client": profile.client_type.to_string(),
        "kind": if auxiliary { "auxiliary" } else { "chat" },
        "expected_format": if auxiliary { expected_format(req, sp) } else { None },
        "system_prompt_len": sp.chars().count(),
        "system_prompt_head": head,
        "response_format": req.response_format,
        "max_tokens": req.max_tokens.or(req.max_completion_tokens),
        "tool_count": req.tools.as_ref().and_then(|t| t.as_array()).map(|a| a.len()).unwrap_or(0),
        "message_count": req.messages.len(),
    })
}

/// One line telling a responder what shape an auxiliary call must answer
/// in, so it need not read the caller's whole system prompt. `None` when
/// the shape cannot be told.
fn expected_format(req: &ChatCompletionRequest, sp: &str) -> Option<String> {
    if let Some((name, required)) = forced_tool(req) {
        let args: Vec<String> = required.iter().map(|p| format!("{p}='...'")).collect();
        return Some(format!(
            "a call to its only tool: `oh-seat tool {name} {}` (the call is the answer; no result comes back)",
            args.join(" ")
        ));
    }
    if let Some(rf) = &req.response_format {
        if rf.get("type").and_then(|v| v.as_str()) == Some("json_schema") {
            let schema = rf
                .pointer("/json_schema/schema")
                .cloned()
                .unwrap_or(Value::Null);
            return Some(format!("JSON matching schema {}", schema));
        }
        if rf.get("type").and_then(|v| v.as_str()) == Some("json_object") {
            return Some("a JSON object".to_string());
        }
    }
    let last_user = req
        .messages
        .iter()
        .rev()
        .find(|m| m.role == crate::openai_compat::ChatRole::User)
        .map(|m| m.content.to_text())
        .unwrap_or_default();
    if last_user.trim_start().starts_with("Web page content:") {
        return Some(
            "plain text: answer the question that follows the page, from the page only".to_string(),
        );
    }
    let hay = format!("{}\n{}", sp, last_user).to_lowercase();
    if hay.contains("no_suggestion") {
        return Some("suggestion lines, or exactly NO_SUGGESTION".to_string());
    }
    if hay.contains("\"suggestions\"") {
        return Some("JSON {\"suggestions\": [...]} (empty list when none)".to_string());
    }
    if hay.contains("title") {
        // Only a quoted "title" key means a JSON object: a bare mention of
        // JSON elsewhere in a long prompt (opencode's) does not.
        if hay.contains("\"title\"") || hay.contains("{title") {
            return Some("JSON {\"title\": \"...\"}".to_string());
        }
        return Some("only a short title, no quotes or extra text".to_string());
    }
    // A prompt that spells out its own output ("Output ONLY the fragment:
    // 5-12 words") says it better than any generic label.
    let sp_low = sp.to_lowercase();
    if let Some(i) = sp_low.find("output only") {
        let rule: String = sp[i..]
            .split_terminator(['\n'])
            .next()
            .unwrap_or("")
            .chars()
            .take(160)
            .collect();
        let first: String = sp
            .split_terminator(['.', '\n'])
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .chars()
            .take(160)
            .collect();
        return Some(format!("{first}. {}", rule.trim()));
    }
    if hay.contains("summar") {
        return Some("only the summary".to_string());
    }
    // Otherwise the prompt's own first sentence says what it wants.
    let first: String = sp
        .split_terminator(['.', '\n'])
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .chars()
        .take(160)
        .collect();
    (!first.is_empty() && hay.contains("only"))
        .then(|| format!("what the caller's prompt asks: {first}"))
}

/// Short, stable digest identifying the calling client.
///
/// Derived from headers a client sends consistently on every request, so the
/// same caller keeps the same conversation while different callers do not
/// collide. Not a security boundary — only a disambiguator.
fn client_fingerprint(headers: &HeaderMap) -> String {
    use sha2::{Digest, Sha256};

    fn value<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .trim()
    }

    let seed = format!(
        "{}\u{1f}{}\u{1f}{}",
        value(headers, "user-agent"),
        value(headers, "accept-language"),
        value(headers, "x-stainless-arch"),
    );
    let digest = Sha256::digest(seed.as_bytes());
    hex::encode(&digest[..6])
}

/// The acknowledgement streamed to the agent before the human answers.
///
/// Presence goes both ways: an offline human is told via push, but the
/// caller sitting on an open request gets nothing either way unless we say
/// something. `None` disables the notice entirely.
///
/// The notice is wrapped in `[Omni2API ...]` markers so agents can
/// recognize and filter it: it is transport status, not the human's reply,
/// and must not be treated as conversation content.
fn status_notice(is_online: bool) -> Option<String> {
    if is_online {
        std::env::var("OMNI2API_ONLINE_MESSAGE")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .or_else(|| {
                if std::env::var("OMNI2API_ACK_ONLINE")
                    .map(|v| v.trim() == "0" || v.trim().eq_ignore_ascii_case("false"))
                    .unwrap_or(false)
                {
                    None
                } else {
                    Some(
                        "[Omni2API 提示]: 人类专家当前在线，消息已送达，正在等待回复...\n\n"
                            .to_string(),
                    )
                }
            })
    } else {
        Some(
            std::env::var("OMNI2API_OFFLINE_MESSAGE")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| {
                    "[Omni2API 提示]: 人类专家当前处于离线状态，已发送待办通知至移动端。本会话保持等待中，请稍候回复...\n\n"
                        .to_string()
                }),
        )
    }
}

/// Infer a MIME type for an image reference.
///
/// Handles the `data:` URLs agents send for inline images, which is the
/// common case — a bare `https://...` may or may not end in an extension,
/// and guessing wrong only affects how the console labels the attachment.
fn guess_image_type(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("data:") {
        let mime = rest.split(';').next().unwrap_or("").trim();
        if !mime.is_empty() {
            return mime.to_string();
        }
    }
    let lower = url.to_lowercase();
    for (ext, mime) in [
        (".png", "image/png"),
        (".jpg", "image/jpeg"),
        (".jpeg", "image/jpeg"),
        (".gif", "image/gif"),
        (".webp", "image/webp"),
        (".bmp", "image/bmp"),
        (".svg", "image/svg+xml"),
    ] {
        if lower.split('?').next().unwrap_or("").ends_with(ext) {
            return mime.to_string();
        }
    }
    "image/*".to_string()
}

/// First 16 hex chars of the SHA-256 of a system prompt.
///
/// Not a security boundary — it only needs to be stable across identical
/// prompts and different across distinct ones, which is why real SHA-256 is
/// used instead of `DefaultHasher` (whose output is not stable across
/// process restarts, and would silently fork conversations on every deploy).
fn prompt_fingerprint(prompt: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(prompt.as_bytes());
    hex::encode(&digest[..8])
}

// ── Inbound message processing ──

/// Inbound → internal mapping.
///
/// Extracts `system` messages as conversation metadata and stores
/// `user` messages as `Message{sender=AgentIdentity}`.
/// `assistant` messages are treated as context replay (skipped).
/// `tool` messages are stored with `tool_call_id` metadata.
fn process_inbound_messages(
    req: &ChatCompletionRequest,
    principal: &AuthPrincipal,
    conversation_id: &str,
    client_type: &crate::profile::ClientType,
) -> (Option<String>, Vec<Message>) {
    let mut system_prompt: Option<String> = None;
    let mut messages = Vec::new();
    let now = Utc::now();

    // ── Which messages are new ──
    //
    // Callers send the FULL history with every request. What is new is the
    // tail after the last `assistant` message: the user's fresh input (one
    // or more `user` messages: some agents append their own runtime or
    // skill injections after what the person typed) and any `tool` results
    // answering the last tool call. Everything before that tail was
    // delivered on an earlier request.
    //
    // Taking only "the last user message" lost the person's words whenever
    // an agent appended an injection after them, and re-pushing every
    // `tool` message re-delivered old results on every follow-up request.
    let tail_start = req
        .messages
        .iter()
        .rposition(|m| m.role == crate::openai_compat::ChatRole::Assistant)
        .map(|i| i + 1)
        .unwrap_or(0);

    let mut user_texts: Vec<String> = Vec::new();
    let mut user_images: Vec<String> = Vec::new();
    let mut raw_tail: Vec<Value> = Vec::new();

    for (idx, msg) in req.messages.iter().enumerate() {
        match msg.role {
            crate::openai_compat::ChatRole::System => {
                let text = msg.content.to_text();
                match &mut system_prompt {
                    Some(existing) => {
                        existing.push('\n');
                        existing.push_str(&text);
                    }
                    None => system_prompt = Some(text),
                }
            }
            crate::openai_compat::ChatRole::User => {
                if idx < tail_start {
                    continue;
                }
                raw_tail.push(json!({ "role": "user", "content": msg.content.to_text() }));
                // Peel the agent wrapper off before anything reaches the
                // timeline (see `normalize.rs`).
                // Then drop the blocks clients inject into user turns
                // (`<environment_context>`, `<system-reminder>` …): the
                // responder still has them in `raw_tail`.
                let clean_text = crate::session::strip_injections_part(
                    &crate::normalize::normalize_user_message(client_type, &msg.content),
                );
                // pi sends a tool's picture as a user turn captioned
                // "Attached image(s) from tool result:": a caption, not words
                // from the person.
                let caption_only = msg.content.has_image() && is_tool_image_caption(&clean_text);
                if !clean_text.trim().is_empty()
                    && !is_image_placeholder(&clean_text)
                    && !caption_only
                {
                    user_texts.push(clean_text);
                }
                user_images.extend(msg.content.image_urls());
            }
            crate::openai_compat::ChatRole::Assistant => {
                // Context replay; the caller already has it.
            }
            crate::openai_compat::ChatRole::Tool => {
                if idx < tail_start {
                    continue;
                }
                let text = msg.content.to_text();
                let tool_call_id = msg.tool_call_id.clone();
                raw_tail.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_call_id,
                    "content_len": text.len(),
                }));

                tracing::info!(
                    tool_call_id = %tool_call_id.as_deref().unwrap_or("unknown"),
                    content_len = text.len(),
                    "[tool_result] received from agent"
                );

                // The call this result answers, as the client echoed it in
                // its own assistant message. Clients that rewrite our call
                // ids (Trae) still echo the tool name and arguments, which is
                // how the result is routed back (see `session`).
                let echoed = tool_call_id.as_deref().and_then(|id| {
                    req.messages[..idx].iter().rev().find_map(|m| {
                        m.tool_calls.as_ref()?.as_array()?.iter().find_map(|c| {
                            (c.get("id").and_then(|v| v.as_str()) == Some(id)).then(|| {
                                let f = c.get("function").cloned().unwrap_or(Value::Null);
                                (
                                    f.get("name")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string(),
                                    f.get("arguments").cloned().unwrap_or(Value::Null),
                                )
                            })
                        })
                    })
                });

                let message = Message {
                    id: format!("msg_{}", Uuid::now_v7().simple()),
                    conversation_id: conversation_id.to_string(),
                    sender: ParticipantRef::agent_named(&principal.agent.id, &principal.agent.name),
                    content: ContentBlock::text(text),
                    attachments: Vec::new(),
                    metadata: {
                        let mut m = HashMap::new();
                        m.insert("openai_role".to_string(), Value::String("tool".to_string()));
                        if let Some(ref id) = tool_call_id {
                            m.insert("tool_call_id".to_string(), Value::String(id.clone()));
                            m.insert("client_tool_call_id".to_string(), Value::String(id.clone()));
                        }
                        if let Some((name, args)) = echoed {
                            m.insert("echo_tool".to_string(), Value::String(name));
                            m.insert("echo_args".to_string(), args);
                        }
                        // A tool that returns a picture (Read on a PNG): the
                        // text only says `[图片]`, so carry the image itself.
                        let imgs = msg.content.image_urls();
                        if !imgs.is_empty() {
                            m.insert(
                                "images".to_string(),
                                Value::Array(imgs.into_iter().map(Value::String).collect()),
                            );
                        }
                        m
                    },
                    status: MessageStatus::Pending,
                    visibility: crate::model::message::MessageVisibility::InternalOnly,
                    created_at: now,
                };
                messages.push(message);
            }
        }
    }

    // A tool that returns an image (Trae's Read on a picture) sends it back
    // as an extra user turn holding only the image. It belongs to the tool
    // result, not to a new message from the person.
    // Every part was harness blocks: keep what was sent rather than nothing.
    if user_texts.is_empty() && user_images.is_empty() {
        if let Some(last) = raw_tail
            .iter()
            .rev()
            .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
            .filter_map(|m| m.get("content").and_then(|c| c.as_str()))
            .find(|t| !t.trim().is_empty())
        {
            user_texts.push(crate::session::strip_injections(last));
        }
    }
    if user_texts.is_empty() && !user_images.is_empty() {
        if let Some(last_tool) = messages.last_mut() {
            last_tool.metadata.insert(
                "images".to_string(),
                Value::Array(
                    user_images
                        .iter()
                        .map(|u| Value::String(u.clone()))
                        .collect(),
                ),
            );
            user_images.clear();
        }
    }

    if !user_texts.is_empty() || !user_images.is_empty() {
        let images = user_images;
        let message = Message {
            id: format!("msg_{}", Uuid::now_v7().simple()),
            conversation_id: conversation_id.to_string(),
            sender: ParticipantRef::agent_named(&principal.agent.id, &principal.agent.name),
            content: ContentBlock::text(user_texts.join("\n\n")),
            attachments: images
                .iter()
                .enumerate()
                .map(|(i, url)| crate::model::content::Attachment {
                    id: format!("att_{}_{}", i, Uuid::now_v7().simple()),
                    filename: crate::openai_compat::data_url_name(url)
                        .unwrap_or_else(|| format!("image-{}.png", i + 1)),
                    content_type: guess_image_type(url),
                    size_bytes: url.len() as u64,
                    url: url.clone(),
                })
                .collect(),
            metadata: {
                let mut m = HashMap::new();
                m.insert("openai_role".to_string(), Value::String("user".to_string()));
                if !images.is_empty() {
                    m.insert(
                        "images".to_string(),
                        Value::Array(images.iter().map(|u| Value::String(u.clone())).collect()),
                    );
                }
                // The unstripped tail, for responders that read raw context
                // (agent seats). Humans see `content` only.
                m.insert("raw_tail".to_string(), Value::Array(raw_tail));
                m
            },
            status: MessageStatus::Pending,
            visibility: crate::model::message::MessageVisibility::HumanVisible,
            created_at: now,
        };
        // Tool results first: they answer the call that closed the previous
        // turn, and the person's words came after them (a client that held a
        // result for approval sends both in one request).
        messages.push(message);
    }

    (system_prompt, messages)
}

/// A user turn that only stands in for an image (`[图片]`, `[image]`).
/// A caption a client puts on a tool's picture it re-sends as a user turn.
fn is_tool_image_caption(text: &str) -> bool {
    let t = text.trim().to_ascii_lowercase();
    t.len() < 80 && (t.starts_with("attached image") || t.contains("from tool result"))
}

fn is_image_placeholder(text: &str) -> bool {
    let rest = text
        .replace("[图片]", "")
        .replace("[image]", "")
        .replace("[Image]", "");
    rest.trim().is_empty()
}

/// Push inbound messages to the human's WebSocket connections.
fn push_messages_to_human(app: &AppState, inbound_messages: Vec<Message>, human_id: String) {
    let app = app.clone();
    tokio::spawn(async move {
        for msg in &inbound_messages {
            // A tool result is not a chat message. Delivering it as
            // `message.received` makes the console render an unrelated text
            // bubble while the tool card it belongs to stays on "pending"
            // forever — the console has no way to correlate the two. Send
            // the dedicated `tool.result` event instead.
            if let Some(event) = build_tool_result_event(&app, msg).await {
                let event_json = crate::ws::protocol::event_to_json(&event);
                let delivered = app.connections.send_to_human(&human_id, &event_json).await;
                tracing::info!(
                    delivered = delivered,
                    tool_call_id = %msg.metadata.get("tool_call_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown"),
                    "[tool_result] routed back to the originating call"
                );
                if delivered {
                    // Otherwise a reconnect replays it through the backlog,
                    // where it is indistinguishable from a chat message and
                    // shows up as a stray text bubble.
                    app.message
                        .mark_delivered(std::slice::from_ref(&msg.id))
                        .await;
                }
                continue;
            }

            let images: Vec<String> = msg
                .metadata
                .get("images")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();

            let mut event = if images.is_empty() {
                crate::ws::protocol::message_received(
                    &msg.conversation_id,
                    &msg.id,
                    &msg.sender.participant_id,
                    &format!("{:?}", msg.sender.participant_type).to_lowercase(),
                    msg.sender.display_name.as_deref(),
                    msg.content.as_text().unwrap_or(""),
                )
            } else {
                crate::ws::protocol::message_received_with_images(
                    &msg.conversation_id,
                    &msg.id,
                    &msg.sender.participant_id,
                    &format!("{:?}", msg.sender.participant_type).to_lowercase(),
                    msg.sender.display_name.as_deref(),
                    msg.content.as_text().unwrap_or(""),
                    &images,
                )
            };
            // Raw context for responders that read it (agent seats): what
            // this request carried beyond the cleaned text, and the request's
            // shape (client, auxiliary call, output contract).
            for key in ["request", "raw_tail"] {
                if let (Some(v), Some(obj)) = (msg.metadata.get(key), event.data.as_object_mut()) {
                    obj.insert(key.to_string(), v.clone());
                }
            }
            let event_json = crate::ws::protocol::event_to_json(&event);
            let delivered = app.connections.send_to_human(&human_id, &event_json).await;
            if delivered {
                tracing::info!(
                    "Pushed message to human WS: conversation_id={}, message_id={}",
                    msg.conversation_id,
                    msg.id
                );
                // Mark it seen now, not just on reconnect. Leaving it
                // pending means the next reconnect replays a message the
                // human already answered.
                app.message
                    .mark_delivered(std::slice::from_ref(&msg.id))
                    .await;
            } else {
                tracing::info!(
                    "Human offline, message persisted as Pending: conversation_id={}, message_id={}",
                    msg.conversation_id,
                    msg.id
                );
                let sender_name = msg
                    .sender
                    .display_name
                    .clone()
                    .unwrap_or_else(|| "AI Agent".to_string());
                let preview = msg.content.as_text().unwrap_or("").to_string();
                crate::notification::dispatch_offline_notification(
                    human_id.clone(),
                    sender_name,
                    msg.conversation_id.clone(),
                    preview,
                );
            }
        }
    });
}

/// Build a `tool.result` event for an inbound `role: "tool"` message.
///
/// Returns `None` for anything that is not a tool result, so the caller
/// falls through to the normal `message.received` path.
///
/// The `tool_id` is recovered from the stored call the result answers: the
/// agent only echoes `tool_call_id`, and the console needs the tool name to
/// know which card to settle.
async fn build_tool_result_event(
    app: &AppState,
    msg: &Message,
) -> Option<crate::model::event::Event> {
    let is_tool_role = msg.metadata.get("openai_role").and_then(|v| v.as_str()) == Some("tool");
    if !is_tool_role {
        return None;
    }

    let tool_call_id = msg
        .metadata
        .get("tool_call_id")
        .and_then(|v| v.as_str())?
        .to_string();

    let tool_id = app
        .message
        .find_tool_call(&msg.conversation_id, &tool_call_id)
        .await
        .and_then(|m| {
            m.metadata
                .get("tool_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .or_else(|| crate::session::tool_name_for(&msg.conversation_id, &tool_call_id))
        .or_else(|| {
            msg.metadata
                .get("echo_tool")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "unknown".to_string());

    let output = serde_json::Value::String(msg.content.as_text().unwrap_or("").to_string());

    let mut event = crate::ws::protocol::tool_result_event(
        &msg.conversation_id,
        &tool_call_id,
        &tool_id,
        "success",
        Some(&output),
        None,
    );
    if let Some(obj) = event.data.as_object_mut() {
        for key in ["images", "client_tool_call_id"] {
            if let Some(v) = msg.metadata.get(key) {
                obj.insert(key.to_string(), v.clone());
            }
        }
    }
    Some(event)
}

// ── Handler ──

/// Handler for `POST /v1/chat/completions`.
///
/// Phase 7.1.1: Extracts agent profile (tools, context) from HTTP request.
/// Phase 7.3: Supports human-triggered tool_calls responses.
pub async fn chat_completions(
    State(app): State<AppState>,
    principal: AuthPrincipal,
    headers: HeaderMap,
    Json(req): Json<ChatCompletionRequest>,
) -> Response {
    // ── Filter meta-requests from WorkBuddy ──
    //
    // WorkBuddy sends multiple requests per user message:
    //   - x-agent-purpose: conversation_topic → generates a title (not real chat)
    //   - x-agent-purpose: conversation       → the actual user conversation
    //
    // The topic request wraps the user's text in <session>...</session> tags
    // and uses a system prompt about title generation. We must NOT push these
    // to the human's chat timeline.
    //
    // Strategy: If x-agent-purpose is present and != "conversation", treat it
    // as a meta-request — respond with a minimal auto-generated reply without
    // forwarding to the human.
    let agent_purpose = headers
        .get("x-agent-purpose")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("conversation");

    let is_meta_request = agent_purpose != "conversation";

    if is_meta_request {
        tracing::info!(
            agent_purpose = %agent_purpose,
            "Meta-request detected — auto-responding without human"
        );
        return handle_meta_request(req, agent_purpose);
    }

    // 1. Extract agent profile from the HTTP request.
    let profile = AgentProfile::extract(&headers, &req, &principal.agent.id);
    tracing::info!(
        agent_id = %profile.agent_id,
        client_type = %profile.client_type,
        tools_count = profile.capabilities.len(),
        "Extracted agent profile from HTTP request"
    );

    let auxiliary = is_auxiliary_request(&req);
    let tokenizer = headers
        .get("x-omni2api-tokenizer")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(crate::session::default_tokenizer);
    if !crate::session::tokenizer_available(&tokenizer) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": {
                "message": format!("Unknown tokenizer '{}'.", tokenizer),
                "type": "invalid_request_error",
                "code": "unknown_tokenizer",
                "available": crate::session::available_tokenizers(),
            }})),
        )
            .into_response();
    }
    let prompt_tokens = crate::session::count_request_tokens(&tokenizer, &req);

    // 2. Process inbound messages
    let (system_prompt, mut inbound_messages) =
        process_inbound_messages(&req, &principal, "pending", &profile.client_type);

    // Auxiliary calls carry no client fingerprint of their own: they belong
    // to whichever client this agent's main chat is running in.
    let client_name = {
        let mut detected = profile.client_type.to_string();
        if detected == "unknown" {
            if let Some(n) = client_from_user_agent(&headers) {
                detected = n;
            }
        }
        if auxiliary {
            if detected == "unknown" || detected == "custom" {
                crate::session::last_client(&principal.agent.id).unwrap_or(detected)
            } else {
                detected
            }
        } else {
            crate::session::set_last_client(&principal.agent.id, &detected);
            detected
        }
    };

    let mut info = request_info(&req, &profile, system_prompt.as_deref(), auxiliary);
    info["client"] = Value::String(client_name.clone());
    info["prompt_tokens"] = json!(prompt_tokens);
    for msg in &mut inbound_messages {
        if msg.metadata.contains_key("raw_tail") {
            msg.metadata.insert("request".to_string(), info.clone());
        }
    }

    // 3. Resolve which conversation this request belongs to.
    //
    //    An explicit id (header, body field, `user`) wins. Without one, the
    //    replies we sent earlier identify the conversation: the caller
    //    replays them in its history. Only a request carrying none of our
    //    replies (a fresh session) falls back to the fingerprint key.
    let session_key = resolve_session_key(&headers, &req, system_prompt.as_deref(), auxiliary);
    let fallback_key = session_key.starts_with("prm:") || session_key.starts_with("cli:");
    let continued = if fallback_key && !auxiliary {
        let digests = history_digests(&req);
        match crate::session::lookup_continuity(&principal.agent.id, &digests) {
            Some(id) => app
                .conversation
                .get(&id)
                .await
                .filter(|c| c.human_id() == Some(principal.human.id.as_str())),
            None => None,
        }
    } else {
        None
    };
    tracing::info!(
        agent_id = %principal.agent.id,
        session_key = %session_key,
        continued = continued.is_some(),
        auxiliary = auxiliary,
        "Resolved conversation session scope"
    );

    let request_started = Utc::now();
    let conversation_id =
        {
            let cnv = match continued {
                Some(c) => c,
                None => {
                    app.conversation
                        .find_or_create(
                            &principal.agent.id,
                            &principal.human.id,
                            &session_key,
                            system_prompt.clone(),
                        )
                        .await
                }
            };
            // A conversation minted by this very request is a new session on
            // the caller's side: say so, so both a human console and an agent
            // seat can open it as its own thread.
            if !auxiliary && cnv.created_at >= request_started - chrono::Duration::seconds(1) {
                let event = crate::ws::protocol::conversation_started(
                    &cnv.id,
                    &principal.agent.id,
                    &principal.agent.name,
                    &client_name,
                );
                let _ = app
                    .connections
                    .send_to_human(
                        &principal.human.id,
                        &crate::ws::protocol::event_to_json(&event),
                    )
                    .await;
            }

            // Route tool results to the calls we emitted (clients may rewrite
            // the ids), and deliver each result and each user turn once: a
            // client that retries a request, or replays it after a 409, must
            // not hand the responder the same thing twice.
            let message_count = req.messages.len();
            inbound_messages.retain_mut(|msg| {
            msg.conversation_id = cnv.id.clone();
            let is_tool = msg.metadata.get("openai_role").and_then(|v| v.as_str()) == Some("tool");
            let content = msg.content.as_text().unwrap_or("").to_string();
            if is_tool {
                let client_id = msg
                    .metadata
                    .get("tool_call_id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let echo_tool = msg
                    .metadata
                    .get("echo_tool")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let echo_args = msg.metadata.get("echo_args").cloned().unwrap_or(Value::Null);
                let resolved = crate::session::resolve_call_id(
                    &cnv.id,
                    client_id.as_deref(),
                    echo_tool.as_deref().map(|t| (t, &echo_args)),
                );
                if let Some(id) = &resolved {
                    if Some(id) != client_id.as_ref() {
                        tracing::info!(
                            client_id = ?client_id,
                            resolved = %id,
                            "[tool_result] client rewrote the call id; routed by tool and arguments"
                        );
                    }
                    msg.metadata
                        .insert("tool_call_id".to_string(), Value::String(id.clone()));
                }
                let key = resolved.as_deref().or(client_id.as_deref());
                crate::session::first_result(&cnv.id, key, &content)
            } else {
                let images = msg
                    .metadata
                    .get("images")
                    .map(|v| v.to_string())
                    .unwrap_or_default();
                let fingerprint = format!("user:{}:{}:{}", message_count, content, images);
                crate::session::first_user_turn(&cnv.id, &fingerprint)
            }
        });
            for msg in &inbound_messages {
                app.message.insert(msg.clone()).await;
            }
            cnv.id
        };

    // 4. Context and capabilities of this conversation. Auxiliary calls
    //    carry their own small prompt and no tools; recording them would
    //    overwrite the real chat's context with a title-generator prompt.
    //    An auxiliary call answered through its one tool (Grok's
    //    session_title) in a conversation of its own records that tool, so
    //    the seat can call it.
    let aux_tool = auxiliary && forced_tool(&req).is_some() && session_key.starts_with("aux:");
    if !auxiliary || aux_tool {
        let capabilities = profile.capabilities.clone();
        let has_tools = !capabilities.is_empty();
        let context = profile.to_context();
        let agent_id = profile.agent_id.clone();
        let human_id = principal.human.id.clone();
        let conv_for_caps = conversation_id.clone();
        crate::session::set_context(
            &conversation_id,
            json!({
                "client": client_name,
                "system_prompt": system_prompt,
                "tools": capabilities,
                "context": context,
            }),
        );
        let app_clone = app.clone();
        tokio::spawn(async move {
            // Only update capabilities if the request actually has tools.
            if has_tools {
                app_clone
                    .capability
                    .set_capabilities(&agent_id, capabilities)
                    .await;
            }
            app_clone.context.set_context(&agent_id, context).await;

            let caps = app_clone.capability.get_capabilities(&agent_id).await;
            let ctx = app_clone.context.get_context(&agent_id).await;
            let event = crate::ws::protocol::capability_updated(
                &conv_for_caps,
                &agent_id,
                &caps,
                ctx.as_ref(),
            );
            if !capability_changed(&human_id, &agent_id, &event.data) {
                return;
            }
            let event_json = crate::ws::protocol::event_to_json(&event);
            let _ = app_clone
                .connections
                .send_to_human(&human_id, &event_json)
                .await;
        });
    }

    // 5. Keepalive interval based on agent's client_type
    let keepalive_secs = principal.agent.client_type.keepalive_secs();
    let human_id = principal.human.id.clone();

    // Check if the client requested usage info in streaming mode.
    // WorkBuddy and Claude Code send `stream_options: { include_usage: true }`.
    let include_usage = req
        .stream_options
        .as_ref()
        .and_then(|so| so.get("include_usage"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // 5b. A human reply that arrived after the previous request ended would
    //     otherwise be lost: there was nothing in flight to carry it. Surface
    //     it now, on the next request, instead of making the caller wait for
    //     an answer the human already gave.
    if let Some(backlog) = app.outbound.drain(&conversation_id).await {
        let payload = select_backlog_payload(&backlog);
        tracing::info!(
            conversation_id = %conversation_id,
            backlog_len = backlog.len(),
            "Delivering reply that was parked after the previous request ended"
        );
        // The new message still reaches the human — they can answer it, and
        // that answer rides the next request the same way.
        push_messages_to_human(&app, inbound_messages, human_id.clone());
        return handle_backlog(
            app,
            conversation_id,
            payload,
            req.stream,
            is_async_mode(&headers),
            include_usage,
            TurnCtx {
                agent_id: principal.agent.id.clone(),
                prompt_tokens,
                tokenizer: tokenizer.clone(),
            },
        )
        .await;
    }

    // 6. Branch: streaming vs non-streaming vs async.
    //
    // A second concurrent turn on one conversation is refused outright:
    // replies route per conversation, so two open requests would race for
    // the same answer and one of them would hang until timeout. The caller
    // retries after the first turn closes.
    if !is_async_mode(&headers) && app.reply.has_waiter(&conversation_id).await {
        // A second request while a turn is open is the same caller
        // resubmitting (fast client retry, or a tunnel replaying it). The
        // newest request takes over the turn: register() drops the older
        // one's channel, so the reply goes to the connection still listening.
        tracing::info!(conversation_id = %conversation_id, "resubmitted turn supersedes the open one");
    }

    if req.stream {
        handle_streaming(
            app,
            conversation_id,
            keepalive_secs,
            inbound_messages,
            human_id,
            include_usage,
            TurnCtx {
                agent_id: principal.agent.id.clone(),
                prompt_tokens,
                tokenizer: tokenizer.clone(),
            },
        )
        .await
    } else if is_async_mode(&headers) {
        handle_async(app, conversation_id, inbound_messages, human_id).await
    } else {
        handle_non_streaming(
            app,
            conversation_id,
            inbound_messages,
            human_id,
            TurnCtx {
                agent_id: principal.agent.id.clone(),
                prompt_tokens,
                tokenizer: tokenizer.clone(),
            },
        )
        .await
    }
}

/// Pick which parked payload answers the current request.
///
/// A tool invocation wins over prose: it terminates the turn, whereas text
/// would leave the agent waiting for more. Otherwise the parked prose is
/// joined so a human who sent several replies loses none of them.
///
/// Detection parses the payload rather than substring-matching for
/// `"tool.invoke"`: a human typing the words "tool.invoke" in a normal
/// reply must not be mistaken for a tool call.
fn select_backlog_payload(backlog: &[String]) -> String {
    backlog
        .iter()
        .rev()
        .find(|p| matches!(parse_reply(p), HumanReply::ToolCalls { .. }))
        .cloned()
        .unwrap_or_else(|| backlog.join("\n\n"))
}

/// Answer immediately from a parked reply instead of waiting on the human.
/// What a turn needs to know about the request that opened it.
#[derive(Clone)]
pub(crate) struct TurnCtx {
    pub agent_id: String,
    pub prompt_tokens: u32,
    pub tokenizer: String,
}

impl TurnCtx {
    /// Remember what this turn sent, so the caller's next request (which
    /// replays it) is recognized as the same conversation and its tool
    /// results route back to these calls.
    fn record(&self, conversation_id: &str, content: &str, calls: &[(String, String, Value)]) {
        if calls.is_empty() {
            crate::session::turn_answered(conversation_id);
        }
        if let Some(d) = crate::session::text_digest(content) {
            crate::session::remember_reply(&self.agent_id, d, conversation_id);
        }
        for (id, name, args) in calls {
            crate::session::record_call(conversation_id, id, name, args);
            crate::session::remember_reply(
                &self.agent_id,
                crate::session::call_digest(name, args),
                conversation_id,
            );
        }
    }

    fn usage(&self, completion_text: &str) -> crate::openai_compat::Usage {
        let completion = crate::session::count_tokens_with(&self.tokenizer, completion_text);
        crate::openai_compat::Usage {
            prompt_tokens: self.prompt_tokens,
            completion_tokens: completion,
            total_tokens: self.prompt_tokens + completion,
        }
    }
}

/// Seconds between SSE heartbeats while a turn waits for its responder.
/// Short by default: some clients give up on a silent stream within
/// seconds (Trae retried every ~5 s).
fn sse_heartbeat_secs() -> u64 {
    std::env::var("OMNI2API_SSE_HEARTBEAT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v >= 1)
        .unwrap_or(3)
}

/// Heartbeat frame: an SSE comment by default (ignored by every SSE
/// parser), or an empty-delta chunk with `OMNI2API_SSE_HEARTBEAT=chunk`.
fn heartbeat_event(completion_id: &str, model: &str) -> SseEvent {
    if matches!(
        std::env::var("OMNI2API_SSE_HEARTBEAT").as_deref(),
        Ok("chunk")
    ) {
        let chunk = ChatCompletionChunk::heartbeat(completion_id, model);
        SseEvent::default().data(serde_json::to_string(&chunk).unwrap())
    } else {
        SseEvent::default().comment("keepalive")
    }
}

async fn handle_backlog(
    app: AppState,
    conversation_id: String,
    payload: String,
    stream: bool,
    async_mode: bool,
    include_usage: bool,
    turn: TurnCtx,
) -> Response {
    let model_name = app.model.get();
    if async_mode {
        // Short-circuit: create and complete in one step so the caller's
        // first poll already has the answer.
        let invocation_id = app.async_reply.create(&conversation_id).await;
        app.async_reply.complete(&invocation_id, &payload).await;
        return (
            StatusCode::ACCEPTED,
            Json(json!({
                "id": invocation_id,
                "object": "chat.completion.async",
                "status": "completed",
                "conversation_id": conversation_id,
                "poll_url": format!("/v1/replies/{}", invocation_id),
                "source": "outbound_backlog",
            })),
        )
            .into_response();
    }

    if !stream {
        let body = build_nonstreaming_json(&app, &conversation_id, &payload, &turn).await;
        return Response::builder()
            .header("Content-Type", "application/json")
            .body(Body::from(body))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
            .into_response();
    }

    let completion_id = format!("chatcmpl-{}", Uuid::now_v7().simple());
    let app_c = app.clone();
    let conv = conversation_id.clone();
    let reply = parse_reply(&payload);

    let sse = async_stream::stream! {
        let role_chunk = ChatCompletionChunk::role_header(&completion_id, &model_name);
        yield Ok::<SseEvent, std::convert::Infallible>(
            SseEvent::default().data(serde_json::to_string(&role_chunk).unwrap())
        );

        let mut sent_text = String::new();
        match reply {
            HumanReply::Text { text, .. } => {
                if let Some(r) = crate::session::take_reasoning(&conv) {
                    let chunk = ChatCompletionChunk::reasoning_delta(&completion_id, &r, &model_name);
                    yield Ok(SseEvent::default().data(serde_json::to_string(&chunk).unwrap()));
                }
                for word in text.split_inclusive(char::is_whitespace) {
                    let chunk = ChatCompletionChunk::content_delta(&completion_id, word, &model_name);
                    yield Ok(SseEvent::default().data(serde_json::to_string(&chunk).unwrap()));
                }
                sent_text.push_str(&text);
                turn.record(&conv, &text, &[]);
                app_c.reply.remove(&conv).await;
                let chunk = ChatCompletionChunk::finish(&completion_id, &model_name);
                yield Ok(SseEvent::default().data(serde_json::to_string(&chunk).unwrap()));
            }
            HumanReply::ToolCalls { calls, text } => {
                let prose = text.filter(|t| !t.trim().is_empty()).unwrap_or_default();
                for word in prose.split_inclusive(char::is_whitespace) {
                    let chunk = ChatCompletionChunk::content_delta(&completion_id, word, &model_name);
                    yield Ok(SseEvent::default().data(serde_json::to_string(&chunk).unwrap()));
                }
                sent_text.push_str(&prose);

                let mut recorded = Vec::new();
                for (ci, call) in calls.iter().enumerate() {
                    let call_id = call
                        .invocation_id
                        .clone()
                        .filter(|id| !id.trim().is_empty())
                        .unwrap_or_else(|| format!("call_{}", Uuid::now_v7().simple()));
                    let chunk = ChatCompletionChunk::tool_call_delta_at(
                        &completion_id,
                        ci as u32,
                        &call_id,
                        &call.tool_name,
                        &call.arguments.to_string(),
                        &model_name,
                    );
                    yield Ok(SseEvent::default().data(serde_json::to_string(&chunk).unwrap()));
                    sent_text.push_str(&call.arguments.to_string());
                    recorded.push((call_id, call.tool_name.clone(), call.arguments.clone()));
                }
                turn.record(&conv, &prose, &recorded);

                app_c.reply.remove(&conv).await;
                let chunk = ChatCompletionChunk::finish_tool_calls(&completion_id, &model_name);
                yield Ok(SseEvent::default().data(serde_json::to_string(&chunk).unwrap()));
            }
            HumanReply::EndTurn => {
                app_c.reply.remove(&conv).await;
                let chunk = ChatCompletionChunk::finish(&completion_id, &model_name);
                yield Ok(SseEvent::default().data(serde_json::to_string(&chunk).unwrap()));
            }
        }

        if include_usage {
            let chunk = ChatCompletionChunk::usage_chunk_with(&completion_id, turn.usage(&sent_text), &model_name);
            yield Ok(SseEvent::default().data(serde_json::to_string(&chunk).unwrap()));
        }

        yield Ok(SseEvent::default().data("[DONE]"));
    };

    Sse::new(sse)
        .keep_alive(KeepAlive::default())
        .into_response()
}

// ── Async mode ──

/// Header that opts a request into asynchronous reply mode.
const ASYNC_MODE_HEADER: &str = "x-omni2api-mode";

/// How long the blocking path waits for a human reply.
///
/// Overridable because 5 minutes is arbitrary: some deployments want a short
/// fail-fast window, others are willing to hold a connection for an hour.
/// Callers that need longer than any sane HTTP timeout should use async mode
/// instead of raising this.
fn reply_timeout_secs() -> u64 {
    std::env::var("OMNI2API_REPLY_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(300)
}

/// Whether the caller asked for asynchronous delivery.
///
/// Accepts `x-omni2api-mode: async`, and also honours a global override via
/// `OMNI2API_ASYNC_MODE=always` for deployments where no request should ever
/// block on a human.
fn is_async_mode(headers: &HeaderMap) -> bool {
    let always = std::env::var("OMNI2API_ASYNC_MODE")
        .map(|v| v.trim().eq_ignore_ascii_case("always"))
        .unwrap_or(false);
    if always {
        return true;
    }

    headers
        .get(ASYNC_MODE_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().eq_ignore_ascii_case("async"))
        .unwrap_or(false)
}

/// Asynchronous handler — hand the turn to the human and return at once.
///
/// Answers `202 Accepted` with an `invocation_id` instead of blocking. The
/// agent then polls `GET /v1/replies/{invocation_id}` (optionally long-polling
/// with `?wait=<secs>`) until the human answers.
///
/// This exists because the blocking path can only ever wait as long as an
/// HTTP client is willing to hold a connection open, and humans routinely
/// take longer than that.
async fn handle_async(
    app: AppState,
    conversation_id: String,
    inbound_messages: Vec<Message>,
    human_id: String,
) -> Response {
    let invocation_id = app.async_reply.create(&conversation_id).await;

    // Deliver the turn to the human. The invocation id rides along so the
    // console can echo it back on `reply.send` and complete this invocation
    // rather than a stale one.
    let app_clone = app.clone();
    let conv = conversation_id.clone();
    let inv = invocation_id.clone();
    tokio::spawn(async move {
        // `push_messages_to_human` spawns its own task; the invocation event
        // is emitted alongside it so the console learns the id either way.
        push_messages_to_human(&app_clone, inbound_messages, human_id.clone());
        let event = crate::ws::protocol::async_invocation_created(&conv, &inv);
        let event_json = crate::ws::protocol::event_to_json(&event);
        let _ = app_clone
            .connections
            .send_to_human(&human_id, &event_json)
            .await;
    });

    tracing::info!(
        conversation_id = %conversation_id,
        invocation_id = %invocation_id,
        "Async invocation created — awaiting human reply"
    );

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "id": invocation_id,
            "object": "chat.completion.async",
            "status": "pending",
            "conversation_id": conversation_id,
            "poll_url": format!("/v1/replies/{}", invocation_id),
            // Tell the agent how to wait without spinning.
            "hint": "Poll poll_url until status is \"completed\". ?wait=<secs> long-polls."
        })),
    )
        .into_response()
}

// ── Meta-request handler ──

/// Handle WorkBuddy meta-requests (e.g., conversation_topic) without
/// forwarding to the human.
///
/// These requests are internal to WorkBuddy's workflow — they generate
/// session titles, intent classifications, etc. We auto-respond with
/// a minimal reply so WorkBuddy doesn't block waiting for human input.
fn handle_meta_request(req: ChatCompletionRequest, purpose: &str) -> Response {
    // Meta answers are synthesized on the spot; echo the caller's own
    // model name back, as a real model endpoint would.
    let model_name = req.model.clone().unwrap_or_else(|| "omni2api".to_string());
    // Generate a minimal response based on the purpose.
    let reply = match purpose {
        "conversation_topic" => {
            // Extract the user's text from <session>...</session> tags.
            let raw = req
                .messages
                .iter()
                .filter(|m| m.role == crate::openai_compat::ChatRole::User)
                .map(|m| m.content.to_text())
                .collect::<Vec<_>>()
                .join(" ");

            // Try to extract content from <session> tags.
            let title =
                extract_session_content(&raw).unwrap_or_else(|| "New conversation".to_string());

            // WorkBuddy expects a JSON object with title. Serialized, not
            // format!-built: a title containing a quote would otherwise
            // break the JSON and the topic request with it.
            serde_json::to_string(&json!({
                "isNewTopic": true,
                "title": title.trim(),
            }))
            .unwrap_or_else(|_| r#"{"isNewTopic":true,"title":"New conversation"}"#.to_string())
        }
        _ => "ok".to_string(),
    };

    if req.stream {
        // For streaming meta-requests, send the reply as SSE chunks.
        let completion_id = format!("chatcmpl-{}", Uuid::now_v7().simple());
        let stream = async_stream::stream! {
            let role_chunk = ChatCompletionChunk::role_header(&completion_id, &model_name);
            yield Ok::<SseEvent, std::convert::Infallible>(
                SseEvent::default().data(serde_json::to_string(&role_chunk).unwrap())
            );

            for word in reply.split_inclusive(char::is_whitespace) {
                let chunk = ChatCompletionChunk::content_delta(&completion_id, word, &model_name);
                yield Ok(SseEvent::default().data(serde_json::to_string(&chunk).unwrap()));
            }

            let chunk = ChatCompletionChunk::finish(&completion_id, &model_name);
            yield Ok(SseEvent::default().data(serde_json::to_string(&chunk).unwrap()));

            let chunk = ChatCompletionChunk::usage_chunk(&completion_id, &model_name);
            yield Ok(SseEvent::default().data(serde_json::to_string(&chunk).unwrap()));

            yield Ok(SseEvent::default().data("[DONE]"));
        };
        Sse::new(stream).into_response()
    } else {
        Json(ChatCompletionResponse::from_human_reply(
            &reply,
            &model_name,
        ))
        .into_response()
    }
}

/// Extract content from `<session>...</session>` XML tags.
fn extract_session_content(text: &str) -> Option<String> {
    let start = text.find("<session>")?;
    let end = text.rfind("</session>")?;
    let content = &text[start + "<session>".len()..end];
    let trimmed = content.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

// NOTE: inbound user-message normalization used to live here as
// `extract_user_message` (WorkBuddy `<user_query>` + `<session>` only).
// It now lives in `crate::normalize`, with one normalizer per agent
// family — see that module for the observed formats.

// ── Non-streaming handler ──

/// How often the non-streaming path writes a keepalive byte.
///
/// 5 seconds matches the SSE heartbeat — comfortably below the ~60s idle
/// threshold most proxies use to declare a connection dead.
fn nonstreaming_keepalive_secs() -> u64 {
    std::env::var("OMNI2API_KEEPALIVE_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(5)
}

/// Non-streaming handler: wait for the human, then return one JSON body.
///
/// The response is written **incrementally** even though the payload is a
/// single JSON object. A `stream: false` request that sits silent for two
/// minutes gets killed by an idle-response timeout — Cloudflare answers
/// `524` at roughly 100 seconds, well before the human has had time to
/// read the question, let alone answer it.
///
/// The fix is to keep bytes moving: we emit a bare `\n` every few seconds
/// and then the JSON object. RFC 8259 permits whitespace before a JSON
/// value, so every conforming parser — including the OpenAI SDKs, which
/// call `response.json()` — ignores the padding. The caller sees an
/// ordinary `application/json` response, unchanged except that it arrives
/// late instead of never.
async fn handle_non_streaming(
    app: AppState,
    conversation_id: String,
    inbound_messages: Vec<Message>,
    human_id: String,
    turn: TurnCtx,
) -> Response {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    app.reply.register(&conversation_id, tx).await;
    // Cloned: the timeout branch below still needs the human id to announce
    // the handoff, and the success branch shadows it with the resolved one.
    push_messages_to_human(&app, inbound_messages, human_id.clone());

    // Configurable via OMNI2API_REPLY_TIMEOUT_SECS. Callers that need to
    // wait longer than any HTTP client tolerates should use async mode
    // (`x-omni2api-mode: async`) rather than raising this.
    let timeout = Duration::from_secs(reply_timeout_secs());
    let keepalive = Duration::from_secs(nonstreaming_keepalive_secs());

    let app_s = app.clone();
    let conv_s = conversation_id.clone();
    let human_s = human_id.clone();

    let body = async_stream::stream! {
        let mut ticker = interval(keepalive);
        ticker.tick().await;
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());

            let out: Bytes = tokio::select! {
                _ = ticker.tick() => Bytes::from_static(b"\n"),
                maybe_payload = rx.recv() => {
                    match maybe_payload {
                        Some(first) => {
                            // Let the rest of this turn arrive before
                            // answering, so prose and tool calls can share
                            // one assistant message.
                            let mut payloads = vec![first];
                            payloads.append(&mut collect_rest(&mut rx, merge_window()).await);
                            let reply = merge_replies(&payloads);
                            let json = build_nonstreaming_json_from_reply(
                                &app_s, &conv_s, reply, &turn,
                            ).await;
                            Bytes::from(json)
                        }
                        // Channel dropped — nothing left to wait for.
                        None => break,
                    }
                }
                _ = tokio::time::sleep(remaining) => {
                    let json = handoff_to_async_json(&app_s, &conv_s, &human_s).await;
                    Bytes::from(json)
                }
            };

            // A keepalive is pure padding; anything else is the payload and
            // therefore the last thing we write.
            let is_keepalive = out.as_ref() == b"\n";
            yield Ok::<Bytes, std::convert::Infallible>(out);

            if !is_keepalive {
                break;
            }
        }
    };

    Response::builder()
        .header("Content-Type", "application/json")
        .header("Cache-Control", "no-cache")
        .body(Body::from_stream(body))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        .into_response()
}

/// Turn one human reply into the JSON body of a non-streaming completion.
///
/// Shared by the wait loop above so the reply is parsed, persisted and
/// serialised exactly once.
async fn build_nonstreaming_json(
    app: &AppState,
    conversation_id: &str,
    reply_payload: &str,
    turn: &TurnCtx,
) -> String {
    let reply = parse_reply(reply_payload);
    build_nonstreaming_json_from_reply(app, conversation_id, reply, turn).await
}

async fn build_nonstreaming_json_from_reply(
    app: &AppState,
    conversation_id: &str,
    reply: HumanReply,
    turn: &TurnCtx,
) -> String {
    let model_name = app.model.get();
    let now = Utc::now();
    let conversation = app.conversation.get(conversation_id).await;
    let human_id = conversation
        .as_ref()
        .and_then(|c| c.human_id().map(|s| s.to_string()))
        .unwrap_or_default();

    app.reply.remove(conversation_id).await;

    // Correlation metadata for the stored reply. Populated only for tool
    // calls, so a later `role: "tool"` result can be routed back to the
    // exact call instead of arriving as an orphan message.
    let mut reply_meta: HashMap<String, Value> = HashMap::new();

    let (response, stored_content) = match reply {
        HumanReply::Text { text, .. } => {
            tracing::info!(
                conversation_id = %conversation_id,
                reply_len = text.len(),
                "Human text reply received"
            );
            let mut resp = ChatCompletionResponse::from_human_reply(&text, &model_name);
            resp.usage = turn.usage(&text);
            turn.record(conversation_id, &text, &[]);
            (resp, text)
        }
        HumanReply::ToolCalls { calls, text } => {
            tracing::info!(
                conversation_id = %conversation_id,
                call_count = calls.len(),
                companion_text = ?text,
                "[tool_call] generated for agent"
            );

            let specs: Vec<ToolCallSpec> = calls
                .iter()
                .map(|c| {
                    // Reuse the originating `invocation_id` as the OpenAI
                    // call id so the agent's `role: "tool"` reply can be
                    // paired back to the exact call the human triggered.
                    let call_id = c
                        .invocation_id
                        .clone()
                        .filter(|id| !id.trim().is_empty())
                        .unwrap_or_else(|| format!("call_{}", Uuid::now_v7().simple()));
                    ToolCallSpec {
                        id: call_id,
                        name: c.tool_name.clone(),
                        arguments: c.arguments.clone(),
                    }
                })
                .collect();

            // Record each call so later `role: "tool"` results can be routed
            // back instead of arriving as orphan messages.
            if let Some(first) = specs.first() {
                reply_meta.insert("tool_call_id".to_string(), Value::String(first.id.clone()));
                reply_meta.insert("tool_id".to_string(), Value::String(first.name.clone()));
            }
            reply_meta.insert(
                "tool_call_ids".to_string(),
                Value::Array(specs.iter().map(|s| Value::String(s.id.clone())).collect()),
            );

            // `content` and `tool_calls` coexist: the human's words ride
            // along with the call instead of being discarded.
            let mut resp =
                ChatCompletionResponse::from_tool_calls(specs.clone(), text.clone(), &model_name);
            let prose = text.clone().unwrap_or_default();
            let mut counted = prose.clone();
            let recorded: Vec<(String, String, Value)> = specs
                .iter()
                .map(|s| {
                    counted.push_str(&s.arguments.to_string());
                    (s.id.clone(), s.name.clone(), s.arguments.clone())
                })
                .collect();
            resp.usage = turn.usage(&counted);
            turn.record(conversation_id, &prose, &recorded);

            let content = serde_json::to_string(&serde_json::json!({
                "tool_calls": specs.iter().map(|s| json!({
                    "tool_call": s.name,
                    "arguments": s.arguments,
                })).collect::<Vec<_>>(),
            }))
            .unwrap_or_else(|_| "{}".to_string());

            (resp, content)
        }
        HumanReply::EndTurn => {
            // Non-streaming mode cannot do multi-turn — an explicit
            // end-of-turn resolves to an empty (acknowledging) reply.
            tracing::info!(
                conversation_id = %conversation_id,
                "Human ended the turn (non-streaming)"
            );
            let resp = ChatCompletionResponse::from_human_reply("(turn ended)", &model_name);
            (resp, "(turn ended)".to_string())
        }
    };

    let mut response = response;
    if let (Some(r), Some(choice)) = (
        crate::session::take_reasoning(conversation_id),
        response.choices.first_mut(),
    ) {
        choice.message.reasoning_content = Some(r);
    }

    // Store the human's reply in conversation history.
    app.message
        .insert(Message {
            id: format!("msg_{}", Uuid::now_v7().simple()),
            conversation_id: conversation_id.to_string(),
            sender: ParticipantRef::human_named(&human_id, "You"),
            content: ContentBlock::text(&stored_content),
            attachments: Vec::new(),
            metadata: reply_meta,
            status: MessageStatus::Sent,
            visibility: crate::model::message::MessageVisibility::HumanVisible,
            created_at: now,
        })
        .await;

    serde_json::to_string(&response).unwrap_or_else(|err| {
        tracing::error!("failed to serialise completion: {err}");
        r#"{"error":{"message":"internal serialisation error"}}"#.to_string()
    })
}

/// Elapsed reply window: keep the turn alive in async mode instead of
/// dropping it, and tell the caller where to pick it up.
///
/// The status code is already committed by the time this runs — the first
/// keepalive byte has gone out — so the handoff is described in the body
/// rather than as a `202`.
async fn handoff_to_async_json(app: &AppState, conversation_id: &str, human_id: &str) -> String {
    app.reply.remove(conversation_id).await;

    let invocation_id = app.async_reply.create(conversation_id).await;

    // Tell the console which invocation to complete. A client that does not
    // track ids still works: the WS handler falls back to the newest pending
    // invocation for this conversation.
    let app_clone = app.clone();
    let conv = conversation_id.to_string();
    let inv = invocation_id.clone();
    let hid = human_id.to_string();
    tokio::spawn(async move {
        let event = crate::ws::protocol::async_invocation_created(&conv, &inv);
        let event_json = crate::ws::protocol::event_to_json(&event);
        let _ = app_clone.connections.send_to_human(&hid, &event_json).await;
    });

    tracing::info!(
        conversation_id = %conversation_id,
        invocation_id = %invocation_id,
        timeout_secs = reply_timeout_secs(),
        "Reply window elapsed — handed off to async mode"
    );

    json!({
        "id": invocation_id,
        "object": "chat.completion.async",
        "status": "pending",
        "conversation_id": conversation_id,
        "poll_url": format!("/v1/replies/{}", invocation_id),
        "timed_out_after_secs": reply_timeout_secs(),
        "hint": "The turn is still open. Poll poll_url until status is \"completed\"; ?wait=<secs> long-polls."
    })
    .to_string()
}

// ── Streaming handler ──

/// Streaming handler: SSE with keepalive, then content chunks, then [DONE].
///
/// If the human sends a tool call reply, the SSE stream emits
/// `tool_calls` delta chunks instead of content deltas.
///
/// If `include_usage` is true, a usage chunk is sent after the
/// finish chunk (per OpenAI `stream_options.include_usage` spec).
async fn handle_streaming(
    app: AppState,
    conversation_id: String,
    keepalive_secs: u64,
    inbound_messages: Vec<Message>,
    human_id: String,
    include_usage: bool,
    turn: TurnCtx,
) -> Response {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    let model_name = app.model.get();
    let is_online = app.connections.is_online(&human_id).await;
    // The presence notice is for a person on the other end. When the only
    // responder online is an agent seat, it would just be written into the
    // caller's transcript as if the agent had said it.
    // A seat that detached a moment ago is an agent between polls: it will
    // pick the request up from the backlog, and "offline" is not news the
    // caller needs written into its transcript either.
    let seat_only = if is_online {
        !app.connections.has_human_device(&human_id).await
    } else {
        crate::seat::seat_recently_detached(&human_id)
    };

    app.reply.register(&conversation_id, tx).await;
    push_messages_to_human(&app, inbound_messages, human_id);

    let completion_id = format!("chatcmpl-{}", Uuid::now_v7().simple());
    let conv_id = conversation_id.clone();
    let app_clone = app.clone();
    // The client-type keepalive is an upper bound; a silent stream is what
    // makes clients give up, so beat at least every `sse_heartbeat_secs`.
    let ka_secs = keepalive_secs.max(1).min(sse_heartbeat_secs());

    let stream = async_stream::stream! {
        // ── Multi-turn reply loop ──
        //
        // The human can send SEVERAL replies per agent request. Each text
        // reply is streamed as content deltas; the SSE stream stays open
        // (with keepalive heartbeats) until the human either:
        //   1. Ends the turn explicitly   → `finish_reason: stop` + [DONE]
        //   2. Requests a tool call      → `finish_reason: tool_calls`
        //      (the client executes the tool and opens a NEW request)

        // Open the stream with the role header at once: a client then has a
        // well-formed first chunk instead of an empty body while the
        // responder thinks.
        let role_chunk = ChatCompletionChunk::role_header(&completion_id, &model_name);
        yield Ok::<SseEvent, std::convert::Infallible>(
            SseEvent::default().data(serde_json::to_string(&role_chunk).unwrap())
        );

        let mut keepalive = interval(Duration::from_secs(ka_secs));
        // The first tick completes immediately; consume it so the next one
        // lands a full interval after the role header.
        keepalive.tick().await;

        // Everything this turn put in front of the caller, for the usage
        // count and the continuity digest.
        let mut sent_text = String::new();

        let notice = if seat_only { None } else { status_notice(is_online) };
        if let Some(notice) = notice {
            for word in notice.split_inclusive(char::is_whitespace) {
                let chunk = ChatCompletionChunk::content_delta(&completion_id, word, &model_name);
                yield Ok(SseEvent::default().data(serde_json::to_string(&chunk).unwrap()));
            }
            sent_text.push_str(&notice);
        }

        let conv_for_cleanup = conv_id.clone();
        let result: HumanReply = loop {
            let msg = tokio::select! {
                _ = keepalive.tick() => {
                    yield Ok::<SseEvent, std::convert::Infallible>(heartbeat_event(&completion_id, &model_name));
                    continue;
                }
                msg = rx.recv() => {
                    match msg {
                        Some(text) => text,
                        None => { return; }
                    }
                }
            };

            // Hold briefly for whatever else the human does: typing and then
            // tapping a tool are two separate events, and answering the first
            // at once strands the second in a later response.
            let mut payloads = vec![msg];
            payloads.append(&mut collect_rest(&mut rx, merge_window()).await);
            let reply = merge_replies(&payloads);

            match reply {
                HumanReply::Text { text, ended } => {
                    tracing::info!(
                        conversation_id = %conv_id,
                        reply_len = text.len(),
                        ended,
                        "Human text reply received (streaming, multi-turn)"
                    );

                    // Resolve the human's identity for storage.
                    let conversation = app_clone.conversation.get(&conv_id).await;
                    let human_id = conversation
                        .as_ref()
                        .and_then(|c| c.human_id().map(|s| s.to_string()))
                        .unwrap_or_default();

                    // Store this reply in the conversation history.
                    app_clone.message.insert(Message {
                        id: format!("msg_{}", Uuid::now_v7().simple()),
                        conversation_id: conv_id.clone(),
                        sender: ParticipantRef::human_named(&human_id, "You"),
                        content: ContentBlock::text(&text),
                        attachments: Vec::new(),
                        metadata: HashMap::new(),
                        status: MessageStatus::Sent,
                        visibility: crate::model::message::MessageVisibility::HumanVisible,
                        created_at: Utc::now(),
                    }).await;

                    if let Some(r) = crate::session::take_reasoning(&conv_id) {
                        let chunk = ChatCompletionChunk::reasoning_delta(&completion_id, &r, &model_name);
                        yield Ok(SseEvent::default().data(serde_json::to_string(&chunk).unwrap()));
                    }

                    // Consecutive replies are separated by a blank line put
                    // in front of the later one. Appending it after every
                    // reply left a trailing "\n\n" on single answers, which
                    // strict consumers (Qoder's title generator) rejected.
                    let payload = if !sent_text.is_empty() && !sent_text.ends_with("\n\n") {
                        format!("\n\n{}", text)
                    } else {
                        text.clone()
                    };
                    for word in payload.split_inclusive(char::is_whitespace) {
                        let chunk = ChatCompletionChunk::content_delta(&completion_id, word, &model_name);
                        yield Ok(SseEvent::default().data(serde_json::to_string(&chunk).unwrap()));
                    }
                    sent_text.push_str(&payload);
                    // Each reply on its own too: a client may store them
                    // apart or only keep the last.
                    turn.record(&conv_id, &text, &[]);

                    // Keep waiting for more — unless the human closed the
                    // turn in the same breath, in which case this prose is
                    // the whole answer.
                    if ended {
                        break HumanReply::EndTurn;
                    }
                    continue;
                }
                HumanReply::ToolCalls { .. } | HumanReply::EndTurn => break reply,
            }
        };

        // ── Turn finished — clean up the reply channel ──
        app_clone.reply.remove(&conv_for_cleanup).await;

        let conversation = app_clone.conversation.get(&conv_for_cleanup).await;
        let human_id = conversation
            .as_ref()
            .and_then(|c| c.human_id().map(|s| s.to_string()))
            .unwrap_or_default();
        let now = Utc::now();

        match &result {
            HumanReply::EndTurn => {
                tracing::info!(
                    conversation_id = %conv_for_cleanup,
                    "Human ended the turn (streaming)"
                );
                turn.record(&conv_for_cleanup, &sent_text, &[]);
                let chunk = ChatCompletionChunk::finish(&completion_id, &model_name);
                yield Ok(SseEvent::default().data(serde_json::to_string(&chunk).unwrap()));
            }
            HumanReply::ToolCalls { calls, text } => {
                let companion = text.clone().filter(|t| !t.trim().is_empty());

                tracing::info!(
                    conversation_id = %conv_for_cleanup,
                    call_count = calls.len(),
                    companion_text = ?companion,
                    "[tool_call] generated for agent (streaming)"
                );

                if let Some(r) = crate::session::take_reasoning(&conv_for_cleanup) {
                    let chunk = ChatCompletionChunk::reasoning_delta(&completion_id, &r, &model_name);
                    yield Ok(SseEvent::default().data(serde_json::to_string(&chunk).unwrap()));
                }

                // Stream the companion prose before the calls so the agent
                // sees both halves of what the human meant to say.
                if let Some(prose) = companion.as_deref() {
                    let payload = if !sent_text.is_empty() && !sent_text.ends_with("\n\n") {
                        format!("\n\n{}", prose)
                    } else {
                        prose.to_string()
                    };
                    for word in payload.split_inclusive(char::is_whitespace) {
                        let chunk = ChatCompletionChunk::content_delta(&completion_id, word, &model_name);
                        yield Ok(SseEvent::default().data(serde_json::to_string(&chunk).unwrap()));
                    }
                    sent_text.push_str(&payload);
                }

                let mut recorded = Vec::new();
                let mut call_text = String::new();
                for (ci, call) in calls.iter().enumerate() {
                    let stored_content = format!(
                        "{{\"tool_call\":\"{}\",\"arguments\":{}}}",
                        call.tool_name, call.arguments
                    );

                    // Reuse the originating invocation id as the OpenAI call
                    // id, so a later `role: "tool"` result can be routed back
                    // to this exact call — see `push_messages_to_human`.
                    let call_id = call
                        .invocation_id
                        .clone()
                        .filter(|id| !id.trim().is_empty())
                        .unwrap_or_else(|| format!("call_{}", Uuid::now_v7().simple()));

                    let mut tool_meta = HashMap::new();
                    tool_meta.insert(
                        "tool_call_id".to_string(),
                        Value::String(call_id.clone()),
                    );
                    tool_meta.insert(
                        "tool_id".to_string(),
                        Value::String(call.tool_name.clone()),
                    );
                    tool_meta.insert(
                        "openai_role".to_string(),
                        Value::String("assistant".to_string()),
                    );

                    app_clone.message.insert(Message {
                        id: format!("msg_{}", Uuid::now_v7().simple()),
                        conversation_id: conv_for_cleanup.clone(),
                        sender: ParticipantRef::human_named(&human_id, "You"),
                        content: ContentBlock::text(&stored_content),
                        attachments: Vec::new(),
                        metadata: tool_meta,
                        status: MessageStatus::Sent,
                        visibility: crate::model::message::MessageVisibility::HumanVisible,
                        created_at: now,
                    }).await;

                    let args = call.arguments.to_string();
                    let chunk = ChatCompletionChunk::tool_call_delta_at(
                        &completion_id,
                        ci as u32,
                        &call_id,
                        &call.tool_name,
                        &args,
                        &model_name,
                    );
                    yield Ok(SseEvent::default().data(serde_json::to_string(&chunk).unwrap()));
                    call_text.push_str(&call.tool_name);
                    call_text.push_str(&args);
                    recorded.push((call_id, call.tool_name.clone(), call.arguments.clone()));
                }
                turn.record(&conv_for_cleanup, &sent_text, &recorded);
                sent_text.push_str(&call_text);

                // Finish chunk with finish_reason: tool_calls.
                let chunk = ChatCompletionChunk::finish_tool_calls(&completion_id, &model_name);
                yield Ok(SseEvent::default().data(serde_json::to_string(&chunk).unwrap()));
            }
            HumanReply::Text { .. } => {
                unreachable!("text replies break out of the loop only via the arm above")
            }
        }

        // Usage chunk (if stream_options.include_usage was requested):
        // counted by the server, whoever answered.
        if include_usage {
            let chunk = ChatCompletionChunk::usage_chunk_with(&completion_id, turn.usage(&sent_text), &model_name);
            yield Ok(SseEvent::default().data(serde_json::to_string(&chunk).unwrap()));
        }

        // [DONE]
        yield Ok(SseEvent::default().data("[DONE]"));
    };

    Sse::new(stream).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_image_captions() {
        assert!(is_tool_image_caption("Attached image(s) from tool result:"));
        assert!(!is_tool_image_caption("这张图是什么颜色？"));
    }
    use crate::openai_compat::ChatCompletionRequest;

    /// Build a request the same way the wire does, so the fixtures used here
    /// cannot drift from what serde actually accepts.
    fn request(body: serde_json::Value) -> ChatCompletionRequest {
        serde_json::from_value(body).expect("fixture must deserialize")
    }

    // ── parse_reply ──

    #[test]
    fn tool_invoke_payload_becomes_a_real_tool_call() {
        // The shape a Human Console sends as a WS `tool.invoke` event. It
        // must not be handed to the agent as a quoted JSON string — that is
        // what silently downgraded tool calls to text with
        // `finish_reason: stop`, so no client ever executed them.
        let payload = r#"{"type":"tool.invoke","invocation_id":"inv_1","tool_id":"Bash","input":{"command":"ls -la"}}"#;

        match parse_reply(payload) {
            HumanReply::ToolCalls { calls, text } => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].tool_name, "Bash");
                assert_eq!(calls[0].arguments, json!({"command": "ls -la"}));
                assert_eq!(calls[0].invocation_id.as_deref(), Some("inv_1"));
                assert!(text.is_none(), "no companion prose in this payload");
            }
            other => panic!("expected a ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn tool_invoke_can_carry_companion_text() {
        // A human who says something and taps a tool in the same breath must
        // not lose the words: OpenAI allows content and tool_calls together.
        let payload =
            r#"{"type":"tool.invoke","tool_id":"Bash","input":{"command":"ls"},"text":"我触发了"}"#;

        match parse_reply(payload) {
            HumanReply::ToolCalls { calls, text } => {
                assert_eq!(calls[0].tool_name, "Bash");
                assert_eq!(text.as_deref(), Some("我触发了"));
            }
            other => panic!("expected a ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn blank_companion_text_is_dropped() {
        let payload = r#"{"type":"tool.invoke","tool_id":"Bash","input":{},"text":"   "}"#;
        match parse_reply(payload) {
            HumanReply::ToolCalls { text, .. } => {
                assert!(
                    text.is_none(),
                    "whitespace-only text must not become content"
                );
            }
            other => panic!("expected a ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn tool_invoke_without_input_defaults_to_empty_object() {
        let payload = r#"{"type":"tool.invoke","tool_id":"Read"}"#;
        match parse_reply(payload) {
            HumanReply::ToolCalls { calls, .. } => {
                assert_eq!(calls[0].tool_name, "Read");
                assert_eq!(calls[0].arguments, json!({}));
                assert!(calls[0].invocation_id.is_none());
            }
            other => panic!("expected a ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn legacy_tool_call_payload_still_works() {
        let payload =
            r#"{"__tool_call__":true,"tool_name":"Bash","arguments":{"command":"echo hi"}}"#;
        match parse_reply(payload) {
            HumanReply::ToolCalls { calls, .. } => {
                assert_eq!(calls[0].tool_name, "Bash");
                assert_eq!(calls[0].arguments, json!({"command": "echo hi"}));
                assert!(
                    calls[0].invocation_id.is_none(),
                    "legacy payloads carry no id"
                );
            }
            other => panic!("expected a ToolCall, got {other:?}"),
        }
    }

    // ── merge_replies: one turn, one assistant message ──

    #[test]
    fn prose_and_tool_call_merge_into_one_message() {
        // The whole point of the merge window: typing and then tapping a
        // tool are two events, and the agent must see one message carrying
        // both, not two responses.
        let payloads = vec![
            "我先看个时间".to_string(),
            r#"{"type":"tool.invoke","invocation_id":"inv_1","tool_id":"get_current_time","input":{}}"#
                .to_string(),
        ];

        match merge_replies(&payloads) {
            HumanReply::ToolCalls { calls, text } => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].tool_name, "get_current_time");
                assert_eq!(
                    text.as_deref(),
                    Some("我先看个时间"),
                    "prose must survive alongside the call"
                );
            }
            other => panic!("expected ToolCalls, got {other:?}"),
        }
    }

    #[test]
    fn several_tools_merge_into_one_array() {
        let payloads = vec![
            r#"{"type":"tool.invoke","invocation_id":"inv_a","tool_id":"Read","input":{}}"#
                .to_string(),
            r#"{"type":"tool.invoke","invocation_id":"inv_b","tool_id":"Bash","input":{"c":"ls"}}"#
                .to_string(),
        ];

        match merge_replies(&payloads) {
            HumanReply::ToolCalls { calls, text } => {
                assert_eq!(calls.len(), 2, "both calls belong to one message");
                assert_eq!(calls[0].invocation_id.as_deref(), Some("inv_a"));
                assert_eq!(calls[1].invocation_id.as_deref(), Some("inv_b"));
                assert!(text.is_none());
            }
            other => panic!("expected ToolCalls, got {other:?}"),
        }
    }

    #[test]
    fn inline_image_is_parsed_not_dropped() {
        // Vision input arrives as an `image_url` part. Dropping it left the
        // human answering a question about a picture they never received.
        let body = json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "这张图里有什么？"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw0KGgo="}}
                ]
            }]
        });

        let req = request(body);
        let content = &req.messages[0].content;

        assert!(content.has_image(), "image part must be recognised");
        assert_eq!(
            content.image_urls(),
            vec!["data:image/png;base64,iVBORw0KGgo="]
        );
        // The text survives, and the image leaves a marker so the human is
        // not staring at a message that looks empty.
        let text = content.to_text();
        assert!(text.contains("这张图里有什么？"), "got {text:?}");
        assert!(text.contains("[图片]"), "got {text:?}");
    }

    #[test]
    fn image_mime_is_read_from_the_data_url() {
        assert_eq!(
            guess_image_type("data:image/jpeg;base64,AAAA"),
            "image/jpeg"
        );
        assert_eq!(guess_image_type("https://x.example/a.png"), "image/png");
    }

    #[test]
    fn merged_turn_travelling_as_an_array_is_recombined() {
        // Async turns submit several payloads as one JSON array; the parser
        // must see that as a single turn, not as a text blob.
        let payload = serde_json::to_string(&vec![
            serde_json::Value::String("我先看个时间".to_string()),
            serde_json::json!({
                "type": "tool.invoke",
                "invocation_id": "inv_1",
                "tool_id": "get_current_time",
                "input": {},
            }),
        ])
        .unwrap();

        match parse_turn(&payload) {
            HumanReply::ToolCalls { calls, text } => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].tool_name, "get_current_time");
                assert_eq!(text.as_deref(), Some("我先看个时间"));
            }
            other => panic!("expected ToolCalls, got {other:?}"),
        }
    }

    #[test]
    fn single_payload_is_never_mistaken_for_a_merged_turn() {
        assert!(matches!(parse_turn("plain text"), HumanReply::Text { .. }));
    }

    #[test]
    fn online_and_offline_both_acknowledge() {
        // A caller holding an open request gets told something either way;
        // silence for a minute is indistinguishable from a lost turn.
        let online = status_notice(true).expect("online turn is acknowledged");
        let offline = status_notice(false).expect("offline turn is acknowledged");

        assert!(online.contains("在线"), "got {online:?}");
        assert!(offline.contains("离线"), "got {offline:?}");
    }

    #[test]
    fn consecutive_prose_is_joined_not_overwritten() {
        let payloads = vec!["first".to_string(), "second".to_string()];
        match merge_replies(&payloads) {
            HumanReply::Text { text: t, .. } => {
                assert!(t.contains("first"), "got {t:?}");
                assert!(t.contains("second"), "got {t:?}");
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn end_turn_alone_yields_no_phantom_content() {
        let payloads = vec![END_TURN_SIGNAL.to_string()];
        assert!(matches!(merge_replies(&payloads), HumanReply::EndTurn));
    }

    #[test]
    fn tool_plus_end_turn_still_reports_the_tool() {
        // Closing the turn must not swallow a call made in the same breath.
        let payloads = vec![
            r#"{"type":"tool.invoke","invocation_id":"inv_1","tool_id":"Read","input":{}}"#
                .to_string(),
            END_TURN_SIGNAL.to_string(),
        ];
        assert!(matches!(
            merge_replies(&payloads),
            HumanReply::ToolCalls { .. }
        ));
    }

    #[test]
    fn other_json_stays_human_text() {
        // A human legitimately pasting JSON must not be reinterpreted as a
        // tool call just because it starts with `{`.
        let payload = r#"{"type":"something.else","tool_id":"Bash"}"#;
        assert!(matches!(parse_reply(payload), HumanReply::Text { .. }));

        let payload = r#"{"note":"call me"}"#;
        assert!(matches!(parse_reply(payload), HumanReply::Text { .. }));
    }

    #[test]
    fn plain_text_and_end_turn() {
        assert!(matches!(
            parse_reply("Sure, on it."),
            HumanReply::Text { .. }
        ));
        assert!(matches!(
            parse_reply("  __END_TURN__  "),
            HumanReply::EndTurn
        ));
    }

    // ── resolve_session_key ──

    fn session_key(headers: &[(&str, &str)], body: serde_json::Value) -> String {
        let mut map = HeaderMap::new();
        for (k, v) in headers {
            let name = axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap();
            map.insert(name, v.parse().unwrap());
        }
        resolve_session_key(&map, &request(body), None, false)
    }

    fn body() -> serde_json::Value {
        json!({"model": "omni2api", "messages": []})
    }

    #[test]
    fn no_identifier_falls_back_to_a_client_scoped_key() {
        // Deliberately *not* a single global "default": callers that say
        // nothing about who they are would otherwise share one conversation
        // — and one outbound queue, so a tool call issued in one client gets
        // collected by a request from another.
        let key = session_key(&[], body());
        assert!(key.starts_with("cli:"), "got {key}");
        assert_ne!(key, "default");
    }

    #[test]
    fn unidentified_callers_do_not_share_a_conversation() {
        let curl = session_key(&[("user-agent", "curl/8.4.0")], body());
        let workbuddy = session_key(&[("user-agent", "WorkBuddy/1.0")], body());
        assert_ne!(
            curl, workbuddy,
            "distinct clients must not land in one conversation"
        );
        // Same client, repeated requests: stable, so the thread continues.
        let again = session_key(&[("user-agent", "curl/8.4.0")], body());
        assert_eq!(curl, again);
    }

    #[test]
    fn same_prompt_different_clients_stay_apart() {
        let req_a = || {
            json!({"messages": [
                {"role": "system", "content": "You are a release engineer."},
                {"role": "user", "content": "hi"},
            ]})
        };

        let prompt = "You are a release engineer.";
        let mut h1 = HeaderMap::new();
        h1.insert("user-agent", "curl/8.4.0".parse().unwrap());
        let a = resolve_session_key(&h1, &request(req_a()), Some(prompt), false);

        let mut h2 = HeaderMap::new();
        h2.insert("user-agent", "WorkBuddy/1.0".parse().unwrap());
        let b = resolve_session_key(&h2, &request(req_a()), Some(prompt), false);

        assert!(a.starts_with("cli:"), "got {a}");
        assert_ne!(a, b, "a shared prompt must not merge two clients");
    }

    #[test]
    fn conversation_id_header_wins() {
        let key = session_key(
            &[
                ("x-omni2api-conversation-id", "design-review"),
                ("x-session-id", "ignored"),
            ],
            body(),
        );
        assert_eq!(key, "hdr:design-review");
    }

    #[test]
    fn conversation_body_field_is_used() {
        let key = session_key(
            &[],
            json!({"messages": [], "conversation": {"id": "conv_7"}}),
        );
        assert_eq!(key, "cnv:conv_7");

        let key = session_key(&[], json!({"messages": [], "conversation": "conv_8"}));
        assert_eq!(key, "cnv:conv_8");
    }

    #[test]
    fn sources_are_namespaced_so_equal_strings_cannot_collide() {
        let by_user = session_key(&[], json!({"messages": [], "user": "alice"}));
        let by_header = session_key(&[("x-session-id", "alice")], body());
        assert_eq!(by_user, "usr:alice");
        assert_eq!(by_header, "ses:alice");
        assert_ne!(by_user, by_header);
    }

    #[test]
    fn distinct_chat_windows_get_distinct_keys() {
        let a = session_key(&[("x-omni2api-conversation-id", "window-a")], body());
        let b = session_key(&[("x-omni2api-conversation-id", "window-b")], body());
        assert_ne!(a, b);
    }

    #[test]
    fn main_chat_key_ignores_a_changing_system_prompt() {
        // Clients rewrite their system prompt between turns (Trae, dsh);
        // keying on it split one session into many. Auxiliary calls still
        // separate by prompt (a title generator is not the chat).
        let h = HeaderMap::new();
        let req = request(body());
        let first = resolve_session_key(&h, &req, Some("You are a release engineer."), false);
        let second = resolve_session_key(
            &h,
            &req,
            Some("You are a release engineer. (turn 2)"),
            false,
        );
        assert_eq!(
            first, second,
            "a rewritten prompt must not start a new session"
        );

        let aux_a = resolve_session_key(&h, &req, Some("Make a title."), true);
        let aux_b = resolve_session_key(&h, &req, Some("Suggest a reply."), true);
        assert!(aux_a.starts_with("aux:prm:"));
        assert_ne!(aux_a, aux_b);
    }

    #[test]
    fn first_message_anchor_ignores_injected_reminders() {
        let h = HeaderMap::new();
        let turn1 = request(json!({"messages": [
            {"role": "user", "content": "<system-reminder>terminals: 0</system-reminder>\n<user_input>hi</user_input>"},
        ]}));
        let turn2 = request(json!({"messages": [
            {"role": "user", "content": "<system-reminder>terminals: 1</system-reminder>\n<user_input>hi</user_input>"},
        ]}));
        assert_eq!(
            resolve_session_key(&h, &turn1, None, false),
            resolve_session_key(&h, &turn2, None, false)
        );
    }

    #[test]
    fn history_digests_find_our_replies() {
        let req = request(json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "hello there\n\n"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "call_theirs", "type": "function", "function": {"name": "Bash", "arguments": "{\"command\":\"ls\"}"}}
            ]},
            {"role": "tool", "tool_call_id": "call_theirs", "content": "a.txt"},
        ]}));
        let d = history_digests(&req);
        assert!(d.contains(&crate::session::call_digest(
            "Bash",
            &json!({"command": "ls"})
        )));
        assert!(d.contains(&crate::session::text_digest("hello there").unwrap()));
    }

    // ── 2026-10-21 seat fixes ──

    #[test]
    fn developer_role_is_accepted_as_system() {
        let req = request(json!({
            "model": "omni2api",
            "messages": [
                {"role": "developer", "content": "be brief"},
                {"role": "user", "content": "hi"}
            ]
        }));
        assert_eq!(req.messages[0].role, crate::openai_compat::ChatRole::System);
    }

    #[test]
    fn expected_format_for_title_and_suggestion() {
        let title: ChatCompletionRequest = serde_json::from_value(json!({
            "messages": [{"role": "user", "content": "Generate a title for this conversation: hi"}]
        }))
        .unwrap();
        assert_eq!(
            expected_format(&title, "").as_deref(),
            Some("only a short title, no quotes or extra text")
        );
        let sugg: ChatCompletionRequest = serde_json::from_value(json!({
            "messages": [{"role": "user", "content": "x"}]
        }))
        .unwrap();
        assert!(expected_format(&sugg, "Reply NO_SUGGESTION if none")
            .unwrap()
            .contains("NO_SUGGESTION"));
        assert_eq!(expected_format(&sugg, "You are helpful."), None);
        let dash = "Write an ultra-short dashboard line that captures the reply.\nOutput ONLY the fragment: 5-12 words.";
        assert!(expected_format(&sugg, dash)
            .unwrap()
            .contains("ultra-short dashboard line"));
        let oc = "You are a title generator. You output ONLY a thread title. Keep exact: technical terms, JSON keys";
        assert_eq!(
            expected_format(&title, oc).as_deref(),
            Some("only a short title, no quotes or extra text")
        );
        let cc = "Return JSON with a single \"title\" field";
        assert!(expected_format(&title, cc).unwrap().contains("{\"title\""));
    }

    #[test]
    fn forced_single_tool_is_a_side_call() {
        let req: ChatCompletionRequest = serde_json::from_value(json!({
            "model": "m",
            "messages": [{"role": "user", "content": "run pwd"}],
            "tools": [{"type": "function", "function": {"name": "session_title",
                "parameters": {"type": "object", "properties": {"session_title": {"type": "string"}}, "required": ["session_title"]}}}],
            "tool_choice": {"type": "function", "function": {"name": "session_title"}}
        })).unwrap();
        assert!(is_auxiliary_request(&req));
        assert!(expected_format(&req, "")
            .unwrap()
            .contains("oh-seat tool session_title session_title="));
        let mut chat = req.clone();
        chat.tool_choice = Some(json!("auto"));
        assert!(!is_auxiliary_request(&chat));
        let mut titled = chat.clone();
        titled.messages.insert(0, serde_json::from_value(json!({"role": "system", "content": "You are tasked with generating the session title."})).unwrap());
        assert!(is_auxiliary_request(&titled));
    }

    #[test]
    fn user_agent_names_unknown_clients() {
        let h = |ua: &str| {
            let mut m = HeaderMap::new();
            m.insert("user-agent", ua.parse().unwrap());
            client_from_user_agent(&m)
        };
        assert_eq!(
            h("reasonix/2.37.0 (linux; arm64)").as_deref(),
            Some("reasonix")
        );
        assert_eq!(h("OpenAI/JS 4.0.0").as_deref(), None);
        assert_eq!(h("node-fetch/1.0 Cline/3.2").as_deref(), Some("cline"));
        let mut m = HeaderMap::new();
        m.insert("user-agent", "OpenAI/JS 5.20.1".parse().unwrap());
        m.insert("x-initiator", "agent".parse().unwrap());
        assert_eq!(client_from_user_agent(&m).as_deref(), Some("copilot"));
        assert_eq!(
            h("ai-sdk/openai-compatible/1.0 ai-sdk/provider-utils/3").as_deref(),
            None
        );
    }

    #[test]
    fn title_calls_are_auxiliary_but_chat_is_not() {
        let title = request(json!({
            "messages": [
                {"role": "system", "content": "Create a concise title"},
                {"role": "user", "content": "Generate the session title"}
            ],
            "max_completion_tokens": 64
        }));
        assert!(is_auxiliary_request(&title));
        let chat = request(json!({
            "messages": [
                {"role": "system", "content": "You are an agent"},
                {"role": "user", "content": "hi"}
            ],
            "tools": [{"type": "function", "function": {"name": "read", "parameters": {}}}],
            "max_tokens": 64
        }));
        assert!(!is_auxiliary_request(&chat));

        // opencode: two user parts, a big cap, a title prompt, and the same
        // x-session-id as the main chat.
        let oc_title = request(json!({
            "messages": [
                {"role": "system", "content": "You are a title generator. You output ONLY a thread title."},
                {"role": "user", "content": "Generate a title for this conversation:\n"},
                {"role": "user", "content": "\"hi\""}
            ],
            "max_tokens": 32000
        }));
        assert!(is_auxiliary_request(&oc_title));
        let mut h = HeaderMap::new();
        h.insert("x-session-id", "ses_1".parse().unwrap());
        assert!(
            resolve_session_key(&h, &oc_title, Some("You are a title generator."), true)
                .starts_with("aux:")
        );

        // A tool-less chat client capping its output is still a chat.
        let plain = request(json!({
            "messages": [
                {"role": "system", "content": "You are a helpful assistant."},
                {"role": "user", "content": "hi"}
            ],
            "max_tokens": 4096
        }));
        assert!(!is_auxiliary_request(&plain));
    }

    #[test]
    fn new_session_of_same_client_gets_its_own_key() {
        let h = HeaderMap::new();
        let s1 = request(json!({"messages": [
            {"role": "system", "content": "P"},
            {"role": "user", "content": "first session"}
        ]}));
        let s1_later = request(json!({"messages": [
            {"role": "system", "content": "P"},
            {"role": "user", "content": "first session"},
            {"role": "assistant", "content": "ok"},
            {"role": "user", "content": "more"}
        ]}));
        let s2 = request(json!({"messages": [
            {"role": "system", "content": "P"},
            {"role": "user", "content": "second session"}
        ]}));
        let k1 = resolve_session_key(&h, &s1, Some("P"), false);
        assert_eq!(k1, resolve_session_key(&h, &s1_later, Some("P"), false));
        assert_ne!(k1, resolve_session_key(&h, &s2, Some("P"), false));
        // Auxiliary calls share one key per prompt, whatever they carry.
        assert_eq!(
            resolve_session_key(&h, &s1, Some("P"), true),
            resolve_session_key(&h, &s2, Some("P"), true)
        );
    }
}
