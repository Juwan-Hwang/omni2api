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
/// Media rides here too (images to edit, a seat's generated pictures and
/// audio as base64), hence 64 MiB.
/// Overridable via `OMNI2API_MAX_BODY_BYTES`.
fn max_body_bytes() -> usize {
    std::env::var("OMNI2API_MAX_BODY_BYTES")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(64 * 1024 * 1024)
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
        .route(
            "/v1/chat/completions",
            post(api::stored::chat_create)
                .get(api::stored::list_chat)
                .layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route(
            "/v1/chat/completions/:id",
            get(api::stored::get_chat)
                .post(api::stored::update_chat)
                .delete(api::stored::delete_chat),
        )
        .route(
            "/v1/chat/completions/:id/messages",
            get(api::stored::chat_messages),
        )
        // Same pipeline, other wires: Anthropic Messages (Claude Code) and
        // OpenAI Responses (Codex CLI).
        .route(
            "/v1/messages",
            post(api::adapters::messages).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route(
            "/v1/messages/count_tokens",
            post(api::adapters::count_tokens),
        )
        .route(
            "/v1/responses",
            post(api::adapters::responses).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route(
            "/v1/responses/compact",
            post(api::adapters::responses_compact)
                .layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route(
            "/v1/responses/input_tokens",
            post(api::adapters::responses_input_tokens),
        )
        .route(
            "/v1/responses/:id",
            get(api::stored::get_response).delete(api::stored::delete_response),
        )
        .route(
            "/v1/responses/:id/cancel",
            post(api::stored::cancel_response),
        )
        .route(
            "/v1/responses/:id/input_items",
            get(api::stored::response_input_items),
        )
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
        .route("/v1/models/:model_id", get(get_model).delete(delete_model))
        // A base URL configured as `.../v1` by a client that appends `/v1`
        // itself (Copilot's Anthropic provider, several SDKs) lands here.
        .route(
            "/v1/v1/chat/completions",
            post(api::stored::chat_create).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route(
            "/v1/v1/messages",
            post(api::adapters::messages).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route(
            "/v1/v1/messages/count_tokens",
            post(api::adapters::count_tokens),
        )
        .route(
            "/v1/v1/responses",
            post(api::adapters::responses).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route("/v1/v1/models", get(list_models))
        .route("/v1/v1/models/:model_id", get(get_model))
        .route("/v1/tokenizers", get(list_tokenizers))
        // The other OpenAI endpoints: each request is a task the seat
        // answers (see `api/task.rs`).
        .route("/v1/images/generations", post(api::images::generations))
        .route("/v1/v1/images/generations", post(api::images::generations))
        .route(
            "/v1/images/edits",
            post(api::images::edits).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route(
            "/v1/v1/images/edits",
            post(api::images::edits).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route(
            "/v1/embeddings",
            post(api::embeddings::embeddings).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route(
            "/v1/v1/embeddings",
            post(api::embeddings::embeddings).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route(
            "/v1/vector_stores",
            post(api::vector_stores::create).get(api::vector_stores::list),
        )
        .route(
            "/v1/vector_stores/:id",
            get(api::vector_stores::retrieve)
                .post(api::vector_stores::modify)
                .delete(api::vector_stores::delete),
        )
        .route(
            "/v1/vector_stores/:id/search",
            post(api::vector_stores::search),
        )
        .route(
            "/v1/vector_stores/:id/files",
            post(api::vector_stores::add_file).get(api::vector_stores::list_files),
        )
        .route(
            "/v1/vector_stores/:id/files/:file_id",
            get(api::vector_stores::get_file)
                .post(api::vector_stores::update_file)
                .delete(api::vector_stores::delete_file),
        )
        .route(
            "/v1/vector_stores/:id/files/:file_id/content",
            get(api::vector_stores::file_content),
        )
        .route(
            "/v1/vector_stores/:id/file_batches",
            post(api::vector_stores::create_batch),
        )
        .route(
            "/v1/vector_stores/:id/file_batches/:batch_id",
            get(api::vector_stores::get_batch_route),
        )
        .route(
            "/v1/vector_stores/:id/file_batches/:batch_id/cancel",
            post(api::vector_stores::cancel_batch),
        )
        .route(
            "/v1/vector_stores/:id/file_batches/:batch_id/files",
            get(api::vector_stores::batch_files),
        )
        .route(
            "/v1/videos",
            post(api::videos::create)
                .get(api::videos::list)
                .layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route(
            "/v1/videos/:id",
            get(api::videos::retrieve).delete(api::videos::delete),
        )
        .route("/v1/videos/:id/content", get(api::videos::content))
        .route("/v1/videos/:id/remix", post(api::videos::remix))
        .route(
            "/v1/realtime/client_secrets",
            post(api::realtime::client_secrets),
        )
        .route("/v1/realtime/sessions", post(api::realtime::client_secrets))
        .route(
            "/v1/containers",
            post(api::containers::create).get(api::containers::list),
        )
        .route(
            "/v1/containers/:id",
            get(api::containers::retrieve).delete(api::containers::delete),
        )
        .route(
            "/v1/containers/:id/files",
            post(api::containers::add_file)
                .get(api::containers::list_files)
                .layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route(
            "/v1/containers/:id/files/:file_id",
            get(api::containers::get_file).delete(api::containers::delete_file),
        )
        .route(
            "/v1/containers/:id/files/:file_id/content",
            get(api::containers::file_content),
        )
        .route(
            "/v1/fine_tuning/jobs",
            post(api::evals::create_job).get(api::evals::list_jobs),
        )
        .route("/v1/fine_tuning/jobs/:id", get(api::evals::get_job_route))
        .route(
            "/v1/fine_tuning/jobs/:id/events",
            get(api::evals::job_events),
        )
        .route(
            "/v1/fine_tuning/jobs/:id/checkpoints",
            get(api::evals::job_checkpoints),
        )
        .route(
            "/v1/fine_tuning/jobs/:id/:action",
            post(api::evals::job_action),
        )
        .route(
            "/v1/fine_tuning/alpha/graders/run",
            post(api::evals::graders_run),
        )
        .route(
            "/v1/fine_tuning/alpha/graders/validate",
            post(api::evals::graders_validate),
        )
        .route(
            "/v1/evals",
            post(api::evals::create_eval).get(api::evals::list_evals),
        )
        .route(
            "/v1/evals/:id",
            get(api::evals::get_eval_route)
                .post(api::evals::update_eval)
                .delete(api::evals::delete_eval),
        )
        .route(
            "/v1/evals/:id/runs",
            post(api::evals::create_run).get(api::evals::list_runs),
        )
        .route(
            "/v1/evals/:id/runs/:run_id",
            get(api::evals::get_run_route)
                .post(api::evals::cancel_run)
                .delete(api::evals::delete_run),
        )
        .route(
            "/v1/evals/:id/runs/:run_id/cancel",
            post(api::evals::cancel_run),
        )
        .route(
            "/v1/evals/:id/runs/:run_id/output_items",
            get(api::evals::list_output_items),
        )
        .route(
            "/v1/evals/:id/runs/:run_id/output_items/:item_id",
            get(api::evals::get_output_item),
        )
        .route("/v1/uploads", post(api::uploads::create))
        .route(
            "/v1/uploads/:id/parts",
            post(api::uploads::add_part).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route("/v1/uploads/:id/complete", post(api::uploads::complete))
        .route("/v1/uploads/:id/cancel", post(api::uploads::cancel))
        .route(
            "/v1/batches",
            post(api::batches::create).get(api::batches::list),
        )
        .route("/v1/batches/:id", get(api::batches::retrieve))
        .route("/v1/batches/:id/cancel", post(api::batches::cancel))
        .route("/v1/completions", post(api::completions::completions))
        .route("/v1/v1/completions", post(api::completions::completions))
        .route(
            "/v1/moderations",
            post(api::moderations::moderations).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route(
            "/v1/v1/moderations",
            post(api::moderations::moderations).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route("/v1/audio/speech", post(api::audio::speech))
        .route("/v1/v1/audio/speech", post(api::audio::speech))
        .route(
            "/v1/audio/transcriptions",
            post(api::audio::transcriptions).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route(
            "/v1/v1/audio/transcriptions",
            post(api::audio::transcriptions).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route(
            "/v1/audio/translations",
            post(api::audio::translations).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route(
            "/v1/v1/audio/translations",
            post(api::audio::translations).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route(
            "/v1/images/variations",
            post(api::images::variations).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route(
            "/v1/v1/images/variations",
            post(api::images::variations).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        // Companion to async mode: poll for a human's reply without
        // holding the original request open.
        // axum 0.7 path-parameter syntax is `:name` — `{name}` would be
        // registered as a literal segment and match nothing.
        .route("/v1/replies/:invocation_id", get(api::replies::get_reply))
        // History. The backlog on connect deliberately carries only
        // undelivered messages, so a client that wants the past has to ask
        // for it — replaying everything on every reconnect meant the human
        // watched conversations they had already answered arrive again.
        // Shared with the OpenAI Conversations API: `conv_` ids are
        // OpenAI's, `cnv_` ours (see `api/conversations.rs`).
        .route(
            "/v1/conversations",
            get(api::conversations::list).post(api::conversations::create),
        )
        .route(
            "/v1/conversations/:conversation_id/items",
            get(api::conversations::list_items).post(api::conversations::add_items),
        )
        .route(
            "/v1/conversations/:conversation_id/items/:item_id",
            get(api::conversations::get_item).delete(api::conversations::delete_item),
        )
        .route(
            "/v1/conversations/:conversation_id/messages",
            get(api::history::list_messages),
        )
        // Trash: delete moves a conversation aside, restore brings it back,
        // and only the permanent route actually destroys anything.
        .route(
            "/v1/conversations/:conversation_id",
            delete(api::conversations::delete)
                .get(api::conversations::retrieve)
                .post(api::conversations::update),
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
        // Attachments (a generated picture, audio) are base64 in the body.
        .route(
            "/v1/seat/reply",
            post(seat::reply).layer(axum::extract::DefaultBodyLimit::disable()),
        )
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
        // Seat-made outputs (pictures, audio) handed back as links.
        .route("/files/:file_id", get(api::files::public))
        // Realtime authenticates itself: browsers send the key as a
        // WebSocket subprotocol, which no header middleware sees.
        .route("/v1/realtime", get(api::realtime::realtime))
        .route("/v1/v1/realtime", get(api::realtime::realtime))
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
    principal: crate::auth::AuthPrincipal,
) -> axum::Json<serde_json::Value> {
    let name = app.model.get();
    // The instance's own name, then this key's fine-tuned models.
    let mut data = vec![model_object(&app)];
    data.extend(api::evals::fine_tuned_models(&principal.agent.id));
    let last = data
        .last()
        .and_then(|m| m.get("id"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    axum::Json(serde_json::json!({
        "object": "list",
        "data": data,
        "has_more": false,
        "first_id": name,
        "last_id": last,
    }))
}

/// `DELETE /v1/models/:id`: a fine-tuned model goes with its job's name.
async fn delete_model(
    principal: crate::auth::AuthPrincipal,
    axum::extract::Path(model_id): axum::extract::Path<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let jobs = api::stored::list(&principal.agent.id, "fine_tuning.job");
    match jobs
        .into_iter()
        .find(|(_, s)| s.obj["fine_tuned_model"] == serde_json::json!(model_id))
    {
        Some((id, _)) => {
            api::stored::update(&id, |s| s.obj["fine_tuned_model"] = serde_json::Value::Null);
            axum::Json(serde_json::json!({"id": model_id, "object": "model", "deleted": true}))
                .into_response()
        }
        None => api::stored::not_found("fine-tuned model", &model_id),
    }
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
