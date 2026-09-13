//! The `/api/*` handlers: chat/turn SSE, consent, threads, memory, reflection.

use agent_core::ChatSession;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::{bridge, error};

use super::{
    AppState, ApprovalBody, ChatBody, MemoryQuery, ThreadQuery, resolve_existing, resolve_thread,
    thread_error,
};

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
                "tool_call_id": message.tool_call_id,
                "artifacts": message.artifacts
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
/* ---------- handlers ---------- */

pub(super) async fn post_chat(
    State(state): State<AppState>,
    Json(body): Json<ChatBody>,
) -> Response {
    if body.message.trim().is_empty() {
        return error::json_error(
            StatusCode::BAD_REQUEST,
            "config",
            "message must not be empty",
        );
    }
    let attachments = match attachments_for(&state, &body.attachments) {
        Ok(attachments) => attachments,
        Err(response) => return *response,
    };
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
    match session.send_with_attachments(&body.message, attachments) {
        // Request-scoped M1 wiring (ADR-015): the SSE response owns the turn.
        Ok(handle) => bridge::sse_response(handle.into_events()),
        Err(err) => error::api_error(&err),
    }
}

/// Validate the chat's attachment paths against the workspace (M5): each must
/// be a single-component upload that exists, so a stored message can never
/// name something the file routes would refuse to serve. The error is boxed
/// for clippy's `result_large_err`, like [`resolve_thread`].
fn attachments_for(
    state: &AppState,
    paths: &[String],
) -> std::result::Result<Vec<agent_core::Artifact>, Box<Response>> {
    let bad = |message: String| {
        Box::new(error::json_error(
            StatusCode::BAD_REQUEST,
            "config",
            message,
        ))
    };
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let Some(files) = state.files.as_ref() else {
        return Err(bad("attachments need the workspace file flow".to_owned()));
    };
    let mut attachments = Vec::with_capacity(paths.len());
    for path in paths {
        let name = agent_core::safe_file_name(path).map_err(|err| bad(err.message))?;
        if !files.workspace.join(&name).is_file() {
            return Err(bad(format!(
                "attachment {name:?} is not an uploaded workspace file"
            )));
        }
        attachments.push(agent_core::Artifact::new(
            name.clone(),
            agent_core::mime_hint(std::path::Path::new(&name)),
        ));
    }
    Ok(attachments)
}

pub(super) async fn post_abort(
    State(state): State<AppState>,
    Query(query): Query<ThreadQuery>,
) -> Response {
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
pub(super) async fn post_regenerate(
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
pub(super) async fn get_stream(
    State(state): State<AppState>,
    Query(query): Query<ThreadQuery>,
) -> Response {
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
pub(super) async fn post_approval(
    State(state): State<AppState>,
    Json(body): Json<ApprovalBody>,
) -> Response {
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

pub(super) async fn get_models(State(state): State<AppState>) -> Response {
    match state.core.client().list_models().await {
        Ok(models) => Json(json!({ "models": models })).into_response(),
        Err(err) => error::api_error(&err),
    }
}

pub(super) async fn get_session(
    State(state): State<AppState>,
    Query(query): Query<ThreadQuery>,
) -> Response {
    // M2 compatibility alias: reload/restore payload for one thread.
    match resolve_thread(&state, query.thread.as_deref()) {
        Ok(session) => Json(thread_payload(&state, &session)).into_response(),
        Err(response) => *response,
    }
}

pub(super) async fn list_threads(State(state): State<AppState>) -> Response {
    match state.registry.list() {
        Ok(threads) => Json(json!({ "threads": threads })).into_response(),
        Err(err) => error::api_error(&err),
    }
}

pub(super) async fn create_thread(State(state): State<AppState>) -> Response {
    match state.registry.create(None) {
        Ok(session) => (
            StatusCode::CREATED,
            Json(json!({ "id": session.conversation_id() })),
        )
            .into_response(),
        Err(err) => error::api_error(&err),
    }
}

pub(super) async fn get_thread(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.registry.get(&id) {
        Ok(session) => Json(thread_payload(&state, &session)).into_response(),
        Err(err) => thread_error(&err),
    }
}

pub(super) async fn delete_thread(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    match state.registry.delete(&id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => thread_error(&err),
    }
}

/// The memory browser (M4): durable notes, newest first, optionally filtered
/// by a free-text `?q=`. Read-only; writes still go through the consent-gated
/// `memory_write` tool.
pub(super) async fn list_memory(
    State(state): State<AppState>,
    Query(query): Query<MemoryQuery>,
) -> Response {
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
pub(super) async fn post_reflect(State(state): State<AppState>) -> Response {
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
