//! Wire-serializable core event stream (ADR-010/011/015).
//!
//! Every frontend programs against this normalized event vocabulary. The enum
//! is complete from M1 on but variants arrive progressively: M1 uses
//! `Delta`/`TurnDone`/`Error`; `ToolCall`/`ToolResult` and `ApprovalRequest`
//! come with the agent loop (M3), `Artifact` with files (M5).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::ApiErrorKind;

/// A per-conversation subscription of live `CoreEvent`s.
///
/// M1: live tap of the active turn (no replay yet). M3 completes this with a
/// bounded replay buffer so reconnects resume mid-turn (ADR-015).
pub type EventStream = tokio::sync::broadcast::Receiver<CoreEvent>;

/// Normalized agent turn events, shared by all frontends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CoreEvent {
    /// One incremental chunk of assistant text.
    Delta { text: String },
    /// One incremental chunk of model "thinking" (`reasoning_content`), kept
    /// out of the conversation context and shown as a collapsible block.
    Reasoning { text: String },
    /// The model requested a tool call [M3].
    ToolCall {
        id: String,
        name: String,
        input: Value,
    },
    /// A tool finished; `output` is the fenced result the model will see [M3].
    ToolResult {
        id: String,
        name: String,
        output: String,
        is_error: bool,
    },
    /// A workspace file surfaced for display/download [M5].
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

/// What a consent card asks for (ADR-014). Extends as tools arrive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApprovalKind {
    /// Install Python packages from the configured index [M5].
    PackageInstall { packages: Vec<String> },
    /// Grant network access inside the sandbox [M5].
    NetworkAccess { reason: String },
}

/// A user decision on an `ApprovalRequest`. Deny (or timeout) fails closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Deny,
}

/// Tool risk levels; `NeedsApproval` routes through the consent flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    Safe,
    NeedsApproval,
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
}
