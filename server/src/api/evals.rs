//! Graders, Evals and Fine-tuning, with the seat where a model would be.
//!
//! - Graders: `string_check` and `text_similarity` are computed here;
//!   model graders (`label_model`, `score_model`) and `python` graders are
//!   seat tasks that answer a score or label; `multi` averages.
//! - Evals: CRUD; a run samples each data row through the chat pipeline
//!   when its data source asks for sampling, then grades every row with the
//!   eval's testing criteria; output items and counts as OpenAI shapes them.
//! - Fine-tuning: a job hands the training file to the seat as a task;
//!   once the seat acknowledges it the job succeeds with an `ft:` model
//!   name, events, a checkpoint and a metrics file.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::api::stored::{self, Stored};
use crate::api::task::{self, Delivery, Finished};
use crate::auth::AuthPrincipal;
use crate::storage::AppState;

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

#[derive(Debug, serde::Deserialize, Default)]
pub struct ListQuery {
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    after: Option<String>,
    #[serde(default)]
    order: Option<String>,
}

fn page(mut items: Vec<Value>, q: &ListQuery, default_order: &str) -> Value {
    if q.order.as_deref().unwrap_or(default_order) == "desc" {
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

// ── templates and graders ──

/// Fill `{{item.x}}` / `{{sample.output_text}}` (dotted paths) from the
/// row and its sample.
pub fn render(t: &str, item: &Value, sample: &Value) -> String {
    let mut out = String::new();
    let mut rest = t;
    while let Some(i) = rest.find("{{") {
        out.push_str(&rest[..i]);
        let Some(j) = rest[i..].find("}}") else {
            out.push_str(&rest[i..]);
            return out;
        };
        let path = rest[i + 2..i + j].trim();
        let mut parts = path.split('.');
        let root = match parts.next() {
            Some("item") => item,
            Some("sample") => sample,
            _ => &Value::Null,
        };
        let mut v = root;
        for p in parts {
            v = match v {
                Value::Array(a) => p
                    .parse::<usize>()
                    .ok()
                    .and_then(|n| a.get(n))
                    .unwrap_or(&Value::Null),
                _ => v.get(p).unwrap_or(&Value::Null),
            };
        }
        out.push_str(&match v {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            o => o.to_string(),
        });
        rest = &rest[i + j + 2..];
    }
    out.push_str(rest);
    out
}

/// Similarity in 0..1 from edit distance (fuzzy_match), shared by every
/// text_similarity metric here.
pub fn similarity(a: &str, b: &str) -> f64 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + (ca != cb) as usize;
            cur.push(sub.min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    1.0 - prev[b.len()] as f64 / a.len().max(b.len()) as f64
}

/// A grader's verdict: score, pass/fail, and errors.
pub struct Grade {
    pub score: f64,
    pub passed: bool,
    pub detail: Value,
}

/// Grade one row. Model and python graders become one seat task each.
pub async fn grade(
    g: &Value,
    item: &Value,
    sample: &Value,
    app: &AppState,
    principal: &AuthPrincipal,
    headers: &HeaderMap,
) -> Result<Grade, String> {
    let kind = g["type"].as_str().unwrap_or("");
    match kind {
        "string_check" => {
            let input = render(g["input"].as_str().unwrap_or(""), item, sample);
            let reference = render(g["reference"].as_str().unwrap_or(""), item, sample);
            let ok = match g["operation"].as_str().unwrap_or("eq") {
                "eq" => input == reference,
                "ne" => input != reference,
                "like" => input.contains(&reference),
                "ilike" => input.to_lowercase().contains(&reference.to_lowercase()),
                o => return Err(format!("unknown string_check operation {o}")),
            };
            Ok(Grade {
                score: ok as u8 as f64,
                passed: ok,
                detail: json!({}),
            })
        }
        "text_similarity" => {
            let input = render(g["input"].as_str().unwrap_or(""), item, sample);
            let reference = render(g["reference"].as_str().unwrap_or(""), item, sample);
            let score = similarity(&input, &reference);
            let threshold = g["pass_threshold"].as_f64().unwrap_or(0.5);
            Ok(Grade {
                score,
                passed: score >= threshold,
                detail: json!({"evaluation_metric": g["evaluation_metric"]}),
            })
        }
        "multi" => {
            let subs = g["graders"].as_object().cloned().unwrap_or_default();
            let mut total = 0.0;
            for (_, sg) in &subs {
                total += Box::pin(grade(sg, item, sample, app, principal, headers))
                    .await?
                    .score;
            }
            let score = if subs.is_empty() {
                0.0
            } else {
                total / subs.len() as f64
            };
            Ok(Grade {
                score,
                passed: score >= 0.5,
                detail: json!({}),
            })
        }
        "label_model" | "score_model" | "python" => {
            seat_grade(g, item, sample, app, principal, headers).await
        }
        o => Err(format!("unknown grader type {o}")),
    }
}

/// A grader the seat applies: it answers JSON `{"score": x}` or
/// `{"label": "..."}`.
async fn seat_grade(
    g: &Value,
    item: &Value,
    sample: &Value,
    app: &AppState,
    principal: &AuthPrincipal,
    headers: &HeaderMap,
) -> Result<Grade, String> {
    let kind = g["type"].as_str().unwrap_or("");
    let rendered = match kind {
        "python" => format!(
            "Grader source:\n{}\n\nitem: {item}\nsample: {sample}",
            g["source"].as_str().unwrap_or("")
        ),
        _ => g["input"]
            .as_array()
            .map(|msgs| {
                msgs.iter()
                    .map(|m| {
                        let c = m["content"]
                            .as_str()
                            .map(String::from)
                            .unwrap_or_else(|| m["content"].to_string());
                        format!(
                            "[{}] {}",
                            m["role"].as_str().unwrap_or("user"),
                            render(&c, item, sample)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n")
            })
            .unwrap_or_default(),
    };
    let (want, spec) = match kind {
        "label_model" => {
            let labels = g["labels"].clone();
            (format!("grade as the messages ask: answer JSON {{\"label\": one of {labels}}}"), json!({"labels": labels}))
        }
        "score_model" => {
            let range = g["range"].as_array().cloned().unwrap_or_else(|| vec![json!(0), json!(1)]);
            (format!("grade as the messages ask: answer JSON {{\"score\": a number in {}}}", Value::Array(range.clone())), json!({"range": range}))
        }
        _ => ("run the grader on the item and sample (as Python's grade(sample, item) would): answer JSON {\"score\": a number}".to_string(), json!({})),
    };
    let (tx, rx) = tokio::sync::oneshot::channel();
    let finish: task::Finish = Box::new(move |a| {
        let _ = tx.send(a.map(|a| a.raw));
        Finished::Json(StatusCode::OK, json!({}))
    });
    let resp = task::run(
        app.clone(),
        principal.clone(),
        headers.clone(),
        "graders",
        g["model"].as_str(),
        rendered,
        Vec::new(),
        want,
        false,
        spec,
        Delivery::Json,
        finish,
    )
    .await;
    let _ = axum::body::to_bytes(resp.into_body(), usize::MAX).await;
    let raw = rx.await.map_err(|_| "no answer".to_string())??;
    let v: Value = serde_json::from_str(raw.trim())
        .map_err(|_| format!("grader answer is not JSON: {raw}"))?;
    match kind {
        "label_model" => {
            let label = v["label"].as_str().unwrap_or("").to_string();
            let passed = g["passing_labels"]
                .as_array()
                .is_some_and(|p| p.iter().any(|x| x == &json!(label)));
            Ok(Grade {
                score: passed as u8 as f64,
                passed,
                detail: json!({"label": label}),
            })
        }
        _ => {
            let score = v["score"].as_f64().ok_or("grader answer has no score")?;
            let threshold = g["pass_threshold"].as_f64().unwrap_or(0.5);
            Ok(Grade {
                score,
                passed: score >= threshold,
                detail: json!({}),
            })
        }
    }
}

/// Check a seat's grading answer before it is delivered.
pub fn check_grade(text: &str, spec: &Value) -> Result<(), String> {
    let v: Value = serde_json::from_str(text.trim())
        .map_err(|_| "answer with JSON: {\"score\": 0.8} or {\"label\": \"...\"}".to_string())?;
    if let Some(labels) = spec.get("labels").and_then(|l| l.as_array()) {
        let l = v["label"]
            .as_str()
            .ok_or("answer with {\"label\": \"...\"}")?;
        if !labels.iter().any(|x| x == &json!(l)) {
            return Err(format!(
                "label must be one of {}",
                Value::Array(labels.clone())
            ));
        }
        return Ok(());
    }
    let s = v["score"]
        .as_f64()
        .ok_or("answer with {\"score\": <number>}")?;
    if let Some(r) = spec.get("range").and_then(|r| r.as_array()) {
        let (lo, hi) = (
            r.first().and_then(|x| x.as_f64()).unwrap_or(0.0),
            r.get(1).and_then(|x| x.as_f64()).unwrap_or(1.0),
        );
        if s < lo || s > hi {
            return Err(format!("score must be within {lo}..{hi}"));
        }
    }
    Ok(())
}

/// `POST /v1/fine_tuning/alpha/graders/run`
pub async fn graders_run(
    State(app): State<AppState>,
    principal: AuthPrincipal,
    headers: HeaderMap,
    Json(b): Json<Value>,
) -> Response {
    let g = b.get("grader").cloned().unwrap_or(Value::Null);
    let item = b.get("item").cloned().unwrap_or(json!({}));
    let sample_text = b.get("model_sample").cloned().unwrap_or(json!(""));
    let sample =
        json!({"output_text": sample_text, "choices": [{"message": {"content": sample_text}}]});
    let started = std::time::Instant::now();
    match grade(&g, &item, &sample, &app, &principal, &headers).await {
        Ok(r) => Json(json!({
            "reward": r.score,
            "metadata": {"name": g["name"], "type": g["type"], "errors": {}, "execution_time": started.elapsed().as_secs_f64(),
                         "scores": {}, "token_usage": null, "sampled_model_name": null},
            "sub_rewards": {},
            "model_grader_token_usage_per_model": {},
        }))
        .into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, &e),
    }
}

/// `POST /v1/fine_tuning/alpha/graders/validate`
pub async fn graders_validate(Json(b): Json<Value>) -> Response {
    let g = b.get("grader").cloned().unwrap_or(Value::Null);
    match g["type"].as_str() {
        Some("string_check" | "text_similarity" | "label_model" | "score_model" | "python" | "multi") => {
            Json(json!({"grader": g})).into_response()
        }
        _ => err(StatusCode::BAD_REQUEST, "grader.type must be string_check, text_similarity, label_model, score_model, python or multi"),
    }
}

// ── evals ──

fn get_eval(owner: &str, id: &str) -> Option<Stored> {
    stored::get(owner, id).filter(|s| s.obj["object"] == "eval")
}

pub async fn create_eval(principal: AuthPrincipal, Json(b): Json<Value>) -> Response {
    let Some(criteria) = b.get("testing_criteria").and_then(|v| v.as_array()) else {
        return err(StatusCode::BAD_REQUEST, "testing_criteria is required");
    };
    let id = format!("eval_{}", uuid::Uuid::now_v7().simple());
    let criteria: Vec<Value> = criteria
        .iter()
        .map(|c| {
            let mut c = c.clone();
            if c.get("name").is_none() {
                c["name"] = c["type"].clone();
            }
            c
        })
        .collect();
    let obj = json!({
        "id": id,
        "object": "eval",
        "name": b.get("name").cloned().unwrap_or(Value::Null),
        "data_source_config": b.get("data_source_config").cloned().unwrap_or(json!({"type": "custom", "schema": {}})),
        "testing_criteria": criteria,
        "created_at": now(),
        "metadata": b.get("metadata").cloned().unwrap_or(json!({})),
    });
    stored::put(
        &id,
        Stored {
            owner: principal.agent.id.clone(),
            obj: obj.clone(),
            input: Vec::new(),
            history: Vec::new(),
        },
    );
    Json(obj).into_response()
}

pub async fn list_evals(principal: AuthPrincipal, Query(q): Query<ListQuery>) -> Response {
    let items = stored::list(&principal.agent.id, "eval")
        .into_iter()
        .map(|(_, s)| s.obj)
        .collect();
    Json(page(items, &q, "asc")).into_response()
}

pub async fn get_eval_route(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    match get_eval(&principal.agent.id, &id) {
        Some(s) => Json(s.obj).into_response(),
        None => stored::not_found("eval", &id),
    }
}

pub async fn update_eval(
    principal: AuthPrincipal,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> Response {
    if get_eval(&principal.agent.id, &id).is_none() {
        return stored::not_found("eval", &id);
    }
    stored::update(&id, |s| {
        for k in ["name", "metadata"] {
            if let Some(v) = b.get(k) {
                s.obj[k] = v.clone();
            }
        }
    });
    get_eval_route(principal, Path(id)).await
}

pub async fn delete_eval(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    if get_eval(&principal.agent.id, &id).is_some() && stored::remove(&principal.agent.id, &id) {
        Json(json!({"object": "eval.deleted", "deleted": true, "eval_id": id})).into_response()
    } else {
        stored::not_found("eval", &id)
    }
}

/// Rows of a run's data source: `{item, sample?}` each.
fn rows(ds: &Value, owner: &str) -> Result<Vec<Value>, String> {
    let src = &ds["source"];
    match src["type"].as_str().unwrap_or("") {
        "file_content" => Ok(src["content"].as_array().cloned().unwrap_or_default()),
        "file_id" => {
            let id = src["id"].as_str().unwrap_or("");
            let f = crate::api::files::get(id).filter(|f| f.owner == owner).ok_or(format!("No file found with id '{id}'."))?;
            String::from_utf8_lossy(&f.bytes)
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| serde_json::from_str(l).map_err(|e| e.to_string()))
                .collect()
        }
        "stored_completions" => Ok(stored::list(owner, "chat.completion")
            .into_iter()
            .map(|(_, s)| json!({"item": {"input": s.input}, "sample": {"output_text": s.obj.pointer("/choices/0/message/content").cloned().unwrap_or(Value::Null)}}))
            .collect()),
        o => Err(format!("unsupported data source {o}")),
    }
}

pub async fn create_run(
    State(app): State<AppState>,
    principal: AuthPrincipal,
    headers: HeaderMap,
    Path(eval_id): Path<String>,
    Json(b): Json<Value>,
) -> Response {
    let owner = principal.agent.id.clone();
    let Some(ev) = get_eval(&owner, &eval_id) else {
        return stored::not_found("eval", &eval_id);
    };
    let ds = b.get("data_source").cloned().unwrap_or(Value::Null);
    let data = match rows(&ds, &owner) {
        Ok(r) => r,
        Err(e) => return err(StatusCode::BAD_REQUEST, &e),
    };
    let id = format!("evalrun_{}", uuid::Uuid::now_v7().simple());
    let criteria = ev.obj["testing_criteria"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let obj = json!({
        "id": id,
        "object": "eval.run",
        "eval_id": eval_id,
        "name": b.get("name").cloned().unwrap_or(json!(id)),
        "status": "queued",
        "model": ds.get("model").cloned().unwrap_or(Value::Null),
        "data_source": ds,
        "created_at": now(),
        "result_counts": {"total": data.len(), "passed": 0, "failed": 0, "errored": 0},
        "per_model_usage": [],
        "per_testing_criteria_results": criteria.iter().map(|c| json!({"testing_criteria": c["name"], "passed": 0, "failed": 0})).collect::<Vec<_>>(),
        "report_url": null,
        "error": null,
        "metadata": b.get("metadata").cloned().unwrap_or(json!({})),
    });
    stored::put(
        &id,
        Stored {
            owner: owner.clone(),
            obj: obj.clone(),
            input: Vec::new(),
            history: Vec::new(),
        },
    );
    let rid = id.clone();
    let task = tokio::spawn(async move {
        stored::update(&rid, |s| s.obj["status"] = json!("in_progress"));
        let template = ds
            .pointer("/input_messages/template")
            .and_then(|t| t.as_array())
            .cloned();
        for (i, row) in data.into_iter().enumerate() {
            let item = row.get("item").cloned().unwrap_or_else(|| row.clone());
            let mut sample = row.get("sample").cloned().unwrap_or(Value::Null);
            let mut errored = None;
            // Sampling: the template rendered per row, answered by the seat.
            if let Some(t) = &template {
                let msgs: Vec<Value> = t
                    .iter()
                    .map(|m| {
                        let c = m["content"].as_str().map(String::from).unwrap_or_else(|| m["content"].to_string());
                        json!({"role": m["role"].as_str().unwrap_or("user"), "content": render(&c, &item, &Value::Null)})
                    })
                    .collect();
                let mut h = headers.clone();
                if let Ok(v) = axum::http::HeaderValue::from_str(&format!("eval:{rid}:{i}")) {
                    h.insert("x-omni2api-session-id", v);
                }
                let body = json!({"model": ds["model"], "messages": msgs, "stream": false});
                let resp = crate::api::stored::chat_create(
                    State(app.clone()),
                    principal.clone(),
                    h,
                    Json(body),
                )
                .await;
                let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .unwrap_or_default();
                match serde_json::from_str::<Value>(String::from_utf8_lossy(&bytes).trim()) {
                    Ok(v) if v.pointer("/choices/0/message").is_some() => {
                        let text = v
                            .pointer("/choices/0/message/content")
                            .cloned()
                            .unwrap_or(json!(""));
                        sample = json!({"output_text": text, "choices": v["choices"], "model": v["model"], "usage": v["usage"]});
                    }
                    Ok(v) => {
                        errored = Some(
                            v.pointer("/error/message")
                                .cloned()
                                .unwrap_or(json!("sampling failed")),
                        )
                    }
                    Err(e) => errored = Some(json!(e.to_string())),
                }
            }
            let mut results = Vec::new();
            let mut all_pass = errored.is_none();
            for c in &criteria {
                match grade(c, &item, &sample, &app, &principal, &headers).await {
                    Ok(g) => {
                        all_pass &= g.passed;
                        let name = c["name"].clone();
                        stored::update(&rid, |s| {
                            for r in s.obj["per_testing_criteria_results"]
                                .as_array_mut()
                                .into_iter()
                                .flatten()
                            {
                                if r["testing_criteria"] == name {
                                    let k = if g.passed { "passed" } else { "failed" };
                                    r[k] = json!(r[k].as_u64().unwrap_or(0) + 1);
                                }
                            }
                        });
                        results.push(json!({"name": c["name"], "type": c["type"], "score": g.score, "passed": g.passed, "sample": null, "detail": g.detail}));
                    }
                    Err(e) => {
                        all_pass = false;
                        errored = Some(json!(e));
                    }
                }
            }
            let status = if errored.is_some() {
                "fail"
            } else if all_pass {
                "pass"
            } else {
                "fail"
            };
            let out = json!({
                "id": format!("outputitem_{}", uuid::Uuid::now_v7().simple()),
                "object": "eval.run.output_item",
                "eval_id": eval_id,
                "run_id": rid,
                "created_at": now(),
                "status": status,
                "datasource_item_id": i,
                "datasource_item": item,
                "results": results,
                "sample": if sample.is_null() { Value::Null } else { json!({"input": [], "output": [{"role": "assistant", "content": sample["output_text"]}], "model": sample["model"], "usage": sample["usage"], "error": errored, "finish_reason": "stop", "temperature": 1.0, "top_p": 1.0, "seed": null, "max_completion_tokens": null}) },
            });
            stored::update(&rid, |s| {
                let k = if errored.is_some() {
                    "errored"
                } else if all_pass {
                    "passed"
                } else {
                    "failed"
                };
                s.obj["result_counts"][k] =
                    json!(s.obj["result_counts"][k].as_u64().unwrap_or(0) + 1);
                s.input.push(out);
            });
        }
        stored::update(&rid, |s| s.obj["status"] = json!("completed"));
        stored::done_running(&rid);
    });
    stored::set_running(&id, task.abort_handle());
    Json(obj).into_response()
}

fn get_run(owner: &str, eval_id: &str, run: &str) -> Option<Stored> {
    stored::get(owner, run)
        .filter(|s| s.obj["object"] == "eval.run" && s.obj["eval_id"] == json!(eval_id))
}

pub async fn list_runs(
    principal: AuthPrincipal,
    Path(eval_id): Path<String>,
    Query(q): Query<ListQuery>,
) -> Response {
    let items = stored::list(&principal.agent.id, "eval.run")
        .into_iter()
        .map(|(_, s)| s.obj)
        .filter(|o| o["eval_id"] == json!(eval_id))
        .collect();
    Json(page(items, &q, "asc")).into_response()
}

pub async fn get_run_route(
    principal: AuthPrincipal,
    Path((eval_id, run)): Path<(String, String)>,
) -> Response {
    match get_run(&principal.agent.id, &eval_id, &run) {
        Some(s) => Json(s.obj).into_response(),
        None => stored::not_found("eval run", &run),
    }
}

pub async fn cancel_run(
    principal: AuthPrincipal,
    Path((eval_id, run)): Path<(String, String)>,
) -> Response {
    if get_run(&principal.agent.id, &eval_id, &run).is_none() {
        return stored::not_found("eval run", &run);
    }
    if stored::abort_running(&run) {
        stored::update(&run, |s| s.obj["status"] = json!("canceled"));
    }
    get_run_route(principal, Path((eval_id, run))).await
}

pub async fn delete_run(
    principal: AuthPrincipal,
    Path((eval_id, run)): Path<(String, String)>,
) -> Response {
    if get_run(&principal.agent.id, &eval_id, &run).is_some() {
        stored::abort_running(&run);
        stored::remove(&principal.agent.id, &run);
        Json(json!({"object": "eval.run.deleted", "deleted": true, "run_id": run})).into_response()
    } else {
        stored::not_found("eval run", &run)
    }
}

#[derive(Debug, serde::Deserialize, Default)]
pub struct ItemsQuery {
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    after: Option<String>,
    #[serde(default)]
    order: Option<String>,
    #[serde(default)]
    status: Option<String>,
}

pub async fn list_output_items(
    principal: AuthPrincipal,
    Path((eval_id, run)): Path<(String, String)>,
    Query(q): Query<ItemsQuery>,
) -> Response {
    let Some(s) = get_run(&principal.agent.id, &eval_id, &run) else {
        return stored::not_found("eval run", &run);
    };
    let items = s
        .input
        .into_iter()
        .filter(|o| {
            q.status
                .as_deref()
                .is_none_or(|st| o["status"] == json!(st))
        })
        .collect();
    let lq = ListQuery {
        limit: q.limit,
        after: q.after,
        order: q.order,
    };
    Json(page(items, &lq, "asc")).into_response()
}

pub async fn get_output_item(
    principal: AuthPrincipal,
    Path((eval_id, run, item)): Path<(String, String, String)>,
) -> Response {
    match get_run(&principal.agent.id, &eval_id, &run)
        .and_then(|s| s.input.into_iter().find(|o| o["id"] == json!(item)))
    {
        Some(o) => Json(o).into_response(),
        None => stored::not_found("output item", &item),
    }
}

// ── fine-tuning ──

fn get_job(owner: &str, id: &str) -> Option<Stored> {
    stored::get(owner, id).filter(|s| s.obj["object"] == "fine_tuning.job")
}

fn event(level: &str, message: &str) -> Value {
    json!({"id": format!("ftevent-{}", uuid::Uuid::now_v7().simple()), "object": "fine_tuning.job.event",
           "created_at": now(), "level": level, "message": message, "type": "message", "data": {}})
}

pub async fn create_job(
    State(app): State<AppState>,
    principal: AuthPrincipal,
    headers: HeaderMap,
    Json(b): Json<Value>,
) -> Response {
    let owner = principal.agent.id.clone();
    let (Some(model), Some(training)) = (b["model"].as_str(), b["training_file"].as_str()) else {
        return err(
            StatusCode::BAD_REQUEST,
            "model and training_file are required",
        );
    };
    let Some(tf) = crate::api::files::get(training).filter(|f| f.owner == owner) else {
        return err(
            StatusCode::NOT_FOUND,
            &format!("No file found with id '{training}'."),
        );
    };
    let mut attach = vec![crate::openai_compat::named_data_url(
        &tf.mime,
        Some(&tf.filename),
        &crate::openai_compat::b64_encode(&tf.bytes),
    )];
    if let Some(vf) = b["validation_file"]
        .as_str()
        .and_then(crate::api::files::get)
        .filter(|f| f.owner == owner)
    {
        attach.push(crate::openai_compat::named_data_url(
            &vf.mime,
            Some(&vf.filename),
            &crate::openai_compat::b64_encode(&vf.bytes),
        ));
    }
    let id = format!("ftjob-{}", uuid::Uuid::now_v7().simple());
    let suffix = b["suffix"].as_str().unwrap_or("custom").to_string();
    let short: String = id
        .chars()
        .rev()
        .take(8)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    let ft_model = format!("ft:{model}:omni2api:{suffix}:{short}");
    let method = b
        .get("method")
        .cloned()
        .unwrap_or(json!({"type": "supervised"}));
    let obj = json!({
        "id": id,
        "object": "fine_tuning.job",
        "model": model,
        "created_at": now(),
        "finished_at": null,
        "fine_tuned_model": null,
        "organization_id": "org-omni2api",
        "result_files": [],
        "status": "validating_files",
        "validation_file": b.get("validation_file").cloned().unwrap_or(Value::Null),
        "training_file": training,
        "hyperparameters": b.get("hyperparameters").cloned().unwrap_or(json!({"n_epochs": "auto", "batch_size": "auto", "learning_rate_multiplier": "auto"})),
        "trained_tokens": null,
        "method": method,
        "metadata": b.get("metadata").cloned().unwrap_or(Value::Null),
        "seed": b.get("seed").cloned().unwrap_or(json!(0)),
        "suffix": b.get("suffix").cloned().unwrap_or(Value::Null),
        "estimated_finish": null,
        "error": null,
        "integrations": [],
    });
    stored::put(
        &id,
        Stored {
            owner: owner.clone(),
            obj: obj.clone(),
            input: vec![event("info", "Validating training file")],
            history: Vec::new(),
        },
    );
    let jid = id.clone();
    let tokens = (tf.bytes.len() as u64).div_ceil(4);
    let task = tokio::spawn(async move {
        stored::update(&jid, |s| {
            s.obj["status"] = json!("running");
            s.input.push(event("info", "Fine-tuning job started"));
        });
        let want = format!(
            "fine-tuning job ({}): learn from the attached training examples; callers will then ask for model {ft_model}. Answer with a short note when done (text alone; it is recorded as the job's last event)",
            method["type"].as_str().unwrap_or("supervised")
        );
        let (tx, rx) = tokio::sync::oneshot::channel();
        let finish: task::Finish = Box::new(move |a| {
            let _ = tx.send(a.map(|a| a.text));
            Finished::Json(StatusCode::OK, json!({}))
        });
        let resp = task::run(
            app,
            principal,
            headers,
            "fine_tuning",
            Some(&ft_model),
            format!("Training file: {}", tf.filename),
            attach,
            want,
            false,
            Value::Null,
            Delivery::Json,
            finish,
        )
        .await;
        let _ = axum::body::to_bytes(resp.into_body(), usize::MAX).await;
        let outcome = rx.await.unwrap_or_else(|_| Err("no answer".into()));
        let metrics = "step,train_loss,train_accuracy\n1,0.0,1.0\n".to_string();
        let mf = crate::api::files::put(crate::api::files::StoredFile {
            owner: owner.clone(),
            filename: "step_metrics.csv".into(),
            mime: "text/csv".into(),
            purpose: "fine-tune-results".into(),
            bytes: metrics.into_bytes(),
            created_at: now(),
        });
        let t = now();
        stored::update(&jid, |s| match &outcome {
            Ok(note) => {
                s.obj["status"] = json!("succeeded");
                s.obj["fine_tuned_model"] = json!(ft_model);
                s.obj["finished_at"] = json!(t);
                s.obj["trained_tokens"] = json!(tokens);
                s.obj["result_files"] = json!([mf]);
                if !note.is_empty() {
                    s.input.push(event("info", note));
                }
                s.input
                    .push(event("info", "The job has successfully completed"));
                s.history = vec![json!({
                    "id": format!("ftckpt_{}", uuid::Uuid::now_v7().simple()),
                    "object": "fine_tuning.job.checkpoint",
                    "created_at": t,
                    "fine_tuned_model_checkpoint": format!("{ft_model}:ckpt-step-1"),
                    "fine_tuning_job_id": s.obj["id"],
                    "step_number": 1,
                    "metrics": {"step": 1, "train_loss": 0.0, "train_mean_token_accuracy": 1.0},
                })];
            }
            Err(e) => {
                s.obj["status"] = json!("failed");
                s.obj["finished_at"] = json!(t);
                s.obj["error"] = json!({"code": "seat_unavailable", "message": e, "param": null});
                s.input.push(event("error", e));
            }
        });
        stored::done_running(&jid);
    });
    stored::set_running(&id, task.abort_handle());
    Json(obj).into_response()
}

pub async fn list_jobs(principal: AuthPrincipal, Query(q): Query<ListQuery>) -> Response {
    let items = stored::list(&principal.agent.id, "fine_tuning.job")
        .into_iter()
        .map(|(_, s)| s.obj)
        .collect();
    Json(page(items, &q, "desc")).into_response()
}

pub async fn get_job_route(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    match get_job(&principal.agent.id, &id) {
        Some(s) => Json(s.obj).into_response(),
        None => stored::not_found("fine-tuning job", &id),
    }
}

/// `cancel`, `pause`, `resume`.
pub async fn job_action(
    principal: AuthPrincipal,
    Path((id, action)): Path<(String, String)>,
) -> Response {
    let Some(s) = get_job(&principal.agent.id, &id) else {
        return stored::not_found("fine-tuning job", &id);
    };
    let status = s.obj["status"].as_str().unwrap_or("").to_string();
    let active = matches!(
        status.as_str(),
        "validating_files" | "queued" | "running" | "paused"
    );
    match action.as_str() {
        "cancel" if active => {
            stored::abort_running(&id);
            stored::update(&id, |s| {
                s.obj["status"] = json!("cancelled");
                s.input.push(event("info", "The job was cancelled"));
            });
        }
        "pause" if status == "running" => stored::update(&id, |s| {
            s.obj["status"] = json!("paused");
            s.input.push(event("info", "The job was paused"));
        }),
        "resume" if status == "paused" => stored::update(&id, |s| {
            s.obj["status"] = json!("running");
            s.input.push(event("info", "The job was resumed"));
        }),
        "cancel" | "pause" | "resume" => {
            return err(
                StatusCode::BAD_REQUEST,
                &format!("cannot {action} a job that is {status}"),
            )
        }
        _ => return err(StatusCode::NOT_FOUND, "unknown action"),
    }
    get_job_route(principal, Path(id)).await
}

pub async fn job_events(
    principal: AuthPrincipal,
    Path(id): Path<String>,
    Query(q): Query<ListQuery>,
) -> Response {
    match get_job(&principal.agent.id, &id) {
        Some(s) => Json(page(s.input, &q, "desc")).into_response(),
        None => stored::not_found("fine-tuning job", &id),
    }
}

pub async fn job_checkpoints(
    principal: AuthPrincipal,
    Path(id): Path<String>,
    Query(q): Query<ListQuery>,
) -> Response {
    match get_job(&principal.agent.id, &id) {
        Some(s) => Json(page(s.history, &q, "desc")).into_response(),
        None => stored::not_found("fine-tuning job", &id),
    }
}

/// Fine-tuned models of this key, for `/v1/models`.
pub fn fine_tuned_models(owner: &str) -> Vec<Value> {
    stored::list(owner, "fine_tuning.job")
        .into_iter()
        .filter_map(|(_, s)| {
            let m = s.obj["fine_tuned_model"].as_str()?.to_string();
            Some(json!({"id": m, "object": "model", "created": s.obj["finished_at"], "owned_by": "omni2api"}))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_fill_paths() {
        let item = json!({"q": "2+2", "a": {"n": 4}});
        let sample = json!({"output_text": "4"});
        assert_eq!(
            render("Q: {{item.q}} = {{ item.a.n }}", &item, &sample),
            "Q: 2+2 = 4"
        );
        assert_eq!(render("{{sample.output_text}}!", &item, &sample), "4!");
    }

    #[test]
    fn similarity_bounds() {
        assert_eq!(similarity("abc", "abc"), 1.0);
        assert!(similarity("kitten", "sitting") > 0.5);
        assert_eq!(similarity("", "x"), 0.0);
    }

    #[test]
    fn grade_answers_checked() {
        assert!(check_grade(r#"{"label": "good"}"#, &json!({"labels": ["good", "bad"]})).is_ok());
        assert!(check_grade(r#"{"label": "meh"}"#, &json!({"labels": ["good", "bad"]})).is_err());
        assert!(check_grade(r#"{"score": 7}"#, &json!({"range": [0, 5]})).is_err());
        assert!(check_grade(r#"{"score": 0.4}"#, &json!({})).is_ok());
    }
}
