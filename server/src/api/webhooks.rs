//! Webhook endpoints: register a URL for event types, and the server
//! posts `{"id","object":"event","created_at","type","data":{"id"}}` to
//! it when such an event happens here (a background response, a batch,
//! a video, a fine-tuning job or eval run, an agent session), signed the
//! Standard Webhooks way (`webhook-id`, `webhook-timestamp`,
//! `webhook-signature: v1,<base64 HMAC-SHA256 of id.timestamp.body>`
//! with the `whsec_` secret), which the SDKs' `webhooks.unwrap` checks.

// Handlers return the HTTP error response itself as their `Err`.
#![allow(clippy::result_large_err)]

use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::api::stored::{self, Stored};
use crate::auth::AuthPrincipal;

pub const EVENT_TYPES: &[&str] = &[
    "batch.completed",
    "batch.failed",
    "batch.expired",
    "batch.cancelled",
    "response.completed",
    "response.failed",
    "response.cancelled",
    "response.incomplete",
    "eval.run.succeeded",
    "eval.run.failed",
    "eval.run.canceled",
    "fine_tuning.job.succeeded",
    "fine_tuning.job.failed",
    "fine_tuning.job.cancelled",
    "realtime.call.incoming",
    "video.completed",
    "video.failed",
    "agent.environment.ready",
    "agent.environment.failed",
    "agent.environment.suspended",
    "agent.environment.expired",
    "agent.session.created",
    "agent.session.action_required",
    "agent.session.in_progress",
    "agent.session.idle",
    "agent.session.failed",
    "safety.alert.created",
];

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn err(msg: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error": {"message": msg, "type": "invalid_request_error", "code": null, "param": null}})),
    )
        .into_response()
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let pad = |b: u8| k.iter().map(|x| x ^ b).collect::<Vec<u8>>();
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(msg)
        .finalize();
    Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .into()
}

/// The `webhook-signature` value for a delivery.
pub fn sign(secret: &str, id: &str, ts: i64, body: &str) -> String {
    let key = secret
        .strip_prefix("whsec_")
        .and_then(crate::openai_compat::b64_decode)
        .unwrap_or_else(|| secret.as_bytes().to_vec());
    let mac = hmac_sha256(&key, format!("{id}.{ts}.{body}").as_bytes());
    format!("v1,{}", crate::openai_compat::b64_encode(&mac))
}

fn new_secret() -> String {
    let raw: Vec<u8> = uuid::Uuid::new_v4()
        .as_bytes()
        .iter()
        .chain(uuid::Uuid::new_v4().as_bytes())
        .copied()
        .collect();
    format!("whsec_{}", crate::openai_compat::b64_encode(&raw))
}

fn hint(secret: &str) -> String {
    let tail: String = secret
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("whsec_...{tail}")
}

fn types_of(b: &Value) -> Result<Vec<String>, Response> {
    let Some(a) = b["event_types"].as_array() else {
        return Err(err("event_types is required"));
    };
    let mut out = Vec::new();
    for t in a {
        let t = t.as_str().unwrap_or("");
        if !EVENT_TYPES.contains(&t) {
            return Err(err(&format!("unknown event type {t}")));
        }
        out.push(t.to_string());
    }
    Ok(out)
}

fn valid_url(u: &str) -> bool {
    u.starts_with("https://") || u.starts_with("http://")
}

/// The endpoint as shown; the secret stays in `history[0]`.
fn endpoint(owner: &str, id: &str) -> Option<Stored> {
    stored::get(owner, id).filter(|s| s.obj["object"] == "webhook_endpoint")
}

fn secret_of(s: &Stored) -> Vec<String> {
    s.history
        .iter()
        .filter(|h| h["until"].as_i64().is_none_or(|u| u > now()))
        .filter_map(|h| h["secret"].as_str().map(String::from))
        .collect()
}

pub async fn create(principal: AuthPrincipal, Json(b): Json<Value>) -> Response {
    let types = match types_of(&b) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let Some(name) = b["name"].as_str() else {
        return err("name is required");
    };
    let Some(url) = b["url"].as_str().filter(|u| valid_url(u)) else {
        return err("url must be an http(s) URL");
    };
    let id = format!("we_{}", uuid::Uuid::now_v7().simple());
    let secret = new_secret();
    let obj = json!({"id": id, "object": "webhook_endpoint", "created_at": now(), "updated_at": now(),
        "event_types": types, "name": name, "url": url, "signing_secret_hint": hint(&secret)});
    stored::put(
        &id,
        Stored {
            owner: principal.agent.id.clone(),
            obj: obj.clone(),
            input: Vec::new(),
            history: vec![json!({"secret": secret})],
        },
    );
    let mut shown = obj;
    shown["signing_secret"] = json!(secret);
    Json(shown).into_response()
}

#[derive(Debug, serde::Deserialize, Default)]
pub struct ListQuery {
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    after: Option<String>,
}

pub async fn list(principal: AuthPrincipal, Query(q): Query<ListQuery>) -> Response {
    let mut v: Vec<Value> = stored::list(&principal.agent.id, "webhook_endpoint")
        .into_iter()
        .map(|(_, s)| s.obj)
        .collect();
    v.reverse();
    if let Some(a) = &q.after {
        if let Some(i) = v.iter().position(|x| x["id"] == json!(a)) {
            v.drain(..=i);
        }
    }
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let has_more = v.len() > limit;
    v.truncate(limit);
    let id_of = |x: Option<&Value>| x.map(|v| v["id"].clone()).unwrap_or(Value::Null);
    Json(json!({"object": "list", "data": v, "first_id": id_of(v.first()), "last_id": id_of(v.last()), "has_more": has_more}))
        .into_response()
}

pub async fn get(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    match endpoint(&principal.agent.id, &id) {
        Some(s) => Json(s.obj).into_response(),
        None => stored::not_found("webhook endpoint", &id),
    }
}

pub async fn update(
    principal: AuthPrincipal,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> Response {
    let Some(s) = endpoint(&principal.agent.id, &id) else {
        return stored::not_found("webhook endpoint", &id);
    };
    let mut o = s.obj;
    if b.get("event_types").is_some() {
        match types_of(&b) {
            Ok(t) => o["event_types"] = json!(t),
            Err(r) => return r,
        }
    }
    if let Some(n) = b["name"].as_str() {
        o["name"] = json!(n);
    }
    if let Some(u) = b.get("url") {
        match u.as_str().filter(|u| valid_url(u)) {
            Some(u) => o["url"] = json!(u),
            None => return err("url must be an http(s) URL"),
        }
    }
    o["updated_at"] = json!(now());
    stored::update(&id, |s| s.obj = o.clone());
    Json(o).into_response()
}

pub async fn delete(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    if endpoint(&principal.agent.id, &id).is_none() {
        return stored::not_found("webhook endpoint", &id);
    }
    stored::remove(&principal.agent.id, &id);
    Json(json!({"id": id, "object": "webhook_endpoint.deleted", "deleted": true})).into_response()
}

pub async fn rotate_secret(
    principal: AuthPrincipal,
    Path(id): Path<String>,
    body: Option<Json<Value>>,
) -> Response {
    let Some(s) = endpoint(&principal.agent.id, &id) else {
        return stored::not_found("webhook endpoint", &id);
    };
    let keep = body
        .and_then(|Json(b)| b["keep_old_secret_active_for_24_hours"].as_bool())
        .unwrap_or(false);
    let secret = new_secret();
    let mut o = s.obj;
    o["signing_secret_hint"] = json!(hint(&secret));
    o["updated_at"] = json!(now());
    stored::update(&id, |s| {
        let old: Vec<Value> = if keep {
            s.history
                .iter()
                .map(|h| {
                    let mut h = h.clone();
                    if h["until"].is_null() {
                        h["until"] = json!(now() + 86_400);
                    }
                    h
                })
                .collect()
        } else {
            Vec::new()
        };
        s.history = std::iter::once(json!({"secret": secret}))
            .chain(old)
            .collect();
        s.obj = o.clone();
    });
    let mut shown = o;
    shown["signing_secret"] = json!(secret);
    Json(shown).into_response()
}

pub async fn event_types() -> Response {
    Json(json!({"object": "list", "data": EVENT_TYPES})).into_response()
}

/// Post one event to one endpoint; the HTTP status it answered.
async fn deliver(url: &str, secrets: &[String], ev: &Value) -> Result<u16, String> {
    let body = ev.to_string();
    let id = format!("wh_{}", uuid::Uuid::now_v7().simple());
    let ts = now();
    let sig = secrets
        .iter()
        .map(|s| sign(s, &id, ts, &body))
        .collect::<Vec<_>>()
        .join(" ");
    let r = reqwest::Client::new()
        .post(url)
        .header("content-type", "application/json")
        .header("webhook-id", &id)
        .header("webhook-timestamp", ts.to_string())
        .header("webhook-signature", sig)
        .body(body)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    Ok(r.status().as_u16())
}

fn event(kind: &str, data_id: &str) -> Value {
    json!({"id": format!("evt_{}", uuid::Uuid::now_v7().simple()), "object": "event",
           "created_at": now(), "type": kind, "data": {"id": data_id}})
}

pub async fn test(
    principal: AuthPrincipal,
    Path(id): Path<String>,
    body: Option<Json<Value>>,
) -> Response {
    let Some(s) = endpoint(&principal.agent.id, &id) else {
        return stored::not_found("webhook endpoint", &id);
    };
    let kind = body
        .and_then(|Json(b)| b["event_type"].as_str().map(String::from))
        .or_else(|| s.obj["event_types"][0].as_str().map(String::from))
        .unwrap_or_else(|| "response.completed".into());
    if !EVENT_TYPES.contains(&kind.as_str()) {
        return err(&format!("unknown event type {kind}"));
    }
    let url = s.obj["url"].as_str().unwrap_or("").to_string();
    match deliver(&url, &secret_of(&s), &event(&kind, "test")).await {
        Ok(code) if (200..300).contains(&code) => Json(json!({"object": "webhook_endpoint.test",
            "event_type": kind, "status_code": code, "success": true, "webhook_endpoint_id": id}))
        .into_response(),
        Ok(code) => err(&format!("the endpoint answered {code}")),
        Err(e) => err(&format!("the endpoint could not be reached: {e}")),
    }
}

/// The event a status change of a stored object is, if any.
pub fn transition(before: &Value, after: &Value) -> Option<&'static str> {
    let (a, b) = (after["status"].as_str()?, before["status"].as_str());
    if b == Some(a) {
        return None;
    }
    Some(match (after["object"].as_str()?, a) {
        ("response", s) if after["background"] == true => match s {
            "completed" => "response.completed",
            "failed" => "response.failed",
            "cancelled" => "response.cancelled",
            "incomplete" => "response.incomplete",
            _ => return None,
        },
        ("batch", "completed") => "batch.completed",
        ("batch", "failed") => "batch.failed",
        ("batch", "expired") => "batch.expired",
        ("batch", "cancelled") => "batch.cancelled",
        ("eval.run", "completed") => "eval.run.succeeded",
        ("eval.run", "failed") => "eval.run.failed",
        ("eval.run", "canceled") => "eval.run.canceled",
        ("fine_tuning.job", "succeeded") => "fine_tuning.job.succeeded",
        ("fine_tuning.job", "failed") => "fine_tuning.job.failed",
        ("fine_tuning.job", "cancelled") => "fine_tuning.job.cancelled",
        ("video", "completed") => "video.completed",
        ("video", "failed") => "video.failed",
        ("agent.session", "in_progress") => "agent.session.in_progress",
        ("agent.session", "idle") => "agent.session.idle",
        ("agent.session", "requires_action") => "agent.session.action_required",
        ("agent.session", "failed") => "agent.session.failed",
        _ => return None,
    })
}

/// Tell the owner's endpoints that subscribe to `kind` about `data_id`.
pub fn fire(owner: &str, kind: &str, data_id: &str) {
    let targets: Vec<(String, Vec<String>)> = stored::list(owner, "webhook_endpoint")
        .into_iter()
        .filter(|(_, s)| {
            s.obj["event_types"]
                .as_array()
                .is_some_and(|a| a.iter().any(|t| t == kind))
        })
        .map(|(_, s)| {
            (
                s.obj["url"].as_str().unwrap_or("").to_string(),
                secret_of(&s),
            )
        })
        .collect();
    if targets.is_empty() {
        return;
    }
    let ev = event(kind, data_id);
    for (url, secrets) in targets {
        let ev = ev.clone();
        tokio::spawn(async move {
            // A few tries with backoff, as a webhook sender does.
            for wait in [0u64, 5, 30, 120] {
                tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                if matches!(deliver(&url, &secrets, &ev).await, Ok(c) if (200..300).contains(&c)) {
                    return;
                }
            }
            tracing::warn!(url = %url, "webhook delivery failed after retries");
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc4231() {
        // RFC 4231 test case 2.
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex::encode(mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn transitions() {
        let r = |s: &str| json!({"object": "response", "background": true, "status": s});
        assert_eq!(
            transition(&r("in_progress"), &r("completed")),
            Some("response.completed")
        );
        assert_eq!(transition(&r("completed"), &r("completed")), None);
        let fg = json!({"object": "response", "status": "completed"});
        assert_eq!(transition(&r("in_progress"), &fg), None);
        let e = |s: &str| json!({"object": "eval.run", "status": s});
        assert_eq!(
            transition(&e("in_progress"), &e("completed")),
            Some("eval.run.succeeded")
        );
    }

    #[test]
    fn signature_shape() {
        let s = sign("whsec_c2VjcmV0", "wh_1", 1, "{}");
        assert!(s.starts_with("v1,"));
        assert_eq!(s, sign("secret", "wh_1", 1, "{}"));
    }
}
