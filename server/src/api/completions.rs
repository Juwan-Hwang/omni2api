//! `/v1/completions` (legacy text completions): the prompt reaches the seat
//! as a task to continue the text; its answer is the continuation. `echo`,
//! `suffix`, `stop`, `max_tokens` and `n` are honored around it, and
//! `stream` gives `text_completion` chunks ending in `[DONE]`.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::Json;
use serde_json::{json, Value};

use crate::api::task::{self, Delivery, Finished};
use crate::auth::AuthPrincipal;
use crate::storage::AppState;

fn prompts(v: Option<&Value>) -> Result<Vec<String>, String> {
    match v {
        None | Some(Value::Null) => Ok(vec![String::new()]),
        Some(Value::String(s)) => Ok(vec![s.clone()]),
        Some(Value::Array(a)) if a.iter().all(|x| x.is_string()) && !a.is_empty() => Ok(a
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect()),
        Some(Value::Array(_)) => Err("prompt must be a string or a list of strings".into()),
        _ => Err("prompt must be a string or a list of strings".into()),
    }
}

/// Cut a continuation at the first stop sequence.
fn cut_at_stop(text: &str, stops: &[String]) -> (String, bool) {
    let hit = stops
        .iter()
        .filter(|s| !s.is_empty())
        .filter_map(|s| text.find(s.as_str()))
        .min();
    match hit {
        Some(i) => (text[..i].to_string(), true),
        None => (text.to_string(), false),
    }
}

pub async fn completions(
    State(app): State<AppState>,
    principal: AuthPrincipal,
    headers: HeaderMap,
    Json(b): Json<Value>,
) -> Response {
    let ps = match prompts(b.get("prompt")) {
        Ok(p) => p,
        Err(e) => return task::invalid(&e),
    };
    if ps.len() > 1 {
        // One task per prompt would race; a batch of prompts is rare in
        // practice and each becomes its own request upstream.
        return task::invalid("send one prompt per request");
    }
    let prompt = ps.into_iter().next().unwrap_or_default();
    let n = b.get("n").and_then(|v| v.as_u64()).unwrap_or(1).clamp(1, 8) as usize;
    let echo = b.get("echo").and_then(|v| v.as_bool()).unwrap_or(false);
    let stream = b.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    let include_usage = b
        .pointer("/stream_options/include_usage")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let suffix = b
        .get("suffix")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from);
    let max_tokens = b.get("max_tokens").and_then(|v| v.as_u64());
    let stops: Vec<String> = match b.get("stop") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    };
    let model = b
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let mut want = vec![if suffix.is_some() {
        "fill in the text that goes between the message (before) and the suffix (after)".to_string()
    } else {
        "continue the message text exactly where it stops".to_string()
    }];
    if let Some(m) = max_tokens {
        want.push(format!("at most ~{m} tokens"));
    }
    if !stops.is_empty() {
        want.push(format!("stop before any of {stops:?}"));
    }
    if n > 1 {
        want.push(format!(
            "{n} different continuations, separated by a line `@@@`"
        ));
    }
    let expected = format!(
        "{}: answer with the continuation only (no quotes, no repeat of the prompt)",
        want.join("; ")
    );
    let message = match &suffix {
        Some(s) => format!("{prompt}\n[suffix]\n{s}"),
        None => prompt.clone(),
    };
    let prompt_tokens = (prompt.chars().count() as u64).div_ceil(4);
    let id = format!("cmpl-{}", uuid::Uuid::new_v4().simple());
    let created = chrono::Utc::now().timestamp();
    let model_c = model.clone();
    let finish: task::Finish = Box::new(move |answer| {
        let a = match answer {
            Ok(a) => a,
            Err(e) => {
                let err = json!({"error": {"message": e, "type": "server_error", "code": "seat_unavailable", "param": null}});
                return if stream {
                    Finished::Sse(vec![format!("data: {err}\n\n"), "data: [DONE]\n\n".into()])
                } else {
                    Finished::Json(StatusCode::GATEWAY_TIMEOUT, err)
                };
            }
        };
        let raw = a.raw.clone();
        let mut parts: Vec<String> = if n > 1 {
            raw.split("\n@@@\n").map(String::from).collect()
        } else {
            vec![raw]
        };
        parts.resize(n, String::new());
        let choices: Vec<(String, &'static str)> = parts
            .into_iter()
            .map(|p| {
                let (mut t, _) = cut_at_stop(&p, &stops);
                let mut fin = "stop";
                if let Some(m) = max_tokens {
                    let cap = (m as usize).saturating_mul(4);
                    if t.chars().count() > cap {
                        t = t.chars().take(cap).collect();
                        fin = "length";
                    }
                }
                let t = if echo { format!("{prompt}{t}") } else { t };
                (t, fin)
            })
            .collect();
        let completion_tokens: u64 = choices
            .iter()
            .map(|(t, _)| (t.chars().count() as u64).div_ceil(4))
            .sum();
        let usage = json!({
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": prompt_tokens + completion_tokens,
        });
        if stream {
            let mut frames = Vec::new();
            for (i, (t, fin)) in choices.iter().enumerate() {
                frames.push(format!(
                    "data: {}\n\n",
                    json!({"id": id, "object": "text_completion", "created": created, "model": model_c,
                           "choices": [{"text": t, "index": i, "logprobs": null, "finish_reason": null}]})
                ));
                frames.push(format!(
                    "data: {}\n\n",
                    json!({"id": id, "object": "text_completion", "created": created, "model": model_c,
                           "choices": [{"text": "", "index": i, "logprobs": null, "finish_reason": fin}]})
                ));
            }
            if include_usage {
                frames.push(format!(
                    "data: {}\n\n",
                    json!({"id": id, "object": "text_completion", "created": created, "model": model_c,
                           "choices": [], "usage": usage})
                ));
            }
            frames.push("data: [DONE]\n\n".into());
            return Finished::Sse(frames);
        }
        Finished::Json(
            StatusCode::OK,
            json!({
                "id": id,
                "object": "text_completion",
                "created": created,
                "model": model_c,
                "choices": choices.iter().enumerate().map(|(i, (t, fin))| json!({
                    "text": t, "index": i, "logprobs": null, "finish_reason": fin,
                })).collect::<Vec<_>>(),
                "usage": usage,
            }),
        )
    });
    task::run(
        app,
        principal,
        headers,
        "completions",
        Some(model.as_str()).filter(|m| !m.is_empty()),
        message,
        Vec::new(),
        expected,
        false,
        Value::Null,
        if stream {
            Delivery::Sse
        } else {
            Delivery::Json
        },
        finish,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stops_cut_the_text() {
        let (t, hit) = cut_at_stop("one\ntwo\nthree", &["\n".into()]);
        assert_eq!((t.as_str(), hit), ("one", true));
        assert_eq!(cut_at_stop("abc", &[]).0, "abc");
        assert!(prompts(Some(&json!(["a", "b"]))).unwrap().len() == 2);
        assert!(prompts(Some(&json!([1, 2]))).is_err());
    }
}
