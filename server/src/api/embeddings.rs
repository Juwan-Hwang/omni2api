//! `/v1/embeddings`: the seat is the embedding model. The inputs reach it
//! as a JSON array of strings with the count and length its vectors must
//! have; it answers with the vectors as JSON (`oh-seat reply -f
//! vectors.json`). A malformed answer is refused to the seat at reply time,
//! so the caller only ever sees a well-formed list. Longer vectors than
//! `dimensions` are shortened and re-normalized (the OpenAI semantics of
//! `dimensions`), and `encoding_format: "base64"` packs little-endian f32.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::Json;
use serde_json::{json, Value};

use crate::api::task::{self, Delivery, Finished};
use crate::auth::AuthPrincipal;
use crate::storage::AppState;

/// The vector length a model name implies, when it implies one.
fn model_dims(model: &str) -> Option<usize> {
    let m = model.to_lowercase();
    if m.contains("3-large") {
        Some(3072)
    } else if m.contains("3-small") || m.contains("ada-002") {
        Some(1536)
    } else {
        None
    }
}

/// Inputs as strings: text, a list of texts, token ids (decoded with
/// cl100k), or lists of token ids.
fn inputs(v: &Value) -> Result<Vec<String>, String> {
    let decode = |ids: &[Value]| -> Result<String, String> {
        let ids: Vec<u32> = ids
            .iter()
            .map(|x| {
                x.as_u64()
                    .map(|n| n as u32)
                    .ok_or("token ids must be integers")
            })
            .collect::<Result<_, _>>()?;
        let bpe = tiktoken_rs::cl100k_base().map_err(|e| e.to_string())?;
        bpe.decode(ids).map_err(|e| e.to_string())
    };
    match v {
        Value::String(s) => Ok(vec![s.clone()]),
        Value::Array(a) if a.is_empty() => Err("input must not be empty".into()),
        Value::Array(a) if a.iter().all(|x| x.is_number()) => Ok(vec![decode(a)?]),
        Value::Array(a) => a
            .iter()
            .map(|x| match x {
                Value::String(s) => Ok(s.clone()),
                Value::Array(ids) => decode(ids),
                _ => Err("input items must be strings or token id lists".to_string()),
            })
            .collect(),
        _ => Err("input must be a string or an array".into()),
    }
}

/// The vectors in a seat's answer, checked against the spec
/// (`{"count", "dims"}`): a JSON list of lists, `{"data": [{"embedding"}]}`,
/// `{"embeddings": [...]}`, or one vector per line.
pub fn parse_vectors(text: &str, spec: &Value) -> Result<Vec<Vec<f64>>, String> {
    let count = spec.get("count").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
    let dims = spec
        .get("dims")
        .and_then(|v| v.as_u64())
        .map(|d| d as usize);
    let t = text.trim();
    let as_vec =
        |v: &Value| -> Option<Vec<f64>> { v.as_array()?.iter().map(|x| x.as_f64()).collect() };
    let vectors: Vec<Vec<f64>> = match serde_json::from_str::<Value>(t) {
        Ok(v) => {
            let list = v
                .get("data")
                .or_else(|| v.get("embeddings"))
                .cloned()
                .unwrap_or(v);
            let list = match list {
                Value::Array(a) if a.first().is_some_and(|x| x.is_number()) => {
                    vec![Value::Array(a)]
                }
                Value::Array(a) => a,
                _ => return Err("answer with a JSON list of vectors: [[0.1, ...], ...]".into()),
            };
            list.iter()
                .enumerate()
                .map(|(i, x)| {
                    as_vec(x.get("embedding").unwrap_or(x))
                        .ok_or(format!("vector {} is not a list of numbers", i + 1))
                })
                .collect::<Result<_, _>>()?
        }
        Err(_) => t
            .lines()
            .filter(|l| !l.trim().is_empty())
            .enumerate()
            .map(|(i, l)| {
                l.split(|c: char| c == ',' || c.is_whitespace())
                    .filter(|x| !x.is_empty())
                    .map(|x| x.trim_matches(['[', ']']).parse::<f64>())
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|_| {
                        format!(
                            "line {} is not a vector; answer with JSON [[0.1, ...], ...]",
                            i + 1
                        )
                    })
            })
            .collect::<Result<_, _>>()?,
    };
    if vectors.len() != count {
        return Err(format!(
            "expected {count} vector(s), one per input in order; got {}",
            vectors.len()
        ));
    }
    if let Some(d) = dims {
        if let Some((i, v)) = vectors.iter().enumerate().find(|(_, v)| v.len() < d) {
            return Err(format!(
                "vector {} has {} numbers; each needs {d} (longer is fine: it is shortened)",
                i + 1,
                v.len()
            ));
        }
    } else if vectors
        .iter()
        .any(|v| v.len() != vectors[0].len() || v.is_empty())
    {
        return Err("all vectors must have the same, non-zero length".into());
    }
    if vectors.iter().flatten().any(|x| !x.is_finite()) {
        return Err("vectors must hold finite numbers".into());
    }
    Ok(vectors)
}

/// Cut to `d` numbers and scale back to unit length.
fn shorten(mut v: Vec<f64>, d: Option<usize>) -> Vec<f64> {
    if let Some(d) = d.filter(|d| *d < v.len()) {
        v.truncate(d);
        let n = v.iter().map(|x| x * x).sum::<f64>().sqrt();
        if n > 0.0 {
            v.iter_mut().for_each(|x| *x /= n);
        }
    }
    v
}

pub async fn embeddings(
    State(app): State<AppState>,
    principal: AuthPrincipal,
    headers: HeaderMap,
    Json(b): Json<Value>,
) -> Response {
    let texts = match b.get("input").map(inputs) {
        Some(Ok(t)) => t,
        Some(Err(e)) => return task::invalid(&e),
        None => return task::invalid("input is required"),
    };
    if texts.len() > 2048 {
        return task::invalid("at most 2048 inputs per request");
    }
    let model = b
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let requested = match b.get("dimensions") {
        None | Some(Value::Null) => None,
        Some(v) => match v.as_u64().filter(|d| *d > 0) {
            Some(d) => Some(d as usize),
            None => return task::invalid("dimensions must be a positive integer"),
        },
    };
    let base64 = match b.get("encoding_format").and_then(|v| v.as_str()) {
        None | Some("float") => false,
        Some("base64") => true,
        Some(o) => {
            return task::invalid(&format!("encoding_format must be float or base64, not {o}"))
        }
    };
    // What the seat must send: the model's own length when it has one
    // (the caller's `dimensions` then shortens it), else what was asked.
    let dims = model_dims(&model).or(requested);
    let n = texts.len();
    let length = match dims {
        Some(d) => format!("{d} floats each"),
        None => "the same length each (any)".to_string(),
    };
    let expected = format!(
        "{n} embedding vector(s), {length}, one per text of the attached inputs.json list, in order (similar texts get nearby vectors): answer with JSON [[0.1, ...], ...] in a file: `reply -f vectors.json`"
    );
    let spec = json!({"count": n, "dims": dims});
    let tokens: u64 = texts
        .iter()
        .map(|t| (t.chars().count() as u64).div_ceil(4))
        .sum();
    let model_name = model.clone();
    let spec_c = spec.clone();
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
        let vectors = match parse_vectors(&a.raw, &spec_c) {
            Ok(v) => v,
            Err(e) => {
                return Finished::Json(
                    StatusCode::BAD_GATEWAY,
                    json!({"error": {"message": e, "type": "server_error", "code": "bad_answer", "param": null}}),
                )
            }
        };
        let data: Vec<Value> = vectors
            .into_iter()
            .enumerate()
            .map(|(i, v)| {
                let v = shorten(v, requested);
                let embedding = if base64 {
                    let bytes: Vec<u8> = v.iter().flat_map(|x| (*x as f32).to_le_bytes()).collect();
                    json!(crate::openai_compat::b64_encode(&bytes))
                } else {
                    json!(v)
                };
                json!({"object": "embedding", "index": i, "embedding": embedding})
            })
            .collect();
        Finished::Json(
            StatusCode::OK,
            json!({
                "object": "list",
                "data": data,
                "model": model_name,
                "usage": {"prompt_tokens": tokens, "total_tokens": tokens},
            }),
        )
    });
    // The texts as a file the seat can feed straight to its embedder, and
    // inline too while they are short enough to read.
    let list = serde_json::to_string(&texts).unwrap_or_default();
    let file = crate::openai_compat::named_data_url(
        "application/json",
        Some("inputs.json"),
        &crate::openai_compat::b64_encode(list.as_bytes()),
    );
    let message = if list.chars().count() <= 2000 {
        serde_json::to_string_pretty(&texts).unwrap_or_default()
    } else {
        format!("{n} texts to embed, in the attached inputs.json.")
    };
    task::run(
        app,
        principal,
        headers,
        "embeddings",
        Some(model.as_str()).filter(|m| !m.is_empty()),
        message,
        vec![file],
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
    fn vectors_in_any_common_shape() {
        let spec = json!({"count": 2, "dims": 3});
        assert_eq!(parse_vectors("[[1,0,0],[0,1,0]]", &spec).unwrap().len(), 2);
        assert_eq!(
            parse_vectors(
                r#"{"data":[{"embedding":[1,0,0]},{"embedding":[0,1,0,5]}]}"#,
                &spec
            )
            .unwrap()[1]
                .len(),
            4
        );
        assert!(parse_vectors("1 0 0\n0, 1, 0\n", &spec).is_ok());
        assert!(parse_vectors("[[1,0,0]]", &spec)
            .unwrap_err()
            .contains("expected 2"));
        assert!(parse_vectors("[[1,0],[0,1]]", &spec)
            .unwrap_err()
            .contains("needs 3"));
        let one = json!({"count": 1, "dims": null});
        assert_eq!(
            parse_vectors("[0.5, 0.5]", &one).unwrap(),
            vec![vec![0.5, 0.5]]
        );
    }

    #[test]
    fn shortening_renormalizes() {
        let v = shorten(vec![3.0, 4.0, 12.0], Some(2));
        assert!((v[0] - 0.6).abs() < 1e-9 && (v[1] - 0.8).abs() < 1e-9);
    }

    #[test]
    fn token_inputs_decode() {
        let bpe = tiktoken_rs::cl100k_base().unwrap();
        let ids: Vec<Value> = bpe
            .encode_ordinary("hello world")
            .into_iter()
            .map(Value::from)
            .collect();
        assert_eq!(inputs(&Value::Array(ids)).unwrap(), vec!["hello world"]);
        assert_eq!(model_dims("text-embedding-3-large"), Some(3072));
    }
}
