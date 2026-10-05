//! `ApiError` → HTTP mapping: one error vocabulary, per-frontend rendering
//! (arc42 section 8). Provider-flavored failures are gateway errors (502),
//! our own config/lifecycle problems are 4xx/500.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use agent_core::{ApiError, ApiErrorKind};

pub fn json_error(status: StatusCode, kind: &str, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({ "error": { "kind": kind, "message": message.into() } })),
    )
        .into_response()
}

pub fn api_error(err: &ApiError) -> Response {
    let status = match err.kind {
        ApiErrorKind::Busy => StatusCode::CONFLICT,
        ApiErrorKind::Config => StatusCode::BAD_REQUEST,
        ApiErrorKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        ApiErrorKind::Unauthorized => StatusCode::BAD_GATEWAY,
        ApiErrorKind::RateLimited => StatusCode::BAD_GATEWAY,
        ApiErrorKind::NotFound => StatusCode::BAD_GATEWAY,
        ApiErrorKind::Forbidden => StatusCode::FORBIDDEN,
        ApiErrorKind::Network => StatusCode::BAD_GATEWAY,
        ApiErrorKind::Protocol => StatusCode::BAD_GATEWAY,
        ApiErrorKind::Provider => StatusCode::BAD_GATEWAY,
        ApiErrorKind::Aborted => StatusCode::INTERNAL_SERVER_ERROR,
    };
    json_error(status, err.kind.as_str(), err.message.clone())
}
