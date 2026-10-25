//! OpenAI Chat Completions API compatibility types.
//!
//! These types map 1:1 to the OpenAI `/v1/chat/completions` request and
//! response shapes. The internal model ↔ OpenAI translation happens here.
//!
//! Phase 7.1.1: Completed schema — supports `role: "tool"`, `tool_calls`,
//! `tool_choice`, `stream_options`, `reasoning_effort`, and `metadata`.
//!
//! See: `protocol/openai-compat/chat-completions.md`

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── Inbound: OpenAI Request ──

/// `POST /v1/chat/completions` request body.
///
/// All fields that real agents send (per Phase 8 observation data) are
/// accepted. Unknown fields are silently dropped by serde (the raw body
/// is preserved separately by the observation middleware).
#[derive(Debug, Clone, Deserialize)]
pub struct ChatCompletionRequest {
    /// The model name requested by the agent. Responses carry the
    /// instance's own model name (see `storage::ModelName`), whatever
    /// was requested.
    pub model: Option<String>,
    pub messages: Vec<ChatMessage>,
    /// If true, response is SSE stream; otherwise, blocking JSON.
    #[serde(default)]
    pub stream: bool,
    /// Honored as character limit on human reply (default: no limit).
    pub max_tokens: Option<u32>,
    /// Newer name for `max_tokens`; only read to recognize auxiliary calls.
    #[serde(default)]
    pub max_completion_tokens: Option<u32>,
    /// Ignored (human is not stochastic).
    pub temperature: Option<f64>,
    /// Ignored.
    pub top_p: Option<f64>,
    /// Ignored.
    pub n: Option<u32>,
    /// Tools the agent exposes — **primary source for capability discovery**.
    /// Format: `[{ "type": "function", "function": { "name", "description", "parameters" } }]`
    pub tools: Option<Value>,
    /// Passed through. Controls tool calling behavior (`"auto"`, `"none"`, etc.).
    /// In practice, no observed agent sends this field.
    pub tool_choice: Option<Value>,
    /// Passed through.
    pub response_format: Option<Value>,
    /// Streaming options. Sent by Claude Code and WorkBuddy.
    /// Format: `{ "include_usage": true }`
    pub stream_options: Option<Value>,
    /// Reasoning effort hint. Sent by WorkBuddy (non-standard field).
    pub reasoning_effort: Option<String>,
    /// Top-level metadata. Not sent by any observed agent, but accepted.
    pub metadata: Option<Value>,
    /// OpenAI Responses-API style conversation handle.
    ///
    /// Accepts both `"conv_123"` and `{"id": "conv_123"}`. When present it
    /// is used to scope the Omni2API conversation, so switching chat
    /// windows produces genuinely separate threads instead of appending to
    /// one shared transcript.
    #[serde(default)]
    pub conversation: Option<ConversationRef>,
    /// End-user identifier. Standard OpenAI field (deprecated upstream but
    /// still widely sent). Reused here as a conversation-scoping hint when
    /// no explicit conversation handle is supplied.
    #[serde(default)]
    pub user: Option<String>,
}

/// A conversation handle — either a bare id string or `{ "id": ... }`.
///
/// OpenAI's Responses API accepts both spellings; both are tolerated here
/// so agents ported from either API surface keep working.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ConversationRef {
    /// `{ "id": "conv_..." }`
    Object { id: String },
    /// `"conv_..."`
    Id(String),
}

impl ConversationRef {
    /// The underlying id, regardless of spelling.
    pub fn id(&self) -> &str {
        match self {
            Self::Object { id } => id,
            Self::Id(id) => id,
        }
    }
}

/// A single message in the OpenAI `messages[]` array.
///
/// Phase 7.1.1: Supports `role: "tool"` messages with `tool_call_id`,
/// and `role: "assistant"` messages with `tool_calls`.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatMessage {
    pub role: ChatRole,
    /// Content can be a string, an array of parts, or null (when
    /// the message has `tool_calls` instead of content).
    #[serde(default)]
    pub content: ChatContent,
    /// Present when `role == "tool"`. Correlates to a `tool_call_id`
    /// from a prior assistant message's `tool_calls`.
    #[serde(default)]
    pub tool_call_id: Option<String>,
    /// Present when `role == "assistant"` and the assistant is
    /// requesting tool execution. Format:
    /// `[{"id": "call_xxx", "type": "function", "function": {"name": "...", "arguments": "..."}}]`
    #[serde(default)]
    pub tool_calls: Option<Value>,
}

/// OpenAI message roles.
///
/// **Critical**: In Omni2API, `assistant` ≠ AI.
/// - `user` → the Agent (API caller)
/// - `assistant` → the Human (API responder)
/// - `system` → conversation metadata, not a stored message
/// - `tool` → tool execution result from the Agent
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ChatRole {
    /// `developer` is OpenAI's newer name for the system role (o1 and later);
    /// clients such as DeepSeek Harness send it by default.
    #[serde(alias = "developer")]
    System,
    User,
    Assistant,
    /// Tool execution result — sent by the Agent after it executes
    /// a tool call that the Human (via Omni2API) requested.
    Tool,
}

/// OpenAI message content — can be a string, an array of parts, or null.
///
/// Phase 7.1.1: Added `None` variant for messages that carry `tool_calls`
/// instead of content (content is `null` in that case).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(untagged)]
pub enum ChatContent {
    Text(String),
    Parts(Vec<ContentPart>),
    /// Null content — present when the message has `tool_calls` instead.
    #[default]
    Null,
}

/// A single part in an array-form message content.
///
/// `image_url` carries vision input: either an `http(s)` URL or a
/// `data:image/...;base64,...` inline payload. It used to be ignored
/// entirely, so the human was asked about an image they could not see and
/// could only answer "I don't see anything".
#[derive(Debug, Clone, Deserialize)]
pub struct ContentPart {
    #[serde(rename = "type")]
    pub part_type: String,
    pub text: Option<String>,
    pub image_url: Option<ImageUrl>,
    /// OpenAI `file` part: `{filename, file_data (data URL), file_id}`.
    #[serde(default)]
    pub file: Option<serde_json::Value>,
}

/// A data URL for an attached file, its name carried as a parameter
/// (`data:application/pdf;name=report.pdf;base64,...`, RFC 2397) so the
/// file rides every path images already take and keeps its name.
pub fn named_data_url(mime: &str, name: Option<&str>, b64: &str) -> String {
    let mime = if mime.trim().is_empty() {
        "application/octet-stream"
    } else {
        mime.trim()
    };
    match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) => format!("data:{mime};name={};base64,{b64}", encode_param(n)),
        None => format!("data:{mime};base64,{b64}"),
    }
}

fn encode_param(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~ ".contains(&b) {
            o.push(if b == b' ' { '+' } else { b as char });
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
    }
    o
}

/// The `name=` parameter of a data URL, decoded.
pub fn data_url_name(url: &str) -> Option<String> {
    let meta = url.strip_prefix("data:")?.split(',').next()?;
    let raw = meta.split(';').find_map(|p| p.strip_prefix("name="))?;
    let mut out = Vec::new();
    let b = raw.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let h = std::str::from_utf8(&b[i + 1..i + 3]).ok();
                match h.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(v) => {
                        out.push(v);
                        i += 3;
                        continue;
                    }
                    None => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8(out).ok().filter(|n| !n.is_empty())
}

/// OpenAI `file` part / Responses `input_file` as a URL: a named data URL,
/// a link, or (an id this server cannot fetch) None.
pub fn file_part_url(f: &serde_json::Value) -> Option<String> {
    let name = f.get("filename").and_then(|v| v.as_str());
    if let Some(d) = f.get("file_data").and_then(|v| v.as_str()) {
        let d = d.trim();
        if let Some(rest) = d.strip_prefix("data:") {
            let (meta, b64) = rest.split_once(',')?;
            let mime = meta.split(';').next().unwrap_or("");
            let mime = if mime.is_empty() {
                guess_mime(name.unwrap_or(""))
            } else {
                mime
            };
            return Some(named_data_url(mime, name, b64));
        }
        // Bare base64 (some SDKs send it without the data: prefix).
        return Some(named_data_url(guess_mime(name.unwrap_or("")), name, d));
    }
    if let Some(u) = f
        .get("file_id")
        .and_then(|v| v.as_str())
        .and_then(crate::api::files::data_url)
    {
        return Some(u);
    }
    f.get("file_url")
        .and_then(|v| v.as_str())
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty())
}

pub fn b64_encode(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut o = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        o.push(T[(n >> 18) as usize & 63] as char);
        o.push(T[(n >> 12) as usize & 63] as char);
        o.push(if c.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        o.push(if c.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    o
}

/// A mime type from a file name, for parts that send none.
pub fn guess_mime(name: &str) -> &'static str {
    let l = name.to_lowercase();
    let ext = l.rsplit('.').next().unwrap_or("");
    match ext {
        "pdf" => "application/pdf",
        "txt" | "log" => "text/plain",
        "md" => "text/markdown",
        "csv" => "text/csv",
        "json" => "application/json",
        "html" | "htm" => "text/html",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "zip" => "application/zip",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        _ => "application/octet-stream",
    }
}

/// The `image_url` object inside an image content part.
#[derive(Debug, Clone, Deserialize)]
pub struct ImageUrl {
    pub url: String,
    /// OpenAI allows `"auto" | "low" | "high"`; unused by Omni2API.
    pub detail: Option<String>,
}

impl ChatContent {
    /// Flatten any content form to a single text string.
    ///
    /// Images become a `[图片]` placeholder: dropping them silently left the
    /// human with an empty-looking message and no way to answer.
    /// Returns empty string for the `Null` variant.
    pub fn to_text(&self) -> String {
        match self {
            ChatContent::Text(s) => s.clone(),
            ChatContent::Parts(parts) => parts
                .iter()
                .filter_map(|p| match p.part_type.as_str() {
                    "text" => p.text.clone(),
                    "image_url" => Some("[图片]".to_string()),
                    "file" => {
                        let f = p.file.as_ref();
                        let n = f
                            .and_then(|f| f.get("filename"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let sep = if n.is_empty() { "" } else { " " };
                        Some(if f.and_then(file_part_url).is_some() {
                            format!("[文件{sep}{n}]")
                        } else {
                            format!("[文件{sep}{n}: only a file_id was sent, its content is not available here]")
                        })
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
            ChatContent::Null => String::new(),
        }
    }

    /// Image URLs carried by this content, in order.
    ///
    /// Empty unless the message used the array form with `image_url` parts.
    pub fn image_urls(&self) -> Vec<String> {
        match self {
            ChatContent::Parts(parts) => parts
                .iter()
                .filter_map(|p| match p.part_type.as_str() {
                    "image_url" => p
                        .image_url
                        .as_ref()
                        .map(|i| i.url.trim().to_string())
                        .filter(|u| !u.is_empty()),
                    // Files ride with images as named data URLs.
                    "file" => p.file.as_ref().and_then(file_part_url),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    /// True when the content carries at least one image.
    pub fn has_image(&self) -> bool {
        !self.image_urls().is_empty()
    }
}

// ── Outbound: OpenAI Response (non-streaming) ──

/// Non-streaming chat completion response — `chat.completion` object.
///
/// Phase 7.1.1: `ResponseMessage` now supports `tool_calls` and nullable
/// `content`, matching the OpenAI spec for tool-call responses.
#[derive(Debug, Clone, Serialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub object: &'static str,
    pub created: i64,
    pub model: String,
    pub choices: Vec<CompletionChoice>,
    pub usage: Usage,
}

impl ChatCompletionResponse {
    /// Build a response from a human text reply.
    pub fn from_human_reply(reply_text: &str, model: &str) -> Self {
        Self {
            id: format!("chatcmpl-{}", uuid::Uuid::now_v7().simple()),
            object: "chat.completion",
            created: Utc::now().timestamp(),
            model: model.to_string(),
            choices: vec![CompletionChoice {
                index: 0,
                message: ResponseMessage::text_reply(reply_text),
                finish_reason: "stop",
            }],
            usage: Usage::default(),
        }
    }

    /// Build a response that requests the agent to execute a tool.
    ///
    /// This constructs a standard OpenAI `tool_calls` response. The agent
    /// will parse `tool_calls`, execute the tool, and send back a
    /// `role: "tool"` message in the next request.
    ///
    /// **Phase 7.3**: Now wired into the request flow via the WS
    /// `reply.send` handler — if the human sends a `tool_call` payload,
    /// it is delivered as a standard OpenAI `tool_calls` response.
    pub fn from_tool_call(tool_name: &str, arguments: &Value, model: &str) -> Self {
        Self::from_tool_call_with_id(None, tool_name, arguments, None, model)
    }

    /// Build a response carrying several tool calls at once.
    ///
    /// A human may tap more than one tool before the turn closes; OpenAI
    /// models one assistant message with an array of calls, so merging them
    /// is what lets a standard client run them in parallel instead of
    /// forcing a round trip per tool.
    pub fn from_tool_calls(calls: Vec<ToolCallSpec>, content: Option<String>, model: &str) -> Self {
        Self {
            id: format!("chatcmpl-{}", uuid::Uuid::now_v7().simple()),
            object: "chat.completion",
            created: Utc::now().timestamp(),
            model: model.to_string(),
            choices: vec![CompletionChoice {
                index: 0,
                message: ResponseMessage::tool_calls(calls, content),
                finish_reason: "tool_calls",
            }],
            usage: Usage::default(),
        }
    }

    /// Build a tool-call response reusing a caller-supplied call id.
    ///
    /// Used when the call originated from a WS `tool.invoke` so that the
    /// id round-trips back in the agent's `role: "tool"` message and can be
    /// matched to the pending invocation. Falls back to `call_{uuid}`
    /// when `call_id` is absent or blank.
    ///
    /// `content` carries whatever the human said alongside the tool call.
    /// The OpenAI spec permits `content` and `tool_calls` together, and
    /// forcing `content: null` here is what made companion prose vanish.
    pub fn from_tool_call_with_id(
        call_id: Option<String>,
        tool_name: &str,
        arguments: &Value,
        content: Option<String>,
        model: &str,
    ) -> Self {
        Self {
            id: format!("chatcmpl-{}", uuid::Uuid::now_v7().simple()),
            object: "chat.completion",
            created: Utc::now().timestamp(),
            model: model.to_string(),
            choices: vec![CompletionChoice {
                index: 0,
                message: ResponseMessage::tool_call_with_id(call_id, tool_name, arguments, content),
                finish_reason: "tool_calls",
            }],
            usage: Usage::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CompletionChoice {
    pub index: u32,
    pub message: ResponseMessage,
    pub finish_reason: &'static str,
}

/// The `message` object in a chat completion response.
///
/// When the human sends a text reply, `content` is set and `tool_calls`
/// is `None`. When the human triggers a tool call, `content` is `None`
/// and `tool_calls` is set.
#[derive(Debug, Clone, Serialize)]
pub struct ResponseMessage {
    pub role: &'static str,
    /// `None` when `tool_calls` is present (OpenAI spec: content is null).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Present when the response requests tool execution.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallResponse>>,
    /// The responder's reasoning, when it shared any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

impl ResponseMessage {
    /// Create a normal text reply from the human.
    pub fn text_reply(text: &str) -> Self {
        Self {
            role: "assistant",
            content: Some(text.to_string()),
            tool_calls: None,
            reasoning_content: None,
        }
    }

    /// Create a tool-call request message.
    pub fn tool_call(tool_name: &str, arguments: &Value) -> Self {
        Self::tool_call_with_id(None, tool_name, arguments, None)
    }

    /// Create a tool-call request message with an explicit call id.
    ///
    /// `call_id` preserves the originating WS `tool.invoke` invocation id so
    /// the agent's follow-up `role: "tool"` message can be correlated.
    pub fn tool_call_with_id(
        call_id: Option<String>,
        tool_name: &str,
        arguments: &Value,
        content: Option<String>,
    ) -> Self {
        Self::tool_calls(
            vec![ToolCallSpec {
                id: normalize_call_id(call_id),
                name: tool_name.to_string(),
                arguments: arguments.clone(),
            }],
            content,
        )
    }

    /// Build a message carrying several tool calls plus optional prose.
    pub fn tool_calls(calls: Vec<ToolCallSpec>, content: Option<String>) -> Self {
        Self {
            role: "assistant",
            content,
            tool_calls: Some(
                calls
                    .into_iter()
                    .map(|c| ToolCallResponse {
                        id: normalize_call_id(Some(c.id)),
                        type_: "function",
                        function: FunctionCallResponse {
                            name: c.name,
                            arguments: c.arguments.to_string(),
                        },
                    })
                    .collect(),
            ),
            reasoning_content: None,
        }
    }
}

/// One tool call to place in the response's `tool_calls` array.
#[derive(Debug, Clone)]
pub struct ToolCallSpec {
    /// OpenAI-visible call id; round-trips the human-side invocation id.
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// Pick the OpenAI call id for a tool call.
///
/// Reuses a non-empty `call_id` verbatim (it is the human-side invocation
/// id and must round-trip unchanged), otherwise generates `call_{uuid}`.
fn normalize_call_id(call_id: Option<String>) -> String {
    match call_id {
        Some(id) if !id.trim().is_empty() => id,
        _ => format!("call_{}", uuid::Uuid::now_v7().simple()),
    }
}

/// A single tool call in the response's `tool_calls` array.
#[derive(Debug, Clone, Serialize)]
pub struct ToolCallResponse {
    pub id: String,
    #[serde(rename = "type")]
    pub type_: &'static str,
    pub function: FunctionCallResponse,
}

/// The `function` object inside a tool call.
#[derive(Debug, Clone, Serialize)]
pub struct FunctionCallResponse {
    pub name: String,
    /// Arguments as a JSON string (per OpenAI spec).
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

// ── Outbound: OpenAI SSE Chunk (streaming) ──

/// SSE chunk — `chat.completion.chunk` object.
///
/// `usage` is present only in the final chunk when the client
/// requests `stream_options: { include_usage: true }`.
#[derive(Debug, Clone, Serialize)]
pub struct ChatCompletionChunk {
    pub id: String,
    pub object: &'static str,
    pub created: i64,
    pub model: String,
    pub choices: Vec<ChunkChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

impl ChatCompletionChunk {
    /// First chunk — carries the `role` delta, no content.
    pub fn role_header(id: &str, model: &str) -> Self {
        Self {
            id: id.to_string(),
            object: "chat.completion.chunk",
            created: Utc::now().timestamp(),
            model: model.to_string(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: Delta {
                    reasoning_content: None,
                    role: Some("assistant"),
                    content: None,
                    tool_calls: None,
                },
                finish_reason: None,
            }],
            usage: None,
        }
    }

    /// Content chunk — carries a text fragment.
    pub fn content_delta(id: &str, text: &str, model: &str) -> Self {
        Self {
            id: id.to_string(),
            object: "chat.completion.chunk",
            created: Utc::now().timestamp(),
            model: model.to_string(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: Delta {
                    reasoning_content: None,
                    role: None,
                    content: Some(text.to_string()),
                    tool_calls: None,
                },
                finish_reason: None,
            }],
            usage: None,
        }
    }

    /// Final chunk — carries `finish_reason: stop`, no content.
    pub fn finish(id: &str, model: &str) -> Self {
        Self {
            id: id.to_string(),
            object: "chat.completion.chunk",
            created: Utc::now().timestamp(),
            model: model.to_string(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: Delta {
                    reasoning_content: None,
                    role: None,
                    content: None,
                    tool_calls: None,
                },
                finish_reason: Some("stop"),
            }],
            usage: None,
        }
    }

    /// Heartbeat chunk — an empty delta with no content.
    ///
    /// Sent as a `data:` line during the keepalive phase to keep the
    /// SSE connection alive. OpenAI SDK ignores SSE comment lines
    /// (`: ...`), so we must send actual `data:` lines.
    pub fn heartbeat(id: &str, model: &str) -> Self {
        Self {
            id: id.to_string(),
            object: "chat.completion.chunk",
            created: Utc::now().timestamp(),
            model: model.to_string(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: Delta {
                    reasoning_content: None,
                    role: None,
                    content: None,
                    tool_calls: None,
                },
                finish_reason: None,
            }],
            usage: None,
        }
    }

    /// Reasoning delta — the responder's thinking, streamed before content.
    pub fn reasoning_delta(id: &str, text: &str, model: &str) -> Self {
        Self {
            id: id.to_string(),
            object: "chat.completion.chunk",
            created: Utc::now().timestamp(),
            model: model.to_string(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: Delta {
                    role: None,
                    reasoning_content: Some(text.to_string()),
                    content: None,
                    tool_calls: None,
                },
                finish_reason: None,
            }],
            usage: None,
        }
    }

    /// Usage chunk carrying counted tokens.
    pub fn usage_chunk_with(id: &str, usage: Usage, model: &str) -> Self {
        let mut c = Self::usage_chunk(id, model);
        c.usage = Some(usage);
        c
    }

    /// Usage chunk — sent after the finish chunk when the client
    /// requests `stream_options: { include_usage: true }`.
    ///
    /// Per OpenAI spec: choices is empty, usage is populated.
    pub fn usage_chunk(id: &str, model: &str) -> Self {
        Self {
            id: id.to_string(),
            object: "chat.completion.chunk",
            created: Utc::now().timestamp(),
            model: model.to_string(),
            choices: vec![],
            usage: Some(Usage::default()),
        }
    }

    /// Tool-call chunk — carries `tool_calls` delta with `finish_reason: tool_calls`.
    ///
    /// Sent as a single chunk in streaming mode so the agent receives
    /// the complete tool call without fragmentation.
    pub fn tool_call_delta(
        id: &str,
        call_id: &str,
        tool_name: &str,
        arguments: &str,
        model: &str,
    ) -> Self {
        Self::tool_call_delta_at(id, 0, call_id, tool_name, arguments, model)
    }

    /// One tool call of several in a streamed message. `index` is the
    /// call's position: clients accumulate deltas by index, so giving every
    /// call index 0 makes them fold all calls into the first one (seen with
    /// pi: three parallel calls arrived as one).
    pub fn tool_call_delta_at(
        id: &str,
        index: u32,
        call_id: &str,
        tool_name: &str,
        arguments: &str,
        model: &str,
    ) -> Self {
        Self {
            id: id.to_string(),
            object: "chat.completion.chunk",
            created: Utc::now().timestamp(),
            model: model.to_string(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: Delta {
                    reasoning_content: None,
                    role: None,
                    content: None,
                    tool_calls: Some(vec![ToolCallDelta {
                        index,
                        id: Some(call_id.to_string()),
                        type_: Some("function"),
                        function: Some(FunctionCallDelta {
                            name: Some(tool_name.to_string()),
                            arguments: Some(arguments.to_string()),
                        }),
                    }]),
                },
                finish_reason: None,
            }],
            usage: None,
        }
    }

    /// Finish chunk for tool calls — `finish_reason: tool_calls`.
    pub fn finish_tool_calls(id: &str, model: &str) -> Self {
        Self {
            id: id.to_string(),
            object: "chat.completion.chunk",
            created: Utc::now().timestamp(),
            model: model.to_string(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: Delta {
                    reasoning_content: None,
                    role: None,
                    content: None,
                    tool_calls: None,
                },
                finish_reason: Some("tool_calls"),
            }],
            usage: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ChunkChoice {
    pub index: u32,
    pub delta: Delta,
    pub finish_reason: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Delta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<&'static str>,
    /// Reasoning text ahead of the answer (DeepSeek-style
    /// `reasoning_content`, which DeepSeek Harness, Trae and most OpenAI-
    /// compatible clients render as the thinking panel).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Present in tool-call delta chunks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallDelta>>,
}

/// A tool call in a streaming delta chunk.
#[derive(Debug, Clone, Serialize)]
pub struct ToolCallDelta {
    pub index: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub type_: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<FunctionCallDelta>,
}

/// The `function` object inside a streaming tool call delta.
#[derive(Debug, Clone, Serialize)]
pub struct FunctionCallDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_text_reply_response() {
        let resp = ChatCompletionResponse::from_human_reply("Hello!", "omni2api");
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["choices"][0]["message"]["content"], "Hello!");
        assert_eq!(json["choices"][0]["message"]["role"], "assistant");
        assert!(json["choices"][0]["message"].get("tool_calls").is_none());
    }

    #[test]
    fn test_tool_call_response() {
        let args = serde_json::json!({"command": "ls -la"});
        let resp = ChatCompletionResponse::from_tool_call("Bash", &args, "omni2api");
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["choices"][0]["finish_reason"], "tool_calls");
        assert!(json["choices"][0]["message"]["content"].is_null());
        assert_eq!(
            json["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
            "Bash"
        );
        assert_eq!(
            json["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
            r#"{"command":"ls -la"}"#
        );
    }

    #[test]
    fn test_deserialize_tool_message() {
        let json = r#"{
            "role": "tool",
            "content": "total 42\ndrwxr-xr-x  3 root root 4096 Aug 24 15:00 .",
            "tool_call_id": "call_abc123"
        }"#;
        let msg: ChatMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.role, ChatRole::Tool);
        assert_eq!(msg.tool_call_id, Some("call_abc123".to_string()));
        assert!(msg.tool_calls.is_none());
    }

    #[test]
    fn test_deserialize_assistant_with_tool_calls() {
        let json = r#"{
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": "call_abc123",
                "type": "function",
                "function": {
                    "name": "Bash",
                    "arguments": "{\"command\": \"ls\"}"
                }
            }]
        }"#;
        let msg: ChatMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.role, ChatRole::Assistant);
        assert!(matches!(msg.content, ChatContent::Null));
        assert!(msg.tool_calls.is_some());
    }

    #[test]
    fn test_deserialize_request_with_all_fields() {
        let json = r#"{
            "model": "omni2api",
            "messages": [{"role": "user", "content": "hello"}],
            "stream": true,
            "max_tokens": 32000,
            "temperature": 1,
            "tools": [{"type": "function", "function": {"name": "Bash", "parameters": {}}}],
            "tool_choice": "auto",
            "stream_options": {"include_usage": true},
            "reasoning_effort": "high"
        }"#;
        let req: ChatCompletionRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.model, Some("omni2api".into()));
        assert_eq!(req.tool_choice, Some(serde_json::json!("auto")));
        assert_eq!(
            req.stream_options,
            Some(serde_json::json!({"include_usage": true}))
        );
        assert_eq!(req.reasoning_effort, Some("high".into()));
    }

    #[test]
    fn test_content_null_to_text() {
        assert_eq!(ChatContent::Null.to_text(), "");
    }
}
