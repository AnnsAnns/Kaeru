use super::*;
use agent_core::{Config, FakeProvider};
use axum::body::Body;
use axum::http::header::{HeaderMap, HeaderName, HeaderValue};
use http_body_util::BodyExt;
use serde_json::json;
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

    let (status, json, _) = get_json(&state, &format!("/api/threads/{id}"), HeaderMap::new()).await;
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
    let (status, _, _) = get_json(&state, &format!("/api/threads/{id}"), HeaderMap::new()).await;
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
    let outside = std::env::temp_dir().join(format!("kaeru-web-outside-{}", std::process::id()));
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
async fn chat_attachments_are_validated_and_persisted_with_the_message() {
    let state = AppState::fake();
    let response = raw_request(
        &state,
        axum::http::Method::POST,
        "/api/files?name=photo.png",
        b"\x89PNG".to_vec(),
        HeaderMap::new(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);

    let thread = state.registry.create(None).unwrap().conversation_id();
    let response = request(
        &state,
        axum::http::Method::POST,
        "/api/chat",
        Some(&json!({
            "message": "rotate this",
            "thread": thread,
            "attachments": ["photo.png"],
        })),
        HeaderMap::new(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    response.into_body().collect().await.unwrap();

    let (status, json, _) =
        get_json(&state, &format!("/api/threads/{thread}"), HeaderMap::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["history"][0]["role"], "user");
    assert_eq!(json["history"][0]["artifacts"][0]["path"], "photo.png");
    // API payloads stay snake_case (the persisted file uses `mimeHint`).
    assert_eq!(json["history"][0]["artifacts"][0]["mime_hint"], "image/png");

    // Traversal-shaped names and files that were never uploaded are both
    // rejected before anything is sent.
    for bad in ["../photo.png", "/etc/passwd", "never-uploaded.png"] {
        let response = request(
            &state,
            axum::http::Method::POST,
            "/api/chat",
            Some(&json!({"message": "x", "thread": thread, "attachments": [bad]})),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "{bad:?} must be rejected"
        );
    }
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
