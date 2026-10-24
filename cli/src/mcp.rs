//! `oh-seat mcp`: the same seat as an MCP server over stdio.
//!
//! For agents whose harness speaks MCP but runs shell commands poorly (or
//! not at all). Every tool call runs this very binary as the matching CLI
//! command, so the behaviour, state file and output are exactly the CLI's;
//! this file only translates JSON-RPC to arguments and back.
//!
//! Waiting: many MCP clients abort a tool call after ~60 s, so `next`,
//! `call_tool` and `result` wait at most OH_SEAT_MCP_WAIT seconds (default
//! 40), never longer than OH_SEAT_MCP_WAIT_MAX (default 50, or
//! OH_SEAT_MCP_WAIT if larger) even when the call asks: Cline aborts at a
//! hard 60 s, and a `next` the client gave up on could swallow a message.
//! They say plainly "nothing yet, call again".

use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PROTOCOLS: [&str; 5] = [
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
    "2026-07-28",
];

const INSTRUCTIONS: &str = "\
You sit in the Omni2API responder seat: callers (Claude Code, Codex, any OpenAI/Anthropic client) talk to you as their model.
Loop: `next` (waits for a message) -> optionally `call_tool` (the CALLER runs one of its tools, you get the output) -> `reply` (answers and ends the turn) -> `next` again.
`next` returning 'nothing yet' is normal: call it again immediately, for as long as you are on duty. Never end your turn while on duty: an agent that stops calling `next` leaves the seat unmanned.
First time: call `name` to see the model name callers use, and set your own unless the user chose one.
Hints in tool output written as `oh-seat X` / `X` refer to the tool of the same name here (`oh-seat tool` = `call_tool`, `oh-seat use` = `use_conv`).
`guide` has the full manual.";

fn wait_default() -> u64 {
    std::env::var("OH_SEAT_MCP_WAIT")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|v: &u64| *v > 0)
        .unwrap_or(40)
}

fn wait_max() -> u64 {
    std::env::var("OH_SEAT_MCP_WAIT_MAX")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|v: &u64| *v > 0)
        .unwrap_or(50)
        .max(wait_default())
}

fn tools() -> Value {
    // Fixed text: hosts like Reasonix pin tool definitions and hold back
    // any that change, so nothing here may depend on env or version.
    let wait = json!({"type": "integer", "minimum": 0,
        "description": "Seconds to wait (default 40; capped at 50 so MCP client timeouts never cut a wait short); 0 = check once."});
    let conv = json!({"type": "string", "description": "Conversation: any unique piece of its id (default: the active one)"});
    json!([
        {"name": "next",
         "description": "Wait for the next caller message and show it (it becomes the active conversation). 'Nothing yet' just means call `next` again.",
         "inputSchema": {"type": "object", "properties": {
            "wait": wait,
            "all": {"type": "boolean", "description": "Return on any event, not only messages"}}}},
        {"name": "reply",
         "description": "Answer the caller. Ends the turn unless more=true. Then call `next`.",
         "inputSchema": {"type": "object", "required": ["text"], "properties": {
            "text": {"type": "string", "description": "The answer, exactly as the caller should see it"},
            "conv": conv,
            "reasoning": {"type": "string", "description": "Thinking shown before the answer"},
            "more": {"type": "boolean", "description": "Keep the turn open for another reply"},
            "force": {"type": "boolean", "description": "Reply although a tool result here is still unread"}}}},
        {"name": "call_tool",
         "description": "Have the CALLER run one of its own tools on its machine (see `caps` for what it has) and get the result. Several independent calls at once: `calls`. Input is checked against the tool's schema first.",
         "inputSchema": {"type": "object", "properties": {
            "name": {"type": "string", "description": "The caller's tool name (case-insensitive), e.g. Bash"},
            "input": {"type": "object", "description": "The tool's arguments, e.g. {\"command\": \"ls\"}", "additionalProperties": true},
            "calls": {"type": "array", "description": "Instead of name/input: several calls run in parallel, results in order",
                "items": {"type": "object", "required": ["name"], "properties": {
                    "name": {"type": "string"},
                    "input": {"type": "object", "additionalProperties": true}}}},
            "conv": conv,
            "text": {"type": "string", "description": "Words shown alongside the call"},
            "reasoning": {"type": "string", "description": "Thinking shown before the call"},
            "wait": wait,
            "force": {"type": "boolean", "description": "Send even if the input does not match the schema"}}}},
        {"name": "result",
         "description": "Wait for the result of an earlier call_tool that was still running (default: the latest pending one).",
         "inputSchema": {"type": "object", "properties": {
            "id": {"type": "string", "description": "Invocation id or a unique piece of it"},
            "wait": wait}}},
        {"name": "caps",
         "description": "The caller's tools as signatures (and system prompt with full=true); tool=X for one tool's parameters.",
         "inputSchema": {"type": "object", "properties": {
            "conv": conv,
            "tool": {"type": "string"},
            "full": {"type": "boolean"},
            "diff": {"type": "boolean", "description": "Only what changed since the last caps"}}}},
        {"name": "history",
         "description": "Recent messages of a conversation.",
         "inputSchema": {"type": "object", "properties": {
            "conv": conv,
            "limit": {"type": "integer", "minimum": 1}}}},
        {"name": "convs",
         "description": "Conversations seen so far, newest first (* = active).",
         "inputSchema": {"type": "object", "properties": {}}},
        {"name": "use_conv",
         "description": "Make a conversation the active one.",
         "inputSchema": {"type": "object", "required": ["conv"], "properties": {"conv": conv}}},
        {"name": "name",
         "description": "The model name callers use: show it, or set your own with name=... (a name the user pinned cannot change).",
         "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}}}},
        {"name": "status",
         "description": "Server reachability, open turns (callers waiting on you), pending tool calls.",
         "inputSchema": {"type": "object", "properties": {}}},
        {"name": "guide",
         "description": "The full seat manual (CLI wording; every command has a tool here).",
         "inputSchema": {"type": "object", "properties": {}}}
    ])
}

fn s(a: &Value, k: &str) -> Option<String> {
    match &a[k] {
        Value::String(v) => Some(v.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}
fn b(a: &Value, k: &str) -> bool {
    a[k].as_bool() == Some(true) || a[k].as_str() == Some("true")
}
fn wait_of(a: &Value) -> u64 {
    a["wait"]
        .as_u64()
        .or_else(|| a["wait"].as_str().and_then(|v| v.trim().parse().ok()))
        .unwrap_or_else(wait_default)
        .min(wait_max())
}

/// The CLI arguments (and stdin) for one MCP tool call.
fn to_cli(name: &str, a: &Value) -> Result<(Vec<String>, Option<String>, u64), String> {
    let mut v: Vec<String> = Vec::new();
    let mut stdin = None;
    let mut wait = 0;
    let push_opt = |v: &mut Vec<String>, flag: &str, val: Option<String>| {
        if let Some(x) = val {
            v.push(flag.into());
            v.push(x);
        }
    };
    match name {
        "next" => {
            wait = wait_of(a);
            v.extend(["next".into(), "--timeout".into(), wait.to_string()]);
            if b(a, "all") {
                v.push("--all".into());
            }
        }
        "reply" => {
            let text = s(a, "text").ok_or("reply needs text")?;
            v.extend(["reply".into(), "-".into()]);
            stdin = Some(text);
            push_opt(&mut v, "-c", s(a, "conv"));
            push_opt(&mut v, "-r", s(a, "reasoning"));
            if b(a, "more") {
                v.push("--more".into());
            }
            if b(a, "force") {
                v.push("--force".into());
            }
        }
        "call_tool" => {
            wait = wait_of(a);
            let mut calls: Vec<(String, Value)> = Vec::new();
            if let Some(arr) = a["calls"].as_array() {
                for c in arr {
                    let n = s(c, "name").ok_or("each entry of calls needs a name")?;
                    calls.push((n, input_obj(&c["input"])?));
                }
            }
            if let Some(n) = s(a, "name") {
                calls.insert(0, (n, input_obj(&a["input"])?));
            }
            if calls.is_empty() {
                return Err("call_tool needs name (+ input) or calls".into());
            }
            v.push("tool".into());
            for (i, (n, inp)) in calls.into_iter().enumerate() {
                if i > 0 {
                    v.push("+".into());
                }
                v.push(n);
                v.push(inp.to_string());
            }
            v.extend(["--timeout".into(), wait.to_string()]);
            push_opt(&mut v, "-c", s(a, "conv"));
            push_opt(&mut v, "--text", s(a, "text"));
            push_opt(&mut v, "-r", s(a, "reasoning"));
            if b(a, "force") {
                v.push("--force".into());
            }
        }
        "result" => {
            wait = wait_of(a);
            v.push("result".into());
            if let Some(id) = s(a, "id") {
                v.push(id);
            }
            v.extend(["--timeout".into(), wait.to_string()]);
        }
        "caps" => {
            v.push("caps".into());
            push_opt(&mut v, "-c", s(a, "conv"));
            push_opt(&mut v, "--tool", s(a, "tool"));
            if b(a, "full") {
                v.push("--full".into());
            }
            if b(a, "diff") {
                v.push("--diff".into());
            }
        }
        "history" => {
            v.push("history".into());
            push_opt(&mut v, "-c", s(a, "conv"));
            push_opt(&mut v, "--limit", s(a, "limit"));
        }
        "convs" => v.push("convs".into()),
        "use_conv" => {
            v.push("use".into());
            v.push(s(a, "conv").ok_or("use_conv needs conv")?);
        }
        "name" => {
            v.push("name".into());
            if let Some(n) = s(a, "name") {
                v.push(n);
            }
        }
        "status" => v.push("status".into()),
        "guide" => v.push("guide".into()),
        other => return Err(format!("unknown tool {other:?}")),
    }
    Ok((v, stdin, wait))
}

/// Tool input as a JSON object; a JSON string holding an object is
/// accepted too (some models stringify nested objects).
fn input_obj(v: &Value) -> Result<Value, String> {
    match v {
        Value::Null => Ok(json!({})),
        Value::Object(_) => Ok(v.clone()),
        Value::String(t) => match serde_json::from_str::<Value>(t) {
            Ok(o @ Value::Object(_)) => Ok(o),
            _ => Err(format!("input must be a JSON object, got the string {t:?}")),
        },
        other => Err(format!("input must be a JSON object, got {other}")),
    }
}

/// Replace `prefix WORD` (WORD = the next run of id characters) by
/// `fmt` with `{}` standing for WORD.
fn rewrite_word(t: &str, prefix: &str, fmt: &str) -> String {
    let mut out = String::new();
    let mut rest = t;
    while let Some(i) = rest.find(prefix) {
        out.push_str(&rest[..i]);
        let after = &rest[i + prefix.len()..];
        let n = after
            .find(|c: char| !(c.is_alphanumeric() || "_-.:".contains(c)))
            .unwrap_or(after.len());
        let word = after[..n].trim_end_matches(['.', ':']);
        out.push_str(&fmt.replace("{}", word));
        rest = &after[word.len()..];
        if prefix.starts_with('`') {
            rest = rest.strip_prefix('`').unwrap_or(rest);
        }
    }
    out.push_str(rest);
    out
}

/// CLI hints name CLI commands and flags; here they are tools and fields.
fn mcpify(t: &str) -> String {
    let t = t
        .replace("`oh-seat caps --full`", "`caps` full=true")
        .replace("oh-seat caps --full", "`caps` full=true")
        .replace("--force replies anyway", "force=true replies anyway")
        .replace("--force sends anyway", "force=true sends anyway")
        .replace("run `oh-seat next` again", "call `next` again")
        .replace("run `next` again", "call `next` again");
    let t = rewrite_word(&t, "`oh-seat caps --tool ", "`caps` tool={}");
    let t = rewrite_word(&t, "`oh-seat result ", "`result` id={}");
    let t = rewrite_word(&t, "oh-seat result ", "`result` id={}");
    let t = rewrite_word(&t, "oh-seat reply -c ", "`reply` conv={}");
    t.replace("`oh-seat tool", "`call_tool")
        .replace("`oh-seat use", "`use_conv")
        .replace("`oh-seat ", "`")
        .replace("`` ", "` ")
}

type Out = Arc<Mutex<std::io::Stdout>>;
type Running = Arc<Mutex<HashMap<String, Child>>>;

fn send(out: &Out, v: &Value) {
    if let Ok(mut o) = out.lock() {
        let _ = writeln!(o, "{v}");
        let _ = o.flush();
    }
}

fn run_tool(
    name: &str,
    args: &Value,
    id_key: &str,
    token: Option<Value>,
    out: &Out,
    running: &Running,
) -> Value {
    let (cli, stdin, wait) = match to_cli(name, args) {
        Ok(x) => x,
        Err(e) => return json!({"content": [{"type": "text", "text": e}], "isError": true}),
    };
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            return json!({"content": [{"type": "text", "text": e.to_string()}], "isError": true})
        }
    };
    let mut cmd = Command::new(exe);
    cmd.args(&cli)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return json!({"content": [{"type": "text", "text": e.to_string()}], "isError": true})
        }
    };
    if let (Some(t), Some(mut si)) = (stdin, child.stdin.take()) {
        let _ = si.write_all(t.as_bytes());
    }
    let mut so = child.stdout.take();
    let mut se = child.stderr.take();
    let t_out = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(o) = so.as_mut() {
            let _ = o.read_to_string(&mut s);
        }
        s
    });
    let t_err = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(o) = se.as_mut() {
            let _ = o.read_to_string(&mut s);
        }
        s
    });
    if let Ok(mut r) = running.lock() {
        r.insert(id_key.to_string(), child);
    }
    let start = Instant::now();
    let mut last_progress = Instant::now();
    let code = loop {
        std::thread::sleep(Duration::from_millis(100));
        let st = {
            let mut r = running.lock().unwrap();
            match r.get_mut(id_key) {
                Some(c) => c.try_wait().ok().flatten(),
                None => break Err(()), // cancelled
            }
        };
        if let Some(st) = st {
            running.lock().unwrap().remove(id_key);
            break Ok(st.code());
        }
        if let Some(tok) = &token {
            if last_progress.elapsed() >= Duration::from_secs(10) {
                last_progress = Instant::now();
                let el = start.elapsed().as_secs();
                send(
                    out,
                    &json!({"jsonrpc": "2.0", "method": "notifications/progress",
                    "params": {"progressToken": tok, "progress": el, "total": wait.max(el + 1),
                               "message": format!("waiting ({el}s)")}}),
                );
            }
        }
    };
    let Ok(code) = code else {
        return json!({"_cancelled": true});
    };
    let stdout = t_out.join().unwrap_or_default();
    let stderr = t_err.join().unwrap_or_default();
    let mut text = stdout.trim_end().to_string();
    let err = stderr.trim();
    if !err.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(err);
    }
    let text = mcpify(&text);
    let (note, is_error) = match code {
        Some(0) => ("", false),
        Some(1) => ("\n[the caller's tool reported an error]", false),
        Some(2) => match name {
            "next" => ("", false),
            _ => ("\n[not back yet: call `result` to keep waiting]", false),
        },
        Some(3) => ("\n[network / auth / server error]", true),
        Some(4) => ("\n[usage error: nothing was sent]", true),
        Some(5) => (
            "\n[no open turn: the caller is not waiting; call `next`]",
            true,
        ),
        None => ("\n[oh-seat was killed]", true),
        Some(_) => ("", true),
    };
    let mut text = format!("{text}{note}");
    if text.trim().is_empty() {
        text = "ok".into();
    }
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

pub fn serve() -> u8 {
    let out: Out = Arc::new(Mutex::new(std::io::stdout()));
    let running: Running = Arc::new(Mutex::new(HashMap::new()));
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                send(
                    &out,
                    &json!({"jsonrpc": "2.0", "id": null,
                    "error": {"code": -32700, "message": format!("parse error: {e}")}}),
                );
                continue;
            }
        };
        // A batch (older protocol versions): handle each element.
        let msgs = match msg {
            Value::Array(a) => a,
            m => vec![m],
        };
        for m in msgs {
            handle(m, &out, &running);
        }
    }
    // stdin closed: the client is gone; stop whatever still waits.
    if let Ok(mut r) = running.lock() {
        for (_, mut c) in r.drain() {
            let _ = c.kill();
        }
    }
    0
}

fn handle(m: Value, out: &Out, running: &Running) {
    let method = m["method"].as_str().unwrap_or("").to_string();
    let id = m.get("id").cloned();
    let Some(id) = id.filter(|i| !i.is_null()) else {
        if method == "notifications/cancelled" {
            let key = m["params"]["requestId"].to_string();
            if let Ok(mut r) = running.lock() {
                if let Some(mut c) = r.remove(&key) {
                    let _ = c.kill();
                }
            }
        }
        return; // other notifications (initialized, …) and responses need nothing
    };
    let reply = |result: Result<Value, (i64, String)>| match result {
        Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
        Err((c, e)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": c, "message": e}}),
    };
    match method.as_str() {
        "initialize" => {
            let asked = m["params"]["protocolVersion"].as_str().unwrap_or("");
            let v = if PROTOCOLS.contains(&asked) {
                asked
            } else {
                PROTOCOLS[1]
            };
            send(
                out,
                &reply(Ok(json!({
                    "protocolVersion": v,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "oh-seat", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": INSTRUCTIONS,
                }))),
            );
        }
        "ping" => send(out, &reply(Ok(json!({})))),
        "tools/list" => send(out, &reply(Ok(json!({"tools": tools()})))),
        "resources/list" => send(out, &reply(Ok(json!({"resources": []})))),
        "prompts/list" => send(out, &reply(Ok(json!({"prompts": []})))),
        "tools/call" => {
            let name = m["params"]["name"].as_str().unwrap_or("").to_string();
            let args = m["params"]["arguments"].clone();
            let args = if args.is_null() { json!({}) } else { args };
            let token = m["params"]["_meta"]["progressToken"].clone();
            let token = (!token.is_null()).then_some(token);
            let (out, running) = (out.clone(), running.clone());
            let key = id.to_string();
            // Calls run concurrently: a long `next` must not block `status`.
            std::thread::spawn(move || {
                let r = run_tool(&name, &args, &key, token, &out, &running);
                // A cancelled request gets no response (MCP cancellation).
                if r["_cancelled"].as_bool() != Some(true) {
                    send(&out, &json!({"jsonrpc": "2.0", "id": id, "result": r}));
                }
            });
        }
        _ => send(
            out,
            &reply(Err((-32601, format!("method not found: {method}")))),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_is_capped_below_client_timeouts() {
        assert_eq!(wait_of(&json!({"wait": 100})), 50);
        assert_eq!(wait_of(&json!({"wait": "5"})), 5);
        assert_eq!(wait_of(&json!({})), 40);
    }

    #[test]
    fn maps_calls_to_cli() {
        let (v, stdin, _) = to_cli("reply", &json!({"text": "hi", "more": true})).unwrap();
        assert_eq!(v, ["reply", "-", "--more"]);
        assert_eq!(stdin.as_deref(), Some("hi"));
        let (v, _, w) = to_cli(
            "call_tool",
            &json!({"calls": [{"name": "Read", "input": {"path": "a"}}, {"name": "Bash", "input": "{\"command\":\"ls\"}"}], "wait": 5}),
        )
        .unwrap();
        assert_eq!(w, 5);
        assert_eq!(
            v,
            [
                "tool",
                "Read",
                "{\"path\":\"a\"}",
                "+",
                "Bash",
                "{\"command\":\"ls\"}",
                "--timeout",
                "5"
            ]
        );
        assert!(to_cli("call_tool", &json!({})).is_err());
        assert!(to_cli("call_tool", &json!({"name": "X", "input": 3})).is_err());
    }

    #[test]
    fn hints_name_tools() {
        assert_eq!(
            mcpify("run `oh-seat next` again; `oh-seat tool Bash`"),
            "call `next` again; `call_tool Bash`"
        );
        assert_eq!(
            mcpify("read it with `oh-seat result call_01ab`, then reply (--force replies anyway)"),
            "read it with `result` id=call_01ab, then reply (force=true replies anyway)"
        );
        assert_eq!(
            mcpify("(`oh-seat caps --tool Bash` explains)"),
            "(`caps` tool=Bash explains)"
        );
        assert_eq!(
            mcpify("waiting: oh-seat result a b"),
            "waiting: `result` id=a b"
        );
    }
}
