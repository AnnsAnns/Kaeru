//! HTTP surface: `/api/threads*`, `/api/chat` (SSE), `/api/abort`,
//! `/api/regenerate`, `/api/stream`, `/api/approval`, `/api/memory`,
//! `/api/models`, `/api/reflect`, `/api/files*`, plus embedded static assets.
//! Static assets are public; every `/api/*` route checks `X-Auth-Token` when a
//! token is configured, and API responses are `no-store`.

mod handlers;
#[cfg(test)]
mod tests;

use handlers::{
    create_thread, delete_thread, get_models, get_session, get_stream, get_thread, list_memory,
    list_threads, post_abort, post_approval, post_chat, post_reflect, post_regenerate,
};

use std::sync::Arc;

use agent_core::{AgentCore, ChatSession, ConversationRegistry, Reflector};
use axum::Router;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post};
use serde::Deserialize;

use crate::{assets, error, files};

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
    /// Workspace files uploaded for this message (M5).
    #[serde(default)]
    attachments: Vec<String>,
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
