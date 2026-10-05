//! Thin daemon client (M6, ADR-011): the frog speaks the same `/api/*` wire
//! API as the web UI, so it needs no core dependency. Everything here is
//! transport + the `CoreEvent` wire vocabulary (mirrored, not imported — the
//! web client mirrors it in JS for the same reason).
//!
//! Endpoints used: `GET/POST /api/threads`, `POST /api/chat` (SSE over the
//! response body), `POST /api/approval`, `POST /api/abort`. Every request
//! carries `X-Auth-Token` when one is configured (mandatory on a tailnet bind,
//! ADR-030).

use std::collections::VecDeque;
use std::fmt;
use std::sync::RwLock;

use reqwest::Method;
use serde::Deserialize;
use serde_json::Value;

/// A client-side error, already phrased for the widget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    /// The daemon rejected the shared secret: prompt for it.
    Unauthorized,
    /// Anything else (transport, HTTP status, malformed payload).
    Message(String),
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unauthorized => write!(f, "the daemon requires an auth token"),
            Self::Message(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for ClientError {}

pub type Result<T> = std::result::Result<T, ClientError>;

/// One normalized turn event, mirroring `agent_core::CoreEvent`'s wire JSON
/// (`{"type": "delta", …}`). Unknown future variants deserialize to
/// [`Event::Unknown`] instead of killing the stream.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Delta {
        text: String,
    },
    Reasoning {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        #[serde(default)]
        input: Value,
    },
    ToolResult {
        #[serde(default)]
        id: String,
        #[serde(default)]
        name: String,
        #[serde(default)]
        output: String,
        #[serde(default)]
        is_error: bool,
    },
    Artifact {
        path: String,
        #[serde(default)]
        mime_hint: Option<String>,
    },
    ApprovalRequest {
        id: String,
        #[serde(default)]
        kind: Value,
        #[serde(default)]
        summary: String,
    },
    TurnDone {
        #[serde(default)]
        usage: Option<Usage>,
    },
    Error {
        #[serde(default)]
        kind: String,
        #[serde(default)]
        message: String,
    },
    #[serde(other)]
    Unknown,
}

/// Token usage, as carried by `turn_done`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
    #[serde(default)]
    pub total_tokens: Option<u64>,
}

/// One row of the thread list (newest first), as returned by `/api/threads`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ThreadSummary {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(rename = "messageCount", default)]
    pub message_count: usize,
}

#[derive(Debug, Deserialize)]
struct ThreadsResponse {
    #[serde(default)]
    threads: Vec<ThreadSummary>,
}

#[derive(Debug, Deserialize)]
struct CreatedThread {
    id: String,
}

/// A daemon API client. `token` is behind a lock so the widget can save a
/// freshly entered secret without rebuilding the client.
pub struct DaemonClient {
    http: reqwest::Client,
    base: String,
    token: RwLock<Option<String>>,
}

impl DaemonClient {
    /// `base` is normalized (trailing slash trimmed); `token` is normalized
    /// (empty = none).
    pub fn new(base: &str, token: Option<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base: base.trim().trim_end_matches('/').to_owned(),
            token: RwLock::new(token.filter(|token| !token.trim().is_empty())),
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// Replace the shared secret (used by the in-widget token prompt).
    pub fn set_token(&self, token: Option<String>) {
        *self.token.write().unwrap_or_else(|err| err.into_inner()) =
            token.filter(|token| !token.trim().is_empty());
    }

    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        let mut request = self.http.request(method, format!("{}{path}", self.base));
        if let Some(token) = self
            .token
            .read()
            .unwrap_or_else(|err| err.into_inner())
            .as_deref()
        {
            request = request.header("x-auth-token", token);
        }
        request
    }

    /// Map a non-success response to a [`ClientError`], reading the daemon's
    /// `{"error": {"message": …}}` envelope when present.
    async fn check(response: reqwest::Response) -> Result<reqwest::Response> {
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(ClientError::Unauthorized);
        }
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(ClientError::Message(api_message(&body, status)));
        }
        Ok(response)
    }

    /// The thread sidebar rows, newest first.
    pub async fn list_threads(&self) -> Result<Vec<ThreadSummary>> {
        let response = self
            .request(Method::GET, "/api/threads")
            .send()
            .await
            .map_err(transport)?;
        let response = Self::check(response).await?;
        let body: ThreadsResponse = response
            .json()
            .await
            .map_err(|err| ClientError::Message(format!("bad thread list: {err}")))?;
        Ok(body.threads)
    }

    /// Create a fresh empty thread, returning its id.
    pub async fn create_thread(&self) -> Result<String> {
        let response = self
            .request(Method::POST, "/api/threads")
            .send()
            .await
            .map_err(transport)?;
        let response = Self::check(response).await?;
        let body: CreatedThread = response
            .json()
            .await
            .map_err(|err| ClientError::Message(format!("bad thread payload: {err}")))?;
        Ok(body.id)
    }

    /// Pick the thread this widget should talk to: an explicitly pinned id when
    /// it still exists, else the newest thread, else a brand-new one. Absent
    /// `thread` matches the daemon's own "newest, created on demand" default.
    pub async fn resolve_thread(&self, configured: Option<&str>) -> Result<String> {
        let threads = self.list_threads().await?;
        if let Some(id) = configured.map(str::trim).filter(|id| !id.is_empty())
            && threads.iter().any(|thread| thread.id == id)
        {
            return Ok(id.to_owned());
        }
        match threads.first() {
            Some(thread) => Ok(thread.id.clone()),
            None => self.create_thread().await,
        }
    }

    /// Start a turn. The returned [`EventStream`] owns the SSE response body.
    pub async fn send(&self, thread: &str, message: &str) -> Result<EventStream> {
        let body = serde_json::json!({ "message": message, "thread": thread });
        let response = self
            .request(Method::POST, "/api/chat")
            .json(&body)
            .send()
            .await
            .map_err(transport)?;
        let response = Self::check(response).await?;
        Ok(EventStream::new(response))
    }

    /// Resolve a pending consent card. `false` denies.
    pub async fn approve(&self, thread: &str, id: &str, allow: bool) -> Result<()> {
        let body = serde_json::json!({
            "id": id,
            "decision": if allow { "allow" } else { "deny" },
            "thread": thread,
        });
        let response = self
            .request(Method::POST, "/api/approval")
            .json(&body)
            .send()
            .await
            .map_err(transport)?;
        Self::check(response).await?;
        Ok(())
    }

    /// Stop the active turn (idempotent server-side).
    pub async fn abort(&self, thread: &str) -> Result<()> {
        // Thread ids are `[A-Za-z0-9_-]` (the daemon validates them), so no
        // percent-encoding is needed here.
        let response = self
            .request(Method::POST, &format!("/api/abort?thread={thread}"))
            .send()
            .await
            .map_err(transport)?;
        Self::check(response).await?;
        Ok(())
    }
}

/// The SSE body of one `/api/chat` turn, decoded into [`Event`]s on demand.
pub struct EventStream {
    response: reqwest::Response,
    decoder: SseDecoder,
    pending: VecDeque<String>,
    done: bool,
}

impl EventStream {
    fn new(response: reqwest::Response) -> Self {
        Self {
            response,
            decoder: SseDecoder::default(),
            pending: VecDeque::new(),
            done: false,
        }
    }

    /// The next event, or `None` once the stream is exhausted.
    pub async fn next(&mut self) -> Option<Result<Event>> {
        loop {
            if let Some(data) = self.pending.pop_front() {
                return Some(parse_event(&data));
            }
            if self.done {
                return None;
            }
            match self.response.chunk().await {
                Ok(Some(chunk)) => self.pending.extend(self.decoder.push(&chunk)),
                Ok(None) => self.done = true,
                Err(err) => {
                    self.done = true;
                    return Some(Err(ClientError::Message(format!(
                        "the stream broke: {err}"
                    ))));
                }
            }
        }
    }
}

/// Byte-level SSE frame splitter. Chunks can split a frame (and a UTF-8 code
/// point) anywhere, so this buffers bytes and only yields whole frames; it
/// knows nothing about the payload.
#[derive(Default)]
struct SseDecoder {
    buffer: Vec<u8>,
}

impl SseDecoder {
    fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(chunk);
        let mut frames = Vec::new();
        while let Some((index, separator)) = find_separator(&self.buffer) {
            let frame: Vec<u8> = self.buffer.drain(..index).collect();
            self.buffer.drain(..separator);
            if let Some(data) = frame_data(&frame) {
                frames.push(data);
            }
        }
        frames
    }
}

/// Earliest blank-line separator in `buffer`, as `(index, separator_len)`.
/// Handles both `\n\n` and `\r\n\r\n`.
fn find_separator(buffer: &[u8]) -> Option<(usize, usize)> {
    [(&b"\r\n\r\n"[..], 4usize), (&b"\n\n"[..], 2usize)]
        .into_iter()
        .filter_map(|(pattern, len)| find_subslice(buffer, pattern).map(|index| (index, len)))
        .min_by_key(|(index, _)| *index)
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// The `data:` payload of one frame (comment/`event:` lines ignored), or
/// `None` for a frame with no data (e.g. a keep-alive comment).
fn frame_data(frame: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(frame);
    let mut data = String::new();
    for raw in text.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.starts_with(':') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            let value = rest.strip_prefix(' ').unwrap_or(rest);
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value);
        }
    }
    (!data.is_empty()).then_some(data)
}

fn parse_event(data: &str) -> Result<Event> {
    serde_json::from_str(data).map_err(|err| ClientError::Message(format!("bad event: {err}")))
}

fn api_message(body: &str, status: reqwest::StatusCode) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| format!("daemon returned {status}"))
}

fn transport(err: reqwest::Error) -> ClientError {
    ClientError::Message(format!("cannot reach the daemon: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(decoder: &mut SseDecoder, chunk: &str) -> Vec<String> {
        decoder.push(chunk.as_bytes())
    }

    #[test]
    fn decoder_reassembles_frames_split_across_chunks() {
        let mut decoder = SseDecoder::default();
        assert!(frames(&mut decoder, "event: delta\ndata: {\"ty").is_empty());
        let out = frames(&mut decoder, "pe\":\"delta\",\"text\":\"hi\"}\n\n");
        assert_eq!(out, vec!["{\"type\":\"delta\",\"text\":\"hi\"}".to_owned()]);
    }

    #[test]
    fn decoder_skips_keep_alive_comments_and_crlf() {
        let mut decoder = SseDecoder::default();
        let out = frames(
            &mut decoder,
            ": keep-alive\r\n\r\nevent: delta\r\ndata: {\"type\":\"delta\",\"text\":\"x\"}\r\n\r\n",
        );
        assert_eq!(out, vec!["{\"type\":\"delta\",\"text\":\"x\"}".to_owned()]);
    }

    #[test]
    fn decoder_yields_multiple_frames_in_one_chunk() {
        let mut decoder = SseDecoder::default();
        let out = frames(
            &mut decoder,
            "data: {\"type\":\"delta\",\"text\":\"a\"}\n\ndata: {\"type\":\"turn_done\",\"usage\":null}\n\n",
        );
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn events_round_trip_from_the_wire() {
        let cases = [
            (
                "{\"type\":\"delta\",\"text\":\"hi\"}",
                Event::Delta { text: "hi".into() },
            ),
            (
                "{\"type\":\"reasoning\",\"text\":\"hmm\"}",
                Event::Reasoning { text: "hmm".into() },
            ),
            (
                "{\"type\":\"tool_call\",\"id\":\"c1\",\"name\":\"web_search\",\"input\":{\"q\":1}}",
                Event::ToolCall {
                    id: "c1".into(),
                    name: "web_search".into(),
                    input: serde_json::json!({"q": 1}),
                },
            ),
            (
                "{\"type\":\"approval_request\",\"id\":\"a1\",\"kind\":{\"kind\":\"todo_write\",\"list\":\"x\"},\"summary\":\"edit x?\"}",
                Event::ApprovalRequest {
                    id: "a1".into(),
                    kind: serde_json::json!({"kind": "todo_write", "list": "x"}),
                    summary: "edit x?".into(),
                },
            ),
            (
                "{\"type\":\"turn_done\",\"usage\":{\"input_tokens\":3,\"output_tokens\":5}}",
                Event::TurnDone {
                    usage: Some(Usage {
                        input_tokens: Some(3),
                        output_tokens: Some(5),
                        total_tokens: None,
                    }),
                },
            ),
            (
                "{\"type\":\"error\",\"kind\":\"aborted\",\"message\":\"turn aborted\"}",
                Event::Error {
                    kind: "aborted".into(),
                    message: "turn aborted".into(),
                },
            ),
        ];
        for (json, expected) in cases {
            assert_eq!(parse_event(json).unwrap(), expected, "{json}");
        }
    }

    #[test]
    fn unknown_event_types_are_tolerated() {
        assert_eq!(
            parse_event("{\"type\":\"future_thing\"}").unwrap(),
            Event::Unknown
        );
    }

    #[test]
    fn thread_list_deserializes_camel_case_counts() {
        let body = "{\"threads\":[{\"id\":\"t1\",\"title\":\"hi\",\"messageCount\":4}]}";
        let parsed: ThreadsResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.threads[0].id, "t1");
        assert_eq!(parsed.threads[0].message_count, 4);
    }

    #[test]
    fn api_message_extracts_the_error_envelope() {
        let body = "{\"error\":{\"message\":\"nope\"}}";
        assert_eq!(api_message(body, reqwest::StatusCode::BAD_REQUEST), "nope");
        assert_eq!(
            api_message("garbage", reqwest::StatusCode::BAD_GATEWAY),
            "daemon returned 502 Bad Gateway"
        );
    }

    /* ---------- end-to-end against a tiny in-process daemon ---------- */

    /// Serve `router` on an ephemeral port; returns the base URL.
    async fn serve(router: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn resolves_a_thread_and_streams_a_turn() {
        use axum::routing::{get, post};

        let router = axum::Router::new()
            .route(
                "/api/threads",
                get(|| async {
                    axum::Json(serde_json::json!({
                        "threads": [{"id": "t1", "title": "hi", "messageCount": 2}]
                    }))
                })
                .post(|| async {
                    (
                        axum::http::StatusCode::CREATED,
                        axum::Json(serde_json::json!({"id": "new"})),
                    )
                }),
            )
            .route(
                "/api/chat",
                post(|| async {
                    let body = "event: delta\ndata: {\"type\":\"delta\",\"text\":\"hi\"}\n\n\
                                event: turn_done\ndata: {\"type\":\"turn_done\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}\n\n";
                    (
                        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                        body,
                    )
                }),
            )
            .route(
                "/api/approval",
                post(|| async { axum::http::StatusCode::NO_CONTENT }),
            )
            .route(
                "/api/abort",
                post(|| async { axum::http::StatusCode::NO_CONTENT }),
            );

        let client = DaemonClient::new(&serve(router).await, None);
        assert_eq!(client.resolve_thread(None).await.unwrap(), "t1");
        assert_eq!(client.resolve_thread(Some("gone")).await.unwrap(), "t1");

        let mut stream = client.send("t1", "hello").await.unwrap();
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event.unwrap());
        }
        assert_eq!(
            events,
            vec![
                Event::Delta { text: "hi".into() },
                Event::TurnDone {
                    usage: Some(Usage {
                        input_tokens: Some(1),
                        output_tokens: Some(2),
                        total_tokens: None,
                    }),
                },
            ]
        );

        client.approve("t1", "a1", true).await.unwrap();
        client.abort("t1").await.unwrap();
    }

    #[tokio::test]
    async fn a_401_becomes_unauthorized() {
        use axum::routing::get;
        let router = axum::Router::new().route(
            "/api/threads",
            get(|| async { axum::http::StatusCode::UNAUTHORIZED }),
        );
        let client = DaemonClient::new(&serve(router).await, None);
        assert_eq!(
            client.list_threads().await.unwrap_err(),
            ClientError::Unauthorized
        );
    }
}
