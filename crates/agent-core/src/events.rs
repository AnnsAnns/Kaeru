//! Wire-serializable core event stream (ADR-010/011/015): the normalized
//! vocabulary every frontend translates. The enum is complete from M1 on, but
//! variants arrive progressively (tool calls and consent with M3, artifacts
//! with M5) — do not add protocol fields early.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

use crate::error::ApiErrorKind;

/// A subscription of `CoreEvent`s with bounded replay (ADR-015): a reconnect
/// replays the buffered events of the active turn, then follows live. With no
/// live source and no buffer the stream is already closed.
///
/// The live half is a [`BroadcastStream`], not a bare `broadcast::Receiver`:
/// creating and dropping `rx.recv()` on every `poll_next` would unregister
/// the broadcast waiter and stall the stream until an unrelated wake-up.
pub struct EventStream {
    replay: std::vec::IntoIter<CoreEvent>,
    live: Option<BroadcastStream<CoreEvent>>,
}

impl EventStream {
    /// A stream that replays `replay` first, then follows `live`.
    pub fn replay(replay: Vec<CoreEvent>, live: broadcast::Receiver<CoreEvent>) -> Self {
        Self {
            replay: replay.into_iter(),
            live: Some(BroadcastStream::new(live)),
        }
    }

    /// A live-only stream (no replay); `into_events()` uses this because the
    /// primordial receiver already captures every event from the turn start.
    pub fn live(live: broadcast::Receiver<CoreEvent>) -> Self {
        Self {
            replay: Vec::new().into_iter(),
            live: Some(BroadcastStream::new(live)),
        }
    }

    /// An already-closed stream (no active turn).
    pub fn closed() -> Self {
        Self {
            replay: Vec::new().into_iter(),
            live: None,
        }
    }

    /// Receive the next event. Mirrors `broadcast::Receiver::recv`, including
    /// `Lagged` (the caller may keep receiving) and `Closed` at the end.
    pub async fn recv(&mut self) -> Result<CoreEvent, broadcast::error::RecvError> {
        if let Some(event) = self.replay.next() {
            return Ok(event);
        }
        match self.live.as_mut() {
            Some(stream) => match stream.next().await {
                Some(Ok(event)) => Ok(event),
                Some(Err(BroadcastStreamRecvError::Lagged(n))) => {
                    Err(broadcast::error::RecvError::Lagged(n))
                }
                None => Err(broadcast::error::RecvError::Closed),
            },
            None => Err(broadcast::error::RecvError::Closed),
        }
    }
}

impl tokio_stream::Stream for EventStream {
    type Item = CoreEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<CoreEvent>> {
        let this = self.as_mut().get_mut();
        if let Some(event) = this.replay.next() {
            return Poll::Ready(Some(event));
        }
        let Some(live) = this.live.as_mut() else {
            return Poll::Ready(None);
        };
        match Pin::new(live).poll_next(cx) {
            Poll::Ready(Some(Ok(event))) => Poll::Ready(Some(event)),
            // A lagged consumer may have missed any event, including a
            // consent card or the terminal event. Close the stream instead
            // of silently skipping: a re-subscribe replays the turn buffer
            // and recovers what was missed.
            Poll::Ready(Some(Err(BroadcastStreamRecvError::Lagged(skipped)))) => {
                tracing::warn!(
                    target: "agent_core::events",
                    skipped,
                    "event stream lagged; closing so the subscriber can replay"
                );
                this.live = None;
                Poll::Ready(None)
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// A frontend-supplied decision on an `ApprovalRequest` (ADR-014).
pub type ApprovalFuture = Pin<Box<dyn Future<Output = Decision> + Send>>;

/// Core-owned consent seam (ADR-014). Tools call this; the frontend supplies
/// the actual resolution (web: `POST /api/approval`; a default-deny timer
/// fails closed when nobody answers).
pub trait ApprovalSink: Send + Sync {
    /// Ask for consent; resolves to [`Decision::Deny`] on timeout or when the
    /// request could not be delivered (fail closed).
    fn request(&self, kind: ApprovalKind, summary: String) -> ApprovalFuture;
}

/// A sink that approves nothing — the fail-closed default for headless tests.
pub struct DenyAll;

impl ApprovalSink for DenyAll {
    fn request(&self, _kind: ApprovalKind, _summary: String) -> ApprovalFuture {
        Box::pin(async { Decision::Deny })
    }
}

/// Normalized agent turn events, shared by all frontends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CoreEvent {
    /// One incremental chunk of assistant text.
    Delta { text: String },
    /// One incremental chunk of model "thinking" (`reasoning_content`), kept
    /// out of the conversation context and shown as a collapsible block.
    Reasoning { text: String },
    /// The model requested a tool call \[M3].
    ToolCall {
        id: String,
        name: String,
        input: Value,
    },
    /// A tool finished; `output` is the fenced result the model will see \[M3].
    ToolResult {
        id: String,
        name: String,
        output: String,
        is_error: bool,
    },
    /// A workspace file surfaced for display/download \[M5].
    Artifact {
        path: String,
        mime_hint: Option<String>,
    },
    /// A risky action needs an explicit decision; blocks the tool [M3+].
    ApprovalRequest {
        id: String,
        kind: ApprovalKind,
        summary: String,
    },
    /// Terminal success event for a turn; carries usage when the provider
    /// reported it.
    TurnDone { usage: Option<Usage> },
    /// Terminal failure event for a turn (includes user aborts, `kind: aborted`).
    Error { kind: ApiErrorKind, message: String },
}

impl CoreEvent {
    /// Event name used on the wire (SSE `event:` field and friends).
    pub fn event_name(&self) -> &'static str {
        match self {
            Self::Delta { .. } => "delta",
            Self::Reasoning { .. } => "reasoning",
            Self::ToolCall { .. } => "tool_call",
            Self::ToolResult { .. } => "tool_result",
            Self::Artifact { .. } => "artifact",
            Self::ApprovalRequest { .. } => "approval_request",
            Self::TurnDone { .. } => "turn_done",
            Self::Error { .. } => "error",
        }
    }

    pub fn error(kind: ApiErrorKind, message: impl Into<String>) -> Self {
        Self::Error {
            kind,
            message: message.into(),
        }
    }
}

/// A workspace file attached to a message (M5): an upload the user sent or an
/// artifact a tool produced. Display-only; the model never receives it as a
/// provider field (attachments are named in the message text).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    /// Workspace-relative path with `/` separators.
    pub path: String,
    /// MIME hint from the extension, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_hint: Option<String>,
}

impl Artifact {
    pub fn new(path: impl Into<String>, mime_hint: Option<&str>) -> Self {
        Self {
            path: path.into(),
            mime_hint: mime_hint.map(str::to_owned),
        }
    }
}

impl CoreEvent {
    /// The event form of an [`Artifact`].
    pub fn artifact(artifact: &Artifact) -> Self {
        Self::Artifact {
            path: artifact.path.clone(),
            mime_hint: artifact.mime_hint.clone(),
        }
    }
}

/// What a consent card asks for (ADR-014). Extends as tools arrive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApprovalKind {
    /// Persist a memory entry. Memory survives across sessions, so a silent
    /// write is a prompt-injection vector and always needs consent (M3,
    /// ADR-016).
    MemoryWrite { path: String },
    /// Install Python packages from the configured index \[M5].
    PackageInstall { packages: Vec<String> },
    /// Grant network access inside the sandbox \[M5].
    NetworkAccess { reason: String },
}

/// A user decision on an `ApprovalRequest`. Deny (or timeout) fails closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Deny,
}

/// Tool risk levels; `NeedsApproval` routes through the consent flow and
/// carries what the card asks for (ADR-014).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    Safe,
    NeedsApproval(ApprovalKind),
}

/// Token usage as reported by the provider, normalized for the wire.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
}

impl Usage {
    /// Accumulate another report into this one (M2). A provider that omits a
    /// field counts it as 0 — a single missing report must not erase the
    /// totals accumulated so far.
    pub fn add(&mut self, other: &Self) {
        self.input_tokens = Some(self.input_tokens.unwrap_or(0) + other.input_tokens.unwrap_or(0));
        self.output_tokens =
            Some(self.output_tokens.unwrap_or(0) + other.output_tokens.unwrap_or(0));
        self.total_tokens = Some(self.total_tokens.unwrap_or(0) + other.total_tokens.unwrap_or(0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_round_trip_through_the_wire_format() {
        let events = vec![
            CoreEvent::Delta { text: "Hel".into() },
            CoreEvent::Reasoning {
                text: "thinking…".into(),
            },
            CoreEvent::ToolCall {
                id: "call_1".into(),
                name: "web_search".into(),
                input: serde_json::json!({"query": "kaeru"}),
            },
            CoreEvent::ToolResult {
                id: "call_1".into(),
                name: "web_search".into(),
                output: "[]".into(),
                is_error: false,
            },
            CoreEvent::Artifact {
                path: "plot.png".into(),
                mime_hint: Some("image/png".into()),
            },
            CoreEvent::ApprovalRequest {
                id: "appr_1".into(),
                kind: ApprovalKind::PackageInstall {
                    packages: vec!["pandas".into()],
                },
                summary: "Install pandas from PyPI?".into(),
            },
            CoreEvent::TurnDone {
                usage: Some(Usage {
                    input_tokens: Some(10),
                    output_tokens: Some(5),
                    total_tokens: Some(15),
                }),
            },
            CoreEvent::error(ApiErrorKind::Aborted, "turn aborted"),
        ];

        for event in &events {
            let json = serde_json::to_string(event).unwrap();
            let back: CoreEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, event);
        }
    }

    #[test]
    fn wire_format_uses_snake_case_tags() {
        let json = serde_json::to_value(CoreEvent::Delta { text: "hi".into() }).unwrap();
        assert_eq!(json, serde_json::json!({"type": "delta", "text": "hi"}));

        let json = serde_json::to_value(CoreEvent::TurnDone { usage: None }).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"type": "turn_done", "usage": null})
        );

        let err =
            serde_json::to_value(CoreEvent::error(ApiErrorKind::RateLimited, "slow down")).unwrap();
        assert_eq!(err["type"], "error");
        assert_eq!(err["kind"], "rate_limited");

        let approval = serde_json::to_value(ApprovalKind::NetworkAccess {
            reason: "pip".into(),
        })
        .unwrap();
        assert_eq!(
            approval,
            serde_json::json!({"kind": "network_access", "reason": "pip"})
        );
    }

    #[test]
    fn usage_skips_missing_fields() {
        let json = serde_json::to_string(&Usage::default()).unwrap();
        assert_eq!(json, "{}");
    }

    #[test]
    fn memory_write_approval_kind_round_trips() {
        let kind = ApprovalKind::MemoryWrite {
            path: "data/memory/2026-09-11".into(),
        };
        let json = serde_json::to_value(&kind).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"kind": "memory_write", "path": "data/memory/2026-09-11"})
        );
        let back: ApprovalKind = serde_json::from_value(json).unwrap();
        assert_eq!(back, kind);
    }

    #[test]
    fn risk_carries_the_approval_kind() {
        let risk = Risk::NeedsApproval(ApprovalKind::MemoryWrite { path: "p".into() });
        assert!(matches!(risk, Risk::NeedsApproval(_)));
        assert_eq!(Risk::Safe, Risk::Safe);
    }

    #[tokio::test]
    async fn event_stream_replays_then_follows_live() {
        let (tx, rx) = broadcast::channel(8);
        let mut stream = EventStream::replay(vec![CoreEvent::Delta { text: "a".into() }], rx);
        // Live event queued before the first recv; replay wins first.
        tx.send(CoreEvent::Delta { text: "b".into() }).unwrap();
        assert_eq!(
            stream.recv().await.unwrap(),
            CoreEvent::Delta { text: "a".into() }
        );
        assert_eq!(
            stream.recv().await.unwrap(),
            CoreEvent::Delta { text: "b".into() }
        );
        drop(tx);
        assert!(matches!(
            stream.recv().await,
            Err(broadcast::error::RecvError::Closed)
        ));
    }

    #[tokio::test]
    async fn closed_event_stream_ends_immediately() {
        let mut stream = EventStream::closed();
        assert!(matches!(
            stream.recv().await,
            Err(broadcast::error::RecvError::Closed)
        ));
    }

    #[tokio::test]
    async fn a_lagged_stream_closes_so_the_subscriber_can_replay() {
        use tokio_stream::StreamExt as _;
        let (tx, rx) = broadcast::channel(1);
        let mut stream = EventStream::live(rx);
        for i in 0..3 {
            tx.send(CoreEvent::Delta {
                text: format!("{i}"),
            })
            .unwrap();
        }
        assert!(
            stream.next().await.is_none(),
            "a lagged stream must close instead of silently skipping events"
        );
    }

    #[test]
    fn poll_next_registers_a_waker_the_sender_can_wake() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::task::{Wake, Waker};
        use tokio_stream::Stream as _;

        struct Flag(AtomicBool);
        impl Wake for Flag {
            fn wake(self: Arc<Self>) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let (tx, rx) = broadcast::channel(8);
        let mut stream = EventStream::live(rx);
        let flag = Arc::new(Flag(AtomicBool::new(false)));
        let waker = Waker::from(Arc::clone(&flag));
        let mut cx = Context::from_waker(&waker);

        // Empty channel: the first poll parks the task and must register the
        // broadcast waiter. If `poll_next` dropped the recv future (the old
        // bug), the waker below would never fire.
        assert!(Pin::new(&mut stream).poll_next(&mut cx).is_pending());
        tx.send(CoreEvent::Delta { text: "x".into() }).unwrap();
        assert!(
            flag.0.load(Ordering::SeqCst),
            "sender did not wake the stream"
        );

        match Pin::new(&mut stream).poll_next(&mut cx) {
            Poll::Ready(Some(CoreEvent::Delta { text })) => assert_eq!(text, "x"),
            other => panic!("expected the queued delta, got {other:?}"),
        }
    }
}
