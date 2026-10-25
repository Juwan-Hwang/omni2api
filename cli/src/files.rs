//! Files between the seat and the caller, through the caller's own tools.
//!
//! Inbound attachments (a file part, a PDF from Read) arrive as named data
//! URLs and are saved under `$OMNI2API_HOME/files/<conv>/`. There is no
//! pull: the seat works on the caller's files in place with the caller's
//! tools. `push` writes a file onto the caller's machine (its write tool for
//! text, shell base64 otherwise), sha256 checked.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::{
    await_result, data_url_bytes, omni2api_home, rand_hex, s, tool_name, tool_schema, Ctx, Fail,
    HTTP, OK, R, USAGE,
};

/// A data URL's `name=` parameter, decoded.
pub fn data_url_name(url: &str) -> Option<String> {
    let meta = url.strip_prefix("data:")?.split(',').next()?;
    let raw = meta.split(';').find_map(|p| p.strip_prefix("name="))?;
    let b = raw.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                match std::str::from_utf8(&b[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                {
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
    String::from_utf8(out).ok().filter(|n| !n.trim().is_empty())
}

/// A file name safe to create: no directories, no leading dots, no
/// characters Windows refuses.
pub fn safe_name(n: &str) -> String {
    let base = n.rsplit(['/', '\\']).next().unwrap_or(n);
    let mut o: String = base
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    o = o.trim().trim_start_matches('.').to_string();
    if o.is_empty() {
        o = "file".into();
    }
    o
}

fn ext_for(mime: &str) -> &str {
    match mime {
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        "text/markdown" => "md",
        "text/csv" => "csv",
        "application/json" => "json",
        "application/zip" => "zip",
        m => m.rsplit('/').next().unwrap_or("bin"),
    }
}

pub fn conv_dir(conv: &str) -> PathBuf {
    omni2api_home()
        .join("files")
        .join(safe_name(conv.trim_start_matches("cnv_")))
}

/// Write `bytes` as `name` in `dir`; an identical file already there is
/// reused, a different one gets `-2`, `-3`… before the extension.
pub fn write_unique(dir: &Path, name: &str, bytes: &[u8]) -> Option<PathBuf> {
    std::fs::create_dir_all(dir).ok()?;
    let name = safe_name(name);
    let (stem, ext) = match name.rsplit_once('.') {
        Some((a, b)) if !a.is_empty() => (a.to_string(), format!(".{b}")),
        _ => (name.clone(), String::new()),
    };
    for i in 1..1000 {
        let p = if i == 1 {
            dir.join(&name)
        } else {
            dir.join(format!("{stem}-{i}{ext}"))
        };
        match std::fs::read(&p) {
            Ok(old) if old == bytes => return Some(p),
            Ok(_) => continue,
            Err(_) => return std::fs::write(&p, bytes).ok().map(|_| p),
        }
    }
    None
}

pub struct Saved {
    pub path: String,
    pub mime: String,
    pub size: usize,
    pub image: bool,
}

/// Save one attachment URL: pictures under images/ (as before), other files
/// under files/<conv>/ by their own name. A link is kept as a link.
pub fn save_url(url: &str, conv: &str, stem: &str, i: usize) -> Option<Saved> {
    let Some((mime, bytes)) = data_url_bytes(url) else {
        return (!url.is_empty()).then(|| Saved {
            path: url.to_string(),
            mime: String::new(),
            size: 0,
            image: !url.to_lowercase().ends_with(".pdf"),
        });
    };
    let image = mime.starts_with("image/");
    let path = if image {
        let dir = omni2api_home().join("images");
        let _ = std::fs::create_dir_all(&dir);
        let ext = ext_for(&mime).replace("jpeg", "jpg");
        let f = dir.join(format!("{stem}-{i}.{ext}"));
        std::fs::write(&f, &bytes).ok()?;
        f
    } else {
        let name = data_url_name(url).unwrap_or_else(|| format!("{stem}-{i}.{}", ext_for(&mime)));
        write_unique(&conv_dir(conv), &name, &bytes)?
    };
    Some(Saved {
        path: path.display().to_string(),
        mime,
        size: bytes.len(),
        image,
    })
}

pub fn describe(f: &Saved) -> String {
    if f.size == 0 {
        return f.path.clone();
    }
    format!("{}  ({}, {})", f.path, f.mime, human(f.size as u64))
}

pub fn human(n: u64) -> String {
    match n {
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / (1u64 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.1} KB", n as f64 / 1024.0),
        n => format!("{n} bytes"),
    }
}

pub fn sha256_hex(b: &[u8]) -> String {
    let d = Sha256::digest(b);
    d.iter().map(|x| format!("{x:02x}")).collect()
}

// ── the caller's tools ──

#[derive(Clone, Copy, PartialEq)]
enum Shell {
    Posix,
    Power,
}

struct ShellTool {
    name: String,
    param: String,
    kind: Shell,
}

const SHELL_NAMES: &[&str] = &[
    "bash",
    "shell",
    "exec_command",
    "run_terminal_command",
    "run_shell_command",
    "execute_command",
    "runcommand",
    "run_command",
    "run_commands",
    "terminal",
    "pwsh",
    "powershell",
];

fn string_params(t: &Value) -> Vec<String> {
    tool_schema(t)
        .get("properties")
        .and_then(|p| p.as_object())
        .map(|o| {
            o.iter()
                .filter(|(_, v)| v.get("type").and_then(|t| t.as_str()) == Some("string"))
                .map(|(k, _)| k.clone())
                .collect()
        })
        .unwrap_or_default()
}

fn find_shell(tools: &[Value], caps: &str) -> Option<ShellTool> {
    for want in SHELL_NAMES {
        let Some(t) = tools.iter().find(|t| tool_name(t).to_lowercase() == *want) else {
            continue;
        };
        let ps = string_params(t);
        let param = ["command", "cmd", "script", "commands"]
            .iter()
            .find(|p| ps.iter().any(|x| x == *p))?
            .to_string();
        let name = tool_name(t);
        let lower = name.to_lowercase();
        let windows = caps.contains("platform=win") || caps.contains("platform=Windows");
        let kind = if lower.contains("pwsh") || lower.contains("powershell") || windows {
            Shell::Power
        } else {
            Shell::Posix
        };
        return Some(ShellTool { name, param, kind });
    }
    None
}

struct WriteTool {
    name: String,
    path: String,
    content: String,
}

fn find_write(tools: &[Value]) -> Option<WriteTool> {
    for want in [
        "write",
        "write_file",
        "write_to_file",
        "create",
        "writefile",
    ] {
        let Some(t) = tools.iter().find(|t| tool_name(t).to_lowercase() == want) else {
            continue;
        };
        let ps = string_params(t);
        let path = ["file_path", "path", "filePath", "filepath", "target_file"]
            .iter()
            .find(|p| ps.iter().any(|x| x == *p))?;
        let content = ["content", "contents", "file_text", "text"]
            .iter()
            .find(|p| ps.iter().any(|x| x == *p))?;
        return Some(WriteTool {
            name: tool_name(t),
            path: path.to_string(),
            content: content.to_string(),
        });
    }
    None
}

/// Tools some callers have for handing a file to their person.
fn present_tool(tools: &[Value]) -> Option<String> {
    [
        "present_files",
        "dsh_im_return_file",
        "OpenPreview",
        "present",
    ]
    .iter()
    .find_map(|w| {
        tools
            .iter()
            .map(tool_name)
            .find(|n| n.eq_ignore_ascii_case(w))
    })
}

/// Send calls at once (one assistant message, parallel tool calls) and wait
/// for every result. Each result's output text, in order.
fn run_calls(cx: &mut Ctx, c: &str, calls: Vec<(String, Value)>, timeout: u64) -> R<Vec<Value>> {
    let bodies: Vec<Value> = calls
        .iter()
        .map(|(t, i)| {
            json!({"conversation_id": c, "tool": t, "input": i, "wait": 0,
                   "client_msg_id": format!("ohs_{}", rand_hex(24))})
        })
        .collect();
    let cxr: &Ctx = cx;
    let sent: Vec<R<Value>> = std::thread::scope(|sc| {
        let hs: Vec<_> = bodies
            .iter()
            .enumerate()
            .map(|(i, b)| {
                sc.spawn(move || {
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
    });
    let mut invs = Vec::new();
    for (r, (t, _)) in sent.into_iter().zip(&calls) {
        let r = r?;
        let inv = s(&r, "invocation_id")
            .ok_or_else(|| Fail(HTTP, format!("server returned no invocation id: {r}")))?
            .to_string();
        cx.st.pending.push([inv.clone(), c.to_string(), t.clone()]);
        invs.push(inv);
    }
    cx.save();
    let deadline = Instant::now() + Duration::from_secs(timeout);
    let mut out = Vec::new();
    for inv in invs {
        match await_result(cx, &inv, deadline)? {
            Some(e) => out.push(e),
            None => {
                return Err(Fail(
                    crate::NOTHING,
                    format!(
                        "the caller did not answer in {timeout}s (or a message arrived; run `oh-seat next`): result {inv} is still pending"
                    ),
                ))
            }
        }
    }
    Ok(out)
}

fn output_of(e: &Value) -> String {
    s(e, "output")
        .map(String::from)
        .or_else(|| e.get("error").map(|v| v.to_string()))
        .unwrap_or_default()
}

/// The text between our markers, harness headers and footers dropped.
fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn psq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn remote_abs(cx: &Ctx, c: &str, p: &str, kind: Shell) -> String {
    let abs = p.starts_with('/')
        || p.starts_with('~')
        || p.starts_with('\\')
        || p.get(1..3).is_some_and(|x| x == ":\\" || x == ":/");
    if abs {
        return p.to_string();
    }
    match crate::caller_cwd(cx, c) {
        Some(cwd) if kind == Shell::Power || cwd.contains('\\') => {
            format!(
                "{}\\{}",
                cwd.trim_end_matches(['\\', '/']),
                p.replace('/', "\\")
            )
        }
        Some(cwd) => format!("{}/{}", cwd.trim_end_matches('/'), p),
        None => p.to_string(),
    }
}

fn b64(bytes: &[u8]) -> String {
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

#[cfg(test)]
fn unb64(s: &str) -> Option<Vec<u8>> {
    data_url_bytes(&format!("data:x;base64,{s}")).map(|(_, b)| b)
}

fn no_shell(c: &str, tools: &[Value], what: &str) -> Fail {
    Fail(
        USAGE,
        format!(
            "the caller in {c} has no tool that can {what} (its tools: {}); file transfer only uses the caller's own tools, so it is not available here",
            tools.iter().map(tool_name).collect::<Vec<_>>().join(", ")
        ),
    )
}

// ── push ──

pub fn cmd_push(
    cx: &mut Ctx,
    local: String,
    remote: Option<String>,
    conv: Option<String>,
    chunk: usize,
    max: u64,
    timeout: u64,
) -> R<u8> {
    let c = cx.conv(conv)?;
    let data = std::fs::read(&local).map_err(|e| Fail(USAGE, format!("{local}: {e}")))?;
    if data.len() as u64 > max {
        return Err(Fail(
            USAGE,
            format!(
                "{local} is {} — over --max {}: every chunk stays in the caller's conversation. Raise --max if you mean it",
                human(data.len() as u64),
                human(max)
            ),
        ));
    }
    let tools = cx.tools_for(&c, false);
    let caps = cx.st.caps.get(&c).cloned().unwrap_or_default();
    let sh = find_shell(&tools, &caps);
    let kind = sh.as_ref().map(|s| s.kind).unwrap_or(Shell::Posix);
    let name = Path::new(&local)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".into());
    let remote = remote_abs(cx, &c, remote.as_deref().unwrap_or(&name), kind);
    let remote = if remote.ends_with('/') || remote.ends_with('\\') {
        format!("{remote}{name}")
    } else {
        remote
    };
    let want = sha256_hex(&data);
    let text = std::str::from_utf8(&data)
        .ok()
        .filter(|t| !t.contains('\0'))
        .map(String::from);
    let wt = find_write(&tools);
    let mut calls = 0usize;
    let how;
    if let (Some(t), Some(w)) = (&text, &wt) {
        let r = run_calls(
            cx,
            &c,
            vec![(w.name.clone(), json!({ &w.path: remote, &w.content: t }))],
            timeout,
        )?;
        calls += 1;
        if crate::result_exit(&r[0]) != OK {
            return Err(Fail(
                crate::TOOL_ERR,
                format!("{} refused: {}", w.name, output_of(&r[0]).trim()),
            ));
        }
        how = "write tool";
    } else {
        let sh = sh
            .as_ref()
            .ok_or_else(|| no_shell(&c, &tools, "write this binary file"))?;
        let enc = b64(&data);
        let step = chunk.clamp(1024, 1 << 20) * 4 / 3 / 4 * 4;
        let pieces: Vec<&str> = if enc.is_empty() {
            vec![""]
        } else {
            enc.as_bytes()
                .chunks(step)
                .map(|b| std::str::from_utf8(b).unwrap())
                .collect()
        };
        let tmp = format!("{remote}.ohpart");
        // Chunks go one per call, in order: each appends to the part file.
        for (i, p) in pieces.iter().enumerate() {
            let redirect = if i == 0 { ">" } else { ">>" };
            let cmd = match sh.kind {
                Shell::Posix => format!(
                    "mkdir -p \"$(dirname {q})\" && printf %s {b} | base64 -d {redirect} {t} && echo OHOK",
                    q = sq(&remote),
                    b = sq(p),
                    t = sq(&tmp)
                ),
                Shell::Power => format!(
                    "$b=[Convert]::FromBase64String({b}); $f=New-Object IO.FileStream({t}, {mode}); $f.Write($b,0,$b.Length); $f.Close(); 'OHOK'",
                    b = psq(p),
                    t = psq(&tmp),
                    mode = if i == 0 { "[IO.FileMode]::Create" } else { "[IO.FileMode]::Append" }
                ),
            };
            let r = run_calls(
                cx,
                &c,
                vec![(sh.name.clone(), json!({ &sh.param: cmd }))],
                timeout,
            )?;
            calls += 1;
            if !output_of(&r[0]).contains("OHOK") {
                return Err(Fail(
                    crate::TOOL_ERR,
                    format!(
                        "chunk {}/{} failed on the caller: {}",
                        i + 1,
                        pieces.len(),
                        output_of(&r[0]).chars().take(300).collect::<String>()
                    ),
                ));
            }
        }
        let fin = match sh.kind {
            Shell::Posix => format!(
                "mv -f {t} {q} && printf 'OHS:' && (sha256sum {q} 2>/dev/null || shasum -a 256 {q}) | cut -c1-64",
                t = sq(&tmp),
                q = sq(&remote)
            ),
            Shell::Power => format!(
                "Move-Item -Force -LiteralPath {t} {q}; 'OHS:' + (Get-FileHash -Algorithm SHA256 -LiteralPath {q}).Hash.ToLower()",
                t = psq(&tmp),
                q = psq(&remote)
            ),
        };
        let r = run_calls(
            cx,
            &c,
            vec![(sh.name.clone(), json!({ &sh.param: fin }))],
            timeout,
        )?;
        calls += 1;
        let o = output_of(&r[0]);
        let got = o
            .find("OHS:")
            .map(|i| {
                o[i + 4..]
                    .chars()
                    .take_while(|c| c.is_ascii_hexdigit())
                    .collect::<String>()
            })
            .unwrap_or_default();
        if got != want {
            return Err(Fail(
                crate::TOOL_ERR,
                format!(
                    "{remote}: the caller reports sha256 {got:?}, expected {want}: {}",
                    o.trim()
                ),
            ));
        }
        how = "shell, sha256 ok";
    }
    let hint = present_tool(&tools)
        .map(|t| format!("\n(to hand it to the person too: `oh-seat tool {t} ...` with this path)"))
        .unwrap_or_default();
    if cx.json {
        cx.out(&json!({"pushed": local, "remote": remote, "bytes": data.len(), "sha256": want, "calls": calls, "via": how}));
    } else {
        println!(
            "pushed {local} -> {remote}  ({}, {how}, {calls} call(s)){hint}",
            human(data.len() as u64)
        );
    }
    Ok(OK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(
            data_url_name("data:application/pdf;name=a+b%2F.pdf;base64,x").as_deref(),
            Some("a b/.pdf")
        );
        assert_eq!(safe_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_name("..hidden"), "hidden");
        assert_eq!(safe_name("a:b?.txt"), "a_b_.txt");
    }

    #[test]
    fn b64_round_trip() {
        for n in 0..20 {
            let v: Vec<u8> = (0..n).map(|i| (i * 37 + 3) as u8).collect();
            assert_eq!(unb64(&b64(&v)).unwrap(), v);
        }
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn unique_names() {
        let d = std::env::temp_dir().join(format!("ohs-files-{}", rand_hex(6)));
        let a = write_unique(&d, "r.txt", b"1").unwrap();
        assert_eq!(write_unique(&d, "r.txt", b"1").unwrap(), a);
        let b = write_unique(&d, "r.txt", b"2").unwrap();
        assert!(b.ends_with("r-2.txt"));
        let _ = std::fs::remove_dir_all(d);
    }
}
