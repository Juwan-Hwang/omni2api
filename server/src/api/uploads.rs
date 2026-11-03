//! `/v1/uploads`: a large file sent in parts, then assembled into an
//! ordinary `/v1/files` file (so it can be referenced by id anywhere).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use axum::extract::{Multipart, Path};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::auth::AuthPrincipal;

struct Upload {
    owner: String,
    filename: String,
    purpose: String,
    mime: String,
    bytes: u64,
    created_at: i64,
    status: &'static str,
    parts: HashMap<String, Vec<u8>>,
    file: Option<Value>,
}

/// Incomplete uploads expire an hour after creation; sweep them at every
/// entry point so stale uploads and their parts cannot pile up.
fn sweep(g: &mut HashMap<String, Upload>) {
    let now = chrono::Utc::now().timestamp();
    g.retain(|_, u| u.created_at + 3600 > now);
}

fn uploads() -> &'static Mutex<HashMap<String, Upload>> {
    static U: OnceLock<Mutex<HashMap<String, Upload>>> = OnceLock::new();
    U.get_or_init(|| Mutex::new(HashMap::new()))
}

fn err(status: StatusCode, msg: &str) -> Response {
    (
        status,
        Json(json!({"error": {"message": msg, "type": "invalid_request_error", "code": null, "param": null}})),
    )
        .into_response()
}

fn obj(id: &str, u: &Upload) -> Value {
    json!({
        "id": id,
        "object": "upload",
        "bytes": u.bytes,
        "created_at": u.created_at,
        "filename": u.filename,
        "purpose": u.purpose,
        "status": u.status,
        "expires_at": u.created_at + 3600,
        "file": u.file,
    })
}

/// `POST /v1/uploads`
pub async fn create(principal: AuthPrincipal, Json(b): Json<Value>) -> Response {
    let s = |k: &str| b.get(k).and_then(|v| v.as_str()).map(String::from);
    let (Some(filename), Some(purpose)) = (s("filename"), s("purpose")) else {
        return err(StatusCode::BAD_REQUEST, "filename and purpose are required");
    };
    if let Err(e) = crate::api::files::check_caller_purpose(&purpose) {
        return err(StatusCode::BAD_REQUEST, &e);
    }
    let Some(bytes) = b.get("bytes").and_then(|v| v.as_u64()) else {
        return err(StatusCode::BAD_REQUEST, "bytes is required");
    };
    let mime =
        s("mime_type").unwrap_or_else(|| crate::openai_compat::guess_mime(&filename).to_string());
    let id = format!("upload_{}", uuid::Uuid::now_v7().simple());
    let u = Upload {
        owner: principal.agent.id.clone(),
        filename,
        purpose,
        mime,
        bytes,
        created_at: chrono::Utc::now().timestamp(),
        status: "pending",
        parts: HashMap::new(),
        file: None,
    };
    let o = obj(&id, &u);
    let mut g = uploads().lock().unwrap();
    sweep(&mut g);
    g.insert(id, u);
    drop(g);
    Json(o).into_response()
}

/// `POST /v1/uploads/:id/parts` (multipart field `data`)
pub async fn add_part(
    principal: AuthPrincipal,
    Path(id): Path<String>,
    mut mp: Multipart,
) -> Response {
    let mut data = None;
    while let Ok(Some(f)) = mp.next_field().await {
        if f.name() == Some("data") {
            match f.bytes().await {
                Ok(b) => data = Some(b.to_vec()),
                Err(e) => return err(StatusCode::BAD_REQUEST, &e.to_string()),
            }
        }
    }
    let Some(data) = data else {
        return err(
            StatusCode::BAD_REQUEST,
            "multipart field `data` is required",
        );
    };
    let mut g = uploads().lock().unwrap();
    sweep(&mut g);
    let Some(u) = g.get_mut(&id).filter(|u| u.owner == principal.agent.id) else {
        return err(
            StatusCode::NOT_FOUND,
            &format!("No upload found with id '{id}'."),
        );
    };
    if u.status != "pending" {
        return err(StatusCode::BAD_REQUEST, &format!("upload is {}", u.status));
    }
    let part = format!("part_{}", uuid::Uuid::now_v7().simple());
    u.parts.insert(part.clone(), data);
    Json(json!({"id": part, "object": "upload.part", "created_at": chrono::Utc::now().timestamp(), "upload_id": id}))
        .into_response()
}

/// `POST /v1/uploads/:id/complete` (`part_ids` in order)
pub async fn complete(
    principal: AuthPrincipal,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> Response {
    let ids: Vec<String> = b
        .get("part_ids")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let mut g = uploads().lock().unwrap();
    sweep(&mut g);
    let Some(u) = g.get_mut(&id).filter(|u| u.owner == principal.agent.id) else {
        return err(
            StatusCode::NOT_FOUND,
            &format!("No upload found with id '{id}'."),
        );
    };
    if u.status != "pending" {
        return err(StatusCode::BAD_REQUEST, &format!("upload is {}", u.status));
    }
    let mut bytes = Vec::new();
    for p in &ids {
        match u.parts.get(p) {
            Some(d) => bytes.extend_from_slice(d),
            None => return err(StatusCode::BAD_REQUEST, &format!("unknown part {p}")),
        }
    }
    if bytes.len() as u64 != u.bytes {
        return err(
            StatusCode::BAD_REQUEST,
            &format!(
                "the parts hold {} bytes, the upload declared {}",
                bytes.len(),
                u.bytes
            ),
        );
    }
    let f = crate::api::files::StoredFile {
        owner: u.owner.clone(),
        filename: u.filename.clone(),
        mime: u.mime.clone(),
        purpose: u.purpose.clone(),
        bytes,
        created_at: chrono::Utc::now().timestamp(),
    };
    let fid = crate::api::files::put(f.clone());
    u.file = Some(crate::api::files::meta(&fid, &f));
    u.status = "completed";
    u.parts.clear();
    Json(obj(&id, u)).into_response()
}

/// `POST /v1/uploads/:id/cancel`
pub async fn cancel(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    let mut g = uploads().lock().unwrap();
    sweep(&mut g);
    let Some(u) = g.get_mut(&id).filter(|u| u.owner == principal.agent.id) else {
        return err(
            StatusCode::NOT_FOUND,
            &format!("No upload found with id '{id}'."),
        );
    };
    if u.status == "pending" {
        u.status = "cancelled";
        u.parts.clear();
    }
    Json(obj(&id, u)).into_response()
}
