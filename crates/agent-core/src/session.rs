//! `ChatSession` + `TurnHandle`: the decoupled turn executor (ADR-015).
//!
//! API shape is final from M1; the wiring is the documented M1 interim:
//! `send` spawns the turn in the background and returns a `TurnHandle` whose
//! event stream is request-scoped — when the last subscriber drops, the turn
//! aborts. M3 replaces that wiring with an executor that survives frontend
//! disconnects and replays a bounded buffer on `subscribe`, without any
//! frontend change.
//!
//! Concurrency model: one active turn per session (`send` while active is
//! `ApiErrorKind::Busy`). All session state lives behind a single
//! `std::sync::Mutex` whose critical sections never await. History-flush
//! races between the turn task and `abort` are resolved by turn-id guard:
//! exactly one of them finalizes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::AgentCore;
use crate::context::{self, ContextPolicy};
use crate::conversations::{
    CONVERSATION_SCHEMA_VERSION, Conversation, ConversationStore, StoredMessage,
};
use crate::error::{ApiError, ApiErrorKind, Result};
use crate::events::{CoreEvent, Decision, EventStream, Usage};
use crate::llm::{ChatMessage, ChatRequest};

/// Broadcast capacity for one turn's events. Generous: a lagged consumer
/// only loses chat text (logged), never correctness.
pub const TURN_EVENT_CAPACITY: usize = 1024;

pub struct ChatSession {
    core: Arc<AgentCore>,
    inner: Arc<Mutex<SessionInner>>,
}

struct SessionInner {
    conversation_id: String,
    history: Vec<ChatMessage>,
    /// Rolling summary of turns that fell out of the context window (M2,
    /// ADR-018); `None` while the conversation fits the budget.
    summary: Option<String>,
    /// Token usage accumulated over all turns incl. summary sub-calls.
    accumulated_usage: Usage,
    /// Human title, derived from the first user message (persisted).
    title: Option<String>,
    /// RFC 3339 creation timestamp (persisted).
    created_at: String,
    model_override: Option<String>,
    next_turn_id: u64,
    active: Option<ActiveTurn>,
    /// Persistence target; `None` for an ephemeral session (M1-style).
    store: Option<ConversationStore>,
}

struct ActiveTurn {
    turn_id: u64,
    join: JoinHandle<()>,
    events: broadcast::Sender<CoreEvent>,
    /// Assistant text streamed so far; read by `abort` to flush a partial
    /// answer into history, since the aborted task cannot.
    partial: Arc<Mutex<String>>,
    cancelled: Arc<AtomicBool>,
}

impl ChatSession {
    /// An ephemeral in-memory session (tests, throwaway use). The frontend
    /// binaries use [`ChatSession::with_store`] instead.
    pub fn new(core: Arc<AgentCore>, conversation_id: impl Into<String>) -> Self {
        Self::build(core, conversation_id.into(), None)
    }

    /// A persisted session: an existing conversation file is loaded into
    /// history/summary/usage on construction, and every finalized or aborted
    /// turn is written back to the store (M2, §6.6 reload/restore).
    /// A broken or future-schema file was already quarantined by the store;
    /// an unreadable one logs a warning and starts empty — never a crash.
    pub fn with_store(
        core: Arc<AgentCore>,
        conversation_id: impl Into<String>,
        store: ConversationStore,
    ) -> Self {
        Self::build(core, conversation_id.into(), Some(store))
    }

    fn build(
        core: Arc<AgentCore>,
        conversation_id: String,
        store: Option<ConversationStore>,
    ) -> Self {
        let mut inner = SessionInner {
            conversation_id: conversation_id.clone(),
            history: Vec::new(),
            summary: None,
            accumulated_usage: Usage::default(),
            title: None,
            created_at: crate::conversations::now_rfc3339(),
            model_override: None,
            next_turn_id: 1,
            active: None,
            store: None,
        };
        if let Some(store) = &store {
            match store.load(&conversation_id) {
                Ok(Some(conversation)) => {
                    inner.history = conversation
                        .messages
                        .iter()
                        .map(StoredMessage::to_chat)
                        .collect();
                    inner.summary = conversation.summary;
                    inner.accumulated_usage = conversation.usage;
                    inner.title = conversation.title;
                    inner.created_at = conversation.created_at;
                }
                Ok(None) => {}
                Err(err) => tracing::warn!(
                    target: "agent_core::session",
                    "cannot load conversation {conversation_id}: {err}; starting empty"
                ),
            }
        }
        inner.store = store;
        Self {
            core,
            inner: Arc::new(Mutex::new(inner)),
        }
    }

    pub fn core(&self) -> &Arc<AgentCore> {
        &self.core
    }

    pub fn conversation_id(&self) -> String {
        self.lock().conversation_id.clone()
    }

    /// Human-readable title (first user message); `None` until the first turn.
    pub fn title(&self) -> Option<String> {
        self.lock().title.clone()
    }

    /// The model used for the next turn: per-conversation override, else the
    /// configured default.
    pub fn current_model(&self) -> String {
        let inner = self.lock();
        inner
            .model_override
            .clone()
            .unwrap_or_else(|| self.core.config().provider.model.clone())
    }

    /// Set or clear (None) the per-conversation model override.
    pub fn set_model(&self, model: Option<String>) {
        self.lock().model_override = model;
    }

    /// Snapshot of the conversation history (the recent window; older turns
    /// live in [`ChatSession::summary`]).
    pub fn history(&self) -> Vec<ChatMessage> {
        self.lock().history.clone()
    }

    /// Rolling summary of turns compacted out of the context window (M2,
    /// ADR-018); `None` while the whole conversation fits the budget.
    pub fn summary(&self) -> Option<String> {
        self.lock().summary.clone()
    }

    /// Token usage accumulated over all turns, including summary sub-calls.
    /// A provider that omits usage on some turns counts those as 0.
    pub fn total_usage(&self) -> Usage {
        self.lock().accumulated_usage
    }

    pub fn is_active(&self) -> bool {
        self.lock().active.is_some()
    }

    /// Send a user message and start a turn.
    ///
    /// Returns immediately with a handle to the running turn. Fails with
    /// `Busy` while another turn is active, and `Config` on empty input.
    pub fn send(&self, message: &str) -> Result<TurnHandle> {
        if message.trim().is_empty() {
            return Err(ApiError::config("message must not be empty"));
        }
        let mut inner = self.lock();
        if inner.active.is_some() {
            return Err(ApiError::new(
                ApiErrorKind::Busy,
                "a turn is already in progress; abort it first",
            ));
        }

        let model = inner
            .model_override
            .clone()
            .unwrap_or_else(|| self.core.config().provider.model.clone());
        inner.history.push(ChatMessage::user(message));
        if inner.title.is_none() {
            inner.title = Some(derive_title(message));
        }
        // The turn task assembles the prompt under the deterministic context
        // budget (ADR-018); with an empty drop set this is exactly `history`.
        let history = inner.history.clone();
        let summary = inner.summary.clone();
        let policy = ContextPolicy {
            max_prompt_tokens: self.core.config().context.max_prompt_tokens,
        };

        let turn_id = inner.next_turn_id;
        inner.next_turn_id += 1;
        let (events, rx) = broadcast::channel(TURN_EVENT_CAPACITY);
        let partial = Arc::new(Mutex::new(String::new()));
        let cancelled = Arc::new(AtomicBool::new(false));
        let join = tokio::spawn(run_turn(TurnTask {
            core: Arc::clone(&self.core),
            session: Arc::clone(&self.inner),
            turn_id,
            events: events.clone(),
            model,
            history,
            summary,
            policy,
            partial: Arc::clone(&partial),
            cancelled: Arc::clone(&cancelled),
        }));
        inner.active = Some(ActiveTurn {
            turn_id,
            join,
            events: events.clone(),
            partial,
            cancelled,
        });
        Ok(TurnHandle {
            turn_id,
            rx,
            events,
            session: Arc::clone(&self.inner),
        })
    }

    /// Live tap into the active turn's events (no replay until M3).
    ///
    /// With no active turn this is an already-closed stream.
    pub fn subscribe(&self) -> EventStream {
        let inner = self.lock();
        match &inner.active {
            Some(active) => active.events.subscribe(),
            None => broadcast::channel(1).1,
        }
    }

    /// Abort the active turn (no-op when idle). The aborted stream receives
    /// one terminal `Error { kind: Aborted }` and closes; the partial answer
    /// is kept in history.
    pub fn abort(&self) -> Result<bool> {
        abort_turn(&self.inner, None)
    }

    /// Record a consent decision. Staged API: consent lands with tools in M3.
    pub async fn approve(&self, _request_id: &str, _decision: Decision) -> Result<()> {
        Err(ApiError::internal(
            "consent flow is not implemented yet; it arrives with the agent loop in M3",
        ))
    }

    fn lock(&self) -> MutexGuard<'_, SessionInner> {
        self.inner.lock().expect("session lock poisoned")
    }
}

impl SessionInner {
    fn to_conversation(&self) -> Conversation {
        Conversation {
            schema: CONVERSATION_SCHEMA_VERSION,
            id: self.conversation_id.clone(),
            title: self.title.clone(),
            created_at: self.created_at.clone(),
            summary: self.summary.clone(),
            messages: self.history.iter().map(StoredMessage::from).collect(),
            usage: self.accumulated_usage,
        }
    }

    /// Write the conversation back to the store (when there is one). Never
    /// fails a turn: persistence errors are logged and the state stays in
    /// memory (quality goal: completed turns are not lost *by the store*).
    fn persist(&self) {
        let Some(store) = &self.store else {
            return;
        };
        if let Err(err) = store.save(&self.to_conversation()) {
            tracing::warn!(
                target: "agent_core::session",
                "cannot persist conversation {}: {err} (kept in memory)",
                self.conversation_id
            );
        }
    }
}

/// Title for a conversation, from its first user message.
fn derive_title(message: &str) -> String {
    let first_line = message.lines().next().unwrap_or("").trim();
    let mut title: String = first_line.chars().take(60).collect();
    if first_line.chars().count() > 60 {
        title.push('…');
    }
    if title.is_empty() {
        title = "untitled".into();
    }
    title
}

/// Per-turn handle: the request-scoped event stream plus turn-scoped abort.
///
/// The handle owns the turn's primordial receiver from the moment `send`
/// returns (no lost-events window), plus the sender for extra live taps.
/// `into_events()` replays from the turn's start; `events()` taps from now.
pub struct TurnHandle {
    turn_id: u64,
    rx: broadcast::Receiver<CoreEvent>,
    events: broadcast::Sender<CoreEvent>,
    session: Arc<Mutex<SessionInner>>,
}

impl std::fmt::Debug for TurnHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnHandle")
            .field("turn_id", &self.turn_id)
            .finish()
    }
}

impl TurnHandle {
    pub fn turn_id(&self) -> u64 {
        self.turn_id
    }

    /// A fresh live subscription to this turn's events (from now on).
    pub fn events(&self) -> EventStream {
        self.events.subscribe()
    }

    /// Consume the handle into the turn's event stream (from the turn start).
    pub fn into_events(self) -> EventStream {
        self.rx
    }

    /// Abort this turn (no-op if it already finished or was superseded).
    pub fn abort(&self) -> Result<bool> {
        abort_turn(&self.session, Some(self.turn_id))
    }
}

fn abort_turn(inner: &Arc<Mutex<SessionInner>>, only: Option<u64>) -> Result<bool> {
    let mut inner = inner.lock().expect("session lock poisoned");
    let is_target = |active: &ActiveTurn| only.is_none_or(|id| active.turn_id == id);
    if !inner.active.as_ref().is_some_and(is_target) {
        return Ok(false);
    }
    let active = inner.active.take().expect("checked above");
    // Order matters: set the flag before aborting so the task's finalizer sees
    // it and never races a duplicate history flush past the id guard.
    active.cancelled.store(true, Ordering::SeqCst);
    active.join.abort();
    let partial = active
        .partial
        .lock()
        .expect("partial lock poisoned")
        .clone();
    if !partial.is_empty() {
        inner.history.push(ChatMessage::assistant(partial));
    }
    let _ = active
        .events
        .send(CoreEvent::error(ApiErrorKind::Aborted, "turn aborted"));
    inner.persist();
    Ok(true)
}

struct TurnTask {
    core: Arc<AgentCore>,
    session: Arc<Mutex<SessionInner>>,
    turn_id: u64,
    events: broadcast::Sender<CoreEvent>,
    model: String,
    history: Vec<ChatMessage>,
    summary: Option<String>,
    policy: ContextPolicy,
    partial: Arc<Mutex<String>>,
    cancelled: Arc<AtomicBool>,
}

async fn run_turn(task: TurnTask) {
    let TurnTask {
        core,
        session,
        turn_id,
        events,
        model,
        history,
        summary,
        policy,
        partial,
        cancelled,
    } = task;
    let mut outcome = TurnOutcome::Completed;
    let mut turn_usage: Option<Usage> = None;

    // Deterministic context assembly (ADR-018): resolve overflow *before*
    // the provider call — drop-oldest, then fold the dropped turns into the
    // rolling summary with one bounded LLM sub-call. A failing summary
    // degrades to a deterministic excerpt, never to a lost turn.
    let (window, dropped) = context::split_window(policy, summary.as_deref(), &history);
    let summary = if dropped.is_empty() {
        summary
    } else {
        let mut summary_usage = Usage::default();
        let new_summary = context::summarize(
            core.client(),
            &model,
            summary.as_deref(),
            &dropped,
            &mut summary_usage,
        )
        .await;
        if cancelled.load(Ordering::SeqCst) {
            return; // aborted from outside; abort_turn owns the flush
        }
        let mut inner = session.lock().expect("session lock poisoned");
        inner.accumulated_usage.add(&summary_usage);
        inner.summary = Some(new_summary.clone());
        inner.history = window.clone();
        drop(inner);
        tracing::info!(
            target: "agent_core::session",
            conversation_dropped = dropped.len(),
            "compacted older turns into the rolling summary"
        );
        Some(new_summary)
    };

    let request = ChatRequest::new(model, context::assemble(None, summary.as_deref(), &window));

    let mut client_rx = match core.client().chat(request).await {
        Ok(rx) => rx,
        Err(err) => {
            outcome = TurnOutcome::Failed;
            let _ = events.send(CoreEvent::error(err.kind, err.message));
            finalize_turn(&session, turn_id, &partial, outcome, turn_usage.as_ref());
            return;
        }
    };

    while let Some(event) = client_rx.recv().await {
        if cancelled.load(Ordering::SeqCst) {
            // Aborted from outside: abort_turn flushed history and emitted the
            // terminal event already.
            return;
        }
        match event {
            CoreEvent::Delta { text } => {
                partial
                    .lock()
                    .expect("partial lock poisoned")
                    .push_str(&text);
                if events.send(CoreEvent::Delta { text }).is_err() {
                    outcome = TurnOutcome::Disconnected;
                    break;
                }
            }
            CoreEvent::TurnDone { usage } => {
                let _ = events.send(CoreEvent::TurnDone { usage });
                turn_usage = usage;
                break;
            }
            CoreEvent::Error { kind, message } => {
                outcome = TurnOutcome::Failed;
                let _ = events.send(CoreEvent::error(kind, message));
                break;
            }
            other => {
                let _ = events.send(other); // future variants pass through
            }
        }
    }

    finalize_turn(&session, turn_id, &partial, outcome, turn_usage.as_ref());
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnOutcome {
    /// Provider finished (or the client stream ended without a terminal event
    /// — tolerated as completion).
    Completed,
    /// Provider failed before finishing.
    Failed,
    /// Every frontend subscriber disappeared; M1 wiring aborts the turn.
    Disconnected,
}

fn finalize_turn(
    session: &Arc<Mutex<SessionInner>>,
    turn_id: u64,
    partial: &Arc<Mutex<String>>,
    outcome: TurnOutcome,
    usage: Option<&Usage>,
) {
    let partial = partial.lock().expect("partial lock poisoned").clone();
    let mut inner = session.lock().expect("session lock poisoned");
    let still_current = inner.active.as_ref().is_some_and(|a| a.turn_id == turn_id);
    if !still_current {
        return; // aborted from outside; abort_turn owns the flush
    }
    inner.active = None;
    if let Some(usage) = usage {
        inner.accumulated_usage.add(usage);
    }

    if !partial.is_empty() {
        inner.history.push(ChatMessage::assistant(partial));
    } else if outcome == TurnOutcome::Failed {
        // The model produced nothing before failing: drop the trailing user
        // message so a retry resends cleanly instead of duplicating it.
        if inner
            .history
            .last()
            .is_some_and(|m| m.role == crate::llm::Role::User)
        {
            inner.history.pop();
        }
    }
    // Persist after every finalized turn ("serialize on finalize", M2).
    inner.persist();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{ApprovalKind, Usage};
    use crate::llm::{Cassette, FakeProvider, Interaction};
    use std::time::Duration;

    fn core_with(fake: FakeProvider) -> Arc<AgentCore> {
        Arc::new(AgentCore::new(
            crate::config::Config::default(),
            std::sync::Arc::new(fake),
        ))
    }

    fn cassette_for(request: &ChatRequest, events: Vec<CoreEvent>) -> Cassette {
        Cassette {
            cassette_version: 1,
            recorded_at_unix: None,
            base_url: None,
            models: vec![],
            interactions: vec![Interaction {
                request: request.clone(),
                events,
            }],
        }
    }

    async fn drain(mut rx: EventStream) -> Vec<CoreEvent> {
        let mut events = Vec::new();
        loop {
            match rx.recv().await {
                Ok(event) => events.push(event),
                Err(broadcast::error::RecvError::Closed) => return events,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    panic!("unexpected lag of {n} events in a unit test stream")
                }
            }
        }
    }

    fn quick_turn(events: Vec<CoreEvent>) -> (Arc<AgentCore>, ChatRequest) {
        let request = ChatRequest::new(
            crate::config::DEFAULT_MODEL,
            vec![ChatMessage::user("hello")],
        );
        (
            core_with(FakeProvider::from_cassette(cassette_for(&request, events))),
            request,
        )
    }

    #[tokio::test]
    async fn happy_path_streams_and_records_history() {
        let (core, request) = quick_turn(vec![
            CoreEvent::Delta { text: "Hel".into() },
            CoreEvent::Delta { text: "lo".into() },
            CoreEvent::TurnDone {
                usage: Some(Usage {
                    input_tokens: Some(1),
                    output_tokens: Some(2),
                    total_tokens: None,
                }),
            },
        ]);
        let session = ChatSession::new(core, "test");

        let handle = session.send("hello").unwrap();
        let events = drain(handle.into_events()).await;
        assert_eq!(
            events,
            vec![
                CoreEvent::Delta { text: "Hel".into() },
                CoreEvent::Delta { text: "lo".into() },
                CoreEvent::TurnDone {
                    usage: Some(Usage {
                        input_tokens: Some(1),
                        output_tokens: Some(2),
                        total_tokens: None
                    })
                },
            ]
        );

        let history = session.history();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0], ChatMessage::user("hello"));
        assert_eq!(history[1], ChatMessage::assistant("Hello"));
        assert!(!session.is_active());
        // The request the provider saw must contain exactly this history.
        assert_eq!(request.messages.len(), 1);
    }

    #[tokio::test]
    async fn second_turn_request_carries_conversation_history() {
        let first = ChatRequest::new(
            crate::config::DEFAULT_MODEL,
            vec![ChatMessage::user("hello")],
        );
        let second = ChatRequest::new(
            crate::config::DEFAULT_MODEL,
            vec![
                ChatMessage::user("hello"),
                ChatMessage::assistant("Hello"),
                ChatMessage::user("again"),
            ],
        );
        let cassette = Cassette {
            interactions: vec![
                Interaction {
                    request: first,
                    events: vec![
                        CoreEvent::Delta {
                            text: "Hello".into(),
                        },
                        CoreEvent::TurnDone { usage: None },
                    ],
                },
                Interaction {
                    request: second,
                    events: vec![
                        CoreEvent::Delta {
                            text: "Again".into(),
                        },
                        CoreEvent::TurnDone { usage: None },
                    ],
                },
            ],
            ..cassette_for(&ChatRequest::new("", vec![]), vec![])
        };
        let session = ChatSession::new(core_with(FakeProvider::from_cassette(cassette)), "test");

        let h1 = session.send("hello").unwrap();
        drain(h1.into_events()).await;
        let h2 = session.send("again").unwrap();
        let events = drain(h2.into_events()).await;
        assert_eq!(
            events,
            vec![
                CoreEvent::Delta {
                    text: "Again".into()
                },
                CoreEvent::TurnDone { usage: None },
            ]
        );
        assert_eq!(session.history().len(), 4);
    }

    #[tokio::test]
    async fn busy_turn_is_rejected() {
        let request = ChatRequest::new(
            crate::config::DEFAULT_MODEL,
            vec![ChatMessage::user("hello")],
        );
        let session = ChatSession::new(
            core_with(
                FakeProvider::from_cassette(cassette_for(
                    &request,
                    vec![
                        CoreEvent::Delta { text: "s".into() },
                        CoreEvent::TurnDone { usage: None },
                    ],
                ))
                .with_delay(Duration::from_millis(30)),
            ),
            "test",
        );

        let handle = session.send("hello").unwrap();
        let err = session.send("hello").unwrap_err();
        assert_eq!(err.kind, ApiErrorKind::Busy);
        drain(handle.into_events()).await;
        assert!(!session.is_active());
    }

    #[tokio::test]
    async fn abort_stops_the_turn_and_keeps_partial_text() {
        let request = ChatRequest::new(
            crate::config::DEFAULT_MODEL,
            vec![ChatMessage::user("hello")],
        );
        let mut events = Vec::new();
        for i in 0..20 {
            events.push(CoreEvent::Delta {
                text: format!("chunk{i} "),
            });
        }
        events.push(CoreEvent::TurnDone { usage: None });
        let fake = FakeProvider::from_cassette(cassette_for(&request, events))
            .with_delay(Duration::from_millis(25));
        let session = ChatSession::new(core_with(fake), "test");

        let handle = session.send("hello").unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(session.abort().unwrap());
        let received = drain(handle.into_events()).await;

        assert!(received.iter().any(|e| matches!(
            e,
            CoreEvent::Error {
                kind: ApiErrorKind::Aborted,
                ..
            }
        )));
        let history = session.history();
        assert_eq!(history[0], ChatMessage::user("hello"));
        let partial = history[1].content.clone();
        assert!(
            partial.contains("chunk0"),
            "partial answer must be kept, got: {partial:?}"
        );
        assert!(
            partial.len() < "chunk0 ".len() * 20,
            "partial must be truncated, got: {partial:?}"
        );
        assert!(!session.is_active());
    }

    #[tokio::test]
    async fn provider_failure_with_no_output_pops_the_user_message_for_a_clean_retry() {
        let request = ChatRequest::new(
            crate::config::DEFAULT_MODEL,
            vec![ChatMessage::user("hello")],
        );
        let session = ChatSession::new(
            core_with(FakeProvider::from_cassette(cassette_for(
                &request,
                vec![CoreEvent::error(ApiErrorKind::Unauthorized, "bad key")],
            ))),
            "test",
        );

        let handle = session.send("hello").unwrap();
        let received = drain(handle.into_events()).await;
        assert!(matches!(
            received[0],
            CoreEvent::Error {
                kind: ApiErrorKind::Unauthorized,
                ..
            }
        ));
        assert!(
            session.history().is_empty(),
            "failed turn with no output must leave history clean"
        );
    }

    #[tokio::test]
    async fn provider_failure_after_partial_output_keeps_the_partial() {
        let request = ChatRequest::new(
            crate::config::DEFAULT_MODEL,
            vec![ChatMessage::user("hello")],
        );
        let session = ChatSession::new(
            core_with(FakeProvider::from_cassette(cassette_for(
                &request,
                vec![
                    CoreEvent::Delta {
                        text: "partial answer".into(),
                    },
                    CoreEvent::error(ApiErrorKind::Provider, "provider exploded mid-stream"),
                ],
            ))),
            "test",
        );

        let handle = session.send("hello").unwrap();
        drain(handle.into_events()).await;
        let history = session.history();
        assert_eq!(history.len(), 2);
        assert_eq!(history[1], ChatMessage::assistant("partial answer"));
    }

    #[tokio::test]
    async fn dropped_subscriber_aborts_the_turn_with_partial_kept() {
        let request = ChatRequest::new(
            crate::config::DEFAULT_MODEL,
            vec![ChatMessage::user("hello")],
        );
        let mut events = Vec::new();
        for i in 0..10 {
            events.push(CoreEvent::Delta {
                text: format!("d{i} "),
            });
        }
        events.push(CoreEvent::TurnDone { usage: None });
        let fake = FakeProvider::from_cassette(cassette_for(&request, events))
            .with_delay(Duration::from_millis(20));
        let session = ChatSession::new(core_with(fake), "test");

        let handle = session.send("hello").unwrap();
        drop(handle); // frontend disconnect: M1 wiring aborts the turn

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if session.history().len() == 2 || tokio::time::Instant::now() > deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let history = session.history();
        assert_eq!(
            history.len(),
            2,
            "user message + partial answer must be kept"
        );
        assert!(history[1].content.contains("d0"));
        assert!(!session.is_active());
    }

    #[tokio::test]
    async fn subscribe_taps_the_active_turn_and_closes_after_it_ends() {
        let request = ChatRequest::new(
            crate::config::DEFAULT_MODEL,
            vec![ChatMessage::user("hello")],
        );
        let fake = FakeProvider::from_cassette(cassette_for(
            &request,
            vec![
                CoreEvent::Delta { text: "x".into() },
                CoreEvent::TurnDone { usage: None },
            ],
        ))
        .with_delay(Duration::from_millis(40));
        let session = ChatSession::new(core_with(fake), "test");

        let handle = session.send("hello").unwrap();
        let mut tap = session.subscribe();
        let tapped = tokio::time::timeout(Duration::from_secs(2), tap.recv()).await;
        assert!(tapped.is_ok(), "live subscriber must receive events");
        drain(handle.into_events()).await;

        let mut idle = session.subscribe();
        match idle.recv().await {
            Err(broadcast::error::RecvError::Closed) => {}
            other => panic!("idle subscribe must be a closed stream, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn model_override_reaches_the_provider_request() {
        let request = ChatRequest::new("custom/model", vec![ChatMessage::user("hello")]);
        let session = ChatSession::new(
            core_with(FakeProvider::from_cassette(cassette_for(
                &request,
                vec![CoreEvent::TurnDone { usage: None }],
            ))),
            "test",
        );
        session.set_model(Some("custom/model".into()));
        assert_eq!(session.current_model(), "custom/model");
        let handle = session.send("hello").unwrap();
        drain(handle.into_events()).await;
        session.set_model(None);
        assert_eq!(session.current_model(), crate::config::DEFAULT_MODEL);
    }

    #[test]
    fn empty_message_is_rejected() {
        let session = ChatSession::new(core_with(FakeProvider::builtin()), "test");
        let err = session.send("   ").unwrap_err();
        assert_eq!(err.kind, ApiErrorKind::Config);
    }

    #[tokio::test]
    async fn approve_is_staged_for_m3() {
        let session = ChatSession::new(core_with(FakeProvider::builtin()), "test");
        let err = session
            .approve("appr_1", Decision::Allow)
            .await
            .unwrap_err();
        assert_eq!(err.kind, ApiErrorKind::Internal);
        let _ = ApprovalKind::PackageInstall { packages: vec![] }; // types present from M1
    }

    #[test]
    fn turn_handle_abort_is_id_guarded() {
        let session = ChatSession::new(core_with(FakeProvider::builtin()), "test");
        // Idle session: aborting a stale handle is a no-op.
        let fake_handle_session = Arc::new(Mutex::new(SessionInner {
            conversation_id: "test".into(),
            history: vec![],
            summary: None,
            accumulated_usage: Usage::default(),
            title: None,
            created_at: "1970-01-01T00:00:00Z".into(),
            model_override: None,
            next_turn_id: 1,
            active: None,
            store: None,
        }));
        assert!(!abort_turn(&fake_handle_session, Some(99)).unwrap());
        assert!(!session.abort().unwrap());
    }

    #[tokio::test]
    async fn over_budget_turn_compacts_oldest_turns_into_a_summary_and_answers() {
        // Budget 4 estimated tokens: the ("hello", "Hello!") pair falls out of
        // the window together (user-boundary alignment); only the new
        // message stays and the pair is folded into the rolling summary.
        let mut config = crate::config::Config::default();
        config.context.max_prompt_tokens = 4;
        let model = crate::config::DEFAULT_MODEL;

        let first_request = ChatRequest::new(model, vec![ChatMessage::user("hello")]);
        let dropped = vec![ChatMessage::user("hello"), ChatMessage::assistant("Hello!")];
        let summary_request = crate::context::summary_request(model, None, &dropped);
        let second_window = vec![ChatMessage::user("again")];
        let second_request = ChatRequest::new(
            model,
            crate::context::assemble(None, Some("compact summary"), &second_window),
        );

        let cassette = Cassette {
            interactions: vec![
                Interaction {
                    request: first_request,
                    events: vec![
                        CoreEvent::Delta {
                            text: "Hello!".into(),
                        },
                        CoreEvent::TurnDone {
                            usage: Some(Usage {
                                input_tokens: Some(1),
                                output_tokens: Some(2),
                                total_tokens: None,
                            }),
                        },
                    ],
                },
                Interaction {
                    request: summary_request,
                    events: vec![
                        CoreEvent::Delta {
                            text: "compact summary".into(),
                        },
                        CoreEvent::TurnDone {
                            usage: Some(Usage {
                                input_tokens: Some(10),
                                output_tokens: Some(5),
                                total_tokens: None,
                            }),
                        },
                    ],
                },
                Interaction {
                    request: second_request,
                    events: vec![
                        CoreEvent::Delta {
                            text: "Answer 2".into(),
                        },
                        CoreEvent::TurnDone {
                            usage: Some(Usage {
                                input_tokens: Some(3),
                                output_tokens: Some(4),
                                total_tokens: None,
                            }),
                        },
                    ],
                },
            ],
            ..cassette_for(&ChatRequest::new("", vec![]), vec![])
        };
        let session = ChatSession::new(
            Arc::new(AgentCore::new(
                config,
                std::sync::Arc::new(FakeProvider::from_cassette(cassette)),
            )),
            "test",
        );

        assert_eq!(session.summary(), None);
        let h1 = session.send("hello").unwrap();
        let received = drain(h1.into_events()).await;
        assert!(
            received
                .last()
                .is_some_and(|e| matches!(e, CoreEvent::TurnDone { .. }))
        );
        assert_eq!(session.total_usage().input_tokens, Some(1));

        let h2 = session.send("again").unwrap();
        let received = drain(h2.into_events()).await;
        // The over-budget turn still answers (Appendix C, M2).
        assert!(received.contains(&CoreEvent::Delta {
            text: "Answer 2".into()
        }));
        assert!(
            received
                .iter()
                .any(|e| matches!(e, CoreEvent::TurnDone { .. }))
        );

        // Drop-oldest + summary: history is truncated, summary is stored.
        assert_eq!(session.summary(), Some("compact summary".into()));
        assert_eq!(
            session.history(),
            vec![
                ChatMessage::user("again"),
                ChatMessage::assistant("Answer 2"),
            ]
        );
        // Usage accounting covers the main turns and the summary sub-call.
        assert_eq!(session.total_usage().input_tokens, Some(1 + 10 + 3));
        assert_eq!(session.total_usage().output_tokens, Some(2 + 5 + 4));
    }

    #[tokio::test]
    async fn persisted_conversation_reloads_after_restart() {
        let dir = std::env::temp_dir().join(format!("kaeru-test-{}-persist", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let store = ConversationStore::new(&dir);

        let request = ChatRequest::new(
            crate::config::DEFAULT_MODEL,
            vec![ChatMessage::user(
                "a longer first question, good for a title",
            )],
        );
        let cassette = cassette_for(
            &request,
            vec![
                CoreEvent::Delta {
                    text: "Hello!".into(),
                },
                CoreEvent::TurnDone {
                    usage: Some(Usage {
                        input_tokens: Some(4),
                        output_tokens: Some(6),
                        total_tokens: Some(10),
                    }),
                },
            ],
        );

        // "Process 1": chat, then shut down.
        {
            let session = ChatSession::with_store(
                core_with(FakeProvider::from_cassette(cassette.clone())),
                "default",
                store.clone(),
            );
            assert!(session.history().is_empty());
            let handle = session
                .send("a longer first question, good for a title")
                .unwrap();
            drain(handle.into_events()).await;
            assert_eq!(session.history().len(), 2);
        }

        // "Process 2": same data dir — reload restores history, usage, title.
        let session = ChatSession::with_store(
            core_with(FakeProvider::from_cassette(cassette)),
            "default",
            store,
        );
        assert_eq!(
            session.conversation_id(),
            "default",
            "conversation id must survive restarts"
        );
        let history = session.history();
        assert_eq!(history.len(), 2);
        assert_eq!(
            history[0],
            ChatMessage::user("a longer first question, good for a title")
        );
        assert_eq!(history[1], ChatMessage::assistant("Hello!"));
        assert_eq!(
            session.title().as_deref(),
            Some("a longer first question, good for a title")
        );
        assert_eq!(session.total_usage().input_tokens, Some(4));
        assert_eq!(session.total_usage().output_tokens, Some(6));

        // The file on disk carries the planned schema (Appendix C: schema: 1).
        let text = std::fs::read_to_string(dir.join("default.json")).unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["schema"], 1);
        assert_eq!(json["messages"].as_array().unwrap().len(), 2);
        assert_eq!(json["usage"]["total_tokens"], 10);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn an_unreadable_conversation_file_starts_empty_not_broken() {
        let dir = std::env::temp_dir().join(format!("kaeru-test-{}-broken", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("default.json"), "}{ broken").unwrap();

        let session = ChatSession::with_store(
            core_with(FakeProvider::builtin()),
            "default",
            ConversationStore::new(&dir),
        );
        assert!(session.history().is_empty());
        assert_eq!(session.conversation_id(), "default");
        assert!(dir.join("default.json.quarantine").is_file());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn titles_come_from_the_first_user_message() {
        assert_eq!(derive_title("hello"), "hello");
        assert_eq!(derive_title("first line\nsecond line"), "first line");
        let long = "x".repeat(100);
        let title = derive_title(&long);
        assert_eq!(title.chars().count(), 61);
        assert!(title.ends_with('…'));
        assert_eq!(derive_title("   "), "untitled");
    }
}
