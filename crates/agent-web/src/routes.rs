//! HTTP surface: `/api/models`, `/api/chat` (SSE), `/api/abort`, `/api/session`,
//! plus embedded static assets. Static assets are public and cacheable;
//! every `/api/*` route goes through the `X-Auth-Token` check when a token is
//! configured, and API responses are never cacheable.

use std::sync::Arc;

use agent_core::{AgentCore, ChatSession};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::{assets, bridge, error};

#[derive(Clone)]
pub struct AppState {
    pub core: Arc<AgentCore>,
    pub session: Arc<ChatSession>,
}

impl AppState {
    pub fn new(core: Arc<AgentCore>, session: Arc<ChatSession>) -> Self {
        Self { core, session }
    }

    /// Test helper: a state with the builtin fake provider.
    #[cfg(test)]
    pub fn fake() -> Self {
        let core = Arc::new(AgentCore::with_mode(
            agent_core::Config::default(),
            Arc::new(agent_core::FakeProvider::builtin()),
            agent_core::ClientMode::Fake {
                cassette: std::path::PathBuf::new(),
            },
        ));
        let session = Arc::new(ChatSession::new(Arc::clone(&core), "test"));
        Self { core, session }
    }
}

pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/session", get(get_session))
        .route("/models", get(get_models))
        .route("/chat", post(post_chat))
        .route("/abort", post(post_abort))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));
    Router::new()
        .nest("/api", api)
        .fallback(assets::static_handler)
        .with_state(state)
}

/// Constant-time-ish comparison so the shared token is not trivially
/// guessable byte-by-byte through timing (single-user threat model: cheap
/// defense, zero dependencies).
fn tokens_equal(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

async fn auth_middleware(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if let Some(expected) = state.core.config().auth_token() {
        let provided = request
            .headers()
            .get("x-auth-token")
            .and_then(|value| value.to_str().ok());
        if !provided.is_some_and(|provided| tokens_equal(provided, expected)) {
            return error::json_error(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "missing or invalid X-Auth-Token header",
            );
        }
    }
    next.run(request).await
}

#[derive(Debug, Deserialize)]
struct ChatBody {
    message: String,
    /// Optional per-conversation model override (empty string clears it).
    #[serde(default)]
    model: Option<String>,
}

async fn post_chat(State(state): State<AppState>, Json(body): Json<ChatBody>) -> Response {
    if body.message.trim().is_empty() {
        return error::json_error(
            StatusCode::BAD_REQUEST,
            "config",
            "message must not be empty",
        );
    }
    if let Some(model) = body.model.as_deref() {
        let model = model.trim();
        state
            .session
            .set_model((!model.is_empty()).then(|| model.to_owned()));
    }
    match state.session.send(&body.message) {
        // Request-scoped M1 wiring (ADR-015): the SSE response owns the turn.
        Ok(handle) => bridge::sse_response(handle.into_events()),
        Err(err) => error::api_error(&err),
    }
}

async fn post_abort(State(state): State<AppState>) -> Response {
    match state.session.abort() {
        // Idempotent: aborting an idle session is a no-op, not an error.
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => error::api_error(&err),
    }
}

async fn get_models(State(state): State<AppState>) -> Response {
    match state.core.client().list_models().await {
        Ok(models) => Json(json!({ "models": models })).into_response(),
        Err(err) => error::api_error(&err),
    }
}

async fn get_session(State(state): State<AppState>) -> Response {
    Json(json!({
        "conversation": state.session.conversation_id(),
        "model": state.session.current_model(),
        "active": state.session.is_active(),
        "fake": state.core.is_fake(),
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{Config, FakeProvider};
    use axum::body::Body;
    use axum::http::header::{HeaderMap, HeaderName, HeaderValue};
    use http_body_util::BodyExt;
    use tower::util::ServiceExt;

    fn state_with_token(token: Option<&str>) -> AppState {
        let config = match token {
            Some(token) => Config::default().with_auth_token(token),
            None => Config::default(),
        };
        let core = Arc::new(AgentCore::new(config, Arc::new(FakeProvider::builtin())));
        let session = Arc::new(ChatSession::new(Arc::clone(&core), "test"));
        AppState { core, session }
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                HeaderName::try_from(*name).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    async fn request(
        state: &AppState,
        method: axum::http::Method,
        path: &str,
        body: Option<&serde_json::Value>,
        headers: HeaderMap,
    ) -> axum::response::Response {
        let app = router(state.clone());
        let mut builder = axum::http::Request::builder().method(method).uri(path);
        for (name, value) in headers.iter() {
            builder = builder.header(name, value);
        }
        let body = body.map(|b| serde_json::to_string(b).unwrap());
        let request = builder
            .header("content-type", "application/json")
            .body(Body::from(body.unwrap_or_default()))
            .unwrap();
        app.oneshot(request).await.unwrap()
    }

    async fn get_json(
        state: &AppState,
        path: &str,
        headers: HeaderMap,
    ) -> (StatusCode, serde_json::Value, HeaderMap) {
        let response = request(state, axum::http::Method::GET, path, None, headers).await;
        let (parts, body) = response.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes();
        let json: serde_json::Value = if bytes.is_empty() {
            json!(null)
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (parts.status, json, parts.headers)
    }

    #[tokio::test]
    async fn session_endpoint_reports_fake_state() {
        let state = AppState::fake();
        let (status, json, _) = get_json(&state, "/api/session", HeaderMap::new()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["conversation"], "test");
        assert_eq!(json["fake"], true);
        assert_eq!(json["model"], agent_core::DEFAULT_MODEL);
    }

    #[tokio::test]
    async fn models_endpoint_lists_fake_models() {
        let state = AppState::fake();
        let (status, json, _) = get_json(&state, "/api/models", HeaderMap::new()).await;
        assert_eq!(status, StatusCode::OK);
        let models = json["models"].as_array().unwrap();
        assert!(!models.is_empty());
        assert_eq!(models[0]["id"], "openai/gpt-4o-mini");
    }

    #[tokio::test]
    async fn auth_is_enforced_when_a_token_is_configured() {
        let state = state_with_token(Some("secret-token"));
        let (status, _, _) = get_json(&state, "/api/session", HeaderMap::new()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let (status, _, _) = get_json(
            &state,
            "/api/session",
            headers(&[("x-auth-token", "wrong")]),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let (status, json, _) = get_json(
            &state,
            "/api/session",
            headers(&[("x-auth-token", "secret-token")]),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["fake"], false);
    }

    #[tokio::test]
    async fn auth_is_optional_without_a_token() {
        let state = state_with_token(None);
        let (status, _, _) = get_json(&state, "/api/session", HeaderMap::new()).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn chat_streams_sse_from_the_fake_provider() {
        let state = AppState::fake();
        let response = request(
            &state,
            axum::http::Method::POST,
            "/api/chat",
            Some(&json!({ "message": "hello" })),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let (parts, body) = response.into_parts();
        assert!(
            parts.headers["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/event-stream")
        );
        assert_eq!(parts.headers["cache-control"], "no-store");
        let bytes = body.collect().await.unwrap().to_bytes();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains("event: delta"), "missing delta frame: {text}");
        assert!(
            text.contains("fake provider"),
            "unexpected fake answer: {text}"
        );
        assert!(
            text.contains("event: turn_done"),
            "missing terminal frame: {text}"
        );
        // History must now hold the exchange.
        assert_eq!(state.session.history().len(), 2);
    }

    #[tokio::test]
    async fn chat_rejects_empty_messages() {
        let state = AppState::fake();
        let response = request(
            &state,
            axum::http::Method::POST,
            "/api/chat",
            Some(&json!({ "message": "   " })),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn chat_rejects_a_second_concurrent_turn() {
        let core = Arc::new(AgentCore::new(
            Config::default(),
            Arc::new(FakeProvider::builtin().with_delay(std::time::Duration::from_secs(30))),
        ));
        let session = Arc::new(ChatSession::new(Arc::clone(&core), "test"));
        let slow = AppState { core, session };

        let first = request(
            &slow,
            axum::http::Method::POST,
            "/api/chat",
            Some(&json!({ "message": "first" })),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(first.status(), StatusCode::OK);
        let response = request(
            &slow,
            axum::http::Method::POST,
            "/api/chat",
            Some(&json!({ "message": "second" })),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        slow.session.abort().unwrap();
    }

    #[tokio::test]
    async fn abort_is_idempotent() {
        let state = AppState::fake();
        let response = request(
            &state,
            axum::http::Method::POST,
            "/api/abort",
            None,
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn model_override_is_applied_and_clearable() {
        let state = AppState::fake();
        let response = request(
            &state,
            axum::http::Method::POST,
            "/api/chat",
            Some(&json!({ "message": "hi", "model": "custom/m" })),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        // Drain the SSE body: it only ends when the turn completed, which
        // makes the follow-up send deterministic (no busy race).
        let _ = response.into_body().collect().await;
        assert_eq!(state.session.current_model(), "custom/m");

        let response = request(
            &state,
            axum::http::Method::POST,
            "/api/chat",
            Some(&json!({ "message": "again", "model": "" })),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let _ = response.into_body().collect().await;
        assert_eq!(state.session.current_model(), agent_core::DEFAULT_MODEL);
    }
}
