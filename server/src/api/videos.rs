//! `/v1/videos`: the seat makes the video. A request answers at once with a
//! queued job; the seat gets the prompt (and any reference image, or the
//! video to remix) as a task and attaches an mp4 (optionally a thumbnail
//! picture); the job then completes and `/content` serves the file.

use axum::extract::{FromRequest, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::api::stored::{self, Stored};
use crate::api::task::{self, Delivery, Finished, Output};
use crate::auth::AuthPrincipal;
use crate::storage::AppState;

fn err(status: StatusCode, msg: &str) -> Response {
    (
        status,
        Json(json!({"error": {"message": msg, "type": "invalid_request_error", "code": null, "param": null}})),
    )
        .into_response()
}

fn get_video(owner: &str, id: &str) -> Option<Stored> {
    stored::get(owner, id).filter(|s| s.obj["object"] == "video")
}

/// Fields and an optional reference picture, from JSON or multipart.
async fn read_body(
    headers: &HeaderMap,
    body: axum::body::Bytes,
) -> Result<(Value, Option<String>), String> {
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if !ct.starts_with("multipart/form-data") {
        let v: Value = serde_json::from_slice(&body).map_err(|e| format!("invalid JSON: {e}"))?;
        let reference = v
            .pointer("/input_reference/image_url")
            .or_else(|| v.get("input_reference"))
            .and_then(|u| u.as_str())
            .map(String::from)
            .or_else(|| {
                v.pointer("/input_reference/file_id")
                    .and_then(|f| f.as_str())
                    .and_then(crate::api::files::data_url)
            });
        return Ok((v, reference));
    }
    let req = axum::http::Request::builder()
        .header(header::CONTENT_TYPE, ct.as_str())
        .body(axum::body::Body::from(body))
        .map_err(|e| e.to_string())?;
    let mut mp = axum::extract::Multipart::from_request(req, &())
        .await
        .map_err(|e| e.body_text())?;
    let mut fields = serde_json::Map::new();
    let mut reference = None;
    while let Some(f) = mp.next_field().await.map_err(|e| e.to_string())? {
        let name = f.name().unwrap_or("").to_string();
        let file_name = f.file_name().map(String::from);
        let mime = f.content_type().map(String::from);
        let bytes = f.bytes().await.map_err(|e| e.to_string())?;
        if name == "input_reference" && file_name.is_some() {
            let fname = file_name.unwrap_or_else(|| "reference".into());
            let mime = crate::api::files::sniff_mime(&bytes)
                .map(String::from)
                .or(mime)
                .unwrap_or_else(|| crate::openai_compat::guess_mime(&fname).to_string());
            reference = Some(crate::openai_compat::named_data_url(
                &mime,
                Some(&fname),
                &crate::openai_compat::b64_encode(&bytes),
            ));
        } else {
            fields.insert(
                name,
                Value::String(String::from_utf8_lossy(&bytes).to_string()),
            );
        }
    }
    Ok((Value::Object(fields), reference))
}

/// Queue a video job and run its seat task in the background.
#[allow(clippy::too_many_arguments)]
fn start(
    app: AppState,
    principal: AuthPrincipal,
    headers: HeaderMap,
    prompt: String,
    model: String,
    seconds: String,
    size: String,
    attach: Vec<String>,
    remixed_from: Option<String>,
) -> Value {
    let id = format!("video_{}", uuid::Uuid::now_v7().simple());
    let now = chrono::Utc::now().timestamp();
    let obj = json!({
        "id": id,
        "object": "video",
        "model": model,
        "status": "queued",
        "progress": 0,
        "created_at": now,
        "completed_at": null,
        "expires_at": null,
        "size": size,
        "seconds": seconds,
        "quality": "standard",
        "prompt": prompt,
        "remixed_from_video_id": remixed_from,
        "error": null,
    });
    let owner = principal.agent.id.clone();
    stored::put(
        &id,
        Stored {
            owner,
            obj: obj.clone(),
            input: Vec::new(),
            history: Vec::new(),
        },
    );
    let what = if remixed_from.is_some() {
        "remix the attached video as the message says"
    } else if attach.is_empty() {
        "make the video the message describes"
    } else {
        "make the video the message describes, starting from the attached picture"
    };
    let expected = format!(
        "{seconds}s video, {size}: {what} and attach it: `oh-seat reply -a out.mp4` (a picture attached too becomes its thumbnail); text alone is a refusal"
    );
    let vid = id.clone();
    let finish: task::Finish = Box::new(move |answer| {
        let now = chrono::Utc::now().timestamp();
        let mut video = None;
        let mut thumb = None;
        let mut failure = None;
        match answer {
            Err(e) => failure = Some(e),
            Ok(a) => {
                for o in &a.files {
                    if let Output::Bytes { mime, bytes, .. } = o {
                        if mime.starts_with("video/") && video.is_none() {
                            video = Some((mime.clone(), bytes.clone()));
                        } else if mime.starts_with("image/") && thumb.is_none() {
                            thumb = Some((mime.clone(), bytes.clone()));
                        }
                    }
                }
                if video.is_none() {
                    failure = Some(if a.text.is_empty() {
                        "the seat answered without a video".into()
                    } else {
                        a.text
                    });
                }
            }
        }
        stored::update(&vid, |s| match &failure {
            Some(e) => {
                s.obj["status"] = json!("failed");
                s.obj["error"] = json!({"code": "video_generation_failed", "message": e});
            }
            None => {
                s.obj["status"] = json!("completed");
                s.obj["progress"] = json!(100);
                s.obj["completed_at"] = json!(now);
                s.obj["expires_at"] = json!(now + 86400);
                let keep = |name: &str, (mime, bytes): (String, Vec<u8>)| json!({"name": name, "mime": mime, "b64": crate::openai_compat::b64_encode(&bytes)});
                s.input = video
                    .clone()
                    .map(|v| keep("video", v))
                    .into_iter()
                    .collect();
                s.input.extend(thumb.clone().map(|t| keep("thumbnail", t)));
            }
        });
        Finished::Json(StatusCode::OK, json!({}))
    });
    let vid2 = id.clone();
    tokio::spawn(async move {
        stored::update(&vid2, |s| {
            s.obj["status"] = json!("in_progress");
            s.obj["progress"] = json!(1);
        });
        let resp = task::run(
            app,
            principal,
            headers,
            "videos",
            Some(model.as_str()),
            prompt,
            attach,
            expected,
            true,
            Value::Null,
            Delivery::Json,
            finish,
        )
        .await;
        // Drive the response to its end: the finish above records the job.
        let _ = axum::body::to_bytes(resp.into_body(), usize::MAX).await;
    });
    obj
}

/// `POST /v1/videos`
pub async fn create(
    State(app): State<AppState>,
    principal: AuthPrincipal,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let (b, reference) = match read_body(&headers, body).await {
        Ok(r) => r,
        Err(e) => return err(StatusCode::BAD_REQUEST, &e),
    };
    let s = |k: &str| {
        b.get(k)
            .map(|v| {
                v.as_str()
                    .map(String::from)
                    .unwrap_or_else(|| v.to_string())
            })
            .filter(|v| !v.is_empty())
    };
    let Some(prompt) = s("prompt") else {
        return err(StatusCode::BAD_REQUEST, "prompt is required");
    };
    let obj = start(
        app,
        principal,
        headers,
        prompt,
        s("model").unwrap_or_else(|| "sora-2".into()),
        s("seconds").unwrap_or_else(|| "4".into()),
        s("size").unwrap_or_else(|| "720x1280".into()),
        reference.into_iter().collect(),
        None,
    );
    Json(obj).into_response()
}

/// `POST /v1/videos/:id/remix`
pub async fn remix(
    State(app): State<AppState>,
    principal: AuthPrincipal,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> Response {
    let Some(src) = get_video(&principal.agent.id, &id) else {
        return stored::not_found("video", &id);
    };
    let Some(v) = src.input.iter().find(|f| f["name"] == "video") else {
        return err(StatusCode::BAD_REQUEST, "the source video is not completed");
    };
    let Some(prompt) = b.get("prompt").and_then(|v| v.as_str()).map(String::from) else {
        return err(StatusCode::BAD_REQUEST, "prompt is required");
    };
    let url = crate::openai_compat::named_data_url(
        v["mime"].as_str().unwrap_or("video/mp4"),
        Some("source.mp4"),
        v["b64"].as_str().unwrap_or(""),
    );
    let o = &src.obj;
    let obj = start(
        app,
        principal,
        headers,
        prompt,
        o["model"].as_str().unwrap_or("sora-2").to_string(),
        o["seconds"].as_str().unwrap_or("4").to_string(),
        o["size"].as_str().unwrap_or("720x1280").to_string(),
        vec![url],
        Some(id),
    );
    Json(obj).into_response()
}

pub async fn retrieve(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    match get_video(&principal.agent.id, &id) {
        Some(s) => Json(s.obj).into_response(),
        None => stored::not_found("video", &id),
    }
}

pub async fn delete(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    if get_video(&principal.agent.id, &id).is_some() && stored::remove(&principal.agent.id, &id) {
        Json(json!({"id": id, "object": "video.deleted", "deleted": true})).into_response()
    } else {
        stored::not_found("video", &id)
    }
}

#[derive(Debug, serde::Deserialize, Default)]
pub struct ContentQuery {
    #[serde(default)]
    variant: Option<String>,
}

/// `GET /v1/videos/:id/content?variant=video|thumbnail`
pub async fn content(
    principal: AuthPrincipal,
    Path(id): Path<String>,
    Query(q): Query<ContentQuery>,
) -> Response {
    let Some(s) = get_video(&principal.agent.id, &id) else {
        return stored::not_found("video", &id);
    };
    let want = match q.variant.as_deref().unwrap_or("video") {
        "video" => "video",
        "thumbnail" => "thumbnail",
        other => return err(StatusCode::NOT_FOUND, &format!("no {other} for this video")),
    };
    match s.input.iter().find(|f| f["name"] == want) {
        Some(f) => {
            let bytes = crate::openai_compat::b64_decode(f["b64"].as_str().unwrap_or(""))
                .unwrap_or_default();
            (
                [(
                    header::CONTENT_TYPE,
                    f["mime"].as_str().unwrap_or("video/mp4").to_string(),
                )],
                bytes,
            )
                .into_response()
        }
        None if s.obj["status"] != "completed" => {
            err(StatusCode::NOT_FOUND, "the video is not ready yet")
        }
        None => err(StatusCode::NOT_FOUND, &format!("no {want} for this video")),
    }
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

pub async fn list(principal: AuthPrincipal, Query(q): Query<ListQuery>) -> Response {
    let mut items: Vec<Value> = stored::list(&principal.agent.id, "video")
        .into_iter()
        .map(|(_, s)| s.obj)
        .collect();
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
    Json(json!({"object": "list", "data": items, "first_id": id_of(items.first()), "last_id": id_of(items.last()), "has_more": has_more}))
        .into_response()
}
