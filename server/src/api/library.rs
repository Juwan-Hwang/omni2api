//! Stored libraries: custom voices and voice consents (`/v1/audio/voices`,
//! `/v1/audio/voice_consents`) and skills with versions (`/v1/skills`).
//! A custom voice's sample goes along with every speech request that uses
//! it, so the seat can speak in it.

use axum::extract::{FromRequest, Path, Query};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};

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

/// A request's fields and files (`(field, name, mime, bytes)`), from
/// multipart or JSON.
async fn form(
    headers: &HeaderMap,
    body: axum::body::Bytes,
) -> Result<(Map<String, Value>, Vec<(String, String, String, Vec<u8>)>), String> {
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if !ct.starts_with("multipart/form-data") {
        let v: Value = if body.is_empty() {
            json!({})
        } else {
            serde_json::from_slice(&body).map_err(|e| format!("invalid JSON: {e}"))?
        };
        return Ok((v.as_object().cloned().unwrap_or_default(), Vec::new()));
    }
    let req = axum::http::Request::builder()
        .header(header::CONTENT_TYPE, ct.as_str())
        .body(axum::body::Body::from(body))
        .map_err(|e| e.to_string())?;
    let mut mp = axum::extract::Multipart::from_request(req, &())
        .await
        .map_err(|e| e.body_text())?;
    let mut fields = Map::new();
    let mut files = Vec::new();
    while let Some(f) = mp.next_field().await.map_err(|e| e.to_string())? {
        let name = f.name().unwrap_or("").trim_end_matches("[]").to_string();
        let file_name = f.file_name().map(String::from);
        let mime = f.content_type().map(String::from);
        let bytes = f.bytes().await.map_err(|e| e.to_string())?.to_vec();
        match file_name {
            Some(fname) => {
                let mime = crate::api::files::sniff_mime(&bytes)
                    .map(String::from)
                    .or(mime.filter(|m| m != "application/octet-stream"))
                    .unwrap_or_else(|| crate::openai_compat::guess_mime(&fname).to_string());
                files.push((name, fname, mime, bytes));
            }
            None => {
                fields.insert(
                    name,
                    Value::String(String::from_utf8_lossy(&bytes).to_string()),
                );
            }
        }
    }
    Ok((fields, files))
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

fn keep_file(owner: &str, f: &(String, String, String, Vec<u8>), purpose: &str) -> String {
    crate::api::files::put(crate::api::files::StoredFile {
        owner: owner.to_string(),
        filename: f.1.clone(),
        mime: f.2.clone(),
        purpose: purpose.to_string(),
        bytes: f.3.clone(),
        created_at: now(),
    })
}

fn s(m: &Map<String, Value>, k: &str) -> Option<String> {
    m.get(k)
        .and_then(|v| v.as_str())
        .map(String::from)
        .filter(|v| !v.is_empty())
}

// ── voices ──

pub async fn create_voice(
    principal: AuthPrincipal,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let (f, files) = match form(&headers, body).await {
        Ok(r) => r,
        Err(e) => return err(StatusCode::BAD_REQUEST, &e),
    };
    let Some(name) = s(&f, "name") else {
        return err(StatusCode::BAD_REQUEST, "name is required");
    };
    let owner = principal.agent.id.clone();
    let sample = files.iter().find(|x| x.0 == "audio_sample");
    let kind = s(&f, "type").unwrap_or_else(|| {
        if sample.is_some() {
            "audio_sample".into()
        } else {
            "prompt".into()
        }
    });
    let mut input = Vec::new();
    match kind.as_str() {
        "audio_sample" => {
            let Some(smp) = sample else {
                return err(StatusCode::BAD_REQUEST, "audio_sample is required");
            };
            let Some(cid) = s(&f, "consent") else {
                return err(StatusCode::BAD_REQUEST, "consent is required");
            };
            if stored::get(&owner, &cid)
                .filter(|c| c.obj["object"] == "audio.voice_consent")
                .is_none()
            {
                return stored::not_found("voice consent", &cid);
            }
            input.push(json!({"file_id": keep_file(&owner, smp, "voice_sample")}));
        }
        "prompt" if s(&f, "prompt").is_none() => {
            return err(StatusCode::BAD_REQUEST, "prompt is required")
        }
        "prompt" => {}
        o => {
            return err(
                StatusCode::BAD_REQUEST,
                &format!("type must be audio_sample or prompt, not {o}"),
            )
        }
    }
    let id = format!("voice_{}", uuid::Uuid::now_v7().simple());
    let obj = json!({"id": id, "object": "audio.voice", "created_at": now(), "name": name, "type": kind, "prompt": s(&f, "prompt")});
    stored::put(
        &id,
        Stored {
            owner,
            obj: obj.clone(),
            input,
            history: Vec::new(),
        },
    );
    Json(obj).into_response()
}

pub async fn list_voices(principal: AuthPrincipal, Query(q): Query<ListQuery>) -> Response {
    let items = stored::list(&principal.agent.id, "audio.voice")
        .into_iter()
        .map(|(_, s)| s.obj)
        .collect();
    Json(page(items, &q)).into_response()
}

pub async fn get_voice(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    match stored::get(&principal.agent.id, &id).filter(|s| s.obj["object"] == "audio.voice") {
        Some(s) => Json(s.obj).into_response(),
        None => stored::not_found("voice", &id),
    }
}

pub async fn delete_voice(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    if stored::get(&principal.agent.id, &id)
        .filter(|s| s.obj["object"] == "audio.voice")
        .is_some()
        && stored::remove(&principal.agent.id, &id)
    {
        Json(json!({"id": id, "object": "audio.voice.deleted", "deleted": true})).into_response()
    } else {
        stored::not_found("voice", &id)
    }
}

/// A custom voice for a speech request: its name, how it should sound, and
/// its sample as a data URL.
pub fn voice_for_speech(owner: &str, id: &str) -> Option<(String, Option<String>, Option<String>)> {
    let v = stored::get(owner, id).filter(|s| s.obj["object"] == "audio.voice")?;
    let sample = v
        .input
        .first()
        .and_then(|x| x["file_id"].as_str())
        .and_then(crate::api::files::data_url);
    Some((
        v.obj["name"].as_str().unwrap_or(id).to_string(),
        v.obj["prompt"].as_str().map(String::from),
        sample,
    ))
}

// ── voice consents ──

pub async fn create_consent(
    principal: AuthPrincipal,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let (f, files) = match form(&headers, body).await {
        Ok(r) => r,
        Err(e) => return err(StatusCode::BAD_REQUEST, &e),
    };
    let (Some(name), Some(language)) = (s(&f, "name"), s(&f, "language")) else {
        return err(StatusCode::BAD_REQUEST, "name and language are required");
    };
    let Some(rec) = files.iter().find(|x| x.0 == "recording") else {
        return err(StatusCode::BAD_REQUEST, "recording is required");
    };
    let owner = principal.agent.id.clone();
    let fid = keep_file(&owner, rec, "voice_consent");
    let id = format!("cons_{}", uuid::Uuid::now_v7().simple());
    let obj = json!({"id": id, "object": "audio.voice_consent", "created_at": now(), "language": language, "name": name});
    stored::put(
        &id,
        Stored {
            owner,
            obj: obj.clone(),
            input: vec![json!({"file_id": fid})],
            history: Vec::new(),
        },
    );
    Json(obj).into_response()
}

pub async fn list_consents(principal: AuthPrincipal, Query(q): Query<ListQuery>) -> Response {
    let items = stored::list(&principal.agent.id, "audio.voice_consent")
        .into_iter()
        .map(|(_, s)| s.obj)
        .collect();
    Json(page(items, &q)).into_response()
}

pub async fn get_consent(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    match stored::get(&principal.agent.id, &id).filter(|s| s.obj["object"] == "audio.voice_consent")
    {
        Some(s) => Json(s.obj).into_response(),
        None => stored::not_found("voice consent", &id),
    }
}

pub async fn update_consent(
    principal: AuthPrincipal,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> Response {
    if stored::get(&principal.agent.id, &id)
        .filter(|s| s.obj["object"] == "audio.voice_consent")
        .is_none()
    {
        return stored::not_found("voice consent", &id);
    }
    if let Some(n) = b.get("name") {
        let n = n.clone();
        stored::update(&id, |s| s.obj["name"] = n);
    }
    get_consent(principal, Path(id)).await
}

pub async fn delete_consent(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    if stored::get(&principal.agent.id, &id)
        .filter(|s| s.obj["object"] == "audio.voice_consent")
        .is_some()
        && stored::remove(&principal.agent.id, &id)
    {
        Json(json!({"id": id, "object": "audio.voice_consent", "deleted": true})).into_response()
    } else {
        stored::not_found("voice consent", &id)
    }
}

// ── skills ──

/// `name` and `description` from a SKILL.md front matter, if any.
fn skill_meta(files: &[(String, String, String, Vec<u8>)]) -> (Option<String>, Option<String>) {
    let Some(md) = files.iter().find(|f| {
        f.1.rsplit('/')
            .next()
            .is_some_and(|n| n.eq_ignore_ascii_case("SKILL.md"))
    }) else {
        return (None, None);
    };
    let text = String::from_utf8_lossy(&md.3);
    let mut name = None;
    let mut desc = None;
    if let Some(fm) = text
        .strip_prefix("---")
        .and_then(|r| r.split("\n---").next())
    {
        for l in fm.lines() {
            if let Some(v) = l.strip_prefix("name:") {
                name = Some(v.trim().trim_matches('"').to_string());
            }
            if let Some(v) = l.strip_prefix("description:") {
                desc = Some(v.trim().trim_matches('"').to_string());
            }
        }
    }
    (name, desc)
}

fn new_version(
    owner: &str,
    skill_id: &str,
    n: u64,
    files: &[(String, String, String, Vec<u8>)],
    name: &str,
    desc: &str,
) -> Value {
    let ids: Vec<Value> = files
        .iter()
        .map(|f| json!(keep_file(owner, f, "skills")))
        .collect();
    let vid = format!("skillver_{}", uuid::Uuid::now_v7().simple());
    json!({"id": vid, "object": "skill.version", "created_at": now(), "description": desc, "name": name,
           "skill_id": skill_id, "version": n.to_string(), "file_ids": ids})
}

fn public_version(v: &Value) -> Value {
    let mut v = v.clone();
    if let Some(o) = v.as_object_mut() {
        o.remove("file_ids");
    }
    v
}

pub async fn create_skill(
    principal: AuthPrincipal,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let (f, files) = match form(&headers, body).await {
        Ok(r) => r,
        Err(e) => return err(StatusCode::BAD_REQUEST, &e),
    };
    let owner = principal.agent.id.clone();
    let (mname, mdesc) = skill_meta(&files);
    let name = s(&f, "name")
        .or(mname)
        .or_else(|| {
            files
                .first()
                .map(|x| x.1.split('/').next().unwrap_or("skill").to_string())
        })
        .unwrap_or_else(|| "skill".into());
    let desc = s(&f, "description").or(mdesc).unwrap_or_default();
    let id = format!("skill_{}", uuid::Uuid::now_v7().simple());
    let v = new_version(&owner, &id, 1, &files, &name, &desc);
    let obj = json!({"id": id, "object": "skill", "created_at": now(), "default_version": "1", "latest_version": "1", "name": name, "description": desc});
    stored::put(
        &id,
        Stored {
            owner,
            obj: obj.clone(),
            input: vec![v],
            history: Vec::new(),
        },
    );
    Json(obj).into_response()
}

fn get_skill(owner: &str, id: &str) -> Option<Stored> {
    stored::get(owner, id).filter(|s| s.obj["object"] == "skill")
}

pub async fn list_skills(principal: AuthPrincipal, Query(q): Query<ListQuery>) -> Response {
    let items = stored::list(&principal.agent.id, "skill")
        .into_iter()
        .map(|(_, s)| s.obj)
        .collect();
    Json(page(items, &q)).into_response()
}

pub async fn retrieve_skill(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    match get_skill(&principal.agent.id, &id) {
        Some(s) => Json(s.obj).into_response(),
        None => stored::not_found("skill", &id),
    }
}

pub async fn update_skill(
    principal: AuthPrincipal,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> Response {
    let Some(sk) = get_skill(&principal.agent.id, &id) else {
        return stored::not_found("skill", &id);
    };
    if let Some(dv) = b.get("default_version").map(|v| {
        v.as_str()
            .map(String::from)
            .unwrap_or_else(|| v.to_string())
    }) {
        if !sk.input.iter().any(|v| v["version"] == json!(dv)) {
            return err(StatusCode::BAD_REQUEST, &format!("no version {dv}"));
        }
        stored::update(&id, |s| s.obj["default_version"] = json!(dv));
    }
    retrieve_skill(principal, Path(id)).await
}

pub async fn delete_skill(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    if get_skill(&principal.agent.id, &id).is_some() && stored::remove(&principal.agent.id, &id) {
        Json(json!({"id": id, "object": "skill.deleted", "deleted": true})).into_response()
    } else {
        stored::not_found("skill", &id)
    }
}

/// The files of a version: one file as itself, several as a list of names
/// and contents (no zip writer here; JSON keeps every file).
fn content_of(v: &Value) -> Response {
    let files: Vec<crate::api::files::StoredFile> = v["file_ids"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|f| f.as_str().and_then(crate::api::files::get))
        .collect();
    if files.len() == 1 {
        let f = &files[0];
        return ([(header::CONTENT_TYPE, f.mime.clone())], f.bytes.clone()).into_response();
    }
    Json(json!({"files": files.iter().map(|f| json!({"name": f.filename, "content": String::from_utf8_lossy(&f.bytes)})).collect::<Vec<_>>()})).into_response()
}

pub async fn skill_content(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    let Some(sk) = get_skill(&principal.agent.id, &id) else {
        return stored::not_found("skill", &id);
    };
    let dv = sk.obj["default_version"].clone();
    match sk.input.iter().find(|v| v["version"] == dv) {
        Some(v) => content_of(v),
        None => stored::not_found("skill version", dv.as_str().unwrap_or("")),
    }
}

pub async fn create_version(
    principal: AuthPrincipal,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let owner = principal.agent.id.clone();
    let Some(sk) = get_skill(&owner, &id) else {
        return stored::not_found("skill", &id);
    };
    let (f, files) = match form(&headers, body).await {
        Ok(r) => r,
        Err(e) => return err(StatusCode::BAD_REQUEST, &e),
    };
    let n = sk.obj["latest_version"]
        .as_str()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0)
        + 1;
    let (mname, mdesc) = skill_meta(&files);
    let name = mname.unwrap_or_else(|| sk.obj["name"].as_str().unwrap_or("").to_string());
    let desc = mdesc.unwrap_or_else(|| sk.obj["description"].as_str().unwrap_or("").to_string());
    let v = new_version(&owner, &id, n, &files, &name, &desc);
    let make_default = f
        .get("default")
        .and_then(|d| d.as_str().map(|x| x == "true").or(d.as_bool()))
        .unwrap_or(false);
    let vc = v.clone();
    stored::update(&id, |s| {
        s.input.push(vc);
        s.obj["latest_version"] = json!(n.to_string());
        if make_default {
            s.obj["default_version"] = json!(n.to_string());
        }
    });
    Json(public_version(&v)).into_response()
}

pub async fn list_versions(
    principal: AuthPrincipal,
    Path(id): Path<String>,
    Query(q): Query<ListQuery>,
) -> Response {
    match get_skill(&principal.agent.id, &id) {
        Some(s) => Json(page(s.input.iter().map(public_version).collect(), &q)).into_response(),
        None => stored::not_found("skill", &id),
    }
}

pub async fn get_version(
    principal: AuthPrincipal,
    Path((id, ver)): Path<(String, String)>,
) -> Response {
    match get_skill(&principal.agent.id, &id)
        .and_then(|s| s.input.into_iter().find(|v| v["version"] == json!(ver)))
    {
        Some(v) => Json(public_version(&v)).into_response(),
        None => stored::not_found("skill version", &ver),
    }
}

pub async fn delete_version(
    principal: AuthPrincipal,
    Path((id, ver)): Path<(String, String)>,
) -> Response {
    let Some(sk) = get_skill(&principal.agent.id, &id) else {
        return stored::not_found("skill", &id);
    };
    if !sk.input.iter().any(|v| v["version"] == json!(ver)) {
        return stored::not_found("skill version", &ver);
    }
    if sk.obj["default_version"] == json!(ver) {
        return err(StatusCode::BAD_REQUEST, "cannot delete the default version");
    }
    let vid = sk
        .input
        .iter()
        .find(|v| v["version"] == json!(ver))
        .map(|v| v["id"].clone())
        .unwrap_or(Value::Null);
    stored::update(&id, |s| s.input.retain(|v| v["version"] != json!(ver)));
    Json(json!({"id": vid, "object": "skill.version.deleted", "deleted": true, "version": ver}))
        .into_response()
}

pub async fn version_content(
    principal: AuthPrincipal,
    Path((id, ver)): Path<(String, String)>,
) -> Response {
    match get_skill(&principal.agent.id, &id)
        .and_then(|s| s.input.into_iter().find(|v| v["version"] == json!(ver)))
    {
        Some(v) => content_of(&v),
        None => stored::not_found("skill version", &ver),
    }
}
