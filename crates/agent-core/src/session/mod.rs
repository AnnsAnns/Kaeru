//! `ChatSession`: the decoupled turn executor (M3, ADR-015). A turn runs in a
//! background task that survives frontend disconnects; events are buffered
//! (bounded) for replay and broadcast live. One active turn per session; all
//! state sits behind one `Mutex` whose critical sections never await, and the
//! turn-id guard flushes history exactly once between turn task and `abort`.

mod approvals;
mod registry;
#[cfg(test)]
mod tests;
mod turn;

pub use registry::{ConversationRegistry, ThreadSummary};
pub use turn::TurnHandle;

use std::collections::VecDeque;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::AgentCore;
use crate::agent::Emitter;
use crate::context::ContextPolicy;
use crate::conversations::{
    CONVERSATION_SCHEMA_VERSION, Conversation, ConversationStore, StoredMessage,
};
use crate::error::{ApiError, ApiErrorKind, Result};
use crate::events::{Artifact, CoreEvent, Decision, EventStream, Usage};
use crate::llm::ChatMessage;

use approvals::{APPROVAL_TIMEOUT, SessionApprovals};
use turn::{TurnTask, abort_turn, run_turn};

/// Broadcast capacity for one turn's events. Generous: a lagged consumer
/// only loses chat text (logged), never correctness.
const TURN_EVENT_CAPACITY: usize = 1024;

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
    /// RFC 3339 last-write timestamp (persisted); re-stamped by the store on
    /// every save (M2.5). Kept here so in-memory state round-trips honestly.
    updated_at: String,
    model_override: Option<String>,
    /// Per-conversation reasoning effort override (models that support it).
    reasoning_effort_override: Option<String>,
    next_turn_id: u64,
    active: Option<ActiveTurn>,
    /// Persistence target; `None` for an ephemeral session (M1-style).
    store: Option<ConversationStore>,
}

struct ActiveTurn {
    turn_id: u64,
    join: JoinHandle<()>,
    events: broadcast::Sender<CoreEvent>,
    /// Bounded replay buffer for reconnects (M3, ADR-015).
    buffer: Arc<Mutex<VecDeque<CoreEvent>>>,
    /// Assistant text streamed so far; read by `abort` to flush a partial
    /// answer into history, since the aborted task cannot.
    partial: Arc<Mutex<String>>,
    /// Model thinking streamed so far (display-only); flushed like `partial`.
    reasoning: Arc<Mutex<String>>,
    cancelled: Arc<AtomicBool>,
    /// Consent seam for tools that need approval (M3).
    approvals: Arc<SessionApprovals>,
    /// Workspace files the turn's tools produced (M5); flushed onto the
    /// final answer (or the aborted partial) so a reload can render them.
    artifacts: Arc<Mutex<Vec<Artifact>>>,
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
            updated_at: crate::conversations::now_rfc3339(),
            model_override: None,
            reasoning_effort_override: None,
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
                    if !conversation.updated_at.is_empty() {
                        inner.updated_at = conversation.updated_at;
                    }
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

    /// The reasoning effort used for the next turn: per-conversation override,
    /// else the configured default, else the provider's own default.
    pub fn current_reasoning_effort(&self) -> Option<String> {
        let inner = self.lock();
        inner.reasoning_effort_override.clone().or_else(|| {
            self.core
                .config()
                .provider
                .reasoning_effort()
                .map(str::to_owned)
        })
    }

    /// Set or clear (None) the per-conversation reasoning effort override.
    pub fn set_reasoning_effort(&self, effort: Option<String>) {
        self.lock().reasoning_effort_override = effort;
    }

    /// Set the conversation title (M2.5: used when a thread is created with
    /// one). A later first user message no longer overrides it.
    pub fn set_title(&self, title: Option<String>) {
        self.lock().title = title;
    }

    /// Force a write of the current conversation to the store; a no-op for an
    /// ephemeral session. Used by the registry so a freshly created empty
    /// thread shows up in listings immediately.
    pub fn persist(&self) {
        self.lock().persist();
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
        self.send_with_attachments(message, Vec::new())
    }

    /// Like [`ChatSession::send`], with workspace files attached to the user
    /// message (M5 uploads). The paths should already be validated against the
    /// workspace; they are stored for the UI only (the model sees them through
    /// the message text).
    pub fn send_with_attachments(
        &self,
        message: &str,
        attachments: Vec<Artifact>,
    ) -> Result<TurnHandle> {
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
        let reasoning_effort = inner.reasoning_effort_override.clone().or_else(|| {
            self.core
                .config()
                .provider
                .reasoning_effort()
                .map(str::to_owned)
        });
        inner
            .history
            .push(ChatMessage::user(message).with_artifacts(attachments));
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
        let buffer = Arc::new(Mutex::new(VecDeque::new()));
        let emitter = Emitter::new(events.clone(), Arc::clone(&buffer));
        let partial = Arc::new(Mutex::new(String::new()));
        let reasoning = Arc::new(Mutex::new(String::new()));
        let cancelled = Arc::new(AtomicBool::new(false));
        let approvals = Arc::new(SessionApprovals::new(emitter.clone(), APPROVAL_TIMEOUT));
        let artifacts = Arc::new(Mutex::new(Vec::new()));
        let join = tokio::spawn(run_turn(TurnTask {
            core: Arc::clone(&self.core),
            session: Arc::clone(&self.inner),
            turn_id,
            emitter: emitter.clone(),
            model,
            reasoning_effort,
            history,
            summary,
            policy,
            partial: Arc::clone(&partial),
            reasoning: Arc::clone(&reasoning),
            cancelled: Arc::clone(&cancelled),
            approvals: Arc::clone(&approvals),
            artifacts: Arc::clone(&artifacts),
        }));
        inner.active = Some(ActiveTurn {
            turn_id,
            join,
            events: events.clone(),
            buffer,
            partial,
            reasoning,
            cancelled,
            approvals,
            artifacts,
        });
        Ok(TurnHandle {
            turn_id,
            rx,
            events,
            session: Arc::clone(&self.inner),
        })
    }

    /// Live events plus a bounded replay of the active turn (M3, ADR-015).
    ///
    /// A reconnect replays everything buffered so far, then streams live.
    /// With no active turn this is an already-closed stream.
    pub fn subscribe(&self) -> EventStream {
        let inner = self.lock();
        match &inner.active {
            Some(active) => {
                // Snapshot and subscribe while holding the buffer lock, so no
                // event can slip between them: `Emitter::emit` pushes to the
                // buffer and broadcasts in the same critical section.
                let buffer = active.buffer.lock().expect("turn buffer poisoned");
                let replay = buffer.iter().cloned().collect();
                let live = active.events.subscribe();
                drop(buffer);
                EventStream::replay(replay, live)
            }
            None => EventStream::closed(),
        }
    }

    /// Abort the active turn (no-op when idle). The aborted stream receives
    /// one terminal `Error { kind: Aborted }` and closes; the partial answer
    /// is kept in history.
    pub fn abort(&self) -> Result<bool> {
        abort_turn(&self.inner, None)
    }

    /// Record a consent decision for a pending `ApprovalRequest` (M3). An
    /// unknown request id is `NotFound` (already resolved or timed out).
    pub fn approve(&self, request_id: &str, decision: Decision) -> Result<()> {
        let inner = self.lock();
        let Some(active) = &inner.active else {
            return Err(ApiError::new(
                ApiErrorKind::NotFound,
                "no turn is awaiting approval",
            ));
        };
        if active.approvals.resolve(request_id, decision) {
            Ok(())
        } else {
            Err(ApiError::new(
                ApiErrorKind::NotFound,
                format!("no pending approval with id {request_id:?}"),
            ))
        }
    }

    /// Regenerate the last exchange (M3): drop the trailing assistant/tool
    /// messages back to the last user message and re-run that turn. Fails
    /// `Busy` while a turn is active and `Config` when there is nothing to
    /// regenerate.
    pub fn regenerate(&self) -> Result<TurnHandle> {
        let mut inner = self.lock();
        if inner.active.is_some() {
            return Err(ApiError::new(
                ApiErrorKind::Busy,
                "a turn is already in progress; abort it first",
            ));
        }
        let Some(last_user) = inner
            .history
            .iter()
            .rposition(|message| message.role == crate::llm::Role::User)
        else {
            return Err(ApiError::config("nothing to regenerate"));
        };
        if last_user + 1 >= inner.history.len() {
            return Err(ApiError::config("nothing to regenerate"));
        }
        // Drop everything after the last user message and replay it,
        // attachments included.
        let message = inner.history[last_user].content.clone();
        let attachments = inner.history[last_user].artifacts.clone();
        inner.history.truncate(last_user);
        // Re-dispatch through the same path as a fresh send (which appends the
        // user message and starts the turn).
        drop(inner);
        self.send_with_attachments(&message, attachments)
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
            updated_at: self.updated_at.clone(),
            summary: self.summary.clone(),
            messages: self.history.iter().map(StoredMessage::from).collect(),
            usage: self.accumulated_usage,
        }
    }

    /// Write the conversation back to the store (when there is one). Never
    /// fails a turn: persistence errors are logged and the state stays in
    /// memory. The synchronous write runs under the session lock — deliberate
    /// at personal scale, see arc42 §11.
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
