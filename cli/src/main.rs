//! oh-seat — sit in the Omni2API responder seat from any terminal.
//!
//! Built for AI agents that drive a shell one command at a time:
//! one command = one complete step, short structured output, state kept in
//! a file (never in the agent's head), every blocking call bounded by a
//! timeout the agent picks. See docs/seat-cli.md.

use clap::{Args, Parser, Subcommand};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod input;
mod mcp;

// stdout closed early (`| head`): stop quietly instead of panicking.
macro_rules! println {
    ($($t:tt)*) => {{
        use std::io::Write as _;
        if writeln!(std::io::stdout(), $($t)*).is_err() {
            std::process::exit(0)
        }
    }};
}

const POLL_MAX: u64 = 85; // server caps a long-poll at 90 s
const UA: &str = concat!("oh-seat/", env!("CARGO_PKG_VERSION"));

// ── exit codes ──
const OK: u8 = 0;
const TOOL_ERR: u8 = 1; // the tool ran on the caller and reported an error
const NOTHING: u8 = 2; // timed out with nothing new
const HTTP: u8 = 3; // network / auth / server error
const USAGE: u8 = 4; // bad command, unknown tool/parameter, unknown conversation
const NO_TURN: u8 = 5; // the caller is not waiting: no open turn to act in

const GUIDE: &str = include_str!("guide.txt");

#[derive(Parser)]
#[command(
    name = "oh-seat",
    version,
    about = "Omni2API Agent Seat: answer callers (Claude Code, Codex, ...) and run tools on their side",
    after_help = "Start with `oh-seat guide` (one screen, everything you need).\n\nExit codes: 0 ok, 1 tool reported an error, 2 nothing new before --timeout, 3 network/auth/server, 4 usage (unknown tool/parameter/conversation), 5 no open turn."
)]
struct Cli {
    #[command(flatten)]
    g: Global,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Args)]
struct Global {
    /// Server base URL. Default: the local `omni2api up` (~/.omni2api/endpoint.json)
    #[arg(long, env = "OH_SEAT_URL", global = true)]
    url: Option<String>,
    /// Responder API key (prefer OH_SEAT_KEY_FILE so it stays out of your history)
    #[arg(long, env = "OH_SEAT_KEY", hide_env_values = true, global = true)]
    key: Option<String>,
    /// File holding the key (a bare ohk_... or an `api_key=ohk_...` line). Default ~/.omni2api/seat_key
    #[arg(long, env = "OH_SEAT_KEY_FILE", global = true)]
    key_file: Option<PathBuf>,
    /// State file (cursor, active conversation, buffered events). Default ~/.omni2api/seat-state.json ($OMNI2API_HOME/seat-state.json when set)
    #[arg(long, env = "OH_SEAT_STATE", global = true)]
    state: Option<PathBuf>,
    /// One JSON object per line instead of text
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Wait until a caller message needs an answer, then print it (and make it the active conversation)
    Next {
        /// Give up after this many seconds (exit 2). Keep it under your shell's command timeout.
        #[arg(long, default_value_t = 100)]
        timeout: u64,
        /// Return on any event, not only messages
        #[arg(long)]
        all: bool,
    },
    /// Reply in a conversation. TEXT may be `-` (or omitted) to read stdin
    Reply {
        #[arg(allow_hyphen_values = true)]
        text: Option<String>,
        /// Read the reply from a file (no shell quoting problems)
        #[arg(long, short = 'f')]
        file: Option<PathBuf>,
        /// Conversation (any unique piece of its id; default the active one)
        #[arg(long, short = 'c')]
        conv: Option<String>,
        /// Thinking shown before the reply (reasoning_content)
        #[arg(long, short = 'r')]
        reasoning: Option<String>,
        /// Keep the turn open for another reply
        #[arg(long)]
        more: bool,
        /// After replying, wait for the next message (same as running `next`)
        #[arg(long)]
        next: bool,
        /// Timeout for --next
        #[arg(long, default_value_t = 100)]
        timeout: u64,
        /// Reply even though a tool call in this conversation has a result you have not seen
        #[arg(long)]
        force: bool,
    },
    /// Ask the caller to run one of its tools (or several: `A k=v + B k=v`); waits and prints results in order.
    ///
    /// Input: key=value pairs typed by the tool's schema (string params keep the text as is),
    /// key=@path reads a file, key=@- reads stdin (several key=@-: texts split by a `@@@` line), key:=v forces a string, key:json=v forces JSON;
    /// or one JSON object; or `-` for the whole input on stdin. Checked before sending.
    Tool {
        /// Tool name as the caller declared it (case-insensitive)
        name: String,
        /// key=value pairs (see above), `+` between calls of a batch
        input: Vec<String>,
        /// Send even if the input does not match the tool's schema
        #[arg(long)]
        force: bool,
        /// Conversation (any unique piece of its id; default the active one)
        #[arg(long, short = 'c')]
        conv: Option<String>,
        /// Words shown alongside the call
        #[arg(long)]
        text: Option<String>,
        /// Thinking shown before the call (reasoning_content)
        #[arg(long, short = 'r')]
        reasoning: Option<String>,
        /// Wait at most this long for the result; if it is not back, fetch it later with `oh-seat result ID`
        #[arg(long, default_value_t = 100)]
        timeout: u64,
    },
    /// Wait for the result of an earlier tool call (default: the latest one still pending)
    Result {
        /// Invocation id, or any unique piece of it; omitted: the latest pending call
        /// (in the active conversation if it has one)
        invocation_id: Option<String>,
        /// Give up after this many seconds (exit 2)
        #[arg(long, default_value_t = 100)]
        timeout: u64,
    },
    /// The caller's tools and system prompt for a conversation
    Caps {
        #[arg(long, short = 'c')]
        conv: Option<String>,
        /// Print the whole system prompt and every tool schema
        #[arg(long)]
        full: bool,
        /// Only what changed since the last `caps` of this conversation
        #[arg(long)]
        diff: bool,
        /// One tool: signature and what each parameter means
        #[arg(long)]
        tool: Option<String>,
        /// With --tool: the raw JSON schema instead
        #[arg(long)]
        schema: bool,
    },
    /// Recent messages of a conversation
    History {
        #[arg(long, short = 'c')]
        conv: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Conversations this seat has seen, newest first (* = active)
    Convs,
    /// Make a conversation the active one
    Use { conv: String },
    /// Persist a conversation on the server (messages are in memory by default)
    Save {
        #[arg(long, short = 'c')]
        conv: Option<String>,
    },
    /// The model name you answer to: show it, or set your own
    /// (a name the user pinned at `omni2api up --model …` cannot be changed)
    Name {
        /// The name to answer to; omit to show the current one
        name: Option<String>,
    },
    /// Show URL, consumer, cursor, active conversation, open turns
    Status,
    /// Everything you need on one screen: the loop, input rules, exit codes
    Guide,
    /// Serve these commands as an MCP server over stdio (for MCP-only harnesses)
    Mcp,
}

// ── errors ──

#[derive(Debug)]
struct Fail(u8, String);
type R<T> = Result<T, Fail>;
fn usage<T>(m: impl Into<String>) -> R<T> {
    Err(Fail(USAGE, m.into()))
}

// ── state ──

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct State {
    #[serde(default)]
    cursor: u64,
    #[serde(default)]
    consumer: String,
    #[serde(default)]
    active: Option<String>,
    /// Events read from the server but not yet handed to the agent.
    #[serde(default)]
    buffer: Vec<Value>,
    #[serde(default)]
    convs: BTreeMap<String, ConvInfo>,
    /// Last `caps` text per conversation, for --diff.
    #[serde(default)]
    caps: BTreeMap<String, String>,
    /// Tool list (with schemas) per conversation, to type and check input.
    #[serde(default)]
    tools: BTreeMap<String, Vec<Value>>,
    /// Tool results already printed: a replay never shows them again.
    #[serde(default)]
    done: Vec<String>,
    /// Late results already mentioned once by `next`.
    #[serde(default)]
    hinted: Vec<String>,
    /// Tool calls sent whose result has not been seen: [invocation, conv, tool].
    #[serde(default)]
    pending: Vec<[String; 3]>,
    /// Messages and new conversations already taken in: a seat that is
    /// reaped before it acknowledges its last poll gets those events again.
    #[serde(default)]
    seen: Vec<String>,
}

#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
struct ConvInfo {
    #[serde(default)]
    client: Option<String>,
    #[serde(default)]
    last: String,
    #[serde(default)]
    t: u64,
    /// The latest message was an auxiliary call (title, summary...).
    #[serde(default)]
    aux: bool,
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn rand_hex(n: usize) -> String {
    let mut x = (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(1) as u64)
        ^ (std::process::id() as u64).rotate_left(32);
    let mut s = String::new();
    while s.len() < n {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.push_str(&format!("{:016x}", x));
    }
    s.truncate(n);
    s
}

/// The seat often sits inside an agent sandbox whose only way out is an
/// HTTP proxy, while the server it must reach is a public URL (the
/// documented "server on the user's machine, seat in the sandbox" shape).
/// Honor the standard proxy variables like other command-line tools do.
/// Loopback targets — the common same-machine server — and `NO_PROXY`
/// matches are never proxied.
fn env_proxy_for(url: &str) -> Option<ureq::Proxy> {
    let host = url_host(url);
    let no_proxy = std::env::var("NO_PROXY")
        .or_else(|_| std::env::var("no_proxy"))
        .unwrap_or_default();
    if host_is_bypassed(&host, &no_proxy) {
        return None;
    }
    for var in [
        "ALL_PROXY",
        "all_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
    ] {
        if let Ok(v) = std::env::var(var) {
            if let Ok(p) = ureq::Proxy::new(v) {
                return Some(p);
            }
        }
    }
    None
}

/// The host part of a URL, lowercased, without port or brackets.
fn url_host(url: &str) -> String {
    let rest = url.split("://").nth(1).unwrap_or(url);
    let hostport = rest.split('/').next().unwrap_or("");
    let host = if let Some(v6) = hostport.strip_prefix('[') {
        v6.split(']').next().unwrap_or("")
    } else {
        hostport.split(':').next().unwrap_or("")
    };
    host.to_ascii_lowercase()
}

fn host_is_bypassed(host: &str, no_proxy: &str) -> bool {
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        if ip.is_loopback() {
            return true;
        }
    }
    no_proxy
        .split(',')
        .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
        .any(|e| e == "*" || (!e.is_empty() && (host == e || host.ends_with(&format!(".{e}")))))
}

struct Ctx {
    url: String,
    key: String,
    state_path: PathBuf,
    st: State,
    json: bool,
    agent: ureq::Agent,
}

/// `~/.omni2api` (or `$OMNI2API_HOME`).
fn omni2api_home() -> PathBuf {
    std::env::var("OMNI2API_HOME")
        .ok()
        .filter(|h| !h.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".omni2api"))
}

/// URL and responder key written by `omni2api up` (`~/.omni2api/endpoint.json`,
/// or `$OMNI2API_HOME/endpoint.json`).
fn omni2api_endpoint() -> Option<(String, String)> {
    let dir = omni2api_home();
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("endpoint.json")).ok()?).ok()?;
    let url = v["seat"]["OH_SEAT_URL"].as_str()?.to_string();
    let key = v["responder_key"].as_str()?.to_string();
    Some((url, key))
}

/// After a successful rename, keep the local `endpoint.json` honest: it is
/// what `omni2api status` prints and what setup guides hand to the user.
/// Only touches the file when it describes this very endpoint (same seat
/// URL and responder key); best-effort — the server already renamed.
fn sync_endpoint_model(cx: &Ctx, model: &str) {
    let path = omni2api_home().join("endpoint.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return;
    };
    if v["responder_key"].as_str() != Some(cx.key.as_str())
        || v["seat"]["OH_SEAT_URL"].as_str() != Some(cx.url.as_str())
    {
        return;
    }
    v["model"] = serde_json::Value::String(model.to_string());
    if let Ok(pretty) = serde_json::to_string_pretty(&v) {
        let _ = std::fs::write(&path, pretty);
    }
}

impl Ctx {
    fn new(g: Global) -> R<Ctx> {
        let local = omni2api_endpoint();
        let url = g
            .url
            .filter(|u| !u.trim().is_empty())
            .or_else(|| local.as_ref().map(|(u, _)| u.clone()))
            .unwrap_or_else(|| "http://127.0.0.1:8080".to_string());
        let key = match g.key.filter(|k| !k.trim().is_empty()) {
            Some(k) => k.trim().to_string(),
            None if g.key_file.is_none() && local.is_some() => {
                local.as_ref().map(|(_, k)| k.clone()).unwrap_or_default()
            }
            None => {
                let path = g
                    .key_file
                    .unwrap_or_else(|| omni2api_home().join("seat_key"));
                let txt = std::fs::read_to_string(&path).map_err(|e| {
                    Fail(
                        USAGE,
                        format!(
                            "no key: run `omni2api up` first, or set OH_SEAT_KEY / OH_SEAT_KEY_FILE ({}: {e})",
                            path.display()
                        ),
                    )
                })?;
                txt.lines()
                    .map(|l| l.trim().trim_start_matches("api_key=").to_string())
                    .find(|l| l.starts_with("ohk_"))
                    .ok_or_else(|| {
                        Fail(USAGE, format!("no ohk_ key found in {}", path.display()))
                    })?
            }
        };
        let state_path = g
            .state
            .unwrap_or_else(|| omni2api_home().join("seat-state.json"));
        let mut st: State = std::fs::read_to_string(&state_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        if st.consumer.is_empty() {
            st.consumer = std::env::var("OH_SEAT_CONSUMER")
                .unwrap_or_else(|_| format!("oh-seat-{}", rand_hex(8)));
        }
        let mut agent_builder = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(15))
            .user_agent(UA);
        if let Some(proxy) = env_proxy_for(&url) {
            agent_builder = agent_builder.proxy(proxy);
        }
        let agent = agent_builder.build();
        Ok(Ctx {
            url: url.trim().trim_end_matches('/').to_string(),
            key,
            state_path,
            st,
            json: g.json,
            agent,
        })
    }

    fn save(&self) {
        if let Some(dir) = self.state_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = self.state_path.with_extension("tmp");
        if let Ok(s) = serde_json::to_string(&self.st) {
            if std::fs::write(&tmp, s).is_ok() {
                let _ = std::fs::rename(&tmp, &self.state_path);
            }
        }
    }

    /// Save the images a caller attached to a message as files, so a
    /// responder can open them with its own tools. Links stay links.
    fn save_images(&self, c: &str, msg_id: &str) -> Vec<String> {
        let path = format!(
            "/v1/seat/history?conversation_id={}&limit=20&images=true",
            enc(c)
        );
        let Ok(r) = self.call("GET", &path, None, 60) else {
            return Vec::new();
        };
        let Some(m) = r["messages"]
            .as_array()
            .and_then(|a| a.iter().find(|m| s(m, "message_id") == Some(msg_id)))
        else {
            return Vec::new();
        };
        let dir = omni2api_home().join("images");
        let _ = std::fs::create_dir_all(&dir);
        let mut out = Vec::new();
        for (i, img) in m["images"].as_array().into_iter().flatten().enumerate() {
            let url = s(img, "url").unwrap_or("");
            match data_url_bytes(url) {
                Some((mime, bytes)) => {
                    let ext = mime
                        .rsplit('/')
                        .next()
                        .unwrap_or("bin")
                        .replace("jpeg", "jpg");
                    let f = dir.join(format!("{msg_id}-{}.{ext}", i + 1));
                    if std::fs::write(&f, bytes).is_ok() {
                        out.push(f.display().to_string());
                    }
                }
                None if !url.is_empty() => out.push(url.to_string()),
                None => {}
            }
        }
        out
    }

    fn call(&self, method: &str, path: &str, body: Option<Value>, timeout: u64) -> R<Value> {
        let req = self
            .agent
            .request(method, &format!("{}{}", self.url, path))
            .timeout(Duration::from_secs(timeout))
            .set("Authorization", &format!("Bearer {}", self.key));
        let res = match body {
            Some(b) => req.send_json(b),
            None => req.call(),
        };
        let short = path.split('?').next().unwrap_or(path);
        match res {
            Ok(r) => {
                let txt = r.into_string().unwrap_or_default();
                if txt.trim().is_empty() {
                    Ok(json!({}))
                } else {
                    serde_json::from_str(&txt)
                        .map_err(|e| Fail(HTTP, format!("{short}: bad JSON from server: {e}")))
                }
            }
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_default();
                let msg = serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|v| {
                        v.pointer("/error/message")
                            .and_then(|m| m.as_str())
                            .map(String::from)
                    })
                    .unwrap_or_else(|| body.chars().take(400).collect());
                let ecode = serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|v| {
                        v.pointer("/error/code")
                            .and_then(|c| c.as_str())
                            .map(String::from)
                    })
                    .unwrap_or_default();
                let hint = match code {
                    401 => " (key missing, wrong or revoked)",
                    403 => " (this key may not sit in the seat: it needs scope responder or both)",
                    _ => "",
                };
                let exit = match (code, ecode.as_str()) {
                    (409, "no_open_turn") => NO_TURN,
                    (400 | 404 | 409 | 422, _) => USAGE,
                    _ => HTTP,
                };
                Err(Fail(exit, format!("HTTP {code} {short}: {msg}{hint}")))
            }
            Err(e) => Err(Fail(HTTP, format!("{short}: {e}"))),
        }
    }

    /// One long-poll; buffers new events and advances the cursor.
    ///
    /// Returns the server's `waiting_turns`. If the server's `latest` is
    /// below our cursor the seat was re-created (idle reclaim or restart) and
    /// its cursors started over: reset and read again at once, otherwise
    /// every new event would sit below our cursor and never be returned.
    fn poll(&mut self, wait: u64) -> R<Value> {
        let wait = wait.min(POLL_MAX);
        let mut from_zero = self.st.cursor == 0;
        let mut r = self.inbox(self.st.cursor, wait)?;
        let latest = r.get("latest").and_then(|v| v.as_u64());
        if matches!(latest, Some(l) if l < self.st.cursor) {
            self.st.cursor = 0;
            from_zero = true;
            r = self.inbox(0, 0)?;
        }
        // Reading from the start (a fresh state file, or a re-created seat)
        // returns whatever the queue still holds, including messages another
        // seat process already answered. Only those whose turn is still open
        // need an answer; replaying the rest makes an agent answer twice.
        let open: Vec<String> = r
            .get("waiting_turns")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        if let Some(c) = r.get("cursor").and_then(|v| v.as_u64()) {
            self.st.cursor = c;
        }
        for e in r
            .get("events")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
        {
            // Delivery receipts tell the seat nothing it can act on.
            if matches!(s(&e, "type"), Some("reply.delivered")) {
                continue;
            }
            if from_zero
                && s(&e, "type") == Some("message.received")
                && !open
                    .iter()
                    .any(|o| Some(o.as_str()) == s(&e, "conversation_id"))
            {
                continue;
            }
            if s(&e, "type") == Some("tool.result") {
                let inv = s(&e, "invocation_id").unwrap_or("");
                let dup = self.st.done.iter().any(|d| d == inv)
                    || self
                        .st
                        .buffer
                        .iter()
                        .any(|b| s(b, "invocation_id") == Some(inv));
                if dup {
                    continue;
                }
            }
            let key = match s(&e, "type") {
                Some("message.received") => s(&e, "message_id").map(|m| format!("m:{m}")),
                Some("conversation.started") => conv_id(&e).map(|c| format!("c:{c}")),
                _ => None,
            };
            if let Some(k) = key {
                if self.st.seen.contains(&k) {
                    continue;
                }
                self.st.seen.push(k);
            }
            self.note(&e);
            self.st.buffer.push(e);
        }
        if self.st.seen.len() > 1000 {
            let cut = self.st.seen.len() - 1000;
            self.st.seen.drain(..cut);
        }
        if self.st.buffer.len() > 500 {
            let cut = self.st.buffer.len() - 500;
            self.st.buffer.drain(..cut);
        }
        self.save(); // buffer persisted together with the advanced cursor
        Ok(r.get("waiting_turns").cloned().unwrap_or(json!([])))
    }

    fn inbox(&self, after: u64, wait: u64) -> R<Value> {
        let path = format!(
            "/v1/seat/inbox?after={}&wait={}&consumer={}",
            after,
            wait,
            enc(&self.st.consumer)
        );
        self.call("GET", &path, None, wait + 30)
    }

    /// Bookkeeping for an event as it arrives. A tool result stays in
    /// `pending` until it is shown (`result`, `tool`, `next --all`): an
    /// arrived but unread result must survive buffer pruning and still
    /// block a final reply.
    fn note(&mut self, e: &Value) {
        let Some(c) = conv_id(e) else { return };
        let info = self.st.convs.entry(c.to_string()).or_default();
        if let Some(cl) = s(e, "client") {
            info.client = Some(cl.to_string());
        }
        if s(e, "type") == Some("message.received") {
            info.last = s(e, "text").unwrap_or("").chars().take(100).collect();
            info.t = now_secs();
            info.aux = aux_hint(e).is_some();
            // A new message opens a new turn: older calls are settled.
            self.st.pending.retain(|p| p[1] != c);
        }
        if self.st.convs.len() > 50 {
            let mut v: Vec<_> = self
                .st
                .convs
                .iter()
                .map(|(k, i)| (i.t, k.clone()))
                .collect();
            v.sort();
            for (_, k) in v.into_iter().take(self.st.convs.len() - 50) {
                self.st.convs.remove(&k);
                self.st.caps.remove(&k);
                self.st.tools.remove(&k);
            }
        }
    }

    fn conv(&self, c: Option<String>) -> R<String> {
        match c {
            Some(c) => self.resolve_conv(&c),
            None => self.st.active.clone().ok_or_else(|| {
                Fail(
                    USAGE,
                    "no active conversation: run `oh-seat next` first, or pass -c CONV".into(),
                )
            }),
        }
    }

    /// A full id, or any unique piece of one this seat has seen (`-c 6da0c`).
    fn resolve_conv(&self, c: &str) -> R<String> {
        let c = c.trim();
        if self.st.convs.contains_key(c) {
            return Ok(c.to_string());
        }
        let hits: Vec<&String> = self.st.convs.keys().filter(|k| k.contains(c)).collect();
        match hits.len() {
            1 => Ok(hits[0].clone()),
            0 if c.starts_with("cnv_") && c.len() >= 20 => Ok(c.to_string()),
            0 => usage(format!(
                "no conversation matches `{c}` (known: {}; `oh-seat convs`)",
                self.short_list()
            )),
            _ => usage(format!(
                "`{c}` matches {} conversations: {}; give more of the id",
                hits.len(),
                hits.iter()
                    .map(|h| h.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    fn short_list(&self) -> String {
        if self.st.convs.is_empty() {
            return "none yet".into();
        }
        let mut v: Vec<(&u64, &String)> = self.st.convs.iter().map(|(k, i)| (&i.t, k)).collect();
        v.sort_by(|a, b| b.cmp(a));
        let more = v.len().saturating_sub(5);
        let mut out = v
            .iter()
            .take(5)
            .map(|x| x.1.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        if more > 0 {
            out.push_str(&format!(" and {more} older"));
        }
        out
    }

    /// The caller's tools for a conversation, cached; `fresh` re-reads them.
    fn tools_for(&mut self, c: &str, fresh: bool) -> Vec<Value> {
        if !fresh {
            if let Some(t) = self.st.tools.get(c).filter(|t| !t.is_empty()) {
                return t.clone();
            }
        }
        match fetch_caps(self, c) {
            Ok(r) => {
                let t = tool_list(&r);
                self.st.tools.insert(c.to_string(), t.clone());
                self.save();
                t
            }
            Err(_) => self.st.tools.get(c).cloned().unwrap_or_default(),
        }
    }

    fn out(&self, v: &Value) {
        println!("{}", serde_json::to_string(v).unwrap_or_default());
    }
}

fn enc(x: &str) -> String {
    x.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{:02X}", b),
        })
        .collect()
}

fn s<'a>(e: &'a Value, k: &str) -> Option<&'a str> {
    e.get(k).and_then(|v| v.as_str()).filter(|v| !v.is_empty())
}

fn conv_id(e: &Value) -> Option<&str> {
    s(e, "conversation_id").filter(|c| c.starts_with("cnv_"))
}

// ── rendering ──

fn aux_hint(e: &Value) -> Option<String> {
    if let Some(f) = s(e, "expected_format") {
        return Some(format!("answer with {f}"));
    }
    let kind = s(e, "kind");
    let text = s(e, "text").unwrap_or("").to_lowercase();
    let head = s(e, "system_prompt_head").unwrap_or("").to_lowercase();
    let hay = format!("{head}\n{text}");
    let guess = if hay.contains("no_suggestion") {
        Some("suggestion lines, or exactly NO_SUGGESTION")
    } else if hay.contains("\"suggestions\"") {
        Some("JSON {\"suggestions\": [...]} (empty list when none)")
    } else if hay.contains("title") && hay.contains("\"title\"") {
        Some("JSON {\"title\": \"...\"}")
    } else if (hay.contains("title") && kind == Some("auxiliary"))
        || text.starts_with("generate a title")
    {
        Some("only a short title, no quotes or extra text")
    } else {
        None
    };
    match (guess, kind) {
        (Some(g), _) => Some(format!("answer with {g}")),
        (None, Some("auxiliary")) => Some(
            "auxiliary call: answer exactly in the format its prompt asks (oh-seat caps --full)"
                .into(),
        ),
        _ => None,
    }
}

fn max_output() -> usize {
    std::env::var("OH_SEAT_MAX_OUTPUT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30000)
}

/// Long tool output is cut so it does not flood the agent's context; the
/// whole of it is kept in a file the agent can read or grep.
fn cap_output(body: String, id: &str) -> String {
    let max = max_output();
    let n = body.chars().count();
    if n <= max {
        return body;
    }
    let dir = omni2api_home().join("results");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("{}.txt", id.replace(['/', '\\', '.'], "_")));
    let saved = std::fs::write(&path, &body).is_ok();
    let head: String = body.chars().take(max * 2 / 3).collect();
    let tail: String = body.chars().skip(n - max / 3).collect();
    let note = if saved {
        format!("full output: {}", path.display())
    } else {
        "full output could not be saved".into()
    };
    format!(
        "{head}\n[... {} of {n} chars cut; {note} ...]\n{tail}",
        n - head.chars().count() - tail.chars().count()
    )
}

/// Pictures a tool returned (data URLs saved under ~/.omni2api/images,
/// links kept as they are).
fn save_result_images(e: &Value) -> Vec<String> {
    let Some(imgs) = e.get("images").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let id = s(e, "invocation_id")
        .unwrap_or("result")
        .replace(['/', '\\', '.'], "_");
    let dir = omni2api_home().join("images");
    let _ = std::fs::create_dir_all(&dir);
    let mut out = Vec::new();
    for (i, url) in imgs.iter().filter_map(|v| v.as_str()).enumerate() {
        match data_url_bytes(url) {
            Some((mime, bytes)) => {
                let ext = mime
                    .rsplit('/')
                    .next()
                    .unwrap_or("bin")
                    .replace("jpeg", "jpg");
                let f = dir.join(format!("{id}-{}.{ext}", i + 1));
                if std::fs::write(&f, bytes).is_ok() {
                    out.push(f.display().to_string());
                }
            }
            None => out.push(url.to_string()),
        }
    }
    out
}

/// `data:image/png;base64,...` → (mime, bytes).
fn data_url_bytes(url: &str) -> Option<(String, Vec<u8>)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let mime = meta.split(';').next().unwrap_or("application/octet-stream");
    if !meta.ends_with(";base64") {
        return None;
    }
    let mut out = Vec::with_capacity(data.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for b in data.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' | b'\n' | b'\r' | b' ' => continue,
            _ => return None,
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some((mime.to_string(), out))
}

fn print_event(e: &Value) {
    let c = conv_id(e).unwrap_or("-");
    match s(e, "type").unwrap_or("event") {
        "message.received" => {
            let mut h = format!("MESSAGE conv={c} client={}", s(e, "client").unwrap_or("?"));
            if let Some(a) = aux_hint(e) {
                h.push_str(&format!("  AUX: {a}; reply with -c {c}"));
            }
            println!("{h}");
            println!("{}", s(e, "text").unwrap_or(""));
            if let Some(n) = e
                .get("image_count")
                .and_then(|v| v.as_u64())
                .or_else(|| {
                    e.get("images")
                        .and_then(|v| v.as_array())
                        .map(|a| a.len() as u64)
                })
                .filter(|n| *n > 0)
            {
                match e.get("image_files").and_then(|v| v.as_array()) {
                    Some(f) if !f.is_empty() => {
                        println!("[{n} image(s) attached, saved: open them with your own file/image tool]");
                        for p in f.iter().filter_map(|v| v.as_str()) {
                            println!("  image: {p}");
                        }
                    }
                    _ => println!("[{n} image(s) attached: oh-seat history -c {c}]"),
                }
            }
        }
        "conversation.started" => println!("NEW CONVERSATION conv={c}"),
        "tool.result" => {
            let ec = exit_code(e);
            let code = ec.map(|c| format!(" exit_code={c}")).unwrap_or_default();
            // Callers mark a failed command "success" (the tool ran): show
            // the outcome, not the transport status.
            let mut status = s(e, "status").unwrap_or("?");
            if status == "success"
                && (ec.is_some_and(|c| c != 0) || error_text(e) || json_failed(e))
            {
                status = "failed";
            }
            println!(
                "TOOL RESULT {} status={status}{code} id={}",
                s(e, "tool_id").unwrap_or("?"),
                s(e, "invocation_id").unwrap_or("?")
            );
            let body = s(e, "output")
                .map(trim_codex_header)
                .or_else(|| {
                    e.get("error").filter(|v| !v.is_null()).map(|v| {
                        v.as_str()
                            .map(String::from)
                            .unwrap_or_else(|| v.to_string())
                    })
                })
                .unwrap_or_default();
            println!(
                "{}",
                cap_output(body, s(e, "invocation_id").unwrap_or("result"))
            );
            for p in save_result_images(e) {
                println!("  image: {p}  (the tool returned a picture: open it with your own image/file tool)");
            }
        }
        t => {
            let mut rest = e.as_object().cloned().unwrap_or_default();
            for k in ["type", "conversation_id", "cursor"] {
                rest.remove(k);
            }
            let txt: String = Value::Object(rest).to_string().chars().take(300).collect();
            println!("{} conv={c} {txt}", t.to_uppercase());
        }
    }
}

// ── commands ──

/// Keep in the buffer only what someone will still read: messages, new
/// conversations, and results of calls still pending (a result whose call
/// is gone, or a capability notice, would sit there forever).
fn prune_buffer(cx: &mut Ctx) {
    let pending: Vec<String> = cx.st.pending.iter().map(|p| p[0].clone()).collect();
    cx.st.buffer.retain(|e| match s(e, "type") {
        Some("message.received") | Some("conversation.started") => true,
        Some("tool.result") => {
            s(e, "invocation_id").is_some_and(|i| pending.iter().any(|p| p == i))
        }
        _ => false,
    });
}

fn cmd_next(cx: &mut Ctx, timeout: u64, all: bool) -> R<u8> {
    let deadline = Instant::now() + Duration::from_secs(timeout);
    let mut polled = false;
    if !all {
        prune_buffer(cx);
    }
    loop {
        let wanted = |e: &Value| {
            all || matches!(
                s(e, "type"),
                Some("message.received") | Some("conversation.started")
            )
        };
        let has_msg = cx
            .st
            .buffer
            .iter()
            .any(|e| all || s(e, "type") == Some("message.received"));
        if has_msg {
            let buf = std::mem::take(&mut cx.st.buffer);
            // tool results nobody asked for yet stay buffered for `result`
            let (show, keep): (Vec<Value>, Vec<Value>) = buf.into_iter().partition(|e| wanted(e));
            cx.st.buffer = keep;
            for e in show.iter().filter(|e| s(e, "type") == Some("tool.result")) {
                if let Some(i) = s(e, "invocation_id") {
                    cx.st.pending.retain(|p| p[0] != i);
                }
            }
            prune_buffer(cx);
            let mut new_convs = Vec::new();
            for e in &show {
                // Auxiliary calls (titles, suggestions) never take over the
                // active conversation: a later bare `reply` must still land
                // in the real chat.
                if s(e, "type") == Some("message.received") && aux_hint(e).is_none() {
                    cx.st.active = conv_id(e).map(String::from);
                }
                if s(e, "type") == Some("conversation.started") {
                    if let Some(c) = conv_id(e) {
                        new_convs.push(c.to_string());
                    }
                }
            }
            for e in &show {
                if let Some(c) = conv_id(e).filter(|_| aux_hint(e).is_none()) {
                    if !cx.st.caps.contains_key(c) && !new_convs.iter().any(|n| n == c) {
                        new_convs.push(c.to_string());
                    }
                }
            }
            let mut tools_lines = Vec::new();
            let ev_env = |c: &str| -> Option<String> {
                show.iter()
                    .filter(|e| conv_id(e) == Some(c))
                    .find_map(|e| e.get("env").and_then(|v| v.as_object()).cloned())
                    .map(|m| {
                        m.iter()
                            .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or("")))
                            .collect::<Vec<_>>()
                            .join("  ")
                    })
            };
            for c in &new_convs {
                if let Ok(r) = fetch_caps(cx, c) {
                    let (_, sp_len, client) = caps_summary(&r);
                    if let Some(cl) = client {
                        cx.st.convs.entry(c.clone()).or_default().client = Some(cl);
                    }
                    let list = tool_list(&r);
                    if list.is_empty() {
                        tools_lines.push(format!(
                            "{c} declared no tools (system prompt {sp_len} chars): reply only"
                        ));
                    } else {
                        tools_lines.push(format!(
                            "tools in {c} ({}; * = required; system prompt {sp_len} chars, `oh-seat caps --full`):",
                            list.len()
                        ));
                        for t in &list {
                            tools_lines.push(format!("  {}", tool_sig(t)));
                        }
                    }
                    if let Some(env) = env_line(&r).or_else(|| ev_env(c)) {
                        tools_lines.push(format!("  caller env: {env}"));
                    }
                    if let Some(d) = deferred_line(&r) {
                        tools_lines.push(format!("  {d}"));
                    }
                    cx.st.tools.insert(c.clone(), list);
                    cx.st.caps.insert(c.clone(), caps_text(&r, true));
                }
            }
            cx.save();
            let mut show = show;
            for e in show.iter_mut() {
                if s(e, "client").is_none() {
                    if let Some(cl) = conv_id(e)
                        .and_then(|c| cx.st.convs.get(c))
                        .and_then(|i| i.client.clone())
                    {
                        e["client"] = json!(cl);
                    }
                }
            }
            for e in show.iter_mut() {
                let has_images = s(e, "type") == Some("message.received")
                    && e.get("image_count").and_then(|v| v.as_u64()).unwrap_or(0) > 0;
                if has_images {
                    if let (Some(c), Some(m)) = (conv_id(e), s(e, "message_id")) {
                        let files = cx.save_images(c, m);
                        if !files.is_empty() {
                            e["image_files"] = json!(files);
                        }
                    }
                }
            }
            for e in &show {
                if cx.json {
                    cx.out(e);
                } else {
                    print_event(e);
                }
            }
            // A conversation seen for the first time: say what tools it has,
            // so the next step needs no extra call.
            if !cx.json {
                for l in tools_lines {
                    println!("{l}");
                }
                let late: Vec<String> = cx
                    .st
                    .buffer
                    .iter()
                    .filter(|e| s(e, "type") == Some("tool.result"))
                    .filter_map(|e| s(e, "invocation_id"))
                    .filter(|i| !cx.st.hinted.iter().any(|h| h == i))
                    .map(String::from)
                    .collect();
                cx.st.hinted.extend(late.iter().cloned());
                let n = cx.st.hinted.len();
                if n > 200 {
                    cx.st.hinted.drain(..n - 200);
                }
                cx.save();
                if !late.is_empty() {
                    println!(
                        "({} earlier tool result(s) waiting: oh-seat result {})",
                        late.len(),
                        late.join(" / ")
                    );
                }
            }
            return Ok(OK);
        }
        let left = deadline.saturating_duration_since(Instant::now()).as_secs();
        // Poll at least once: `--timeout 0`/`1` must still see what is there.
        if left == 0 && polled {
            if !cx.json {
                let back: Vec<&str> = cx
                    .st
                    .buffer
                    .iter()
                    .filter(|e| s(e, "type") == Some("tool.result"))
                    .filter_map(|e| s(e, "invocation_id"))
                    .collect();
                if back.is_empty() {
                    println!("nothing new in {timeout}s (run `oh-seat next` again)");
                } else {
                    println!(
                        "no new message in {timeout}s; {} tool result(s) back, unread: oh-seat result {}",
                        back.len(),
                        back.join(" / ")
                    );
                }
            }
            return Ok(NOTHING);
        }
        cx.poll(left)?;
        polled = true;
    }
}

fn cmd_reply(
    cx: &mut Ctx,
    text: Option<String>,
    file: Option<PathBuf>,
    conv: Option<String>,
    reasoning: Option<String>,
    more: bool,
    force: bool,
) -> R<u8> {
    let c = cx.conv(conv)?;
    // Ending the turn while a call is out loses the reply: it rides along
    // with the call and the caller, busy running the tool, never shows it.
    if !more && !force && cx.st.pending.iter().any(|p| p[1] == c) {
        let _ = cx.poll(0);
    }
    if !more && !force {
        let out: Vec<&[String; 3]> = cx.st.pending.iter().filter(|p| p[1] == c).collect();
        if !out.is_empty() {
            let arrived = |i: &str| {
                cx.st.buffer.iter().any(|e| {
                    s(e, "type") == Some("tool.result") && s(e, "invocation_id") == Some(i)
                })
            };
            let list: Vec<String> = out
                .iter()
                .map(|p| {
                    let st = if arrived(&p[0]) {
                        "back, unread"
                    } else {
                        "not back yet"
                    };
                    format!("{} ({}, {st})", p[0], p[2])
                })
                .collect();
            return usage(format!(
                "a tool call in {c} has a result you have not seen: {}\nread it with `oh-seat result {}`, then reply (--force replies anyway)",
                list.join(", "),
                out[0][0]
            ));
        }
    }
    let text = match (file, text) {
        (Some(f), _) if f.as_os_str() != "-" => {
            std::fs::read_to_string(&f).or_else(|e| usage(format!("{}: {e}", f.display())))?
        }
        (None, Some(t)) if t != "-" => t,
        _ => {
            let mut b = String::new();
            std::io::stdin()
                .read_to_string(&mut b)
                .or_else(|e| usage(format!("stdin: {e}")))?;
            b
        }
    };
    if text.trim().is_empty() {
        return usage("empty reply");
    }
    let mut body = json!({
        "conversation_id": c, "text": text, "end_turn": !more,
        "client_msg_id": format!("ohs_{}", rand_hex(24)),
    });
    if let Some(r) = reasoning {
        body["reasoning"] = json!(r);
    }
    let r = cx.call("POST", "/v1/seat/reply", Some(body), 60)?;
    if cx.json {
        cx.out(&r);
    } else {
        let how = match r.get("live").and_then(|v| v.as_bool()) {
            Some(true) => "delivered live",
            Some(false) => "caller not connected; it gets the reply with its next request",
            None => "sent",
        };
        println!("replied conv={c} ({how})");
    }
    Ok(OK)
}

/// Wait for one tool result (from the buffer or new polls) until `deadline`.
fn await_result(cx: &mut Ctx, inv: &str, deadline: Instant) -> R<Option<Value>> {
    let mut polled = false;
    loop {
        if let Some(i) =
            cx.st.buffer.iter().position(|e| {
                s(e, "type") == Some("tool.result") && s(e, "invocation_id") == Some(inv)
            })
        {
            let e = cx.st.buffer.remove(i);
            cx.st.done.push(inv.to_string());
            cx.st.pending.retain(|p| p[0] != inv);
            let n = cx.st.done.len();
            if n > 200 {
                cx.st.done.drain(..n - 200);
            }
            cx.save();
            return Ok(Some(e));
        }
        // A message while a tool runs may be the tool itself asking (Claude
        // Code's WebFetch hands the page to the model): waiting on would
        // deadlock, so stop and show it. Another conversation's message
        // cannot be that, so it waits its turn.
        let conv = cx
            .st
            .pending
            .iter()
            .find(|p| p[0] == inv)
            .map(|p| p[1].clone());
        if message_waiting(cx, conv.as_deref()) {
            return Ok(None);
        }
        let left = deadline.saturating_duration_since(Instant::now()).as_secs();
        if left == 0 && polled {
            return Ok(None);
        }
        cx.poll(left)?;
        polled = true;
    }
}

/// A message is buffered: in `conv` when given (any conversation when the
/// conversation is unknown).
fn message_waiting(cx: &Ctx, conv: Option<&str>) -> bool {
    cx.st.buffer.iter().any(|e| {
        s(e, "type") == Some("message.received") && conv.is_none_or(|c| conv_id(e) == Some(c))
    })
}

/// After a tool wait in `conv`: show a message from that conversation (the
/// tool may be waiting on it), the way `next` would; messages from other
/// conversations stay queued for `next` and are only counted.
fn show_interrupting(cx: &mut Ctx, conv: Option<&str>) -> R<()> {
    if message_waiting(cx, conv) {
        if !cx.json {
            println!(
                "-- a message came in while waiting (answer it; the tool may be waiting on it):"
            );
        }
        return cmd_next(cx, 0, false).map(|_| ());
    }
    let mut others: Vec<&str> = cx
        .st
        .buffer
        .iter()
        .filter(|e| s(e, "type") == Some("message.received"))
        .filter_map(conv_id)
        .collect();
    others.sort();
    others.dedup();
    if !others.is_empty() {
        if cx.json {
            cx.out(&json!({"waiting_in": others}));
        } else {
            println!(
                "({} other conversation(s) have a message waiting: `oh-seat next` after this one)",
                others.len()
            );
        }
    }
    Ok(())
}

/// The shell exit code a caller wrote into a result's text. pi and Codex
/// report a failed command as a successful tool call whose output ends
/// with "Command exited with code 2" / "Process exited with code 2".
fn exit_code(e: &Value) -> Option<i64> {
    let out = s(e, "output")?;
    // Trae RunCommand: `<command_exit_code>3</command_exit_code>` near the top.
    if let Some(i) = out.find("<command_exit_code>") {
        let n: String = out[i + 19..]
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '-')
            .collect();
        if let Ok(n) = n.parse::<i64>() {
            return Some(n);
        }
    }
    // `; echo exit=$?` (what the guide suggests for callers whose shell
    // results carry no status, like opencode): its last line is the code.
    if let Some(last) = out.lines().rev().find(|l| !l.trim().is_empty()) {
        let l = last.trim();
        if let Some(n) = l
            .strip_prefix("exit=")
            .or_else(|| l.strip_prefix("exit_code="))
        {
            if let Ok(n) = n.parse::<i64>() {
                return Some(n);
            }
        }
    }
    let tail: String = {
        let t = out.trim_end();
        let start = t.char_indices().rev().nth(400).map(|(i, _)| i).unwrap_or(0);
        t[start..].to_string()
    };
    let mut found = None;
    for line in tail.lines().chain(out.lines().take(4)) {
        let l = line.trim().to_ascii_lowercase();
        // Claude Code "Error: Exit code 1", Reasonix "error: command exited:
        // exit status 2", Cline "[Command exited with code 2]".
        let l = l
            .trim_start_matches('[')
            .trim_start_matches("error: ")
            .trim_start_matches("command exited: ")
            .to_string();
        for pre in [
            "exit status ",
            "command exited with code ",
            "process exited with code ",
            "exit code: ",
            "exit code ",
            "exited with code ",
            "exit: ",
        ] {
            if let Some(rest) = l.strip_prefix(pre) {
                let n: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '-')
                    .collect();
                if let Ok(n) = n.parse::<i64>() {
                    found = Some(n);
                }
            }
        }
        // Copilot: "<shellId: 0 completed with exit code 2>"
        if found.is_none() {
            if let Some(i) = l.find("completed with exit code ") {
                let n: String = l[i + 25..]
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '-')
                    .collect();
                found = n.parse().ok();
            }
        }
        if found.is_some() {
            break;
        }
    }
    found
}

/// Codex wraps every exec_command result in a header (Chunk ID, Wall time,
/// Process exited with code N, Original token count, Output:). The exit
/// code is already on the TOOL RESULT line; drop the rest. A still-running
/// session keeps its "Process running with session ID" line.
fn trim_codex_header(out: &str) -> String {
    if !out.starts_with("Chunk ID:") {
        return out.to_string();
    }
    let mut keep = Vec::new();
    let mut lines = out.lines();
    for l in lines.by_ref() {
        if l == "Output:" {
            break;
        }
        if l.starts_with("Process running") {
            keep.push(l.to_string());
        }
    }
    let rest: Vec<&str> = lines.collect();
    keep.extend(rest.iter().map(|l| l.to_string()));
    keep.join("\n")
}

/// A result whose output is JSON carrying `"success": false` (Cline's
/// run_commands answers a list of per-command records).
fn json_failed(e: &Value) -> bool {
    let Some(out) = s(e, "output") else {
        return false;
    };
    let t = out.trim();
    if !(t.starts_with('[') || t.starts_with('{')) {
        return false;
    }
    let Ok(v) = serde_json::from_str::<Value>(t) else {
        return false;
    };
    let bad = |x: &Value| x.get("success").and_then(|b| b.as_bool()) == Some(false);
    match &v {
        Value::Array(a) => a.iter().any(bad),
        o => bad(o),
    }
}

/// A result the caller marked successful whose output is an error message
/// (Reasonix answers a refused or failed call with `error: ...` and status
/// success).
fn error_text(e: &Value) -> bool {
    if s(e, "status") != Some("success") {
        return false;
    }
    let t = s(e, "output").unwrap_or("").trim_start();
    let head: String = t.chars().take(24).collect::<String>().to_ascii_lowercase();
    head.starts_with("error:")
        || head.starts_with("error -")
        || head.starts_with("<tool_use_error>")
        || head.starts_with("tool error")
        || head.starts_with("blocked:")
}

fn result_exit(e: &Value) -> u8 {
    if matches!(exit_code(e), Some(c) if c != 0) || json_failed(e) || error_text(e) {
        return TOOL_ERR;
    }
    match s(e, "status") {
        Some("error") | Some("failed") | Some("failure") => TOOL_ERR,
        _ if e.get("error").map(|v| !v.is_null()).unwrap_or(false) => TOOL_ERR,
        _ => OK,
    }
}

/// Which pending call `oh-seat result [ID]` means: a full id passes as is
/// (it may be one this state never saw), a piece must match exactly one
/// pending id, and none means the latest pending call, preferring the
/// active conversation.
fn pick_pending(
    pending: &[[String; 3]],
    active: Option<&str>,
    want: Option<&str>,
) -> Result<String, String> {
    let list = || {
        pending
            .iter()
            .map(|p| format!("  {} {} in {}", p[0], p[2], p[1]))
            .collect::<Vec<_>>()
            .join("\n")
    };
    match want {
        Some(w) => {
            if pending.iter().any(|p| p[0] == w) || w.starts_with("call_") && w.len() > 20 {
                return Ok(w.to_string());
            }
            let hits: Vec<&[String; 3]> = pending.iter().filter(|p| p[0].contains(w)).collect();
            match hits.len() {
                1 => Ok(hits[0][0].clone()),
                0 if pending.is_empty() => {
                    Err(format!("no pending call matches `{w}` (none pending)"))
                }
                0 => Err(format!(
                    "no pending call matches `{w}`; pending:\n{}",
                    list()
                )),
                _ => Err(format!("`{w}` matches several pending calls:\n{}", list())),
            }
        }
        None => pending
            .iter()
            .rev()
            .find(|p| Some(p[1].as_str()) == active)
            .or_else(|| pending.last())
            .map(|p| p[0].clone())
            .ok_or_else(|| {
                "no tool call is pending (give an id to wait for one anyway)".to_string()
            }),
    }
}

fn wait_result(cx: &mut Ctx, inv: &str, timeout: u64) -> R<u8> {
    let deadline = Instant::now() + Duration::from_secs(timeout);
    let conv = cx
        .st
        .pending
        .iter()
        .find(|p| p[0] == inv)
        .map(|p| p[1].clone());
    match await_result(cx, inv, deadline)? {
        Some(mut e) => {
            if let Some(c) = exit_code(&e) {
                e["exit_code"] = json!(c);
            }
            if cx.json {
                cx.out(&e);
            } else {
                print_event(&e);
            }
            show_interrupting(cx, conv.as_deref())?;
            Ok(result_exit(&e))
        }
        None => {
            if cx.json {
                cx.out(&json!({"pending": inv}));
            } else {
                println!("result not back yet: run `oh-seat result {inv}` to keep waiting");
            }
            show_interrupting(cx, conv.as_deref())?;
            Ok(NOTHING)
        }
    }
}

/// Find a tool by name: exact, then case-insensitive.
fn find_tool<'a>(tools: &'a [Value], name: &str) -> Option<&'a Value> {
    tools.iter().find(|t| tool_name(t) == name).or_else(|| {
        let hits: Vec<&Value> = tools
            .iter()
            .filter(|t| tool_name(t).eq_ignore_ascii_case(name))
            .collect();
        (hits.len() == 1).then(|| hits[0])
    })
}

fn unknown_tool(c: &str, name: &str, tools: &[Value]) -> String {
    let names: Vec<String> = tools.iter().map(tool_name).collect();
    let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    let mut m = format!("no tool `{name}` in {c}");
    if let Some(n) = input::closest(name, &refs) {
        if let Some(t) = find_tool(tools, n) {
            m.push_str(&format!("; did you mean {}?", tool_sig(t)));
        }
    }
    m.push_str(&format!("\ntools: {}", names.join(", ")));
    m
}

struct Prepared {
    tool: String,
    input: Value,
}

/// The most recently active other conversations with tools, as a `-c` hint.
fn other_tool_convs<'a>(
    tools: impl Iterator<Item = (&'a String, &'a Vec<Value>)>,
    seen: &BTreeMap<String, ConvInfo>,
    c: &str,
) -> Option<String> {
    let mut v: Vec<(u64, &str)> = tools
        .filter(|(k, t)| k.as_str() != c && !t.is_empty())
        .map(|(k, _)| (seen.get(k).map(|i| i.t).unwrap_or(0), k.as_str()))
        .collect();
    v.sort_by(|a, b| b.cmp(a));
    v.truncate(3);
    if v.is_empty() {
        return None;
    }
    Some(format!(
        "Latest conversations with tools: {} (pass -c CONV, any piece of the id).",
        v.iter().map(|x| x.1).collect::<Vec<_>>().join(", ")
    ))
}

fn prepare(
    cx: &mut Ctx,
    c: &str,
    calls: Vec<(String, Vec<String>)>,
    force: bool,
) -> R<Vec<Prepared>> {
    let mut tools = cx.tools_for(c, false);
    let mut refreshed = false;
    let mut out = Vec::new();
    let mut errs = Vec::new();
    let many = calls.len() > 1;
    for (i, (name, args)) in calls.into_iter().enumerate() {
        let tag = if many {
            format!("call {} ({name})", i + 1)
        } else {
            name.clone()
        };
        let parsed = input::parse(&args).map_err(|e| Fail(USAGE, format!("{tag}: {e}")))?;
        if tools.is_empty() && !force {
            // A reply-only conversation (a title or summary request) can
            // be the active one: point at the conversations that do have
            // tools instead of letting the server answer 404.
            if let Some(h) = other_tool_convs(cx.st.tools.iter(), &cx.st.convs, c) {
                return usage(format!(
                    "{c} declared no tools (reply only), so `{name}` cannot run there. {h}"
                ));
            }
        }
        if tools.is_empty() {
            // No tool list known (the caller declared none we can read):
            // send as written and let the server judge the name.
            let m = input::build(parsed, &Value::Null).unwrap_or_default();
            out.push(Prepared {
                tool: name,
                input: Value::Object(m),
            });
            continue;
        }
        let mut t = find_tool(&tools, &name).cloned();
        if t.is_none() && !refreshed {
            tools = cx.tools_for(c, true);
            refreshed = true;
            t = find_tool(&tools, &name).cloned();
        }
        let Some(t) = t else {
            if force {
                let m = input::build(parsed, &Value::Null).unwrap_or_default();
                out.push(Prepared {
                    tool: name,
                    input: Value::Object(m),
                });
                continue;
            }
            let deferred = cx.st.caps.get(c).and_then(|t| {
                t.lines()
                    .find_map(|l| l.trim().strip_prefix(DEFERRED))
                    .and_then(|d| d.split(", ").find(|n| n.eq_ignore_ascii_case(&name)))
                    .map(String::from)
            });
            match deferred {
                Some(d) => errs.push(format!(
                    "`{d}` is a deferred tool in {c}: load it first with\n  oh-seat tool ToolSearch query=select:{d} max_results=1\nthen call it"
                )),
                None => errs.push(unknown_tool(c, &name, &tools)),
            }
            continue;
        };
        let real = tool_name(&t);
        match input::build(parsed.clone(), tool_schema(&t)) {
            Ok(mut m) => out.push({
                if let Some(cwd) = caller_cwd(cx, c) {
                    absolutize_paths(&mut m, &cwd);
                }
                Prepared {
                    tool: real,
                    input: Value::Object(m),
                }
            }),
            Err(_) if force => {
                let m = input::build(parsed, &Value::Null).unwrap_or_default();
                out.push(Prepared {
                    tool: real,
                    input: Value::Object(m),
                });
            }
            Err(e) => errs.push(format!(
                "{tag}: {}\n  usage: {}   (`oh-seat caps --tool {real}` explains each parameter)",
                e.join("; "),
                tool_sig(&t)
            )),
        }
    }
    let left = input::stdin_leftover();
    if left > 0 {
        return Err(Fail(
            USAGE,
            format!(
                "stdin has {left} more text(s) than there are key=@- (texts are separated by `{}` lines); nothing was sent",
                input::STDIN_SPLIT
            ),
        ));
    }
    if !errs.is_empty() {
        return usage(format!(
            "{}\nnothing was sent (--force sends anyway)",
            errs.join("\n")
        ));
    }
    Ok(out)
}

/// The caller's working directory, as its caps' env line names it.
fn caller_cwd(cx: &Ctx, c: &str) -> Option<String> {
    let caps = cx.st.caps.get(c)?;
    let env = caps
        .lines()
        .find_map(|l| l.trim().strip_prefix("caller env: "))?;
    let rest = env.split_once("cwd=")?.1;
    let cwd = rest.split("  ").next()?.trim();
    (!cwd.is_empty()).then(|| cwd.to_string())
}

/// Path parameters given relative (`file_path=src/a.py`) are resolved
/// against the caller's working directory: most callers' file tools demand
/// absolute paths, and typing the cwd into every call is pure friction.
fn absolutize_paths(m: &mut serde_json::Map<String, Value>, cwd: &str) {
    const KEYS: [&str; 9] = [
        "file_path",
        "filePath",
        "path",
        "target_file",
        "notebook_path",
        "absolute_path",
        "file",
        "filename",
        "dir_path",
    ];
    let win = cwd.contains('\\') && !cwd.contains('/');
    for k in KEYS {
        let Some(Value::String(v)) = m.get(k) else {
            continue;
        };
        let v = v.trim();
        let absolute = v.is_empty()
            || v.starts_with('/')
            || v.starts_with('~')
            || v.starts_with('\\')
            || v.contains("://")
            || v.as_bytes().get(1) == Some(&b':');
        if absolute {
            continue;
        }
        let rel = v
            .strip_prefix("./")
            .or_else(|| v.strip_prefix(".\\"))
            .unwrap_or(v);
        // One separator style on Windows: `E:\\a\\b/c` reads as a typo.
        let rel = if win {
            rel.replace('/', "\\")
        } else {
            rel.to_string()
        };
        let rel = rel.as_str();
        let sep = if win { '\\' } else { '/' };
        let base = cwd.trim_end_matches(['/', '\\']);
        let joined = if rel == "." {
            base.to_string()
        } else {
            format!("{base}{sep}{rel}")
        };
        m.insert(k.to_string(), Value::String(joined));
    }
}

#[allow(clippy::too_many_arguments)]
fn cmd_tool(
    cx: &mut Ctx,
    name: String,
    input: Vec<String>,
    conv: Option<String>,
    text: Option<String>,
    reasoning: Option<String>,
    timeout: u64,
    force: bool,
) -> R<u8> {
    let c = cx.conv(conv)?;
    let calls = input::split_batch(&name, &input).map_err(|e| Fail(USAGE, e))?;
    let prepared = prepare(cx, &c, calls, force)?;
    let n = prepared.len();
    let bodies: Vec<Value> = prepared
        .into_iter()
        .enumerate()
        .map(|(i, p)| {
            let mut body = json!({
                "conversation_id": c, "tool": p.tool, "input": p.input, "wait": 0,
                "client_msg_id": format!("ohs_{}", rand_hex(24)),
            });
            // Words and thinking ride on the first call only.
            if i == 0 {
                if let Some(t) = &text {
                    body["text"] = json!(t);
                }
                if let Some(r) = &reasoning {
                    body["reasoning"] = json!(r);
                }
            }
            body
        })
        .collect();
    // All calls go out at once so they land inside the server's merge
    // window and reach the caller as one assistant message (parallel
    // tool_calls), exactly like a model calling several tools.
    let sent: Vec<R<Value>> = if n == 1 {
        vec![cx.call("POST", "/v1/seat/tool-call", Some(bodies[0].clone()), 60)]
    } else {
        let cxr: &Ctx = cx;
        std::thread::scope(|sc| {
            let hs: Vec<_> = bodies
                .iter()
                .enumerate()
                .map(|(i, b)| {
                    sc.spawn(move || {
                        // keep the caller-visible order stable
                        std::thread::sleep(Duration::from_millis(40 * i as u64));
                        cxr.call("POST", "/v1/seat/tool-call", Some(b.clone()), 60)
                    })
                })
                .collect();
            hs.into_iter()
                .map(|h| {
                    h.join()
                        .unwrap_or_else(|_| Err(Fail(HTTP, "thread panicked".into())))
                })
                .collect()
        })
    };
    let mut invs: Vec<R<String>> = Vec::new();
    for (r, b) in sent.into_iter().zip(&bodies) {
        invs.push(r.and_then(|r| match s(&r, "invocation_id") {
            Some(i) => Ok(i.to_string()),
            None => Err(Fail(
                HTTP,
                format!("server returned no invocation id for {}: {r}", b["tool"]),
            )),
        }));
    }
    for (r, b) in invs.iter().zip(&bodies) {
        if let Ok(i) = r {
            cx.st.pending.push([
                i.clone(),
                c.clone(),
                b["tool"].as_str().unwrap_or("?").to_string(),
            ]);
        }
    }
    let keep = cx.st.pending.len().saturating_sub(50);
    cx.st.pending.drain(..keep);
    cx.save();
    if n == 1 {
        let inv = invs.pop().unwrap()?;
        // An auxiliary call answered by a tool call (Grok's session_title)
        // takes the call itself as its answer: nothing comes back.
        if cx.st.convs.get(&c).map(|i| i.aux).unwrap_or(false) {
            cx.st.pending.retain(|p| p[0] != inv);
            cx.save();
            if cx.json {
                cx.out(&json!({"invocation_id": inv, "auxiliary": true}));
            } else {
                println!(
                    "called {} in {c}: the auxiliary call takes it as its answer",
                    bodies[0]["tool"].as_str().unwrap_or("?")
                );
            }
            return Ok(OK);
        }
        return wait_result(cx, &inv, timeout);
    }
    let deadline = Instant::now() + Duration::from_secs(timeout);
    let mut worst = OK;
    for (i, (inv, b)) in invs.into_iter().zip(&bodies).enumerate() {
        let tool = b["tool"].as_str().unwrap_or("?").to_string();
        let res = match inv {
            Ok(inv) => await_result(cx, &inv, deadline).map(|r| (inv, r)),
            Err(e) => Err(e),
        };
        match res {
            Ok((_, Some(e))) => {
                worst = worst.max(result_exit(&e));
                if cx.json {
                    let mut e = e;
                    e["batch_index"] = json!(i + 1);
                    if let Some(c) = exit_code(&e) {
                        e["exit_code"] = json!(c);
                    }
                    cx.out(&e);
                } else {
                    println!("[{}/{n}]", i + 1);
                    print_event(&e);
                }
            }
            Ok((inv, None)) => {
                worst = worst.max(NOTHING);
                if cx.json {
                    cx.out(&json!({"batch_index": i + 1, "pending": inv, "tool": tool}));
                } else {
                    println!(
                        "[{}/{n}] {tool}: not back yet: `oh-seat result {inv}`",
                        i + 1
                    );
                }
            }
            Err(Fail(code, m)) => {
                worst = worst.max(code);
                if cx.json {
                    cx.out(&json!({"batch_index": i + 1, "tool": tool, "error": m, "exit": code}));
                } else {
                    println!("[{}/{n}] {tool}: not sent: {m}", i + 1);
                }
            }
        }
    }
    show_interrupting(cx, Some(&c))?;
    Ok(worst)
}

fn fetch_caps(cx: &Ctx, c: &str) -> R<Value> {
    cx.call(
        "GET",
        &format!("/v1/seat/capabilities?conversation_id={}&view=full", enc(c)),
        None,
        60,
    )
}

fn tool_list(r: &Value) -> Vec<Value> {
    for p in ["/context/tools", "/tools", "/capabilities"] {
        if let Some(a) = r
            .pointer(p)
            .and_then(|v| v.as_array())
            .filter(|a| !a.is_empty())
        {
            return a.clone();
        }
    }
    Vec::new()
}

fn tool_name(t: &Value) -> String {
    ["id", "name"]
        .iter()
        .find_map(|k| s(t, k))
        .or_else(|| t.pointer("/function/name").and_then(|v| v.as_str()))
        .unwrap_or("?")
        .to_string()
}

fn tool_schema(t: &Value) -> &Value {
    t.get("input_schema")
        .or_else(|| t.get("parameters"))
        .or_else(|| t.pointer("/function/parameters"))
        .unwrap_or(&Value::Null)
}

fn tool_desc(t: &Value) -> &str {
    s(t, "description")
        .or_else(|| t.pointer("/function/description").and_then(|v| v.as_str()))
        .unwrap_or("")
}

fn first_sentence(d: &str, max: usize) -> String {
    let line = d
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    // A sentence ends at ". ", but not inside "e.g. " / "i.e. ".
    let mut end = None;
    let mut from = 0;
    while let Some(i) = line[from..].find(". ").map(|i| i + from) {
        let before = line[..=i].to_ascii_lowercase();
        if before.ends_with("e.g.") || before.ends_with("i.e.") || before.ends_with("etc.") {
            from = i + 2;
            continue;
        }
        end = Some(i);
        break;
    }
    let cut = end.map(|i| &line[..=i]).unwrap_or(line);
    let mut out: String = cut.chars().take(max).collect();
    if cut.chars().count() > max {
        out.push('…');
    }
    out
}

fn ptype(p: &Value) -> String {
    match p.get("type") {
        Some(Value::String(t)) if t == "array" => {
            let inner = p.get("items").map(ptype).unwrap_or_else(|| "any".into());
            format!("{inner}[]")
        }
        // A small object spells out its fields, so an `object[]` parameter
        // (pi's edit `edits`) can be filled without reading the raw schema.
        Some(Value::String(t)) if t == "object" => {
            let props = p.get("properties").and_then(|v| v.as_object());
            match props {
                Some(pr) if !pr.is_empty() && pr.len() <= 6 => {
                    let req: Vec<&str> = p
                        .get("required")
                        .and_then(|v| v.as_array())
                        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                        .unwrap_or_default();
                    let mut f: Vec<(bool, String)> = pr
                        .iter()
                        .map(|(k, v)| {
                            let r = req.contains(&k.as_str());
                            (!r, format!("{k}{}: {}", if r { "*" } else { "" }, ptype(v)))
                        })
                        .collect();
                    f.sort_by_key(|(o, _)| *o);
                    let f: Vec<String> = f.into_iter().map(|(_, x)| x).collect();
                    format!("{{{}}}", f.join(", "))
                }
                _ => t.clone(),
            }
        }
        Some(Value::String(t)) => t.clone(),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join("|"),
        _ if p.get("enum").is_some() => "enum".into(),
        _ => "any".into(),
    }
}

/// `Bash(command*: string, timeout: number)` — required params first, marked *.
fn tool_sig(t: &Value) -> String {
    let sc = tool_schema(t);
    let req: Vec<&str> = sc
        .get("required")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    let mut params: Vec<(bool, String)> = Vec::new();
    if let Some(props) = sc.get("properties").and_then(|v| v.as_object()) {
        for (k, p) in props {
            let r = req.contains(&k.as_str());
            params.push((!r, format!("{k}{}: {}", if r { "*" } else { "" }, ptype(p))));
        }
    }
    params.sort_by_key(|a| a.0);
    format!(
        "{}({})",
        tool_name(t),
        params
            .into_iter()
            .map(|p| p.1)
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// The caller's machine as its system prompt describes it: working
/// directory, platform, shell, date. Saves reading 10k chars of prompt to
/// learn where relative paths land.
fn env_hints(sp: &str) -> Vec<(&'static str, String)> {
    let mut out: Vec<(&'static str, String)> = Vec::new();
    let mut put = |k: &'static str, v: &str| {
        let v = v.trim().trim_matches('`').trim();
        if !v.is_empty() && v.len() < 300 && !out.iter().any(|(x, _)| *x == k) {
            out.push((k, v.to_string()));
        }
    };
    for line in sp.lines() {
        let l = line.trim().trim_start_matches(['-', '*', ' ']);
        // "4. Working Directory: /w" (Cline numbers its env lines)
        let l = match l.split_once(". ") {
            Some((n, rest)) if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) => rest,
            _ => l,
        };
        let low = l.to_ascii_lowercase();
        for (pre, k) in [
            ("current working directory:", "cwd"),
            ("workspace path:", "cwd"),
            ("os version:", "platform"),
            ("working directory:", "cwd"),
            ("primary working directory:", "cwd"),
            ("cwd:", "cwd"),
            ("platform:", "platform"),
            ("os:", "platform"),
            ("shell:", "shell"),
            ("current date:", "date"),
            ("today's date:", "date"),
        ] {
            if low.starts_with(pre) {
                put(k, &l[pre.len()..]);
            }
        }
        // DeepSeek Harness: "Your working directory is C:\\Users\\a\\p."
        if let Some(v) = low
            .strip_prefix("your working directory is ")
            .map(|_| &l[26..])
        {
            put("cwd", v.trim().trim_end_matches('.'));
        }
        for (tag, k) in [("cwd", "cwd"), ("shell", "shell"), ("current_date", "date")] {
            let open = format!("<{tag}>");
            let close = format!("</{tag}>");
            if let Some(a) = l.find(&open) {
                if let Some(b) = l[a..].find(&close) {
                    put(k, &l[a + open.len()..a + b]);
                }
            }
        }
        if let Some(rest) = low.strip_prefix("today's date is ") {
            put("date", rest.trim_end_matches('.'));
        }
    }
    let rank = |k: &str| {
        ["cwd", "platform", "shell", "date"]
            .iter()
            .position(|x| *x == k)
    };
    out.sort_by_key(|(k, _)| rank(k));
    out.retain(|(_, v)| v != "unknown");
    out
}

const DEFERRED: &str = "deferred tools (ToolSearch query=select:NAME first): ";

/// Tools a caller lists by name only (Claude Code's deferred tools): they
/// become callable after ToolSearch loads them.
fn deferred_names(sp: &str) -> Vec<String> {
    let Some(i) = sp.find("deferred tools are now available via ToolSearch") else {
        return Vec::new();
    };
    let rest = &sp[i..];
    let mut out = Vec::new();
    for l in rest.lines().skip(1) {
        let l = l.trim();
        if l.is_empty() || l.starts_with('<') {
            break;
        }
        if l.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            && l.len() < 64
        {
            out.push(l.to_string());
        } else {
            break;
        }
    }
    out
}

fn deferred_line(r: &Value) -> Option<String> {
    let sp = r
        .pointer("/context/system_prompt")
        .or_else(|| r.get("system_prompt"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let d = deferred_names(sp);
    (!d.is_empty()).then(|| format!("{DEFERRED}{}", d.join(", ")))
}

fn env_line(r: &Value) -> Option<String> {
    let sp = r
        .pointer("/context/system_prompt")
        .or_else(|| r.get("system_prompt"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let h = env_hints(sp);
    (!h.is_empty()).then(|| {
        h.iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("  ")
    })
}

fn caps_summary(r: &Value) -> (Vec<String>, usize, Option<String>) {
    let tools = tool_list(r).iter().map(tool_name).collect();
    let sp = r
        .pointer("/context/system_prompt")
        .or_else(|| r.get("system_prompt"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let client = r
        .pointer("/context/client")
        .or_else(|| r.get("client"))
        .and_then(|v| v.as_str())
        .map(String::from);
    (tools, sp.chars().count(), client)
}

fn caps_text(r: &Value, full: bool) -> String {
    let (tools, sp_len, client) = caps_summary(r);
    let mut out = vec![
        format!("client: {}", client.unwrap_or_else(|| "?".into())),
        format!("tools ({}): {}", tools.len(), tools.join(", ")),
    ];
    if let Some(env) = env_line(r) {
        out.push(format!("caller env: {env}"));
    }
    if let Some(d) = deferred_line(r) {
        out.push(d);
    }
    if full {
        let sp = r
            .pointer("/context/system_prompt")
            .or_else(|| r.get("system_prompt"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        out.push(format!("system prompt ({sp_len} chars):"));
        out.push(sp.to_string());
        for t in tool_list(r) {
            out.push(format!("--- tool {}", tool_name(&t)));
            out.push(t.to_string());
        }
    } else {
        for t in tool_list(r) {
            out.push(format!(
                "  {}  {}",
                tool_sig(&t),
                first_sentence(tool_desc(&t), 90)
            ));
        }
        out.push(format!(
            "system prompt: {sp_len} chars (--full to read it; * = required)"
        ));
    }
    out.join("\n")
}

fn line_diff(old: &str, new: &str) -> String {
    let o: Vec<&str> = old.lines().collect();
    let n: Vec<&str> = new.lines().collect();
    let mut out = Vec::new();
    for l in &o {
        if !n.contains(l) {
            out.push(format!("- {l}"));
        }
    }
    for l in &n {
        if !o.contains(l) {
            out.push(format!("+ {l}"));
        }
    }
    out.join("\n")
}

fn cmd_caps(
    cx: &mut Ctx,
    conv: Option<String>,
    full: bool,
    diff: bool,
    tool: Option<String>,
    schema: bool,
) -> R<u8> {
    let c = cx.conv(conv)?;
    let r = fetch_caps(cx, &c)?;
    if let Some(name) = tool {
        let list = tool_list(&r);
        cx.st.tools.insert(c.clone(), list.clone());
        let Some(t) = find_tool(&list, &name) else {
            return usage(unknown_tool(&c, &name, &list));
        };
        let name = tool_name(t);
        if cx.json {
            cx.out(t);
        } else if schema {
            println!(
                "{}",
                serde_json::to_string_pretty(tool_schema(t)).unwrap_or_default()
            );
        } else {
            println!("{}", tool_sig(t));
            // The detail view is where a responder goes to read: nothing cut
            // with --full, otherwise the first paragraph.
            let d = tool_desc(t).trim();
            if full {
                println!("{d}");
            } else {
                let para = d.split("\n\n").next().unwrap_or("");
                println!("{}", para.split_whitespace().collect::<Vec<_>>().join(" "));
            }
            if let Some(props) = tool_schema(t).get("properties").and_then(|v| v.as_object()) {
                for (k, p) in props {
                    let mut line = format!("  {k}: {}", ptype(p));
                    if let Some(en) = p.get("enum").and_then(|v| v.as_array()) {
                        line.push_str(&format!(" one of {}", Value::Array(en.clone())));
                    }
                    if let Some(d) = s(p, "description") {
                        let d = d.split_whitespace().collect::<Vec<_>>().join(" ");
                        line.push_str(&format!(" — {d}"));
                    }
                    println!("{line}");
                }
            }
            println!(
                "(oh-seat tool {name} key=value ... ;{} --schema for raw JSON)",
                if full {
                    ""
                } else {
                    " --full for the whole description;"
                }
            );
        }
        return Ok(OK);
    }
    if cx.json {
        cx.out(&r);
        return Ok(OK);
    }
    let txt = caps_text(&r, true);
    if diff {
        match cx.st.caps.get(&c) {
            None => println!(
                "{}",
                if full {
                    txt.clone()
                } else {
                    caps_text(&r, false)
                }
            ),
            Some(old) if *old == txt => println!("no change since last view"),
            Some(old) => println!("{}", line_diff(old, &txt)),
        }
    } else {
        println!(
            "{}",
            if full {
                txt.clone()
            } else {
                caps_text(&r, false)
            }
        );
    }
    cx.st.caps.insert(c, txt);
    cx.save();
    Ok(OK)
}

fn cmd_history(cx: &mut Ctx, conv: Option<String>, limit: usize) -> R<u8> {
    let c = cx.conv(conv)?;
    let r = cx.call(
        "GET",
        &format!("/v1/seat/history?conversation_id={}&limit={limit}", enc(&c)),
        None,
        60,
    )?;
    if cx.json {
        cx.out(&r);
        return Ok(OK);
    }
    let msgs = r
        .get("messages")
        .and_then(|v| v.as_array())
        .cloned()
        .or_else(|| r.as_array().cloned())
        .unwrap_or_default();
    if msgs.is_empty() {
        println!("no messages");
    }
    for m in msgs {
        let role = s(&m, "role").or_else(|| s(&m, "side")).unwrap_or("?");
        let when = s(&m, "time").or_else(|| s(&m, "timestamp")).unwrap_or("");
        let text = match m.get("text").or_else(|| m.get("content")) {
            Some(Value::String(t)) => t.clone(),
            Some(v) => v.to_string(),
            None => String::new(),
        };
        println!("[{when}] {role}: {text}");
    }
    Ok(OK)
}

fn cmd_convs(cx: &Ctx) -> R<u8> {
    let mut v: Vec<_> = cx.st.convs.iter().collect();
    v.sort_by_key(|(_, i)| std::cmp::Reverse(i.t));
    if v.is_empty() {
        println!("no conversations seen yet (run `oh-seat next`)");
    }
    for (c, i) in v {
        let mark = if cx.st.active.as_deref() == Some(c.as_str()) {
            "*"
        } else {
            " "
        };
        if cx.json {
            cx.out(&json!({"conversation_id": c, "client": i.client, "last": i.last, "active": mark == "*"}));
        } else {
            println!(
                "{mark} {c} client={} last={:?}",
                i.client.as_deref().unwrap_or("?"),
                i.last
            );
        }
    }
    Ok(OK)
}

fn cmd_status(cx: &mut Ctx) -> R<u8> {
    // A wait=0 poll is still a read: keep what it returned.
    let (waiting, ok) = match cx.poll(0) {
        Ok(w) => (w, "ok".to_string()),
        Err(Fail(_, m)) => (json!([]), m),
    };
    let st = json!({
        "url": cx.url, "server": ok, "consumer": cx.st.consumer, "cursor": cx.st.cursor,
        "active": cx.st.active, "buffered_events": cx.st.buffer.len(), "open_turns": waiting,
        "pending_calls": cx.st.pending.iter().map(|p| format!("{} {} in {}", p[0], p[2], p[1])).collect::<Vec<_>>(),
        "state_file": cx.state_path.display().to_string(),
    });
    if cx.json {
        cx.out(&st);
    } else {
        for (k, v) in st.as_object().unwrap() {
            println!(
                "{k}: {}",
                v.as_str()
                    .map(String::from)
                    .unwrap_or_else(|| v.to_string())
            );
        }
    }
    Ok(if ok == "ok" { OK } else { HTTP })
}

fn run(cli: Cli) -> R<u8> {
    let cmd = match cli.cmd {
        None | Some(Cmd::Guide) => {
            println!("{}", GUIDE.trim_end());
            return Ok(OK);
        }
        Some(Cmd::Mcp) => return Ok(mcp::serve()),
        Some(c) => c,
    };
    let mut cx = Ctx::new(cli.g)?;
    match cmd {
        Cmd::Next { timeout, all } => cmd_next(&mut cx, timeout, all),
        Cmd::Reply {
            text,
            file,
            conv,
            reasoning,
            more,
            next,
            timeout,
            force,
        } => {
            cmd_reply(&mut cx, text, file, conv, reasoning, more, force)?;
            if next {
                cmd_next(&mut cx, timeout, false)
            } else {
                Ok(OK)
            }
        }
        Cmd::Tool {
            name,
            input,
            conv,
            text,
            reasoning,
            timeout,
            force,
        } => cmd_tool(&mut cx, name, input, conv, text, reasoning, timeout, force),
        Cmd::Result {
            invocation_id,
            timeout,
        } => {
            let inv = pick_pending(
                &cx.st.pending,
                cx.st.active.as_deref(),
                invocation_id.as_deref(),
            )
            .map_err(|m| Fail(USAGE, m))?;
            wait_result(&mut cx, &inv, timeout)
        }
        Cmd::Caps {
            conv,
            full,
            diff,
            tool,
            schema,
        } => cmd_caps(&mut cx, conv, full, diff, tool, schema),
        Cmd::History { conv, limit } => cmd_history(&mut cx, conv, limit),
        Cmd::Convs => cmd_convs(&cx),
        Cmd::Use { conv } => {
            let conv = cx.resolve_conv(&conv)?;
            cx.st.active = Some(conv.clone());
            cx.save();
            println!("active conversation: {conv}");
            Ok(OK)
        }
        Cmd::Save { conv } => {
            let c = cx.conv(conv)?;
            let r = cx.call(
                "POST",
                "/v1/seat/save",
                Some(json!({"conversation_id": c})),
                60,
            )?;
            if cx.json {
                cx.out(&r);
            } else {
                println!("saved conv={c}");
            }
            Ok(OK)
        }
        Cmd::Name { name } => {
            let r = match &name {
                Some(n) => cx.call(
                    "POST",
                    "/v1/seat/model-name",
                    Some(json!({ "name": n })),
                    60,
                )?,
                None => cx.call("GET", "/v1/seat/model-name", None, 60)?,
            };
            if name.is_some() {
                if let Some(m) = r["model"].as_str() {
                    sync_endpoint_model(&cx, m);
                }
            }
            if cx.json {
                cx.out(&r);
            } else if name.is_some() {
                println!("model: {}", r["model"].as_str().unwrap_or("?"));
            } else if r["locked"].as_bool() == Some(true) {
                println!(
                    "model: {} (pinned by the user; the seat cannot change it)",
                    r["model"].as_str().unwrap_or("?")
                );
            } else {
                println!(
                    "model: {} (yours to choose: oh-seat name <name>)",
                    r["model"].as_str().unwrap_or("?")
                );
            }
            Ok(OK)
        }
        Cmd::Status => cmd_status(&mut cx),
        Cmd::Guide | Cmd::Mcp => unreachable!(),
    }
}

fn main() -> ExitCode {
    // Rust ignores SIGPIPE at startup, so a closed pipe — our output is
    // piped through `head`, files and other tools all the time — surfaces
    // as a "failed printing to stdout: Broken pipe" panic. Restore the
    // default disposition and let the process die quietly instead.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
    match run(cli) {
        Ok(c) => ExitCode::from(c),
        Err(Fail(c, m)) => {
            eprintln!("error: {m}");
            ExitCode::from(c)
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn proxy_bypass_covers_loopback_and_no_proxy() {
        assert!(super::host_is_bypassed("localhost", ""));
        assert!(super::host_is_bypassed("dev.localhost", ""));
        assert!(super::host_is_bypassed("127.0.0.1", ""));
        assert!(super::host_is_bypassed("::1", ""));
        assert!(!super::host_is_bypassed("x.run.pinggy-free.link", ""));
        assert!(super::host_is_bypassed(
            "api.example.com",
            "internal, .example.com"
        ));
        assert!(!super::host_is_bypassed("api.example.com", "other.com"));
        assert!(super::host_is_bypassed("anything.net", "*"));
        assert_eq!(
            super::url_host("https://X.Run.Pinggy-Free.Link:443/v1"),
            "x.run.pinggy-free.link"
        );
        assert_eq!(super::url_host("http://[::1]:8080"), "::1");
        assert_eq!(super::url_host("http://127.0.0.1:8080"), "127.0.0.1");
    }

    #[test]
    fn first_sentence_skips_eg() {
        assert_eq!(
            super::first_sentence("Roll dice (e.g. 2d6, d20). Returns rolls.", 90),
            "Roll dice (e.g. 2d6, d20)."
        );
    }

    #[test]
    fn object_items_spell_out_fields() {
        let p = serde_json::json!({"type":"array","items":{"type":"object",
            "properties":{"oldText":{"type":"string"},"newText":{"type":"string"},"n":{"type":"integer"}},
            "required":["oldText","newText"]}});
        assert_eq!(
            super::ptype(&p),
            "{newText*: string, oldText*: string, n: integer}[]"
        );
        assert_eq!(
            super::ptype(&serde_json::json!({"type":"object"})),
            "object"
        );
    }

    use super::*;

    #[test]
    fn data_urls_decode() {
        let (m, b) = data_url_bytes("data:image/png;base64,aGVsbG8=").unwrap();
        assert_eq!(
            (m.as_str(), b.as_slice()),
            ("image/png", b"hello".as_slice())
        );
        assert!(data_url_bytes("https://x/y.png").is_none());
    }

    #[test]
    fn exit_code_wordings() {
        let r = |o: &str| json!({"type": "tool.result", "status": "success", "output": o});
        assert_eq!(
            exit_code(&r("Error: Exit code 1\nNo module named pytest")),
            Some(1)
        );
        assert_eq!(
            exit_code(&r("error: command exited: exit status 2\nls: x")),
            Some(2)
        );
        assert_eq!(
            exit_code(&r("[Command exited with code 2]  [stderr] ls")),
            Some(2)
        );
    }

    #[test]
    fn error_output_is_an_error() {
        let r = |o: &str| json!({"type": "tool.result", "status": "success", "output": o});
        assert_eq!(
            result_exit(&r("error: bash sandbox requested but unavailable")),
            TOOL_ERR
        );
        assert_eq!(
            result_exit(&r("blocked: this command runs a verification check")),
            TOOL_ERR
        );
        assert_eq!(result_exit(&r("no errors found")), OK);
    }

    #[test]
    fn json_success_false_is_an_error() {
        let r = |o: &str| json!({"type": "tool.result", "status": "success", "output": o});
        assert_eq!(
            result_exit(&r(r#"[{"query":"ls nope","success":false}]"#)),
            TOOL_ERR
        );
        assert_eq!(result_exit(&r(r#"[{"query":"pwd","success":true}]"#)), OK);
        assert_eq!(
            env_hints("1. Platform: linux\n4. Working Directory: /w")[0].1,
            "/w"
        );
    }

    #[test]
    fn codex_header_trimmed() {
        let o = "Chunk ID: 1\nWall time: 0.0 seconds\nProcess exited with code 0\nOriginal token count: 9\nOutput:\n/w\n";
        assert_eq!(trim_codex_header(o), "/w");
        let o = "Chunk ID: 1\nWall time: 10 seconds\nProcess running with session ID 7\nOriginal token count: 1\nOutput:\nx";
        assert_eq!(trim_codex_header(o), "Process running with session ID 7\nx");
        assert_eq!(trim_codex_header("plain"), "plain");
    }

    #[test]
    fn deferred_tool_names() {
        let sp = "x\nThe following deferred tools are now available via ToolSearch. Their schemas are NOT loaded:\nCronCreate\nWebFetch\n\nmore";
        assert_eq!(deferred_names(sp), vec!["CronCreate", "WebFetch"]);
        assert!(deferred_names("nothing").is_empty());
    }

    #[test]
    fn result_picks_pending_call() {
        let p = |i: &str, c: &str| [i.to_string(), c.to_string(), "bash".to_string()];
        let pend = vec![
            p("call_aaa111", "cnv_x"),
            p("call_bbb222", "cnv_y"),
            p("call_ccc333", "cnv_x"),
        ];
        assert_eq!(
            pick_pending(&pend, Some("cnv_x"), None).unwrap(),
            "call_ccc333"
        );
        assert_eq!(
            pick_pending(&pend, Some("cnv_y"), None).unwrap(),
            "call_bbb222"
        );
        assert_eq!(
            pick_pending(&pend, Some("cnv_z"), None).unwrap(),
            "call_ccc333"
        );
        assert_eq!(
            pick_pending(&pend, None, Some("bbb")).unwrap(),
            "call_bbb222"
        );
        assert!(pick_pending(&pend, None, Some("call_")).is_err());
        assert!(pick_pending(&pend, None, Some("zzz")).is_err());
        assert!(pick_pending(&[], None, None).is_err());
        let full = "call_01a15f590a277192abcd22f7503380eb";
        assert_eq!(pick_pending(&[], None, Some(full)).unwrap(), full);
    }

    #[test]
    fn exit_codes_in_output() {
        let r = |o: &str| json!({"type": "tool.result", "status": "success", "output": o});
        assert_eq!(
            exit_code(&r("ls: nope\n\n\nCommand exited with code 2")),
            Some(2)
        );
        assert_eq!(result_exit(&r("x\nCommand exited with code 2")), TOOL_ERR);
        let codex =
            "Chunk ID: 1\nWall time: 0.0 seconds\nProcess exited with code 0\nOutput:\n/w\n";
        assert_eq!(exit_code(&r(codex)), Some(0));
        assert_eq!(result_exit(&r(codex)), OK);
        assert_eq!(
            exit_code(&r("Exit code 127\nbash: foo: not found")),
            Some(127)
        );
        assert_eq!(exit_code(&r("hello")), None);
        let trae = "<toolcall_status>done</toolcall_status>\n<terminal_cwd>e:\\a</terminal_cwd>\n<command_status>Exited</command_status>\n<command_exit_code>3</command_exit_code>\n<command_run_logs>\nx\n</command_run_logs>";
        assert_eq!(exit_code(&r(trae)), Some(3));
        assert_eq!(
            exit_code(&r(
                "Stdout: ok\nStderr: (empty)\nExit Code: 1\nSignal: (none)"
            )),
            Some(1)
        );
        assert_eq!(exit_code(&r("exit: 1\nboom")), Some(1));
        assert_eq!(exit_code(&r("boom\nexit=1\n")), Some(1));
        assert_eq!(result_exit(&r("boom\nexit=1")), TOOL_ERR);
        assert_eq!(exit_code(&r("23 passed\nexit=0")), Some(0));
        assert_eq!(exit_code(&r("exit=1 is a flag\nok")), None);
        assert_eq!(
            exit_code(&r("x\n<shellId: 0 completed with exit code 2>")),
            Some(2)
        );
    }

    #[test]
    fn relative_paths_resolve_against_caller_cwd() {
        let mut m = serde_json::Map::new();
        m.insert("file_path".into(), json!("src/a.py"));
        m.insert("path".into(), json!("/abs/b"));
        m.insert("command".into(), json!("ls x"));
        absolutize_paths(&mut m, "/w/proj/");
        assert_eq!(m["file_path"], "/w/proj/src/a.py");
        assert_eq!(m["path"], "/abs/b");
        assert_eq!(m["command"], "ls x");
        let mut w = serde_json::Map::new();
        w.insert("filePath".into(), json!("./a.txt"));
        absolutize_paths(&mut w, "C:\\work");
        assert_eq!(w["filePath"], "C:\\work\\a.txt");
        let mut q = serde_json::Map::new();
        q.insert("file_path".into(), json!("aevum/package.json"));
        absolutize_paths(&mut q, "E:\\aevum");
        assert_eq!(q["file_path"], "E:\\aevum\\aevum\\package.json");
    }

    #[test]
    fn env_from_prompts() {
        let claude = "<env>\nWorking directory: /home/a/p\nIs directory a git repo: no\nPlatform: linux\nOS Version: Linux 6\n</env>\nToday's date is 2026-10-21.";
        let h = env_hints(claude);
        assert_eq!(h[0], ("cwd", "/home/a/p".to_string()));
        assert!(h.contains(&("platform", "linux".to_string())));
        assert!(h.contains(&("date", "2026-10-21".to_string())));
        let codex = "<environment_context>\n  <cwd>/srv/x</cwd>\n  <shell>bash</shell>\n</environment_context>";
        let h = env_hints(codex);
        assert_eq!(
            h,
            vec![("cwd", "/srv/x".to_string()), ("shell", "bash".to_string())]
        );
        assert_eq!(
            env_hints("Current working directory: /w\nCurrent date: 2026-10-21")[0].1,
            "/w"
        );
        assert!(env_hints("you are helpful").is_empty());
        assert_eq!(
            env_hints("Your working directory is C:\\Users\\J\\Zephyr.")[0],
            ("cwd", "C:\\Users\\J\\Zephyr".to_string())
        );
    }

    #[test]
    fn other_tool_convs_hint() {
        let a = "cnv_a".to_string();
        let b = "cnv_b".to_string();
        let full = vec![json!({"name": "bash"})];
        let none: Vec<Value> = vec![];
        let m = [(&a, &none), (&b, &full)];
        let seen = BTreeMap::new();
        let h = other_tool_convs(m.iter().copied(), &seen, "cnv_a").unwrap();
        assert!(h.contains("cnv_b") && h.contains("-c"));
        assert!(other_tool_convs(m.iter().copied(), &seen, "cnv_b").is_none());
    }

    #[test]
    fn aux_hints() {
        let e = json!({"type": "message.received", "text": "Generate a title for this conversation: hi"});
        assert!(aux_hint(&e).unwrap().contains("short title"));
        let e =
            json!({"type": "message.received", "text": "x", "expected_format": "a JSON object"});
        assert_eq!(aux_hint(&e).unwrap(), "answer with a JSON object");
        let e = json!({"type": "message.received", "text": "hello", "kind": "chat"});
        assert!(aux_hint(&e).is_none());
    }

    #[test]
    fn diff_lines() {
        assert_eq!(line_diff("a\nb", "a\nc"), "- b\n+ c");
    }
}
