//! Evening reflection (M4.5, ADR-028): a daily job digests the conversations
//! changed since the last successful run through the tool-free reflector
//! worker (C16) and writes `reflect`-tagged memory notes. Enabling `[reflect]`
//! is the owner's standing consent; every run is audited, and the state file
//! advances only after a fully successful run.

mod parse;
#[cfg(test)]
mod tests;

use parse::{PersonaRevision, ReflectedNote, parse_reflection};

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::AgentCore;
use crate::agent::fence;
use crate::audit::AuditEntry;
use crate::conversations::{Conversation, ConversationStore, rfc3339_from_unix};
use crate::error::{ApiError, Result};
use crate::memory::store::{MemoryStore, local_date_from_unix, local_minutes_from_unix, now_unix};
use crate::util::truncate_chars;

/// How often the scheduler wakes to check the clock.
pub const REFLECT_TICK: Duration = Duration::from_secs(60);

/// Most conversations digested in one run (deterministic bound).
pub const REFLECT_MAX_CONVERSATIONS: usize = 20;

/// Longest transcript fed to the worker per conversation, in characters.
pub const REFLECT_TRANSCRIPT_MAX_CHARS: usize = 4000;

/// Every reflection note carries this tag.
pub const REFLECT_TAG: &str = "reflect";

/// Tag on the note that records a persona revision (M4.5).
pub const PERSONA_TAG: &str = "persona";

/// Absolute cap on the persona file the reflector may write, in characters.
pub const PERSONA_MAX_CHARS: usize = 8000;

/// How much longer than the current persona a revision may be ("a bit"). The
/// agent may only nudge its own character, never rewrite it wholesale.
pub const PERSONA_MAX_GROWTH_CHARS: usize = 800;
/// What a reflection run did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReflectStatus {
    /// `[reflect] enabled = false`.
    Disabled,
    /// Enabled but not yet due (the scheduled time has not arrived).
    NotDue,
    /// The digest ran (possibly with nothing to digest).
    Ran,
}

/// The outcome of an attempted (or skipped) reflection run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReflectOutcome {
    pub status: ReflectStatus,
    /// Conversations digested this run.
    pub conversations: usize,
    /// Memory notes written this run.
    pub notes: usize,
    /// Whether the reflector's persona revision was applied this run.
    pub persona_changed: bool,
}

/// The reflection state file (`data/reflect-state.json`, ADR-028).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct ReflectState {
    #[serde(rename = "lastRun", default)]
    last_run: u64,
}
/// The evening-reflection job (M4.5, ADR-028): reads changed conversations,
/// runs the tool-free reflector worker, and writes `reflect`-tagged notes.
pub struct Reflector {
    core: Arc<AgentCore>,
    memory: MemoryStore,
    conversations: ConversationStore,
    state_path: PathBuf,
    enabled: bool,
    scheduled_minutes: u32,
    /// Whether the reflector may revise `data/persona.md` (M4.5).
    persona_edits: bool,
    tick: Duration,
}

impl Reflector {
    /// Build the reflector over the stores the core already owns: the memory
    /// store is taken from the core, so the digest and the agent always share
    /// one instance. Fails when no memory store is configured (reflection has
    /// nowhere to write). The digest is a no-op unless `config.reflect.enabled`
    /// is set.
    pub fn from_core(
        core: Arc<AgentCore>,
        conversations: ConversationStore,
        state_path: impl Into<PathBuf>,
    ) -> Result<Self> {
        let memory = core.memory().cloned().ok_or_else(|| {
            ApiError::config("evening reflection requires a memory store on the core")
        })?;
        Ok(Self::new(core, memory, conversations, state_path))
    }

    fn new(
        core: Arc<AgentCore>,
        memory: MemoryStore,
        conversations: ConversationStore,
        state_path: impl Into<PathBuf>,
    ) -> Self {
        let config = &core.config().reflect;
        let (enabled, scheduled_minutes, persona_edits) = (
            config.enabled,
            config.scheduled_minutes(),
            config.persona_edits,
        );
        Self {
            core,
            memory,
            conversations,
            state_path: state_path.into(),
            enabled,
            scheduled_minutes,
            persona_edits,
            tick: REFLECT_TICK,
        }
    }

    /// Scheduling interval override (tests).
    pub fn with_tick(mut self, tick: Duration) -> Self {
        self.tick = tick;
        self
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn tick(&self) -> Duration {
        self.tick
    }

    /// The unix time of the last fully successful run (`0` when never run).
    pub fn last_run(&self) -> u64 {
        match std::fs::read_to_string(&self.state_path) {
            Ok(text) => serde_json::from_str::<ReflectState>(&text)
                .map(|state| state.last_run)
                .unwrap_or_else(|err| {
                    tracing::warn!(
                        target: "agent_core::reflect",
                        "unreadable reflect state {}: {err}; treating as never run",
                        self.state_path.display()
                    );
                    0
                }),
            Err(_) => 0,
        }
    }

    /// The most recent scheduled cycle `(local date, minutes)` at `now`.
    fn scheduled_cycle(&self, now: u64) -> (String, u32) {
        let now = now as i64;
        if local_minutes_from_unix(now) >= self.scheduled_minutes {
            (local_date_from_unix(now), self.scheduled_minutes)
        } else {
            (local_date_from_unix(now - 86_400), self.scheduled_minutes)
        }
    }

    /// The cycle to run, when the last successful run predates it. Pure with
    /// respect to the injected `now`, so the scheduler is unit-testable.
    fn due_cycle(&self, now: u64) -> Option<(String, u32)> {
        if !self.enabled {
            return None;
        }
        let cycle = self.scheduled_cycle(now);
        let last_run = self.last_run();
        let last_cycle = (
            local_date_from_unix(last_run as i64),
            local_minutes_from_unix(last_run as i64),
        );
        (last_cycle < cycle).then_some(cycle)
    }

    /// Run at most once per scheduled cycle, when due (the boot catch-up and
    /// the scheduler both call this).
    pub async fn run_scheduled(&self, now: u64) -> Result<ReflectOutcome> {
        if !self.enabled {
            return Ok(ReflectOutcome {
                status: ReflectStatus::Disabled,
                conversations: 0,
                notes: 0,
                persona_changed: false,
            });
        }
        if self.due_cycle(now).is_none() {
            return Ok(ReflectOutcome {
                status: ReflectStatus::NotDue,
                conversations: 0,
                notes: 0,
                persona_changed: false,
            });
        }
        self.run_digest(now).await
    }

    /// Run the digest on demand (`POST /api/reflect`): bypasses the schedule
    /// but still honors the enabled flag.
    pub async fn run_now(&self, now: u64) -> Result<ReflectOutcome> {
        if !self.enabled {
            return Ok(ReflectOutcome {
                status: ReflectStatus::Disabled,
                conversations: 0,
                notes: 0,
                persona_changed: false,
            });
        }
        self.run_digest(now).await
    }

    /// Digest and write, auditing the attempt either way. The state file
    /// advances only when the whole run succeeds.
    async fn run_digest(&self, now: u64) -> Result<ReflectOutcome> {
        let started = Instant::now();
        let result = self.digest(now).await;
        let (status, input) = match &result {
            Ok(outcome) => (
                "ok".to_owned(),
                json!({
                    "conversations": outcome.conversations,
                    "notes": outcome.notes,
                    "persona_changed": outcome.persona_changed,
                }),
            ),
            Err(err) => (format!("error: {}", err.kind.as_str()), json!({})),
        };
        self.core.audit().append(&AuditEntry {
            turn_id: None,
            tool: "reflect".to_owned(),
            input,
            decision: None,
            model: None,
            status,
            duration_ms: started.elapsed().as_millis() as u64,
        });
        result
    }

    async fn digest(&self, now: u64) -> Result<ReflectOutcome> {
        let last_run = self.last_run();
        let cutoff = rfc3339_from_unix(last_run);
        // `>=` (not `>`): a conversation finalized in the same second as the
        // last run is retried on the next run instead of being skipped
        // forever. A repeated digest is harmless; a permanent gap loses data.
        let mut remaining: Vec<Conversation> = self
            .conversations
            .list()?
            .into_iter()
            .filter(|conversation| conversation.updated_at.as_str() >= cutoff.as_str())
            .filter(has_exchange)
            .collect();

        if remaining.is_empty() {
            self.write_state(now)?;
            return Ok(ReflectOutcome {
                status: ReflectStatus::Ran,
                conversations: 0,
                notes: 0,
                persona_changed: false,
            });
        }

        let persona = self.core.persona();

        // Digest every changed conversation, `REFLECT_MAX_CONVERSATIONS` per
        // worker call. Iterating (rather than taking one capped batch) means
        // a backlog is never stranded: the state only advances to `now` after
        // the entire changed set has been digested.
        //
        // Worker output is staged before anything is written: a worker failure
        // halfway through then leaves memory untouched, so a retry starts from
        // a clean slate instead of duplicating notes that already landed.
        let mut staged_notes: Vec<ReflectedNote> = Vec::new();
        let mut staged_persona: Option<PersonaRevision> = None;
        let mut digested = 0;
        while !remaining.is_empty() {
            let take = remaining.len().min(REFLECT_MAX_CONVERSATIONS);
            let batch: Vec<Conversation> = remaining.drain(..take).collect();
            // Fence every transcript as data (ADR-016): a day's history can
            // hold fetched web content, and the reflector must never treat it
            // as instructions.
            let content = digest_content(&batch, persona.as_deref());
            let output = self
                .core
                .workers()
                .run("reflector", &content, self.core.audit(), None)
                .await?;
            let reflection = parse_reflection(&output.text);
            if reflection.notes.is_empty() && reflection.persona.is_none() {
                return Err(ApiError::internal(
                    "reflector produced no parsable notes; state left untouched",
                ));
            }
            digested += batch.len();
            staged_notes.extend(reflection.notes);
            if reflection.persona.is_some() {
                staged_persona = reflection.persona;
            }
        }

        let mut written = 0;
        for note in staged_notes {
            // A retry after a partial write must not duplicate a note that
            // already landed.
            if self.memory.contains_body(&note.body) {
                continue;
            }
            let tags = with_reflect_tag(note.tags);
            self.memory.write(&note.body, &tags)?;
            written += 1;
        }
        // Every persona reflection is recorded in memory (why + how), whether
        // or not an actual change was applied (M4.5).
        let persona_changed = match staged_persona {
            Some(revision) => {
                let changed = self.record_persona(revision)?;
                written += 1;
                changed
            }
            None => false,
        };
        self.write_state(now)?;
        Ok(ReflectOutcome {
            status: ReflectStatus::Ran,
            conversations: digested,
            notes: written,
            persona_changed,
        })
    }

    /// Handle the reflector's persona reflection: record what it noticed and
    /// what it might want to change as a `persona` memory note, and apply the
    /// proposed revision to `data/persona.md` only when one is present, edits
    /// are enabled, and it is a small, genuine change. Returns whether the file
    /// was actually changed.
    fn record_persona(&self, revision: PersonaRevision) -> Result<bool> {
        let current = self.core.persona().unwrap_or_default();
        let proposed = revision.persona.trim();
        let mut applied: Option<(usize, usize)> = None;

        if !proposed.is_empty()
            && self.persona_edits
            && let Some(path) = self.core.persona_path()
        {
            if proposed == current {
                tracing::info!(
                    target: "agent_core::reflect",
                    "reflector proposed an unchanged persona; recording the thought only"
                );
            } else {
                let new_len = proposed.chars().count();
                let cap = if current.is_empty() {
                    PERSONA_MAX_CHARS
                } else {
                    (current.chars().count() + PERSONA_MAX_GROWTH_CHARS).min(PERSONA_MAX_CHARS)
                };
                if new_len <= cap {
                    write_persona(path, proposed)?;
                    applied = Some((current.chars().count(), new_len));
                } else {
                    tracing::warn!(
                        target: "agent_core::reflect",
                        "persona revision ignored: {new_len} chars exceeds the {cap}-char allowance"
                    );
                }
            }
        }

        let why = non_empty_or(revision.why.trim(), "(not given)");
        let how = non_empty_or(revision.how.trim(), "(not given)");
        let note = match applied {
            Some((previous, new)) => format!(
                "I revised my persona during my evening reflection.\n\n\
                 Why: {why}\n\n\
                 How: {how}\n\n\
                 (persona changed from {previous} to {new} characters)"
            ),
            None => format!(
                "I reflected on my persona during my evening reflection.\n\n\
                 Why: {why}\n\n\
                 How: {how}\n\n\
                 (I left my persona unchanged for now.)"
            ),
        };
        self.memory
            .write(&note, &[REFLECT_TAG.to_owned(), PERSONA_TAG.to_owned()])?;
        self.core.audit().append(&AuditEntry {
            turn_id: None,
            tool: PERSONA_TAG.to_owned(),
            input: json!({
                "applied": applied.is_some(),
                "previousChars": applied.map(|(previous, _)| previous).unwrap_or_else(|| current.chars().count()),
                "newChars": applied.map(|(_, new)| new),
            }),
            decision: None,
            model: None,
            status: if applied.is_some() { "edited" } else { "considered" }.to_owned(),
            duration_ms: 0,
        });
        tracing::info!(
            target: "agent_core::reflect",
            "persona reflection recorded (applied: {})",
            applied.is_some()
        );
        Ok(applied.is_some())
    }

    /// Persist the run time atomically (tmp + rename).
    fn write_state(&self, now: u64) -> Result<()> {
        if let Some(parent) = self
            .state_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).map_err(|e| {
                ApiError::internal(format!(
                    "cannot create reflect state dir {}: {e}",
                    parent.display()
                ))
            })?;
        }
        let state = ReflectState { last_run: now };
        let text = serde_json::to_string_pretty(&state)
            .map_err(|e| ApiError::internal(format!("cannot serialize reflect state: {e}")))?;
        let tmp = self.state_path.with_extension("json.tmp");
        std::fs::write(&tmp, text)
            .map_err(|e| ApiError::internal(format!("cannot write {}: {e}", tmp.display())))?;
        std::fs::rename(&tmp, &self.state_path).map_err(|e| {
            ApiError::internal(format!(
                "cannot finalize {}: {e}",
                self.state_path.display()
            ))
        })
    }
}
/// Render one worker prompt from a batch of conversations: the persona prefix
/// (when one exists) plus every transcript fenced as data (ADR-016).
fn digest_content(conversations: &[Conversation], persona: Option<&str>) -> String {
    let mut content = String::new();
    if let Some(persona) = persona {
        content.push_str("Assistant persona (write in this character):\n");
        content.push_str(persona);
        content.push_str("\n\n");
    }
    content.push_str("Conversations changed since the last reflection:\n");
    for conversation in conversations {
        let source = conversation
            .title
            .clone()
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| conversation.id.clone());
        content.push('\n');
        content.push_str(&fence(
            &format!("conversation {source}"),
            &transcript(conversation),
        ));
    }
    content
}

/// A conversation is a candidate only when it holds a real exchange: at least
/// one user message and one assistant answer.
fn has_exchange(conversation: &Conversation) -> bool {
    let has_user = conversation
        .messages
        .iter()
        .any(|message| message.role == crate::llm::Role::User);
    let has_assistant = conversation
        .messages
        .iter()
        .any(|message| message.role == crate::llm::Role::Assistant);
    has_user && has_assistant
}

/// Render one conversation as a bounded transcript (`[role] content` lines).
fn transcript(conversation: &Conversation) -> String {
    let mut text = String::new();
    for message in &conversation.messages {
        text.push_str(&format!("[{}] {}\n", message.role, message.content));
        if text.chars().count() >= REFLECT_TRANSCRIPT_MAX_CHARS {
            break;
        }
    }
    truncate_chars(&text, REFLECT_TRANSCRIPT_MAX_CHARS)
}
/// Always include the `reflect` tag, without duplicating it.
fn with_reflect_tag(mut tags: Vec<String>) -> Vec<String> {
    if !tags.iter().any(|tag| tag.eq_ignore_ascii_case(REFLECT_TAG)) {
        tags.push(REFLECT_TAG.to_owned());
    }
    tags
}

/// Write the persona file atomically (tmp + rename), creating its parent.
fn write_persona(path: &std::path::Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| {
            ApiError::internal(format!(
                "cannot create persona dir {}: {e}",
                parent.display()
            ))
        })?;
    }
    let tmp = path.with_extension("md.tmp");
    std::fs::write(&tmp, format!("{text}\n"))
        .map_err(|e| ApiError::internal(format!("cannot write {}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| ApiError::internal(format!("cannot finalize {}: {e}", path.display())))
}

fn non_empty_or<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    if value.is_empty() { fallback } else { value }
}

/// The scheduler task (spawned by a frontend at startup): ticks on a local
/// clock, runs the digest at most once per scheduled cycle, and returns
/// immediately when reflection is disabled.
pub async fn scheduler(reflector: Arc<Reflector>) {
    let mut attempted: Option<(String, u32)> = None;
    loop {
        if reflector.enabled() {
            let now = now_unix();
            if let Some(cycle) = reflector.due_cycle(now)
                && attempted.as_ref() != Some(&cycle)
            {
                attempted = Some(cycle);
                match reflector.run_digest(now).await {
                    Ok(outcome) => tracing::info!(
                        target: "agent_core::reflect",
                        conversations = outcome.conversations,
                        notes = outcome.notes,
                        "evening reflection finished"
                    ),
                    Err(err) => tracing::warn!(
                        target: "agent_core::reflect",
                        "evening reflection failed ({}): {}; will retry next cycle",
                        err.kind.as_str(),
                        err.message
                    ),
                }
            }
        }
        tokio::time::sleep(reflector.tick()).await;
    }
}
