//! Omni2API Server — library crate.
//!
//! All modules are declared here so integration tests can access
//! `omni2api_server::build_router` etc.

pub mod api;
pub mod async_reply;
pub mod auth;
pub mod crypto;
pub mod model;
pub mod normalize;
pub mod notification;
pub mod observation;
pub mod openai_compat;
pub mod profile;
pub mod quick;
pub mod seat;
pub mod session;
pub mod state;
pub mod storage;
pub mod ws;

use axum::middleware::from_fn_with_state;
use axum::routing::{delete, get, patch, post};
use axum::Router;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;

use crate::storage::AppState;

/// Maximum inbound request body, in bytes.
///
/// Agent requests routinely reach hundreds of KB (system prompts + tool
/// schemas + replayed history), so the cap is generous — its job is to
/// stop absurd payloads from pinning memory, not to police normal traffic.
/// Overridable via `OMNI2API_MAX_BODY_BYTES`.
fn max_body_bytes() -> usize {
    std::env::var("OMNI2API_MAX_BODY_BYTES")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(8 * 1024 * 1024)
}

/// Maximum concurrent in-flight requests.
///
/// Each open turn pins a connection for as long as the human takes to
/// answer; an unbounded server lets one caller starve the rest.
/// Overridable via `OMNI2API_MAX_CONCURRENCY`.
fn max_concurrency() -> usize {
    std::env::var("OMNI2API_MAX_CONCURRENCY")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(256)
}

/// Build the Axum router with all routes.
///
/// Routes:
///   - `GET  /health`, `GET /healthz`    — health check
///   - `GET  /ws`                        — WebSocket Human Gateway
///   - `POST /v1/chat/completions`       — OpenAI-compatible API (auth + observation)
///   - `GET  /v1/replies/:invocation_id`— async reply polling (auth)
///   - `POST /v1/mock/human-reply`       — Mock human reply (dev/test)
///   - `POST /v1/mock/end-turn`          — Close an open streaming turn (dev/test)
///
/// The `/v1/chat/completions` route has an observation layer that
/// records the raw request to `observations/` for analysis.
///
/// Layer order (outer → inner):
///   1. TraceLayer (HTTP tracing)
///   2. Observation layer (raw request capture)
///   3. Auth layer (API key validation)
///   4. Handler (chat_completions)
pub fn build_router(state: AppState) -> Router {
    let protected = Router::new()
        .route("/v1/chat/completions", post(api::openai::chat_completions))
        // Same pipeline, other wires: Anthropic Messages (Claude Code) and
        // OpenAI Responses (Codex CLI).
        .route("/v1/messages", post(api::adapters::messages))
        .route(
            "/v1/messages/count_tokens",
            post(api::adapters::count_tokens),
        )
        .route("/v1/responses", post(api::adapters::responses))
        .route(
            "/v1/files",
            post(api::files::upload)
                .get(api::files::list)
                .layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route(
            "/v1/files/:file_id",
            get(api::files::get_meta).delete(api::files::delete),
        )
        .route("/v1/files/:file_id/content", get(api::files::content))
        .route("/v1/models", get(list_models))
        .route("/v1/models/:model_id", get(get_model))
        // A base URL configured as `.../v1` by a client that appends `/v1`
        // itself (Copilot's Anthropic provider, several SDKs) lands here.
        .route(
            "/v1/v1/chat/completions",
            post(api::openai::chat_completions),
        )
        .route("/v1/v1/messages", post(api::adapters::messages))
        .route(
            "/v1/v1/messages/count_tokens",
            post(api::adapters::count_tokens),
        )
        .route("/v1/v1/responses", post(api::adapters::responses))
        .route("/v1/v1/models", get(list_models))
        .route("/v1/v1/models/:model_id", get(get_model))
        .route("/v1/tokenizers", get(list_tokenizers))
        // Companion to async mode: poll for a human's reply without
        // holding the original request open.
        // axum 0.7 path-parameter syntax is `:name` — `{name}` would be
        // registered as a literal segment and match nothing.
        .route("/v1/replies/:invocation_id", get(api::replies::get_reply))
        // History. The backlog on connect deliberately carries only
        // undelivered messages, so a client that wants the past has to ask
        // for it — replaying everything on every reconnect meant the human
        // watched conversations they had already answered arrive again.
        .route("/v1/conversations", get(api::history::list_conversations))
        .route(
            "/v1/conversations/:conversation_id/messages",
            get(api::history::list_messages),
        )
        // Trash: delete moves a conversation aside, restore brings it back,
        // and only the permanent route actually destroys anything.
        .route(
            "/v1/conversations/:conversation_id",
            delete(api::history::delete_conversation),
        )
        // Keep a conversation: messages live in memory unless saved.
        .route(
            "/v1/conversations/:conversation_id/save",
            post(api::history::save_conversation),
        )
        .route(
            "/v1/conversations/:conversation_id/restore",
            post(api::history::restore_conversation),
        )
        .route(
            "/v1/conversations/:conversation_id/permanent",
            delete(api::history::delete_conversation_permanent),
        )
        // The seeded agent name is rarely what the human calls it.
        .route("/v1/agents/:agent_id", patch(api::history::rename_agent))
        // Agent Seat: the human seat over plain HTTP, for agents that
        // cannot hold a WebSocket (see `seat.rs`).
        .route("/v1/seat/inbox", get(seat::inbox))
        .route("/v1/seat/send", post(seat::send))
        .route("/v1/seat/reply", post(seat::reply))
        .route("/v1/seat/tool-call", post(seat::tool_call))
        .route("/v1/seat/capabilities", get(seat::capabilities))
        .route("/v1/seat/history", get(seat::history))
        .route("/v1/seat/save", post(seat::save))
        .route(
            "/v1/seat/model-name",
            post(seat::model_name).get(seat::model_name_get),
        )
        // Observation runs *inside* auth: only authenticated requests are
        // captured, so an anonymous caller cannot fill the disk.
        .layer(axum::middleware::from_fn(observation::observe_request))
        .layer(from_fn_with_state(state.clone(), auth::require_api_key));

    let public = Router::new()
        .route("/health", get(health))
        // `/healthz` is the Kubernetes convention and what the production
        // Dockerfile's HEALTHCHECK and docker-compose probe actually hit.
        // Without it the container is reported unhealthy forever even
        // though the server is serving traffic fine.
        .route("/healthz", get(health))
        .route("/ws", get(ws::ws_handler));

    // `/v1/mock/*` impersonates the human — forgeable replies, tool calls,
    // and turn closures for anyone who can reach them. The routes exist
    // only when explicitly enabled (cargo feature `mock_endpoints` or
    // `OMNI2API_MOCK_ENDPOINTS=1`); otherwise probes get a plain 404.
    let mock = if api::mock::mock_enabled() {
        tracing::warn!("Mock endpoints ENABLED — /v1/mock/* can impersonate the human");
        Router::new()
            .route("/v1/mock/human-reply", post(api::mock::human_reply))
            .route("/v1/mock/tool-call", post(api::mock::tool_call))
            .route("/v1/mock/end-turn", post(api::mock::end_turn))
    } else {
        Router::new()
    };

    Router::new()
        .merge(protected)
        .merge(public)
        .merge(mock)
        .with_state(state)
        // Backpressure before tracing: oversized bodies are rejected with
        // 413, and excess concurrency waits rather than piling up.
        .layer(RequestBodyLimitLayer::new(max_body_bytes()))
        .layer(tower::limit::ConcurrencyLimitLayer::new(max_concurrency()))
        .layer(TraceLayer::new_for_http())
}

/// Health check endpoint.
async fn health() -> &'static str {
    "ok"
}

/// `GET /v1/models` — the single "human" model, with its limits.
///
/// Many OpenAI-compatible clients call this on startup to verify
/// connectivity, and several (DeepSeek Harness, gateway importers) also
/// auto-fill a model's context window, output cap and abilities from it.
/// There is no common schema, so the same facts are given under the names
/// clients are known to read: `context_window` / `context_length`,
/// `max_output_tokens` / `max_tokens`, their camelCase forms, and a
/// `capabilities` object. The responder is a person or an agent, so the
/// limits are generous defaults, overridable with
/// `OMNI2API_MODEL_CONTEXT_WINDOW` and `OMNI2API_MODEL_MAX_OUTPUT`.
/// The model's name is the seat's to choose (see `storage::ModelName`).
fn model_object(app: &AppState) -> serde_json::Value {
    fn env_u64(name: &str, default: u64) -> u64 {
        std::env::var(name)
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(default)
    }
    let context = env_u64("OMNI2API_MODEL_CONTEXT_WINDOW", 1_000_000);
    let output = env_u64("OMNI2API_MODEL_MAX_OUTPUT", 131_072);
    let name = app.model.get();
    serde_json::json!({
        "id": name,
        "object": "model",
        // Anthropic's model object.
        "type": "model",
        "display_name": name,
        "created_at": "2023-11-14T22:13:20Z",
        "created": 1700000000,
        "owned_by": "omni2api",
        "name": name,
        "context_window": context,
        "context_length": context,
        "max_output_tokens": output,
        "max_tokens": output,
        "contextWindow": context,
        "maxTokens": output,
        "input_modalities": ["text", "image"],
        "input": ["text", "image"],
        "reasoning": false,
        "capabilities": {
            "vision": true,
            "tools": true,
            "function_calling": true,
            "reasoning": false,
            "streaming": true
        }
    })
}

async fn list_models(
    axum::extract::State(app): axum::extract::State<AppState>,
) -> axum::Json<serde_json::Value> {
    let name = app.model.get();
    axum::Json(serde_json::json!({
        "object": "list",
        "data": [model_object(&app)],
        "has_more": false,
        "first_id": name,
        "last_id": name,
    }))
}

/// `GET /v1/models/:id` — every model name reaches the same responder, so
/// whatever id a client checks for exists (it is reported back as asked).
async fn get_model(
    axum::extract::State(app): axum::extract::State<AppState>,
    axum::extract::Path(model_id): axum::extract::Path<String>,
) -> axum::Json<serde_json::Value> {
    let mut m = model_object(&app);
    m["id"] = serde_json::Value::String(model_id);
    axum::Json(m)
}

/// `GET /v1/tokenizers` — the tokenizers usage can be counted with. Only
/// ones that loaded are listed; pick one with `x-omni2api-tokenizer`.
async fn list_tokenizers() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "default": crate::session::default_tokenizer(),
        "tokenizers": crate::session::available_tokenizers(),
    }))
}
