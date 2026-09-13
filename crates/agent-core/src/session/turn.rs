//! The background turn task: context assembly (ADR-018), the agent loop, and
//! the id-guarded history flush shared by completion and abort.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;

use crate::AgentCore;
use crate::agent::Emitter;
use crate::agent::r#loop::{self, LoopResult, TurnInput};
use crate::context::{self, ContextPolicy};
use crate::error::{ApiErrorKind, Result};
use crate::events::{Artifact, CoreEvent, EventStream, Usage};
use crate::llm::ChatMessage;

use super::approvals::SessionApprovals;
use super::{ActiveTurn, SessionInner};

/// Per-turn handle: the request-scoped event stream plus turn-scoped abort.
///
/// The handle owns the turn's primordial receiver from the moment `send`
/// returns (no lost-events window), plus the sender for extra live taps.
/// `into_events()` replays from the turn's start; `events()` taps from now.
pub struct TurnHandle {
    pub(super) turn_id: u64,
    pub(super) rx: broadcast::Receiver<CoreEvent>,
    pub(super) events: broadcast::Sender<CoreEvent>,
    pub(super) session: Arc<Mutex<SessionInner>>,
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

pub(super) fn abort_turn(inner: &Arc<Mutex<SessionInner>>, only: Option<u64>) -> Result<bool> {
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
        let mut message = assistant_message(partial, reasoning);
        message.artifacts = active
            .artifacts
            .lock()
            .expect("artifacts lock poisoned")
            .clone();
        inner.history.push(message);
    }
    let _ = active
        .events
        .send(CoreEvent::error(ApiErrorKind::Aborted, "turn aborted"));
    inner.persist();
    Ok(true)
}

pub(super) struct TurnTask {
    pub(super) core: Arc<AgentCore>,
    pub(super) session: Arc<Mutex<SessionInner>>,
    pub(super) turn_id: u64,
    pub(super) emitter: Emitter,
    pub(super) model: String,
    pub(super) reasoning_effort: Option<String>,
    pub(super) history: Vec<ChatMessage>,
    pub(super) summary: Option<String>,
    pub(super) policy: ContextPolicy,
    pub(super) partial: Arc<Mutex<String>>,
    pub(super) reasoning: Arc<Mutex<String>>,
    pub(super) cancelled: Arc<AtomicBool>,
    pub(super) approvals: Arc<SessionApprovals>,
    pub(super) artifacts: Arc<Mutex<Vec<Artifact>>>,
}

pub(super) async fn run_turn(task: TurnTask) {
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
        artifacts,
    } = task;

    // Persona (M4.5, ADR-027): the owner-written character is read fresh at
    // every turn start and becomes the system prompt. An absent or unreadable
    // file changes nothing.
    let system = core.persona();
    let system_tokens = system
        .as_deref()
        .map(ContextPolicy::estimate_tokens)
        .unwrap_or(0);

    // Memory injection (M4, ADR-007): select a bounded block of durable notes
    // relevant to the newest user message and account for it in the budget.
    let memory = core.memory().and_then(|store| {
        let query = history
            .iter()
            .rev()
            .find(|message| message.role == crate::llm::Role::User)
            .map(|message| message.content.as_str())
            .unwrap_or_default();
        crate::memory::memory_block(store, query)
    });
    let memory_tokens = memory
        .as_deref()
        .map(ContextPolicy::estimate_tokens)
        .unwrap_or(0);

    // Deterministic context assembly (ADR-018): resolve overflow *before*
    // the provider call — drop-oldest, then fold the dropped turns into the
    // rolling summary with one bounded LLM sub-call. A failing summary
    // degrades to a deterministic excerpt, never to a lost turn.
    let (window, dropped) = context::split_window(
        policy,
        summary.as_deref(),
        system_tokens + memory_tokens,
        &history,
    );
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
        system,
        window,
        summary,
        memory,
        emitter,
        partial: Arc::clone(&partial),
        reasoning: Arc::clone(&reasoning),
        cancelled: Arc::clone(&cancelled),
        approvals,
        turn_id,
        max_steps: core.config().agent.max_steps,
        artifacts: Arc::clone(&artifacts),
    })
    .await;

    if cancelled.load(Ordering::SeqCst) {
        return; // aborted from outside; abort_turn already flushed
    }
    finalize_turn(&session, turn_id, &partial, &reasoning, &artifacts, result);
}

/// Build the assistant message for a finished turn, attaching the model's
/// thinking (when any) so the UI can show it again after a reload.
/// Attach a turn's artifacts (M5) to its final plain answer; intermediate
/// assistant messages carry tool calls, never artifacts.
fn attach_artifacts(messages: &mut [ChatMessage], artifacts: Vec<Artifact>) {
    if artifacts.is_empty() {
        return;
    }
    if let Some(message) = messages
        .iter_mut()
        .rev()
        .find(|message| message.role == crate::llm::Role::Assistant && message.tool_calls.is_none())
    {
        message.artifacts = artifacts;
    }
}

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
    artifacts: &Arc<Mutex<Vec<Artifact>>>,
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

    // A turn's artifacts (M5) belong to its final plain answer, so a reload
    // renders them next to the reply that produced them.
    let collected: Vec<Artifact> = artifacts.lock().expect("artifacts lock poisoned").clone();
    let mut produced = result.new_messages;
    attach_artifacts(&mut produced, collected.clone());

    // The loop's messages are already complete and ordered; commit them.
    let committed = produced.len();
    inner.history.extend(produced);

    if committed == 0 {
        let partial = partial.lock().expect("partial lock poisoned").clone();
        if !partial.is_empty() {
            // Failed after streaming text (no final message): keep what showed.
            let reasoning = reasoning.lock().expect("reasoning lock poisoned").clone();
            let mut message = assistant_message(partial, reasoning);
            message.artifacts = collected;
            inner.history.push(message);
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
