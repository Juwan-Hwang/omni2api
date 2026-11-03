//! `/v1/vector_stores`: stores of files the seat searches. Files are kept
//! as they were uploaded (no embeddings of our own); a search is a seat
//! task with the store's files attached and the query as the message, and
//! the seat answers with ranked passages, checked at reply time.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::api::stored::{self, Stored};
use crate::api::task::{self, Delivery, Finished};
use crate::auth::AuthPrincipal;
use crate::storage::AppState;

fn err(status: StatusCode, msg: &str) -> Response {
    (
        status,
        Json(json!({"error": {"message": msg, "type": "invalid_request_error", "code": null, "param": null}})),
    )
        .into_response()
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn get_store(owner: &str, id: &str) -> Option<Stored> {
    stored::get(owner, id).filter(|s| s.obj["object"] == "vector_store")
}

/// Recount the store's files and bytes after a change.
fn refresh(id: &str) {
    stored::update(id, |s| {
        let total = s.input.len();
        let bytes: u64 = s
            .input
            .iter()
            .filter_map(|f| f["usage_bytes"].as_u64())
            .sum();
        s.obj["file_counts"] = json!({"in_progress": 0, "completed": total, "failed": 0, "cancelled": 0, "total": total});
        s.obj["usage_bytes"] = json!(bytes);
        s.obj["last_active_at"] = json!(now());
    });
}

/// A file entry for a store, when the file exists and is the caller's.
fn file_entry(
    owner: &str,
    vs: &str,
    fid: &str,
    attributes: Value,
    chunking: Value,
) -> Option<Value> {
    let f = crate::api::files::get(fid).filter(|f| f.owner == owner)?;
    Some(json!({
        "id": fid,
        "object": "vector_store.file",
        "usage_bytes": f.bytes.len(),
        "created_at": now(),
        "vector_store_id": vs,
        "status": "completed",
        "last_error": null,
        "attributes": attributes,
        "chunking_strategy": if chunking.is_null() { json!({"type": "static", "static": {"max_chunk_size_tokens": 800, "chunk_overlap_tokens": 400}}) } else { chunking },
    }))
}

fn add_files(
    owner: &str,
    vs: &str,
    files: &[(String, Value, Value)],
) -> Result<Vec<Value>, String> {
    let mut added = Vec::new();
    for (fid, attrs, chunking) in files {
        let e = file_entry(owner, vs, fid, attrs.clone(), chunking.clone())
            .ok_or(format!("No file found with id '{fid}'."))?;
        added.push(e);
    }
    let ids: Vec<String> = added
        .iter()
        .map(|e| e["id"].as_str().unwrap_or("").to_string())
        .collect();
    let new = added.clone();
    stored::update(vs, |s| {
        s.input.retain(|f| !ids.iter().any(|i| f["id"] == json!(i)));
        s.input.extend(new);
    });
    refresh(vs);
    Ok(added)
}

#[derive(Debug, serde::Deserialize, Default)]
pub struct ListQuery {
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    order: Option<String>,
    #[serde(default)]
    after: Option<String>,
    #[serde(default)]
    filter: Option<String>,
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

// ── stores ──

pub async fn create(principal: AuthPrincipal, body: Option<Json<Value>>) -> Response {
    let b = body.map(|Json(b)| b).unwrap_or(json!({}));
    let owner = principal.agent.id.clone();
    let id = format!("vs_{}", uuid::Uuid::now_v7().simple());
    let t = now();
    let obj = json!({
        "id": id,
        "object": "vector_store",
        "created_at": t,
        "name": b.get("name").cloned().unwrap_or(Value::Null),
        "usage_bytes": 0,
        "file_counts": {"in_progress": 0, "completed": 0, "failed": 0, "cancelled": 0, "total": 0},
        "status": "completed",
        "expires_after": b.get("expires_after").cloned().unwrap_or(Value::Null),
        "expires_at": null,
        "last_active_at": t,
        "metadata": b.get("metadata").cloned().unwrap_or(json!({})),
    });
    stored::put(
        &id,
        Stored {
            owner: owner.clone(),
            obj,
            input: Vec::new(),
            history: Vec::new(),
        },
    );
    let files: Vec<(String, Value, Value)> = b
        .get("file_ids")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .map(|f| {
                    (
                        f.to_string(),
                        json!({}),
                        b.get("chunking_strategy").cloned().unwrap_or(Value::Null),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    if let Err(e) = add_files(&owner, &id, &files) {
        stored::remove(&owner, &id);
        return err(StatusCode::NOT_FOUND, &e);
    }
    Json(get_store(&owner, &id).map(|s| s.obj)).into_response()
}

pub async fn list(principal: AuthPrincipal, Query(q): Query<ListQuery>) -> Response {
    let items = stored::list(&principal.agent.id, "vector_store")
        .into_iter()
        .map(|(_, s)| s.obj)
        .collect();
    Json(page(items, &q)).into_response()
}

pub async fn retrieve(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    match get_store(&principal.agent.id, &id) {
        Some(s) => Json(s.obj).into_response(),
        None => stored::not_found("vector store", &id),
    }
}

pub async fn modify(
    principal: AuthPrincipal,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> Response {
    if get_store(&principal.agent.id, &id).is_none() {
        return stored::not_found("vector store", &id);
    }
    stored::update(&id, |s| {
        for k in ["name", "metadata", "expires_after"] {
            if let Some(v) = b.get(k) {
                s.obj[k] = v.clone();
            }
        }
    });
    retrieve(principal, Path(id)).await
}

pub async fn delete(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    if get_store(&principal.agent.id, &id).is_some() && stored::remove(&principal.agent.id, &id) {
        Json(json!({"id": id, "object": "vector_store.deleted", "deleted": true})).into_response()
    } else {
        stored::not_found("vector store", &id)
    }
}

// ── files ──

pub async fn add_file(
    principal: AuthPrincipal,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> Response {
    if get_store(&principal.agent.id, &id).is_none() {
        return stored::not_found("vector store", &id);
    }
    let Some(fid) = b.get("file_id").and_then(|v| v.as_str()) else {
        return err(StatusCode::BAD_REQUEST, "file_id is required");
    };
    let attrs = b.get("attributes").cloned().unwrap_or(json!({}));
    let chunking = b.get("chunking_strategy").cloned().unwrap_or(Value::Null);
    match add_files(
        &principal.agent.id,
        &id,
        &[(fid.to_string(), attrs, chunking)],
    ) {
        Ok(mut v) => Json(v.remove(0)).into_response(),
        Err(e) => err(StatusCode::NOT_FOUND, &e),
    }
}

pub async fn list_files(
    principal: AuthPrincipal,
    Path(id): Path<String>,
    Query(q): Query<ListQuery>,
) -> Response {
    let Some(s) = get_store(&principal.agent.id, &id) else {
        return stored::not_found("vector store", &id);
    };
    let items = s
        .input
        .into_iter()
        .filter(|f| {
            q.filter
                .as_deref()
                .is_none_or(|st| f["status"] == json!(st))
        })
        .collect();
    Json(page(items, &q)).into_response()
}

pub async fn get_file(
    principal: AuthPrincipal,
    Path((id, fid)): Path<(String, String)>,
) -> Response {
    match get_store(&principal.agent.id, &id)
        .and_then(|s| s.input.into_iter().find(|f| f["id"] == json!(fid)))
    {
        Some(f) => Json(f).into_response(),
        None => stored::not_found("vector store file", &fid),
    }
}

pub async fn update_file(
    principal: AuthPrincipal,
    Path((id, fid)): Path<(String, String)>,
    Json(b): Json<Value>,
) -> Response {
    let found = get_store(&principal.agent.id, &id)
        .is_some_and(|s| s.input.iter().any(|f| f["id"] == json!(fid)));
    if !found {
        return stored::not_found("vector store file", &fid);
    }
    let attrs = b.get("attributes").cloned().unwrap_or(json!({}));
    stored::update(&id, |s| {
        for f in s.input.iter_mut().filter(|f| f["id"] == json!(fid)) {
            f["attributes"] = attrs.clone();
        }
    });
    get_file(principal, Path((id, fid))).await
}

pub async fn delete_file(
    principal: AuthPrincipal,
    Path((id, fid)): Path<(String, String)>,
) -> Response {
    let found = get_store(&principal.agent.id, &id)
        .is_some_and(|s| s.input.iter().any(|f| f["id"] == json!(fid)));
    if !found {
        return stored::not_found("vector store file", &fid);
    }
    stored::update(&id, |s| s.input.retain(|f| f["id"] != json!(fid)));
    refresh(&id);
    Json(json!({"id": fid, "object": "vector_store.file.deleted", "deleted": true})).into_response()
}

pub async fn file_content(
    principal: AuthPrincipal,
    Path((id, fid)): Path<(String, String)>,
) -> Response {
    let found = get_store(&principal.agent.id, &id)
        .is_some_and(|s| s.input.iter().any(|f| f["id"] == json!(fid)));
    let file = crate::api::files::get(&fid).filter(|_| found);
    match file {
        Some(f) => Json(json!({
            "object": "vector_store.file_content.page",
            "data": [{"type": "text", "text": String::from_utf8_lossy(&f.bytes)}],
            "has_more": false,
            "next_page": null,
        }))
        .into_response(),
        None => stored::not_found("vector store file", &fid),
    }
}

// ── file batches ──

pub async fn create_batch(
    principal: AuthPrincipal,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> Response {
    if get_store(&principal.agent.id, &id).is_none() {
        return stored::not_found("vector store", &id);
    }
    let chunking = b.get("chunking_strategy").cloned().unwrap_or(Value::Null);
    let attrs = b.get("attributes").cloned().unwrap_or(json!({}));
    let mut files: Vec<(String, Value, Value)> = b
        .get("file_ids")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .map(|f| (f.to_string(), attrs.clone(), chunking.clone()))
                .collect()
        })
        .unwrap_or_default();
    for f in b
        .get("files")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
    {
        if let Some(fid) = f.get("file_id").and_then(|v| v.as_str()) {
            files.push((
                fid.to_string(),
                f.get("attributes").cloned().unwrap_or(attrs.clone()),
                f.get("chunking_strategy")
                    .cloned()
                    .unwrap_or(chunking.clone()),
            ));
        }
    }
    if files.is_empty() {
        return err(StatusCode::BAD_REQUEST, "file_ids or files is required");
    }
    let added = match add_files(&principal.agent.id, &id, &files) {
        Ok(a) => a,
        Err(e) => return err(StatusCode::NOT_FOUND, &e),
    };
    let bid = format!("vsfb_{}", uuid::Uuid::now_v7().simple());
    let n = added.len();
    let obj = json!({
        "id": bid,
        "object": "vector_store.files_batch",
        "created_at": now(),
        "vector_store_id": id,
        "status": "completed",
        "file_counts": {"in_progress": 0, "completed": n, "failed": 0, "cancelled": 0, "total": n},
    });
    stored::put(
        &bid,
        Stored {
            owner: principal.agent.id.clone(),
            obj: obj.clone(),
            input: added,
            history: Vec::new(),
        },
    );
    Json(obj).into_response()
}

fn get_batch(owner: &str, id: &str, bid: &str) -> Option<Stored> {
    stored::get(owner, bid).filter(|s| {
        s.obj["object"] == "vector_store.files_batch" && s.obj["vector_store_id"] == json!(id)
    })
}

pub async fn get_batch_route(
    principal: AuthPrincipal,
    Path((id, bid)): Path<(String, String)>,
) -> Response {
    match get_batch(&principal.agent.id, &id, &bid) {
        Some(s) => Json(s.obj).into_response(),
        None => stored::not_found("vector store file batch", &bid),
    }
}

/// Batches complete as they are created, so there is nothing to stop.
pub async fn cancel_batch(
    principal: AuthPrincipal,
    Path((id, bid)): Path<(String, String)>,
) -> Response {
    get_batch_route(principal, Path((id, bid))).await
}

pub async fn batch_files(
    principal: AuthPrincipal,
    Path((id, bid)): Path<(String, String)>,
    Query(q): Query<ListQuery>,
) -> Response {
    match get_batch(&principal.agent.id, &id, &bid) {
        Some(s) => Json(page(s.input, &q)).into_response(),
        None => stored::not_found("vector store file batch", &bid),
    }
}

// ── search ──

/// The hits in a seat's answer, checked against the store's files
/// (`spec.files`: `[{id, filename}]`) and the result cap.
pub fn parse_hits(text: &str, spec: &Value) -> Result<Vec<Value>, String> {
    let max = spec.get("max").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
    let files = spec
        .get("files")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let v: Value = serde_json::from_str(text.trim()).map_err(|_| {
        "answer with JSON: [{\"file\": \"<file id or name>\", \"score\": 0.9, \"text\": \"the passage\"}] best first, [] for none".to_string()
    })?;
    let list = v
        .get("data")
        .or_else(|| v.get("results"))
        .cloned()
        .unwrap_or(v);
    let list = list
        .as_array()
        .ok_or("answer with a JSON list of hits")?
        .clone();
    if list.len() > max {
        return Err(format!("at most {max} hits; got {}", list.len()));
    }
    list.iter()
        .enumerate()
        .map(|(i, h)| {
            let f = h
                .get("file")
                .or_else(|| h.get("file_id"))
                .or_else(|| h.get("filename"))
                .and_then(|v| v.as_str())
                .ok_or(format!("hit {}: `file` (id or name) is required", i + 1))?;
            let file = files
                .iter()
                .find(|x| x["id"] == json!(f) || x["filename"] == json!(f))
                .ok_or(format!("hit {}: `{f}` is not a file of this store", i + 1))?;
            let text = h
                .get("text")
                .or_else(|| h.get("content"))
                .and_then(|v| v.as_str())
                .ok_or(format!("hit {}: `text` (the passage) is required", i + 1))?;
            let score = h
                .get("score")
                .and_then(|v| v.as_f64())
                .unwrap_or(1.0 - i as f64 * 0.01);
            if !(0.0..=1.0).contains(&score) {
                return Err(format!("hit {}: score must be within 0..1", i + 1));
            }
            Ok(json!({
                "file_id": file["id"],
                "filename": file["filename"],
                "score": score,
                "attributes": file.get("attributes").cloned().unwrap_or(json!({})),
                "content": [{"type": "text", "text": text}],
            }))
        })
        .collect()
}

pub async fn search(
    State(app): State<AppState>,
    principal: AuthPrincipal,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> Response {
    let Some(s) = get_store(&principal.agent.id, &id) else {
        return stored::not_found("vector store", &id);
    };
    let queries: Vec<String> = match b.get("query") {
        Some(Value::String(q)) => vec![q.clone()],
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect(),
        _ => return err(StatusCode::BAD_REQUEST, "query is required"),
    };
    if queries.is_empty() {
        return err(StatusCode::BAD_REQUEST, "query is required");
    }
    let max = b
        .get("max_num_results")
        .and_then(|v| v.as_u64())
        .unwrap_or(10)
        .clamp(1, 50);
    // Filters are enforced here, on the files' attributes; the seat only
    // ever sees files that passed.
    let filter = b.get("filters").filter(|f| !f.is_null());
    if let Some(f) = filter {
        if let Err(e) = check_filter(f) {
            return err(StatusCode::BAD_REQUEST, &e);
        }
    }
    let mut candidates = Vec::new();
    for (order, e) in s.input.iter().enumerate() {
        if filter.is_some_and(|f| !filter_matches(f, &e["attributes"])) {
            continue;
        }
        let fid = e["id"].as_str().unwrap_or("");
        if let Some(f) = crate::api::files::get(fid) {
            candidates.push(Candidate {
                rel: relevance(&f.bytes, &queries),
                order,
                id: fid.to_string(),
                attributes: e["attributes"].clone(),
                file: f,
            });
        }
    }
    let matching = candidates.len();
    let limit = search_max_bytes();
    let chosen = pick_within(candidates, limit);
    let mut files = Vec::new();
    let mut attach = Vec::new();
    for Candidate {
        id,
        attributes,
        file: f,
        ..
    } in &chosen
    {
        files.push(json!({"id": id, "filename": f.filename, "attributes": attributes}));
        attach.push(crate::openai_compat::named_data_url(
            &f.mime,
            Some(&f.filename),
            &crate::openai_compat::b64_encode(&f.bytes),
        ));
    }
    let mut want = format!(
        "search the {} attached file(s) for the query; answer with JSON, best first, at most {max}: [{{\"file\": \"<file name or id>\", \"score\": 0..1, \"text\": \"the matching passage\"}}], [] when nothing matches",
        files.len()
    );
    if files.len() < matching {
        want.push_str(&format!(
            "; truncated: {} of the store's {matching} matching files are attached (the most relevant to the query, then the newest, within {limit} bytes); the rest were left out",
            files.len()
        ));
    }
    let spec = json!({"max": max, "files": files});
    let spec_c = spec.clone();
    let q_c = queries.clone();
    let finish: task::Finish = Box::new(move |answer| {
        let a = match answer {
            Ok(a) => a,
            Err(e) => {
                return Finished::Json(
                    StatusCode::GATEWAY_TIMEOUT,
                    json!({"error": {"message": e, "type": "server_error", "code": "seat_unavailable"}}),
                )
            }
        };
        match parse_hits(&a.raw, &spec_c) {
            Ok(hits) => Finished::Json(
                StatusCode::OK,
                json!({"object": "vector_store.search_results.page", "search_query": q_c, "data": hits, "has_more": false, "next_page": null}),
            ),
            Err(e) => Finished::Json(
                StatusCode::BAD_GATEWAY,
                json!({"error": {"message": e, "type": "server_error", "code": "bad_answer"}}),
            ),
        }
    });
    stored::update(&id, |s| s.obj["last_active_at"] = json!(now()));
    task::run(
        app,
        principal,
        headers,
        "vector_stores.search",
        None,
        queries.join("\n"),
        attach,
        want,
        false,
        spec,
        Delivery::Json,
        finish,
    )
    .await
}

/// Total bytes of files one search may attach
/// (`OMNI2API_VECTOR_SEARCH_MAX_BYTES`, default 16 MiB).
fn search_max_bytes() -> usize {
    std::env::var("OMNI2API_VECTOR_SEARCH_MAX_BYTES")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(16 * 1024 * 1024)
}

/// How often the query's words occur in a file read as text (0 for
/// binary content): a cheap ranking for which files to attach first.
fn relevance(bytes: &[u8], queries: &[String]) -> usize {
    let text = String::from_utf8_lossy(bytes).to_lowercase();
    queries
        .iter()
        .flat_map(|q| q.split_whitespace())
        .map(|w| w.to_lowercase())
        .filter(|w| w.chars().count() > 1)
        .map(|w| text.matches(w.as_str()).count())
        .sum()
}

/// A store file a search may attach.
struct Candidate {
    /// Query-word hits (see `relevance`).
    rel: usize,
    /// Position in the store: later is newer.
    order: usize,
    id: String,
    attributes: Value,
    file: crate::api::files::StoredFile,
}

/// The candidates that fit within `limit` bytes: most relevant first,
/// newest first among equals; a file that does not fit is skipped.
fn pick_within(mut c: Vec<Candidate>, limit: usize) -> Vec<Candidate> {
    c.sort_by(|a, b| b.rel.cmp(&a.rel).then(b.order.cmp(&a.order)));
    let mut used = 0;
    c.into_iter()
        .filter(|x| {
            let fits = used + x.file.bytes.len() <= limit;
            if fits {
                used += x.file.bytes.len();
            }
            fits
        })
        .collect()
}

/// A filter is a comparison (`eq ne gt gte lt lte in nin` on `key`) or a
/// compound (`and`/`or` over `filters`), as in OpenAI's file search.
fn check_filter(f: &Value) -> Result<(), String> {
    match f["type"].as_str().unwrap_or("") {
        "and" | "or" => match f["filters"].as_array() {
            Some(a) => a.iter().try_for_each(check_filter),
            None => Err("a compound filter needs filters".into()),
        },
        "eq" | "ne" | "gt" | "gte" | "lt" | "lte" | "in" | "nin" => {
            if f["key"].as_str().is_none() {
                return Err("a comparison filter needs key".into());
            }
            if f.get("value").is_none() {
                return Err("a comparison filter needs value".into());
            }
            Ok(())
        }
        other => Err(format!("unknown filter type {other:?}")),
    }
}

/// Whether attributes pass a (checked) filter.
pub fn filter_matches(f: &Value, attrs: &Value) -> bool {
    let num = |v: &Value| v.as_f64();
    match f["type"].as_str().unwrap_or("") {
        "and" => f["filters"]
            .as_array()
            .is_some_and(|a| a.iter().all(|x| filter_matches(x, attrs))),
        "or" => f["filters"]
            .as_array()
            .is_some_and(|a| a.iter().any(|x| filter_matches(x, attrs))),
        op => {
            let key = f["key"].as_str().unwrap_or("");
            let want = &f["value"];
            let have = attrs.get(key);
            let eq = |h: &Value, w: &Value| match (num(h), num(w)) {
                (Some(a), Some(b)) => a == b,
                _ => h == w,
            };
            match (op, have) {
                ("eq", Some(h)) => eq(h, want),
                ("eq", None) => false,
                ("ne", Some(h)) => !eq(h, want),
                ("ne", None) => true,
                ("in", Some(h)) => want.as_array().is_some_and(|a| a.iter().any(|w| eq(h, w))),
                ("in", None) => false,
                ("nin", Some(h)) => !want.as_array().is_some_and(|a| a.iter().any(|w| eq(h, w))),
                ("nin", None) => true,
                (cmp, Some(h)) => match (num(h), num(want)) {
                    (Some(a), Some(b)) => match cmp {
                        "gt" => a > b,
                        "gte" => a >= b,
                        "lt" => a < b,
                        "lte" => a <= b,
                        _ => false,
                    },
                    _ => false,
                },
                _ => false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hits_name_store_files() {
        let spec = json!({"max": 2, "files": [{"id": "file-1", "filename": "a.txt"}]});
        let h = parse_hits(r#"[{"file": "a.txt", "score": 0.8, "text": "x"}]"#, &spec).unwrap();
        assert_eq!(h[0]["file_id"], "file-1");
        assert!(parse_hits(r#"[{"file": "b.txt", "text": "x"}]"#, &spec)
            .unwrap_err()
            .contains("not a file"));
        assert!(parse_hits("[]", &spec).unwrap().is_empty());
        assert!(parse_hits(r#"[{"file":"a.txt","text":"1"},{"file":"a.txt","text":"2"},{"file":"a.txt","text":"3"}]"#, &spec).is_err());
    }

    #[test]
    fn filters_are_enforced_on_attributes() {
        let a = json!({"author": "kim", "year": 2024, "draft": false});
        let eq = |k: &str, v: Value| json!({"type": "eq", "key": k, "value": v});
        assert!(filter_matches(&eq("author", json!("kim")), &a));
        assert!(!filter_matches(&eq("author", json!("lee")), &a));
        assert!(!filter_matches(&eq("missing", json!("kim")), &a));
        assert!(filter_matches(&eq("year", json!(2024.0)), &a));
        assert!(filter_matches(&eq("draft", json!(false)), &a));
        assert!(filter_matches(
            &json!({"type": "ne", "key": "author", "value": "lee"}),
            &a
        ));
        assert!(filter_matches(
            &json!({"type": "gte", "key": "year", "value": 2024}),
            &a
        ));
        assert!(!filter_matches(
            &json!({"type": "lt", "key": "year", "value": 2024}),
            &a
        ));
        assert!(filter_matches(
            &json!({"type": "in", "key": "author", "value": ["x", "kim"]}),
            &a
        ));
        assert!(!filter_matches(
            &json!({"type": "nin", "key": "author", "value": ["kim"]}),
            &a
        ));
        let and = json!({"type": "and", "filters": [eq("author", json!("kim")), eq("year", json!(2023))]});
        assert!(!filter_matches(&and, &a));
        let or =
            json!({"type": "or", "filters": [eq("author", json!("lee")), eq("year", json!(2024))]});
        assert!(filter_matches(&or, &a));
        assert!(check_filter(&json!({"type": "like", "key": "a", "value": 1})).is_err());
        assert!(check_filter(&json!({"type": "eq", "value": 1})).is_err());
        assert!(check_filter(&and).is_ok());
    }

    #[test]
    fn attachments_fit_the_byte_limit_most_relevant_first() {
        let q = vec!["rust borrow".to_string()];
        let f = |order: usize, id: &str, body: &str| Candidate {
            rel: relevance(body.as_bytes(), &q),
            order,
            id: id.into(),
            attributes: json!({}),
            file: crate::api::files::StoredFile {
                owner: "o".into(),
                filename: format!("{id}.txt"),
                mime: "text/plain".into(),
                purpose: "assistants".into(),
                bytes: body.as_bytes().to_vec(),
                created_at: 0,
            },
        };
        let c: Vec<_> = [
            ("old", "rust borrow rust"),  // relevant, older
            ("new", "nothing here...."),  // newest, irrelevant
            ("mid", "rust only here..."), // somewhat relevant
        ]
        .into_iter()
        .enumerate()
        .map(|(i, (id, body))| f(i, id, body))
        .collect();
        // Room for two of the three 16-17 byte files.
        let picked = pick_within(c, 34);
        let ids: Vec<_> = picked.iter().map(|x| x.id.as_str()).collect();
        assert_eq!(ids, ["old", "mid"]);
        // Equal relevance: the newest wins.
        let c: Vec<_> = (0..3)
            .map(|i| f(i, &format!("f{i}"), "xxxxxxxxxx"))
            .collect();
        let picked = pick_within(c, 10);
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].id, "f2");
    }
}
