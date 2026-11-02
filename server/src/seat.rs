//! Agent Seat — the human seat, opened to agents over plain HTTP.
//!
//! The human side of Omni2API normally speaks WebSocket: a Flutter client
//! holds a socket open, receives `message.received`, and answers with
//! `reply.send` / `tool.invoke` / `end.turn`. An *agent* sitting in that
//! seat (a remote assistant answering a developer who types into Claude
//! Code or Codex) usually cannot hold a socket: it runs in short bursts,
//! from a shell or an MCP client, and is gone between turns.
//!
//! The seat bridges that gap without a second code path:
//!
//! - On first use it registers a **virtual device** in the same
//!   [`ConnectionRegistry`](crate::ws::registry::ConnectionRegistry) the
//!   WebSocket handler uses, so every event a human device would receive
//!   (messages, tool results, capability updates) lands in a per-seat
//!   queue instead of a socket. Pending backlog is drained into it exactly
//!   as on a WebSocket connect.
//! - `GET /v1/seat/inbox` long-polls that queue with a cursor. Polling with
//!   `after=N` acknowledges everything up to `N`, so a seat that crashes
//!   mid-turn replays what it had not acknowledged instead of losing it.
//! - `POST /v1/seat/send` takes the very same client event JSON a
//!   WebSocket client sends and runs it through the same handler.
//!   `/v1/seat/reply` and `/v1/seat/tool-call` are typed shortcuts.
//! - A seat nobody has polled for [`seat_ttl`] is detached, so the human
//!   correctly reads as offline and the offline/async paths take over.
//!
//! Tool calls proposed from the seat are *proposals*: they travel back to
//! the calling client as OpenAI `tool_calls`, and that client executes
//! them under its own sandbox and permission prompts.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{Mutex, Notify};
use uuid::Uuid;

use crate::auth::AuthPrincipal;
use crate::storage::AppState;

/// Longest a single long-poll may hold. Kept under the ~100 s idle timeout
/// CDNs apply to quiet HTTP responses.
const MAX_WAIT_SECS: u64 = 90;
/// Default long-poll hold when the caller does not say.
const DEFAULT_WAIT_SECS: u64 = 25;
/// Events kept per seat before the oldest are dropped.
const QUEUE_CAP: usize = 1000;
/// A consumer that has not polled for this long no longer holds events back.
const CONSUMER_IDLE: Duration = Duration::from_secs(300);
/// `client_msg_id`s remembered per seat for idempotent sends.
const DEDUP_CAP: usize = 256;
/// Text longer than this is cut in the compact view (full view keeps it).
const COMPACT_TEXT_CHARS: usize = 8000;
/// Tool output is what the responder works from, and its end (the failing
/// test, the exit status) matters as much as its start: keep far more, and
/// keep both ends.
const COMPACT_OUTPUT_CHARS: usize = 60000;

/// Detach a seat after this long without a poll (`OMNI2API_SEAT_TTL_SECS`).
fn seat_ttl() -> Duration {
    let secs = std::env::var("OMNI2API_SEAT_TTL_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v >= 10)
        .unwrap_or(180);
    Duration::from_secs(secs)
}

/// Where a freshly attached seat starts counting.
///
/// A seat is re-created after an idle reclaim or a server restart. If it
/// started from 0 again, a consumer still holding the old (larger) cursor
/// would ask for `after=<old>` and never see a new event. Starting from the
/// current Unix time in milliseconds keeps cursors increasing across
/// re-creations (a seat would need more than 1000 events a second for its
/// whole life to catch up with the clock).
fn cursor_origin() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

struct SeatState {
    queue: VecDeque<(u64, Value)>,
    next_cursor: u64,
    last_poll: Instant,
    seen_ids: VecDeque<String>,
    /// Per-consumer acknowledged cursor and last poll. Events are dropped only
    /// once every consumer polled recently has acknowledged them, so two
    /// pollers on one seat key never steal each other's events.
    consumers: std::collections::HashMap<String, (u64, Instant)>,
}

struct Seat {
    human_id: String,
    device_id: String,
    state: Mutex<SeatState>,
    notify: Notify,
}

impl Seat {
    async fn push(&self, event: Value) {
        let mut st = self.state.lock().await;
        st.next_cursor += 1;
        let c = st.next_cursor;
        st.queue.push_back((c, event));
        while st.queue.len() > QUEUE_CAP {
            st.queue.pop_front();
        }
        drop(st);
        self.notify.notify_waiters();
    }

    /// Acknowledge `after` for `consumer`, drop events every recent consumer
    /// has acknowledged, and return everything after `after`.
    async fn take_after(&self, after: u64, consumer: &str) -> (Vec<(u64, Value)>, u64) {
        let mut st = self.state.lock().await;
        let now = Instant::now();
        st.last_poll = now;
        st.consumers.insert(consumer.to_string(), (after, now));
        st.consumers
            .retain(|_, (_, seen)| now.duration_since(*seen) < CONSUMER_IDLE);
        let floor = st
            .consumers
            .values()
            .map(|(a, _)| *a)
            .min()
            .unwrap_or(after);
        while st.queue.front().map(|(c, _)| *c <= floor).unwrap_or(false) {
            st.queue.pop_front();
        }
        let events: Vec<_> = st
            .queue
            .iter()
            .filter(|(c, _)| *c > after)
            .cloned()
            .collect();
        (events, st.next_cursor)
    }

    /// Record a `client_msg_id`; `false` when it was already seen.
    async fn first_time(&self, id: &str) -> bool {
        let mut st = self.state.lock().await;
        if st.seen_ids.iter().any(|s| s == id) {
            return false;
        }
        st.seen_ids.push_back(id.to_string());
        while st.seen_ids.len() > DEDUP_CAP {
            st.seen_ids.pop_front();
        }
        true
    }
}

/// All attached seats, keyed by API key id (one seat per key).
#[derive(Default)]
struct SeatHub {
    seats: Mutex<HashMap<String, Arc<Seat>>>,
}

/// When each human's agent seat last detached. A seat that dropped off a
/// moment ago (its agent spent a while thinking between polls) will most
/// likely be back, and the caller is an agent too: telling it the "human
/// expert is offline" only writes a status line into its transcript.
fn detached_at() -> &'static std::sync::Mutex<HashMap<String, Instant>> {
    static AT: OnceLock<std::sync::Mutex<HashMap<String, Instant>>> = OnceLock::new();
    AT.get_or_init(Default::default)
}

/// How long after a seat detaches its human still counts as answered by
/// an agent (`OMNI2API_SEAT_GRACE_SECS`, default 30 minutes).
fn seat_grace() -> Duration {
    let secs = std::env::var("OMNI2API_SEAT_GRACE_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(1800);
    Duration::from_secs(secs)
}

/// Events a reaped seat still held that no consumer had read, by key. They
/// were already marked delivered (they reached the seat's device), so
/// without this a message that landed just before the reap was lost.
fn orphans() -> &'static std::sync::Mutex<HashMap<String, Vec<Value>>> {
    static O: OnceLock<std::sync::Mutex<HashMap<String, Vec<Value>>>> = OnceLock::new();
    O.get_or_init(Default::default)
}

impl SeatState {
    /// Queued events past every consumer's acknowledgement.
    fn unread(&self) -> Vec<Value> {
        let seen = self.consumers.values().map(|(a, _)| *a).max().unwrap_or(0);
        self.queue
            .iter()
            .filter(|(c, _)| *c > seen)
            .map(|(_, v)| v.clone())
            .collect()
    }
}

/// Whether this human's seat detached within the grace period.
pub(crate) fn seat_recently_detached(human_id: &str) -> bool {
    detached_at()
        .lock()
        .map(|m| m.get(human_id).is_some_and(|t| t.elapsed() <= seat_grace()))
        .unwrap_or(false)
}

fn hub() -> &'static SeatHub {
    static HUB: OnceLock<SeatHub> = OnceLock::new();
    HUB.get_or_init(SeatHub::default)
}

/// Get the seat for this key, attaching a virtual device on first use.
async fn attach(app: &AppState, p: &AuthPrincipal) -> Arc<Seat> {
    let key = p.api_key.id.clone();
    let mut seats = hub().seats.lock().await;
    if let Some(s) = seats.get(&key) {
        return s.clone();
    }

    let seat = Arc::new(Seat {
        human_id: p.human.id.clone(),
        device_id: format!("seat_{}", Uuid::now_v7().simple()),
        state: Mutex::new(SeatState {
            queue: VecDeque::new(),
            next_cursor: cursor_origin(),
            last_poll: Instant::now(),
            seen_ids: VecDeque::new(),
            consumers: std::collections::HashMap::new(),
        }),
        notify: Notify::new(),
    });
    seats.insert(key.clone(), seat.clone());
    drop(seats);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    app.connections
        .register(&seat.human_id, &seat.device_id, tx.clone())
        .await;

    // Pump: everything the registry fans out to this "device" becomes a
    // queued event.
    let pump_seat = seat.clone();
    tokio::spawn(async move {
        while let Some(raw) = rx.recv().await {
            let v = serde_json::from_str::<Value>(&raw).unwrap_or(Value::String(raw));
            pump_seat.push(v).await;
        }
    });

    // What the previous seat on this key held unread comes first.
    let carried = orphans()
        .lock()
        .ok()
        .and_then(|mut o| o.remove(&key))
        .unwrap_or_default();
    for v in carried {
        seat.push(v).await;
    }

    // Same backlog + presence handling as a WebSocket connect.
    crate::ws::handler::drain_pending_messages(app, &seat.human_id, &tx).await;
    // The "human expert is online" notice is injected into the caller's
    // assistant text. For an agent seat that re-attaches every few minutes
    // it just pollutes transcripts, so it is opt-in here.
    if matches!(
        std::env::var("OMNI2API_SEAT_ANNOUNCE").as_deref(),
        Ok("1") | Ok("true") | Ok("on")
    ) {
        crate::ws::handler::announce_online(app, &seat.human_id).await;
    }

    // Reaper: detach once nobody polls, so the human reads as offline.
    let reap_app = app.clone();
    let reap_seat = seat.clone();
    tokio::spawn(async move {
        let ttl = seat_ttl();
        loop {
            tokio::time::sleep(Duration::from_secs(10)).await;
            let idle = reap_seat.state.lock().await.last_poll.elapsed();
            if idle > ttl {
                hub().seats.lock().await.remove(&key);
                let unread = reap_seat.state.lock().await.unread();
                if !unread.is_empty() {
                    if let Ok(mut o) = orphans().lock() {
                        o.entry(key.clone()).or_default().extend(unread);
                    }
                }
                if let Ok(mut m) = detached_at().lock() {
                    m.insert(reap_seat.human_id.clone(), Instant::now());
                }
                reap_app
                    .connections
                    .unregister(&reap_seat.human_id, &reap_seat.device_id)
                    .await;
                tracing::info!("Seat detached after {:?} idle: key={}", idle, key);
                break;
            }
        }
    });

    tracing::info!(
        "Seat attached: human_id={}, device_id={}",
        seat.human_id,
        seat.device_id
    );
    seat
}

fn err(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({ "error": { "code": code, "message": message } })),
    )
        .into_response()
}

/// Keep the first two thirds and the last third of `max`, with a marker
/// saying how much was cut between them.
fn truncate_middle(s: &str, max: usize) -> (String, bool) {
    let n = s.chars().count();
    if n <= max {
        return (s.to_string(), false);
    }
    let head: String = s.chars().take(max * 2 / 3).collect();
    let tail: String = s.chars().skip(n - max / 3).collect();
    let cut = n - head.chars().count() - tail.chars().count();
    (
        format!("{head}\n[... {cut} of {n} chars cut by the server ...]\n{tail}"),
        true,
    )
}

fn truncate(s: &str, max: usize) -> (String, bool) {
    if s.chars().count() <= max {
        (s.to_string(), false)
    } else {
        (s.chars().take(max).collect(), true)
    }
}

/// Flatten one queued event for an agent reader.
///
/// Compact keeps what a turn needs (who, what, which conversation) and
/// swaps a capability flood for the list of tool names; `full` returns the
/// event exactly as a WebSocket client would have seen it.
fn render(cursor: u64, ev: &Value, full: bool) -> Value {
    if full {
        return json!({ "cursor": cursor, "event": ev });
    }
    let ty = ev.get("event_type").and_then(|v| v.as_str()).unwrap_or("");
    let conv = ev.get("conversation_id").cloned().unwrap_or(Value::Null);
    let data = ev.get("data").cloned().unwrap_or(Value::Null);
    let mut out = json!({ "cursor": cursor, "type": ty, "conversation_id": conv });
    match ty {
        "message.received" => {
            let text = data.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let (t, cut) = truncate_middle(text, COMPACT_TEXT_CHARS);
            out["text"] = Value::String(t);
            if cut {
                out["truncated"] = Value::Bool(true);
            }
            out["message_id"] = data.get("message_id").cloned().unwrap_or(Value::Null);
            out["from"] = data.get("sender_name").cloned().unwrap_or(Value::Null);
            if let Some(imgs) = data.get("images").and_then(|v| v.as_array()) {
                out["image_count"] = json!(imgs.len());
            }
            // Request shape: which client, and whether this is an auxiliary
            // call (a title, a suggestion) whose output contract lives in its
            // system prompt — compact shows that contract so the seat can
            // answer in the format the caller will parse.
            if let Some(req) = data.get("request") {
                out["client"] = req.get("client").cloned().unwrap_or(Value::Null);
                let kind = req.get("kind").cloned().unwrap_or(Value::Null);
                if matches!(kind.as_str(), Some("auxiliary") | Some("api")) {
                    if let Some(api) = req.get("api").filter(|v| !v.is_null()) {
                        out["api"] = api.clone();
                    }
                    for k in [
                        "expected_format",
                        "system_prompt_head",
                        "response_format",
                        "max_tokens",
                    ] {
                        if let Some(v) = req.get(k).filter(|v| !v.is_null()) {
                            out[k] = v.clone();
                        }
                    }
                }
                out["kind"] = kind;
            }
            // How many raw `user` parts the tail had: more than one means
            // the agent appended injections the cleaned text leaves out
            // (full view carries them in `raw_tail`).
            if let Some(tail) = data.get("raw_tail").and_then(|v| v.as_array()) {
                let users = tail
                    .iter()
                    .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
                    .count();
                if users > 1 {
                    out["raw_user_parts"] = json!(users);
                }
                // Codex puts the caller's machine (cwd, shell, date) in an
                // injected user block, not the system prompt; the cleaned
                // text drops it, so lift those few facts out.
                let env = injected_env(tail);
                if !env.is_empty() {
                    out["env"] = Value::Object(env);
                }
            }
        }
        "capability.updated" => {
            let names: Vec<Value> = data
                .get("capabilities")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|c| c.get("id").cloned()).collect())
                .unwrap_or_default();
            out["agent_id"] = data.get("agent_id").cloned().unwrap_or(Value::Null);
            out["tools"] = Value::Array(names);
        }
        "tool.result" => {
            for k in ["invocation_id", "tool_id", "status", "error"] {
                out[k] = data.get(k).cloned().unwrap_or(Value::Null);
            }
            let output = data.get("output").cloned().unwrap_or(Value::Null);
            let s = match &output {
                Value::String(s) => s.clone(),
                Value::Null => String::new(),
                other => other.to_string(),
            };
            let (t, cut) = truncate_middle(&s, COMPACT_OUTPUT_CHARS);
            out["output"] = Value::String(t);
            if cut {
                out["truncated"] = Value::Bool(true);
            }
            if let Some(imgs) = data
                .get("images")
                .filter(|v| v.as_array().is_some_and(|a| !a.is_empty()))
            {
                out["images"] = imgs.clone();
            }
        }
        _ => {
            out["data"] = data;
        }
    }
    out
}

// ── GET /v1/seat/inbox ──

#[derive(Debug, Deserialize)]
pub struct InboxQuery {
    #[serde(default)]
    after: u64,
    wait: Option<u64>,
    #[serde(default)]
    view: Option<String>,
    /// Only return events of these types (comma separated).
    #[serde(default)]
    types: Option<String>,
    /// Name of this poller. Each consumer keeps its own cursor; events are
    /// dropped only once every recently active consumer has seen them.
    #[serde(default)]
    consumer: Option<String>,
}

/// `<cwd>`, `<shell>`, `<current_date>`, `<timezone>` from the raw user
/// parts of a request tail (Codex's `<environment_context>`).
fn injected_env(tail: &[Value]) -> serde_json::Map<String, Value> {
    let mut env = serde_json::Map::new();
    for m in tail {
        if m.get("role").and_then(|r| r.as_str()) != Some("user") {
            continue;
        }
        let text = m.get("content").and_then(|c| c.as_str()).unwrap_or("");
        // Plain "Key: value" lines (Grok's <user_info>, Cline, ...).
        for line in text.lines() {
            let l = line.trim();
            let low = l.to_ascii_lowercase();
            for (pre, key) in [
                ("workspace path:", "cwd"),
                ("current working directory:", "cwd"),
                ("working directory:", "cwd"),
                ("os version:", "platform"),
                ("platform:", "platform"),
                ("shell:", "shell"),
                ("today's date:", "date"),
            ] {
                if let Some(v) = low.strip_prefix(pre).map(|_| l[pre.len()..].trim()) {
                    if !v.is_empty() && v.len() < 300 && !env.contains_key(key) {
                        env.insert(key.to_string(), Value::String(v.to_string()));
                    }
                }
            }
        }
        // Reasonix: `Current workspace: "/path".`
        if let Some(a) = text.find("Current workspace: \"") {
            let rest = &text[a + "Current workspace: \"".len()..];
            if let Some(b) = rest.find('"') {
                if !env.contains_key("cwd") && b < 300 {
                    env.insert("cwd".to_string(), Value::String(rest[..b].to_string()));
                }
            }
        }
        for (tag, key) in [
            ("cwd", "cwd"),
            ("shell", "shell"),
            ("current_date", "date"),
            ("timezone", "timezone"),
        ] {
            let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
            if let Some(a) = text.find(&open) {
                if let Some(b) = text[a + open.len()..].find(&close) {
                    let v = text[a + open.len()..a + open.len() + b].trim();
                    if !v.is_empty() && v.len() < 300 && !env.contains_key(key) {
                        env.insert(key.to_string(), Value::String(v.to_string()));
                    }
                }
            }
        }
    }
    env
}

/// Long-poll the seat's queue. Returns as soon as anything newer than
/// `after` exists, or empty when `wait` runs out.
pub async fn inbox(
    State(app): State<AppState>,
    principal: AuthPrincipal,
    Query(q): Query<InboxQuery>,
) -> Response {
    let seat = attach(&app, &principal).await;
    let wait = Duration::from_secs(q.wait.unwrap_or(DEFAULT_WAIT_SECS).min(MAX_WAIT_SECS));
    let deadline = Instant::now() + wait;
    let full = q.view.as_deref() == Some("full");
    let filter: Option<Vec<String>> = q
        .types
        .as_ref()
        .map(|t| t.split(',').map(|s| s.trim().to_string()).collect());

    loop {
        let notified = seat.notify.notified();
        let (events, latest) = seat
            .take_after(q.after, q.consumer.as_deref().unwrap_or(""))
            .await;
        let events: Vec<_> = events
            .into_iter()
            .filter(|(_, ev)| match &filter {
                None => true,
                Some(f) => ev
                    .get("event_type")
                    .and_then(|v| v.as_str())
                    .map(|t| f.iter().any(|x| x == t))
                    .unwrap_or(false),
            })
            .collect();
        let now = Instant::now();
        if !events.is_empty() || now >= deadline {
            let rendered: Vec<Value> = events.iter().map(|(c, e)| render(*c, e, full)).collect();
            let cursor = events.last().map(|(c, _)| *c).unwrap_or(q.after);
            // Only this seat's own open turns: never leak other humans'
            // conversation ids.
            let mut waiting = Vec::new();
            for id in app.reply.conversations_with_waiters().await {
                let mine = app
                    .conversation
                    .get(&id)
                    .await
                    .and_then(|c| c.human_id().map(|h| h == principal.human.id))
                    .unwrap_or(false);
                if mine {
                    waiting.push(id);
                }
            }
            return Json(json!({
                "cursor": cursor,
                "latest": latest,
                "events": rendered,
                "waiting_turns": waiting,
            }))
            .into_response();
        }
        let _ = tokio::time::timeout(deadline - now, notified).await;
    }
}

// ── GET /v1/seat/history ──

#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    conversation_id: String,
    #[serde(default)]
    limit: Option<usize>,
    /// Include each image's data (a `data:` URL or a link), so a seat can
    /// save and look at what the caller attached.
    #[serde(default)]
    images: Option<bool>,
}

/// Recent messages of one of this seat's conversations, oldest first:
/// what the caller said, what was replied, which tools ran. The seat only
/// sees the newest message per event; this is how it reads back.
pub async fn history(
    State(app): State<AppState>,
    principal: AuthPrincipal,
    Query(q): Query<HistoryQuery>,
) -> Response {
    let owned = app
        .conversation
        .get(&q.conversation_id)
        .await
        .and_then(|c| c.human_id().map(|h| h == principal.human.id))
        .unwrap_or(false);
    if !owned {
        return err(
            StatusCode::NOT_FOUND,
            "not_found",
            "No such conversation for this seat.",
        );
    }
    let limit = q.limit.unwrap_or(30).clamp(1, 200);
    let all = app.message.list(&q.conversation_id).await;
    let skip = all.len().saturating_sub(limit);
    let items: Vec<Value> = all
        .iter()
        .skip(skip)
        .map(|m| {
            let text = m.content.as_text().unwrap_or("");
            let (t, cut) = truncate_middle(text, 4000);
            // Sides, not species: the caller is whoever sent the request,
            // the responder whoever answered it (this seat or a person).
            let caller = matches!(
                m.sender.participant_type,
                crate::model::identity::ParticipantType::Agent
            );
            let role = m
                .metadata
                .get("openai_role")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| if caller { "user" } else { "assistant" }.to_string());
            let mut o = json!({
                "message_id": m.id,
                "from": if caller { "caller" } else { "responder" },
                "role": role,
                "text": t,
                "created_at": m.created_at,
                "created_at_local": m.created_at.with_timezone(&chrono::Local).to_rfc3339(),
            });
            if cut {
                o["truncated"] = Value::Bool(true);
            }
            for k in ["tool_call_id", "tool_calls"] {
                if let Some(v) = m.metadata.get(k) {
                    o[k] = v.clone();
                }
            }
            if !m.attachments.is_empty() {
                o["image_count"] = json!(m.attachments.len());
                if q.images == Some(true) {
                    o["images"] = m
                        .attachments
                        .iter()
                        .map(|a| json!({"content_type": a.content_type, "url": a.url}))
                        .collect();
                }
            }
            o
        })
        .collect();
    Json(json!({
        "conversation_id": q.conversation_id,
        "total": all.len(),
        "messages": items,
    }))
    .into_response()
}

// ── POST /v1/seat/model-name ──

#[derive(Debug, Deserialize)]
pub struct ModelNameBody {
    name: String,
}

/// The seat names the model it sits behind. Callers see this name in
/// `/v1/models` and use it as the `model` in their requests. A name the
/// user pinned (`omni2api up --model …` / `OMNI2API_MODEL`) cannot be
/// overridden — that refusal is the answer, not an error to retry.
pub async fn model_name(
    State(app): State<AppState>,
    principal: AuthPrincipal,
    Json(b): Json<ModelNameBody>,
) -> Response {
    if !principal.api_key.scope.may_respond() {
        return err(
            StatusCode::FORBIDDEN,
            "forbidden",
            "Only the seat may name the model.",
        );
    }
    let name = b.name.trim();
    match app.model.set_seat(name) {
        Ok(()) => Json(json!({ "ok": true, "model": app.model.get() })).into_response(),
        Err(crate::storage::SeatNameError::Invalid) => err(
            StatusCode::BAD_REQUEST,
            "invalid_model_name",
            "A model name is 1–64 characters with no whitespace or control characters.",
        ),
        Err(crate::storage::SeatNameError::Locked) => err(
            StatusCode::CONFLICT,
            "model_name_locked",
            "The user pinned this instance's model name; it cannot be changed from the seat.",
        ),
    }
}

/// `GET /v1/seat/model-name` — the current name and whether the user
/// pinned it (a pinned name cannot be changed from the seat).
pub async fn model_name_get(State(app): State<AppState>, principal: AuthPrincipal) -> Response {
    if !principal.api_key.scope.may_respond() {
        return err(
            StatusCode::FORBIDDEN,
            "forbidden",
            "Only the seat may read the model name.",
        );
    }
    Json(json!({
        "model": app.model.get(),
        "locked": app.model.is_locked(),
    }))
    .into_response()
}

// ── POST /v1/seat/save ──

#[derive(Debug, Deserialize)]
pub struct SaveBody {
    conversation_id: String,
}

/// Keep a conversation: write its in-memory messages to the database
/// (messages are not stored by default; see `storage::ephemeral`).
pub async fn save(
    State(app): State<AppState>,
    principal: AuthPrincipal,
    Json(b): Json<SaveBody>,
) -> Response {
    let owned = app
        .conversation
        .get(&b.conversation_id)
        .await
        .and_then(|c| c.human_id().map(|h| h == principal.human.id))
        .unwrap_or(false);
    if !owned {
        return err(
            StatusCode::NOT_FOUND,
            "not_found",
            "No such conversation for this seat.",
        );
    }
    let saved = match &app.ephemeral {
        Some(eph) => eph.save(&b.conversation_id).await,
        None => 0,
    };
    Json(json!({ "ok": true, "saved": saved, "persistent": app.ephemeral.is_none() }))
        .into_response()
}

// ── POST /v1/seat/send ──

/// Send a raw client event, the same JSON a WebSocket client sends
/// (`{"event_type":"reply.send", ...}`). Optional top-level
/// `client_msg_id` makes the send idempotent.
pub async fn send(
    State(app): State<AppState>,
    principal: AuthPrincipal,
    Json(body): Json<Value>,
) -> Response {
    let seat = attach(&app, &principal).await;
    if let Some(id) = body.get("client_msg_id").and_then(|v| v.as_str()) {
        if !seat.first_time(id).await {
            return Json(json!({ "ok": true, "duplicate": true })).into_response();
        }
    }
    // Short form: `{conversation_id, text}` is a reply that leaves the turn
    // open; with no request in flight it is parked and rides the caller's
    // next request.
    let body = if body.get("event_type").is_none() {
        match (
            body.get("conversation_id").and_then(|v| v.as_str()),
            body.get("text").and_then(|v| v.as_str()),
        ) {
            (Some(c), Some(t)) => json!({
                "event_type": "reply.send",
                "conversation_id": c,
                "text": t,
            }),
            _ => {
                return err(
                    StatusCode::BAD_REQUEST,
                    "invalid_event",
                    "Send {conversation_id, text}, or a full client event with event_type.",
                )
            }
        }
    } else {
        body
    };
    let raw = body.to_string();
    if let Err(e) = crate::ws::protocol::ClientMessage::from_json(&raw) {
        return err(StatusCode::BAD_REQUEST, "invalid_event", &e.to_string());
    }
    crate::ws::handler::handle_client_message(&raw, &app, &principal.human.id, &principal.agent.id)
        .await;
    Json(json!({ "ok": true })).into_response()
}

// ── POST /v1/seat/reply ──

#[derive(Debug, Deserialize)]
pub struct ReplyBody {
    conversation_id: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    invocation_id: Option<String>,
    /// Close the turn after this reply (default true: one reply, one turn).
    #[serde(default = "yes")]
    end_turn: bool,
    #[serde(default)]
    client_msg_id: Option<String>,
    /// Thinking to show ahead of the reply (streamed as
    /// `reasoning_content`).
    #[serde(default)]
    reasoning: Option<String>,
    /// Files that answer an API task (the picture an image request asked
    /// for, the audio of a speech request): base64 bytes with a name.
    #[serde(default)]
    attachments: Vec<Attachment>,
}

#[derive(Debug, Deserialize)]
pub struct Attachment {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    mime: Option<String>,
    data: String,
}

fn yes() -> bool {
    true
}

/// Reply with text, and by default end the turn so the caller's request
/// finishes with `finish_reason: stop`.
pub async fn reply(
    State(app): State<AppState>,
    principal: AuthPrincipal,
    Json(b): Json<ReplyBody>,
) -> Response {
    let seat = attach(&app, &principal).await;
    if let Some(id) = &b.client_msg_id {
        if !seat.first_time(id).await {
            return Json(json!({ "ok": true, "duplicate": true })).into_response();
        }
    }
    let mut b = b;
    if !b.attachments.is_empty() {
        if crate::session::task_api(&b.conversation_id).is_none() {
            return err(
                StatusCode::BAD_REQUEST,
                "not_a_task",
                "attachments answer API tasks (images, audio...); a chat conversation takes text",
            );
        }
        let mut ids = Vec::new();
        for (i, a) in b.attachments.iter().enumerate() {
            let data = a.data.trim();
            let data = data
                .strip_prefix("data:")
                .and_then(|r| r.split_once(','))
                .map(|(_, d)| d)
                .unwrap_or(data);
            let Some(bytes) = crate::openai_compat::b64_decode(data).filter(|b| !b.is_empty())
            else {
                return err(
                    StatusCode::BAD_REQUEST,
                    "bad_attachment",
                    &format!("attachment {} is not base64", i + 1),
                );
            };
            let filename = a
                .name
                .clone()
                .filter(|n| !n.trim().is_empty())
                .unwrap_or_else(|| format!("output-{}", i + 1));
            let mime = a
                .mime
                .clone()
                .filter(|m| !m.trim().is_empty())
                .unwrap_or_else(|| {
                    crate::api::files::sniff_mime(&bytes)
                        .unwrap_or(crate::openai_compat::guess_mime(&filename))
                        .to_string()
                });
            ids.push(crate::api::files::put(crate::api::files::StoredFile {
                owner: principal.agent.id.clone(),
                filename,
                mime,
                purpose: crate::api::files::OUTPUT_PURPOSE.to_string(),
                bytes,
                created_at: chrono::Utc::now().timestamp(),
            }));
        }
        let mut text = b.text.trim().to_string();
        for id in ids {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&id);
        }
        b.text = text;
    }
    let had_waiter = app.reply.has_waiter(&b.conversation_id).await;
    if let Some(r) = &b.reasoning {
        crate::session::set_reasoning(&b.conversation_id, r);
    }
    let ev = json!({
        "event_type": "reply.send",
        "conversation_id": b.conversation_id,
        "text": b.text,
        "invocation_id": b.invocation_id,
    });
    crate::ws::handler::handle_client_message(
        &ev.to_string(),
        &app,
        &principal.human.id,
        &principal.agent.id,
    )
    .await;
    if b.end_turn && had_waiter {
        let end = json!({ "event_type": "end.turn", "conversation_id": b.conversation_id });
        crate::ws::handler::handle_client_message(
            &end.to_string(),
            &app,
            &principal.human.id,
            &principal.agent.id,
        )
        .await;
    }
    // A task is one request: once its caller stopped waiting there is no
    // next request to carry the answer.
    let abandoned = !had_waiter && crate::session::task_api(&b.conversation_id).is_some();
    Json(json!({
        "ok": true,
        "live": had_waiter,
        "turn_ended": b.end_turn && had_waiter,
        "abandoned": abandoned,
    }))
    .into_response()
}

// ── POST /v1/seat/tool-call ──

#[derive(Debug, Deserialize)]
pub struct ToolCallBody {
    conversation_id: String,
    tool: String,
    #[serde(default)]
    input: Value,
    /// Words to show alongside the call.
    #[serde(default)]
    text: Option<String>,
    /// Hold up to this many seconds for the `tool.result` (0 = return now).
    #[serde(default)]
    wait: Option<u64>,
    #[serde(default)]
    client_msg_id: Option<String>,
    /// Thinking to show ahead of the call.
    #[serde(default)]
    reasoning: Option<String>,
}

/// Propose a tool call to the calling client. The client executes it under
/// its own sandbox; with `wait` the result is returned inline when it
/// arrives in time, otherwise poll the inbox for `tool.result`.
pub async fn tool_call(
    State(app): State<AppState>,
    principal: AuthPrincipal,
    Json(b): Json<ToolCallBody>,
) -> Response {
    let seat = attach(&app, &principal).await;
    if let Some(id) = &b.client_msg_id {
        if !seat.first_time(id).await {
            return Json(json!({ "ok": true, "duplicate": true })).into_response();
        }
    }
    let conv_tools: Option<Vec<String>> = crate::session::get_context(&b.conversation_id)
        .and_then(|c| c.get("tools").cloned())
        .and_then(|t| t.as_array().cloned())
        .map(|a| {
            a.iter()
                .filter_map(|t| t.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
                .collect()
        })
        .filter(|v: &Vec<String>| !v.is_empty());
    let known = match &conv_tools {
        Some(tools) => tools.iter().any(|t| t == &b.tool),
        None => app
            .capability
            .get_capability(&principal.agent.id, &b.tool)
            .await
            .is_some(),
    };
    if !known {
        let tools: Vec<String> = match conv_tools {
            Some(t) => t,
            None => app
                .capability
                .get_capabilities(&principal.agent.id)
                .await
                .into_iter()
                .map(|c| c.id)
                .collect(),
        };
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": {
                "code": "tool_not_found",
                "message": format!("The caller has not declared a tool named '{}'.", b.tool),
                "available": tools,
            }})),
        )
            .into_response();
    }

    // A tool call can only ride an open turn: the caller must be waiting
    // on this conversation (a live request, or an async invocation). Without
    // one the call used to be parked and spliced into the caller's *next*
    // request, which then answered a question nobody asked.
    if !app.reply.has_waiter(&b.conversation_id).await
        && !app.async_reply.has_pending_for(&b.conversation_id).await
    {
        return err(
            StatusCode::CONFLICT,
            "no_open_turn",
            "The caller is not waiting on this conversation (its turn is closed), so nothing can run the tool now. Wait for the caller's next message, then call the tool in that turn.",
        );
    }

    let invocation_id = format!("call_{}", Uuid::now_v7().simple());
    let input = if b.input.is_null() {
        json!({})
    } else {
        b.input
    };
    if let Some(r) = &b.reasoning {
        crate::session::set_reasoning(&b.conversation_id, r);
    }
    let ev = json!({
        "event_type": "tool.invoke",
        "conversation_id": b.conversation_id,
        "invocation_id": invocation_id,
        "tool_id": b.tool,
        "input": input,
        "text": b.text,
    });
    let start_cursor = seat.state.lock().await.next_cursor;
    crate::ws::handler::handle_client_message(
        &ev.to_string(),
        &app,
        &principal.human.id,
        &principal.agent.id,
    )
    .await;

    let wait = Duration::from_secs(b.wait.unwrap_or(0).min(MAX_WAIT_SECS));
    let deadline = Instant::now() + wait;
    loop {
        let notified = seat.notify.notified();
        {
            let st = seat.state.lock().await;
            let hit = st.queue.iter().find(|(c, e)| {
                *c > start_cursor
                    && e.get("event_type").and_then(|v| v.as_str()) == Some("tool.result")
                    && e.pointer("/data/invocation_id").and_then(|v| v.as_str())
                        == Some(invocation_id.as_str())
            });
            if let Some((c, e)) = hit {
                return Json(json!({
                    "ok": true,
                    "invocation_id": invocation_id,
                    "result": render(*c, e, false),
                }))
                .into_response();
            }
        }
        let now = Instant::now();
        if now >= deadline {
            return Json(json!({
                "ok": true,
                "invocation_id": invocation_id,
                "result": null,
                "hint": "Result not back yet; poll /v1/seat/inbox?types=tool.result",
            }))
            .into_response();
        }
        let _ = tokio::time::timeout(deadline - now, notified).await;
    }
}

// ── GET /v1/seat/capabilities ──

#[derive(Debug, Deserialize)]
pub struct CapQuery {
    #[serde(default)]
    view: Option<String>,
    /// The conversation whose context to read. Each conversation keeps the
    /// system prompt and tools of its own last request; without this the
    /// agent-wide snapshot (whichever window sent last) is returned.
    #[serde(default)]
    conversation_id: Option<String>,
}

/// The tools the calling client declared, and its context.
pub async fn capabilities(
    State(app): State<AppState>,
    principal: AuthPrincipal,
    Query(q): Query<CapQuery>,
) -> Response {
    if let Some(conv) = &q.conversation_id {
        let owned = app
            .conversation
            .get(conv)
            .await
            .and_then(|c| c.human_id().map(|h| h == principal.human.id))
            .unwrap_or(false);
        let snap = if owned {
            crate::session::get_context(conv)
        } else {
            None
        };
        let Some(snap) = snap else {
            return err(
                StatusCode::NOT_FOUND,
                "not_found",
                "No context for this conversation.",
            );
        };
        let full = q.view.as_deref() == Some("full");
        let tools: Vec<Value> = snap
            .get("tools")
            .and_then(|t| t.as_array())
            .map(|a| {
                a.iter()
                    .map(|t| {
                        if full {
                            t.clone()
                        } else {
                            let d = t.get("description").and_then(|v| v.as_str()).unwrap_or("");
                            let (d, _) = truncate(d, 200);
                            let required = t
                                .pointer("/input_schema/required")
                                .or_else(|| t.pointer("/parameters/required"))
                                .cloned()
                                .unwrap_or(Value::Null);
                            json!({ "id": t.get("id"), "description": d, "required": required })
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        let sp = snap
            .get("system_prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        return Json(json!({
            "conversation_id": conv,
            "client": snap.get("client"),
            "tools": tools,
            "system_prompt": if full { Value::String(sp.to_string()) } else { Value::String(truncate(sp, 600).0) },
            "system_prompt_len": sp.chars().count(),
            "context": snap.get("context"),
        }))
        .into_response();
    }
    let caps = app.capability.get_capabilities(&principal.agent.id).await;
    let ctx = app.context.get_context(&principal.agent.id).await;
    let tools: Vec<Value> = if q.view.as_deref() == Some("full") {
        caps.iter().map(|c| json!(c)).collect()
    } else {
        caps.iter()
            .map(|c| {
                let d = c.description.clone().unwrap_or_default();
                let (d, _) = truncate(&d, 200);
                json!({ "id": c.id, "description": d })
            })
            .collect()
    };
    Json(json!({
        "agent_id": principal.agent.id,
        "agent_name": principal.agent.name,
        "tools": tools,
        "context": ctx,
    }))
    .into_response()
}

#[cfg(test)]
mod consumer_tests {
    use super::*;

    #[test]
    fn codex_env_context_is_lifted() {
        let tail = vec![
            json!({"role":"user","content":"<environment_context>\n  <cwd>/srv/p</cwd>\n  <shell>zsh</shell>\n</environment_context>"}),
            json!({"role":"user","content":"run pwd"}),
        ];
        let env = injected_env(&tail);
        assert_eq!(env["cwd"], "/srv/p");
        assert_eq!(env["shell"], "zsh");
        assert!(injected_env(&[json!({"role":"user","content":"hi"})]).is_empty());
        let grok = injected_env(&[
            json!({"role":"user","content":"<user_info>\nOS Version: linux\nShell: /bin/sh\nWorkspace Path: /w\n</user_info>"}),
        ]);
        assert_eq!(grok["cwd"], "/w");
        assert_eq!(grok["platform"], "linux");
    }

    fn seat() -> Seat {
        Seat {
            human_id: "hum_t".into(),
            device_id: "seat_t".into(),
            state: Mutex::new(SeatState {
                queue: VecDeque::new(),
                next_cursor: 0,
                last_poll: Instant::now(),
                seen_ids: VecDeque::new(),
                consumers: std::collections::HashMap::new(),
            }),
            notify: Notify::new(),
        }
    }

    #[tokio::test]
    async fn two_consumers_do_not_steal_events() {
        let s = seat();
        s.take_after(0, "a").await;
        s.take_after(0, "b").await;
        s.push(json!({"event_type": "message.received", "n": 1}))
            .await;
        s.push(json!({"event_type": "message.received", "n": 2}))
            .await;
        // a reads and acknowledges both
        let (ev, _) = s.take_after(0, "a").await;
        assert_eq!(ev.len(), 2);
        let (ev, _) = s.take_after(2, "a").await;
        assert!(ev.is_empty());
        // b still gets both
        let (ev, _) = s.take_after(0, "b").await;
        assert_eq!(ev.len(), 2);
        // once b acknowledges, the queue is empty
        s.take_after(2, "b").await;
        assert!(s.state.lock().await.queue.is_empty());
    }

    #[tokio::test]
    async fn single_legacy_poller_unchanged() {
        let s = seat();
        s.push(json!({"event_type": "x"})).await;
        let (ev, _) = s.take_after(0, "").await;
        assert_eq!(ev.len(), 1);
        let (ev, _) = s.take_after(1, "").await;
        assert!(ev.is_empty());
        assert!(s.state.lock().await.queue.is_empty());
    }
}

#[cfg(test)]
mod cursor_origin_tests {
    #[test]
    fn a_new_seat_starts_above_any_old_small_cursor() {
        // Cursors handed out by a seat created at time T stay below the
        // origin of any seat created later (in practice: ms clock).
        let a = super::cursor_origin();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let b = super::cursor_origin();
        assert!(a > 1_600_000_000_000, "origin is a ms timestamp");
        assert!(b > a);
    }
}

#[test]
fn unread_is_past_every_consumer() {
    let now = Instant::now();
    let mut st = SeatState {
        queue: VecDeque::new(),
        next_cursor: 0,
        last_poll: now,
        seen_ids: VecDeque::new(),
        consumers: HashMap::new(),
    };
    for c in 1..=4 {
        st.queue.push_back((c, json!({ "n": c })));
    }
    st.consumers.insert("a".into(), (2, now));
    st.consumers.insert("b".into(), (3, now));
    assert_eq!(st.unread(), vec![json!({"n": 4})]);
}

#[test]
fn a_seat_that_just_detached_still_counts() {
    assert!(!seat_recently_detached("hum_grace_test"));
    detached_at()
        .lock()
        .unwrap()
        .insert("hum_grace_test".into(), Instant::now());
    assert!(seat_recently_detached("hum_grace_test"));
}

#[test]
fn long_output_keeps_both_ends() {
    let body: String = (1..=20000).map(|i| format!("{i}\n")).collect();
    let (t, cut) = truncate_middle(&body, 6000);
    assert!(cut && t.starts_with("1\n") && t.ends_with("20000\n") && t.contains("chars cut"));
}
