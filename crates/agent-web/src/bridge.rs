//! `CoreEvent` → SSE bridge (C10: streaming through both hops; C12: the
//! browser client uses `fetch` + `ReadableStream`, so we speak SSE over a
//! POST response body).

use std::convert::Infallible;
use std::time::Duration;

use agent_core::{CoreEvent, EventStream};
use axum::http::header;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::BroadcastStream;

/// Interval for the SSE comment heartbeat; keeps intermediate proxies (later:
/// cloudflared, M2) from buffering idle streams.
const KEEP_ALIVE: Duration = Duration::from_secs(15);

pub fn to_sse_event(event: &CoreEvent) -> Event {
    Event::default()
        .event(event.event_name())
        .data(serde_json::to_string(event).unwrap_or_else(|_| "{}".into()))
}

/// Wrap a session event stream as an SSE response (`Cache-Control: no-store`).
pub fn sse_response(events: EventStream) -> Response {
    let stream = BroadcastStream::new(events).filter_map(|item| match item {
        Ok(event) => Some(Ok::<Event, Infallible>(to_sse_event(&event))),
        Err(lagged) => {
            tracing::warn!(target: "agent_web::bridge", "sse subscriber lagged by {lagged} events");
            None
        }
    });
    let mut response = Sse::new(stream)
        .keep_alive(KeepAlive::default().interval(KEEP_ALIVE).text("keep-alive"))
        .into_response();
    // SSE must never be cached, by browsers or edges.
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::Usage;

    // Raw frame bytes (`event: ...\ndata: {...}`) are asserted end-to-end in
    // routes::tests::chat_streams_sse_from_the_fake_provider; here we pin the
    // mapping itself: event name + wire JSON of the payload.

    fn payload(event: &CoreEvent) -> serde_json::Value {
        serde_json::from_str(&serde_json::to_string(event).unwrap()).unwrap()
    }

    #[test]
    fn delta_frames_carry_their_name_and_full_json() {
        let event = CoreEvent::Delta { text: "hi".into() };
        assert_eq!(event.event_name(), "delta");
        assert_eq!(
            payload(&event),
            serde_json::json!({"type": "delta", "text": "hi"})
        );
    }

    #[test]
    fn reasoning_frames_carry_their_name_and_full_json() {
        let event = CoreEvent::Reasoning { text: "hmm".into() };
        assert_eq!(event.event_name(), "reasoning");
        assert_eq!(
            payload(&event),
            serde_json::json!({"type": "reasoning", "text": "hmm"})
        );
    }

    #[test]
    fn usage_round_trips_on_turn_done() {
        let event = CoreEvent::TurnDone {
            usage: Some(Usage {
                input_tokens: Some(3),
                output_tokens: None,
                total_tokens: None,
            }),
        };
        assert_eq!(event.event_name(), "turn_done");
        assert_eq!(payload(&event)["usage"]["input_tokens"], 3);
    }

    #[test]
    fn error_events_name_their_kind() {
        let event = CoreEvent::error(agent_core::ApiErrorKind::Aborted, "turn aborted");
        assert_eq!(event.event_name(), "error");
        assert_eq!(payload(&event)["kind"], "aborted");
    }
}
