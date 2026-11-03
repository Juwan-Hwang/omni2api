//! `/v1/batches`: a JSONL file of requests run through this server's own
//! endpoints (each line a normal request the seat answers), results
//! collected into an output file and an error file.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::api::stored::{self, Stored};
use crate::auth::AuthPrincipal;
use crate::storage::AppState;

const ENDPOINTS: &[&str] = &[
    "/v1/responses",
    "/v1/chat/completions",
    "/v1/embeddings",
    "/v1/completions",
    "/v1/moderations",
];

/// How many lines of a batch run at once.
const PARALLEL: usize = 4;

/// The completion window: a batch expires this long after it is created.
const WINDOW_SECS: i64 = 86_400;

/// Most request lines one batch may hold (`OMNI2API_BATCH_MAX_LINES`,
/// default 50 000 as at OpenAI).
fn max_lines() -> usize {
    std::env::var("OMNI2API_BATCH_MAX_LINES")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(50_000)
}

/// What became of one line.
#[derive(Debug, Clone, PartialEq)]
enum Outcome {
    Done(u16, Value),
    /// Not answered before `expires_at`.
    Expired,
}

/// Run every line through `f`, `PARALLEL` at a time, in order; a line
/// still unanswered at `deadline` (or not started by then) is `Expired`.
async fn run_lines<F, Fut>(
    lines: Vec<Value>,
    deadline: tokio::time::Instant,
    f: F,
) -> Vec<(Value, Outcome)>
where
    F: Fn(Value) -> Fut,
    Fut: std::future::Future<Output = (u16, Value)>,
{
    use futures::StreamExt;
    futures::stream::iter(lines)
        .map(|line| {
            let fut = f(line.clone());
            async move {
                match tokio::time::timeout_at(deadline, fut).await {
                    Ok((status, v)) => (line, Outcome::Done(status, v)),
                    Err(_) => (line, Outcome::Expired),
                }
            }
        })
        .buffered(PARALLEL)
        .collect()
        .await
}

/// The output and error file lines for the outcomes.
fn result_lines(results: Vec<(Value, Outcome)>) -> (Vec<Value>, Vec<Value>) {
    let (mut out, mut errs) = (Vec::new(), Vec::new());
    for (line, outcome) in results {
        let custom_id = line["custom_id"].clone();
        let rid = format!("batch_req_{}", uuid::Uuid::now_v7().simple());
        match outcome {
            Outcome::Done(status, v) if status < 400 => out.push(json!({"id": rid, "custom_id": custom_id,
                "response": {"status_code": status, "request_id": rid, "body": v}, "error": null})),
            Outcome::Done(status, v) => errs.push(json!({"id": rid, "custom_id": custom_id,
                "response": {"status_code": status, "request_id": rid, "body": v},
                "error": {"code": "request_failed", "message": v.pointer("/error/message").cloned().unwrap_or(Value::Null)}})),
            Outcome::Expired => errs.push(json!({"id": rid, "custom_id": custom_id, "response": null,
                "error": {"code": "batch_expired", "message": "This request could not be executed before the completion window expired."}})),
        }
    }
    (out, errs)
}

fn err(status: StatusCode, msg: &str) -> Response {
    (
        status,
        Json(json!({"error": {"message": msg, "type": "invalid_request_error", "code": null, "param": null}})),
    )
        .into_response()
}

/// Run one request line through the endpoint it names.
async fn dispatch(
    app: AppState,
    principal: AuthPrincipal,
    headers: HeaderMap,
    url: &str,
    mut body: Value,
) -> (u16, Value) {
    body["stream"] = json!(false);
    let resp = match url {
        "/v1/chat/completions" => {
            crate::api::stored::chat_create(State(app), principal, headers, Json(body)).await
        }
        "/v1/responses" => {
            crate::api::adapters::responses(State(app), principal, headers, Json(body)).await
        }
        "/v1/embeddings" => {
            crate::api::embeddings::embeddings(State(app), principal, headers, Json(body)).await
        }
        "/v1/completions" => {
            crate::api::completions::completions(State(app), principal, headers, Json(body)).await
        }
        "/v1/moderations" => {
            crate::api::moderations::moderations(State(app), principal, headers, Json(body)).await
        }
        other => {
            return (
                400,
                json!({"error": {"message": format!("unsupported url {other}")}}),
            )
        }
    };
    let status = resp.status().as_u16();
    let bytes = axum::body::to_bytes(resp.into_body(), 256 * 1024 * 1024)
        .await
        .unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes);
    let v = serde_json::from_str(text.trim()).unwrap_or_else(|_| json!({"raw": text.trim()}));
    // A body that carries an error after padding started is still a failure.
    let status = if status == 200 && v.get("error").is_some_and(|e| !e.is_null()) {
        500
    } else {
        status
    };
    (status, v)
}

fn jsonl_file(owner: &str, name: &str, purpose: &str, lines: &[Value]) -> Option<String> {
    if lines.is_empty() {
        return None;
    }
    let mut bytes = Vec::new();
    for l in lines {
        bytes.extend_from_slice(l.to_string().as_bytes());
        bytes.push(b'\n');
    }
    Some(crate::api::files::put(crate::api::files::StoredFile {
        owner: owner.to_string(),
        filename: name.to_string(),
        mime: "application/jsonl".into(),
        purpose: purpose.to_string(),
        bytes,
        created_at: chrono::Utc::now().timestamp(),
    }))
}

/// `POST /v1/batches`
pub async fn create(
    State(app): State<AppState>,
    principal: AuthPrincipal,
    headers: HeaderMap,
    Json(b): Json<Value>,
) -> Response {
    let Some(file_id) = b.get("input_file_id").and_then(|v| v.as_str()) else {
        return err(StatusCode::BAD_REQUEST, "input_file_id is required");
    };
    let Some(endpoint) = b
        .get("endpoint")
        .and_then(|v| v.as_str())
        .filter(|e| ENDPOINTS.contains(e))
    else {
        return err(
            StatusCode::BAD_REQUEST,
            &format!("endpoint must be one of {}", ENDPOINTS.join(", ")),
        );
    };
    let Some(file) = crate::api::files::get(file_id).filter(|f| f.owner == principal.agent.id)
    else {
        return err(
            StatusCode::NOT_FOUND,
            &format!("No file found with id '{file_id}'."),
        );
    };
    let text = String::from_utf8_lossy(&file.bytes).to_string();
    let limit = max_lines();
    let count = text.lines().filter(|l| !l.trim().is_empty()).count();
    if count > limit {
        return err(
            StatusCode::BAD_REQUEST,
            &format!("the input file has {count} requests; a batch holds at most {limit}"),
        );
    }
    let mut lines = Vec::new();
    for (i, l) in text
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
    {
        let v: Value = match serde_json::from_str(l) {
            Ok(v) => v,
            Err(e) => return err(StatusCode::BAD_REQUEST, &format!("line {}: {e}", i + 1)),
        };
        let url = v.get("url").and_then(|u| u.as_str()).unwrap_or("");
        if url != endpoint {
            return err(
                StatusCode::BAD_REQUEST,
                &format!(
                    "line {}: url {url} differs from the batch endpoint {endpoint}",
                    i + 1
                ),
            );
        }
        if v.get("custom_id").and_then(|c| c.as_str()).is_none() {
            return err(
                StatusCode::BAD_REQUEST,
                &format!("line {}: custom_id is required", i + 1),
            );
        }
        lines.push(v);
    }
    if lines.is_empty() {
        return err(StatusCode::BAD_REQUEST, "the input file has no requests");
    }
    let id = format!("batch_{}", uuid::Uuid::now_v7().simple());
    let now = chrono::Utc::now().timestamp();
    let total = lines.len();
    let obj = json!({
        "id": id,
        "object": "batch",
        "endpoint": endpoint,
        "errors": null,
        "input_file_id": file_id,
        "completion_window": b.get("completion_window").cloned().unwrap_or(json!("24h")),
        "status": "in_progress",
        "output_file_id": null,
        "error_file_id": null,
        "created_at": now,
        "in_progress_at": now,
        "expires_at": now + WINDOW_SECS,
        "finalizing_at": null,
        "completed_at": null,
        "failed_at": null,
        "expired_at": null,
        "cancelling_at": null,
        "cancelled_at": null,
        "request_counts": {"total": total, "completed": 0, "failed": 0},
        "metadata": b.get("metadata").cloned().unwrap_or(Value::Null),
    });
    let owner = principal.agent.id.clone();
    stored::put(
        &id,
        Stored {
            owner: owner.clone(),
            obj: obj.clone(),
            input: Vec::new(),
            history: Vec::new(),
        },
    );
    let bid = id.clone();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(WINDOW_SECS as u64);
    let task = tokio::spawn(async move {
        let results = run_lines(lines, deadline, |line| {
            let (app, principal, headers, bid) =
                (app.clone(), principal.clone(), headers.clone(), bid.clone());
            async move {
                // Each line its own conversation, even when two lines
                // start with the same words.
                let mut headers = headers;
                let key = format!("batch:{bid}:{}", line["custom_id"].as_str().unwrap_or(""));
                if let Ok(v) = axum::http::HeaderValue::from_str(&key) {
                    headers.insert("x-omni2api-session-id", v);
                }
                let url = line["url"].as_str().unwrap_or("").to_string();
                let body = line.get("body").cloned().unwrap_or(json!({}));
                let (status, v) = dispatch(app, principal, headers, &url, body).await;
                stored::update(&bid, |s| {
                    let k = if status < 400 { "completed" } else { "failed" };
                    let n = s.obj["request_counts"][k].as_u64().unwrap_or(0);
                    s.obj["request_counts"][k] = json!(n + 1);
                });
                (status, v)
            }
        })
        .await;
        let expired = results
            .iter()
            .filter(|(_, o)| *o == Outcome::Expired)
            .count();
        let (out, errs) = result_lines(results);
        let of = jsonl_file(&owner, &format!("{bid}_output.jsonl"), "batch_output", &out);
        let ef = jsonl_file(&owner, &format!("{bid}_error.jsonl"), "batch_output", &errs);
        let now = chrono::Utc::now().timestamp();
        stored::update(&bid, |s| {
            s.obj["output_file_id"] = json!(of);
            s.obj["error_file_id"] = json!(ef);
            s.obj["finalizing_at"] = json!(now);
            if expired > 0 {
                // Past expires_at: what finished is kept, the rest is
                // reported as batch_expired in the error file.
                s.obj["status"] = json!("expired");
                s.obj["expired_at"] = json!(now);
                let n = s.obj["request_counts"]["failed"].as_u64().unwrap_or(0);
                s.obj["request_counts"]["failed"] = json!(n + expired as u64);
            } else {
                s.obj["status"] = json!("completed");
                s.obj["completed_at"] = json!(now);
            }
        });
        stored::done_running(&bid);
    });
    stored::set_running(&id, task.abort_handle());
    Json(obj).into_response()
}

pub async fn retrieve(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    match stored::get(&principal.agent.id, &id).filter(|s| s.obj["object"] == "batch") {
        Some(s) => Json(s.obj).into_response(),
        None => stored::not_found("batch", &id),
    }
}

/// `POST /v1/batches/:id/cancel`: lines not answered yet are dropped.
pub async fn cancel(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    let Some(s) = stored::get(&principal.agent.id, &id).filter(|s| s.obj["object"] == "batch")
    else {
        return stored::not_found("batch", &id);
    };
    if s.obj["status"] == "in_progress" && stored::abort_running(&id) {
        let now = chrono::Utc::now().timestamp();
        stored::update(&id, |s| {
            s.obj["status"] = json!("cancelled");
            s.obj["cancelling_at"] = json!(now);
            s.obj["cancelled_at"] = json!(now);
        });
    }
    retrieve(principal, Path(id)).await
}

#[derive(Debug, serde::Deserialize, Default)]
pub struct ListQuery {
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    after: Option<String>,
}

/// `GET /v1/batches`, newest first.
pub async fn list(principal: AuthPrincipal, Query(q): Query<ListQuery>) -> Response {
    let mut items: Vec<Value> = stored::list(&principal.agent.id, "batch")
        .into_iter()
        .map(|(_, s)| s.obj)
        .collect();
    items.reverse();
    if let Some(a) = &q.after {
        if let Some(i) = items.iter().position(|x| x["id"] == json!(a)) {
            items.drain(..=i);
        }
    }
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let has_more = items.len() > limit;
    items.truncate(limit);
    let id_of = |x: Option<&Value>| x.and_then(|v| v.get("id")).cloned().unwrap_or(Value::Null);
    Json(json!({"object": "list", "first_id": id_of(items.first()), "last_id": id_of(items.last()), "has_more": has_more, "data": items}))
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn lines_past_expires_at_expire() {
        let lines: Vec<Value> = (0..6)
            .map(|i| json!({"custom_id": format!("r{i}"), "slow": i >= 3}))
            .collect();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        let r = run_lines(lines, deadline, |line| async move {
            if line["slow"] == json!(true) {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            }
            (200, json!({"ok": line["custom_id"]}))
        })
        .await;
        assert_eq!(r.len(), 6, "order and count kept");
        assert!(r[..3]
            .iter()
            .all(|(_, o)| matches!(o, Outcome::Done(200, _))));
        assert!(r[3..].iter().all(|(_, o)| *o == Outcome::Expired));
        let (out, errs) = result_lines(r);
        assert_eq!(out.len(), 3);
        assert_eq!(errs.len(), 3);
        assert_eq!(errs[0]["custom_id"], "r3");
        assert_eq!(errs[0]["error"]["code"], "batch_expired");
        assert!(errs[0]["response"].is_null());
    }

    #[test]
    fn line_cap_defaults_to_openai() {
        assert_eq!(max_lines(), 50_000);
    }
}
