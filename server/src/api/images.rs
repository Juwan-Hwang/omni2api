//! `/v1/images/generations`: the seat makes the picture. The prompt reaches
//! it as one message with the request's shape (how many, size, format) as
//! its answer contract; it answers with image files (`oh-seat reply -a`),
//! and they come back as `b64_json` or `url`, or as `image_generation.*`
//! events when the caller streams.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::Json;
use serde_json::{json, Value};

use crate::api::task::{self, Delivery, Finished, Output};
use crate::auth::AuthPrincipal;
use crate::storage::AppState;

/// The fields of an image request that shape the answer.
struct Shape {
    n: usize,
    size: String,
    quality: Option<String>,
    background: Option<String>,
    style: Option<String>,
    output_format: String,
    url: bool,
    stream: bool,
}

fn str_field(b: &Value, k: &str) -> Option<String> {
    b.get(k)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn shape(b: &Value) -> Result<Shape, String> {
    let model = str_field(b, "model").unwrap_or_default();
    let n = match b.get("n") {
        None | Some(Value::Null) => 1,
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=10).contains(n))
            .ok_or("n must be an integer from 1 to 10")? as usize,
    };
    let output_format = str_field(b, "output_format")
        .unwrap_or_else(|| "png".into())
        .to_lowercase();
    if !["png", "jpeg", "jpg", "webp"].contains(&output_format.as_str()) {
        return Err("output_format must be png, jpeg or webp".into());
    }
    let url = match str_field(b, "response_format").as_deref() {
        Some("url") => true,
        Some("b64_json") => false,
        Some(o) => return Err(format!("response_format must be url or b64_json, not {o}")),
        // dall-e answers with links by default, gpt-image with base64.
        None => model.starts_with("dall-e"),
    };
    Ok(Shape {
        n,
        size: str_field(b, "size").unwrap_or_else(|| "auto".into()),
        quality: str_field(b, "quality").filter(|q| q != "auto"),
        background: str_field(b, "background").filter(|q| q != "auto"),
        style: str_field(b, "style"),
        output_format: if output_format == "jpg" {
            "jpeg".into()
        } else {
            output_format
        },
        url,
        stream: b.get("stream").and_then(|v| v.as_bool()).unwrap_or(false),
    })
}

/// The one line the seat reads to know what to send back.
fn contract(s: &Shape) -> String {
    let mut want = vec![format!("{} {} image(s)", s.n, s.output_format)];
    if s.size != "auto" {
        want.push(format!("size {}", s.size));
    }
    if let Some(q) = &s.quality {
        want.push(format!("quality {q}"));
    }
    if let Some(bg) = &s.background {
        want.push(format!("{bg} background"));
    }
    if let Some(st) = &s.style {
        want.push(format!("{st} style"));
    }
    let each = if s.n > 1 { " (one file per image)" } else { "" };
    format!(
        "{}: make the picture the message describes and attach the file{each}: `oh-seat reply -a out.{}`; any text you add becomes the revised prompt, text alone (no file) is a refusal",
        want.join(", "),
        if s.output_format == "jpeg" { "jpg" } else { &s.output_format }
    )
}

/// Rough token count for usage: four characters a token.
fn tokens(text: &str) -> u64 {
    (text.chars().count() as u64).div_ceil(4)
}

pub async fn generations(
    State(app): State<AppState>,
    principal: AuthPrincipal,
    headers: HeaderMap,
    Json(b): Json<Value>,
) -> Response {
    let Some(prompt) = str_field(&b, "prompt") else {
        return task::invalid("prompt is required");
    };
    let s = match shape(&b) {
        Ok(s) => s,
        Err(e) => return task::invalid(&e),
    };
    let expected = contract(&s);
    let model = str_field(&b, "model");
    let owner = principal.agent.id.clone();
    let hdrs = headers.clone();
    let prompt_tokens = tokens(&prompt);
    let delivery = if s.stream {
        Delivery::Sse
    } else {
        Delivery::Json
    };
    let finish: task::Finish = Box::new(move |answer| {
        let fail = |status: StatusCode, code: &str, msg: &str| {
            let err = json!({"error": {"message": msg, "type": "invalid_request_error", "code": code, "param": null}});
            if s.stream {
                Finished::Sse(vec![task::sse(
                    "error",
                    &json!({"type": "error", "error": err["error"]}),
                )])
            } else {
                Finished::Json(status, err)
            }
        };
        let a = match answer {
            Ok(a) => a,
            Err(e) => return fail(StatusCode::GATEWAY_TIMEOUT, "seat_unavailable", &e),
        };
        let images: Vec<&Output> = a
            .files
            .iter()
            .filter(|o| match o {
                Output::Bytes { mime, .. } => mime.starts_with("image/"),
                Output::Link(_) => true,
            })
            .collect();
        if images.is_empty() {
            let why = if a.text.is_empty() {
                "the seat answered without an image".to_string()
            } else {
                a.text.clone()
            };
            return fail(StatusCode::BAD_REQUEST, "image_generation_refused", &why);
        }
        let created = chrono::Utc::now().timestamp();
        let revised = (!a.text.is_empty()).then(|| a.text.clone());
        let mut data = Vec::new();
        let mut real_size = None;
        let mut real_format = None;
        for o in &images {
            let mut item = json!({});
            match o {
                Output::Bytes { mime, bytes, .. } => {
                    if real_size.is_none() {
                        real_size = task::image_size(bytes).map(|(w, h)| format!("{w}x{h}"));
                        real_format = mime.strip_prefix("image/").map(String::from);
                    }
                    if s.url && !s.stream {
                        item["url"] = json!(task::publish(&hdrs, &owner, mime, bytes.clone()));
                    } else {
                        item["b64_json"] = json!(crate::openai_compat::b64_encode(bytes));
                    }
                }
                Output::Link(u) => {
                    item["url"] = json!(u);
                }
            }
            if let Some(r) = &revised {
                item["revised_prompt"] = json!(r);
            }
            data.push(item);
        }
        let size = real_size.unwrap_or_else(|| s.size.clone());
        let format = real_format.unwrap_or_else(|| s.output_format.clone());
        let quality = s.quality.clone().unwrap_or_else(|| "auto".into());
        let background = s.background.clone().unwrap_or_else(|| "opaque".into());
        let usage = json!({
            "input_tokens": prompt_tokens,
            "input_tokens_details": {"text_tokens": prompt_tokens, "image_tokens": 0},
            "output_tokens": 0,
            "total_tokens": prompt_tokens,
        });
        if s.stream {
            let frames = data
                .iter()
                .map(|d| {
                    task::sse(
                        "image_generation.completed",
                        &json!({
                            "type": "image_generation.completed",
                            "b64_json": d.get("b64_json").cloned().unwrap_or(Value::Null),
                            "url": d.get("url").cloned().unwrap_or(Value::Null),
                            "created_at": created,
                            "size": size,
                            "quality": quality,
                            "background": background,
                            "output_format": format,
                            "usage": usage,
                        }),
                    )
                })
                .collect();
            return Finished::Sse(frames);
        }
        Finished::Json(
            StatusCode::OK,
            json!({
                "created": created,
                "data": data,
                "background": background,
                "output_format": format,
                "quality": quality,
                "size": size,
                "usage": usage,
            }),
        )
    });
    task::run(
        app,
        principal,
        headers,
        "images.generations",
        model.as_deref(),
        prompt,
        Vec::new(),
        expected,
        delivery,
        finish,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract_names_count_size_and_format() {
        let s = shape(&json!({"prompt": "x", "n": 2, "size": "512x512", "output_format": "webp"}))
            .unwrap();
        let c = contract(&s);
        assert!(c.starts_with("2 webp image(s), size 512x512"));
        assert!(c.contains("one file per image"));
        assert!(c.contains("out.webp"));
    }

    #[test]
    fn dalle_defaults_to_links() {
        assert!(shape(&json!({"model": "dall-e-3"})).unwrap().url);
        assert!(!shape(&json!({"model": "gpt-image-1"})).unwrap().url);
        assert!(shape(&json!({"n": 11})).is_err());
        assert!(shape(&json!({"response_format": "png"})).is_err());
    }
}
