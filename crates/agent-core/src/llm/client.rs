//! The OpenAI-compatible HTTP adapter — the single place that knows the
//! provider protocol (ADR-005). Everything downstream sees `CoreEvent`s.
//!
//! Robustness rules (Quality Goal 1, "works with any OpenAI-compatible base
//! URL"):
//! - an empty `api_key` sends no `Authorization` header (local servers),
//! - a non-SSE answer (provider ignored `stream: true`) is parsed as a full
//!   JSON completion and forwarded as one delta,
//! - the `[DONE]` sentinel *or* a plain stream end both complete the turn,
//! - malformed SSE payloads are logged and skipped, never fatal,
//! - provider HTTP errors surface as `CoreEvent::Error` with a mapped kind.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_stream::StreamExt as _;

use crate::error::{ApiError, ApiErrorKind, Result};
use crate::events::{CoreEvent, Usage};
use crate::llm::sse::SseParser;
use crate::llm::types::{
    ChatRequest, ModelInfo, WireChatRequest, WireChunk, WireCompletion, WireErrorBody,
    WireModelList,
};

/// Backpressure channel from the adapter to the session layer.
pub const CLIENT_EVENT_CAPACITY: usize = 64;

pub type ChatFuture = Pin<Box<dyn Future<Output = Result<mpsc::Receiver<CoreEvent>>> + Send>>;
pub type ModelsFuture = Pin<Box<dyn Future<Output = Result<Vec<ModelInfo>>> + Send>>;

/// The seam between the core and any provider (real, fake, recording).
///
/// `chat` spawns the provider call and returns the event receiver
/// immediately; the returned stream always ends with exactly one terminal
/// event (`TurnDone` or `Error`), then closes.
pub trait LlmClient: Send + Sync {
    fn chat(&self, request: ChatRequest) -> ChatFuture;
    fn list_models(&self) -> ModelsFuture;
}

pub struct HttpClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl HttpClient {
    /// Build a streaming client for an OpenAI-compatible base URL.
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| ApiError::internal(format!("failed to build http client: {e}")))?;
        let base_url = base_url.into();
        let api_key = api_key.into();
        if base_url.trim().is_empty() {
            return Err(ApiError::config("provider base_url must not be empty"));
        }
        Ok(Self {
            http,
            base_url: base_url.trim().trim_end_matches('/').to_owned(),
            api_key,
        })
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }
}

impl LlmClient for HttpClient {
    fn chat(&self, request: ChatRequest) -> ChatFuture {
        let http = self.http.clone();
        let url = self.endpoint("/chat/completions");
        let api_key = self.api_key.clone();
        Box::pin(async move {
            let (tx, rx) = mpsc::channel(CLIENT_EVENT_CAPACITY);
            tokio::spawn(async move {
                let wire = WireChatRequest::streaming(&request);
                let builder = http.post(&url).json(&wire);
                let builder = if api_key.is_empty() {
                    builder
                } else {
                    builder.bearer_auth(&api_key)
                };
                let response = match builder.send().await {
                    Ok(response) => response,
                    Err(e) => {
                        emit(
                            &tx,
                            CoreEvent::error(
                                ApiErrorKind::Network,
                                format!("cannot reach provider {url}: {e}"),
                            ),
                        )
                        .await;
                        return;
                    }
                };

                let status = response.status();
                if !status.is_success() {
                    let body = response.text().await.unwrap_or_default();
                    emit(&tx, provider_error_event(status.as_u16(), &body)).await;
                    return;
                }

                let is_event_stream = response
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|v| v.to_ascii_lowercase().contains("text/event-stream"));

                if is_event_stream {
                    stream_events(response, &tx).await;
                } else {
                    json_fallback(response, &tx).await;
                }
            });
            Ok(rx)
        })
    }

    fn list_models(&self) -> ModelsFuture {
        let http = self.http.clone();
        let url = self.endpoint("/models");
        let api_key = self.api_key.clone();
        Box::pin(async move {
            let builder = http.get(&url);
            let builder = if api_key.is_empty() {
                builder
            } else {
                builder.bearer_auth(&api_key)
            };
            let response = builder.send().await.map_err(|e| {
                ApiError::new(
                    ApiErrorKind::Network,
                    format!("cannot reach provider {url}: {e}"),
                )
            })?;
            let status = response.status();
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                let event = provider_error_event(status.as_u16(), &body);
                let CoreEvent::Error { kind, message } = event else {
                    unreachable!("provider_error_event always returns an error event");
                };
                return Err(ApiError::new(kind, message));
            }
            let list: WireModelList = response.json().await.map_err(|e| {
                ApiError::new(
                    ApiErrorKind::Protocol,
                    format!("provider model list is not understood: {e}"),
                )
            })?;
            Ok(list
                .data
                .into_iter()
                .filter(|m| m.id.is_some())
                .map(|m| ModelInfo { id: m.id.unwrap() })
                .collect())
        })
    }
}

/// Forward one event; returns false when the consumer is gone.
async fn emit(tx: &mpsc::Sender<CoreEvent>, event: CoreEvent) -> bool {
    tx.send(event).await.is_ok()
}

/// Read an SSE response and forward it as `CoreEvent`s, tolerantly.
async fn stream_events(response: reqwest::Response, tx: &mpsc::Sender<CoreEvent>) {
    let mut parser = SseParser::new();
    let mut usage: Option<Usage> = None;
    let mut done = false;
    let mut stream = response.bytes_stream();

    while let Some(item) = stream.next().await {
        let bytes = match item {
            Ok(bytes) => bytes,
            Err(e) => {
                emit(
                    tx,
                    CoreEvent::error(
                        ApiErrorKind::Network,
                        format!("provider stream read failed: {e}"),
                    ),
                )
                .await;
                return;
            }
        };
        for payload in parser.push(&bytes) {
            if !forward_payload(tx, &mut usage, &mut done, payload).await {
                return;
            }
        }
        if done {
            return;
        }
    }

    // Tolerate providers that close the stream without `[DONE]` or that end
    // with a final payload lacking its terminating blank line.
    for payload in parser.finish() {
        if !forward_payload(tx, &mut usage, &mut done, payload).await {
            return;
        }
    }
    if !done {
        emit(tx, CoreEvent::TurnDone { usage }).await;
    }
}

/// Handle one SSE payload. Returns false when the consumer is gone.
async fn forward_payload(
    tx: &mpsc::Sender<CoreEvent>,
    usage: &mut Option<Usage>,
    done: &mut bool,
    payload: String,
) -> bool {
    if payload == "[DONE]" {
        *done = true;
        return emit(
            tx,
            CoreEvent::TurnDone {
                usage: usage.take(),
            },
        )
        .await;
    }
    for event in events_from_payload(&payload, usage) {
        if !emit(tx, event).await {
            return false;
        }
    }
    true
}

/// Parse one SSE payload into delta events (pure; unit-tested).
fn events_from_payload(payload: &str, usage: &mut Option<Usage>) -> Vec<CoreEvent> {
    let chunk: WireChunk = match serde_json::from_str(payload) {
        Ok(chunk) => chunk,
        Err(e) => {
            tracing::warn!(target: "agent_core::llm", "skipping malformed sse payload: {e}");
            return Vec::new();
        }
    };
    if let Some(wire_usage) = chunk.usage {
        *usage = Some(wire_usage.into());
    }
    chunk
        .choices
        .into_iter()
        .filter_map(|choice| choice.delta.content)
        .filter(|text| !text.is_empty())
        .map(|text| CoreEvent::Delta { text })
        .collect()
}

/// Fallback for providers that answer JSON despite `stream: true`.
async fn json_fallback(response: reqwest::Response, tx: &mpsc::Sender<CoreEvent>) {
    let body = match response.text().await {
        Ok(body) => body,
        Err(e) => {
            emit(
                tx,
                CoreEvent::error(
                    ApiErrorKind::Network,
                    format!("failed reading provider response: {e}"),
                ),
            )
            .await;
            return;
        }
    };
    let completion: WireCompletion = match serde_json::from_str(&body) {
        Ok(completion) => completion,
        Err(e) => {
            emit(
                tx,
                CoreEvent::error(
                    ApiErrorKind::Protocol,
                    format!("provider response is neither SSE nor a valid completion: {e}"),
                ),
            )
            .await;
            return;
        }
    };
    let text = completion
        .choices
        .into_iter()
        .find_map(|c| c.message.content)
        .unwrap_or_default();
    if !text.is_empty() && !emit(tx, CoreEvent::Delta { text }).await {
        return;
    }
    let usage = completion.usage.map(Usage::from);
    emit(tx, CoreEvent::TurnDone { usage }).await;
}

/// Map a provider HTTP failure to an error event (pure; unit-tested).
fn provider_error_event(status: u16, body: &str) -> CoreEvent {
    let kind = match status {
        401 | 403 => ApiErrorKind::Unauthorized,
        404 => ApiErrorKind::NotFound,
        429 => ApiErrorKind::RateLimited,
        _ => ApiErrorKind::Provider,
    };
    let detail = serde_json::from_str::<WireErrorBody>(body)
        .ok()
        .and_then(|b| b.error)
        .and_then(|e| e.message)
        .unwrap_or_default();
    let message = if detail.is_empty() {
        format!("provider returned HTTP {status}: {}", truncate(body, 300))
    } else {
        format!("provider returned HTTP {status}: {detail}")
    };
    CoreEvent::Error { kind, message }
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_owned()
    } else {
        format!("{}…", s.chars().take(max_chars).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_payload_becomes_delta_event() {
        let mut usage = None;
        let events = events_from_payload(
            r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":"Hel"}}]}"#,
            &mut usage,
        );
        assert_eq!(events, vec![CoreEvent::Delta { text: "Hel".into() }]);
        assert!(usage.is_none());
    }

    #[test]
    fn usage_payload_is_captured_not_emitted() {
        let mut usage = None;
        let events = events_from_payload(
            r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#,
            &mut usage,
        );
        assert!(events.is_empty());
        assert_eq!(
            usage,
            Some(Usage {
                input_tokens: Some(10),
                output_tokens: Some(5),
                total_tokens: Some(15),
            })
        );
    }

    #[test]
    fn empty_content_deltas_are_dropped() {
        let mut usage = None;
        let events = events_from_payload(r#"{"choices":[{"delta":{"content":""}}]}"#, &mut usage);
        assert!(events.is_empty());
    }

    #[test]
    fn malformed_payloads_are_skipped() {
        let mut usage = None;
        assert!(events_from_payload("not json at all", &mut usage).is_empty());
        assert!(events_from_payload(r#"{"unexpected": true}"#, &mut usage).is_empty());
        assert!(usage.is_none());
    }

    #[test]
    fn error_status_maps_to_kinds() {
        let CoreEvent::Error { kind, message } =
            provider_error_event(401, r#"{"error":{"message":"invalid key"}}"#)
        else {
            panic!("expected error event");
        };
        assert_eq!(kind, ApiErrorKind::Unauthorized);
        assert!(message.contains("invalid key"));

        let CoreEvent::Error { kind, .. } = provider_error_event(429, "") else {
            panic!("expected error event");
        };
        assert_eq!(kind, ApiErrorKind::RateLimited);

        let CoreEvent::Error { kind, message } = provider_error_event(500, "boom") else {
            panic!("expected error event");
        };
        assert_eq!(kind, ApiErrorKind::Provider);
        assert!(message.contains("boom"), "expected body text in: {message}");
    }

    #[test]
    fn error_message_falls_back_to_truncated_body() {
        let long = "x".repeat(500);
        let CoreEvent::Error { message, .. } = provider_error_event(502, &long) else {
            panic!("expected error event");
        };
        assert!(
            message.contains('…'),
            "expected truncation marker in: {message}"
        );
    }

    #[tokio::test]
    async fn forward_payload_done_completes_turn_with_captured_usage() {
        let (tx, mut rx) = mpsc::channel(CLIENT_EVENT_CAPACITY);
        let mut usage = Some(Usage {
            input_tokens: Some(1),
            output_tokens: Some(2),
            total_tokens: None,
        });
        let mut done = false;
        assert!(forward_payload(&tx, &mut usage, &mut done, "[DONE]".into()).await);
        assert!(done);
        drop(tx);
        let events: Vec<CoreEvent> = rx.recv().await.into_iter().collect();
        assert_eq!(
            events,
            vec![CoreEvent::TurnDone {
                usage: Some(Usage {
                    input_tokens: Some(1),
                    output_tokens: Some(2),
                    total_tokens: None,
                })
            }]
        );
    }

    #[tokio::test]
    async fn forward_payload_stops_when_consumer_is_gone() {
        let (tx, rx) = mpsc::channel(CLIENT_EVENT_CAPACITY);
        drop(rx);
        let mut usage = None;
        let mut done = false;
        assert!(!forward_payload(&tx, &mut usage, &mut done, "[DONE]".into()).await);
    }
}
