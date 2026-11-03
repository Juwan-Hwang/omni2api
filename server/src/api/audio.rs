//! `/v1/audio/*`: the seat speaks and listens.
//!
//! - `speech`: the text reaches the seat with the voice and format as its
//!   contract; it answers with an audio file, returned as the body (or as
//!   `speech.audio.*` events with `stream_format: "sse"`).
//! - `transcriptions` / `translations`: the audio reaches the seat as an
//!   attachment; it answers with the transcript, as plain text or, when
//!   timings are wanted, as SRT, which is turned into whatever format the
//!   caller asked for.

use axum::body::Bytes;
use axum::extract::{FromRequest, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::Json;
use serde_json::{json, Value};

use crate::api::task::{self, Delivery, Finished, Output};
use crate::auth::AuthPrincipal;
use crate::storage::AppState;

fn str_field(b: &Value, k: &str) -> Option<String> {
    b.get(k)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn audio_mime(format: &str) -> &'static str {
    match format {
        "mp3" => "audio/mpeg",
        "opus" => "audio/opus",
        "aac" => "audio/aac",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "pcm" => "audio/pcm",
        _ => "application/octet-stream",
    }
}

fn fail_json(status: StatusCode, code: &str, msg: &str) -> Finished {
    Finished::Json(
        status,
        json!({"error": {"message": msg, "type": "invalid_request_error", "code": code, "param": null}}),
    )
}

// ── speech ──

pub async fn speech(
    State(app): State<AppState>,
    principal: AuthPrincipal,
    headers: HeaderMap,
    Json(b): Json<Value>,
) -> Response {
    let Some(input) = b
        .get("input")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(String::from)
    else {
        return task::invalid("input is required");
    };
    let voice_id = match b.get("voice") {
        Some(Value::String(v)) if !v.trim().is_empty() => v.trim().to_string(),
        Some(Value::Object(o)) => o
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("custom")
            .to_string(),
        _ => return task::invalid("voice is required"),
    };
    // A custom voice: speak like its sample (attached) or its description.
    let mut voice_files = Vec::new();
    let mut voice_note = None;
    let voice = match crate::api::library::voice_for_speech(&principal.agent.id, &voice_id) {
        Some((name, prompt, sample)) => {
            if let Some(s) = sample {
                voice_files.push(s);
                voice_note = Some("sound like the attached voice sample".to_string());
            } else if let Some(p) = prompt {
                voice_note = Some(format!("the voice: {p}"));
            }
            name
        }
        None if voice_id.starts_with("voice_") => {
            return task::invalid(&format!("No voice found with id '{voice_id}'."))
        }
        None => voice_id,
    };
    let format = str_field(&b, "response_format")
        .unwrap_or_else(|| "mp3".into())
        .to_lowercase();
    if !["mp3", "opus", "aac", "flac", "wav", "pcm"].contains(&format.as_str()) {
        return task::invalid("response_format must be mp3, opus, aac, flac, wav or pcm");
    }
    let speed = b.get("speed").and_then(|v| v.as_f64()).unwrap_or(1.0);
    if !(0.25..=4.0).contains(&speed) {
        return task::invalid("speed must be between 0.25 and 4.0");
    }
    let sse = str_field(&b, "stream_format").as_deref() == Some("sse");
    let mut want = vec![format!("voice {voice}"), format.clone()];
    if (speed - 1.0).abs() > 1e-6 {
        want.push(format!("speed {speed}"));
    }
    if format == "pcm" {
        want.push("raw 24 kHz 16-bit mono".into());
    }
    let mut expected = format!(
        "{}: speak the message text aloud, word for word, and attach the audio file: `oh-seat reply -a out.{format}`; text alone (no file) is a refusal",
        want.join(", ")
    );
    if let Some(i) = str_field(&b, "instructions") {
        expected.push_str(&format!("; how to speak: {i}"));
    }
    if let Some(n) = voice_note {
        expected.push_str(&format!("; {n}"));
    }
    let tokens = (input.chars().count() as u64).div_ceil(4);
    let finish: task::Finish = Box::new(move |answer| {
        let fail = |status: StatusCode, code: &str, msg: &str| {
            if sse {
                Finished::Sse(vec![task::sse(
                    "error",
                    &json!({"type": "error", "error": {"message": msg, "code": code}}),
                )])
            } else {
                fail_json(status, code, msg)
            }
        };
        let a = match answer {
            Ok(a) => a,
            Err(e) => return fail(StatusCode::GATEWAY_TIMEOUT, "seat_unavailable", &e),
        };
        let audio = a.files.iter().find_map(|o| match o {
            Output::Bytes { mime, bytes, .. }
                if mime.starts_with("audio/") || mime == "application/octet-stream" =>
            {
                Some((mime.clone(), bytes.clone()))
            }
            _ => None,
        });
        let Some((mime, bytes)) = audio else {
            let why = if a.text.is_empty() {
                "the seat answered without audio".to_string()
            } else {
                a.text.clone()
            };
            return fail(StatusCode::BAD_REQUEST, "speech_refused", &why);
        };
        if sse {
            let usage = json!({"input_tokens": tokens, "output_tokens": 0, "total_tokens": tokens});
            let mut frames: Vec<String> = bytes
                .chunks(48 * 1024)
                .map(|c| {
                    task::sse(
                        "speech.audio.delta",
                        &json!({"type": "speech.audio.delta", "audio": crate::openai_compat::b64_encode(c)}),
                    )
                })
                .collect();
            frames.push(task::sse(
                "speech.audio.done",
                &json!({"type": "speech.audio.done", "usage": usage}),
            ));
            return Finished::Sse(frames);
        }
        // The caller asked for one format; a seat that made another still
        // says which in the content type.
        let ct = if mime == "application/octet-stream" {
            audio_mime(&format).to_string()
        } else {
            mime
        };
        Finished::Raw(StatusCode::OK, ct, bytes)
    });
    let model = str_field(&b, "model");
    task::run(
        app,
        principal,
        headers,
        "audio.speech",
        model.as_deref(),
        input,
        voice_files,
        expected,
        true,
        Value::Null,
        if sse {
            Delivery::Sse
        } else {
            Delivery::JsonRaw
        },
        finish,
    )
    .await
}

// ── transcriptions / translations ──

/// The multipart fields of a transcription request, and its audio as a
/// named data URL.
async fn read_audio_form(headers: &HeaderMap, body: Bytes) -> Result<(Value, String), String> {
    let ct = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if !ct.starts_with("multipart/form-data") {
        return Err("expected multipart/form-data with a `file` field".into());
    }
    let req = axum::http::Request::builder()
        .header(axum::http::header::CONTENT_TYPE, ct.as_str())
        .body(axum::body::Body::from(body))
        .map_err(|e| e.to_string())?;
    let mut mp = axum::extract::Multipart::from_request(req, &())
        .await
        .map_err(|e| e.body_text())?;
    let mut fields = serde_json::Map::new();
    let mut file = None;
    while let Some(field) = mp.next_field().await.map_err(|e| e.to_string())? {
        let name = field.name().unwrap_or("").to_string();
        let file_name = field.file_name().map(String::from);
        let mime = field.content_type().map(String::from);
        let bytes = field.bytes().await.map_err(|e| e.to_string())?;
        let key = name.trim_end_matches("[]").to_string();
        if key == "file" {
            let fname = file_name.unwrap_or_else(|| "audio".into());
            let mime = crate::api::files::sniff_mime(&bytes)
                .map(String::from)
                .or(mime.filter(|m| m != "application/octet-stream"))
                .unwrap_or_else(|| crate::openai_compat::guess_mime(&fname).to_string());
            file = Some(crate::openai_compat::named_data_url(
                &mime,
                Some(&fname),
                &crate::openai_compat::b64_encode(&bytes),
            ));
            continue;
        }
        let text = String::from_utf8_lossy(&bytes).to_string();
        match fields.get_mut(&key) {
            // Repeated fields (timestamp_granularities[], include[]).
            Some(Value::Array(a)) => a.push(Value::String(text)),
            Some(prev) => {
                let p = prev.take();
                *prev = Value::Array(vec![p, Value::String(text)]);
            }
            None => {
                fields.insert(key, Value::String(text));
            }
        }
    }
    let file = file.ok_or("file is required")?;
    Ok((Value::Object(fields), file))
}

/// One cue of a timed transcript.
#[derive(Debug, Clone, PartialEq)]
struct Cue {
    start: f64,
    end: f64,
    text: String,
}

fn parse_ts(s: &str) -> Option<f64> {
    let s = s.trim().replace(',', ".");
    let parts: Vec<&str> = s.split(':').collect();
    let mut t = 0.0;
    for p in &parts {
        t = t * 60.0 + p.parse::<f64>().ok()?;
    }
    Some(t)
}

/// Cues out of SRT or WebVTT text; empty when it has no timings.
fn parse_cues(text: &str) -> Vec<Cue> {
    let mut out = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let Some((a, b)) = line.split_once("-->") else {
            continue;
        };
        let (Some(start), Some(end)) = (
            parse_ts(a),
            parse_ts(b.split_whitespace().next().unwrap_or("")),
        ) else {
            continue;
        };
        let mut t = Vec::new();
        while let Some(l) = lines.peek() {
            if l.trim().is_empty() {
                break;
            }
            t.push(lines.next().unwrap().trim().to_string());
        }
        out.push(Cue {
            start,
            end,
            text: t.join(" "),
        });
    }
    out
}

fn fmt_ts(t: f64, sep: char) -> String {
    let ms = (t * 1000.0).round() as u64;
    format!(
        "{:02}:{:02}:{:02}{sep}{:03}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    )
}

fn to_srt(cues: &[Cue]) -> String {
    cues.iter()
        .enumerate()
        .map(|(i, c)| {
            format!(
                "{}\n{} --> {}\n{}\n",
                i + 1,
                fmt_ts(c.start, ','),
                fmt_ts(c.end, ','),
                c.text
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn to_vtt(cues: &[Cue]) -> String {
    let body: Vec<String> = cues
        .iter()
        .map(|c| {
            format!(
                "{} --> {}\n{}\n",
                fmt_ts(c.start, '.'),
                fmt_ts(c.end, '.'),
                c.text
            )
        })
        .collect();
    format!("WEBVTT\n\n{}", body.join("\n"))
}

async fn transcribe(
    app: AppState,
    principal: AuthPrincipal,
    headers: HeaderMap,
    body: Bytes,
    translate: bool,
) -> Response {
    let (b, file) = match read_audio_form(&headers, body).await {
        Ok(r) => r,
        Err(e) => return task::invalid(&e),
    };
    let format = str_field(&b, "response_format")
        .unwrap_or_else(|| "json".into())
        .to_lowercase();
    let formats = if translate {
        &["json", "text", "srt", "verbose_json", "vtt"][..]
    } else {
        &[
            "json",
            "text",
            "srt",
            "verbose_json",
            "vtt",
            "diarized_json",
        ][..]
    };
    if !formats.contains(&format.as_str()) {
        return task::invalid(&format!(
            "response_format must be one of {}",
            formats.join(", ")
        ));
    }
    let stream = !translate && str_field(&b, "stream").as_deref() == Some("true");
    // Raw bodies (plain text, srt, vtt) can never share a response with
    // keepalive padding; the JSON formats can — padding is valid leading
    // whitespace, so they keep the slow-path hold instead of dying at the
    // tunnel's ~100 s.
    let delivery = if stream {
        Delivery::Sse
    } else {
        match format.as_str() {
            "text" | "srt" | "vtt" => Delivery::JsonRaw,
            _ => Delivery::Json,
        }
    };
    let timed = matches!(
        format.as_str(),
        "srt" | "vtt" | "verbose_json" | "diarized_json"
    );
    let language = str_field(&b, "language");
    let mut what = if translate {
        "translate the attached audio into English".to_string()
    } else {
        match &language {
            Some(l) => format!("transcribe the attached audio (language {l})"),
            None => "transcribe the attached audio".to_string(),
        }
    };
    if timed {
        what.push_str(
            "; answer with the text as SRT (numbered cues with HH:MM:SS,mmm --> HH:MM:SS,mmm timings)",
        );
        if format == "diarized_json" {
            what.push_str(", each cue starting with its speaker and a colon (A: ...)");
        }
    } else {
        what.push_str("; answer with the text alone");
    }
    if let Some(p) = str_field(&b, "prompt") {
        what.push_str(&format!("; context from the caller: {p}"));
    }
    let api = if translate {
        "audio.translations"
    } else {
        "audio.transcriptions"
    };
    let lang = language.clone();
    let finish: task::Finish = Box::new(move |answer| {
        let a = match answer {
            Ok(a) => a,
            Err(e) => {
                return if stream {
                    Finished::Sse(vec![task::sse(
                        "error",
                        &json!({"type": "error", "error": {"message": e}}),
                    )])
                } else {
                    fail_json(StatusCode::GATEWAY_TIMEOUT, "seat_unavailable", &e)
                };
            }
        };
        let raw = a.raw.trim().to_string();
        let cues = parse_cues(&raw);
        // Plain text either way: the cue texts, or the answer as given.
        let text = if cues.is_empty() {
            raw.clone()
        } else {
            cues.iter()
                .map(|c| c.text.clone())
                .collect::<Vec<_>>()
                .join(" ")
        };
        let cues = if cues.is_empty() {
            vec![Cue {
                start: 0.0,
                end: 0.0,
                text: text.clone(),
            }]
        } else {
            cues
        };
        let duration = cues.last().map(|c| c.end).unwrap_or(0.0);
        let tokens = (text.chars().count() as u64).div_ceil(4);
        let usage = json!({"type": "tokens", "input_tokens": 0, "output_tokens": tokens, "total_tokens": tokens});
        if stream {
            let mut frames: Vec<String> = text
                .split_inclusive(' ')
                .collect::<Vec<_>>()
                .chunks(8)
                .map(|w| {
                    task::sse(
                        "transcript.text.delta",
                        &json!({"type": "transcript.text.delta", "delta": w.concat()}),
                    )
                })
                .collect();
            frames.push(task::sse(
                "transcript.text.done",
                &json!({"type": "transcript.text.done", "text": text, "usage": usage}),
            ));
            return Finished::Sse(frames);
        }
        match format.as_str() {
            "text" => Finished::Raw(
                StatusCode::OK,
                "text/plain; charset=utf-8".into(),
                text.into_bytes(),
            ),
            "srt" => Finished::Raw(
                StatusCode::OK,
                "text/plain; charset=utf-8".into(),
                to_srt(&cues).into_bytes(),
            ),
            "vtt" => Finished::Raw(
                StatusCode::OK,
                "text/vtt; charset=utf-8".into(),
                to_vtt(&cues).into_bytes(),
            ),
            "verbose_json" => {
                let segments: Vec<Value> = cues
                    .iter()
                    .enumerate()
                    .map(|(i, c)| {
                        json!({"id": i, "seek": 0, "start": c.start, "end": c.end, "text": c.text,
                               "tokens": [], "temperature": 0.0, "avg_logprob": 0.0,
                               "compression_ratio": 1.0, "no_speech_prob": 0.0})
                    })
                    .collect();
                Finished::Json(
                    StatusCode::OK,
                    json!({
                        "task": if translate { "translate" } else { "transcribe" },
                        "language": if translate { "english".to_string() } else { lang.clone().unwrap_or_else(|| "unknown".into()) },
                        "duration": duration,
                        "text": text,
                        "segments": segments,
                        "usage": {"type": "duration", "seconds": duration.ceil() as u64},
                    }),
                )
            }
            "diarized_json" => {
                let segments: Vec<Value> = cues
                    .iter()
                    .enumerate()
                    .map(|(i, c)| {
                        let (speaker, t) = match c.text.split_once(':') {
                            Some((s, t)) if s.len() <= 24 && !s.contains(' ') => {
                                (s.trim().to_string(), t.trim().to_string())
                            }
                            _ => ("A".to_string(), c.text.clone()),
                        };
                        json!({"type": "transcript.text.segment", "id": format!("seg_{i:03}"),
                               "start": c.start, "end": c.end, "speaker": speaker, "text": t})
                    })
                    .collect();
                Finished::Json(
                    StatusCode::OK,
                    json!({"task": "transcribe", "duration": duration, "text": text,
                           "segments": segments, "usage": usage}),
                )
            }
            _ => Finished::Json(StatusCode::OK, json!({"text": text, "usage": usage})),
        }
    });
    let model = str_field(&b, "model");
    let message = if translate {
        "Translate the attached audio.".to_string()
    } else {
        "Transcribe the attached audio.".to_string()
    };
    task::run(
        app,
        principal,
        headers,
        api,
        model.as_deref(),
        message,
        vec![file],
        what,
        false,
        Value::Null,
        delivery,
        finish,
    )
    .await
}

pub async fn transcriptions(
    State(app): State<AppState>,
    principal: AuthPrincipal,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    transcribe(app, principal, headers, body, false).await
}

pub async fn translations(
    State(app): State<AppState>,
    principal: AuthPrincipal,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    transcribe(app, principal, headers, body, true).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srt_round_trip() {
        let srt = "1\n00:00:00,000 --> 00:00:01,500\nHello there.\n\n2\n00:00:01,500 --> 00:00:03,000\nGeneral Kenobi.\n";
        let cues = parse_cues(srt);
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[1].start, 1.5);
        assert_eq!(cues[1].text, "General Kenobi.");
        assert_eq!(to_srt(&cues).trim(), srt.trim());
        assert!(to_vtt(&cues).starts_with("WEBVTT\n\n00:00:00.000 --> 00:00:01.500\nHello there."));
    }

    #[test]
    fn plain_text_has_no_cues() {
        assert!(parse_cues("just words").is_empty());
        assert_eq!(parse_ts("01:02.5"), Some(62.5));
    }
}
