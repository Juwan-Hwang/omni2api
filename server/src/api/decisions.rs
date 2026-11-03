//! `/v1/decisions`: classification and scoring questions about an input,
//! answered by the seat. The questions arrive numbered; it answers with a
//! JSON list, one entry per question in order (a predicate's probability,
//! a choice's value, a score's level label, or null to refuse), optionally
//! as `{"answer": ..., "confidence": 0..1}`, and the server builds the
//! OpenAI answers with their probabilities. `/v1/content_provenance_checks`
//! looks for C2PA marks, and says whether a file is one this server's seat
//! made.

use axum::extract::{Multipart, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::api::task::{self, Delivery, Finished};
use crate::auth::AuthPrincipal;
use crate::storage::AppState;

/// What each question takes, for checking the seat's answer.
fn question_spec(q: &Value) -> Result<Value, String> {
    let kind = q["type"].as_str().unwrap_or("");
    let name = q.get("name").cloned().unwrap_or(Value::Null);
    match kind {
        "predicate" => Ok(json!({"type": "predicate", "name": name})),
        "choice" => {
            let values: Vec<Value> = q["choices"]
                .as_array()
                .map(|a| a.iter().map(|c| c["value"].clone()).collect())
                .unwrap_or_default();
            if values.len() < 2 {
                return Err("a choice question needs at least two choices".into());
            }
            Ok(json!({"type": "choice", "name": name, "values": values}))
        }
        "score" => {
            let labels: Vec<Value> = q["levels"]
                .as_array()
                .map(|a| a.iter().map(|l| l["label"].clone()).collect())
                .unwrap_or_default();
            if labels.len() < 2 {
                return Err("a score question needs at least two levels".into());
            }
            Ok(json!({"type": "score", "name": name, "labels": labels}))
        }
        _ => Err("each question's type is predicate, choice or score".into()),
    }
}

fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .filter_map(|p| {
                p["text"]
                    .as_str()
                    .map(String::from)
                    .or_else(|| p.get("json").map(|j| j.to_string()))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// The OpenAI answers from the seat's reply, or why it does not fit.
pub fn parse_answers(text: &str, spec: &Value) -> Result<Vec<Value>, String> {
    let qs = spec["questions"].as_array().cloned().unwrap_or_default();
    let v: Value = serde_json::from_str(text.trim()).map_err(|_| {
        "answer with a JSON list, one entry per question in order: a probability 0..1 for a predicate, the chosen value for a choice, the level label for a score, or null to refuse".to_string()
    })?;
    let list = match v.get("answers").cloned().unwrap_or(v) {
        Value::Array(a) => a,
        x if qs.len() == 1 => vec![x],
        _ => return Err("answer with a JSON list, one entry per question".into()),
    };
    if list.len() != qs.len() {
        return Err(format!(
            "expected {} answer(s), one per question in order; got {}",
            qs.len(),
            list.len()
        ));
    }
    qs.iter()
        .zip(list)
        .enumerate()
        .map(|(i, (q, a))| {
            let n = i + 1;
            let name = q["name"].clone();
            let (a, conf) = match &a {
                Value::Object(o) if o.contains_key("answer") => (
                    o["answer"].clone(),
                    o.get("confidence").and_then(|c| c.as_f64()),
                ),
                _ => (a, None),
            };
            if let Some(c) = conf {
                if !(0.0..=1.0).contains(&c) {
                    return Err(format!("answer {n}: confidence must be within 0..1"));
                }
            }
            if a.is_null() {
                return Ok(json!({"type": "refusal", "name": name}));
            }
            match q["type"].as_str().unwrap_or("") {
                "predicate" => {
                    let p = match &a {
                        Value::Bool(b) => conf.map(|c| if *b { c } else { 1.0 - c }).unwrap_or(*b as u8 as f64),
                        Value::Number(x) => x.as_f64().unwrap_or(-1.0),
                        _ => -1.0,
                    };
                    if !(0.0..=1.0).contains(&p) {
                        return Err(format!("answer {n}: a predicate takes a probability 0..1 (or true/false)"));
                    }
                    Ok(json!({"type": "predicate", "name": name, "probability": p}))
                }
                "choice" => {
                    let values = q["values"].as_array().cloned().unwrap_or_default();
                    let hit = values.iter().position(|v| {
                        v == &a || v.as_str().zip(a.as_str()).is_some_and(|(x, y)| x.eq_ignore_ascii_case(y.trim()))
                    });
                    let Some(k) = hit else {
                        return Err(format!("answer {n}: choose one of {}", Value::Array(values)));
                    };
                    let c = conf.unwrap_or(1.0);
                    let rest = if values.len() > 1 { (1.0 - c) / (values.len() - 1) as f64 } else { 0.0 };
                    let probs: Vec<Value> = values
                        .iter()
                        .enumerate()
                        .map(|(j, v)| json!({"value": v, "probability": if j == k { c } else { rest }}))
                        .collect();
                    Ok(json!({"type": "choice", "name": name, "choice": values[k], "confidence": c, "probabilities": probs}))
                }
                "score" => {
                    let labels = q["labels"].as_array().cloned().unwrap_or_default();
                    let hit = match &a {
                        Value::String(s) => labels.iter().position(|l| l.as_str().is_some_and(|l| l.eq_ignore_ascii_case(s.trim()))),
                        Value::Number(x) => x.as_u64().map(|x| x as usize).filter(|x| *x < labels.len()),
                        _ => None,
                    };
                    let Some(k) = hit else {
                        return Err(format!("answer {n}: give one of the level labels {}", Value::Array(labels)));
                    };
                    let c = conf.unwrap_or(1.0);
                    let rest = if labels.len() > 1 { (1.0 - c) / (labels.len() - 1) as f64 } else { 0.0 };
                    let probs: Vec<Value> = labels
                        .iter()
                        .enumerate()
                        .map(|(j, l)| json!({"label": l, "value": j, "probability": if j == k { c } else { rest }}))
                        .collect();
                    let expected: f64 = (0..labels.len())
                        .map(|j| j as f64 * if j == k { c } else { rest })
                        .sum();
                    Ok(json!({"type": "score", "name": name, "score": expected, "confidence": c, "probabilities": probs}))
                }
                _ => Err(format!("answer {n}: unknown question type")),
            }
        })
        .collect()
}

pub async fn create(
    State(app): State<AppState>,
    principal: AuthPrincipal,
    headers: HeaderMap,
    Json(b): Json<Value>,
) -> Response {
    let Some(questions) = b["questions"].as_array().filter(|q| !q.is_empty()) else {
        return task::invalid("questions is required (1 to 400)");
    };
    if questions.len() > 400 {
        return task::invalid("at most 400 questions");
    }
    let mut specs = Vec::new();
    for q in questions {
        match question_spec(q) {
            Ok(s) => specs.push(s),
            Err(e) => return task::invalid(&e),
        }
    }
    // The input: a string, or messages and tool items, as a transcript.
    let mut images = Vec::new();
    let input = match &b["input"] {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(|it| {
                if let Some(parts) = it["content"].as_array() {
                    for p in parts {
                        if let Some(u) = p["image_url"].as_str() {
                            images.push(u.to_string());
                        }
                    }
                }
                match it["type"].as_str().unwrap_or("message") {
                    "function_call" | "custom_tool_call" => format!(
                        "[tool call {}] {}",
                        it["name"].as_str().unwrap_or(""),
                        it["arguments"]
                            .as_str()
                            .or(it["input"].as_str())
                            .unwrap_or("")
                    ),
                    "function_call_output" | "custom_tool_call_output" => {
                        format!("[tool output] {}", text_of(&it["output"]))
                    }
                    _ => format!(
                        "{}: {}",
                        it["role"].as_str().unwrap_or("user"),
                        text_of(&it["content"])
                    ),
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => return task::invalid("input is required"),
    };
    let mut asked = Vec::new();
    for (i, q) in questions.iter().enumerate() {
        let ins = text_of(&q["instructions"]);
        let line = match q["type"].as_str().unwrap_or("") {
            "predicate" => format!("{}. (predicate: probability it is true) {ins}", i + 1),
            "choice" => {
                let opts: Vec<String> = q["choices"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|c| {
                        let d = c["description"]
                            .as_str()
                            .map(|d| format!(" ({d})"))
                            .unwrap_or_default();
                        format!("{}{d}", c["value"])
                    })
                    .collect();
                format!("{}. (choice: one of {}) {ins}", i + 1, opts.join(", "))
            }
            _ => {
                let lv: Vec<String> = q["levels"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|l| {
                        let d = l["description"]
                            .as_str()
                            .map(|d| format!(" ({d})"))
                            .unwrap_or_default();
                        format!("\"{}\"{d}", l["label"].as_str().unwrap_or(""))
                    })
                    .collect();
                format!("{}. (score: one level of {}) {ins}", i + 1, lv.join(", "))
            }
        };
        asked.push(line);
    }
    let n = questions.len();
    let message = format!("Input:\n{input}\n\nQuestions:\n{}", asked.join("\n"));
    let expected = format!(
        "answer {n} question(s) about the input: a JSON list, one entry per question in order: a probability 0..1 for a predicate, the chosen value for a choice, the level label for a score, null to refuse; any entry may be {{\"answer\": ..., \"confidence\": 0..1}}"
    );
    let spec = json!({"questions": specs});
    let spec_c = spec.clone();
    let model = b["model"].as_str().unwrap_or("gpt-6-luna").to_string();
    let model_c = model.clone();
    let tokens_in = (message.chars().count() as u64).div_ceil(4);
    let finish: task::Finish = Box::new(move |answer| {
        let a = match answer {
            Ok(a) => a,
            Err(e) => {
                return Finished::Json(
                    StatusCode::GATEWAY_TIMEOUT,
                    json!({"error": {"message": e, "type": "server_error", "code": "seat_unavailable", "param": null}}),
                )
            }
        };
        match parse_answers(&a.raw, &spec_c) {
            Ok(answers) => {
                let out = (a.raw.chars().count() as u64).div_ceil(4);
                Finished::Json(
                    StatusCode::OK,
                    json!({"model": model_c, "answers": answers, "usage": {
                        "input_tokens": tokens_in, "input_tokens_details": {"cache_write_tokens": 0, "cached_tokens": 0},
                        "output_tokens": out, "output_tokens_details": {"reasoning_tokens": 0},
                        "total_tokens": tokens_in + out}}),
                )
            }
            Err(e) => Finished::Json(
                StatusCode::BAD_GATEWAY,
                json!({"error": {"message": e, "type": "server_error", "code": "bad_answer", "param": null}}),
            ),
        }
    });
    task::run(
        app,
        principal,
        headers,
        "decisions",
        Some(&model),
        message,
        images,
        expected,
        false,
        spec,
        Delivery::Json,
        finish,
    )
    .await
}

/// Whether the bytes carry a C2PA manifest (JUMBF `c2pa` box), as a
/// signed picture or video does.
fn has_c2pa(b: &[u8]) -> bool {
    b.windows(4).any(|w| w == b"c2pa") && b.windows(4).any(|w| w == b"jumb")
}

/// `POST /v1/content_provenance_checks` (multipart `file`).
pub async fn provenance(_principal: AuthPrincipal, mut mp: Multipart) -> Response {
    let mut file: Option<Vec<u8>> = None;
    while let Ok(Some(f)) = mp.next_field().await {
        if f.name() == Some("file") {
            file = f.bytes().await.ok().map(|b| b.to_vec());
        }
    }
    let Some(bytes) = file else {
        return task::invalid("file is required");
    };
    let ours = crate::api::files::made_by_seat(&bytes);
    let c2pa = has_c2pa(&bytes);
    // A manifest's presence is reported, not verified: no signature check
    // runs here, so "unverified", never "valid".
    let mut c = json!({"type": "c2pa", "outcome": if c2pa { "detected" } else { "not_detected" },
        "validation_state": if c2pa { "unverified" } else { "not_present" }});
    if let Some((model, at)) = ours {
        // Made here: this server is the issuer, whatever the file carries.
        c = json!({"type": "c2pa", "outcome": "detected", "validation_state": "trusted",
            "issuer": "omni2api", "model": model, "generated_at": chrono::DateTime::from_timestamp(at, 0)
                .unwrap_or_default().to_rfc3339()});
    }
    Json(
        json!({"object": "content_provenance_check", "created_at": chrono::Utc::now().timestamp(),
        "results": [c, {"type": "synthid", "outcome": "not_detected"}]}),
    )
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> Value {
        json!({"questions": [
            question_spec(&json!({"type": "predicate", "instructions": "is it rude?"})).unwrap(),
            question_spec(&json!({"type": "choice", "instructions": "lang", "choices": [{"value": "en"}, {"value": "fr"}]})).unwrap(),
            question_spec(&json!({"type": "score", "instructions": "tone", "levels": [{"label": "low"}, {"label": "mid"}, {"label": "high"}]})).unwrap(),
        ]})
    }

    #[test]
    fn answers_take_shape() {
        let a = parse_answers(
            r#"[0.2, "FR", {"answer": "high", "confidence": 0.8}]"#,
            &spec(),
        )
        .unwrap();
        assert_eq!(a[0]["probability"], 0.2);
        assert_eq!(a[1]["choice"], "fr");
        assert_eq!(a[1]["probabilities"][1]["probability"], 1.0);
        assert_eq!(a[2]["probabilities"][2]["probability"], 0.8);
        assert!((a[2]["score"].as_f64().unwrap() - 1.7).abs() < 1e-9);
        let r = parse_answers("[null, \"en\", \"low\"]", &spec()).unwrap();
        assert_eq!(r[0]["type"], "refusal");
    }

    #[test]
    fn bad_answers_say_why() {
        assert!(parse_answers("[0.2]", &spec())
            .unwrap_err()
            .contains("expected 3"));
        assert!(parse_answers("[2, \"en\", \"low\"]", &spec()).is_err());
        assert!(parse_answers("[0.1, \"de\", \"low\"]", &spec())
            .unwrap_err()
            .contains("choose one"));
    }

    #[test]
    fn c2pa_sniff() {
        assert!(has_c2pa(b"xxjumbyyc2pazz"));
        assert!(!has_c2pa(b"plain"));
    }
}
