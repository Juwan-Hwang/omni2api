//! `/v1/containers`: containers and their files (the code-interpreter
//! sandbox objects). Storage only; the files are kept as uploaded.

use axum::extract::{Multipart, Path, Query};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::api::stored::{self, Stored};
use crate::auth::AuthPrincipal;

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn err(status: StatusCode, msg: &str) -> Response {
    (
        status,
        Json(json!({"error": {"message": msg, "type": "invalid_request_error", "code": null, "param": null}})),
    )
        .into_response()
}

fn get_container(owner: &str, id: &str) -> Option<Stored> {
    stored::get(owner, id).filter(|s| s.obj["object"] == "container")
}

#[derive(Debug, serde::Deserialize, Default)]
pub struct ListQuery {
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    after: Option<String>,
    #[serde(default)]
    order: Option<String>,
}

fn page(mut items: Vec<Value>, q: &ListQuery) -> Value {
    if q.order.as_deref().unwrap_or("desc") == "desc" {
        items.reverse();
    }
    if let Some(a) = &q.after {
        if let Some(i) = items.iter().position(|x| x["id"] == json!(a)) {
            items.drain(..=i);
        }
    }
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let has_more = items.len() > limit;
    items.truncate(limit);
    let id_of = |x: Option<&Value>| x.and_then(|v| v.get("id")).cloned().unwrap_or(Value::Null);
    json!({"object": "list", "data": items, "first_id": id_of(items.first()), "last_id": id_of(items.last()), "has_more": has_more})
}

fn file_entry(container: &str, fid: &str, path: &str, bytes: usize, source: &str) -> Value {
    json!({
        "id": fid,
        "object": "container.file",
        "created_at": now(),
        "bytes": bytes,
        "container_id": container,
        "path": path,
        "source": source,
    })
}

pub async fn create(principal: AuthPrincipal, Json(b): Json<Value>) -> Response {
    let Some(name) = b.get("name").and_then(|v| v.as_str()) else {
        return err(StatusCode::BAD_REQUEST, "name is required");
    };
    let owner = principal.agent.id.clone();
    let id = format!("cntr_{}", uuid::Uuid::now_v7().simple());
    let mut files = Vec::new();
    for fid in b
        .get("file_ids")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
    {
        let fid = fid.as_str().unwrap_or("");
        match crate::api::files::get(fid).filter(|f| f.owner == owner) {
            Some(f) => files.push(file_entry(
                &id,
                fid,
                &format!("/mnt/data/{}", f.filename),
                f.bytes.len(),
                "user",
            )),
            None => {
                return err(
                    StatusCode::NOT_FOUND,
                    &format!("No file found with id '{fid}'."),
                )
            }
        }
    }
    let obj = json!({
        "id": id,
        "object": "container",
        "created_at": now(),
        "status": "running",
        "name": name,
        "expires_after": b.get("expires_after").cloned().unwrap_or(json!({"anchor": "last_active_at", "minutes": 20})),
        "last_active_at": now(),
        "memory_limit": b.get("memory_limit").cloned().unwrap_or(json!("1g")),
    });
    stored::put(
        &id,
        Stored {
            owner,
            obj: obj.clone(),
            input: files,
            history: Vec::new(),
        },
    );
    Json(obj).into_response()
}

pub async fn list(principal: AuthPrincipal, Query(q): Query<ListQuery>) -> Response {
    let items = stored::list(&principal.agent.id, "container")
        .into_iter()
        .map(|(_, s)| s.obj)
        .collect();
    Json(page(items, &q)).into_response()
}

pub async fn retrieve(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    match get_container(&principal.agent.id, &id) {
        Some(s) => Json(s.obj).into_response(),
        None => stored::not_found("container", &id),
    }
}

pub async fn delete(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    if get_container(&principal.agent.id, &id).is_some() && stored::remove(&principal.agent.id, &id)
    {
        Json(json!({"id": id, "object": "container.deleted", "deleted": true})).into_response()
    } else {
        stored::not_found("container", &id)
    }
}

/// `POST /v1/containers/:id/files`: multipart `file`, or JSON `file_id`.
pub async fn add_file(
    principal: AuthPrincipal,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let owner = principal.agent.id.clone();
    if get_container(&owner, &id).is_none() {
        return stored::not_found("container", &id);
    }
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let entry = if ct.starts_with("multipart/form-data") {
        use axum::extract::FromRequest;
        let Ok(req) = axum::http::Request::builder()
            .header(header::CONTENT_TYPE, ct.as_str())
            .body(axum::body::Body::from(body))
        else {
            return err(StatusCode::BAD_REQUEST, "bad multipart body");
        };
        let mut mp = match Multipart::from_request(req, &()).await {
            Ok(m) => m,
            Err(e) => return err(StatusCode::BAD_REQUEST, &e.body_text()),
        };
        let mut got = None;
        while let Ok(Some(f)) = mp.next_field().await {
            if f.name() == Some("file") {
                let name = f.file_name().unwrap_or("upload").to_string();
                let mime = f.content_type().map(String::from);
                if let Ok(b) = f.bytes().await {
                    got = Some((name, mime, b.to_vec()));
                }
            }
        }
        let Some((name, mime, bytes)) = got else {
            return err(
                StatusCode::BAD_REQUEST,
                "multipart field `file` is required",
            );
        };
        let n = bytes.len();
        let fid = crate::api::files::put(crate::api::files::StoredFile {
            owner: owner.clone(),
            filename: name.clone(),
            mime: mime.unwrap_or_else(|| crate::openai_compat::guess_mime(&name).to_string()),
            purpose: "container".into(),
            bytes,
            created_at: now(),
        });
        file_entry(&id, &fid, &format!("/mnt/data/{name}"), n, "user")
    } else {
        let b: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
        let Some(fid) = b.get("file_id").and_then(|v| v.as_str()) else {
            return err(
                StatusCode::BAD_REQUEST,
                "file (multipart) or file_id is required",
            );
        };
        let Some(f) = crate::api::files::get(fid).filter(|f| f.owner == owner) else {
            return err(
                StatusCode::NOT_FOUND,
                &format!("No file found with id '{fid}'."),
            );
        };
        file_entry(
            &id,
            fid,
            &format!("/mnt/data/{}", f.filename),
            f.bytes.len(),
            "user",
        )
    };
    let e = entry.clone();
    stored::update(&id, |s| {
        s.input.retain(|x| x["id"] != e["id"]);
        s.input.push(e);
        s.obj["last_active_at"] = json!(now());
    });
    Json(entry).into_response()
}

pub async fn list_files(
    principal: AuthPrincipal,
    Path(id): Path<String>,
    Query(q): Query<ListQuery>,
) -> Response {
    match get_container(&principal.agent.id, &id) {
        Some(s) => Json(page(s.input, &q)).into_response(),
        None => stored::not_found("container", &id),
    }
}

pub async fn get_file(
    principal: AuthPrincipal,
    Path((id, fid)): Path<(String, String)>,
) -> Response {
    match get_container(&principal.agent.id, &id)
        .and_then(|s| s.input.into_iter().find(|f| f["id"] == json!(fid)))
    {
        Some(f) => Json(f).into_response(),
        None => stored::not_found("container file", &fid),
    }
}

pub async fn delete_file(
    principal: AuthPrincipal,
    Path((id, fid)): Path<(String, String)>,
) -> Response {
    let found = get_container(&principal.agent.id, &id)
        .is_some_and(|s| s.input.iter().any(|f| f["id"] == json!(fid)));
    if !found {
        return stored::not_found("container file", &fid);
    }
    stored::update(&id, |s| s.input.retain(|f| f["id"] != json!(fid)));
    Json(json!({"id": fid, "object": "container.file.deleted", "deleted": true})).into_response()
}

pub async fn file_content(
    principal: AuthPrincipal,
    Path((id, fid)): Path<(String, String)>,
) -> Response {
    let found = get_container(&principal.agent.id, &id)
        .is_some_and(|s| s.input.iter().any(|f| f["id"] == json!(fid)));
    match crate::api::files::get(&fid).filter(|_| found) {
        Some(f) => ([(header::CONTENT_TYPE, f.mime)], f.bytes).into_response(),
        None => stored::not_found("container file", &fid),
    }
}
