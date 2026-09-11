//! HTTP surface: `/api/threads*`, `/api/chat` (SSE), `/api/abort`,
//! `/api/models`, and the `/api/session` compatibility alias, plus embedded
//! static assets. Static assets are public and cacheable; every `/api/*`
//! route goes through the `X-Auth-Token` check when a token is configured,
//! and API responses are never cacheable.
//!
//! M2.5: the UI calls conversations *threads*. The [`ConversationRegistry`]
//! owns one live `ChatSession` per thread; `/api/chat` and `/api/abort` take
//! an optional `thread` id (absent = the newest thread, created on demand).

use std::sync::Arc;

use agent_core::{AgentCore, ChatSession, ConversationRegistry};
use axum::extract::{Path, Query, Request, State};
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
    pub registry: Arc<ConversationRegistry>,
}

impl AppState {
    pub fn new(core: Arc<AgentCore>, registry: Arc<ConversationRegistry>) -> Self {
        Self { core, registry }
    }

    /// Test helper: a state on the builtin fake provider, backed by a fresh
    /// throwaway conversations directory.
    #[cfg(test)]
    pub fn fake() -> Self {
        let core = Arc::new(AgentCore::with_mode(
            agent_core::Config::default(),
            Arc::new(agent_core::FakeProvider::builtin()),
            agent_core::ClientMode::Fake {
                cassette: std::path::PathBuf::new(),
            },
        ));
        Self::with_core(core)
    }

    #[cfg(test)]
    pub fn with_core(core: Arc<AgentCore>) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "kaeru-web-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let registry = Arc::new(ConversationRegistry::new(
            Arc::clone(&core),
            agent_core::ConversationStore::new(dir),
        ));
        Self { core, registry }
    }
}

pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/session", get(get_session))
        .route("/models", get(get_models))
        .route("/chat", post(post_chat))
        .route("/abort", post(post_abort))
        .route("/threads", get(list_threads).post(create_thread))
        .route("/threads/{id}", get(get_thread).delete(delete_thread))
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

/* ---------- request bodies / queries ---------- */

#[derive(Debug, Deserialize)]
struct ChatBody {
    message: String,
    /// Optional per-conversation model override (empty string clears it).
    #[serde(default)]
    model: Option<String>,
    /// Target thread; absent = newest thread (created on demand).
    #[serde(default)]
    thread: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ThreadQuery {
    #[serde(default)]
    thread: Option<String>,
}

/* ---------- helpers ---------- */

fn thread_payload(state: &AppState, session: &ChatSession) -> serde_json::Value {
    let id = session.conversation_id();
    json!({
        // `conversation` kept as an M2-compatible alias of `id`.
        "conversation": id,
        "id": id,
        "model": session.current_model(),
        "active": session.is_active(),
        "fake": state.core.is_fake(),
        "title": session.title(),
        "summary": session.summary(),
        "history": serde_json::to_value(session.history()).unwrap_or_default(),
        "usage": serde_json::to_value(session.total_usage()).unwrap_or_default(),
    })
}

/// Map a core error to HTTP, honoring an explicit thread lookup: a missing
/// thread is a plain 404 (a client mistake), unlike the provider-flavored
/// `NotFound` that becomes a gateway error.
fn thread_error(err: &agent_core::ApiError) -> Response {
    if err.kind == agent_core::ApiErrorKind::NotFound {
        error::json_error(StatusCode::NOT_FOUND, "not_found", err.message.clone())
    } else {
        error::api_error(err)
    }
}

/// Resolve the session a request targets: the named thread (404 if absent),
/// else the newest thread, else a fresh one.
fn resolve_thread(
    state: &AppState,
    thread: Option<&str>,
) -> std::result::Result<Arc<ChatSession>, Box<Response>> {
    match thread {
        Some(id) => state
            .registry
            .get(id)
            .map_err(|e| Box::new(thread_error(&e))),
        None => match state.registry.latest() {
            Ok(Some(session)) => Ok(session),
            Ok(None) => state
                .registry
                .create(None)
                .map_err(|e| Box::new(error::api_error(&e))),
            Err(e) => Err(Box::new(error::api_error(&e))),
        },
    }
}

/// Resolve without ever creating a thread (for abort).
fn resolve_existing(
    state: &AppState,
    thread: Option<&str>,
) -> std::result::Result<Option<Arc<ChatSession>>, Box<Response>> {
    match thread {
        Some(id) => state
            .registry
            .get(id)
            .map(Some)
            .map_err(|e| Box::new(thread_error(&e))),
        None => state
            .registry
            .latest()
            .map_err(|e| Box::new(error::api_error(&e))),
    }
}

/* ---------- handlers ---------- */

async fn post_chat(State(state): State<AppState>, Json(body): Json<ChatBody>) -> Response {
    if body.message.trim().is_empty() {
        return error::json_error(
            StatusCode::BAD_REQUEST,
            "config",
            "message must not be empty",
        );
    }
    let session = match resolve_thread(&state, body.thread.as_deref()) {
        Ok(session) => session,
        Err(response) => return *response,
    };
    if let Some(model) = body.model.as_deref() {
        let model = model.trim();
        session.set_model((!model.is_empty()).then(|| model.to_owned()));
    }
    match session.send(&body.message) {
        // Request-scoped M1 wiring (ADR-015): the SSE response owns the turn.
        Ok(handle) => bridge::sse_response(handle.into_events()),
        Err(err) => error::api_error(&err),
    }
}

async fn post_abort(State(state): State<AppState>, Query(query): Query<ThreadQuery>) -> Response {
    let session = match resolve_existing(&state, query.thread.as_deref()) {
        Ok(session) => session,
        Err(response) => return *response,
    };
    let result = match session {
        Some(session) => session.abort(),
        None => Ok(false),
    };
    match result {
        // Idempotent: aborting an idle/absent thread is a no-op, not an error.
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

async fn get_session(State(state): State<AppState>, Query(query): Query<ThreadQuery>) -> Response {
    // M2 compatibility alias: reload/restore payload for one thread.
    match resolve_thread(&state, query.thread.as_deref()) {
        Ok(session) => Json(thread_payload(&state, &session)).into_response(),
        Err(response) => *response,
    }
}

async fn list_threads(State(state): State<AppState>) -> Response {
    match state.registry.list() {
        Ok(threads) => Json(json!({ "threads": threads })).into_response(),
        Err(err) => error::api_error(&err),
    }
}

async fn create_thread(State(state): State<AppState>) -> Response {
    match state.registry.create(None) {
        Ok(session) => (
            StatusCode::CREATED,
            Json(json!({ "id": session.conversation_id() })),
        )
            .into_response(),
        Err(err) => error::api_error(&err),
    }
}

async fn get_thread(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.registry.get(&id) {
        Ok(session) => Json(thread_payload(&state, &session)).into_response(),
        Err(err) => thread_error(&err),
    }
}

async fn delete_thread(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.registry.delete(&id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => thread_error(&err),
    }
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
        AppState::with_core(core)
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

    /// POST /api/chat and drain the SSE body to completion.
    async fn chat(state: &AppState, body: serde_json::Value) -> axum::response::Response {
        let response = request(
            state,
            axum::http::Method::POST,
            "/api/chat",
            Some(&body),
            HeaderMap::new(),
        )
        .await;
        let status = response.status();
        if status.is_success() {
            let _ = response.into_body().collect().await;
        }
        // Rebuild a response is unnecessary; callers mostly want the status.
        axum::http::Response::builder()
            .status(status)
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn session_endpoint_reports_fake_state_and_auto_creates_a_thread() {
        let state = AppState::fake();
        let (status, json, _) = get_json(&state, "/api/session", HeaderMap::new()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["fake"], true);
        assert_eq!(json["model"], agent_core::DEFAULT_MODEL);
        assert!(json["id"].as_str().is_some());
        // Auto-creation persisted an empty thread.
        assert_eq!(state.registry.list().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn session_endpoint_restores_history_after_a_reload() {
        let state = AppState::fake();
        let response = chat(&state, json!({ "message": "hello" })).await;
        assert_eq!(response.status(), StatusCode::OK);

        let (status, json, _) = get_json(&state, "/api/session", HeaderMap::new()).await;
        assert_eq!(status, StatusCode::OK);
        let history = json["history"].as_array().unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0]["role"], "user");
        assert_eq!(history[0]["content"], "hello");
        assert_eq!(history[1]["role"], "assistant");
        assert_eq!(json["title"], "hello");
        assert_eq!(json["summary"], serde_json::Value::Null);
        // Accumulated usage: the builtin fake reports 21 in / 42 out / 63 total.
        assert_eq!(json["usage"]["input_tokens"], 21);
        assert_eq!(json["usage"]["total_tokens"], 63);
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
        let (_, json, _) = get_json(&state, "/api/session", HeaderMap::new()).await;
        assert_eq!(json["history"].as_array().unwrap().len(), 2);
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
        // Rejected before any thread is created.
        assert!(state.registry.list().unwrap().is_empty());
    }

    #[tokio::test]
    async fn chat_rejects_a_second_concurrent_turn() {
        let core = Arc::new(AgentCore::new(
            Config::default(),
            Arc::new(FakeProvider::builtin().with_delay(std::time::Duration::from_secs(30))),
        ));
        let slow = AppState::with_core(core);

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
        // Abort through the API to release the slow turn.
        let abort = request(
            &slow,
            axum::http::Method::POST,
            "/api/abort",
            None,
            HeaderMap::new(),
        )
        .await;
        assert_eq!(abort.status(), StatusCode::NO_CONTENT);
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
        // Aborting with no threads must not create one.
        assert!(state.registry.list().unwrap().is_empty());
    }

    #[tokio::test]
    async fn model_override_is_applied_and_clearable() {
        let state = AppState::fake();
        chat(&state, json!({ "message": "hi", "model": "custom/m" })).await;
        let (_, json, _) = get_json(&state, "/api/session", HeaderMap::new()).await;
        assert_eq!(json["model"], "custom/m");

        chat(&state, json!({ "message": "again", "model": "" })).await;
        let (_, json, _) = get_json(&state, "/api/session", HeaderMap::new()).await;
        assert_eq!(json["model"], agent_core::DEFAULT_MODEL);
    }

    #[tokio::test]
    async fn threads_can_be_created_listed_fetched_and_deleted() {
        let state = AppState::fake();

        let created = request(
            &state,
            axum::http::Method::POST,
            "/api/threads",
            None,
            HeaderMap::new(),
        )
        .await;
        assert_eq!(created.status(), StatusCode::CREATED);
        let (_, body) = created.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let id = json["id"].as_str().unwrap().to_owned();

        let (status, json, _) = get_json(&state, "/api/threads", HeaderMap::new()).await;
        assert_eq!(status, StatusCode::OK);
        let threads = json["threads"].as_array().unwrap();
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0]["id"], id);
        assert_eq!(threads[0]["messageCount"], 0);

        let (status, json, _) =
            get_json(&state, &format!("/api/threads/{id}"), HeaderMap::new()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["id"], id);

        let deleted = request(
            &state,
            axum::http::Method::DELETE,
            &format!("/api/threads/{id}"),
            None,
            HeaderMap::new(),
        )
        .await;
        assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
        let (status, _, _) =
            get_json(&state, &format!("/api/threads/{id}"), HeaderMap::new()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn unknown_thread_ids_are_404() {
        let state = AppState::fake();
        let (status, _, _) = get_json(&state, "/api/threads/nope", HeaderMap::new()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let response = request(
            &state,
            axum::http::Method::POST,
            "/api/chat",
            Some(&json!({ "message": "hi", "thread": "nope" })),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn chat_targets_an_explicit_thread_and_keeps_histories_separate() {
        let state = AppState::fake();
        let a = state.registry.create(None).unwrap().conversation_id();
        // A second, unrelated thread that must stay empty.
        let b = state.registry.create(None).unwrap().conversation_id();

        chat(&state, json!({ "message": "hello a", "thread": a })).await;

        let (_, json, _) = get_json(&state, &format!("/api/threads/{a}"), HeaderMap::new()).await;
        assert_eq!(json["history"].as_array().unwrap().len(), 2);
        let (_, json, _) = get_json(&state, &format!("/api/threads/{b}"), HeaderMap::new()).await;
        assert_eq!(json["history"].as_array().unwrap().len(), 0);
    }
}
