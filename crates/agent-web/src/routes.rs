//! HTTP surface: `/api/threads*`, `/api/chat` (SSE), `/api/abort`,
//! `/api/regenerate`, `/api/stream`, `/api/approval`, `/api/memory`,
//! `/api/models`, and the `/api/session` compatibility alias, plus embedded
//! static assets. Static assets are public and cacheable; every `/api/*`
//! route goes through the `X-Auth-Token` check when a token is configured,
//! and API responses are never cacheable.
//!
//! M2.5: the UI calls conversations *threads*. The [`ConversationRegistry`]
//! owns one live `ChatSession` per thread; `/api/chat` and `/api/abort` take
//! an optional `thread` id (absent = the newest thread, created on demand).

use std::sync::Arc;

use agent_core::{AgentCore, ChatSession, ConversationRegistry, Reflector};
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::{assets, bridge, error, files};

#[derive(Clone)]
pub struct AppState {
    pub core: Arc<AgentCore>,
    pub registry: Arc<ConversationRegistry>,
    /// Evening-reflection job (M4.5); `None` when the frontend did not wire one.
    pub reflector: Option<Arc<Reflector>>,
    /// Workspace file flow (M5); `None` when the frontend did not wire one.
    pub files: Option<files::Files>,
}

impl AppState {
    pub fn new(
        core: Arc<AgentCore>,
        registry: Arc<ConversationRegistry>,
        reflector: Option<Arc<Reflector>>,
        files: Option<files::Files>,
    ) -> Self {
        Self {
            core,
            registry,
            reflector,
            files,
        }
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
            agent_core::ConversationStore::new(dir.clone()),
        ));
        let workspace = dir.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        Self {
            core,
            registry,
            reflector: None,
            files: Some(files::Files {
                workspace,
                max_upload_bytes: 1024 * 1024,
            }),
        }
    }
}

pub fn router(state: AppState) -> Router {
    // The upload body limit is per-route; everything else keeps axum's small
    // JSON default.
    let upload_limit = state
        .files
        .as_ref()
        .map(|files| files.max_upload_bytes)
        .unwrap_or(2 * 1024 * 1024);
    let api = Router::new()
        .route("/session", get(get_session))
        .route("/models", get(get_models))
        .route("/chat", post(post_chat))
        .route("/abort", post(post_abort))
        .route("/regenerate", post(post_regenerate))
        .route("/stream", get(get_stream))
        .route("/approval", post(post_approval))
        .route("/threads", get(list_threads).post(create_thread))
        .route("/threads/{id}", get(get_thread).delete(delete_thread))
        .route("/memory", get(list_memory))
        .route("/reflect", post(post_reflect))
        .route(
            "/files",
            post(files::upload).layer(DefaultBodyLimit::max(upload_limit.max(1))),
        )
        .route("/files/{*path}", get(files::serve))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .route_layer(middleware::from_fn(no_store_middleware));
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

/// Every `/api/*` response is dynamic and must never be cached, so a browser
/// (or an intermediate proxy) cannot serve a stale thread list or model list.
async fn no_store_middleware(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}

/* ---------- request bodies / queries ---------- */

#[derive(Debug, Deserialize)]
struct ChatBody {
    message: String,
    /// Optional per-conversation model override (empty string clears it).
    #[serde(default)]
    model: Option<String>,
    /// Optional per-conversation reasoning effort (empty string clears it).
    #[serde(default)]
    reasoning_effort: Option<String>,
    /// Target thread; absent = newest thread (created on demand).
    #[serde(default)]
    thread: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ThreadQuery {
    #[serde(default)]
    thread: Option<String>,
}

/// Optional free-text filter for the memory browser (M4).
#[derive(Debug, Default, Deserialize)]
struct MemoryQuery {
    #[serde(default)]
    q: Option<String>,
}

/// Consent decision for a pending `ApprovalRequest` (M3).
#[derive(Debug, Deserialize)]
struct ApprovalBody {
    id: String,
    decision: agent_core::Decision,
    #[serde(default)]
    thread: Option<String>,
}

/* ---------- helpers ---------- */

fn thread_payload(state: &AppState, session: &ChatSession) -> serde_json::Value {
    let id = session.conversation_id();
    // Assistant replies are rendered to sanitized HTML here (ADR-026); the raw
    // Markdown is kept alongside for reference. User input stays plain text.
    let history: Vec<serde_json::Value> = session
        .history()
        .into_iter()
        .map(|message| {
            let html = (message.role == agent_core::Role::Assistant)
                .then(|| crate::markdown::render(&message.content));
            json!({
                "role": message.role,
                "content": message.content,
                "html": html,
                "reasoning": message.reasoning,
                "tool_calls": message.tool_calls,
                "tool_call_id": message.tool_call_id
            })
        })
        .collect();
    json!({
        // `conversation` kept as an M2-compatible alias of `id`.
        "conversation": id,
        "id": id,
        "model": session.current_model(),
        "reasoning_effort": session.current_reasoning_effort(),
        "active": session.is_active(),
        "fake": state.core.is_fake(),
        "title": session.title(),
        "summary": session.summary(),
        "history": history,
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
///
/// The error is boxed because `Response` is large enough to trip clippy's
/// `result_large_err`; only the error path pays for the allocation.
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

/// Resolve without ever creating a thread (for abort). See [`resolve_thread`]
/// for why the error is boxed.
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
    if let Some(effort) = body.reasoning_effort.as_deref() {
        let effort = effort.trim();
        session.set_reasoning_effort((!effort.is_empty()).then(|| effort.to_owned()));
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

/// Re-run the last user message for a thread (M3), dropping its old answer.
async fn post_regenerate(
    State(state): State<AppState>,
    Query(query): Query<ThreadQuery>,
) -> Response {
    let session = match resolve_thread(&state, query.thread.as_deref()) {
        Ok(session) => session,
        Err(response) => return *response,
    };
    match session.regenerate() {
        Ok(handle) => bridge::sse_response(handle.into_events()),
        Err(err) => error::api_error(&err),
    }
}

/// Re-attach to a thread's active turn: replays the buffered events then
/// streams live (M3, §6.3a). 204 when the thread has no active turn.
async fn get_stream(State(state): State<AppState>, Query(query): Query<ThreadQuery>) -> Response {
    let session = match resolve_existing(&state, query.thread.as_deref()) {
        Ok(session) => session,
        Err(response) => return *response,
    };
    match session {
        Some(session) if session.is_active() => bridge::sse_response(session.subscribe()),
        _ => StatusCode::NO_CONTENT.into_response(),
    }
}

/// Resolve a pending consent card (M3). Unknown ids (already resolved or
/// timed out) are a plain 404.
async fn post_approval(State(state): State<AppState>, Json(body): Json<ApprovalBody>) -> Response {
    let session = match resolve_existing(&state, body.thread.as_deref()) {
        Ok(session) => session,
        Err(response) => return *response,
    };
    let Some(session) = session else {
        return error::json_error(
            StatusCode::NOT_FOUND,
            "not_found",
            "no thread to approve for",
        );
    };
    match session.approve(&body.id, body.decision) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => thread_error(&err),
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

/// The memory browser (M4): durable notes, newest first, optionally filtered
/// by a free-text `?q=`. Read-only; writes still go through the consent-gated
/// `memory_write` tool.
async fn list_memory(State(state): State<AppState>, Query(query): Query<MemoryQuery>) -> Response {
    let Some(store) = state.core.memory() else {
        return Json(json!({ "configured": false, "count": 0, "notes": [] })).into_response();
    };
    let notes = match query.q.as_deref().map(str::trim) {
        Some(q) if !q.is_empty() => store.search(q, 50),
        _ => store.list().into_iter().take(200).collect(),
    };
    let notes: Vec<serde_json::Value> = notes
        .into_iter()
        .map(|note| {
            json!({
                "day": note.day,
                "slug": note.slug,
                "tags": note.tags,
                "created": note.created,
                "content": note.content,
                "modified": note.modified_unix,
            })
        })
        .collect();
    Json(json!({ "configured": true, "count": notes.len(), "notes": notes })).into_response()
}

/// Trigger an evening reflection on demand (M4.5, ADR-028). The same digest the
/// scheduler runs; the schedule is bypassed but `[reflect] enabled` still gates
/// it. Used by tests and the "reflect now" button in the memory panel.
async fn post_reflect(State(state): State<AppState>) -> Response {
    let Some(reflector) = state.reflector.as_ref() else {
        return error::json_error(
            StatusCode::NOT_FOUND,
            "not_found",
            "reflection is not configured",
        );
    };
    let now = agent_core::memory::store::now_unix();
    match reflector.run_now(now).await {
        Ok(outcome) => Json(json!({
            "status": reflect_status_name(outcome.status),
            "conversations": outcome.conversations,
            "notes": outcome.notes,
            "persona_changed": outcome.persona_changed,
        }))
        .into_response(),
        Err(err) => error::api_error(&err),
    }
}

fn reflect_status_name(status: agent_core::ReflectStatus) -> &'static str {
    match status {
        agent_core::ReflectStatus::Disabled => "disabled",
        agent_core::ReflectStatus::NotDue => "not_due",
        agent_core::ReflectStatus::Ran => "ran",
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
    async fn thread_history_ships_sanitized_html_for_assistant_messages() {
        let state = AppState::fake();
        chat(&state, json!({ "message": "**hi**" })).await;
        let (_, json, _) = get_json(&state, "/api/session", HeaderMap::new()).await;
        let history = json["history"].as_array().unwrap();
        assert_eq!(history[0]["role"], "user");
        // User text stays plain: no html.
        assert!(history[0]["html"].is_null());
        assert_eq!(history[1]["role"], "assistant");
        // Assistant replies carry server-rendered, sanitized HTML.
        assert!(history[1]["html"].as_str().unwrap().starts_with("<p>"));
        // Raw Markdown is preserved alongside.
        assert!(
            history[1]["content"]
                .as_str()
                .unwrap()
                .contains("fake provider")
        );
    }

    #[tokio::test]
    async fn memory_endpoint_is_empty_when_no_store_is_configured() {
        let state = AppState::fake();
        let (status, json, _) = get_json(&state, "/api/memory", HeaderMap::new()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["configured"], false);
        assert!(json["notes"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn memory_endpoint_lists_and_searches_notes() {
        let dir = std::env::temp_dir().join(format!("kaeru-web-memory-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let store = agent_core::MemoryStore::new(dir.join("memory"));
        store
            .write("frogs are amphibians", &["animals".into()])
            .unwrap();
        store.write("rust ownership", &["rust".into()]).unwrap();
        let core = Arc::new(
            AgentCore::with_mode(
                Config::default(),
                Arc::new(FakeProvider::builtin()),
                agent_core::ClientMode::Fake {
                    cassette: std::path::PathBuf::new(),
                },
            )
            .with_memory(store.clone()),
        );
        let state = AppState::with_core(core);

        let (status, json, _) = get_json(&state, "/api/memory", HeaderMap::new()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["configured"], true);
        assert_eq!(json["count"], 2);

        let (status, json, _) = get_json(&state, "/api/memory?q=frogs", HeaderMap::new()).await;
        assert_eq!(status, StatusCode::OK);
        let notes = json["notes"].as_array().unwrap();
        assert_eq!(notes.len(), 1);
        assert!(notes[0]["content"].as_str().unwrap().contains("frogs"));
        assert_eq!(notes[0]["tags"][0], "animals");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn reflect_endpoint_is_404_without_a_reflector() {
        let state = AppState::fake();
        let response = request(
            &state,
            axum::http::Method::POST,
            "/api/reflect",
            None,
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn reflect_endpoint_runs_the_digest_on_demand() {
        let dir = std::env::temp_dir().join(format!("kaeru-web-reflect-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let config = Config::parse("[reflect]\nenabled = true\n").unwrap();
        let core = Arc::new(
            AgentCore::new(config, Arc::new(FakeProvider::builtin()))
                .with_memory(agent_core::MemoryStore::new(dir.join("memory"))),
        );
        let store = agent_core::ConversationStore::new(dir.join("conversations"));
        let registry = Arc::new(ConversationRegistry::new(Arc::clone(&core), store.clone()));
        let reflector = Arc::new(
            Reflector::from_core(Arc::clone(&core), store, dir.join("reflect-state.json")).unwrap(),
        );
        let state = AppState::new(core, registry, Some(reflector), None);

        let response = request(
            &state,
            axum::http::Method::POST,
            "/api/reflect",
            None,
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let (_, body) = response.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["status"], "ran");
        assert_eq!(json["conversations"], 0);
        std::fs::remove_dir_all(&dir).ok();
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
    async fn api_responses_are_never_cacheable() {
        let state = AppState::fake();
        for path in ["/api/models", "/api/threads"] {
            let (status, _, headers) = get_json(&state, path, HeaderMap::new()).await;
            assert_eq!(status, StatusCode::OK, "{path}");
            assert_eq!(headers["cache-control"], "no-store", "{path}");
        }
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
    async fn reasoning_effort_is_applied_and_clearable() {
        let state = AppState::fake();
        chat(
            &state,
            json!({ "message": "hi", "reasoning_effort": "low" }),
        )
        .await;
        let (_, json, _) = get_json(&state, "/api/session", HeaderMap::new()).await;
        assert_eq!(json["reasoning_effort"], "low");

        chat(
            &state,
            json!({ "message": "again", "reasoning_effort": "" }),
        )
        .await;
        let (_, json, _) = get_json(&state, "/api/session", HeaderMap::new()).await;
        assert_eq!(json["reasoning_effort"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn chat_surfaces_reasoning_and_persists_it_for_reload() {
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
        let (_, body) = response.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            text.contains("event: reasoning"),
            "missing reasoning frame: {text}"
        );

        let (_, json, _) = get_json(&state, "/api/session", HeaderMap::new()).await;
        let assistant = &json["history"][1];
        assert!(
            assistant["reasoning"]
                .as_str()
                .unwrap()
                .contains("fake provider"),
            "reasoning not persisted: {assistant}"
        );
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

    /* ---------- M3 endpoints ---------- */

    #[tokio::test]
    async fn approval_for_an_unknown_request_is_404() {
        let state = AppState::fake();
        let thread = state.registry.create(None).unwrap().conversation_id();
        let response = request(
            &state,
            axum::http::Method::POST,
            "/api/approval",
            Some(&json!({ "id": "appr1", "decision": "allow", "thread": thread })),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn stream_endpoint_is_204_when_no_turn_is_active() {
        let state = AppState::fake();
        let thread = state.registry.create(None).unwrap().conversation_id();
        let response = request(
            &state,
            axum::http::Method::GET,
            &format!("/api/stream?thread={thread}"),
            None,
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn regenerate_without_a_previous_answer_is_a_config_error() {
        let state = AppState::fake();
        let thread = state.registry.create(None).unwrap().conversation_id();
        let response = request(
            &state,
            axum::http::Method::POST,
            &format!("/api/regenerate?thread={thread}"),
            None,
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    /* ---------- M5 file flow ---------- */

    async fn raw_request(
        state: &AppState,
        method: axum::http::Method,
        path: &str,
        body: Vec<u8>,
        headers: HeaderMap,
    ) -> axum::response::Response {
        let app = router(state.clone());
        let mut builder = axum::http::Request::builder().method(method).uri(path);
        for (name, value) in headers.iter() {
            builder = builder.header(name, value);
        }
        let request = builder.body(Body::from(body)).unwrap();
        app.oneshot(request).await.unwrap()
    }

    #[tokio::test]
    async fn uploads_land_in_the_workspace_and_serve_with_mime_headers() {
        let state = AppState::fake();
        let response = raw_request(
            &state,
            axum::http::Method::POST,
            "/api/files?name=plot.png",
            b"\x89PNG fake".to_vec(),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED);
        let (_, body) = response.into_parts();
        let json: serde_json::Value =
            serde_json::from_slice(&body.collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(json["path"], "plot.png");
        assert_eq!(json["size"], 9);

        let response = raw_request(
            &state,
            axum::http::Method::GET,
            "/api/files/plot.png",
            Vec::new(),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "image/png");
        assert!(
            response.headers()["content-disposition"]
                .to_str()
                .unwrap()
                .starts_with("inline")
        );
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            &b"\x89PNG fake"[..]
        );
    }

    #[tokio::test]
    async fn non_image_artifacts_download_as_attachments() {
        let state = AppState::fake();
        let response = raw_request(
            &state,
            axum::http::Method::POST,
            "/api/files?name=data.csv",
            b"a,b\n1,2\n".to_vec(),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED);

        let response = raw_request(
            &state,
            axum::http::Method::GET,
            "/api/files/data.csv",
            Vec::new(),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "text/csv");
        assert!(
            response.headers()["content-disposition"]
                .to_str()
                .unwrap()
                .starts_with("attachment")
        );
    }

    #[tokio::test]
    async fn traversal_and_absolute_paths_are_forbidden() {
        let state = AppState::fake();
        std::fs::write(
            state.files.as_ref().unwrap().workspace.join("secret.txt"),
            b"x",
        )
        .unwrap();
        for path in [
            "/api/files/../secret.txt",
            "/api/files/%2e%2e/secret.txt",
            "/api/files/..%2fsecret.txt",
            "/api/files/%2Fetc%2Fpasswd",
        ] {
            let response = raw_request(
                &state,
                axum::http::Method::GET,
                path,
                Vec::new(),
                HeaderMap::new(),
            )
            .await;
            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "{path} must be refused"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinks_out_of_the_workspace_are_forbidden() {
        let state = AppState::fake();
        let workspace = state.files.as_ref().unwrap().workspace.clone();
        let outside =
            std::env::temp_dir().join(format!("kaeru-web-outside-{}", std::process::id()));
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"no").unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), workspace.join("link.txt")).unwrap();

        let response = raw_request(
            &state,
            axum::http::Method::GET,
            "/api/files/link.txt",
            Vec::new(),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn uploads_are_name_checked_and_size_capped() {
        let mut state = AppState::fake();
        state.files.as_mut().unwrap().max_upload_bytes = 8;

        for name in ["../x", ".hidden", "a/b", ""] {
            let response = raw_request(
                &state,
                axum::http::Method::POST,
                &format!("/api/files?name={name}"),
                b"data".to_vec(),
                HeaderMap::new(),
            )
            .await;
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "{name:?} must be rejected"
            );
        }

        let response = raw_request(
            &state,
            axum::http::Method::POST,
            "/api/files?name=big.bin",
            vec![b'x'; 9],
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);

        let response = raw_request(
            &state,
            axum::http::Method::GET,
            "/api/files/missing.png",
            Vec::new(),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn file_routes_require_auth_when_a_token_is_configured() {
        let state = state_with_token(Some("s3cret"));
        let response = raw_request(
            &state,
            axum::http::Method::POST,
            "/api/files?name=x.txt",
            b"x".to_vec(),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let response = raw_request(
            &state,
            axum::http::Method::GET,
            "/api/files/x.txt",
            Vec::new(),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let response = raw_request(
            &state,
            axum::http::Method::POST,
            "/api/files?name=x.txt",
            b"x".to_vec(),
            headers(&[("x-auth-token", "s3cret")]),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED);
    }
}
