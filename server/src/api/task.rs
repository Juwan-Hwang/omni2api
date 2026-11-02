//! API tasks: the non-chat endpoints (images, audio, ...) ride the chat
//! pipeline. Each request becomes one message to the seat, in a
//! conversation of its own, carrying a one-line answer contract (what to
//! send back and how); the seat answers with text and, for media, files
//! (`oh-seat reply -a FILE`). This module runs that round trip and hands the
//! answer back to the endpoint, which shapes it into its own response.

use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use futures::StreamExt;
use serde_json::{json, Value};

use crate::auth::AuthPrincipal;
use crate::storage::AppState;

/// What the seat sent back for a task.
pub struct Answer {
    /// The reply exactly as the seat sent it (a transcript keeps its links).
    pub raw: String,
    /// The reply text with attachment ids, data URLs and links removed.
    pub text: String,
    /// Files, in the order the seat gave them.
    pub files: Vec<Output>,
    /// Tool calls the seat made instead of (or with) text, chat-shaped.
    pub tool_calls: Vec<Value>,
}

pub enum Output {
    /// Bytes we hold (an attachment, or a data URL in the text).
    Bytes {
        mime: String,
        name: String,
        bytes: Vec<u8>,
    },
    /// A link the seat gave instead.
    Link(String),
}

/// An OpenAI-shaped error response.
pub fn error(status: StatusCode, kind: &str, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({"error": {"message": message, "type": kind, "code": code, "param": null}})),
    )
        .into_response()
}

/// Endpoints this server does not offer: OpenAI platform administration
/// (organization, projects, safety cases) and live phone-call sessions.
pub async fn not_offered(uri: axum::http::Uri) -> Response {
    error(
        StatusCode::NOT_IMPLEMENTED,
        "invalid_request_error",
        "not_implemented",
        &format!(
            "{} is not offered by this server: it manages an OpenAI organization or phone calls, which a seat has none of.",
            uri.path()
        ),
    )
}

pub fn invalid(message: &str) -> Response {
    error(
        StatusCode::BAD_REQUEST,
        "invalid_request_error",
        "invalid_request",
        message,
    )
}

/// How the endpoint wants the answer delivered.
pub enum Delivery {
    /// One JSON body. Its status is held back while the seat is quick, and
    /// once it is not, whitespace keeps the connection alive (a JSON parser
    /// skips it) and the status is 200 whatever follows.
    Json,
    /// One body that may be raw media (audio bytes, a plain-text
    /// transcript). Padding would commit JSON headers and a 200 status
    /// before the answer is known, corrupting the download, so these
    /// responses never pad: the request stays open (the inner task has its
    /// own timeout) and the answer always arrives with its real status and
    /// content type. Callers wanting liveness for long waits should use the
    /// endpoint's streaming mode instead.
    JsonRaw,
    /// Server-sent events from the start; `:` comments keep it alive.
    Sse,
}

/// The endpoint's turn: the seat's answer, or why there is none.
pub type Finish = Box<dyn FnOnce(Result<Answer, String>) -> Finished + Send>;

pub enum Finished {
    /// A JSON body and its status (the status only counts while it can
    /// still be sent).
    Json(StatusCode, Value),
    /// SSE frames, already formatted.
    Sse(Vec<String>),
    /// A raw body with its content type (audio bytes); like Json, the
    /// status only counts while it can still be sent.
    Raw(StatusCode, String, Vec<u8>),
}

/// How long the status may be held back before padding starts: long enough
/// that most answers, a refusal included, arrive with their real status,
/// short of the ~100 s a tunnel (Cloudflare) allows a silent response.
fn hold() -> Duration {
    Duration::from_secs(
        std::env::var("OMNI2API_TASK_HOLD_SECS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(85),
    )
}

/// Send `text` (and any attached pictures, as data URLs) to the seat as
/// an `api` task with answer contract `expected`, and deliver what
/// `finish` makes of the answer.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    app: AppState,
    principal: AuthPrincipal,
    mut headers: HeaderMap,
    api: &str,
    model: Option<&str>,
    text: String,
    images: Vec<String>,
    expected: String,
    wants_files: bool,
    spec: Value,
    delivery: Delivery,
    finish: Finish,
) -> Response {
    let content = if images.is_empty() {
        Value::String(text)
    } else {
        let mut parts = vec![json!({"type": "text", "text": text})];
        for u in images {
            parts.push(json!({"type": "image_url", "image_url": {"url": u}}));
        }
        Value::Array(parts)
    };
    let req = json!({
        "model": model,
        "messages": [{"role": "user", "content": content}],
        "stream": false,
        "metadata": {crate::api::openai::TASK_KEY: {"api": api, "expected": expected, "files": wants_files, "spec": spec}},
    });
    let req = match serde_json::from_value(req) {
        Ok(r) => r,
        Err(e) => return invalid(&e.to_string()),
    };
    // A task always waits for its answer here; async mode is a chat thing.
    headers.remove("x-omni2api-mode");
    let resp =
        crate::api::openai::chat_completions(State(app), principal, headers, Json(req)).await;
    if resp.status() != StatusCode::OK {
        return resp;
    }

    // Read the chat body to its end in the background: padding first, then
    // one JSON object.
    let (tx, rx) = tokio::sync::oneshot::channel::<Result<Answer, String>>();
    tokio::spawn(async move {
        let mut body = resp.into_body().into_data_stream();
        let mut buf = Vec::new();
        while let Some(chunk) = body.next().await {
            match chunk {
                Ok(b) => buf.extend_from_slice(&b),
                Err(e) => {
                    let _ = tx.send(Err(e.to_string()));
                    return;
                }
            }
        }
        let _ = tx.send(answer_from_chat(&buf).await);
    });

    match delivery {
        Delivery::JsonRaw => {
            let a = rx.await.unwrap_or_else(|_| Err("no answer".into()));
            finished_response(finish(a))
        }
        Delivery::Json => {
            let mut rx = rx;
            match tokio::time::timeout(hold(), &mut rx).await {
                Ok(a) => finished_response(finish(a.unwrap_or_else(|_| Err("no answer".into())))),
                Err(_) => {
                    let stream = async_stream::stream! {
                        let mut tick = tokio::time::interval(Duration::from_secs(5));
                        tick.tick().await;
                        let mut rx = rx;
                        loop {
                            tokio::select! {
                                _ = tick.tick() => yield Ok::<Bytes, std::convert::Infallible>(Bytes::from_static(b"\n")),
                                a = &mut rx => {
                                    let out = finish(a.unwrap_or_else(|_| Err("no answer".into())));
                                    let bytes = match out {
                                        Finished::Json(_, v) => v.to_string().into_bytes(),
                                        Finished::Raw(_, _, b) => b,
                                        Finished::Sse(f) => f.concat().into_bytes(),
                                    };
                                    yield Ok(Bytes::from(bytes));
                                    break;
                                }
                            }
                        }
                    };
                    Response::builder()
                        .header("Content-Type", "application/json")
                        .header("Cache-Control", "no-cache")
                        .body(Body::from_stream(stream))
                        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
                }
            }
        }
        Delivery::Sse => {
            let stream = async_stream::stream! {
                let mut tick = tokio::time::interval(Duration::from_secs(5));
                tick.tick().await;
                let mut rx = rx;
                loop {
                    tokio::select! {
                        _ = tick.tick() => yield Ok::<Bytes, std::convert::Infallible>(Bytes::from_static(b": waiting\n\n")),
                        a = &mut rx => {
                            let frames = match finish(a.unwrap_or_else(|_| Err("no answer".into()))) {
                                Finished::Sse(f) => f,
                                Finished::Json(_, v) => vec![sse("error", &v)],
                                Finished::Raw(..) => vec![],
                            };
                            for f in frames {
                                yield Ok(Bytes::from(f));
                            }
                            break;
                        }
                    }
                }
            };
            Response::builder()
                .header("Content-Type", "text/event-stream")
                .header("Cache-Control", "no-cache")
                .body(Body::from_stream(stream))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
    }
}

fn finished_response(f: Finished) -> Response {
    match f {
        Finished::Json(status, v) => (status, Json(v)).into_response(),
        Finished::Raw(status, ct, b) => {
            (status, [(axum::http::header::CONTENT_TYPE, ct)], b).into_response()
        }
        Finished::Sse(frames) => Response::builder()
            .header("Content-Type", "text/event-stream")
            .body(Body::from(frames.concat()))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
    }
}

/// Check a seat's reply against its task's spec before it is delivered, so
/// a malformed answer is refused to the seat (which can fix it) instead of
/// reaching the caller.
pub fn check_reply(api: &str, spec: &Value, text: &str) -> Result<(), String> {
    match api {
        "embeddings" => crate::api::embeddings::parse_vectors(text, spec).map(|_| ()),
        "moderations" => crate::api::moderations::parse_verdicts(text, spec).map(|_| ()),
        "vector_stores.search" => crate::api::vector_stores::parse_hits(text, spec).map(|_| ()),
        "graders" => crate::api::evals::check_grade(text, spec),
        "decisions" => crate::api::decisions::parse_answers(text, spec).map(|_| ()),
        _ => Ok(()),
    }
}

/// One SSE frame.
pub fn sse(event: &str, data: &Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

/// The seat's answer out of a non-streaming chat completion body.
pub(crate) async fn answer_from_chat(body: &[u8]) -> Result<Answer, String> {
    let text = String::from_utf8_lossy(body);
    let v: Value = serde_json::from_str(text.trim())
        .map_err(|_| format!("unreadable answer: {}", text.trim()))?;
    if v.get("object").and_then(|o| o.as_str()) == Some("chat.completion.async") {
        return Err(format!(
            "the seat did not answer within {} s",
            crate::api::openai::task_timeout_secs()
        ));
    }
    if let Some(e) = v.get("error") {
        return Err(e
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("error")
            .to_string());
    }
    let content = v
        .pointer("/choices/0/message/content")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();
    let mut a = parse_answer(&content);
    a.tool_calls = v
        .pointer("/choices/0/message/tool_calls")
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(a)
}

/// Split a reply into its text and the files it names: attachment ids
/// (`file-<32 hex>`), data URLs and http(s) links.
pub fn parse_answer(content: &str) -> Answer {
    let mut files = Vec::new();
    let mut text = String::new();
    let mut rest = content;
    loop {
        let next = [
            rest.find("file-"),
            rest.find("data:"),
            rest.find("https://"),
            rest.find("http://"),
        ]
        .into_iter()
        .flatten()
        .min();
        let Some(i) = next else {
            text.push_str(rest);
            break;
        };
        let tail = &rest[i..];
        let end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, ')' | '"' | '\'' | '<' | '>' | ']'))
            .unwrap_or(tail.len());
        let token = &tail[..end];
        let taken = if let Some(f) = token
            .strip_prefix("file-")
            .filter(|h| h.len() >= 32 && h[..32].bytes().all(|b| b.is_ascii_hexdigit()))
            .and_then(|h| crate::api::files::get(&format!("file-{}", &h[..32])))
        {
            files.push(Output::Bytes {
                mime: f.mime,
                name: f.filename,
                bytes: f.bytes,
            });
            Some(5 + 32)
        } else if token.starts_with("data:") {
            token
                .strip_prefix("data:")
                .and_then(|r| r.split_once(','))
                .filter(|(m, _)| m.ends_with(";base64"))
                .and_then(|(m, d)| {
                    let bytes = crate::openai_compat::b64_decode(d)?;
                    let mime = m.split(';').next().unwrap_or("").to_string();
                    let name = crate::openai_compat::data_url_name(token)
                        .unwrap_or_else(|| format!("output-{}", files.len() + 1));
                    files.push(Output::Bytes { mime, name, bytes });
                    Some(end)
                })
        } else if token.starts_with("http") && token.len() > 8 {
            files.push(Output::Link(token.to_string()));
            Some(end)
        } else {
            None
        };
        match taken {
            Some(n) => {
                text.push_str(&rest[..i]);
                rest = &rest[i + n..];
            }
            None => {
                text.push_str(&rest[..i + 1]);
                rest = &rest[i + 1..];
            }
        }
    }
    // Markdown wrappers left around a removed image: `![alt]()`.
    let text = text.replace("![](", "").replace("]()", "]");
    Answer {
        raw: content.to_string(),
        text: text.trim().to_string(),
        files,
        tool_calls: Vec::new(),
    }
}

/// Bytes for an output: held ones as they are, a link fetched.
pub async fn fetch(o: &Output) -> Option<(String, Vec<u8>)> {
    match o {
        Output::Bytes { mime, bytes, .. } => Some((mime.clone(), bytes.clone())),
        Output::Link(u) => {
            // The seat names the URL but the server fetches it: same policy
            // as webhook targets (public hosts only, no redirects), plus a
            // hard size cap so a link cannot exhaust memory.
            const MAX: usize = 64 * 1024 * 1024;
            if !crate::api::webhooks::valid_url(u) {
                return None;
            }
            let r = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .ok()?
                .get(u)
                .timeout(Duration::from_secs(60))
                .send()
                .await
                .ok()?;
            if !r.status().is_success() {
                return None;
            }
            if r.content_length().is_some_and(|n| n as usize > MAX) {
                return None;
            }
            let ct = r
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.split(';').next().unwrap_or("").trim().to_string());
            let mut b = Vec::new();
            let mut stream = r.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.ok()?;
                if b.len() + chunk.len() > MAX {
                    return None;
                }
                b.extend_from_slice(&chunk);
            }
            let mime = crate::api::files::sniff_mime(&b)
                .map(String::from)
                .or(ct)
                .unwrap_or_else(|| "application/octet-stream".into());
            Some((mime, b))
        }
    }
}

/// Keep an output under a public id and return its URL, for answers that
/// hand back links (`response_format: "url"`).
pub fn publish(headers: &HeaderMap, owner: &str, mime: &str, bytes: Vec<u8>) -> String {
    let ext = match mime {
        "image/png" => ".png",
        "image/jpeg" => ".jpg",
        "image/webp" => ".webp",
        "image/gif" => ".gif",
        "audio/mpeg" => ".mp3",
        "audio/wav" => ".wav",
        "video/mp4" => ".mp4",
        _ => "",
    };
    let id = crate::api::files::put(crate::api::files::StoredFile {
        owner: owner.to_string(),
        filename: format!("output{ext}"),
        mime: mime.to_string(),
        purpose: crate::api::files::OUTPUT_PURPOSE.to_string(),
        bytes,
        created_at: chrono::Utc::now().timestamp(),
    });
    format!("{}/files/{id}{ext}", base_url(headers))
}

/// This server as the caller reached it.
pub fn base_url(headers: &HeaderMap) -> String {
    if let Ok(u) = std::env::var("SERVER_PUBLIC_URL") {
        let u = u.trim().trim_end_matches('/');
        if !u.is_empty() {
            return u.to_string();
        }
    }
    let h = |n: &str| {
        headers
            .get(n)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.split(',').next().unwrap_or("").trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let host = h("x-forwarded-host")
        .or_else(|| h("host"))
        .unwrap_or_else(|| "localhost".into());
    let local =
        host.starts_with("localhost") || host.starts_with("127.") || host.starts_with("[::1]");
    let proto =
        h("x-forwarded-proto").unwrap_or_else(|| if local { "http" } else { "https" }.into());
    format!("{proto}://{host}")
}

/// Width and height of a PNG, JPEG, GIF or WebP.
pub fn image_size(b: &[u8]) -> Option<(u32, u32)> {
    let be32 = |i: usize| Some(u32::from_be_bytes(b.get(i..i + 4)?.try_into().ok()?));
    let be16 = |i: usize| Some(u16::from_be_bytes(b.get(i..i + 2)?.try_into().ok()?) as u32);
    let le16 = |i: usize| Some(u16::from_le_bytes(b.get(i..i + 2)?.try_into().ok()?) as u32);
    if b.starts_with(b"\x89PNG") {
        return Some((be32(16)?, be32(20)?));
    }
    if b.starts_with(b"GIF8") {
        return Some((le16(6)?, le16(8)?));
    }
    if b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP") {
        return match b.get(12..16)? {
            b"VP8X" => {
                let w = 1 + (b[24] as u32 | (b[25] as u32) << 8 | (b[26] as u32) << 16);
                let h = 1 + (b[27] as u32 | (b[28] as u32) << 8 | (b[29] as u32) << 16);
                Some((w, h))
            }
            b"VP8 " => Some((le16(26)? & 0x3FFF, le16(28)? & 0x3FFF)),
            b"VP8L" => {
                let v = u32::from_le_bytes(b.get(21..25)?.try_into().ok()?);
                Some(((v & 0x3FFF) + 1, ((v >> 14) & 0x3FFF) + 1))
            }
            _ => None,
        };
    }
    if b.starts_with(&[0xFF, 0xD8]) {
        let mut i = 2;
        while i + 9 < b.len() {
            if b[i] != 0xFF {
                i += 1;
                continue;
            }
            let m = b[i + 1];
            if (0xC0..=0xCF).contains(&m) && !matches!(m, 0xC4 | 0xC8 | 0xCC) {
                return Some((be16(i + 7)?, be16(i + 5)?));
            }
            i += 2 + be16(i + 2)? as usize;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answer_splits_text_and_files() {
        let a =
            parse_answer("A red fox.\n![x](data:image/png;base64,aGVsbG8=) see https://e.x/a.png");
        assert_eq!(a.files.len(), 2);
        assert!(a.text.starts_with("A red fox."));
        assert!(!a.text.contains("data:"));
        match &a.files[0] {
            Output::Bytes { mime, bytes, .. } => {
                assert_eq!(mime, "image/png");
                assert_eq!(bytes, b"hello");
            }
            _ => panic!(),
        }
        assert!(matches!(&a.files[1], Output::Link(u) if u == "https://e.x/a.png"));
    }

    #[test]
    fn unknown_file_ids_stay_text() {
        let a = parse_answer("see file-nothing here");
        assert!(a.files.is_empty());
        assert_eq!(a.text, "see file-nothing here");
    }

    #[test]
    fn png_size() {
        let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        b.extend_from_slice(&1024u32.to_be_bytes());
        b.extend_from_slice(&768u32.to_be_bytes());
        assert_eq!(image_size(&b), Some((1024, 768)));
    }
}
