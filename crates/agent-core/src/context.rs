//! Deterministic context/token budget policy (ADR-018, M2).
//!
//! Prompt assembly is a fixed pipeline, never a heuristic:
//!
//! 1. system prompt (the owner-written persona, M4.5)
//! 2. injected memory block (selected durable notes, budgeted, M4)
//! 3. rolling summary (a system message once the budget was exceeded)
//! 4. the recent message window
//!
//! Overflow is resolved *before* the provider call: the oldest turns fall
//! out of the window and are folded into the summary by one bounded LLM
//! sub-call. The turn always answers; a failing summary degrades to a
//! deterministic excerpt, never to a lost turn.

use crate::events::Usage;
use crate::llm::{ChatMessage, ChatRequest, LlmClient, Role};
use crate::util::{CHARS_PER_TOKEN, truncate_chars};

/// Upper bound for stored summaries; a summary may never outgrow the turns
/// it replaces.
const SUMMARY_MAX_CHARS: usize = 4000;

/// Upper bound for the deterministic fallback excerpt.
const FALLBACK_EXCERPT_CHARS: usize = 2000;

/// Budget for the assembled prompt (system + summary + window), in estimated
/// tokens. From `[context] max_prompt_tokens`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextPolicy {
    pub max_prompt_tokens: u64,
}

impl ContextPolicy {
    /// Rough token count of a text.
    pub fn estimate_tokens(text: &str) -> u64 {
        (text.len() as u64).div_ceil(CHARS_PER_TOKEN)
    }
}

/// Split the history into `(window, dropped)` under the budget (minus what
/// the summary and the fixed system blocks (persona, memory) will cost). The
/// trailing message is always kept — even when it alone exceeds the budget —
/// and the window is aligned to start at a `user` message so pairs stay
/// coherent.
pub fn split_window(
    policy: ContextPolicy,
    summary: Option<&str>,
    injected_tokens: u64,
    history: &[ChatMessage],
) -> (Vec<ChatMessage>, Vec<ChatMessage>) {
    let summary_tokens = summary
        .filter(|s| !s.is_empty())
        .map(ContextPolicy::estimate_tokens)
        .unwrap_or(0);
    let budget = policy
        .max_prompt_tokens
        .saturating_sub(summary_tokens + injected_tokens);

    let mut used = 0u64;
    let mut start = history.len();
    while start > 0 {
        let cost = ContextPolicy::estimate_tokens(&history[start - 1].content);
        if start < history.len() && used + cost > budget {
            break;
        }
        used += cost;
        start -= 1;
    }
    // Never an empty window: if even the last message does not fit, it is
    // kept anyway (the turn must go through).
    start = start.min(history.len().saturating_sub(1));
    // Align to a user boundary: drop a leading orphan assistant message.
    while start < history.len() && history[start].role != Role::User {
        start += 1;
    }
    if start >= history.len() && !history.is_empty() {
        start = history.len() - 1; // no user message fits: keep the last one
    }
    (
        history[start.min(history.len())..].to_vec(),
        history[..start.min(history.len())].to_vec(),
    )
}

/// Assemble the provider-bound messages: optional system prompt, the injected
/// memory block, the summary as a system message, then the window. The order is
/// fixed by ADR-018 (system → memory → summary → window).
pub fn assemble(
    system: Option<&str>,
    memory: Option<&str>,
    summary: Option<&str>,
    window: &[ChatMessage],
) -> Vec<ChatMessage> {
    let mut messages = Vec::new();
    if let Some(system) = system.filter(|s| !s.is_empty()) {
        messages.push(ChatMessage::system(system));
    }
    if let Some(memory) = memory.filter(|m| !m.is_empty()) {
        messages.push(ChatMessage::system(memory));
    }
    if let Some(summary) = summary.filter(|s| !s.is_empty()) {
        messages.push(ChatMessage::system(format!(
            "Summary of the earlier part of this conversation (older turns were compacted):\n{summary}"
        )));
    }
    messages.extend(window.iter().cloned());
    messages
}

/// Re-summarize: fold `dropped` (and the previous summary, if any) into a
/// new bounded summary with one LLM sub-call.
///
/// Never fails: on a provider error, empty output, or a timeout-free stream
/// end, the summary degrades to a deterministic excerpt of the dropped turns
/// so the turn still answers. The sub-call's usage is folded into
/// `usage_out` so it is accounted like any other call.
pub async fn summarize(
    client: &dyn LlmClient,
    model: &str,
    previous: Option<&str>,
    dropped: &[ChatMessage],
    usage_out: &mut Usage,
) -> String {
    debug_assert!(!dropped.is_empty());
    match client.chat(summary_request(model, previous, dropped)).await {
        Ok(mut events) => {
            let mut text = String::new();
            while let Some(event) = events.recv().await {
                match event {
                    crate::events::CoreEvent::Delta { text: chunk } => text.push_str(&chunk),
                    crate::events::CoreEvent::TurnDone { usage } => {
                        usage_out.add(&usage.unwrap_or_default());
                        break;
                    }
                    crate::events::CoreEvent::Error { kind, .. } => {
                        tracing::warn!(
                            target: "agent_core::context",
                            "summary sub-call failed ({}): falling back to an excerpt",
                            kind.as_str()
                        );
                        return fallback_summary(previous, dropped);
                    }
                    _ => {}
                }
            }
            let text = text.trim();
            if text.is_empty() {
                tracing::warn!(
                    target: "agent_core::context",
                    "summary sub-call produced no text; falling back to an excerpt"
                );
                fallback_summary(previous, dropped)
            } else {
                truncate_chars(text, SUMMARY_MAX_CHARS)
            }
        }
        Err(err) => {
            tracing::warn!(
                target: "agent_core::context",
                "summary sub-call failed ({}): falling back to an excerpt",
                err.kind.as_str()
            );
            fallback_summary(previous, dropped)
        }
    }
}

/// The summary sub-call's request. Shared by the implementation and tests so
/// cassette fixtures cannot drift from what is really sent.
pub(crate) fn summary_request(
    model: &str,
    previous: Option<&str>,
    dropped: &[ChatMessage],
) -> ChatRequest {
    let mut messages = vec![ChatMessage::system(
        "You maintain the rolling memory of a personal assistant conversation. \
         Summarize the given older conversation turns in at most 150 words. \
         Keep facts, decisions, names and open questions; drop pleasantries. \
         If a previous summary is given, merge it into one continuous summary. \
         Output only the summary text, no preamble.",
    )];
    let mut content = String::new();
    if let Some(previous) = previous.filter(|p| !p.is_empty()) {
        content.push_str("Previous summary:\n");
        content.push_str(previous);
        content.push_str("\n\n");
    }
    content.push_str("Older conversation turns to fold into the summary:");
    for message in dropped {
        content.push_str(&format!("\n[{}] {}", message.role, message.content));
    }
    messages.push(ChatMessage::user(content));
    ChatRequest::new(model, messages)
}

/// Deterministic fallback when the model cannot summarize: keep a bounded
/// excerpt of the dropped turns, clearly labeled, so information is lost
/// progressively instead of all at once.
fn fallback_summary(previous: Option<&str>, dropped: &[ChatMessage]) -> String {
    let mut text = String::from("[older turns compacted; summary unavailable, excerpt follows]");
    if let Some(previous) = previous.filter(|p| !p.is_empty()) {
        text.push_str("\n\nPrevious summary:\n");
        text.push_str(previous);
    }
    let mut excerpt = String::new();
    for message in dropped {
        excerpt.push_str(&format!("\n[{}] {}", message.role, message.content));
    }
    text.push_str("\n\n");
    text.push_str(&truncate_chars(&excerpt, FALLBACK_EXCERPT_CHARS));
    truncate_chars(&text, SUMMARY_MAX_CHARS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(max_prompt_tokens: u64) -> ContextPolicy {
        ContextPolicy { max_prompt_tokens }
    }

    fn turn(user: &str, assistant: &str) -> Vec<ChatMessage> {
        vec![ChatMessage::user(user), ChatMessage::assistant(assistant)]
    }

    #[test]
    fn estimate_is_four_chars_per_token() {
        assert_eq!(ContextPolicy::estimate_tokens(""), 0);
        assert_eq!(ContextPolicy::estimate_tokens("abcd"), 1);
        assert_eq!(ContextPolicy::estimate_tokens("abcde"), 2);
    }

    #[test]
    fn short_history_fits_entirely() {
        let history = turn("hello", "Hello!");
        let (window, dropped) = split_window(policy(100), None, 0, &history);
        assert_eq!(window, history);
        assert!(dropped.is_empty());
    }

    #[test]
    fn overflow_drops_oldest_turns_and_keeps_the_current_message() {
        let mut history = Vec::new();
        for i in 0..10 {
            history.extend(turn(&format!("question {i} "), "a fairly long answer "));
        }
        history.push(ChatMessage::user("the new question"));
        let (window, dropped) = split_window(policy(20), None, 0, &history);
        assert!(
            !dropped.is_empty(),
            "a history this size must overflow a 20-token budget"
        );
        assert_eq!(window.last(), Some(&ChatMessage::user("the new question")));
        assert_eq!(window[0].role, Role::User, "window starts at a user turn");
        assert_eq!(dropped.len() + window.len(), history.len());
        assert_eq!(dropped.last().map(|m| m.role), Some(Role::Assistant));
    }

    #[test]
    fn a_single_huge_message_still_goes_through() {
        let history = vec![ChatMessage::user("a huge message far over any tiny budget")];
        let (window, dropped) = split_window(policy(1), None, 0, &history);
        assert_eq!(window, history);
        assert!(dropped.is_empty());
    }

    #[test]
    fn an_existing_summary_spends_budget() {
        let history = vec![
            ChatMessage::user("hello"),
            ChatMessage::assistant("Hello!"),
            ChatMessage::user("again"),
        ];
        let (window, dropped) = split_window(policy(1), Some("summary text"), 0, &history);
        // The summary alone exceeds the budget: only the trailing turn survives.
        assert_eq!(window, vec![ChatMessage::user("again")]);
        assert_eq!(dropped.len(), 2);
    }

    #[test]
    fn injected_memory_spends_budget_like_the_summary() {
        let history = vec![
            ChatMessage::user("hello"),
            ChatMessage::assistant("Hello!"),
            ChatMessage::user("again"),
        ];
        // 2 tokens of memory is enough to push the first turn out.
        let (window, dropped) = split_window(policy(4), None, 2, &history);
        assert_eq!(window, vec![ChatMessage::user("again")]);
        assert_eq!(dropped.len(), 2);
    }

    #[test]
    fn assemble_places_system_then_memory_then_summary_then_window() {
        let window = vec![ChatMessage::user("hi")];
        let messages = assemble(
            Some("be helpful"),
            Some("Durable memory notes:\n- [2026-09-10] a fact"),
            Some("so far: greetings"),
            &window,
        );
        assert_eq!(
            messages,
            vec![
                ChatMessage::system("be helpful"),
                ChatMessage::system("Durable memory notes:\n- [2026-09-10] a fact"),
                ChatMessage::system(
                    "Summary of the earlier part of this conversation (older turns were compacted):\nso far: greetings"
                ),
                ChatMessage::user("hi"),
            ]
        );
    }

    #[test]
    fn assemble_places_system_then_summary_then_window() {
        let window = vec![ChatMessage::user("hi")];
        let messages = assemble(Some("be helpful"), None, Some("so far: greetings"), &window);
        assert_eq!(
            messages,
            vec![
                ChatMessage::system("be helpful"),
                ChatMessage::system(
                    "Summary of the earlier part of this conversation (older turns were compacted):\nso far: greetings"
                ),
                ChatMessage::user("hi"),
            ]
        );
    }

    #[test]
    fn assemble_skips_empty_pieces() {
        let messages = assemble(None, None, None, &[]);
        assert!(messages.is_empty());
        let messages = assemble(Some(""), Some(""), Some(""), &[]);
        assert!(messages.is_empty());
    }

    #[tokio::test]
    async fn summarize_collects_model_text_and_usage() {
        use crate::events::CoreEvent;
        use crate::llm::{Cassette, FakeProvider, Interaction};

        let dropped = vec![ChatMessage::user("hello")];
        let request = summary_request(crate::config::DEFAULT_MODEL, None, &dropped);
        let cassette = Cassette {
            cassette_version: crate::llm::CASSETTE_VERSION,
            recorded_at_unix: None,
            base_url: None,
            models: vec![],
            interactions: vec![Interaction {
                request,
                events: vec![
                    CoreEvent::Delta {
                        text: "User greeted.".into(),
                    },
                    CoreEvent::TurnDone {
                        usage: Some(Usage {
                            input_tokens: Some(7),
                            output_tokens: Some(3),
                            total_tokens: Some(10),
                        }),
                    },
                ],
            }],
        };
        let fake = FakeProvider::from_cassette(cassette);
        let mut usage = Usage::default();
        let summary = summarize(
            &fake,
            crate::config::DEFAULT_MODEL,
            None,
            &dropped,
            &mut usage,
        )
        .await;
        assert_eq!(summary, "User greeted.");
        assert_eq!(usage.input_tokens, Some(7));
        assert_eq!(usage.output_tokens, Some(3));
    }

    #[tokio::test]
    async fn summarize_falls_back_to_an_excerpt_on_provider_error() {
        use crate::error::ApiErrorKind;
        use crate::events::CoreEvent;
        use crate::llm::{Cassette, FakeProvider, Interaction};

        let dropped = vec![ChatMessage::user("hi")];
        let request = summary_request(crate::config::DEFAULT_MODEL, None, &dropped);
        let cassette = Cassette {
            cassette_version: crate::llm::CASSETTE_VERSION,
            recorded_at_unix: None,
            base_url: None,
            models: vec![],
            interactions: vec![Interaction {
                request,
                events: vec![CoreEvent::error(ApiErrorKind::Provider, "boom")],
            }],
        };
        let fake = FakeProvider::from_cassette(cassette);
        let mut usage = Usage::default();
        let summary = summarize(
            &fake,
            crate::config::DEFAULT_MODEL,
            None,
            &dropped,
            &mut usage,
        )
        .await;
        assert!(summary.contains("summary unavailable"));
        assert!(summary.contains("[user] hi"));
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate_chars("abc", 10), "abc");
        let truncated = truncate_chars("héllo wörld", 5);
        assert!(truncated.starts_with("h"));
        assert!(truncated.ends_with('…'));
        assert!(truncated.chars().count() <= 6);
    }
}
