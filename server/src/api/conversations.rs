//! `/v1/conversations`: the OpenAI Conversations API (items a Responses
//! turn continues from and appends to), sharing its path with this
//! server's own history routes. They never collide: OpenAI conversation
//! ids start `conv_`, ours `cnv_`, so an id says which one is meant;
//! creating is OpenAI's, listing is ours (OpenAI has no list).

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::api::stored::{self, Stored};
use crate::auth::AuthPrincipal;
use crate::storage::AppState;

fn is_openai(id: &str) -> bool {
    id.starts_with("conv_")
}

/// The key may use this side of the path: OpenAI conversations need a
/// caller key, our history a responder key.
fn scope(principal: &AuthPrincipal, openai: bool) -> Option<Response> {
    let ok = if openai {
        principal.api_key.scope.may_call()
    } else {
        principal.api_key.scope.may_respond()
    };
    (!ok).then(|| {
        (
            StatusCode::FORBIDDEN,
            Json(json!({"error": {"message": format!(
                "This key's scope is '{}' and cannot use {} conversations.",
                principal.api_key.scope.as_str(),
                if openai { "OpenAI (conv_)" } else { "history (cnv_)" }
            ), "type": "invalid_request_error", "code": "wrong_key_scope"}})),
        )
            .into_response()
    })
}

fn invalid(msg: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error": {"message": msg, "type": "invalid_request_error", "code": null, "param": null}})),
    )
        .into_response()
}

/// Items as stored: each with a type and an id.
fn normalize(items: &[Value]) -> Vec<Value> {
    stored::with_ids(
        stored::input_items(Some(&Value::Array(items.to_vec()))),
        "msg",
    )
}

/// The items of a caller's conversation, when it exists.
pub fn items(owner: &str, id: &str) -> Option<Vec<Value>> {
    stored::get(owner, id)
        .filter(|s| s.obj["object"] == "conversation")
        .map(|s| s.input)
}

/// Append a Responses turn (its input and output) to a conversation.
pub fn append(id: &str, new: &[Value]) {
    let new = normalize(new);
    stored::update(id, |s| s.input.extend(new));
}

/// `POST /v1/conversations`
pub async fn create(principal: AuthPrincipal, body: Option<Json<Value>>) -> Response {
    if let Some(r) = scope(&principal, true) {
        return r;
    }
    let b = body.map(|Json(b)| b).unwrap_or(json!({}));
    let initial = b
        .get("items")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if initial.len() > 20 {
        return invalid("at most 20 items when creating a conversation");
    }
    let id = format!("conv_{}", uuid::Uuid::now_v7().simple());
    let obj = json!({
        "id": id,
        "object": "conversation",
        "created_at": chrono::Utc::now().timestamp(),
        "metadata": b.get("metadata").cloned().unwrap_or(json!({})),
    });
    stored::put(
        &id,
        Stored {
            owner: principal.agent.id.clone(),
            obj: obj.clone(),
            input: normalize(&initial),
            history: Vec::new(),
        },
    );
    Json(obj).into_response()
}

/// `GET /v1/conversations`: this server's history list.
pub async fn list(
    state: State<AppState>,
    principal: AuthPrincipal,
    q: Query<crate::api::history::ConversationsQuery>,
) -> Response {
    if let Some(r) = scope(&principal, false) {
        return r;
    }
    crate::api::history::list_conversations(state, axum::Extension(principal), q)
        .await
        .into_response()
}

/// `GET /v1/conversations/:id`
pub async fn retrieve(principal: AuthPrincipal, Path(id): Path<String>) -> Response {
    if let Some(r) = scope(&principal, true) {
        return r;
    }
    match stored::get(&principal.agent.id, &id).filter(|s| s.obj["object"] == "conversation") {
        Some(s) => Json(s.obj).into_response(),
        None => stored::not_found("conversation", &id),
    }
}

/// `POST /v1/conversations/:id`: new metadata.
pub async fn update(
    principal: AuthPrincipal,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> Response {
    if let Some(r) = scope(&principal, true) {
        return r;
    }
    if items(&principal.agent.id, &id).is_none() {
        return stored::not_found("conversation", &id);
    }
    let md = b.get("metadata").cloned().unwrap_or(json!({}));
    stored::update(&id, |s| s.obj["metadata"] = md);
    retrieve(principal, Path(id)).await
}

/// `DELETE /v1/conversations/:id`: an OpenAI conversation is deleted; one of
/// ours goes to the trash as before.
pub async fn delete(
    state: State<AppState>,
    principal: AuthPrincipal,
    Path(id): Path<String>,
) -> Response {
    if let Some(r) = scope(&principal, is_openai(&id)) {
        return r;
    }
    if !is_openai(&id) {
        return crate::api::history::delete_conversation(
            state,
            Path(id),
            axum::Extension(principal),
        )
        .await
        .into_response();
    }
    if items(&principal.agent.id, &id).is_some() && stored::remove(&principal.agent.id, &id) {
        Json(json!({"id": id, "object": "conversation.deleted", "deleted": true})).into_response()
    } else {
        stored::not_found("conversation", &id)
    }
}

/// `POST /v1/conversations/:id/items`
pub async fn add_items(
    principal: AuthPrincipal,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> Response {
    if let Some(r) = scope(&principal, true) {
        return r;
    }
    if items(&principal.agent.id, &id).is_none() {
        return stored::not_found("conversation", &id);
    }
    let new = b
        .get("items")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if new.is_empty() || new.len() > 20 {
        return invalid("items: 1 to 20 items");
    }
    let new = normalize(&new);
    stored::update(&id, |s| s.input.extend(new.clone()));
    let id_of = |x: Option<&Value>| x.and_then(|v| v.get("id")).cloned().unwrap_or(Value::Null);
    Json(json!({
        "object": "list",
        "data": new,
        "first_id": id_of(new.first()),
        "last_id": id_of(new.last()),
        "has_more": false,
    }))
    .into_response()
}

#[derive(Debug, serde::Deserialize, Default)]
pub struct ItemsQuery {
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    order: Option<String>,
    #[serde(default)]
    after: Option<String>,
}

/// `GET /v1/conversations/:id/items` (newest first by default).
pub async fn list_items(
    principal: AuthPrincipal,
    Path(id): Path<String>,
    Query(q): Query<ItemsQuery>,
) -> Response {
    if let Some(r) = scope(&principal, true) {
        return r;
    }
    let Some(mut list) = items(&principal.agent.id, &id) else {
        return stored::not_found("conversation", &id);
    };
    if q.order.as_deref().unwrap_or("desc") == "desc" {
        list.reverse();
    }
    if let Some(a) = &q.after {
        if let Some(i) = list.iter().position(|x| x["id"] == json!(a)) {
            list.drain(..=i);
        }
    }
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let has_more = list.len() > limit;
    list.truncate(limit);
    let id_of = |x: Option<&Value>| x.and_then(|v| v.get("id")).cloned().unwrap_or(Value::Null);
    Json(json!({
        "object": "list",
        "first_id": id_of(list.first()),
        "last_id": id_of(list.last()),
        "has_more": has_more,
        "data": list,
    }))
    .into_response()
}

/// `GET /v1/conversations/:id/items/:item_id`
pub async fn get_item(
    principal: AuthPrincipal,
    Path((id, item)): Path<(String, String)>,
) -> Response {
    if let Some(r) = scope(&principal, true) {
        return r;
    }
    match items(&principal.agent.id, &id)
        .and_then(|l| l.into_iter().find(|x| x["id"] == json!(item)))
    {
        Some(x) => Json(x).into_response(),
        None => stored::not_found("item", &item),
    }
}

/// `DELETE /v1/conversations/:id/items/:item_id`: returns the conversation.
pub async fn delete_item(
    principal: AuthPrincipal,
    Path((id, item)): Path<(String, String)>,
) -> Response {
    if let Some(r) = scope(&principal, true) {
        return r;
    }
    let Some(list) = items(&principal.agent.id, &id) else {
        return stored::not_found("conversation", &id);
    };
    if !list.iter().any(|x| x["id"] == json!(item)) {
        return stored::not_found("item", &item);
    }
    stored::update(&id, |s| s.input.retain(|x| x["id"] != json!(item)));
    retrieve(principal, Path(id)).await
}
