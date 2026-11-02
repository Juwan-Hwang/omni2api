//! `/v1/moderations`: the seat is the classifier. The inputs arrive as
//! `inputs.json` (pictures as attachments); it answers per input with the
//! categories it flags, as names or as scores, and the server builds the
//! full OpenAI result (every category, booleans, scores, input types).

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::Json;
use serde_json::{json, Map, Value};

use crate::api::task::{self, Delivery, Finished};
use crate::auth::AuthPrincipal;
use crate::storage::AppState;

pub const CATEGORIES: &[&str] = &[
    "harassment",
    "harassment/threatening",
    "hate",
    "hate/threatening",
    "illicit",
    "illicit/violent",
    "self-harm",
    "self-harm/intent",
    "self-harm/instructions",
    "sexual",
    "sexual/minors",
    "violence",
    "violence/graphic",
];

/// Categories that also apply to pictures (omni-moderation).
const IMAGE_CATEGORIES: &[&str] = &[
    "self-harm",
    "self-harm/intent",
    "self-harm/instructions",
    "sexual",
    "violence",
    "violence/graphic",
];

/// One verdict per input: category -> score in 0..=1. Accepts, per input,
/// a list of flagged names (`[]` = clean) or an object of scores (or of
/// booleans); the whole answer as a list, or `{"results": [...]}`.
pub fn parse_verdicts(text: &str, spec: &Value) -> Result<Vec<Map<String, Value>>, String> {
    let count = spec.get("count").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
    let v: Value = serde_json::from_str(text.trim()).map_err(|_| {
        "answer with JSON, one entry per input: [[], [\"violence\"]] or [{\"violence\": 0.9}]"
            .to_string()
    })?;
    let list = match v.get("results").cloned().unwrap_or(v) {
        // A single input may be answered without the outer list.
        Value::Array(a) if count == 1 && a.iter().all(|x| x.is_string()) => {
            vec![Value::Array(a)]
        }
        Value::Object(o) if count == 1 => vec![Value::Object(o)],
        Value::Array(a) => a,
        _ => return Err("answer with a JSON list, one entry per input".into()),
    };
    if list.len() != count {
        return Err(format!(
            "expected {count} entr(ies), one per input in order; got {}",
            list.len()
        ));
    }
    let known = |c: &str| -> Result<String, String> {
        let c = c.trim().to_lowercase().replace('_', "-");
        // `self_harm_intent` and `self-harm-intent` both mean self-harm/intent.
        let canon = CATEGORIES
            .iter()
            .find(|k| k.replace('/', "-") == c.replace('/', "-"))
            .map(|k| k.to_string());
        canon.ok_or(format!(
            "unknown category `{c}`; use: {}",
            CATEGORIES.join(", ")
        ))
    };
    list.iter()
        .enumerate()
        .map(|(i, e)| {
            let mut m = Map::new();
            match e {
                Value::Array(names) => {
                    for n in names {
                        let n = n
                            .as_str()
                            .ok_or(format!("entry {}: names must be strings", i + 1))?;
                        m.insert(known(n)?, json!(1.0));
                    }
                }
                Value::Object(o) => {
                    let o = o.get("categories").and_then(|c| c.as_object()).unwrap_or(o);
                    for (k, s) in o {
                        let score = match s {
                            Value::Bool(b) => *b as u8 as f64,
                            Value::Number(n) => n.as_f64().unwrap_or(0.0),
                            _ => {
                                return Err(format!(
                                    "entry {}: `{k}` must be a score or true/false",
                                    i + 1
                                ))
                            }
                        };
                        if !(0.0..=1.0).contains(&score) {
                            return Err(format!(
                                "entry {}: `{k}` score must be within 0..1",
                                i + 1
                            ));
                        }
                        m.insert(known(k)?, json!(score));
                    }
                }
                Value::Null => {}
                _ => {
                    return Err(format!(
                        "entry {}: a list of names or an object of scores",
                        i + 1
                    ))
                }
            }
            Ok(m)
        })
        .collect()
}

pub async fn moderations(
    State(app): State<AppState>,
    principal: AuthPrincipal,
    headers: HeaderMap,
    Json(b): Json<Value>,
) -> Response {
    // Inputs: text, texts, or omni parts ({type: text|image_url}). Each
    // part list is one input; its pictures go along as attachments.
    let mut texts: Vec<Value> = Vec::new();
    let mut images: Vec<String> = Vec::new();
    let mut has_image: Vec<bool> = Vec::new();
    match b.get("input") {
        Some(Value::String(s)) => {
            texts.push(json!(s));
            has_image.push(false);
        }
        Some(Value::Array(a)) if a.iter().all(|x| x.is_string()) && !a.is_empty() => {
            for s in a {
                texts.push(s.clone());
                has_image.push(false);
            }
        }
        Some(Value::Array(parts)) if !parts.is_empty() => {
            let mut text = Vec::new();
            let mut pics = 0;
            for p in parts {
                match p.get("type").and_then(|t| t.as_str()) {
                    Some("text") => text.push(
                        p.get("text")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string(),
                    ),
                    Some("image_url") => {
                        if let Some(u) = p
                            .pointer("/image_url/url")
                            .or_else(|| p.get("image_url"))
                            .and_then(|u| u.as_str())
                        {
                            images.push(u.to_string());
                            pics += 1;
                        }
                    }
                    _ => return task::invalid("input parts must be text or image_url"),
                }
            }
            texts.push(json!({"text": text.join("\n"), "images": pics}));
            has_image.push(pics > 0);
        }
        _ => return task::invalid("input is required"),
    }
    let n = texts.len();
    let list = serde_json::to_string(&texts).unwrap_or_default();
    let mut attach = vec![crate::openai_compat::named_data_url(
        "application/json",
        Some("inputs.json"),
        &crate::openai_compat::b64_encode(list.as_bytes()),
    )];
    attach.extend(images);
    let expected = format!(
        "moderate {n} input(s) (inputs.json; pictures attached): answer with JSON, one entry per input in order: the categories it violates as a list ([] when clean) or scores 0..1 ([[], [\"violence\"]] or [{{\"violence\": 0.92}}]); categories: {}",
        CATEGORIES.join(", ")
    );
    let spec = json!({"count": n});
    let spec_c = spec.clone();
    let model = b
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("omni-moderation-latest")
        .to_string();
    let model_c = model.clone();
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
        let verdicts = match parse_verdicts(&a.raw, &spec_c) {
            Ok(v) => v,
            Err(e) => {
                return Finished::Json(
                    StatusCode::BAD_GATEWAY,
                    json!({"error": {"message": e, "type": "server_error", "code": "bad_answer", "param": null}}),
                )
            }
        };
        let results: Vec<Value> = verdicts
            .iter()
            .zip(&has_image)
            .map(|(v, img)| {
                let mut categories = Map::new();
                let mut scores = Map::new();
                let mut applied = Map::new();
                for c in CATEGORIES {
                    let s = v.get(*c).and_then(|x| x.as_f64()).unwrap_or(0.0);
                    categories.insert(c.to_string(), json!(s >= 0.5));
                    scores.insert(c.to_string(), json!(s));
                    let mut types = vec!["text"];
                    if *img && IMAGE_CATEGORIES.contains(c) {
                        types.push("image");
                    }
                    applied.insert(c.to_string(), json!(types));
                }
                let flagged = categories.values().any(|x| x == &json!(true));
                json!({
                    "flagged": flagged,
                    "categories": categories,
                    "category_scores": scores,
                    "category_applied_input_types": applied,
                })
            })
            .collect();
        Finished::Json(
            StatusCode::OK,
            json!({
                "id": format!("modr-{}", uuid::Uuid::new_v4().simple()),
                "model": model_c,
                "results": results,
            }),
        )
    });
    let message = if list.chars().count() <= 2000 {
        serde_json::to_string_pretty(&texts).unwrap_or_default()
    } else {
        format!("{n} inputs to moderate, in the attached inputs.json.")
    };
    task::run(
        app,
        principal,
        headers,
        "moderations",
        Some(model.as_str()),
        message,
        attach,
        expected,
        false,
        spec,
        Delivery::Json,
        finish,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_scores() {
        let spec = json!({"count": 2});
        let v = parse_verdicts(r#"[[], ["violence", "self_harm_intent"]]"#, &spec).unwrap();
        assert!(v[0].is_empty());
        assert_eq!(v[1]["self-harm/intent"], json!(1.0));
        let v = parse_verdicts(r#"[{"hate": 0.2}, {"sexual": true}]"#, &spec).unwrap();
        assert_eq!(v[1]["sexual"], json!(1.0));
        assert!(parse_verdicts(r#"[["gore"], []]"#, &spec)
            .unwrap_err()
            .contains("unknown category"));
        assert!(parse_verdicts("[[]]", &spec)
            .unwrap_err()
            .contains("expected 2"));
        let one = json!({"count": 1});
        assert_eq!(
            parse_verdicts(r#"["hate"]"#, &one).unwrap()[0]["hate"],
            json!(1.0)
        );
        assert!(parse_verdicts("[]", &one).unwrap()[0].is_empty());
    }
}
