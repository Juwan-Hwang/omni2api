//! Process-local session bookkeeping for the OpenAI-compatible ingress.
//!
//! The caller replays its whole transcript on every request, so the
//! transcript itself is the source of truth. What the server still has to
//! remember is small and lives here, in memory:
//!
//! * **Continuity index** — a digest of every assistant reply this server
//!   sent, mapped to the conversation it belongs to. A follow-up request
//!   carries those replies back in its history, which identifies the
//!   conversation even when the client sends no id and rewrites its first
//!   message or system prompt between turns (Trae injects a changing
//!   `<system-reminder>`).
//! * **Pending tool calls** — the calls this server emitted, so a result can
//!   be routed back even when the client rewrote the `tool_call_id` (Trae
//!   replaces our ids with its own), and so a replayed result is delivered
//!   once only.
//! * **Per-conversation context** — the system prompt and tools of the last
//!   request, so a responder reads the context of *this* conversation
//!   rather than whatever another window of the same agent sent last.
//! * **Last client per agent** — auxiliary calls (titles, suggestions) carry
//!   no client fingerprint of their own.
//!
//! A restart forgets all of it; the next request rebuilds what matters.

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Mutex, OnceLock};

const INDEX_CAP: usize = 20_000;
const PENDING_CAP: usize = 64;

#[derive(Clone, Debug)]
pub struct PendingCall {
    pub call_id: String,
    pub tool: String,
    pub args_digest: String,
    pub answered: bool,
}

#[derive(Default)]
struct Inner {
    /// (agent_id, digest) → conversation_id
    index: HashMap<(String, String), String>,
    index_order: VecDeque<(String, String)>,
    /// conversation_id → calls emitted, oldest first
    pending: HashMap<String, VecDeque<PendingCall>>,
    /// conversation_id → digests of tool results already delivered
    seen_results: HashMap<String, HashSet<String>>,
    /// conversation_id → fingerprints of user turns delivered since the
    /// last final reply
    seen_users: HashMap<String, HashSet<String>>,
    /// conversation_id → context snapshot
    context: HashMap<String, Value>,
    /// agent_id → client type of its last main request
    last_client: HashMap<String, String>,
    /// conversation_id → reasoning to stream ahead of the next reply
    reasoning: HashMap<String, String>,
}

fn inner() -> &'static Mutex<Inner> {
    static S: OnceLock<Mutex<Inner>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(Inner::default()))
}

fn sha(s: &str) -> String {
    let d = Sha256::digest(s.as_bytes());
    d.iter().take(12).map(|b| format!("{:02x}", b)).collect()
}

/// Digest of reply text, insensitive to whitespace: clients trim, re-wrap
/// or drop the trailing blank line we append between replies.
pub fn text_digest(text: &str) -> Option<String> {
    let squeezed: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    if squeezed.is_empty() {
        None
    } else {
        Some(format!("t:{}", sha(&squeezed)))
    }
}

/// JSON with object keys sorted recursively, so re-serialised arguments
/// compare equal.
pub fn canonical_json(v: &Value) -> String {
    fn canon(v: &Value) -> Value {
        match v {
            Value::Object(m) => {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                let mut out = serde_json::Map::new();
                for k in keys {
                    out.insert(k.clone(), canon(&m[k]));
                }
                Value::Object(out)
            }
            Value::Array(a) => Value::Array(a.iter().map(canon).collect()),
            other => other.clone(),
        }
    }
    canon(v).to_string()
}

/// Arguments arrive either as a JSON value or as a JSON-encoded string.
pub fn args_digest(args: &Value) -> String {
    let parsed = match args {
        Value::String(s) => serde_json::from_str::<Value>(s).unwrap_or(Value::String(s.clone())),
        other => other.clone(),
    };
    sha(&canonical_json(&parsed))
}

pub fn call_digest(tool: &str, args: &Value) -> String {
    format!("c:{}:{}", tool, args_digest(args))
}

/// Remember that `digest` (a reply or a call) was sent on `conversation_id`.
pub fn remember_reply(agent_id: &str, digest: String, conversation_id: &str) {
    let mut g = inner().lock().unwrap();
    let key = (agent_id.to_string(), digest);
    if g.index
        .insert(key.clone(), conversation_id.to_string())
        .is_none()
    {
        g.index_order.push_back(key);
        while g.index_order.len() > INDEX_CAP {
            if let Some(old) = g.index_order.pop_front() {
                g.index.remove(&old);
            }
        }
    }
}

/// The conversation whose replies this request's history carries, newest
/// assistant message first.
pub fn lookup_continuity(agent_id: &str, digests_newest_first: &[String]) -> Option<String> {
    let g = inner().lock().unwrap();
    digests_newest_first
        .iter()
        .find_map(|d| g.index.get(&(agent_id.to_string(), d.clone())).cloned())
}

pub fn record_call(conversation_id: &str, call_id: &str, tool: &str, args: &Value) {
    let mut g = inner().lock().unwrap();
    let q = g.pending.entry(conversation_id.to_string()).or_default();
    q.push_back(PendingCall {
        call_id: call_id.to_string(),
        tool: tool.to_string(),
        args_digest: args_digest(args),
        answered: false,
    });
    while q.len() > PENDING_CAP {
        q.pop_front();
    }
}

/// Map a `tool_call_id` the client sent back to one this server emitted.
///
/// Our own id passes through. A rewritten id is matched by the call the
/// client says it answers (`tool` + arguments from its echoed assistant
/// message), else by the newest unanswered call.
pub fn resolve_call_id(
    conversation_id: &str,
    client_id: Option<&str>,
    echoed: Option<(&str, &Value)>,
) -> Option<String> {
    let g = inner().lock().unwrap();
    let q = g.pending.get(conversation_id)?;
    if let Some(id) = client_id {
        if q.iter().any(|c| c.call_id == id) {
            return Some(id.to_string());
        }
    }
    if let Some((tool, args)) = echoed {
        let d = args_digest(args);
        if let Some(c) = q
            .iter()
            .rev()
            .find(|c| !c.answered && c.tool == tool && c.args_digest == d)
        {
            return Some(c.call_id.clone());
        }
        if let Some(c) = q.iter().rev().find(|c| !c.answered && c.tool == tool) {
            return Some(c.call_id.clone());
        }
    }
    q.iter()
        .rev()
        .find(|c| !c.answered)
        .map(|c| c.call_id.clone())
}

pub fn tool_name_for(conversation_id: &str, call_id: &str) -> Option<String> {
    let g = inner().lock().unwrap();
    g.pending
        .get(conversation_id)?
        .iter()
        .find(|c| c.call_id == call_id)
        .map(|c| c.tool.clone())
}

/// `true` the first time a result is seen on this conversation. A result is
/// identified by its (resolved) call id, or by its content when the call is
/// unknown, so a client replaying its transcript delivers it once.
pub fn first_result(conversation_id: &str, call_id: Option<&str>, content: &str) -> bool {
    let mut g = inner().lock().unwrap();
    let key = match call_id {
        Some(id) => format!("id:{}", id),
        None => format!("body:{}", sha(content)),
    };
    let fresh = g
        .seen_results
        .entry(conversation_id.to_string())
        .or_default()
        .insert(key);
    if fresh {
        if let (Some(id), Some(q)) = (call_id, g.pending.get_mut(conversation_id)) {
            if let Some(c) = q.iter_mut().find(|c| c.call_id == id) {
                c.answered = true;
            }
        }
    }
    fresh
}

/// `true` the first time a user turn is seen since the conversation's last
/// final reply. A retried request repeats its user turn and is dropped; the
/// same words after the turn was answered are a new session saying them
/// again (two runs of one prompt in one directory resolve to one
/// conversation) and must reach the responder.
pub fn first_user_turn(conversation_id: &str, fingerprint: &str) -> bool {
    inner()
        .lock()
        .unwrap()
        .seen_users
        .entry(conversation_id.to_string())
        .or_default()
        .insert(sha(fingerprint))
}

/// A turn ended with a final reply: later repeats of earlier user turns are
/// new turns.
pub fn turn_answered(conversation_id: &str) {
    inner().lock().unwrap().seen_users.remove(conversation_id);
}

pub fn set_context(conversation_id: &str, ctx: Value) {
    inner()
        .lock()
        .unwrap()
        .context
        .insert(conversation_id.to_string(), ctx);
}

pub fn get_context(conversation_id: &str) -> Option<Value> {
    inner()
        .lock()
        .unwrap()
        .context
        .get(conversation_id)
        .cloned()
}

pub fn set_last_client(agent_id: &str, client: &str) {
    inner()
        .lock()
        .unwrap()
        .last_client
        .insert(agent_id.to_string(), client.to_string());
}

pub fn last_client(agent_id: &str) -> Option<String> {
    inner().lock().unwrap().last_client.get(agent_id).cloned()
}

/// Strip blocks that clients inject into user turns and that change from
/// turn to turn (`<system-reminder>`, environment snapshots, attachment
/// lists), so what is left is what the person typed.
pub fn strip_injections(text: &str) -> String {
    strip_injections_with(text, false)
}

/// One user part of a request with several: it may consist of nothing but
/// harness blocks (Grok sends `<user_info>`/`<rules>` as their own user
/// message before the typed one), so it may strip to nothing. The caller
/// falls back to the raw text when every part does.
pub fn strip_injections_part(text: &str) -> String {
    strip_injections_with(text, true)
}

fn strip_injections_with(text: &str, allow_empty: bool) -> String {
    let mut out = text.to_string();
    for tag in [
        "system-reminder",
        "uploaded_files",
        "user_uploaded_files",
        "environment_details",
        "environment_context",
        "runtime-context",
        "runtime_context",
    ] {
        let open = format!("<{}", tag);
        let close = format!("</{}>", tag);
        while let Some(s) = out.find(&open) {
            // `<context` must not swallow `<contextual…>` prose.
            let after = &out[s + open.len()..];
            if !after.starts_with('>') && !after.starts_with(' ') && !after.starts_with('\n') {
                break;
            }
            match out[s..].find(&close) {
                Some(e) => out.replace_range(s..s + e + close.len(), ""),
                None => break,
            }
        }
    }
    out = strip_tag_blocks(&out, allow_empty);
    // Trae wraps the typed text itself.
    if let (Some(s), Some(e)) = (out.find("<user_input>"), out.find("</user_input>")) {
        if s < e {
            out = out[s + "<user_input>".len()..e].to_string();
        }
    }
    out.trim().to_string()
}

/// Harness blocks an agent wraps around what the person typed: environment
/// facts, skill indexes, language and policy settings (Reasonix:
/// `<workspace>`, `<current-date>`, `<available-skills>`,
/// `<execution-policy preset="…">`; Grok: `<user_info>`, `<rules>`). Only
/// these named blocks go: a tag that carries content the responder must
/// read (`<agent_reply>`, `<document>`) stays. Blocks that wrap the typed
/// text itself (`<user_query>`, `<user_message>`) are unwrapped instead.
fn strip_tag_blocks(text: &str, allow_empty: bool) -> String {
    const DROP: [&str; 14] = [
        "workspace",
        "current-date",
        "session-scratch",
        "available-skills",
        "reasoning-language",
        "response-language",
        "execution-policy",
        "user_info",
        "rules",
        "user_rules",
        "env",
        "environment",
        "current_datetime",
        "available_skills",
    ];
    const UNWRAP: [&str; 3] = ["user_query", "user_message", "user_request"];
    let lines: Vec<&str> = text.lines().collect();
    let mut keep = vec![true; lines.len()];
    let tag_of = |l: &str| -> (String, bool) {
        let l = l.trim_start();
        let tag: String = l
            .strip_prefix('<')
            .map(|r| {
                r.chars()
                    .take_while(|c| {
                        c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-' || *c == '_'
                    })
                    .collect()
            })
            .unwrap_or_default();
        let after = l.get(1 + tag.len()..).unwrap_or("");
        let opens = !tag.is_empty() && (after.starts_with('>') || after.starts_with(' '));
        (tag, opens)
    };
    let mut i = 0;
    while i < lines.len() {
        let (tag, opens) = tag_of(lines[i]);
        if opens && (DROP.contains(&tag.as_str()) || UNWRAP.contains(&tag.as_str())) {
            let close = format!("</{tag}>");
            if let Some(end) = (i..lines.len()).find(|&j| lines[j].trim_end().ends_with(&close)) {
                if DROP.contains(&tag.as_str()) {
                    for k in keep.iter_mut().take(end + 1).skip(i) {
                        *k = false;
                    }
                    i = end + 1;
                    continue;
                }
                // Unwrap: drop only the tag lines, when each stands alone.
                if end > i && lines[i].trim_end().ends_with('>') && lines[end].trim() == close {
                    keep[i] = false;
                    keep[end] = false;
                }
            }
        }
        i += 1;
    }
    let kept: Vec<&str> = lines
        .iter()
        .zip(&keep)
        .filter(|(_, k)| **k)
        .map(|(l, _)| *l)
        .collect();
    let joined = kept.join("\n");
    if joined.trim().is_empty() && !allow_empty {
        text.to_string()
    } else {
        joined
    }
}

/// Reasoning a responder attached to its next reply or tool call.
pub fn set_reasoning(conversation_id: &str, text: &str) {
    if text.trim().is_empty() {
        return;
    }
    let mut g = inner().lock().unwrap();
    let e = g.reasoning.entry(conversation_id.to_string()).or_default();
    if !e.is_empty() {
        e.push_str("\n\n");
    }
    e.push_str(text);
}

pub fn take_reasoning(conversation_id: &str) -> Option<String> {
    inner().lock().unwrap().reasoning.remove(conversation_id)
}

// ── Token counting ──
//
// The server counts tokens itself, whoever answered: a responder's own
// figure could be anything, and nothing is estimated. Only tokenizers that
// actually load can be chosen:
//
//   built in: o200k_base (default), cl100k_base, p50k_base, r50k_base
//   plus every `<name>.json` Hugging Face tokenizer file in
//   `OMNI2API_TOKENIZER_DIR` (DeepSeek, Qwen… download their tokenizer.json)
//
// `GET /v1/tokenizers` lists them. Server default: `OMNI2API_TOKENIZER`
// (startup fails on a name not in the list). Per request:
// `x-omni2api-tokenizer` (400 on a name not in the list).

const BUILTIN_TOKENIZERS: [&str; 4] = ["o200k_base", "cl100k_base", "p50k_base", "r50k_base"];

enum Counter {
    Tiktoken(tiktoken_rs::CoreBPE),
    #[cfg(feature = "hf-tokenizers")]
    Hf(Box<tokenizers::Tokenizer>),
}

impl Counter {
    fn count(&self, text: &str) -> u32 {
        match self {
            Counter::Tiktoken(b) => b.encode_with_special_tokens(text).len() as u32,
            #[cfg(feature = "hf-tokenizers")]
            Counter::Hf(t) => t.encode(text, false).map(|e| e.len() as u32).unwrap_or(0),
        }
    }
}

/// Every tokenizer that loaded, by name. Built once.
fn registry() -> &'static HashMap<String, Counter> {
    static R: OnceLock<HashMap<String, Counter>> = OnceLock::new();
    R.get_or_init(|| {
        let mut m = HashMap::new();
        for name in BUILTIN_TOKENIZERS {
            let bpe = match name {
                "o200k_base" => tiktoken_rs::o200k_base(),
                "cl100k_base" => tiktoken_rs::cl100k_base(),
                "p50k_base" => tiktoken_rs::p50k_base(),
                _ => tiktoken_rs::r50k_base(),
            };
            if let Ok(b) = bpe {
                m.insert(name.to_string(), Counter::Tiktoken(b));
            }
        }
        #[cfg(feature = "hf-tokenizers")]
        if let Ok(dir) = std::env::var("OMNI2API_TOKENIZER_DIR") {
            if let Ok(entries) = std::fs::read_dir(dir.trim()) {
                for e in entries.flatten() {
                    let path = e.path();
                    if path.extension().and_then(|x| x.to_str()) != Some("json") {
                        continue;
                    }
                    let Some(stem) = path.file_stem().and_then(|x| x.to_str()) else {
                        continue;
                    };
                    match tokenizers::Tokenizer::from_file(&path) {
                        Ok(t) => {
                            m.insert(stem.to_string(), Counter::Hf(Box::new(t)));
                        }
                        Err(err) => tracing::warn!("skipping tokenizer {}: {err}", path.display()),
                    }
                }
            }
        }
        m
    })
}

/// Names of the tokenizers that can be chosen, sorted.
pub fn available_tokenizers() -> Vec<String> {
    let mut v: Vec<String> = registry().keys().cloned().collect();
    v.sort();
    v
}

pub fn tokenizer_available(name: &str) -> bool {
    registry().contains_key(name)
}

/// The server's default tokenizer name (validated at startup).
pub fn default_tokenizer() -> String {
    std::env::var("OMNI2API_TOKENIZER")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "o200k_base".to_string())
}

/// Tokens in `text` with the named tokenizer (one from
/// `available_tokenizers`; callers validate the name first).
pub fn count_tokens_with(tokenizer: &str, text: &str) -> u32 {
    if text.is_empty() {
        return 0;
    }
    registry()
        .get(tokenizer)
        .map(|c| c.count(text))
        .unwrap_or(0)
}

/// Tokens in `text` with the server's default tokenizer.
pub fn count_tokens(text: &str) -> u32 {
    count_tokens_with(&default_tokenizer(), text)
}

/// Prompt tokens of a request: every message's text plus the tool schemas,
/// with the per-message framing OpenAI counts.
pub fn count_request_tokens(
    tokenizer: &str,
    req: &crate::openai_compat::ChatCompletionRequest,
) -> u32 {
    let mut n: u32 = 3;
    for m in &req.messages {
        n += 4 + count_tokens_with(tokenizer, &m.content.to_text());
        if let Some(tc) = &m.tool_calls {
            n += count_tokens_with(tokenizer, &tc.to_string());
        }
    }
    if let Some(t) = &req.tools {
        n += count_tokens_with(tokenizer, &t.to_string());
    }
    n
}

// ── API tasks ──

struct TaskInfo {
    api: String,
    files: bool,
    /// What a valid answer looks like (an embeddings task: how many vectors
    /// of what length), checked when the seat replies.
    spec: Value,
}

/// Conversations opened by a non-chat endpoint (an image, a transcript...).
fn tasks() -> &'static Mutex<HashMap<String, TaskInfo>> {
    static T: std::sync::OnceLock<Mutex<HashMap<String, TaskInfo>>> = std::sync::OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `files`: whether the answer is files (a picture) or text (a transcript).
pub fn mark_task(conversation_id: &str, api: &str, files: bool, spec: Value) {
    let mut g = tasks().lock().unwrap();
    // Bounded: a task is answered once and never looked up again.
    if g.len() > 10_000 {
        g.clear();
    }
    g.insert(
        conversation_id.to_string(),
        TaskInfo {
            api: api.to_string(),
            files,
            spec,
        },
    );
}

/// The endpoint a task conversation answers and whether it takes files.
pub fn task_api(conversation_id: &str) -> Option<(String, bool)> {
    tasks()
        .lock()
        .unwrap()
        .get(conversation_id)
        .map(|t| (t.api.clone(), t.files))
}

/// The endpoint and answer spec of a task conversation.
pub fn task_spec(conversation_id: &str) -> Option<(String, Value)> {
    tasks()
        .lock()
        .unwrap()
        .get(conversation_id)
        .map(|t| (t.api.clone(), t.spec.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_user_turn_repeats_after_an_answer() {
        let c = "cnv_user_turn_test";
        assert!(first_user_turn(c, "user:2:hi:"));
        assert!(!first_user_turn(c, "user:2:hi:"));
        turn_answered(c);
        assert!(first_user_turn(c, "user:2:hi:"));
    }

    #[test]
    fn harness_tag_blocks_are_stripped() {
        let t = "<workspace>\nCurrent workspace: \"/w\".\n</workspace>\n\n<current-date>\nToday\n</current-date>\n\nrun pwd\n\n<execution-policy preset=\"balanced\" version=\"3\">\nverify=targeted\n</execution-policy>";
        assert_eq!(strip_injections(t), "run pwd");
        assert_eq!(
            strip_injections("<div>hi</div> typed html"),
            "<div>hi</div> typed html"
        );
        assert_eq!(
            strip_injections("<only-block>\nx\n</only-block>"),
            "<only-block>\nx\n</only-block>"
        );
        let dash = "<user_message>\n<user_query>\nrun pwd\n</user_query>\n</user_message>\n\n<agent_reply>\n/w\n</agent_reply>";
        let out = strip_injections(dash);
        assert!(
            out.contains("run pwd") && out.contains("<agent_reply>\n/w\n</agent_reply>"),
            "{out}"
        );
        assert!(!out.contains("<user_query>"), "{out}");
    }
    use serde_json::json;

    #[test]
    fn whitespace_does_not_change_a_reply_digest() {
        assert_eq!(text_digest("hello world\n\n"), text_digest(" hello  world"));
        assert_eq!(text_digest("   "), None);
    }

    #[test]
    fn argument_order_does_not_change_a_call_digest() {
        let a = json!({"b": 1, "a": {"y": 2, "x": 3}});
        let b = Value::String(r#"{"a":{"x":3,"y":2},"b":1}"#.to_string());
        assert_eq!(args_digest(&a), args_digest(&b));
    }

    #[test]
    fn rewritten_call_id_maps_back_to_ours() {
        let conv = "cnv_test_rewrite";
        record_call(conv, "call_ours", "RunCommand", &json!({"command": "ls"}));
        let got = resolve_call_id(
            conv,
            Some("call_theirs"),
            Some(("RunCommand", &json!(r#"{"command":"ls"}"#))),
        );
        assert_eq!(got.as_deref(), Some("call_ours"));
        assert!(first_result(conv, got.as_deref(), "out"));
        assert!(
            !first_result(conv, got.as_deref(), "out"),
            "replay is delivered once"
        );
    }

    #[test]
    fn injections_are_stripped_from_the_anchor() {
        let t =
            "<system-reminder>terminals: 0</system-reminder>\n<user_input>\n你好\n</user_input>";
        assert_eq!(strip_injections(t), "你好");
        assert_eq!(strip_injections("plain"), "plain");
        assert_eq!(
            strip_injections_part("<user_info>\nOS: linux\n</user_info>"),
            ""
        );
    }

    #[test]
    fn tokens_are_counted() {
        assert!(count_tokens("hello world") >= 2);
        assert_eq!(count_tokens(""), 0);
        assert!(count_tokens_with("cl100k_base", "你好，世界") >= 2);
        assert!(tokenizer_available("o200k_base"));
        assert!(
            !tokenizer_available("claude"),
            "only real tokenizers are offered"
        );
    }

    #[test]
    fn continuity_finds_the_conversation() {
        let d = text_digest("unique reply 123").unwrap();
        remember_reply("agt_x", d.clone(), "cnv_x");
        assert_eq!(
            lookup_continuity("agt_x", std::slice::from_ref(&d)).as_deref(),
            Some("cnv_x")
        );
        assert_eq!(lookup_continuity("agt_other", &[d]), None);
    }
}
