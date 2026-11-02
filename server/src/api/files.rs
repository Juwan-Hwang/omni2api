//! `/v1/files`: the OpenAI and Anthropic Files APIs, so a caller that
//! uploads first and then refers to a `file_id` (SDKs, Anthropic's
//! `{"type": "file"}` document source) reaches the seat with the file
//! itself. Kept in memory per process, owned by the uploading key's agent.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use axum::extract::{Multipart, Path};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::state::AuthPrincipal;

#[derive(Clone)]
pub struct StoredFile {
    pub owner: String,
    pub filename: String,
    pub mime: String,
    pub purpose: String,
    pub bytes: Vec<u8>,
    pub created_at: i64,
}

/// Purpose of a file a seat made to answer an API task (a picture, audio).
/// Only these are served without a key, at their unguessable id.
pub const OUTPUT_PURPOSE: &str = "output";

/// Most bytes kept per owner; that owner's oldest files go first beyond it.
/// `OMNI2API_FILES_MAX_BYTES_PER_OWNER` overrides.
fn keep_bytes_per_owner() -> usize {
    std::env::var("OMNI2API_FILES_MAX_BYTES_PER_OWNER")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(512 * 1024 * 1024)
}

/// Backstop across all owners. `OMNI2API_FILES_MAX_BYTES_TOTAL` overrides.
fn keep_bytes_total() -> usize {
    std::env::var("OMNI2API_FILES_MAX_BYTES_TOTAL")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(4 * 512 * 1024 * 1024)
}

/// Files by id, plus ids oldest first.
type Store = (HashMap<String, StoredFile>, Vec<String>);

fn store() -> &'static Mutex<Store> {
    static S: OnceLock<Mutex<Store>> = OnceLock::new();
    S.get_or_init(|| Mutex::new((HashMap::new(), Vec::new())))
}

pub fn put(f: StoredFile) -> String {
    let id = format!("file-{}", Uuid::now_v7().simple());
    let mut g = store().lock().unwrap();
    g.0.insert(id.clone(), f);
    g.1.push(id.clone());
    // The owner's own oldest files go first; other owners are untouched.
    let owner = g.0.get(&id).map(|x| x.owner.clone()).unwrap_or_default();
    let mut mine: usize =
        g.0.values()
            .filter(|x| x.owner == owner)
            .map(|x| x.bytes.len())
            .sum();
    while mine > keep_bytes_per_owner() {
        let Some(pos) =
            g.1.iter()
                .position(|i| g.0.get(i.as_str()).is_some_and(|x| x.owner == owner))
        else {
            break;
        };
        if g.1[pos] == id {
            break; // the owner's only file is the one just stored; keep it
        }
        let old = g.1.remove(pos);
        if let Some(x) = g.0.remove(&old) {
            mine -= x.bytes.len();
        }
    }
    // Global backstop: total across owners.
    let mut total: usize = g.0.values().map(|x| x.bytes.len()).sum();
    while total > keep_bytes_total() && g.1.len() > 1 {
        let old = g.1.remove(0);
        if let Some(x) = g.0.remove(&old) {
            total -= x.bytes.len();
        }
    }
    id
}

/// A stored file as a named data URL, when it exists (any owner: ids are
/// unguessable and only reach the seat inside the owner's own request).
pub fn data_url(id: &str) -> Option<String> {
    let g = store().lock().unwrap();
    let f = g.0.get(id.trim())?;
    Some(crate::openai_compat::named_data_url(
        &f.mime,
        Some(&f.filename),
        &crate::openai_compat::b64_encode(&f.bytes),
    ))
}

/// A stored file, whoever owns it (ids are unguessable).
/// When exactly these bytes were produced by the seat through this server:
/// (what made it, when). `purpose == output` is the trust signal: only
/// server-side `put` callers may set it (callers cannot upload with it), so
/// every output file in the store came back from the seat, whichever key —
/// the seat's or the caller's — it was stored under.
pub fn made_by_seat(bytes: &[u8]) -> Option<(String, i64)> {
    let g = store().lock().unwrap();
    g.0.values()
        .find(|f| f.purpose == OUTPUT_PURPOSE && f.bytes == bytes)
        .map(|f| ("omni2api-seat".to_string(), f.created_at))
}

pub fn get(id: &str) -> Option<StoredFile> {
    store().lock().unwrap().0.get(id.trim()).cloned()
}

/// The mime type of common media from its first bytes.
pub fn sniff_mime(b: &[u8]) -> Option<&'static str> {
    let starts = |p: &[u8]| b.starts_with(p);
    if starts(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if starts(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if starts(b"GIF8") {
        Some("image/gif")
    } else if starts(b"RIFF") && b.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else if starts(b"RIFF") && b.get(8..12) == Some(b"WAVE") {
        Some("audio/wav")
    } else if starts(b"ID3") || (b.len() > 1 && b[0] == 0xFF && b[1] & 0xE0 == 0xE0) {
        Some("audio/mpeg")
    } else if starts(b"fLaC") {
        Some("audio/flac")
    } else if starts(b"OggS") {
        Some("audio/ogg")
    } else if b.get(4..8) == Some(b"ftyp") {
        Some("video/mp4")
    } else if starts(&[0x1A, 0x45, 0xDF, 0xA3]) {
        Some("video/webm")
    } else if starts(b"%PDF") {
        Some("application/pdf")
    } else {
        None
    }
}

/// `GET /files/:id`: a seat-made output, without a key, so a URL in an
/// API answer (`response_format: "url"`) opens in any browser or SDK.
pub async fn public(Path(id): Path<String>) -> Response {
    let id = id.split('.').next().unwrap_or("").to_string();
    match get(&id).filter(|f| f.purpose == OUTPUT_PURPOSE) {
        Some(f) => (
            [
                (header::CONTENT_TYPE, f.mime.clone()),
                (header::CACHE_CONTROL, "private, max-age=3600".to_string()),
            ],
            f.bytes,
        )
            .into_response(),
        None => not_found(&id),
    }
}

pub fn meta(id: &str, f: &StoredFile) -> Value {
    // One object both SDK families accept: OpenAI's FileObject fields and
    // Anthropic's FileMetadata fields side by side.
    let created = chrono::DateTime::from_timestamp(f.created_at, 0)
        .map(|d| d.to_rfc3339())
        .unwrap_or_default();
    json!({
        "id": id,
        "object": "file",
        "type": "file",
        "bytes": f.bytes.len(),
        "size_bytes": f.bytes.len(),
        "created_at": f.created_at,
        "created_at_iso": created,
        "filename": f.filename,
        "mime_type": f.mime,
        "purpose": f.purpose,
        "status": "processed",
        "downloadable": true,
    })
}

fn not_found(id: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error": {"message": format!("No such file: {id}"), "type": "invalid_request_error", "code": "file_not_found"}})),
    )
        .into_response()
}

pub async fn upload(principal: AuthPrincipal, mut mp: Multipart) -> Response {
    let mut file: Option<(String, String, Vec<u8>)> = None;
    let mut purpose = "user_data".to_string();
    loop {
        let field = match mp.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": {"message": e.to_string(), "type": "invalid_request_error"}})),
            )
                .into_response(),
        };
        let name = field.name().unwrap_or("").to_string();
        if name == "purpose" {
            purpose = field.text().await.unwrap_or_default();
            continue;
        }
        if name != "file" {
            continue;
        }
        let filename = field.file_name().unwrap_or("upload").to_string();
        let mime = field
            .content_type()
            .map(|s| s.to_string())
            .filter(|m| !m.is_empty() && m != "application/octet-stream")
            .unwrap_or_else(|| crate::openai_compat::guess_mime(&filename).to_string());
        match field.bytes().await {
            Ok(b) => file = Some((filename, mime, b.to_vec())),
            Err(e) => return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": {"message": e.to_string(), "type": "invalid_request_error"}})),
            )
                .into_response(),
        }
    }
    // `output` marks seat-made files served on the anonymous download route;
    // letting a caller set it would hand out public URLs for arbitrary
    // uploaded content. Only server-side `put` callers may use it.
    if purpose == OUTPUT_PURPOSE {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": {"message": format!("purpose \"{OUTPUT_PURPOSE}\" is reserved for files the seat produces"), "type": "invalid_request_error"}})),
        )
            .into_response();
    }
    let Some((filename, mime, bytes)) = file else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": {"message": "multipart field `file` is required", "type": "invalid_request_error"}})),
        )
            .into_response();
    };
    let f = StoredFile {
        owner: principal.agent.id.clone(),
        filename,
        mime,
        purpose,
        bytes,
        created_at: chrono::Utc::now().timestamp(),
    };
    let id = put(f.clone());
    Json(meta(&id, &f)).into_response()
}

fn owned(principal: &AuthPrincipal, id: &str) -> Option<StoredFile> {
    let g = store().lock().unwrap();
    g.0.get(id)
        .filter(|f| f.owner == principal.agent.id)
        .cloned()
}

pub async fn get_meta(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    match owned(&principal, &id) {
        Some(f) => Json(meta(&id, &f)).into_response(),
        None => not_found(&id),
    }
}

pub async fn content(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    match owned(&principal, &id) {
        Some(f) => ([(header::CONTENT_TYPE, f.mime.clone())], f.bytes).into_response(),
        None => not_found(&id),
    }
}

pub async fn list(principal: AuthPrincipal) -> Response {
    let g = store().lock().unwrap();
    let data: Vec<Value> =
        g.1.iter()
            .rev()
            .filter_map(|id| {
                g.0.get(id)
                    .filter(|f| f.owner == principal.agent.id)
                    .map(|f| meta(id, f))
            })
            .collect();
    Json(json!({"object": "list", "data": data, "has_more": false})).into_response()
}

pub async fn delete(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    if owned(&principal, &id).is_none() {
        return not_found(&id);
    }
    let mut g = store().lock().unwrap();
    g.0.remove(&id);
    g.1.retain(|x| x != &id);
    Json(json!({"id": id, "object": "file", "type": "file_deleted", "deleted": true}))
        .into_response()
}
