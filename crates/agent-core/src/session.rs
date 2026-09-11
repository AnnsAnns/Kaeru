//! `ChatSession` + `TurnHandle`: the decoupled turn executor (ADR-015).
//!
//! M3 completes the wiring: a turn runs in a background task that survives
//! frontend disconnects. Every event is buffered (bounded) for replay and
//! broadcast live; `subscribe()` replays the buffer then follows live, so a
//! reconnect resumes the same turn. The agent loop (`agent::loop`) drives the
//! tool calls, consent middleware and fencing.
//!
//! Concurrency model: one active turn per session (`send` while active is
//! `ApiErrorKind::Busy`). All session state lives behind a single
//! `std::sync::Mutex` whose critical sections never await. History-flush
//! races between the turn task and `abort` are resolved by turn-id guard:
//! exactly one of them finalizes.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, oneshot};
use tokio::task::JoinHandle;

use crate::AgentCore;
use crate::agent::Emitter;
use crate::agent::r#loop::{self, LoopResult, TurnInput};
use crate::context::{self, ContextPolicy};
use crate::conversations::{
    CONVERSATION_SCHEMA_VERSION, Conversation, ConversationStore, StoredMessage,
};
use crate::error::{ApiError, ApiErrorKind, Result};
use crate::events::{
    ApprovalFuture, ApprovalKind, ApprovalSink, CoreEvent, Decision, EventStream, Usage,
};
use crate::llm::ChatMessage;

/// How long a consent card may wait for a decision before it fails closed.
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);

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
}

/// Core-owned consent resolution for one frontend (ADR-014): emits
/// `ApprovalRequest` and waits for `ChatSession::approve`, failing closed on
/// timeout.
struct SessionApprovals {
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Decision>>>>,
    next_id: AtomicU64,
    emitter: Emitter,
    timeout: Duration,
}

impl SessionApprovals {
    fn new(emitter: Emitter, timeout: Duration) -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
            emitter,
            timeout,
        }
    }

    /// Deliver a decision to a waiting request; false when the id is unknown.
    fn resolve(&self, request_id: &str, decision: Decision) -> bool {
        match self
            .pending
            .lock()
            .expect("approvals lock poisoned")
            .remove(request_id)
        {
            Some(sender) => sender.send(decision).is_ok(),
            None => false,
        }
    }
}

impl ApprovalSink for SessionApprovals {
    fn request(&self, kind: ApprovalKind, summary: String) -> ApprovalFuture {
        let id = format!("appr{:x}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .expect("approvals lock poisoned")
            .insert(id.clone(), tx);
        self.emitter.emit(CoreEvent::ApprovalRequest {
            id: id.clone(),
            kind,
            summary,
        });
        let pending = Arc::clone(&self.pending);
        let timeout = self.timeout;
        Box::pin(async move {
            let decision = tokio::time::timeout(timeout, rx)
                .await
                .ok()
                .and_then(|result| result.ok())
                .unwrap_or(Decision::Deny);
            pending.lock().expect("approvals lock poisoned").remove(&id);
            decision
        })
    }
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
        let buffer = Arc::new(Mutex::new(VecDeque::new()));
        let emitter = Emitter::new(events.clone(), Arc::clone(&buffer));
        let partial = Arc::new(Mutex::new(String::new()));
        let reasoning = Arc::new(Mutex::new(String::new()));
        let cancelled = Arc::new(AtomicBool::new(false));
        let approvals = Arc::new(SessionApprovals::new(emitter.clone(), APPROVAL_TIMEOUT));
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
                let replay = active
                    .buffer
                    .lock()
                    .expect("turn buffer poisoned")
                    .iter()
                    .cloned()
                    .collect();
                EventStream::replay(replay, active.events.subscribe())
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
    pub async fn approve(&self, request_id: &str, decision: Decision) -> Result<()> {
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
        // Drop everything after the last user message and replay it.
        let message = inner.history[last_user].content.clone();
        inner.history.truncate(last_user);
        // Re-dispatch through the same path as a fresh send (which appends the
        // user message and starts the turn).
        drop(inner);
        self.send(&message)
    }

    fn lock(&self) -> MutexGuard<'_, SessionInner> {
        self.inner.lock().expect("session lock poisoned")
    }
}

/// One row of the thread sidebar (M2.5): header fields only, newest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadSummary {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
    #[serde(rename = "messageCount")]
    pub message_count: usize,
    #[serde(default)]
    pub usage: Usage,
}

/// Platform-agnostic cache of live sessions over the plain-file store
/// (M2.5, §5.5 / ADR-024).
///
/// The UI calls conversations *threads*; the core keeps the term
/// *conversation*. One `Arc<ChatSession>` is cached per conversation id and
/// lazily loaded from disk, so an in-flight turn stays reachable across HTTP
/// requests. Storage stays the source of truth: a restart rebuilds every
/// thread from `data/conversations/`.
pub struct ConversationRegistry {
    core: Arc<AgentCore>,
    store: ConversationStore,
    sessions: Mutex<HashMap<String, Arc<ChatSession>>>,
}

impl ConversationRegistry {
    pub fn new(core: Arc<AgentCore>, store: ConversationStore) -> Self {
        Self {
            core,
            store,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    pub fn store(&self) -> &ConversationStore {
        &self.store
    }

    /// Sidebar rows, newest `updatedAt` first.
    pub fn list(&self) -> Result<Vec<ThreadSummary>> {
        let conversations = self.store.list()?;
        Ok(conversations
            .into_iter()
            .map(|c| ThreadSummary {
                id: c.id,
                title: c.title,
                created_at: c.created_at,
                updated_at: c.updated_at,
                message_count: c.messages.len(),
                usage: c.usage,
            })
            .collect())
    }

    /// Create a new empty thread and persist it immediately so it appears in
    /// listings before its first turn.
    pub fn create(&self, title: Option<String>) -> Result<Arc<ChatSession>> {
        let id = new_thread_id();
        let session = Arc::new(ChatSession::with_store(
            Arc::clone(&self.core),
            id.clone(),
            self.store.clone(),
        ));
        if title.is_some() {
            session.set_title(title);
        }
        session.persist();
        self.sessions
            .lock()
            .expect("registry lock poisoned")
            .insert(id, Arc::clone(&session));
        Ok(session)
    }

    /// The cached (or lazily loaded) session for `id`. An unknown id is
    /// `ApiErrorKind::NotFound`.
    pub fn get(&self, id: &str) -> Result<Arc<ChatSession>> {
        if let Some(session) = self
            .sessions
            .lock()
            .expect("registry lock poisoned")
            .get(id)
        {
            return Ok(Arc::clone(session));
        }
        if self.store.load(id)?.is_none() {
            return Err(ApiError::new(
                ApiErrorKind::NotFound,
                format!("no thread with id {id:?}"),
            ));
        }
        let session = Arc::new(ChatSession::with_store(
            Arc::clone(&self.core),
            id.to_owned(),
            self.store.clone(),
        ));
        let mut sessions = self.sessions.lock().expect("registry lock poisoned");
        Ok(Arc::clone(sessions.entry(id.to_owned()).or_insert(session)))
    }

    /// The most recently updated thread, if any.
    pub fn latest(&self) -> Result<Option<Arc<ChatSession>>> {
        match self.store.list()?.into_iter().next() {
            Some(conversation) => Ok(Some(self.get(&conversation.id)?)),
            None => Ok(None),
        }
    }

    /// Drop the session and remove its file. Deleting the selected thread is
    /// the caller's cue to reselect (fresh or newest).
    pub fn delete(&self, id: &str) -> Result<()> {
        if let Some(session) = self
            .sessions
            .lock()
            .expect("registry lock poisoned")
            .remove(id)
        {
            // Stop a running turn first so it cannot re-create the file.
            let _ = session.abort();
        }
        self.store.delete(id)
    }
}

/// Thread id generator: no `uuid` dependency (C15). Nanoseconds since the
/// epoch plus a process-local counter keeps ids unique and file-name-safe.
fn new_thread_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("t{nanos:x}{counter:x}")
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
        EventStream::live(self.events.subscribe())
    }

    /// Consume the handle into the turn's event stream (from the turn start).
    pub fn into_events(self) -> EventStream {
        EventStream::live(self.rx)
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
    let reasoning = active
        .reasoning
        .lock()
        .expect("reasoning lock poisoned")
        .clone();
    if !partial.is_empty() {
        inner.history.push(assistant_message(partial, reasoning));
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
    emitter: Emitter,
    model: String,
    reasoning_effort: Option<String>,
    history: Vec<ChatMessage>,
    summary: Option<String>,
    policy: ContextPolicy,
    partial: Arc<Mutex<String>>,
    reasoning: Arc<Mutex<String>>,
    cancelled: Arc<AtomicBool>,
    approvals: Arc<SessionApprovals>,
}

async fn run_turn(task: TurnTask) {
    let TurnTask {
        core,
        session,
        turn_id,
        emitter,
        model,
        reasoning_effort,
        history,
        summary,
        policy,
        partial,
        reasoning,
        cancelled,
        approvals,
    } = task;

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

    // The agent loop owns the provider calls, tool executions, consent and
    // fencing; the session owns the history flush (id-guarded).
    let result = r#loop::run(TurnInput {
        core: Arc::clone(&core),
        model,
        reasoning_effort,
        window,
        summary,
        emitter,
        partial: Arc::clone(&partial),
        reasoning: Arc::clone(&reasoning),
        cancelled: Arc::clone(&cancelled),
        approvals,
        turn_id,
        max_steps: core.config().agent.max_steps,
    })
    .await;

    if cancelled.load(Ordering::SeqCst) {
        return; // aborted from outside; abort_turn already flushed
    }
    finalize_turn(&session, turn_id, &partial, &reasoning, result);
}

/// Build the assistant message for a finished turn, attaching the model's
/// thinking (when any) so the UI can show it again after a reload.
fn assistant_message(content: String, reasoning: String) -> ChatMessage {
    let message = ChatMessage::assistant(content);
    if reasoning.is_empty() {
        message
    } else {
        message.with_reasoning(reasoning)
    }
}

fn finalize_turn(
    session: &Arc<Mutex<SessionInner>>,
    turn_id: u64,
    partial: &Arc<Mutex<String>>,
    reasoning: &Arc<Mutex<String>>,
    result: LoopResult,
) {
    let mut inner = session.lock().expect("session lock poisoned");
    let still_current = inner.active.as_ref().is_some_and(|a| a.turn_id == turn_id);
    if !still_current {
        return; // aborted from outside; abort_turn owns the flush
    }
    inner.active = None;
    if let Some(usage) = result.usage {
        inner.accumulated_usage.add(&usage);
    }

    // The loop's messages are already complete and ordered; commit them.
    let committed = result.new_messages.len();
    inner.history.extend(result.new_messages);

    if committed == 0 {
        let partial = partial.lock().expect("partial lock poisoned").clone();
        if !partial.is_empty() {
            // Failed after streaming text (no final message): keep what showed.
            let reasoning = reasoning.lock().expect("reasoning lock poisoned").clone();
            inner.history.push(assistant_message(partial, reasoning));
        } else if result.outcome == crate::agent::TurnOutcome::Failed {
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
    }
    // Persist after every finalized turn ("serialize on finalize", M2).
    inner.persist();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::AuditLog;
    use crate::events::{ApprovalKind, Risk, Usage};
    use crate::llm::{
        Cassette, ChatFuture, ChatRequest, ClientMode, FakeProvider, Interaction, LlmClient,
        ModelsFuture, Role,
    };
    use crate::search::{FakeSearch, SearchResult};
    use crate::tools::{MemoryStore, MemoryWriteTool, Tool, ToolContext, ToolFuture, ToolRegistry};
    use serde_json::{Value, json};
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;
    use tokio::sync::mpsc;

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
    async fn dropped_subscriber_no_longer_aborts_the_turn() {
        // M3 (ADR-015): the turn executor survives a frontend disconnect; a
        // reconnect would replay the buffer. The full answer is persisted.
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
        drop(handle); // frontend disconnect: the turn keeps running (M3)

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if session.history().len() == 2 || tokio::time::Instant::now() > deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let history = session.history();
        assert_eq!(history.len(), 2, "user message + full answer must be kept");
        // The whole answer arrived even with nobody listening.
        assert!(history[1].content.contains("d0"));
        assert!(history[1].content.contains("d9"));
        assert!(!session.is_active());
    }

    #[tokio::test]
    async fn subscribe_replays_the_turn_buffer_for_a_reconnect() {
        let request = ChatRequest::new(
            crate::config::DEFAULT_MODEL,
            vec![ChatMessage::user("hello")],
        );
        let mut events = Vec::new();
        for i in 0..6 {
            events.push(CoreEvent::Delta {
                text: format!("d{i} "),
            });
        }
        events.push(CoreEvent::TurnDone { usage: None });
        let fake = FakeProvider::from_cassette(cassette_for(&request, events))
            .with_delay(Duration::from_millis(20));
        let session = ChatSession::new(core_with(fake), "test");

        let handle = session.send("hello").unwrap();
        // Let a couple of deltas land in the buffer, then "reconnect".
        tokio::time::sleep(Duration::from_millis(45)).await;
        let mut reconnected = session.subscribe();
        let mut replayed = Vec::new();
        while let Ok(event) = reconnected.recv().await {
            let terminal = matches!(event, CoreEvent::TurnDone { .. });
            replayed.push(event);
            if terminal {
                break;
            }
        }
        // The replay starts from the beginning of the turn, not the reconnect.
        assert!(matches!(replayed[0], CoreEvent::Delta { ref text } if text.contains("d0")));
        assert!(matches!(replayed.last(), Some(CoreEvent::TurnDone { .. })));
        drain(handle.into_events()).await;
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

    #[tokio::test]
    async fn reasoning_streams_through_and_is_kept_out_of_the_answer() {
        let (core, _request) = quick_turn(vec![
            CoreEvent::Reasoning {
                text: "let me think".into(),
            },
            CoreEvent::Delta {
                text: "answer".into(),
            },
            CoreEvent::TurnDone { usage: None },
        ]);
        let session = ChatSession::new(core, "test");

        let handle = session.send("hello").unwrap();
        let events = drain(handle.into_events()).await;
        assert_eq!(
            events,
            vec![
                CoreEvent::Reasoning {
                    text: "let me think".into()
                },
                CoreEvent::Delta {
                    text: "answer".into()
                },
                CoreEvent::TurnDone { usage: None },
            ]
        );

        // Thinking is stored alongside the assistant message (for display on
        // reload) but is never folded into its content.
        let history = session.history();
        assert_eq!(history[0].reasoning, None);
        assert_eq!(history[1].content, "answer");
        assert_eq!(history[1].reasoning.as_deref(), Some("let me think"));
    }

    #[tokio::test]
    async fn reasoning_effort_override_reaches_the_provider_request() {
        let request = ChatRequest::new(
            crate::config::DEFAULT_MODEL,
            vec![ChatMessage::user("hello")],
        )
        .with_reasoning_effort(Some("high".into()));
        let session = ChatSession::new(
            core_with(FakeProvider::from_cassette(cassette_for(
                &request,
                vec![CoreEvent::TurnDone { usage: None }],
            ))),
            "test",
        );
        session.set_reasoning_effort(Some("high".into()));
        assert_eq!(session.current_reasoning_effort(), Some("high".into()));
        // A mismatch with the cassette would surface as an Error event, so
        // reaching TurnDone proves the effort rode along on the request.
        let handle = session.send("hello").unwrap();
        let events = drain(handle.into_events()).await;
        assert_eq!(events, vec![CoreEvent::TurnDone { usage: None }]);
        session.set_reasoning_effort(None);
        assert_eq!(session.current_reasoning_effort(), None);
    }

    #[test]
    fn empty_message_is_rejected() {
        let session = ChatSession::new(core_with(FakeProvider::builtin()), "test");
        let err = session.send("   ").unwrap_err();
        assert_eq!(err.kind, ApiErrorKind::Config);
    }

    #[tokio::test]
    async fn approving_an_unknown_request_is_not_found() {
        // M3: the consent flow is implemented; an id nobody is waiting on is a
        // plain NotFound (already resolved or timed out).
        let session = ChatSession::new(core_with(FakeProvider::builtin()), "test");
        let err = session
            .approve("appr_1", Decision::Allow)
            .await
            .unwrap_err();
        assert_eq!(err.kind, ApiErrorKind::NotFound);
        let _ = ApprovalKind::PackageInstall { packages: vec![] }; // M5 consent type
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
            updated_at: "1970-01-01T00:00:00Z".into(),
            model_override: None,
            reasoning_effort_override: None,
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

        // The file on disk carries the planned schema (Appendix C: schema: 2,
        // M2.5 added `updatedAt`).
        let text = std::fs::read_to_string(dir.join("default.json")).unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["schema"], 2);
        assert!(json["updatedAt"].is_string());
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

    /* ---------- ConversationRegistry (M2.5, §5.5 / ADR-024) ---------- */

    fn registry(name: &str) -> (ConversationRegistry, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("kaeru-test-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let registry = ConversationRegistry::new(
            core_with(FakeProvider::builtin()),
            ConversationStore::new(&dir),
        );
        (registry, dir)
    }

    #[test]
    fn registry_create_persists_an_empty_thread_and_lists_it() {
        let (registry, dir) = registry("registry-create");
        let session = registry.create(None).unwrap();
        let id = session.conversation_id();
        assert!(dir.join(format!("{id}.json")).is_file());

        let listed = registry.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, id);
        assert_eq!(listed[0].message_count, 0);
        assert!(listed[0].title.is_none());

        // A named thread keeps its explicit title.
        let named = registry.create(Some("My thread".into())).unwrap();
        assert_eq!(named.title().as_deref(), Some("My thread"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn registry_get_is_cached_and_unknown_ids_are_not_found() {
        let (registry, dir) = registry("registry-get");
        let created = registry.create(None).unwrap();
        let id = created.conversation_id();

        let fetched = registry.get(&id).unwrap();
        assert!(Arc::ptr_eq(&created, &fetched), "session must be cached");

        let err = registry.get("does-not-exist").err().unwrap();
        assert_eq!(err.kind, ApiErrorKind::NotFound);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn registry_delete_drops_the_session_and_the_file() {
        let (registry, dir) = registry("registry-delete");
        let session = registry.create(None).unwrap();
        let id = session.conversation_id();

        registry.delete(&id).unwrap();
        assert!(!dir.join(format!("{id}.json")).exists());
        assert!(registry.get(&id).is_err());
        assert!(registry.list().unwrap().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    fn stamp_updated_at(dir: &std::path::Path, id: &str, timestamp: &str) {
        let path = dir.join(format!("{id}.json"));
        let text = std::fs::read_to_string(&path).unwrap();
        let mut json: serde_json::Value = serde_json::from_str(&text).unwrap();
        json["updatedAt"] = serde_json::Value::String(timestamp.into());
        std::fs::write(&path, serde_json::to_string_pretty(&json).unwrap()).unwrap();
    }

    #[test]
    fn registry_picks_the_newest_thread_and_falls_back_after_delete() {
        let (registry, dir) = registry("registry-latest");
        assert!(registry.latest().unwrap().is_none());
        let older = registry.create(None).unwrap();
        let newer = registry.create(None).unwrap();
        stamp_updated_at(&dir, &older.conversation_id(), "2020-01-01T00:00:00Z");
        stamp_updated_at(&dir, &newer.conversation_id(), "2024-01-01T00:00:00Z");

        let latest = registry.latest().unwrap().unwrap();
        assert_eq!(latest.conversation_id(), newer.conversation_id());

        registry.delete(&newer.conversation_id()).unwrap();
        let latest = registry.latest().unwrap().unwrap();
        assert_eq!(latest.conversation_id(), older.conversation_id());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn threads_keep_independent_histories() {
        let dir = std::env::temp_dir().join(format!("kaeru-test-{}-threads", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let registry = ConversationRegistry::new(
            core_with(FakeProvider::builtin()),
            ConversationStore::new(&dir),
        );

        let a = registry.create(None).unwrap();
        let b = registry.create(None).unwrap();
        let handle = a.send("only in a").unwrap();
        drain(handle.into_events()).await;

        assert_eq!(a.history().len(), 2);
        assert!(b.history().is_empty());
        assert_eq!(registry.list().unwrap().len(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    /* ---------- M3: agent loop, consent, fencing, audit ---------- */

    /// A scripted client: returns queued event batches in call order and
    /// records every request it saw (so tests can inspect the payloads).
    struct ScriptedClient {
        script: StdMutex<std::collections::VecDeque<Vec<CoreEvent>>>,
        requests: StdMutex<Vec<ChatRequest>>,
    }

    impl ScriptedClient {
        fn new(script: Vec<Vec<CoreEvent>>) -> Arc<Self> {
            Arc::new(Self {
                script: StdMutex::new(script.into()),
                requests: StdMutex::new(Vec::new()),
            })
        }

        fn requests(&self) -> Vec<ChatRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl LlmClient for ScriptedClient {
        fn chat(&self, request: ChatRequest) -> ChatFuture {
            self.requests.lock().unwrap().push(request);
            let events = self
                .script
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| vec![CoreEvent::TurnDone { usage: None }]);
            Box::pin(async move {
                let (tx, rx) = mpsc::channel(64);
                tokio::spawn(async move {
                    for event in events {
                        if tx.send(event).await.is_err() {
                            return;
                        }
                    }
                });
                Ok(rx)
            })
        }

        fn list_models(&self) -> ModelsFuture {
            Box::pin(async { Ok(vec![]) })
        }
    }

    /// A tool that records its inputs and returns a fixed output.
    struct StubTool {
        name: &'static str,
        risk: Risk,
        output: String,
        seen: Arc<StdMutex<Vec<Value>>>,
    }

    impl Tool for StubTool {
        fn name(&self) -> &'static str {
            self.name
        }
        fn description(&self) -> &'static str {
            "a test stub"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object"})
        }
        fn risk(&self, _input: &Value) -> Risk {
            self.risk.clone()
        }
        fn execute(&self, input: Value, _ctx: ToolContext) -> ToolFuture {
            self.seen.lock().unwrap().push(input);
            let output = self.output.clone();
            Box::pin(async move { Ok(output) })
        }
    }

    fn core_scripted(
        client: Arc<dyn LlmClient>,
        tools: ToolRegistry,
        audit: AuditLog,
    ) -> Arc<AgentCore> {
        Arc::new(
            AgentCore::with_mode(crate::config::Config::default(), client, ClientMode::Live)
                .with_tools(tools)
                .with_audit(audit),
        )
    }

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("kaeru-m3-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn tool_calls_run_and_their_result_is_fenced() {
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let mut registry = ToolRegistry::new();
        registry.register(StubTool {
            name: "echo",
            risk: Risk::Safe,
            output: "TOOL-OUTPUT".into(),
            seen: Arc::clone(&seen),
        });
        let client = ScriptedClient::new(vec![
            vec![
                CoreEvent::ToolCall {
                    id: "c1".into(),
                    name: "echo".into(),
                    input: json!({"x": 1}),
                },
                CoreEvent::TurnDone { usage: None },
            ],
            vec![
                CoreEvent::Delta {
                    text: "final answer".into(),
                },
                CoreEvent::TurnDone { usage: None },
            ],
        ]);
        let core = core_scripted(client.clone(), registry, AuditLog::disabled());
        let session = ChatSession::new(core, "test");

        let handle = session.send("hello").unwrap();
        let events = drain(handle.into_events()).await;

        assert!(
            events
                .iter()
                .any(|e| matches!(e, CoreEvent::ToolCall { name, .. } if name == "echo"))
        );
        let tool_result = events
            .iter()
            .find_map(|e| match e {
                CoreEvent::ToolResult {
                    output, is_error, ..
                } => Some((output.clone(), *is_error)),
                _ => None,
            })
            .expect("a ToolResult was emitted");
        assert!(!tool_result.1);
        assert!(tool_result.0.contains("TOOL-OUTPUT"));
        assert!(tool_result.0.contains("untrusted-data"));
        assert!(tool_result.0.contains("not instructions"));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, CoreEvent::Delta { text } if text == "final answer"))
        );
        assert!(matches!(events.last(), Some(CoreEvent::TurnDone { .. })));

        // History: user, assistant(tool_calls), tool(result), assistant(answer).
        let history = session.history();
        assert_eq!(history.len(), 4);
        assert_eq!(history[1].role, Role::Assistant);
        assert_eq!(history[1].tool_calls.as_ref().unwrap()[0].name, "echo");
        assert_eq!(history[2].role, Role::Tool);
        assert!(history[2].content.contains("TOOL-OUTPUT"));
        assert_eq!(history[3], ChatMessage::assistant("final answer"));

        // The tool ran once; the second main-model request carried the fenced
        // result and still advertised the tools.
        assert_eq!(seen.lock().unwrap().len(), 1);
        let requests = client.requests();
        assert_eq!(requests.len(), 2);
        assert!(!requests[1].tools.is_empty());
        assert!(
            requests[1]
                .messages
                .iter()
                .any(|m| m.role == Role::Tool && m.content.contains("TOOL-OUTPUT"))
        );
    }

    #[tokio::test]
    async fn consent_gated_tool_waits_then_persists_when_allowed() {
        let dir = temp_dir("consent-allow");
        let audit_path = dir.join("audit.jsonl");
        let mut registry = ToolRegistry::new();
        registry.register(MemoryWriteTool::new(MemoryStore::new(dir.join("memory"))));
        let client = ScriptedClient::new(vec![
            vec![
                CoreEvent::ToolCall {
                    id: "c1".into(),
                    name: "memory_write".into(),
                    input: json!({"content": "remember frogs"}),
                },
                CoreEvent::TurnDone { usage: None },
            ],
            vec![
                CoreEvent::Delta {
                    text: "saved".into(),
                },
                CoreEvent::TurnDone { usage: None },
            ],
        ]);
        let core = core_scripted(client, registry, AuditLog::new(&audit_path));
        let session = Arc::new(ChatSession::new(core, "test"));

        let handle = session.send("remember frogs").unwrap();
        let mut tap = handle.events();
        let approver = Arc::clone(&session);
        let watcher = tokio::spawn(async move {
            while let Ok(event) = tap.recv().await {
                match event {
                    CoreEvent::ApprovalRequest { id, .. } => {
                        approver.approve(&id, Decision::Allow).await.unwrap();
                        break;
                    }
                    CoreEvent::TurnDone { .. } | CoreEvent::Error { .. } => break,
                    _ => {}
                }
            }
        });
        let events = drain(handle.into_events()).await;
        watcher.await.unwrap();

        // A consent card was shown, and the tool ran afterwards.
        assert!(
            events
                .iter()
                .any(|e| matches!(e, CoreEvent::ApprovalRequest { .. }))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, CoreEvent::ToolResult { is_error, .. } if !is_error))
        );
        // The entry was actually persisted (only after consent).
        let memory_files: Vec<_> = std::fs::read_dir(dir.join("memory"))
            .map(|entries| entries.flatten().collect())
            .unwrap_or_default();
        assert_eq!(memory_files.len(), 1);
        // Audit recorded the allow decision.
        let audit = std::fs::read_to_string(&audit_path).unwrap();
        assert!(audit.contains("\"decision\":\"allow\""));
        assert!(audit.contains("memory_write"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn denied_consent_writes_nothing_and_is_audited() {
        let dir = temp_dir("consent-deny");
        let audit_path = dir.join("audit.jsonl");
        let mut registry = ToolRegistry::new();
        registry.register(MemoryWriteTool::new(MemoryStore::new(dir.join("memory"))));
        let client = ScriptedClient::new(vec![vec![
            CoreEvent::ToolCall {
                id: "c1".into(),
                name: "memory_write".into(),
                input: json!({"content": "poison"}),
            },
            CoreEvent::TurnDone { usage: None },
        ]]);
        let core = core_scripted(client, registry, AuditLog::new(&audit_path));
        let session = Arc::new(ChatSession::new(core, "test"));

        let handle = session.send("write something").unwrap();
        let mut tap = handle.events();
        let approver = Arc::clone(&session);
        let watcher = tokio::spawn(async move {
            while let Ok(event) = tap.recv().await {
                match event {
                    CoreEvent::ApprovalRequest { id, .. } => {
                        approver.approve(&id, Decision::Deny).await.unwrap();
                        break;
                    }
                    CoreEvent::TurnDone { .. } | CoreEvent::Error { .. } => break,
                    _ => {}
                }
            }
        });
        let events = drain(handle.into_events()).await;
        watcher.await.unwrap();

        // The tool result is a structured denial; nothing was written.
        assert!(events.iter().any(
            |e| matches!(e, CoreEvent::ToolResult { output, is_error, .. } if *is_error && output.contains("denied"))
        ));
        assert!(std::fs::read_dir(dir.join("memory")).is_err());
        let audit = std::fs::read_to_string(&audit_path).unwrap();
        assert!(audit.contains("\"decision\":\"deny\""));
        assert!(audit.contains("\"status\":\"denied\""));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn raw_fetched_pages_never_reach_the_main_model() {
        // The worker reads the raw page; only its distillation is fenced back.
        let search = std::sync::Arc::new(FakeSearch::from_results(vec![SearchResult {
            title: "Frogs".into(),
            url: "https://example.test/frogs".into(),
            snippet: "RAW-PAGE-MARKER".into(),
        }]));
        let client = ScriptedClient::new(vec![
            // Main model asks for a search.
            vec![
                CoreEvent::ToolCall {
                    id: "c1".into(),
                    name: "web_search".into(),
                    input: json!({"query": "frogs"}),
                },
                CoreEvent::TurnDone { usage: None },
            ],
            // Summarizer worker returns the distillation.
            vec![
                CoreEvent::Delta {
                    text: "DISTILLED-SUMMARY".into(),
                },
                CoreEvent::TurnDone { usage: None },
            ],
            // Main model answers.
            vec![
                CoreEvent::Delta {
                    text: "grounded answer".into(),
                },
                CoreEvent::TurnDone { usage: None },
            ],
        ]);
        let core = Arc::new(
            AgentCore::with_mode(
                crate::config::Config::default(),
                client.clone(),
                ClientMode::Live,
            )
            .with_tools(ToolRegistry::with_defaults(5, None))
            .with_search(search),
        );
        let session = ChatSession::new(core, "test");

        let handle = session.send("search frogs").unwrap();
        let events = drain(handle.into_events()).await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e, CoreEvent::Delta { text } if text == "grounded answer"))
        );

        let requests = client.requests();
        // The worker saw the raw page text (fenced)...
        assert!(requests.iter().any(|r| {
            r.messages
                .iter()
                .any(|m| m.content.contains("RAW-PAGE-MARKER"))
        }));
        // ...but no main-model request (the ones with tools) ever did.
        assert!(requests.iter().filter(|r| !r.tools.is_empty()).all(|r| {
            r.messages
                .iter()
                .all(|m| !m.content.contains("RAW-PAGE-MARKER"))
        }));
    }

    #[tokio::test]
    async fn regenerate_reruns_the_last_user_message() {
        let client = ScriptedClient::new(vec![
            vec![
                CoreEvent::Delta { text: "one".into() },
                CoreEvent::TurnDone { usage: None },
            ],
            vec![
                CoreEvent::Delta { text: "two".into() },
                CoreEvent::TurnDone { usage: None },
            ],
        ]);
        let session = ChatSession::new(
            core_scripted(client.clone(), ToolRegistry::new(), AuditLog::disabled()),
            "test",
        );

        let handle = session.send("hello").unwrap();
        drain(handle.into_events()).await;
        assert_eq!(session.history()[1], ChatMessage::assistant("one"));

        let handle = session.regenerate().unwrap();
        drain(handle.into_events()).await;
        let history = session.history();
        assert_eq!(history.len(), 2, "the old answer is replaced, not appended");
        assert_eq!(history[0], ChatMessage::user("hello"));
        assert_eq!(history[1], ChatMessage::assistant("two"));
        assert_eq!(client.requests().len(), 2);
    }
}
