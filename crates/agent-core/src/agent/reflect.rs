//! Evening reflection (M4.5, §6.9 / ADR-028).
//!
//! A core-owned daily job digests the conversations changed since the last
//! successful run through the tool-free **reflector** worker and writes the
//! resulting notes (facts plus the agent's own in-character thoughts) into the
//! memory store, tagged `reflect`. Enabling `[reflect]` is the owner's standing
//! consent: no per-note approval cards, because the reflector has no tools
//! (C16) and reads only first-party history. Every run and worker call lands in
//! `audit.jsonl`; `reflect-state.json` advances only after a fully successful
//! run, so a missed evening is retried instead of silently skipped.

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

/// Markers for the optional persona block in the reflector's output.
const PERSONA_OPEN: &str = "===PERSONA===";
const PERSONA_CLOSE: &str = "===END===";

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

/// One note parsed from the reflector worker's output.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ReflectedNote {
    tags: Vec<String>,
    body: String,
}

/// An optional persona revision the reflector proposed (M4.5). The agent may
/// nudge its own character during reflection; the change and its reasoning are
/// always recorded in memory.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PersonaRevision {
    why: String,
    how: String,
    persona: String,
}

/// The parsed reflector output: durable notes plus an optional persona nudge.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Reflection {
    notes: Vec<ReflectedNote>,
    persona: Option<PersonaRevision>,
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
    /// Build the reflector from the core's `[reflect]` config. The digest is a
    /// no-op unless `config.reflect.enabled` is set.
    pub fn new(
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

/// Parse the reflector's output into durable notes plus an optional persona
/// revision.
///
/// Notes are segments separated by a line of `===`, each in the distiller's
/// lenient `tags\n---\nbody` shape. The optional persona block is fenced by
/// `===PERSONA===` and `===END===`; inside, the lines before the first `---`
/// are `WHY:`/`HOW:` metadata and the rest is the complete revised persona.
fn parse_reflection(text: &str) -> Reflection {
    let mut reflection = Reflection::default();
    let mut segment: Vec<&str> = Vec::new();
    let mut in_persona = false;
    let mut persona_lines: Vec<&str> = Vec::new();

    let flush_notes = |lines: &mut Vec<&str>, notes: &mut Vec<ReflectedNote>| {
        let segment = lines.join("\n");
        if let Some(note) = parse_segment(&segment) {
            notes.push(note);
        }
        lines.clear();
    };

    for line in text.lines() {
        if in_persona {
            if line.trim() == PERSONA_CLOSE {
                reflection.persona = parse_persona_block(&persona_lines.join("\n"));
                persona_lines.clear();
                in_persona = false;
            } else {
                persona_lines.push(line);
            }
            continue;
        }
        if line.trim() == PERSONA_OPEN {
            flush_notes(&mut segment, &mut reflection.notes);
            in_persona = true;
            continue;
        }
        if line.trim() == "===" {
            flush_notes(&mut segment, &mut reflection.notes);
        } else {
            segment.push(line);
        }
    }
    // An unterminated persona block is ignored; regular notes still count.
    flush_notes(&mut segment, &mut reflection.notes);
    reflection
}

/// Parse the inside of a persona block: `WHY:`/`HOW:` lines describing the
/// reflection, an optional `---`, then an optional complete revised persona.
///
/// A block with only the reasoning (no revision) is a persona *reflection*: it
/// is recorded in memory but changes nothing. Returns `None` only when the
/// block holds nothing at all.
fn parse_persona_block(block: &str) -> Option<PersonaRevision> {
    let (meta, persona) = match block.split_once("\n---") {
        Some((meta, persona)) => (meta, persona.trim().to_owned()),
        None => (block, String::new()),
    };
    let mut why = String::new();
    let mut how = String::new();
    for line in meta.lines() {
        let line = line.trim();
        if let Some(value) = line
            .strip_prefix("WHY:")
            .or_else(|| line.strip_prefix("Why:"))
        {
            why = value.trim().to_owned();
        } else if let Some(value) = line
            .strip_prefix("HOW:")
            .or_else(|| line.strip_prefix("How:"))
        {
            how = value.trim().to_owned();
        }
    }
    if why.is_empty() && how.is_empty() && persona.is_empty() {
        return None;
    }
    Some(PersonaRevision { why, how, persona })
}

fn parse_segment(segment: &str) -> Option<ReflectedNote> {
    let mut tags = Vec::new();
    let mut body = String::new();
    let mut in_body = false;
    for line in segment.lines() {
        if !in_body {
            if line.trim() == "---" {
                in_body = true;
                continue;
            }
            tags.push(line);
        } else {
            body.push_str(line);
            body.push('\n');
        }
    }
    let body = body.trim();
    if !in_body || body.is_empty() {
        let fallback = segment.trim();
        if fallback.is_empty() {
            return None;
        }
        return Some(ReflectedNote {
            tags: Vec::new(),
            body: fallback.to_owned(),
        });
    }
    Some(ReflectedNote {
        tags: parse_tag_line(&tags.join(" ")),
        body: body.to_owned(),
    })
}

fn parse_tag_line(line: &str) -> Vec<String> {
    let line = line.trim();
    let line = line
        .strip_prefix("tags:")
        .or_else(|| line.strip_prefix("Tags:"))
        .unwrap_or(line);
    line.split(',')
        .map(|tag| tag.trim().trim_matches(['[', ']', '"', '\'']).to_owned())
        .filter(|tag| !tag.is_empty())
        .collect()
}

/// Always include the `reflect` tag, without duplicating it.
fn with_reflect_tag(mut tags: Vec<String>) -> Vec<String> {
    if !tags.iter().any(|tag| tag.eq_ignore_ascii_case(REFLECT_TAG)) {
        tags.push(REFLECT_TAG.to_owned());
    }
    tags
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::workers::REFLECTOR_SYSTEM;
    use crate::config::Config;
    use crate::conversations::{CONVERSATION_SCHEMA_VERSION, StoredMessage};
    use crate::events::{CoreEvent, Usage};
    use crate::llm::{Cassette, ChatMessage, ChatRequest, FakeProvider, Interaction, LlmClient};
    use std::path::Path;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kaeru-reflect-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn conversation(id: &str, updated_at: &str, messages: Vec<(&str, &str)>) -> Conversation {
        Conversation {
            schema: CONVERSATION_SCHEMA_VERSION,
            id: id.to_owned(),
            title: Some(format!("thread {id}")),
            created_at: "2026-09-01T00:00:00Z".into(),
            updated_at: updated_at.to_owned(),
            summary: None,
            messages: messages
                .into_iter()
                .map(|(role, content)| StoredMessage {
                    role: match role {
                        "user" => crate::llm::Role::User,
                        _ => crate::llm::Role::Assistant,
                    },
                    content: content.to_owned(),
                    tool_call_id: None,
                    tool_calls: None,
                    reasoning: None,
                })
                .collect(),
            usage: Usage::default(),
        }
    }

    /// Persist a conversation with an exact `updatedAt` (the store normally
    /// owns the clock, so tests that need a chosen timestamp write directly).
    fn save_with_updated(conversations: &ConversationStore, conversation: &Conversation) {
        std::fs::create_dir_all(conversations.dir()).unwrap();
        std::fs::write(
            conversations
                .dir()
                .join(format!("{}.json", conversation.id)),
            serde_json::to_string(conversation).unwrap(),
        )
        .unwrap();
    }

    /// The content the digest builds for a conversation set (no persona).
    fn digest_content_for_store(conversations: &ConversationStore) -> String {
        digest_content_for_store_with_persona(conversations, None)
    }

    /// The content the digest builds, including the persona prefix when given.
    fn digest_content_for_store_with_persona(
        conversations: &ConversationStore,
        persona: Option<&str>,
    ) -> String {
        let list = conversations.list().unwrap();
        digest_content(&list, persona)
    }

    fn reflector(
        config: Config,
        client: Arc<dyn LlmClient>,
        dir: &Path,
        conversations: ConversationStore,
    ) -> Reflector {
        let core = Arc::new(AgentCore::with_mode(
            config,
            client,
            crate::llm::ClientMode::Fake {
                cassette: PathBuf::new(),
            },
        ));
        Reflector::new(
            core,
            MemoryStore::new(dir.join("memory")),
            conversations,
            dir.join("reflect-state.json"),
        )
    }

    #[test]
    fn parse_reflection_splits_notes_and_tags() {
        let text = "rust, axum\n---\nOwnership notes\n===\n---\nA body with no tag line\n";
        let reflection = parse_reflection(text);
        let notes = reflection.notes;
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].tags, vec!["rust".to_string(), "axum".to_string()]);
        assert_eq!(notes[0].body, "Ownership notes");
        assert!(notes[1].tags.is_empty());
        assert_eq!(notes[1].body, "A body with no tag line");
        assert!(reflection.persona.is_none());
    }

    #[test]
    fn parse_reflection_extracts_an_optional_persona_block() {
        let text = "frogs\n---\nA note\n\
                    ===PERSONA===\n\
                    WHY: the day showed patience matters\n\
                    HOW: added a line about slowing down\n\
                    ---\n\
                    # Character\n\
                    I am a patient pond frog.\n\
                    ===END===\n";
        let reflection = parse_reflection(text);
        assert_eq!(reflection.notes.len(), 1);
        let revision = reflection.persona.expect("persona block parsed");
        assert_eq!(revision.why, "the day showed patience matters");
        assert_eq!(revision.how, "added a line about slowing down");
        assert_eq!(revision.persona, "# Character\nI am a patient pond frog.");
    }

    #[test]
    fn parse_reflection_handles_unterminated_and_consideration_only_persona_blocks() {
        // An unterminated block is ignored; regular notes still count.
        let text = "frogs\n---\nA note\n===PERSONA===\nWHY: x\n---\nno end marker\n";
        let reflection = parse_reflection(text);
        assert_eq!(reflection.notes.len(), 1);
        assert!(reflection.persona.is_none());

        // A block with only the reasoning is a reflection: no revision.
        let text = "frogs\n---\nA note\n===PERSONA===\nWHY: x\nHOW: still thinking\n===END===\n";
        let reflection = parse_reflection(text);
        assert_eq!(reflection.notes.len(), 1);
        let revision = reflection.persona.expect("consideration parsed");
        assert_eq!(revision.why, "x");
        assert_eq!(revision.how, "still thinking");
        assert!(revision.persona.is_empty());
    }

    #[test]
    fn reflect_tag_is_added_once() {
        assert_eq!(with_reflect_tag(vec![]), vec![REFLECT_TAG.to_string()]);
        assert_eq!(
            with_reflect_tag(vec!["reflect".into()]),
            vec![REFLECT_TAG.to_string()]
        );
        assert_eq!(
            with_reflect_tag(vec!["frogs".into()]),
            vec!["frogs".to_string(), REFLECT_TAG.to_string()]
        );
    }

    #[test]
    fn scheduling_is_due_when_last_run_precedes_the_cycle() {
        let dir = temp_dir("sched");
        let config = Config::parse("[reflect]\nenabled = true\n").unwrap();
        let client: Arc<dyn LlmClient> = Arc::new(FakeProvider::builtin());
        let reflector = reflector(
            config,
            client,
            &dir,
            ConversationStore::new(dir.join("conversations")),
        );
        // A time far in the future keeps the test timezone-independent.
        let now = 4_102_444_800u64;
        assert!(reflector.due_cycle(now).is_some(), "never run is due");
        reflector.write_state(now).unwrap();
        assert!(reflector.due_cycle(now).is_none(), "same cycle is not due");
        assert!(
            reflector.due_cycle(now + 2 * 86_400).is_some(),
            "next cycle"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn disabled_is_never_due() {
        let dir = temp_dir("disabled");
        let config = Config::parse("[reflect]\nenabled = false\n").unwrap();
        let client: Arc<dyn LlmClient> = Arc::new(FakeProvider::builtin());
        let reflector = reflector(
            config,
            client,
            &dir,
            ConversationStore::new(dir.join("conversations")),
        );
        assert!(reflector.due_cycle(1_000_000_000).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn successful_run_writes_notes_and_advances_state() {
        let dir = temp_dir("success");
        let conversations = ConversationStore::new(dir.join("conversations"));
        conversations
            .save(&conversation(
                "t1",
                "2099-01-01T10:00:00Z",
                vec![("user", "my frog is called Kaeru"), ("assistant", "noted!")],
            ))
            .unwrap();

        let config = Config::parse("[reflect]\nenabled = true\n").unwrap();
        let request = ChatRequest::new(
            config.provider.model.clone(),
            vec![
                ChatMessage::system(REFLECTOR_SYSTEM),
                ChatMessage::user(digest_content_for_store(&conversations)),
            ],
        )
        .with_max_tokens(Some(config.workers.reflector.max_output_tokens));
        let cassette = Cassette {
            cassette_version: crate::llm::CASSETTE_VERSION,
            recorded_at_unix: None,
            base_url: None,
            models: vec![],
            interactions: vec![Interaction {
                request,
                events: vec![
                    CoreEvent::Delta {
                        text: "frog, home\n---\nThe user's frog is called Kaeru.\n===\n---\nI should ask how the frog is doing.\n"
                            .into(),
                    },
                    CoreEvent::TurnDone { usage: None },
                ],
            }],
        };
        let client: Arc<dyn LlmClient> = Arc::new(FakeProvider::from_cassette(cassette));
        let reflector = reflector(config, client, &dir, conversations);

        let outcome = reflector.run_now(2_000_000_000).await.unwrap();
        assert_eq!(outcome.status, ReflectStatus::Ran);
        assert_eq!(outcome.conversations, 1);
        assert_eq!(outcome.notes, 2);

        let notes = reflector.memory.list();
        assert_eq!(notes.len(), 2);
        assert!(
            notes
                .iter()
                .all(|note| note.tags.iter().any(|tag| tag == REFLECT_TAG))
        );
        assert!(
            notes
                .iter()
                .any(|note| note.content.contains("called Kaeru"))
        );
        assert_eq!(reflector.last_run(), 2_000_000_000);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn persona_revision_is_applied_and_recorded_in_memory() {
        let dir = temp_dir("persona-apply");
        let conversations = ConversationStore::new(dir.join("conversations"));
        conversations
            .save(&conversation(
                "t1",
                "2099-01-01T10:00:00Z",
                vec![("user", "hi"), ("assistant", "hello")],
            ))
            .unwrap();
        let persona_path = dir.join("persona.md");
        std::fs::write(&persona_path, "I am a pond frog.\n").unwrap();

        let config = Config::parse("[reflect]\nenabled = true\n").unwrap();
        let request = ChatRequest::new(
            config.provider.model.clone(),
            vec![
                ChatMessage::system(REFLECTOR_SYSTEM),
                ChatMessage::user(digest_content_for_store_with_persona(
                    &conversations,
                    Some("I am a pond frog."),
                )),
            ],
        )
        .with_max_tokens(Some(config.workers.reflector.max_output_tokens));
        let cassette = Cassette {
            cassette_version: crate::llm::CASSETTE_VERSION,
            recorded_at_unix: None,
            base_url: None,
            models: vec![],
            interactions: vec![Interaction {
                request,
                events: vec![
                    CoreEvent::Delta {
                        text: "frogs\n---\nA note\n===PERSONA===\nWHY: it helps\nHOW: added a line about mornings\n---\nI am a pond frog.\nI like quiet mornings.\n===END===\n"
                            .into(),
                    },
                    CoreEvent::TurnDone { usage: None },
                ],
            }],
        };
        let client: Arc<dyn LlmClient> = Arc::new(FakeProvider::from_cassette(cassette));
        let core = Arc::new(
            AgentCore::with_mode(
                config,
                client,
                crate::llm::ClientMode::Fake {
                    cassette: PathBuf::new(),
                },
            )
            .with_persona(persona_path.clone()),
        );
        let reflector = Reflector::new(
            core,
            MemoryStore::new(dir.join("memory")),
            conversations,
            dir.join("reflect-state.json"),
        );

        let outcome = reflector.run_now(2_000_000_000).await.unwrap();
        assert!(outcome.persona_changed);
        assert_eq!(outcome.notes, 2, "one durable note plus the persona note");

        let updated = std::fs::read_to_string(&persona_path).unwrap();
        assert!(updated.contains("quiet mornings"));
        let notes = reflector.memory.list();
        let persona_note = notes
            .iter()
            .find(|note| note.tags.iter().any(|tag| tag == PERSONA_TAG))
            .expect("the persona change was recorded in memory");
        assert!(persona_note.content.contains("it helps"), "why is recorded");
        assert!(
            persona_note.content.contains("added a line about mornings"),
            "how is recorded"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn persona_consideration_only_is_recorded_without_changing_the_file() {
        let dir = temp_dir("persona-think");
        let conversations = ConversationStore::new(dir.join("conversations"));
        conversations
            .save(&conversation(
                "t1",
                "2099-01-01T10:00:00Z",
                vec![("user", "hi"), ("assistant", "hello")],
            ))
            .unwrap();
        let persona_path = dir.join("persona.md");
        std::fs::write(&persona_path, "I am a pond frog.\n").unwrap();

        let config = Config::parse("[reflect]\nenabled = true\n").unwrap();
        let request = ChatRequest::new(
            config.provider.model.clone(),
            vec![
                ChatMessage::system(REFLECTOR_SYSTEM),
                ChatMessage::user(digest_content_for_store_with_persona(
                    &conversations,
                    Some("I am a pond frog."),
                )),
            ],
        )
        .with_max_tokens(Some(config.workers.reflector.max_output_tokens));
        // A block with only WHY/HOW: a reflection, no revision.
        let cassette = Cassette {
            cassette_version: crate::llm::CASSETTE_VERSION,
            recorded_at_unix: None,
            base_url: None,
            models: vec![],
            interactions: vec![Interaction {
                request,
                events: vec![
                    CoreEvent::Delta {
                        text: "frogs\n---\nA note\n===PERSONA===\nWHY: I noticed I can be terse\nHOW: maybe soften my tone someday\n===END===\n"
                            .into(),
                    },
                    CoreEvent::TurnDone { usage: None },
                ],
            }],
        };
        let client: Arc<dyn LlmClient> = Arc::new(FakeProvider::from_cassette(cassette));
        let core = Arc::new(
            AgentCore::with_mode(
                config,
                client,
                crate::llm::ClientMode::Fake {
                    cassette: PathBuf::new(),
                },
            )
            .with_persona(persona_path.clone()),
        );
        let reflector = Reflector::new(
            core,
            MemoryStore::new(dir.join("memory")),
            conversations,
            dir.join("reflect-state.json"),
        );

        let outcome = reflector.run_now(2_000_000_000).await.unwrap();
        assert!(!outcome.persona_changed);
        assert_eq!(outcome.notes, 2);
        let updated = std::fs::read_to_string(&persona_path).unwrap();
        assert_eq!(updated, "I am a pond frog.\n", "file untouched");

        let notes = reflector.memory.list();
        let persona_note = notes
            .iter()
            .find(|note| note.tags.iter().any(|tag| tag == PERSONA_TAG))
            .expect("the persona reflection was recorded");
        assert!(persona_note.content.contains("I noticed I can be terse"));
        assert!(persona_note.content.contains("soften my tone someday"));
        assert!(persona_note.content.contains("left my persona unchanged"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn persona_edits_disabled_records_the_reflection_but_not_the_change() {
        let dir = temp_dir("persona-off");
        let conversations = ConversationStore::new(dir.join("conversations"));
        conversations
            .save(&conversation(
                "t1",
                "2099-01-01T10:00:00Z",
                vec![("user", "hi"), ("assistant", "hello")],
            ))
            .unwrap();
        let persona_path = dir.join("persona.md");
        std::fs::write(&persona_path, "I am a pond frog.\n").unwrap();

        let config = Config::parse("[reflect]\nenabled = true\npersona_edits = false\n").unwrap();
        let request = ChatRequest::new(
            config.provider.model.clone(),
            vec![
                ChatMessage::system(REFLECTOR_SYSTEM),
                ChatMessage::user(digest_content_for_store_with_persona(
                    &conversations,
                    Some("I am a pond frog."),
                )),
            ],
        )
        .with_max_tokens(Some(config.workers.reflector.max_output_tokens));
        let cassette = Cassette {
            cassette_version: crate::llm::CASSETTE_VERSION,
            recorded_at_unix: None,
            base_url: None,
            models: vec![],
            interactions: vec![Interaction {
                request,
                events: vec![
                    CoreEvent::Delta {
                        text: "frogs\n---\nA note\n===PERSONA===\nWHY: it helps\nHOW: nope\n---\nA different frog.\n===END===\n"
                            .into(),
                    },
                    CoreEvent::TurnDone { usage: None },
                ],
            }],
        };
        let client: Arc<dyn LlmClient> = Arc::new(FakeProvider::from_cassette(cassette));
        let core = Arc::new(
            AgentCore::with_mode(
                config,
                client,
                crate::llm::ClientMode::Fake {
                    cassette: PathBuf::new(),
                },
            )
            .with_persona(persona_path.clone()),
        );
        let reflector = Reflector::new(
            core,
            MemoryStore::new(dir.join("memory")),
            conversations,
            dir.join("reflect-state.json"),
        );

        let outcome = reflector.run_now(2_000_000_000).await.unwrap();
        assert!(!outcome.persona_changed);
        assert_eq!(outcome.notes, 2, "the reflection note is still recorded");
        let updated = std::fs::read_to_string(&persona_path).unwrap();
        assert_eq!(updated, "I am a pond frog.\n", "persona untouched");
        let notes = reflector.memory.list();
        let persona_note = notes
            .iter()
            .find(|note| note.tags.iter().any(|tag| tag == PERSONA_TAG))
            .expect("the persona reflection was still recorded");
        assert!(persona_note.content.contains("left my persona unchanged"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_failed_run_leaves_state_untouched() {
        let dir = temp_dir("failed");
        let conversations = ConversationStore::new(dir.join("conversations"));
        conversations
            .save(&conversation(
                "t1",
                "2099-01-01T10:00:00Z",
                vec![("user", "hi"), ("assistant", "hello")],
            ))
            .unwrap();
        let config = Config::parse("[reflect]\nenabled = true\n").unwrap();
        // The provider fails: the run must not advance the state file.
        let request = ChatRequest::new(
            config.provider.model.clone(),
            vec![
                ChatMessage::system(REFLECTOR_SYSTEM),
                ChatMessage::user(digest_content_for_store(&conversations)),
            ],
        )
        .with_max_tokens(Some(config.workers.reflector.max_output_tokens));
        let cassette = Cassette {
            cassette_version: crate::llm::CASSETTE_VERSION,
            recorded_at_unix: None,
            base_url: None,
            models: vec![],
            interactions: vec![Interaction {
                request,
                events: vec![CoreEvent::Error {
                    kind: crate::error::ApiErrorKind::Provider,
                    message: "boom".into(),
                }],
            }],
        };
        let client: Arc<dyn LlmClient> = Arc::new(FakeProvider::from_cassette(cassette));
        let reflector = reflector(config, client, &dir, conversations);

        let err = reflector.run_now(2_000_000_000).await.unwrap_err();
        assert_ne!(err.kind, crate::error::ApiErrorKind::Internal);
        assert_eq!(reflector.last_run(), 0, "state must not advance on failure");
        assert!(reflector.memory.list().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn disabled_run_reports_disabled_without_touching_state() {
        let dir = temp_dir("off");
        let config = Config::parse("[reflect]\nenabled = false\n").unwrap();
        let client: Arc<dyn LlmClient> = Arc::new(FakeProvider::builtin());
        let reflector = reflector(
            config,
            client,
            &dir,
            ConversationStore::new(dir.join("conversations")),
        );
        let outcome = reflector.run_now(2_000_000_000).await.unwrap();
        assert_eq!(outcome.status, ReflectStatus::Disabled);
        assert_eq!(reflector.last_run(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn empty_run_advances_state_without_calling_the_worker() {
        let dir = temp_dir("empty");
        let conversations = ConversationStore::new(dir.join("conversations"));
        // A conversation with no exchange must not be a candidate.
        conversations
            .save(&conversation(
                "t1",
                "2099-01-01T10:00:00Z",
                vec![("user", "hi")],
            ))
            .unwrap();
        let config = Config::parse("[reflect]\nenabled = true\n").unwrap();
        // No cassette: any worker call would be a loud error, proving none ran.
        let client: Arc<dyn LlmClient> = Arc::new(FakeProvider::builtin());
        let reflector = reflector(config, client, &dir, conversations);
        let outcome = reflector.run_now(2_000_000_000).await.unwrap();
        assert_eq!(outcome.status, ReflectStatus::Ran);
        assert_eq!(outcome.conversations, 0);
        assert_eq!(outcome.notes, 0);
        assert_eq!(reflector.last_run(), 2_000_000_000);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn every_changed_conversation_is_digested_past_the_batch_cap() {
        let dir = temp_dir("batches");
        let conversations = ConversationStore::new(dir.join("conversations"));
        // One more changed conversation than a single worker batch holds, with
        // distinct timestamps so newest-first order (and the split) is stable.
        for i in 0..(REFLECT_MAX_CONVERSATIONS + 1) {
            save_with_updated(
                &conversations,
                &conversation(
                    &format!("t{i:02}"),
                    &format!("2099-01-{:02}T10:00:00Z", i + 1),
                    vec![("user", "hi"), ("assistant", "hello")],
                ),
            );
        }
        let all = conversations.list().unwrap();
        assert_eq!(all.len(), REFLECT_MAX_CONVERSATIONS + 1);
        let batches = [
            all[..REFLECT_MAX_CONVERSATIONS].to_vec(),
            all[REFLECT_MAX_CONVERSATIONS..].to_vec(),
        ];

        let config = Config::parse("[reflect]\nenabled = true\n").unwrap();
        let interactions = batches
            .iter()
            .enumerate()
            .map(|(i, batch)| Interaction {
                request: ChatRequest::new(
                    config.provider.model.clone(),
                    vec![
                        ChatMessage::system(REFLECTOR_SYSTEM),
                        ChatMessage::user(digest_content(batch, None)),
                    ],
                )
                .with_max_tokens(Some(config.workers.reflector.max_output_tokens)),
                events: vec![
                    CoreEvent::Delta {
                        text: format!("batch{i}\n---\nnote from batch {i}\n"),
                    },
                    CoreEvent::TurnDone { usage: None },
                ],
            })
            .collect();
        let cassette = Cassette {
            cassette_version: crate::llm::CASSETTE_VERSION,
            recorded_at_unix: None,
            base_url: None,
            models: vec![],
            interactions,
        };
        let client: Arc<dyn LlmClient> = Arc::new(FakeProvider::from_cassette(cassette));
        let reflector = reflector(config, client, &dir, conversations);

        let outcome = reflector.run_now(2_000_000_000).await.unwrap();
        assert_eq!(outcome.status, ReflectStatus::Ran);
        assert_eq!(
            outcome.conversations,
            REFLECT_MAX_CONVERSATIONS + 1,
            "every changed conversation is digested, not just the newest batch"
        );
        assert_eq!(outcome.notes, 2);
        assert_eq!(reflector.memory.list().len(), 2);
        assert_eq!(reflector.last_run(), 2_000_000_000);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_conversation_from_the_last_runs_second_is_retried_not_skipped() {
        let dir = temp_dir("same-second");
        let conversations = ConversationStore::new(dir.join("conversations"));
        let now = 2_000_000_000u64;
        save_with_updated(
            &conversations,
            &conversation(
                "t1",
                &rfc3339_from_unix(now),
                vec![("user", "hi"), ("assistant", "hello")],
            ),
        );
        let config = Config::parse("[reflect]\nenabled = true\n").unwrap();
        let request = ChatRequest::new(
            config.provider.model.clone(),
            vec![
                ChatMessage::system(REFLECTOR_SYSTEM),
                ChatMessage::user(digest_content_for_store(&conversations)),
            ],
        )
        .with_max_tokens(Some(config.workers.reflector.max_output_tokens));
        let cassette = Cassette {
            cassette_version: crate::llm::CASSETTE_VERSION,
            recorded_at_unix: None,
            base_url: None,
            models: vec![],
            interactions: vec![Interaction {
                request,
                events: vec![
                    CoreEvent::Delta {
                        text: "second\n---\nnoted in the same second\n".into(),
                    },
                    CoreEvent::TurnDone { usage: None },
                ],
            }],
        };
        let client: Arc<dyn LlmClient> = Arc::new(FakeProvider::from_cassette(cassette));
        let reflector = reflector(config, client, &dir, conversations);
        // The previous run finished exactly in this conversation's second.
        reflector.write_state(now).unwrap();

        let outcome = reflector.run_now(now).await.unwrap();
        assert_eq!(
            outcome.conversations, 1,
            "a same-second conversation is retried, not skipped forever"
        );
        assert_eq!(outcome.notes, 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_retry_after_a_partial_run_does_not_duplicate_notes() {
        let dir = temp_dir("retry-dedupe");
        let conversations = ConversationStore::new(dir.join("conversations"));
        conversations
            .save(&conversation(
                "t1",
                "2099-01-01T10:00:00Z",
                vec![("user", "hi"), ("assistant", "hello")],
            ))
            .unwrap();
        let config = Config::parse("[reflect]\nenabled = true\n").unwrap();
        let request = ChatRequest::new(
            config.provider.model.clone(),
            vec![
                ChatMessage::system(REFLECTOR_SYSTEM),
                ChatMessage::user(digest_content_for_store(&conversations)),
            ],
        )
        .with_max_tokens(Some(config.workers.reflector.max_output_tokens));
        let cassette = Cassette {
            cassette_version: crate::llm::CASSETTE_VERSION,
            recorded_at_unix: None,
            base_url: None,
            models: vec![],
            interactions: vec![Interaction {
                request,
                events: vec![
                    CoreEvent::Delta {
                        text: "one\n---\nfirst note\n===\ntwo\n---\nsecond note\n".into(),
                    },
                    CoreEvent::TurnDone { usage: None },
                ],
            }],
        };
        let client: Arc<dyn LlmClient> = Arc::new(FakeProvider::from_cassette(cassette));
        let reflector = reflector(config, client, &dir, conversations);

        let outcome = reflector.run_now(2_000_000_000).await.unwrap();
        assert_eq!(outcome.notes, 2);
        // Simulate a run that wrote its notes but crashed before advancing the
        // state: the retry must not store the same notes again.
        reflector.write_state(0).unwrap();
        let retry = reflector.run_now(2_000_000_000).await.unwrap();
        assert_eq!(retry.conversations, 1);
        assert_eq!(retry.notes, 0, "identical notes are not written twice");
        assert_eq!(reflector.memory.list().len(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }
}
