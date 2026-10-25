//! Authentication middleware — Bearer API Key → AuthPrincipal.
//!
//! Flow:
//!   Authorization: Bearer <api_key>
//!     → validate key via IdentityRepository
//!     → resolve AgentIdentity + bound HumanIdentity
//!     → inject AuthPrincipal into request extension

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Json, Response};
use serde_json::json;

use crate::storage::AppState;

/// Re-export so handlers can use `crate::auth::AuthPrincipal`.
pub use crate::state::AuthPrincipal;

/// Auth middleware — checks `Authorization: Bearer <key>`.
///
/// Called via `from_fn_with_state(state, require_api_key)`.
pub async fn require_api_key(
    state: axum::extract::State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok());

    // Anthropic clients (Claude Code with ANTHROPIC_API_KEY) send the key
    // as `x-api-key` instead of a bearer token.
    let x_api_key = request
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let bearer = auth_header
        .filter(|h| h.starts_with("Bearer "))
        .map(|h| h[7..].trim().to_string());
    let raw_key_owned = bearer.or(x_api_key);
    let raw_key = match raw_key_owned.as_deref() {
        Some(k) => k,
        _ => {
            return auth_error(
                StatusCode::UNAUTHORIZED,
                "missing_api_key",
                "Missing or invalid Authorization header. Expected: Bearer <key>",
            )
        }
    };

    let principal = state.identity.resolve_principal(raw_key).await;

    let principal = match principal {
        Some(p) => p,
        None => {
            return auth_error(
                StatusCode::UNAUTHORIZED,
                "invalid_api_key",
                "The provided API key is not recognized.",
            )
        }
    };

    // Which side this route belongs to. The OpenAI-compatible ingress is
    // the caller's; everything else under the protected router (history,
    // trash, renames, the agent seat) is the responder's.
    let raw_path = request.uri().path();
    let path = raw_path
        .strip_prefix("/v1")
        .filter(|p| p.starts_with("/v1/"))
        .unwrap_or(raw_path);
    let caller_route = path == "/v1/chat/completions"
        || path == "/v1/models"
        || path.starts_with("/v1/models/")
        || path == "/v1/tokenizers"
        || path == "/v1/responses"
        || path.starts_with("/v1/messages")
        || path == "/v1/files"
        || path.starts_with("/v1/files/")
        || path.starts_with("/v1/replies");
    let allowed = if caller_route {
        principal.api_key.scope.may_call()
    } else {
        principal.api_key.scope.may_respond()
    };
    if !allowed {
        return auth_error(
            StatusCode::FORBIDDEN,
            "wrong_key_scope",
            &format!(
                "This key's scope is '{}' and cannot be used on {}.",
                principal.api_key.scope.as_str(),
                path
            ),
        );
    }

    request.extensions_mut().insert(principal);
    next.run(request).await
}

/// Produce an OpenAI-formatted error response.
fn auth_error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({
            "error": {
                "message": message,
                "type": "invalid_request_error",
                "code": code
            }
        })),
    )
        .into_response()
}

/// Axum extractor — pulls `AuthPrincipal` from request extensions.
#[axum::async_trait]
impl<S> axum::extract::FromRequestParts<S> for AuthPrincipal
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, Json<serde_json::Value>);

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        parts.extensions.get::<AuthPrincipal>().cloned().ok_or((
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "error": {
                    "message": "Authentication required.",
                    "type": "invalid_request_error",
                    "code": "no_principal"
                }
            })),
        ))
    }
}
