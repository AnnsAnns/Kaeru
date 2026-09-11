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
    ChatRequest, ModelInfo, WireChatRequest, WireChunk, WireCompletion, WireDeltaToolCall,
    WireErrorBody, WireModelList,
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
                .filter_map(|m| m.id.map(|id| (id, m.reasoning)))
                .map(|(id, reasoning)| {
                    let mut info = ModelInfo::new(id);
                    if let Some(reasoning) = reasoning {
                        info.reasoning_effort_levels = reasoning
                            .effort_levels
                            .into_iter()
                            .filter_map(|level| level.value)
                            .collect();
                        info.default_reasoning_effort = reasoning
                            .default_effort_level
                            .filter(|effort| !effort.trim().is_empty());
                    }
                    info
                })
                .collect())
        })
    }
}

/// Forward one event; returns false when the consumer is gone.
async fn emit(tx: &mpsc::Sender<CoreEvent>, event: CoreEvent) -> bool {
    tx.send(event).await.is_ok()
}

/// Accumulates streamed `tool_calls` fragments into complete calls (M3).
///
/// Fragments are indexed; `id`/`name` usually arrive first, `arguments` is
/// split across many chunks. Completed calls are emitted as `ToolCall` events
/// just before the turn's terminal event.
#[derive(Debug, Default)]
struct ToolCallAccumulator {
    calls: Vec<PartialToolCall>,
}

#[derive(Debug, Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl ToolCallAccumulator {
    fn push(&mut self, fragments: &[WireDeltaToolCall]) {
        for fragment in fragments {
            while self.calls.len() <= fragment.index {
                self.calls.push(PartialToolCall::default());
            }
            let call = &mut self.calls[fragment.index];
            if let Some(id) = &fragment.id {
                call.id = id.clone();
            }
            if let Some(function) = &fragment.function {
                if let Some(name) = &function.name {
                    call.name.push_str(name);
                }
                if let Some(arguments) = &function.arguments {
                    call.arguments.push_str(arguments);
                }
            }
        }
    }

    /// Drain the accumulated calls as `ToolCall` events (arguments parsed; a
    /// malformed payload becomes `null` so the tool can report a clear error).
    fn take_events(&mut self) -> Vec<CoreEvent> {
        std::mem::take(&mut self.calls)
            .into_iter()
            .filter(|call| !call.name.is_empty())
            .map(|call| CoreEvent::ToolCall {
                id: call.id,
                name: call.name,
                input: serde_json::from_str(&call.arguments).unwrap_or(serde_json::Value::Null),
            })
            .collect()
    }
}

/// Read an SSE response and forward it as `CoreEvent`s, tolerantly.
async fn stream_events(response: reqwest::Response, tx: &mpsc::Sender<CoreEvent>) {
    let mut parser = SseParser::new();
    let mut usage: Option<Usage> = None;
    let mut tool_calls = ToolCallAccumulator::default();
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
            if !forward_payload(tx, &mut usage, &mut tool_calls, &mut done, payload).await {
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
        if !forward_payload(tx, &mut usage, &mut tool_calls, &mut done, payload).await {
            return;
        }
    }
    if !done {
        finish_stream(tx, usage, &mut tool_calls).await;
    }
}

/// Emit any tool calls produced by the turn, then its terminal `TurnDone`.
/// Returns false when the consumer is gone.
async fn finish_stream(
    tx: &mpsc::Sender<CoreEvent>,
    usage: Option<Usage>,
    tool_calls: &mut ToolCallAccumulator,
) -> bool {
    for event in tool_calls.take_events() {
        if !emit(tx, event).await {
            return false;
        }
    }
    emit(tx, CoreEvent::TurnDone { usage }).await
}

/// Handle one SSE payload. Returns false when the consumer is gone.
async fn forward_payload(
    tx: &mpsc::Sender<CoreEvent>,
    usage: &mut Option<Usage>,
    tool_calls: &mut ToolCallAccumulator,
    done: &mut bool,
    payload: String,
) -> bool {
    if payload == "[DONE]" {
        *done = true;
        return finish_stream(tx, usage.take(), tool_calls).await;
    }
    for event in events_from_payload(&payload, usage, tool_calls) {
        if !emit(tx, event).await {
            return false;
        }
    }
    true
}

/// Parse one SSE payload into delta events (pure; unit-tested). Tool-call
/// fragments are accumulated rather than emitted (see [`ToolCallAccumulator`]).
fn events_from_payload(
    payload: &str,
    usage: &mut Option<Usage>,
    tool_calls: &mut ToolCallAccumulator,
) -> Vec<CoreEvent> {
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
    let mut events = Vec::new();
    for choice in chunk.choices {
        // Thinking usually arrives before the answer in the same delta stream.
        if let Some(text) = choice.delta.reasoning_text()
            && !text.is_empty()
        {
            events.push(CoreEvent::Reasoning {
                text: text.to_owned(),
            });
        }
        if let Some(text) = choice.delta.content
            && !text.is_empty()
        {
            events.push(CoreEvent::Delta { text });
        }
        if !choice.delta.tool_calls.is_empty() {
            tool_calls.push(&choice.delta.tool_calls);
        }
    }
    events
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
    let (reasoning, text, tool_calls) = completion
        .choices
        .into_iter()
        .next()
        .map(|c| {
            let calls = c.message.tool_calls();
            (
                c.message.reasoning_text().unwrap_or_default().to_owned(),
                c.message.content.unwrap_or_default(),
                calls,
            )
        })
        .unwrap_or_default();
    if !reasoning.is_empty() && !emit(tx, CoreEvent::Reasoning { text: reasoning }).await {
        return;
    }
    if !text.is_empty() && !emit(tx, CoreEvent::Delta { text }).await {
        return;
    }
    for call in tool_calls {
        if !emit(
            tx,
            CoreEvent::ToolCall {
                id: call.id,
                name: call.name,
                input: call.arguments,
            },
        )
        .await
        {
            return;
        }
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
        let mut tool_calls = ToolCallAccumulator::default();
        let events = events_from_payload(
            r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":"Hel"}}]}"#,
            &mut usage,
            &mut tool_calls,
        );
        assert_eq!(events, vec![CoreEvent::Delta { text: "Hel".into() }]);
        assert!(usage.is_none());
    }

    #[test]
    fn usage_payload_is_captured_not_emitted() {
        let mut usage = None;
        let mut tool_calls = ToolCallAccumulator::default();
        let events = events_from_payload(
            r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#,
            &mut usage,
            &mut tool_calls,
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
    fn tool_call_fragments_accumulate_into_complete_calls() {
        let mut usage = None;
        let mut tool_calls = ToolCallAccumulator::default();
        for payload in [
            r#"{"choices":[{"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"web_search","arguments":""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"query\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"frogs\"}"}}]}}]}"#,
        ] {
            assert!(events_from_payload(payload, &mut usage, &mut tool_calls).is_empty());
        }
        assert_eq!(
            tool_calls.take_events(),
            vec![CoreEvent::ToolCall {
                id: "call_1".into(),
                name: "web_search".into(),
                input: serde_json::json!({"query": "frogs"}),
            }]
        );
    }

    #[test]
    fn empty_content_deltas_are_dropped() {
        let mut usage = None;
        let mut tool_calls = ToolCallAccumulator::default();
        let events = events_from_payload(
            r#"{"choices":[{"delta":{"content":""}}]}"#,
            &mut usage,
            &mut tool_calls,
        );
        assert!(events.is_empty());
    }

    #[test]
    fn malformed_payloads_are_skipped() {
        let mut usage = None;
        let mut tool_calls = ToolCallAccumulator::default();
        assert!(events_from_payload("not json at all", &mut usage, &mut tool_calls).is_empty());
        assert!(
            events_from_payload(r#"{"unexpected": true}"#, &mut usage, &mut tool_calls).is_empty()
        );
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
        let mut tool_calls = ToolCallAccumulator::default();
        assert!(
            forward_payload(&tx, &mut usage, &mut tool_calls, &mut done, "[DONE]".into()).await
        );
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
        let mut tool_calls = ToolCallAccumulator::default();
        assert!(
            !forward_payload(&tx, &mut usage, &mut tool_calls, &mut done, "[DONE]".into()).await
        );
    }
}
