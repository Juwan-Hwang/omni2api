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
    /// Streaming event prefix: `image_generation` or `image_edit`.
    event: &'static str,
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
        event: "image_generation",
    })
}

/// The one line the seat reads to know what to send back.
fn contract(s: &Shape, what: &str) -> String {
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
        "{}: {what} and attach the file{each}: `oh-seat reply -a out.{}`; any text you add becomes the revised prompt, text alone (no file) is a refusal",
        want.join(", "),
        if s.output_format == "jpeg" { "jpg" } else { &s.output_format }
    )
}

/// Rough token count for usage: four characters a token.
fn tokens(text: &str) -> u64 {
    (text.chars().count() as u64).div_ceil(4)
}

/// What an image endpoint makes of the seat's answer.
fn finish_images(s: Shape, prompt_tokens: u64, owner: String, hdrs: HeaderMap) -> task::Finish {
    Box::new(move |answer| {
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
            let kind = s.event;
            let frames = data
                .iter()
                .map(|d| {
                    task::sse(
                        &format!("{kind}.completed"),
                        &json!({
                            "type": format!("{kind}.completed"),
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
    })
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
    let expected = contract(&s, "make the picture the message describes");
    let model = str_field(&b, "model");
    let delivery = if s.stream {
        Delivery::Sse
    } else {
        Delivery::Json
    };
    let finish = finish_images(
        s,
        tokens(&prompt),
        principal.agent.id.clone(),
        headers.clone(),
    );
    task::run(
        app,
        principal,
        headers,
        "images.generations",
        model.as_deref(),
        prompt,
        Vec::new(),
        expected,
        true,
        Value::Null,
        delivery,
        finish,
    )
    .await
}

/// An image request with pictures: multipart (`image`, `image[]`, `mask`
/// files plus text fields) or JSON (`images: [{image_url | file_id}]`,
/// `mask: {...}`). Returns the fields as JSON and the pictures as data
/// URLs, the mask last.
async fn read_with_images(
    headers: &HeaderMap,
    body: axum::body::Bytes,
) -> Result<(Value, Vec<String>, bool), String> {
    let ct = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let mut fields = serde_json::Map::new();
    let mut images = Vec::new();
    let mut mask = None;
    if ct.starts_with("multipart/form-data") {
        use axum::extract::FromRequest;
        let req = axum::http::Request::builder()
            .header(axum::http::header::CONTENT_TYPE, ct.as_str())
            .body(axum::body::Body::from(body))
            .map_err(|e| e.to_string())?;
        let mut mp = axum::extract::Multipart::from_request(req, &())
            .await
            .map_err(|e| e.body_text())?;
        while let Some(field) = mp.next_field().await.map_err(|e| e.to_string())? {
            let name = field.name().unwrap_or("").to_string();
            let file_name = field.file_name().map(String::from);
            let mime = field.content_type().map(|m| m.to_string());
            let bytes = field.bytes().await.map_err(|e| e.to_string())?;
            let key = name.trim_end_matches("[]");
            if key == "image" || key == "mask" {
                if bytes.is_empty() {
                    continue;
                }
                let name = file_name.unwrap_or_else(|| format!("{key}.png"));
                let mime = crate::api::files::sniff_mime(&bytes)
                    .map(String::from)
                    .or(mime.filter(|m| m != "application/octet-stream"))
                    .unwrap_or_else(|| crate::openai_compat::guess_mime(&name).to_string());
                let url = crate::openai_compat::named_data_url(
                    &mime,
                    Some(&name),
                    &crate::openai_compat::b64_encode(&bytes),
                );
                if key == "mask" {
                    mask = Some(url);
                } else {
                    images.push(url);
                }
            } else {
                let text = String::from_utf8_lossy(&bytes).to_string();
                let v = match key {
                    "n" | "partial_images" | "output_compression" => text
                        .trim()
                        .parse::<u64>()
                        .map(Value::from)
                        .unwrap_or(Value::String(text)),
                    "stream" => Value::Bool(text.trim() == "true"),
                    _ => Value::String(text),
                };
                fields.insert(key.to_string(), v);
            }
        }
    } else {
        let v: Value = serde_json::from_slice(&body).map_err(|e| format!("invalid JSON: {e}"))?;
        let ref_url = |r: &Value| -> Option<String> {
            if let Some(u) = r.as_str() {
                return Some(u.to_string());
            }
            if let Some(u) = r
                .get("image_url")
                .and_then(|u| u.as_str().or_else(|| u.get("url").and_then(|x| x.as_str())))
            {
                return Some(u.to_string());
            }
            r.get("file_id")
                .and_then(|f| f.as_str())
                .and_then(crate::api::files::data_url)
        };
        for k in ["images", "image"] {
            match v.get(k) {
                Some(Value::Array(a)) => images.extend(a.iter().filter_map(ref_url)),
                Some(r) => images.extend(ref_url(r)),
                None => {}
            }
        }
        mask = v.get("mask").and_then(ref_url);
        if let Value::Object(o) = v {
            fields = o;
        }
    }
    if images.is_empty() {
        return Err("image is required".into());
    }
    let has_mask = mask.is_some();
    images.extend(mask);
    Ok((Value::Object(fields), images, has_mask))
}

pub async fn edits(
    State(app): State<AppState>,
    principal: AuthPrincipal,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let (b, images, has_mask) = match read_with_images(&headers, body).await {
        Ok(r) => r,
        Err(e) => return task::invalid(&e),
    };
    let Some(prompt) = str_field(&b, "prompt") else {
        return task::invalid("prompt is required");
    };
    let mut s = match shape(&b) {
        Ok(s) => s,
        Err(e) => return task::invalid(&e),
    };
    s.event = "image_edit";
    let n = images.len() - has_mask as usize;
    let what = match (n, has_mask) {
        (1, false) => "edit the attached image as the message says".to_string(),
        (1, true) => "edit the attached image as the message says, only where the mask (the last attachment) is transparent".to_string(),
        (k, false) => format!("make one picture from the {k} attached images as the message says"),
        (k, true) => format!("make one picture from the first {k} attached images as the message says, changing only where the mask (the last attachment) is transparent"),
    };
    let expected = contract(&s, &what);
    let model = str_field(&b, "model");
    let delivery = if s.stream {
        Delivery::Sse
    } else {
        Delivery::Json
    };
    let finish = finish_images(
        s,
        tokens(&prompt),
        principal.agent.id.clone(),
        headers.clone(),
    );
    task::run(
        app,
        principal,
        headers,
        "images.edits",
        model.as_deref(),
        prompt,
        images,
        expected,
        true,
        Value::Null,
        delivery,
        finish,
    )
    .await
}

pub async fn variations(
    State(app): State<AppState>,
    principal: AuthPrincipal,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let (b, images, _) = match read_with_images(&headers, body).await {
        Ok(r) => r,
        Err(e) => return task::invalid(&e),
    };
    let mut s = match shape(&b) {
        Ok(s) => s,
        Err(e) => return task::invalid(&e),
    };
    // dall-e-2 is the only variations model; it answers with links.
    if str_field(&b, "response_format").is_none() {
        s.url = true;
    }
    s.stream = false;
    let expected = contract(&s, "make a variation of the attached image");
    let model = str_field(&b, "model");
    let finish = finish_images(s, 0, principal.agent.id.clone(), headers.clone());
    task::run(
        app,
        principal,
        headers,
        "images.variations",
        model.as_deref(),
        "Variation of the attached image.".to_string(),
        images.into_iter().take(1).collect(),
        expected,
        true,
        Value::Null,
        Delivery::Json,
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
        let c = contract(&s, "make the picture the message describes");
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
